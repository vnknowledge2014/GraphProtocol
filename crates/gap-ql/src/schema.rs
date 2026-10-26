//! Built-in GAP-QL schema (`spec/02-gap-ql.md` §§3–5), programmatic introspection extensions,
//! supergraph SDL composition helpers, and `GQL-004` reserved-name checks (`L1`).

use apollo_compiler::ast::{
    self, Definition, DirectiveList as AstDirectiveList, FieldDefinition, InputValueDefinition,
    ObjectTypeExtension, Type, Value,
};
use apollo_compiler::collections::IndexMap;
use apollo_compiler::collections::IndexSet;
use apollo_compiler::parser::{Parser, SourceMap, SourceSpan};
use apollo_compiler::schema::{Component, DirectiveList, ExtendedType, ObjectType};
use apollo_compiler::validation::Valid;
use apollo_compiler::{Name, Node, Schema, name};
use gap_core::error::{ErrorCode, GapError, GapResult, SourceLocation};
use gap_core::ids::{RESERVED_BUILTIN_TYPES, RESERVED_ROOT_FIELDS};

/// Embedded single-source-of-truth built-in GAP-QL SDL (`spec/02-gap-ql.md` §3).
pub const BUILTIN_SDL: &str = include_str!("../schema/builtin.graphql");

/// Embedded single-source-of-truth GAP type-system directives SDL (`spec/02-gap-ql.md` §5).
pub const TYPE_SYSTEM_DIRECTIVES_SDL: &str =
    include_str!("../schema/type_system_directives.graphql");

/// Embedded single-source-of-truth GAP executable directives SDL (`spec/02-gap-ql.md` §5).
pub const EXECUTABLE_DIRECTIVES_SDL: &str = include_str!("../schema/executable_directives.graphql");

/// Directive names reserved by GraphQL and GAP (`spec/02-gap-ql.md` §§3, 5; `GQL-004`).
pub const RESERVED_DIRECTIVE_NAMES: &[&str] = &[
    "skip",
    "include",
    "deprecated",
    "specifiedBy",
    "oneOf",
    "branch",
    "capabilityGroup",
    "key",
    "join",
    "sideEffect",
    "compensate",
    "auth",
    "cost",
    "provenance",
    "temporal",
    "pii",
    "budget",
    "dryRun",
    "format",
    "defer",
    "stream",
];

/// Default parser recursion cap protecting against deeply nested adversarial inputs.
pub const DEFAULT_PARSER_RECURSION_LIMIT: usize = 256;

/// Default parser token cap protecting against oversized token floods.
pub const DEFAULT_PARSER_TOKEN_LIMIT: usize = 100_000;

/// Returns a bounded [`Parser`] configured with GAP's recursion and token limits.
#[must_use]
pub fn bounded_parser() -> Parser {
    Parser::new()
        .recursion_limit(DEFAULT_PARSER_RECURSION_LIMIT)
        .token_limit(DEFAULT_PARSER_TOKEN_LIMIT)
}

/// Converts an `apollo-compiler` [`SourceSpan`] into a 1-based [`SourceLocation`].
#[must_use]
pub fn span_to_source_location(
    span: Option<SourceSpan>,
    sources: &SourceMap,
) -> Option<SourceLocation> {
    let lc = span?.line_column(sources)?;
    let line = u32::try_from(lc.line).unwrap_or(u32::MAX);
    let column = u32::try_from(lc.column).unwrap_or(u32::MAX);
    Some(SourceLocation { line, column })
}

fn attach_span(err: GapError, span: Option<SourceSpan>, sources: &SourceMap) -> GapError {
    if let Some(loc) = span_to_source_location(span, sources) {
        err.with_locations(vec![loc])
    } else {
        err
    }
}

/// Returns `true` if `name` is one of the three default GraphQL root operation object types.
#[must_use]
pub fn is_root_operation_object_name(name: &str) -> bool {
    matches!(name, "Query" | "Mutation" | "Subscription")
}

/// Returns `true` if `name` collides with a reserved GraphQL or GAP directive (`GQL-004`).
#[must_use]
pub fn is_reserved_directive_name(name: &str) -> bool {
    RESERVED_DIRECTIVE_NAMES.contains(&name)
}

