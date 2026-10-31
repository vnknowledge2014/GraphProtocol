//! End-to-end two-branch execution tests over `gap_plan::plan_query`, `MockConnector` via
//! `in_memory_grpc` (zero sockets), `tower::ServiceExt::oneshot`, and `INV-01` snapshot pinning.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gap_core::clock::ManualClock;
use gap_core::error::{ErrorCode, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{BranchId, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, Endpoint, FieldCoordinate,
    FieldOwnership, Protocol, Snapshot, TrustTier,
};
use gap_exec::{BranchClient, ConnectorRegistry, ExecLayer, ExecRequest};
use gap_ngc_sdk::error::prost_struct_to_json_map;
use gap_ngc_sdk::mock::MockConnector;
use gap_ql::with_gap_builtins;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;
use tower::ServiceExt as _;

use common::{
    build_two_branch_snapshot, compile_plan, crm_uid, make_in_process_branch, shop_uid,
    spawn_grpc_branch, stream_rows,
};

// Pull Plan IR types from gap_plan for constructing explicit Sequence test plans.
use gap_exec as _;

// spec: RUN-001, RUN-015, RUN-060, GQL-050, NGC-020
#[tokio::test(start_paused = true)]
async fn e2e_two_branch_parallel_aliases_typename_nested_list_object_scalar_and_variables() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    let shop_mock = MockConnector::new().on_query(|req, _scope| {
        let args = req
            .arguments
            .as_ref()
            .map(prost_struct_to_json_map)
            .unwrap_or_default();
        match req.operation.as_str() {
            "ping" => {
                let echo = args
                    .get("echo")
                    .and_then(|v| v.as_str())
                    .unwrap_or("missing-echo");
                let suffix = args
                    .get("suffix")
                    .and_then(|v| v.as_str())
                    .unwrap_or("missing-suffix");
                Ok(stream_rows(vec![json!({
                    "value": format!("{echo}:{suffix}")
                })]))
            }
            "rawPayload" => Ok(stream_rows(vec![json!({
                "env": "staging",
                "flags": [true, false]
            })])),
            "customer" => {
                let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id == "c-missing" {
                    Ok(stream_rows(vec![]))
                } else {
                    Ok(stream_rows(vec![json!({
                        "id": id,
                        "name": "Ada Lovelace",
                        "extraUnselectedSecret": "must-not-be-projected",
                        "orders": [
                            { "id": "o-1", "sku": "BOOK-1", "totalCents": 2500 },
                            { "id": "o-2", "sku": "PEN-9", "totalCents": 400 }
                        ]
                    })]))
                }
            }
            "orders" => Ok(stream_rows(vec![
                json!({ "id": "o-10", "sku": "DESK", "totalCents": 15000 }),
                json!({ "id": "o-11", "sku": "LAMP", "totalCents": 3200 }),
            ])),
            other => Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("unexpected shop op `{other}`"),
            )),
        }
    });

    let crm_mock = MockConnector::new().on_query(|req, _scope| {
        let args = req
            .arguments
            .as_ref()
            .map(prost_struct_to_json_map)
            .unwrap_or_default();
        match req.operation.as_str() {
            "lead" => {
                let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let region = args.get("region").and_then(|v| v.as_str()).unwrap_or("");
                Ok(stream_rows(vec![json!({
                    "id": id,
                    "email": format!("lead@{region}.example"),
                    "company": {
                        "id": "co-1",
                        "name": "Analytical Engines",
                        "hq": { "city": "London" }
                    },
                    "strictCompany": {
                        "id": "co-1",
                        "name": "Analytical Engines",
                        "hq": { "city": "London" }
                    }
                })]))
            }
            other => Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("unexpected crm op `{other}`"),
            )),
        }
    });

    let (shop_client, _shop_h) = spawn_grpc_branch(shop_mock, clock.clone());
    let (crm_client, _crm_h) = spawn_grpc_branch(crm_mock, clock.clone());

    let mut clients: BTreeMap<_, Arc<dyn BranchClient>> = BTreeMap::new();
    clients.insert(shop_uid(), shop_client);
    clients.insert(crm_uid(), crm_client);
    let registry = Arc::new(ConnectorRegistry::from_snapshot(&snapshot, clients));
    let exec = ExecLayer::new(registry).with_clock(clock);

    let query = r#"
        query TwoBranchE2E($echoVal: String!, $suffixVal: String!, $custId: ID!, $leadId: ID!) {
            ping(echo: $echoVal, suffix: $suffixVal)
            rawPayload
            primaryCustomer: customer(id: $custId) {
                __typename
                typeAlias: __typename
                id
                custName: name
                nickname
                orders {
                    __typename
                    orderId: id
                    sku
                }
            }
            absentCustomer: customer(id: "c-missing") {
                id
                name
            }
            orders(first: 2) {
                id
                totalCents
            }
            lead(id: $leadId) {
                __typename
                id
                email
                company {
                    __typename
                    name
                    hq {
                        city
                        zip
                    }
                }
            }
        }
    "#;

    let mut plan_vars = serde_json::Map::new();
    plan_vars.insert("echoVal".to_owned(), json!("hello"));
    plan_vars.insert("suffixVal".to_owned(), json!("world"));
    plan_vars.insert("custId".to_owned(), json!("c-42"));
    plan_vars.insert("leadId".to_owned(), json!("l-99"));

    let plan = compile_plan(&snapshot, query, &plan_vars);

    // Execute with DIFFERENT runtime variable values to prove variable-independent plan binding!
    let mut exec_vars = serde_json::Map::new();
    exec_vars.insert("echoVal".to_owned(), json!("alpha"));
    exec_vars.insert("suffixVal".to_owned(), json!("omega"));
    exec_vars.insert("custId".to_owned(), json!("c-777"));
    exec_vars.insert("leadId".to_owned(), json!("l-888"));

    let req = ExecRequest::new(
        snapshot.clone(),
        plan.clone(),
        Instant::now() + Duration::from_secs(5),
    )
    .with_variables(exec_vars);

    let resp = exec.clone().oneshot(req).await.expect("infallible service");
    assert!(
        resp.errors.is_empty(),
        "unexpected errors: {:?}",
        resp.errors
    );
    assert!(!resp.extensions.partial);
    assert_eq!(resp.extensions.generation, 1);
    assert_eq!(resp.extensions.cost.planned, plan.cost.planned_cost);
    assert!(resp.extensions.cost.actual > 0);

    assert_eq!(
        resp.data,
        Some(json!({
            "ping": "alpha:omega",
            "rawPayload": { "env": "staging", "flags": [true, false] },
            "primaryCustomer": {
                "__typename": "shop_Customer",
                "typeAlias": "shop_Customer",
                "id": "c-777",
                "custName": "Ada Lovelace",
                "nickname": null,
                "orders": [
                    { "__typename": "shop_Order", "orderId": "o-1", "sku": "BOOK-1" },
                    { "__typename": "shop_Order", "orderId": "o-2", "sku": "PEN-9" }
                ]
            },
            "absentCustomer": null,
            "orders": [
                { "id": "o-10", "totalCents": 15000 },
                { "id": "o-11", "totalCents": 3200 }
            ],
            "lead": {
                "__typename": "crm_Lead",
                "id": "l-888",
                "email": "lead@US.example",
                "company": {
                    "__typename": "crm_Company",
                    "name": "Analytical Engines",
                    "hq": {
                        "city": "London",
                        "zip": null
                    }
                }
            }
        }))
    );
}

