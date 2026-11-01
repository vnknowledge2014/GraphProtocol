//! Mounting of protocol facades (`HttpModule`, `ARC-007`) on the transport router.
//!
//! A request whose path matches a mounted module prefix is authenticated, admission-checked and pinned to one
//! snapshot exactly like `/graphql` (`SEC-010`, `INV-01`), its body is bounded while reading, and the module
//! then receives a ready [`RequestContext`] plus a transport-neutral [`HttpExchangeRequest`] (client bearer
//! credentials stripped, `SEC-011`). Everything else keeps the plain `404`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, header};
use gap_core::context::{Budget, RequestContext};
use gap_core::error::{ErrorCode, GapError};
use gap_core::telemetry::tenant_bucket;
use gap_core::traits::{HttpExchangeRequest, HttpModule};
use uuid::Uuid;

use crate::app::{AppState, RequestCorrelation};
use crate::gateway::{
    AdmissionOutcome, authenticate_headers, read_body_bounded, resolve_trace_context,
};
use crate::pipeline::build_transport_error_response;

/// Response extension set by [`module_or_not_found`] so the telemetry layer labels the request `module`.
#[derive(Debug, Clone, Copy)]
pub struct ModuleRoute;

/// `true` iff `path` is `prefix` itself or lies below it (`/mcp` matches `/mcp` and `/mcp/x`, not `/mcpx`).
#[must_use]
pub fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && (prefix.ends_with('/') || path.as_bytes().get(prefix.len()) == Some(&b'/')))
}

fn module_for<'a>(state: &'a AppState, path: &str) -> Option<&'a Arc<dyn HttpModule>> {
    state.modules.iter().find(|m| {
        m.mount_prefixes()
            .iter()
            .any(|p| path_matches_prefix(path, p))
    })
}

/// Router fallback: serve a mounted facade or answer `404`.
pub async fn module_or_not_found(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    let Some(module) = module_for(&state, req.uri().path()).cloned() else {
        return crate::app::not_found_handler(req).await;
    };
    let mut response = serve_module(&state, module, req).await;
    response.extensions_mut().insert(ModuleRoute);
    response
}

/// HTTP status for an error a module (or the front half of the pipeline) returned.
#[must_use]
pub fn status_for_error(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden | ErrorCode::TenantMismatch => StatusCode::FORBIDDEN,
        ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::ParseError
        | ErrorCode::ValidationError
        | ErrorCode::TypeMismatch
        | ErrorCode::OperationNotAllowed
        | ErrorCode::UnsupportedFeature => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn serve_module(
    state: &Arc<AppState>,
    module: Arc<dyn HttpModule>,
    req: Request<Body>,
) -> Response<Body> {
    let correlation = req.extensions().get::<RequestCorrelation>().cloned();
    let trace = correlation
        .as_ref()
        .map_or_else(|| resolve_trace_context(req.headers()), |c| c.trace.clone());
    let request_id = correlation
        .as_ref()
        .map_or_else(|| Uuid::now_v7().to_string(), |c| c.request_id.clone());
    let refuse = |status: StatusCode, error: GapError| {
        build_transport_error_response(status, None, error, &request_id)
    };

    let principal = match authenticate_headers(
        req.headers(),
        &state.config,
        state.api_key_verifier.as_ref(),
    ) {
        Ok(principal) => principal,
        Err(error) => return refuse(StatusCode::UNAUTHORIZED, error),
    };
    let admission = match state.admission.admit(&principal, req.headers()) {
        Ok(guard) => guard,
        Err(error) => return refuse(status_for_error(error.code()), error),
    };

    // INV-01: one snapshot per request, pinned right after authentication.
    let snapshot = state.snapshot.pin();
    let now = std::time::Instant::now();
    let deadline = now + state.config.default_deadline.min(state.config.max_deadline);
    let ctx = match RequestContext::try_new(
        Uuid::parse_str(&request_id).unwrap_or_else(|_| Uuid::now_v7()),
        principal.tenant.clone(),
        principal,
        deadline,
        Budget::new(state.config.limits.max_cost_units),
        trace,
        0,
        None,
        snapshot,
    ) {
        Ok(ctx) => ctx,
        Err(error) => return refuse(status_for_error(error.code()), error),
    };
    tracing::debug!(
        module = module.module_name(),
        gap.generation = ctx.snapshot.generation,
        gap.tenant = u64::from(tenant_bucket(&ctx.tenant)),
        "mounted module request"
    );

    let (parts, body) = req.into_parts();
    let max_body = state.config.limits.max_request_body_bytes;
    if declared_length(&parts.headers).is_some_and(|len| len > max_body) {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, body_too_large(max_body));
    }
    let body = match tokio::time::timeout(
        state.config.default_deadline.min(state.config.max_deadline),
        read_body_bounded(body, max_body),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err((413, error))) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, error),
        Ok(Err((_, error))) => return refuse(StatusCode::BAD_REQUEST, error),
        Err(_) => {
            return refuse(
                StatusCode::REQUEST_TIMEOUT,
                GapError::new(
                    ErrorCode::DeadlineExceeded,
                    "timed out reading HTTP request body",
                ),
            );
        }
    };

    let exchange = HttpExchangeRequest {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().map(str::to_owned),
        headers: exchange_headers(&parts.headers),
        body: body.to_vec(),
    };

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let handled = tokio::time::timeout(remaining, module.handle(&ctx, exchange)).await;
    admission.set_outcome(match &handled {
        Ok(Ok(_)) => AdmissionOutcome::Success,
        Ok(Err(_)) => AdmissionOutcome::Ignore,
        Err(_) => AdmissionOutcome::Dropped,
    });
    match handled {
        Ok(Ok(out)) => module_response(out.status, &out.headers, out.body, &request_id),
        Ok(Err(error)) => refuse(status_for_error(error.code()), error),
        Err(_) => refuse(
            StatusCode::GATEWAY_TIMEOUT,
            GapError::new(ErrorCode::DeadlineExceeded, "the request deadline passed"),
        ),
    }
}

