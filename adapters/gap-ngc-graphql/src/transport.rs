//! Outbound HTTP GraphQL transport handling HTTP `200`/`294` + `application/graphql-response+json` (`ADR-023`, `NGC-025`, `SEC-011`, `INV-15`).

use std::future::Future;
use std::time::Duration;

use gap_core::error::{NgcErrorCode, NgcErrorInfo, clamp_error_message};
use gap_ngc_sdk::connector::{CallScope, NgcResult};
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::{Map, Value, json};

use crate::config::GraphqlConfig;

/// GraphQL-over-HTTP media type (`ADR-023`, `GQL-003`).
pub const GRAPHQL_RESPONSE_JSON: &str = "application/graphql-response+json";

/// Outbound HTTP GraphQL POST payload (`query`, `variables`, optional `operationName`).
#[derive(Debug, Clone)]
pub struct GraphqlHttpRequest {
    /// Executable GraphQL document.
    pub query: String,
    /// Parameterized variables object.
    pub variables: Map<String, Value>,
    /// Optional `operationName`.
    pub operation_name: Option<String>,
    /// Configured static auth headers + safe observability headers (`traceparent`, `x-request-id`).
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Bounded request timeout.
    pub timeout: Duration,
}

/// Parsed response returned by [`GraphqlTransport::post`].
#[derive(Debug, Clone)]
pub struct GraphqlWireResponse {
    /// Raw HTTP status code returned by upstream (`200`, `294`, `400`, `422`, ...).
    pub status: StatusCode,
    /// True if `Content-Type` was `application/graphql-response+json` (or `application/json`).
    pub is_graphql_body: bool,
    /// Parsed `data` entry when present (and not omitted).
    pub data: Option<Value>,
    /// Parsed `errors` array elements if present.
    pub errors: Vec<Value>,
}

/// Pluggable upstream GraphQL HTTP transport (`ADR-023`).
pub trait GraphqlTransport: Send + Sync + 'static {
    /// Executes one GraphQL-over-HTTP `POST` request and decodes the GraphQL response envelope.
    fn post(
        &self,
        req: GraphqlHttpRequest,
    ) -> impl Future<Output = NgcResult<GraphqlWireResponse>> + Send;
}

/// `reqwest`-backed GraphQL-over-HTTP transport (`ADR-023`, `CMP-033`, `INV-15`).
#[derive(Debug, Clone)]
pub struct ReqwestGraphqlTransport {
    client: reqwest::Client,
    endpoint: String,
    max_body_bytes: usize,
}

impl ReqwestGraphqlTransport {
    /// Builds a redirect-disabled, proxy-disabled `reqwest` GraphQL transport targeting `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` if endpoint validation fails or `INTERNAL` if HTTP client creation fails.
    pub fn new(endpoint: impl Into<String>, max_body_bytes: usize) -> NgcResult<Self> {
        let endpoint = endpoint.into();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::Internal,
                    "failed to initialize GraphQL reqwest client",
                )
            })?;
        Ok(Self {
            client,
            endpoint,
            max_body_bytes: max_body_bytes.max(1024),
        })
    }
}

/// Builds safe outbound headers combining connector credentials (`SEC-011`) and tracing (`traceparent`, `x-request-id`),
/// NEVER forwarding caller bearer tokens from [`CallScope::metadata`].
#[must_use]
pub fn build_outbound_headers(
    static_auth_headers: &[(HeaderName, HeaderValue)],
    scope: &CallScope,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::with_capacity(static_auth_headers.len() + 2);
    out.extend(static_auth_headers.iter().cloned());

    if !scope.traceparent.trim().is_empty()
        && let Ok(val) = HeaderValue::from_str(scope.traceparent.trim())
    {
        out.push((HeaderName::from_static("traceparent"), val));
    }
    if !scope.request_id.trim().is_empty()
        && let Ok(val) = HeaderValue::from_str(scope.request_id.trim())
    {
        out.push((HeaderName::from_static("x-request-id"), val));
    }
    out
}

