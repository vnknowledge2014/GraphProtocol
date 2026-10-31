//! Exhaustive HTTP status-code and media-type matrix tests (`Brief §3.5`, `§4`, `spec/02-gap-ql.md`
//! §10, `ADR-023`, and `third_party/specs/graphql-over-http/spec/GraphQLOverHTTP.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::{TestHarness, default_crm_mock, make_api_key, make_persisted_store, send_oneshot};
use gap_core::context::Principal;
use gap_core::error::{ErrorCode, GapError, NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::mock::MockConnector;
use gap_proto::ngc::v1::{QueryResponse, RowSetChunk, query_response};
use gap_transport::{AdmissionPolicy, Profile, TransportConfig};
use serde_json::{Value, json};

#[derive(Debug)]
struct StaticRefusalAdmission {
    code: ErrorCode,
    after_ms: Option<u32>,
}

impl AdmissionPolicy for StaticRefusalAdmission {
    fn check(&self, _principal: &Principal, _headers: &HeaderMap) -> Result<(), GapError> {
        Err(GapError::new(self.code, "shed by admission policy").with_retry(true, self.after_ms))
    }
}

fn assert_request_error_envelope(
    resp: &common::ObservedHttpResponse,
    expected_status: StatusCode,
    expected_media_prefix: &str,
    expected_code: &str,
) -> Value {
    assert_eq!(
        resp.status,
        expected_status,
        "body: {}",
        String::from_utf8_lossy(&resp.raw_body)
    );
    let ct = resp.content_type().unwrap_or("");
    assert!(
        ct.starts_with(expected_media_prefix),
        "expected content-type starting with `{expected_media_prefix}`, got `{ct}`"
    );
    if expected_media_prefix == "application/json" {
        assert!(
            !ct.contains("graphql-response+json"),
            "transport/auth error must NOT use application/graphql-response+json (ADR-023), got `{ct}`"
        );
    }
    let body = resp.json();
    assert!(
        body.get("data").is_none(),
        "request/transport errors MUST omit `data` completely (never `data: null`): {body}"
    );
    let errors = body["errors"]
        .as_array()
        .expect("errors array must be present");
    assert!(!errors.is_empty(), "errors array must be non-empty");
    assert_eq!(errors[0]["extensions"]["code"], expected_code);
    assert!(
        errors[0]["extensions"]["correlationId"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "error must carry extensions.correlationId: {body}"
    );
    body
}

// spec: GQL-003, GQL-050
#[tokio::test(start_paused = true)]
async fn status_400_on_malformed_json_and_graphql_syntax_error() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));

    // 400a: invalid JSON body
    let bad_json = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(b"{\"query\":".to_vec()))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &bad_json,
        StatusCode::BAD_REQUEST,
        "application/graphql-response+json",
        "GAP_PARSE_ERROR",
    );

    // 400b: valid JSON envelope with broken GraphQL document syntax
    let bad_gql = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "query { order(id: " })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &bad_gql,
        StatusCode::BAD_REQUEST,
        "application/graphql-response+json",
        "GAP_PARSE_ERROR",
    );
}

// spec: GQL-003, GQL-070
#[tokio::test(start_paused = true)]
async fn status_422_on_validation_coercion_operation_resolution_and_unsupported_subscription() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));

    // 422a: unknown field (`didYouMean` hint attached, `data` omitted)
    let unknown_field = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ ordr(id: \"1\") { id } }" }))
                    .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    let body_uf = assert_request_error_envelope(
        &unknown_field,
        StatusCode::UNPROCESSABLE_ENTITY,
        "application/graphql-response+json",
        "GAP_UNKNOWN_FIELD",
    );
    assert!(
        body_uf["errors"][0]["extensions"]["suggestion"]["didYouMean"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|v| v == "order")),
        "expected didYouMean to include `order`: {body_uf}"
    );

    // 422b: variable coercion error (`Int` variable given object/boolean or missing required var)
    let bad_var = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "query Q($n: Int!) { orders(first: $n) { id } }",
                    "variables": { "n": "not-an-int" }
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(bad_var.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad_var.json().get("data").is_none());

    // 422c: ambiguous multi-operation document without `operationName`
    let ambig_op = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "query A { shopVersion } query B { shopVersion }"
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &ambig_op,
        StatusCode::UNPROCESSABLE_ENTITY,
        "application/graphql-response+json",
        "GAP_VALIDATION_ERROR",
    );

    // 422d: `subscription` operation refused with `GAP_UNSUPPORTED_FEATURE` (Brief §1)
    let sub_resp = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "subscription { gapTaskEvents(taskId: \"t1\") { seq state } }"
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &sub_resp,
        StatusCode::UNPROCESSABLE_ENTITY,
        "application/graphql-response+json",
        "GAP_UNSUPPORTED_FEATURE",
    );

    // 422e: ad-hoc `GET /graphql?query=...` without persisted operation hash (`Brief §3.2`)
    let adhoc_get = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/graphql?query=%7BshopVersion%7D")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &adhoc_get,
        StatusCode::UNPROCESSABLE_ENTITY,
        "application/graphql-response+json",
        "GAP_OPERATION_NOT_ALLOWED",
    );

    // 422f: planner cost budget exceeded (`extensions.gap.budget.costUnits: 1`)
    let over_budget = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ orders(first: 10) { id sku totalCents } }",
                    "extensions": { "gap": { "budget": { "costUnits": 1 } } }
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &over_budget,
        StatusCode::UNPROCESSABLE_ENTITY,
        "application/graphql-response+json",
        "GAP_BUDGET_EXCEEDED",
    );
}

