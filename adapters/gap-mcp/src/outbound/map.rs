//! Mapping of MCP tools and resources to NGC operations and result rows (`CMP-011`, `CMP-012`, `ADR-015`).
//!
//! Everything a server says about a tool (name, description, annotations) is **untrusted**: names are sanitised, texts clipped, annotations
//! kept as strings and never used to change the operation kind or the side-effect hint.

use std::collections::{BTreeMap, BTreeSet};

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_proto::ngc::v1::{OpKind, OperationDescriptor, SideEffectHint};
use serde_json::{Map, Value};

use super::config::OpClassification;
use super::error::{Redactor, sanitize_text};

/// Operation exposed for `resources/read`.
pub const READ_RESOURCE_OP: &str = "mcp_read_resource";
/// Node category of MCP resources (`CMP-011`).
pub const RESOURCE_CATEGORY: &str = "McpResource";
/// Longest description kept (characters).
pub const MAX_DESCRIPTION_CHARS: usize = 2_000;

/// A tool as listed by the server (untrusted).
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// Server-side tool name (any string).
    pub name: String,
    /// Description (sanitised, clipped).
    pub description: String,
    /// `inputSchema` (JSON Schema).
    pub input_schema: Value,
    /// `outputSchema`, if any.
    pub output_schema: Option<Value>,
    /// Annotations as listed (hints only).
    pub annotations: Map<String, Value>,
}

/// A resource as listed by the server (untrusted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// URI.
    pub uri: String,
    /// Display name.
    pub name: String,
    /// MIME type.
    pub mime_type: String,
}

/// How the arguments of an operation reach the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgMode {
    /// One GraphQL argument per `inputSchema` property.
    Typed,
    /// A single `arguments: JSON` argument (property names are not valid GraphQL names).
    Json,
}

/// What an operation calls upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `tools/call` of the tool with this server-side name.
    Tool(String),
    /// `resources/read`.
    ReadResource,
}

/// One NGC operation.
#[derive(Debug, Clone)]
pub struct MappedOp {
    /// Operation name (a valid GraphQL name, unique).
    pub name: String,
    /// Upstream target.
    pub target: Target,
    /// Operation kind (`QUERY` unless an admin classified the tool).
    pub kind: OpKind,
    /// How arguments map to the tool's.
    pub arg_mode: ArgMode,
    /// Descriptor reported in `Describe`.
    pub descriptor: OperationDescriptor,
    /// The tool's `inputSchema` (empty object for the resource reader), used to validate arguments before anything is sent.
    pub input_schema: Value,
}

/// Result of mapping a tool list.
#[derive(Debug, Clone, Default)]
pub struct Mapped {
    /// Operations by name.
    pub ops: BTreeMap<String, MappedOp>,
    /// Branch SDL (types `Query`/`Mutation` with one root field per operation).
    pub sdl: String,
    /// Node categories (`McpResource` when the server lists resources).
    pub node_categories: Vec<String>,
}

/// `true` for `[A-Za-z_][A-Za-z0-9_]*` that does not start with `__`.
#[must_use]
pub fn is_graphql_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with("__")
}

/// Sanitises a tool name into an operation name (deterministic, never empty, never reserved).
#[must_use]
pub fn sanitize_op_name(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out = "tool".to_owned();
    }
    if out.starts_with("__") {
        out = format!("tool{out}");
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out = format!("t_{out}");
    }
    if out == "gap" || out == READ_RESOURCE_OP {
        out = format!("tool_{out}");
    }
    out
}

/// Parses the `tools` array of a `tools/list` page; entries without a string name are skipped.
#[must_use]
pub fn parse_tools(items: &[Value]) -> Vec<Tool> {
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let name = obj.get("name")?.as_str()?.to_owned();
            let description = obj
                .get("description")
                .and_then(Value::as_str)
                .map(|d| sanitize_text(d, MAX_DESCRIPTION_CHARS))
                .unwrap_or_default();
            let input_schema = obj
                .get("inputSchema")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| serde_json::json!({"type": "object"}));
            let output_schema = obj.get("outputSchema").cloned().filter(Value::is_object);
            let annotations = obj
                .get("annotations")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            Some(Tool {
                name,
                description,
                input_schema,
                output_schema,
                annotations,
            })
        })
        .collect()
}

