//! GAP type-system directive sanitization (`GQL-030`..`GQL-032`) and executable directive
//! extraction (`@budget`, `@dryRun`, `@format`, `@defer`/`@stream`; `spec/02-gap-ql.md` §§1, 5, 12).

use std::collections::{BTreeMap, HashSet};

use apollo_compiler::ast::{
    self, Argument, Definition, Directive, DirectiveList, FieldDefinition, Selection, Value,
};
use apollo_compiler::parser::SourceMap;
use apollo_compiler::response::{JsonMap, JsonValue};
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::validation::Valid;
use apollo_compiler::{Name, Node, Schema, name};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{BranchId, BranchUid};
use gap_core::snapshot::{Protocol, SideEffectLevel, TrustTier};

use crate::schema::{
    compose_supergraph_from_asts, is_root_operation_object_name, normalize_branch_root_definitions,
    parse_sdl_ast, span_to_source_location, validate_branch_ast_reserved_names,
};
use crate::validate::ValidatedOperation;

/// Join lifecycle status corresponding to `enum JoinStatus` in `spec/02-gap-ql.md` §5 (`GQL-031`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JoinDirectiveStatus {
    /// Proposed join awaiting verification (`PROPOSED`).
    Proposed,
    /// Verified join awaiting activation (`VERIFIED`).
    Verified,
    /// Active join eligible for query planning (`ACTIVE`, `INV-06`).
    Active,
    /// Deprecated join excluded from new plans (`DEPRECATED`).
    Deprecated,
}

/// Plannable `@join` edge extracted from a supergraph `Schema` (`GQL-031`, `INV-06`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannableJoin {
    /// Source GraphQL object or interface type name.
    pub parent_type: String,
    /// Field name carrying `@join`.
    pub field_name: String,
    /// Target operation ID (`to: String!`).
    pub target_operation: String,
    /// Lifecycle status (always [`JoinDirectiveStatus::Active`] in [`plannable_joins`]).
    pub status: JoinDirectiveStatus,
}

/// Input descriptor for sanitizing and composing a single branch's SDL (`GQL-030`..`GQL-032`).
#[derive(Debug, Clone)]
pub struct BranchCompositionInput<'a> {
    /// Validated branch slug (`id`).
    pub branch_id: BranchId,
    /// Immutable branch instance UUIDv7 (`uid`).
    pub branch_uid: BranchUid,
    /// Wire protocol implemented by the branch.
    pub protocol: Protocol,
    /// Trust tier (`T0`, `T1`, or `T2`).
    pub trust: TrustTier,
    /// Raw SDL reported by the branch.
    pub sdl: &'a str,
}

/// Sanitized branch AST paired with any `GAP_DIRECTIVE_IGNORED` warnings (`GQL-030`, `GQL-032`).
#[derive(Debug, Clone)]
pub struct SanitizedBranchAst {
    /// Normalized and directive-sanitized branch AST ready for supergraph composition.
    pub ast: ast::Document,
    /// Non-fatal composition warnings (`ErrorCode::DirectiveIgnored`).
    pub warnings: Vec<GapError>,
}

/// Supergraph `Schema` produced from branch inputs along with accumulated composition warnings.
#[derive(Debug, Clone)]
pub struct ComposedSupergraph {
    /// Validated supergraph schema including built-ins and programmatic §4 extensions.
    pub schema: Valid<Schema>,
    /// Non-fatal `GAP_DIRECTIVE_IGNORED` warnings emitted while sanitizing `T1`/`T2` branches.
    pub warnings: Vec<GapError>,
}

/// Requested wire representation on `@format(as: ...)` (`spec/02-gap-ql.md` §§5, 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Standard GraphQL JSON object array (`JSON`).
    Json,
    /// Extension X-1 columnar `{ columns, rows }` representation (`COLUMNAR`, scheduled M7).
    Columnar,
}

/// Parsed `@budget(...)` directive on an executable operation (`spec/02-gap-ql.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OperationBudgetDirective {
    /// Optional wall-clock deadline cap in milliseconds (`ms`).
    pub ms: Option<u64>,
    /// Optional token budget (`tokens`).
    pub tokens: Option<u64>,
    /// Optional static/dynamic cost unit cap (`costUnits`).
    pub cost_units: Option<u64>,
    /// Fail immediately on first budget/branch error (`failFast`, default `false`).
    pub fail_fast: bool,
}

/// Parsed executable directives extracted from a validated operation (`spec/02-gap-ql.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutableDirectives {
    /// Operation-level `@budget(...)` directive, if supplied.
    pub budget: Option<OperationBudgetDirective>,
    /// Whether `@dryRun` was supplied on a mutation operation.
    pub dry_run: bool,
    /// Field-level `@format(as: ...)` selections keyed by response key (`alias` or `name`).
    pub field_formats: BTreeMap<String, OutputFormat>,
    /// `true` if any field requested `@format(as: COLUMNAR)`.
    pub uses_columnar_format: bool,
}

