//! In-memory MCP server for tests (`WP-M1-09`): speaks both eras over stdio (`tokio::io::duplex`) and over the [`HttpPost`] abstraction.
//!
//! It is part of the crate (not `cfg(test)`) because the NGC kit test of `gap-conformance` and the integration tests use it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::stream;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

use super::error::McpError;
use super::jsonrpc::{Incoming, decode_frame};
use super::transport::http::{HttpPost, HttpReply, HttpRequest, HttpTransport};
use super::transport::stdio::StdioTransport;

/// Protocol era(s) the fake server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerEra {
    /// Stateless `2026-07-28` only.
    Modern,
    /// Stateful `2025-11-25` only (`server/discover` is unknown).
    Legacy,
    /// Legacy server whose `initialize` answers a version the client does not know.
    LegacyWrongVersion,
}

/// A tool of the fake server.
#[derive(Debug, Clone)]
pub struct FakeTool {
    /// Tool name.
    pub name: String,
    /// Description (untrusted text).
    pub description: String,
    /// `inputSchema`.
    pub input_schema: Value,
    /// `outputSchema`, if any.
    pub output_schema: Option<Value>,
    /// Annotations (untrusted hints).
    pub annotations: Value,
}

impl FakeTool {
    /// A tool with the given name and `inputSchema`.
    #[must_use]
    pub fn new(name: &str, input_schema: Value) -> Self {
        Self {
            name: name.to_owned(),
            description: format!("{name} tool"),
            input_schema,
            output_schema: None,
            annotations: Value::Null,
        }
    }
}

/// What the fake does with a `tools/call`.
#[derive(Debug, Clone)]
pub enum FakeCall {
    /// Answer with this `CallToolResult`.
    Reply(Value),
    /// Answer after `delay` (tokio time).
    Delayed(Duration, Value),
    /// Never answer.
    Hang,
    /// Answer with a JSON-RPC error.
    Error(i64, String),
}

/// A request as the fake server saw it.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// JSON-RPC method.
    pub method: String,
    /// `params`.
    pub params: Value,
    /// Lower-case HTTP headers (empty on stdio).
    pub headers: BTreeMap<String, String>,
}

type CallHandler = Arc<dyn Fn(&str, &Value) -> FakeCall + Send + Sync>;

struct State {
    tools: Vec<FakeTool>,
    resources: Vec<Value>,
    page_size: Option<usize>,
    handler: Option<CallHandler>,
    requests: Vec<RecordedRequest>,
    cancelled: Vec<Value>,
    session_epoch: u32,
    initialize_count: u32,
    deleted_sessions: usize,
    always_404: bool,
    drop_result_type: bool,
    sse: bool,
    in_flight: usize,
    max_in_flight: usize,
    dropped_in_flight: usize,
    calls: Vec<(String, Value)>,
    resource_reads: Vec<String>,
}

struct Inner {
    era: ServerEra,
    state: Mutex<State>,
    refused: AtomicUsize,
}

/// A controllable fake MCP server.
#[derive(Clone)]
pub struct FakeMcpServer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for FakeMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeMcpServer")
            .field("era", &self.inner.era)
            .finish_non_exhaustive()
    }
}

enum Outcome {
    Result(Value),
    Error(i64, String),
    /// A modern HTTP header problem: HTTP 400 plus the JSON-RPC error.
    HeaderMismatch(String),
    /// No reply (notifications).
    None,
    /// Session unknown (legacy HTTP).
    NotFound,
    /// Never completes.
    Hang,
}

impl FakeMcpServer {
    /// A server of `era` with no tools.
    #[must_use]
    pub fn new(era: ServerEra) -> Self {
        Self {
            inner: Arc::new(Inner {
                era,
                refused: AtomicUsize::new(0),
                state: Mutex::new(State {
                    tools: Vec::new(),
                    resources: Vec::new(),
                    page_size: None,
                    handler: None,
                    requests: Vec::new(),
                    cancelled: Vec::new(),
                    session_epoch: 1,
                    initialize_count: 0,
                    deleted_sessions: 0,
                    always_404: false,
                    drop_result_type: false,
                    sse: false,
                    in_flight: 0,
                    max_in_flight: 0,
                    dropped_in_flight: 0,
                    calls: Vec::new(),
                    resource_reads: Vec::new(),
                }),
            }),
        }
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the tool list.
    pub fn set_tools(&self, tools: Vec<FakeTool>) {
        self.st().tools = tools;
    }

    /// Replaces the resource list (each `{uri, name, mimeType}`).
    pub fn set_resources(&self, resources: Vec<Value>) {
        self.st().resources = resources;
    }

    /// Lists are served in pages of `n` items.
    pub fn set_page_size(&self, n: usize) {
        self.st().page_size = Some(n);
    }

    /// Installs the `tools/call` behaviour.
    pub fn set_call_handler(
        &self,
        handler: impl Fn(&str, &Value) -> FakeCall + Send + Sync + 'static,
    ) {
        self.st().handler = Some(Arc::new(handler));
    }

    /// Answers over SSE instead of a JSON body (HTTP only).
    pub fn use_sse(&self, on: bool) {
        self.st().sse = on;
    }

    /// Makes legacy sessions unknown (the next request gets `404`).
    pub fn expire_sessions(&self) {
        self.st().session_epoch += 1;
    }

    /// Answers every legacy request with `404`.
    pub fn always_404(&self, on: bool) {
        self.st().always_404 = on;
    }

    /// Omits `resultType` from modern results.
    pub fn drop_result_type(&self, on: bool) {
        self.st().drop_result_type = on;
    }

    /// Requests the server received, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.st().requests.clone()
    }

