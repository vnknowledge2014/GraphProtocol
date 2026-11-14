//! GraphQL document type rewriting (`<ns>_<Type>` -> upstream `<Type>`), selection rendering, Relay cursor wrapping, and aliased / `_entities` batch builders (`spec/09-compat.md` §5.4, `NGC-020`, `NGC-026`, `INV-18`).

use std::collections::{BTreeMap, BTreeSet};

use apollo_compiler::ast::{self, Selection as AstSelection, Type as AstType};
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::{Name, Node, Schema};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_core::ids::is_allowed_graphql_identifier;
use gap_ngc_sdk::connector::NgcResult;
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_proto::ngc::v1::{OpKind, Page, Selection, SelectionField};
use serde_json::{Map, Value};

use crate::config::EntityBatchMapping;
use crate::schema::{ExtractedGraphqlOp, GraphqlArgDef, RelayConnectionInfo};

/// Rendered upstream GraphQL query/mutation ready for HTTP `POST` execution.
#[derive(Debug, Clone)]
pub struct PreparedGraphqlCall {
    /// Executable GraphQL document referencing upstream type names.
    pub query: String,
    /// Parameterized variables object (never string-interpolated).
    pub variables: Map<String, Value>,
    /// Operation name matching the header in `query`.
    pub operation_name: String,
    /// True when the connector auto-wrapped a Relay Connection selection to unwrap `edges[].node`.
    pub relay_unwrap: bool,
}

/// Rewrites all named type references (`variableDefinitions`, `fragment ... on`, `... on <Type>`)
/// in a raw GraphQL executable document from branch-namespaced `<ns>_<Type>` back to upstream `<Type>`
/// (`spec/09-compat.md` §5.4).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if `doc_src` fails GraphQL AST parsing or contains an invalid type name.
pub fn rewrite_executable_document_types(
    doc_src: &str,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<String> {
    let mut parser = apollo_compiler::parser::Parser::new()
        .recursion_limit(256)
        .token_limit(100_000);
    let mut doc = parser
        .parse_ast(doc_src, "query.graphql")
        .map_err(|with_errs| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("failed to parse GraphQL document: {}", with_errs.errors),
            )
        })?;

    for def in &mut doc.definitions {
        match def {
            ast::Definition::OperationDefinition(op_node) => {
                let op = op_node.make_mut();
                for var_node in &mut op.variables {
                    let var = var_node.make_mut();
                    rewrite_ast_type(var.ty.make_mut(), branch_to_upstream)?;
                }
                rewrite_ast_selection_set(&mut op.selection_set, branch_to_upstream)?;
            }
            ast::Definition::FragmentDefinition(frag_node) => {
                let frag = frag_node.make_mut();
                if let Some(up) = branch_to_upstream.get(frag.type_condition.as_str()) {
                    frag.type_condition = Name::new(up).map_err(|err| {
                        NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("invalid upstream type name `{up}`: {err}"),
                        )
                    })?;
                }
                rewrite_ast_selection_set(&mut frag.selection_set, branch_to_upstream)?;
            }
            _ => {}
        }
    }

    Ok(doc.serialize().to_string())
}

fn rewrite_ast_type(
    ty: &mut AstType,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<()> {
    match ty {
        AstType::Named(n) | AstType::NonNullNamed(n) => {
            if let Some(up) = branch_to_upstream.get(n.as_str()) {
                *n = Name::new(up).map_err(|err| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("invalid upstream type name `{up}`: {err}"),
                    )
                })?;
            }
            Ok(())
        }
        AstType::List(inner) | AstType::NonNullList(inner) => {
            rewrite_ast_type(inner, branch_to_upstream)
        }
    }
}

fn rewrite_ast_selection_set(
    set: &mut [AstSelection],
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<()> {
    for sel in set {
        match sel {
            AstSelection::Field(field_node) => {
                let field = field_node.make_mut();
                rewrite_ast_selection_set(&mut field.selection_set, branch_to_upstream)?;
            }
            AstSelection::InlineFragment(inf_node) => {
                let inf = inf_node.make_mut();
                if let Some(tc) = &mut inf.type_condition
                    && let Some(up) = branch_to_upstream.get(tc.as_str())
                {
                    *tc = Name::new(up).map_err(|err| {
                        NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("invalid upstream type name `{up}`: {err}"),
                        )
                    })?;
                }
                rewrite_ast_selection_set(&mut inf.selection_set, branch_to_upstream)?;
            }
            AstSelection::FragmentSpread(_) => {}
        }
    }
    Ok(())
}

