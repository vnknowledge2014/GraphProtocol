//! `RUN-030` deadline capping (`min(client deadlineMs, max_deadline)`), connector deadline
//! observation, `RUN-031` `<= 50 ms` drop cancellation, and `graphql-sse` v2.6.3 distinct-connections
//! streaming (success, request errors inside stream, bounded `mpsc` slow-consumer backpressure,
//! and stream-drop cancellation).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestHarness, default_crm_mock, send_oneshot, stream_rows};
use futures_util::stream;
use gap_core::clock::Clock as _;
use gap_ngc_sdk::connector::BoxStream;
use gap_ngc_sdk::mock::MockConnector;
use gap_proto::ngc::v1::QueryResponse;
use gap_transport::{Profile, TransportConfig};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

#[derive(Debug, PartialEq, Eq)]
struct ParsedSseEvent {
    event: String,
    data: String,
}

fn parse_sse_frames(raw: &str) -> Vec<ParsedSseEvent> {
    let mut out = Vec::new();
    for block in raw.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut ev_name = String::new();
        let mut data_lines = Vec::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                ev_name = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("data:") {
                let trimmed = rest.strip_prefix(' ').unwrap_or(rest);
                data_lines.push(trimmed.to_owned());
            }
        }
        out.push(ParsedSseEvent {
            event: ev_name,
            data: data_lines.join("\n"),
        });
    }
    out
}

// spec: RUN-030
#[tokio::test(start_paused = true)]
async fn run_030_client_deadline_ms_is_capped_by_max_deadline_and_seen_by_connector() {
    let seen_deadline_unix_ms = Arc::new(AtomicI64::new(0));
    let seen_clone = Arc::clone(&seen_deadline_unix_ms);

    let shop_mock = MockConnector::new().on_query(move |req, _scope| {
        let dl = req.ctx.as_ref().map_or(0, |ctx| ctx.deadline_unix_ms);
        seen_clone.store(dl, Ordering::Release);
        Ok(stream_rows(vec![json!({ "value": "ok" })]))
    });

    let mut cfg = TransportConfig::for_profile(Profile::Dev);
    cfg.max_deadline = Duration::from_secs(5); // 5_000 ms hard cap
    cfg.default_deadline = Duration::from_secs(3); // 3_000 ms default
    cfg.exec.margin_floor = Duration::from_millis(50);
    cfg.exec.margin_fraction = 0.05;

    let harness = TestHarness::with_mocks(cfg, shop_mock, default_crm_mock(), None, None);
    let base_ms = harness.clock.unix_millis();

    // 1. Client asks for `deadlineMs: 60_000` (60 s) -> capped to `max_deadline` (`5_000 ms`),
    //    minus 5% margin (`250 ms`) => `base_ms + 4_750 ms`!
    let resp_capped = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ shopVersion }",
                    "extensions": { "gap": { "deadlineMs": 60_000 } }
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_capped.status, StatusCode::OK);
    let observed_capped = seen_deadline_unix_ms.load(Ordering::Acquire) - base_ms;
    assert!(
        (4_700..=5_000).contains(&observed_capped),
        "expected connector deadline capped by max_deadline=5000ms (around 4750ms), got {observed_capped}ms"
    );

    // 2. Client asks for smaller `deadlineMs: 1_000` (1 s) -> respected (`1_000 ms - 50 ms = 950 ms`).
    let resp_short = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ shopVersion }",
                    "extensions": { "gap": { "deadlineMs": 1_000 } }
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_short.status, StatusCode::OK);
    let observed_short = seen_deadline_unix_ms.load(Ordering::Acquire) - base_ms;
    assert!(
        (900..=1_000).contains(&observed_short),
        "expected connector deadline ~950ms for client deadlineMs=1000, got {observed_short}ms"
    );
}