fn non_null_named(type_name: Name) -> Type {
    Type::NonNullNamed(type_name)
}

fn nullable_named(type_name: Name) -> Type {
    Type::Named(type_name)
}

fn non_null_list_of_non_null(type_name: Name) -> Type {
    Type::NonNullList(Box::new(Type::NonNullNamed(type_name)))
}

fn nullable_list_of_non_null(type_name: Name) -> Type {
    Type::List(Box::new(Type::NonNullNamed(type_name)))
}

fn make_field(
    name: Name,
    ty: Type,
    arguments: Vec<Node<InputValueDefinition>>,
) -> Component<FieldDefinition> {
    Component::new(FieldDefinition {
        description: None,
        name,
        arguments,
        ty,
        directives: AstDirectiveList::new(),
    })
}

fn make_arg(
    name: Name,
    ty: Type,
    default_value: Option<Node<Value>>,
) -> Node<InputValueDefinition> {
    Node::new(InputValueDefinition {
        description: None,
        name,
        ty: Node::new(ty),
        default_value,
        directives: AstDirectiveList::new(),
    })
}

/// Injects the `SearchResult` and `SearchHit` object types from `spec/02-gap-ql.md` §4 into `schema`
/// prior to `Schema::validate()`.
pub fn inject_gap_introspection_types(schema: &mut Schema) {
    let mut search_result_fields = IndexMap::default();
    search_result_fields.insert(
        name!("hits"),
        make_field(
            name!("hits"),
            non_null_list_of_non_null(name!("SearchHit")),
            Vec::new(),
        ),
    );
    search_result_fields.insert(
        name!("tokensEstimate"),
        make_field(
            name!("tokensEstimate"),
            non_null_named(name!("Int")),
            Vec::new(),
        ),
    );
    search_result_fields.insert(
        name!("abstained"),
        make_field(
            name!("abstained"),
            non_null_named(name!("Boolean")),
            Vec::new(),
        ),
    );
    schema.types.insert(
        name!("SearchResult"),
        ExtendedType::Object(Node::new(ObjectType {
            description: None,
            name: name!("SearchResult"),
            implements_interfaces: IndexSet::default(),
            directives: DirectiveList::default(),
            fields: search_result_fields,
        })),
    );

    let mut search_hit_fields = IndexMap::default();
    search_hit_fields.insert(
        name!("score"),
        make_field(name!("score"), non_null_named(name!("Float")), Vec::new()),
    );
    search_hit_fields.insert(
        name!("kind"),
        make_field(
            name!("kind"),
            non_null_named(name!("ElementKind")),
            Vec::new(),
        ),
    );
    search_hit_fields.insert(
        name!("name"),
        make_field(name!("name"), non_null_named(name!("String")), Vec::new()),
    );
    search_hit_fields.insert(
        name!("path"),
        make_field(name!("path"), non_null_named(name!("String")), Vec::new()),
    );
    search_hit_fields.insert(
        name!("branch"),
        make_field(name!("branch"), non_null_named(name!("ID")), Vec::new()),
    );
    search_hit_fields.insert(
        name!("group"),
        make_field(name!("group"), nullable_named(name!("ID")), Vec::new()),
    );
    search_hit_fields.insert(
        name!("description"),
        make_field(
            name!("description"),
            non_null_named(name!("String")),
            Vec::new(),
        ),
    );
    search_hit_fields.insert(
        name!("sdl"),
        make_field(name!("sdl"), non_null_named(name!("String")), Vec::new()),
    );
    search_hit_fields.insert(
        name!("sideEffect"),
        make_field(
            name!("sideEffect"),
            nullable_named(name!("SideEffectLevel")),
            Vec::new(),
        ),
    );
    search_hit_fields.insert(
        name!("trust"),
        make_field(
            name!("trust"),
            non_null_named(name!("TrustTier")),
            Vec::new(),
        ),
    );
    schema.types.insert(
        name!("SearchHit"),
        ExtendedType::Object(Node::new(ObjectType {
            description: None,
            name: name!("SearchHit"),
            implements_interfaces: IndexSet::default(),
            directives: DirectiveList::default(),
            fields: search_hit_fields,
        })),
    );
}