fn normalize_integral_numbers(val: Value) -> Value {
    match val {
        Value::Number(ref n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else if let Some(f) = n.as_f64()
                && f.is_finite()
                && f.fract() == 0.0
                && (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&f)
            {
                Value::Number((f as i64).into())
            } else {
                val
            }
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(normalize_integral_numbers).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, normalize_integral_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Validates and merges `base` + `overlay` arguments plus optional Relay `page` (`first` / `after`).
#[must_use]
pub fn merge_operation_args(
    op: &ExtractedGraphqlOp,
    base: Option<&prost_types::Struct>,
    overlay: Option<&prost_types::Struct>,
    page: Option<&Page>,
) -> Map<String, Value> {
    let mut merged = base.map(prost_struct_to_json_map).unwrap_or_default();
    if let Some(ov) = overlay {
        for (k, v) in prost_struct_to_json_map(ov) {
            merged.insert(k, v);
        }
    }
    for val in merged.values_mut() {
        let taken = std::mem::replace(val, Value::Null);
        *val = normalize_integral_numbers(taken);
    }
    if let Some(p) = page
        && let Some(relay) = &op.relay
    {
        if relay.has_first_arg && p.limit > 0 && !merged.contains_key("first") {
            merged.insert("first".to_owned(), Value::Number(p.limit.into()));
        }
        if relay.has_after_arg && !p.cursor.is_empty() && !merged.contains_key("after") {
            merged.insert("after".to_owned(), Value::String(p.cursor.clone()));
        }
    }
    merged
}

/// Builds a single upstream GraphQL query or mutation document (`spec/09-compat.md` §5.4).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if selection fields contain invalid GraphQL identifiers.
pub fn build_single_call(
    op: &ExtractedGraphqlOp,
    args_map: &Map<String, Value>,
    selection: Option<&Selection>,
    upstream_schema: &Schema,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<PreparedGraphqlCall> {
    let op_keyword = match op.kind {
        OpKind::Mutation => "mutation",
        _ => "query",
    };
    let op_doc_name = "GapSingleOp".to_owned();

    let mut var_decls = Vec::new();
    let mut arg_bindings = Vec::new();
    let mut variables = Map::new();

    for arg_def in &op.args {
        if let Some(val) = args_map.get(&arg_def.name) {
            var_decls.push(format!("${}: {}", arg_def.name, arg_def.upstream_type));
            arg_bindings.push(format!("{}: ${}", arg_def.name, arg_def.name));
            variables.insert(arg_def.name.clone(), val.clone());
        } else if arg_def.required {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "missing required GraphQL argument `{}` on operation `{}`",
                    arg_def.name, op.name
                ),
            ));
        }
    }

    let (sel_str, relay_unwrap) =
        render_operation_selection(op, selection, upstream_schema, branch_to_upstream)?;

    let decls_part = if var_decls.is_empty() {
        String::new()
    } else {
        format!("({})", var_decls.join(", "))
    };
    let args_part = if arg_bindings.is_empty() {
        String::new()
    } else {
        format!("({})", arg_bindings.join(", "))
    };

    let query = format!(
        "{op_keyword} {op_doc_name}{decls_part} {{\n  {}{args_part}{sel_str}\n}}",
        op.name
    );

    Ok(PreparedGraphqlCall {
        query,
        variables,
        operation_name: op_doc_name,
        relay_unwrap,
    })
}

/// Builds ONE GraphQL query bundling `batch_args` as aliased root fields (`_b0`, `_b1`, ...) (`NGC-020`, `NGC-026`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if selection fields are invalid or a required argument is missing.
pub fn build_aliased_batch_call(
    op: &ExtractedGraphqlOp,
    batch_args: &[Map<String, Value>],
    selection: Option<&Selection>,
    upstream_schema: &Schema,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<(PreparedGraphqlCall, Vec<String>)> {
    let op_doc_name = "GapAliasedBatch".to_owned();
    let mut var_decls = Vec::new();
    let mut variables = Map::new();
    let mut root_fields = Vec::with_capacity(batch_args.len());
    let mut aliases = Vec::with_capacity(batch_args.len());

    let (sel_str, relay_unwrap) =
        render_operation_selection(op, selection, upstream_schema, branch_to_upstream)?;

    for (idx, args_map) in batch_args.iter().enumerate() {
        let alias = format!("_b{idx}");
        let mut bindings = Vec::new();

        for arg_def in &op.args {
            if let Some(val) = args_map.get(&arg_def.name) {
                let var_name = format!("b{idx}_{}", arg_def.name);
                var_decls.push(format!("${var_name}: {}", arg_def.upstream_type));
                bindings.push(format!("{}: ${var_name}", arg_def.name));
                variables.insert(var_name, val.clone());
            } else if arg_def.required {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!(
                        "missing required GraphQL argument `{}` for batch item {idx} on `{}`",
                        arg_def.name, op.name
                    ),
                ));
            }
        }

        let args_part = if bindings.is_empty() {
            String::new()
        } else {
            format!("({})", bindings.join(", "))
        };
        root_fields.push(format!("  {alias}: {}{args_part}{sel_str}", op.name));
        aliases.push(alias);
    }

    let decls_part = if var_decls.is_empty() {
        String::new()
    } else {
        format!("({})", var_decls.join(", "))
    };
    let query = format!(
        "query {op_doc_name}{decls_part} {{\n{}\n}}",
        root_fields.join("\n")
    );

    Ok((
        PreparedGraphqlCall {
            query,
            variables,
            operation_name: op_doc_name,
            relay_unwrap,
        },
        aliases,
    ))
}