// spec: RUN-015, RUN-060, GQL-050
#[tokio::test(start_paused = true)]
async fn e2e_skip_and_include_directives_on_root_fetches_and_selection_fields() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    let shop_mock = MockConnector::new()
        .on_query(|_req, _scope| Ok(stream_rows(vec![json!({ "value": "pong" })])));
    let crm_mock = MockConnector::new().on_query(|_req, _scope| {
        Ok(stream_rows(vec![json!({
            "id": "l-1",
            "email": "ada@example.com",
            "phone": "+44-20-7946"
        })]))
    });

    let (shop_client, _sh) = spawn_grpc_branch(shop_mock.clone(), clock.clone());
    let (crm_client, _ch) = spawn_grpc_branch(crm_mock.clone(), clock.clone());

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), shop_client, 16)
            .register(crm_uid(), crm_client, 16)
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);

    let query = r#"
        query Conditional($skipShop: Boolean!, $incCrm: Boolean!, $skipPhone: Boolean!, $incEmail: Boolean!) {
            ping @skip(if: $skipShop)
            lead(id: "l-1") @include(if: $incCrm) {
                id
                email @include(if: $incEmail)
                phone @skip(if: $skipPhone)
            }
        }
    "#;

    let mut vars = serde_json::Map::new();
    vars.insert("skipShop".to_owned(), json!(true));
    vars.insert("incCrm".to_owned(), json!(true));
    vars.insert("skipPhone".to_owned(), json!(true));
    vars.insert("incEmail".to_owned(), json!(false));

    let plan = compile_plan(&snapshot, query, &vars);
    let resp = exec
        .execute(
            ExecRequest::new(snapshot, plan, Instant::now() + Duration::from_secs(5))
                .with_variables(vars),
        )
        .await;

    assert!(resp.errors.is_empty());
    assert_eq!(
        shop_mock.total_calls(),
        0,
        "skipped root Fetch must NOT invoke the shop connector"
    );
    assert_eq!(crm_mock.total_calls(), 1);
    assert_eq!(
        resp.data,
        Some(json!({
            "lead": {
                "id": "l-1"
            }
        }))
    );
}

