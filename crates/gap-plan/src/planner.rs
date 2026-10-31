//! Single-branch deterministic query planner over an immutable [`Snapshot`] (`RUN-005`, `RUN-010`,
//! `RUN-011`, `RUN-013`).

use gap_core::config::LimitsConfig;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::Snapshot;
use gap_ql::{CostModelConfig, ValidatedOperation};

use crate::ir::Plan;

/// Planner configuration governing complexity caps, static cost model parameters, and per-request
/// budget overrides (`RUN-005`, `GQL-040`, `GQL-041`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanConfig {
    /// Complexity, depth, alias, and `max_cost_units` caps (`spec/02-gap-ql.md` §8).
    pub limits: LimitsConfig,
    /// Static `GQL-041` cost-model parameters.
    pub cost_model: CostModelConfig,
    /// Optional caller cost budget override (`extensions.gap.budget.costUnits`).
    pub request_budget_cost_units: Option<u32>,
}

/// Compiles a validated GraphQL query operation into a deterministic, variable-independent [`Plan`]
/// over `snapshot` (`RUN-005`, `RUN-010`, `RUN-011`, `RUN-013`).
///
/// # Errors
///
/// Returns [`GapError`] when:
/// - the selected operation is a mutation or subscription (`GAP_UNSUPPORTED_FEATURE`),
/// - the planned `GQL-041` cost exceeds `config.limits.max_cost_units` (`GAP_COMPLEXITY_EXCEEDED`)
///   or the caller's budget (`GAP_BUDGET_EXCEEDED`) prior to any execution (`RUN-005`),
/// - a root field targets GAP built-ins / introspection (`GAP_UNSUPPORTED_FEATURE`), or
/// - a root field has no owning branch or matching `QUERY` operation in `snapshot`
///   (`GAP_UNKNOWN_FIELD`, `RUN-011`).
pub fn plan(
    _snapshot: &Snapshot,
    _document: &ValidatedOperation,
    _config: &PlanConfig,
) -> GapResult<Plan> {
    Err(GapError::new(
        ErrorCode::UnsupportedFeature,
        "planner stub not yet implemented",
    ))
}

/// Validates `query` against `snapshot.schema`, enforces structural limits, and compiles a [`Plan`]
/// via [`plan`].
///
/// # Errors
///
/// Returns [`GapError`] on parse, validation, structural-limit, cost, or ownership failure.
pub fn plan_query(
    _snapshot: &Snapshot,
    _query: &str,
    _operation_name: Option<&str>,
    _variables: &serde_json::Map<String, serde_json::Value>,
    _config: &PlanConfig,
) -> GapResult<Plan> {
    Err(GapError::new(
        ErrorCode::UnsupportedFeature,
        "plan_query stub not yet implemented",
    ))
}
