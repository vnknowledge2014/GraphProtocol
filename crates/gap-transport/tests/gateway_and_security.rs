//! Gateway normalization, `X-Request-Id` echo/mint/reject, streaming `413` body-cap enforcement
//! (`CountingBody`), JSON depth cap, `max_variables` cap, `dev`/`prod` API-key authn, constant-time
//! `Blake3ApiKeyVerifier` structural verification, `SEC-010` tenant/principal isolation, `INV-15`
//! canary non-leakage, and `INV-02` lock-free source audit.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use bytes::Bytes;
use common::{TestHarness, default_crm_mock, make_api_key, send_oneshot};
use futures_util::Stream;
use gap_core::context::Principal;
use gap_core::error::GapError;
use gap_core::ids::TenantId;
use gap_testkit::canary::CanarySink;
use gap_testkit::tracing_capture::TracingCapture;
use gap_transport::{
    AdmissionPolicy, Blake3ApiKeyVerifier, Profile, TransportConfig, VerificationStats,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

struct CountingChunkStream {
    chunk_size: usize,
    total_chunks: usize,
    emitted_chunks: usize,
    bytes_polled: Arc<AtomicUsize>,
}

impl Stream for CountingChunkStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.emitted_chunks >= self.total_chunks {
            return Poll::Ready(None);
        }
        self.emitted_chunks += 1;
        self.bytes_polled
            .fetch_add(self.chunk_size, Ordering::AcqRel);
        let chunk = Bytes::from(vec![b' '; self.chunk_size]);
        Poll::Ready(Some(Ok(chunk)))
    }
}

#[derive(Debug, Default)]
struct PrincipalRecorder {
    observed: Mutex<Vec<Principal>>,
}

impl AdmissionPolicy for PrincipalRecorder {
    fn check(&self, principal: &Principal, _headers: &HeaderMap) -> Result<(), GapError> {
        self.observed
            .lock()
            .expect("recorder mutex")
            .push(principal.clone());
        Ok(())
    }
}

// spec: OPS-010, GQL-003
#[tokio::test(start_paused = true)]
async fn gateway_request_id_echo_mint_and_reject_invalid_or_oversized() {
    let harness = TestHarness::new(TransportConfig::for_profile(Profile::Dev));

    // 1. Valid alphanumeric-hyphen `X-Request-Id` (`<= 64` chars) is echoed verbatim on response
    //    header AND in error `extensions.correlationId`.
    let valid_id = "req-alpha-0019-valid";
    let resp_echo = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-request-id", valid_id)
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ unknownField }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_echo.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        resp_echo
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some(valid_id)
    );
    assert_eq!(
        resp_echo.json()["errors"][0]["extensions"]["correlationId"],
        valid_id
    );

    // 2. Valid UUID `X-Request-Id` is echoed verbatim.
    let valid_uuid = "018bcfe5-6c01-7010-8101-010101010101";
    let resp_uuid = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-request-id", valid_uuid)
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(
        resp_uuid
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some(valid_uuid)
    );

    // 3. Exact boundary lengths `1` and `64` are echoed verbatim; `65` is rejected -> UUIDv7!
    for boundary_len in [1_usize, 64] {
        let exact_id = "a".repeat(boundary_len);
        let resp_exact = send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .header("x-request-id", &exact_id)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
                ))
                .expect("req"),
        )
        .await;
        assert_eq!(
            resp_exact
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(exact_id.as_str())
        );
    }

    let oversized_id = "a".repeat(65);
    let resp_over = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-request-id", &oversized_id)
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    let minted_over = resp_over
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("minted x-request-id");
    assert_ne!(minted_over, oversized_id);
    let parsed_over = Uuid::parse_str(minted_over).expect("minted ID must be a valid UUID");
    assert_eq!(parsed_over.get_version_num(), 7);

    // 4. Invalid characters (`;` / spaces / underscores) are rejected and replaced with UUIDv7.
    let hostile_id = "bad_id;drop-table";
    let resp_bad = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-request-id", hostile_id)
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    let minted_bad = resp_bad
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("minted x-request-id");
    assert_ne!(minted_bad, hostile_id);
    assert_eq!(
        Uuid::parse_str(minted_bad).expect("UUID").get_version_num(),
        7
    );
}

