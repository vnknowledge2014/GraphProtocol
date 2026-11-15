//! Unit tests for incremental supergraph composition, descriptor pooling, and branch removal
//! grace-period behavior (`WP-M4-09`, `RUN-009`, `ARC-005`, `ING-055`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::error::ErrorCode;
use gap_core::ids::{BranchId, BranchUid, OperationId, TenantId};
use gap_core::snapshot::{BranchStatus, Endpoint, Protocol, Snapshot, TrustTier};
use gap_index::HybridIndexConfig;
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits, MetaSummary,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, SideEffectHint,
};

use crate::compose::{BranchInput, compose};
use crate::groups::{GroupAssignmentConfig, assign_capability_groups};
use crate::handle::SnapshotBuilder;

use super::{
    BranchRemovalTracker, DEFAULT_BRANCH_REMOVAL_GRACE, IncrementalComposer,
    RemovedOperationResolution,
};

fn uid_for(idx: u16) -> BranchUid {
    let bytes = idx.to_be_bytes();
    BranchUid::from_unix_ms(
        1_761_900_000_000 + u64::from(idx),
        [bytes[0], bytes[1], 0, 0, 0, 0, 0, 0, 0, 1],
    )
}

fn make_op(name: &str, desc: &str) -> ProtoOp {
    ProtoOp {
        name: name.to_owned(),
        kind: ProtoOpKind::Query as i32,
        description: desc.to_owned(),
        input_schema: None,
        output_schema: None,
        side_effect_hint: SideEffectHint::SeNone as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 1,
        pagination: 0,
        key_fields: Vec::new(),
        annotations: HashMap::new(),
    }
}

fn make_branch_with_ops(idx: u16, ops: &[(&str, &str)], rev: u32) -> BranchInput {
    let slug = format!("svc{idx:03}");
    let mut fields = String::new();
    let mut proto_ops = Vec::with_capacity(ops.len());
    for (op_name, desc) in ops {
        fields.push_str(&format!("  {op_name}(id: ID!): String\n"));
        proto_ops.push(make_op(op_name, desc));
    }
    let sdl = format!("type Query {{\n{fields}}}\n");

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
            connector_id: slug.clone(),
            connector_version: format!("1.{rev}.0"),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
            spec_hash: format!("{idx:063x}{rev:x}"),
            capabilities: Some(ProtoCapabilities::default()),
            limits: Some(ProtoLimits::default()),
            schema_sdl: sdl,
            operations: proto_ops,
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: format!("service {idx}"),
            }),
            auth: None,
        },
        status: BranchStatus::Active,
    }
}

fn ts_sec(secs: i64) -> Timestamp {
    Timestamp::from_unix_nanos(i128::from(secs) * 1_000_000_000).unwrap()
}

// spec: RUN-009, ARC-005
#[test]
fn incremental_single_branch_delta_shares_unchanged_arcs_across_5_generations() {
    let mut branches: Vec<BranchInput> = (0..10_u16)
        .map(|i| {
            let field = format!("getItem{i}");
            make_branch_with_ops(i, &[(&field, "read item")], 1)
        })
        .collect();

    let mut composer = IncrementalComposer::new();
    let base = Snapshot::empty(0);
    let (mut current, stats0) = composer
        .build_next_snapshot(&base, &branches, ts_sec(1_700_000_000), "incr", None)
        .unwrap();
    assert_eq!(stats0.total_branches, 10);
    assert_eq!(stats0.recompiled_branches, 10);
    assert_eq!(stats0.reused_branches, 0);

    let mut ring = Vec::with_capacity(5);
    for step in 1_u32..=5 {
        // Modify only branch 0 while leaving branches 1..10 untouched.
        branches[0] = make_branch_with_ops(
            0,
            &[("getItem0", &format!("read item rev {step}"))],
            step + 1,
        );
        let (next, stats) = composer
            .build_next_snapshot(
                &current,
                &branches,
                ts_sec(1_700_000_000 + i64::from(step) * 60),
                "incr",
                None,
            )
            .unwrap();

        assert_eq!(stats.total_branches, 10);
        assert_eq!(stats.reused_branches, 9);
        assert_eq!(stats.recompiled_branches, 1);
        assert!(!stats.schema_reused);

        // Every unchanged branch shares the exact `Arc<BranchDescriptor>` & `Arc<CapabilityGroup>`
        // pointers with the previous snapshot generation (`ARC-005`).
        for idx in 1..10_u16 {
            let uid = uid_for(idx);
            let old_br = current.branches.get(&uid).unwrap();
            let new_br = next.branches.get(&uid).unwrap();
            assert!(
                Arc::ptr_eq(old_br, new_br),
                "branch {idx} must keep Arc identity"
            );
            assert!(Arc::ptr_eq(&old_br.operations[0], &new_br.operations[0]));
            let gid = &old_br.operations[0].group;
            assert!(Arc::ptr_eq(
                current.groups.get(gid).unwrap(),
                next.groups.get(gid).unwrap()
            ));
        }

        // Output matches full scratch compose semantically.
        let scratch = SnapshotBuilder::new(&current, compose(&branches).unwrap()).build();
        assert_eq!(next.branch_set_hash, scratch.branch_set_hash);
        assert_eq!(next.ownership, scratch.ownership);

        ring.push(next.clone());
        current = next;
    }

    assert_eq!(ring.len(), 5);
}

