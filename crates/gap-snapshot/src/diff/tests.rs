//! Unit tests for snapshot generation diffing (`WP-M4-09`, `ING-052`, `SEC-082`, `RUN-009`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use gap_core::ids::{BranchId, BranchUid, OperationId};
use gap_core::joingraph::{
    JoinCardinality, JoinEdge, JoinGraph, JoinKeyPair, JoinStatus, JoinStrategy,
};
use gap_core::snapshot::{BranchStatus, Endpoint, Protocol, SideEffectLevel, Snapshot, TrustTier};
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits, MetaSummary,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, SideEffectHint,
};

use crate::compose::{BranchInput, compose};
use crate::handle::SnapshotBuilder;

use super::{BranchChangeKind, OperationChangeKind, diff_snapshots};

fn uid_for(idx: u16) -> BranchUid {
    let bytes = idx.to_be_bytes();
    BranchUid::from_unix_ms(
        1_761_900_100_000 + u64::from(idx),
        [bytes[0], bytes[1], 0, 0, 0, 0, 0, 0, 0, 2],
    )
}

fn make_branch(
    idx: u16,
    op_name: &str,
    desc: &str,
    arg_sdl: &str,
    status: BranchStatus,
    side_effect: SideEffectHint,
) -> BranchInput {
    let slug = format!("svc{idx:03}");
    let sdl = format!("type Query {{ {op_name}({arg_sdl}): String }}\n");
    let op = ProtoOp {
        name: op_name.to_owned(),
        kind: ProtoOpKind::Query as i32,
        description: desc.to_owned(),
        input_schema: None,
        output_schema: None,
        side_effect_hint: side_effect as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 1,
        pagination: 0,
        key_fields: Vec::new(),
        annotations: HashMap::new(),
    };

    BranchInput {
        id: BranchId::new(&slug).unwrap(),
        uid: uid_for(idx),
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: format!("https://{slug}.internal/ngc"),
            transport: "grpc".to_owned(),
        },
        description: ConnectorDescription {
            connector_id: slug,
            connector_version: "1.0.0".to_owned(),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
            spec_hash: format!("{idx:064x}"),
            capabilities: Some(ProtoCapabilities::default()),
            limits: Some(ProtoLimits::default()),
            schema_sdl: sdl,
            operations: vec![op],
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: format!("service {idx}"),
            }),
            auth: None,
        },
        status,
    }
}

// spec: RUN-009
#[test]
fn diff_identical_snapshots_is_empty_and_non_breaking() {
    let b1 = make_branch(
        1,
        "getOrder",
        "Fetch order",
        "id: ID!",
        BranchStatus::Active,
        SideEffectHint::SeNone,
    );
    let snap = SnapshotBuilder::new(&Snapshot::empty(0), compose(&[b1]).unwrap()).build();

    let d = diff_snapshots(&snap, &snap);
    assert!(d.is_empty());
    assert!(!d.has_breaking_changes);
    assert!(d.render_text().contains("(no changes)"));
    assert!(d.to_canonical_json().unwrap().contains("\"branches\":[]"));
}