// spec: RUN-001, RUN-015
#[tokio::test(start_paused = true)]
async fn e2e_sequence_executes_nodes_strictly_in_order() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();
    let step_counter = Arc::new(AtomicUsize::new(0));
    let shop_step = Arc::new(AtomicUsize::new(0));
    let crm_step = Arc::new(AtomicUsize::new(0));

    let sc1 = Arc::clone(&step_counter);
    let ss = Arc::clone(&shop_step);
    let shop_mock = MockConnector::new().on_query_async(move |_req, _scope| {
        let sc1 = Arc::clone(&sc1);
        let ss = Arc::clone(&ss);
        async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let n = sc1.fetch_add(1, Ordering::SeqCst) + 1;
            ss.store(n, Ordering::SeqCst);
            Ok(stream_rows(vec![json!({ "value": "first" })]))
        }
    });

    let sc2 = Arc::clone(&step_counter);
    let cs = Arc::clone(&crm_step);
    let crm_mock = MockConnector::new().on_query_async(move |_req, _scope| {
        let sc2 = Arc::clone(&sc2);
        let cs = Arc::clone(&cs);
        async move {
            let n = sc2.fetch_add(1, Ordering::SeqCst) + 1;
            cs.store(n, Ordering::SeqCst);
            Ok(stream_rows(vec![json!({
                "id": "l-seq",
                "email": "seq@example.com"
            })]))
        }
    });

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(
                shop_uid(),
                make_in_process_branch(shop_mock, clock.clone()),
                16,
            )
            .register(
                crm_uid(),
                make_in_process_branch(crm_mock, clock.clone()),
                16,
            )
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);

    let seq_plan = Arc::new(gap_plan::Plan {
        root: gap_plan::PlanNode::Sequence {
            nodes: vec![
                gap_plan::PlanNode::Fetch(gap_plan::Fetch {
                    branch: "shop".parse::<BranchId>().expect("shop"),
                    branch_uid: shop_uid(),
                    operation: "ping".to_owned(),
                    arguments: BTreeMap::new(),
                    selection: Vec::new(),
                    response_key: "ping".to_owned(),
                    directives: Vec::new(),
                    pushdown: gap_plan::Pushdown::default(),
                }),
                gap_plan::PlanNode::Fetch(gap_plan::Fetch {
                    branch: "crm".parse::<BranchId>().expect("crm"),
                    branch_uid: crm_uid(),
                    operation: "lead".to_owned(),
                    arguments: BTreeMap::from([(
                        "id".to_owned(),
                        gap_plan::ArgValue::Literal(json!("l-seq")),
                    )]),
                    selection: vec![
                        gap_plan::SelectionField::leaf("id"),
                        gap_plan::SelectionField::leaf("email"),
                    ],
                    response_key: "lead".to_owned(),
                    directives: Vec::new(),
                    pushdown: gap_plan::Pushdown::default(),
                }),
            ],
        },
        operation_name: None,
        cost: gap_plan::PlanCost {
            planned_cost: 4,
            effective_budget: None,
            warnings: Vec::new(),
        },
        branches: vec![
            "crm".parse::<BranchId>().expect("crm"),
            "shop".parse::<BranchId>().expect("shop"),
        ],
        policy_version: 7,
    });

    let resp = exec
        .execute(ExecRequest::new(
            snapshot,
            seq_plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    assert!(resp.errors.is_empty());
    assert_eq!(shop_step.load(Ordering::SeqCst), 1);
    assert_eq!(
        crm_step.load(Ordering::SeqCst),
        2,
        "Sequence must finish stage 1 (shop) before starting stage 2 (crm)"
    );
    assert_eq!(
        resp.data,
        Some(json!({
            "ping": "first",
            "lead": { "id": "l-seq", "email": "seq@example.com" }
        }))
    );
}

// spec: RUN-024, RUN-060, GQL-050, GQL-051
#[tokio::test(start_paused = true)]
async fn e2e_partial_failure_one_branch_fails_and_all_branches_failing_yields_data_null() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    // Case 1: shop succeeds (`orders`), crm fails (`lead`, nullable) -> partial = true!
    let shop_ok = MockConnector::new().on_query(|_req, _scope| {
        Ok(stream_rows(vec![json!({
            "id": "o-1",
            "sku": "WIDGET",
            "totalCents": 900
        })]))
    });
    let crm_fail = MockConnector::new().on_query(|_req, _scope| {
        Err(NgcErrorInfo::new(
            NgcErrorCode::Unavailable,
            "crm upstream down",
        ))
    });

    let (sc1, _sh1) = spawn_grpc_branch(shop_ok, clock.clone());
    let (cc1, _ch1) = spawn_grpc_branch(crm_fail.clone(), clock.clone());
    let exec_partial = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), sc1, 16)
            .register(crm_uid(), cc1, 16)
            .build(),
    ))
    .with_clock(clock.clone());

    let partial_plan = compile_plan(
        &snapshot,
        r#"{ orders { id sku } lead(id: "l-1") { id email } }"#,
        &serde_json::Map::new(),
    );
    let partial_resp = exec_partial
        .execute(ExecRequest::new(
            snapshot.clone(),
            partial_plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    assert_eq!(
        partial_resp.data,
        Some(json!({
            "orders": [{ "id": "o-1", "sku": "WIDGET" }],
            "lead": null
        }))
    );
    assert_eq!(partial_resp.errors.len(), 1);
    assert_eq!(partial_resp.errors[0].code(), ErrorCode::BranchError);
    assert_eq!(
        serde_json::to_value(&partial_resp.errors[0].path).unwrap(),
        json!(["lead"])
    );
    assert!(
        partial_resp.extensions.partial,
        "partial MUST be true when data is present and one branch failed"
    );

    // Case 2: ALL branches fail on non-null root fields (`orders` and `leads`) -> data: null, partial: false!
    let shop_fail = MockConnector::new().on_query(|_req, _scope| {
        Err(NgcErrorInfo::new(
            NgcErrorCode::UpstreamError,
            "shop db offline",
        ))
    });
    let (sc2, _sh2) = spawn_grpc_branch(shop_fail, clock.clone());
    let (cc2, _ch2) = spawn_grpc_branch(crm_fail, clock.clone());
    let exec_all_fail = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), sc2, 16)
            .register(crm_uid(), cc2, 16)
            .build(),
    ))
    .with_clock(clock);

    let all_fail_plan = compile_plan(
        &snapshot,
        r#"{ orders { id } leads { id } }"#,
        &serde_json::Map::new(),
    );
    let all_fail_resp = exec_all_fail
        .execute(ExecRequest::new(
            snapshot,
            all_fail_plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    assert_eq!(all_fail_resp.data, None);
    let serialized = serde_json::to_value(&all_fail_resp).unwrap();
    assert_eq!(serialized["data"], serde_json::Value::Null);
    assert_eq!(all_fail_resp.errors.len(), 2);
    assert!(
        !all_fail_resp.extensions.partial,
        "partial MUST be false when top-level data is null"
    );
}

// spec: RUN-001, GQL-050
#[tokio::test(start_paused = true)]
async fn e2e_missing_required_variable_returns_validation_error_without_calling_connectors() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();
    let shop_mock = MockConnector::new();

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(
                shop_uid(),
                make_in_process_branch(shop_mock.clone(), clock.clone()),
                16,
            )
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);

    let mut plan_vars = serde_json::Map::new();
    plan_vars.insert("custId".to_owned(), json!("c-1"));
    let plan = compile_plan(
        &snapshot,
        r#"query Q($custId: ID!) { customer(id: $custId) { id name } }"#,
        &plan_vars,
    );

    // Execute with empty variables map (missing required `$custId`)
    let resp = exec
        .execute(ExecRequest::new(
            snapshot,
            plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    assert_eq!(resp.data, None);
    assert_eq!(resp.errors.len(), 1);
    assert_eq!(resp.errors[0].code(), ErrorCode::ValidationError);
    assert!(!resp.extensions.partial);
    assert_eq!(
        shop_mock.total_calls(),
        0,
        "connector must not be called when required variable is missing"
    );
}

// spec: RUN-001, INV-01, GQL-050
#[tokio::test(start_paused = true)]
async fn inv_01_pinned_snapshot_answers_request_even_after_new_generation_published() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snap_gen1, _) = build_two_branch_snapshot();

    // Build generation 2 with a distinct schema type name (`shop_BuyerV2`) for `customer`!
    let sdl_gen2 = r#"
        type shop_BuyerV2 @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) {
            id: ID!
            name: String!
        }
        type Query {
            customer(id: ID!): shop_BuyerV2 @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2)
        }
    "#;
    let valid_gen2 = with_gap_builtins(sdl_gen2).expect("valid gen2 SDL");
    let mut snap_gen2_raw =
        Snapshot::empty_for_tenant(2, TenantId::new("acme").expect("acme tenant"));
    snap_gen2_raw.branches.insert(
        shop_uid(),
        Arc::new(BranchDescriptor {
            uid: shop_uid(),
            id: "shop".parse::<BranchId>().expect("shop"),
            protocol: Protocol::Grpc,
            trust: TrustTier::T0,
            endpoint: Endpoint {
                uri: "https://shop.internal:8443".to_owned(),
                transport: "grpc".to_owned(),
            },
            spec_hash: [0x33; 32],
            pinned_desc_hash: [0x34; 32],
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: "Gen 2 shop".to_owned(),
            operations: snap_gen1
                .branches
                .get(&shop_uid())
                .expect("shop")
                .operations
                .clone(),
            status: BranchStatus::Active,
        }),
    );
    snap_gen2_raw.schema = Arc::new(valid_gen2.into_inner());
    snap_gen2_raw.ownership = Arc::new(FieldOwnership::new(BTreeMap::from([
        (FieldCoordinate::new("Query", "customer"), shop_uid()),
        (FieldCoordinate::new("shop_BuyerV2", "id"), shop_uid()),
        (FieldCoordinate::new("shop_BuyerV2", "name"), shop_uid()),
    ])));
    let snap_gen2 = Arc::new(snap_gen2_raw);

    let observed_generations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let og = Arc::clone(&observed_generations);
    let gen1_in_flight = Arc::new(Notify::new());
    let gen1_sig = Arc::clone(&gen1_in_flight);
    let release_gen1 = Arc::new(Notify::new());
    let rel_sig = Arc::clone(&release_gen1);

    let shop_mock = MockConnector::new().on_query_async(move |req, _scope| {
        let generation = req.ctx.as_ref().map_or(0, |c| c.snapshot_generation);
        og.lock().expect("test mutex").push(generation);
        let gen1_sig = Arc::clone(&gen1_sig);
        let rel_sig = Arc::clone(&rel_sig);
        async move {
            if generation == 1 {
                gen1_sig.notify_one();
                rel_sig.notified().await;
            }
            Ok(stream_rows(vec![
                json!({ "id": "c-1", "name": "Grace Hopper" }),
            ]))
        }
    });

    let (shop_client, _sh) = spawn_grpc_branch(shop_mock, clock.clone());
    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), shop_client, 16)
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);

    let query = r#"{ customer(id: "c-1") { __typename id name } }"#;
    let plan_gen1 = compile_plan(&snap_gen1, query, &serde_json::Map::new());
    let plan_gen2 = compile_plan(&snap_gen2, query, &serde_json::Map::new());

    // Start gen 1 request and pause it mid-flight while gen 2 is published and served!
    let req1 = ExecRequest::new(
        snap_gen1,
        plan_gen1,
        Instant::now() + Duration::from_secs(5),
    );
    let req2 = ExecRequest::new(
        snap_gen2,
        plan_gen2,
        Instant::now() + Duration::from_secs(5),
    );

    let svc1 = exec.clone();
    let fut1 = tokio::spawn(async move { svc1.oneshot(req1).await });
    gen1_in_flight.notified().await;

    // Serve gen 2 to completion while gen 1 is still in-flight!
    let resp2 = exec.clone().oneshot(req2).await.expect("infallible");
    release_gen1.notify_one();
    let resp1 = fut1.await.expect("join req1").expect("infallible");

    assert_eq!(resp1.extensions.generation, 1);
    assert_eq!(
        resp1.data,
        Some(json!({
            "customer": {
                "__typename": "shop_Customer",
                "id": "c-1",
                "name": "Grace Hopper"
            }
        }))
    );

    assert_eq!(resp2.extensions.generation, 2);
    assert_eq!(
        resp2.data,
        Some(json!({
            "customer": {
                "__typename": "shop_BuyerV2",
                "id": "c-1",
                "name": "Grace Hopper"
            }
        }))
    );
}
