//! Shared hand-built two-branch `Snapshot`, `MockConnector`, and `in_memory_grpc` helpers for
//! `gap-exec` integration tests (`L3`, zero sockets, zero `gap-snapshot` dependency).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use apollo_compiler::Schema;
use apollo_compiler::validation::Valid;
use futures_util::stream;
use gap_core::clock::Clock;
use gap_core::ids::{BranchId, BranchUid, GroupId, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CostHint, Endpoint,
    FieldCoordinate, FieldOwnership, OpKind, OperationDescriptor, Protocol, SideEffect,
    SideEffectLevel, Snapshot, TrustTier,
};
use gap_ngc_sdk::connector::BoxStream;
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_ngc_sdk::grpc::GrpcClient;
use gap_ngc_sdk::mock::MockConnector;
use gap_ngc_sdk::testing::{InMemoryGrpcHandle, in_memory_grpc};
use gap_plan::{Plan, PlanConfig, plan_query};
use gap_proto::ngc::v1::{Complete, QueryResponse, RowSetChunk, query_response};
use gap_ql::with_gap_builtins;

use gap_exec::InProcess;

pub fn shop_uid() -> BranchUid {
    BranchUid::parse("018bcfe5-6c01-7010-8101-010101010101").expect("valid shop UUIDv7")
}

pub fn crm_uid() -> BranchUid {
    BranchUid::parse("018bcfe5-6c02-7020-8202-020202020202").expect("valid crm UUIDv7")
}

fn make_query_op(
    branch: &str,
    name: &str,
    sdl: &str,
    weight: u32,
    group_byte: u8,
) -> Arc<OperationDescriptor> {
    let group_str = format!("{group_byte:02x}").repeat(16);
    let group: GroupId = group_str.parse().expect("valid group id");
    Arc::new(OperationDescriptor {
        id: format!("{branch}.{name}").parse().expect("valid op id"),
        group,
        kind: OpKind::Query,
        side_effect: SideEffect {
            level: SideEffectLevel::None,
            verified: true,
            requires_approval: false,
        },
        idempotent: true,
        cost: CostHint {
            weight,
            default_list_size: None,
            per_key_weight: 1,
        },
        desc_hash: [group_byte; 32],
        sdl: Arc::from(sdl),
        examples: Vec::new(),
    })
}

pub const EXEC_TEST_SDL: &str = r#"
input shop_OrderFilter {
    status: String
    minTotal: Int
}

type shop_Order @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @key(fields: "id") {
    id: ID!
    sku: String!
    note: String
    totalCents: Int! @cost(weight: 2)
}

type shop_Customer @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @key(fields: "id") {
    id: ID!
    name: String!
    nickname: String
    orders: [shop_Order!]! @cost(weight: 2, listSize: 10)
}

type crm_Address @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) {
    city: String!
    zip: String
}

type crm_Company @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) {
    id: ID!
    name: String!
    hq: crm_Address!
}

type crm_Lead @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) @key(fields: "id") {
    id: ID!
    email: String!
    phone: String
    company: crm_Company
    strictCompany: crm_Company!
}

type Query {
    ping(echo: String, suffix: String): String! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 1)
    optionalNote: String @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 1)
    rawPayload: JSON @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 1)
    customer(id: ID!): shop_Customer @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2)
    strictOrder(id: ID!): shop_Order! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 1)
    orders(first: Int, filter: shop_OrderFilter): [shop_Order!]! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2, listSize: 10)
    nullableOrders: [shop_Order!] @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2, listSize: 10)
    sparseOrders: [shop_Order]! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2, listSize: 10)
    lead(id: ID!, region: String = "US"): crm_Lead @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) @cost(weight: 3)
    leads(first: Int): [crm_Lead!]! @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) @cost(weight: 2, listSize: 10)
}
"#;

pub fn build_two_branch_snapshot() -> (Arc<Snapshot>, Valid<Schema>) {
    build_two_branch_snapshot_with_limits(BranchLimits::default(), BranchLimits::default(), 1)
}

