//! Variables reach the connector as coerced against the operation's variable definitions
//! (GraphQL "CoerceVariableValues": declared defaults applied; `GQL-002`). NGC arguments travel as a protobuf
//! `Struct`, so numbers are compared as doubles.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestHarness, default_crm_mock, send_oneshot, stream_rows};
use gap_ngc_sdk::error::prost_struct_to_json_map;
use gap_ngc_sdk::mock::MockConnector;
use gap_transport::{Profile, TransportConfig};
use serde_json::{Value, json};

/// A shop connector that records the arguments of every `orders` / `order` call.
fn recording_shop(seen: Arc<Mutex<Vec<(String, Value)>>>) -> MockConnector {
    MockConnector::new().on_query(move |req, _scope| {
        let args = req
            .arguments
            .as_ref()
            .map(prost_struct_to_json_map)
            .map_or(Value::Null, Value::Object);
        seen.lock().unwrap().push((req.operation.clone(), args));
        Ok(stream_rows(vec![
            json!({ "id": "ord-1", "sku": "SKU-1", "totalCents": 1 }),
        ]))
    })
}

async fn post(harness: &TestHarness, body: Value) -> (StatusCode, Value) {
    let resp = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).expect("json")))
            .expect("req"),
    )
    .await;
    (resp.status, resp.json())
}

// spec: GQL-002
#[tokio::test(start_paused = true)]
async fn operation_variable_defaults_reach_the_connector() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        recording_shop(Arc::clone(&seen)),
        default_crm_mock(),
        None,
        None,
    );

    // Optional argument fed by a variable that has a default: the default must be sent.
    let (status, body) = post(
        &harness,
        json!({ "query": "query Q($n: Int = 1) { orders(first: $n) { id } }" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Required argument fed by a variable that has a default: the request must not fail.
    let (status, body) = post(
        &harness,
        json!({ "query": "query Q($id: ID! = \"7\") { order(id: $id) { id } }" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["errors"].is_null() || body["errors"] == json!([]),
        "{body}"
    );

    // A supplied value overrides the default.
    let (status, _) = post(
        &harness,
        json!({ "query": "query Q($n: Int = 1) { orders(first: $n) { id } }", "variables": { "n": 3 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0].0, "orders");
    assert_eq!(
        calls[0].1["first"].as_f64(),
        Some(1.0),
        "declared default of `$n` must be sent: {calls:?}"
    );
    assert_eq!(calls[1].0, "order");
    assert_eq!(
        calls[1].1["id"],
        json!("7"),
        "declared default of `$id` must be sent: {calls:?}"
    );
    assert_eq!(
        calls[2].1["first"].as_f64(),
        Some(3.0),
        "a supplied variable wins over the default: {calls:?}"
    );
}

// spec: GQL-002
#[tokio::test(start_paused = true)]
async fn undeclared_variables_are_not_forwarded_to_the_connector() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        recording_shop(Arc::clone(&seen)),
        default_crm_mock(),
        None,
        None,
    );
    let (status, body) = post(
        &harness,
        json!({ "query": "query Q($n: Int) { orders(first: $n) { id } }", "variables": { "n": 2, "smuggled": "x" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["first"].as_f64(), Some(2.0));
    assert!(calls[0].1.get("smuggled").is_none(), "{calls:?}");
}
