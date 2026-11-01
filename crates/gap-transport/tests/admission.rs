//! The production admission policy end to end: per-principal rate limit, adaptive concurrency limit with
//! priority shedding, per-minute cost budget, stream permit lifetime (`RUN-040`, `RUN-050`, `RUN-051`,
//! `SEC-050`, `ARC-011`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestHarness, default_crm_mock, make_api_key, send_oneshot};
use futures_util::stream;
use gap_core::clock::{Clock, ManualClock};
use gap_ngc_sdk::connector::BoxStream;
use gap_ngc_sdk::mock::MockConnector;
use gap_proto::ngc::v1::QueryResponse;
use gap_transport::ratelimit::RateLimitConfig;
use gap_transport::shed::LimiterConfig;
use gap_transport::{GatewayAdmission, Profile, TransportConfig};
use serde_json::json;
use tower::ServiceExt as _;

fn policy(
    limiter: LimiterConfig,
    rate: RateLimitConfig,
) -> (Arc<GatewayAdmission>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let policy =
        GatewayAdmission::new(limiter, rate, Arc::clone(&clock) as Arc<dyn Clock>).expect("valid");
    (Arc::new(policy), clock)
}

fn small_limiter() -> LimiterConfig {
    LimiterConfig {
        initial_limit: 16,
        min_limit: 16,
        max_limit: 16,
        ..LimiterConfig::default()
    }
}

fn config() -> TransportConfig {
    let mut cfg = TransportConfig::for_profile(Profile::Dev);
    cfg.api_keys
        .push(make_api_key("agent-a", "key-a", "default", &["read"]));
    cfg.api_keys
        .push(make_api_key("agent-b", "key-b", "default", &["read"]));
    cfg
}

fn graphql(key: &str, query: &str, priority: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(p) = priority {
        b = b.header("x-gap-priority", p);
    }
    b.body(Body::from(
        serde_json::to_vec(&json!({ "query": query })).unwrap(),
    ))
    .unwrap()
}

/// A shop connector whose `Query` never completes: every request using it stays in flight until dropped.
fn hanging_shop() -> MockConnector {
    MockConnector::new().on_query(|_req, _scope| {
        let pending: BoxStream<QueryResponse> = Box::pin(stream::pending());
        Ok(pending)
    })
}

async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
    }
    panic!("condition not reached");
}

// spec: RUN-050, SEC-050
#[tokio::test(start_paused = true)]
async fn a_principal_over_its_rate_gets_429_with_retry_after_and_others_are_unaffected() {
    let (policy, clock) = policy(
        LimiterConfig::default(),
        RateLimitConfig {
            rate_per_sec: 10,
            burst: 3,
            ..RateLimitConfig::default()
        },
    );
    let harness = TestHarness::with_mocks(
        config(),
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(policy),
    );
    for _ in 0..3 {
        let ok = send_oneshot(&harness.router, graphql("key-a", "{ shopVersion }", None)).await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text());
    }
    let limited = send_oneshot(&harness.router, graphql("key-a", "{ shopVersion }", None)).await;
    assert_eq!(
        limited.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        limited.text()
    );
    assert_eq!(
        limited.json()["errors"][0]["extensions"]["code"],
        "GAP_RATE_LIMITED"
    );
    assert_eq!(
        limited
            .headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );

    let other = send_oneshot(&harness.router, graphql("key-b", "{ shopVersion }", None)).await;
    assert_eq!(
        other.status,
        StatusCode::OK,
        "another principal has its own bucket"
    );

    clock.advance(Duration::from_millis(100));
    let again = send_oneshot(&harness.router, graphql("key-a", "{ shopVersion }", None)).await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "one token refilled after 100 ms at 10 rps"
    );
}

