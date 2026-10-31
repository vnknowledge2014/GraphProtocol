//! `POST` Streamable HTTP transport (`CMP-010`, `CMP-012`) over a minimal [`HttpPost`] client.
//!
//! The reply is either one JSON body or an SSE stream; both are bounded. Cancellation on the stateless `2026-07-28` era drops the response
//! stream (the connection closes); on the legacy `2025-11-25` era the transport also sends `notifications/cancelled` (SPEC-NOTES Q-8).

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::stream::{self, Stream, StreamExt};
use gap_core::secret::SecretString;
use serde_json::{Value, json};

use super::{CallOptions, HeaderHints, McpTransport, Reply};
use crate::outbound::error::McpError;
use crate::outbound::jsonrpc::{
    Incoming, decode_frame, encode_notification, encode_request, numeric_id,
};

/// Cap on the number of events read from one SSE reply.
pub const MAX_SSE_EVENTS: usize = 10_000;
/// Cap on the total bytes read from one reply, as a multiple of the frame cap.
const TOTAL_CAP_FACTOR: usize = 4;

/// Body of an HTTP reply as a stream of byte chunks.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, McpError>> + Send>>;

/// One `POST` to the MCP endpoint.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// Header name/value pairs (lower-case names).
    pub headers: Vec<(String, String)>,
    /// JSON-RPC body.
    pub body: Vec<u8>,
    /// Per-attempt timeout.
    pub timeout: Duration,
}

/// Reply head plus streamed body.
pub struct HttpReply {
    /// HTTP status.
    pub status: u16,
    /// Header name/value pairs (lower-case names).
    pub headers: Vec<(String, String)>,
    /// Body chunks.
    pub body: BodyStream,
}

impl std::fmt::Debug for HttpReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpReply")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Minimal HTTP client: one `POST` to the fixed MCP endpoint. Dropping the returned future or the body closes the connection.
pub trait HttpPost: Send + Sync + 'static {
    /// Sends the request and returns the reply head with a lazily read body.
    fn post(
        &self,
        req: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpReply, McpError>> + Send;

    /// `DELETE` on the endpoint (legacy session teardown); returns the HTTP status. The default does nothing.
    fn delete(
        &self,
        _headers: Vec<(String, String)>,
    ) -> impl std::future::Future<Output = Result<u16, McpError>> + Send {
        std::future::ready(Ok(405))
    }
}

/// Streamable HTTP MCP transport.
#[derive(Debug)]
pub struct HttpTransport<C> {
    client: C,
    next_id: AtomicU64,
    max_frame_bytes: usize,
}

impl<C: HttpPost> HttpTransport<C> {
    /// Wraps `client`; `max_frame_bytes` bounds one JSON body or SSE event.
    pub fn new(client: C, max_frame_bytes: usize) -> Self {
        Self {
            client,
            next_id: AtomicU64::new(1),
            max_frame_bytes,
        }
    }

