//! `/healthz`, `/readyz` (every failing readiness condition), `/metrics` deterministic Prometheus
//! text exposition & bounded label cardinality (`INV-15`, `OPS-020`), and `OPS-010`/`OPS-011`
//! `TracingCapture` span hierarchy/attribute verification.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestHarness, make_api_key, send_oneshot};
use gap_core::snapshot::Snapshot;
use gap_core::telemetry::{
    SPAN_ASSEMBLE, SPAN_AUTHZ, SPAN_EXEC, SPAN_GATEWAY_AUTHN, SPAN_OPERATION_VALIDATE, SPAN_PLAN,
    SPAN_REQUEST,
};
use gap_exec::ConnectorRegistry;
use gap_plan::PlanCache;
use gap_snapshot::SnapshotHandle;
use gap_testkit::canary::CanarySink;
use gap_testkit::tracing_capture::TracingCapture;
use gap_transport::{AppState, Profile, TransportConfig, TransportMetrics, router};
use serde_json::json;

// spec: OPS-002
#[tokio::test(start_paused = true)]
async fn healthz_and_readyz_reflect_snapshot_draining_and_registry_conditions() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));

    // 1. `/healthz` is `200 {"status":"ok"}` even when draining.
    let hz = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(hz.status, StatusCode::OK);
    assert_eq!(hz.json(), json!({ "status": "ok" }));

    // 2. `/readyz` is `200` when snapshot loaded (gen 1), not draining, and >= 1 branch registered.
    let rz_ok = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/readyz")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(rz_ok.status, StatusCode::OK);
    assert_eq!(rz_ok.json()["status"], "ready");

    // 3. Failing condition A: `draining == true` -> `/readyz` returns `503` listing `"draining"`,
    //    while `/healthz` still returns `200`.
    harness.state.config.draining.store(true, Ordering::Release);
    let rz_drain = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/readyz")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(rz_drain.status, StatusCode::SERVICE_UNAVAILABLE);
    let drain_reasons = rz_drain.json()["failing"]
        .as_array()
        .expect("failing array")
        .clone();
    assert!(
        drain_reasons.iter().any(|v| v == "draining"),
        "expected `draining` in failing conditions: {drain_reasons:?}"
    );

    let hz_while_draining = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(hz_while_draining.status, StatusCode::OK);

    // 4. Failing condition B: unloaded bootstrap snapshot (`Snapshot::empty(0)`) AND empty
    //    `ConnectorRegistry` -> `/readyz` returns `503` listing both conditions!
    let unloaded_state = Arc::new(AppState::new(
        Arc::new(SnapshotHandle::new(Snapshot::empty(0))),
        Arc::new(ConnectorRegistry::empty()),
        Arc::new(PlanCache::default()),
        TransportConfig::for_profile(Profile::Dev),
        Arc::new(TransportMetrics::new()),
    ));
    let unloaded_router = router(unloaded_state);

    let rz_unloaded = send_oneshot(
        &unloaded_router,
        Request::builder()
            .method("GET")
            .uri("/readyz")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(rz_unloaded.status, StatusCode::SERVICE_UNAVAILABLE);
    let unloaded_reasons = rz_unloaded.json()["failing"]
        .as_array()
        .expect("failing array")
        .clone();
    assert!(
        unloaded_reasons.iter().any(|v| v == "snapshot_not_loaded"),
        "expected `snapshot_not_loaded`, got {unloaded_reasons:?}"
    );
    assert!(
        unloaded_reasons
            .iter()
            .any(|v| v == "no_branches_registered"),
        "expected `no_branches_registered`, got {unloaded_reasons:?}"
    );
}

// spec: OPS-002, OPS-020, INV-15
#[tokio::test(start_paused = true)]
async fn metrics_renders_deterministic_prometheus_text_and_never_echoes_user_paths() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));

    // Fire 1 GraphQL 200 OK request and 1 hostile 404 URI containing a secret/injection token.
    let _ = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;

    let hostile_slug = "user-injected-label-99999";
    let r404 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri(format!("/evil/{hostile_slug}"))
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(r404.status, StatusCode::NOT_FOUND);

    let m_resp = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_eq!(m_resp.status, StatusCode::OK);
    assert!(
        m_resp
            .content_type()
            .unwrap_or("")
            .starts_with("text/plain; version=0.0.4"),
        "unexpected /metrics Content-Type: {:?}",
        m_resp.content_type()
    );

    let text = m_resp.text();
    assert!(
        !text.contains(hostile_slug),
        "Prometheus exposition MUST NEVER include raw user path labels (INV-15, OPS-020):\n{text}"
    );

    // Golden check on required metric families, HELP/TYPE headers, closed labels, and deterministic order.
    let expected_lines_in_order = [
        "# HELP gap_http_requests_total Total HTTP requests handled by gap-transport.",
        "# TYPE gap_http_requests_total counter",
        r#"gap_http_requests_total{route="/graphql",status="200"} 1"#,
        r#"gap_http_requests_total{route="unmatched",status="404"} 1"#,
        "# HELP gap_http_in_flight Currently executing HTTP requests.",
        "# TYPE gap_http_in_flight gauge",
        "gap_http_in_flight 1",
        "# HELP gap_http_request_duration_seconds HTTP request latency in seconds.",
        "# TYPE gap_http_request_duration_seconds histogram",
        r#"gap_http_request_duration_seconds_bucket{route="/graphql",le="0.001"}"#,
        r#"gap_http_request_duration_seconds_bucket{route="/graphql",le="+Inf"} 1"#,
        r#"gap_http_request_duration_seconds_count{route="/graphql"} 1"#,
        "# HELP gap_plan_cache_total PlanCache lookups partitioned by hit or miss.",
        "# TYPE gap_plan_cache_total counter",
        r#"gap_plan_cache_total{result="hit"} 0"#,
        r#"gap_plan_cache_total{result="miss"} 1"#,
        "# HELP gap_snapshot_generation Active supergraph snapshot generation.",
        "# TYPE gap_snapshot_generation gauge",
        "gap_snapshot_generation 1",
    ];

    let mut search_from = 0_usize;
    for expected in expected_lines_in_order {
        let rel = text[search_from..].find(expected).unwrap_or_else(|| {
            panic!("missing or out-of-order Prometheus line `{expected}` in:\n{text}")
        });
        search_from += rel + expected.len();
    }
}

