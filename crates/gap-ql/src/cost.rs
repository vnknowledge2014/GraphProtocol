//! Static query cost model (`GQL-041`) and `@join` cycle boundedness verification (`GQL-042`,
//! `spec/02-gap-ql.md` §§5, 8, 10).

use std::collections::{BTreeMap, HashMap, HashSet};

use apollo_compiler::Name;
use apollo_compiler::Schema;
use apollo_compiler::ast::{DirectiveList, Type, Value};
use apollo_compiler::executable::Operation;
use apollo_compiler::response::{JsonMap, JsonValue};
use apollo_compiler::schema::{ExtendedType, FieldDefinition};
use gap_core::config::LimitsConfig;
use gap_core::error::{ErrorCode, GapError, GapResult};

use crate::limits::{extract_and_check_traverse_bounds, extract_i64_from_ast_or_vars};
use crate::validate::ValidatedOperation;

/// Default per-hop branching cap in `GQL-041`'s `traverse` `visited_bound` formula when no custom
/// cap is configured (`UNSPECIFIED` constant documented in `WP-M1-01` handoff).
pub const DEFAULT_TRAVERSE_BRANCHING_CAP: u32 = 8;

/// Static cost model configuration (`GQL-041`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostModelConfig {
    /// Branching factor cap used in `visited_bound = sum_{d=1..=maxDepth} min(first, branching_cap)^d`
    /// truncated by `first`.
    pub traverse_branching_cap: u32,
}

impl Default for CostModelConfig {
    fn default() -> Self {
        Self {
            traverse_branching_cap: DEFAULT_TRAVERSE_BRANCHING_CAP,
        }
    }
}

/// Result of static query cost analysis prior to execution (`GQL-041`, `RUN-005`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostEstimate {
    /// Total planned cost units across the selected operation.
    pub planned_cost: u64,
    /// Optional caller cost budget extracted from `@budget(costUnits:)` and/or request extensions.
    pub effective_budget: Option<u64>,
    /// Non-fatal clamp warnings (e.g., list field omitted `first`/`limit`/`pageSize` and fell back
    /// to `limits.default_list_size`).
    pub warnings: Vec<String>,
}

/// Computes `visited_bound = (sum_{d=1..=maxDepth} min(first, branching_cap)^d).min(first)` using
/// saturating 64-bit arithmetic (`GQL-041`).
#[must_use]
pub fn traverse_visited_bound(first: u32, max_depth: u32, branching_cap: u32) -> u64 {
    let first_u64 = u64::from(first);
    if first_u64 == 0 || max_depth == 0 {
        return 0;
    }
    let base = first_u64.min(u64::from(branching_cap.max(1)));
    let mut sum = 0_u64;
    for depth in 1..=max_depth {
        let term = base.saturating_pow(depth);
        sum = sum.saturating_add(term);
        if sum >= first_u64 {
            return first_u64;
        }
    }
    sum.min(first_u64)
}

#[derive(Debug, Clone, Copy)]
struct ParsedFieldCost {
    weight: u64,
    list_size: Option<u64>,
    per_key: Option<u64>,
}

fn parse_cost_directive(directives: &DirectiveList) -> ParsedFieldCost {
    let mut out = ParsedFieldCost {
        weight: 1,
        list_size: None,
        per_key: None,
    };
    let Some(cost_dir) = directives.get("cost") else {
        return out;
    };
    for arg in &cost_dir.arguments {
        match arg.name.as_str() {
            "weight" => {
                if let Value::Int(v) = &*arg.value
                    && let Ok(i) = v.try_to_i32()
                {
                    out.weight = u64::try_from(i.max(0)).unwrap_or(1);
                }
            }
            "listSize" => {
                if let Value::Int(v) = &*arg.value
                    && let Ok(i) = v.try_to_i32()
                    && i >= 0
                {
                    out.list_size = u64::try_from(i).ok();
                }
            }
            "perKey" => {
                if let Value::Int(v) = &*arg.value
                    && let Ok(i) = v.try_to_i32()
                    && i >= 0
                {
                    out.per_key = u64::try_from(i).ok();
                }
            }
            _ => {}
        }
    }
    out
}