    fn headers(&self, method: &str, hints: &HeaderHints) -> Vec<(String, String)> {
        let mut headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
        ];
        if let Some(version) = &hints.protocol_version {
            headers.push(("mcp-protocol-version".to_owned(), version.clone()));
        }
        if hints.is_modern() {
            headers.push(("mcp-method".to_owned(), method.to_owned()));
            if let Some(name) = &hints.mcp_name {
                headers.push(("mcp-name".to_owned(), name.clone()));
            }
        }
        if let Some(session) = &hints.session_id {
            headers.push(("mcp-session-id".to_owned(), session.clone()));
        }
        headers
    }

    async fn exchange(
        &self,
        method: &str,
        body: String,
        hints: &HeaderHints,
        id: u64,
        timeout: Duration,
    ) -> Result<Reply, McpError> {
        let reply = self
            .client
            .post(HttpRequest {
                headers: self.headers(method, hints),
                body: body.into_bytes(),
                timeout,
            })
            .await?;
        let session_id = header(&reply.headers, "mcp-session-id");
        match reply.status {
            200..=202 => {}
            404 if hints.session_id.is_some() => return Err(McpError::SessionNotFound),
            status @ 400 => {
                // A JSON-RPC error object may explain the refusal (`-32020..-32022`).
                return match read_response(reply.body, id, self.max_frame_bytes).await {
                    Err(rpc @ McpError::Rpc { .. }) => Err(rpc),
                    _ => Err(McpError::HttpStatus(status)),
                };
            }
            status => return Err(McpError::HttpStatus(status)),
        }
        let value = read_response(reply.body, id, self.max_frame_bytes).await?;
        Ok(Reply { value, session_id })
    }
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// Reads the reply body (JSON or SSE, decided by the first bytes) until the response to `id` arrives.
async fn read_response(mut body: BodyStream, id: u64, max_frame: usize) -> Result<Value, McpError> {
    let total_cap = max_frame.saturating_mul(TOTAL_CAP_FACTOR);
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0_usize;
    let mut events = 0_usize;
    let mut sse: Option<bool> = None;
    let mut consumed = 0_usize;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        total = total.saturating_add(chunk.len());
        if total > total_cap {
            return Err(McpError::FrameTooLarge);
        }
        buf.extend_from_slice(&chunk);
        let is_sse = *sse.get_or_insert_with(|| {
            let head = buf.iter().copied().find(|b| !b.is_ascii_whitespace());
            !matches!(head, Some(b'{'))
        });
        if !is_sse {
            if buf.len() > max_frame {
                return Err(McpError::FrameTooLarge);
            }
            continue;
        }
        while let Some((event_end, next)) = find_event_end(&buf[consumed..]) {
            let event = &buf[consumed..consumed + event_end];
            consumed += next;
            events += 1;
            if events > MAX_SSE_EVENTS {
                return Err(McpError::Protocol("too many SSE events"));
            }
            if event.len() > max_frame {
                return Err(McpError::FrameTooLarge);
            }
            if let Some(data) = sse_data(event)
                && let Ok(Incoming::Response { id: rid, outcome }) = decode_frame(&data, max_frame)
                && numeric_id(&rid) == Some(id)
            {
                return outcome;
            }
        }
        if buf.len() - consumed > max_frame {
            return Err(McpError::FrameTooLarge);
        }
    }
    match sse {
        Some(false) => match decode_frame(&buf, max_frame)? {
            Incoming::Response { id: rid, outcome } if numeric_id(&rid) == Some(id) => outcome,
            Incoming::Response { .. } => {
                Err(McpError::Protocol("reply id does not match the request"))
            }
            _ => Err(McpError::Protocol("reply is not a JSON-RPC response")),
        },
        // A trailing event without the blank line terminator.
        Some(true) => {
            if let Some(data) = sse_data(&buf[consumed..])
                && let Ok(Incoming::Response { id: rid, outcome }) = decode_frame(&data, max_frame)
                && numeric_id(&rid) == Some(id)
            {
                return outcome;
            }
            Err(McpError::Protocol("SSE reply ended without a response"))
        }
        None => Err(McpError::Protocol("empty reply")),
    }
}

/// Finds the end of the first SSE event in `buf`: `(length of the event, bytes to skip)`.
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(b"\n\n") {
            return Some((i, i + 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, i + 4));
        }
        i += 1;
    }
    None
}

/// Joins the `data:` lines of one SSE event (`None` when it has none).
fn sse_data(event: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(event).ok()?;
    let mut data: Option<String> = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            match &mut data {
                Some(existing) => {
                    existing.push('\n');
                    existing.push_str(rest);
                }
                None => data = Some(rest.to_owned()),
            }
        }
    }
    data.map(String::into_bytes)
}