// spec: SEC-001, SEC-002, SEC-010
#[tokio::test(start_paused = true)]
async fn status_401_and_403_in_prod_profile() {
    let mut cfg = TransportConfig::for_profile(Profile::Prod);
    cfg.api_keys.push(make_api_key(
        "k-prod",
        "secret-prod-key-1",
        "acme",
        &["read"],
    ));
    let harness = TestHarness::new(cfg);

    // 401: missing credentials in `prod` -> 401 + `WWW-Authenticate: Bearer`
    let unauth = send_oneshot(
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
    assert_request_error_envelope(
        &unauth,
        StatusCode::UNAUTHORIZED,
        "application/json",
        "GAP_UNAUTHENTICATED",
    );
    assert_eq!(
        unauth
            .headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );

    // 403: authenticated in `prod`, valid query -> Authz stub denies everything in `prod` until WP-M6
    let forbidden = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret-prod-key-1")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &forbidden,
        StatusCode::FORBIDDEN,
        "application/json",
        "GAP_FORBIDDEN",
    );
}

// spec: GQL-003, OPS-002
#[tokio::test(start_paused = true)]
async fn status_413_415_405_406_and_404_transport_errors() {
    let mut cfg = TransportConfig::for_profile(Profile::Dev);
    cfg.limits.max_request_body_bytes = 16_384;

    let mut_doc = r#"mutation CreateIt { createOrder(sku: "S1") { id } }"#;
    let (store, hashes) = make_persisted_store(&[mut_doc]);
    let mut_hash = &hashes[0];

    let harness = TestHarness::with_mocks(
        cfg,
        common::default_shop_mock(),
        default_crm_mock(),
        Some(store),
        None,
    );

    // 413: body larger than `limits.max_request_body_bytes`
    let oversized = vec![b' '; 16_385];
    let r413 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(oversized))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r413,
        StatusCode::PAYLOAD_TOO_LARGE,
        "application/json",
        "GAP_COMPLEXITY_EXCEEDED",
    );

    // 415: wrong `Content-Type` on POST
    let r415 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "text/plain")
            .body(Body::from("{ shopVersion }"))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r415,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "application/json",
        "GAP_UNSUPPORTED_FEATURE",
    );

    // 405a: wrong HTTP verb (`DELETE /graphql`) -> 405 + `Allow`
    let r405_verb = send_oneshot(
        &harness.router,
        Request::builder()
            .method("DELETE")
            .uri("/graphql")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r405_verb,
        StatusCode::METHOD_NOT_ALLOWED,
        "application/json",
        "GAP_OPERATION_NOT_ALLOWED",
    );
    let allow_hdr = r405_verb
        .headers
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .expect("405 MUST include Allow header");
    assert!(allow_hdr.contains("GET") && allow_hdr.contains("POST"));

    // 405b: persisted mutation over HTTP `GET /graphql` -> 405 + `Allow: POST`
    let ext_enc = "%7B%22persistedQuery%22%3A%7B%22version%22%3A1%2C%22sha256Hash%22%3A%22"
        .to_owned()
        + mut_hash
        + "%22%7D%7D";
    let r405_mut = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri(format!("/graphql?extensions={ext_enc}"))
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r405_mut,
        StatusCode::METHOD_NOT_ALLOWED,
        "application/json",
        "GAP_OPERATION_NOT_ALLOWED",
    );
    assert_eq!(
        r405_mut.headers.get("allow").and_then(|v| v.to_str().ok()),
        Some("POST")
    );

    // 405c: `GET /graphql/stream` (`POST`-only SSE alias) -> 405 + `Allow: POST`
    let r405_stream = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/graphql/stream")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r405_stream,
        StatusCode::METHOD_NOT_ALLOWED,
        "application/json",
        "GAP_OPERATION_NOT_ALLOWED",
    );
    assert_eq!(
        r405_stream
            .headers
            .get("allow")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );

    // 406: unacceptable `Accept` header
    let r406 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("accept", "image/png")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r406,
        StatusCode::NOT_ACCEPTABLE,
        "application/json",
        "GAP_UNSUPPORTED_FEATURE",
    );

    // 404: unknown URI path
    let r404 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("GET")
            .uri("/unknown-route")
            .body(Body::empty())
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &r404,
        StatusCode::NOT_FOUND,
        "application/json",
        "GAP_VALIDATION_ERROR",
    );
}