/// Injects the implicit `Query.__search` and `Query.__metaGraph` root fields from
/// `spec/02-gap-ql.md` §4 onto `Query` after `Schema::validate()`.
///
/// Section 4 intentionally uses `__`-prefixed root field names that are prohibited in SDL
/// (`GQL-004`), so they are constructed programmatically in code rather than parsed from SDL.
pub fn inject_gap_introspection_fields(schema: &mut Schema) -> GapResult<()> {
    let query_name = schema
        .schema_definition
        .query
        .as_ref()
        .map(|cn| cn.name.clone())
        .unwrap_or_else(|| name!("Query"));

    let Some(ExtendedType::Object(query_obj)) = schema.types.get_mut(&query_name) else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("supergraph schema is missing root query object `{query_name}`"),
        ));
    };

    let query_mut = query_obj.make_mut();
    let search_args = vec![
        make_arg(name!("query"), non_null_named(name!("String")), None),
        make_arg(
            name!("first"),
            nullable_named(name!("Int")),
            Some(5_i32.into()),
        ),
        make_arg(
            name!("kinds"),
            nullable_list_of_non_null(name!("ElementKind")),
            None,
        ),
        make_arg(
            name!("branches"),
            nullable_list_of_non_null(name!("ID")),
            None,
        ),
        make_arg(name!("budgetTokens"), nullable_named(name!("Int")), None),
    ];
    query_mut.fields.insert(
        name!("__search"),
        make_field(
            name!("__search"),
            non_null_named(name!("SearchResult")),
            search_args,
        ),
    );
    query_mut.fields.insert(
        name!("__metaGraph"),
        make_field(
            name!("__metaGraph"),
            non_null_named(name!("MetaGraph")),
            Vec::new(),
        ),
    );
    Ok(())
}

/// Validates `@oneOf` type-system constraints on every `@oneOf` input object in `schema`
/// (each field must be nullable and must not declare a default value).
pub fn validate_oneof_schema_definitions(schema: &Schema) -> GapResult<()> {
    for (type_name, ext_ty) in &schema.types {
        let ExtendedType::InputObject(input_obj) = ext_ty else {
            continue;
        };
        if !input_obj.directives.has("oneOf") {
            continue;
        }
        if input_obj.fields.is_empty() {
            return Err(attach_span(
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("@oneOf input object `{type_name}` must define at least one field"),
                ),
                input_obj.name.location(),
                &schema.sources,
            ));
        }
        for (field_name, field_def) in &input_obj.fields {
            if field_def.ty.is_non_null() {
                return Err(attach_span(
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!("@oneOf input field `{type_name}.{field_name}` must be nullable"),
                    ),
                    field_def.name.location(),
                    &schema.sources,
                ));
            }
            if field_def.default_value.is_some() {
                return Err(attach_span(
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "@oneOf input field `{type_name}.{field_name}` must not have a default value"
                        ),
                    ),
                    field_def.name.location(),
                    &schema.sources,
                ));
            }
        }
    }
    Ok(())
}

fn check_no_double_underscore(ident: &Name, what: &str, sources: &SourceMap) -> GapResult<()> {
    if ident.as_str().starts_with("__") {
        return Err(attach_span(
            GapError::new(
                ErrorCode::ReservedName,
                format!(
                    "reserved identifier `{ident}` in {what}: names starting with `__` are forbidden (GQL-004)"
                ),
            ),
            ident.location(),
            sources,
        ));
    }
    Ok(())
}

fn check_type_name_not_reserved(
    type_name: &Name,
    allow_root_object: bool,
    sources: &SourceMap,
) -> GapResult<()> {
    check_no_double_underscore(type_name, "type definition", sources)?;
    let raw = type_name.as_str();
    if RESERVED_BUILTIN_TYPES.contains(&raw) {
        return Err(attach_span(
            GapError::new(
                ErrorCode::ReservedName,
                format!(
                    "branch schema cannot redefine reserved GAP built-in type `{raw}` (GQL-004)"
                ),
            ),
            type_name.location(),
            sources,
        ));
    }
    if !allow_root_object && is_root_operation_object_name(raw) {
        return Err(attach_span(
            GapError::new(
                ErrorCode::ReservedName,
                format!("`{raw}` is reserved as a root operation object type (GQL-004)"),
            ),
            type_name.location(),
            sources,
        ));
    }
    Ok(())
}

