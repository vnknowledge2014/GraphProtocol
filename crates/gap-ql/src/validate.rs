//! Strict GraphQL executable document parsing, validation, variable coercion, `@oneOf` and
//! `GQL-043` checks, and `GapError` diagnostic mapping (`spec/02-gap-ql.md` §§1, 8–10).

use std::collections::HashSet;

use apollo_compiler::ast::{self, Definition, Selection, Type, Value};
use apollo_compiler::executable::Operation;
use apollo_compiler::parser::{SourceMap, SourceSpan};
use apollo_compiler::request::coerce_variable_values;
use apollo_compiler::response::{JsonMap, JsonValue, ResponseDataPathSegment};
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::validation::Valid;
use apollo_compiler::{ExecutableDocument, Name, Schema};
use gap_core::error::{ErrorCode, GapError, GapResult, PathSegment, SourceLocation};
use gap_core::ids::STANDARD_INTROSPECTION_NAMES;

use crate::schema::{bounded_parser, span_to_source_location};

/// Validated GraphQL executable document paired with the selected operation and coerced variables.
#[derive(Debug, Clone)]
pub struct ValidatedOperation {
    /// Conforming `apollo-compiler` validated executable document (`GQL-002`).
    pub document: Valid<ExecutableDocument>,
    /// Selected operation name, or `None` when executing the sole anonymous operation.
    pub operation_name: Option<String>,
    /// Coerced GraphQL variable map validated against the operation's variable definitions.
    pub coerced_variables: Valid<JsonMap>,
}

impl ValidatedOperation {
    /// Borrows the selected [`Operation`] from [`Self::document`].
    #[must_use]
    pub fn operation(&self) -> Option<&Operation> {
        self.document
            .operations
            .get(self.operation_name.as_deref())
            .ok()
            .map(|op| &**op)
    }
}

fn attach_span(err: GapError, span: Option<SourceSpan>, sources: &SourceMap) -> GapError {
    if let Some(loc) = span_to_source_location(span, sources) {
        err.with_locations(vec![loc])
    } else {
        err
    }
}

/// Converts a `serde_json::Value` into `apollo-compiler`'s [`JsonValue`].
#[must_use]
pub fn serde_to_apollo_json_value(value: &serde_json::Value) -> JsonValue {
    match value {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                JsonValue::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                JsonValue::from(f)
            } else {
                JsonValue::Null
            }
        }
        serde_json::Value::String(s) => JsonValue::String(s.as_str().into()),
        serde_json::Value::Array(items) => {
            JsonValue::Array(items.iter().map(serde_to_apollo_json_value).collect())
        }
        serde_json::Value::Object(map) => {
            let mut out = JsonMap::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.as_str(), serde_to_apollo_json_value(v));
            }
            JsonValue::Object(out)
        }
    }
}

/// Converts a `serde_json::Map` of GraphQL variables into `apollo-compiler`'s [`JsonMap`].
#[must_use]
pub fn serde_to_apollo_json_map(variables: &serde_json::Map<String, serde_json::Value>) -> JsonMap {
    let mut out = JsonMap::with_capacity(variables.len());
    for (k, v) in variables {
        out.insert(k.as_str(), serde_to_apollo_json_value(v));
    }
    out
}

/// Parses an executable GraphQL query string into an [`ast::Document`], returning all syntax
/// diagnostics as [`ErrorCode::ParseError`] [`GapError`]s (`GQL-001`, `GQL-002`).
pub fn parse_executable_ast(
    query: &str,
    source_name: &str,
) -> Result<ast::Document, Vec<GapError>> {
    bounded_parser()
        .parse_ast(query, source_name)
        .map_err(|with_errs| {
            let mut out = Vec::new();
            for diag in with_errs.errors.iter() {
                out.push(graphql_error_to_gap(ErrorCode::ParseError, diag.to_json()));
            }
            if out.is_empty() {
                out.push(GapError::new(
                    ErrorCode::ParseError,
                    "invalid GraphQL syntax",
                ));
            }
            out
        })
}

pub(crate) fn graphql_error_to_gap(
    code: ErrorCode,
    gql_err: apollo_compiler::response::GraphQLError,
) -> GapError {
    let mut err = GapError::new(code, gql_err.message);
    if !gql_err.locations.is_empty() {
        let locs = gql_err
            .locations
            .into_iter()
            .map(|lc| SourceLocation {
                line: u32::try_from(lc.line).unwrap_or(u32::MAX),
                column: u32::try_from(lc.column).unwrap_or(u32::MAX),
            })
            .collect();
        err = err.with_locations(locs);
    }
    if !gql_err.path.is_empty() {
        let path = gql_err
            .path
            .into_iter()
            .map(|seg| match seg {
                ResponseDataPathSegment::Field(name) => PathSegment::Field(name.to_string()),
                ResponseDataPathSegment::ListIndex(idx) => PathSegment::Index(idx),
            })
            .collect();
        err = err.with_path(path);
    }
    err
}