// spec: GQL-003, RUN-002
#[tokio::test(start_paused = true)]
async fn gateway_enforces_streaming_body_cap_without_buffering_full_stream_and_caps_json_depth_and_variables()
 {
    let mut cfg = TransportConfig::for_profile(Profile::Dev);
    cfg.limits.max_request_body_bytes = 16_384; // 16 KiB
    cfg.max_json_depth = 16;
    cfg.limits.max_variables = 4;
    let harness = TestHarness::new(cfg);

    // 1. Streaming 40 KiB body (10 chunks x 4096 B) against 16 KiB cap MUST stop polling as soon as
    //    chunk 5 pushes accumulated bytes (`20_480`) past `16_384` — never buffering all `40_960` B!
    let bytes_polled = Arc::new(AtomicUsize::new(0));
    let stream_body = Body::from_stream(CountingChunkStream {
        chunk_size: 4_096,
        total_chunks: 10,
        emitted_chunks: 0,
        bytes_polled: Arc::clone(&bytes_polled),
    });

    let resp_413 = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(stream_body)
            .expect("req"),
    )
    .await;
    assert_eq!(resp_413.status, StatusCode::PAYLOAD_TOO_LARGE);
    let polled = bytes_polled.load(Ordering::Acquire);
    assert!(
        polled <= 20_480,
        "streaming body cap must stop reading immediately upon exceeding 16_384 B, but polled {polled} B"
    );

    // 2. Deeply nested JSON exceeding `max_json_depth` (`16`) -> `400 GAP_PARSE_ERROR`.
    let deep_json = format!(
        r#"{{"query":"{{ shopVersion }}","extensions":{{"nested":{}}}}}"#,
        "[".repeat(24) + &"]".repeat(24)
    );
    let resp_deep = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(deep_json))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_deep.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        resp_deep.json()["errors"][0]["extensions"]["code"],
        "GAP_PARSE_ERROR"
    );

    // 3. Variable map larger than `limits.max_variables` (`4`) -> `422 GAP_VALIDATION_ERROR`.
    let mut vars = Map::new();
    for i in 0..6 {
        vars.insert(format!("v{i}"), Value::from(i));
    }
    let resp_vars = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ shopVersion }",
                    "variables": vars
                }))
                .expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_vars.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        resp_vars.json()["errors"][0]["extensions"]["code"],
        "GAP_VALIDATION_ERROR"
    );

    // 4. Exact byte-cap boundary: `16_384` B padded JSON body succeeds (`200`), `16_385` B fails (`413`).
    let prefix = br#"{"query":"{ shopVersion }","extensions":{"pad":""#;
    let suffix = br#""}}"#;
    let pad_len_ok = 16_384 - prefix.len() - suffix.len();
    let mut exact_16k = Vec::with_capacity(16_384);
    exact_16k.extend_from_slice(prefix);
    exact_16k.resize(prefix.len() + pad_len_ok, b'x');
    exact_16k.extend_from_slice(suffix);
    assert_eq!(exact_16k.len(), 16_384);

    let resp_exact_16k = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from(exact_16k))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_exact_16k.status, StatusCode::OK);
    assert_eq!(resp_exact_16k.json()["data"]["shopVersion"], "shop-gen1");

    // 5. Stalled slow-body stream times out cleanly (`408 GAP_DEADLINE_EXCEEDED`, no `data`).
    struct StalledBodyStream {
        sent_first: bool,
    }
    impl Stream for StalledBodyStream {
        type Item = Result<Bytes, Infallible>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if !self.sent_first {
                self.sent_first = true;
                return Poll::Ready(Some(Ok(Bytes::from_static(b"{\"query\":"))));
            }
            Poll::Pending
        }
    }
    let mut slow_cfg = TransportConfig::for_profile(Profile::Dev);
    slow_cfg.default_deadline = std::time::Duration::from_millis(200);
    slow_cfg.max_deadline = std::time::Duration::from_millis(500);
    let slow_harness = TestHarness::new(slow_cfg);
    let resp_slow = send_oneshot(
        &slow_harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .body(Body::from_stream(StalledBodyStream { sent_first: false }))
            .expect("req"),
    )
    .await;
    assert_eq!(resp_slow.status, StatusCode::REQUEST_TIMEOUT);
    let slow_json = resp_slow.json();
    assert!(slow_json.get("data").is_none());
    assert_eq!(
        slow_json["errors"][0]["extensions"]["code"],
        "GAP_DEADLINE_EXCEEDED"
    );
}

