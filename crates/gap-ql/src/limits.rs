//! Pre-resolution request body, variable, root-field, alias, selection-depth, and `traverse`
//! limits (`spec/02-gap-ql.md` §§8, 10; `GQL-040`, `GQL-042`).

use std::collections::{HashMap, HashSet};

use apollo_compiler::Name;
use apollo_compiler::ast::{self, Definition, Selection, Value};
use apollo_compiler::response::{JsonMap, JsonValue};
use gap_core::config::LimitsConfig;
use gap_core::error::{ErrorCode, GapError, GapResult};

use crate::validate::ValidatedOperation;

/// HTTP status code mandated by `spec/02-gap-ql.md` §8 when `max_request_body_bytes` is exceeded.
pub const HTTP_PAYLOAD_TOO_LARGE_STATUS: u16 = 413;

/// Hard upper bound on `TraverseInput.maxDepth` (`GQL-040`, not configurable above 16).
pub const HARD_MAX_TRAVERSE_DEPTH: u32 = 16;

/// Hard upper bound on `TraverseInput.first` (`GQL-040`).
pub const HARD_MAX_TRAVERSE_FIRST: u32 = 1_000;

/// Summary of static structural metrics computed for a GraphQL request document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentMetrics {
    /// Request payload length in UTF-8 bytes.
    pub body_bytes: usize,
    /// Effective variable count (`max(declared_variables, provided_variables)`).
    pub variables_count: u64,
    /// Root field selections in the target operation after fragment expansion.
    pub root_fields: u64,
    /// Field alias count (`max(ast_aliases, expanded_operation_aliases)`).
    pub aliases: u64,
    /// Maximum nested field selection depth in the target operation.
    pub selection_depth: u64,
}

/// Rejects oversized raw request bodies before any GraphQL parsing occurs (`spec/02-gap-ql.md` §8).
///
/// Transport layers map this pre-parse failure to HTTP `413 Payload Too Large`
/// ([`HTTP_PAYLOAD_TOO_LARGE_STATUS`]).
pub fn check_request_body_size(body: &[u8], limits: &LimitsConfig) -> GapResult<()> {
    let actual = body.len() as u64;
    let max = limits.max_request_body_bytes;
    if actual > max {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("request body size {actual} bytes exceeds maximum {max} bytes (HTTP 413)"),
        ));
    }
    Ok(())
}

/// Rejects incoming JSON variable maps whose entry count exceeds `limits.max_variables`
/// before GraphQL variable coercion runs (`spec/02-gap-ql.md` §8).
pub fn check_variable_map_size(variables_len: usize, limits: &LimitsConfig) -> GapResult<()> {
    let actual = variables_len as u64;
    let max = u64::from(limits.max_variables);
    if actual > max {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("variables count {actual} exceeds maximum {max}"),
        ));
    }
    Ok(())
}

fn count_ast_selections_aliases(selections: &[Selection]) -> u64 {
    let mut total = 0_u64;
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                if field.alias.is_some() {
                    total = total.saturating_add(1);
                }
                total = total.saturating_add(count_ast_selections_aliases(&field.selection_set));
            }
            Selection::InlineFragment(inline) => {
                total = total.saturating_add(count_ast_selections_aliases(&inline.selection_set));
            }
            Selection::FragmentSpread(_) => {}
        }
    }
    total
}

/// Enforces fast AST-level limits (declared variables and literal AST aliases) prior to schema
/// validation so adversarial inputs fail before building executable IR.
pub fn check_ast_pre_validation_limits(
    ast_doc: &ast::Document,
    provided_variables_len: usize,
    limits: &LimitsConfig,
) -> GapResult<()> {
    check_variable_map_size(provided_variables_len, limits)?;

    let mut declared_vars = 0_u64;
    let mut ast_aliases = 0_u64;
    for def in &ast_doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                declared_vars = declared_vars.saturating_add(op.variables.len() as u64);
                ast_aliases =
                    ast_aliases.saturating_add(count_ast_selections_aliases(&op.selection_set));
            }
            Definition::FragmentDefinition(frag) => {
                ast_aliases =
                    ast_aliases.saturating_add(count_ast_selections_aliases(&frag.selection_set));
            }
            _ => {}
        }
    }

    let max_vars = u64::from(limits.max_variables);
    if declared_vars > max_vars {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("declared variables count {declared_vars} exceeds maximum {max_vars}"),
        ));
    }

    let max_aliases = u64::from(limits.max_aliases);
    if ast_aliases > max_aliases {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("alias count {ast_aliases} exceeds maximum {max_aliases}"),
        ));
    }

    Ok(())
}

