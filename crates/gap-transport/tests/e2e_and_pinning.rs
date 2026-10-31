//! End-to-end two-branch `gap_snapshot::compose` execution over `in_memory_grpc`, `GET` persisted
//! operations, `GQL-050` `tokensEstimate`, `GAP_RESPONSE_TOO_LARGE`, `INV-01` mid-flight snapshot
//! pinning, and `PlanCache` hit/miss verification.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    TestHarness, compose_two_branch_snapshot, default_crm_mock, make_persisted_store, send_oneshot,
    stream_rows,
};
use gap_ngc_sdk::mock::MockConnector;
use gap_ql::{PersistedOperationId, PersistedOperationStore};
use gap_transport::{Profile, TransportConfig};
use serde_json::{Value, json};
use tokio::sync::Notify;

// spec: GQL-003, GQL-050, OPS-002, RUN-001, RUN-002
#[tokio::test(start_paused = true)]
async fn e2e_two_branch_post_and_persisted_get_succeed_over_in_memory_grpc() {
    let query_doc = r#"query OrderAndLead($oid: ID!, $lid: ID!) { order(id: $oid) { id sku totalCents } lead(id: $lid) { id email region } }"#;
    let (store, hashes) = make_persisted_store(&[query_doc]);
    let hash = &hashes[0];

    let harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        common::default_shop_mock(),
        default_crm_mock(),
        Some(store),
        None,
    );

    // 1. POST /graphql across both `shop` and `crm` branches.
    let body = serde_json::to_vec(&json!({
        "query": query_doc,
        "operationName": "OrderAndLead",
        "variables": { "oid": "ord-42", "lid": "lead-7" }
    }))
    .expect("serialize request");

    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("build request");

    let resp = send_oneshot(&harness.router, req).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.content_type()
            .unwrap_or("")
            .starts_with("application/graphql-response+json"),
        "unexpected content-type: {:?}",
        resp.content_type()
    );
    assert!(
        resp.headers.contains_key("x-request-id"),
        "every response must carry x-request-id"
    );

    let payload = resp.json();
    assert!(
        payload.get("errors").is_none(),
        "unexpected errors: {payload}"
    );
    assert_eq!(payload["data"]["order"]["id"], "ord-42");
    assert_eq!(payload["data"]["order"]["sku"], "SKU-ALPHA");
    assert_eq!(payload["data"]["order"]["totalCents"], 2500);
    assert_eq!(payload["data"]["lead"]["id"], "lead-7");
    assert_eq!(payload["data"]["lead"]["email"], "ada@example.test");
    assert_eq!(payload["data"]["lead"]["region"], "US");
    assert_eq!(payload["extensions"]["gap"]["generation"], 1);
    assert_eq!(payload["extensions"]["gap"]["partial"], false);
    assert!(
        payload["extensions"]["gap"]["cost"]["planned"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(
        payload["extensions"]["gap"]["cost"]["actual"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(
        payload["extensions"]["gap"].get("tokensEstimate").is_none(),
        "tokensEstimate should be omitted when client sets no budget or feature flag"
    );

    // 2. GET /graphql with registered persisted operation (second execution hits PlanCache).
    let ext_enc = "%7B%22persistedQuery%22%3A%7B%22version%22%3A1%2C%22sha256Hash%22%3A%22"
        .to_owned()
        + hash
        + "%22%7D%7D";
    let vars_enc = "%7B%22oid%22%3A%22ord-99%22%2C%22lid%22%3A%22lead-9%22%7D";
    let uri =
        format!("/graphql?operationName=OrderAndLead&variables={vars_enc}&extensions={ext_enc}");

    let get_req = Request::builder()
        .method("GET")
        .uri(&uri)
        .header("accept", "application/graphql-response+json")
        .body(Body::empty())
        .expect("build GET request");

    let get_resp = send_oneshot(&harness.router, get_req).await;
    assert_eq!(get_resp.status, StatusCode::OK);
    let get_json = get_resp.json();
    assert_eq!(get_json["data"]["order"]["id"], "ord-99");
    assert_eq!(get_json["data"]["lead"]["id"], "lead-9");
    assert_eq!(get_json["extensions"]["gap"]["generation"], 1);

    // Verify PlanCache recorded 1 miss then 1 hit in both PlanCache and `/metrics`.
    assert_eq!(harness.state.plan_cache.misses(), 1);
    assert_eq!(harness.state.plan_cache.hits(), 1);

    let metrics_resp = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(Body::empty())
            .expect("metrics req"),
    )
    .await;
    assert_eq!(metrics_resp.status, StatusCode::OK);
    let metrics_text = metrics_resp.text();
    assert!(
        metrics_text.contains(r#"gap_plan_cache_total{result="miss"} 1"#),
        "metrics missing plan_cache miss=1:\n{metrics_text}"
    );
    assert!(
        metrics_text.contains(r#"gap_plan_cache_total{result="hit"} 1"#),
        "metrics missing plan_cache hit=1:\n{metrics_text}"
    );
}

// spec: GQL-050, GQL-051, RUN-060
#[tokio::test(start_paused = true)]
async fn gql_050_tokens_estimate_on_budget_and_response_too_large_aborts_data() {
    // 1. Setting a client budget triggers `extensions.gap.tokensEstimate` (GQL-050).
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));
    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "query": "{ order(id: \"ord-1\") { id sku totalCents } }",
                "extensions": { "gap": { "budget": { "costUnits": 500 } } }
            }))
            .expect("json"),
        ))
        .expect("req");

    let resp = send_oneshot(&harness.router, req).await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    let tokens_est = body["extensions"]["gap"]["tokensEstimate"]
        .as_u64()
        .expect("tokensEstimate must be present when client sets a budget (GQL-050)");
    assert!(tokens_est > 0);

    // 2. Exceeding `exec.max_response_bytes` during execution returns HTTP 200 with `"data": null`
    // and `GAP_RESPONSE_TOO_LARGE` in `errors[]` (ADR-023, GQL-050, RUN-060).
    let mut small_cap_cfg = TransportConfig::for_profile(Profile::Dev);
    small_cap_cfg.exec.max_response_bytes = 24;
    let tight_harness = TestHarness::new(small_cap_cfg);

    let too_large_req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "query": "{ orders(first: 2) { id sku totalCents } }"
            }))
            .expect("json"),
        ))
        .expect("req");

    let too_large_resp = send_oneshot(&tight_harness.router, too_large_req).await;
    assert_eq!(too_large_resp.status, StatusCode::OK);
    assert!(
        too_large_resp
            .content_type()
            .unwrap_or("")
            .starts_with("application/graphql-response+json")
    );
    let tl_json = too_large_resp.json();
    assert_eq!(tl_json.get("data"), Some(&Value::Null));
    assert_eq!(
        tl_json["errors"][0]["extensions"]["code"],
        "GAP_RESPONSE_TOO_LARGE"
    );
    assert!(
        tl_json["errors"][0]["extensions"]["correlationId"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert_eq!(tl_json["extensions"]["gap"]["generation"], 1);
    assert_eq!(tl_json["extensions"]["gap"]["partial"], false);
}

/// Custom persisted store that publishes Snapshot Generation 2 right inside `Operation` (after
/// `Gateway` has pinned Generation 1, before `Operation` validates or `Plan` plans) to catch any
/// double-pinning bug between `Gateway`, `Operation`, `Plan`, and `Exec` (`INV-01`).
struct MidFlightPublishingStore {
    doc: String,
    id: PersistedOperationId,
    handle: Arc<gap_snapshot::SnapshotHandle>,
    next_snap: Option<std::sync::Mutex<Option<gap_core::snapshot::Snapshot>>>,
    calls: AtomicUsize,
}

impl PersistedOperationStore for MidFlightPublishingStore {
    fn get_document(&self, id: &PersistedOperationId) -> Option<String> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(ref slot) = self.next_snap
            && let Some(snap2) = slot.lock().expect("test mutex").take()
        {
            let published = self.handle.publish(snap2).expect("publish gen 2");
            assert_eq!(published, 2);
        }
        (id == &self.id).then(|| self.doc.clone())
    }
}

