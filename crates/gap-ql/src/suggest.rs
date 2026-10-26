//! Deterministic self-repair hints (`didYouMean`, `template`, `missingArguments`) computed strictly
//! over caller-visible schema elements (`spec/02-gap-ql.md` §9; `GQL-062`, `GQL-070`, `GQL-072`).

use std::collections::HashSet;

use apollo_compiler::Schema;
use apollo_compiler::ast::{Definition, Selection, Type};
use apollo_compiler::schema::ExtendedType;
use gap_core::error::{ErrorCode, GapError, SourceLocation, Suggestion};

use crate::visibility::Visibility;

/// Maximum Levenshtein edit distance eligible for `didYouMean` (`GQL-070`).
pub const MAX_SUGGESTION_EDIT_DISTANCE: usize = 2;

/// Maximum fallback `__search` hits appended after edit-distance matches (`GQL-070`).
pub const MAX_SEARCH_FALLBACK_HITS: usize = 3;

/// Computes the exact Levenshtein edit distance between `a` and `b`.
#[must_use]
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.is_empty() {
        return b_chars.len();
    }
    if b_chars.is_empty() {
        return a_chars.len();
    }

    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr: Vec<usize> = vec![0; b_chars.len() + 1];

    for (i, &ca) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let deletion = prev[j + 1] + 1;
            let insertion = curr[j] + 1;
            let substitution = prev[j] + cost;
            curr[j + 1] = deletion.min(insertion).min(substitution);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

/// Ranks `candidates` within Levenshtein edit distance `<= 2` of `target`, ordered by
/// `(distance, candidate_name)`.
#[must_use]
pub fn rank_did_you_mean<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut scored: Vec<(usize, &str)> = candidates
        .into_iter()
        .filter(|cand| !cand.starts_with("__") || *cand == "__search" || *cand == "__metaGraph")
        .filter_map(|cand| {
            let dist = levenshtein_distance(target, cand);
            (dist > 0 && dist <= MAX_SUGGESTION_EDIT_DISTANCE).then_some((dist, cand))
        })
        .collect();
    scored.sort_by(|(d1, n1), (d2, n2)| d1.cmp(d2).then_with(|| n1.cmp(n2)));
    scored.dedup_by(|(_, n1), (_, n2)| n1 == n2);
    scored.into_iter().map(|(_, n)| n.to_owned()).collect()
}

/// Synthesizes a minimal valid selection set snippet (e.g. `"{ id }"`) for `type_name` using only
/// fields present in the caller-projected `visible_schema` (`GQL-070`).
#[must_use]
pub fn minimal_selection_template(visible_schema: &Schema, type_name: &str) -> Option<String> {
    minimal_selection_inner(visible_schema, type_name, 0)
}

fn is_leaf_type(visible_schema: &Schema, ty: &Type) -> bool {
    let inner = ty.inner_named_type().as_str();
    matches!(
        visible_schema.types.get(inner),
        Some(ExtendedType::Scalar(_) | ExtendedType::Enum(_))
    )
}

fn has_required_arguments(field_def: &apollo_compiler::schema::FieldDefinition) -> bool {
    field_def
        .arguments
        .iter()
        .any(|arg| arg.ty.is_non_null() && arg.default_value.is_none())
}

fn placeholder_arg_literal(visible_schema: &Schema, ty: &Type, depth: usize) -> Option<String> {
    if depth >= 3 {
        return None;
    }
    match ty {
        Type::List(_) | Type::NonNullList(_) => Some("[]".to_owned()),
        Type::Named(name) | Type::NonNullNamed(name) => match name.as_str() {
            "Int" => Some("1".to_owned()),
            "Float" => Some("0.0".to_owned()),
            "Boolean" => Some("false".to_owned()),
            "ID" | "String" | "DateTime" | "Cursor" => Some("\"...\"".to_owned()),
            other => match visible_schema.types.get(other)? {
                ExtendedType::Scalar(_) => Some("\"...\"".to_owned()),
                ExtendedType::Enum(e) => e.values.keys().next().map(|v| v.as_str().to_owned()),
                ExtendedType::InputObject(inp) => {
                    if inp.directives.has("oneOf") {
                        let (fname, fdef) = inp.fields.iter().next()?;
                        let lit = placeholder_arg_literal(visible_schema, &fdef.ty, depth + 1)?;
                        Some(format!("{{ {fname}: {lit} }}"))
                    } else {
                        let mut parts = Vec::new();
                        for (fname, fdef) in &inp.fields {
                            if fdef.ty.is_non_null() && fdef.default_value.is_none() {
                                let lit =
                                    placeholder_arg_literal(visible_schema, &fdef.ty, depth + 1)?;
                                parts.push(format!("{fname}: {lit}"));
                            }
                        }
                        Some(format!("{{ {} }}", parts.join(", ")))
                    }
                }
                _ => None,
            },
        },
    }
}

