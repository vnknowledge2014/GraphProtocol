//! GraphQL-over-HTTP `Accept` negotiation, `GET`/`POST` envelope parsing, `gap-ql` validation,
//! and `spec/02-gap-ql.md` §10 status/error mapping (`GQL-003`, `GQL-050`, `GQL-051`, `ADR-023`).

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use gap_core::error::{ErrorCode, GapError};
use gap_core::snapshot::Snapshot;
use gap_exec::ExecResponse;
use gap_ql::{
    AllowAllVisibility, DocumentMetrics, ExecutableDirectives, PersistedOperationId,
    PersistedOperationStore, Valid, ValidatedOperation, check_ast_pre_validation_limits,
    check_executable_incremental_directives, check_variable_map_size, enforce_operation_limits,
    enforce_operation_mode, extract_executable_directives, parse_executable_ast,
    resolve_persisted_operation, sha256_bytes, validate_document_with_visibility,
    validate_executable_reserved_names,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config::TransportConfig;
use crate::gateway::check_json_depth;
use crate::metrics::RouteLabel;
use crate::pipeline::PipelineError;

/// Canonical `Content-Type` header for `application/graphql-response+json` responses.
pub const CONTENT_TYPE_GRAPHQL_RESPONSE_JSON: &str =
    "application/graphql-response+json; charset=utf-8";

/// Canonical `Content-Type` header for `application/json` responses (`401`/`403`/`4xx` transport & legacy `2xx`).
pub const CONTENT_TYPE_APPLICATION_JSON: &str = "application/json; charset=utf-8";

/// Canonical `Content-Type` header for `graphql-sse` streams.
pub const CONTENT_TYPE_EVENT_STREAM: &str = "text/event-stream; charset=utf-8";

/// Negotiated response encoding selected from the HTTP `Accept` header (`GQL-003`, `Brief §3.5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedMedia {
    /// Standard `application/graphql-response+json; charset=utf-8`.
    GraphqlResponseJson,
    /// Legacy `application/json; charset=utf-8` (`2xx` uses `application/json`, `GraphQLOverHTTP.md` L452–458).
    LegacyApplicationJson,
    /// `graphql-sse` distinct-connections mode (`text/event-stream`).
    EventStream,
}

impl NegotiatedMedia {
    /// `Content-Type` header emitted on `200 OK` GraphQL execution responses.
    #[must_use]
    pub const fn success_content_type(self) -> &'static str {
        match self {
            Self::GraphqlResponseJson => CONTENT_TYPE_GRAPHQL_RESPONSE_JSON,
            Self::LegacyApplicationJson => CONTENT_TYPE_APPLICATION_JSON,
            Self::EventStream => CONTENT_TYPE_EVENT_STREAM,
        }
    }
}

/// Parsed GraphQL-over-HTTP request payload (`POST` JSON body or `GET` URL query parameters).
#[derive(Debug, Clone, Default)]
pub struct ParsedHttpGraphqlRequest {
    /// Optional GraphQL document text (`query`).
    pub query: Option<String>,
    /// Optional target `operationName`.
    pub operation_name: Option<String>,
    /// Optional GraphQL `variables` JSON map.
    pub variables: Option<Map<String, Value>>,
    /// Optional GraphQL `extensions` JSON map.
    pub extensions: Option<Map<String, Value>>,
    /// Raw payload byte count measured on the wire.
    pub raw_bytes: usize,
    /// Whether this request arrived via HTTP `GET` (persisted operations only, mutations forbidden).
    pub is_get: bool,
}

/// Parsed `extensions.gap` request parameters (`deadlineMs`, `budget`, `tokensEstimate`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GapRequestExtensions {
    /// Optional client deadline in milliseconds (`extensions.gap.deadlineMs`, `RUN-030`).
    pub client_deadline_ms: Option<u64>,
    /// Optional client cost budget in cost units (`extensions.gap.budget.costUnits`).
    pub client_cost_budget: Option<u32>,
    /// Whether the caller supplied any budget or requested `tokensEstimate` (`GQL-050`).
    pub include_tokens_estimate: bool,
}

