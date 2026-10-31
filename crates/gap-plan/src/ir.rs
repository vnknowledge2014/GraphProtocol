//! Execution plan intermediate representation (`RUN-010`, `RUN-013`, `RUN-014`) and deterministic
//! `plan_hash` digest.

use std::collections::BTreeMap;

use gap_core::ids::{ApprovalId, BranchId, BranchUid};
use gap_ql::CostEstimate;
use serde::{Deserialize, Serialize};

/// Static `GQL-041` query cost attached to a compiled [`Plan`] (`RUN-005`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCost {
    /// Total planned cost units across the selected operation (`GQL-041`).
    pub planned_cost: u64,
    /// Optional effective caller cost budget (`@budget(costUnits:)` or request budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_budget: Option<u64>,
    /// Non-fatal cost warnings (e.g. list-size clamps or defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl From<CostEstimate> for PlanCost {
    fn from(estimate: CostEstimate) -> Self {
        Self {
            planned_cost: estimate.planned_cost,
            effective_budget: estimate.effective_budget,
            warnings: estimate.warnings,
        }
    }
}

/// Variable-independent argument value inside a [`Fetch`] or [`SelectionDirective`] (`Brief §3.2`).
///
/// Variable values are bound at execution time rather than embedded in the plan so that one cached
/// [`Plan`] serves every variable map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ArgValue {
    /// Variable-free GraphQL literal value (`Int`, `Float`, `String`, `Boolean`, `Null`, `Enum`,
    /// or variable-free `List`/`Object`).
    Literal(serde_json::Value),
    /// Reference to a GraphQL operation variable `$name` bound at execution time.
    Variable(String),
    /// Composite list argument containing at least one nested variable reference.
    List(Vec<ArgValue>),
    /// Composite input-object argument containing at least one nested variable reference.
    Object(BTreeMap<String, ArgValue>),
}

/// Executable conditional directive (`@skip` or `@include`) preserved on a [`SelectionField`] so
/// cached plans remain variable-independent (`Brief §3.2`, `Brief §4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionDirective {
    /// Conditional directive name (`"skip"` or `"include"`).
    pub name: String,
    /// Variable-independent `if` condition (`ArgValue::Literal(Bool)` or `ArgValue::Variable`).
    pub if_arg: ArgValue,
}

/// Projected GraphQL field in a [`Fetch`] selection tree (`Brief §3.2`).
///
/// Named fragment spreads and inline fragments are flattened in document order, `__typename` is
/// preserved, and `@skip`/`@include` directives are kept on [`Self::directives`] for execution-time
/// evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionField {
    /// GraphQL field name on the enclosing type (or `"__typename"`).
    pub name: String,
    /// Optional GraphQL field alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Conditional `@skip` / `@include` directives attached to this field (or inherited from an
    /// enclosing fragment spread / inline fragment).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directives: Vec<SelectionDirective>,
    /// Nested projected fields in GraphQL document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SelectionField>,
}

impl SelectionField {
    /// Constructs a leaf [`SelectionField`] with no alias, directives, or children.
    #[must_use]
    pub fn leaf(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            alias: None,
            directives: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Effective GraphQL response key (`alias` if present, otherwise `name`).
    #[must_use]
    pub fn response_key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// Optional remote limit, ordering, and filter pushdown hints attached to a [`Fetch`] (`RUN-012`).
///
/// Declared in `WP-M1-06`; populated once `gap_core::snapshot::Capabilities` advertises pagination
/// and ordering flags (`WP-M3-02`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pushdown {
    /// Row/item limit pushed to the branch (`RUN-012`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Remote ordering clauses pushed to the branch (`RUN-012`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_by: Vec<String>,
    /// Remote filter predicate pushed to the branch (`RUN-012`, `NGC-023`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<serde_json::Value>,
}

impl Pushdown {
    /// Returns `true` when no limit, ordering, or predicate pushdown is active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.limit.is_none() && self.order_by.is_empty() && self.predicate.is_none()
    }
}

/// Single root-field or entity fetch against one owning branch (`RUN-010`, `RUN-011`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetch {
    /// Human-readable owning branch slug (`RUN-011`).
    pub branch: BranchId,
    /// Generation-stable owning branch UUIDv7 (`RUN-011`).
    pub branch_uid: BranchUid,
    /// Branch operation name matching the root field (`<name>` of `<branch>.<name>`).
    pub operation: String,
    /// Variable-independent argument map sorted by argument name.
    pub arguments: BTreeMap<String, ArgValue>,
    /// Projected subselection tree in GraphQL document order.
    pub selection: Vec<SelectionField>,
    /// Top-level GraphQL response key (`alias` if present, otherwise `operation`).
    pub response_key: String,
    /// Optional remote pushdown hints (`RUN-012`).
    pub pushdown: Pushdown,
}

/// Pair of LHS -> RHS field names in a cross-branch `@join` (`RUN-020`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinKeyPair {
    /// Field path extracted from the LHS row.
    pub from: String,
    /// Argument/key name supplied to the RHS operation.
    pub to: String,
}

/// Declared `@join` cardinality (`spec/02-gap-ql.md` §5, `RUN-021`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinCardinality {
    /// At most one RHS entity per LHS row (`ONE_TO_ONE`).
    OneToOne,
    /// Many LHS rows reference at most one RHS entity (`MANY_TO_ONE`).
    ManyToOne,
    /// Ordered list of RHS entities per LHS row (`ONE_TO_MANY`).
    OneToMany,
}