/// Checks whether `Content-Type` indicates `application/graphql-response+json` (per `ADR-023`) or `application/json`.
#[must_use]
pub fn content_type_is_graphql(headers: &HeaderMap) -> (bool, bool) {
    let media = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();

    let is_graphql_response = media == GRAPHQL_RESPONSE_JSON;
    let is_json = is_graphql_response || media == "application/json";
    (is_graphql_response, is_json)
}

/// Maps HTTP status & optional GraphQL response payload per `ADR-023`:
/// - Any `2xx` (`200`, `294`, ...) is accepted as carrying `data`.
/// - `application/graphql-response+json` bodies are parsed regardless of HTTP status.
/// - Non-2xx without usable `data` returns a sanitized [`NgcErrorInfo`].
///
/// # Errors
///
/// Returns appropriate [`NgcErrorInfo`] when upstream rejects the request or returns transport/auth failure.
pub fn interpret_wire_response(resp: GraphqlWireResponse) -> NgcResult<GraphqlWireResponse> {
    if resp.status.is_success() {
        if resp.data.is_none() && !resp.errors.is_empty() {
            return Err(map_graphql_errors(resp.status, &resp.errors));
        }
        return Ok(resp);
    }

    if resp.is_graphql_body && !resp.errors.is_empty() {
        // Execution-phase partial data with non-2xx? Still honor data if present ONLY if 2xx;
        // per ADR-023 / Sept 2025, 4xx/5xx request errors have no usable data and carry errors[].
        if let Some(ref d) = resp.data
            && !d.is_null()
            && resp.status.as_u16() == 294
        {
            return Ok(resp);
        }
        return Err(map_graphql_errors(resp.status, &resp.errors));
    }

    Err(map_http_status_error(resp.status))
}

fn map_http_status_error(status: StatusCode) -> NgcErrorInfo {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!(
                "upstream GraphQL rejected request (HTTP {})",
                status.as_u16()
            ),
        ),
        StatusCode::UNAUTHORIZED => NgcErrorInfo::new(
            NgcErrorCode::Unauthenticated,
            "upstream GraphQL authentication failed (HTTP 401)",
        ),
        StatusCode::FORBIDDEN => NgcErrorInfo::new(
            NgcErrorCode::PermissionDenied,
            "upstream GraphQL access denied (HTTP 403)",
        ),
        StatusCode::NOT_FOUND => NgcErrorInfo::new(
            NgcErrorCode::NotFound,
            "upstream GraphQL endpoint not found (HTTP 404)",
        ),
        StatusCode::TOO_MANY_REQUESTS => NgcErrorInfo::new(
            NgcErrorCode::ResourceExhausted,
            "upstream GraphQL rate limited request (HTTP 429)",
        )
        .with_retry(true, 1000),
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT | StatusCode::BAD_GATEWAY => {
            NgcErrorInfo::new(
                NgcErrorCode::Unavailable,
                format!("upstream GraphQL unavailable (HTTP {})", status.as_u16()),
            )
            .with_retry(true, 250)
        }
        other => NgcErrorInfo::new(
            NgcErrorCode::UpstreamError,
            format!("upstream GraphQL returned HTTP {}", other.as_u16()),
        ),
    }
}