fn count_root_fields_exec(
    doc: &apollo_compiler::ExecutableDocument,
    selections: &[apollo_compiler::executable::Selection],
    visiting: &mut HashSet<Name>,
    memo: &mut HashMap<Name, u64>,
) -> u64 {
    let mut count = 0_u64;
    for sel in selections {
        match sel {
            apollo_compiler::executable::Selection::Field(_) => {
                count = count.saturating_add(1);
            }
            apollo_compiler::executable::Selection::InlineFragment(inline) => {
                count = count.saturating_add(count_root_fields_exec(
                    doc,
                    &inline.selection_set.selections,
                    visiting,
                    memo,
                ));
            }
            apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                if let Some(&cached) = memo.get(&spread.fragment_name) {
                    count = count.saturating_add(cached);
                } else if visiting.insert(spread.fragment_name.clone()) {
                    let frag_count = doc.fragments.get(&spread.fragment_name).map_or(0, |frag| {
                        count_root_fields_exec(doc, &frag.selection_set.selections, visiting, memo)
                    });
                    visiting.remove(&spread.fragment_name);
                    memo.insert(spread.fragment_name.clone(), frag_count);
                    count = count.saturating_add(frag_count);
                }
            }
        }
    }
    count
}

fn count_expanded_aliases_exec(
    doc: &apollo_compiler::ExecutableDocument,
    selections: &[apollo_compiler::executable::Selection],
    visiting: &mut HashSet<Name>,
    memo: &mut HashMap<Name, u64>,
) -> u64 {
    let mut total = 0_u64;
    for sel in selections {
        match sel {
            apollo_compiler::executable::Selection::Field(field) => {
                if field.alias.is_some() {
                    total = total.saturating_add(1);
                }
                total = total.saturating_add(count_expanded_aliases_exec(
                    doc,
                    &field.selection_set.selections,
                    visiting,
                    memo,
                ));
            }
            apollo_compiler::executable::Selection::InlineFragment(inline) => {
                total = total.saturating_add(count_expanded_aliases_exec(
                    doc,
                    &inline.selection_set.selections,
                    visiting,
                    memo,
                ));
            }
            apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                if let Some(&cached) = memo.get(&spread.fragment_name) {
                    total = total.saturating_add(cached);
                } else if visiting.insert(spread.fragment_name.clone()) {
                    let frag_aliases = doc.fragments.get(&spread.fragment_name).map_or(0, |frag| {
                        count_expanded_aliases_exec(
                            doc,
                            &frag.selection_set.selections,
                            visiting,
                            memo,
                        )
                    });
                    visiting.remove(&spread.fragment_name);
                    memo.insert(spread.fragment_name.clone(), frag_aliases);
                    total = total.saturating_add(frag_aliases);
                }
            }
        }
    }
    total
}

fn max_selection_depth_exec(
    doc: &apollo_compiler::ExecutableDocument,
    selections: &[apollo_compiler::executable::Selection],
    visiting: &mut HashSet<Name>,
    memo: &mut HashMap<Name, u64>,
) -> u64 {
    let mut max_depth = 0_u64;
    for sel in selections {
        let candidate = match sel {
            apollo_compiler::executable::Selection::Field(field) => {
                let child_depth =
                    max_selection_depth_exec(doc, &field.selection_set.selections, visiting, memo);
                1_u64.saturating_add(child_depth)
            }
            apollo_compiler::executable::Selection::InlineFragment(inline) => {
                max_selection_depth_exec(doc, &inline.selection_set.selections, visiting, memo)
            }
            apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                if let Some(&cached) = memo.get(&spread.fragment_name) {
                    cached
                } else if visiting.insert(spread.fragment_name.clone()) {
                    let frag_depth = doc.fragments.get(&spread.fragment_name).map_or(0, |frag| {
                        max_selection_depth_exec(
                            doc,
                            &frag.selection_set.selections,
                            visiting,
                            memo,
                        )
                    });
                    visiting.remove(&spread.fragment_name);
                    memo.insert(spread.fragment_name.clone(), frag_depth);
                    frag_depth
                } else {
                    0
                }
            }
        };
        if candidate > max_depth {
            max_depth = candidate;
        }
    }
    max_depth
}

