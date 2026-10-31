//! `OpenApiConnector<U>` implementing `gap_ngc_sdk::Connector` (`CMP-030..033`, `NGC-010..027`, `NGC-060..072`, `NGC-091`).

use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, StatusCode};
use futures_util::Stream;
use futures_util::stream::{self, StreamExt};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, HealthRequest,
    HealthResponse, Limits, OrderBy, PaginationKind, Protocol, QueryRequest, QueryResponse,
    RowSetChunk, ServingStatus, query_response,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, sleep_until};

use crate::auth::{AuthConfig, AuthError, CredentialSource, ResolvedAuth, ValidatedBaseUrl};
use crate::document::{ExtractError, ExtractLimits, OpenApiDocument};
use crate::extract::{ExtractedOperation, ExtractionReport, NativeBatchSpec, extract_operations};
use crate::request::{
    BuiltRequest, build_request, json_number_as_exact_i64, validate_arguments,
};
use crate::schema::{GeneratedOperationSchema, build_schema_bundle};
use crate::upstream::{
    DEFAULT_MAX_UPSTREAM_BODY_BYTES, HttpUpstream, Upstream, UpstreamError, UpstreamRequest,
    decode_2xx_payload,
};

/// Default connector identifier reported in `Describe` (`NGC-010`).
pub const DEFAULT_CONNECTOR_ID: &str = "gap-ngc-openapi";

/// Default per-attempt HTTP timeout (`30 s`, `CMP-033`).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on a single `Retry-After` sleep (`5 s`, `CMP-033`).
pub const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Default connector-enforced concurrent in-flight limit (`64`, `NGC-071`).
pub const DEFAULT_MAX_IN_FLIGHT: u32 = 64;

/// Default maximum `Page.limit` advertised in `Limits` (`200`, `NGC-021`).
pub const DEFAULT_MAX_PAGE_SIZE: u32 = 200;

/// Default maximum serialized response row bytes per `Query` call (`4 MiB`, `NGC-027`).
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Default bounded concurrency for emulated `foreach` fan-out (`8`, `CMP-032`).
pub const DEFAULT_FANOUT_CONCURRENCY: usize = 8;

/// Default `Limits.max_batch_size` advertised when all operations use emulated `foreach` (`100`, `CMP-032`).
pub const DEFAULT_EMULATED_BATCH_LIMIT: u32 = 100;

/// Maximum automatic retries on `GET` (`<= 2` retries = at most `3` attempts, `CMP-033`).
const MAX_GET_RETRIES: usize = 2;

/// Default retry delay when `Retry-After` is absent on `429`/`503` or on connect error (`CMP-033`).
const DEFAULT_BACKOFF_DELAY: Duration = Duration::from_millis(50);

/// Fallback `Retry-After` delay when an HTTP-date string is returned instead of delta-seconds (`CMP-033`).
const HTTP_DATE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Minimum positive floor when clamping `X-RateLimit-Remaining / X-RateLimit-Limit` into `(0, 1]` (`NGC-072`).
const MIN_CAPACITY_WEIGHT: f64 = 0.01;

/// `retry_after_ms` returned with `RESOURCE_EXHAUSTED` when `max_in_flight` is full (`NGC-071`).
const SATURATED_RETRY_AFTER_MS: u32 = 100;

/// Current wire version tag inside opaque pagination cursors (`NGC-022`).
const CURSOR_VERSION: u8 = 1;

/// Configuration for building an [`OpenApiConnector`] (`CMP-030..033`).
#[derive(Debug, Clone)]
pub struct OpenApiConfig {
    /// Stable connector identifier advertised in `Describe` (`NGC-010`).
    pub connector_id: String,
    /// Connector semantic version advertised in `Describe`.
    pub connector_version: String,
    /// Locked upstream HTTP origin and optional base path prefix (`CMP-030`, `CMP-031`).
    pub base_url: String,
    /// Declarative authentication configuration resolved via [`CredentialSource`] (`CMP-031`).
    pub auth: AuthConfig,
    /// Explicit override permitting cleartext `http://` credentials to non-loopback hosts (`CMP-031`).
    pub allow_insecure_upstream: bool,
    /// Wrap non-JSON `2xx` bodies as `{"raw": "<base64>"}` instead of failing `UPSTREAM_ERROR` (`CMP-033`).
    pub allow_binary: bool,
    /// Structural limits enforced while parsing and resolving the OpenAPI document (`ING-012`).
    pub extract_limits: ExtractLimits,
    /// Per-attempt HTTP request timeout (`CMP-033`).
    pub request_timeout: Duration,
    /// Maximum sleep duration for a single `Retry-After` backoff (`CMP-033`).
    pub max_retry_delay: Duration,
    /// Connector-enforced concurrent in-flight RPC cap (`>= 1`, `NGC-071`).
    pub max_in_flight: u32,
    /// Maximum `Page.limit` advertised in `Describe.limits` (`>= 1`, `NGC-021`).
    pub max_page_size: u32,
    /// Maximum prost-encoded row bytes emitted before truncating (`NGC-027`).
    pub max_response_bytes: u64,
    /// Hard raw HTTP body byte cap enforced before JSON decoding (`CMP-033`).
    pub max_upstream_body_bytes: usize,
    /// Maximum concurrent upstream requests during `foreach` fan-out (`>= 1`, `CMP-032`).
    pub fanout_concurrency: usize,
    /// `Describe.limits.max_batch_size` when no operation declares `x-gap-in-param` (`>= 1`, `CMP-032`).
    pub emulated_batch_limit: u32,
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self {
            connector_id: DEFAULT_CONNECTOR_ID.to_owned(),
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
            base_url: "http://127.0.0.1:8080".to_owned(),
            auth: AuthConfig::None,
            allow_insecure_upstream: false,
            allow_binary: false,
            extract_limits: ExtractLimits::default(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_retry_delay: DEFAULT_MAX_RETRY_DELAY,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            max_page_size: DEFAULT_MAX_PAGE_SIZE,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_upstream_body_bytes: DEFAULT_MAX_UPSTREAM_BODY_BYTES,
            fanout_concurrency: DEFAULT_FANOUT_CONCURRENCY,
            emulated_batch_limit: DEFAULT_EMULATED_BATCH_LIMIT,
        }
    }
}

/// Startup failure returned when constructing an [`OpenApiConnector`].
#[derive(Debug, Error)]
pub enum ConnectorBuildError {
    /// `OpenApiConfig::connector_id` was empty or whitespace-only.
    #[error("connector_id must not be empty")]
    EmptyConnectorId,
    /// A positive capacity/concurrency limit in [`OpenApiConfig`] was set to `0`.
    #[error("invalid connector configuration: `{0}` must be >= 1")]
    InvalidLimit(&'static str),
    /// OpenAPI 3.x document parsing, `$ref` resolution, or schema generation failed.
    #[error(transparent)]
    Extract(#[from] ExtractError),
    /// `base_url` validation or credential resolution failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// Building the underlying HTTP client failed.
    #[error(transparent)]
    Transport(#[from] UpstreamError),
}

#[derive(Clone)]
struct RegisteredOperation {
    extracted: ExtractedOperation,
    schema: GeneratedOperationSchema,
}

struct ConnectorShared<U> {
    config: OpenApiConfig,
    base_url: ValidatedBaseUrl,
    auth: ResolvedAuth,
    report: ExtractionReport,
    description: ConnectorDescription,
    operations: BTreeMap<String, RegisteredOperation>,
    by_path: BTreeMap<String, String>,
    in_flight_gate: Arc<Semaphore>,
    capacity_weight_bits: AtomicU64,
    upstream: U,
}

/// RAII stream wrapper holding a connector `max_in_flight` semaphore permit until completion or drop (`NGC-071`, `NGC-090`).
struct GuardedQueryStream {
    inner: BoxStream<QueryResponse>,
    _permit: OwnedSemaphorePermit,
}

impl Stream for GuardedQueryStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CursorToken {
    v: u8,
    op: String,
    args_hash: String,
    tenant: String,
    offset: u64,
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || hex.is_empty() {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut idx = 0;
    while idx < bytes.len() {
        let hi = decode_hex_nibble(bytes[idx])?;
        let lo = decode_hex_nibble(bytes[idx + 1])?;
        out.push((hi << 4) | lo);
        idx += 2;
    }
    Some(out)
}

fn canonicalize_json_for_hash(val: &Value) -> Value {
    match val {
        Value::Null | Value::Bool(_) | Value::String(_) => val.clone(),
        Value::Number(num) => {
            if let Some(i) = json_number_as_exact_i64(val) {
                json!(i)
            } else if let Some(f) = num.as_f64() {
                let norm = if f == 0.0 { 0.0 } else { f };
                serde_json::Number::from_f64(norm).map_or(Value::Null, Value::Number)
            } else {
                val.clone()
            }
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_json_for_hash).collect()),
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize_json_for_hash(v)))
                .collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
    }
}

fn canonical_args_and_order_hash(args: &Map<String, Value>, order_by: &[OrderBy]) -> String {
    let canon_args = canonicalize_json_for_hash(&Value::Object(args.clone()));
    let canon_order: Vec<Value> = order_by
        .iter()
        .map(|o| json!({ "path": o.path, "descending": o.descending }))
        .collect();
    let payload = json!({
        "args": canon_args,
        "order_by": canon_order,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

fn encode_cursor(op: &str, args_hash: &str, tenant: &str, offset: u64) -> String {
    let token = CursorToken {
        v: CURSOR_VERSION,
        op: op.to_owned(),
        args_hash: args_hash.to_owned(),
        tenant: tenant.to_owned(),
        offset,
    };
    let json_bytes = serde_json::to_vec(&token).unwrap_or_default();
    encode_hex(&json_bytes)
}

fn decode_and_verify_cursor(
    raw_cursor: &str,
    expected_op: &str,
    expected_args_hash: &str,
    expected_tenant: &str,
) -> NgcResult<u64> {
    let bytes = decode_hex(raw_cursor).ok_or_else(|| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "invalid pagination cursor encoding",
        )
    })?;
    let token: CursorToken = serde_json::from_slice(&bytes).map_err(|_| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "malformed pagination cursor payload",
        )
    })?;
    if token.v != CURSOR_VERSION
        || token.op != expected_op
        || token.args_hash != expected_args_hash
        || token.tenant != expected_tenant
    {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "pagination cursor does not match operation, arguments, or tenant",
        ));
    }
    Ok(token.offset)
}