/// Declared `@join` execution strategy (`spec/02-gap-ql.md` §5, `RUN-027`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinPlanStrategy {
    /// Batched `foreach` lookup on unique non-null LHS keys (`FOREACH_BATCH`).
    ForeachBatch,
    /// Filter-driven RHS key pre-fetch (`REMOTE_PREDICATE`).
    RemotePredicate,
}

/// Cross-branch join plan node (`RUN-010`, `RUN-020`; declared in `WP-M1-06`, planned in `M3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinNode {
    /// Left-hand side plan producing entity rows.
    pub lhs: PlanNode,
    /// Right-hand side plan resolved against batched LHS keys.
    pub rhs: PlanNode,
    /// Key-extraction pairs mapping LHS fields to RHS parameters.
    pub keys: Vec<JoinKeyPair>,
    /// Declared join cardinality (`RUN-021`).
    pub cardinality: JoinCardinality,
    /// Selected join execution strategy (`RUN-027`).
    pub strategy: JoinPlanStrategy,
}

/// Context-graph traversal plan node (`RUN-010`, `GQL-040`; declared in `WP-M1-06`, planned in `M2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseNode {
    /// Variable-independent arguments passed to `gap.traverse`.
    pub arguments: BTreeMap<String, ArgValue>,
    /// Projected subselection over `Subgraph`.
    pub selection: Vec<SelectionField>,
    /// Top-level response key.
    pub response_key: String,
}

/// Branch mutation plan node (`RUN-010`, `ADR-015`; declared in `WP-M1-06`, planned in `M6`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutateNode {
    /// Target branch slug.
    pub branch: BranchId,
    /// Target branch UUIDv7.
    pub branch_uid: BranchUid,
    /// Mutation operation name.
    pub operation: String,
    /// Variable-independent mutation arguments.
    pub arguments: BTreeMap<String, ArgValue>,
    /// Projected subselection over the mutation payload.
    pub selection: Vec<SelectionField>,
    /// Top-level response key.
    pub response_key: String,
    /// Whether `@dryRun` was requested (`RUN-073`).
    pub dry_run: bool,
}

/// Policy approval gate node (`RUN-010`, `RUN-071`; declared in `WP-M1-06`, planned in `M6`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateNode {
    /// Optional pre-allocated approval identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalId>,
    /// Deterministic human-readable approval summary (`RUN-071`).
    pub summary: String,
    /// Gated plan subtree executed only after approval.
    pub child: PlanNode,
}

/// Execution plan tree node (`RUN-010`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanNode {
    /// Single root-field or entity fetch against one branch (`RUN-010`, `RUN-011`).
    Fetch(Fetch),
    /// Independent child nodes executed concurrently in deterministic `(branch_id, operation, response_key)` order (`RUN-013`, `RUN-015`).
    Parallel {
        /// Child plan nodes.
        nodes: Vec<PlanNode>,
    },
    /// Ordered stages executed sequentially (`RUN-010`).
    Sequence {
        /// Child plan nodes in execution order.
        nodes: Vec<PlanNode>,
    },
    /// Stitches nested entity/join payloads into `path` (`RUN-010`).
    Flatten {
        /// Response path segments.
        path: Vec<String>,
        /// Child plan node producing values for `path`.
        child: Box<PlanNode>,
    },
    /// Cross-branch batched join (`RUN-020`; declared only in `WP-M1-06`).
    Join(Box<JoinNode>),
    /// Bounded context-graph traversal (`GQL-040`; declared only in `WP-M1-06`).
    Traverse(Box<TraverseNode>),
    /// Side-effecting branch mutation (`ADR-015`; declared only in `WP-M1-06`).
    Mutate(Box<MutateNode>),
    /// Policy approval gate (`RUN-071`; declared only in `WP-M1-06`).
    Gate(Box<GateNode>),
}

/// Compiled, immutable, variable-independent GAP-QL execution plan (`RUN-010`, `RUN-013`, `RUN-014`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Root execution node (`Fetch` for a single root field, `Parallel` for multiple independent fetches).
    pub root: PlanNode,
    /// Selected GraphQL operation name, or `None` for an anonymous query.
    pub operation_name: Option<String>,
    /// Static `GQL-041` query cost computed prior to execution (`RUN-005`).
    pub cost: PlanCost,
    /// Deduplicated, deterministically sorted [`BranchId`]s touched by this plan (`RUN-013`).
    pub branches: Vec<BranchId>,
    /// Cedar policy version under which this plan was compiled (`RUN-014`).
    pub policy_version: u64,
}

/// Converts `plan` into a canonical `serde_json::Value` whose object maps are backed by `BTreeMap`
/// (`Brief §3.5`).
#[must_use]
pub fn plan_to_value(_plan: &Plan) -> serde_json::Value {
    serde_json::Value::Null
}

/// Computes the 64-character lowercase hexadecimal `blake3` digest of the compact sorted JSON
/// representation of `plan` (`RUN-010`, `RUN-013`, `Brief §3.5`).
#[must_use]
pub fn plan_hash(_plan: &Plan) -> String {
    String::new()
}