/// Parses the `resources` array of a `resources/list` page.
#[must_use]
pub fn parse_resources(items: &[Value]) -> Vec<Resource> {
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let uri = obj.get("uri")?.as_str()?.to_owned();
            let text = |k: &str| {
                obj.get(k)
                    .and_then(Value::as_str)
                    .map(|s| sanitize_text(s, 200))
                    .unwrap_or_default()
            };
            Some(Resource {
                uri,
                name: text("name"),
                mime_type: text("mimeType"),
            })
        })
        .collect()
}

fn gql_type(schema: &Value) -> String {
    let ty = match schema.get("type") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    };
    match ty {
        Some("string") => "String".to_owned(),
        Some("integer") => "Int".to_owned(),
        Some("number") => "Float".to_owned(),
        Some("boolean") => "Boolean".to_owned(),
        Some("array") => match schema.get("items") {
            Some(items) if items.is_object() => format!("[{}]", gql_type(items)),
            _ => "[JSON]".to_owned(),
        },
        _ => "JSON".to_owned(),
    }
}

/// Decides how the arguments of `schema` are exposed and renders the GraphQL argument list.
fn render_args(schema: &Value) -> (ArgMode, String) {
    let props = schema.get("properties").and_then(Value::as_object);
    let Some(props) = props.filter(|p| !p.is_empty()) else {
        return (ArgMode::Typed, String::new());
    };
    if !props.keys().all(|k| is_graphql_name(k)) {
        return (ArgMode::Json, "arguments: JSON".to_owned());
    }
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let rendered: Vec<String> = props
        .iter()
        .map(|(name, sub)| {
            format!(
                "{name}: {}{}",
                gql_type(sub),
                if required.contains(name.as_str()) {
                    "!"
                } else {
                    ""
                }
            )
        })
        .collect();
    (ArgMode::Typed, rendered.join(", "))
}

fn string_annotations(tool: &Tool) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    out.insert("batch".to_owned(), "emulated".to_owned());
    out.insert("mcp.tool".to_owned(), sanitize_text(&tool.name, 200));
    for (k, v) in &tool.annotations {
        let rendered = match v {
            Value::String(s) => sanitize_text(s, 200),
            other => sanitize_text(&other.to_string(), 200),
        };
        out.insert(format!("mcp.annotation.{}", sanitize_text(k, 60)), rendered);
    }
    out
}

