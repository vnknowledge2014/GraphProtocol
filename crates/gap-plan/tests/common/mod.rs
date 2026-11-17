//! Hand-built multi-branch `Snapshot` fixtures for `gap-plan` (`L2`, zero `gap-snapshot` dependency).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use apollo_compiler::Schema;
use apollo_compiler::validation::Valid;
use gap_core::ids::{BranchId, BranchUid, GroupId, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CostHint, Endpoint,
    FieldCoordinate, FieldOwnership, OpKind, OperationDescriptor, Protocol, SideEffect,
    SideEffectLevel, Snapshot, TrustTier,
};
use gap_ql::{ValidatedOperation, validate_document, with_gap_builtins};

/// Deterministic branch UIDs (`crm` > `shop` or `shop` < `crm` — note we intentionally give `shop`
/// the lower UUIDv7 and `crm` the higher UUIDv7 so `shop` is ALWAYS the first branch in
/// `Snapshot.branches: OrdMap<BranchUid, Arc<BranchDescriptor>>`).
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
    group_hex_byte: u8,
) -> Arc<OperationDescriptor> {
    let group_str = format!("{group_hex_byte:02x}").repeat(16);
    let group: GroupId = group_str.parse().expect("valid 32-hex group id");
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
        cost: CostHint::default(),
        desc_hash: [group_hex_byte; 32],
        sdl: Arc::from(sdl),
        examples: Vec::new(),
    })
}

fn make_mutation_op(
    branch: &str,
    name: &str,
    sdl: &str,
    group_hex_byte: u8,
) -> Arc<OperationDescriptor> {
    let group_str = format!("{group_hex_byte:02x}").repeat(16);
    let group: GroupId = group_str.parse().expect("valid 32-hex group id");
    Arc::new(OperationDescriptor {
        id: format!("{branch}.{name}").parse().expect("valid op id"),
        group,
        kind: OpKind::Mutation,
        side_effect: SideEffect {
            level: SideEffectLevel::Irreversible,
            verified: true,
            requires_approval: true,
        },
        idempotent: false,
        cost: CostHint {
            weight: 5,
            default_list_size: None,
            per_key_weight: 1,
        },
        desc_hash: [group_hex_byte; 32],
        sdl: Arc::from(sdl),
        examples: Vec::new(),
    })
}

/// Composed supergraph SDL matching what `gap-snapshot::compose` produces for two branches (`shop`
/// and `crm`), plus an intentionally unowned `orphanMetric` field on `Query` to test `RUN-011`
/// missing-owner handling, and recursive `shop_Customer.friends` to test deep selections (`GQL-040`).
pub const TWO_BRANCH_SDL: &str = r#"
input shop_OrderFilter {
    status: String
    minTotal: Int
}

type shop_Order @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @key(fields: "id") {
    id: ID!
    sku: String!
    totalCents: Int! @cost(weight: 2)
}

type shop_Customer @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @key(fields: "id") {
    id: ID!
    name: String!
    orders(first: Int): [shop_Order!]! @cost(weight: 2, listSize: 50)
    friends(first: Int): [shop_Customer!]! @cost(weight: 1, listSize: 10)
}

input crm_LeadFilter {
    region: String
    active: Boolean
}

type crm_Company @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) {
    id: ID!
    name: String!
}

type crm_Lead @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) @key(fields: "id") {
    id: ID!
    email: String!
    phone: String
    score: Int!
    company: crm_Company
}

type Query {
    ping: String! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 1)
    customer(id: ID!): shop_Customer @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2)
    order(id: ID!): shop_Order @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 1)
    orders(first: Int, filter: shop_OrderFilter): [shop_Order!]! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @cost(weight: 2)
    lead(id: ID!, tags: [String!], filter: crm_LeadFilter): crm_Lead @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) @cost(weight: 3)
    leads(first: Int): [crm_Lead!]! @branch(id: "crm", uid: "018bcfe5-6c02-7020-8202-020202020202", protocol: REST, trust: T1) @cost(weight: 2, listSize: 40)
    orphanMetric: Int!
}

type Mutation {
    createOrder(sku: String!): shop_Order! @branch(id: "shop", uid: "018bcfe5-6c01-7010-8101-010101010101", protocol: GRPC, trust: T0) @sideEffect(level: IRREVERSIBLE) @cost(weight: 5)
}
"#;

