//! Cross-branch `@join` graph types and lifecycle states (`WP-M0-08`, `WP-M3-02`).

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

    /// Number of [`JoinStatus::Active`] edges (`INV-06`).
    #[must_use]
    pub fn active_edge_count(&self) -> usize {
        self.active_edges().count()
    }
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
    }
}