/// Exact protobuf varint byte length of a non-negative `u64`.
const fn varint_len_u64(mut v: u64) -> usize {
    let mut len = 1;
    while v >= 0x80 {
        v >>= 7;
        len += 1;
    }
    len
}

fn varint_len_usize(v: usize) -> usize {
    varint_len_u64(u64::try_from(v).unwrap_or(u64::MAX))
}

fn encoded_prost_value_len(val: &prost_types::Value) -> usize {
    use prost_types::value::Kind;
    match &val.kind {
        None => 0,
        Some(Kind::NullValue(v)) => {
            #[allow(clippy::cast_sign_loss)]
            let bits = *v as i64 as u64;
            1 + varint_len_u64(bits)
        }
        Some(Kind::NumberValue(_)) => 1 + 8,
        Some(Kind::StringValue(s)) => 1 + varint_len_usize(s.len()) + s.len(),
        Some(Kind::BoolValue(_)) => 1 + 1,
        Some(Kind::StructValue(st)) => {
            let inner = encoded_prost_struct_len(st);
            1 + varint_len_usize(inner) + inner
        }
        Some(Kind::ListValue(list)) => {
            let inner: usize = list
                .values
                .iter()
                .map(|elem| {
                    let elem_len = encoded_prost_value_len(elem);
                    1 + varint_len_usize(elem_len) + elem_len
                })
                .sum();
            1 + varint_len_usize(inner) + inner
        }
    }
}

/// Exact prost wire encoded byte length of a [`prost_types::Struct`] (`NGC-027`).
fn encoded_prost_struct_len(st: &prost_types::Struct) -> usize {
    st.fields
        .iter()
        .map(|(k, v)| {
            let key_len = if k.is_empty() {
                0
            } else {
                1 + varint_len_usize(k.len()) + k.len()
            };
            let v_inner = encoded_prost_value_len(v);
            let val_len = 1 + varint_len_usize(v_inner) + v_inner;
            let entry_len = key_len + val_len;
            1 + varint_len_usize(entry_len) + entry_len
        })
        .sum()
}

fn json_payload_to_rows(payload: Value) -> Vec<prost_types::Struct> {
    match payload {
        Value::Null => Vec::new(),
        Value::Object(map) => vec![json_map_to_prost_struct(&map)],
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::Object(map) => json_map_to_prost_struct(&map),
                other => {
                    let mut wrapper = Map::with_capacity(1);
                    wrapper.insert("value".to_owned(), other);
                    json_map_to_prost_struct(&wrapper)
                }
            })
            .collect(),
        other => {
            let mut wrapper = Map::with_capacity(1);
            wrapper.insert("value".to_owned(), other);
            vec![json_map_to_prost_struct(&wrapper)]
        }
    }
}

fn json_key_matches(row_field: &Value, key: &Value) -> bool {
    if let (Some(ia), Some(ib)) = (
        json_number_as_exact_i64(row_field),
        json_number_as_exact_i64(key),
    ) {
        return ia == ib;
    }
    match (row_field.as_f64(), key.as_f64()) {
        (Some(fa), Some(fb)) if row_field.is_number() && key.is_number() => (fa - fb).abs() == 0.0,
        _ => row_field == key,
    }
}

fn parse_retry_after_delay(headers: &HeaderMap, max_retry_delay: Duration) -> (Duration, u32) {
    let raw_delay = match headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => DEFAULT_BACKOFF_DELAY,
        Some(raw) => match raw.parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => HTTP_DATE_RETRY_DELAY,
        },
    };
    let capped = raw_delay.min(max_retry_delay);
    let ms = u32::try_from(capped.as_millis()).unwrap_or(u32::MAX);
    (capped, ms)
}

impl<U: Upstream> ConnectorShared<U> {
    fn check_scope_alive(scope: &CallScope) -> NgcResult<Instant> {
        if scope.cancel.is_cancelled() {
            return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
        }
        let now = Instant::now();
        if now >= scope.deadline {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "call deadline exceeded",
            ));
        }
        Ok(now)
    }

    fn update_capacity_weight(&self, headers: &HeaderMap, request_id: &str) {
        let limit_opt = headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        let rem_opt = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);

        let weight = match (limit_opt, rem_opt) {
            (Some(lim_str), Some(rem_str)) => {
                match (lim_str.parse::<f64>(), rem_str.parse::<f64>()) {
                    (Ok(lim), Ok(rem))
                        if lim.is_finite() && rem.is_finite() && lim > 0.0 && rem >= 0.0 =>
                    {
                        (rem / lim).clamp(MIN_CAPACITY_WEIGHT, 1.0)
                    }
                    _ => {
                        tracing::warn!(
                            request_id = request_id,
                            "ignoring invalid or out-of-range upstream rate-limit headers; defaulting capacity_weight to 1.0"
                        );
                        1.0
                    }
                }
            }
            (None, None) => 1.0,
            _ => {
                tracing::warn!(
                    request_id = request_id,
                    "ignoring incomplete upstream rate-limit header pair; defaulting capacity_weight to 1.0"
                );
                1.0
            }
        };
        self.capacity_weight_bits
            .store(weight.to_bits(), Ordering::Release);
    }

    fn current_capacity_weight(&self) -> f64 {
        let w = f64::from_bits(self.capacity_weight_bits.load(Ordering::Acquire));
        if w.is_finite() && w > 0.0 && w <= 1.0 {
            w
        } else {
            1.0
        }
    }

    fn attach_auth(&self, mut built: BuiltRequest) -> NgcResult<BuiltRequest> {
        self.auth.apply(&mut built).map_err(|_| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                "failed to attach connector credentials",
            )
        })?;
        // Verify that joining onto the locked `base_url` leaves scheme, host, and port untouched.
        let _ = self
            .base_url
            .join_relative(&built.path_and_query)
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "built request path violated upstream origin boundary",
                )
            })?;
        Ok(built)
    }

    async fn send_with_retry(
        &self,
        built: BuiltRequest,
        is_keyed_read: bool,
        scope: &CallScope,
        attempts: &AtomicU32,
    ) -> NgcResult<Option<Value>> {
        let authed = self.attach_auth(built)?;

        for attempt in 0..=MAX_GET_RETRIES {
            let now = Self::check_scope_alive(scope)?;
            let remaining = scope.deadline.duration_since(now);
            let timeout = self.config.request_timeout.min(remaining);

            attempts.fetch_add(1, Ordering::AcqRel);
            let up_req = UpstreamRequest {
                path_and_query: authed.path_and_query.clone(),
                headers: authed.headers.clone(),
                timeout,
            };

            let send_outcome = tokio::select! {
                biased;
                () = scope.cancel.cancelled() => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
                }
                () = sleep_until(scope.deadline) => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "call deadline exceeded"));
                }
                res = self.upstream.send(up_req) => res,
            };

            match send_outcome {
                Err(UpstreamError::Timeout) => {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::DeadlineExceeded,
                        "upstream request timed out",
                    ));
                }
                Err(UpstreamError::Connect) => {
                    let delay = DEFAULT_BACKOFF_DELAY.min(self.config.max_retry_delay);
                    let retry_ms = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
                    if attempt < MAX_GET_RETRIES {
                        if Instant::now() + delay >= scope.deadline {
                            return Err(NgcErrorInfo::new(
                                NgcErrorCode::Unavailable,
                                "upstream connection failed",
                            )
                            .with_retry(true, retry_ms));
                        }
                        tokio::select! {
                            biased;
                            () = scope.cancel.cancelled() => {
                                return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
                            }
                            () = sleep_until(scope.deadline) => {
                                return Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "call deadline exceeded"));
                            }
                            () = tokio::time::sleep(delay) => {}
                        }
                        continue;
                    }
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::Unavailable,
                        "upstream connection failed",
                    )
                    .with_retry(true, retry_ms));
                }
                Err(UpstreamError::BodyTooLarge { max_bytes }) => {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::UpstreamError,
                        format!("upstream response body exceeds cap of {max_bytes} bytes"),
                    ));
                }
                Err(UpstreamError::Transport) => {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::UpstreamError,
                        "upstream HTTP transport error",
                    ));
                }
                Ok(resp) => {
                    self.update_capacity_weight(&resp.headers, &scope.request_id);
                    let status = resp.status;

                    if status.is_success() {
                        let json_val = decode_2xx_payload(
                            &resp.headers,
                            &resp.body,
                            self.config.max_upstream_body_bytes,
                            self.config.allow_binary,
                        )
                        .map_err(|e| NgcErrorInfo::new(NgcErrorCode::UpstreamError, e.to_string()))?;
                        return Ok(Some(json_val));
                    }

                    if status == StatusCode::NOT_FOUND {
                        if is_keyed_read {
                            return Ok(None);
                        }
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::UpstreamError,
                            "upstream endpoint returned 404",
                        ));
                    }

                    if status == StatusCode::BAD_REQUEST
                        || status == StatusCode::UNPROCESSABLE_ENTITY
                    {
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            "upstream rejected request arguments",
                        ));
                    }

                    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::UpstreamError,
                            "upstream rejected connector credentials",
                        ));
                    }

                    if status == StatusCode::TOO_MANY_REQUESTS
                        || status == StatusCode::SERVICE_UNAVAILABLE
                    {
                        let (delay, retry_ms) =
                            parse_retry_after_delay(&resp.headers, self.config.max_retry_delay);
                        if attempt < MAX_GET_RETRIES {
                            if Instant::now() + delay >= scope.deadline {
                                return Err(NgcErrorInfo::new(
                                    NgcErrorCode::Unavailable,
                                    "upstream service unavailable or rate limited",
                                )
                                .with_retry(true, retry_ms));
                            }
                            tokio::select! {
                                biased;
                                () = scope.cancel.cancelled() => {
                                    return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
                                }
                                () = sleep_until(scope.deadline) => {
                                    return Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "call deadline exceeded"));
                                }
                                () = tokio::time::sleep(delay) => {}
                            }
                            continue;
                        }
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::Unavailable,
                            "upstream service unavailable or rate limited",
                        )
                        .with_retry(true, retry_ms));
                    }

                    if status.is_redirection() {
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::UpstreamError,
                            "upstream returned redirect response (redirects are disabled)",
                        ));
                    }

                    if status.is_server_error() {
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::UpstreamError,
                            "upstream returned server error",
                        )
                        .with_retry(true, 0));
                    }

                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::UpstreamError,
                        "upstream request failed",
                    ));
                }
            }
        }

        Err(NgcErrorInfo::new(
            NgcErrorCode::Unavailable,
            "upstream retries exhausted",
        )
        .with_retry(true, 0))
    }
}

