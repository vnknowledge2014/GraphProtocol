//! Deterministic Cedar authorization (`SEC-012`, `SEC-020`..`SEC-024`, `INV-04`, `INV-05`, `RTE-010`).

pub mod engine;
pub mod layer;
pub mod parser;
pub mod schema;

#[cfg(test)]
mod tests;

pub use engine::{
    AuthzContext, CompiledPolicySet, DecisionKind, EvalDecision, PolicyStore, ResourceAttributes,
    config_policy_error,
};
pub use layer::{
    DecisionLogEntry, DecisionLogger, FieldClassification, FieldDecision, FieldDecisionOutcome,
    GENERIC_FORBIDDEN_MESSAGE, InMemoryDecisionLog, PlanAuthzReport, QueryDenyPolicy,
    TouchedFieldRef, TouchedPlanSurface, authorize_plan_surface, should_log_decision,
    visibility_bitset,
};
pub use parser::{
    ActionConstraint, AttrRoot, ClauseKind, CmpOp, Effect, EntityUid, Expr, Literal, Policy,
    PrincipalConstraint, ResourceConstraint, parse_and_validate_policies,
};
pub use schema::{
    AuthzAction, CedarSchema, DEFAULT_CEDAR_POLICIES, DEFAULT_CEDAR_SCHEMA, ResourceKind,
    SchemaAttrType,
};