/// Builds ONE Apollo Federation `_entities(representations: $reps)` query for a batch of entity keys (`spec/09-compat.md` §5.4, `NGC-020`).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if key fields are missing from an item or selection syntax is invalid.
pub fn build_entities_batch_call(
    op: &ExtractedGraphqlOp,
    entity_map: &EntityBatchMapping,
    batch_args: &[Map<String, Value>],
    selection: Option<&Selection>,
    upstream_schema: &Schema,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<PreparedGraphqlCall> {
    let op_doc_name = "GapEntitiesBatch".to_owned();
    let mut reps = Vec::with_capacity(batch_args.len());

    for (idx, args_map) in batch_args.iter().enumerate() {
        let mut rep = Map::new();
        rep.insert(
            "__typename".to_owned(),
            Value::String(entity_map.upstream_typename.clone()),
        );
        for key in &entity_map.key_fields {
            let val = args_map.get(key).ok_or_else(|| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!(
                        "missing Federation representation key `{key}` in batch item {idx} for `{}`",
                        op.name
                    ),
                )
            })?;
            rep.insert(key.clone(), val.clone());
        }
        reps.push(Value::Object(rep));
    }

    let inner_body = render_composite_body(
        selection,
        &entity_map.upstream_typename,
        upstream_schema,
        branch_to_upstream,
    )?;

    let query = format!(
        "query {op_doc_name}($reps: [_Any!]!) {{\n  _entities(representations: $reps) {{\n    __typename\n    ... on {} {{\n{inner_body}\n    }}\n  }}\n}}",
        entity_map.upstream_typename
    );

    let mut variables = Map::new();
    variables.insert("reps".to_owned(), Value::Array(reps));

    Ok(PreparedGraphqlCall {
        query,
        variables,
        operation_name: op_doc_name,
        relay_unwrap: false,
    })
}