// spec: RUN-030, RUN-031, INV-12
#[tokio::test(start_paused = true)]
async fn dropping_http_request_future_cancels_connector_stream_within_50ms() {
    let query_started = Arc::new(Notify::new());
    let observed_scope_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));

    let started_clone = Arc::clone(&query_started);
    let cancel_slot = Arc::clone(&observed_scope_cancel);

    let slow_shop = MockConnector::new().on_query(move |_req, scope| {
        *cancel_slot.lock().expect("cancel slot") = Some(scope.cancel.clone());
        started_clone.notify_one();
        let pending_stream: BoxStream<QueryResponse> = Box::pin(stream::pending());
        Ok(pending_stream)
    });

    let harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        slow_shop.clone(),
        default_crm_mock(),
        None,
        None,
    );

    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
        ))
        .expect("req");

    let mut resp_fut = Box::pin(harness.router.clone().oneshot(req));

    // Poll the router request future concurrently until the mock connector's `Query` stream opens.
    tokio::select! {
        _ = &mut resp_fut => panic!("pending stream should not finish before drop"),
        () = query_started.notified() => {}
    }

    assert_eq!(slow_shop.in_flight(), 1);
    assert_eq!(harness.metrics.in_flight(), 1);

    // Client disconnects -> drop the HTTP response future!
    let t_drop = tokio::time::Instant::now();
    drop(resp_fut);

    tokio::time::timeout(Duration::from_millis(50), slow_shop.wait_for_drops(1))
        .await
        .expect("connector stream must be dropped within 50 ms of HTTP future drop");

    let elapsed = t_drop.elapsed();
    assert!(
        elapsed <= Duration::from_millis(50),
        "cancellation propagation took {elapsed:?} (> 50 ms)"
    );
    assert_eq!(slow_shop.in_flight(), 0);
    assert_eq!(harness.metrics.in_flight(), 0);
    let cancel_tok = observed_scope_cancel
        .lock()
        .expect("cancel slot")
        .clone()
        .expect("scope cancel recorded");
    assert!(
        cancel_tok.is_cancelled(),
        "connector CallScope::cancel must be cancelled when client drops HTTP request"
    );
}

// spec: GQL-003, OPS-002
#[tokio::test(start_paused = true)]
async fn graphql_sse_distinct_connections_emits_next_and_complete_and_streams_request_errors() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));

    // 1. Valid query on `/graphql` with `Accept: text/event-stream` AND on `/graphql/stream`.
    for uri in ["/graphql", "/graphql/stream"] {
        let resp = send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "query": "{ order(id: \"ord-7\") { id sku } }"
                    }))
                    .expect("json"),
                ))
                .expect("req"),
        )
        .await;

        assert_eq!(resp.status, StatusCode::OK);
        assert!(
            resp.content_type()
                .unwrap_or("")
                .starts_with("text/event-stream"),
            "SSE response must have Content-Type: text/event-stream, got {:?}",
            resp.content_type()
        );
        let raw_sse = resp.text();
        assert!(
            raw_sse.contains("event: complete\ndata: \n\n"),
            "EventSource compatibility requires `event: complete\\ndata: \\n\\n`: {raw_sse:?}"
        );
        let events = parse_sse_frames(&raw_sse);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "next");
        let next_payload: Value = serde_json::from_str(&events[0].data).expect("next JSON");
        assert_eq!(next_payload["data"]["order"]["id"], "ord-7");
        assert_eq!(next_payload["extensions"]["gap"]["generation"], 1);
        assert_eq!(events[1].event, "complete");
        assert_eq!(events[1].data, "");
    }

    // 2. GraphQL parse, validation, and `subscription` (`GAP_UNSUPPORTED_FEATURE`) errors MUST be
    //    reported INSIDE the `200 text/event-stream` as `event: next` carrying `errors` (no `data`),
    //    followed by `event: complete` — NOT as HTTP 400/422!
    for (bad_query, expected_code) in [
        ("query { broken(", "GAP_PARSE_ERROR"),
        ("{ nonExistentField }", "GAP_UNKNOWN_FIELD"),
        (
            "subscription { gapTaskEvents(taskId: \"t1\") { seq } }",
            "GAP_UNSUPPORTED_FEATURE",
        ),
    ] {
        let err_sse = send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri("/graphql/stream")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "query": bad_query })).expect("json"),
                ))
                .expect("req"),
        )
        .await;
        assert_eq!(
            err_sse.status,
            StatusCode::OK,
            "graphql-sse distinct-connections mode reports GraphQL errors on 200 stream"
        );
        assert!(
            err_sse
                .content_type()
                .unwrap_or("")
                .starts_with("text/event-stream")
        );
        let frames = parse_sse_frames(&err_sse.text());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event, "next");
        let err_json: Value = serde_json::from_str(&frames[0].data).expect("error JSON");
        assert!(
            err_json.get("data").is_none(),
            "request error inside SSE next event must omit `data`: {err_json}"
        );
        assert_eq!(err_json["errors"][0]["extensions"]["code"], expected_code);
        assert_eq!(frames[1].event, "complete");
    }
}