/// Validated GraphQL operation output produced by [`prepare_http_operation`].
#[derive(Debug, Clone)]
pub struct PreparedHttpOperation {
    ///Canonical SHA-256 persisted operation identifier.
    pub operation_id: PersistedOperationId,
    /// Whether the operation came from the registered persisted store.
    pub is_registered: bool,
    /// 64-character lowercase `blake3` hex digest of the query text (`OPS-011`).
    pub query_blake3_hex: String,
    /// Validated executable operation and coerced variables.
    pub validated: ValidatedOperation,
    /// Structural metrics (`max_selection_depth`, `alias_count`, `root_field_count`).
    pub metrics: DocumentMetrics,
    /// Extracted executable directives (`@budget`, `@dryRun`, `@format`).
    pub directives: ExecutableDirectives,
    /// Extracted `extensions.gap` controls.
    pub gap_ext: GapRequestExtensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedMediaRange<'a> {
    ty: &'a str,
    sub: &'a str,
    q_thousandths: u16,
    valid_charset: bool,
    has_unsupported_ext_param: bool,
}

fn parse_q_value(raw: &str) -> Option<u16> {
    let s = raw.trim();
    if let Some(frac) = s.strip_prefix("1.") {
        if frac.len() <= 3 && frac.bytes().all(|b| b == b'0') {
            return Some(1_000);
        }
        return None;
    }
    if s == "1" {
        return Some(1_000);
    }
    if s == "0" {
        return Some(0);
    }
    let frac = s.strip_prefix("0.")?;
    if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut val: u16 = 0;
    let mut scale: u16 = 100;
    for b in frac.bytes() {
        val += u16::from(b - b'0') * scale;
        scale /= 10;
    }
    Some(val)
}

fn parse_media_range(item: &str) -> Option<ParsedMediaRange<'_>> {
    let mut parts = item.split(';');
    let essence = parts.next()?.trim();
    let (ty, sub) = essence.split_once('/')?;
    let ty = ty.trim();
    let sub = sub.trim();
    if ty.is_empty() || sub.is_empty() || (ty == "*" && sub != "*") {
        return None;
    }

    let mut q_thousandths: u16 = 1_000;
    let mut valid_charset = true;
    let mut has_unsupported_ext_param = false;
    let mut seen_q = false;

    for param in parts {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        let (k, v) = param.split_once('=')?;
        let k = k.trim();
        let v = v.trim().trim_matches('"');
        if k.eq_ignore_ascii_case("q") {
            q_thousandths = parse_q_value(v)?;
            seen_q = true;
        } else if !seen_q && k.eq_ignore_ascii_case("charset") {
            if !v.eq_ignore_ascii_case("utf-8") {
                valid_charset = false;
            }
        } else if !seen_q {
            has_unsupported_ext_param = true;
        }
    }

    Some(ParsedMediaRange {
        ty,
        sub,
        q_thousandths,
        valid_charset,
        has_unsupported_ext_param,
    })
}

fn candidate_type_parts(media: NegotiatedMedia) -> (&'static str, &'static str) {
    match media {
        NegotiatedMedia::GraphqlResponseJson => ("application", "graphql-response+json"),
        NegotiatedMedia::LegacyApplicationJson => ("application", "json"),
        NegotiatedMedia::EventStream => ("text", "event-stream"),
    }
}

fn server_preference(media: NegotiatedMedia, route: RouteLabel) -> u8 {
    match route {
        RouteLabel::GraphqlStream => match media {
            NegotiatedMedia::EventStream => 3,
            NegotiatedMedia::GraphqlResponseJson => 2,
            NegotiatedMedia::LegacyApplicationJson => 1,
        },
        _ => match media {
            NegotiatedMedia::GraphqlResponseJson => 3,
            NegotiatedMedia::LegacyApplicationJson => 2,
            NegotiatedMedia::EventStream => 1,
        },
    }
}

