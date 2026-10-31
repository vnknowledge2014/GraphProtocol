//! `/healthz`, `/readyz`, and `/metrics` HTTP handlers (`Brief §3.6`, `OPS-002`, `OPS-020`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::app::AppState;

/// Handles `GET /healthz` (`200 {"status":"ok"}`).
pub async fn healthz_handler(State(_state): State<Arc<AppState>>) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

/// Handles `GET /readyz` (`200` when snapshot loaded, not draining, and >= 1 branch registered; else `503`).
pub async fn readyz_handler(State(_state): State<Arc<AppState>>) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

/// Handles `GET /metrics` (deterministic Prometheus text exposition).
pub async fn metrics_handler(State(_state): State<Arc<AppState>>) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}