// spec: ING-052, SEC-082, RUN-009
#[test]
fn diff_detects_added_removed_modified_branches_ops_groups_ownership_and_joins() {
    let b1 = make_branch(
        1,
        "getOrder",
        "Fetch order",
        "id: ID!",
        BranchStatus::Active,
        SideEffectHint::SeNone,
    );
    let b2 = make_branch(
        2,
        "getLegacy",
        "Legacy endpoint",
        "id: ID!",
        BranchStatus::Active,
        SideEffectHint::SeNone,
    );

    let mut before = SnapshotBuilder::new(&Snapshot::empty(0), compose(&[b1, b2]).unwrap()).build();
    let edge_removed = JoinEdge {
        id: "join_order_legacy".to_owned(),
        source_branch: uid_for(1),
        source_type: "Order".to_owned(),
        field_name: "legacy".to_owned(),
        target_operation: OperationId::new("svc002.getLegacy").unwrap(),
        keys: vec![JoinKeyPair {
            source_field: "id".to_owned(),
            target_argument: "id".to_owned(),
        }],
        cardinality: JoinCardinality::OneToOne,
        strategy: JoinStrategy::ForeachBatch,
        allow_per_key_fallback: false,
        max_depth: None,
        status: JoinStatus::Active,
    };
    before.joins = Arc::new(JoinGraph::new(vec![edge_removed]));

    // In `after`: branch 1 enters Drift + changes SDL signature + escalates side effect + desc_hash;
    // branch 2 is removed; branch 3 is added; active join is removed.
    let b1_after = make_branch(
        1,
        "getOrder",
        "Fetch order updated description",
        "id: ID!, region: String!",
        BranchStatus::Drift,
        SideEffectHint::SeReversible,
    );
    let b3 = make_branch(
        3,
        "getShipment",
        "Fetch shipment",
        "id: ID!",
        BranchStatus::Active,
        SideEffectHint::SeNone,
    );
    let mut after = SnapshotBuilder::new(&before, compose(&[b1_after, b3]).unwrap()).build();
    after.joins = Arc::new(JoinGraph::empty());

    let d = diff_snapshots(&before, &after);
    assert!(!d.is_empty());
    assert!(d.has_breaking_changes);
    assert!(d.branch_set_hash_changed);

    assert!(d.branches.iter().any(|b| {
        b.uid == uid_for(1)
            && b.change == BranchChangeKind::Modified
            && b.status_change == Some((BranchStatus::Active, BranchStatus::Drift))
            && b.pinned_desc_hash_changed
    }));
    assert!(
        d.branches
            .iter()
            .any(|b| { b.uid == uid_for(2) && b.change == BranchChangeKind::Removed })
    );
    assert!(
        d.branches
            .iter()
            .any(|b| { b.uid == uid_for(3) && b.change == BranchChangeKind::Added })
    );

    let op1 = d
        .operations
        .iter()
        .find(|o| o.operation_id.as_str() == "svc001.getOrder")
        .unwrap();
    assert!(op1.breaking);
    assert!(op1.changes.contains(&OperationChangeKind::SignatureChanged));
    assert!(
        op1.changes
            .contains(&OperationChangeKind::DescriptionChanged)
    );
    assert!(
        op1.changes
            .contains(&OperationChangeKind::SideEffectChanged)
    );
    assert_eq!(op1.before_side_effect, Some(SideEffectLevel::None));
    assert_eq!(op1.after_side_effect, Some(SideEffectLevel::Reversible));

    assert!(d.ownership.iter().any(|o| {
        o.coordinate.field == "getLegacy"
            && o.before_owner == Some(uid_for(2))
            && o.after_owner.is_none()
    }));
    assert!(d.ownership.iter().any(|o| {
        o.coordinate.field == "getShipment"
            && o.before_owner.is_none()
            && o.after_owner == Some(uid_for(3))
    }));
    assert!(d.joins.iter().any(|j| {
        j.edge_id == "join_order_legacy"
            && j.before_status == Some(JoinStatus::Active)
            && j.after_status.is_none()
    }));

    let rendered = d.render_text();
    assert!(rendered.contains("breaking=true"));
    assert!(rendered.contains("- branch svc002"));
    assert!(rendered.contains("+ branch svc003"));
    assert!(rendered.contains("~ op svc001.getOrder"));

    let json = d.to_canonical_json().unwrap();
    assert!(json.contains("\"has_breaking_changes\":true"));
}

// spec: ING-052, RUN-009
#[test]
fn diff_additive_only_changes_are_non_breaking() {
    let b1 = make_branch(
        1,
        "getOrder",
        "Fetch order",
        "id: ID!",
        BranchStatus::Active,
        SideEffectHint::SeNone,
    );
    let b2 = make_branch(
        2,
        "getShipment",
        "Fetch shipment",
        "id: ID!",
        BranchStatus::Active,
        SideEffectHint::SeNone,
    );
    let s1 = SnapshotBuilder::new(
        &Snapshot::empty(0),
        compose(std::slice::from_ref(&b1)).unwrap(),
    )
    .build();
    let s2 = SnapshotBuilder::new(&s1, compose(&[b1, b2]).unwrap()).build();

    let d = diff_snapshots(&s1, &s2);
    assert!(!d.is_empty());
    assert!(!d.has_breaking_changes);
    assert_eq!(d.branches.len(), 1);
    assert_eq!(d.branches[0].change, BranchChangeKind::Added);
    assert_eq!(d.operations.len(), 1);
    assert_eq!(d.operations[0].changes, vec![OperationChangeKind::Added]);
}