fn check_input_values_reserved(
    args: &[Node<InputValueDefinition>],
    what: &str,
    sources: &SourceMap,
) -> GapResult<()> {
    for arg in args {
        check_no_double_underscore(&arg.name, what, sources)?;
    }
    Ok(())
}

fn check_fields_reserved(
    parent_type: &Name,
    fields: &[Node<FieldDefinition>],
    sources: &SourceMap,
) -> GapResult<()> {
    let is_root = is_root_operation_object_name(parent_type.as_str());
    for field in fields {
        check_no_double_underscore(&field.name, "field definition", sources)?;
        if is_root && RESERVED_ROOT_FIELDS.contains(&field.name.as_str()) {
            return Err(attach_span(
                GapError::new(
                    ErrorCode::ReservedName,
                    format!(
                        "branch schema cannot define reserved root field `{parent_type}.{}` (GQL-004)",
                        field.name
                    ),
                ),
                field.name.location(),
                sources,
            ));
        }
        check_input_values_reserved(&field.arguments, "argument definition", sources)?;
    }
    Ok(())
}

/// Validates a parsed branch SDL [`ast::Document`] against `GQL-004` reserved-name rules.
///
/// Rejects:
/// - executable definitions (`query`/`mutation`/`subscription`/`fragment`) in branch SDL
///   (`GAP_VALIDATION_ERROR`),
/// - any type, field, argument, input field, enum value, or directive starting with `__`
///   (`GAP_RESERVED_NAME`),
/// - any branch type colliding with [`RESERVED_BUILTIN_TYPES`] (`GAP_RESERVED_NAME`),
/// - any branch directive definition colliding with [`RESERVED_DIRECTIVE_NAMES`] (`GAP_RESERVED_NAME`),
/// - any field on `Query`, `Mutation`, or `Subscription` colliding with [`RESERVED_ROOT_FIELDS`]
///   (`gap`, `__search`, `__metaGraph`, `gapTaskEvents`; `GAP_RESERVED_NAME`).
pub fn validate_branch_ast_reserved_names(doc: &ast::Document) -> GapResult<()> {
    for def in &doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                return Err(attach_span(
                    GapError::new(
                        ErrorCode::ValidationError,
                        "branch SDL must not contain executable operation definitions",
                    ),
                    op.location(),
                    &doc.sources,
                ));
            }
            Definition::FragmentDefinition(frag) => {
                return Err(attach_span(
                    GapError::new(
                        ErrorCode::ValidationError,
                        "branch SDL must not contain executable fragment definitions",
                    ),
                    frag.location(),
                    &doc.sources,
                ));
            }
            Definition::DirectiveDefinition(dir) => {
                check_no_double_underscore(&dir.name, "directive definition", &doc.sources)?;
                if is_reserved_directive_name(dir.name.as_str()) {
                    return Err(attach_span(
                        GapError::new(
                            ErrorCode::ReservedName,
                            format!(
                                "branch schema cannot redefine reserved directive `@{}` (GQL-004)",
                                dir.name
                            ),
                        ),
                        dir.name.location(),
                        &doc.sources,
                    ));
                }
                check_input_values_reserved(&dir.arguments, "directive argument", &doc.sources)?;
            }
            Definition::SchemaDefinition(schema_def) => {
                for root_op in &schema_def.root_operations {
                    check_no_double_underscore(&root_op.1, "schema root operation", &doc.sources)?;
                }
            }
            Definition::SchemaExtension(schema_ext) => {
                for root_op in &schema_ext.root_operations {
                    check_no_double_underscore(&root_op.1, "schema extension", &doc.sources)?;
                }
            }
            Definition::ScalarTypeDefinition(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
            }
            Definition::ScalarTypeExtension(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
            }
            Definition::ObjectTypeDefinition(ty) => {
                check_type_name_not_reserved(&ty.name, true, &doc.sources)?;
                for iface in &ty.implements_interfaces {
                    check_no_double_underscore(iface, "implements interface", &doc.sources)?;
                }
                check_fields_reserved(&ty.name, &ty.fields, &doc.sources)?;
            }
            Definition::ObjectTypeExtension(ty) => {
                check_type_name_not_reserved(&ty.name, true, &doc.sources)?;
                for iface in &ty.implements_interfaces {
                    check_no_double_underscore(iface, "implements interface", &doc.sources)?;
                }
                check_fields_reserved(&ty.name, &ty.fields, &doc.sources)?;
            }
            Definition::InterfaceTypeDefinition(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                for iface in &ty.implements_interfaces {
                    check_no_double_underscore(iface, "implements interface", &doc.sources)?;
                }
                check_fields_reserved(&ty.name, &ty.fields, &doc.sources)?;
            }
            Definition::InterfaceTypeExtension(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                for iface in &ty.implements_interfaces {
                    check_no_double_underscore(iface, "implements interface", &doc.sources)?;
                }
                check_fields_reserved(&ty.name, &ty.fields, &doc.sources)?;
            }
            Definition::UnionTypeDefinition(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                for member in &ty.members {
                    check_no_double_underscore(member, "union member", &doc.sources)?;
                }
            }
            Definition::UnionTypeExtension(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                for member in &ty.members {
                    check_no_double_underscore(member, "union member", &doc.sources)?;
                }
            }
            Definition::EnumTypeDefinition(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                for val in &ty.values {
                    check_no_double_underscore(&val.value, "enum value", &doc.sources)?;
                }
            }
            Definition::EnumTypeExtension(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                for val in &ty.values {
                    check_no_double_underscore(&val.value, "enum value", &doc.sources)?;
                }
            }
            Definition::InputObjectTypeDefinition(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                check_input_values_reserved(&ty.fields, "input field", &doc.sources)?;
            }
            Definition::InputObjectTypeExtension(ty) => {
                check_type_name_not_reserved(&ty.name, false, &doc.sources)?;
                check_input_values_reserved(&ty.fields, "input field", &doc.sources)?;
            }
        }
    }
    Ok(())
}