impl<C: HttpPost> McpTransport for HttpTransport<C> {
    async fn request(
        &self,
        method: &str,
        params: Value,
        hints: HeaderHints,
        opts: CallOptions,
    ) -> Result<Reply, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let body = encode_request(id, method, &params);
        let result = tokio::select! {
            biased;
            () = opts.cancel.cancelled() => Err(McpError::Cancelled),
            () = tokio::time::sleep(opts.timeout) => Err(McpError::Timeout),
            res = self.exchange(method, body, &hints, id, opts.timeout) => res,
        };
        if matches!(result, Err(McpError::Cancelled | McpError::Timeout))
            && !hints.is_modern()
            && hints.protocol_version.is_some()
        {
            // Legacy era: the server only learns about the abandoned request from the notification.
            let note = encode_notification(
                "notifications/cancelled",
                &json!({"requestId": id, "reason": "client cancelled"}),
            );
            let _ = tokio::time::timeout(
                Duration::from_millis(500),
                self.client.post(HttpRequest {
                    headers: self.headers("notifications/cancelled", &hints),
                    body: note.into_bytes(),
                    timeout: Duration::from_millis(500),
                }),
            )
            .await;
        }
        result
    }

    async fn notify(
        &self,
        method: &str,
        params: Value,
        hints: HeaderHints,
    ) -> Result<(), McpError> {
        let body = encode_notification(method, &params);
        let reply = self
            .client
            .post(HttpRequest {
                headers: self.headers(method, &hints),
                body: body.into_bytes(),
                timeout: Duration::from_secs(10),
            })
            .await?;
        match reply.status {
            200 | 202 | 204 => Ok(()),
            status => Err(McpError::HttpStatus(status)),
        }
    }

    async fn close(&self, hints: HeaderHints) -> Result<(), McpError> {
        if hints.is_modern() || hints.session_id.is_none() {
            return Ok(());
        }
        let status = self.client.delete(self.headers("delete", &hints)).await?;
        // `405` means the server does not support client-initiated termination, which the legacy spec allows.
        if matches!(status, 200 | 202 | 204 | 404 | 405) {
            Ok(())
        } else {
            Err(McpError::HttpStatus(status))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// reqwest-backed client
// ---------------------------------------------------------------------------------------------------------------------------------

/// Failure to build a [`ReqwestPost`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpConfigError {
    /// The endpoint URL does not parse or has no host.
    #[error("invalid MCP endpoint URL")]
    InvalidUrl,
    /// `https` needs a TLS backend, which is not vendored: refused (fail closed).
    #[error("https MCP endpoints are not supported in v0 (no TLS backend)")]
    TlsUnsupported,
    /// Only `http` is supported.
    #[error("unsupported MCP endpoint scheme")]
    UnsupportedScheme,
    /// The URL carries userinfo, a query or a fragment.
    #[error("MCP endpoint URL must not contain userinfo, query or fragment")]
    MalformedUrl,
    /// A bearer token would travel over cleartext HTTP to a non-loopback host.
    #[error("refusing to send credentials over cleartext http to a non-loopback host")]
    InsecureCredentials,
    /// The HTTP client could not be built.
    #[error("could not build the HTTP client")]
    Client,
}

/// `reqwest`-backed [`HttpPost`] (HTTP/1.1, redirects off, no proxy, no TLS).
pub struct ReqwestPost {
    client: reqwest::Client,
    url: reqwest::Url,
    bearer: Option<SecretString>,
}

impl std::fmt::Debug for ReqwestPost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestPost")
            .field("host", &self.url.host_str())
            .field("bearer", &self.bearer.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

impl ReqwestPost {
    /// Validates `endpoint` (an `http://host:port/path` URL) and builds the client.
    pub fn new(
        endpoint: &str,
        bearer: Option<SecretString>,
        allow_insecure_upstream: bool,
    ) -> Result<Self, HttpConfigError> {
        let url = reqwest::Url::parse(endpoint).map_err(|_| HttpConfigError::InvalidUrl)?;
        match url.scheme() {
            "http" => {}
            "https" => return Err(HttpConfigError::TlsUnsupported),
            _ => return Err(HttpConfigError::UnsupportedScheme),
        }
        if url.host().is_none() {
            return Err(HttpConfigError::InvalidUrl);
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(HttpConfigError::MalformedUrl);
        }
        if bearer.is_some() && !is_loopback(&url) && !allow_insecure_upstream {
            return Err(HttpConfigError::InsecureCredentials);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| HttpConfigError::Client)?;
        Ok(Self {
            client,
            url,
            bearer,
        })
    }
}

fn map_reqwest_error(err: &reqwest::Error) -> McpError {
    if err.is_timeout() {
        McpError::Timeout
    } else if err.is_connect() {
        McpError::Connect
    } else {
        McpError::Transport
    }
}

impl HttpPost for ReqwestPost {
    async fn post(&self, req: HttpRequest) -> Result<HttpReply, McpError> {
        let mut builder = self
            .client
            .post(self.url.clone())
            .timeout(req.timeout)
            .body(req.body);
        for (name, value) in req.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpError::Protocol("invalid header name"))?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| McpError::Protocol("invalid header value"))?;
            builder = builder.header(name, value);
        }
        if let Some(token) = &self.bearer {
            let mut value = reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                token.expose_secret()
            ))
            .map_err(|_| McpError::Protocol("invalid bearer token"))?;
            value.set_sensitive(true);
            builder = builder.header(reqwest::header::AUTHORIZATION, value);
        }
        let response = builder.send().await.map_err(|e| map_reqwest_error(&e))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_ascii_lowercase(), v.to_owned()))
            })
            .collect();
        let body: BodyStream = Box::pin(stream::unfold(Some(response), |state| async move {
            let mut response = state?;
            match response.chunk().await {
                Ok(Some(bytes)) => Some((Ok(bytes.to_vec()), Some(response))),
                Ok(None) => None,
                Err(err) => Some((Err(map_reqwest_error(&err)), None)),
            }
        }));
        Ok(HttpReply {
            status,
            headers,
            body,
        })
    }

    async fn delete(&self, headers: Vec<(String, String)>) -> Result<u16, McpError> {
        let mut builder = self
            .client
            .delete(self.url.clone())
            .timeout(Duration::from_secs(5));
        for (name, value) in headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(&value),
            ) {
                builder = builder.header(name, value);
            }
        }
        if let Some(token) = &self.bearer
            && let Ok(mut value) =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
        {
            value.set_sensitive(true);
            builder = builder.header(reqwest::header::AUTHORIZATION, value);
        }
        let response = builder.send().await.map_err(|e| map_reqwest_error(&e))?;
        Ok(response.status().as_u16())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio_util::sync::CancellationToken;

    use super::super::VERSION_MODERN;
    use super::*;

    type ScriptedReply = (u16, Vec<(String, String)>, Vec<Vec<u8>>);

    /// Scripted client: answers each POST with the next scripted reply and records the requests.
    struct Script {
        replies: Mutex<Vec<ScriptedReply>>,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl Script {
        fn new(replies: Vec<ScriptedReply>) -> Self {
            Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpPost for Script {
        async fn post(&self, req: HttpRequest) -> Result<HttpReply, McpError> {
            self.seen.lock().expect("seen").push(req);
            let next = {
                let mut replies = self.replies.lock().expect("replies");
                if replies.is_empty() {
                    None
                } else {
                    Some(replies.remove(0))
                }
            };
            let Some((status, headers, chunks)) = next else {
                // Pending forever: models a stream the server never finishes.
                std::future::pending::<()>().await;
                return Err(McpError::Transport);
            };
            Ok(HttpReply {
                status,
                headers,
                body: Box::pin(stream::iter(chunks.into_iter().map(Ok))),
            })
        }
    }

    fn opts() -> CallOptions {
        CallOptions {
            timeout: Duration::from_secs(30),
            cancel: CancellationToken::new(),
        }
    }

    fn modern() -> HeaderHints {
        HeaderHints {
            protocol_version: Some(VERSION_MODERN.to_owned()),
            mcp_name: Some("t".to_owned()),
            session_id: None,
        }
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_http_modern_request_carries_the_routing_headers_and_decodes_a_json_body() {
        let client = Script::new(vec![(
            200,
            vec![],
            vec![br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_vec()],
        )]);
        let t = HttpTransport::new(client, 1 << 20);
        let reply = t
            .request("tools/call", json!({}), modern(), opts())
            .await
            .expect("reply");
        assert_eq!(reply.value, json!({"ok": true}));
        let seen = t.client.seen.lock().expect("seen");
        let h = |n: &str| header(&seen[0].headers, n);
        assert_eq!(h("mcp-method").as_deref(), Some("tools/call"));
        assert_eq!(h("mcp-name").as_deref(), Some("t"));
        assert_eq!(h("mcp-protocol-version").as_deref(), Some("2026-07-28"));
        assert_eq!(
            h("accept").as_deref(),
            Some("application/json, text/event-stream")
        );
        assert!(h("mcp-session-id").is_none());
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_http_sse_reply_split_across_chunks_with_noise_events_yields_the_matching_response()
     {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n: comment\n\nid: 5\ndata: {\"jsonrpc\":\"2.0\",\n data-ignored\ndata: \"id\":1,\"result\":{\"v\":1}}\r\n\r\n";
        // split into awkward 7-byte chunks
        let chunks: Vec<Vec<u8>> = sse.as_bytes().chunks(7).map(<[u8]>::to_vec).collect();
        let client = Script::new(vec![(
            200,
            vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            chunks,
        )]);
        let t = HttpTransport::new(client, 1 << 20);
        let reply = t
            .request("tools/list", json!({}), modern(), opts())
            .await
            .expect("reply");
        assert_eq!(reply.value, json!({"v": 1}));
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_http_status_mapping_400_with_rpc_error_404_with_session_and_others() {
        let rpc400 =
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32020,"message":"HeaderMismatch"}}"#
                .to_vec();
        let t = HttpTransport::new(Script::new(vec![(400, vec![], vec![rpc400])]), 1 << 20);
        assert!(matches!(
            t.request("m", json!({}), modern(), opts()).await,
            Err(McpError::Rpc { code: -32020, .. })
        ));
        let t = HttpTransport::new(
            Script::new(vec![(400, vec![], vec![b"<html>".to_vec()])]),
            1 << 20,
        );
        assert_eq!(
            t.request("m", json!({}), modern(), opts())
                .await
                .expect_err("400"),
            McpError::HttpStatus(400)
        );
        let legacy = HeaderHints {
            protocol_version: Some("2025-11-25".to_owned()),
            mcp_name: None,
            session_id: Some("S1".to_owned()),
        };
        let t = HttpTransport::new(Script::new(vec![(404, vec![], vec![])]), 1 << 20);
        assert_eq!(
            t.request("m", json!({}), legacy.clone(), opts())
                .await
                .expect_err("404"),
            McpError::SessionNotFound
        );
        {
            let seen = t.client.seen.lock().expect("seen");
            assert_eq!(
                header(&seen[0].headers, "mcp-session-id").as_deref(),
                Some("S1")
            );
            assert!(
                header(&seen[0].headers, "mcp-method").is_none(),
                "legacy requests carry no Mcp-Method"
            );
        }
        let t = HttpTransport::new(Script::new(vec![(503, vec![], vec![])]), 1 << 20);
        assert_eq!(
            t.request("m", json!({}), modern(), opts())
                .await
                .expect_err("503"),
            McpError::HttpStatus(503)
        );
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_http_oversized_bodies_events_and_wrong_ids_are_rejected() {
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"{}"}}"#,
            "x".repeat(5_000)
        );
        let t = HttpTransport::new(
            Script::new(vec![(200, vec![], vec![big.into_bytes()])]),
            1_000,
        );
        assert_eq!(
            t.request("m", json!({}), modern(), opts())
                .await
                .expect_err("big"),
            McpError::FrameTooLarge
        );
        let wrong = br#"{"jsonrpc":"2.0","id":99,"result":{}}"#.to_vec();
        let t = HttpTransport::new(Script::new(vec![(200, vec![], vec![wrong])]), 1 << 20);
        assert!(matches!(
            t.request("m", json!({}), modern(), opts()).await,
            Err(McpError::Protocol(_))
        ));
        let endless = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n"
            .repeat(MAX_SSE_EVENTS + 5);
        let t = HttpTransport::new(
            Script::new(vec![(200, vec![], vec![endless.into_bytes()])]),
            1 << 20,
        );
        assert!(matches!(
            t.request("m", json!({}), modern(), opts()).await,
            Err(McpError::Protocol(_) | McpError::FrameTooLarge)
        ));
    }

    // spec: CMP-012
    #[tokio::test(start_paused = true)]
    async fn outbound_http_cancel_sends_nothing_on_the_modern_era_and_a_notification_on_the_legacy_era()
     {
        for (hints, expect_posts) in [
            (modern(), 1_usize),
            (
                HeaderHints {
                    protocol_version: Some("2025-11-25".to_owned()),
                    mcp_name: None,
                    session_id: Some("S".to_owned()),
                },
                2,
            ),
        ] {
            let t = std::sync::Arc::new(HttpTransport::new(Script::new(vec![]), 1 << 20));
            let cancel = CancellationToken::new();
            let o = CallOptions {
                timeout: Duration::from_secs(60),
                cancel: cancel.clone(),
            };
            let h = tokio::spawn({
                let t = std::sync::Arc::clone(&t);
                async move { t.request("tools/call", json!({}), hints, o).await }
            });
            tokio::time::sleep(Duration::from_millis(5)).await;
            cancel.cancel();
            assert_eq!(
                h.await.expect("join").expect_err("cancel"),
                McpError::Cancelled
            );
            let seen = t.client.seen.lock().expect("seen");
            assert_eq!(seen.len(), expect_posts, "posts recorded: {}", seen.len());
            if expect_posts == 2 {
                let note: Value = serde_json::from_slice(&seen[1].body).expect("json");
                assert_eq!(note["method"], json!("notifications/cancelled"));
            }
        }
    }

    // spec: CMP-010, INV-15
    #[test]
    fn outbound_reqwest_post_refuses_https_userinfo_and_cleartext_credentials() {
        assert!(ReqwestPost::new("http://127.0.0.1:9/mcp", None, false).is_ok());
        assert_eq!(
            ReqwestPost::new("https://example.com/mcp", None, false).expect_err("tls"),
            HttpConfigError::TlsUnsupported
        );
        assert_eq!(
            ReqwestPost::new("ftp://example.com/mcp", None, false).expect_err("scheme"),
            HttpConfigError::UnsupportedScheme
        );
        assert_eq!(
            ReqwestPost::new("http://u:p@127.0.0.1/mcp", None, false).expect_err("userinfo"),
            HttpConfigError::MalformedUrl
        );
        let token = SecretString::new("tok".to_owned());
        assert_eq!(
            ReqwestPost::new("http://10.0.0.5/mcp", Some(token.clone()), false)
                .expect_err("cleartext"),
            HttpConfigError::InsecureCredentials
        );
        assert!(ReqwestPost::new("http://10.0.0.5/mcp", Some(token.clone()), true).is_ok());
        assert!(ReqwestPost::new("http://localhost:3000/mcp", Some(token), false).is_ok());
        let dbg = format!(
            "{:?}",
            ReqwestPost::new(
                "http://127.0.0.1/mcp",
                Some(SecretString::new("CANARY-xyz".to_owned())),
                false
            )
            .expect("ok")
        );
        assert!(!dbg.contains("CANARY"));
    }
}