fn render_operation_selection(
    op: &ExtractedGraphqlOp,
    selection: Option<&Selection>,
    upstream_schema: &Schema,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<(String, bool)> {
    if !op.returns_composite {
        return Ok((String::new(), false));
    }

    if let Some(relay) = &op.relay {
        let wants_connection_shape = selection.is_some_and(|sel| {
            sel.fields
                .iter()
                .any(|f| matches!(f.name.as_str(), "edges" | "pageInfo" | "totalCount"))
        });

        if wants_connection_shape {
            let mut rendered = render_selection_fields(
                selection.map_or(&[], |s| s.fields.as_slice()),
                branch_to_upstream,
                1,
            )?;
            let has_page_info =
                selection.is_some_and(|s| s.fields.iter().any(|f| f.name == "pageInfo"));
            if !has_page_info {
                rendered.push_str("\n    pageInfo { hasNextPage endCursor }");
            }
            return Ok((format!(" {{\n{rendered}\n  }}"), false));
        }

        // Caller asked for item fields directly or omitted selection -> unwrap Relay `edges { node { ... } cursor } pageInfo`.
        let node_body = render_composite_body(
            selection,
            &relay.node_type,
            upstream_schema,
            branch_to_upstream,
        )?;
        let wrapped = format!(
            " {{\n    edges {{\n      cursor\n      node {{\n{node_body}\n      }}\n    }}\n    pageInfo {{\n      hasNextPage\n      endCursor\n    }}\n  }}"
        );
        return Ok((wrapped, true));
    }

    let body = render_composite_body(
        selection,
        &op.return_named_type,
        upstream_schema,
        branch_to_upstream,
    )?;
    Ok((format!(" {{\n{body}\n  }}"), false))
}

fn render_composite_body(
    selection: Option<&Selection>,
    upstream_type_name: &str,
    upstream_schema: &Schema,
    branch_to_upstream: &BTreeMap<String, String>,
) -> NgcResult<String> {
    if let Some(sel) = selection
        && !sel.fields.is_empty()
    {
        return render_selection_fields(&sel.fields, branch_to_upstream, 2);
    }

    // Default scalar/enum leaf selection from upstream schema when caller left Selection empty.
    let mut leaf_fields = Vec::new();
    if let Some(obj) = upstream_schema.get_object(upstream_type_name) {
        for (fname, fcomp) in &obj.fields {
            let is_leaf = is_scalar_or_enum(upstream_schema, &fcomp.ty);
            if is_leaf {
                leaf_fields.push(format!("      {}", fname.as_str()));
                if leaf_fields.len() >= 32 {
                    break;
                }
            }
        }
    }
    if leaf_fields.is_empty() {
        leaf_fields.push("      __typename".to_owned());
    }
    Ok(leaf_fields.join("\n"))
}

fn is_scalar_or_enum(schema: &Schema, ty: &AstType) -> bool {
    let base = match ty {
        AstType::Named(n) | AstType::NonNullNamed(n) => n.as_str(),
        AstType::List(inner) | AstType::NonNullList(inner) => {
            return is_scalar_or_enum(schema, inner);
        }
    };
    matches!(base, "String" | "Int" | "Float" | "Boolean" | "ID" | "JSON")
        || matches!(
            schema.types.get(base),
            Some(ExtendedType::Scalar(_) | ExtendedType::Enum(_))
        )
}

fn render_selection_fields(
    fields: &[SelectionField],
    branch_to_upstream: &BTreeMap<String, String>,
    depth: usize,
) -> NgcResult<String> {
    let indent = "  ".repeat(depth);
    let mut lines = Vec::with_capacity(fields.len());

    for field in fields {
        let raw_name = field.name.trim();
        if let Some(rest) = raw_name.strip_prefix("... on ") {
            let branch_ty = rest.trim();
            if !is_allowed_graphql_identifier(branch_ty) {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("invalid inline fragment type `{branch_ty}`"),
                ));
            }
            let target_ty = branch_to_upstream
                .get(branch_ty)
                .map_or(branch_ty, String::as_str);
            let child_body = match &field.children {
                Some(ch) if !ch.fields.is_empty() => {
                    render_selection_fields(&ch.fields, branch_to_upstream, depth + 1)?
                }
                _ => format!("{indent}  __typename"),
            };
            lines.push(format!(
                "{indent}... on {target_ty} {{\n{child_body}\n{indent}}}"
            ));
            continue;
        }

        if raw_name != "__typename" && !is_allowed_graphql_identifier(raw_name) {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("invalid GraphQL selection field name `{raw_name}`"),
            ));
        }
        let prefix = if field.alias.is_empty() {
            String::new()
        } else {
            let alias = field.alias.trim();
            if !is_allowed_graphql_identifier(alias) {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("invalid GraphQL selection alias `{alias}`"),
                ));
            }
            format!("{alias}: ")
        };

        let child_block = match &field.children {
            Some(ch) if !ch.fields.is_empty() => {
                let inner = render_selection_fields(&ch.fields, branch_to_upstream, depth + 1)?;
                format!(" {{\n{inner}\n{indent}}}")
            }
            _ => String::new(),
        };
        lines.push(format!("{indent}{prefix}{raw_name}{child_block}"));
    }

    Ok(lines.join("\n"))
}

