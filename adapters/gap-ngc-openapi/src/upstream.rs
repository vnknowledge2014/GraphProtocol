//! Upstream HTTP transport trait and `reqwest`-backed `HttpUpstream` (`CMP-033`).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::http::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::{Value, json};
use thiserror::Error;

use crate::auth::{ValidatedBaseUrl, encode_base64};

/// Default hard cap on upstream HTTP response body bytes in [`HttpUpstream`] (`16 MiB`, `CMP-033`).
///
/// Deliberately larger than `Limits.max_response_bytes` (`4 MiB`) so row-level NGC truncation (`NGC-027`)
/// operates on decoded arrays while `HttpUpstream` bounds raw wire memory.
pub const DEFAULT_MAX_UPSTREAM_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Outbound `GET` request sent to an [`Upstream`] (`CMP-030`, `CMP-033`).
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// Relative path and optional `?query` (always begins with `'/'`).
    pub path_and_query: String,
    /// Outbound HTTP headers (may carry sensitive [`HeaderValue`]s).
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Per-attempt timeout (`min(config.request_timeout, remaining_deadline)`).
    pub timeout: Duration,
}

/// Raw HTTP response returned by an [`Upstream`] (`CMP-033`).
#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    /// HTTP status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Raw response body bytes, already bounded by `max_body_bytes`.
    pub body: Vec<u8>,
}

/// Transport-level failure from [`Upstream::send`] (`CMP-033`, `INV-15`).
///
/// Variants intentionally carry no upstream URLs or response bodies so secrets in query parameters
/// or hostile upstream text can never leak into [`gap_core::error::NgcErrorInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpstreamError {
    /// TCP/connection establishment failed (eligible for bounded `GET` retry under `CMP-033`).
    #[error("upstream connection failed")]
    Connect,
    /// Attempt exceeded its per-request timeout / call deadline (`DEADLINE_EXCEEDED`).
    #[error("upstream request timed out")]
    Timeout,
    /// Response `Content-Length` or streamed chunks exceeded `max_body_bytes` (`UPSTREAM_ERROR`).
    #[error("upstream response body exceeds cap of {max_bytes} bytes")]
    BodyTooLarge {
        /// Configured hard byte cap.
        max_bytes: usize,
    },
    /// Other HTTP protocol or request framing failure (`UPSTREAM_ERROR`).
    #[error("upstream HTTP transport error")]
    Transport,
}

/// Failure when decoding a `2xx` [`UpstreamResponse`] payload (`CMP-033`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PayloadDecodeError {
    /// Body exceeded `max_body_bytes`.
    #[error("upstream response body exceeds cap of {max_bytes} bytes")]
    BodyTooLarge {
        /// Configured hard byte cap.
        max_bytes: usize,
    },
    /// `Content-Type` was not `application/json` or `*/*+json` and `allow_binary` is `false`.
    #[error("upstream response Content-Type is not JSON")]
    UnsupportedContentType,
    /// Body claimed JSON `Content-Type` but was malformed or exceeded structural parse bounds.
    #[error("upstream JSON body could not be parsed")]
    InvalidJson,
}

/// Abstract HTTP `GET` transport (`CMP-033`).
///
/// `OpenApiConnector<U>` is generic over `U: Upstream` (no `dyn`), matching `gap-ngc-sdk`.
pub trait Upstream: Send + Sync + 'static {
    /// Executes one `GET` attempt and returns a bounded [`UpstreamResponse`].
    fn send(
        &self,
        req: UpstreamRequest,
    ) -> impl Future<Output = Result<UpstreamResponse, UpstreamError>> + Send;
}

impl<U: Upstream> Upstream for Arc<U> {
    fn send(
        &self,
        req: UpstreamRequest,
    ) -> impl Future<Output = Result<UpstreamResponse, UpstreamError>> + Send {
        (**self).send(req)
    }
}

fn map_reqwest_error(err: &reqwest::Error) -> UpstreamError {
    if err.is_timeout() {
        UpstreamError::Timeout
    } else if err.is_connect() {
        UpstreamError::Connect
    } else {
        UpstreamError::Transport
    }
}

/// Async chunk producer used by [`read_capped_chunks`] (`CMP-033`).
pub trait ChunkSource {
    /// Yields the next body chunk, or `Ok(None)` at EOF.
    fn next_chunk(&mut self)
    -> impl Future<Output = Result<Option<Vec<u8>>, UpstreamError>> + Send;
}

