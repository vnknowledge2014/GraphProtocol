//! `graphql-sse` v2.6.3 distinct-connections SSE framing and bounded `mpsc` event stream
//! (`Brief §3.7`, `GQL-003`, `OPS-002`, `ARC-011`, `ARC-012`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Placeholder SSE builder until Step 5 implementation.
#[must_use]
pub fn empty_sse_stub() -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}
