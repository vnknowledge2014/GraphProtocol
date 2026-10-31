//! JSON Schema 2020-12 and GAP-QL SDL generator (`NGC-010`, `NGC-012`, `NGC-013`, `GQL-004`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gap_core::ids::{is_allowed_graphql_identifier, is_reserved_builtin_type};
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_proto::ngc::v1::{OpKind, OperationDescriptor, PaginationKind, SideEffectHint};
use serde_json::{Map, Value, json};

use crate::document::{ExtractError, OpenApiDocument};
use crate::extract::{ExtractedOperation, Extraction, ParamPrimitive, ParamShape};

/// JSON Schema 2020-12 dialect URI (`NGC-013`).
pub const JSON_SCHEMA_DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Maximum enum cardinality lifted into a dedicated GraphQL `enum` definition; larger enums stay `String` in SDL.
const MAX_SDL_ENUM_VARIANTS: usize = 256;

/// GraphQL built-in scalar and root names that a generated branch type must never shadow (`GQL-004`).
const BUILTIN_SDL_NAMES: &[&str] = &[
    "Query",
    "Mutation",
    "Subscription",
    "String",
    "Int",
    "Float",
    "Boolean",
    "ID",
];

/// Generated JSON Schema + wire descriptor for one extracted operation (`NGC-012`, `NGC-013`).
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedOperationSchema {
    /// NGC wire `OperationDescriptor`.
    pub descriptor: OperationDescriptor,
    /// Raw JSON Schema 2020-12 object for input validation (`request` module).
    pub input_json_schema: Value,
    /// Raw JSON Schema 2020-12 object for output documentation/validation.
    pub output_json_schema: Value,
}

/// Complete schema bundle derived from an [`OpenApiDocument`] and [`Extraction`].
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaBundle {
    /// Hex `blake3` digest of the source OpenAPI document bytes (`NGC-010`).
    pub spec_hash: String,
    /// Branch-local GraphQL SDL (`NGC-012`, `GQL-004`).
    pub schema_sdl: String,
    /// Generated per-operation descriptors and JSON schemas keyed by operation name.
    pub operations: BTreeMap<String, GeneratedOperationSchema>,
    /// Ordered descriptors matching `Extraction::operations`.
    pub descriptors: Vec<OperationDescriptor>,
}

fn to_pascal_case(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut upper_next = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            if upper_next {
                out.push(ch.to_ascii_uppercase());
                upper_next = false;
            } else {
                out.push(ch);
            }
        } else {
            upper_next = true;
        }
    }
    out
}

fn is_forbidden_sdl_type_name(name: &str) -> bool {
    name.is_empty()
        || !is_allowed_graphql_identifier(name)
        || name.starts_with("__")
        || is_reserved_builtin_type(name)
        || BUILTIN_SDL_NAMES.contains(&name)
}

fn sanitize_field_name(raw: &str, used: &mut BTreeSet<String>) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    let mut prev_u = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            cleaned.push(ch);
            prev_u = false;
        } else if !prev_u {
            cleaned.push('_');
            prev_u = true;
        }
    }
    let trimmed = cleaned.trim_matches('_');
    let base = if trimmed.is_empty() {
        "field".to_owned()
    } else if raw.starts_with("__") || trimmed.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
        format!("f_{trimmed}")
    } else {
        trimmed.to_owned()
    };
    if used.insert(base.clone()) {
        return base;
    }
    let mut idx = 2_usize;
    loop {
        let cand = format!("{base}_{idx}");
        if used.insert(cand.clone()) {
            return cand;
        }
        idx = idx.saturating_add(1);
    }
}

fn is_valid_graphql_enum_value(v: &str) -> bool {
    is_allowed_graphql_identifier(v)
        && !v.starts_with("__")
        && !matches!(v, "true" | "false" | "null")
}

#[derive(Default)]
struct SdlBuilder {
    /// Type name -> rendered SDL definition (`type Foo { ... }` or `enum Bar { ... }`).
    definitions: BTreeMap<String, String>,
    /// Canonical body -> already-assigned type name so identical component schemas reuse one type.
    body_to_name: BTreeMap<String, String>,
    used_type_names: BTreeSet<String>,
}