/// Decoded single-call outcome including rows and optional Relay `endCursor`.
#[derive(Debug, Clone)]
pub struct DecodedSingleResult {
    pub rows: Vec<prost_types::Struct>,
    pub next_cursor: String,
    pub partial_error: Option<String>,
}

/// Extracts NGC rows and optional Relay cursor from a single upstream GraphQL response payload (`spec/09-compat.md` §5.4).
///
/// # Errors
///
/// Returns `UPSTREAM_ERROR` when `data` lacks the target field and `errors[]` indicates execution failure.
pub fn extract_single_result(
    op: &ExtractedGraphqlOp,
    data: Option<&Value>,
    errors: &[Value],
    relay_unwrap: bool,
) -> NgcResult<DecodedSingleResult> {
    let field_val = data
        .and_then(Value::as_object)
        .and_then(|m| m.get(&op.name));

    let partial_error = first_error_message(errors);
    let Some(val) = field_val else {
        if let Some(msg) = partial_error {
            return Err(NgcErrorInfo::new(NgcErrorCode::UpstreamError, msg));
        }
        return Ok(DecodedSingleResult {
            rows: Vec::new(),
            next_cursor: String::new(),
            partial_error: None,
        });
    };

    if val.is_null() {
        if let Some(msg) = partial_error {
            return Err(NgcErrorInfo::new(NgcErrorCode::UpstreamError, msg));
        }
        return Ok(DecodedSingleResult {
            rows: Vec::new(),
            next_cursor: String::new(),
            partial_error: None,
        });
    }

    let (rows, next_cursor) = value_to_rows(val, op.relay.as_ref(), relay_unwrap)?;
    Ok(DecodedSingleResult {
        rows,
        next_cursor,
        partial_error,
    })
}

/// Extracts per-index results from an aliased root-field batch response (`_b0`, `_b1`, ...) (`NGC-020`).
pub fn extract_aliased_batch_results(
    op: &ExtractedGraphqlOp,
    aliases: &[String],
    data: Option<&Value>,
    errors: &[Value],
    relay_unwrap: bool,
) -> NgcResult<Vec<NgcResult<Vec<prost_types::Struct>>>> {
    let data_obj = data.and_then(Value::as_object);
    let mut err_by_alias: BTreeMap<String, String> = BTreeMap::new();
    let mut root_errors = Vec::new();

    for err in errors {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream GraphQL error")
            .to_owned();
        if let Some(first_seg) = err
            .get("path")
            .and_then(Value::as_array)
            .and_then(|p| p.first())
            .and_then(Value::as_str)
        {
            err_by_alias.entry(first_seg.to_owned()).or_insert(msg);
        } else {
            root_errors.push(msg);
        }
    }

    if data_obj.is_none()
        && let Some(msg) = root_errors.first()
    {
        return Err(NgcErrorInfo::new(NgcErrorCode::UpstreamError, msg.clone()));
    }

    let mut out = Vec::with_capacity(aliases.len());
    for alias in aliases {
        if let Some(err_msg) = err_by_alias.get(alias) {
            out.push(Err(NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                err_msg.clone(),
            )));
            continue;
        }
        let field_val = data_obj.and_then(|m| m.get(alias));
        match field_val {
            None | Some(Value::Null) => out.push(Ok(Vec::new())),
            Some(v) => match value_to_rows(v, op.relay.as_ref(), relay_unwrap) {
                Ok((rows, _)) => out.push(Ok(rows)),
                Err(e) => out.push(Err(e)),
            },
        }
    }
    Ok(out)
}