// spec: RUN-009, ARC-005
#[test]
fn incremental_identical_inputs_reuse_schema_and_all_descriptors() {
    let branches = vec![
        make_branch_with_ops(1, &[("getOrder", "get order")], 1),
        make_branch_with_ops(2, &[("getUser", "get user")], 1),
    ];

    let mut composer = IncrementalComposer::new();
    let s0 = Snapshot::empty(0);
    let (s1, st1) = composer
        .build_next_snapshot(&s0, &branches, ts_sec(1_700_000_000), "incr", None)
        .unwrap();
    assert!(!st1.schema_reused);

    let (s2, st2) = composer
        .build_next_snapshot(&s1, &branches, ts_sec(1_700_000_060), "incr", None)
        .unwrap();
    assert!(st2.schema_reused);
    assert_eq!(st2.reused_branches, 2);
    assert_eq!(st2.recompiled_branches, 0);
    assert_eq!(st2.reused_operations, 2);
    assert!(Arc::ptr_eq(&s1.ownership, &s2.ownership));
    for idx in [1_u16, 2] {
        assert!(Arc::ptr_eq(
            s1.branches.get(&uid_for(idx)).unwrap(),
            s2.branches.get(&uid_for(idx)).unwrap()
        ));
    }
}

// spec: RUN-009, ARC-005
#[test]
fn incremental_op_pool_reuses_unchanged_ops_within_modified_branch() {
    let mut branches = vec![make_branch_with_ops(
        1,
        &[
            ("getOrder", "get order"),
            ("listOrders", "list orders"),
            ("getInvoice", "get invoice"),
        ],
        1,
    )];

    let mut composer = IncrementalComposer::new();
    let (s1, _) = composer
        .build_next_snapshot(
            &Snapshot::empty(0),
            &branches,
            ts_sec(1_700_000_000),
            "incr",
            None,
        )
        .unwrap();

    // Mutate only `getInvoice` inside the same branch.
    branches[0] = make_branch_with_ops(
        1,
        &[
            ("getOrder", "get order"),
            ("listOrders", "list orders"),
            ("getInvoice", "get customer invoice v2"),
        ],
        2,
    );
    let (s2, stats) = composer
        .build_next_snapshot(&s1, &branches, ts_sec(1_700_000_060), "incr", None)
        .unwrap();

    assert_eq!(stats.recompiled_branches, 1);
    assert_eq!(stats.reused_operations, 2);
    assert_eq!(stats.recompiled_operations, 1);

    let find_op = |snap: &Snapshot, id: &str| {
        snap.branches
            .get(&uid_for(1))
            .unwrap()
            .operations
            .iter()
            .find(|o| o.id.as_str() == id)
            .cloned()
            .unwrap()
    };
    assert!(Arc::ptr_eq(
        &find_op(&s1, "svc001.getOrder"),
        &find_op(&s2, "svc001.getOrder")
    ));
    assert!(Arc::ptr_eq(
        &find_op(&s1, "svc001.listOrders"),
        &find_op(&s2, "svc001.listOrders")
    ));
    assert!(!Arc::ptr_eq(
        &find_op(&s1, "svc001.getInvoice"),
        &find_op(&s2, "svc001.getInvoice")
    ));
}