    /// `(tool, arguments)` of every `tools/call` received.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.st().calls.clone()
    }

    /// URIs of every `resources/read` received.
    #[must_use]
    pub fn resource_reads(&self) -> Vec<String> {
        self.st().resource_reads.clone()
    }

    /// `requestId`s of the `notifications/cancelled` received.
    #[must_use]
    pub fn cancelled_ids(&self) -> Vec<Value> {
        self.st().cancelled.clone()
    }

    /// Number of `initialize` requests handled.
    #[must_use]
    pub fn initialize_count(&self) -> u32 {
        self.st().initialize_count
    }

    /// Number of `DELETE` session teardowns received.
    #[must_use]
    pub fn deleted_sessions(&self) -> usize {
        self.st().deleted_sessions
    }

    /// Highest number of simultaneously open `tools/call`s.
    #[must_use]
    pub fn max_in_flight(&self) -> usize {
        self.st().max_in_flight
    }

    /// Open calls that were dropped before completing (a closed HTTP stream or an abandoned request).
    #[must_use]
    pub fn dropped_in_flight(&self) -> usize {
        self.st().dropped_in_flight
    }

    /// Server-initiated requests the client refused (stdio).
    #[must_use]
    pub fn refused_by_client(&self) -> usize {
        self.inner.refused.load(Ordering::Relaxed)
    }

    fn session_id(&self) -> String {
        format!("fake-session-{}", self.st().session_epoch)
    }

    fn page(&self, items: Vec<Value>, cursor: Option<&str>, key: &str) -> Value {
        let size = self.st().page_size.unwrap_or(usize::MAX);
        let start: usize = cursor
            .and_then(|c| c.strip_prefix("p"))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let end = start.saturating_add(size).min(items.len());
        let mut out = Map::new();
        out.insert(
            key.to_owned(),
            Value::Array(items.get(start..end).unwrap_or_default().to_vec()),
        );
        if end < items.len() {
            out.insert("nextCursor".to_owned(), json!(format!("p{end}")));
        }
        Value::Object(out)
    }

    fn envelope(&self, mut value: Value) -> Value {
        if self.inner.era == ServerEra::Modern
            && let Value::Object(map) = &mut value
            && !self.st().drop_result_type
        {
            map.entry("resultType").or_insert(json!("complete"));
        }
        value
    }

    /// Core request handler shared by stdio and HTTP.
    async fn handle(
        &self,
        method: &str,
        params: Value,
        headers: &BTreeMap<String, String>,
        is_http: bool,
    ) -> Outcome {
        self.st().requests.push(RecordedRequest {
            method: method.to_owned(),
            params: params.clone(),
            headers: headers.clone(),
        });
        let era = self.inner.era;
        if is_http && era == ServerEra::Modern {
            if let Some(m) = headers.get("mcp-method") {
                if m != method {
                    return Outcome::HeaderMismatch(
                        "Mcp-Method does not match the body".to_owned(),
                    );
                }
            } else if method != "notifications/cancelled" {
                return Outcome::HeaderMismatch("missing Mcp-Method".to_owned());
            }
            let name = match method {
                "tools/call" => params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                "resources/read" => params.get("uri").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            };
            if let Some(expected) = name
                && headers.get("mcp-name") != Some(&expected)
            {
                return Outcome::HeaderMismatch("missing or wrong Mcp-Name".to_owned());
            }
        }
        if era == ServerEra::Modern && !matches!(method, "notifications/cancelled" | "initialize") {
            let meta = params.get("_meta").cloned().unwrap_or(Value::Null);
            if meta
                .get("io.modelcontextprotocol/protocolVersion")
                .and_then(Value::as_str)
                != Some("2026-07-28")
            {
                return Outcome::Error(-32602, "missing _meta protocolVersion".to_owned());
            }
        }
        if era != ServerEra::Modern
            && is_http
            && !matches!(method, "initialize" | "server/discover")
        {
            let state_epoch = format!("fake-session-{}", self.st().session_epoch);
            if self.st().always_404 || headers.get("mcp-session-id") != Some(&state_epoch) {
                return Outcome::NotFound;
            }
        }
        match method {
            "server/discover" if era == ServerEra::Modern => {
                Outcome::Result(self.envelope(json!({
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "fake", "version": "1"},
                    "ttlMs": 60_000,
                    "cacheScope": "public",
                })))
            }
            "server/discover" => Outcome::Error(-32601, "method not found".to_owned()),
            "initialize" if era == ServerEra::Modern => {
                Outcome::Error(-32601, "method not found".to_owned())
            }
            "initialize" => {
                self.st().initialize_count += 1;
                let version = if era == ServerEra::LegacyWrongVersion {
                    "2024-11-05"
                } else {
                    "2025-11-25"
                };
                Outcome::Result(
                    json!({"protocolVersion": version, "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "fake", "version": "1"}}),
                )
            }
            "notifications/initialized" => Outcome::None,
            "notifications/cancelled" => {
                if let Some(id) = params.get("requestId") {
                    self.st().cancelled.push(id.clone());
                }
                Outcome::None
            }
            "tools/list" => {
                let tools: Vec<Value> = self
                    .st()
                    .tools
                    .iter()
                    .map(|t| {
                        let mut v = json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema});
                        if let Some(o) = &t.output_schema {
                            v["outputSchema"] = o.clone();
                        }
                        if !t.annotations.is_null() {
                            v["annotations"] = t.annotations.clone();
                        }
                        v
                    })
                    .collect();
                let cursor = params.get("cursor").and_then(Value::as_str);
                let mut page = self.page(tools, cursor, "tools");
                if era == ServerEra::Modern {
                    page["ttlMs"] = json!(60_000);
                    page["cacheScope"] = json!("public");
                }
                Outcome::Result(self.envelope(page))
            }
            "resources/list" => {
                let resources = self.st().resources.clone();
                let cursor = params.get("cursor").and_then(Value::as_str);
                let mut page = self.page(resources, cursor, "resources");
                if era == ServerEra::Modern {
                    page["ttlMs"] = json!(60_000);
                    page["cacheScope"] = json!("public");
                }
                Outcome::Result(self.envelope(page))
            }
            "resources/read" => {
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.st().resource_reads.push(uri.clone());
                let mut result = json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": "{\"read\":true}"}]});
                if era == ServerEra::Modern {
                    result["ttlMs"] = json!(0);
                    result["cacheScope"] = json!("private");
                }
                Outcome::Result(self.envelope(result))
            }
            "tools/call" => self.call(&params).await,
            _ => Outcome::Error(-32601, "method not found".to_owned()),
        }
    }

    async fn call(&self, params: &Value) -> Outcome {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let handler = {
            let mut st = self.st();
            st.calls.push((name.clone(), args.clone()));
            st.in_flight += 1;
            st.max_in_flight = st.max_in_flight.max(st.in_flight);
            st.handler.clone()
        };
        struct Guard<'a>(&'a FakeMcpServer, bool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let mut st = self.0.st();
                st.in_flight = st.in_flight.saturating_sub(1);
                if !self.1 {
                    st.dropped_in_flight += 1;
                }
            }
        }
        let mut guard = Guard(self, false);
        let decision = handler.map_or_else(
            || FakeCall::Reply(json!({"content": [{"type": "text", "text": "{}"}]})),
            |h| h(&name, &args),
        );
        let outcome = match decision {
            FakeCall::Reply(v) => Outcome::Result(self.envelope(v)),
            FakeCall::Delayed(d, v) => {
                tokio::time::sleep(d).await;
                Outcome::Result(self.envelope(v))
            }
            FakeCall::Hang => {
                std::future::pending::<()>().await;
                Outcome::Hang
            }
            FakeCall::Error(code, msg) => Outcome::Error(code, msg),
        };
        guard.1 = true;
        outcome
    }

    // -------------------------------------------------------------------------------------------------------------------
    // transports
    // -------------------------------------------------------------------------------------------------------------------

    /// An HTTP transport talking to this server (no sockets).
    #[must_use]
    pub fn http_transport(&self) -> HttpTransport<FakeHttp> {
        HttpTransport::new(
            FakeHttp {
                server: self.clone(),
            },
            4 * 1024 * 1024,
        )
    }

    /// A stdio transport talking to this server through `tokio::io::duplex` (the server runs as a task).
    #[must_use]
    pub fn stdio_transport(&self) -> StdioTransport {
        let (client_w, server_r) = duplex(256 * 1024);
        let (server_w, client_r) = duplex(256 * 1024);
        let transport = StdioTransport::new(BufReader::new(client_r), client_w, 4 * 1024 * 1024);
        let server = self.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_r);
            let writer = Arc::new(tokio::sync::Mutex::new(server_w));
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let Ok(frame) = decode_frame(line.trim_end().as_bytes(), 8 * 1024 * 1024) else {
                    continue;
                };
                match frame {
                    Incoming::Notification { method } => {
                        let params = serde_json::from_str::<Value>(line.trim_end())
                            .ok()
                            .and_then(|v| v.get("params").cloned())
                            .unwrap_or(Value::Null);
                        let _ = server
                            .handle(&method, params, &BTreeMap::new(), false)
                            .await;
                    }
                    Incoming::Request { id, method } => {
                        let params = serde_json::from_str::<Value>(line.trim_end())
                            .ok()
                            .and_then(|v| v.get("params").cloned())
                            .unwrap_or(Value::Null);
                        let server = server.clone();
                        let writer = Arc::clone(&writer);
                        tokio::spawn(async move {
                            let reply = match server
                                .handle(&method, params, &BTreeMap::new(), false)
                                .await
                            {
                                Outcome::Result(v) => {
                                    json!({"jsonrpc": "2.0", "id": id, "result": v})
                                }
                                Outcome::Error(code, message) => {
                                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                                }
                                _ => return,
                            };
                            let mut w = writer.lock().await;
                            let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
                            let _ = w.flush().await;
                        });
                    }
                    Incoming::Response { .. } => {
                        server.inner.refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
        transport
    }
}