fn collect_non_empty_selection_field_spans(
    selections: &[Selection],
    spans: &mut HashSet<(usize, usize)>,
) {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                if !field.selection_set.is_empty()
                    && let Some(loc) = field.location()
                {
                    spans.insert((loc.offset(), loc.end_offset()));
                }
                collect_non_empty_selection_field_spans(&field.selection_set, spans);
            }
            Selection::InlineFragment(inline) => {
                collect_non_empty_selection_field_spans(&inline.selection_set, spans);
            }
            Selection::FragmentSpread(_) => {}
        }
    }
}

fn non_empty_ast_field_spans(ast_doc: &ast::Document) -> HashSet<(usize, usize)> {
    let mut spans = HashSet::new();
    for def in &ast_doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                collect_non_empty_selection_field_spans(&op.selection_set, &mut spans);
            }
            Definition::FragmentDefinition(frag) => {
                collect_non_empty_selection_field_spans(&frag.selection_set, &mut spans);
            }
            _ => {}
        }
    }
    spans
}

fn classify_diagnostic(
    diag: &apollo_compiler::diagnostic::Diagnostic<'_, apollo_compiler::validation::DiagnosticData>,
    message: &str,
) -> ErrorCode {
    if let Some(kind) = diag.error.unstable_error_name() {
        return match kind {
            "UndefinedField" | "UndefinedInputValue" => ErrorCode::UnknownField,
            "UndefinedArgument" => ErrorCode::UnknownArgument,
            "RequiredArgument" | "RequiredField" => ErrorCode::MissingArgument,
            "UnsupportedValueType"
            | "UndefinedEnumValue"
            | "IntCoercionError"
            | "FloatCoercionError"
            | "DisallowedVariableUsage" => ErrorCode::TypeMismatch,
            "ReservedName" => ErrorCode::ReservedName,
            _ => ErrorCode::ValidationError,
        };
    }
    classify_validation_message(message)
}
/// Classifies an `apollo-compiler` validation message into the specific [`ErrorCode`] family
/// defined in `spec/02-gap-ql.md` §10.
#[must_use]
pub fn classify_validation_message(message: &str) -> ErrorCode {
    if (message.starts_with("type `") && message.contains("` does not have a field `"))
        || (message.starts_with("field `") && message.contains("` does not exist on `"))
    {
        ErrorCode::UnknownField
    } else if message.starts_with("the argument `") && message.contains("` is not supported by `") {
        ErrorCode::UnknownArgument
    } else if message.starts_with("the required argument `")
        || message.starts_with("the required field `")
    {
        ErrorCode::MissingArgument
    } else if message.starts_with("expected value of type ")
        || (message.starts_with("value `") && message.contains("` does not exist on `"))
        || message.starts_with("int cannot represent non 32-bit")
        || message.starts_with("float cannot represent non-finite")
        || (message.starts_with("variable `$") && message.contains("cannot be used for argument"))
    {
        ErrorCode::TypeMismatch
    } else {
        ErrorCode::ValidationError
    }
}

fn check_exec_ident_reserved(
    name: &Name,
    allow_introspection: bool,
    what: &str,
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    let raw = name.as_str();
    if raw.starts_with("__")
        && (!allow_introspection || !STANDARD_INTROSPECTION_NAMES.contains(&raw))
    {
        errors.push(attach_span(
            GapError::new(
                ErrorCode::ReservedName,
                format!(
                    "reserved {what} `{raw}`: identifiers starting with `__` are forbidden (GQL-004)"
                ),
            ),
            name.location(),
            sources,
        ));
    }
}

fn check_ast_value_reserved(value: &Value, sources: &SourceMap, errors: &mut Vec<GapError>) {
    match value {
        Value::Variable(var) => {
            check_exec_ident_reserved(var, false, "variable", sources, errors);
        }
        Value::List(items) => {
            for item in items {
                check_ast_value_reserved(item, sources, errors);
            }
        }
        Value::Object(entries) => {
            for (field_name, field_val) in entries {
                check_exec_ident_reserved(field_name, false, "input field", sources, errors);
                check_ast_value_reserved(field_val, sources, errors);
            }
        }
        Value::Null
        | Value::Enum(_)
        | Value::String(_)
        | Value::Float(_)
        | Value::Int(_)
        | Value::Boolean(_) => {}
    }
}

fn check_directives_reserved(
    directives: &ast::DirectiveList,
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    for dir in directives {
        check_exec_ident_reserved(&dir.name, false, "directive", sources, errors);
        for arg in &dir.arguments {
            check_exec_ident_reserved(&arg.name, false, "directive argument", sources, errors);
            check_ast_value_reserved(&arg.value, sources, errors);
        }
    }
}

fn check_selections_reserved(
    selections: &[Selection],
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                if let Some(alias) = &field.alias {
                    check_exec_ident_reserved(alias, false, "field alias", sources, errors);
                }
                check_exec_ident_reserved(&field.name, true, "field", sources, errors);
                for arg in &field.arguments {
                    check_exec_ident_reserved(&arg.name, false, "argument", sources, errors);
                    check_ast_value_reserved(&arg.value, sources, errors);
                }
                check_directives_reserved(&field.directives, sources, errors);
                check_selections_reserved(&field.selection_set, sources, errors);
            }
            Selection::FragmentSpread(spread) => {
                check_exec_ident_reserved(
                    &spread.fragment_name,
                    false,
                    "fragment spread",
                    sources,
                    errors,
                );
                check_directives_reserved(&spread.directives, sources, errors);
            }
            Selection::InlineFragment(inline) => {
                if let Some(cond) = &inline.type_condition {
                    check_exec_ident_reserved(cond, true, "type condition", sources, errors);
                }
                check_directives_reserved(&inline.directives, sources, errors);
                check_selections_reserved(&inline.selection_set, sources, errors);
            }
        }
    }
}

