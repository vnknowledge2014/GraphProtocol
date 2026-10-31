//! GraphQL null/error propagation matrix with golden JSON, all 14 `NgcErrorCode` mappings,
//! `INV-15` canary credential scrubbing, and `RUN-061` `ResponseRedactor` seam tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream;
use gap_core::clock::ManualClock;
use gap_core::error::{ErrorCode, NgcErrorCode, NgcErrorInfo, PathSegment};
use gap_core::snapshot::Snapshot;
use gap_exec::{
    ConnectorRegistry, ExecLayer, ExecRequest, ResponseRedactor, map_ngc_error,
    sanitize_connector_message,
};
use gap_ngc_sdk::error::ngc_error_to_proto;
use gap_ngc_sdk::mock::MockConnector;
use gap_proto::ngc::v1::{QueryResponse, RowSetChunk, query_response};
use serde_json::json;
use tokio::time::Instant;

use common::{
    build_two_branch_snapshot, compile_plan, complete_response, crm_uid, make_in_process_branch,
    shop_uid, spawn_grpc_branch, stream_rows,
};

// spec: RUN-060, GQL-051, RUN-024
#[tokio::test(start_paused = true)]
async fn null_propagation_matrix_matches_golden_graphql_json() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    // Cell 1: Absent nullable scalar field -> null field, 0 errors, partial = false
    {
        let shop = MockConnector::new().on_query(|_req, _scope| {
            Ok(stream_rows(vec![json!({ "id": "c-1", "name": "Ada" })]))
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ customer(id: "c-1") { id name nickname } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            golden["data"],
            json!({ "customer": { "id": "c-1", "name": "Ada", "nickname": null } })
        );
        assert!(golden.get("errors").is_none());
        assert_eq!(golden["extensions"]["partial"], json!(false));
    }

    // Cell 2: Absent non-null scalar (`name: String!`) on nullable root object (`customer`) -> nulls `customer`, sibling `ping` survives, partial = true
    {
        let shop = MockConnector::new().on_query(|req, _scope| match req.operation.as_str() {
            "ping" => Ok(stream_rows(vec![json!({ "value": "pong" })])),
            "customer" => Ok(stream_rows(vec![json!({ "id": "c-1" })])), // missing non-null `name`
            _ => unreachable!(),
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ ping customer(id: "c-1") { id name } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(golden["data"], json!({ "ping": "pong", "customer": null }));
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(golden["errors"][0]["path"], json!(["customer", "name"]));
        assert_eq!(golden["extensions"]["partial"], json!(true));
    }

    // Cell 3: Absent non-null scalar (`sku: String!`) on non-null root object (`strictOrder: shop_Order!`) -> bubbles to top-level `data: null`, partial = false
    {
        let shop = MockConnector::new().on_query(|req, _scope| match req.operation.as_str() {
            "ping" => Ok(stream_rows(vec![json!({ "value": "pong" })])),
            "strictOrder" => Ok(stream_rows(vec![json!({ "id": "o-1", "totalCents": 100 })])), // missing `sku`
            _ => unreachable!(),
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ ping strictOrder(id: "o-1") { id sku } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(golden["data"], serde_json::Value::Null);
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(golden["errors"][0]["path"], json!(["strictOrder", "sku"]));
        assert_eq!(golden["extensions"]["partial"], json!(false));
    }

    // Cell 4: Deep nested bubbling stops at the NEAREST nullable ancestor (`lead.company` is nullable, `company.hq` is `crm_Address!`, `hq.city` is `String!` missing) -> `company: null`, `lead` stays intact!
    {
        let crm = MockConnector::new().on_query(|_req, _scope| {
            Ok(stream_rows(vec![json!({
                "id": "l-1",
                "email": "ada@example.com",
                "company": {
                    "id": "co-1",
                    "name": "Babbage Ltd",
                    "hq": { "zip": "EC1A" } // missing non-null `city`!
                }
            })]))
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(crm_uid(), make_in_process_branch(crm, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ lead(id: "l-1") { id email company { id name hq { city zip } } } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            golden["data"],
            json!({
                "lead": {
                    "id": "l-1",
                    "email": "ada@example.com",
                    "company": null
                }
            })
        );
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(
            golden["errors"][0]["path"],
            json!(["lead", "company", "hq", "city"])
        );
        assert_eq!(golden["extensions"]["partial"], json!(true));
    }

    // Cell 5: `[shop_Order]!` (`sparseOrders`: non-null list of nullable items) -> item 1 missing `sku: String!` nulls ONLY item 1!
    {
        let shop = MockConnector::new().on_query(|_req, _scope| {
            Ok(stream_rows(vec![
                json!({ "id": "o-0", "sku": "OK-0", "totalCents": 10 }),
                json!({ "id": "o-1", "totalCents": 20 }), // missing non-null `sku`
                json!({ "id": "o-2", "sku": "OK-2", "totalCents": 30 }),
            ]))
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ sparseOrders { id sku } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            golden["data"],
            json!({
                "sparseOrders": [
                    { "id": "o-0", "sku": "OK-0" },
                    null,
                    { "id": "o-2", "sku": "OK-2" }
                ]
            })
        );
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(
            golden["errors"][0]["path"],
            json!(["sparseOrders", 1, "sku"])
        );
        assert_eq!(golden["extensions"]["partial"], json!(true));
    }

    // Cell 6: `[shop_Order!]` (`nullableOrders`: nullable list of non-null items) -> item 1 missing `sku` bubbles to `nullableOrders: null`!
    {
        let shop = MockConnector::new().on_query(|req, _scope| match req.operation.as_str() {
            "ping" => Ok(stream_rows(vec![json!({ "value": "alive" })])),
            "nullableOrders" => Ok(stream_rows(vec![
                json!({ "id": "o-0", "sku": "OK-0", "totalCents": 10 }),
                json!({ "id": "o-1", "totalCents": 20 }), // missing `sku`
            ])),
            _ => unreachable!(),
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ ping nullableOrders { id sku } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            golden["data"],
            json!({ "ping": "alive", "nullableOrders": null })
        );
        assert_eq!(
            golden["errors"][0]["path"],
            json!(["nullableOrders", 1, "sku"])
        );
        assert_eq!(golden["extensions"]["partial"], json!(true));
    }

    // Cell 7: `[shop_Order!]!` (`orders`: non-null list of non-null items) -> item 1 missing `sku` bubbles through `orders` to top-level `data: null`!
    {
        let shop = MockConnector::new().on_query(|req, _scope| match req.operation.as_str() {
            "ping" => Ok(stream_rows(vec![json!({ "value": "alive" })])),
            "orders" => Ok(stream_rows(vec![
                json!({ "id": "o-0", "sku": "OK-0", "totalCents": 10 }),
                json!({ "id": "o-1", "totalCents": 20 }), // missing `sku`
            ])),
            _ => unreachable!(),
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock.clone());
        let plan = compile_plan(
            &snapshot,
            r#"{ ping orders { id sku } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        let golden = serde_json::to_value(&resp).unwrap();
        assert_eq!(golden["data"], serde_json::Value::Null);
        assert_eq!(golden["errors"][0]["path"], json!(["orders", 1, "sku"]));
        assert_eq!(golden["extensions"]["partial"], json!(false));
    }

    // Cell 8: Per-chunk `RowSetChunk.error` on nullable `lead` -> `lead: null`, mapped error at `["lead"]`, `partial: true`
    {
        let shop = MockConnector::new().on_query(|_req, _scope| {
            Ok(stream_rows(vec![json!({ "value": "ok" })]))
        });
        let crm = MockConnector::new().on_query(|_req, _scope| {
            let chunk_err =
                NgcErrorInfo::new(NgcErrorCode::PermissionDenied, "row policy denied lead");
            let resp_item = QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: Vec::new(),
                    last_for_index: true,
                    error: Some(ngc_error_to_proto(&chunk_err)),
                })),
            };
            Ok(Box::pin(stream::iter(vec![
                Ok(resp_item),
                Ok(complete_response()),
            ])))
        });
        let exec = ExecLayer::new(Arc::new(
            ConnectorRegistry::builder()
                .register(shop_uid(), make_in_process_branch(shop, clock.clone()), 16)
                .register(crm_uid(), make_in_process_branch(crm, clock.clone()), 16)
                .build(),
        ))
        .with_clock(clock);
        let plan = compile_plan(
            &snapshot,
            r#"{ ping lead(id: "l-1") { id email } }"#,
            &serde_json::Map::new(),
        );
        let resp = exec
            .execute(ExecRequest::new(
                snapshot,
                plan,
                Instant::now() + Duration::from_secs(5),
            ))
            .await;
        assert_eq!(resp.data, Some(json!({ "ping": "ok", "lead": null })));
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(resp.errors[0].code(), ErrorCode::Forbidden);
        assert_eq!(
            serde_json::to_value(&resp.errors[0].path).unwrap(),
            json!(["lead"])
        );
        assert!(resp.extensions.partial);
    }
}

// spec: NGC-060, NGC-063, GQL-051
#[test]
fn all_14_ngc_error_codes_map_to_documented_gap_codes_and_drop_details() {
    let table: [(NgcErrorCode, ErrorCode, Option<bool>); 14] = [
        (NgcErrorCode::Unspecified, ErrorCode::BranchError, None),
        (
            NgcErrorCode::InvalidArgument,
            ErrorCode::ValidationError,
            None,
        ),
        (NgcErrorCode::NotFound, ErrorCode::BranchError, None),
        (
            NgcErrorCode::PermissionDenied,
            ErrorCode::Forbidden,
            Some(false),
        ),
        (
            NgcErrorCode::Unauthenticated,
            ErrorCode::Unauthenticated,
            None,
        ),
        (
            NgcErrorCode::ResourceExhausted,
            ErrorCode::RateLimited,
            Some(true),
        ),
        (
            NgcErrorCode::FailedPrecondition,
            ErrorCode::BranchError,
            None,
        ),
        (
            NgcErrorCode::DeadlineExceeded,
            ErrorCode::BranchTimeout,
            None,
        ),
        (NgcErrorCode::Cancelled, ErrorCode::Cancelled, None),
        (NgcErrorCode::Unavailable, ErrorCode::BranchError, None),
        (
            NgcErrorCode::Unimplemented,
            ErrorCode::UnsupportedFeature,
            None,
        ),
        (NgcErrorCode::Internal, ErrorCode::BranchError, None),
        (
            NgcErrorCode::Conflict,
            ErrorCode::IdempotencyConflict,
            None,
        ),
        (NgcErrorCode::UpstreamError, ErrorCode::BranchError, None),
    ];

    for (tag, (ngc_code, expected_gap_code, forced_retryable)) in table.into_iter().enumerate() {
        assert_eq!(
            NgcErrorCode::try_from(i32::try_from(tag).unwrap()).unwrap(),
            ngc_code
        );

        for flag in [false, true] {
            let mut details = serde_json::Map::new();
            details.insert("internalHost".to_owned(), json!("db-01.corp.internal"));
            let info = NgcErrorInfo::new(ngc_code, "upstream error summary")
                .with_retry(flag, 350)
                .with_details(details);

            let mapped = map_ngc_error(
                &info,
                Some(vec![PathSegment::Field("orders".to_owned())]),
                &[],
            );
            assert_eq!(
                mapped.code(),
                expected_gap_code,
                "wrong GAP code for {ngc_code:?}"
            );
            let expected_retry = forced_retryable.unwrap_or(flag);
            let retry = mapped.extensions.retry.expect("retry info");
            assert_eq!(
                retry.retryable, expected_retry,
                "wrong retryable flag for {ngc_code:?} with connector retryable={flag}"
            );
            assert_eq!(
                retry.after_ms,
                expected_retry.then_some(350),
                "after_ms must only be present when retryable"
            );

            let serialized = serde_json::to_string(&mapped).unwrap();
            assert!(
                !serialized.contains("db-01.corp.internal")
                    && !serialized.contains("internalHost"),
                "NgcError.details must be dropped when mapping to GraphqlError"
            );
        }
    }
}

// spec: INV-15, NGC-061, GQL-051
#[tokio::test(start_paused = true)]
async fn inv_15_canary_credentials_and_hostile_error_text_never_leak() {
    let canary_bearer = "gap-canary-bearer-secret-9988776655";
    let canary_apikey = "gap-canary-apikey-1122334455";
    let canary_dbpass = "gap-canary-dbpass-5566778899";
    let canary_sk = "sk-live-canary00112233445566778899";
    let canary_meta = "vault-branch-secret-token-aabbccddeeff";

    let raw_hostile = format!(
        "\x1b[31mFATAL\x1b[0m\0 upstream auth failed: Authorization: Bearer {canary_bearer}; \
         api_key={canary_apikey}; dsn=postgres://svc:{canary_dbpass}@db.internal:5432/app; \
         openai={canary_sk}; meta={canary_meta}\nstack backtrace:\n   0: internal::secret_fn\n   at /srv/src/main.rs:42\n{}",
        "x".repeat(500)
    );

    let sanitised = sanitize_connector_message(&raw_hostile, &[canary_meta]);
    assert!(sanitised.chars().count() <= 300);
    for forbidden in [
        canary_bearer,
        canary_apikey,
        canary_dbpass,
        canary_sk,
        canary_meta,
        "stack backtrace",
        "internal::secret_fn",
        "\x1b",
        "\0",
    ] {
        assert!(
            !sanitised.contains(forbidden),
            "sanitised message leaked `{forbidden}`: {sanitised}"
        );
    }

    // End-to-end over `in_memory_grpc` where connector echoes branch metadata credential in error!
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();
    let crm_mock = MockConnector::new().on_query(move |_req, scope| {
        let auth_hdr = scope
            .metadata
            .get("authorization")
            .cloned()
            .unwrap_or_default();
        Err(NgcErrorInfo::new(
            NgcErrorCode::Unauthenticated,
            format!(
                "upstream rejected header {auth_hdr} and dsn postgres://u:{canary_dbpass}@h/db\0\nstack backtrace:\n  0: leak"
            ),
        ))
    });

    let (crm_client, _ch) = spawn_grpc_branch(crm_mock, clock.clone());
    let mut branch_meta = BTreeMap::new();
    branch_meta.insert(
        "authorization".to_owned(),
        format!("Bearer {canary_meta}"),
    );

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register_with_metadata(crm_uid(), crm_client, 16, branch_meta)
            .build(),
    );
    let dbg_registry = format!("{registry:?}");
    assert!(!dbg_registry.contains(canary_meta));

    let exec = ExecLayer::new(registry).with_clock(clock);
    let plan = compile_plan(
        &snapshot,
        r#"{ lead(id: "l-1") { id email } }"#,
        &serde_json::Map::new(),
    );
    let resp = exec
        .execute(ExecRequest::new(
            snapshot,
            plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    let resp_json = serde_json::to_string(&resp).unwrap();
    let resp_dbg = format!("{resp:?}");
    for surface in [&resp_json, &resp_dbg] {
        for secret in [canary_meta, canary_dbpass, "stack backtrace", "\0"] {
            assert!(
                !surface.contains(secret),
                "ExecResponse leaked `{secret}` in `{surface}`"
            );
        }
    }
    assert!(resp.errors[0].message.chars().count() <= 300);
}

#[derive(Debug)]
struct MaskEmailRedactor;

impl ResponseRedactor for MaskEmailRedactor {
    fn redact_field(
        &self,
        _snapshot: &Snapshot,
        parent_type: &str,
        field_name: &str,
        _path: &[PathSegment],
        value: serde_json::Value,
    ) -> Option<serde_json::Value> {
        if parent_type == "crm_Lead" && field_name == "email" {
            Some(json!("***"))
        } else {
            Some(value)
        }
    }
}

// spec: RUN-060
#[tokio::test(start_paused = true)]
async fn run_061_response_redactor_seam_masks_projected_fields() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();
    let crm_mock = MockConnector::new().on_query(|_req, _scope| {
        Ok(stream_rows(vec![json!({
            "id": "l-1",
            "email": "pii@secret.example"
        })]))
    });

    let exec = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(
                crm_uid(),
                make_in_process_branch(crm_mock, clock.clone()),
                16,
            )
            .build(),
    ))
    .with_clock(clock)
    .with_redactor(Arc::new(MaskEmailRedactor));

    let plan = compile_plan(
        &snapshot,
        r#"{ lead(id: "l-1") { id email } }"#,
        &serde_json::Map::new(),
    );
    let resp = exec
        .execute(ExecRequest::new(
            snapshot,
            plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    assert_eq!(
        resp.data,
        Some(json!({ "lead": { "id": "l-1", "email": "***" } }))
    );
}
