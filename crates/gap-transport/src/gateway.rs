//! Gateway request normalization, streaming body cap, `X-Request-Id`, `traceparent`,
//! admission seam, and `dev`/`prod` API-key authentication (`Brief §3.4`, `SEC-010`, `RUN-030`).

use std::fmt;
use std::time::Duration;

use axum::body::Body;
use axum::http::HeaderMap;
use bytes::Bytes;
use gap_core::context::{Principal, PrincipalKind, TraceCtx};
use gap_core::error::{ErrorCode, GapError};
use http_body_util::BodyExt as _;
use tokio::time::Instant;
use uuid::Uuid;

use crate::config::{ApiKeyVerifier, Profile, TransportConfig};

/// Maximum initial buffer pre-allocation when reading a streaming HTTP body (`64 KiB`), preventing
/// oversized `Content-Length` allocation spikes before chunks actually arrive.
const MAX_INITIAL_BODY_ALLOC: usize = 65_536;

/// Pluggable transport-level admission check (`429` rate limit / `503` load shed seam for `WP-M3-07`).
pub trait AdmissionPolicy: Send + Sync + fmt::Debug {
    /// Checks whether a request authenticated as `principal` may proceed to operation validation.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] with `ErrorCode::RateLimited` (`429`) or `ErrorCode::Overloaded` (`503`)
    /// (carrying optional `extensions.retry.after_ms` converted into HTTP `Retry-After`) when the
    /// request is shed before execution.
    fn check(&self, principal: &Principal, headers: &HeaderMap) -> Result<(), GapError>;
}

/// Default [`AdmissionPolicy`] allowing all requests until `WP-M3-07` wires `ratelimit` and `shed`.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllAdmission;

impl AdmissionPolicy for AllowAllAdmission {
    fn check(&self, _principal: &Principal, _headers: &HeaderMap) -> Result<(), GapError> {
        Ok(())
    }
}

/// Returns `true` iff `candidate` is a valid correlation ID (`UUID` or `^[A-Za-z0-9-]{1,64}$`).
#[must_use]
pub fn is_valid_request_id(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > 64 {
        return false;
    }
    let ascii_safe = candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !ascii_safe {
        return false;
    }
    Uuid::try_parse(candidate).is_ok() || ascii_safe
}

/// Validates an incoming `X-Request-Id` header or mints a fresh UUIDv7 string (`Brief §3.4`).
#[must_use]
pub fn resolve_request_id(headers: &HeaderMap) -> String {
    if let Some(raw_val) = headers.get("x-request-id")
        && let Ok(text) = raw_val.to_str()
        && is_valid_request_id(text)
    {
        return text.to_owned();
    }
    Uuid::now_v7().to_string()
}

/// Extracts or generates a W3C [`TraceCtx`] from HTTP headers (`OPS-010`).
#[must_use]
pub fn resolve_trace_context(headers: &HeaderMap) -> TraceCtx {
    if let Some(tp_val) = headers.get("traceparent")
        && let Ok(tp_str) = tp_val.to_str()
        && let Ok(mut ctx) = TraceCtx::parse_traceparent(tp_str.trim())
    {
        if let Some(ts_val) = headers.get("tracestate")
            && let Ok(ts_str) = ts_val.to_str()
            && !ts_str.is_empty()
            && ts_str.len() <= 512
            && ts_str.bytes().all(|b| (0x20..=0x7e).contains(&b))
        {
            ctx.tracestate = Some(ts_str.to_owned());
        }
        return ctx;
    }
    TraceCtx::generate_root(true)
}

/// Validates that an HTTP `POST` carries `Content-Type: application/json` (`415` otherwise).
///
/// # Errors
///
/// Returns `GapError` (`ErrorCode::UnsupportedFeature`) when `Content-Type` is missing, non-UTF-8,
/// not `application/json`, or specifies a non-UTF-8 charset.
pub fn check_post_content_type(headers: &HeaderMap) -> Result<(), GapError> {
    let Some(ct_header) = headers.get("content-type") else {
        return Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "POST requests require Content-Type: application/json",
        ));
    };
    let Ok(ct_str) = ct_header.to_str() else {
        return Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "invalid Content-Type header encoding",
        ));
    };

    let mut parts = ct_str.split(';');
    let essence = parts.next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("application/json") {
        return Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "unsupported Content-Type; POST /graphql requires application/json",
        ));
    }

    for param in parts {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        if let Some((k, v)) = param.split_once('=')
            && k.trim().eq_ignore_ascii_case("charset")
        {
            let unquoted = v.trim().trim_matches('"');
            if !unquoted.eq_ignore_ascii_case("utf-8") {
                return Err(GapError::new(
                    ErrorCode::UnsupportedFeature,
                    "unsupported Content-Type charset; only utf-8 is supported",
                ));
            }
        }
    }

    Ok(())
}

