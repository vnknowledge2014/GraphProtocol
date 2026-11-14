//! Cross-branch `@join` graph types and lifecycle states (`WP-M0-08`, `WP-M3-02`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, GapError, GapResult};
use crate::ids::{BranchUid, OperationId};

/// Lifecycle state of a cross-branch join edge (`INV-06`, `GQL-031`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinStatus {
    /// Proposed by ingestion; never participates in execution planning.
    Proposed,
    /// Under shadow-traffic canary evaluation; never participates in live planning.
    Canary,
    /// Verified and active; the ONLY status allowed in planning (`INV-06`).
    Active,
    /// Automatically suspended due to drift or containment violation.
    Quarantined,
    /// Administratively disabled.
    Disabled,
}

/// Cardinality of a cross-branch join edge (`spec/02-gap-ql.md`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinCardinality {
    /// At most one RHS record per LHS key (`GAP_JOIN_KEY_NOT_UNIQUE` if violated).
    OneToOne,
    /// Zero or more RHS records per LHS key.
    OneToMany,
    /// Many LHS rows map to at most one RHS record.
    ManyToOne,
    /// Many-to-many relation.
    ManyToMany,
}

/// Execution strategy for resolving RHS join keys (`INV-18`, `NGC-015`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinStrategy {
    /// Batched `foreach` call issuing at most `ceil(unique_keys / max_batch)` calls (`INV-18`).
    ForeachBatch,
    /// Explicit per-key fallback when `allow_per_key_fallback = true` (`NGC-015`).
    PerKeyFallback,
}

/// Mapping from a parent field path to a target operation argument.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct JoinKeyPair {
    /// Field name on the source GraphQL type.
    pub source_field: String,
    /// Argument name on the target operation.
    pub target_argument: String,
}

/// Directed cross-branch join edge in the supergraph (`INV-06`, `INV-18`, `GQL-042`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinEdge {
    /// Stable join edge identifier.
    pub id: String,
    /// Branch owning the LHS GraphQL type.
    pub source_branch: BranchUid,
    /// LHS GraphQL object type name.
    pub source_type: String,
    /// Synthesized join field exposed on `source_type`.
    pub field_name: String,
    /// RHS operation invoked to resolve joined rows.
    pub target_operation: OperationId,
    /// Key bindings from LHS fields to RHS operation arguments.
    pub keys: Vec<JoinKeyPair>,
    /// Declared join cardinality.
    pub cardinality: JoinCardinality,
    /// Batching strategy (`ForeachBatch` by default, `INV-18`).
    pub strategy: JoinStrategy,
    /// Whether per-key fallback is explicitly permitted when `max_batch_size == 0` (`NGC-015`).
    pub allow_per_key_fallback: bool,
    /// Optional recursion depth cap required when edges form a cycle (`GQL-042`).
    pub max_depth: Option<u8>,
    /// Current lifecycle status (`INV-06`).
    pub status: JoinStatus,
}

/// Immutable collection of cross-branch [`JoinEdge`]s attached to a `Snapshot` (`ARC-005`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinGraph {
    edges: Arc<[JoinEdge]>,
}

impl Default for JoinGraph {
    fn default() -> Self {
        Self::empty()
    }
}

impl JoinGraph {
    /// Constructs an empty [`JoinGraph`].
    #[must_use]
    pub fn empty() -> Self {
        Self {
            edges: Arc::from([]),
        }
    }

    /// Constructs an immutable [`JoinGraph`] from `edges`.
    #[must_use]
    pub fn new(edges: Vec<JoinEdge>) -> Self {
        Self {
            edges: Arc::from(edges),
        }
    }

    /// Constructs a [`JoinGraph`] retaining only [`JoinStatus::Active`] edges sorted deterministically
    /// by edge ID (`INV-06`, `GQL-031`, `RUN-013`).
    #[must_use]
    pub fn from_active_only(edges: impl IntoIterator<Item = JoinEdge>) -> Self {
        let mut active: Vec<JoinEdge> = edges
            .into_iter()
            .filter(|edge| edge.status == JoinStatus::Active)
            .collect();
        active.sort_by(|a, b| {
            (&a.id, &a.source_type, &a.field_name).cmp(&(&b.id, &b.source_type, &b.field_name))
        });
        Self::new(active)
    }