/// Zero-ETL OpenAPI 3.x Native Graph Connector generic over `U: Upstream` (`CMP-030..033`, `NGC-090`).
pub struct OpenApiConnector<U> {
    shared: Arc<ConnectorShared<U>>,
}

impl<U> Clone for OpenApiConnector<U> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<U> fmt::Debug for OpenApiConnector<U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenApiConnector")
            .field("connector_id", &self.shared.config.connector_id)
            .field("base_url", &self.shared.base_url)
            .field("auth", &self.shared.auth)
            .field("spec_hash", &self.shared.description.spec_hash)
            .field("operations", &self.shared.operations.len())
            .finish_non_exhaustive()
    }
}

impl<U: Upstream> OpenApiConnector<U> {
    /// Parses `doc_bytes`, validates `config` and `credentials`, and builds an [`OpenApiConnector`] over `upstream`.
    pub fn from_document_bytes(
        doc_bytes: &[u8],
        config: OpenApiConfig,
        credentials: &dyn CredentialSource,
        upstream: U,
    ) -> Result<Self, ConnectorBuildError> {
        if config.connector_id.trim().is_empty() {
            return Err(ConnectorBuildError::EmptyConnectorId);
        }
        if config.max_in_flight == 0 {
            return Err(ConnectorBuildError::InvalidLimit("max_in_flight"));
        }
        if config.max_page_size == 0 {
            return Err(ConnectorBuildError::InvalidLimit("max_page_size"));
        }
        if config.fanout_concurrency == 0 {
            return Err(ConnectorBuildError::InvalidLimit("fanout_concurrency"));
        }
        if config.emulated_batch_limit == 0 {
            return Err(ConnectorBuildError::InvalidLimit("emulated_batch_limit"));
        }

        // Scheme, host, and port come ONLY from `config.base_url` — `servers[]` in the OpenAPI document is never followed.
        let base_url = ValidatedBaseUrl::parse(
            &config.base_url,
            config.auth.has_credentials(),
            config.allow_insecure_upstream,
        )?;
        let auth = ResolvedAuth::resolve(&config.auth, credentials)?;

        let doc = OpenApiDocument::parse_with_limits(doc_bytes, config.extract_limits)?;
        let extraction = extract_operations(&doc)?;
        let bundle = build_schema_bundle(&doc, &extraction)?;

        let mut operations = BTreeMap::new();
        let mut by_path = BTreeMap::new();
        let mut native_batch_min: Option<u32> = None;
        let mut any_sort = false;
        let mut any_pagination = false;

        for op in extraction.operations {
            if let Some(ref nb) = op.native_batch {
                native_batch_min = Some(match native_batch_min {
                    Some(cur) => cur.min(nb.max_batch_size),
                    None => nb.max_batch_size,
                });
            }
            if op.has_sort {
                any_sort = true;
            }
            if op.has_offset_pagination {
                any_pagination = true;
            }
            let Some(gen_schema) = bundle.operations.get(&op.name).cloned() else {
                return Err(ConnectorBuildError::Extract(
                    ExtractError::InvalidStructure(format!(
                        "missing generated schema for operation `{}`",
                        op.name
                    )),
                ));
            };
            by_path.insert(op.path_template.clone(), op.name.clone());
            operations.insert(
                op.name.clone(),
                RegisteredOperation {
                    extracted: op,
                    schema: gen_schema,
                },
            );
        }

        let has_native_batch = native_batch_min.is_some();
        let max_batch_size = if operations.is_empty() {
            0
        } else {
            native_batch_min.unwrap_or(config.emulated_batch_limit)
        };

        let description = ConnectorDescription {
            connector_id: config.connector_id.clone(),
            connector_version: config.connector_version.clone(),
            ngc_version: NGC_WIRE_VERSION.to_owned(),
            protocol: Protocol::Rest as i32,
            spec_hash: bundle.spec_hash,
            capabilities: Some(Capabilities {
                foreach_batch: has_native_batch,
                predicate_ops: Vec::new(),
                order_by: any_sort,
                pagination: if any_pagination {
                    PaginationKind::PaginationCursor as i32
                } else {
                    PaginationKind::PaginationNone as i32
                },
                native_traverse: false,
                subscribe: false,
                tasks: false,
                dry_run: false,
                idempotency: false,
                sketch: false,
                explain: false,
                as_of: false,
            }),
            limits: Some(Limits {
                max_batch_size,
                max_in_flight: config.max_in_flight,
                max_page_size: config.max_page_size,
                default_timeout_ms: u32::try_from(config.request_timeout.as_millis())
                    .unwrap_or(u32::MAX),
                max_response_bytes: config.max_response_bytes,
            }),
            schema_sdl: bundle.schema_sdl,
            operations: bundle.descriptors,
            meta: None,
            auth: None,
        };

        let in_flight_permits = usize::try_from(config.max_in_flight).unwrap_or(usize::MAX);
        Ok(Self {
            shared: Arc::new(ConnectorShared {
                config,
                base_url,
                auth,
                report: extraction.report,
                description,
                operations,
                by_path,
                in_flight_gate: Arc::new(Semaphore::new(in_flight_permits)),
                capacity_weight_bits: AtomicU64::new(1.0_f64.to_bits()),
                upstream,
            }),
        })
    }

    /// Borrows the validated, locked upstream base URL (`CMP-030`).
    #[must_use]
    pub fn base_url(&self) -> &ValidatedBaseUrl {
        &self.shared.base_url
    }

    /// Borrows the deterministic extraction report (`servers[]` and `skipped` operations).
    #[must_use]
    pub fn extraction_report(&self) -> &ExtractionReport {
        &self.shared.report
    }

    /// Borrows the precomputed [`ConnectorDescription`].
    #[must_use]
    pub fn description(&self) -> &ConnectorDescription {
        &self.shared.description
    }