impl ExecutableDirectives {
    /// Rejects `@format(as: COLUMNAR)` with [`ErrorCode::UnsupportedFeature`] when extension X-1
    /// (`M7`) is disabled (`spec/02-gap-ql.md` §12).
    pub fn ensure_columnar_allowed(&self, allow_columnar: bool) -> GapResult<()> {
        if self.uses_columnar_format && !allow_columnar {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "@format(as: COLUMNAR) requires extension X-1 and is not enabled",
            ));
        }
        Ok(())
    }
}

fn protocol_enum_name(protocol: Protocol) -> Name {
    match protocol {
        Protocol::Sql => name!("SQL"),
        Protocol::Rest => name!("REST"),
        Protocol::Grpc => name!("GRPC"),
        Protocol::Graphql => name!("GRAPHQL"),
        Protocol::Mcp => name!("MCP"),
        Protocol::A2a => name!("A2A"),
        Protocol::Cli => name!("CLI"),
        Protocol::CodeGraph => name!("CODE_GRAPH"),
        Protocol::Custom => name!("CUSTOM"),
    }
}

fn trust_enum_name(trust: TrustTier) -> Name {
    match trust {
        TrustTier::T0 => name!("T0"),
        TrustTier::T1 => name!("T1"),
        TrustTier::T2 => name!("T2"),
    }
}

fn make_branch_directive(input: &BranchCompositionInput<'_>) -> Node<Directive> {
    Node::new(Directive {
        name: name!("branch"),
        arguments: vec![
            Node::new(Argument {
                name: name!("id"),
                value: input.branch_id.as_str().into(),
            }),
            Node::new(Argument {
                name: name!("uid"),
                value: input.branch_uid.to_string().into(),
            }),
            Node::new(Argument {
                name: name!("protocol"),
                value: Node::new(Value::Enum(protocol_enum_name(input.protocol))),
            }),
            Node::new(Argument {
                name: name!("trust"),
                value: Node::new(Value::Enum(trust_enum_name(input.trust))),
            }),
        ],
    })
}

fn make_forced_unknown_side_effect() -> Node<Directive> {
    Node::new(Directive {
        name: name!("sideEffect"),
        arguments: vec![
            Node::new(Argument {
                name: name!("level"),
                value: Node::new(Value::Enum(name!("UNKNOWN"))),
            }),
            Node::new(Argument {
                name: name!("idempotent"),
                value: false.into(),
            }),
            Node::new(Argument {
                name: name!("dryRun"),
                value: false.into(),
            }),
        ],
    })
}

fn warning_with_span(
    msg: String,
    span: Option<apollo_compiler::parser::SourceSpan>,
    sources: &SourceMap,
) -> GapError {
    let err = GapError::new(ErrorCode::DirectiveIgnored, msg);
    if let Some(loc) = span_to_source_location(span, sources) {
        err.with_locations(vec![loc])
    } else {
        err
    }
}

fn sanitize_type_or_field_directives(
    directives: &mut DirectiveList,
    target_label: &str,
    input: &BranchCompositionInput<'_>,
    is_field: bool,
    stamp_branch: bool,
    sources: &SourceMap,
    warnings: &mut Vec<GapError>,
) {
    let untrusted_composer_directives = matches!(input.trust, TrustTier::T1 | TrustTier::T2);

    if untrusted_composer_directives {
        let mut kept = DirectiveList::new();
        for dir in directives.iter() {
            let dname = dir.name.as_str();
            if matches!(dname, "branch" | "capabilityGroup" | "provenance") {
                warnings.push(warning_with_span(
                    format!(
                        "ignored `@{dname}` on `{target_label}` from {:?} branch `{}` (GQL-030)",
                        input.trust, input.branch_id
                    ),
                    dir.location(),
                    sources,
                ));
                continue;
            }
            if dname == "join" {
                let mut had_status = None;
                let mut new_args = Vec::with_capacity(dir.arguments.len());
                for arg in &dir.arguments {
                    if arg.name.as_str() == "status" {
                        had_status = Some(arg.location());
                    } else {
                        new_args.push(arg.clone());
                    }
                }
                if let Some(arg_span) = had_status {
                    warnings.push(warning_with_span(
                        format!(
                            "ignored `@join(status:)` on `{target_label}` from {:?} branch `{}`; reset to PROPOSED (GQL-030)",
                            input.trust, input.branch_id
                        ),
                        arg_span.or_else(|| dir.location()),
                        sources,
                    ));
                    new_args.push(Node::new(Argument {
                        name: name!("status"),
                        value: Node::new(Value::Enum(name!("PROPOSED"))),
                    }));
                    let mut rewritten = (**dir).clone();
                    rewritten.arguments = new_args;
                    kept.push(Node::new(rewritten));
                } else {
                    kept.push(dir.clone());
                }
                continue;
            }
            kept.push(dir.clone());
        }
        *directives = kept;
    }

    if is_field && input.trust == TrustTier::T2 {
        // GQL-032: @sideEffect on fields of T2 branches is forced to UNKNOWN regardless of authored value.
        let mut authored_non_unknown = None;
        for dir in directives.iter() {
            if dir.name.as_str() == "sideEffect" {
                let is_already_unknown = dir.arguments.iter().any(|a| {
                    a.name.as_str() == "level"
                        && matches!(&*a.value, Value::Enum(v) if v.as_str() == "UNKNOWN")
                });
                if !is_already_unknown {
                    authored_non_unknown = Some(dir.location());
                }
            }
        }
        if let Some(span) = authored_non_unknown {
            warnings.push(warning_with_span(
                format!(
                    "forced `@sideEffect(level: UNKNOWN)` on T2 field `{target_label}` in branch `{}` (GQL-032)",
                    input.branch_id
                ),
                span,
                sources,
            ));
        }
        directives.retain(|d| d.name.as_str() != "sideEffect");
        directives.push(make_forced_unknown_side_effect());
    }

    if stamp_branch && !directives.has("branch") {
        directives.push(make_branch_directive(input));
    }
}

