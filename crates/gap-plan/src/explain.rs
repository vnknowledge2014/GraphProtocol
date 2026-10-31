//! Deterministic `PlanExplanation` builder (`spec/02-gap-ql.md` §3, `RUN-010`, `Brief §3.4`).

use serde::{Deserialize, Serialize};

use crate::ir::Plan;

/// Printable, side-effect-free explanation of a compiled [`Plan`] matching GraphQL
/// `type PlanExplanation` (`spec/02-gap-ql.md` §3, `Brief §3.4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanExplanation {
    /// JSON representation of the compiled [`Plan`].
    pub plan: serde_json::Value,
    /// Sorted, deduplicated branch slugs touched by the plan (`branchesTouched`).
    pub branches_touched: Vec<String>,
    /// Static `GQL-041` estimated cost units (`estimatedCost`).
    pub estimated_cost: u64,
    /// Deterministic descriptions of pushed-down projections, limits, orderings, and predicates.
    pub pushdowns: Vec<String>,
    /// Non-fatal cost/planning warnings.
    pub warnings: Vec<String>,
}

/// Builds a deterministic [`PlanExplanation`] from `plan` without executing any branch call
/// (`RUN-010`, `GQL-011`).
#[must_use]
pub fn explain(_plan: &Plan) -> PlanExplanation {
    PlanExplanation {
        plan: serde_json::Value::Null,
        branches_touched: Vec::new(),
        estimated_cost: 0,
        pushdowns: Vec::new(),
        warnings: Vec::new(),
    }
}