fn check_unique_input_fields_in_value(
    value: &Value,
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    match value {
        Value::List(items) => {
            for item in items {
                check_unique_input_fields_in_value(item, sources, errors);
            }
        }
        Value::Object(entries) => {
            let mut seen: Vec<&str> = Vec::with_capacity(entries.len());
            for (field_name, field_val) in entries {
                if seen.contains(&field_name.as_str()) {
                    errors.push(attach_span(
                        GapError::new(
                            ErrorCode::ValidationError,
                            format!(
                                "the input object has multiple fields named `{}`",
                                field_name.as_str()
                            ),
                        ),
                        field_name.location(),
                        sources,
                    ));
                } else {
                    seen.push(field_name.as_str());
                }
                check_unique_input_fields_in_value(field_val, sources, errors);
            }
        }
        Value::Null
        | Value::Enum(_)
        | Value::Variable(_)
        | Value::String(_)
        | Value::Float(_)
        | Value::Int(_)
        | Value::Boolean(_) => {}
    }
}

fn check_unique_input_fields_in_directives(
    directives: &ast::DirectiveList,
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    for dir in directives {
        for arg in &dir.arguments {
            check_unique_input_fields_in_value(&arg.value, sources, errors);
        }
    }
}

fn check_unique_input_fields_in_selections(
    selections: &[Selection],
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    for sel in selections {
        match sel {
            Selection::Field(field) => {
                for arg in &field.arguments {
                    check_unique_input_fields_in_value(&arg.value, sources, errors);
                }
                check_unique_input_fields_in_directives(&field.directives, sources, errors);
                check_unique_input_fields_in_selections(&field.selection_set, sources, errors);
            }
            Selection::FragmentSpread(spread) => {
                check_unique_input_fields_in_directives(&spread.directives, sources, errors);
            }
            Selection::InlineFragment(inline) => {
                check_unique_input_fields_in_directives(&inline.directives, sources, errors);
                check_unique_input_fields_in_selections(&inline.selection_set, sources, errors);
            }
        }
    }
}