/// Reads an HTTP request body stream incrementally, aborting immediately with `413` as soon as
/// accumulated byte length exceeds `max_bytes` (`Brief §3.4`).
///
/// # Errors
///
/// Returns `Err((413, GapError))` (`ErrorCode::ComplexityExceeded`) if the body exceeds `max_bytes`
/// or `Err((400, GapError))` (`ErrorCode::ParseError`) if reading a body frame fails.
pub async fn read_body_bounded(mut body: Body, max_bytes: u64) -> Result<Bytes, (u16, GapError)> {
    let initial_cap = usize::try_from(max_bytes)
        .unwrap_or(MAX_INITIAL_BODY_ALLOC)
        .min(MAX_INITIAL_BODY_ALLOC);
    let mut accumulated = Vec::with_capacity(initial_cap);

    while let Some(frame_result) = body.frame().await {
        let frame = frame_result.map_err(|_| {
            (
                400_u16,
                GapError::new(ErrorCode::ParseError, "failed to read HTTP request body"),
            )
        })?;
        if let Ok(chunk) = frame.into_data() {
            let next_len = (accumulated.len() as u64).saturating_add(chunk.len() as u64);
            if next_len > max_bytes {
                return Err((
                    413_u16,
                    GapError::new(
                        ErrorCode::ComplexityExceeded,
                        format!("request body exceeds maximum allowed size of {max_bytes} bytes"),
                    ),
                ));
            }
            accumulated.extend_from_slice(&chunk);
        }
    }

    Ok(Bytes::from(accumulated))
}

/// Validates that `bytes` has JSON structural nesting depth `<= max_depth` without recursing (`Brief §3.4`).
///
/// # Errors
///
/// Returns [`GapError`] (`ErrorCode::ParseError`) if JSON nesting exceeds `max_depth`.
pub fn check_json_depth(bytes: &[u8], max_depth: usize) -> Result<(), GapError> {
    let mut in_string = false;
    let mut escaped = false;
    let mut depth: usize = 0;

    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => {
                    depth = depth.saturating_add(1);
                    if depth > max_depth {
                        return Err(GapError::new(
                            ErrorCode::ParseError,
                            format!("JSON structural nesting depth exceeds maximum of {max_depth}"),
                        ));
                    }
                }
                b'}' | b']' => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Authenticates the caller from HTTP headers (`Authorization: Bearer` or `X-API-Key`), ignoring
/// `X-Tenant` and any body fields (`SEC-010`, `Brief §3.4`, `INV-15`).
///
/// # Errors
///
/// Returns `GapError` (`ErrorCode::Unauthenticated`) on missing credentials in `Prod` or invalid
/// credentials in either `Dev` or `Prod`.
pub fn authenticate_headers(
    headers: &HeaderMap,
    config: &TransportConfig,
    verifier: &dyn ApiKeyVerifier,
) -> Result<Principal, GapError> {
    let auth_header = headers.get("authorization");
    let api_key_header = headers.get("x-api-key");

    if auth_header.is_some() && api_key_header.is_some() {
        return Err(GapError::new(
            ErrorCode::Unauthenticated,
            "provide either Authorization or X-API-Key, not both",
        ));
    }

    let extracted_key: Option<&str> = if let Some(raw_auth) = auth_header {
        let auth_str = raw_auth.to_str().map_err(|_| {
            GapError::new(
                ErrorCode::Unauthenticated,
                "invalid Authorization header encoding",
            )
        })?;
        let trimmed = auth_str.trim();
        let Some((scheme, rest)) = trimmed.split_once(' ') else {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "Authorization header must use Bearer scheme",
            ));
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "Authorization header must use Bearer scheme",
            ));
        }
        let token = rest.trim();
        if token.is_empty() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "Bearer token must not be empty",
            ));
        }
        Some(token)
    } else if let Some(raw_key) = api_key_header {
        let key_str = raw_key.to_str().map_err(|_| {
            GapError::new(
                ErrorCode::Unauthenticated,
                "invalid X-API-Key header encoding",
            )
        })?;
        let token = key_str.trim();
        if token.is_empty() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "X-API-Key header must not be empty",
            ));
        }
        Some(token)
    } else {
        None
    };

    if let Some(candidate_key) = extracted_key {
        let Some(matched) = verifier.verify(candidate_key, &config.api_keys) else {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "invalid credentials",
            ));
        };
        let mut principal = Principal::new(
            matched.id.clone(),
            PrincipalKind::Agent,
            matched.tenant.clone(),
        );
        principal.scopes.clone_from(&matched.scopes);
        return Ok(principal);
    }

    match config.profile {
        Profile::Dev => {
            let mut anon = Principal::new(
                "anonymous",
                PrincipalKind::Human,
                config.default_tenant.clone(),
            );
            anon.scopes.insert("read".to_owned());
            Ok(anon)
        }
        Profile::Prod => Err(GapError::new(
            ErrorCode::Unauthenticated,
            "authentication required",
        )),
    }
}