/// Negotiates response media type from `Accept` header and target `route` (`Brief §3.5`, `§3.7`).
///
/// # Errors
///
/// Returns `GapError` (`ErrorCode::UnsupportedFeature`, mapped to HTTP `406`) if `Accept` allows
/// none of the supported media types.
pub fn negotiate_accept(
    headers: &HeaderMap,
    route: RouteLabel,
) -> Result<NegotiatedMedia, GapError> {
    let default_for_route = match route {
        RouteLabel::GraphqlStream => NegotiatedMedia::EventStream,
        _ => NegotiatedMedia::GraphqlResponseJson,
    };

    let mut raw_values = Vec::new();
    for val in headers.get_all("accept") {
        let s = val.to_str().map_err(|_| {
            GapError::new(
                ErrorCode::UnsupportedFeature,
                "invalid Accept header encoding",
            )
        })?;
        if !s.trim().is_empty() {
            raw_values.push(s);
        }
    }

    if raw_values.is_empty() {
        return Ok(default_for_route);
    }

    let joined = raw_values.join(",");
    let mut ranges = Vec::new();
    for item in joined.split(',') {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(parsed) = parse_media_range(trimmed) {
            ranges.push(parsed);
        }
    }

    if ranges.is_empty() {
        return Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "unacceptable Accept header",
        ));
    }

    let candidates = [
        NegotiatedMedia::GraphqlResponseJson,
        NegotiatedMedia::LegacyApplicationJson,
        NegotiatedMedia::EventStream,
    ];

    let mut best: Option<(u16, u8, u8, NegotiatedMedia)> = None;

    for candidate in candidates {
        let (cand_ty, cand_sub) = candidate_type_parts(candidate);
        let mut matched_spec: u8 = 0;
        let mut matched_q: u16 = 0;

        for r in &ranges {
            if !r.valid_charset || r.has_unsupported_ext_param {
                continue;
            }
            let specificity =
                if r.ty.eq_ignore_ascii_case(cand_ty) && r.sub.eq_ignore_ascii_case(cand_sub) {
                    3_u8
                } else if r.ty.eq_ignore_ascii_case(cand_ty) && r.sub == "*" {
                    2_u8
                } else if r.ty == "*" && r.sub == "*" {
                    1_u8
                } else {
                    0_u8
                };

            if specificity == 0 {
                continue;
            }
            if specificity > matched_spec
                || (specificity == matched_spec && r.q_thousandths > matched_q)
            {
                matched_spec = specificity;
                matched_q = r.q_thousandths;
            }
        }

        if matched_spec > 0 && matched_q > 0 {
            let pref = server_preference(candidate, route);
            let score = (matched_q, matched_spec, pref, candidate);
            if best.is_none_or(|cur| (score.0, score.1, score.2) > (cur.0, cur.1, cur.2)) {
                best = Some(score);
            }
        }
    }

    best.map(|(_, _, _, media)| media).ok_or_else(|| {
        GapError::new(
            ErrorCode::UnsupportedFeature,
            "Accept header excludes all supported media types (application/graphql-response+json, application/json, text/event-stream)",
        )
    })
}

/// Computes `Retry-After` integer seconds (`ceil(after_ms / 1000)`, minimum `1`) for `429`/`503`.
#[must_use]
pub const fn retry_after_seconds(after_ms: Option<u32>) -> u64 {
    match after_ms {
        Some(0) | None => 1,
        Some(ms) => ((ms as u64) + 999) / 1_000,
    }
}

fn graphql_req_err(status: StatusCode, err: GapError) -> PipelineError {
    PipelineError::GraphqlRequest {
        status,
        errors: vec![err],
    }
}