fn sanitize_fields(
    parent_name: &str,
    fields: &mut [Node<FieldDefinition>],
    input: &BranchCompositionInput<'_>,
    stamp_field_branch: bool,
    sources: &SourceMap,
    warnings: &mut Vec<GapError>,
) {
    for field_node in fields {
        let field = field_node.make_mut();
        let coord = format!("{parent_name}.{}", field.name);
        sanitize_type_or_field_directives(
            &mut field.directives,
            &coord,
            input,
            true,
            stamp_field_branch,
            sources,
            warnings,
        );
    }
}

/// Sanitizes a branch's SDL according to its [`TrustTier`] (`GQL-004`, `GQL-030`, `GQL-032`):
/// - Rejects reserved names (`GAP_RESERVED_NAME`).
/// - Strips `@branch`, `@capabilityGroup`, `@provenance`, and `@join(status:)` supplied by `T1`/`T2`
///   authors, recording `GAP_DIRECTIVE_IGNORED` warnings (`GQL-030`).
/// - Forces `@sideEffect(level: UNKNOWN)` on every field of `T2` branches (`GQL-032`).
/// - Stamps authoritative `@branch(...)` metadata from `input`.
pub fn sanitize_branch_sdl(input: &BranchCompositionInput<'_>) -> GapResult<SanitizedBranchAst> {
    let source_name = format!("{}.graphql", input.branch_id);
    let mut ast_doc = parse_sdl_ast(input.sdl, &source_name)?;
    validate_branch_ast_reserved_names(&ast_doc)?;
    normalize_branch_root_definitions(&mut ast_doc);

    let sources = ast_doc.sources.clone();
    let mut warnings = Vec::new();

    for def in &mut ast_doc.definitions {
        match def {
            Definition::ObjectTypeDefinition(obj_node) => {
                let obj = obj_node.make_mut();
                let type_name = obj.name.as_str().to_owned();
                let is_root = is_root_operation_object_name(&type_name);
                sanitize_type_or_field_directives(
                    &mut obj.directives,
                    &type_name,
                    input,
                    false,
                    !is_root,
                    &sources,
                    &mut warnings,
                );
                sanitize_fields(
                    &type_name,
                    &mut obj.fields,
                    input,
                    true,
                    &sources,
                    &mut warnings,
                );
            }
            Definition::ObjectTypeExtension(obj_node) => {
                let obj = obj_node.make_mut();
                let type_name = obj.name.as_str().to_owned();
                let is_root = is_root_operation_object_name(&type_name);
                sanitize_type_or_field_directives(
                    &mut obj.directives,
                    &type_name,
                    input,
                    false,
                    !is_root,
                    &sources,
                    &mut warnings,
                );
                sanitize_fields(
                    &type_name,
                    &mut obj.fields,
                    input,
                    true,
                    &sources,
                    &mut warnings,
                );
            }
            Definition::InterfaceTypeDefinition(iface_node) => {
                let iface = iface_node.make_mut();
                let type_name = iface.name.as_str().to_owned();
                sanitize_fields(
                    &type_name,
                    &mut iface.fields,
                    input,
                    false,
                    &sources,
                    &mut warnings,
                );
            }
            Definition::InterfaceTypeExtension(iface_node) => {
                let iface = iface_node.make_mut();
                let type_name = iface.name.as_str().to_owned();
                sanitize_fields(
                    &type_name,
                    &mut iface.fields,
                    input,
                    false,
                    &sources,
                    &mut warnings,
                );
            }
            _ => {}
        }
    }

    Ok(SanitizedBranchAst {
        ast: ast_doc,
        warnings,
    })
}