impl SdlBuilder {
    fn allocate_type_name(&mut self, hint: &str) -> String {
        let pascal = to_pascal_case(hint);
        let base = if pascal.is_empty() {
            "OpenApiRecord".to_owned()
        } else if pascal.bytes().next().is_some_and(|b| b.is_ascii_digit())
            || is_forbidden_sdl_type_name(&pascal)
        {
            format!("OpenApi{pascal}")
        } else {
            pascal
        };
        if self.used_type_names.insert(base.clone()) {
            return base;
        }
        let mut idx = 2_usize;
        loop {
            let cand = format!("{base}_{idx}");
            if self.used_type_names.insert(cand.clone()) {
                return cand;
            }
            idx = idx.saturating_add(1);
        }
    }

    fn register_enum(&mut self, hint: &str, values: &[&str]) -> String {
        let mut dedup = Vec::with_capacity(values.len());
        let mut seen = BTreeSet::new();
        for &v in values {
            if seen.insert(v) {
                dedup.push(v);
            }
        }
        let canonical = format!("ENUM:{}", dedup.join("|"));
        if let Some(existing) = self.body_to_name.get(&canonical) {
            return existing.clone();
        }
        let name = self.allocate_type_name(hint);
        let body = format!("enum {name} {{\n  {}\n}}", dedup.join("\n  "));
        self.body_to_name.insert(canonical, name.clone());
        self.definitions.insert(name.clone(), body);
        name
    }

    fn sdl_type_for_schema(&mut self, hint: &str, schema: &Value, non_null: bool) -> String {
        let base = self.base_sdl_type(hint, schema);
        let nullable_in_schema = schema_allows_null(schema);
        if non_null && !nullable_in_schema {
            format!("{base}!")
        } else {
            base
        }
    }

    fn base_sdl_type(&mut self, hint: &str, schema: &Value) -> String {
        let Some(obj) = schema.as_object() else {
            return "JSON".to_owned();
        };
        if obj.contains_key("oneOf") || obj.contains_key("anyOf") {
            return "JSON".to_owned();
        }

        let type_str = match obj.get("type") {
            Some(Value::String(s)) => Some(s.as_str()),
            Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_str).find(|t| *t != "null"),
            _ => None,
        };

        match type_str {
            Some("integer") => "Int".to_owned(),
            Some("number") => "Float".to_owned(),
            Some("boolean") => "Boolean".to_owned(),
            Some("string") => {
                if let Some(enum_arr) = obj.get("enum").and_then(Value::as_array) {
                    let strs: Vec<&str> = enum_arr.iter().filter_map(Value::as_str).collect();
                    if !strs.is_empty()
                        && strs.len() == enum_arr.len()
                        && strs.len() <= MAX_SDL_ENUM_VARIANTS
                        && strs.iter().copied().all(is_valid_graphql_enum_value)
                    {
                        return self.register_enum(hint, &strs);
                    }
                }
                "String".to_owned()
            }
            Some("array") => {
                if let Some(items) = obj.get("items") {
                    let elem = self.sdl_type_for_schema(&format!("{hint}Item"), items, true);
                    format!("[{elem}]")
                } else {
                    "[JSON]".to_owned()
                }
            }
            Some("object") | None if obj.contains_key("properties") => {
                let Some(props) = obj.get("properties").and_then(Value::as_object) else {
                    return "JSON".to_owned();
                };
                if props.is_empty() {
                    return "JSON".to_owned();
                }
                let required_set: BTreeSet<&str> = obj
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();

                let mut used_fields = BTreeSet::new();
                let mut rendered_fields = Vec::with_capacity(props.len());
                for (prop_name, prop_schema) in props {
                    let field_name = sanitize_field_name(prop_name, &mut used_fields);
                    let child_hint = format!("{hint}_{}", to_pascal_case(prop_name));
                    let is_req = required_set.contains(prop_name.as_str());
                    let field_ty = self.sdl_type_for_schema(&child_hint, prop_schema, is_req);
                    rendered_fields.push(format!("{field_name}: {field_ty}"));
                }
                let canonical = format!("OBJ:{}", rendered_fields.join(";"));
                if let Some(existing) = self.body_to_name.get(&canonical) {
                    return existing.clone();
                }
                let name = self.allocate_type_name(hint);
                let body = format!("type {name} {{\n  {}\n}}", rendered_fields.join("\n  "));
                self.body_to_name.insert(canonical, name.clone());
                self.definitions.insert(name.clone(), body);
                name
            }
            _ => "JSON".to_owned(),
        }
    }
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return true;
    };
    if obj.get("nullable").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if let Some(Value::Array(arr)) = obj.get("type") {
        return arr.iter().any(|v| v.as_str() == Some("null"));
    }
    false
}