/// Parses a `POST /graphql` JSON body (`400` on invalid/deep JSON, `422` on non-object or wrong parameter types).
///
/// Unknown top-level keys (such as `"tenant"` or `"principal"`) are ignored per `GraphQLOverHTTP.md`
/// L354–355 and `SEC-010`.
///
/// # Errors
///
/// Returns [`PipelineError::GraphqlRequest`] with `400` (`GAP_PARSE_ERROR`) or `422` (`GAP_VALIDATION_ERROR`).
pub fn parse_post_json_body(
    bytes: &[u8],
    max_json_depth: usize,
) -> Result<ParsedHttpGraphqlRequest, PipelineError> {
    check_json_depth(bytes, max_json_depth)
        .map_err(|e| graphql_req_err(StatusCode::BAD_REQUEST, e))?;

    let root: Value = serde_json::from_slice(bytes).map_err(|_| {
        graphql_req_err(
            StatusCode::BAD_REQUEST,
            GapError::new(ErrorCode::ParseError, "malformed JSON request body"),
        )
    })?;

    let Some(obj) = root.as_object() else {
        return Err(graphql_req_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            GapError::new(
                ErrorCode::ValidationError,
                "GraphQL-over-HTTP JSON body must be a JSON object",
            ),
        ));
    };

    let query = match obj.get("query") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(graphql_req_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                GapError::new(ErrorCode::ValidationError, "`query` must be a JSON string"),
            ));
        }
    };

    let operation_name = match obj.get("operationName") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(graphql_req_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                GapError::new(
                    ErrorCode::ValidationError,
                    "`operationName` must be a JSON string or null",
                ),
            ));
        }
    };

    let variables = match obj.get("variables") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => {
            return Err(graphql_req_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                GapError::new(
                    ErrorCode::ValidationError,
                    "`variables` must be a JSON object or null",
                ),
            ));
        }
    };

    let extensions = match obj.get("extensions") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => {
            return Err(graphql_req_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                GapError::new(
                    ErrorCode::ValidationError,
                    "`extensions` must be a JSON object or null",
                ),
            ));
        }
    };

    Ok(ParsedHttpGraphqlRequest {
        query,
        operation_name,
        variables,
        extensions,
        raw_bytes: bytes.len(),
        is_get: false,
    })
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode_query_component(raw: &str) -> Result<String, GapError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' => {
                if idx + 2 >= bytes.len() {
                    return Err(GapError::new(
                        ErrorCode::ParseError,
                        "truncated percent-encoding in URL query parameter",
                    ));
                }
                let hi = hex_val(bytes[idx + 1]).ok_or_else(|| {
                    GapError::new(
                        ErrorCode::ParseError,
                        "invalid hex digit in URL query parameter",
                    )
                })?;
                let lo = hex_val(bytes[idx + 2]).ok_or_else(|| {
                    GapError::new(
                        ErrorCode::ParseError,
                        "invalid hex digit in URL query parameter",
                    )
                })?;
                out.push((hi << 4) | lo);
                idx += 3;
            }
            other => {
                out.push(other);
                idx += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| GapError::new(ErrorCode::ParseError, "URL query parameter is not UTF-8"))
}