/// Maps tools and resources to operations and the branch SDL (`CMP-011`).
///
/// Deterministic: tools are processed sorted by their original name; name collisions get `_2`, `_3`, ... suffixes.
#[must_use]
pub fn map_discovery(
    tools: &[Tool],
    resources: &[Resource],
    classification: &OpClassification,
) -> Mapped {
    let mut mapped = Mapped::default();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut query_fields: Vec<String> = Vec::new();
    let mut mutation_fields: Vec<String> = Vec::new();

    if !resources.is_empty() {
        used.insert(READ_RESOURCE_OP.to_owned());
        let descriptor = OperationDescriptor {
            name: READ_RESOURCE_OP.to_owned(),
            kind: OpKind::Query as i32,
            description: "Reads an MCP resource by URI.".to_owned(),
            input_schema: Some(json_map_to_prost_struct(
                serde_json::json!({"type": "object", "properties": {"uri": {"type": "string"}}, "required": ["uri"], "additionalProperties": false})
                    .as_object()
                    .unwrap_or(&Map::new()),
            )),
            output_schema: None,
            side_effect_hint: SideEffectHint::SeNone as i32,
            idempotent_hint: true,
            annotations: [("batch".to_owned(), "emulated".to_owned())].into_iter().collect(),
            ..OperationDescriptor::default()
        };
        query_fields.push(format!("  {READ_RESOURCE_OP}(uri: String!): JSON"));
        mapped.ops.insert(
            READ_RESOURCE_OP.to_owned(),
            MappedOp {
                name: READ_RESOURCE_OP.to_owned(),
                target: Target::ReadResource,
                kind: OpKind::Query,
                arg_mode: ArgMode::Typed,
                descriptor,
                input_schema: Value::Object(Map::new()),
            },
        );
        mapped.node_categories.push(RESOURCE_CATEGORY.to_owned());
    }

    let mut sorted: Vec<&Tool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in sorted {
        let base = sanitize_op_name(&tool.name);
        let mut name = base.clone();
        let mut n = 2;
        while used.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        used.insert(name.clone());
        let is_mutation = classification.mutations.contains(&tool.name);
        let kind = if is_mutation {
            OpKind::Mutation
        } else {
            OpKind::Query
        };
        let (arg_mode, args) = render_args(&tool.input_schema);
        let field = if args.is_empty() {
            format!("  {name}: JSON")
        } else {
            format!("  {name}({args}): JSON")
        };
        if is_mutation {
            mutation_fields.push(field);
        } else {
            query_fields.push(field);
        }
        let descriptor = OperationDescriptor {
            name: name.clone(),
            kind: kind as i32,
            description: tool.description.clone(),
            input_schema: tool.input_schema.as_object().map(json_map_to_prost_struct),
            output_schema: Some(json_map_to_prost_struct(
                tool.output_schema
                    .as_ref()
                    .and_then(Value::as_object)
                    .unwrap_or(&Map::new()),
            )),
            // Hints from the server never relax anything: reads stay `NONE` only because the kind is a query, mutations stay unclassified.
            side_effect_hint: if is_mutation {
                SideEffectHint::SideEffectUnspecified as i32
            } else {
                SideEffectHint::SeNone as i32
            },
            idempotent_hint: false,
            supports_dry_run: false,
            annotations: string_annotations(tool),
            ..OperationDescriptor::default()
        };
        mapped.ops.insert(
            name.clone(),
            MappedOp {
                name,
                target: Target::Tool(tool.name.clone()),
                kind,
                arg_mode,
                descriptor,
                input_schema: tool.input_schema.clone(),
            },
        );
    }

    let mut sdl = String::new();
    if !query_fields.is_empty() {
        sdl.push_str("type Query {\n");
        sdl.push_str(&query_fields.join("\n"));
        sdl.push_str("\n}\n");
    }
    if !mutation_fields.is_empty() {
        sdl.push_str("type Mutation {\n");
        sdl.push_str(&mutation_fields.join("\n"));
        sdl.push_str("\n}\n");
    }
    mapped.sdl = sdl;
    mapped
}

/// Hex `blake3` of the canonical text of the sorted tool and resource lists (the connector's change detector, `NGC-010`).
#[must_use]
pub fn spec_hash(tools: &[Tool], resources: &[Resource]) -> String {
    let mut sorted: Vec<&Tool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let tool_values: Vec<Value> = sorted
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name, "description": t.description, "inputSchema": t.input_schema,
                "outputSchema": t.output_schema, "annotations": t.annotations,
            })
        })
        .collect();
    let mut res: Vec<(&str, &str, &str)> = resources
        .iter()
        .map(|r| (r.uri.as_str(), r.name.as_str(), r.mime_type.as_str()))
        .collect();
    res.sort_unstable();
    let canonical = serde_json::json!({"tools": tool_values, "resources": res}).to_string();
    blake3::hash(canonical.as_bytes()).to_hex().to_string()
}

/// Largest integer an `f64` represents exactly (`2^53`).
const MAX_EXACT_INT: f64 = 9_007_199_254_740_992.0;

/// Protobuf `Struct` numbers are doubles: turns whole-number doubles back into JSON integers (`7.0` -> `7`) so that servers with strict
/// `integer` schemas accept the arguments; fractional and huge values are left alone.
#[must_use]
pub fn normalize_numbers(value: &Value) -> Value {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(f)
                if n.is_f64() && f.is_finite() && f.fract() == 0.0 && f.abs() <= MAX_EXACT_INT =>
            {
                #[allow(clippy::cast_possible_truncation)]
                let i = f as i64;
                Value::from(i)
            }
            _ => value.clone(),
        },
        Value::Array(items) => Value::Array(items.iter().map(normalize_numbers).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_numbers(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Arguments of the upstream call for `op` given the NGC call arguments.
pub fn call_arguments(op: &MappedOp, args: &Map<String, Value>) -> Result<Value, NgcErrorInfo> {
    match (&op.target, op.arg_mode) {
        (Target::ReadResource, _) => Ok(Value::Object(args.clone())),
        (Target::Tool(_), ArgMode::Typed) => Ok(normalize_numbers(&Value::Object(args.clone()))),
        (Target::Tool(_), ArgMode::Json) => match args.get("arguments") {
            None | Some(Value::Null) => Ok(Value::Object(Map::new())),
            Some(v @ Value::Object(_)) => Ok(normalize_numbers(v)),
            Some(_) => Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "argument `arguments` must be an object",
            )),
        },
    }
}

fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn check_value(path: &str, schema: &Value, value: &Value, depth: usize) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    let allowed: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !allowed.is_empty() && !allowed.iter().any(|t| type_matches(t, value)) {
        return Err(format!("argument `{path}` has the wrong type"));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(format!(
            "argument `{path}` is not one of the allowed values"
        ));
    }
    if depth < 8
        && let (Some(items), Some(list)) = (schema.get("items"), value.as_array())
    {
        for (i, item) in list.iter().enumerate() {
            check_value(&format!("{path}[{i}]"), items, item, depth + 1)?;
        }
    }
    Ok(())
}

/// Checks the call arguments against the tool's `inputSchema` before anything is sent (`required`, property types, enums, array items,
/// `additionalProperties: false`). Messages name the argument, never its value. The server still validates everything else.
pub fn validate_arguments(schema: &Value, arguments: &Value) -> Result<(), NgcErrorInfo> {
    let Some(args) = arguments.as_object() else {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "arguments must be an object",
        ));
    };
    let fail = |message: String| NgcErrorInfo::new(NgcErrorCode::InvalidArgument, message);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !args.contains_key(key) {
                return Err(fail(format!(
                    "missing required argument `{}`",
                    sanitize_text(key, 80)
                )));
            }
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
    for (key, value) in args {
        match props.and_then(|p| p.get(key)) {
            Some(prop) => check_value(&sanitize_text(key, 80), prop, value, 0).map_err(fail)?,
            None if closed => {
                return Err(fail(format!(
                    "unknown argument `{}`",
                    sanitize_text(key, 80)
                )));
            }
            None => {}
        }
    }
    Ok(())
}

/// Maps a `CallToolResult` (or `ReadResourceResult`) to rows (`CMP-012`).
///
/// `isError` becomes `UPSTREAM_ERROR` with sanitised, redacted text; `input_required` results are refused in v0.
pub fn map_call_result(result: &Value, redactor: &Redactor) -> Result<Vec<Value>, NgcErrorInfo> {
    if result.get("resultType").and_then(Value::as_str) == Some("input_required") {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::UpstreamError,
            "input_required results are not supported in v0",
        ));
    }
    let texts = || -> Vec<&str> {
        result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default()
    };
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = redactor.apply(&sanitize_text(
            &texts().join(" "),
            super::error::MAX_SERVER_TEXT_CHARS,
        ));
        return Err(NgcErrorInfo::new(
            NgcErrorCode::UpstreamError,
            format!("upstream MCP tool reported an error: {text}"),
        ));
    }
    if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return Ok(rows_of(structured.clone()));
    }
    if let Some(contents) = result.get("contents").and_then(Value::as_array) {
        // `resources/read`: one row per content block, JSON text parsed when it is JSON.
        return Ok(contents
            .iter()
            .map(|c| {
                let text = c.get("text").and_then(Value::as_str).unwrap_or_default();
                serde_json::from_str::<Value>(text)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| {
                        let mut row = Map::new();
                        row.insert(
                            "uri".to_owned(),
                            c.get("uri").cloned().unwrap_or(Value::Null),
                        );
                        row.insert("text".to_owned(), Value::String(text.to_owned()));
                        Value::Object(row)
                    })
            })
            .collect());
    }
    let texts = texts();
    if texts.len() == 1
        && let Ok(parsed) = serde_json::from_str::<Value>(texts[0])
    {
        return Ok(rows_of(parsed));
    }
    let mut row = Map::new();
    row.insert("text".to_owned(), Value::String(texts.join("\n")));
    Ok(vec![Value::Object(row)])
}