// spec: SEC-001, SEC-002, SEC-010, INV-15
#[tokio::test(start_paused = true)]
async fn authn_dev_and_prod_api_keys_constant_time_verifier_and_sec_010_tenant_isolation() {
    let raw_key_1 = "gap-canary-secret-api-key-acme-91a2b3c4d5e6".to_owned();
    let raw_key_2 = "gap-canary-secret-api-key-globex-71f2e3d4c5b6".to_owned();
    let raw_key_3 = "gap-canary-secret-api-key-initech-51a9b8c7d6e5".to_owned();
    let bad_canary = "gap-canary-secret-wrong-api-key-001122334455".to_owned();
    let sinks = [
        CanarySink::new(&raw_key_1).expect("canary 1"),
        CanarySink::new(&raw_key_2).expect("canary 2"),
        CanarySink::new(&raw_key_3).expect("canary 3"),
        CanarySink::new(&bad_canary).expect("canary bad"),
    ];

    let rec_1 = make_api_key("key-acme", &raw_key_1, "acme", &["read", "write"]);
    let rec_2 = make_api_key("key-globex", &raw_key_2, "globex", &["read"]);
    let rec_3 = make_api_key("key-initech", &raw_key_3, "initech", &["read"]);
    let records = vec![rec_1.clone(), rec_2.clone(), rec_3.clone()];

    // 1. Structural constant-time proof: `Blake3ApiKeyVerifier` inspects ALL 3 records and compares
    //    ALL `32 * 3 = 96` digest bytes whether matching first, middle, last, or missing!
    let verifier = Blake3ApiKeyVerifier;
    let expected_stats = VerificationStats {
        records_inspected: 3,
        bytes_compared: 96,
    };

    let (m1, s1) = verifier.verify_with_stats(&raw_key_1, &records);
    assert_eq!(m1.map(|r| r.id.as_str()), Some("key-acme"));
    assert_eq!(s1, expected_stats);

    let (m2, s2) = verifier.verify_with_stats(&raw_key_2, &records);
    assert_eq!(m2.map(|r| r.id.as_str()), Some("key-globex"));
    assert_eq!(s2, expected_stats);

    let (m3, s3) = verifier.verify_with_stats(&raw_key_3, &records);
    assert_eq!(m3.map(|r| r.id.as_str()), Some("key-initech"));
    assert_eq!(s3, expected_stats);

    // Same prefix as key 1 except final character -> still compares all 96 bytes and returns None.
    let wrong_key = format!("{raw_key_1}-wrong");
    let (m_miss, s_miss) = verifier.verify_with_stats(&wrong_key, &records);
    assert!(m_miss.is_none());
    assert_eq!(s_miss, expected_stats);

    // 2. `Debug` formatting on `ApiKeyRecord` and `TransportConfig` redacts key digests (`INV-15`).
    let mut dev_cfg = TransportConfig::for_profile(Profile::Dev);
    dev_cfg.default_tenant = TenantId::new("default").expect("default tenant");
    dev_cfg.api_keys = records.clone();
    assert!(dev_cfg.validate().is_ok());

    let debug_cfg = format!("{dev_cfg:?}");
    assert!(!debug_cfg.contains(&rec_1.blake3_hex));
    for sink in &sinks {
        sink.check_debug(&dev_cfg)
            .expect("TransportConfig Debug must not leak canary");
    }

    // 3. `dev` profile:
    //    - no credentials -> anonymous principal (`tenant = default`, `scopes = {"read"}`)
    //    - valid `Authorization: Bearer` -> key's principal (`tenant = acme`)
    //    - valid `X-API-Key` -> key's principal (`tenant = globex`)
    //    - wrong key -> `401 GAP_UNAUTHENTICATED` even in `dev`!
    //    - `X-Tenant: evil-tenant` and body `"tenant": "evil-tenant"` are completely ignored (`SEC-010`)!
    let recorder = Arc::new(PrincipalRecorder::default());
    let harness = TestHarness::with_mocks(
        dev_cfg,
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(recorder.clone()),
    );

    let capture_all = TracingCapture::new();
    let _thread_guard = capture_all.enter_guard();
    tracing::callsite::rebuild_interest_cache();

    let anon_resp = tracing::instrument::WithSubscriber::with_subscriber(
        send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .header("x-tenant", "evil-tenant")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "query": "{ shopVersion }",
                        "tenant": "evil-tenant",
                        "principal": "evil-admin",
                        "extensions": { "gap": { "tenant": "evil-tenant" } }
                    }))
                    .expect("json"),
                ))
                .expect("req"),
        ),
        capture_all.subscriber(),
    )
    .await;
    assert_eq!(anon_resp.status, StatusCode::OK);

    let bearer_resp = tracing::instrument::WithSubscriber::with_subscriber(
        send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {raw_key_1}"))
                .header("x-tenant", "evil-tenant")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "query": "{ shopVersion }",
                        "tenant": "evil-tenant"
                    }))
                    .expect("json"),
                ))
                .expect("req"),
        ),
        capture_all.subscriber(),
    )
    .await;
    assert_eq!(bearer_resp.status, StatusCode::OK);

    let x_api_resp = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-api-key", &raw_key_2)
            .header("x-tenant", "evil-tenant")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(x_api_resp.status, StatusCode::OK);

    let observed = recorder.observed.lock().expect("recorder").clone();
    assert_eq!(observed.len(), 3);
    assert_eq!(observed[0].tenant.as_str(), "default");
    assert!(observed[0].scopes.contains("read"));
    assert_eq!(observed[0].scopes.len(), 1);

    assert_eq!(observed[1].id, "key-acme");
    assert_eq!(observed[1].tenant.as_str(), "acme");
    assert!(observed[1].scopes.contains("write"));

    assert_eq!(observed[2].id, "key-globex");
    assert_eq!(observed[2].tenant.as_str(), "globex");

    // Wrong API key in `dev` -> `401` + `WWW-Authenticate: Bearer`, and canary never leaks!
    let bad_dev_resp = tracing::instrument::WithSubscriber::with_subscriber(
        send_oneshot(
            &harness.router,
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {bad_canary}"))
                .body(Body::from(
                    serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
                ))
                .expect("req"),
        ),
        capture_all.subscriber(),
    )
    .await;
    assert_eq!(bad_dev_resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        bad_dev_resp
            .headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    for sink in &sinks {
        sink.check_report_text(&bad_dev_resp.text())
            .expect("401 response body must never leak canary");
    }

    // Simultaneous `Authorization` + `X-API-Key` is rejected as ambiguous (`401 GAP_UNAUTHENTICATED`).
    let dual_creds = send_oneshot(
        &harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {raw_key_1}"))
            .header("x-api-key", &raw_key_2)
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(dual_creds.status, StatusCode::UNAUTHORIZED);

    // 4. `prod` profile: missing key -> 401, wrong key -> 401, good key -> passes authn (reaches Authz 403).
    let mut prod_cfg = TransportConfig::for_profile(Profile::Prod);
    prod_cfg.api_keys = records;
    let prod_recorder = Arc::new(PrincipalRecorder::default());
    let prod_harness = TestHarness::with_mocks(
        prod_cfg,
        common::default_shop_mock(),
        default_crm_mock(),
        None,
        Some(prod_recorder.clone()),
    );

    let prod_missing = send_oneshot(
        &prod_harness.router,
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
    assert_eq!(prod_missing.status, StatusCode::UNAUTHORIZED);

    let prod_bad = send_oneshot(
        &prod_harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-api-key", &bad_canary)
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(prod_bad.status, StatusCode::UNAUTHORIZED);

    let prod_good = send_oneshot(
        &prod_harness.router,
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header("content-type", "application/json")
            .header("x-api-key", &raw_key_3)
            .header("x-tenant", "evil-tenant")
            .body(Body::from(
                serde_json::to_vec(&json!({ "query": "{ shopVersion }" })).expect("json"),
            ))
            .expect("req"),
    )
    .await;
    assert_eq!(prod_good.status, StatusCode::FORBIDDEN);
    let prod_obs = prod_recorder.observed.lock().expect("prod recorder");
    assert_eq!(prod_obs.len(), 1);
    assert_eq!(prod_obs[0].id, "key-initech");
    assert_eq!(prod_obs[0].tenant.as_str(), "initech");

    // Verify zero canary secrets in any captured span or event across all traced calls (`INV-15`).
    assert!(
        !capture_all.spans().is_empty(),
        "expected spans captured across authenticated calls"
    );
    for sink in &sinks {
        sink.check_tracing(&capture_all)
            .expect("captured spans and events must never leak canary");
    }
}

// spec: INV-02
#[test]
fn inv_02_gap_transport_source_contains_zero_locks_on_request_path() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        "std::sync::Mutex",
        "std::sync::RwLock",
        "parking_lot::Mutex",
        "parking_lot::RwLock",
        "tokio::sync::Mutex",
        "tokio::sync::RwLock",
        "DashMap",
    ];
    for entry in std::fs::read_dir(&src_dir).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).expect("read rust source");
        let prod_code = content.split("#[cfg(test)]").next().unwrap_or(&content);
        for token in forbidden {
            assert!(
                !prod_code.contains(token),
                "{} must not use `{token}` on the request path (INV-02)",
                path.display()
            );
        }
    }
}