fn is_active_join(directives: &DirectiveList) -> bool {
    let Some(join_dir) = directives.get("join") else {
        return false;
    };
    for arg in &join_dir.arguments {
        if arg.name.as_str() == "status"
            && let Value::Enum(status_name) = &*arg.value
        {
            return status_name.as_str() == "ACTIVE";
        }
    }
    // Default in SDL is `status: JoinStatus = PROPOSED` (GQL-031: only ACTIVE joins are planned).
    false
}

fn is_non_deprecated_join(directives: &DirectiveList) -> bool {
    let Some(join_dir) = directives.get("join") else {
        return false;
    };
    for arg in &join_dir.arguments {
        if arg.name.as_str() == "status"
            && let Value::Enum(status_name) = &*arg.value
        {
            return status_name.as_str() != "DEPRECATED";
        }
    }
    true
}

fn is_list_type(ty: &Type) -> bool {
    matches!(ty, Type::List(_) | Type::NonNullList(_))
}

fn extract_list_limit_arg(
    field: &apollo_compiler::executable::Field,
    coerced_variables: &JsonMap,
) -> Option<u64> {
    for arg_name in ["first", "limit", "pageSize"] {
        if let Some(arg) = field.arguments.iter().find(|a| a.name.as_str() == arg_name)
            && let Some(raw) = extract_i64_from_ast_or_vars(&arg.value, coerced_variables)
        {
            return Some(u64::try_from(raw.max(0)).unwrap_or(0));
        }
    }
    // Check schema default value on `first` / `limit` / `pageSize` if present on definition.
    for arg_name in ["first", "limit", "pageSize"] {
        if let Some(arg_def) = field
            .definition
            .arguments
            .iter()
            .find(|d| d.name.as_str() == arg_name)
            && let Some(default_val) = &arg_def.default_value
            && let Value::Int(v) = &**default_val
            && let Ok(i) = v.try_to_i32()
        {
            return Some(u64::try_from(i.max(0)).unwrap_or(0));
        }
    }
    None
}

/// Extracts `@budget(costUnits: ...)` from an operation's directives if present.
#[must_use]
pub fn extract_operation_budget_cost_units(
    op: &Operation,
    coerced_variables: &JsonMap,
) -> Option<u64> {
    let budget_dir = op.directives.get("budget")?;
    let arg = budget_dir
        .arguments
        .iter()
        .find(|a| a.name.as_str() == "costUnits")?;
    let val = match &*arg.value {
        Value::Int(v) => v.try_to_i32().ok().map(i64::from),
        Value::Variable(var_name) => coerced_variables
            .get(var_name.as_str())
            .and_then(JsonValue::as_i64),
        _ => None,
    }?;
    u64::try_from(val.max(0)).ok()
}

struct CostWalkCtx<'a> {
    doc: &'a apollo_compiler::ExecutableDocument,
    coerced_variables: &'a JsonMap,
    limits: &'a LimitsConfig,
    config: CostModelConfig,
    warnings: Vec<String>,
}

