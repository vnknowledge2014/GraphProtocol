//! Single-branch deterministic query planner over an immutable [`Snapshot`] (`RUN-005`, `RUN-010`,
//! `RUN-011`, `RUN-013`).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use apollo_compiler::ast::{DirectiveList, OperationType, Value};
use apollo_compiler::executable::{Field, Selection};
use apollo_compiler::validation::Valid;
use apollo_compiler::{ExecutableDocument, Name};
use gap_core::config::LimitsConfig;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{BranchId, STANDARD_INTROSPECTION_NAMES, is_reserved_root_field};
use gap_core::snapshot::{FieldCoordinate, OpKind, Snapshot};
use gap_ql::{
    CostEstimate, CostModelConfig, ValidatedOperation, check_ast_pre_validation_limits,
    check_executable_incremental_directives, check_request_body_size, check_variable_map_size,
    enforce_operation_cost, enforce_operation_limits, parse_executable_ast, validate_document,
    validate_executable_reserved_names,
};

use crate::ir::{
    ArgValue, Fetch, Plan, PlanCost, PlanNode, Pushdown, SelectionDirective, SelectionField,
};

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

fn value_contains_variable(val: &Value) -> bool {
    match val {
        Value::Variable(_) => true,
        Value::List(items) => items.iter().any(|item| value_contains_variable(item)),
        Value::Object(entries) => entries.iter().any(|(_, v)| value_contains_variable(v)),
        Value::Null
        | Value::Enum(_)
        | Value::String(_)
        | Value::Float(_)
        | Value::Int(_)
        | Value::Boolean(_) => false,
    }
}

fn ast_literal_to_json(val: &Value) -> GapResult<serde_json::Value> {
    match val {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(*b)),
        Value::String(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Enum(name) => Ok(serde_json::Value::String(name.as_str().to_owned())),
        Value::Int(int_val) => {
            let raw = int_val.as_str();
            if let Ok(i) = raw.parse::<i64>() {
                Ok(serde_json::Value::Number(i.into()))
            } else if let Ok(u) = raw.parse::<u64>() {
                Ok(serde_json::Value::Number(u.into()))
            } else {
                Err(GapError::new(
                    ErrorCode::TypeMismatch,
                    format!("integer literal `{raw}` overflows 64-bit representation"),
                ))
            }
        }
        Value::Float(float_val) => {
            let f = float_val.try_to_f64().map_err(|_| {
                GapError::new(
                    ErrorCode::TypeMismatch,
                    format!("float literal `{}` is non-finite", float_val.as_str()),
                )
            })?;
            let Some(num) = serde_json::Number::from_f64(f) else {
                return Err(GapError::new(
                    ErrorCode::TypeMismatch,
                    "float literal cannot be represented as finite JSON number",
                ));
            };
            Ok(serde_json::Value::Number(num))
        }
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(ast_literal_to_json(item)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        Value::Object(entries) => {
            let mut map = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                map.insert(k.as_str().to_owned(), ast_literal_to_json(v)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        Value::Variable(var) => Err(GapError::new(
            ErrorCode::ValidationError,
            format!("unexpected variable `${var}` in literal conversion"),
        )),
    }
}

fn ast_value_to_arg_value(val: &Value) -> GapResult<ArgValue> {
    if !value_contains_variable(val) {
        return ast_literal_to_json(val).map(ArgValue::Literal);
    }
    match val {
        Value::Variable(var) => Ok(ArgValue::Variable(var.as_str().to_owned())),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(ast_value_to_arg_value(item)?);
            }
            Ok(ArgValue::List(out))
        }
        Value::Object(entries) => {
            let mut out = BTreeMap::new();
            for (k, v) in entries {
                out.insert(k.as_str().to_owned(), ast_value_to_arg_value(v)?);
            }
            Ok(ArgValue::Object(out))
        }
        _ => ast_literal_to_json(val).map(ArgValue::Literal),
    }
}

fn extract_conditional_directives(
    directives: &DirectiveList,
) -> GapResult<Vec<SelectionDirective>> {
    let mut out = Vec::new();
    for dir in directives {
        let name = dir.name.as_str();
        if (name == "skip" || name == "include")
            && let Some(arg) = dir.arguments.iter().find(|a| a.name.as_str() == "if")
        {
            out.push(SelectionDirective {
                name: name.to_owned(),
                if_arg: ast_value_to_arg_value(&arg.value)?,
            });
        }
    }
    Ok(out)
}

fn collect_root_fields<'a>(
    doc: &'a ExecutableDocument,
    selections: &'a [Selection],
    inherited_directives: &[SelectionDirective],
    visiting_fragments: &mut HashSet<Name>,
    out: &mut Vec<(&'a Field, Vec<SelectionDirective>)>,
) -> GapResult<()> {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                let mut combined = inherited_directives.to_vec();
                combined.extend(extract_conditional_directives(&field.directives)?);
                out.push((field, combined));
            }
            Selection::InlineFragment(inline) => {
                let mut combined = inherited_directives.to_vec();
                combined.extend(extract_conditional_directives(&inline.directives)?);
                collect_root_fields(
                    doc,
                    &inline.selection_set.selections,
                    &combined,
                    visiting_fragments,
                    out,
                )?;
            }
            Selection::FragmentSpread(spread) => {
                if visiting_fragments.insert(spread.fragment_name.clone()) {
                    if let Some(frag) = doc.fragments.get(&spread.fragment_name) {
                        let mut combined = inherited_directives.to_vec();
                        combined.extend(extract_conditional_directives(&spread.directives)?);
                        combined.extend(extract_conditional_directives(&frag.directives)?);
                        collect_root_fields(
                            doc,
                            &frag.selection_set.selections,
                            &combined,
                            visiting_fragments,
                            out,
                        )?;
                    }
                    visiting_fragments.remove(&spread.fragment_name);
                }
            }
        }
    }
    Ok(())
}