/// Computes the effective monotonic request deadline (`now + min(client_deadline, max_deadline)`, `RUN-030`).
#[must_use]
pub fn compute_request_deadline(
    now: Instant,
    client_deadline_ms: Option<u64>,
    default_deadline: Duration,
    max_deadline: Duration,
) -> Instant {
    let requested = client_deadline_ms.map_or(default_deadline, Duration::from_millis);
    let capped = requested.min(max_deadline);
    now + capped
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::{ApiKeyRecord, Blake3ApiKeyVerifier};
    use gap_core::ids::TenantId;
    use std::collections::BTreeSet;

    // spec: OPS-010, GQL-003
    #[test]
    fn request_id_accepts_uuid_and_safe_slug_and_rejects_oversized_or_unsafe() {
        let mut h = HeaderMap::new();
        h.insert("x-request-id", "req-123-ABC".parse().unwrap());
        assert_eq!(resolve_request_id(&h), "req-123-ABC");

        let over = "x".repeat(65);
        h.insert("x-request-id", over.parse().unwrap());
        let minted = resolve_request_id(&h);
        assert_ne!(minted, over);
        assert_eq!(Uuid::parse_str(&minted).unwrap().get_version_num(), 7);

        h.insert("x-request-id", "bad_underscore".parse().unwrap());
        let minted2 = resolve_request_id(&h);
        assert_eq!(Uuid::parse_str(&minted2).unwrap().get_version_num(), 7);
    }

    // spec: GQL-003, RUN-002
    #[test]
    fn json_depth_scanner_ignores_brackets_inside_strings() {
        let ok_str = br#"{"query":"{ a(x: \"[[[[[[[[[[\") }"}"#;
        assert!(check_json_depth(ok_str, 2).is_ok());

        let too_deep = br#"{"a":{"b":{"c":1}}}"#;
        assert!(check_json_depth(too_deep, 2).is_err());
        assert!(check_json_depth(too_deep, 3).is_ok());
    }

    // spec: SEC-010, INV-15
    #[test]
    fn authn_ignores_x_tenant_and_enforces_profile_rules() {
        let mut cfg = TransportConfig::for_profile(Profile::Dev);
        cfg.api_keys.push(ApiKeyRecord::from_raw_key(
            "k1",
            "top-secret-canary-key",
            TenantId::new("acme").unwrap(),
            BTreeSet::from(["read".to_owned(), "write".to_owned()]),
        ));

        let mut headers = HeaderMap::new();
        headers.insert("x-tenant", "spoofed-tenant".parse().unwrap());
        let p_anon = authenticate_headers(&headers, &cfg, &Blake3ApiKeyVerifier).unwrap();
        assert_eq!(p_anon.tenant.as_str(), "default");
        assert_eq!(p_anon.id, "anonymous");

        headers.insert(
            "authorization",
            "Bearer top-secret-canary-key".parse().unwrap(),
        );
        let p_key = authenticate_headers(&headers, &cfg, &Blake3ApiKeyVerifier).unwrap();
        assert_eq!(p_key.tenant.as_str(), "acme");
        assert_eq!(p_key.id, "k1");

        headers.insert("authorization", "Bearer wrong-canary-key".parse().unwrap());
        let err = authenticate_headers(&headers, &cfg, &Blake3ApiKeyVerifier).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Unauthenticated);
        assert!(!err.message().contains("wrong-canary-key"));
    }
}