/// Extracts per-index results from an Apollo Federation `_entities` batch response (`NGC-020`, `INV-18`).
pub fn extract_entities_batch_results(
    expected_count: usize,
    data: Option<&Value>,
    errors: &[Value],
) -> NgcResult<Vec<NgcResult<Vec<prost_types::Struct>>>> {
    let mut err_by_idx: BTreeMap<usize, String> = BTreeMap::new();
    let mut root_errors = Vec::new();

    for err in errors {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream GraphQL _entities error")
            .to_owned();
        if let Some(path) = err.get("path").and_then(Value::as_array)
            && path.first().and_then(Value::as_str) == Some("_entities")
            && let Some(idx_u64) = path.get(1).and_then(Value::as_u64)
            && let Ok(idx) = usize::try_from(idx_u64)
        {
            err_by_idx.entry(idx).or_insert(msg);
            continue;
        }
        root_errors.push(msg);
    }

    let Some(entities_arr) = data
        .and_then(Value::as_object)
        .and_then(|m| m.get("_entities"))
        .and_then(Value::as_array)
    else {
        let msg = root_errors.first().cloned().unwrap_or_else(|| {
            "upstream Federation response missing `data._entities` array".to_owned()
        });
        return Err(NgcErrorInfo::new(NgcErrorCode::UpstreamError, msg));
    };

    let mut out = Vec::with_capacity(expected_count);
    for idx in 0..expected_count {
        if let Some(err_msg) = err_by_idx.get(&idx) {
            out.push(Err(NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                err_msg.clone(),
            )));
            continue;
        }
        match entities_arr.get(idx) {
            None | Some(Value::Null) => out.push(Ok(Vec::new())),
            Some(Value::Object(obj)) => out.push(Ok(vec![json_map_to_prost_struct(obj)])),
            Some(other) => {
                let mut wrap = Map::new();
                wrap.insert("value".to_owned(), other.clone());
                out.push(Ok(vec![json_map_to_prost_struct(&wrap)]));
            }
        }
    }
    Ok(out)
}

fn value_to_rows(
    val: &Value,
    relay: Option<&RelayConnectionInfo>,
    relay_unwrap: bool,
) -> NgcResult<(Vec<prost_types::Struct>, String)> {
    if relay.is_some()
        && let Value::Object(obj) = val
    {
        let next_cursor = extract_relay_cursor(obj);
        if relay_unwrap {
            let mut rows = Vec::new();
            if let Some(edges) = obj.get("edges").and_then(Value::as_array) {
                for edge in edges {
                    if let Some(node) = edge.get("node").and_then(Value::as_object) {
                        rows.push(json_map_to_prost_struct(node));
                    }
                }
            }
            return Ok((rows, next_cursor));
        }
        return Ok((vec![json_map_to_prost_struct(obj)], next_cursor));
    }

    match val {
        Value::Null => Ok((Vec::new(), String::new())),
        Value::Object(obj) => Ok((vec![json_map_to_prost_struct(obj)], String::new())),
        Value::Array(items) => {
            let mut rows = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Object(obj) => rows.push(json_map_to_prost_struct(obj)),
                    Value::Null => {}
                    other => {
                        let mut wrap = Map::new();
                        wrap.insert("value".to_owned(), other.clone());
                        rows.push(json_map_to_prost_struct(&wrap));
                    }
                }
            }
            Ok((rows, String::new()))
        }
        other => {
            let mut wrap = Map::new();
            wrap.insert("value".to_owned(), other.clone());
            Ok((vec![json_map_to_prost_struct(&wrap)], String::new()))
        }
    }
}

fn extract_relay_cursor(conn_obj: &Map<String, Value>) -> String {
    let Some(page_info) = conn_obj.get("pageInfo").and_then(Value::as_object) else {
        return String::new();
    };
    let has_next = page_info
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !has_next {
        return String::new();
    }
    page_info
        .get("endCursor")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn first_error_message(errors: &[Value]) -> Option<String> {
    errors
        .first()
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Validates that a batch variable slice does not exceed `BTreeSet` invariants (unused dummy check helper removed).
#[allow(dead_code)]
fn _unused(_: &BTreeSet<String>, _: &GraphqlArgDef, _: &Node<ast::Field>) {}
