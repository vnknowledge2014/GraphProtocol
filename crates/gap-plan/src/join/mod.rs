//! Cross-branch `ACTIVE` `@join` planning, strategy selection, and cycle checks (`WP-M3-02`).
//!
//! Spec references: `RUN-010`, `RUN-013`, `RUN-021`, `RUN-023`, `RUN-027`, `GQL-031`, `GQL-042`,
//! `INV-06`, `INV-18`.

use std::collections::BTreeMap;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::OperationId;
use gap_core::joingraph::{
    self, JoinCardinality as CoreJoinCardinality, JoinEdge, JoinStatus,
    JoinStrategy as CoreJoinStrategy, MAX_JOIN_DEPTH,
};
use gap_core::snapshot::{BranchDescriptor, BranchStatus, Snapshot};
use serde::{Deserialize, Serialize};

use crate::ir::{
    ArgValue, Fetch, JoinCardinality, JoinKeyPair, JoinNode, JoinPlanStrategy, PlanNode,
    SelectionField,
};
use crate::pushdown::{
    PushdownCapabilities, PushdownWarning, RequestedPushdown, negotiate_pushdown,
};

/// Request to attach one resolved cross-branch `@join` field onto an existing LHS plan node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinFieldPlanRequest {
    pub source_type: String,
    pub field_name: String,
    pub response_key: String,
    pub selection: Vec<SelectionField>,
    pub arguments: BTreeMap<String, ArgValue>,
    pub pushdown: RequestedPushdown,
    /// Current join recursion depth (`1` for top-level join hop).
    pub current_depth: u8,
}

/// Output of planning one or more cross-branch `@join` fields onto a base `PlanNode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedJoinResult {
    pub node: PlanNode,
    pub pushdown_warnings: Vec<PushdownWarning>,
}

/// Maps a snapshot join cardinality into a planner IR cardinality, rejecting unsupported `M:N`
/// joins cleanly (`RUN-021`).
pub fn map_join_cardinality(cardinality: CoreJoinCardinality) -> GapResult<JoinCardinality> {
    match cardinality {
        CoreJoinCardinality::OneToOne => Ok(JoinCardinality::OneToOne),
        CoreJoinCardinality::ManyToOne => Ok(JoinCardinality::ManyToOne),
        CoreJoinCardinality::OneToMany => Ok(JoinCardinality::OneToMany),
        CoreJoinCardinality::ManyToMany => Err(GapError::new(
            ErrorCode::ValidationError,
            "MANY_TO_MANY joins are not supported without a decomposition bridge type",
        )),
    }
}

/// Selects a batched execution strategy for an active `JoinEdge` against `target_branch`
/// (`RUN-023`, `RUN-027`, `INV-18`).
///
/// N+1 per-key fallback is never silently emitted: if neither `foreach_batch` (`max_batch_size > 0`)
/// nor single-key `IN` remote predicate pushdown is available, `GAP_JOIN_NOT_BATCHABLE` is raised.
pub fn select_join_strategy(
    edge: &JoinEdge,
    target_branch: &BranchDescriptor,
) -> GapResult<JoinPlanStrategy> {
    if edge.cardinality == CoreJoinCardinality::ManyToMany {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "join '{}.{}' ({}) uses unsupported MANY_TO_MANY cardinality",
                edge.source_type, edge.field_name, edge.id
            ),
        ));
    }
    if edge.keys.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "join '{}.{}' ({}) declares no join key pairs",
                edge.source_type, edge.field_name, edge.id
            ),
        ));
    }

    let supports_foreach =
        target_branch.capabilities.foreach_batch && target_branch.limits.max_batch_size > 0;
    let supports_in_predicate = edge.keys.len() == 1
        && target_branch
            .capabilities
            .predicate_ops
            .iter()
            .any(|op| op.eq_ignore_ascii_case("IN"));

    match edge.strategy {
        CoreJoinStrategy::ForeachBatch => {
            if supports_foreach {
                Ok(JoinPlanStrategy::ForeachBatch)
            } else if supports_in_predicate {
                Ok(JoinPlanStrategy::RemotePredicate)
            } else {
                Err(GapError::new(
                    ErrorCode::JoinNotBatchable,
                    format!(
                        "join '{}.{}' target branch '{}' supports neither foreach_batch (max_batch_size={}) nor single-key IN predicate pushdown",
                        edge.source_type,
                        edge.field_name,
                        target_branch.id,
                        target_branch.limits.max_batch_size
                    ),
                ))
            }
        }
        CoreJoinStrategy::PerKeyFallback => {
            // Planner v1 forbids silent N+1 plans unless a safe batched/predicate path exists;
            // validate through gap-core batchability helper and prefer batched execution if enabled.
            joingraph::validate_edge_batchability(
                edge,
                target_branch.limits.max_batch_size,
                target_branch.capabilities.foreach_batch,
            )?;
            if supports_in_predicate {
                Ok(JoinPlanStrategy::RemotePredicate)
            } else if supports_foreach {
                Ok(JoinPlanStrategy::ForeachBatch)
            } else {
                Err(GapError::new(
                    ErrorCode::JoinNotBatchable,
                    format!(
                        "join '{}.{}' requires per-key N+1 fallback which cannot be represented as a batched JoinPlanStrategy in planner v1",
                        edge.source_type, edge.field_name
                    ),
                ))
            }
        }
    }
}

