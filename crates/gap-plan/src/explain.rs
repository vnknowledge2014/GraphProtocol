//! Deterministic `PlanExplanation` builder (`spec/02-gap-ql.md` §3, `RUN-010`, `Brief §3.4`).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ir::{Plan, PlanNode, plan_to_value};

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

fn collect_pushdowns(node: &PlanNode, out: &mut BTreeSet<String>) {
    match node {
        PlanNode::Fetch(fetch) => {
            if !fetch.selection.is_empty() {
                out.insert(format!("{}.{}:projection", fetch.branch, fetch.operation));
            }
            if let Some(limit) = fetch.pushdown.limit {
                out.insert(format!(
                    "{}.{}:limit={limit}",
                    fetch.branch, fetch.operation
                ));
            }
            if !fetch.pushdown.order_by.is_empty() {
                out.insert(format!(
                    "{}.{}:order_by={}",
                    fetch.branch,
                    fetch.operation,
                    fetch.pushdown.order_by.join(",")
                ));
            }
            if fetch.pushdown.predicate.is_some() {
                out.insert(format!("{}.{}:predicate", fetch.branch, fetch.operation));
            }
        }
        PlanNode::Parallel { nodes } | PlanNode::Sequence { nodes } => {
            for child in nodes {
                collect_pushdowns(child, out);
            }
        }
        PlanNode::Flatten { child, .. } => collect_pushdowns(child, out),
        PlanNode::Join(join) => {
            collect_pushdowns(&join.lhs, out);
            collect_pushdowns(&join.rhs, out);
        }
        PlanNode::Gate(gate) => collect_pushdowns(&gate.child, out),
        PlanNode::Traverse(_) | PlanNode::Mutate(_) => {}
    }
}

/// Builds a deterministic [`PlanExplanation`] from `plan` without executing any branch call
/// (`RUN-010`, `GQL-011`).
#[must_use]
pub fn explain(plan: &Plan) -> PlanExplanation {
    let mut pushdown_set = BTreeSet::new();
    collect_pushdowns(&plan.root, &mut pushdown_set);

    let mut branches_set = BTreeSet::new();
    for b in &plan.branches {
        branches_set.insert(b.as_str().to_owned());
    }

    PlanExplanation {
        plan: plan_to_value(plan),
        branches_touched: branches_set.into_iter().collect(),
        estimated_cost: plan.cost.planned_cost,
        pushdowns: pushdown_set.into_iter().collect(),
        warnings: plan.cost.warnings.clone(),
    }
}