fn body_too_large(max_body: u64) -> GapError {
    GapError::new(
        ErrorCode::ComplexityExceeded,
        format!("request body exceeds maximum allowed size of {max_body} bytes"),
    )
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Lower-case header pairs handed to a module, without client credentials (`SEC-011`).
fn exchange_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !is_credential_header(name))
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

fn is_credential_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "x-api-key" | "proxy-authorization" | "cookie"
    )
}

fn module_response(
    status: u16,
    headers: &[(String, String)],
    body: Vec<u8>,
    request_id: &str,
) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let out = response.headers_mut();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }
    if !out.contains_key(header::CONTENT_TYPE) {
        out.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    if let Ok(value) = HeaderValue::from_str(request_id) {
        out.entry("x-request-id").or_insert(value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: ARC-007
    #[test]
    fn prefix_matching_is_exact_or_below_a_path_segment() {
        assert!(path_matches_prefix("/mcp", "/mcp"));
        assert!(path_matches_prefix("/mcp/tools", "/mcp"));
        assert!(!path_matches_prefix("/mcpx", "/mcp"));
        assert!(!path_matches_prefix("/mc", "/mcp"));
        assert!(path_matches_prefix(
            "/.well-known/agent-card.json",
            "/.well-known/agent-card.json"
        ));
        assert!(!path_matches_prefix("/", "/mcp"));
    }

    // spec: SEC-011
    #[test]
    fn client_credentials_never_reach_a_module() {
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer secret"),
            ("x-api-key", "secret"),
            ("cookie", "s=secret"),
            ("proxy-authorization", "Basic c2VjcmV0"),
            ("mcp-method", "tools/list"),
            ("origin", "http://localhost"),
        ] {
            headers.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let kept = exchange_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            names.contains(&"mcp-method") && names.contains(&"origin"),
            "{names:?}"
        );
        assert!(
            !kept
                .iter()
                .any(|(_, v)| v.contains("secret") || v.contains("c2VjcmV0")),
            "{kept:?}"
        );
    }

    // spec: SEC-010
    #[test]
    fn error_codes_map_to_http_statuses() {
        for (code, status) in [
            (ErrorCode::Unauthenticated, 401),
            (ErrorCode::Forbidden, 403),
            (ErrorCode::TenantMismatch, 403),
            (ErrorCode::RateLimited, 429),
            (ErrorCode::Overloaded, 503),
            (ErrorCode::DeadlineExceeded, 504),
            (ErrorCode::ParseError, 400),
            (ErrorCode::ValidationError, 400),
            (ErrorCode::Internal, 500),
        ] {
            assert_eq!(status_for_error(code).as_u16(), status, "{code:?}");
        }
    }
}