fn flatten_subselection(
    doc: &ExecutableDocument,
    selections: &[Selection],
    inherited_directives: &[SelectionDirective],
    visiting_fragments: &mut HashSet<Name>,
    out: &mut Vec<SelectionField>,
) -> GapResult<()> {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                let mut directives = inherited_directives.to_vec();
                directives.extend(extract_conditional_directives(&field.directives)?);
                let mut children = Vec::new();
                flatten_subselection(
                    doc,
                    &field.selection_set.selections,
                    &[],
                    visiting_fragments,
                    &mut children,
                )?;
                out.push(SelectionField {
                    name: field.name.as_str().to_owned(),
                    alias: field.alias.as_ref().map(|a| a.as_str().to_owned()),
                    directives,
                    children,
                });
            }
            Selection::InlineFragment(inline) => {
                let mut combined = inherited_directives.to_vec();
                combined.extend(extract_conditional_directives(&inline.directives)?);
                flatten_subselection(
                    doc,
                    &inline.selection_set.selections,
                    &combined,
                    visiting_fragments,
                    out,
                )?;
            }
            Selection::FragmentSpread(spread) => {
                if visiting_fragments.insert(spread.fragment_name.clone()) {
                    if let Some(frag) = doc.fragments.get(&spread.fragment_name) {
                        let mut combined = inherited_directives.to_vec();
                        combined.extend(extract_conditional_directives(&spread.directives)?);
                        combined.extend(extract_conditional_directives(&frag.directives)?);
                        flatten_subselection(
                            doc,
                            &frag.selection_set.selections,
                            &combined,
                            visiting_fragments,
                            out,
                        )?;
                    }
                    visiting_fragments.remove(&spread.fragment_name);
                }
            }
        }
    }
    Ok(())
}

fn is_builtin_or_introspection_root_field(field_name: &str) -> bool {
    is_reserved_root_field(field_name) || STANDARD_INTROSPECTION_NAMES.contains(&field_name)
}