    fn acquire_permit(&self) -> NgcResult<OwnedSemaphorePermit> {
        Arc::clone(&self.shared.in_flight_gate)
            .try_acquire_owned()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::ResourceExhausted,
                    "connector max_in_flight capacity exhausted",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })
    }

    fn execute_foreach(
        &self,
        reg: &RegisteredOperation,
        base_args: Map<String, Value>,
        request: QueryRequest,
        scope: &CallScope,
        permit: OwnedSemaphorePermit,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        if request.page.is_some() {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "pagination cannot be combined with foreach",
            ));
        }
        if !request.order_by.is_empty() {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "order_by cannot be combined with foreach",
            ));
        }

        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_complete = Arc::clone(&attempts);
        let shared = Arc::clone(&self.shared);
        let scope_owned = scope.clone();

        let foreach_run = if let Some(nb) = reg.extracted.native_batch.clone() {
            self.start_native_foreach(
                reg,
                nb,
                base_args,
                request.foreach,
                scope,
                scope_owned,
                shared,
                attempts,
            )?
        } else {
            self.start_emulated_foreach(
                reg,
                base_args,
                request.foreach,
                scope,
                scope_owned,
                shared,
                attempts,
            )?
        };

        let (chunk_stream, _sdk_calls) = foreach_run.into_parts();
        struct ForeachStreamState {
            chunks: BoxStream<RowSetChunk>,
            attempts: Arc<AtomicU32>,
            rows_total: u64,
            done: bool,
        }

        let inner: BoxStream<QueryResponse> = Box::pin(stream::unfold(
            ForeachStreamState {
                chunks: chunk_stream,
                attempts: attempts_for_complete,
                rows_total: 0,
                done: false,
            },
            |mut st| async move {
                if st.done {
                    return None;
                }
                match st.chunks.next().await {
                    Some(Ok(chunk)) => {
                        st.rows_total = st.rows_total.saturating_add(chunk.rows.len() as u64);
                        Some((
                            Ok(QueryResponse {
                                chunk: Some(query_response::Chunk::Rows(chunk)),
                            }),
                            st,
                        ))
                    }
                    Some(Err(err)) => {
                        st.done = true;
                        Some((Err(err), st))
                    }
                    None => {
                        st.done = true;
                        Some((
                            Ok(QueryResponse {
                                chunk: Some(query_response::Chunk::Complete(Complete {
                                    next_cursor: String::new(),
                                    truncated: false,
                                    rows_total: st.rows_total,
                                    upstream_calls: st.attempts.load(Ordering::Acquire),
                                    latency_us: 0,
                                })),
                            }),
                            st,
                        ))
                    }
                }
            },
        ));

        Ok(Box::pin(GuardedQueryStream {
            inner,
            _permit: permit,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn start_native_foreach(
        &self,
        reg: &RegisteredOperation,
        nb: NativeBatchSpec,
        base_args: Map<String, Value>,
        sets: Vec<gap_proto::ngc::v1::VariableSet>,
        scope: &CallScope,
        scope_owned: CallScope,
        shared: Arc<ConnectorShared<U>>,
        attempts: Arc<AtomicU32>,
    ) -> NgcResult<gap_ngc_sdk::foreach::ForeachRun> {
        let Some(target_name) = self.shared.by_path.get(&nb.path) else {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::Internal,
                "native batch target operation missing from connector registry",
            ));
        };
        let Some(target_reg) = self.shared.operations.get(target_name).cloned() else {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::Internal,
                "native batch target schema missing from connector registry",
            ));
        };
        let Some(key_param) = reg.extracted.key_fields.first().cloned() else {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::Internal,
                "native batch operation lacks key parameter",
            ));
        };

        let batch_cap = usize::try_from(nb.max_batch_size).unwrap_or(1).max(1);
        let cfg = ForeachConfig::new(batch_cap, self.shared.config.fanout_concurrency.max(1));
        let item_input_schema = reg.schema.input_json_schema.clone();

        foreach(sets, cfg, scope, move |batch_structs| {
            let shared = Arc::clone(&shared);
            let target_reg = target_reg.clone();
            let base_args = base_args.clone();
            let item_input_schema = item_input_schema.clone();
            let key_param = key_param.clone();
            let nb = nb.clone();
            let scope_owned = scope_owned.clone();
            let attempts = Arc::clone(&attempts);

            async move {
                let mut elem_outcomes: Vec<Option<NgcResult<Vec<prost_types::Struct>>>> =
                    vec![None; batch_structs.len()];
                let mut valid_positions: Vec<(usize, Value)> =
                    Vec::with_capacity(batch_structs.len());

                for (pos, st) in batch_structs.iter().enumerate() {
                    let mut merged = base_args.clone();
                    for (k, v) in prost_struct_to_json_map(st) {
                        merged.insert(k, v);
                    }
                    if let Err(err) = validate_arguments(&item_input_schema, &merged) {
                        elem_outcomes[pos] = Some(Err(err.into_ngc_error()));
                        continue;
                    }
                    let Some(key_val) = merged.get(&key_param).cloned() else {
                        elem_outcomes[pos] = Some(Err(NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            "missing required key parameter",
                        )));
                        continue;
                    };
                    valid_positions.push((pos, key_val));
                }

                if !valid_positions.is_empty() {
                    let mut batch_args = Map::new();
                    for param in &target_reg.extracted.parameters {
                        if param.name == nb.param {
                            continue;
                        }
                        if let Some(shared_val) = base_args.get(&param.name) {
                            batch_args.insert(param.name.clone(), shared_val.clone());
                        }
                    }
                    let keys_array: Vec<Value> =
                        valid_positions.iter().map(|(_, k)| k.clone()).collect();
                    batch_args.insert(nb.param.clone(), Value::Array(keys_array));

                    let built = build_request(
                        &target_reg.extracted,
                        &target_reg.schema.input_json_schema,
                        &batch_args,
                    )
                    .map_err(|e| e.into_ngc_error())?;

                    let payload_opt = shared
                        .send_with_retry(built, true, &scope_owned, &attempts)
                        .await?;

                    let raw_objects: Vec<Map<String, Value>> = match payload_opt {
                        None | Some(Value::Null) => Vec::new(),
                        Some(Value::Array(items)) => items
                            .into_iter()
                            .filter_map(|v| match v {
                                Value::Object(m) => Some(m),
                                _ => None,
                            })
                            .collect(),
                        Some(Value::Object(m)) => vec![m],
                        Some(_) => Vec::new(),
                    };

                    for (pos, key_val) in valid_positions {
                        let matched: Vec<prost_types::Struct> = raw_objects
                            .iter()
                            .filter(|row| {
                                row.get(&nb.key_field)
                                    .is_some_and(|field_val| json_key_matches(field_val, &key_val))
                            })
                            .map(json_map_to_prost_struct)
                            .collect();
                        elem_outcomes[pos] = Some(Ok(matched));
                    }
                }

                let finalized: Vec<NgcResult<Vec<prost_types::Struct>>> = elem_outcomes
                    .into_iter()
                    .map(|opt| {
                        opt.unwrap_or_else(|| {
                            Err(NgcErrorInfo::new(
                                NgcErrorCode::Internal,
                                "unpopulated foreach batch slot",
                            ))
                        })
                    })
                    .collect();
                Ok(finalized)
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn start_emulated_foreach(
        &self,
        reg: &RegisteredOperation,
        base_args: Map<String, Value>,
        sets: Vec<gap_proto::ngc::v1::VariableSet>,
        scope: &CallScope,
        scope_owned: CallScope,
        shared: Arc<ConnectorShared<U>>,
        attempts: Arc<AtomicU32>,
    ) -> NgcResult<gap_ngc_sdk::foreach::ForeachRun> {
        let cfg = ForeachConfig::new(1, self.shared.config.fanout_concurrency.max(1));
        let op_clone = reg.clone();
        let is_keyed = !op_clone.extracted.key_fields.is_empty();

        foreach(sets, cfg, scope, move |batch_structs| {
            let shared = Arc::clone(&shared);
            let op_clone = op_clone.clone();
            let base_args = base_args.clone();
            let scope_owned = scope_owned.clone();
            let attempts = Arc::clone(&attempts);

            async move {
                let mut results = Vec::with_capacity(batch_structs.len());
                for st in &batch_structs {
                    let mut merged = base_args.clone();
                    for (k, v) in prost_struct_to_json_map(st) {
                        merged.insert(k, v);
                    }
                    let built = match build_request(
                        &op_clone.extracted,
                        &op_clone.schema.input_json_schema,
                        &merged,
                    ) {
                        Ok(b) => b,
                        Err(err) => {
                            results.push(Err(err.into_ngc_error()));
                            continue;
                        }
                    };
                    match shared
                        .send_with_retry(built, is_keyed, &scope_owned, &attempts)
                        .await
                    {
                        Ok(None) => results.push(Ok(Vec::new())),
                        Ok(Some(payload)) => results.push(Ok(json_payload_to_rows(payload))),
                        Err(err) => {
                            if matches!(
                                err.code,
                                NgcErrorCode::Cancelled | NgcErrorCode::DeadlineExceeded
                            ) {
                                return Err(err);
                            }
                            results.push(Err(err));
                        }
                    }
                }
                Ok(results)
            }
        })
    }
}

impl OpenApiConnector<HttpUpstream> {
    /// Builds an [`OpenApiConnector`] backed by [`HttpUpstream`] (`reqwest`, redirects off, no TLS).
    pub fn new_http(
        doc_bytes: &[u8],
        config: OpenApiConfig,
        credentials: &dyn CredentialSource,
    ) -> Result<Self, ConnectorBuildError> {
        let base_url = ValidatedBaseUrl::parse(
            &config.base_url,
            config.auth.has_credentials(),
            config.allow_insecure_upstream,
        )?;
        let upstream = HttpUpstream::new(
            base_url,
            config.max_upstream_body_bytes,
            config.allow_binary,
        )?;
        Self::from_document_bytes(doc_bytes, config, credentials, upstream)
    }
}

impl<U: Upstream> Connector for OpenApiConnector<U> {
    async fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        let _ = ConnectorShared::<U>::check_scope_alive(&scope)?;
        if !request.known_spec_hash.is_empty()
            && request.known_spec_hash == self.shared.description.spec_hash
        {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }
        Ok(DescribeResponse {
            not_modified: false,
            description: Some(self.shared.description.clone()),
        })
    }

    async fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        let _ = ConnectorShared::<U>::check_scope_alive(&scope)?;

        let Some(reg) = self.shared.operations.get(&request.operation).cloned() else {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::NotFound,
                "unknown query operation",
            ));
        };

        if request.predicate.is_some() {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "predicate pushdown is not supported by this connector",
            ));
        }

        if !request.as_of.is_empty() {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "as_of time travel is not supported by this connector",
            ));
        }

        let user_args = request
            .arguments
            .as_ref()
            .map_or_else(Map::new, prost_struct_to_json_map);

        let permit = self.acquire_permit()?;

        if !request.foreach.is_empty() {
            return self.execute_foreach(&reg, user_args, request, &scope, permit);
        }

        // Validate base user arguments first before injecting pagination/sort query parameters.
        validate_arguments(&reg.schema.input_json_schema, &user_args)
            .map_err(crate::request::RequestBuildError::into_ngc_error)?;

        let mut effective_args = user_args.clone();

        if !request.order_by.is_empty() {
            if !reg.extracted.has_sort {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "operation does not support order_by",
                ));
            }
            if request.order_by.len() > 1 {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "only single-column order_by is supported",
                ));
            }
            let ob = &request.order_by[0];
            if ob.path.trim().is_empty() {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "order_by path must not be empty",
                ));
            }
            let sort_expr = if ob.descending {
                format!("-{}", ob.path)
            } else {
                ob.path.clone()
            };
            effective_args.insert("sort".to_owned(), Value::String(sort_expr));
        }

        let args_hash = canonical_args_and_order_hash(&user_args, &request.order_by);
        let mut start_offset = 0_u64;
        let mut requested_page_limit: Option<u32> = None;

        if let Some(ref page) = request.page {
            let has_paging_fields = !page.cursor.is_empty() || page.limit > 0 || page.offset > 0;
            if has_paging_fields && !reg.extracted.has_offset_pagination {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "operation does not support pagination",
                ));
            }
            if !page.cursor.is_empty() {
                start_offset = decode_and_verify_cursor(
                    &page.cursor,
                    &reg.extracted.name,
                    &args_hash,
                    &scope.tenant_id,
                )?;
            } else if page.offset > 0 {
                start_offset = u64::from(page.offset);
            }
            if page.limit > 0 {
                requested_page_limit = Some(page.limit.min(self.shared.config.max_page_size));
            }
        }

        if reg.extracted.has_offset_pagination {
            if let Some(lim) = requested_page_limit {
                // Request `limit + 1` rows from upstream so exact boundaries emit `next_cursor = ""` without a trailing empty page.
                let lookahead = u64::from(lim).saturating_add(1);
                effective_args.insert("limit".to_owned(), json!(lookahead));
                effective_args.insert("offset".to_owned(), json!(start_offset));
            } else if start_offset > 0 {
                effective_args.insert("offset".to_owned(), json!(start_offset));
            }
        }

        let built = build_request(
            &reg.extracted,
            &reg.schema.input_json_schema,
            &effective_args,
        )
        .map_err(crate::request::RequestBuildError::into_ngc_error)?;

        let shared = Arc::clone(&self.shared);
        let is_keyed = !reg.extracted.key_fields.is_empty();
        let is_paginated = reg.extracted.has_offset_pagination;
        let op_name = reg.extracted.name.clone();
        let max_response_bytes = self.shared.config.max_response_bytes;

        struct SoloState<U> {
            shared: Arc<ConnectorShared<U>>,
            built: Option<BuiltRequest>,
            pending_complete: Option<Complete>,
            scope: CallScope,
            is_keyed: bool,
            is_paginated: bool,
            op_name: String,
            args_hash: String,
            start_offset: u64,
            requested_page_limit: Option<u32>,
            max_response_bytes: u64,
            done: bool,
        }

        let init = SoloState {
            shared,
            built: Some(built),
            pending_complete: None,
            scope,
            is_keyed,
            is_paginated,
            op_name,
            args_hash,
            start_offset,
            requested_page_limit,
            max_response_bytes,
            done: false,
        };

        let inner: BoxStream<QueryResponse> = Box::pin(stream::unfold(init, |mut st| async move {
            if st.done {
                return None;
            }
            if let Some(complete) = st.pending_complete.take() {
                st.done = true;
                return Some((
                    Ok(QueryResponse {
                        chunk: Some(query_response::Chunk::Complete(complete)),
                    }),
                    st,
                ));
            }

            let Some(built_req) = st.built.take() else {
                return None;
            };

            let attempts = AtomicU32::new(0);
            let started = Instant::now();
            match st
                .shared
                .send_with_retry(built_req, st.is_keyed, &st.scope, &attempts)
                .await
            {
                Err(err) => {
                    st.done = true;
                    Some((Err(err), st))
                }
                Ok(payload_opt) => {
                    let mut rows = match payload_opt {
                        None => Vec::new(),
                        Some(payload) => json_payload_to_rows(payload),
                    };

                    let mut has_more_page = false;
                    if let Some(lim) = st.requested_page_limit {
                        let lim_usize = usize::try_from(lim).unwrap_or(usize::MAX);
                        if rows.len() > lim_usize {
                            rows.truncate(lim_usize);
                            has_more_page = true;
                        }
                    }

                    let mut truncated = false;
                    if st.max_response_bytes > 0 {
                        let mut acc_bytes = 0_u64;
                        let mut keep_count = 0_usize;
                        for row in &rows {
                            let row_bytes =
                                u64::try_from(encoded_prost_struct_len(row)).unwrap_or(u64::MAX);
                            if acc_bytes.saturating_add(row_bytes) > st.max_response_bytes {
                                truncated = true;
                                break;
                            }
                            acc_bytes = acc_bytes.saturating_add(row_bytes);
                            keep_count += 1;
                        }
                        if truncated {
                            rows.truncate(keep_count);
                        }
                    }

                    let emitted_u64 = u64::try_from(rows.len()).unwrap_or(u64::MAX);
                    let next_cursor = if st.is_paginated
                        && (has_more_page || (truncated && emitted_u64 > 0))
                    {
                        encode_cursor(
                            &st.op_name,
                            &st.args_hash,
                            &st.scope.tenant_id,
                            st.start_offset.saturating_add(emitted_u64),
                        )
                    } else {
                        String::new()
                    };

                    let latency_us =
                        u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
                    st.pending_complete = Some(Complete {
                        next_cursor,
                        truncated,
                        rows_total: emitted_u64,
                        upstream_calls: attempts.load(Ordering::Acquire),
                        latency_us,
                    });

                    Some((
                        Ok(QueryResponse {
                            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                foreach_index: 0,
                                rows,
                                last_for_index: true,
                                error: None,
                            })),
                        }),
                        st,
                    ))
                }
            }
        }));

        Ok(Box::pin(GuardedQueryStream {
            inner,
            _permit: permit,
        }))
    }

    async fn health(
        &self,
        _request: HealthRequest,
        scope: CallScope,
    ) -> NgcResult<HealthResponse> {
        let _ = ConnectorShared::<U>::check_scope_alive(&scope)?;
        let weight = self.shared.current_capacity_weight();
        let mut details = Map::with_capacity(1);
        details.insert("capacity_weight".to_owned(), json!(weight));
        Ok(HealthResponse {
            status: ServingStatus::Serving as i32,
            version: NGC_WIRE_VERSION.to_owned(),
            details: Some(json_map_to_prost_struct(&details)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::http::header::{CONTENT_TYPE, LOCATION};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use futures_util::StreamExt;
    use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
    use gap_proto::ngc::v1::{
        Cmp, Expr, MutateRequest, OrderBy, Page, VariableSet, expr, query_response,
    };
    use serde_json::{Value, json};
    use tokio::time::Instant;

    use super::*;
    use crate::auth::{ApiKeyLocation, StaticCredentialSource};
    use crate::fixture::{FIXTURE_BLOB_COUNT, FIXTURE_CUSTOMER_COUNT, FixtureService};
    use crate::upstream::UpstreamResponse;

    fn shop_bytes() -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/openapi/shop.openapi.json");
        std::fs::read(path).expect("read shop.openapi.json")
    }

    fn scope_in(dur: Duration) -> CallScope {
        let mut s = CallScope::for_test(Instant::now() + dur);
        s.tenant_id = "tenant-alpha".to_owned();
        s.request_id = "req-test-1".to_owned();
        s
    }

    fn json_struct(val: &Value) -> prost_types::Struct {
        json_map_to_prost_struct(val.as_object().expect("json object"))
    }

    async fn collect_stream(
        mut stream: BoxStream<QueryResponse>,
    ) -> Result<(Vec<RowSetChunk>, Complete), NgcErrorInfo> {
        let mut chunks = Vec::new();
        let mut complete = None;
        while let Some(item) = stream.next().await {
            let resp = item?;
            match resp.chunk {
                Some(query_response::Chunk::Rows(r)) => chunks.push(r),
                Some(query_response::Chunk::Complete(c)) => complete = Some(c),
                None => panic!("QueryResponse missing chunk"),
            }
        }
        Ok((chunks, complete.expect("stream must end with Complete")))
    }

    struct LogCollector {
        next_id: AtomicU64,
        events: Arc<Mutex<Vec<String>>>,
    }

    struct FieldVisitor(String);

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    impl tracing::Subscriber for LogCollector {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            let mut v = FieldVisitor(String::new());
            span.values().record(&mut v);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(v.0);
            tracing::span::Id::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed))
        }

        fn record(&self, _span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
            let mut v = FieldVisitor(String::new());
            values.record(&mut v);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(v.0);
        }

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut v = FieldVisitor(String::new());
            event.record(&mut v);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(v.0);
        }

        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    // spec: NGC-010, NGC-012, NGC-013, CMP-030, CMP-031
    #[tokio::test]
    async fn describe_is_stable_honors_known_spec_hash_and_never_uses_document_servers() {
        let doc_with_hijack_server = serde_json::to_vec(&json!({
            "openapi": "3.1.0",
            "info": { "title": "ServerHijackCheck", "version": "1.0.0" },
            "servers": [{ "url": "http://10.99.99.99:9999/hijack" }],
            "paths": {
                "/items/{id}": {
                    "get": {
                        "operationId": "getItem",
                        "parameters": [
                            { "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } }
                        ],
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": { "application/json": { "schema": { "type": "object", "properties": { "id": { "type": "integer" } } } } }
                            }
                        }
                    }
                }
            }
        }))
        .expect("serialize doc");

        let creds = StaticCredentialSource::new().with_secret("api_tok", "s3cr3t-loopback-only");
        let cfg = OpenApiConfig {
            base_url: "http://127.0.0.1:18080/v2".to_owned(),
            auth: AuthConfig::Bearer {
                credential: "api_tok".to_owned(),
            },
            allow_insecure_upstream: false,
            ..OpenApiConfig::default()
        };
        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &doc_with_hijack_server,
            cfg,
            &creds,
            Arc::clone(&fixture),
        )
        .expect("build connector");

        // `servers[]` is recorded in the extraction report and NEVER replaces `config.base_url`.
        assert_eq!(conn.base_url().as_url().as_str(), "http://127.0.0.1:18080/v2");
        assert_eq!(
            conn.extraction_report().servers,
            vec!["http://10.99.99.99:9999/hijack".to_owned()]
        );

        // `https://` base_url is refused fail-closed even on `from_document_bytes`.
        let https_err = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig {
                base_url: "https://127.0.0.1:8443".to_owned(),
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::new(FixtureService::new()),
        )
        .expect_err("https must be refused");
        assert!(matches!(
            https_err,
            ConnectorBuildError::Auth(AuthError::TlsUnsupported)
        ));

        let shop_conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig::default(),
            &StaticCredentialSource::new(),
            Arc::new(FixtureService::new()),
        )
        .expect("build shop connector");

        let d1 = shop_conn
            .describe(
                DescribeRequest {
                    known_spec_hash: String::new(),
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("describe 1");
        let d2 = shop_conn
            .describe(
                DescribeRequest {
                    known_spec_hash: String::new(),
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("describe 2");
        assert!(!d1.not_modified);
        assert_eq!(d1, d2);

        let desc = d1.description.expect("description present");
        gap_ql::schema::with_gap_builtins(&desc.schema_sdl).expect("valid branch SDL");
        assert!(desc.capabilities.as_ref().expect("caps").foreach_batch);
        assert_eq!(desc.limits.as_ref().expect("limits").max_batch_size, 50);

        let not_mod = shop_conn
            .describe(
                DescribeRequest {
                    known_spec_hash: desc.spec_hash.clone(),
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("describe known hash");
        assert!(not_mod.not_modified);
        assert!(not_mod.description.is_none());

        let stale = shop_conn
            .describe(
                DescribeRequest {
                    known_spec_hash: "stale-hash".to_owned(),
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("describe stale hash");
        assert!(!stale.not_modified);
        assert!(stale.description.is_some());

        // Unadvertised Mutate defaults to UNIMPLEMENTED (even for dry_run).
        let mut_err = shop_conn
            .mutate(
                MutateRequest {
                    operation: "create_order".to_owned(),
                    dry_run: true,
                    ..MutateRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect_err("mutate unimplemented");
        assert_eq!(mut_err.code, NgcErrorCode::Unimplemented);
    }

    // spec: CMP-030, CMP-031, INV-15, NGC-061
    #[tokio::test]
    async fn auth_modes_reach_upstream_once_and_never_leak_to_debug_logs_or_errors() {
        let canary = "CANARY_ULTRA_SECRET_TOKEN_987654";
        let creds = StaticCredentialSource::new().with_secret("vault_key", canary);

        let modes = [
            (
                AuthConfig::Bearer {
                    credential: "vault_key".to_owned(),
                },
                Some(("authorization", format!("Bearer {canary}"))),
                None,
            ),
            (
                AuthConfig::ApiKey {
                    location: ApiKeyLocation::Header,
                    name: "X-Shop-Key".to_owned(),
                    credential: "vault_key".to_owned(),
                },
                Some(("x-shop-key", canary.to_owned())),
                None,
            ),
            (
                AuthConfig::ApiKey {
                    location: ApiKeyLocation::Query,
                    name: "api_key".to_owned(),
                    credential: "vault_key".to_owned(),
                },
                None,
                Some(format!("api_key={canary}")),
            ),
            (
                AuthConfig::Basic {
                    user: "svc".to_owned(),
                    credential: "vault_key".to_owned(),
                },
                Some((
                    "authorization",
                    format!(
                        "Basic {}",
                        crate::auth::encode_base64(format!("svc:{canary}").as_bytes())
                    ),
                )),
                None,
            ),
        ];

        let captured_logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = LogCollector {
            next_id: AtomicU64::new(1),
            events: Arc::clone(&captured_logs),
        };
        let _guard = tracing::subscriber::set_default(subscriber);

        for (auth_cfg, expected_header, expected_query) in modes {
            let fixture = Arc::new(FixtureService::new());
            let conn = OpenApiConnector::from_document_bytes(
                &shop_bytes(),
                OpenApiConfig {
                    base_url: "http://127.0.0.1:8080".to_owned(),
                    auth: auth_cfg,
                    ..OpenApiConfig::default()
                },
                &creds,
                Arc::clone(&fixture),
            )
            .expect("build connector");

            assert!(!format!("{conn:?}").contains(canary));

            // 1. Successful call: secret reaches upstream once (and OpenAPI `Authorization` param is ignored!).
            let req = QueryRequest {
                operation: "customer".to_owned(),
                arguments: Some(json_struct(&json!({
                    "id": 1,
                    "X-Request-Tag": "trace-42"
                }))),
                ..QueryRequest::default()
            };
            let stream = conn
                .query(req, scope_in(Duration::from_secs(5)))
                .await
                .expect("query ok");
            let (chunks, complete) = collect_stream(stream).await.expect("collect ok");
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].rows.len(), 1);
            assert!(!format!("{complete:?}").contains(canary));

            let recorded = fixture.recorded_requests();
            assert_eq!(recorded.len(), 1);
            if let Some((hdr_name, hdr_val)) = &expected_header {
                let matching: Vec<_> = recorded[0]
                    .headers
                    .iter()
                    .filter(|(k, _)| k == hdr_name)
                    .collect();
                assert_eq!(matching.len(), 1, "auth header must appear exactly once");
                assert_eq!(matching[0].1, *hdr_val);
            }
            if let Some(q_sub) = &expected_query {
                assert!(
                    recorded[0].path_and_query.contains(q_sub),
                    "expected query auth in {}",
                    recorded[0].path_and_query
                );
            }

            // 2. Hostile 401/403/500 responses echoing the canary in body MUST NOT leak canary in NgcErrorInfo.
            for status in [
                StatusCode::UNAUTHORIZED,
                StatusCode::FORBIDDEN,
                StatusCode::INTERNAL_SERVER_ERROR,
            ] {
                let mut headers = HeaderMap::new();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                fixture.push_scripted_response(UpstreamResponse {
                    status,
                    headers,
                    body: format!("rejected token {canary} at http://internal.secret/v1")
                        .into_bytes(),
                });
                let mut scope = scope_in(Duration::from_secs(5));
                scope
                    .metadata
                    .insert("authorization".to_owned(), format!("Bearer {canary}"));
                let stream = conn
                    .query(
                        QueryRequest {
                            operation: "customer".to_owned(),
                            arguments: Some(json_struct(&json!({ "id": 1 }))),
                            ..QueryRequest::default()
                        },
                        scope,
                    )
                    .await
                    .expect("stream starts");
                let err = collect_stream(stream)
                    .await
                    .expect_err("upstream error expected");
                assert!(
                    !err.message().contains(canary),
                    "leaked canary: {}",
                    err.message()
                );
                assert!(
                    !err.message().contains("internal.secret"),
                    "leaked upstream URL/body: {}",
                    err.message()
                );
            }
        }

        let logs = captured_logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join("\n");
        assert!(!logs.contains(canary), "logs leaked canary: {logs}");
    }

    // spec: CMP-030
    #[tokio::test]
    async fn recorded_requests_preserve_segments_encode_hostile_sku_and_reject_crlf_headers() {
        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig::default(),
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build");

        for sku in ["..", ".", "a/b?admin=1#frag", "%2e%2e/secret"] {
            fixture.clear_recorded();
            let stream = conn
                .query(
                    QueryRequest {
                        operation: "product".to_owned(),
                        arguments: Some(json_struct(&json!({ "sku": sku }))),
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(5)),
                )
                .await
                .expect("query product");
            let (chunks, _) = collect_stream(stream).await.expect("collect product");
            assert_eq!(chunks[0].rows.len(), 1);
            let rec = &fixture.recorded_requests()[0];
            assert!(!rec.path_and_query.contains('?'));
            assert!(!rec.path_and_query.contains('#'));
            let segs: Vec<&str> = rec.path_and_query.split('/').skip(1).collect();
            assert_eq!(
                segs.len(),
                2,
                "must have exactly 2 segments: {}",
                rec.path_and_query
            );
            assert_eq!(segs[0], "products");
            assert_ne!(segs[1], ".");
            assert_ne!(segs[1], "..");
        }

        // CR/LF in header parameter is rejected BEFORE calling upstream.
        fixture.clear_recorded();
        let bad_hdr_err = conn
            .query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    arguments: Some(json_struct(&json!({
                        "id": 1,
                        "X-Request-Tag": "good\r\nX-Injected: evil"
                    }))),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .err()
            .expect("CRLF header must fail synchronously");
        assert_eq!(bad_hdr_err.code, NgcErrorCode::InvalidArgument);
        assert_eq!(fixture.request_count(), 0);
    }

    // spec: NGC-060, NGC-061, NGC-023, CMP-033
    #[tokio::test]
    async fn error_mapping_table_covers_every_status_content_type_cap_redirect_and_keyed_404() {
        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig {
                max_upstream_body_bytes: 512,
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build");

        // Unknown operation -> NOT_FOUND.
        let unknown = conn
            .query(
                QueryRequest {
                    operation: "no_such_op".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .err()
            .expect("unknown op");
        assert_eq!(unknown.code, NgcErrorCode::NotFound);

        // Unadvertised predicate -> INVALID_ARGUMENT (NGC-023).
        let pred_err = conn
            .query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    predicate: Some(Expr {
                        node: Some(expr::Node::Cmp(Cmp {
                            path: "id".to_owned(),
                            op: 1,
                            value: None,
                        })),
                    }),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .err()
            .expect("predicate rejected");
        assert_eq!(pred_err.code, NgcErrorCode::InvalidArgument);

        // Keyed 404 (`GET /customers/999`) -> 0 rows, NOT an error.
        let keyed_404 = conn
            .query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    arguments: Some(json_struct(&json!({ "id": 999 }))),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("keyed 404 starts");
        let (chunks, complete) = collect_stream(keyed_404).await.expect("keyed 404 empty ok");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].rows.is_empty());
        assert!(chunks[0].last_for_index);
        assert_eq!(complete.rows_total, 0);

        // Unkeyed 404 (`GET /customers`) -> UPSTREAM_ERROR (non-retryable).
        fixture.fail_next_with_status(StatusCode::NOT_FOUND, None, 1);
        let unkeyed_404 = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect_err("unkeyed 404 must fail");
        assert_eq!(unkeyed_404.code, NgcErrorCode::UpstreamError);
        assert!(!unkeyed_404.retryable);

        // Status table: 400, 422, 401, 403, 500, 502, 302 (redirect not followed).
        let cases = [
            (StatusCode::BAD_REQUEST, NgcErrorCode::InvalidArgument, false),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                NgcErrorCode::InvalidArgument,
                false,
            ),
            (StatusCode::UNAUTHORIZED, NgcErrorCode::UpstreamError, false),
            (StatusCode::FORBIDDEN, NgcErrorCode::UpstreamError, false),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                NgcErrorCode::UpstreamError,
                true,
            ),
            (StatusCode::BAD_GATEWAY, NgcErrorCode::UpstreamError, true),
            (StatusCode::FOUND, NgcErrorCode::UpstreamError, false),
        ];
        for (status, expected_code, expected_retryable) in cases {
            fixture.clear_recorded();
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            if status.is_redirection() {
                headers.insert(
                    LOCATION,
                    HeaderValue::from_static("http://169.254.169.254/latest/meta-data"),
                );
            }
            fixture.push_scripted_response(UpstreamResponse {
                status,
                headers,
                body: br#"[{"id":1}]"#.to_vec(),
            });
            let err = collect_stream(
                conn.query(
                    QueryRequest {
                        operation: "customers".to_owned(),
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(5)),
                )
                .await
                .expect("starts"),
            )
            .await
            .expect_err("status must fail");
            assert_eq!(err.code, expected_code, "status {status}");
            assert_eq!(err.retryable, expected_retryable, "status {status}");
            assert_eq!(
                fixture.request_count(),
                1,
                "status {status} must not retry or follow redirect"
            );
        }

        // Over-cap upstream body -> UPSTREAM_ERROR.
        let mut hdrs = HeaderMap::new();
        hdrs.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        fixture.push_scripted_response(UpstreamResponse {
            status: StatusCode::OK,
            headers: hdrs,
            body: vec![b' '; 600],
        });
        let over_cap = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect_err("over-cap body must fail");
        assert_eq!(over_cap.code, NgcErrorCode::UpstreamError);

        // Non-JSON content-type -> UPSTREAM_ERROR when allow_binary == false, {"raw": base64} when true.
        let mut bin_hdrs = HeaderMap::new();
        bin_hdrs.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        fixture.push_scripted_response(UpstreamResponse {
            status: StatusCode::OK,
            headers: bin_hdrs.clone(),
            body: b"\x00\x01\x02gap".to_vec(),
        });
        let bad_ct = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect_err("binary rejected by default");
        assert_eq!(bad_ct.code, NgcErrorCode::UpstreamError);

        let bin_conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig {
                allow_binary: true,
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build allow_binary");
        fixture.push_scripted_response(UpstreamResponse {
            status: StatusCode::OK,
            headers: bin_hdrs,
            body: b"\x00\x01\x02gap".to_vec(),
        });
        let (bin_chunks, _) = collect_stream(
            bin_conn
                .query(
                    QueryRequest {
                        operation: "customers".to_owned(),
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(5)),
                )
                .await
                .expect("starts"),
        )
        .await
        .expect("allow_binary succeeds");
        let row_json = Value::Object(prost_struct_to_json_map(&bin_chunks[0].rows[0]));
        assert_eq!(row_json, json!({ "raw": "AAECZ2Fw" }));
    }

    // spec: CMP-033, NGC-025, NGC-026
    #[tokio::test(start_paused = true)]
    async fn retry_table_honors_cap_retry_after_and_deadline_budget() {
        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig {
                max_retry_delay: Duration::from_secs(5),
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build");

        // 1. 429 with `Retry-After: 3` + 503 with HTTP-date (`1 s`) -> succeeds on 3rd attempt (`upstream_calls == 3`, elapsed == 4s).
        fixture.clear_recorded();
        fixture.fail_next_with_status(StatusCode::TOO_MANY_REQUESTS, Some("3"), 1);
        fixture.fail_next_with_status(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
            1,
        );
        let t0 = Instant::now();
        let (chunks, complete) = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    arguments: Some(json_struct(&json!({ "id": 1 }))),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(20)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect("retries succeed");
        assert_eq!(chunks[0].rows.len(), 1);
        assert_eq!(complete.upstream_calls, 3);
        assert_eq!(Instant::now().duration_since(t0), Duration::from_secs(4));

        // 2. Connect error is retried (up to 2 times) and counted in `upstream_calls`.
        fixture.clear_recorded();
        fixture.push_scripted_error(UpstreamError::Connect);
        let (_, complete_conn) = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    arguments: Some(json_struct(&json!({ "id": 2 }))),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(20)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect("connect retry succeeds");
        assert_eq!(complete_conn.upstream_calls, 2);

        // 3. Retry cap is strictly 2 (3 total attempts): 3 consecutive 503s fail UNAVAILABLE even if 4th would succeed.
        fixture.clear_recorded();
        fixture.fail_next_with_status(StatusCode::SERVICE_UNAVAILABLE, Some("1"), 3);
        let cap_err = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    arguments: Some(json_struct(&json!({ "id": 1 }))),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(20)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect_err("must stop after 2 retries");
        assert_eq!(cap_err.code, NgcErrorCode::Unavailable);
        assert!(cap_err.retryable);
        assert_eq!(fixture.request_count(), 3);

        // 4. `Retry-After` exceeding remaining deadline fails IMMEDIATELY without sleeping!
        fixture.clear_recorded();
        fixture.fail_next_with_status(StatusCode::TOO_MANY_REQUESTS, Some("4"), 1);
        let t_before = Instant::now();
        let budget_err = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    arguments: Some(json_struct(&json!({ "id": 1 }))),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(2)),
            )
            .await
            .expect("starts"),
        )
        .await
        .expect_err("must fail immediately when Retry-After >= deadline");
        assert_eq!(budget_err.code, NgcErrorCode::Unavailable);
        assert!(budget_err.retryable);
        assert_eq!(budget_err.retry_after_ms, 4_000);
        assert_eq!(
            Instant::now(),
            t_before,
            "must not sleep when Retry-After exceeds deadline"
        );
        assert_eq!(fixture.request_count(), 1);
    }

    // spec: NGC-021, NGC-022, NGC-027
    #[tokio::test]
    async fn pagination_cursor_round_trip_tamper_rejections_and_size_truncation_drain() {
        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig {
                max_response_bytes: 8_192,
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build");

        // Walk `customers` sorted descending by `credit_cents` in pages of 5 (`12` total -> `5 + 5 + 2`).
        let mut cursor = String::new();
        let mut collected_credits = Vec::new();
        let mut first_cursor = String::new();
        for page_idx in 0..3 {
            let (chunks, complete) = collect_stream(
                conn.query(
                    QueryRequest {
                        operation: "customers".to_owned(),
                        order_by: vec![OrderBy {
                            path: "credit_cents".to_owned(),
                            descending: true,
                        }],
                        page: Some(Page {
                            limit: 5,
                            cursor: cursor.clone(),
                            offset: 0,
                        }),
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(5)),
                )
                .await
                .expect("page starts"),
            )
            .await
            .expect("page ok");
            for row in &chunks[0].rows {
                let m = prost_struct_to_json_map(row);
                collected_credits.push(m["credit_cents"].as_f64().expect("num"));
            }
            if page_idx == 0 {
                first_cursor = complete.next_cursor.clone();
            }
            cursor = complete.next_cursor;
        }
        assert!(
            cursor.is_empty(),
            "3rd page of 12 must finish without trailing empty page"
        );
        assert_eq!(collected_credits.len(), FIXTURE_CUSTOMER_COUNT);
        for w in collected_credits.windows(2) {
            assert!(w[0] >= w[1], "not sorted descending: {collected_credits:?}");
        }

        // Cursor rejections: other arguments, other operation, other tenant, garbage.
        let wrong_args = conn
            .query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    arguments: Some(json_struct(&json!({ "region": "eu" }))),
                    order_by: vec![OrderBy {
                        path: "credit_cents".to_owned(),
                        descending: true,
                    }],
                    page: Some(Page {
                        limit: 5,
                        cursor: first_cursor.clone(),
                        offset: 0,
                    }),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .err()
            .expect("cursor bound to arguments");
        assert_eq!(wrong_args.code, NgcErrorCode::InvalidArgument);

        let wrong_op = conn
            .query(
                QueryRequest {
                    operation: "orders".to_owned(),
                    page: Some(Page {
                        limit: 5,
                        cursor: first_cursor.clone(),
                        offset: 0,
                    }),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .err()
            .expect("cursor bound to operation");
        assert_eq!(wrong_op.code, NgcErrorCode::InvalidArgument);

        let mut other_tenant_scope = scope_in(Duration::from_secs(5));
        other_tenant_scope.tenant_id = "tenant-beta".to_owned();
        let wrong_tenant = conn
            .query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    order_by: vec![OrderBy {
                        path: "credit_cents".to_owned(),
                        descending: true,
                    }],
                    page: Some(Page {
                        limit: 5,
                        cursor: first_cursor,
                        offset: 0,
                    }),
                    ..QueryRequest::default()
                },
                other_tenant_scope,
            )
            .await
            .err()
            .expect("cursor bound to tenant");
        assert_eq!(wrong_tenant.code, NgcErrorCode::InvalidArgument);

        let garbage = conn
            .query(
                QueryRequest {
                    operation: "customers".to_owned(),
                    page: Some(Page {
                        limit: 5,
                        cursor: "not-a-valid-cursor".to_owned(),
                        offset: 0,
                    }),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(5)),
            )
            .await
            .err()
            .expect("garbage cursor");
        assert_eq!(garbage.code, NgcErrorCode::InvalidArgument);

        // Size truncation (`NGC-027`): drain `blobs` (40 x ~1 KiB) across 8 KiB caps; every row arrives once!
        let mut blob_ids = Vec::new();
        let mut blob_cursor = String::new();
        let mut saw_truncated = false;
        loop {
            let page = if blob_cursor.is_empty() {
                None
            } else {
                Some(Page {
                    limit: 0,
                    cursor: blob_cursor.clone(),
                    offset: 0,
                })
            };
            let (chunks, complete) = collect_stream(
                conn.query(
                    QueryRequest {
                        operation: "blobs".to_owned(),
                        page,
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(5)),
                )
                .await
                .expect("blobs query"),
            )
            .await
            .expect("blobs stream");
            if complete.truncated {
                saw_truncated = true;
            }
            for row in &chunks[0].rows {
                let m = prost_struct_to_json_map(row);
                #[allow(clippy::cast_possible_truncation)]
                blob_ids.push(m["id"].as_f64().expect("id") as i64);
            }
            if complete.next_cursor.is_empty() {
                break;
            }
            blob_cursor = complete.next_cursor;
        }
        assert!(
            saw_truncated,
            "40 KiB of blobs must truncate under 8 KiB cap"
        );
        let expected_ids: Vec<i64> =
            (1..=i64::try_from(FIXTURE_BLOB_COUNT).expect("i64")).collect();
        assert_eq!(blob_ids, expected_ids);
    }

    // spec: CMP-032, NGC-020, NGC-026, INV-18
    #[tokio::test(start_paused = true)]
    async fn native_and_emulated_foreach_batching_dedup_isolation_and_concurrency() {
        // 1. Native foreach with `max_batch_size = 3` (customized from shop doc) to verify `ceil(distinct / max_batch_size)`!
        let mut shop_val: Value = serde_json::from_slice(&shop_bytes()).expect("shop json");
        shop_val["paths"]["/customers/{id}"]["get"]["x-gap-in-param"]["max_batch_size"] = json!(3);
        let custom_doc = serde_json::to_vec(&shop_val).expect("serialize custom shop");

        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &custom_doc,
            OpenApiConfig {
                fanout_concurrency: 3,
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build");

        // 7 distinct valid keys (1..=6 plus absent 999) + 1 duplicate (key 1 again) + 1 failing key ("bad") = 9 sets.
        // Note: `foreach` chunks ALL 8 distinct groups (`7` valid + `1` invalid `"bad"`) into batches of 3:
        // Batch 1: `[1, 2, 999]` (3 valid -> 1 upstream call),
        // Batch 2: `["bad", 3, 4]` (2 valid -> 1 upstream call),
        // Batch 3: `[5, 6]` (2 valid -> 1 upstream call) -> 3 upstream calls total, each with <= 3 ids!
        let sets = vec![
            VariableSet {
                index: 10,
                values: Some(json_struct(&json!({ "id": 1 }))),
            },
            VariableSet {
                index: 20,
                values: Some(json_struct(&json!({ "id": 2 }))),
            },
            VariableSet {
                index: 30,
                values: Some(json_struct(&json!({ "id": 999 }))),
            },
            VariableSet {
                index: 40,
                values: Some(json_struct(&json!({ "id": 1 }))),
            },
            VariableSet {
                index: 50,
                values: Some(json_struct(&json!({ "id": "bad" }))),
            },
            VariableSet {
                index: 60,
                values: Some(json_struct(&json!({ "id": 3 }))),
            },
            VariableSet {
                index: 70,
                values: Some(json_struct(&json!({ "id": 4 }))),
            },
            VariableSet {
                index: 80,
                values: Some(json_struct(&json!({ "id": 5 }))),
            },
            VariableSet {
                index: 90,
                values: Some(json_struct(&json!({ "id": 6 }))),
            },
        ];

        fixture.clear_recorded();
        let (chunks, complete) = collect_stream(
            conn.query(
                QueryRequest {
                    operation: "customer".to_owned(),
                    foreach: sets,
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(10)),
            )
            .await
            .expect("native foreach starts"),
        )
        .await
        .expect("native foreach completes");

        assert_eq!(complete.upstream_calls, 3, "expected 3 batched upstream calls");
        assert_eq!(fixture.request_count(), 3);
        for rec in fixture.recorded_requests() {
            let ids_part = rec
                .path_and_query
                .split("ids=")
                .nth(1)
                .expect("ids query param");
            let count = ids_part.split("%2C").count();
            assert!(
                count <= 3,
                "batch exceeded max_batch_size=3: {}",
                rec.path_and_query
            );
        }

        // Absent key (index 30) closed empty without error; duplicate (10 and 40) both received id=1; failing (50) isolated.
        let by_idx: BTreeMap<u32, _> = chunks.iter().map(|c| (c.foreach_index, c)).collect();
        assert_eq!(by_idx.len(), 9);
        assert!(
            by_idx[&30].rows.is_empty()
                && by_idx[&30].last_for_index
                && by_idx[&30].error.is_none()
        );
        assert_eq!(by_idx[&10].rows, by_idx[&40].rows);
        assert_eq!(by_idx[&10].rows.len(), 1);
        assert!(by_idx[&50].error.is_some() && by_idx[&50].last_for_index);

        // 2. Emulated foreach (`order`, no `x-gap-in-param`): 6 distinct keys + 1 duplicate -> 6 upstream calls,
        //    and max concurrent requests never exceeds `fanout_concurrency = 3`.
        let emu_fixture = Arc::new(FixtureService::new());
        emu_fixture.set_handler_delay(Duration::from_millis(50));
        let emu_conn = OpenApiConnector::from_document_bytes(
            &custom_doc,
            OpenApiConfig {
                fanout_concurrency: 3,
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&emu_fixture),
        )
        .expect("build emu");

        let order_sets = vec![
            VariableSet {
                index: 1,
                values: Some(json_struct(&json!({ "id": 1 }))),
            },
            VariableSet {
                index: 2,
                values: Some(json_struct(&json!({ "id": 2 }))),
            },
            VariableSet {
                index: 3,
                values: Some(json_struct(&json!({ "id": 3 }))),
            },
            VariableSet {
                index: 4,
                values: Some(json_struct(&json!({ "id": 4 }))),
            },
            VariableSet {
                index: 5,
                values: Some(json_struct(&json!({ "id": 5 }))),
            },
            VariableSet {
                index: 6,
                values: Some(json_struct(&json!({ "id": 999 }))),
            },
            VariableSet {
                index: 7,
                values: Some(json_struct(&json!({ "id": 1 }))),
            },
        ];
        let (emu_chunks, emu_complete) = collect_stream(
            emu_conn
                .query(
                    QueryRequest {
                        operation: "order".to_owned(),
                        foreach: order_sets,
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(10)),
                )
                .await
                .expect("emulated foreach starts"),
        )
        .await
        .expect("emulated foreach completes");

        assert_eq!(emu_complete.upstream_calls, 6);
        assert!(
            emu_fixture.max_concurrent_seen() <= 3,
            "concurrency {} exceeded fanout_concurrency=3",
            emu_fixture.max_concurrent_seen()
        );
        assert_eq!(emu_chunks.len(), 7);
    }

    // spec: NGC-025, NGC-071, NGC-072, NGC-090
    #[tokio::test(start_paused = true)]
    async fn deadlines_cancellation_and_health_capacity_weight_behave_deterministically() {
        let fixture = Arc::new(FixtureService::new());
        let conn = OpenApiConnector::from_document_bytes(
            &shop_bytes(),
            OpenApiConfig {
                max_in_flight: 2,
                ..OpenApiConfig::default()
            },
            &StaticCredentialSource::new(),
            Arc::clone(&fixture),
        )
        .expect("build");

        // 1. Deadline already past -> upstream is NEVER called.
        let past_scope = CallScope::for_test(Instant::now() - Duration::from_millis(1));
        let past_err = conn
            .query(
                QueryRequest {
                    operation: "slow".to_owned(),
                    ..QueryRequest::default()
                },
                past_scope,
            )
            .await
            .err()
            .expect("past deadline fails immediately");
        assert_eq!(past_err.code, NgcErrorCode::DeadlineExceeded);
        assert_eq!(fixture.request_count(), 0);

        // 2. Mid-call deadline on `slow` -> drops upstream future (`active_in_flight() == 0`) and returns DEADLINE_EXCEEDED.
        let mut slow_stream = conn
            .query(
                QueryRequest {
                    operation: "slow".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_millis(200)),
            )
            .await
            .expect("slow starts");
        let mid_err = slow_stream
            .next()
            .await
            .expect("item")
            .expect_err("deadline must fire");
        assert_eq!(mid_err.code, NgcErrorCode::DeadlineExceeded);
        drop(slow_stream);
        assert_eq!(fixture.active_in_flight(), 0);

        // 3. Cancellation & `max_in_flight` saturation (`max_in_flight = 2`):
        let cancel_scope_1 = scope_in(Duration::from_secs(30));
        let cancel_tok_1 = cancel_scope_1.cancel.clone();
        let mut s1 = conn
            .query(
                QueryRequest {
                    operation: "slow".to_owned(),
                    ..QueryRequest::default()
                },
                cancel_scope_1,
            )
            .await
            .expect("slot 1");

        // Drive `s1` once so `FixtureService::handle("/slow")` is actively running (`active_in_flight == 1`).
        tokio::select! {
            biased;
            _ = s1.next() => panic!("slow must not finish on its own"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        assert_eq!(fixture.active_in_flight(), 1);

        let s2 = conn
            .query(
                QueryRequest {
                    operation: "slow".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(30)),
            )
            .await
            .expect("slot 2");
        let saturated = conn
            .query(
                QueryRequest {
                    operation: "slow".to_owned(),
                    ..QueryRequest::default()
                },
                scope_in(Duration::from_secs(30)),
            )
            .await
            .err()
            .expect("slot 3 refused");
        assert_eq!(saturated.code, NgcErrorCode::ResourceExhausted);
        assert!(saturated.retryable);

        cancel_tok_1.cancel();
        let cancel_err = s1.next().await.expect("item").expect_err("cancelled");
        assert_eq!(cancel_err.code, NgcErrorCode::Cancelled);
        assert_eq!(
            fixture.active_in_flight(),
            0,
            "cancelling must drop the upstream request future immediately"
        );
        drop(s1);
        drop(s2);
        assert_eq!(fixture.active_in_flight(), 0);

        // 4. `Health` capacity_weight table: absent/garbage/out-of-range -> 1.0; valid ratio clamped to (0, 1].
        let weight_of = |resp: &HealthResponse| -> f64 {
            let details = resp.details.as_ref().expect("details present");
            let m = prost_struct_to_json_map(details);
            m["capacity_weight"].as_f64().expect("capacity_weight f64")
        };

        // Initial weight before any upstream response is 1.0.
        let h0 = conn
            .health(HealthRequest {}, scope_in(Duration::from_secs(1)))
            .await
            .expect("health 0");
        assert_eq!(h0.status, ServingStatus::Serving as i32);
        assert!((weight_of(&h0) - 1.0).abs() < 1e-9);

        let rate_cases: &[(Option<&str>, Option<&str>, f64, bool)] = &[
            (Some("100"), Some("25"), 0.25, true),
            (Some("100"), Some("0"), 0.0, false),  // strictly > 0.0 and <= 0.05
            (Some("100"), Some("250"), 1.0, true), // clamped to <= 1.0
            (Some("100"), Some("garbage"), 1.0, true),
            (Some("0"), Some("10"), 1.0, true),    // limit <= 0 invalid -> 1.0
            (Some("100"), Some("-5"), 1.0, true),  // negative remaining invalid -> 1.0
            (None, None, 1.0, true),
        ];
        for &(lim, rem, expected, exact) in rate_cases {
            fixture.set_rate_limit_headers(lim, rem);
            let _ = collect_stream(
                conn.query(
                    QueryRequest {
                        operation: "customer".to_owned(),
                        arguments: Some(json_struct(&json!({ "id": 1 }))),
                        ..QueryRequest::default()
                    },
                    scope_in(Duration::from_secs(5)),
                )
                .await
                .expect("query"),
            )
            .await
            .expect("collect");
            let h = conn
                .health(HealthRequest {}, scope_in(Duration::from_secs(1)))
                .await
                .expect("health");
            let w = weight_of(&h);
            assert!(w > 0.0 && w <= 1.0, "capacity_weight {w} out of (0, 1]");
            if exact {
                assert!(
                    (w - expected).abs() < 1e-9,
                    "for ({lim:?}, {rem:?}) expected {expected}, got {w}"
                );
            } else {
                assert!(
                    w > 0.0 && w <= 0.05,
                    "zero remaining should clamp to small positive floor, got {w}"
                );
            }
        }
    }
}