fn parse_query_json_object(
    field_name: &'static str,
    decoded: &str,
    max_json_depth: usize,
) -> Result<Option<Map<String, Value>>, PipelineError> {
    check_json_depth(decoded.as_bytes(), max_json_depth)
        .map_err(|e| graphql_req_err(StatusCode::BAD_REQUEST, e))?;
    let val: Value = serde_json::from_str(decoded).map_err(|_| {
        graphql_req_err(
            StatusCode::BAD_REQUEST,
            GapError::new(
                ErrorCode::ParseError,
                format!("malformed JSON in `{field_name}` query parameter"),
            ),
        )
    })?;
    match val {
        Value::Null => Ok(None),
        Value::Object(m) => Ok(Some(m)),
        _ => Err(graphql_req_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_name}` query parameter must decode to a JSON object"),
            ),
        )),
    }
}

/// Parses URL query parameters on `GET /graphql` (`Brief §3.2`, `§3.5`).
///
/// # Errors
///
/// Returns [`PipelineError`] on oversized query string (`413`), invalid percent/JSON syntax (`400`),
/// or non-object `variables`/`extensions` (`422`).
pub fn parse_get_query_params(
    raw_uri_query: Option<&str>,
    max_bytes: u64,
    max_json_depth: usize,
) -> Result<ParsedHttpGraphqlRequest, PipelineError> {
    let qs = raw_uri_query.unwrap_or("");
    if (qs.len() as u64) > max_bytes {
        return Err(PipelineError::Transport {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            allow: None,
            error: GapError::new(
                ErrorCode::ComplexityExceeded,
                "GET query string exceeds max_request_body_bytes",
            ),
        });
    }

    let mut query = None;
    let mut operation_name = None;
    let mut variables = None;
    let mut extensions = None;

    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_k, raw_v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode_query_component(raw_k)
            .map_err(|e| graphql_req_err(StatusCode::BAD_REQUEST, e))?;
        let val = percent_decode_query_component(raw_v)
            .map_err(|e| graphql_req_err(StatusCode::BAD_REQUEST, e))?;

        match key.as_str() {
            "query" => query = Some(val),
            "operationName" => {
                if !val.is_empty() && val != "null" {
                    operation_name = Some(val);
                }
            }
            "variables" => {
                if !val.is_empty() {
                    variables = parse_query_json_object("variables", &val, max_json_depth)?;
                }
            }
            "extensions" => {
                if !val.is_empty() {
                    extensions = parse_query_json_object("extensions", &val, max_json_depth)?;
                }
            }
            _ => {}
        }
    }

    Ok(ParsedHttpGraphqlRequest {
        query,
        operation_name,
        variables,
        extensions,
        raw_bytes: qs.len(),
        is_get: true,
    })
}

/// Extracts `extensions.gap.{deadlineMs, budget, tokensEstimate}` (`GQL-050`, `RUN-030`).
///
/// Unknown fields inside `extensions.gap` (e.g. spoofed `"tenant"`) are ignored (`SEC-010`).
///
/// # Errors
///
/// Returns [`GapError`] (`ErrorCode::ValidationError`) if `deadlineMs` or `budget.costUnits` has an
/// invalid type or out-of-range value.
pub fn extract_gap_request_extensions(
    extensions: Option<&Map<String, Value>>,
) -> Result<GapRequestExtensions, GapError> {
    let Some(ext_map) = extensions else {
        return Ok(GapRequestExtensions::default());
    };
    let Some(gap_val) = ext_map.get("gap") else {
        return Ok(GapRequestExtensions::default());
    };
    if gap_val.is_null() {
        return Ok(GapRequestExtensions::default());
    }
    let Some(gap_obj) = gap_val.as_object() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`extensions.gap` must be a JSON object",
        ));
    };

    let client_deadline_ms = match gap_obj.get("deadlineMs") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let Some(ms) = v.as_u64() else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`extensions.gap.deadlineMs` must be a positive integer",
                ));
            };
            if ms == 0 {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`extensions.gap.deadlineMs` must be >= 1",
                ));
            }
            Some(ms)
        }
    };

    let mut client_cost_budget = None;
    let mut has_budget = false;

    if let Some(budget_val) = gap_obj.get("budget")
        && !budget_val.is_null()
    {
        let Some(budget_obj) = budget_val.as_object() else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`extensions.gap.budget` must be a JSON object",
            ));
        };
        if budget_obj.values().any(|v| !v.is_null()) {
            has_budget = true;
        }
        if let Some(cost_val) = budget_obj.get("costUnits")
            && !cost_val.is_null()
        {
            let Some(raw_i64) = cost_val.as_i64() else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`extensions.gap.budget.costUnits` must be a non-negative integer",
                ));
            };
            let Ok(cost_u32) = u32::try_from(raw_i64) else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`extensions.gap.budget.costUnits` is out of u32 range",
                ));
            };
            client_cost_budget = Some(cost_u32);
        }
    }

    let explicit_tokens_flag = gap_obj
        .get("includeTokensEstimate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || gap_obj
            .get("tokensEstimate")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || gap_obj
            .get("features")
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("tokensEstimate")));

    Ok(GapRequestExtensions {
        client_deadline_ms,
        client_cost_budget,
        include_tokens_estimate: has_budget || explicit_tokens_flag,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AstOpKind {
    Query,
    Mutation,
    Subscription,
}

fn parse_rendered_operation_header(rendered: &str) -> Option<(AstOpKind, Option<String>)> {
    let trimmed = rendered.trim_start();
    if trimmed.starts_with('{') {
        return Some((AstOpKind::Query, None));
    }
    let word_len = trimmed
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic() || *b == b'_')
        .count();
    let keyword = &trimmed[..word_len];
    let kind = match keyword {
        "fragment" => return None,
        "query" => AstOpKind::Query,
        "mutation" => AstOpKind::Mutation,
        "subscription" => AstOpKind::Subscription,
        _ => return None,
    };

    let after_kw = trimmed[word_len..].trim_start();
    let name_len = after_kw
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    let op_name = (name_len > 0).then(|| after_kw[..name_len].to_owned());
    Some((kind, op_name))
}

fn has_persisted_query_key(extensions: Option<&Map<String, Value>>) -> bool {
    extensions
        .and_then(|m| m.get("persistedQuery"))
        .is_some_and(|v| !v.is_null())
}

/// Maps a pre-execution `GapError` to its HTTP status code (`400` for `ParseError`, else `422`).
#[must_use]
pub const fn graphql_request_error_status(err: &GapError) -> StatusCode {
    match err.code() {
        ErrorCode::ParseError => StatusCode::BAD_REQUEST,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

/// Validates and prepares a parsed HTTP GraphQL request against the pinned [`Snapshot`] (`Brief §3.5`).
///
/// # Errors
///
/// Returns [`PipelineError::Transport`] (`405` on `GET` mutation, `413` on resolved body cap) or
/// [`PipelineError::GraphqlRequest`] (`400` on syntax error, `422` on validation/limits/persisted/unsupported).
pub fn prepare_http_operation(
    snapshot: &Snapshot,
    http_req: &ParsedHttpGraphqlRequest,
    config: &TransportConfig,
    persisted_store: &dyn PersistedOperationStore,
) -> Result<PreparedHttpOperation, PipelineError> {
    let empty_vars = Map::new();
    let vars = http_req.variables.as_ref().unwrap_or(&empty_vars);

    check_variable_map_size(vars.len(), &config.limits)
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    let mut gap_ext = extract_gap_request_extensions(http_req.extensions.as_ref())
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    let has_pq = has_persisted_query_key(http_req.extensions.as_ref());
    if http_req.query.is_none() && !has_pq {
        return Err(graphql_req_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            GapError::new(
                ErrorCode::ValidationError,
                "GraphQL request must include `query` or `extensions.persistedQuery`",
            ),
        ));
    }

    if http_req.is_get && !has_pq {
        return Err(graphql_req_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            GapError::new(
                ErrorCode::OperationNotAllowed,
                "GET /graphql accepts registered persisted operations only; use POST for ad-hoc queries",
            ),
        ));
    }

    let resolved = resolve_persisted_operation(
        http_req.query.as_deref(),
        http_req.extensions.as_ref(),
        persisted_store,
    )
    .map_err(|e| graphql_req_err(graphql_request_error_status(&e), e))?;

    if http_req.is_get && !resolved.is_registered {
        return Err(graphql_req_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            GapError::new(
                ErrorCode::OperationNotAllowed,
                "GET /graphql accepts registered persisted operations only",
            ),
        ));
    }

    let effective_bytes = http_req.raw_bytes.max(resolved.query.len());
    if (effective_bytes as u64) > config.limits.max_request_body_bytes {
        return Err(PipelineError::Transport {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            allow: None,
            error: GapError::new(
                ErrorCode::ComplexityExceeded,
                format!(
                    "resolved operation size {effective_bytes} bytes exceeds max_request_body_bytes ({})",
                    config.limits.max_request_body_bytes
                ),
            ),
        });
    }

    let query_blake3_hex = blake3::hash(resolved.query.as_bytes()).to_hex().to_string();

    let ast_doc = parse_executable_ast(&resolved.query, "request.graphql").map_err(|errs| {
        PipelineError::GraphqlRequest {
            status: StatusCode::BAD_REQUEST,
            errors: errs,
        }
    })?;

    // Inspect AST operations prior to schema validation so `subscription` returns
    // `GAP_UNSUPPORTED_FEATURE` even on supergraphs with no `Subscription` root type, and `GET`
    // mutations return `405` + `Allow: POST` (`GraphQLOverHTTP.md` L423–428, `Brief §1` & `§3.5`).
    let mut ast_ops = Vec::new();
    for def in &ast_doc.definitions {
        if let Some(parsed_op) = parse_rendered_operation_header(&def.to_string()) {
            ast_ops.push(parsed_op);
        }
    }
    let selected_ast_kind = match http_req.operation_name.as_deref() {
        Some(target) => ast_ops
            .iter()
            .find(|(_, name)| name.as_deref() == Some(target))
            .map(|(k, _)| *k),
        None if ast_ops.len() == 1 => Some(ast_ops[0].0),
        None => None,
    };

    if selected_ast_kind == Some(AstOpKind::Subscription) {
        return Err(graphql_req_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            GapError::new(
                ErrorCode::UnsupportedFeature,
                "GraphQL subscriptions are not supported in GAP v1",
            ),
        ));
    }

    if http_req.is_get && selected_ast_kind == Some(AstOpKind::Mutation) {
        return Err(PipelineError::Transport {
            status: StatusCode::METHOD_NOT_ALLOWED,
            allow: Some("POST"),
            error: GapError::new(
                ErrorCode::OperationNotAllowed,
                "GraphQL mutations are not permitted over HTTP GET; use POST",
            ),
        });
    }

    check_executable_incremental_directives(&ast_doc, false)
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    check_ast_pre_validation_limits(&ast_doc, vars.len(), &config.limits)
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    validate_executable_reserved_names(&ast_doc).map_err(|errs| PipelineError::GraphqlRequest {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        errors: errs,
    })?;
    enforce_operation_mode(
        config.effective_operation_mode(),
        resolved.is_registered,
        &ast_doc,
    )
    .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    let valid_schema = Valid::assume_valid_ref(snapshot.schema.as_ref());
    let validated = validate_document_with_visibility(
        valid_schema,
        &resolved.query,
        http_req.operation_name.as_deref(),
        vars,
        &AllowAllVisibility::default(),
    )
    .map_err(|errs| PipelineError::GraphqlRequest {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        errors: errs,
    })?;

    if let Some(op) = validated.operation() {
        if op.operation_type.is_subscription() {
            return Err(graphql_req_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                GapError::new(
                    ErrorCode::UnsupportedFeature,
                    "GraphQL subscriptions are not supported in GAP v1",
                ),
            ));
        }
        if http_req.is_get && op.operation_type.is_mutation() {
            return Err(PipelineError::Transport {
                status: StatusCode::METHOD_NOT_ALLOWED,
                allow: Some("POST"),
                error: GapError::new(
                    ErrorCode::OperationNotAllowed,
                    "GraphQL mutations are not permitted over HTTP GET; use POST",
                ),
            });
        }
    }

    let metrics = enforce_operation_limits(&validated, effective_bytes, vars.len(), &config.limits)
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    let directives = extract_executable_directives(&validated)
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    directives
        .ensure_columnar_allowed(false)
        .map_err(|e| graphql_req_err(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    if directives.budget.is_some() {
        gap_ext.include_tokens_estimate = true;
    }

    Ok(PreparedHttpOperation {
        operation_id: resolved.operation_id,
        is_registered: resolved.is_registered,
        query_blake3_hex,
        validated,
        metrics,
        directives,
        gap_ext,
    })
}

/// Converts a validated [`PersistedOperationId`] into its 32-byte SHA-256 digest (`RUN-014`).
#[must_use]
pub fn persisted_id_bytes(id: &PersistedOperationId) -> [u8; 32] {
    let raw = id.as_str().as_bytes();
    if raw.len() == 64 {
        let mut out = [0_u8; 32];
        let mut ok = true;
        for (idx, pair) in raw.chunks_exact(2).enumerate() {
            match (hex_val(pair[0]), hex_val(pair[1])) {
                (Some(hi), Some(lo)) => out[idx] = (hi << 4) | lo,
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return out;
        }
    }
    sha256_bytes(raw)
}

#[derive(Serialize)]
struct RequestErrorEnvelope {
    errors: Vec<GapError>,
}

/// Serializes a request/transport error JSON body omitting `"data"` and attaching `correlationId` (`ADR-023`).
#[must_use]
pub fn serialize_errors_envelope(errors: Vec<GapError>, request_id: &str) -> Bytes {
    let tagged: Vec<GapError> = errors
        .into_iter()
        .map(|err| {
            if err.correlation_id().is_some() {
                err
            } else {
                err.with_correlation_id(request_id)
            }
        })
        .collect();

    let payload = RequestErrorEnvelope { errors: tagged };
    let vec = serde_json::to_vec(&payload)
        .unwrap_or_else(|_| br#"{"errors":[{"message":"internal error"}]}"#.to_vec());
    Bytes::from(vec)
}

/// Computes `GQL-050` `tokensEstimate = ceil(utf8_bytes(data) / 3.6)`.
#[must_use]
pub fn compute_tokens_estimate(data: Option<&Value>) -> u64 {
    let Some(val) = data else {
        return 0;
    };
    let byte_len = serde_json::to_vec(val).map_or(0_u64, |v| v.len() as u64);
    if byte_len == 0 {
        0
    } else {
        byte_len.saturating_mul(10).saturating_add(35) / 36
    }
}

/// Serializes an [`ExecResponse`] into the canonical GAP-QL HTTP JSON wire envelope (`GQL-050`, `GQL-051`).
#[must_use]
pub fn serialize_exec_response_envelope(
    exec_response: &ExecResponse,
    request_id: &str,
    include_tokens_estimate: bool,
) -> Bytes {
    let mut gap_meta = Map::with_capacity(4);
    gap_meta.insert(
        "generation".to_owned(),
        Value::from(exec_response.extensions.generation),
    );
    gap_meta.insert(
        "cost".to_owned(),
        json!({
            "planned": exec_response.extensions.cost.planned,
            "actual": exec_response.extensions.cost.actual,
        }),
    );
    gap_meta.insert(
        "partial".to_owned(),
        Value::Bool(exec_response.extensions.partial),
    );
    if include_tokens_estimate {
        gap_meta.insert(
            "tokensEstimate".to_owned(),
            Value::from(compute_tokens_estimate(exec_response.data.as_ref())),
        );
    }

    let mut root = Map::with_capacity(3);
    root.insert(
        "data".to_owned(),
        exec_response.data.clone().unwrap_or(Value::Null),
    );
    if !exec_response.errors.is_empty() {
        let errs: Vec<Value> = exec_response
            .errors
            .iter()
            .cloned()
            .map(|e| {
                let gap_err = GapError::from(e).with_correlation_id(request_id);
                serde_json::to_value(gap_err).unwrap_or(Value::Null)
            })
            .collect();
        root.insert("errors".to_owned(), Value::Array(errs));
    }

    let mut ext_wrapper = Map::with_capacity(1);
    ext_wrapper.insert("gap".to_owned(), Value::Object(gap_meta));
    root.insert("extensions".to_owned(), Value::Object(ext_wrapper));

    let vec =
        serde_json::to_vec(&Value::Object(root)).unwrap_or_else(|_| br#"{"data":null}"#.to_vec());
    Bytes::from(vec)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // spec: GQL-003
    #[test]
    fn accept_negotiation_respects_q_weights_wildcards_and_route_defaults() {
        let empty = HeaderMap::new();
        assert_eq!(
            negotiate_accept(&empty, RouteLabel::Graphql).unwrap(),
            NegotiatedMedia::GraphqlResponseJson
        );
        assert_eq!(
            negotiate_accept(&empty, RouteLabel::GraphqlStream).unwrap(),
            NegotiatedMedia::EventStream
        );

        let mut h = HeaderMap::new();
        h.insert(
            "accept",
            "application/json;q=0.9, application/graphql-response+json;q=0.8"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            negotiate_accept(&h, RouteLabel::Graphql).unwrap(),
            NegotiatedMedia::LegacyApplicationJson
        );

        h.insert("accept", "text/event-stream".parse().unwrap());
        assert_eq!(
            negotiate_accept(&h, RouteLabel::Graphql).unwrap(),
            NegotiatedMedia::EventStream
        );

        h.insert(
            "accept",
            "image/webp, application/xml;q=0.5".parse().unwrap(),
        );
        assert!(negotiate_accept(&h, RouteLabel::Graphql).is_err());
    }

    // spec: GQL-003, GQL-050
    #[test]
    fn request_error_serializer_omits_data_and_attaches_correlation_id() {
        let bytes = serialize_errors_envelope(
            vec![GapError::new(ErrorCode::ValidationError, "bad field")],
            "corr-123",
        );
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed.get("data").is_none());
        assert_eq!(
            parsed["errors"][0]["extensions"]["correlationId"],
            "corr-123"
        );
    }
}