pub fn build_two_branch_snapshot_with_limits(
    shop_limits: BranchLimits,
    crm_limits: BranchLimits,
    generation: u64,
) -> (Arc<Snapshot>, Valid<Schema>) {
    let valid_schema = with_gap_builtins(EXEC_TEST_SDL).expect("valid supergraph SDL");
    let s_uid = shop_uid();
    let c_uid = crm_uid();

    let shop_branch = Arc::new(BranchDescriptor {
        uid: s_uid,
        id: "shop".parse::<BranchId>().expect("shop"),
        protocol: Protocol::Grpc,
        trust: TrustTier::T0,
        endpoint: Endpoint {
            uri: "https://shop.internal:8443".to_owned(),
            transport: "grpc".to_owned(),
        },
        spec_hash: [0x11; 32],
        pinned_desc_hash: [0x12; 32],
        capabilities: Capabilities {
            foreach_batch: true,
            idempotency: true,
            ..Capabilities::default()
        },
        limits: shop_limits,
        synopsis: "Orders and customer catalogue".to_owned(),
        operations: vec![
            make_query_op(
                "shop",
                "ping",
                "ping(echo: String, suffix: String): String!",
                1,
                0x01,
            ),
            make_query_op("shop", "optionalNote", "optionalNote: String", 1, 0x02),
            make_query_op("shop", "rawPayload", "rawPayload: JSON", 1, 0x03),
            make_query_op(
                "shop",
                "customer",
                "customer(id: ID!): shop_Customer",
                2,
                0x04,
            ),
            make_query_op(
                "shop",
                "strictOrder",
                "strictOrder(id: ID!): shop_Order!",
                1,
                0x05,
            ),
            make_query_op(
                "shop",
                "orders",
                "orders(first: Int, filter: shop_OrderFilter): [shop_Order!]!",
                2,
                0x06,
            ),
            make_query_op(
                "shop",
                "nullableOrders",
                "nullableOrders: [shop_Order!]",
                2,
                0x07,
            ),
            make_query_op(
                "shop",
                "sparseOrders",
                "sparseOrders: [shop_Order]!",
                2,
                0x08,
            ),
        ],
        status: BranchStatus::Active,
    });

    let crm_branch = Arc::new(BranchDescriptor {
        uid: c_uid,
        id: "crm".parse::<BranchId>().expect("crm"),
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: "https://crm.internal:8443".to_owned(),
            transport: "grpc".to_owned(),
        },
        spec_hash: [0x21; 32],
        pinned_desc_hash: [0x22; 32],
        capabilities: Capabilities::default(),
        limits: crm_limits,
        synopsis: "CRM leads and companies".to_owned(),
        operations: vec![
            make_query_op(
                "crm",
                "lead",
                "lead(id: ID!, region: String = \"US\"): crm_Lead",
                3,
                0x11,
            ),
            make_query_op("crm", "leads", "leads(first: Int): [crm_Lead!]!", 2, 0x12),
        ],
        status: BranchStatus::Active,
    });

    let mut owners = BTreeMap::new();
    for root_f in [
        "ping",
        "optionalNote",
        "rawPayload",
        "customer",
        "strictOrder",
        "orders",
        "nullableOrders",
        "sparseOrders",
    ] {
        owners.insert(FieldCoordinate::new("Query", root_f), s_uid);
    }
    for root_f in ["lead", "leads"] {
        owners.insert(FieldCoordinate::new("Query", root_f), c_uid);
    }
    for f in ["id", "sku", "note", "totalCents"] {
        owners.insert(FieldCoordinate::new("shop_Order", f), s_uid);
    }
    for f in ["id", "name", "nickname", "orders"] {
        owners.insert(FieldCoordinate::new("shop_Customer", f), s_uid);
    }
    for f in ["city", "zip"] {
        owners.insert(FieldCoordinate::new("crm_Address", f), c_uid);
    }
    for f in ["id", "name", "hq"] {
        owners.insert(FieldCoordinate::new("crm_Company", f), c_uid);
    }
    for f in ["id", "email", "phone", "company", "strictCompany"] {
        owners.insert(FieldCoordinate::new("crm_Lead", f), c_uid);
    }

    let mut snap =
        Snapshot::empty_for_tenant(generation, TenantId::new("acme").expect("acme tenant"));
    snap.branches.insert(s_uid, shop_branch);
    snap.branches.insert(c_uid, crm_branch);
    snap.schema = Arc::new(valid_schema.clone().into_inner());
    snap.ownership = Arc::new(FieldOwnership::new(owners));
    snap.policy_version = 7;
    snap.branch_set_hash = [0xAB; 32];

    (Arc::new(snap), valid_schema)
}

pub fn compile_plan(
    snapshot: &Snapshot,
    query: &str,
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Arc<Plan> {
    Arc::new(
        plan_query(snapshot, query, None, variables, &PlanConfig::default())
            .expect("test query must plan cleanly"),
    )
}

pub fn json_obj_to_prost_struct(val: &serde_json::Value) -> prost_types::Struct {
    let map = val.as_object().expect("row fixture must be a JSON object");
    json_map_to_prost_struct(map)
}

pub fn rows_response(rows: Vec<serde_json::Value>) -> QueryResponse {
    QueryResponse {
        chunk: Some(query_response::Chunk::Rows(RowSetChunk {
            foreach_index: 0,
            rows: rows.iter().map(json_obj_to_prost_struct).collect(),
            last_for_index: true,
            error: None,
        })),
    }
}

pub fn complete_response() -> QueryResponse {
    QueryResponse {
        chunk: Some(query_response::Chunk::Complete(Complete {
            upstream_calls: 1,
            ..Complete::default()
        })),
    }
}

pub fn stream_rows(rows: Vec<serde_json::Value>) -> BoxStream<QueryResponse> {
    Box::pin(stream::iter(vec![
        Ok(rows_response(rows)),
        Ok(complete_response()),
    ]))
}

pub fn spawn_grpc_branch(
    mock: MockConnector,
    clock: Arc<dyn Clock>,
) -> (Arc<GrpcClient>, InMemoryGrpcHandle) {
    let cfg = ServerConfig::default()
        .with_clock(clock)
        .with_max_in_flight(256);
    let core = Arc::new(ServerCore::new(mock, cfg));
    let (client, handle) = in_memory_grpc(core);
    (Arc::new(client), handle)
}

pub fn make_in_process_branch(
    mock: MockConnector,
    clock: Arc<dyn Clock>,
) -> Arc<InProcess<MockConnector>> {
    let cfg = ServerConfig::default()
        .with_clock(clock)
        .with_max_in_flight(256);
    Arc::new(InProcess::from_connector(mock, cfg))
}
