//! HTTP `/v1/systemone` sidecar backend and golden-oracle harness (`ADR-005`, `RTE-041`, `RTE-044`, `WP-M5-06`).
//!
//! Designed to speak directly to local or remote `/v1/systemone` (single) and
//! `/v1/systemone/batch` endpoints compatible with `omni decide --endpoint` and `LoopbackSidecarServer`.

pub mod egress;
pub mod protocol;
pub mod server;
#[cfg(test)]
mod tests;

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::traits::BoxFuture;
use gap_route::s1_adapter::DecisionBackend;
use gap_route::{GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT, sanitize_default};

use crate::candle::sequence::DEFAULT_HEAD_MAX_LEN;
pub use egress::{
    DefaultSidecarEgressGuard, EgressGuardHook, ParsedEndpoint, resolve_and_validate,
};
pub use protocol::{
    DEFAULT_MAX_RESPONSE_BYTES, HttpResponseFrame, MAX_HTTP_HEADER_BYTES, SystemOneBatchRequest,
    SystemOneBatchResponse, SystemOneRequest, SystemOneResponse, read_http_response,
    serialize_json_value,
};
pub use server::LoopbackSidecarServer;

/// Default TCP connect timeout for sidecar requests (`200 ms`).
pub const DEFAULT_SIDECAR_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// Default total per-request inference timeout (`400 ms`, `RTE-044`).
pub const DEFAULT_SIDECAR_IO_TIMEOUT: Duration = Duration::from_millis(400);

/// Configuration for [`HttpSidecar`].
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Base HTTP endpoint (`http://127.0.0.1:8080` or `http://host:port/v1/systemone`).
    pub endpoint: String,
    /// Stable model identifier included in logs/metrics and calibration version keys.
    pub model_id: String,
    /// Encoder max sequence length (`head_max_len`, default `512` per `RTE-042`).
    pub head_max_len: usize,
    /// Maximum TCP connect timeout (`RTE-044`).
    pub connect_timeout: Duration,
    /// Default HTTP write+read timeout (`RTE-044`).
    pub io_timeout: Duration,
    /// Maximum allowed HTTP response body in bytes (`DEFAULT_MAX_RESPONSE_BYTES`).
    pub max_response_bytes: usize,
    /// Fail-closed egress policy invoked prior to every socket connection (`SEC-*` SSRF guard).
    pub egress_guard: Arc<dyn EgressGuardHook>,
}

impl SidecarConfig {
    /// Builds a strict configuration pointing at `endpoint` (blocks loopback & private IPs unless changed).
    #[must_use]
    pub fn strict(endpoint: impl Into<String>) -> Self {
        let ep = endpoint.into();
        let model_id = format!("http-sidecar:{ep}");
        Self {
            endpoint: ep,
            model_id,
            head_max_len: DEFAULT_HEAD_MAX_LEN,
            connect_timeout: DEFAULT_SIDECAR_CONNECT_TIMEOUT,
            io_timeout: DEFAULT_SIDECAR_IO_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            egress_guard: DefaultSidecarEgressGuard::strict().into_hook(),
        }
    }

    /// Builds a local loopback-permitted configuration (`omni decide --endpoint` / golden tests).
    #[must_use]
    pub fn loopback(endpoint: impl Into<String>) -> Self {
        let mut cfg = Self::strict(endpoint);
        cfg.egress_guard = DefaultSidecarEgressGuard::loopback_allowed().into_hook();
        cfg
    }
}

/// Synchronous-over-`TcpStream` `/v1/systemone` HTTP client implementing [`DecisionBackend`]
/// (`RTE-041`, `RTE-044`).
#[derive(Debug, Clone)]
pub struct HttpSidecar {
    config: SidecarConfig,
    parsed: ParsedEndpoint,
}

impl HttpSidecar {
    /// Constructs a validated [`HttpSidecar`], rejecting invalid endpoints immediately.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if `config.endpoint` violates URL syntax or static host egress rules.
    pub fn new(config: SidecarConfig) -> GapResult<Self> {
        let parsed = ParsedEndpoint::parse(&config.endpoint)?;
        Ok(Self { config, parsed })
    }

    /// Borrows the parsed endpoint components.
    #[must_use]
    pub const fn endpoint(&self) -> &ParsedEndpoint {
        &self.parsed
    }