/// Validates every [`JoinStatus::Active`] edge and cycle bound in `snapshot` (`INV-06`, `GQL-042`).
///
/// Non-`ACTIVE` edges (`Proposed`, `Canary`, `Quarantined`, `Disabled`) are ignored per `INV-06`.
pub fn validate_snapshot_joins(
    snapshot: &Snapshot,
    op_return_types: &BTreeMap<OperationId, String>,
) -> GapResult<()> {
    snapshot.joins.validate_join_cycles(op_return_types)?;

    for edge in snapshot.joins.active_edges() {
        if !snapshot.branches.contains_key(&edge.source_branch) {
            return Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!(
                    "join edge '{}' references unknown source branch '{}'",
                    edge.id, edge.source_branch
                ),
            ));
        }
        let target_branch = find_branch_for_operation(snapshot, &edge.target_operation)?;
        let _ = select_join_strategy(edge, target_branch)?;
    }

    Ok(())
}

/// Plans a single [`JoinEdge`] against `lhs`, returning the nested `PlanNode::Join` and any
/// bounded pushdown fallback warnings (`RUN-012`, `RUN-013`, `INV-06`).
pub fn plan_join_edge(
    lhs: PlanNode,
    edge: &JoinEdge,
    snapshot: &Snapshot,
    request: &JoinFieldPlanRequest,
    pushdown_caps_override: Option<&PushdownCapabilities>,
) -> GapResult<PlannedJoinResult> {
    if edge.status != JoinStatus::Active {
        return Err(GapError::new(
            ErrorCode::UnknownField,
            format!(
                "join field '{}.{}' is not ACTIVE (status={:?})",
                edge.source_type, edge.field_name, edge.status
            ),
        ));
    }

    let max_depth = edge.max_depth.unwrap_or(MAX_JOIN_DEPTH);
    if max_depth == 0 || max_depth > MAX_JOIN_DEPTH {
        return Err(GapError::new(
            ErrorCode::CycleUnbounded,
            format!(
                "join '{}.{}' has invalid max_depth={max_depth} (must be 1..={MAX_JOIN_DEPTH})",
                edge.source_type, edge.field_name
            ),
        ));
    }
    if request.current_depth > max_depth {
        return Err(GapError::new(
            ErrorCode::DepthExceeded,
            format!(
                "join '{}.{}' depth {} exceeds max_depth {}",
                edge.source_type, edge.field_name, request.current_depth, max_depth
            ),
        ));
    }

    let target_branch = find_branch_for_operation(snapshot, &edge.target_operation)?;
    match target_branch.status {
        BranchStatus::Quarantined => {
            return Err(GapError::new(
                ErrorCode::BranchQuarantined,
                format!("target branch '{}' is quarantined", target_branch.id),
            ));
        }
        BranchStatus::Disabled => {
            return Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!("target branch '{}' is disabled", target_branch.id),
            ));
        }
        BranchStatus::Active | BranchStatus::Drift => {}
    }

    let strategy = select_join_strategy(edge, target_branch)?;
    let cardinality = map_join_cardinality(edge.cardinality)?;

    let derived_caps;
    let effective_caps = match pushdown_caps_override {
        Some(caps) => caps,
        None => {
            derived_caps =
                PushdownCapabilities::from_snapshot_capabilities(&target_branch.capabilities);
            &derived_caps
        }
    };
    let negotiated = negotiate_pushdown(&request.pushdown, effective_caps)?;

    let keys: Vec<JoinKeyPair> = edge
        .keys
        .iter()
        .map(|k| JoinKeyPair {
            from: k.source_field.clone(),
            to: k.target_argument.clone(),
        })
        .collect();

    let rhs = PlanNode::Fetch(Fetch {
        branch: target_branch.id.clone(),
        branch_uid: target_branch.uid,
        operation: edge.target_operation.name().to_owned(),
        arguments: request.arguments.clone(),
        selection: request.selection.clone(),
        response_key: request.response_key.clone(),
        directives: Vec::new(),
        pushdown: negotiated.remote,
    });

    Ok(PlannedJoinResult {
        node: PlanNode::Join(Box::new(JoinNode {
            lhs,
            rhs,
            keys,
            cardinality,
            strategy,
        })),
        pushdown_warnings: negotiated.warnings,
    })
}