pub(crate) fn extract_i64_from_ast_or_vars(
    val: &Value,
    coerced_variables: &JsonMap,
) -> Option<i64> {
    match val {
        Value::Int(int_val) => int_val.try_to_i32().ok().map(i64::from),
        Value::Variable(var_name) => coerced_variables
            .get(var_name.as_str())
            .and_then(JsonValue::as_i64),
        _ => None,
    }
}

/// Extracted `TraverseInput` bounds (`maxDepth` and `first`) for a `GapQuery.traverse` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraverseBounds {
    /// Validated `maxDepth` in `1..=min(16, limits.traverse_max_depth)`.
    pub max_depth: u32,
    /// Validated `first` in `1..=1_000` (default `100`).
    pub first: u32,
}

/// Extracts and validates `TraverseInput.maxDepth` (`1..=min(16, limits.traverse_max_depth)`) and
/// `TraverseInput.first` (`1..=1_000`) on a `GapQuery.traverse` field call (`GQL-040`).
pub fn extract_and_check_traverse_bounds(
    field: &apollo_compiler::executable::Field,
    coerced_variables: &JsonMap,
    limits: &LimitsConfig,
) -> GapResult<TraverseBounds> {
    let max_depth_cap = u32::from(limits.traverse_max_depth).min(HARD_MAX_TRAVERSE_DEPTH);
    let mut raw_max_depth: Option<i64> = None;
    let mut raw_first: Option<i64> = None;

    if let Some(input_arg) = field.arguments.iter().find(|a| a.name.as_str() == "input") {
        match &*input_arg.value {
            Value::Object(entries) => {
                for (k, v) in entries {
                    if k.as_str() == "maxDepth" {
                        raw_max_depth = extract_i64_from_ast_or_vars(v, coerced_variables);
                    } else if k.as_str() == "first" {
                        raw_first = extract_i64_from_ast_or_vars(v, coerced_variables);
                    }
                }
            }
            Value::Variable(var_name) => {
                if let Some(obj) = coerced_variables
                    .get(var_name.as_str())
                    .and_then(JsonValue::as_object)
                {
                    if let Some(md) = obj.get("maxDepth").and_then(JsonValue::as_i64) {
                        raw_max_depth = Some(md);
                    }
                    if let Some(f) = obj.get("first").and_then(JsonValue::as_i64) {
                        raw_first = Some(f);
                    }
                }
            }
            _ => {}
        }
    }

    let max_depth_val = raw_max_depth.unwrap_or(1);
    if max_depth_val < 1 {
        return Err(GapError::new(
            ErrorCode::DepthExceeded,
            format!(
                "traverse.maxDepth {max_depth_val} is below minimum 1 (maximum {max_depth_cap})"
            ),
        ));
    }
    let max_depth_u64 = u64::try_from(max_depth_val).unwrap_or(u64::MAX);
    if max_depth_u64 > u64::from(max_depth_cap) {
        return Err(GapError::new(
            ErrorCode::DepthExceeded,
            format!("traverse.maxDepth {max_depth_u64} exceeds maximum {max_depth_cap}"),
        ));
    }

    let first_val = raw_first.unwrap_or(100);
    if first_val < 1 {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!(
                "traverse.first {first_val} is below minimum 1 (maximum {HARD_MAX_TRAVERSE_FIRST})"
            ),
        ));
    }
    let first_u64 = u64::try_from(first_val).unwrap_or(u64::MAX);
    if first_u64 > u64::from(HARD_MAX_TRAVERSE_FIRST) {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("traverse.first {first_u64} exceeds maximum {HARD_MAX_TRAVERSE_FIRST}"),
        ));
    }

    Ok(TraverseBounds {
        max_depth: u32::try_from(max_depth_u64).unwrap_or(max_depth_cap),
        first: u32::try_from(first_u64).unwrap_or(HARD_MAX_TRAVERSE_FIRST),
    })
}