fn format_required_args_call(
    visible_schema: &Schema,
    field_def: &apollo_compiler::schema::FieldDefinition,
) -> Option<String> {
    let mut parts = Vec::new();
    for arg in &field_def.arguments {
        if arg.ty.is_non_null() && arg.default_value.is_none() {
            let lit = placeholder_arg_literal(visible_schema, &arg.ty, 0)?;
            parts.push(format!("{}: {lit}", arg.name));
        }
    }
    if parts.is_empty() {
        Some(String::new())
    } else {
        Some(format!("({})", parts.join(", ")))
    }
}

fn minimal_selection_inner(
    visible_schema: &Schema,
    type_name: &str,
    depth: usize,
) -> Option<String> {
    if depth >= 3 {
        return None;
    }
    let fields = match visible_schema.types.get(type_name)? {
        ExtendedType::Object(obj) => &obj.fields,
        ExtendedType::Interface(iface) => &iface.fields,
        _ => return None,
    };

    // Prefer `id` if it is a visible zero-required-arg leaf field.
    if let Some(id_def) = fields.get("id")
        && !id_def.name.as_str().starts_with("__")
        && is_leaf_type(visible_schema, &id_def.ty)
        && !has_required_arguments(id_def)
    {
        return Some("{ id }".to_owned());
    }

    // Next, any visible zero-required-arg leaf field.
    for (fname, fdef) in fields {
        if !fname.as_str().starts_with("__")
            && is_leaf_type(visible_schema, &fdef.ty)
            && !has_required_arguments(fdef)
        {
            return Some(format!("{{ {fname} }}"));
        }
    }

    // Next, recurse into a visible zero-required-arg composite field.
    for (fname, fdef) in fields {
        if !fname.as_str().starts_with("__")
            && !has_required_arguments(fdef)
            && let Some(nested) = minimal_selection_inner(
                visible_schema,
                fdef.ty.inner_named_type().as_str(),
                depth + 1,
            )
        {
            return Some(format!("{{ {fname} {nested} }}"));
        }
    }

    // Fallback when all visible fields require arguments: synthesize placeholder required args.
    for (fname, fdef) in fields {
        if fname.as_str().starts_with("__") {
            continue;
        }
        let Some(args_call) = format_required_args_call(visible_schema, fdef) else {
            continue;
        };
        if is_leaf_type(visible_schema, &fdef.ty) {
            return Some(format!("{{ {fname}{args_call} }}"));
        }
        if let Some(nested) = minimal_selection_inner(
            visible_schema,
            fdef.ty.inner_named_type().as_str(),
            depth + 1,
        ) {
            return Some(format!("{{ {fname}{args_call} {nested} }}"));
        }
    }

    None
}

fn extract_backticked_segments(message: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = message;
    while let Some(start) = rest.find('`') {
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('`') else {
            break;
        };
        out.push(&after_start[..end]);
        rest = &after_start[end + 1..];
    }
    out
}

fn find_ast_provided_args_at_location(
    ast_doc: &apollo_compiler::ast::Document,
    target_loc: SourceLocation,
    expected_field: &str,
) -> Option<HashSet<String>> {
    for def in &ast_doc.definitions {
        let selections = match def {
            Definition::OperationDefinition(op) => &op.selection_set,
            Definition::FragmentDefinition(frag) => &frag.selection_set,
            _ => continue,
        };
        if let Some(found) =
            find_in_selections(selections, &ast_doc.sources, target_loc, expected_field)
        {
            return Some(found);
        }
    }
    None
}