/// Sanitizes and composes multiple typed branch inputs onto GAP's built-in schema, returning the
/// validated supergraph `Schema` and all `GAP_DIRECTIVE_IGNORED` warnings (`GQL-030`..`GQL-032`).
pub fn compose_branch_inputs(
    branches: &[BranchCompositionInput<'_>],
) -> GapResult<ComposedSupergraph> {
    let mut asts = Vec::with_capacity(branches.len());
    let mut warnings = Vec::new();
    for branch in branches {
        let sanitized = sanitize_branch_sdl(branch)?;
        warnings.extend(sanitized.warnings);
        asts.push(sanitized.ast);
    }
    let schema = compose_supergraph_from_asts(&asts)?;
    Ok(ComposedSupergraph { schema, warnings })
}

fn parse_join_status(directives: &DirectiveList) -> Option<(String, JoinDirectiveStatus)> {
    let join_dir = directives.get("join")?;
    let mut to_op = String::new();
    let mut status = JoinDirectiveStatus::Proposed;
    for arg in &join_dir.arguments {
        match arg.name.as_str() {
            "to" => {
                if let Value::String(s) = &*arg.value {
                    to_op.clone_from(s);
                }
            }
            "status" => {
                if let Value::Enum(v) = &*arg.value {
                    status = match v.as_str() {
                        "VERIFIED" => JoinDirectiveStatus::Verified,
                        "ACTIVE" => JoinDirectiveStatus::Active,
                        "DEPRECATED" => JoinDirectiveStatus::Deprecated,
                        _ => JoinDirectiveStatus::Proposed,
                    };
                }
            }
            _ => {}
        }
    }
    Some((to_op, status))
}

/// Returns only `status: ACTIVE` `@join` edges from `schema` for the query planner (`GQL-031`,
/// `INV-06`). `PROPOSED`, `VERIFIED`, and `DEPRECATED` joins are never planned.
#[must_use]
pub fn plannable_joins(schema: &Schema) -> Vec<PlannableJoin> {
    let mut out = Vec::new();
    for (type_name, ext_ty) in &schema.types {
        let fields = match ext_ty {
            ExtendedType::Object(obj) => &obj.fields,
            ExtendedType::Interface(iface) => &iface.fields,
            _ => continue,
        };
        for (field_name, field_def) in fields {
            if let Some((to_op, JoinDirectiveStatus::Active)) =
                parse_join_status(&field_def.directives)
            {
                out.push(PlannableJoin {
                    parent_type: type_name.as_str().to_owned(),
                    field_name: field_name.as_str().to_owned(),
                    target_operation: to_op,
                    status: JoinDirectiveStatus::Active,
                });
            }
        }
    }
    out
}

/// Computes the effective [`SideEffectLevel`] of `parent_type.field_name` in `schema`, enforcing
/// `GQL-032` (`T2` fields always resolve to [`SideEffectLevel::Unknown`]).
#[must_use]
pub fn effective_field_side_effect(
    schema: &Schema,
    parent_type: &str,
    field_name: &str,
) -> Option<SideEffectLevel> {
    let field_def = schema.type_field(parent_type, field_name).ok()?;

    // Check field `@branch(trust: T2)` or parent object `@branch(trust: T2)` first (GQL-032).
    let field_is_t2 = field_def
        .directives
        .get("branch")
        .and_then(|d| d.arguments.iter().find(|a| a.name.as_str() == "trust"))
        .is_some_and(|a| matches!(&*a.value, Value::Enum(v) if v.as_str() == "T2"));
    let parent_is_t2 = match schema.types.get(parent_type) {
        Some(ExtendedType::Object(obj)) => obj
            .directives
            .get("branch")
            .and_then(|d| d.arguments.iter().find(|a| a.name.as_str() == "trust"))
            .is_some_and(|a| matches!(&*a.value, Value::Enum(v) if v.as_str() == "T2")),
        _ => false,
    };
    if field_is_t2 || parent_is_t2 {
        return Some(SideEffectLevel::Unknown);
    }

    if let Some(se_dir) = field_def.directives.get("sideEffect") {
        for arg in &se_dir.arguments {
            if arg.name.as_str() == "level"
                && let Value::Enum(level) = &*arg.value
            {
                return Some(match level.as_str() {
                    "NONE" => SideEffectLevel::None,
                    "REVERSIBLE" => SideEffectLevel::Reversible,
                    "IRREVERSIBLE" => SideEffectLevel::Irreversible,
                    _ => SideEffectLevel::Unknown,
                });
            }
        }
    }

    None
}

fn check_incremental_in_directives(
    directives: &DirectiveList,
    sources: &SourceMap,
) -> GapResult<()> {
    for dir in directives {
        let name = dir.name.as_str();
        if matches!(name, "defer" | "stream") {
            let err = GapError::new(
                ErrorCode::UnsupportedFeature,
                format!("`@{name}` incremental delivery is not enabled (GQL-001, RUN-062)"),
            );
            return Err(
                if let Some(loc) = span_to_source_location(dir.location(), sources) {
                    err.with_locations(vec![loc])
                } else {
                    err
                },
            );
        }
    }
    Ok(())
}

fn check_incremental_in_selections(selections: &[Selection], sources: &SourceMap) -> GapResult<()> {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                check_incremental_in_directives(&field.directives, sources)?;
                check_incremental_in_selections(&field.selection_set, sources)?;
            }
            Selection::FragmentSpread(spread) => {
                check_incremental_in_directives(&spread.directives, sources)?;
            }
            Selection::InlineFragment(inline) => {
                check_incremental_in_directives(&inline.directives, sources)?;
                check_incremental_in_selections(&inline.selection_set, sources)?;
            }
        }
    }
    Ok(())
}