// spec: GQL-003, RUN-002
#[tokio::test(start_paused = true)]
async fn status_429_and_503_with_retry_after_header() {
    // 429: GAP_RATE_LIMITED with after_ms = 2500 -> Retry-After: 3
    let h429 = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(Arc::new(StaticRefusalAdmission {
            code: ErrorCode::RateLimited,
            after_ms: Some(2_500),
        })),
    );
    let r429 = send_oneshot(
        &h429.router,
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
    let body429 = assert_request_error_envelope(
        &r429,
        StatusCode::TOO_MANY_REQUESTS,
        "application/json",
        "GAP_RATE_LIMITED",
    );
    assert_eq!(
        r429.headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        body429["errors"][0]["extensions"]["retry"]["retryable"],
        true
    );
    assert_eq!(body429["errors"][0]["extensions"]["retry"]["afterMs"], 2500);

    // 503: GAP_OVERLOADED with after_ms = 1200 -> Retry-After: 2
    let h503 = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(Arc::new(StaticRefusalAdmission {
            code: ErrorCode::Overloaded,
            after_ms: Some(1_200),
        })),
    );
    let r503 = send_oneshot(
        &h503.router,
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
    assert_request_error_envelope(
        &r503,
        StatusCode::SERVICE_UNAVAILABLE,
        "application/json",
        "GAP_OVERLOADED",
    );
    assert_eq!(
        r503.headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );

    // Exact ceiling boundary: `1_000 ms -> 1`, `1_001 ms -> 2`, `None -> 1`.
    assert_eq!(gap_transport::retry_after_seconds(Some(1_000)), 1);
    assert_eq!(gap_transport::retry_after_seconds(Some(1_001)), 2);
    assert_eq!(gap_transport::retry_after_seconds(None), 1);
}

// spec: GQL-003, GQL-050, GQL-051
#[tokio::test(start_paused = true)]
async fn status_200_with_partial_errors_and_legacy_application_json_accept_fallback() {
    // `crm` fails with `UPSTREAM_ERROR` while `shop` succeeds -> HTTP 200 with `partial: true`,
    // `data.order` populated, `data.lead == null`, and `errors[0].extensions.code == "GAP_BRANCH_ERROR"`.
    let failing_crm = MockConnector::new().on_query(|_req, _scope| {
        let proto_err = gap_proto::ngc::v1::NgcError {
            code: i32::from(NgcErrorCode::UpstreamError),
            message: "crm upstream unavailable".to_owned(),
            retryable: true,
            retry_after_ms: 500,
            details: None,
        };
        let _ = NgcErrorInfo::new(NgcErrorCode::UpstreamError, "unused");
        let stream: gap_ngc_sdk::connector::BoxStream<QueryResponse> =
            Box::pin(futures_util::stream::iter(vec![
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: Vec::new(),
                        last_for_index: true,
                        error: Some(proto_err),
                    })),
                }),
                Ok(common::complete_response()),
            ]));
        Ok(stream)
    });

    let harness = TestHarness::with_mocks(
        TransportConfig::for_profile(Profile::Dev),
        common::default_shop_mock(),
        failing_crm,
        None,
        None,
    );

    // 1. Default `application/graphql-response+json`
    let r200_partial = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ order(id: \"ord-1\") { id sku } lead(id: \"lead-1\") { id email } }"
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(r200_partial.status, StatusCode::OK);
    assert!(
        r200_partial
            .content_type()
            .unwrap_or("")
            .starts_with("application/graphql-response+json")
    );
    let p_json = r200_partial.json();
    assert_eq!(p_json["data"]["order"]["id"], "ord-1");
    assert_eq!(p_json["data"]["lead"], Value::Null);
    assert_eq!(p_json["extensions"]["gap"]["partial"], true);
    assert_eq!(p_json["extensions"]["gap"]["generation"], 1);
    assert_eq!(
        p_json["errors"][0]["extensions"]["code"],
        "GAP_BRANCH_ERROR"
    );
    assert_eq!(p_json["errors"][0]["path"], json!(["lead"]));
    assert!(
        p_json["errors"][0]["extensions"]["correlationId"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    // 2. Legacy `Accept: application/json` fallback (`GraphQLOverHTTP.md` L452-458):
    //    - 2xx responses use `Content-Type: application/json`
    //    - 400 / 422 request errors keep status 400 / 422 and `Content-Type: application/graphql-response+json`!
    let legacy_200 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ order(id: \"ord-1\") { id } lead(id: \"lead-1\") { id } }"
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(legacy_200.status, StatusCode::OK);
    assert!(
        legacy_200
            .content_type()
            .unwrap_or("")
            .starts_with("application/json"),
        "expected application/json on 200 with legacy Accept, got {:?}",
        legacy_200.content_type()
    );
    assert!(
        !legacy_200
            .content_type()
            .unwrap_or("")
            .contains("graphql-response+json")
    );
    assert_eq!(legacy_200.json()["extensions"]["gap"]["partial"], true);

    let legacy_400 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &legacy_400,
        StatusCode::BAD_REQUEST,
        "application/graphql-response+json",
        "GAP_PARSE_ERROR",
    );

    let legacy_422 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ unknownRoot }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_request_error_envelope(
        &legacy_422,
        StatusCode::UNPROCESSABLE_ENTITY,
        "application/graphql-response+json",
        "GAP_UNKNOWN_FIELD",
    );
}