pub(crate) fn plan_with_verified_cost(
    snapshot: &Snapshot,
    document: &ValidatedOperation,
    cost_estimate: CostEstimate,
) -> GapResult<Plan> {
    let Some(op) = document.operation() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "validated operation handle is missing target operation",
        ));
    };

    match op.operation_type {
        OperationType::Query => {}
        OperationType::Mutation => {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "mutation planning is not implemented in gap-plan v0 (Mutate IR is declared only)",
            ));
        }
        OperationType::Subscription => {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "subscription planning is not implemented in gap-plan v0",
            ));
        }
    }

    let mut root_fields = Vec::new();
    let mut visiting = HashSet::new();
    collect_root_fields(
        &document.document,
        &op.selection_set.selections,
        &[],
        &mut visiting,
        &mut root_fields,
    )?;

    if root_fields.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "query operation selected zero executable root fields",
        ));
    }

    let mut fetches = Vec::with_capacity(root_fields.len());
    let mut touched_branches: BTreeSet<BranchId> = BTreeSet::new();

    for (root_field, root_directives) in root_fields {
        let field_name = root_field.name.as_str();
        if is_builtin_or_introspection_root_field(field_name) {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                format!(
                    "built-in discovery/introspection root field `{field_name}` is not planned as a branch Fetch"
                ),
            ));
        }

        let coord = FieldCoordinate::new("Query", field_name);
        let Some(branch_uid) = snapshot.ownership.owner(&coord) else {
            return Err(GapError::new(
                ErrorCode::UnknownField,
                format!(
                    "root field `Query.{field_name}` has no owning branch in snapshot generation {} (RUN-011)",
                    snapshot.generation
                ),
            ));
        };

        let Some(branch_desc) = snapshot.branches.get(&branch_uid) else {
            return Err(GapError::new(
                ErrorCode::UnknownField,
                format!(
                    "owning branch `{branch_uid}` for `Query.{field_name}` is missing in snapshot generation {}",
                    snapshot.generation
                ),
            ));
        };

        let has_query_op = branch_desc
            .operations
            .iter()
            .any(|op_desc| op_desc.kind == OpKind::Query && op_desc.id.name() == field_name);
        if !has_query_op {
            return Err(GapError::new(
                ErrorCode::UnknownField,
                format!(
                    "branch `{}` has no QUERY operation matching `Query.{field_name}`",
                    branch_desc.id
                ),
            ));
        }

        let mut arguments = BTreeMap::new();
        for arg in &root_field.arguments {
            arguments.insert(
                arg.name.as_str().to_owned(),
                ast_value_to_arg_value(&arg.value)?,
            );
        }

        let mut selection = Vec::new();
        let mut sub_visiting = HashSet::new();
        flatten_subselection(
            &document.document,
            &root_field.selection_set.selections,
            &[],
            &mut sub_visiting,
            &mut selection,
        )?;

        let response_key = root_field
            .alias
            .as_ref()
            .map_or_else(|| field_name.to_owned(), |alias| alias.as_str().to_owned());

        touched_branches.insert(branch_desc.id.clone());
        fetches.push(Fetch {
            branch: branch_desc.id.clone(),
            branch_uid,
            operation: field_name.to_owned(),
            arguments,
            selection,
            response_key,
            directives: root_directives,
            pushdown: Pushdown::default(),
        });
    }

    let root = if fetches.len() == 1 {
        let Some(single) = fetches.into_iter().next() else {
            return Err(GapError::new(
                ErrorCode::Internal,
                "invariant violated: single fetch vector was empty",
            ));
        };
        PlanNode::Fetch(single)
    } else {
        fetches.sort_by(|a, b| {
            (&a.branch, &a.operation, &a.response_key).cmp(&(
                &b.branch,
                &b.operation,
                &b.response_key,
            ))
        });
        PlanNode::Parallel {
            nodes: fetches.into_iter().map(PlanNode::Fetch).collect(),
        }
    };

    Ok(Plan {
        root,
        operation_name: document.operation_name.clone(),
        cost: PlanCost::from(cost_estimate),
        branches: touched_branches.into_iter().collect(),
        policy_version: snapshot.policy_version,
    })
}

/// Ensures `document` targets a `Query` operation before running cost analysis so mutations and
/// subscriptions consistently fail with [`ErrorCode::UnsupportedFeature`].
pub(crate) fn check_query_operation_kind(document: &ValidatedOperation) -> GapResult<()> {
    let Some(op) = document.operation() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "validated operation handle is missing target operation",
        ));
    };
    match op.operation_type {
        OperationType::Query => Ok(()),
        OperationType::Mutation => Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "mutation planning is not implemented in gap-plan v0 (Mutate IR is declared only)",
        )),
        OperationType::Subscription => Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "subscription planning is not implemented in gap-plan v0",
        )),
    }
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
    snapshot: &Snapshot,
    document: &ValidatedOperation,
    config: &PlanConfig,
) -> GapResult<Plan> {
    check_query_operation_kind(document)?;
    let cost_estimate = enforce_operation_cost(
        document,
        &config.limits,
        config.request_budget_cost_units,
        config.cost_model,
    )?;
    plan_with_verified_cost(snapshot, document, cost_estimate)
}

fn first_error(mut errs: Vec<GapError>) -> GapError {
    if errs.is_empty() {
        GapError::new(ErrorCode::ValidationError, "GraphQL validation failed")
    } else {
        errs.swap_remove(0)
    }
}

/// Validates `query` against `snapshot.schema`, enforces structural limits, and compiles a [`Plan`]
/// via [`plan`].
///
/// # Errors
///
/// Returns [`GapError`] on parse, validation, structural-limit, cost, or ownership failure.
pub fn plan_query(
    snapshot: &Snapshot,
    query: &str,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
    config: &PlanConfig,
) -> GapResult<Plan> {
    check_request_body_size(query.as_bytes(), &config.limits)?;
    check_variable_map_size(variables.len(), &config.limits)?;

    let ast_doc = parse_executable_ast(query, "request.graphql").map_err(first_error)?;
    check_executable_incremental_directives(&ast_doc, false)?;
    check_ast_pre_validation_limits(&ast_doc, variables.len(), &config.limits)?;
    validate_executable_reserved_names(&ast_doc).map_err(first_error)?;

    let valid_schema = Valid::assume_valid_ref(&*snapshot.schema);
    let validated =
        validate_document(valid_schema, query, operation_name, variables).map_err(first_error)?;
    enforce_operation_limits(&validated, query.len(), variables.len(), &config.limits)?;

    plan(snapshot, &validated, config)
}
