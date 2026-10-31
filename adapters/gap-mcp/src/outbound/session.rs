//! Era negotiation and request envelopes (`CMP-010`, `CMP-013`, SPEC-NOTES section (c)).
//!
//! The stateless `2026-07-28` era is probed first (`server/discover` carrying `_meta`); a server that does not know it is spoken to in the
//! stateful `2025-11-25` era (`initialize`, `notifications/initialized`, `MCP-Session-Id`, one re-initialisation on `404`).

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};

use super::error::McpError;
use super::transport::{CallOptions, HeaderHints, McpTransport, VERSION_LEGACY, VERSION_MODERN};

/// `_meta` key of the protocol version (`2026-07-28`).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key of the client capabilities (`2026-07-28`).
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
/// `_meta` key of the client identity (`2026-07-28`).
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";

/// MCP protocol era spoken with a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// Stateless `2026-07-28`.
    Modern,
    /// Stateful `2025-11-25`.
    Legacy,
}

impl Era {
    /// Protocol version string of the era.
    #[must_use]
    pub const fn version(self) -> &'static str {
        match self {
            Self::Modern => VERSION_MODERN,
            Self::Legacy => VERSION_LEGACY,
        }
    }
}

/// Client identity sent to servers.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Client version.
    pub version: String,
}

#[derive(Debug, Default)]
struct State {
    era: Option<Era>,
    session_id: Option<String>,
    capabilities: Value,
}

/// One logical connection to an MCP server.
pub struct Session<T> {
    transport: Arc<T>,
    client: ClientInfo,
    init_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

impl<T> std::fmt::Debug for Session<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("era", &self.state().era)
            .finish_non_exhaustive()
    }
}

/// Error codes and statuses that mean "this server does not speak the stateless era" (SPEC-NOTES Q-1, Q-10).
fn is_not_modern(error: &McpError) -> bool {
    match error {
        McpError::Rpc { code, .. } => {
            matches!(code, -32601 | -32602 | -32600 | -32020 | -32021 | -32022)
        }
        McpError::HttpStatus(status) => matches!(status, 400 | 404 | 405 | 406 | 415),
        _ => false,
    }
}

impl<T> Session<T> {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: McpTransport> Session<T> {
    /// Creates a session; nothing is sent until the first call.
    pub fn new(transport: Arc<T>, client: ClientInfo) -> Self {
        Self {
            transport,
            client,
            init_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(State::default()),
        }
    }

    /// The negotiated era, if the handshake happened.
    #[must_use]
    pub fn era(&self) -> Option<Era> {
        self.state().era
    }

    /// Capabilities the server announced during the handshake (`Null` before it).
    #[must_use]
    pub fn server_capabilities(&self) -> Value {
        self.state().capabilities.clone()
    }

    /// Current `MCP-Session-Id` (legacy era).
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.state().session_id.clone()
    }

    fn meta(&self) -> Value {
        // `roots` and `sampling` are deliberately absent (CMP-013).
        json!({
            META_PROTOCOL_VERSION: VERSION_MODERN,
            META_CLIENT_CAPABILITIES: {},
            META_CLIENT_INFO: {"name": self.client.name, "version": self.client.version},
        })
    }

    fn hints(&self, era: Era, name: Option<&str>) -> HeaderHints {
        HeaderHints {
            protocol_version: Some(era.version().to_owned()),
            mcp_name: name.map(str::to_owned),
            session_id: if era == Era::Legacy {
                self.session_id()
            } else {
                None
            },
        }
    }