impl CostWalkCtx<'_> {
    fn selection_set_cost(
        &mut self,
        parent_type: &str,
        selections: &[apollo_compiler::executable::Selection],
        parent_m: u64,
        visiting_fragments: &mut HashSet<Name>,
    ) -> GapResult<u64> {
        let mut sum = 0_u64;
        for sel in selections {
            match sel {
                apollo_compiler::executable::Selection::Field(field) => {
                    let field_cost =
                        self.field_cost(parent_type, field, parent_m, visiting_fragments)?;
                    sum = sum.saturating_add(field_cost);
                }
                apollo_compiler::executable::Selection::InlineFragment(inline) => {
                    let next_parent = inline
                        .type_condition
                        .as_ref()
                        .map_or(parent_type, Name::as_str);
                    let sub = self.selection_set_cost(
                        next_parent,
                        &inline.selection_set.selections,
                        parent_m,
                        visiting_fragments,
                    )?;
                    sum = sum.saturating_add(sub);
                }
                apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                    if visiting_fragments.insert(spread.fragment_name.clone()) {
                        if let Some(frag) = self.doc.fragments.get(&spread.fragment_name) {
                            let sub = self.selection_set_cost(
                                frag.type_condition().as_str(),
                                &frag.selection_set.selections,
                                parent_m,
                                visiting_fragments,
                            )?;
                            sum = sum.saturating_add(sub);
                        }
                        visiting_fragments.remove(&spread.fragment_name);
                    }
                }
            }
        }
        Ok(sum)
    }

    fn field_cost(
        &mut self,
        parent_type: &str,
        field: &apollo_compiler::executable::Field,
        parent_m: u64,
        visiting_fragments: &mut HashSet<Name>,
    ) -> GapResult<u64> {
        let parsed_cost = parse_cost_directive(&field.definition.directives);
        let default_list = u64::from(self.limits.default_list_size);

        let m_f = if is_list_type(&field.definition.ty) {
            let arg_bound = extract_list_limit_arg(field, self.coerced_variables);
            let mut m = default_list;
            if let Some(cost_ls) = parsed_cost.list_size {
                m = m.min(cost_ls);
            }
            if let Some(arg_ls) = arg_bound {
                if arg_ls > m {
                    self.warnings.push(format!(
                        "list size {arg_ls} on `{parent_type}.{}` clamped to {m}",
                        field.name
                    ));
                }
                m = m.min(arg_ls);
            } else if parsed_cost.list_size.is_none() {
                self.warnings.push(format!(
                    "unspecified list size on `{parent_type}.{}` defaulted to {default_list}",
                    field.name
                ));
            }
            m
        } else {
            1
        };

        let mut base_cost = if parent_type == "GapQuery" && field.name.as_str() == "traverse" {
            let bounds =
                extract_and_check_traverse_bounds(field, self.coerced_variables, self.limits)?;
            traverse_visited_bound(
                bounds.first,
                bounds.max_depth,
                self.config.traverse_branching_cap,
            )
        } else {
            parsed_cost.weight
        };

        if is_active_join(&field.definition.directives) {
            let per_key = parsed_cost.per_key.unwrap_or(1);
            base_cost = base_cost.saturating_add(per_key.saturating_mul(parent_m));
        }

        let next_parent = field.definition.ty.inner_named_type().as_str();
        let children_cost = self.selection_set_cost(
            next_parent,
            &field.selection_set.selections,
            m_f,
            visiting_fragments,
        )?;

        Ok(base_cost.saturating_add(m_f.saturating_mul(children_cost)))
    }
}

/// Computes the static `GQL-041` query cost of `validated` without enforcing caps.
pub fn compute_operation_cost(
    validated: &ValidatedOperation,
    limits: &LimitsConfig,
    request_budget_cost_units: Option<u32>,
    config: CostModelConfig,
) -> GapResult<CostEstimate> {
    let Some(op) = validated.operation() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "validated operation handle is missing target operation",
        ));
    };

    let directive_budget = extract_operation_budget_cost_units(op, &validated.coerced_variables);
    let effective_budget = match (directive_budget, request_budget_cost_units.map(u64::from)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let mut ctx = CostWalkCtx {
        doc: &validated.document,
        coerced_variables: &validated.coerced_variables,
        limits,
        config,
        warnings: Vec::new(),
    };
    let mut visiting = HashSet::new();
    let planned_cost = ctx.selection_set_cost(
        op.object_type().as_str(),
        &op.selection_set.selections,
        1,
        &mut visiting,
    )?;

    Ok(CostEstimate {
        planned_cost,
        effective_budget,
        warnings: ctx.warnings,
    })
}