pub fn valid_two_branch_schema() -> Valid<Schema> {
    with_gap_builtins(TWO_BRANCH_SDL).expect("valid two-branch supergraph schema")
}

/// Builds a deterministic two-branch (`shop` + `crm`) [`Snapshot`] with generation `1` and
/// `policy_version = 7`. Note `shop_uid() < crm_uid()` so `shop` is first in `snapshot.branches`.
pub fn two_branch_snapshot() -> (Snapshot, Valid<Schema>) {
    let valid_schema = valid_two_branch_schema();
    let s_uid = shop_uid();
    let c_uid = crm_uid();
    assert!(s_uid < c_uid, "shop_uid must sort before crm_uid in OrdMap");

    let shop_id: BranchId = "shop".parse().expect("shop id");
    let crm_id: BranchId = "crm".parse().expect("crm id");

    let shop_branch = Arc::new(BranchDescriptor {
        uid: s_uid,
        id: shop_id,
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
        limits: BranchLimits::default(),
        synopsis: "Orders and customer catalogue".to_owned(),
        operations: vec![
            make_query_op("shop", "ping", "ping: String!", 0x0F),
            make_query_op("shop", "customer", "customer(id: ID!): shop_Customer", 0x10),
            make_query_op("shop", "order", "order(id: ID!): shop_Order", 0x11),
            make_query_op(
                "shop",
                "orders",
                "orders(first: Int, filter: shop_OrderFilter): [shop_Order!]!",
                0x12,
            ),
            make_mutation_op(
                "shop",
                "createOrder",
                "createOrder(sku: String!): shop_Order!",
                0x13,
            ),
        ],
        status: BranchStatus::Active,
    });

    let crm_branch = Arc::new(BranchDescriptor {
        uid: c_uid,
        id: crm_id,
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: "https://crm.internal:8443".to_owned(),
            transport: "http".to_owned(),
        },
        spec_hash: [0x21; 32],
        pinned_desc_hash: [0x22; 32],
        capabilities: Capabilities::default(),
        limits: BranchLimits::default(),
        synopsis: "CRM leads and companies".to_owned(),
        operations: vec![
            make_query_op(
                "crm",
                "lead",
                "lead(id: ID!, tags: [String!], filter: crm_LeadFilter): crm_Lead",
                0x20,
            ),
            make_query_op("crm", "leads", "leads(first: Int): [crm_Lead!]!", 0x21),
        ],
        status: BranchStatus::Active,
    });

    let mut owners = BTreeMap::new();
    // Root Query fields (`orphanMetric` deliberately omitted!)
    owners.insert(FieldCoordinate::new("Query", "ping"), s_uid);
    owners.insert(FieldCoordinate::new("Query", "customer"), s_uid);
    owners.insert(FieldCoordinate::new("Query", "order"), s_uid);
    owners.insert(FieldCoordinate::new("Query", "orders"), s_uid);
    owners.insert(FieldCoordinate::new("Query", "lead"), c_uid);
    owners.insert(FieldCoordinate::new("Query", "leads"), c_uid);
    // Root Mutation field
    owners.insert(FieldCoordinate::new("Mutation", "createOrder"), s_uid);
    // Object fields
    for f in ["id", "sku", "totalCents"] {
        owners.insert(FieldCoordinate::new("shop_Order", f), s_uid);
    }
    for f in ["id", "name", "orders", "friends"] {
        owners.insert(FieldCoordinate::new("shop_Customer", f), s_uid);
    }
    for f in ["id", "name"] {
        owners.insert(FieldCoordinate::new("crm_Company", f), c_uid);
    }
    for f in ["id", "email", "phone", "score", "company"] {
        owners.insert(FieldCoordinate::new("crm_Lead", f), c_uid);
    }

    let mut snap = Snapshot::empty_for_tenant(1, TenantId::new("acme").expect("acme tenant"));
    snap.branches.insert(s_uid, shop_branch);
    snap.branches.insert(c_uid, crm_branch);
    snap.schema = Arc::new(valid_schema.clone().into_inner());
    snap.ownership = Arc::new(FieldOwnership::new(owners));
    snap.policy_version = 7;
    snap.branch_set_hash = [0xAB; 32];

    (snap, valid_schema)
}

pub fn validate_op(
    schema: &Valid<Schema>,
    query: &str,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
) -> ValidatedOperation {
    validate_document(schema, query, operation_name, variables)
        .unwrap_or_else(|errs| panic!("expected valid test document, got errors: {errs:?}"))
}
