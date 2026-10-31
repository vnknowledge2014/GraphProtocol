//! `/healthz`, `/readyz`, and `/metrics` HTTP handlers (`Brief §3.6`, `OPS-002`, `OPS-020`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Response, StatusCode, header};
use serde_json::json;

use crate::app::AppState;
use crate::operation::CONTENT_TYPE_APPLICATION_JSON;

const CONTENT_TYPE_PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";

fn json_status_response(status: StatusCode, value: &serde_json::Value) -> Response<Body> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| br#"{"status":"error"}"#.to_vec());
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_APPLICATION_JSON),
    );
    resp
}

/// Handles `GET /healthz` (`200 {"status":"ok"}` while the process answers, `OPS-002`).
pub async fn healthz_handler(State(_state): State<Arc<AppState>>) -> Response<Body> {
    json_status_response(StatusCode::OK, &json!({ "status": "ok" }))
}

/// Handles `GET /readyz` (`200` iff snapshot loaded, server not draining, and >= 1 branch
/// registered in `registry`; else `503` with failing conditions listed, `Brief §3.6`).
pub async fn readyz_handler(State(state): State<Arc<AppState>>) -> Response<Body> {
    let pinned = state.snapshot.pin();
    let mut failing: Vec<&'static str> = Vec::with_capacity(3);

    if pinned.generation == 0 || pinned.branches.is_empty() {
        failing.push("snapshot_not_loaded");
    }
    if state.config.draining.load(Ordering::Acquire) {
        failing.push("draining");
    }
    if state.registry.is_empty() {
        failing.push("no_branches_registered");
    }

    if failing.is_empty() {
        json_status_response(
            StatusCode::OK,
            &json!({
                "status": "ready",
                "generation": pinned.generation,
            }),
        )
    } else {
        json_status_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({
                "status": "not_ready",
                "failing": failing,
            }),
        )
    }
}

/// Handles `GET /metrics` (deterministic Prometheus `0.0.4` text exposition, `Brief §3.6`).
pub async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response<Body> {
    let generation = state.snapshot.generation();
    let body_text = state.metrics.render_prometheus(generation);
    let mut resp = Response::new(Body::from(body_text));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_PROMETHEUS),
    );
    resp
}