    /// Returns all edges regardless of status (for admin inspection and composition audits).
    #[must_use]
    pub fn all_edges(&self) -> &[JoinEdge] {
        &self.edges
    }

    /// Iterates exclusively over [`JoinStatus::Active`] edges (`INV-06`).
    pub fn active_edges(&self) -> impl Iterator<Item = &JoinEdge> {
        self.edges
            .iter()
            .filter(|edge| edge.status == JoinStatus::Active)
    }

    /// Iterates in stored order over [`JoinStatus::Active`] edges leaving `source_type` (`INV-06`).
    pub fn active_edges_for_type<'a>(
        &'a self,
        source_type: &'a str,
    ) -> impl Iterator<Item = &'a JoinEdge> + 'a {
        self.active_edges()
            .filter(move |edge| edge.source_type == source_type)
    }

    /// Looks up an active [`JoinEdge`] for `(source_type, field_name)` (`INV-06`, `GQL-031`).
    ///
    /// Non-`ACTIVE` edges (`Proposed`, `Canary`, `Quarantined`, `Disabled`) are invisible.
    #[must_use]
    pub fn lookup(&self, source_type: &str, field_name: &str) -> Option<&JoinEdge> {
        self.active_edges()
            .find(|edge| edge.source_type == source_type && edge.field_name == field_name)
    }

    /// Number of [`JoinStatus::Active`] edges (`INV-06`).
    #[must_use]
    pub fn active_edge_count(&self) -> usize {
        self.active_edges().count()
    }

    /// Validates that no cycle of [`JoinStatus::Active`] join edges lacks a `max_depth` bound in
    /// `1..=16` (`GQL-042`, `GQL-040`).
    ///
    /// `op_return_types` maps each target [`OperationId`] to its GraphQL object return type name.
    /// A directed cycle of active edges where no edge carries `max_depth` in `1..=16` fails with
    /// [`ErrorCode::CycleUnbounded`] (`GAP_CYCLE_UNBOUNDED`).
    pub fn validate_join_cycles(
        &self,
        op_return_types: &BTreeMap<OperationId, String>,
    ) -> GapResult<()> {
        let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for edge in self.active_edges() {
            let has_valid_bound = matches!(edge.max_depth, Some(d) if (1..=16).contains(&d));
            if has_valid_bound {
                continue;
            }
            if let Some(target_type) = op_return_types.get(&edge.target_operation) {
                adj.entry(edge.source_type.as_str())
                    .or_default()
                    .push(target_type.as_str());
            }
        }
        for targets in adj.values_mut() {
            targets.sort_unstable();
            targets.dedup();
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Color {
            Visiting,
            Visited,
        }

        fn dfs<'a>(
            node: &'a str,
            adj: &BTreeMap<&'a str, Vec<&'a str>>,
            colors: &mut BTreeMap<&'a str, Color>,
            stack: &mut Vec<&'a str>,
        ) -> GapResult<()> {
            colors.insert(node, Color::Visiting);
            stack.push(node);
            if let Some(neighbors) = adj.get(node) {
                for &next in neighbors {
                    match colors.get(next).copied() {
                        Some(Color::Visiting) => {
                            let start = stack.iter().position(|&n| n == next).unwrap_or(0);
                            let mut cycle: Vec<&str> = stack[start..].to_vec();
                            cycle.push(next);
                            return Err(GapError::new(
                                ErrorCode::CycleUnbounded,
                                format!(
                                    "join cycle [{}] has no maxDepth-bounded active edge (GQL-042)",
                                    cycle.join(" -> ")
                                ),
                            ));
                        }
                        Some(Color::Visited) => {}
                        None => dfs(next, adj, colors, stack)?,
                    }
                }
            }
            stack.pop();
            colors.insert(node, Color::Visited);
            Ok(())
        }

        let nodes: BTreeSet<&str> = adj.keys().copied().collect();
        let mut colors: BTreeMap<&str, Color> = BTreeMap::new();
        let mut stack: Vec<&str> = Vec::new();
        for node in nodes {
            if !colors.contains_key(node) {
                dfs(node, &adj, &mut colors, &mut stack)?;
            }
        }

        for edge in self.active_edges() {
            if let Some(depth) = edge.max_depth
                && !(1..=16).contains(&depth)
            {
                return Err(GapError::new(
                    ErrorCode::DepthExceeded,
                    format!(
                        "join `{}` max_depth={depth} must be within 1..=16 (GQL-040)",
                        edge.id
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Maximum allowed join depth cap (`GQL-040`, `GQL-042`).
pub const MAX_JOIN_DEPTH: u8 = 16;

/// Validates an individual [`JoinEdge`] against its target branch capabilities and cardinality
/// rules (`RUN-021`, `RUN-027`, `NGC-015`, `INV-18`).
pub fn validate_edge_batchability(
    edge: &JoinEdge,
    target_max_batch_size: u32,
    target_supports_foreach: bool,
) -> GapResult<()> {
    if edge.cardinality == JoinCardinality::ManyToMany {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "join `{}` declares MANY_TO_MANY cardinality; model via explicit junction joins (RUN-021)",
                edge.id
            ),
        ));
    }
    if edge.keys.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("join `{}` must declare at least one join key pair", edge.id),
        ));
    }
    if edge.strategy == JoinStrategy::ForeachBatch
        && (!target_supports_foreach || target_max_batch_size == 0)
        && !edge.allow_per_key_fallback
    {
        return Err(GapError::new(
            ErrorCode::JoinNotBatchable,
            format!(
                "join `{}` target `{}` cannot batch foreach and allow_per_key_fallback=false (RUN-027)",
                edge.id, edge.target_operation
            ),
        ));
    }
    if let Some(depth) = edge.max_depth
        && (depth == 0 || depth > MAX_JOIN_DEPTH)
    {
        return Err(GapError::new(
            ErrorCode::DepthExceeded,
            format!(
                "join `{}` max_depth={depth} exceeds 1..={MAX_JOIN_DEPTH} bound (GQL-040)",
                edge.id
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edge(id: &str, status: JoinStatus) -> JoinEdge {
        JoinEdge {
            id: id.to_owned(),
            source_branch: BranchUid::new_v7(),
            source_type: "Invoice".to_owned(),
            field_name: "customer".to_owned(),
            target_operation: "crm.getCustomer".parse().expect("valid op"),
            keys: vec![JoinKeyPair {
                source_field: "customerId".to_owned(),
                target_argument: "id".to_owned(),
            }],
            cardinality: JoinCardinality::ManyToOne,
            strategy: JoinStrategy::ForeachBatch,
            allow_per_key_fallback: false,
            max_depth: Some(4),
            status,
        }
    }

    // spec: INV-06, INV-18, NGC-015
    #[test]
    fn inv_06_active_edges_filters_out_non_active_joins() {
        let empty = JoinGraph::empty();
        assert_eq!(empty.active_edge_count(), 0);

        let graph = JoinGraph::new(vec![
            sample_edge("j-proposed", JoinStatus::Proposed),
            sample_edge("j-canary", JoinStatus::Canary),
            sample_edge("j-active", JoinStatus::Active),
            sample_edge("j-quarantined", JoinStatus::Quarantined),
            sample_edge("j-disabled", JoinStatus::Disabled),
        ]);

        assert_eq!(graph.all_edges().len(), 5);
        let active: Vec<&str> = graph.active_edges().map(|e| e.id.as_str()).collect();
        assert_eq!(active, vec!["j-active"]);
        assert!(graph.lookup("Invoice", "customer").is_some());
    }

    // spec: INV-06, GQL-031, RUN-013
    #[test]
    fn inv_06_from_active_only_and_lookup_exclude_non_active_joins() {
        let mut proposed = sample_edge("b-proposed", JoinStatus::Proposed);
        proposed.field_name = "secretLink".to_owned();
        let mut active_b = sample_edge("z-active", JoinStatus::Active);
        active_b.field_name = "customer".to_owned();
        let mut active_a = sample_edge("a-active", JoinStatus::Active);
        active_a.field_name = "account".to_owned();

        let graph = JoinGraph::from_active_only(vec![proposed, active_b, active_a]);
        let ids: Vec<&str> = graph.all_edges().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["a-active", "z-active"]);
        assert!(graph.lookup("Invoice", "secretLink").is_none());
        assert_eq!(
            graph.lookup("Invoice", "customer").map(|e| e.id.as_str()),
            Some("z-active")
        );
    }

    // spec: RUN-021, RUN-027, NGC-015, INV-18
    #[test]
    fn validate_edge_batchability_enforces_cardinality_and_batch_support() {
        let mut non_batchable = sample_edge("e1", JoinStatus::Active);
        non_batchable.allow_per_key_fallback = false;
        let err = validate_edge_batchability(&non_batchable, 0, false)
            .expect_err("max_batch_size == 0 without fallback must fail GAP_JOIN_NOT_BATCHABLE");
        assert_eq!(err.code(), ErrorCode::JoinNotBatchable);

        non_batchable.allow_per_key_fallback = true;
        validate_edge_batchability(&non_batchable, 0, false)
            .expect("explicit allow_per_key_fallback passes batchability check");

        let mut many_to_many = sample_edge("e2", JoinStatus::Active);
        many_to_many.cardinality = JoinCardinality::ManyToMany;
        let err = validate_edge_batchability(&many_to_many, 100, true)
            .expect_err("MANY_TO_MANY rejected");
        assert_eq!(err.code(), ErrorCode::ValidationError);

        let mut ok_edge = sample_edge("e3", JoinStatus::Active);
        ok_edge.allow_per_key_fallback = false;
        validate_edge_batchability(&ok_edge, 100, true).expect("batchable active edge ok");
    }

    // spec: GQL-042, INV-06
    #[test]
    fn validate_join_cycles_requires_max_depth_bound_and_ignores_non_active() {
        let mut e_ab = sample_edge("ab", JoinStatus::Active);
        e_ab.source_type = "Order".to_owned();
        e_ab.field_name = "customer".to_owned();
        e_ab.target_operation = "crm.customerById".parse().expect("op");
        e_ab.max_depth = None;

        let mut e_ba = sample_edge("ba", JoinStatus::Active);
        e_ba.source_type = "Customer".to_owned();
        e_ba.field_name = "orders".to_owned();
        e_ba.target_operation = "shop.ordersByCustomer".parse().expect("op");
        e_ba.max_depth = None;

        let mut ret = BTreeMap::new();
        ret.insert(e_ab.target_operation.clone(), "Customer".to_owned());
        ret.insert(e_ba.target_operation.clone(), "Order".to_owned());

        // Unbounded active cycle -> GAP_CYCLE_UNBOUNDED
        let unbounded = JoinGraph::new(vec![e_ab.clone(), e_ba.clone()]);
        let err = unbounded
            .validate_join_cycles(&ret)
            .expect_err("unbounded active cycle must fail GAP_CYCLE_UNBOUNDED");
        assert_eq!(err.code(), ErrorCode::CycleUnbounded);

        // If the return edge is non-ACTIVE (e.g. Proposed), there is no active cycle (`INV-06`).
        let mut e_ba_proposed = e_ba.clone();
        e_ba_proposed.status = JoinStatus::Proposed;
        let inactive_cycle = JoinGraph::new(vec![e_ab.clone(), e_ba_proposed]);
        inactive_cycle
            .validate_join_cycles(&ret)
            .expect("non-active edges never form a live cycle");

        // Bounding one edge in the cycle with max_depth in 1..=16 breaks the unbounded cycle (`GQL-042`).
        let mut e_ba_bounded = e_ba;
        e_ba_bounded.max_depth = Some(4);
        let bounded = JoinGraph::new(vec![e_ab, e_ba_bounded]);
        bounded
            .validate_join_cycles(&ret)
            .expect("bounded cycle passes");
    }
}