/// Rejects `@defer` and `@stream` directives with [`ErrorCode::UnsupportedFeature`] when the
/// `incremental` feature (`RUN-062`) is not enabled (`GQL-001`).
pub fn check_executable_incremental_directives(
    ast_doc: &ast::Document,
    allow_incremental: bool,
) -> GapResult<()> {
    if allow_incremental {
        return Ok(());
    }
    for def in &ast_doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                check_incremental_in_directives(&op.directives, &ast_doc.sources)?;
                check_incremental_in_selections(&op.selection_set, &ast_doc.sources)?;
            }
            Definition::FragmentDefinition(frag) => {
                check_incremental_in_directives(&frag.directives, &ast_doc.sources)?;
                check_incremental_in_selections(&frag.selection_set, &ast_doc.sources)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn resolve_u64_arg(val: &Value, coerced_variables: &JsonMap) -> Option<u64> {
    let raw = match val {
        Value::Int(i) => i.try_to_i32().ok().map(i64::from),
        Value::Variable(var) => coerced_variables
            .get(var.as_str())
            .and_then(JsonValue::as_i64),
        _ => None,
    }?;
    u64::try_from(raw.max(0)).ok()
}

fn resolve_bool_arg(val: &Value, coerced_variables: &JsonMap) -> Option<bool> {
    match val {
        Value::Boolean(b) => Some(*b),
        Value::Variable(var) => coerced_variables
            .get(var.as_str())
            .and_then(JsonValue::as_bool),
        _ => None,
    }
}

fn collect_field_formats(
    doc: &apollo_compiler::ExecutableDocument,
    selections: &[apollo_compiler::executable::Selection],
    coerced_variables: &JsonMap,
    visited_fragments: &mut HashSet<Name>,
    out: &mut ExecutableDirectives,
) {
    for sel in selections {
        match sel {
            apollo_compiler::executable::Selection::Field(field) => {
                if let Some(fmt_dir) = field.directives.get("format") {
                    let mut fmt = OutputFormat::Json;
                    if let Some(as_arg) = fmt_dir.arguments.iter().find(|a| a.name.as_str() == "as")
                    {
                        match &*as_arg.value {
                            Value::Enum(e) if e.as_str() == "COLUMNAR" => {
                                fmt = OutputFormat::Columnar;
                            }
                            Value::Variable(var)
                                if coerced_variables
                                    .get(var.as_str())
                                    .and_then(JsonValue::as_str)
                                    == Some("COLUMNAR") =>
                            {
                                fmt = OutputFormat::Columnar;
                            }
                            _ => {}
                        }
                    }
                    if fmt == OutputFormat::Columnar {
                        out.uses_columnar_format = true;
                    }
                    out.field_formats
                        .insert(field.response_key().as_str().to_owned(), fmt);
                }
                collect_field_formats(
                    doc,
                    &field.selection_set.selections,
                    coerced_variables,
                    visited_fragments,
                    out,
                );
            }
            apollo_compiler::executable::Selection::InlineFragment(inline) => {
                collect_field_formats(
                    doc,
                    &inline.selection_set.selections,
                    coerced_variables,
                    visited_fragments,
                    out,
                );
            }
            apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                if visited_fragments.insert(spread.fragment_name.clone())
                    && let Some(frag) = doc.fragments.get(&spread.fragment_name)
                {
                    collect_field_formats(
                        doc,
                        &frag.selection_set.selections,
                        coerced_variables,
                        visited_fragments,
                        out,
                    );
                }
            }
        }
    }
}