// spec: OPS-010, OPS-011, INV-15
#[tokio::test(start_paused = true)]
async fn ops_010_and_ops_011_spans_record_normative_names_and_hash_queries_without_leaking_secrets()
{
    let api_key_canary = "gap-canary-secret-span-api-key-445566778899".to_owned();
    let canary_sink = CanarySink::new(&api_key_canary).expect("valid canary");

    let mut cfg = TransportConfig::for_profile(Profile::Dev);
    cfg.api_keys
        .push(make_api_key("k-span", &api_key_canary, "acme", &["read"]));
    let harness = TestHarness::with_mocks(
        cfg,
        common::default_shop_mock(),
        common::default_crm_mock(),
        None,
        None,
    );

    let secret_query_literal = "SUPER_SENSITIVE_RAW_QUERY_MARKER_778899";
    let raw_query =
        format!(r#"query SpanCheck {{ order(id: "{secret_query_literal}") {{ id sku }} }}"#);
    let incoming_traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    let capture = TracingCapture::new();
    let _thread_guard = capture.enter_guard();
    tracing::callsite::rebuild_interest_cache();

    let resp = tracing::instrument::WithSubscriber::with_subscriber(
        send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {api_key_canary}"))
                .header("traceparent", incoming_traceparent)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "query": raw_query,
                        "operationName": "SpanCheck"
                    }))
                    .expect("json"),
                ))
                .expect("req"),
        ),
        capture.subscriber(),
    )
    .await;

    assert_eq!(resp.status, StatusCode::OK);
    let out_tp = resp
        .headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .expect("response must propagate W3C traceparent (OPS-010)");
    assert!(
        out_tp.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"),
        "response traceparent must preserve trace-id, got `{out_tp}`"
    );

    let spans = capture.spans();
    let span_names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    for required_span in [
        SPAN_REQUEST,
        SPAN_GATEWAY_AUTHN,
        SPAN_OPERATION_VALIDATE,
        SPAN_AUTHZ,
        SPAN_PLAN,
        SPAN_EXEC,
        SPAN_ASSEMBLE,
    ] {
        assert!(
            span_names.contains(&required_span),
            "missing normative OPS-010 span `{required_span}` in {span_names:?}"
        );
    }

    let req_span = spans
        .iter()
        .find(|s| s.name == SPAN_REQUEST)
        .expect("gap.request");
    assert_eq!(
        req_span.fields.get("gap.generation").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        req_span
            .fields
            .get("gap.operation_name")
            .map(String::as_str),
        Some("SpanCheck")
    );

    let plan_span = spans
        .iter()
        .find(|s| s.name == SPAN_PLAN)
        .expect("gap.plan");
    assert_eq!(
        plan_span.fields.get("gap.plan_cache").map(String::as_str),
        Some("miss")
    );
    assert!(
        plan_span
            .fields
            .get("gap.plan_hash")
            .is_some_and(|h| h.len() == 64),
        "gap.plan must record 64-hex gap.plan_hash: {:?}",
        plan_span.fields
    );

    let asm_span = spans
        .iter()
        .find(|s| s.name == SPAN_ASSEMBLE)
        .expect("gap.assemble");
    assert_eq!(
        asm_span.fields.get("gap.partial").map(String::as_str),
        Some("false")
    );

    // OPS-011 / INV-15: NO span or event attribute may contain the raw query string or API key canary!
    canary_sink
        .check_tracing(&capture)
        .expect("INV-15: spans and events must never contain API key canary");
    for sp in &spans {
        for (k, v) in &sp.fields {
            assert!(
                !v.contains(&raw_query) && !v.contains(secret_query_literal),
                "OPS-011 violation: span `{}` attribute `{k}` leaked raw query text: `{v}`",
                sp.name
            );
        }
    }
    for ev in capture.events() {
        assert!(!ev.message.contains(secret_query_literal));
        for v in ev.fields.values() {
            assert!(!v.contains(secret_query_literal));
        }
    }
}