/// Parses `branch_sdl` and validates it against `GQL-004` reserved-name constraints.
pub fn validate_branch_sdl_reserved_names(branch_sdl: &str) -> GapResult<()> {
    let doc = parse_sdl_ast(branch_sdl, "branch.graphql")?;
    validate_branch_ast_reserved_names(&doc)
}

/// Parses a GraphQL SDL document into an [`ast::Document`], mapping syntax failures to
/// [`ErrorCode::ParseError`].
pub fn parse_sdl_ast(sdl: &str, source_name: &str) -> GapResult<ast::Document> {
    bounded_parser()
        .parse_ast(sdl, source_name)
        .map_err(|with_errs| {
            if let Some(diag) = with_errs.errors.iter().next() {
                let json = diag.to_json();
                let mut err = GapError::new(ErrorCode::ParseError, json.message);
                let locs: Vec<SourceLocation> = json
                    .locations
                    .into_iter()
                    .map(|lc| SourceLocation {
                        line: u32::try_from(lc.line).unwrap_or(u32::MAX),
                        column: u32::try_from(lc.column).unwrap_or(u32::MAX),
                    })
                    .collect();
                if !locs.is_empty() {
                    err = err.with_locations(locs);
                }
                err
            } else {
                GapError::new(ErrorCode::ParseError, "invalid GraphQL SDL syntax")
            }
        })
}

/// Rewrites `type Query`, `type Mutation`, and `type Subscription` object definitions in a branch
/// AST into `extend type ...` extensions so they compose onto GAP's built-in root operation types.
pub(crate) fn normalize_branch_root_definitions(doc: &mut ast::Document) {
    for def in &mut doc.definitions {
        let rewrite = match def {
            Definition::ObjectTypeDefinition(obj)
                if is_root_operation_object_name(obj.name.as_str()) =>
            {
                Some(Definition::ObjectTypeExtension(Node::new(
                    ObjectTypeExtension {
                        name: obj.name.clone(),
                        implements_interfaces: obj.implements_interfaces.clone(),
                        directives: obj.directives.clone(),
                        fields: obj.fields.clone(),
                    },
                )))
            }
            _ => None,
        };
        if let Some(ext) = rewrite {
            *def = ext;
        }
    }
}

/// Builds and validates the standalone built-in GAP-QL schema (`spec/02-gap-ql.md` §§3–5, `GQL-010`).
pub fn builtin_schema() -> GapResult<Valid<Schema>> {
    compose_supergraph_sdl(&[])
}

