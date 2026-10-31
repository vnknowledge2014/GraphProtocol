//! Gateway request normalization, streaming body cap, `X-Request-Id`, `traceparent`,
//! admission seam, and `dev`/`prod` API-key authentication (`Brief §3.4`, `SEC-010`, `RUN-030`).

use std::fmt;

use axum::body::Body;
use axum::http::HeaderMap;
use bytes::Bytes;
use gap_core::context::{Principal, TraceCtx};
use gap_core::error::GapError;

use crate::config::{ApiKeyVerifier, TransportConfig};

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

/// Validates an incoming `X-Request-Id` header or mints a fresh UUIDv7 string (`Brief §3.4`).
#[must_use]
pub fn resolve_request_id(_headers: &HeaderMap) -> String {
    String::new()
}

/// Extracts or generates a W3C [`TraceCtx`] from HTTP headers (`OPS-010`).
#[must_use]
pub fn resolve_trace_context(_headers: &HeaderMap) -> TraceCtx {
    TraceCtx::generate_root(true)
}

/// Reads an HTTP request body stream incrementally, aborting immediately with `413` as soon as
/// accumulated byte length exceeds `max_bytes` (`Brief §3.4`).
///
/// # Errors
///
/// Returns `Err(GapError)` (`ErrorCode::ComplexityExceeded` for `413` or `ErrorCode::ParseError` on
/// transport body stream error).
pub async fn read_body_bounded(_body: Body, _max_bytes: u64) -> Result<Bytes, (u16, GapError)> {
    Err((
        500,
        GapError::new(gap_core::error::ErrorCode::Internal, "stub"),
    ))
}

/// Validates that `bytes` has JSON structural nesting depth `<= max_depth` without recursing (`Brief §3.4`).
///
/// # Errors
///
/// Returns [`GapError`] (`ErrorCode::ParseError`) if JSON nesting exceeds `max_depth`.
pub fn check_json_depth(_bytes: &[u8], _max_depth: usize) -> Result<(), GapError> {
    Ok(())
}

/// Authenticates the caller from HTTP headers (`Authorization: Bearer` or `X-API-Key`), ignoring
/// `X-Tenant` and any body fields (`SEC-010`, `Brief §3.4`).
///
/// # Errors
///
/// Returns `GapError` (`ErrorCode::Unauthenticated`) on missing credentials in `Prod` or invalid
/// credentials in either `Dev` or `Prod`.
pub fn authenticate_headers(
    _headers: &HeaderMap,
    _config: &TransportConfig,
    _verifier: &dyn ApiKeyVerifier,
) -> Result<Principal, GapError> {
    Err(GapError::new(
        gap_core::error::ErrorCode::Unauthenticated,
        "stub",
    ))
}