fn map_graphql_errors(status: StatusCode, errors: &[Value]) -> NgcErrorInfo {
    let first = errors.first();
    let raw_msg = first
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("upstream GraphQL error");
    let msg = clamp_error_message(raw_msg);

    let ext_code = first
        .and_then(|e| e.get("extensions"))
        .and_then(|ext| ext.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let code = match ext_code {
        "UNAUTHENTICATED" => NgcErrorCode::Unauthenticated,
        "FORBIDDEN" | "PERMISSION_DENIED" => NgcErrorCode::PermissionDenied,
        "RATE_LIMITED" | "TOO_MANY_REQUESTS" => NgcErrorCode::ResourceExhausted,
        "GRAPHQL_PARSE_FAILED"
        | "GRAPHQL_VALIDATION_FAILED"
        | "BAD_USER_INPUT"
        | "GAP_PARSE_ERROR"
        | "GAP_VALIDATION_ERROR" => NgcErrorCode::InvalidArgument,
        _ if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY => {
            NgcErrorCode::InvalidArgument
        }
        _ => NgcErrorCode::UpstreamError,
    };

    NgcErrorInfo::new(code, msg)
}

impl GraphqlTransport for ReqwestGraphqlTransport {
    async fn post(&self, req: GraphqlHttpRequest) -> NgcResult<GraphqlWireResponse> {
        let mut body_obj = Map::new();
        body_obj.insert("query".to_owned(), Value::String(req.query));
        body_obj.insert("variables".to_owned(), Value::Object(req.variables));
        if let Some(op) = req.operation_name {
            body_obj.insert("operationName".to_owned(), Value::String(op));
        }
        let body_bytes = serde_json::to_vec(&Value::Object(body_obj)).map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!("failed to serialize GraphQL POST payload: {err}"),
            )
        })?;

        let mut builder = self
            .client
            .post(&self.endpoint)
            .timeout(req.timeout)
            .header(CONTENT_TYPE, "application/json")
            .header(
                ACCEPT,
                "application/graphql-response+json, application/json;q=0.9",
            );

        for (name, val) in &req.headers {
            builder = builder.header(name, val);
        }

        let response = builder.body(body_bytes).send().await.map_err(|err| {
            if err.is_timeout() {
                NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "upstream GraphQL HTTP request timed out",
                )
                .with_retry(true, 0)
            } else if err.is_connect() {
                NgcErrorInfo::new(
                    NgcErrorCode::Unavailable,
                    "failed to connect to upstream GraphQL endpoint",
                )
                .with_retry(true, 100)
            } else {
                NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    "upstream GraphQL HTTP request failed",
                )
            }
        })?;

        let status = response.status();
        let headers = response.headers().clone();
        if let Some(cl) = response.content_length()
            && usize::try_from(cl).unwrap_or(usize::MAX) > self.max_body_bytes
        {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                format!(
                    "upstream GraphQL response exceeded {} bytes cap",
                    self.max_body_bytes
                ),
            ));
        }

        let mut raw_body = Vec::new();
        let mut resp = response;
        while let Some(chunk) = resp.chunk().await.map_err(|err| {
            if err.is_timeout() {
                NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "upstream GraphQL response body read timed out",
                )
                .with_retry(true, 0)
            } else {
                NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    "failed while streaming upstream GraphQL response body",
                )
            }
        })? {
            if raw_body.len().saturating_add(chunk.len()) > self.max_body_bytes {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    format!(
                        "upstream GraphQL response exceeded {} bytes cap",
                        self.max_body_bytes
                    ),
                ));
            }
            raw_body.extend_from_slice(&chunk);
        }

        let (is_graphql_response_ct, is_json_ct) = content_type_is_graphql(&headers);

        // ADR-023: Parse application/graphql-response+json whatever the status; also parse 2xx/error JSON bodies.
        if !raw_body.is_empty() && (is_graphql_response_ct || is_json_ct || status.is_success()) {
            let parsed: Value = serde_json::from_slice(&raw_body).map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    "upstream GraphQL returned invalid JSON payload",
                )
            })?;
            let data = parsed.get("data").cloned();
            let errors = parsed
                .get("errors")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let has_graphql_body = is_graphql_response_ct || !errors.is_empty() || data.is_some();

            return interpret_wire_response(GraphqlWireResponse {
                status,
                is_graphql_body: has_graphql_body,
                data,
                errors,
            });
        }

        Err(map_http_status_error(status))
    }
}

/// Validates endpoint & credentials for [`GraphqlConfig`] before constructing a [`ReqwestGraphqlTransport`].
pub(crate) fn build_default_transport(
    config: &GraphqlConfig,
) -> NgcResult<ReqwestGraphqlTransport> {
    ReqwestGraphqlTransport::new(config.endpoint.clone(), config.max_upstream_body_bytes)
}

/// Helper for constructing a GraphQL `{ "data": ... }` JSON value in internal paths (re-export convenience).
#[allow(dead_code)]
fn _empty_obj() -> Value {
    json!({})
}