/// Composes one or more branch SDL documents onto the GAP built-in schema, enforcing `GQL-004`
/// reserved-name checks, `@oneOf` input-object rules, GraphQL type-system validation, and
/// programmatic injection of `spec/02-gap-ql.md` §4 (`__search`, `__metaGraph`, `SearchResult`,
/// `SearchHit`).
pub fn compose_supergraph_sdl(branch_sdls: &[&str]) -> GapResult<Valid<Schema>> {
    let mut parsed_branches = Vec::with_capacity(branch_sdls.len());
    for (idx, sdl) in branch_sdls.iter().enumerate() {
        let source_name = format!("branch_{idx}.graphql");
        let mut ast_doc = parse_sdl_ast(sdl, &source_name)?;
        validate_branch_ast_reserved_names(&ast_doc)?;
        normalize_branch_root_definitions(&mut ast_doc);
        parsed_branches.push(ast_doc);
    }
    compose_supergraph_from_asts(&parsed_branches)
}

/// Convenience helper composing a single branch SDL string onto GAP's built-in schema.
pub fn with_gap_builtins(branch_sdl: &str) -> GapResult<Valid<Schema>> {
    if branch_sdl.trim().is_empty() {
        builtin_schema()
    } else {
        compose_supergraph_sdl(&[branch_sdl])
    }
}