fn rows_of(value: Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                obj @ Value::Object(_) => obj,
                other => {
                    let mut m = Map::new();
                    m.insert("value".to_owned(), other);
                    Value::Object(m)
                }
            })
            .collect(),
        obj @ Value::Object(_) => vec![obj],
        other => {
            let mut m = Map::new();
            m.insert("value".to_owned(), other);
            vec![Value::Object(m)]
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str, schema: Value) -> Tool {
        Tool {
            name: name.to_owned(),
            description: format!("{name} d"),
            input_schema: schema,
            output_schema: None,
            annotations: Map::new(),
        }
    }

    // spec: CMP-011, ADR-015
    #[test]
    fn outbound_names_are_sanitised_deterministically_with_collision_suffixes() {
        for (raw, want) in [
            ("get-user", "get_user"),
            ("9lives", "t_9lives"),
            ("__proto", "tool__proto"),
            ("", "tool"),
            ("gap", "tool_gap"),
            ("héllo", "h_llo"),
            ("a b", "a_b"),
        ] {
            assert_eq!(sanitize_op_name(raw), want, "{raw:?}");
        }
        let tools = [
            tool("a-b", json!({})),
            tool("a_b", json!({})),
            tool("a b", json!({})),
        ];
        let mapped = map_discovery(&tools, &[], &OpClassification::default());
        let names: Vec<&String> = mapped.ops.keys().collect();
        assert_eq!(names, ["a_b", "a_b_2", "a_b_3"]);
        // permuting the input must not change which tool gets which name
        let permuted = [tools[2].clone(), tools[0].clone(), tools[1].clone()];
        let again = map_discovery(&permuted, &[], &OpClassification::default());
        for (name, op) in &mapped.ops {
            assert_eq!(again.ops[name].target, op.target);
        }
    }

    // spec: CMP-011, ADR-015
    #[test]
    fn outbound_annotations_are_hints_only_and_admin_classification_decides_the_kind() {
        let mut t = tool(
            "delete_all",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
        );
        t.annotations.insert("readOnlyHint".to_owned(), json!(true));
        t.annotations
            .insert("destructiveHint".to_owned(), json!(false));
        let classification = OpClassification {
            mutations: ["delete_all".to_owned()].into_iter().collect(),
        };
        let mapped = map_discovery(&[t.clone()], &[], &classification);
        let op = &mapped.ops["delete_all"];
        assert_eq!(
            op.kind,
            OpKind::Mutation,
            "readOnlyHint must not relax an admin classification"
        );
        assert_eq!(
            op.descriptor.annotations["mcp.annotation.readOnlyHint"],
            "true"
        );
        assert!(mapped.sdl.contains("type Mutation"));
        assert!(!mapped.sdl.contains("type Query"));
        // without classification, a destructiveHint never makes a tool a mutation either
        t.annotations
            .insert("destructiveHint".to_owned(), json!(true));
        let mapped = map_discovery(&[t], &[], &OpClassification::default());
        assert_eq!(mapped.ops["delete_all"].kind, OpKind::Query);
    }

    // spec: CMP-011, NGC-012
    #[test]
    fn outbound_sdl_types_arguments_and_falls_back_to_json_arguments_for_invalid_property_names() {
        let typed = tool(
            "search",
            json!({"type": "object", "properties": {"q": {"type": "string"}, "limit": {"type": "integer"}, "tags": {"type": "array", "items": {"type": "string"}}, "opts": {"type": "object"}}, "required": ["q"]}),
        );
        let weird = tool(
            "weird",
            json!({"type": "object", "properties": {"a-b": {"type": "string"}}}),
        );
        let none = tool("ping", json!({"type": "object"}));
        let mapped = map_discovery(&[typed, weird, none], &[], &OpClassification::default());
        assert!(
            mapped
                .sdl
                .contains("search(limit: Int, opts: JSON, q: String!, tags: [String]): JSON"),
            "{}",
            mapped.sdl
        );
        assert!(mapped.sdl.contains("weird(arguments: JSON): JSON"));
        assert!(mapped.sdl.contains("  ping: JSON"));
        assert_eq!(mapped.ops["weird"].arg_mode, ArgMode::Json);
        assert!(
            gap_ql::schema::with_gap_builtins(&mapped.sdl).is_ok(),
            "{}",
            mapped.sdl
        );
        // resources add the reserved read operation and the node category
        let with_res = map_discovery(
            &[],
            &[Resource {
                uri: "file:///a".to_owned(),
                name: "a".to_owned(),
                mime_type: "text/plain".to_owned(),
            }],
            &OpClassification::default(),
        );
        assert!(with_res.ops.contains_key(READ_RESOURCE_OP));
        assert_eq!(with_res.node_categories, [RESOURCE_CATEGORY]);
        assert!(gap_ql::schema::with_gap_builtins(&with_res.sdl).is_ok());
    }

    // spec: CMP-012
    #[test]
    fn outbound_result_mapping_prefers_structured_content_then_json_text_then_wrapped_text() {
        let r = Redactor::default();
        let structured = json!({"structuredContent": {"a": 1}, "content": [{"type": "text", "text": "{\"b\":2}"}]});
        assert_eq!(
            map_call_result(&structured, &r).expect("rows"),
            vec![json!({"a": 1})]
        );
        let list = json!({"structuredContent": [{"a": 1}, 5]});
        assert_eq!(
            map_call_result(&list, &r).expect("rows"),
            vec![json!({"a": 1}), json!({"value": 5})]
        );
        let json_text = json!({"content": [{"type": "text", "text": "{\"b\":2}"}]});
        assert_eq!(
            map_call_result(&json_text, &r).expect("rows"),
            vec![json!({"b": 2})]
        );
        let plain = json!({"content": [{"type": "text", "text": "hello"}, {"type": "image", "data": "x"}, {"type": "text", "text": "world"}]});
        assert_eq!(
            map_call_result(&plain, &r).expect("rows"),
            vec![json!({"text": "hello\nworld"})]
        );
        let resource = json!({"contents": [{"uri": "file:///a", "text": "{\"k\":1}"}, {"uri": "file:///b", "text": "raw"}]});
        assert_eq!(
            map_call_result(&resource, &r).expect("rows"),
            vec![json!({"k": 1}), json!({"uri": "file:///b", "text": "raw"})]
        );
    }

    // spec: CMP-012, CMP-013, INV-15
    #[test]
    fn outbound_is_error_and_input_required_results_are_errors_with_redacted_text() {
        let redactor = Redactor::new(["TOKEN-1".to_owned()]);
        let err = map_call_result(&json!({"isError": true, "content": [{"type": "text", "text": "bad TOKEN-1 \u{1b}[0m"}]}), &redactor).expect_err("isError");
        assert_eq!(err.code, NgcErrorCode::UpstreamError);
        assert!(!err.message().contains("TOKEN-1") && !err.message().contains('\u{1b}'));
        let err = map_call_result(
            &json!({"resultType": "input_required", "requestState": "s", "inputRequests": {}}),
            &redactor,
        )
        .expect_err("mrtr");
        assert_eq!(err.code, NgcErrorCode::UpstreamError);
        assert!(err.message().contains("input_required"));
    }

    // spec: CMP-011, NGC-010
    #[test]
    fn outbound_spec_hash_is_order_independent_and_changes_with_any_tool_field() {
        let a = tool("a", json!({"type": "object"}));
        let b = tool("b", json!({"type": "object"}));
        let h1 = spec_hash(&[a.clone(), b.clone()], &[]);
        assert_eq!(h1, spec_hash(&[b.clone(), a.clone()], &[]));
        let mut a2 = a.clone();
        a2.description.push('!');
        assert_ne!(h1, spec_hash(&[a2, b.clone()], &[]));
        let mut b2 = b;
        b2.annotations
            .insert("readOnlyHint".to_owned(), json!(true));
        assert_ne!(h1, spec_hash(&[a, b2], &[]));
    }

    // spec: NGC-060, CMP-012
    #[test]
    fn outbound_arguments_are_validated_against_the_input_schema_without_echoing_values() {
        let schema = json!({"type": "object", "properties": {"id": {"type": "integer"}, "tags": {"type": "array", "items": {"type": "string"}}, "mode": {"enum": ["a", "b"]}, "any": {"type": ["string", "null"]}}, "required": ["id"], "additionalProperties": false});
        let ok = json!({"id": 1, "tags": ["x"], "mode": "a", "any": null});
        assert!(validate_arguments(&schema, &ok).is_ok());
        let canary = "SECRET-VALUE-42";
        for (bad, needle) in [
            (json!({}), "missing required argument `id`"),
            (json!({"id": canary}), "argument `id` has the wrong type"),
            (json!({"id": 1.5}), "argument `id` has the wrong type"),
            (
                json!({"id": 1, "tags": [canary, 5]}),
                "argument `tags[1]` has the wrong type",
            ),
            (
                json!({"id": 1, "mode": canary}),
                "argument `mode` is not one of the allowed values",
            ),
            (
                json!({"id": 1, "extra": canary}),
                "unknown argument `extra`",
            ),
            (
                json!({"id": 1, "any": 5}),
                "argument `any` has the wrong type",
            ),
        ] {
            let err = validate_arguments(&schema, &bad).expect_err("rejected");
            assert_eq!(err.code, NgcErrorCode::InvalidArgument);
            assert!(err.message().contains(needle), "{}", err.message());
            assert!(
                !err.message().contains(canary),
                "value leaked: {}",
                err.message()
            );
        }
        assert!(validate_arguments(&schema, &json!([1])).is_err());
        // an open schema (no additionalProperties:false) accepts unknown keys; a non-object schema accepts everything
        assert!(validate_arguments(&json!({"type": "object"}), &json!({"anything": 1})).is_ok());
        assert!(validate_arguments(&json!(true), &json!({"anything": 1})).is_ok());
    }

    // spec: CMP-012
    #[test]
    fn outbound_whole_number_doubles_become_integers_in_forwarded_arguments() {
        let op = map_discovery(
            &[tool(
                "t",
                json!({"type": "object", "properties": {"id": {"type": "integer"}}}),
            )],
            &[],
            &OpClassification::default(),
        )
        .ops["t"]
            .clone();
        let args = json!({"id": 7.0, "ratio": 0.5, "big": 1e300, "neg": -3.0, "list": [1.0, 2.5, {"k": 4.0}], "s": "7.0", "b": true, "n": null});
        let out = call_arguments(&op, args.as_object().expect("object")).expect("args");
        assert_eq!(out["id"], json!(7));
        assert!(
            out["id"].is_i64() || out["id"].is_u64(),
            "must serialise without a decimal point: {}",
            out["id"]
        );
        assert_eq!(out["ratio"], json!(0.5));
        assert_eq!(out["big"], json!(1e300));
        assert_eq!(out["neg"], json!(-3));
        assert_eq!(out["list"], json!([1, 2.5, {"k": 4}]));
        assert_eq!(out["s"], json!("7.0"));
        assert_eq!(out["n"], Value::Null);
        assert_eq!(
            out.to_string().matches("7.0").count(),
            1,
            "only the string `7.0` may keep the decimal point"
        );
    }

    // spec: CMP-011
    #[test]
    fn outbound_hostile_tool_lists_end_bounded() {
        let many: Vec<Tool> = (0..10_000)
            .map(|i| {
                tool(
                    &format!("t{i}"),
                    json!({"type": "object", "properties": {"x": {"type": "string"}}}),
                )
            })
            .collect();
        let mapped = map_discovery(&many, &[], &OpClassification::default());
        assert_eq!(mapped.ops.len(), 10_000);
        let dup: Vec<Tool> = (0..500).map(|_| tool("same", json!({}))).collect();
        assert_eq!(
            map_discovery(&dup, &[], &OpClassification::default())
                .ops
                .len(),
            500
        );
        let items = vec![
            json!({"name": "x", "description": "d".repeat(1 << 20), "inputSchema": {"type": "object"}}),
            json!({"noname": 1}),
            json!(5),
            json!({"name": "__y", "inputSchema": 7}),
        ];
        let parsed = parse_tools(&items);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].description.chars().count() <= MAX_DESCRIPTION_CHARS);
        assert!(
            gap_ql::schema::with_gap_builtins(
                &map_discovery(&parsed, &[], &OpClassification::default()).sdl
            )
            .is_ok()
        );
    }
}