    /// Adds `_meta` to `params` in the modern era (it is required on every request).
    fn with_meta(&self, era: Era, params: Value) -> Value {
        if era != Era::Modern {
            return params;
        }
        let mut map = match params {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                let mut m = Map::new();
                m.insert("value".to_owned(), other);
                m
            }
        };
        map.insert("_meta".to_owned(), self.meta());
        Value::Object(map)
    }

    /// Performs the handshake if it has not happened yet and returns the era.
    pub async fn ensure(&self, opts: &CallOptions) -> Result<Era, McpError> {
        if let Some(era) = self.era() {
            return Ok(era);
        }
        let _guard = self.init_lock.lock().await;
        if let Some(era) = self.era() {
            return Ok(era);
        }
        self.negotiate(opts).await
    }

    async fn negotiate(&self, opts: &CallOptions) -> Result<Era, McpError> {
        let params = self.with_meta(Era::Modern, json!({}));
        match self
            .transport
            .request(
                "server/discover",
                params,
                self.hints(Era::Modern, None),
                opts.clone(),
            )
            .await
        {
            Ok(reply) => {
                let value = reply.value;
                if value.get("resultType").and_then(Value::as_str) != Some("complete") {
                    return Err(McpError::Protocol(
                        "server/discover result lacks resultType complete",
                    ));
                }
                if let Some(versions) = value.get("supportedVersions").and_then(Value::as_array)
                    && !versions.iter().any(|v| v.as_str() == Some(VERSION_MODERN))
                {
                    return Err(McpError::Protocol("server does not support MCP 2026-07-28"));
                }
                let mut state = self.state();
                state.era = Some(Era::Modern);
                state.capabilities = value.get("capabilities").cloned().unwrap_or(Value::Null);
                Ok(Era::Modern)
            }
            Err(err) if is_not_modern(&err) => self.initialize_legacy(opts).await,
            Err(err) => Err(err),
        }
    }

    async fn initialize_legacy(&self, opts: &CallOptions) -> Result<Era, McpError> {
        let hints = HeaderHints {
            protocol_version: None,
            mcp_name: None,
            session_id: None,
        };
        let reply = self
            .transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": VERSION_LEGACY,
                    "capabilities": {},
                    "clientInfo": {"name": self.client.name, "version": self.client.version},
                }),
                hints,
                opts.clone(),
            )
            .await?;
        if reply.value.get("protocolVersion").and_then(Value::as_str) != Some(VERSION_LEGACY) {
            return Err(McpError::Protocol(
                "server does not speak a supported MCP version",
            ));
        }
        {
            let mut state = self.state();
            state.era = Some(Era::Legacy);
            state.session_id = reply.session_id;
            state.capabilities = reply
                .value
                .get("capabilities")
                .cloned()
                .unwrap_or(Value::Null);
        }
        self.transport
            .notify(
                "notifications/initialized",
                json!({}),
                self.hints(Era::Legacy, None),
            )
            .await?;
        Ok(Era::Legacy)
    }

    /// Forgets the handshake (after a `404` on a legacy session) so the next call re-initialises.
    fn reset(&self) {
        let mut state = self.state();
        state.era = None;
        state.session_id = None;
    }

    /// Sends one request in the negotiated era and validates the result envelope.
    ///
    /// A `404` on a legacy session triggers exactly one re-initialisation and one retry of the request.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        mcp_name: Option<&str>,
        opts: CallOptions,
    ) -> Result<Value, McpError> {
        let mut reinitialised = false;
        loop {
            let era = self.ensure(&opts).await?;
            let hints = self.hints(era, mcp_name);
            let payload = self.with_meta(era, params.clone());
            match self
                .transport
                .request(method, payload, hints, opts.clone())
                .await
            {
                Err(McpError::SessionNotFound) if era == Era::Legacy && !reinitialised => {
                    reinitialised = true;
                    self.reset();
                }
                Err(err) => return Err(err),
                Ok(reply) => return validate_envelope(era, reply.value),
            }
        }
    }

    /// Ends the legacy session (`DELETE`); a no-op on the stateless era.
    pub async fn close(&self) -> Result<(), McpError> {
        let Some(era) = self.era() else {
            return Ok(());
        };
        let result = self.transport.close(self.hints(era, None)).await;
        self.reset();
        result
    }
}