/// Extracts `@budget`, `@dryRun`, and `@format` executable directives from a [`ValidatedOperation`].
pub fn extract_executable_directives(
    validated: &ValidatedOperation,
) -> GapResult<ExecutableDirectives> {
    let Some(op) = validated.operation() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "validated operation handle is missing target operation",
        ));
    };

    let mut out = ExecutableDirectives::default();

    if let Some(budget_dir) = op.directives.get("budget") {
        let mut b = OperationBudgetDirective::default();
        for arg in &budget_dir.arguments {
            match arg.name.as_str() {
                "ms" => b.ms = resolve_u64_arg(&arg.value, &validated.coerced_variables),
                "tokens" => b.tokens = resolve_u64_arg(&arg.value, &validated.coerced_variables),
                "costUnits" => {
                    b.cost_units = resolve_u64_arg(&arg.value, &validated.coerced_variables);
                }
                "failFast" => {
                    if let Some(ff) = resolve_bool_arg(&arg.value, &validated.coerced_variables) {
                        b.fail_fast = ff;
                    }
                }
                _ => {}
            }
        }
        out.budget = Some(b);
    }

    out.dry_run = op.directives.has("dryRun");

    let mut visited_fragments = HashSet::new();
    collect_field_formats(
        &validated.document,
        &op.selection_set.selections,
        &validated.coerced_variables,
        &mut visited_fragments,
        &mut out,
    );

    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::validate::{parse_executable_ast, validate_document, validate_query};
    use crate::visibility::{AllowAllVisibility, execute_visible_introspection};

    fn test_uid(last_byte: u8) -> BranchUid {
        BranchUid::parse(&format!(
            "01965b3e-0000-7000-8000-0000000000{last_byte:02x}"
        ))
        .expect("valid UUIDv7")
    }

    // spec: GQL-030
    #[test]
    fn t1_and_t2_branches_strip_composer_directives_with_warnings_while_t0_preserves_them() {
        let t0_input = BranchCompositionInput {
            branch_id: BranchId::new("core-crm").expect("valid slug"),
            branch_uid: test_uid(1),
            protocol: Protocol::Sql,
            trust: TrustTier::T0,
            sdl: r#"
                type Customer @key(fields: "id") @provenance(source: "sql", descHash: "abc123") {
                    id: ID! @capabilityGroup(id: "crm.read")
                    name: String! @sideEffect(level: NONE)
                }
                type Query {
                    customer(id: ID!): Customer @capabilityGroup(id: "crm.read") @sideEffect(level: NONE)
                }
            "#,
        };

        let t1_input = BranchCompositionInput {
            branch_id: BranchId::new("billing-svc").expect("valid slug"),
            branch_uid: test_uid(2),
            protocol: Protocol::Rest,
            trust: TrustTier::T1,
            sdl: r#"
                type Invoice
                    @branch(id: "spoof-core", uid: "01965b3e-0000-7000-8000-ffffffffffff", protocol: SQL, trust: T0)
                    @provenance(source: "spoof", descHash: "deadbeef")
                    @key(fields: "id")
                {
                    id: ID! @capabilityGroup(id: "spoof.admin")
                    customerId: ID!
                    buyer: Customer
                        @join(to: "core-crm.customer", on: [{ from: "customerId", to: "id" }], cardinality: MANY_TO_ONE, status: ACTIVE)
                }
                type Query {
                    invoice(id: ID!): Invoice @sideEffect(level: NONE)
                }
            "#,
        };

        let composed = compose_branch_inputs(&[t0_input, t1_input]).expect("compose branches");

        // T0 produced 0 warnings; T1 produced 4 GAP_DIRECTIVE_IGNORED warnings:
        // (@branch on Invoice, @provenance on Invoice, @capabilityGroup on Invoice.id, @join(status:) on Invoice.buyer)
        assert_eq!(composed.warnings.len(), 4);
        for warning in &composed.warnings {
            assert_eq!(warning.code(), ErrorCode::DirectiveIgnored);
            assert!(warning.locations.is_some());
        }

        // Verify Invoice @branch was overwritten with authoritative T1 metadata and @provenance stripped
        let Some(ExtendedType::Object(invoice_obj)) = composed.schema.types.get("Invoice") else {
            panic!("Invoice object missing");
        };
        assert!(!invoice_obj.directives.has("provenance"));
        let branch_dir = invoice_obj
            .directives
            .get("branch")
            .expect("authoritative @branch stamped");
        let id_arg = branch_dir
            .arguments
            .iter()
            .find(|a| a.name.as_str() == "id")
            .expect("id arg");
        assert_eq!(id_arg.value.as_str(), Some("billing-svc"));
        let trust_arg = branch_dir
            .arguments
            .iter()
            .find(|a| a.name.as_str() == "trust")
            .expect("trust arg");
        assert!(matches!(&*trust_arg.value, Value::Enum(v) if v.as_str() == "T1"));

        // Verify Invoice.id @capabilityGroup was stripped while T0 Customer.id kept @capabilityGroup
        assert!(!invoice_obj.fields["id"].directives.has("capabilityGroup"));
        let Some(ExtendedType::Object(customer_obj)) = composed.schema.types.get("Customer") else {
            panic!("Customer object missing");
        };
        assert!(customer_obj.directives.has("provenance"));
        assert!(customer_obj.fields["id"].directives.has("capabilityGroup"));

        // Verify Invoice.buyer @join(status: ACTIVE) from T1 was reset to PROPOSED and excluded from plannable_joins
        assert!(plannable_joins(&composed.schema).is_empty());
    }

    // spec: GQL-031, INV-06
    #[test]
    fn inv_06_only_active_joins_are_planned_and_non_active_joins_require_admin_schema_in_introspection()
     {
        let t0_joins = BranchCompositionInput {
            branch_id: BranchId::new("orders-core").expect("valid slug"),
            branch_uid: test_uid(10),
            protocol: Protocol::Sql,
            trust: TrustTier::T0,
            sdl: r#"
                type Account @key(fields: "id") {
                    id: ID!
                }
                type Order @key(fields: "id") {
                    id: ID!
                    accountId: ID!
                    activeAccount: Account
                        @join(to: "orders.acctActive", on: [{ from: "accountId", to: "id" }], cardinality: MANY_TO_ONE, status: ACTIVE)
                    proposedAccount: Account
                        @join(to: "orders.acctProposed", on: [{ from: "accountId", to: "id" }], cardinality: MANY_TO_ONE, status: PROPOSED)
                    verifiedAccount: Account
                        @join(to: "orders.acctVerified", on: [{ from: "accountId", to: "id" }], cardinality: MANY_TO_ONE, status: VERIFIED)
                    deprecatedAccount: Account
                        @join(to: "orders.acctDeprecated", on: [{ from: "accountId", to: "id" }], cardinality: MANY_TO_ONE, status: DEPRECATED)
                }
                type Query {
                    order(id: ID!): Order
                }
            "#,
        };

        let composed = compose_branch_inputs(&[t0_joins]).expect("compose T0 joins");
        assert!(composed.warnings.is_empty());

        // 1. Only ACTIVE join is plannable (INV-06, GQL-031)
        let planned = plannable_joins(&composed.schema);
        assert_eq!(
            planned,
            vec![PlannableJoin {
                parent_type: "Order".to_owned(),
                field_name: "activeAccount".to_owned(),
                target_operation: "orders.acctActive".to_owned(),
                status: JoinDirectiveStatus::Active,
            }]
        );

        // 2. Non-ACTIVE joins cannot be queried even by admin:schema
        let admin_vis = AllowAllVisibility::with_admin_schema(true);
        let non_admin_vis = AllowAllVisibility::with_admin_schema(false);
        crate::validate::validate_query_with_visibility(
            &composed.schema,
            "{ order(id: \"1\") { activeAccount { id } } }",
            &admin_vis,
        )
        .expect("ACTIVE join is executable");
        for non_active in ["proposedAccount", "verifiedAccount", "deprecatedAccount"] {
            let q = format!("{{ order(id: \"1\") {{ {non_active} {{ id }} }} }}");
            let err =
                crate::validate::validate_query_with_visibility(&composed.schema, &q, &admin_vis)
                    .expect_err("non-ACTIVE join must not be executable");
            assert_eq!(err.code(), ErrorCode::UnknownField);
        }

        // 3. Introspection hides PROPOSED/VERIFIED/DEPRECATED from regular principals and shows them to admin:schema
        let introspect_q =
            r#"{ __type(name: "Order") { fields(includeDeprecated: true) { name } } }"#;
        let empty_vars = serde_json::Map::new();

        let regular_json = execute_visible_introspection(
            &composed.schema,
            introspect_q,
            None,
            &empty_vars,
            &non_admin_vis,
        )
        .expect("regular introspection");
        let regular_fields: Vec<&str> = regular_json["data"]["__type"]["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert!(regular_fields.contains(&"activeAccount"));
        assert!(!regular_fields.contains(&"proposedAccount"));
        assert!(!regular_fields.contains(&"verifiedAccount"));
        assert!(!regular_fields.contains(&"deprecatedAccount"));

        let admin_json = execute_visible_introspection(
            &composed.schema,
            introspect_q,
            None,
            &empty_vars,
            &admin_vis,
        )
        .expect("admin introspection");
        let admin_fields: Vec<&str> = admin_json["data"]["__type"]["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert!(admin_fields.contains(&"activeAccount"));
        assert!(admin_fields.contains(&"proposedAccount"));
        assert!(admin_fields.contains(&"verifiedAccount"));
        assert!(admin_fields.contains(&"deprecatedAccount"));
    }

    // spec: GQL-032 (ADR-015)
    #[test]
    fn t2_branch_fields_force_unknown_side_effect_regardless_of_authored_value() {
        let t1_branch = BranchCompositionInput {
            branch_id: BranchId::new("internal-mcp").expect("valid slug"),
            branch_uid: test_uid(20),
            protocol: Protocol::Mcp,
            trust: TrustTier::T1,
            sdl: r#"
                type Query {
                    internalPing: String! @sideEffect(level: NONE)
                }
            "#,
        };
        let t2_branch = BranchCompositionInput {
            branch_id: BranchId::new("external-mcp").expect("valid slug"),
            branch_uid: test_uid(21),
            protocol: Protocol::Mcp,
            trust: TrustTier::T2,
            sdl: r#"
                type ExternalItem {
                    id: ID! @sideEffect(level: NONE)
                    label: String!
                }
                type Query {
                    externalFetch: ExternalItem @sideEffect(level: REVERSIBLE, idempotent: true, dryRun: true)
                }
            "#,
        };

        let composed = compose_branch_inputs(&[t1_branch, t2_branch]).expect("compose T1 + T2");

        // T1 retains NONE
        assert_eq!(
            effective_field_side_effect(&composed.schema, "Query", "internalPing"),
            Some(SideEffectLevel::None)
        );

        // T2 fields forced to UNKNOWN whether authored NONE, REVERSIBLE, or omitted
        assert_eq!(
            effective_field_side_effect(&composed.schema, "ExternalItem", "id"),
            Some(SideEffectLevel::Unknown)
        );
        assert_eq!(
            effective_field_side_effect(&composed.schema, "ExternalItem", "label"),
            Some(SideEffectLevel::Unknown)
        );
        assert_eq!(
            effective_field_side_effect(&composed.schema, "Query", "externalFetch"),
            Some(SideEffectLevel::Unknown)
        );

        // Warnings emitted for the two T2 fields that authored non-UNKNOWN @sideEffect
        assert_eq!(composed.warnings.len(), 2);
        assert!(
            composed
                .warnings
                .iter()
                .all(|w| w.code() == ErrorCode::DirectiveIgnored)
        );
    }

    // spec: GQL-001
    #[test]
    fn executable_directives_extract_budget_dry_run_format_and_reject_defer_stream() {
        let schema = crate::schema::with_gap_builtins(
            r#"
            type Item { id: ID! total: Float! }
            type Query { items: [Item!]! }
            type Mutation { updateItem(id: ID!): Item! }
            "#,
        )
        .expect("schema");

        // 1. @budget + @format(as: COLUMNAR) with variables
        let query = r#"
            query ListItems($cost: Int!, $fmt: Format!) @budget(ms: 2500, tokens: 1200, costUnits: $cost, failFast: true) {
                rows: items @format(as: $fmt) { id total }
            }
        "#;
        let vars = json!({ "cost": 900, "fmt": "COLUMNAR" });
        let validated =
            validate_document(&schema, query, Some("ListItems"), vars.as_object().unwrap())
                .expect("valid query");
        let extracted = extract_executable_directives(&validated).expect("extract directives");
        assert_eq!(
            extracted.budget,
            Some(OperationBudgetDirective {
                ms: Some(2500),
                tokens: Some(1200),
                cost_units: Some(900),
                fail_fast: true,
            })
        );
        assert!(!extracted.dry_run);
        assert!(extracted.uses_columnar_format);
        assert_eq!(
            extracted.field_formats.get("rows"),
            Some(&OutputFormat::Columnar)
        );
        let col_err = extracted
            .ensure_columnar_allowed(false)
            .expect_err("COLUMNAR disabled before M7");
        assert_eq!(col_err.code(), ErrorCode::UnsupportedFeature);
        extracted
            .ensure_columnar_allowed(true)
            .expect("COLUMNAR allowed when X-1 enabled");

        // 2. @dryRun valid on MUTATION, rejected on QUERY
        let valid_mut = validate_query(
            &schema,
            r#"mutation Preview @dryRun { updateItem(id: "1") { id } }"#,
        )
        .expect("mutation @dryRun");
        let mut_dirs = extract_executable_directives(&valid_mut).expect("extract mutation dirs");
        assert!(mut_dirs.dry_run);

        let bad_dry_run = validate_query(&schema, "query Bad @dryRun { items { id } }")
            .expect_err("query @dryRun");
        assert_eq!(bad_dry_run.code(), ErrorCode::ValidationError);

        // 3. @defer and @stream rejected with GAP_UNSUPPORTED_FEATURE when incremental is false
        for inc_q in [
            "{ ... @defer { items { id } } }",
            "{ items @stream(initialCount: 1) { id } }",
        ] {
            let ast = parse_executable_ast(inc_q, "inc.graphql").expect("valid syntax");
            let err = check_executable_incremental_directives(&ast, false)
                .expect_err("incremental disabled");
            assert_eq!(err.code(), ErrorCode::UnsupportedFeature);
            assert!(err.locations.is_some());
        }
    }
}