pub(crate) fn compose_supergraph_from_asts(
    branch_asts: &[ast::Document],
) -> GapResult<Valid<Schema>> {
    let builtin_ast = parse_sdl_ast(BUILTIN_SDL, "builtin.graphql")?;
    let ts_directives_ast =
        parse_sdl_ast(TYPE_SYSTEM_DIRECTIVES_SDL, "type_system_directives.graphql")?;
    let exec_directives_ast =
        parse_sdl_ast(EXECUTABLE_DIRECTIVES_SDL, "executable_directives.graphql")?;

    let mut builder = Schema::builder()
        .add_ast(&builtin_ast)
        .add_ast(&ts_directives_ast)
        .add_ast(&exec_directives_ast);
    for branch_ast in branch_asts {
        builder = builder.add_ast(branch_ast);
    }

    let mut schema = builder.build().map_err(|with_errs| {
        if let Some(diag) = with_errs.errors.iter().next() {
            let json = diag.to_json();
            let mut err = GapError::new(ErrorCode::ValidationError, json.message);
            let locs: Vec<SourceLocation> = json
                .locations
                .into_iter()
                .map(|lc| SourceLocation {
                    line: u32::try_from(lc.line).unwrap_or(u32::MAX),
                    column: u32::try_from(lc.column).unwrap_or(u32::MAX),
                })
                .collect();
            if !locs.is_empty() {
                err = err.with_locations(locs);
            }
            err
        } else {
            GapError::new(
                ErrorCode::ValidationError,
                "failed to build supergraph schema",
            )
        }
    })?;

    inject_gap_introspection_types(&mut schema);

    let valid_schema = schema.validate().map_err(|with_errs| {
        if let Some(diag) = with_errs.errors.iter().next() {
            let json = diag.to_json();
            let mut err = GapError::new(ErrorCode::ValidationError, json.message);
            let locs: Vec<SourceLocation> = json
                .locations
                .into_iter()
                .map(|lc| SourceLocation {
                    line: u32::try_from(lc.line).unwrap_or(u32::MAX),
                    column: u32::try_from(lc.column).unwrap_or(u32::MAX),
                })
                .collect();
            if !locs.is_empty() {
                err = err.with_locations(locs);
            }
            err
        } else {
            GapError::new(ErrorCode::ValidationError, "invalid supergraph schema")
        }
    })?;

    let mut inner = valid_schema.into_inner();
    validate_oneof_schema_definitions(&inner)?;
    inject_gap_introspection_fields(&mut inner)?;
    Ok(Valid::assume_valid(inner))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::path::Path;

    use apollo_compiler::ExecutableDocument;
    use apollo_compiler::introspection::partial_execute;
    use apollo_compiler::request::coerce_variable_values;
    use apollo_compiler::response::JsonMap;
    use proptest::prelude::*;

    use super::*;

    fn extract_graphql_blocks_in_section(markdown: &str, heading_prefix: &str) -> Vec<String> {
        let mut in_section = false;
        let mut in_graphql_fence = false;
        let mut current = String::new();
        let mut blocks = Vec::new();

        for line in markdown.lines() {
            if line.starts_with("## ") {
                if in_section {
                    break;
                }
                if line.starts_with(heading_prefix) {
                    in_section = true;
                }
                continue;
            }
            if !in_section {
                continue;
            }
            if !in_graphql_fence {
                if line.trim() == "```graphql" {
                    in_graphql_fence = true;
                    current.clear();
                }
            } else if line.trim() == "```" {
                in_graphql_fence = false;
                blocks.push(current.clone());
            } else {
                current.push_str(line);
                current.push('\n');
            }
        }
        blocks
    }

    // spec: GQL-001, GQL-010 (ADR-001)
    #[test]
    fn embedded_sdl_files_match_spec_02_fenced_blocks_verbatim() {
        let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/02-gap-ql.md");
        let spec_md = fs::read_to_string(&spec_path).expect("read spec/02-gap-ql.md");

        let sec3_blocks = extract_graphql_blocks_in_section(
            &spec_md,
            "## 3. Reserved namespace and built-in schema",
        );
        assert_eq!(
            sec3_blocks.len(),
            1,
            "expected exactly 1 graphql block in §3"
        );
        assert_eq!(
            BUILTIN_SDL, sec3_blocks[0],
            "crates/gap-ql/schema/builtin.graphql drifted from spec/02-gap-ql.md §3"
        );

        let sec5_blocks = extract_graphql_blocks_in_section(&spec_md, "## 5. Directives");
        assert_eq!(
            sec5_blocks.len(),
            2,
            "expected exactly 2 graphql blocks in §5"
        );
        assert_eq!(
            TYPE_SYSTEM_DIRECTIVES_SDL, sec5_blocks[0],
            "crates/gap-ql/schema/type_system_directives.graphql drifted from spec/02-gap-ql.md §5"
        );
        assert_eq!(
            EXECUTABLE_DIRECTIVES_SDL, sec5_blocks[1],
            "crates/gap-ql/schema/executable_directives.graphql drifted from spec/02-gap-ql.md §5"
        );
    }

    // spec: GQL-010, GQL-001
    #[test]
    fn builtin_schema_validates_and_exposes_all_builtin_and_extension_types_in_introspection() {
        let schema = builtin_schema().expect("built-in schema must validate");

        for reserved_ty in RESERVED_BUILTIN_TYPES {
            assert!(
                schema.types.contains_key(*reserved_ty),
                "missing reserved built-in type `{reserved_ty}` in supergraph schema"
            );
        }

        for dir_name in [
            "oneOf",
            "branch",
            "capabilityGroup",
            "key",
            "join",
            "sideEffect",
            "compensate",
            "auth",
            "cost",
            "provenance",
            "temporal",
            "pii",
            "budget",
            "dryRun",
            "format",
        ] {
            assert!(
                schema.directive_definitions.contains_key(dir_name),
                "missing directive `@{dir_name}` in supergraph schema"
            );
        }

        let query_doc = ExecutableDocument::parse_and_validate(
            &schema,
            r#"{
                __schema {
                    queryType { name fields { name } }
                    mutationType { name fields { name } }
                    subscriptionType { name fields { name } }
                }
                searchResult: __type(name: "SearchResult") {
                    kind
                    fields { name }
                }
                searchHit: __type(name: "SearchHit") {
                    kind
                    fields { name }
                }
            }"#,
            "introspect.graphql",
        )
        .expect("introspection query must validate");

        let op = query_doc.operations.get(None).expect("anonymous query");
        let vars = coerce_variable_values(&schema, op, &JsonMap::default()).expect("empty vars");
        let resp = partial_execute(&schema, &schema.implementers_map(), &query_doc, op, &vars)
            .expect("partial_execute introspection");
        assert!(
            resp.errors.is_empty(),
            "unexpected errors: {:?}",
            resp.errors
        );

        let data_json: serde_json::Value =
            serde_json::to_value(&resp.data).expect("serialize introspection data");
        let query_fields: Vec<&str> = data_json["__schema"]["queryType"]["fields"]
            .as_array()
            .expect("queryType.fields")
            .iter()
            .map(|f| f["name"].as_str().expect("field name"))
            .collect();
        assert!(query_fields.contains(&"gap"));
        assert!(query_fields.contains(&"__search"));
        assert!(query_fields.contains(&"__metaGraph"));

        let search_hit_fields: Vec<&str> = data_json["searchHit"]["fields"]
            .as_array()
            .expect("SearchHit.fields")
            .iter()
            .map(|f| f["name"].as_str().expect("field name"))
            .collect();
        for expected in [
            "score",
            "kind",
            "name",
            "path",
            "branch",
            "group",
            "description",
            "sdl",
            "sideEffect",
            "trust",
        ] {
            assert!(
                search_hit_fields.contains(&expected),
                "SearchHit missing `{expected}`"
            );
        }
    }

    // spec: GQL-004
    #[test]
    fn branch_sdl_rejects_double_underscore_and_reserved_names() {
        let cases = [
            ("type __Evil { id: ID! }", "double-underscore object type"),
            (
                "type Billing { __secret: String }",
                "double-underscore field",
            ),
            (
                "type Billing { invoice(id: ID!, __debug: Boolean): String }",
                "double-underscore argument",
            ),
            (
                "enum InvoiceKind { STANDARD __HIDDEN }",
                "double-underscore enum value",
            ),
            (
                "input InvoiceFilter { __raw: String }",
                "double-underscore input field",
            ),
            (
                "directive @__internal on FIELD_DEFINITION",
                "double-underscore directive",
            ),
            (
                "type Query { gap: String }",
                "reserved root field Query.gap",
            ),
            (
                "extend type Query { __search: String }",
                "reserved root field Query.__search",
            ),
            (
                "extend type Query { __metaGraph: String }",
                "reserved root field Query.__metaGraph",
            ),
            (
                "extend type Mutation { gap: String }",
                "reserved root field Mutation.gap",
            ),
            (
                "extend type Subscription { gapTaskEvents: String }",
                "reserved root field Subscription.gapTaskEvents",
            ),
            ("type GapNode { custom: String }", "reserved built-in type"),
            ("scalar DateTime", "reserved built-in scalar redefinition"),
            (
                "directive @cost(weight: Int) on FIELD_DEFINITION",
                "reserved GAP directive redefinition",
            ),
        ];

        for (sdl, label) in cases {
            let err = with_gap_builtins(sdl)
                .err()
                .unwrap_or_else(|| panic!("case `{label}` should have failed"));
            assert_eq!(
                err.code(),
                ErrorCode::ReservedName,
                "case `{label}` returned wrong code: {err:?}"
            );
            assert!(err.message().chars().count() <= gap_core::error::MAX_ERROR_MESSAGE_CHARS);
        }
    }

    // spec: GQL-004, GQL-010
    #[test]
    fn valid_branch_sdl_composes_cleanly_onto_supergraph() {
        let schema = with_gap_builtins(
            r#"
            type Query {
                billing: BillingQuery!
            }
            type BillingQuery {
                invoice(id: ID!): Invoice
            }
            type Invoice @key(fields: "id") {
                id: ID!
                total: Float!
                issuedAt: DateTime!
            }
            "#,
        )
        .expect("valid branch SDL must compose");

        assert!(schema.types.contains_key("Invoice"));
        assert!(schema.type_field("Query", "gap").is_ok());
        assert!(schema.type_field("Query", "billing").is_ok());
        assert!(schema.type_field("Query", "__search").is_ok());
        assert!(schema.type_field("Query", "__metaGraph").is_ok());
    }

    // spec: GQL-004
    proptest! {
        #[test]
        fn prop_any_double_underscore_branch_field_or_type_is_rejected(
            suffix in "[A-Za-z0-9_]{1,24}"
        ) {
            let type_sdl = format!("type __{suffix} {{ id: ID! }}");
            let type_err = with_gap_builtins(&type_sdl).unwrap_err();
            prop_assert_eq!(type_err.code(), ErrorCode::ReservedName);

            let field_sdl = format!("type SafeType {{ __{suffix}: String }}");
            let field_err = with_gap_builtins(&field_sdl).unwrap_err();
            prop_assert_eq!(field_err.code(), ErrorCode::ReservedName);
        }
    }
}