/// Computes and enforces `GQL-041` cost caps against both the caller budget (`GAP_BUDGET_EXCEEDED`)
/// and `limits.max_cost_units` (`GAP_COMPLEXITY_EXCEEDED`) prior to execution (`RUN-005`).
pub fn enforce_operation_cost(
    validated: &ValidatedOperation,
    limits: &LimitsConfig,
    request_budget_cost_units: Option<u32>,
    config: CostModelConfig,
) -> GapResult<CostEstimate> {
    let estimate = compute_operation_cost(validated, limits, request_budget_cost_units, config)?;

    let max_system = u64::from(limits.max_cost_units);
    if estimate.planned_cost > max_system {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!(
                "planned query cost {} exceeds maximum {max_system}",
                estimate.planned_cost
            ),
        ));
    }

    if let Some(budget) = estimate.effective_budget
        && estimate.planned_cost > budget
    {
        return Err(GapError::new(
            ErrorCode::BudgetExceeded,
            format!(
                "planned query cost {} exceeds budget {budget}",
                estimate.planned_cost
            ),
        ));
    }

    Ok(estimate)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

fn field_has_max_depth_bound(field_def: &FieldDefinition) -> bool {
    field_def
        .arguments
        .iter()
        .any(|arg| arg.name.as_str() == "maxDepth")
}

fn find_unbounded_join_cycle<'a>(
    node: &'a str,
    adj: &BTreeMap<&'a str, Vec<(&'a str, &'a str)>>,
    state: &mut HashMap<&'a str, VisitState>,
    trail: &mut Vec<(&'a str, &'a str)>,
) -> Option<String> {
    state.insert(node, VisitState::Visiting);
    if let Some(edges) = adj.get(node) {
        for &(field_name, dst) in edges {
            trail.push((node, field_name));
            match state.get(dst).copied() {
                Some(VisitState::Visiting) => {
                    let start_idx = trail.iter().position(|(src, _)| *src == dst).unwrap_or(0);
                    let chain: Vec<String> = trail[start_idx..]
                        .iter()
                        .map(|(src, f)| format!("{src}.{f}"))
                        .collect();
                    return Some(format!("{} -> {dst}", chain.join(" -> ")));
                }
                Some(VisitState::Visited) => {}
                None => {
                    if let Some(cycle) = find_unbounded_join_cycle(dst, adj, state, trail) {
                        return Some(cycle);
                    }
                }
            }
            trail.pop();
        }
    }
    state.insert(node, VisitState::Visited);
    None
}