// spec: INV-01, GQL-050, RUN-001
#[tokio::test(start_paused = true)]
async fn inv_01_in_flight_request_pins_generation_once_across_all_stages() {
    let entered_branch = Arc::new(Notify::new());
    let release_branch = Arc::new(Notify::new());

    let entered_clone = Arc::clone(&entered_branch);
    let release_clone = Arc::clone(&release_branch);

    let shop_mock = MockConnector::new().on_query_async(move |req, _scope| {
        let entered = Arc::clone(&entered_clone);
        let release = Arc::clone(&release_clone);
        async move {
            assert_eq!(req.operation, "shopVersion");
            entered.notify_one();
            release.notified().await;
            Ok(stream_rows(vec![json!({ "value": "served-from-gen-1" })]))
        }
    });

    let doc = "query PinnedGen { shopVersion }";
    let op_id = PersistedOperationId::from_document(doc);

    // Build initial harness first, then wrap with MidFlightPublishingStore sharing the same SnapshotHandle.
    let base_harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        shop_mock,
        default_crm_mock(),
        None,
        None,
    );

    let gen1 = base_harness.snapshot_handle.pin();
    assert_eq!(gen1.generation, 1);
    let gen2 = compose_two_branch_snapshot(&gen1, true);
    assert_eq!(gen2.generation, 2);
    let gen3 = compose_two_branch_snapshot(&gen2, false);
    assert_eq!(gen3.generation, 3);

    let publishing_store = Arc::new(MidFlightPublishingStore {
        doc: doc.to_owned(),
        id: op_id.clone(),
        handle: Arc::clone(&base_harness.snapshot_handle),
        next_snap: Some(std::sync::Mutex::new(Some(gen2))),
        calls: AtomicUsize::new(0),
    });

    let state = Arc::new(
        gap_transport::AppState::new(
            Arc::clone(&base_harness.snapshot_handle),
            Arc::clone(&base_harness.state.registry),
            Arc::clone(&base_harness.state.plan_cache),
            base_harness.state.config.clone(),
            Arc::clone(&base_harness.metrics),
        )
        .with_clock(Arc::clone(&base_harness.state.clock))
        .with_persisted_store(publishing_store.clone()),
    );
    let app = gap_transport::router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "extensions": {
                    "persistedQuery": { "version": 1, "sha256Hash": op_id.as_str() }
                }
            }))
            .expect("json"),
        ))
        .expect("req");

    let mut in_flight = tokio::spawn(async move { send_oneshot(&app, req).await });

    // Wait until the request has pinned Gen 1 in `Gateway`, triggered Gen 2 publish inside
    // `Operation`, planned, and entered `Exec` (`shop_mock`).
    tokio::select! {
        early = &mut in_flight => {
            panic!("request finished before entering branch mock: {:?}", early.map(|r| r.status));
        }
        () = entered_branch.notified() => {}
    }
    assert_eq!(publishing_store.calls.load(Ordering::Acquire), 1);
    assert_eq!(base_harness.snapshot_handle.generation(), 2);

    // Publish Generation 3 while `Exec` is mid-flight, before `Assemble` runs!
    let pub3 = base_harness
        .snapshot_handle
        .publish(gen3)
        .expect("publish gen 3");
    assert_eq!(pub3, 3);
    assert_eq!(base_harness.snapshot_handle.generation(), 3);

    release_branch.notify_one();
    let resp = in_flight.await.expect("join in-flight request");
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["data"]["shopVersion"], "served-from-gen-1");
    assert_eq!(
        body["extensions"]["gap"]["generation"], 1,
        "request that pinned generation 1 must report generation 1 even after gen 2 & gen 3 published"
    );
}