/// [`HttpPost`] adapter over a [`FakeMcpServer`].
#[derive(Debug)]
pub struct FakeHttp {
    server: FakeMcpServer,
}

impl HttpPost for FakeHttp {
    async fn post(&self, req: HttpRequest) -> Result<HttpReply, McpError> {
        let headers: BTreeMap<String, String> = req
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        let frame: Value = serde_json::from_slice(&req.body)
            .map_err(|_| McpError::Protocol("fake server got invalid JSON"))?;
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        let id = frame.get("id").cloned();
        let outcome = self.server.handle(&method, params, &headers, true).await;
        let sse = self.server.st().sse;
        let reply = |status: u16, extra: Vec<(String, String)>, body: Value| {
            let bytes = if sse && status == 200 {
                format!("event: message\ndata: {body}\n\n").into_bytes()
            } else {
                body.to_string().into_bytes()
            };
            let ctype = if sse && status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let mut headers = vec![("content-type".to_owned(), ctype.to_owned())];
            headers.extend(extra);
            HttpReply {
                status,
                headers,
                body: Box::pin(stream::iter(vec![Ok(bytes)])),
            }
        };
        Ok(match outcome {
            Outcome::Result(v) => {
                let extra = if method == "initialize" {
                    vec![("mcp-session-id".to_owned(), self.server.session_id())]
                } else {
                    vec![]
                };
                reply(200, extra, json!({"jsonrpc": "2.0", "id": id, "result": v}))
            }
            Outcome::Error(code, message) => reply(
                200,
                vec![],
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            ),
            Outcome::HeaderMismatch(message) => reply(
                400,
                vec![],
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32020, "message": message}}),
            ),
            Outcome::None => HttpReply {
                status: 202,
                headers: vec![],
                body: Box::pin(stream::empty()),
            },
            Outcome::NotFound => HttpReply {
                status: 404,
                headers: vec![],
                body: Box::pin(stream::empty()),
            },
            Outcome::Hang => return Err(McpError::Transport),
        })
    }

    async fn delete(&self, headers: Vec<(String, String)>) -> Result<u16, McpError> {
        let known = headers
            .iter()
            .any(|(k, v)| k == "mcp-session-id" && *v == self.server.session_id());
        if known {
            self.server.st().deleted_sessions += 1;
            Ok(200)
        } else {
            Ok(404)
        }
    }
}

impl FakeMcpServer {
    /// The [`HttpPost`] adapter itself (for tests that serve the fake over a real socket).
    #[must_use]
    pub fn http_client(&self) -> FakeHttp {
        FakeHttp {
            server: self.clone(),
        }
    }
}