/// Checks the result envelope of a reply: in the modern era `resultType` is required (`complete` or `input_required`).
fn validate_envelope(era: Era, value: Value) -> Result<Value, McpError> {
    if era == Era::Legacy {
        return Ok(value);
    }
    match value.get("resultType").and_then(Value::as_str) {
        Some("complete" | "input_required") => Ok(value),
        Some(_) => Err(McpError::Protocol("result has an unknown resultType")),
        None => Err(McpError::Protocol("result lacks resultType")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::outbound::fake::{FakeMcpServer, ServerEra};

    fn opts() -> CallOptions {
        CallOptions {
            timeout: Duration::from_secs(30),
            cancel: CancellationToken::new(),
        }
    }

    fn client() -> ClientInfo {
        ClientInfo {
            name: "gap-mcp-test".to_owned(),
            version: "0".to_owned(),
        }
    }

    // spec: CMP-010, CMP-013
    #[tokio::test(start_paused = true)]
    async fn outbound_modern_server_is_probed_first_and_every_request_carries_meta_without_sampling_or_roots()
     {
        let server = FakeMcpServer::new(ServerEra::Modern);
        let session = Session::new(Arc::new(server.http_transport()), client());
        let out = session
            .call("tools/list", json!({}), None, opts())
            .await
            .expect("list");
        assert_eq!(out["resultType"], json!("complete"));
        assert_eq!(session.era(), Some(Era::Modern));
        let log = server.requests();
        assert_eq!(log[0].method, "server/discover");
        for r in &log {
            let meta = &r.params["_meta"];
            assert_eq!(meta[META_PROTOCOL_VERSION], json!("2026-07-28"), "{r:?}");
            let caps = &meta[META_CLIENT_CAPABILITIES];
            assert!(
                caps.get("sampling").is_none() && caps.get("roots").is_none(),
                "capabilities must omit sampling and roots"
            );
            assert_eq!(
                r.headers.get("mcp-method").map(String::as_str),
                Some(r.method.as_str())
            );
        }
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_legacy_server_gets_initialize_initialized_and_the_session_id_echoed() {
        let server = FakeMcpServer::new(ServerEra::Legacy);
        let session = Session::new(Arc::new(server.http_transport()), client());
        let out = session
            .call("tools/list", json!({}), None, opts())
            .await
            .expect("list");
        assert!(out.get("tools").is_some());
        assert_eq!(session.era(), Some(Era::Legacy));
        let methods: Vec<String> = server.requests().iter().map(|r| r.method.clone()).collect();
        assert_eq!(
            methods,
            [
                "server/discover",
                "initialize",
                "notifications/initialized",
                "tools/list"
            ]
        );
        let last = server.requests().pop().expect("last");
        assert_eq!(
            last.headers.get("mcp-session-id").map(String::as_str),
            Some("fake-session-1")
        );
        assert!(!last.headers.contains_key("mcp-method"));
        assert!(
            last.params.get("_meta").is_none(),
            "the legacy era has no _meta"
        );
        session.close().await.expect("close");
        assert_eq!(server.deleted_sessions(), 1);
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_stdio_legacy_fallback_on_method_not_found() {
        let server = FakeMcpServer::new(ServerEra::Legacy);
        let session = Session::new(Arc::new(server.stdio_transport()), client());
        let out = session
            .call("tools/list", json!({}), None, opts())
            .await
            .expect("list");
        assert!(out.get("tools").is_some());
        assert_eq!(session.era(), Some(Era::Legacy));
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_legacy_404_reinitialises_exactly_once() {
        let server = FakeMcpServer::new(ServerEra::Legacy);
        let session = Session::new(Arc::new(server.http_transport()), client());
        session
            .call("tools/list", json!({}), None, opts())
            .await
            .expect("first");
        server.expire_sessions();
        session
            .call("tools/list", json!({}), None, opts())
            .await
            .expect("after expiry");
        assert_eq!(server.initialize_count(), 2);
        // a server that keeps answering 404 must not loop
        let lists_before = server
            .requests()
            .iter()
            .filter(|r| r.method == "tools/list")
            .count();
        server.always_404(true);
        let err = session
            .call("tools/list", json!({}), None, opts())
            .await
            .expect_err("still 404");
        assert!(
            matches!(err, McpError::SessionNotFound | McpError::HttpStatus(404)),
            "{err:?}"
        );
        assert_eq!(
            server.initialize_count(),
            3,
            "exactly one more initialize, not a loop"
        );
        let lists_after = server
            .requests()
            .iter()
            .filter(|r| r.method == "tools/list")
            .count();
        assert_eq!(
            lists_after - lists_before,
            2,
            "the original attempt plus exactly one retry"
        );
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_a_server_lying_about_its_version_or_envelope_is_rejected() {
        let server = FakeMcpServer::new(ServerEra::Modern);
        server.drop_result_type(true);
        let session = Session::new(Arc::new(server.http_transport()), client());
        assert!(matches!(
            session.call("tools/list", json!({}), None, opts()).await,
            Err(McpError::Protocol(_))
        ));

        let server = FakeMcpServer::new(ServerEra::LegacyWrongVersion);
        let session = Session::new(Arc::new(server.http_transport()), client());
        assert!(matches!(
            session.call("tools/list", json!({}), None, opts()).await,
            Err(McpError::Protocol(_))
        ));
    }
}