    /// Executes a single `POST /v1/systemone` request bounded by `timeout` (`RTE-041`, `RTE-044`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on egress denial, connect/read/write timeout, non-200 HTTP status,
    /// response size overflow, or JSON/logit mismatch.
    pub fn score_choice_sync(
        &self,
        state_jcs: &str,
        question: &str,
        options: &[GroupOption],
        timeout: Duration,
    ) -> GapResult<Vec<f32>> {
        if options.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "cannot invoke /v1/systemone with empty options",
            ));
        }

        let payload = build_request(state_jcs, question, options);
        let body = payload.to_json_string();
        let path = self.parsed.request_path("/v1/systemone");
        let frame = self.post_json(&path, &body, timeout)?;
        if frame.status_code != 200 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "/v1/systemone returned HTTP {}: {}",
                    frame.status_code, frame.body
                ),
            ));
        }
        let decoded = SystemOneResponse::parse(&frame.body, options.len())?;
        Ok(decoded.logits)
    }

    /// Executes a batched `POST /v1/systemone/batch` request bounded by `timeout` (`RTE-041`, `RTE-044`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if any item has empty options, egress blocks the endpoint, I/O times
    /// out, or batch result count/logit lengths mismatch.
    pub fn score_choice_batch(
        &self,
        items: &[(&str, &str, &[GroupOption])],
        timeout: Duration,
    ) -> GapResult<Vec<Vec<f32>>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut reqs = Vec::with_capacity(items.len());
        let mut expected_lens = Vec::with_capacity(items.len());
        for &(state_jcs, question, options) in items {
            if options.is_empty() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "cannot include empty options entry in /v1/systemone/batch",
                ));
            }
            expected_lens.push(options.len());
            reqs.push(build_request(state_jcs, question, options));
        }
        let batch_req = SystemOneBatchRequest { requests: &reqs };
        let body = batch_req.to_json_string();
        let path = self.parsed.request_path("/v1/systemone/batch");
        let frame = self.post_json(&path, &body, timeout)?;
        if frame.status_code != 200 {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "/v1/systemone/batch returned HTTP {}: {}",
                    frame.status_code, frame.body
                ),
            ));
        }
        let decoded = SystemOneBatchResponse::parse(&frame.body, &expected_lens)?;
        Ok(decoded.results.into_iter().map(|r| r.logits).collect())
    }

    fn post_json(
        &self,
        path: &str,
        json_body: &str,
        timeout: Duration,
    ) -> GapResult<HttpResponseFrame> {
        let effective_total = timeout.max(Duration::from_millis(1));
        let started = Instant::now();

        let addrs = resolve_and_validate(&self.parsed, self.config.egress_guard.as_ref())?;
        let connect_budget = self
            .config
            .connect_timeout
            .min(effective_total)
            .max(Duration::from_millis(1));
        let mut stream = connect_first(&addrs, connect_budget)?;

        let remaining = effective_total.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "sidecar request timeout exhausted during connect",
            ));
        }

        let _ = stream.set_write_timeout(Some(remaining));
        let _ = stream.set_read_timeout(Some(remaining));

        let http_req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json_body}",
            self.parsed.host_header(),
            json_body.len()
        );
        stream.write_all(http_req.as_bytes()).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar HTTP write timeout/error: {err}"),
            )
        })?;
        stream.flush().map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("sidecar HTTP flush error: {err}"),
            )
        })?;

        read_http_response(&mut stream, self.config.max_response_bytes)
    }
}

impl DecisionBackend for HttpSidecar {
    fn model_id(&self) -> &str {
        &self.config.model_id
    }

    fn head_max_len(&self) -> usize {
        self.config.head_max_len
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        let timeout = self.config.io_timeout;
        self.score_choice_with_timeout(state_jcs, question, options, timeout)
    }

    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        Box::pin(async move { self.score_choice_sync(state_jcs, question, options, timeout) })
    }
}

fn connect_first(addrs: &[SocketAddr], timeout: Duration) -> GapResult<TcpStream> {
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some(err),
        }
    }
    let detail = last_err.map_or_else(|| "no addresses".to_owned(), |e| e.to_string());
    Err(GapError::new(
        ErrorCode::DiscoveryDegraded,
        format!("failed to connect to sidecar endpoint: {detail}"),
    ))
}

fn build_request(state_jcs: &str, question: &str, options: &[GroupOption]) -> SystemOneRequest {
    let clean_opts = options
        .iter()
        .map(|opt| {
            let text = if opt.key == SINK_OPTION_KEY {
                SINK_OPTION_TEXT.to_owned()
            } else {
                sanitize_default(&opt.text)
            };
            GroupOption {
                key: opt.key.clone(),
                text,
                prior: opt.prior,
            }
        })
        .collect();
    SystemOneRequest {
        head: "choice".to_owned(),
        state_jcs: sanitize_default(state_jcs),
        question: sanitize_default(question),
        options: clean_opts,
        temperature: 1.0,
    }
}