/// Enforces the GraphQL `UniqueInputFieldNames` rule (`GQL-002`): an input object literal must not name
/// a field twice. `apollo-compiler` does not implement this rule, graphql-js and the specification do
/// (found by the differential corpus, `tests/differential`).
///
/// # Errors
///
/// Returns one [`ErrorCode::ValidationError`] per duplicated field.
pub fn validate_unique_input_object_fields(ast_doc: &ast::Document) -> Result<(), Vec<GapError>> {
    let mut errors = Vec::new();
    for def in &ast_doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                for var in &op.variables {
                    if let Some(default_val) = &var.default_value {
                        check_unique_input_fields_in_value(
                            default_val,
                            &ast_doc.sources,
                            &mut errors,
                        );
                    }
                    check_unique_input_fields_in_directives(
                        &var.directives,
                        &ast_doc.sources,
                        &mut errors,
                    );
                }
                check_unique_input_fields_in_directives(
                    &op.directives,
                    &ast_doc.sources,
                    &mut errors,
                );
                check_unique_input_fields_in_selections(
                    &op.selection_set,
                    &ast_doc.sources,
                    &mut errors,
                );
            }
            Definition::FragmentDefinition(frag) => {
                check_unique_input_fields_in_directives(
                    &frag.directives,
                    &ast_doc.sources,
                    &mut errors,
                );
                check_unique_input_fields_in_selections(
                    &frag.selection_set,
                    &ast_doc.sources,
                    &mut errors,
                );
            }
            _ => {}
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Enforces `GQL-004` on an executable AST document, rejecting non-introspection `__`-prefixed
/// names in operations, fragments, variables, fields, aliases, arguments, and directives.
pub fn validate_executable_reserved_names(ast_doc: &ast::Document) -> Result<(), Vec<GapError>> {
    let mut errors = Vec::new();
    for def in &ast_doc.definitions {
        match def {
            Definition::OperationDefinition(op) => {
                if let Some(op_name) = &op.name {
                    check_exec_ident_reserved(
                        op_name,
                        false,
                        "operation name",
                        &ast_doc.sources,
                        &mut errors,
                    );
                }
                for var in &op.variables {
                    check_exec_ident_reserved(
                        &var.name,
                        false,
                        "variable",
                        &ast_doc.sources,
                        &mut errors,
                    );
                    check_exec_ident_reserved(
                        var.ty.inner_named_type(),
                        true,
                        "variable type",
                        &ast_doc.sources,
                        &mut errors,
                    );
                    if let Some(default_val) = &var.default_value {
                        check_ast_value_reserved(default_val, &ast_doc.sources, &mut errors);
                    }
                    check_directives_reserved(&var.directives, &ast_doc.sources, &mut errors);
                }
                check_directives_reserved(&op.directives, &ast_doc.sources, &mut errors);
                check_selections_reserved(&op.selection_set, &ast_doc.sources, &mut errors);
            }
            Definition::FragmentDefinition(frag) => {
                check_exec_ident_reserved(
                    &frag.name,
                    false,
                    "fragment name",
                    &ast_doc.sources,
                    &mut errors,
                );
                check_exec_ident_reserved(
                    &frag.type_condition,
                    true,
                    "type condition",
                    &ast_doc.sources,
                    &mut errors,
                );
                check_directives_reserved(&frag.directives, &ast_doc.sources, &mut errors);
                check_selections_reserved(&frag.selection_set, &ast_doc.sources, &mut errors);
            }
            _ => {}
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_json_input_rules(
    schema: &Schema,
    expected_type: &Type,
    json_val: &JsonValue,
    span: Option<SourceSpan>,
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    if json_val.is_null() {
        return;
    }
    match expected_type {
        Type::List(inner) | Type::NonNullList(inner) => {
            if let Some(arr) = json_val.as_array() {
                for item in arr {
                    validate_json_input_rules(schema, inner, item, span, sources, errors);
                }
            } else {
                validate_json_input_rules(schema, inner, json_val, span, sources, errors);
            }
        }
        Type::Named(type_name) | Type::NonNullNamed(type_name) => {
            let Some(ExtendedType::InputObject(input_obj)) = schema.types.get(type_name) else {
                return;
            };
            let Some(obj_map) = json_val.as_object() else {
                return;
            };

            if input_obj.directives.has("oneOf") {
                if obj_map.len() != 1 {
                    errors.push(attach_span(
                        GapError::new(
                            ErrorCode::TypeMismatch,
                            format!(
                                "@oneOf input object `{type_name}` requires exactly 1 field, got {}",
                                obj_map.len()
                            ),
                        ),
                        span,
                        sources,
                    ));
                } else if let Some((field_key, field_val)) = obj_map.iter().next()
                    && field_val.is_null()
                {
                    errors.push(attach_span(
                        GapError::new(
                            ErrorCode::TypeMismatch,
                            format!(
                                "@oneOf input object `{type_name}` field `{}` must not be null",
                                field_key.as_str()
                            ),
                        ),
                        span,
                        sources,
                    ));
                }
            }

            if type_name.as_str() == "TraverseInput"
                && let Some(scopes_val) = obj_map.get("scopes")
                && let Some(arr) = scopes_val.as_array()
                && arr.is_empty()
            {
                errors.push(attach_span(
                    GapError::new(
                        ErrorCode::ValidationError,
                        "TraverseInput.scopes must not be empty; omit `scopes` to query all visible scopes (GQL-043)",
                    ),
                    span,
                    sources,
                ));
            }

            for (field_key, field_val) in obj_map {
                if let Some(field_def) = input_obj.fields.get(field_key.as_str()) {
                    validate_json_input_rules(
                        schema,
                        &field_def.ty,
                        field_val,
                        span,
                        sources,
                        errors,
                    );
                }
            }
        }
    }
}

fn validate_ast_input_rules(
    schema: &Schema,
    expected_type: &Type,
    ast_val: &apollo_compiler::Node<Value>,
    coerced_variables: &JsonMap,
    sources: &SourceMap,
    errors: &mut Vec<GapError>,
) {
    match &**ast_val {
        Value::Null => {}
        Value::Variable(var_name) => {
            if let Some(json_val) = coerced_variables.get(var_name.as_str()) {
                validate_json_input_rules(
                    schema,
                    expected_type,
                    json_val,
                    ast_val.location(),
                    sources,
                    errors,
                );
            }
        }
        Value::List(items) => match expected_type {
            Type::List(inner) | Type::NonNullList(inner) => {
                for item in items {
                    validate_ast_input_rules(
                        schema,
                        inner,
                        item,
                        coerced_variables,
                        sources,
                        errors,
                    );
                }
            }
            _ => {}
        },
        Value::Object(entries) => match expected_type {
            Type::List(inner) | Type::NonNullList(inner) => {
                validate_ast_input_rules(
                    schema,
                    inner,
                    ast_val,
                    coerced_variables,
                    sources,
                    errors,
                );
            }
            Type::Named(type_name) | Type::NonNullNamed(type_name) => {
                let Some(ExtendedType::InputObject(input_obj)) = schema.types.get(type_name) else {
                    return;
                };

                if input_obj.directives.has("oneOf") {
                    if entries.len() != 1 {
                        errors.push(attach_span(
                            GapError::new(
                                ErrorCode::ValidationError,
                                format!(
                                    "@oneOf input object `{type_name}` must specify exactly 1 field, got {}",
                                    entries.len()
                                ),
                            ),
                            ast_val.location(),
                            sources,
                        ));
                    } else {
                        let (field_name, field_val) = &entries[0];
                        let resolves_to_null = match &**field_val {
                            Value::Null => true,
                            Value::Variable(var_name) => coerced_variables
                                .get(var_name.as_str())
                                .is_none_or(JsonValue::is_null),
                            _ => false,
                        };
                        if resolves_to_null {
                            errors.push(attach_span(
                                GapError::new(
                                    ErrorCode::ValidationError,
                                    format!(
                                        "@oneOf input object `{type_name}` field `{field_name}` must be non-null"
                                    ),
                                ),
                                field_val.location(),
                                sources,
                            ));
                        }
                    }
                }

                if type_name.as_str() == "TraverseInput" {
                    for (field_name, field_val) in entries {
                        if field_name.as_str() == "scopes" {
                            let is_empty_list = match &**field_val {
                                Value::List(list) => list.is_empty(),
                                Value::Variable(var_name) => coerced_variables
                                    .get(var_name.as_str())
                                    .and_then(JsonValue::as_array)
                                    .is_some_and(Vec::is_empty),
                                _ => false,
                            };
                            if is_empty_list {
                                errors.push(attach_span(
                                    GapError::new(
                                        ErrorCode::ValidationError,
                                        "TraverseInput.scopes must not be empty; omit `scopes` to query all visible scopes (GQL-043)",
                                    ),
                                    field_val.location(),
                                    sources,
                                ));
                            }
                        }
                    }
                }

                for (field_name, field_val) in entries {
                    if let Some(field_def) = input_obj.fields.get(field_name) {
                        validate_ast_input_rules(
                            schema,
                            &field_def.ty,
                            field_val,
                            coerced_variables,
                            sources,
                            errors,
                        );
                    }
                }
            }
        },
        Value::Enum(_) | Value::String(_) | Value::Float(_) | Value::Int(_) | Value::Boolean(_) => {
            if let Type::List(inner) | Type::NonNullList(inner) = expected_type {
                validate_ast_input_rules(
                    schema,
                    inner,
                    ast_val,
                    coerced_variables,
                    sources,
                    errors,
                );
            }
        }
    }
}

fn validate_selection_set_input_rules(
    schema: &Schema,
    doc: &ExecutableDocument,
    selection_set: &apollo_compiler::executable::SelectionSet,
    coerced_variables: &JsonMap,
    visited_fragments: &mut HashSet<Name>,
    errors: &mut Vec<GapError>,
) {
    for sel in &selection_set.selections {
        match sel {
            apollo_compiler::executable::Selection::Field(field) => {
                for arg in &field.arguments {
                    if let Some(arg_def) = field
                        .definition
                        .arguments
                        .iter()
                        .find(|d| d.name == arg.name)
                    {
                        validate_ast_input_rules(
                            schema,
                            &arg_def.ty,
                            &arg.value,
                            coerced_variables,
                            &doc.sources,
                            errors,
                        );
                    }
                }
                for dir in &field.directives {
                    if let Some(dir_def) = schema.directive_definitions.get(&dir.name) {
                        for arg in &dir.arguments {
                            if let Some(arg_def) =
                                dir_def.arguments.iter().find(|d| d.name == arg.name)
                            {
                                validate_ast_input_rules(
                                    schema,
                                    &arg_def.ty,
                                    &arg.value,
                                    coerced_variables,
                                    &doc.sources,
                                    errors,
                                );
                            }
                        }
                    }
                }
                validate_selection_set_input_rules(
                    schema,
                    doc,
                    &field.selection_set,
                    coerced_variables,
                    visited_fragments,
                    errors,
                );
            }
            apollo_compiler::executable::Selection::InlineFragment(inline) => {
                validate_selection_set_input_rules(
                    schema,
                    doc,
                    &inline.selection_set,
                    coerced_variables,
                    visited_fragments,
                    errors,
                );
            }
            apollo_compiler::executable::Selection::FragmentSpread(spread) => {
                if visited_fragments.insert(spread.fragment_name.clone())
                    && let Some(frag) = doc.fragments.get(&spread.fragment_name)
                {
                    validate_selection_set_input_rules(
                        schema,
                        doc,
                        &frag.selection_set,
                        coerced_variables,
                        visited_fragments,
                        errors,
                    );
                }
            }
        }
    }
}

pub(crate) fn validate_against_projected_schema(
    projected_schema: &Valid<Schema>,
    query: &str,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
    visibility: &dyn crate::visibility::Visibility,
) -> Result<ValidatedOperation, Vec<GapError>> {
    let ast_doc = parse_executable_ast(query, "request.graphql")?;
    validate_executable_reserved_names(&ast_doc)?;
    crate::directives::check_executable_incremental_directives(&ast_doc, false)
        .map_err(|err| vec![err])?;
    validate_parsed_ast_against_projected_schema(
        projected_schema,
        &ast_doc,
        operation_name,
        variables,
        visibility,
    )
}

pub(crate) fn validate_parsed_ast_against_projected_schema(
    projected_schema: &Valid<Schema>,
    ast_doc: &ast::Document,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
    visibility: &dyn crate::visibility::Visibility,
) -> Result<ValidatedOperation, Vec<GapError>> {
    validate_unique_input_object_fields(ast_doc)?;
    let non_empty_spans = non_empty_ast_field_spans(ast_doc);
    let valid_doc = ast_doc
        .to_executable_validate(projected_schema)
        .map_err(|with_errs| {
            let mut errs = Vec::new();
            for diag in with_errs.errors.iter() {
                if diag.error.unstable_error_name() == Some("MissingSubselection")
                    && let Some(loc) =
                        apollo_compiler::diagnostic::ToCliReport::location(diag.error)
                    && non_empty_spans.contains(&(loc.offset(), loc.end_offset()))
                {
                    // Suppress cascading MissingSubselection when the AST field actually had a
                    // selection set whose children were dropped due to UndefinedField.
                    continue;
                }
                let json_err = diag.to_json();
                let code = classify_diagnostic(&diag, &json_err.message);
                let gap_err = graphql_error_to_gap(code, json_err);
                errs.push(crate::suggest::attach_validation_suggestion(
                    gap_err,
                    projected_schema,
                    Some(ast_doc),
                    visibility,
                ));
            }
            if errs.is_empty() {
                for diag in with_errs.errors.iter() {
                    let json_err = diag.to_json();
                    let code = classify_diagnostic(&diag, &json_err.message);
                    let gap_err = graphql_error_to_gap(code, json_err);
                    errs.push(crate::suggest::attach_validation_suggestion(
                        gap_err,
                        projected_schema,
                        Some(ast_doc),
                        visibility,
                    ));
                }
            }
            if errs.is_empty() {
                errs.push(GapError::new(
                    ErrorCode::ValidationError,
                    "GraphQL document validation failed",
                ));
            }
            errs
        })?;

    let op = valid_doc.operations.get(operation_name).map_err(|op_err| {
        let gql_err = op_err.to_graphql_error(&valid_doc.sources);
        vec![graphql_error_to_gap(ErrorCode::ValidationError, gql_err)]
    })?;
    let selected_op_name = op.name.as_ref().map(|n| n.as_str().to_owned());

    let raw_vars = serde_to_apollo_json_map(variables);
    let coerced_variables =
        coerce_variable_values(projected_schema, op, &raw_vars).map_err(|req_err| {
            let gql_err = req_err.to_graphql_error(&valid_doc.sources);
            vec![graphql_error_to_gap(ErrorCode::TypeMismatch, gql_err)]
        })?;

    let mut semantic_errors = Vec::new();
    for var_def in &op.variables {
        if let Some(json_val) = coerced_variables.get(var_def.name.as_str()) {
            validate_json_input_rules(
                projected_schema,
                &var_def.ty,
                json_val,
                var_def.location(),
                &valid_doc.sources,
                &mut semantic_errors,
            );
        }
    }
    let mut visited_fragments = HashSet::new();
    validate_selection_set_input_rules(
        projected_schema,
        &valid_doc,
        &op.selection_set,
        &coerced_variables,
        &mut visited_fragments,
        &mut semantic_errors,
    );

    if !semantic_errors.is_empty() {
        return Err(semantic_errors);
    }

    Ok(ValidatedOperation {
        document: valid_doc,
        operation_name: selected_op_name,
        coerced_variables,
    })
}

/// Parses, projects `schema` through `visibility` (`GQL-062`), validates `query`, coerces
/// `variables`, enforces `@oneOf` and `GQL-043`, and attaches visibility-safe self-repair
/// suggestions (`GQL-070`, `GQL-072`).
pub fn validate_document_with_visibility(
    schema: &Valid<Schema>,
    query: &str,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
    visibility: &dyn crate::visibility::Visibility,
) -> Result<ValidatedOperation, Vec<GapError>> {
    let projected = crate::visibility::project_visible_schema(schema, visibility);
    validate_against_projected_schema(&projected, query, operation_name, variables, visibility)
}

/// Convenience wrapper validating an anonymous or single-operation query with no variables under
/// caller `visibility` (`GQL-062`, `GQL-070`).
pub fn validate_query_with_visibility(
    schema: &Valid<Schema>,
    query: &str,
    visibility: &dyn crate::visibility::Visibility,
) -> GapResult<ValidatedOperation> {
    let empty_vars = serde_json::Map::new();
    validate_document_with_visibility(schema, query, None, &empty_vars, visibility).map_err(
        |mut errs| {
            if errs.is_empty() {
                GapError::new(ErrorCode::ValidationError, "validation failed")
            } else {
                errs.swap_remove(0)
            }
        },
    )
}

/// Parses, validates, selects an operation, coerces variables, and enforces GAP-QL semantic
/// profile rules (`GQL-001`, `GQL-002`, `GQL-004`, `@oneOf`, `GQL-043`) against `schema` with
/// permissive visibility.
pub fn validate_document(
    schema: &Valid<Schema>,
    query: &str,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
) -> Result<ValidatedOperation, Vec<GapError>> {
    validate_document_with_visibility(
        schema,
        query,
        operation_name,
        variables,
        &crate::visibility::AllowAllVisibility::default(),
    )
}

/// Convenience wrapper validating an anonymous or single-operation query with no variables and
/// returning the first [`GapError`] on failure.
pub fn validate_query(schema: &Valid<Schema>, query: &str) -> GapResult<ValidatedOperation> {
    let empty_vars = serde_json::Map::new();
    validate_document(schema, query, None, &empty_vars).map_err(|mut errs| {
        if errs.is_empty() {
            GapError::new(ErrorCode::ValidationError, "validation failed")
        } else {
            errs.swap_remove(0)
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::schema::{builtin_schema, with_gap_builtins};

    fn test_supergraph() -> Valid<Schema> {
        with_gap_builtins(
            r#"
            type Query {
                billing: BillingQuery!
            }
            type BillingQuery {
                invoice(id: ID!, currency: String = "USD"): Invoice
            }
            type Invoice {
                id: ID!
                total: Float!
            }
            "#,
        )
        .expect("compose test supergraph")
    }

    // spec: GQL-002
    #[test]
    fn duplicate_input_object_fields_are_rejected_everywhere_a_value_can_appear() {
        let schema = test_supergraph();
        for doc in [
            // field argument
            r#"{ billing { invoice(id: "1") { id } } }"#.replace("id: \"1\"", "id: {a: 1, a: 2}"),
            // nested object inside a list inside an object
            r#"{ billing { invoice(id: [{x: {a: 1, a: 2}}]) { id } } }"#.to_owned(),
            // directive argument
            r#"{ billing @skip(if: {a: 1, a: 2}) { invoice(id: "1") { id } } }"#.to_owned(),
            // variable default value
            r#"query Q($v: String = {a: 1, a: 2}) { billing { invoice(id: "1") { id } } }"#
                .to_owned(),
            // inside a fragment definition and an inline fragment
            r#"{ ...F } fragment F on Query { billing { invoice(id: {a: 1, a: 2}) { id } } }"#
                .to_owned(),
            r#"{ ... on Query { billing { invoice(id: {a: 1, a: 2}) { id } } } }"#.to_owned(),
        ] {
            let ast_doc = parse_executable_ast(&doc, "t.graphql").expect("syntactically valid");
            let errors = validate_unique_input_object_fields(&ast_doc).expect_err(&doc);
            assert!(
                errors.iter().all(|e| e.code() == ErrorCode::ValidationError
                    && e.message().contains("multiple fields named `a`")),
                "{doc}: {errors:?}"
            );
        }
        // Distinct names (also repeated across different objects) are fine, and so is the full pipeline.
        let ok = r#"{ billing { invoice(id: {a: 1, b: {a: 2}}) { id } i2: invoice(id: {a: 3}) { id } } }"#;
        let ast_doc = parse_executable_ast(ok, "t.graphql").expect("parses");
        assert!(validate_unique_input_object_fields(&ast_doc).is_ok());
        let dup = r#"{ billing { invoice(id: {a: 1, a: 2}) { id } } }"#;
        let error =
            validate_query(&schema, dup).expect_err("duplicate field must fail the full pipeline");
        assert!(
            error.message().contains("multiple fields named `a`"),
            "{error:?}"
        );
    }

    // spec: GQL-001, GQL-002 (ADR-001)
    #[test]
    fn valid_gap_ql_queries_mutations_and_subscriptions_pass_validation() {
        let schema = test_supergraph();

        validate_query(
            &schema,
            r#"{
                gap {
                    resolve(intent: "fetch invoice total") {
                        outcome
                        router
                        tokensEstimate
                        candidates { groupId score skeleton }
                    }
                    traverse(input: {
                        from: { id: "node-1" }
                        maxDepth: 3
                        scopes: [PRIVATE, TASK]
                        where: { eq: { path: "status", value: "OPEN" } }
                    }) {
                        truncated
                        nodes { id label scope }
                    }
                }
                __search(query: "invoice", first: 3) {
                    abstained
                    hits { name score sdl }
                }
                __metaGraph {
                    generation
                    groupCount
                }
                billing {
                    invoice(id: "inv-1") { id total }
                }
            }"#,
        )
        .expect("valid GAP-QL query must pass");

        let mut vars = serde_json::Map::new();
        vars.insert(
            "taskId".to_owned(),
            json!("0192f000-0000-7000-8000-000000000001"),
        );
        validate_document(
            &schema,
            r#"subscription WatchTask($taskId: ID!) {
                gapTaskEvents(taskId: $taskId) {
                    seq
                    state
                }
            }"#,
            Some("WatchTask"),
            &vars,
        )
        .expect("valid subscription with variables must pass");
    }

    // spec: GQL-001, GQL-002, GQL-072 (ADR-023)
    #[test]
    fn syntax_and_validation_errors_map_to_expected_gap_error_codes_with_locations() {
        let schema = test_supergraph();

        let parse_err = validate_query(&schema, "{ billing { invoice(id: ")
            .expect_err("truncated query must fail");
        assert_eq!(parse_err.code(), ErrorCode::ParseError);
        assert!(parse_err.locations.is_some());
        assert!(parse_err.message().chars().count() <= gap_core::error::MAX_ERROR_MESSAGE_CHARS);

        let unknown_field = validate_query(&schema, "{ billing { invoce(id: \"1\") { total } } }")
            .expect_err("unknown field must fail");
        assert_eq!(unknown_field.code(), ErrorCode::UnknownField);
        assert!(unknown_field.locations.is_some());

        let unknown_arg = validate_query(
            &schema,
            "{ billing { invoice(id: \"1\", bogus: true) { total } } }",
        )
        .expect_err("unknown arg must fail");
        assert_eq!(unknown_arg.code(), ErrorCode::UnknownArgument);

        let missing_arg = validate_query(&schema, "{ billing { invoice { total } } }")
            .expect_err("missing required arg must fail");
        assert_eq!(missing_arg.code(), ErrorCode::MissingArgument);

        let missing_subselection = validate_query(&schema, "{ billing }")
            .expect_err("genuine missing subselection on object field must fail");
        assert_eq!(missing_subselection.code(), ErrorCode::ValidationError);

        let type_mismatch = validate_query(
            &schema,
            "{ gap { traverse(input: { from: { id: \"1\" }, maxDepth: \"notAnInt\" }) { truncated } } }",
        )
        .expect_err("type mismatch must fail");
        assert_eq!(type_mismatch.code(), ErrorCode::TypeMismatch);

        let unused_var = validate_query(
            &schema,
            "query($unused: Int) { __metaGraph { generation } }",
        )
        .expect_err("unused variable must fail");
        assert_eq!(unused_var.code(), ErrorCode::ValidationError);

        let mut bad_vars = serde_json::Map::new();
        bad_vars.insert("id".to_owned(), serde_json::Value::Null);
        let var_err = validate_document(
            &schema,
            "query($id: ID!) { billing { invoice(id: $id) { total } } }",
            None,
            &bad_vars,
        )
        .expect_err("null value for non-null variable must fail");
        assert_eq!(var_err[0].code(), ErrorCode::TypeMismatch);
    }

    // spec: GQL-001, GQL-002
    #[test]
    fn oneof_input_objects_reject_zero_multiple_or_null_fields() {
        let schema = builtin_schema().expect("builtin schema");

        let zero_fields = validate_query(&schema, "{ gap { node(ref: {}) { id } } }")
            .expect_err("empty @oneOf NodeRef must fail");
        assert_eq!(zero_fields.code(), ErrorCode::ValidationError);

        let two_fields = validate_query(
            &schema,
            r#"{ gap { node(ref: { id: "1", key: { type: "User", value: "u1" } }) { id } } }"#,
        )
        .expect_err("two-field @oneOf NodeRef must fail");
        assert_eq!(two_fields.code(), ErrorCode::ValidationError);

        let null_field = validate_query(&schema, "{ gap { node(ref: { id: null }) { id } } }")
            .expect_err("null @oneOf field must fail");
        assert_eq!(null_field.code(), ErrorCode::ValidationError);

        let mut bad_vars = serde_json::Map::new();
        bad_vars.insert(
            "ref".to_owned(),
            json!({ "id": "1", "key": { "type": "User", "value": "u1" } }),
        );
        let var_two_fields = validate_document(
            &schema,
            "query($ref: NodeRef!) { gap { node(ref: $ref) { id } } }",
            None,
            &bad_vars,
        )
        .expect_err("variable @oneOf with 2 fields must fail");
        assert_eq!(var_two_fields[0].code(), ErrorCode::TypeMismatch);
    }

    // spec: GQL-043 (ADR-025)
    #[test]
    fn traverse_input_empty_scopes_is_rejected_while_omitted_is_allowed() {
        let schema = builtin_schema().expect("builtin schema");

        let empty_literal = validate_query(
            &schema,
            r#"{ gap { traverse(input: { from: { id: "n1" }, maxDepth: 2, scopes: [] }) { truncated } } }"#,
        )
        .expect_err("empty scopes literal must fail with GAP_VALIDATION_ERROR");
        assert_eq!(empty_literal.code(), ErrorCode::ValidationError);

        let mut vars = serde_json::Map::new();
        vars.insert(
            "in".to_owned(),
            json!({ "from": { "id": "n1" }, "maxDepth": 2, "scopes": [] }),
        );
        let empty_var = validate_document(
            &schema,
            "query($in: TraverseInput!) { gap { traverse(input: $in) { truncated } } }",
            None,
            &vars,
        )
        .expect_err("empty scopes in variable must fail");
        assert_eq!(empty_var[0].code(), ErrorCode::ValidationError);

        validate_query(
            &schema,
            r#"{ gap { traverse(input: { from: { id: "n1" }, maxDepth: 2 }) { truncated } } }"#,
        )
        .expect("omitted scopes means all visible scopes");
    }

    // spec: GQL-004
    #[test]
    fn executable_document_rejects_non_introspection_double_underscore_identifiers() {
        let schema = test_supergraph();
        let adversarial_queries = [
            "{ __customRoot }",
            "{ __badAlias: __metaGraph { generation } }",
            "{ gap { resolve(intent: \"hi\", __secret: 1) { router } } }",
            "query __EvilOp { __metaGraph { generation } }",
            "query($__evilVar: ID!) { billing { invoice(id: $__evilVar) { total } } }",
            "fragment __EvilFrag on Invoice { total } { billing { invoice(id: \"1\") { ...__EvilFrag } } }",
        ];

        for query in adversarial_queries {
            let err = validate_query(&schema, query).expect_err("reserved __ name must fail");
            assert_eq!(
                err.code(),
                ErrorCode::ReservedName,
                "query `{query}` returned unexpected code: {err:?}"
            );
        }
    }
}