impl ChunkSource for reqwest::Response {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, UpstreamError> {
        self.chunk()
            .await
            .map(|opt| opt.map(|bytes| bytes.to_vec()))
            .map_err(|e| map_reqwest_error(&e))
    }
}

impl<F, Fut> ChunkSource for F
where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<Option<Vec<u8>>, UpstreamError>> + Send,
{
    fn next_chunk(
        &mut self,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, UpstreamError>> + Send {
        (self)()
    }
}

/// Reads chunks from an async producer while enforcing both declared `content_length` and a
/// streaming byte cap, stopping immediately without polling further chunks once `max_body_bytes`
/// is exceeded (`CMP-033`).
pub async fn read_capped_chunks<S: ChunkSource>(
    declared_content_length: Option<u64>,
    max_body_bytes: usize,
    mut source: S,
) -> Result<Vec<u8>, UpstreamError> {
    let cap_u64 = u64::try_from(max_body_bytes).unwrap_or(u64::MAX);
    if let Some(len) = declared_content_length
        && len > cap_u64
    {
        return Err(UpstreamError::BodyTooLarge {
            max_bytes: max_body_bytes,
        });
    }

    let initial_cap = declared_content_length
        .and_then(|n| usize::try_from(n).ok())
        .map_or(0, |n| n.min(64 * 1024).min(max_body_bytes));
    let mut body = Vec::with_capacity(initial_cap);

    while let Some(chunk) = source.next_chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_body_bytes {
            return Err(UpstreamError::BodyTooLarge {
                max_bytes: max_body_bytes,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_json_media_type(headers: &HeaderMap) -> bool {
    let Some(raw) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let media = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    media == "application/json" || (media.contains('/') && media.ends_with("+json"))
}

/// Validates `Content-Type` and decodes a `2xx` upstream body into JSON (`CMP-033`).
pub fn decode_2xx_payload(
    headers: &HeaderMap,
    body: &[u8],
    max_body_bytes: usize,
    allow_binary: bool,
) -> Result<Value, PayloadDecodeError> {
    if body.len() > max_body_bytes {
        return Err(PayloadDecodeError::BodyTooLarge {
            max_bytes: max_body_bytes,
        });
    }
    if is_json_media_type(headers) {
        return serde_json::from_slice(body).map_err(|_| PayloadDecodeError::InvalidJson);
    }
    if allow_binary {
        return Ok(json!({ "raw": encode_base64(body) }));
    }
    Err(PayloadDecodeError::UnsupportedContentType)
}

/// `reqwest`-backed HTTP/1.1 upstream transport (`CMP-033`).
#[derive(Debug, Clone)]
pub struct HttpUpstream {
    client: reqwest::Client,
    base_url: ValidatedBaseUrl,
    max_body_bytes: usize,
    allow_binary: bool,
}

impl HttpUpstream {
    /// Builds a redirect-disabled (`Policy::none()`), proxy-disabled `HttpUpstream` (`CMP-033`).
    pub fn new(
        base_url: ValidatedBaseUrl,
        max_body_bytes: usize,
        allow_binary: bool,
    ) -> Result<Self, UpstreamError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| UpstreamError::Transport)?;
        Ok(Self {
            client,
            base_url,
            max_body_bytes,
            allow_binary,
        })
    }

    /// Borrows the locked [`ValidatedBaseUrl`].
    #[must_use]
    pub const fn base_url(&self) -> &ValidatedBaseUrl {
        &self.base_url
    }

    /// Configured hard body byte cap.
    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// Whether non-JSON `2xx` bodies are wrapped as `{"raw": "<base64>"}`.
    #[must_use]
    pub const fn allow_binary(&self) -> bool {
        self.allow_binary
    }
}

impl Upstream for HttpUpstream {
    async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let target_url = self
            .base_url
            .join_relative(&req.path_and_query)
            .map_err(|_| UpstreamError::Transport)?;

        let mut builder = self.client.get(target_url).timeout(req.timeout);
        let mut has_accept = false;
        for (name, val) in req.headers {
            // Never send `Accept-Encoding` (`reqwest` has compression off in v0).
            if name == ACCEPT_ENCODING {
                continue;
            }
            if name == ACCEPT {
                has_accept = true;
            }
            builder = builder.header(name, val);
        }
        if !has_accept && !self.allow_binary {
            builder = builder.header(ACCEPT, HeaderValue::from_static("application/json"));
        }

        let response = builder.send().await.map_err(|e| map_reqwest_error(&e))?;
        let status = response.status();
        let headers = response.headers().clone();

        let header_cl = headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let declared_len = response.content_length().or(header_cl);

        let body = read_capped_chunks(declared_len, self.max_body_bytes, response).await?;

        Ok(UpstreamResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::fixture::{FixtureService, router};

    // spec: CMP-033
    #[tokio::test]
    async fn streaming_body_cap_stops_early_on_content_length_and_mid_stream_chunks() {
        let pulls = AtomicUsize::new(0);
        // Declared Content-Length over cap must fail BEFORE pulling even chunk 0.
        let err = read_capped_chunks(Some(1025), 1024, || async {
            pulls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(vec![b'a'; 10]))
        })
        .await
        .expect_err("over Content-Length must fail");
        assert_eq!(err, UpstreamError::BodyTooLarge { max_bytes: 1024 });
        assert_eq!(pulls.load(Ordering::SeqCst), 0, "must not read body");

        // Chunked response without Content-Length: stops as soon as chunk crosses cap and never polls remaining chunks.
        let chunk_idx = AtomicUsize::new(0);
        let err = read_capped_chunks(None, 10, || async {
            let idx = chunk_idx.fetch_add(1, Ordering::SeqCst);
            match idx {
                0 => Ok(Some(vec![b'x'; 6])),
                1 => Ok(Some(vec![b'y'; 6])),
                _ => panic!("must never poll after exceeding max_body_bytes"),
            }
        })
        .await
        .expect_err("chunked overflow must fail");
        assert_eq!(err, UpstreamError::BodyTooLarge { max_bytes: 10 });
        assert_eq!(chunk_idx.load(Ordering::SeqCst), 2);

        // Within cap succeeds and concatenates chunks.
        let ok_idx = AtomicUsize::new(0);
        let body = read_capped_chunks(Some(8), 10, || async {
            let idx = ok_idx.fetch_add(1, Ordering::SeqCst);
            match idx {
                0 => Ok(Some(b"abcd".to_vec())),
                1 => Ok(Some(b"efgh".to_vec())),
                _ => Ok(None),
            }
        })
        .await
        .expect("within cap");
        assert_eq!(body, b"abcdefgh");
    }

    // spec: CMP-033
    #[test]
    fn content_type_enforces_json_or_plus_json_and_supports_allow_binary_base64() {
        let mut json_hdr = HeaderMap::new();
        json_hdr.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        let val = decode_2xx_payload(&json_hdr, br#"{"ok":true}"#, 1024, false).expect("json");
        assert_eq!(val, json!({ "ok": true }));

        let mut vnd_hdr = HeaderMap::new();
        vnd_hdr.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.shop.v1+json"),
        );
        let val = decode_2xx_payload(&vnd_hdr, br#"[1,2]"#, 1024, false).expect("+json");
        assert_eq!(val, json!([1, 2]));

        let mut bin_hdr = HeaderMap::new();
        bin_hdr.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        assert_eq!(
            decode_2xx_payload(&bin_hdr, b"\x00\x01\x02", 1024, false),
            Err(PayloadDecodeError::UnsupportedContentType)
        );
        let wrapped =
            decode_2xx_payload(&bin_hdr, b"foobar", 1024, true).expect("allow_binary base64");
        assert_eq!(wrapped, json!({ "raw": "Zm9vYmFy" }));
    }

    // spec: CMP-033, NGC-091
    #[tokio::test]
    #[ignore = "needs loopback sockets"]
    async fn loopback_http_upstream_against_fixture_router_smoke() {
        let svc = Arc::new(FixtureService::new());
        let app = router(Arc::clone(&svc));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base = ValidatedBaseUrl::parse(&format!("http://{addr}"), false, false).expect("base");
        let upstream = HttpUpstream::new(base, DEFAULT_MAX_UPSTREAM_BODY_BYTES, false).expect("up");
        let resp = upstream
            .send(UpstreamRequest {
                path_and_query: "/customers/1".to_owned(),
                headers: Vec::new(),
                timeout: Duration::from_secs(5),
            })
            .await
            .expect("send /customers/1");
        assert_eq!(resp.status, StatusCode::OK);
        let decoded = decode_2xx_payload(&resp.headers, &resp.body, 65_536, false).expect("json");
        assert_eq!(decoded["id"], json!(1));
        server.abort();
    }
}