/// Resolves and folds a slice of [`JoinFieldPlanRequest`]s over `lhs` in deterministic canonical
/// order (`RUN-010`, `RUN-013`, `INV-06`).
pub fn plan_joins_for_fetch(
    lhs: PlanNode,
    snapshot: &Snapshot,
    requests: &[JoinFieldPlanRequest],
) -> GapResult<PlannedJoinResult> {
    let mut ordered: Vec<&JoinFieldPlanRequest> = requests.iter().collect();
    ordered.sort_by(|a, b| {
        (&a.source_type, &a.field_name, &a.response_key).cmp(&(
            &b.source_type,
            &b.field_name,
            &b.response_key,
        ))
    });

    let mut current = lhs;
    let mut warnings = Vec::new();
    for req in ordered {
        let edge = snapshot
            .joins
            .lookup(&req.source_type, &req.field_name)
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::UnknownField,
                    format!(
                        "no ACTIVE join edge registered for '{}.{}'",
                        req.source_type, req.field_name
                    ),
                )
            })?;
        let step = plan_join_edge(current, edge, snapshot, req, None)?;
        current = step.node;
        warnings.extend(step.pushdown_warnings);
    }

    Ok(PlannedJoinResult {
        node: current,
        pushdown_warnings: warnings,
    })
}