fn find_in_selections(
    selections: &[Selection],
    sources: &apollo_compiler::parser::SourceMap,
    target_loc: SourceLocation,
    expected_field: &str,
) -> Option<HashSet<String>> {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                if field.name.as_str() == expected_field
                    && let Some(span) = field.location()
                    && let Some(lc) = span.line_column(sources)
                    && u32::try_from(lc.line).ok() == Some(target_loc.line)
                    && u32::try_from(lc.column).ok() == Some(target_loc.column)
                {
                    let provided = field
                        .arguments
                        .iter()
                        .map(|a| a.name.as_str().to_owned())
                        .collect();
                    return Some(provided);
                }
                if let Some(sub) =
                    find_in_selections(&field.selection_set, sources, target_loc, expected_field)
                {
                    return Some(sub);
                }
            }
            Selection::InlineFragment(inline) => {
                if let Some(sub) =
                    find_in_selections(&inline.selection_set, sources, target_loc, expected_field)
                {
                    return Some(sub);
                }
            }
            Selection::FragmentSpread(_) => {}
        }
    }
    None
}

/// Enriches a validation [`GapError`] with `extensions.suggestion` (`didYouMean`, `template`,
/// `missingArguments`) derived strictly from `visible_schema` and `visibility` (`GQL-062`, `GQL-070`).
#[must_use]
pub fn attach_validation_suggestion(
    err: GapError,
    visible_schema: &Schema,
    ast_doc: Option<&apollo_compiler::ast::Document>,
    visibility: &dyn Visibility,
) -> GapError {
    if err.suggestion().is_some() {
        return err;
    }

    let msg = err.message();
    let ticks = extract_backticked_segments(msg);
    let mut suggestion = Suggestion::default();
    let mut populated = false;

    match err.code() {
        ErrorCode::UnknownField => {
            // Pattern A: "type `{Parent}` does not have a field `{field}`"
            if msg.starts_with("type `")
                && msg.contains("` does not have a field `")
                && ticks.len() >= 2
            {
                let parent_type = ticks[0];
                let unknown_field = ticks[1];
                let mut candidates = match visible_schema.types.get(parent_type) {
                    Some(ExtendedType::Object(obj)) => {
                        rank_did_you_mean(unknown_field, obj.fields.keys().map(|k| k.as_str()))
                    }
                    Some(ExtendedType::Interface(iface)) => {
                        rank_did_you_mean(unknown_field, iface.fields.keys().map(|k| k.as_str()))
                    }
                    _ => Vec::new(),
                };
                for fallback in visibility
                    .search_visible_names(parent_type, unknown_field)
                    .into_iter()
                    .take(MAX_SEARCH_FALLBACK_HITS)
                {
                    if visibility.can_see_field(parent_type, &fallback)
                        && !candidates.contains(&fallback)
                    {
                        candidates.push(fallback);
                    }
                }
                if !candidates.is_empty() {
                    suggestion.did_you_mean = Some(candidates);
                    populated = true;
                }
                if let Some(tpl) = minimal_selection_template(visible_schema, parent_type) {
                    suggestion.template = Some(tpl);
                    populated = true;
                }
            } else if msg.starts_with("field `")
                && msg.contains("` does not exist on `")
                && ticks.len() >= 2
            {
                // Pattern B: "field `{value}` does not exist on `{InputType}`"
                let unknown_input_field = ticks[0];
                let input_type = ticks[1];
                if let Some(ExtendedType::InputObject(input_obj)) =
                    visible_schema.types.get(input_type)
                {
                    let candidates = rank_did_you_mean(
                        unknown_input_field,
                        input_obj.fields.keys().map(|k| k.as_str()),
                    );
                    if !candidates.is_empty() {
                        suggestion.did_you_mean = Some(candidates);
                        populated = true;
                    }
                }
            }
        }
        ErrorCode::UnknownArgument => {
            // Pattern: "the argument `{arg}` is not supported by `{Coordinate}`"
            if ticks.len() >= 2 {
                let unknown_arg = ticks[0];
                let coord = ticks[1];
                if let Some(dir_name) = coord.strip_prefix('@') {
                    if let Some(dir_def) = visible_schema.directive_definitions.get(dir_name) {
                        let candidates = rank_did_you_mean(
                            unknown_arg,
                            dir_def.arguments.iter().map(|a| a.name.as_str()),
                        );
                        if !candidates.is_empty() {
                            suggestion.did_you_mean = Some(candidates);
                            populated = true;
                        }
                    }
                } else if let Some((parent_type, field_name)) = coord.split_once('.')
                    && let Ok(field_def) = visible_schema.type_field(parent_type, field_name)
                {
                    let candidates = rank_did_you_mean(
                        unknown_arg,
                        field_def.arguments.iter().map(|a| a.name.as_str()),
                    );
                    if !candidates.is_empty() {
                        suggestion.did_you_mean = Some(candidates);
                        populated = true;
                    }
                }
            }
        }
        ErrorCode::MissingArgument => {
            // Pattern A: "the required argument `{Parent}.{field}({arg}:)` is not provided"
            // Pattern B: "the required field `{InputType}.{field}` is not provided"
            if let Some(coord) = ticks.first().copied() {
                if let Some((left, right)) = coord.split_once('(') {
                    let single_arg = right.trim_end_matches(":)").trim_end_matches(')');
                    let mut missing = Vec::new();
                    if let Some((parent_type, field_name)) = left.split_once('.')
                        && let Ok(field_def) = visible_schema.type_field(parent_type, field_name)
                    {
                        let provided_opt = ast_doc.and_then(|doc| {
                            let first_loc = err.locations.as_ref()?.first().copied()?;
                            find_ast_provided_args_at_location(doc, first_loc, field_name)
                        });
                        if let Some(provided) = provided_opt {
                            for arg_def in &field_def.arguments {
                                if arg_def.ty.is_non_null()
                                    && arg_def.default_value.is_none()
                                    && !provided.contains(arg_def.name.as_str())
                                {
                                    missing.push(arg_def.name.as_str().to_owned());
                                }
                            }
                        }
                    }
                    if missing.is_empty() && !single_arg.is_empty() {
                        missing.push(single_arg.to_owned());
                    }
                    if !missing.is_empty() {
                        suggestion.missing_arguments = Some(missing);
                        populated = true;
                    }
                } else if let Some((_, field_name)) = coord.split_once('.')
                    && !field_name.is_empty()
                {
                    suggestion.missing_arguments = Some(vec![field_name.to_owned()]);
                    populated = true;
                }
            }
        }
        ErrorCode::TypeMismatch => {
            // Pattern: "value `{val}` does not exist on `{EnumType}`"
            if msg.starts_with("value `")
                && msg.contains("` does not exist on `")
                && ticks.len() >= 2
            {
                let bad_val = ticks[0];
                let enum_type = ticks[1];
                if let Some(ExtendedType::Enum(enum_def)) = visible_schema.types.get(enum_type) {
                    let candidates =
                        rank_did_you_mean(bad_val, enum_def.values.keys().map(|k| k.as_str()));
                    if !candidates.is_empty() {
                        suggestion.did_you_mean = Some(candidates);
                        populated = true;
                    }
                }
            }
        }
        ErrorCode::ValidationError => {
            // Unknown type in variable / fragment condition, or missing subselection on composite type
            if (msg.starts_with("cannot find type `") || msg.starts_with("type condition `"))
                && let Some(&unknown_ty) = ticks.first()
            {
                let candidates = rank_did_you_mean(
                    unknown_ty,
                    visible_schema
                        .types
                        .keys()
                        .map(|k| k.as_str())
                        .filter(|t| !t.starts_with("__")),
                );
                if !candidates.is_empty() {
                    suggestion.did_you_mean = Some(candidates);
                    populated = true;
                }
            }
        }
        _ => {}
    }

    if populated {
        err.with_suggestion(suggestion)
    } else {
        err
    }
}
