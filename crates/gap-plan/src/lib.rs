//! Deterministic, ML-free supergraph query planner, predicate pushdown, and `foreach` join planner (`L2`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cache;
pub mod explain;
pub mod ir;
pub mod join;
pub mod planner;
pub mod pushdown;
pub mod traverse;

pub use crate::cache::{
    CachedPlan, DEFAULT_ESTIMATED_PLAN_BYTES, DEFAULT_PLAN_CACHE_MAX_ENTRIES,
    DEFAULT_PLAN_CACHE_TTL_SECS, PlanCache, PlanCacheConfig, PlanCacheStatus,
};
pub use crate::explain::{PlanExplanation, explain};
pub use crate::ir::{
    ArgValue, Fetch, GateNode, JoinCardinality, JoinKeyPair, JoinNode, JoinPlanStrategy,
    MutateNode, Plan, PlanCost, PlanNode, Pushdown, SelectionDirective, SelectionField,
    TraverseNode, plan_compact_json, plan_hash, plan_to_value,
};
pub use crate::planner::{PlanConfig, plan, plan_query};