fn normalize_to_json_schema_2020_12(node: &Value) -> Value {
    match node {
        Value::Array(items) => {
            Value::Array(items.iter().map(normalize_to_json_schema_2020_12).collect())
        }
        Value::Object(map) => {
            let nullable = map.get("nullable").and_then(Value::as_bool) == Some(true);
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                if k == "nullable" || k.starts_with("x-") {
                    continue;
                }
                if k == "type"
                    && nullable
                    && let Value::String(t) = v
                {
                    out.insert("type".to_owned(), json!([t, "null"]));
                    continue;
                }
                out.insert(k.clone(), normalize_to_json_schema_2020_12(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn build_input_json_schema(op: &ExtractedOperation) -> Value {
    let mut props = Map::with_capacity(op.parameters.len());
    let mut required = Vec::new();
    for p in &op.parameters {
        props.insert(p.name.clone(), normalize_to_json_schema_2020_12(&p.schema));
        if p.required {
            required.push(Value::String(p.name.clone()));
        }
    }
    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        Value::String(JSON_SCHEMA_DRAFT_2020_12.to_owned()),
    );
    root.insert("type".to_owned(), Value::String("object".to_owned()));
    root.insert("additionalProperties".to_owned(), Value::Bool(false));
    root.insert("properties".to_owned(), Value::Object(props));
    if !required.is_empty() {
        root.insert("required".to_owned(), Value::Array(required));
    }
    Value::Object(root)
}

fn build_output_json_schema(op: &ExtractedOperation) -> Value {
    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        Value::String(JSON_SCHEMA_DRAFT_2020_12.to_owned()),
    );
    match &op.response_schema {
        Some(raw) => {
            let norm = normalize_to_json_schema_2020_12(raw);
            if let Value::Object(map) = norm {
                for (k, v) in map {
                    root.insert(k, v);
                }
            }
        }
        None => {
            root.insert("type".to_owned(), json!(["object", "array", "null"]));
        }
    }
    Value::Object(root)
}

fn sdl_primitive(p: ParamPrimitive) -> &'static str {
    match p {
        ParamPrimitive::String => "String",
        ParamPrimitive::Integer => "Int",
        ParamPrimitive::Number => "Float",
        ParamPrimitive::Boolean => "Boolean",
    }
}

fn build_arg_sdl_type(
    sdl: &mut SdlBuilder,
    op_name: &str,
    param: &crate::extract::ParameterSpec,
) -> String {
    let base = match param.shape {
        ParamShape::Scalar(ParamPrimitive::String) => sdl.base_sdl_type(
            &format!(
                "{}_{}",
                to_pascal_case(op_name),
                to_pascal_case(&param.name)
            ),
            &param.schema,
        ),
        ParamShape::Scalar(prim) => sdl_primitive(prim).to_owned(),
        ParamShape::Array(ParamPrimitive::String) => {
            if let Some(items) = param.schema.get("items") {
                let elem = sdl.base_sdl_type(
                    &format!(
                        "{}_{}",
                        to_pascal_case(op_name),
                        to_pascal_case(&param.name)
                    ),
                    items,
                );
                format!("[{elem}!]")
            } else {
                "[String!]".to_owned()
            }
        }
        ParamShape::Array(prim) => format!("[{}!]", sdl_primitive(prim)),
    };
    if param.required {
        format!("{base}!")
    } else {
        base
    }
}

/// Generates JSON Schema 2020-12 descriptors and GAP-QL SDL from `doc` and `extraction`.
pub fn build_schema_bundle(
    doc: &OpenApiDocument,
    extraction: &Extraction,
) -> Result<SchemaBundle, ExtractError> {
    let mut sdl = SdlBuilder::default();
    let mut operations = BTreeMap::new();
    let mut descriptors = Vec::with_capacity(extraction.operations.len());
    let mut query_fields = Vec::with_capacity(extraction.operations.len());

    for op in &extraction.operations {
        let input_json_schema = build_input_json_schema(op);
        let output_json_schema = build_output_json_schema(op);

        let input_struct = input_json_schema
            .as_object()
            .map(json_map_to_prost_struct)
            .ok_or_else(|| {
                ExtractError::InvalidStructure(
                    "generated input schema must be an object".to_owned(),
                )
            })?;
        let output_struct = output_json_schema
            .as_object()
            .map(json_map_to_prost_struct)
            .ok_or_else(|| {
                ExtractError::InvalidStructure(
                    "generated output schema must be an object".to_owned(),
                )
            })?;

        let mut args_sdl = Vec::with_capacity(op.parameters.len());
        for param in &op.parameters {
            let arg_ty = build_arg_sdl_type(&mut sdl, &op.name, param);
            args_sdl.push(format!("{}: {arg_ty}", param.sdl_arg_name));
        }

        let type_hint = op
            .response_type_hint
            .clone()
            .unwrap_or_else(|| format!("{}Result", to_pascal_case(&op.name)));
        let return_sdl = match &op.response_schema {
            Some(resp_schema) => {
                let is_array = resp_schema
                    .as_object()
                    .and_then(|o| o.get("type"))
                    .and_then(Value::as_str)
                    == Some("array");
                if is_array {
                    let items = resp_schema
                        .as_object()
                        .and_then(|o| o.get("items"))
                        .unwrap_or(&Value::Null);
                    let elem = sdl.sdl_type_for_schema(&type_hint, items, true);
                    format!("[{elem}]!")
                } else {
                    // Keyed reads may return 0 rows on upstream 404, so keep them nullable in SDL.
                    let non_null = op.key_fields.is_empty();
                    sdl.sdl_type_for_schema(&type_hint, resp_schema, non_null)
                }
            }
            None => "JSON".to_owned(),
        };

        let field_sig = if args_sdl.is_empty() {
            format!("  {}: {return_sdl}", op.name)
        } else {
            format!("  {}({}): {return_sdl}", op.name, args_sdl.join(", "))
        };
        query_fields.push(field_sig);

        let mut annotations = HashMap::with_capacity(4);
        annotations.insert(
            "batch".to_owned(),
            if op.native_batch.is_some() {
                "native".to_owned()
            } else {
                "emulated".to_owned()
            },
        );
        annotations.insert("openapi.method".to_owned(), "GET".to_owned());
        annotations.insert("openapi.path".to_owned(), op.path_template.clone());
        annotations.insert(
            "openapi.operation_id".to_owned(),
            op.raw_operation_id
                .clone()
                .unwrap_or_else(|| op.name.clone()),
        );

        let descriptor = OperationDescriptor {
            name: op.name.clone(),
            kind: OpKind::Query as i32,
            description: op.summary.clone(),
            input_schema: Some(input_struct),
            output_schema: Some(output_struct),
            side_effect_hint: SideEffectHint::SeNone as i32,
            idempotent_hint: true,
            supports_dry_run: false,
            compensation_operation: String::new(),
            examples: Vec::new(),
            cost_weight: 1,
            pagination: if op.has_offset_pagination {
                PaginationKind::PaginationOffset as i32
            } else {
                PaginationKind::PaginationNone as i32
            },
            key_fields: op.key_fields.clone(),
            annotations,
        };

        descriptors.push(descriptor.clone());
        operations.insert(
            op.name.clone(),
            GeneratedOperationSchema {
                descriptor,
                input_json_schema,
                output_json_schema,
            },
        );
    }

    let mut sdl_blocks: Vec<String> = sdl.definitions.into_values().collect();
    if !query_fields.is_empty() {
        sdl_blocks.push(format!("type Query {{\n{}\n}}", query_fields.join("\n")));
    }
    let schema_sdl = sdl_blocks.join("\n\n");

    Ok(SchemaBundle {
        spec_hash: doc.spec_hash().to_owned(),
        schema_sdl,
        operations,
        descriptors,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::extract::extract_operations;

    fn has_any_ref(v: &Value) -> bool {
        match v {
            Value::Object(map) => map.contains_key("$ref") || map.values().any(has_any_ref),
            Value::Array(items) => items.iter().any(has_any_ref),
            _ => false,
        }
    }

    // spec: NGC-010, NGC-012, NGC-013, GQL-004
    #[test]
    fn shop_sdl_passes_gap_ql_builtins_and_descriptors_carry_inlined_schemas_and_annotations() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/openapi/shop.openapi.json");
        let bytes = std::fs::read(p).expect("read shop");
        let doc = OpenApiDocument::parse(&bytes).expect("parse");
        let ext = extract_operations(&doc).expect("extract");
        let bundle1 = build_schema_bundle(&doc, &ext).expect("bundle 1");
        let bundle2 = build_schema_bundle(&doc, &ext).expect("bundle 2");
        assert_eq!(bundle1, bundle2, "schema generation must be deterministic");

        // Must compose cleanly onto GAP's built-in supergraph SDL with zero GQL-004 violations.
        gap_ql::schema::with_gap_builtins(&bundle1.schema_sdl).expect("valid GAP branch SDL");

        for gen_op in bundle1.operations.values() {
            assert!(
                !has_any_ref(&gen_op.input_json_schema),
                "input_schema of `{}` still contains $ref",
                gen_op.descriptor.name
            );
            assert!(
                !has_any_ref(&gen_op.output_json_schema),
                "output_schema of `{}` still contains $ref",
                gen_op.descriptor.name
            );
            assert_eq!(
                gen_op
                    .descriptor
                    .annotations
                    .get("openapi.method")
                    .map(String::as_str),
                Some("GET")
            );
            assert!(gen_op.descriptor.idempotent_hint);
        }

        let customer = bundle1.operations.get("customer").expect("customer");
        assert_eq!(
            customer
                .descriptor
                .annotations
                .get("batch")
                .map(String::as_str),
            Some("native")
        );
        let order = bundle1.operations.get("order").expect("order");
        assert_eq!(
            order
                .descriptor
                .annotations
                .get("batch")
                .map(String::as_str),
            Some("emulated")
        );
    }

    // spec: NGC-012, GQL-004
    #[test]
    fn sdl_generator_avoids_gap_reserved_types_dunder_names_and_handles_oneof_as_json() {
        let raw = json!({
            "openapi": "3.1.0",
            "info": { "title": "ReservedTest", "version": "1.0.0" },
            "paths": {
                "/tasks": {
                    "get": {
                        "operationId": "list_tasks",
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "#/components/schemas/Task" }
                                    }
                                }
                            }
                        }
                    }
                },
                "/poly": {
                    "get": {
                        "operationId": "poly",
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": {
                                            "oneOf": [
                                                { "type": "string" },
                                                { "type": "integer" }
                                            ]
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "Task": {
                        "type": "object",
                        "required": ["id"],
                        "properties": {
                            "id": { "type": "string" },
                            "__hidden": { "type": "string" }
                        }
                    }
                }
            }
        });
        let doc = OpenApiDocument::parse(&serde_json::to_vec(&raw).expect("json")).expect("parse");
        let ext = extract_operations(&doc).expect("extract");
        let bundle = build_schema_bundle(&doc, &ext).expect("bundle");
        gap_ql::schema::with_gap_builtins(&bundle.schema_sdl)
            .expect("hostile/reserved component names must still produce valid GAP SDL");
    }
}