// spec: ING-055
#[test]
fn incremental_branch_removal_grace_period_suggests_group_sibling_and_expires() {
    assert_eq!(DEFAULT_BRANCH_REMOVAL_GRACE, Duration::from_secs(86_400));

    let tenant = TenantId::new("acme").unwrap();
    // Namespaced root fields satisfy `RUN-009` uniqueness while normalizing to identical `getOrder`.
    let branch_a = make_branch_with_ops(1, &[("svc001_getOrder", "get order")], 1);
    let branch_b = make_branch_with_ops(2, &[("svc002_getOrder", "get order")], 1);
    let branch_solo = make_branch_with_ops(3, &[("getSolo", "get solo")], 1);

    let comp_all = compose(&[branch_a, branch_b.clone(), branch_solo]).unwrap();
    let raw_all = SnapshotBuilder::new(&Snapshot::empty(0), comp_all).build();

    let op_a = OperationId::new("svc001.svc001_getOrder").unwrap();
    let op_b = OperationId::new("svc002.svc002_getOrder").unwrap();
    let op_solo = OperationId::new("svc003.getSolo").unwrap();

    // Assign `svc001.svc001_getOrder` and `svc002.svc002_getOrder` into the same verified group.
    let group_cfg =
        GroupAssignmentConfig::new().with_verified_op_replica(op_a.clone(), op_b.clone());
    let branches_vec: Vec<_> = raw_all.branches.values().cloned().collect();
    let assembled = assign_capability_groups(&branches_vec, &group_cfg).unwrap();
    let snap_before = assembled
        .apply_to_snapshot(&raw_all, tenant.clone(), HybridIndexConfig::default())
        .unwrap();

    // Remove `svc001` (has sibling `svc002.svc002_getOrder`) and `svc003` (no sibling).
    let comp_after = compose(std::slice::from_ref(&branch_b)).unwrap();
    let raw_after = SnapshotBuilder::new(&snap_before, comp_after).build();
    let remaining_vec: Vec<_> = raw_after.branches.values().cloned().collect();
    let snap_after = assign_capability_groups(&remaining_vec, &GroupAssignmentConfig::new())
        .unwrap()
        .apply_to_snapshot(&raw_after, tenant, HybridIndexConfig::default())
        .unwrap();
    let b_group_before = snap_before.branches.get(&uid_for(2)).unwrap().operations[0]
        .group
        .clone();
    let mut snap_after_aligned = snap_after.clone();
    {
        let mut br = (**snap_after_aligned.branches.get(&uid_for(2)).unwrap()).clone();
        let mut op = (*br.operations[0]).clone();
        op.group = b_group_before;
        br.operations[0] = Arc::new(op);
        snap_after_aligned.branches.insert(uid_for(2), Arc::new(br));
    }

    let t0 = ts_sec(1_700_000_000);
    let mut tracker = BranchRemovalTracker::default();
    let removed_count = tracker.record_snapshot_transition(&snap_before, &snap_after_aligned, t0);
    assert_eq!(removed_count, 2);

    // Inside 24 h grace window (`t0 + 1 h`): returns `DEPRECATED` -> `GAP_BRANCH_UNAVAILABLE`.
    let t_during = ts_sec(1_700_000_000 + 3_600);
    assert_eq!(tracker.active_deprecated_operations(t_during).len(), 2);

    let err_with_alt = tracker
        .resolve_removed_operation(&snap_after_aligned, &op_a, t_during)
        .expect("removed op inside grace window must return GAP_BRANCH_UNAVAILABLE");
    assert_eq!(err_with_alt.code(), ErrorCode::BranchUnavailable);
    assert_eq!(
        err_with_alt
            .suggestion()
            .and_then(|s| s.alternative.as_deref()),
        Some("svc002.svc002_getOrder")
    );

    let err_no_alt = tracker
        .resolve_removed_operation(&snap_after_aligned, &op_solo, t_during)
        .unwrap();
    assert_eq!(err_no_alt.code(), ErrorCode::BranchUnavailable);
    assert!(
        err_no_alt
            .suggestion()
            .and_then(|s| s.alternative.as_deref())
            .is_none()
    );

    // At exactly 24 h (`t0 + 86_400 s`): operation disappears cleanly (`ING-055`).
    let t_expired = ts_sec(1_700_000_000 + 86_400);
    assert!(matches!(
        tracker.inspect_removed_operation(&snap_after_aligned, &op_a, t_expired),
        RemovedOperationResolution::Expired
    ));
    assert!(
        tracker
            .resolve_removed_operation(&snap_after_aligned, &op_a, t_expired)
            .is_none()
    );
    assert!(tracker.active_deprecated_operations(t_expired).is_empty());
    assert_eq!(tracker.purge_expired(t_expired), 2);
}