fn check_traverse_limits_in_selections(
    doc: &apollo_compiler::ExecutableDocument,
    parent_type: &str,
    selections: &[apollo_compiler::executable::Selection],
    coerced_variables: &JsonMap,
    limits: &LimitsConfig,
    visited_fragments: &mut HashSet<(Name, String)>,
) -> GapResult<()> {
    for sel in selections {
        match sel {
            apollo_compiler::executable::Selection::Field(field) => {
                if parent_type == "GapQuery" && field.name.as_str() == "traverse" {
                    let _ = extract_and_check_traverse_bounds(field, coerced_variables, limits)?;
                }
                let next_parent = field.definition.ty.inner_named_type().as_str();
                check_traverse_limits_in_selections(
                    doc,
                    next_parent,
                    &field.selection_set.selections,
                    coerced_variables,
                    limits,
                    visited_fragments,
                )?;
            }
            apollo_compiler::executable::Selection::InlineFragment(inline) => {
                let next_parent = inline
                    .type_condition
                    .as_ref()
                    .map_or(parent_type, Name::as_str);
                check_traverse_limits_in_selections(
                    doc,
                    next_parent,
                    &inline.selection_set.selections,
                    coerced_variables,
                    limits,
                    visited_fragments,
                )?;
            }
            apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                if let Some(frag) = doc.fragments.get(&spread.fragment_name) {
                    let next_parent = frag.type_condition().as_str();
                    let key = (spread.fragment_name.clone(), next_parent.to_owned());
                    if visited_fragments.insert(key) {
                        check_traverse_limits_in_selections(
                            doc,
                            next_parent,
                            &frag.selection_set.selections,
                            coerced_variables,
                            limits,
                            visited_fragments,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Computes and enforces all structural document limits (`max_variables`, `max_root_fields`,
/// `max_aliases`, `max_selection_depth`, and `GQL-040` `traverse` bounds) on a
/// [`ValidatedOperation`] before any resolution work (`spec/02-gap-ql.md` §§8, 10).
pub fn enforce_operation_limits(
    validated: &ValidatedOperation,
    body_bytes: usize,
    provided_variables_len: usize,
    limits: &LimitsConfig,
) -> GapResult<DocumentMetrics> {
    check_request_body_size_len(body_bytes, limits)?;
    check_variable_map_size(provided_variables_len, limits)?;

    let Some(op) = validated.operation() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "validated operation handle is missing target operation",
        ));
    };

    let declared_vars = op.variables.len() as u64;
    let variables_count = declared_vars.max(provided_variables_len as u64);
    let max_vars = u64::from(limits.max_variables);
    if variables_count > max_vars {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("variables count {variables_count} exceeds maximum {max_vars}"),
        ));
    }

    let root_fields = count_root_fields_exec(
        &validated.document,
        &op.selection_set.selections,
        &mut HashSet::new(),
        &mut HashMap::new(),
    );
    let max_roots = u64::from(limits.max_root_fields);
    if root_fields > max_roots {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("root field count {root_fields} exceeds maximum {max_roots}"),
        ));
    }

    let expanded_aliases = count_expanded_aliases_exec(
        &validated.document,
        &op.selection_set.selections,
        &mut HashSet::new(),
        &mut HashMap::new(),
    );
    let max_aliases = u64::from(limits.max_aliases);
    if expanded_aliases > max_aliases {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("alias count {expanded_aliases} exceeds maximum {max_aliases}"),
        ));
    }

    let selection_depth = max_selection_depth_exec(
        &validated.document,
        &op.selection_set.selections,
        &mut HashSet::new(),
        &mut HashMap::new(),
    );
    let max_depth = u64::from(limits.max_selection_depth);
    if selection_depth > max_depth {
        return Err(GapError::new(
            ErrorCode::DepthExceeded,
            format!("selection depth {selection_depth} exceeds maximum {max_depth}"),
        ));
    }

    let mut visited_frags = HashSet::new();
    check_traverse_limits_in_selections(
        &validated.document,
        op.object_type().as_str(),
        &op.selection_set.selections,
        &validated.coerced_variables,
        limits,
        &mut visited_frags,
    )?;

    Ok(DocumentMetrics {
        body_bytes,
        variables_count,
        root_fields,
        aliases: expanded_aliases,
        selection_depth,
    })
}

fn check_request_body_size_len(body_bytes: usize, limits: &LimitsConfig) -> GapResult<()> {
    let actual = body_bytes as u64;
    let max = limits.max_request_body_bytes;
    if actual > max {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("request body size {actual} bytes exceeds maximum {max} bytes (HTTP 413)"),
        ));
    }
    Ok(())
}