fn find_branch_for_operation<'a>(
    snapshot: &'a Snapshot,
    op: &OperationId,
) -> GapResult<&'a BranchDescriptor> {
    let branch_id = op.branch();
    snapshot
        .branches
        .values()
        .find(|b| b.id == branch_id)
        .map(std::convert::AsRef::as_ref)
        .ok_or_else(|| {
            GapError::new(
                ErrorCode::BranchUnavailable,
                format!("target branch '{branch_id}' for operation '{op}' not found in snapshot"),
            )
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use gap_core::ids::{BranchId, BranchUid, GroupId};
    use gap_core::joingraph::{JoinGraph, JoinKeyPair as CoreJoinKeyPair};
    use gap_core::snapshot::{
        BranchLimits, Capabilities, CostHint, Endpoint, OpKind, OperationDescriptor, Protocol,
        SideEffect, SideEffectLevel, TrustTier,
    };
    use proptest::prelude::*;
    use serde_json::json;

    use crate::ir::{Plan, PlanCost, plan_hash};
    use crate::pushdown::PredicateExpr;

    fn branch_fixture(
        name: &str,
        foreach_batch: bool,
        max_batch_size: u32,
        predicate_ops: Vec<&str>,
    ) -> Arc<BranchDescriptor> {
        let id = BranchId::new(name).unwrap();
        let uid = BranchUid::new_v7();
        let group: GroupId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        let op_name = if name == "orders" {
            "orders_list"
        } else {
            "users_by_ids"
        };
        let op_id = OperationId::from_parts(&id, op_name).unwrap();
        let op = Arc::new(OperationDescriptor {
            id: op_id,
            group,
            kind: OpKind::Query,
            side_effect: SideEffect {
                level: SideEffectLevel::None,
                verified: true,
                requires_approval: false,
            },
            idempotent: true,
            cost: CostHint::default(),
            desc_hash: [1_u8; 32],
            sdl: Arc::from(format!("{op_name}: [String]")),
            examples: Vec::new(),
        });
        Arc::new(BranchDescriptor {
            uid,
            id,
            protocol: Protocol::Graphql,
            trust: TrustTier::T0,
            endpoint: Endpoint {
                uri: format!("https://{name}.internal/graphql"),
                transport: "graphql".to_owned(),
            },
            spec_hash: [2_u8; 32],
            pinned_desc_hash: [3_u8; 32],
            capabilities: Capabilities {
                foreach_batch,
                predicate_ops: predicate_ops.into_iter().map(str::to_owned).collect(),
                ..Capabilities::default()
            },
            limits: BranchLimits {
                max_batch_size,
                max_in_flight: 16,
                timeout_ms: 2_000,
                max_response_bytes: 1_048_576,
            },
            synopsis: format!("branch {name}"),
            operations: vec![op],
            status: BranchStatus::Active,
        })
    }

    fn snapshot_with_branches_and_edges(
        branches: Vec<Arc<BranchDescriptor>>,
        edges: Vec<JoinEdge>,
    ) -> Snapshot {
        let mut snap = Snapshot::empty(1);
        for b in branches {
            snap.branches.insert(b.uid, b);
        }
        snap.joins = Arc::new(JoinGraph::new(edges));
        snap
    }

    fn base_orders_fetch(snapshot: &Snapshot) -> PlanNode {
        let orders_branch = snapshot
            .branches
            .values()
            .find(|b| b.id.as_str() == "orders")
            .unwrap();
        PlanNode::Fetch(Fetch {
            branch: orders_branch.id.clone(),
            branch_uid: orders_branch.uid,
            operation: "orders_list".to_owned(),
            arguments: BTreeMap::new(),
            selection: vec![
                SelectionField::leaf("id"),
                SelectionField::leaf("customer_id"),
            ],
            response_key: "orders".to_owned(),
            directives: Vec::new(),
            pushdown: Default::default(),
        })
    }

    fn sample_edge(
        source_branch: BranchUid,
        target_branch: &BranchDescriptor,
        status: JoinStatus,
        cardinality: CoreJoinCardinality,
        strategy: CoreJoinStrategy,
    ) -> JoinEdge {
        JoinEdge {
            id: "order_to_customer".to_owned(),
            source_branch,
            source_type: "Order".to_owned(),
            field_name: "customer".to_owned(),
            target_operation: target_branch.operations[0].id.clone(),
            keys: vec![CoreJoinKeyPair {
                source_field: "customer_id".to_owned(),
                target_argument: "ids".to_owned(),
            }],
            cardinality,
            strategy,
            allow_per_key_fallback: false,
            max_depth: Some(4),
            status,
        }
    }

    // spec: RUN-013, RUN-027, INV-06, INV-18
    #[test]
    fn inv_06_join_planning_selects_active_foreach_and_ignores_proposed_canary() {
        let orders = branch_fixture("orders", true, 50, vec![]);
        let users = branch_fixture("users", true, 100, vec![]);
        let proposed = sample_edge(
            orders.uid,
            &users,
            JoinStatus::Proposed,
            CoreJoinCardinality::ManyToOne,
            CoreJoinStrategy::ForeachBatch,
        );
        let canary = JoinEdge {
            id: "order_to_canary".to_owned(),
            field_name: "canaryCustomer".to_owned(),
            status: JoinStatus::Canary,
            ..proposed.clone()
        };
        let active = JoinEdge {
            id: "order_to_active".to_owned(),
            field_name: "activeCustomer".to_owned(),
            status: JoinStatus::Active,
            ..proposed.clone()
        };

        let snapshot = snapshot_with_branches_and_edges(
            vec![Arc::clone(&orders), Arc::clone(&users)],
            vec![proposed, canary, active],
        );
        let lhs = base_orders_fetch(&snapshot);

        // Non-active edges must not resolve in planning (`INV-06`).
        let proposed_req = JoinFieldPlanRequest {
            source_type: "Order".to_owned(),
            field_name: "customer".to_owned(),
            response_key: "customer".to_owned(),
            selection: Vec::new(),
            arguments: BTreeMap::new(),
            pushdown: RequestedPushdown::default(),
            current_depth: 1,
        };
        assert_eq!(
            plan_joins_for_fetch(lhs.clone(), &snapshot, &[proposed_req])
                .unwrap_err()
                .code(),
            ErrorCode::UnknownField
        );

        let canary_req = JoinFieldPlanRequest {
            source_type: "Order".to_owned(),
            field_name: "canaryCustomer".to_owned(),
            response_key: "canaryCustomer".to_owned(),
            selection: Vec::new(),
            arguments: BTreeMap::new(),
            pushdown: RequestedPushdown::default(),
            current_depth: 1,
        };
        assert_eq!(
            plan_joins_for_fetch(lhs.clone(), &snapshot, &[canary_req])
                .unwrap_err()
                .code(),
            ErrorCode::UnknownField
        );

        // Active edge succeeds with ForeachBatch.
        let active_req = JoinFieldPlanRequest {
            source_type: "Order".to_owned(),
            field_name: "activeCustomer".to_owned(),
            response_key: "activeCustomer".to_owned(),
            selection: vec![SelectionField::leaf("email")],
            arguments: BTreeMap::new(),
            pushdown: RequestedPushdown::default(),
            current_depth: 1,
        };
        let planned =
            plan_joins_for_fetch(lhs, &snapshot, &[active_req]).expect("active edge must plan");
        match planned.node {
            PlanNode::Join(join_node) => {
                assert_eq!(join_node.strategy, JoinPlanStrategy::ForeachBatch);
                assert_eq!(join_node.cardinality, JoinCardinality::ManyToOne);
                assert_eq!(join_node.keys.len(), 1);
                assert_eq!(join_node.keys[0].from, "customer_id");
                assert_eq!(join_node.keys[0].to, "ids");
            }
            other => panic!("expected PlanNode::Join, got {other:?}"),
        }
    }

    // spec: RUN-023, RUN-027, INV-18
    #[test]
    fn inv_18_join_uses_remote_predicate_or_rejects_non_batchable_branch() {
        let orders = branch_fixture("orders", true, 50, vec![]);
        let sql_users = branch_fixture("users", false, 0, vec!["IN", "EQ", "LIMIT"]);
        let edge_sql = sample_edge(
            orders.uid,
            &sql_users,
            JoinStatus::Active,
            CoreJoinCardinality::ManyToOne,
            CoreJoinStrategy::ForeachBatch,
        );
        let strategy = select_join_strategy(&edge_sql, &sql_users)
            .expect("single-key IN falls back to RemotePredicate");
        assert_eq!(strategy, JoinPlanStrategy::RemotePredicate);

        // Without foreach_batch and without IN predicate ops -> must reject (`GAP_JOIN_NOT_BATCHABLE`).
        let unbatchable_users = branch_fixture("users", false, 0, vec!["EQ"]);
        let err = select_join_strategy(&edge_sql, &unbatchable_users)
            .expect_err("non-batchable branch without IN pushdown must fail");
        assert_eq!(err.code(), ErrorCode::JoinNotBatchable);
    }

    // spec: RUN-021
    #[test]
    fn join_rejects_many_to_many_cardinality() {
        let orders = branch_fixture("orders", true, 50, vec![]);
        let users = branch_fixture("users", true, 100, vec![]);
        let m2m = sample_edge(
            orders.uid,
            &users,
            JoinStatus::Active,
            CoreJoinCardinality::ManyToMany,
            CoreJoinStrategy::ForeachBatch,
        );
        let err = select_join_strategy(&m2m, &users).expect_err("MANY_TO_MANY must be rejected");
        assert_eq!(err.code(), ErrorCode::ValidationError);
    }

    // spec: GQL-042
    #[test]
    fn join_cycle_without_max_depth_fails_validation_and_depth_exceeded_blocks_runtime() {
        let orders = branch_fixture("orders", true, 50, vec![]);
        let users = branch_fixture("users", true, 50, vec![]);
        let unbounded_o_u = JoinEdge {
            id: "o_u".to_owned(),
            source_branch: orders.uid,
            source_type: "Order".to_owned(),
            field_name: "customer".to_owned(),
            target_operation: users.operations[0].id.clone(),
            keys: vec![CoreJoinKeyPair {
                source_field: "customer_id".to_owned(),
                target_argument: "id".to_owned(),
            }],
            cardinality: CoreJoinCardinality::ManyToOne,
            strategy: CoreJoinStrategy::ForeachBatch,
            allow_per_key_fallback: false,
            max_depth: None,
            status: JoinStatus::Active,
        };
        let unbounded_u_o = JoinEdge {
            id: "u_o".to_owned(),
            source_branch: users.uid,
            source_type: "User".to_owned(),
            field_name: "orders".to_owned(),
            target_operation: orders.operations[0].id.clone(),
            keys: vec![CoreJoinKeyPair {
                source_field: "id".to_owned(),
                target_argument: "user_id".to_owned(),
            }],
            cardinality: CoreJoinCardinality::OneToMany,
            strategy: CoreJoinStrategy::ForeachBatch,
            allow_per_key_fallback: false,
            max_depth: None,
            status: JoinStatus::Active,
        };
        let snapshot = snapshot_with_branches_and_edges(
            vec![Arc::clone(&orders), Arc::clone(&users)],
            vec![unbounded_o_u.clone(), unbounded_u_o],
        );

        let mut ret_types = BTreeMap::new();
        ret_types.insert(orders.operations[0].id.clone(), "Order".to_owned());
        ret_types.insert(users.operations[0].id.clone(), "User".to_owned());

        let err = validate_snapshot_joins(&snapshot, &ret_types)
            .expect_err("unbounded active cycle must fail");
        assert_eq!(err.code(), ErrorCode::CycleUnbounded);

        // Runtime depth bound check during plan_join_edge
        let lhs = base_orders_fetch(&snapshot);
        let bounded = JoinEdge {
            max_depth: Some(2),
            ..unbounded_o_u
        };
        let req_too_deep = JoinFieldPlanRequest {
            source_type: "Order".to_owned(),
            field_name: "customer".to_owned(),
            response_key: "customer".to_owned(),
            selection: Vec::new(),
            arguments: BTreeMap::new(),
            pushdown: RequestedPushdown::default(),
            current_depth: 3,
        };
        let err = plan_join_edge(lhs, &bounded, &snapshot, &req_too_deep, None)
            .expect_err("depth > max_depth must fail");
        assert_eq!(err.code(), ErrorCode::DepthExceeded);
    }

    // spec: RUN-010, RUN-012, RUN-013
    proptest! {
        #[test]
        fn determinism_of_join_plan_order_and_pushdown(
            reverse in any::<bool>(),
            limit in 1u64..100u64,
        ) {
            let orders = branch_fixture("orders", true, 50, vec![]);
            let users = branch_fixture("users", true, 100, vec!["EQ", "LIMIT"]);
            let edge_customer = JoinEdge {
                id: "edge_customer".to_owned(),
                source_branch: orders.uid,
                source_type: "Order".to_owned(),
                field_name: "customer".to_owned(),
                target_operation: users.operations[0].id.clone(),
                keys: vec![CoreJoinKeyPair {
                    source_field: "customer_id".to_owned(),
                    target_argument: "ids".to_owned(),
                }],
                cardinality: CoreJoinCardinality::ManyToOne,
                strategy: CoreJoinStrategy::ForeachBatch,
                allow_per_key_fallback: false,
                max_depth: Some(4),
                status: JoinStatus::Active,
            };
            let edge_billing = JoinEdge {
                id: "edge_billing".to_owned(),
                field_name: "billingUser".to_owned(),
                ..edge_customer.clone()
            };
            let snapshot = snapshot_with_branches_and_edges(
                vec![Arc::clone(&orders), Arc::clone(&users)],
                vec![edge_customer, edge_billing],
            );
            let lhs = base_orders_fetch(&snapshot);

            let req_a = JoinFieldPlanRequest {
                source_type: "Order".to_owned(),
                field_name: "customer".to_owned(),
                response_key: "customer".to_owned(),
                selection: Vec::new(),
                arguments: BTreeMap::new(),
                pushdown: RequestedPushdown {
                    limit: Some(limit),
                    order_by: Vec::new(),
                    predicate: Some(PredicateExpr::Eq {
                        field: "active".to_owned(),
                        value: json!(true),
                    }),
                    projection: BTreeSet::new(),
                },
                current_depth: 1,
            };
            let req_b = JoinFieldPlanRequest {
                source_type: "Order".to_owned(),
                field_name: "billingUser".to_owned(),
                response_key: "billingUser".to_owned(),
                ..req_a.clone()
            };

            let list_first = vec![req_a.clone(), req_b.clone()];
            let list_second = if reverse {
                vec![req_b, req_a]
            } else {
                vec![req_a, req_b]
            };

            let planned_1 = plan_joins_for_fetch(lhs.clone(), &snapshot, &list_first).unwrap();
            let planned_2 = plan_joins_for_fetch(lhs, &snapshot, &list_second).unwrap();
            let plan_1 = Plan {
                root: planned_1.node,
                operation_name: Some("JoinOrders".to_owned()),
                cost: PlanCost {
                    planned_cost: 10,
                    effective_budget: None,
                    warnings: Vec::new(),
                },
                branches: Vec::new(),
                policy_version: 1,
            };
            let plan_2 = Plan {
                root: planned_2.node,
                operation_name: Some("JoinOrders".to_owned()),
                cost: PlanCost {
                    planned_cost: 10,
                    effective_budget: None,
                    warnings: Vec::new(),
                },
                branches: Vec::new(),
                policy_version: 1,
            };
            prop_assert_eq!(plan_hash(&plan_1), plan_hash(&plan_2));
        }
    }
}