// spec: GQL-003, ARC-011, ARC-012, RUN-031
#[tokio::test(start_paused = true)]
async fn graphql_sse_bounded_mpsc_backpressures_slow_consumer_and_cancels_on_stream_drop() {
    // 1. Bounded `mpsc` (`sse_channel_capacity = 1`): producer sends `next` (fills slot 1) and
    //    blocks waiting to send `complete` while consumer has not polled the response body stream!
    let mut cfg = TransportConfig::for_profile(Profile::Dev);
    cfg.sse_channel_capacity = 1;
    let harness = TestHarness::new(cfg);

    let response = harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql/stream")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
                ))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK);

    // Let the spawned SSE producer run until the bounded 1-slot channel blocks on frame 2 (`complete`).
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        harness.metrics.sse_producers_in_flight(),
        1,
        "with sse_channel_capacity=1 and unread stream, producer MUST block on bounded mpsc (unbounded channel would finish immediately!)"
    );

    // Now consume the stream and verify the producer completes cleanly and releases its guard.
    let mut body = response.into_body();
    let mut collected = Vec::new();
    while let Some(frame_res) = body.frame().await {
        let frame = frame_res.expect("SSE frame");
        if let Ok(data) = frame.into_data() {
            collected.extend_from_slice(&data);
        }
    }
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(harness.metrics.sse_producers_in_flight(), 0);
    let events = parse_sse_frames(&String::from_utf8(collected).expect("utf8"));
    assert_eq!(events.len(), 2);

    // 2. Dropping an open SSE response body stream while execution is in progress cancels the
    //    branch connector stream within `<= 50 ms`!
    let sse_query_started = Arc::new(Notify::new());
    let started_clone = Arc::clone(&sse_query_started);
    let slow_shop = MockConnector::new().on_query(move |_req, _scope| {
        started_clone.notify_one();
        let pending_stream: BoxStream<QueryResponse> = Box::pin(stream::pending());
        Ok(pending_stream)
    });

    let cancel_harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        slow_shop.clone(),
        default_crm_mock(),
        None,
        None,
    );

    let sse_resp = cancel_harness
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql/stream")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
                ))
                .expect("req"),
        )
        .await
        .expect("oneshot");
    assert_eq!(sse_resp.status(), StatusCode::OK);

    tokio::time::timeout(Duration::from_secs(1), sse_query_started.notified())
        .await
        .expect("SSE producer must reach branch mock within 1 s");
    assert_eq!(slow_shop.in_flight(), 1);

    // Drop the SSE HTTP response (client closes EventSource connection).
    let sse_drop_start = tokio::time::Instant::now();
    drop(sse_resp);

    tokio::time::timeout(Duration::from_millis(50), slow_shop.wait_for_drops(1))
        .await
        .expect("SSE drop must cancel connector within 50 ms");
    assert!(sse_drop_start.elapsed() <= Duration::from_millis(50));
    assert_eq!(slow_shop.in_flight(), 0);
}