mod proptests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::http::{HeaderMap, HeaderValue};
    use gap_transport::{
        Blake3ApiKeyVerifier, VerificationStats,
        gateway::{is_valid_request_id, resolve_request_id},
    };
    use proptest::prelude::*;
    use uuid::Uuid;

    use crate::common::make_api_key;

    proptest! {
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            ..ProptestConfig::with_cases(48)
        })]

        // spec: OPS-010, GQL-003
        #[test]
        fn prop_request_id_echoes_valid_slugs_and_mints_uuidv7_on_invalid(
            valid_slug in "[A-Za-z0-9-]{1,64}",
            oversized in "[A-Za-z0-9-]{65,128}",
        ) {
            prop_assert!(is_valid_request_id(&valid_slug));
            let mut h_ok = HeaderMap::new();
            h_ok.insert("x-request-id", HeaderValue::from_str(&valid_slug).expect("ascii"));
            prop_assert_eq!(resolve_request_id(&h_ok), valid_slug);

            prop_assert!(!is_valid_request_id(&oversized));
            let mut h_over = HeaderMap::new();
            h_over.insert("x-request-id", HeaderValue::from_str(&oversized).expect("ascii"));
            let minted = resolve_request_id(&h_over);
            let parsed = Uuid::parse_str(&minted).expect("minted UUIDv7");
            prop_assert_eq!(parsed.get_version_num(), 7);
        }

        // spec: SEC-001, SEC-010
        #[test]
        fn prop_blake3_verifier_always_inspects_all_records_and_bytes(
            candidate in ".*",
        ) {
            let records = vec![
                make_api_key("k1", "prop-fixed-secret-1", "acme", &["read"]),
                make_api_key("k2", "prop-fixed-secret-2", "globex", &["read"]),
                make_api_key("k3", "prop-fixed-secret-3", "initech", &["read"]),
            ];
            let (matched, stats) = Blake3ApiKeyVerifier.verify_with_stats(&candidate, &records);
            prop_assert_eq!(
                stats,
                VerificationStats {
                    records_inspected: 3,
                    bytes_compared: 96,
                }
            );
            if candidate != "prop-fixed-secret-1"
                && candidate != "prop-fixed-secret-2"
                && candidate != "prop-fixed-secret-3"
            {
                prop_assert!(matched.is_none());
            }
        }
    }
}