/// Enforces `GQL-042` on `schema`: ordinary object type cycles are permitted, whereas `@join`
/// edges forming a directed cycle with no `maxDepth`-bounded entry are rejected with
/// [`ErrorCode::CycleUnbounded`] (`GAP_CYCLE_UNBOUNDED`).
pub fn validate_join_cycles(schema: &Schema) -> GapResult<()> {
    let mut unbounded_adj: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();

    for (type_name, ext_ty) in &schema.types {
        let fields = match ext_ty {
            ExtendedType::Object(obj) => &obj.fields,
            ExtendedType::Interface(iface) => &iface.fields,
            _ => continue,
        };
        for (field_name, field_def) in fields {
            if !is_non_deprecated_join(&field_def.directives) {
                continue;
            }
            if field_has_max_depth_bound(field_def) {
                continue;
            }
            let target_type = field_def.ty.inner_named_type().as_str();
            unbounded_adj
                .entry(type_name.as_str())
                .or_default()
                .push((field_name.as_str(), target_type));
        }
    }

    let mut state: HashMap<&str, VisitState> = HashMap::new();
    let mut trail: Vec<(&str, &str)> = Vec::new();
    for &start_node in unbounded_adj.keys() {
        if !state.contains_key(start_node)
            && let Some(cycle_desc) =
                find_unbounded_join_cycle(start_node, &unbounded_adj, &mut state, &mut trail)
        {
            return Err(GapError::new(
                ErrorCode::CycleUnbounded,
                format!(
                    "unbounded @join cycle `{cycle_desc}` has no maxDepth-bounded entry (GQL-042)"
                ),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::limits::{
        check_ast_pre_validation_limits, check_request_body_size, enforce_operation_limits,
    };
    use crate::schema::{builtin_schema, with_gap_builtins};
    use crate::validate::{parse_executable_ast, validate_document, validate_query};

    fn cyclic_and_list_supergraph() -> apollo_compiler::validation::Valid<Schema> {
        with_gap_builtins(
            r#"
            type Query {
                users(first: Int): [User!]! @cost(weight: 2, listSize: 200)
                orders(limit: Int): [Order!]! @cost(weight: 1)
            }
            type User @key(fields: "id") {
                id: ID!
                name: String!
                friends(first: Int): [User!]! @cost(weight: 1, listSize: 50)
                recentOrders(first: Int): [Order!]!
                    @join(to: "orders.byUser", on: [{ from: "id", to: "userId" }], cardinality: ONE_TO_MANY, status: ACTIVE)
                    @cost(weight: 2, listSize: 25, perKey: 3)
            }
            type Order @key(fields: "id") {
                id: ID!
                userId: ID!
                amount: Float! @cost(weight: 5)
            }
            "#,
        )
        .expect("compose cyclic object supergraph")
    }

    // spec: GQL-040, GQL-041, GQL-072
    #[test]
    fn request_limits_reject_adversarial_inputs_with_computed_vs_max_messages() {
        let schema = cyclic_and_list_supergraph();
        // 1. Oversized body (> max_request_body_bytes)
        let mut limits = LimitsConfig {
            max_request_body_bytes: 64,
            ..LimitsConfig::default()
        };
        let big_body = vec![b' '; 128];
        let body_err = check_request_body_size(&big_body, &limits).expect_err("oversized body");
        assert_eq!(body_err.code(), ErrorCode::ComplexityExceeded);
        assert!(body_err.message().contains("128") && body_err.message().contains("64"));
        limits.max_request_body_bytes = 1_048_576;

        // 2. Deep nesting (> max_selection_depth)
        limits.max_selection_depth = 4;
        let deep_query = "{ users(first: 1) { friends(first: 1) { friends(first: 1) { friends(first: 1) { id } } } } }";
        let validated_deep = validate_query(&schema, deep_query).expect("valid GraphQL");
        let depth_err = enforce_operation_limits(&validated_deep, deep_query.len(), 0, &limits)
            .expect_err("depth 5 > 4 must fail");
        assert_eq!(depth_err.code(), ErrorCode::DepthExceeded);
        assert!(depth_err.message().contains('5') && depth_err.message().contains('4'));
        limits.max_selection_depth = 12;

        // 3. Alias explosion via fragment amplification (> max_aliases)
        limits.max_aliases = 5;
        let alias_bomb = r#"
            fragment F1 on User { a1: id a2: name a3: id }
            { users(first: 1) { ...F1 b1: id b2: name b3: id } }
        "#;
        let validated_alias = validate_query(&schema, alias_bomb).expect("valid GraphQL");
        let alias_err = enforce_operation_limits(&validated_alias, alias_bomb.len(), 0, &limits)
            .expect_err("6 aliases > 5 must fail");
        assert_eq!(alias_err.code(), ErrorCode::ComplexityExceeded);
        assert!(alias_err.message().contains('6') && alias_err.message().contains('5'));
        limits.max_aliases = 50;

        // 4. Excessive root fields (> max_root_fields)
        limits.max_root_fields = 3;
        let root_flood = "{ r1: __metaGraph { generation } r2: __metaGraph { generation } r3: __metaGraph { generation } r4: __metaGraph { generation } }";
        let validated_roots = validate_query(&schema, root_flood).expect("valid GraphQL");
        let root_err = enforce_operation_limits(&validated_roots, root_flood.len(), 0, &limits)
            .expect_err("4 root fields > 3 must fail");
        assert_eq!(root_err.code(), ErrorCode::ComplexityExceeded);
        assert!(root_err.message().contains('4') && root_err.message().contains('3'));
        limits.max_root_fields = 20;

        // 5. Huge variable map (> max_variables)
        limits.max_variables = 3;
        let ast_doc = parse_executable_ast("{ __metaGraph { generation } }", "q.graphql")
            .expect("parse query");
        let var_err = check_ast_pre_validation_limits(&ast_doc, 10, &limits)
            .expect_err("10 vars > 3 must fail");
        assert_eq!(var_err.code(), ErrorCode::ValidationError);
        assert!(var_err.message().contains("10") && var_err.message().contains('3'));
        limits.max_variables = 100;

        // 6. GQL-040 traverse.maxDepth > 16 and traverse.first > 1000
        let bad_traverse_depth = validate_query(
            &schema,
            r#"{ gap { traverse(input: { from: { id: "n1" }, maxDepth: 17 }) { truncated } } }"#,
        )
        .expect("parse+validate");
        let trav_depth_err = enforce_operation_limits(&bad_traverse_depth, 80, 0, &limits)
            .expect_err("maxDepth 17 > 16 must fail");
        assert_eq!(trav_depth_err.code(), ErrorCode::DepthExceeded);
        assert!(trav_depth_err.message().contains("17") && trav_depth_err.message().contains("16"));

        let bad_traverse_first = validate_query(
            &schema,
            r#"{ gap { traverse(input: { from: { id: "n1" }, maxDepth: 2, first: 1001 }) { truncated } } }"#,
        )
        .expect("parse+validate");
        let trav_first_err = enforce_operation_limits(&bad_traverse_first, 90, 0, &limits)
            .expect_err("first 1001 > 1000 must fail");
        assert_eq!(trav_first_err.code(), ErrorCode::ComplexityExceeded);
        assert!(
            trav_first_err.message().contains("1001") && trav_first_err.message().contains("1000")
        );
    }

    // spec: GQL-041 (ADR-023)
    #[test]
    fn cost_model_matches_formula_for_lists_joins_traverse_and_budgets() {
        let schema = cyclic_and_list_supergraph();
        let limits = LimitsConfig::default();

        // Query:
        // users(first: 10): w=2, m=min(10, 200, 100)=10
        //   id: w=1, m=1 -> 1
        //   recentOrders(first: 5): ACTIVE join! w=2 + perKey(3)*expected_keys(10)=32, m=min(5, 25, 100)=5
        //     amount: w=5, m=1 -> 5
        //   cost(recentOrders) = 32 + 5 * 5 = 57
        // cost(users) = 2 + 10 * (1 + 57) = 582
        let q = "{ users(first: 10) { id recentOrders(first: 5) { amount } } }";
        let validated = validate_query(&schema, q).expect("validate");
        let est = enforce_operation_cost(&validated, &limits, None, CostModelConfig::default())
            .expect("within system max_cost_units");
        assert_eq!(est.planned_cost, 582);

        // Exceeds @budget(costUnits: 500) -> GAP_BUDGET_EXCEEDED
        let budgeted_q = "query @budget(costUnits: 500) { users(first: 10) { id recentOrders(first: 5) { amount } } }";
        let validated_budget = validate_query(&schema, budgeted_q).expect("validate");
        let budget_err =
            enforce_operation_cost(&validated_budget, &limits, None, CostModelConfig::default())
                .expect_err("582 > 500 budget");
        assert_eq!(budget_err.code(), ErrorCode::BudgetExceeded);
        assert!(budget_err.message().contains("582") && budget_err.message().contains("500"));

        // Exceeds limits.max_cost_units -> GAP_COMPLEXITY_EXCEEDED
        let mut tight_limits = limits.clone();
        tight_limits.max_cost_units = 400;
        let sys_err =
            enforce_operation_cost(&validated, &tight_limits, None, CostModelConfig::default())
                .expect_err("582 > 400 max_cost_units");
        assert_eq!(sys_err.code(), ErrorCode::ComplexityExceeded);
        assert!(sys_err.message().contains("582") && sys_err.message().contains("400"));

        // Traverse visited_bound:
        // first=100, maxDepth=2, branching_cap=8 => min(8^1 + 8^2, 100) = 72
        // gap: w=1 + 1*(traverse: w=72 + 1*(truncated: 1)) = 74
        let builtin = builtin_schema().expect("builtin");
        let mut vars = serde_json::Map::new();
        vars.insert("md".to_owned(), json!(2));
        let trav_op = validate_document(
            &builtin,
            r#"query($md: Int!) { gap { traverse(input: { from: { id: "n1" }, maxDepth: $md, first: 100 }) { truncated } } }"#,
            None,
            &vars,
        )
        .expect("validate traverse");
        let trav_est = enforce_operation_cost(&trav_op, &limits, None, CostModelConfig::default())
            .expect("traverse cost");
        assert_eq!(trav_est.planned_cost, 74);
    }

    // spec: GQL-042
    #[test]
    fn cyclic_object_schema_allowed_while_unbounded_join_cycle_is_rejected() {
        // Cyclic object type `User.friends: [User!]!` already succeeded in `cyclic_and_list_supergraph`.
        let _ = cyclic_and_list_supergraph();

        // Unbounded @join cycle A -> B -> A without maxDepth -> GAP_CYCLE_UNBOUNDED
        let unbounded_err = with_gap_builtins(
            r#"
            type Query { a: Alpha }
            type Alpha @key(fields: "id") {
                id: ID!
                beta: Beta @join(to: "b.get", on: [{ from: "id", to: "alphaId" }], cardinality: ONE_TO_ONE, status: ACTIVE)
            }
            type Beta @key(fields: "id") {
                id: ID!
                alpha: Alpha @join(to: "a.get", on: [{ from: "id", to: "betaId" }], cardinality: ONE_TO_ONE, status: ACTIVE)
            }
            "#,
        )
        .expect_err("unbounded @join cycle must fail");
        assert_eq!(unbounded_err.code(), ErrorCode::CycleUnbounded);

        // Same @join cycle with a `maxDepth`-bounded entry -> accepted!
        with_gap_builtins(
            r#"
            type Query { a: Alpha }
            type Alpha @key(fields: "id") {
                id: ID!
                beta(maxDepth: Int! = 3): Beta @join(to: "b.get", on: [{ from: "id", to: "alphaId" }], cardinality: ONE_TO_ONE, status: ACTIVE)
            }
            type Beta @key(fields: "id") {
                id: ID!
                alpha: Alpha @join(to: "a.get", on: [{ from: "id", to: "betaId" }], cardinality: ONE_TO_ONE, status: ACTIVE)
            }
            "#,
        )
        .expect("maxDepth-bounded @join cycle must succeed");
    }

    // spec: GQL-041
    proptest! {
        #[test]
        fn prop_traverse_visited_bound_is_monotonic_and_capped_by_first(
            first in 1_u32..=1_000,
            d1 in 1_u32..=15,
            branching_cap in 1_u32..=32,
        ) {
            let b1 = traverse_visited_bound(first, d1, branching_cap);
            let b2 = traverse_visited_bound(first, d1 + 1, branching_cap);
            prop_assert!(b1 <= b2);
            prop_assert!(b2 <= u64::from(first));
            prop_assert!(b1 >= 1);
        }
    }
}