// spec: RUN-040, ARC-011
#[tokio::test(start_paused = true)]
async fn saturation_sheds_with_503_never_queues_and_batch_goes_first() {
    let (policy, _clock) = policy(
        small_limiter(),
        RateLimitConfig {
            rate_per_sec: 1_000,
            burst: 1_000,
            ..RateLimitConfig::default()
        },
    );
    let shop = hanging_shop();
    let harness = TestHarness::with_mocks(
        config(),
        shop.clone(),
        default_crm_mock(),
        None,
        Some(Arc::clone(&policy) as Arc<dyn gap_transport::AdmissionPolicy>),
    );

    // 12 interactive requests hang in the connector: 12 of 16 slots used (batch ceiling is 80% = 12).
    let mut running = Vec::new();
    for _ in 0..12 {
        running.push(tokio::spawn(harness.router.clone().oneshot(graphql(
            "key-a",
            "{ shopVersion }",
            None,
        ))));
    }
    wait_until(|| policy.limiter().in_flight() == 12).await;

    let batch = send_oneshot(
        &harness.router,
        graphql("key-a", "{ shopVersion }", Some("batch")),
    )
    .await;
    assert_eq!(
        batch.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        batch.text()
    );
    assert_eq!(
        batch.json()["errors"][0]["extensions"]["code"],
        "GAP_OVERLOADED"
    );
    assert!(
        batch.headers.get("retry-after").is_some(),
        "503 carries Retry-After"
    );

    // Interactive still has 4 slots.
    for _ in 0..4 {
        running.push(tokio::spawn(harness.router.clone().oneshot(graphql(
            "key-b",
            "{ shopVersion }",
            None,
        ))));
    }
    wait_until(|| policy.limiter().in_flight() == 16).await;
    let shed = send_oneshot(&harness.router, graphql("key-b", "{ shopVersion }", None)).await;
    assert_eq!(shed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        shop.in_flight(),
        16,
        "a shed request never reached the connector"
    );

    // The ticket is released when the request goes away, exactly once.
    for r in running {
        r.abort();
    }
    wait_until(|| policy.limiter().in_flight() == 0).await;
    let ok = send_oneshot(&harness.router, graphql("key-a", "{ shopVersion }", None)).await;
    assert_ne!(ok.status, StatusCode::SERVICE_UNAVAILABLE);
}

// spec: RUN-050
#[tokio::test(start_paused = true)]
async fn planned_cost_over_the_per_minute_budget_is_refused_before_any_branch_call() {
    let (policy, clock) = policy(
        LimiterConfig::default(),
        RateLimitConfig {
            cost_units_per_min: 60,
            ..RateLimitConfig::default()
        },
    );
    let harness = TestHarness::with_mocks(
        config(),
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(policy),
    );
    // `{ orders { id } }` plans to 12 cost units: five fit in 60, the sixth does not.
    for i in 0..5 {
        let ok = send_oneshot(&harness.router, graphql("key-a", "{ orders { id } }", None)).await;
        assert_eq!(ok.status, StatusCode::OK, "request {i}: {}", ok.text());
    }
    let calls_before = harness
        .shop_mock
        .calls_for(gap_ngc_sdk::mock::RpcMethod::Query);
    let refused = send_oneshot(&harness.router, graphql("key-a", "{ orders { id } }", None)).await;
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        refused.text()
    );
    assert_eq!(
        refused.json()["errors"][0]["extensions"]["code"],
        "GAP_RATE_LIMITED"
    );
    assert_eq!(
        harness
            .shop_mock
            .calls_for(gap_ngc_sdk::mock::RpcMethod::Query),
        calls_before
    );
    clock.advance(Duration::from_secs(12));
    let later = send_oneshot(&harness.router, graphql("key-a", "{ orders { id } }", None)).await;
    assert_eq!(
        later.status,
        StatusCode::OK,
        "the budget refills at 60 units per minute"
    );
}

// spec: RUN-051
#[tokio::test(start_paused = true)]
async fn a_stream_holds_its_permit_until_the_body_is_dropped_or_the_watchdog_fires() {
    let (policy, _clock) = policy(small_limiter(), RateLimitConfig::default());
    let mut cfg = config();
    cfg.max_deadline = Duration::from_secs(5);
    cfg.default_deadline = Duration::from_secs(5);
    let harness = TestHarness::with_mocks(
        cfg,
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(Arc::clone(&policy) as Arc<dyn gap_transport::AdmissionPolicy>),
    );

    let sse = |key: &str| {
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).unwrap(),
            ))
            .unwrap()
    };
    // Held response: the body is not read, so the permit stays with it.
    let response = harness.router.clone().oneshot(sse("key-a")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        policy.limiter().in_flight(),
        1,
        "the stream keeps its slot while the body lives"
    );
    drop(response);
    wait_until(|| policy.limiter().in_flight() == 0).await;

    // A consumer that never reads: the watchdog frees the slot after the longest allowed request.
    let stuck = harness.router.clone().oneshot(sse("key-a")).await.unwrap();
    assert_eq!(policy.limiter().in_flight(), 1);
    tokio::time::sleep(Duration::from_secs(6)).await;
    wait_until(|| policy.limiter().in_flight() == 0).await;
    drop(stuck);
}

// spec: RUN-040
#[tokio::test(start_paused = true)]
async fn requests_that_fail_validation_do_not_feed_the_latency_limiter() {
    let (policy, _clock) = policy(LimiterConfig::default(), RateLimitConfig::default());
    let harness = TestHarness::with_mocks(
        config(),
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(Arc::clone(&policy) as Arc<dyn gap_transport::AdmissionPolicy>),
    );
    let bad = send_oneshot(&harness.router, graphql("key-a", "{ nope }", None)).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let snap = policy.limiter().snapshot();
    assert_eq!(
        snap.in_flight, 0,
        "the permit is released after a request error"
    );
    assert_eq!(snap.admitted[0], 1);
}
