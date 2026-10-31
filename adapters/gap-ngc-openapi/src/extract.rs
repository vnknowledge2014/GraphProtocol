//! Deterministic OpenAPI `GET` operation extractor (`CMP-030`, `CMP-032`, `ING-010`).

use std::collections::{BTreeMap, BTreeSet};

use gap_core::ids::{MAX_OPERATION_NAME_LEN, is_reserved_root_field};
use serde_json::{Map, Value};

use crate::auth::is_dot_segment_any_spelling;
use crate::document::{ExtractError, OpenApiDocument, unescape_pointer_segment};

/// Maximum description length carried onto wire descriptors (`ADR-014`).
const MAX_SUMMARY_CHARS: usize = 512;

/// OpenAPI Path Item HTTP method keys (`spec/05-ingestion.md` §2).
const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Header names ignored when declared as OpenAPI `in: header` parameters (OpenAPI 3.x §4.8.12).
const IGNORED_OPENAPI_HEADERS: &[&str] = &["accept", "content-type", "authorization"];

/// Forbidden hop-by-hop / framing / credential headers that cause an operation to be skipped (`CMP-030`).
const FORBIDDEN_OPENAPI_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "cookie",
];

/// Location of an OpenAPI operation parameter (`CMP-030`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParamLocation {
    /// Path template slot (`/customers/{id}`).
    Path,
    /// URL query string parameter (`?limit=10`).
    Query,
    /// HTTP request header (`X-Request-Tag: abc`).
    Header,
}

/// Serialization style of an OpenAPI parameter (`CMP-030`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamStyle {
    /// OpenAPI `simple` style (used for path and header parameters).
    Simple,
    /// OpenAPI `form` style with `explode` flag (used for query parameters).
    Form {
        /// When `true`, arrays emit repeated `k=v` pairs; when `false`, comma-separated `k=v1,v2`.
        explode: bool,
    },
}

/// Scalar primitive type accepted in OpenAPI parameters (`CMP-030`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamPrimitive {
    /// UTF-8 string.
    String,
    /// 64-bit signed integer.
    Integer,
    /// IEEE-754 finite floating-point number.
    Number,
    /// Boolean (`true` / `false`).
    Boolean,
}

/// Shape of an extracted parameter (`CMP-030`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamShape {
    /// Single scalar value.
    Scalar(ParamPrimitive),
    /// Homogeneous list of scalar values.
    Array(ParamPrimitive),
}

/// Extracted, validated OpenAPI parameter specification (`CMP-030`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterSpec {
    /// Exact parameter wire name in the path template, query string, or HTTP header.
    pub name: String,
    /// Sanitized GraphQL argument identifier used in `schema_sdl` (`NGC-012`, `GQL-004`).
    pub sdl_arg_name: String,
    /// Slot where the parameter travels.
    pub location: ParamLocation,
    /// Serialization style.
    pub style: ParamStyle,
    /// Whether the caller must provide this parameter.
    pub required: bool,
    /// Scalar or scalar-array shape.
    pub shape: ParamShape,
    /// Inlined, `$ref`-free JSON Schema fragment validating this parameter.
    pub schema: Value,
}

/// Parsed `x-gap-in-param` native `foreach` batching extension (`CMP-032`, `NGC-026`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBatchSpec {
    /// Target `GET` path in the same OpenAPI document (e.g. `"/customers"`).
    pub path: String,
    /// List-valued query parameter on `path` (e.g. `"ids"`).
    pub param: String,
    /// Unique key field in each returned row carrying the lookup key (e.g. `"id"`).
    pub key_field: String,
    /// Effective maximum number of distinct keys sent in one upstream batch call (`>= 1`, capped by target `limit.maximum`).
    pub max_batch_size: u32,
    /// `true` when the batch target operation declares an integer `limit` query parameter (`CMP-032`).
    pub target_has_limit: bool,
}

/// Segment of an OpenAPI path template (`/customers/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    /// Literal path segment (`customers`).
    Literal(String),
    /// Single whole-segment parameter placeholder (`{id}`).
    Param(String),
}

/// Extracted read (`GET`) operation ready for schema generation and request building.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedOperation {
    /// Unique, sanitized operation identifier (`[A-Za-z_][A-Za-z0-9_]*`, `NGC-012`, `GQL-004`).
    pub name: String,
    /// Original `operationId` from the OpenAPI document, if present.
    pub raw_operation_id: Option<String>,
    /// Raw OpenAPI path template (always begins with `'/'`).
    pub path_template: String,
    /// Parsed path segments preserving exact template segment count.
    pub segments: Vec<PathSegment>,
    /// Untrusted human summary/description (`ADR-014`).
    pub summary: String,
    /// Deterministic parameter list.
    pub parameters: Vec<ParameterSpec>,
    /// Path parameter names when this operation is a keyed read.
    pub key_fields: Vec<String>,
    /// `true` when the operation declares both `limit` and `offset` integer query parameters.
    pub has_offset_pagination: bool,
    /// `true` when the operation declares a `sort` string query parameter.
    pub has_sort: bool,
    /// Allowed `sort` values if constrained by an `enum` on the `sort` parameter.
    pub sort_allowed_values: Option<Vec<String>>,
    /// Native `foreach` batch target if `x-gap-in-param` is declared and valid.
    pub native_batch: Option<NativeBatchSpec>,
    /// Optional component schema name hint from a top-level `#/components/schemas/<Name>` response `$ref`.
    pub response_type_hint: Option<String>,
    /// Inlined, `$ref`-free JSON Schema of the `200`/`2xx` `application/json` response body.
    pub response_schema: Option<Value>,
}

/// Record of an OpenAPI operation that was skipped during v0 extraction (`ING-010`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedOperation {
    /// OpenAPI path template.
    pub path: String,
    /// Uppercase HTTP method (`POST`, `GET`, ...).
    pub method: String,
    /// Original `operationId`, if present.
    pub operation_id: Option<String>,
    /// Deterministic explanation of why the operation was skipped.
    pub reason: String,
}

/// Diagnostic report produced alongside extracted operations (`ING-010`, `spec/05-ingestion.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionReport {
    /// OpenAPI version string from the source document.
    pub openapi_version: String,
    /// Hex `blake3` digest of the source document bytes.
    pub spec_hash: String,
    /// Untrusted `servers[].url` values recorded from the document and never followed.
    pub servers: Vec<String>,
    /// Operations skipped because of non-`GET` methods or unsupported parameter features.
    pub skipped: Vec<SkippedOperation>,
}

/// Output of deterministic OpenAPI extraction (`ING-010`).
#[derive(Debug, Clone, PartialEq)]
pub struct Extraction {
    /// Extracted `GET` operations in deterministic order.
    pub operations: Vec<ExtractedOperation>,
    /// Extraction report (recorded servers + skipped operations).
    pub report: ExtractionReport,
}

/// Returns `true` if `name` is a valid RFC 7230 `token` (`1*tchar`).
#[must_use]
pub fn is_rfc7230_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn clip_summary(raw: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in raw.chars().enumerate() {
        if idx >= MAX_SUMMARY_CHARS {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn collapse_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_matches('_').to_owned()
}

fn allocate_unique_name(base: &str, used: &mut BTreeSet<String>) -> String {
    let max_stem = MAX_OPERATION_NAME_LEN.saturating_sub(8);
    let stem: String = base.chars().take(max_stem).collect();
    if used.insert(stem.clone()) {
        return stem;
    }
    let mut suffix: usize = 2;
    loop {
        let candidate = format!("{stem}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn make_operation_name(
    raw_operation_id: Option<&str>,
    segments: &[PathSegment],
    used: &mut BTreeSet<String>,
) -> String {
    let base = match raw_operation_id.filter(|s| !s.trim().is_empty()) {
        Some(raw) => {
            let had_dunder = raw.starts_with("__");
            let cleaned = collapse_ident(raw);
            if cleaned.is_empty() {
                "op_get".to_owned()
            } else if had_dunder
                || cleaned.bytes().next().is_some_and(|b| b.is_ascii_digit())
                || is_reserved_root_field(&cleaned)
            {
                format!("op_{cleaned}")
            } else {
                cleaned
            }
        }
        None => {
            if segments.is_empty() {
                "get_root".to_owned()
            } else {
                let mut parts = vec!["get".to_owned()];
                for seg in segments {
                    match seg {
                        PathSegment::Literal(lit) => {
                            let c = collapse_ident(lit);
                            if !c.is_empty() {
                                parts.push(c);
                            }
                        }
                        PathSegment::Param(param) => {
                            let c = collapse_ident(param);
                            parts.push(if c.is_empty() {
                                "by_param".to_owned()
                            } else {
                                format!("by_{c}")
                            });
                        }
                    }
                }
                let joined = parts.join("_");
                if is_reserved_root_field(&joined) {
                    format!("op_{joined}")
                } else {
                    joined
                }
            }
        }
    };
    allocate_unique_name(&base, used)
}

pub(crate) fn sanitize_graphql_arg_name(raw: &str, used: &mut BTreeSet<String>) -> String {
    let had_dunder = raw.starts_with("__");
    let cleaned = collapse_ident(raw);
    let base = if cleaned.is_empty() {
        "arg".to_owned()
    } else if had_dunder || cleaned.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
        format!("arg_{cleaned}")
    } else {
        cleaned
    };
    allocate_unique_name(&base, used)
}

fn parse_path_template(path: &str) -> Result<Vec<PathSegment>, String> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(format!("path `{path}` must begin with `/`"));
    };
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for seg in rest.split('/') {
        if seg.is_empty() {
            return Err(format!("path `{path}` contains an empty segment"));
        }
        if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            if inner.is_empty() || inner.contains('{') || inner.contains('}') {
                return Err(format!(
                    "path `{path}` has malformed parameter placeholder `{seg}`"
                ));
            }
            segments.push(PathSegment::Param(inner.to_owned()));
        } else if seg.contains('{') || seg.contains('}') {
            return Err(format!(
                "path `{path}` uses unsupported partial segment template `{seg}`"
            ));
        } else if is_dot_segment_any_spelling(seg)
            || seg
                .chars()
                .any(|ch| matches!(ch, '\\' | '?' | '#' | '%' | ' ') || ch.is_control())
        {
            return Err("unsafe_path_literal".to_owned());
        } else {
            segments.push(PathSegment::Literal(seg.to_owned()));
        }
    }
    Ok(segments)
}

fn extract_schema_type_str(schema: &Map<String, Value>) -> Option<&str> {
    match schema.get("type") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(arr)) => arr.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

fn parse_primitive(type_str: &str) -> Option<ParamPrimitive> {
    match type_str {
        "string" => Some(ParamPrimitive::String),
        "integer" => Some(ParamPrimitive::Integer),
        "number" => Some(ParamPrimitive::Number),
        "boolean" => Some(ParamPrimitive::Boolean),
        _ => None,
    }
}

fn parse_param_shape(schema: &Value) -> Result<ParamShape, String> {
    let Some(obj) = schema.as_object() else {
        return Err("parameter `schema` must be a JSON object".to_owned());
    };
    let Some(type_str) = extract_schema_type_str(obj) else {
        return Err("parameter `schema.type` must be an explicit scalar or array".to_owned());
    };
    if let Some(prim) = parse_primitive(type_str) {
        return Ok(ParamShape::Scalar(prim));
    }
    if type_str == "array" {
        let Some(items_obj) = obj.get("items").and_then(Value::as_object) else {
            return Err("array parameter schema is missing `items`".to_owned());
        };
        let Some(item_type) = extract_schema_type_str(items_obj) else {
            return Err("array parameter `items.type` must be a scalar".to_owned());
        };
        let Some(prim) = parse_primitive(item_type) else {
            return Err(format!(
                "array parameter `items.type` `{item_type}` is unsupported in v0"
            ));
        };
        return Ok(ParamShape::Array(prim));
    }
    Err(format!("parameter type `{type_str}` is unsupported in v0"))
}

fn parse_native_batch(ext_val: &Value) -> Result<NativeBatchSpec, ExtractError> {
    let Some(obj) = ext_val.as_object() else {
        return Err(ExtractError::InvalidStructure(
            "`x-gap-in-param` must be an object".to_owned(),
        ));
    };
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| s.starts_with('/'))
        .ok_or_else(|| {
            ExtractError::InvalidStructure(
                "`x-gap-in-param.path` must be a string starting with `/`".to_owned(),
            )
        })?
        .to_owned();
    let param = obj
        .get("param")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ExtractError::InvalidStructure(
                "`x-gap-in-param.param` must be a non-empty string".to_owned(),
            )
        })?
        .to_owned();
    let key_field = obj
        .get("key_field")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ExtractError::InvalidStructure(
                "`x-gap-in-param.key_field` must be a non-empty string".to_owned(),
            )
        })?
        .to_owned();
    let max_batch_u64 = obj
        .get("max_batch_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ExtractError::InvalidStructure(
                "`x-gap-in-param.max_batch_size` must be a positive integer".to_owned(),
            )
        })?;
    let max_batch_size = u32::try_from(max_batch_u64)
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| {
            ExtractError::InvalidStructure(
                "`x-gap-in-param.max_batch_size` must be in 1..=u32::MAX".to_owned(),
            )
        })?;
    Ok(NativeBatchSpec {
        path,
        param,
        key_field,
        max_batch_size,
        target_has_limit: false,
    })
}

fn component_name_from_ref(raw_ref: &str) -> Option<String> {
    let seg = raw_ref.strip_prefix("#/components/schemas/")?;
    if seg.contains('/') {
        return None;
    }
    unescape_pointer_segment(seg, raw_ref).ok()
}

fn extract_response_schema(
    doc: &OpenApiDocument,
    op_obj: &Map<String, Value>,
) -> Result<(Option<String>, Option<Value>), ExtractError> {
    let Some(responses) = op_obj.get("responses").and_then(Value::as_object) else {
        return Ok((None, None));
    };
    let chosen = responses
        .get("200")
        .or_else(|| responses.get("201"))
        .or_else(|| {
            responses
                .iter()
                .find(|(code, _)| code.starts_with('2') && code.len() == 3)
                .map(|(_, v)| v)
        })
        .or_else(|| responses.get("2XX"))
        .or_else(|| responses.get("default"));

    let Some(resp_val) = chosen else {
        return Ok((None, None));
    };
    let resolved_resp = doc.inline_refs(resp_val)?;
    // Extract component type hint from the un-inlined response if available.
    let raw_content = resp_val
        .as_object()
        .and_then(|o| o.get("content"))
        .and_then(Value::as_object);
    let raw_media = raw_content.and_then(|c| {
        c.get("application/json").or_else(|| {
            c.iter()
                .find(|(mt, _)| mt.ends_with("+json"))
                .map(|(_, v)| v)
        })
    });
    let hint = raw_media
        .and_then(Value::as_object)
        .and_then(|m| m.get("schema"))
        .and_then(Value::as_object)
        .and_then(|s| {
            if let Some(r) = s.get("$ref").and_then(Value::as_str) {
                component_name_from_ref(r)
            } else {
                s.get("items")
                    .and_then(Value::as_object)
                    .and_then(|it| it.get("$ref"))
                    .and_then(Value::as_str)
                    .and_then(component_name_from_ref)
            }
        });

    let Some(content_obj) = resolved_resp
        .as_object()
        .and_then(|o| o.get("content"))
        .and_then(Value::as_object)
    else {
        return Ok((hint, None));
    };
    let media_obj = content_obj
        .get("application/json")
        .or_else(|| {
            content_obj
                .iter()
                .find(|(mt, _)| mt.ends_with("+json"))
                .map(|(_, v)| v)
        })
        .and_then(Value::as_object);
    let schema = media_obj.and_then(|m| m.get("schema")).cloned();
    Ok((hint, schema))
}

fn collect_merged_raw_params(
    doc: &OpenApiDocument,
    path_obj: &Map<String, Value>,
    op_obj: &Map<String, Value>,
) -> Result<Vec<Value>, ExtractError> {
    let mut merged: BTreeMap<(String, String), Value> = BTreeMap::new();
    for source in [path_obj.get("parameters"), op_obj.get("parameters")] {
        let Some(list_val) = source else {
            continue;
        };
        let Some(arr) = list_val.as_array() else {
            return Err(ExtractError::InvalidStructure(
                "`parameters` must be an array".to_owned(),
            ));
        };
        for raw_param in arr {
            let resolved = doc.inline_refs(raw_param)?;
            let Some(p_obj) = resolved.as_object() else {
                return Err(ExtractError::InvalidStructure(
                    "parameter entry must be an object".to_owned(),
                ));
            };
            let loc = p_obj
                .get("in")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let name = p_obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let key_name = if loc == "header" {
                name.to_ascii_lowercase()
            } else {
                name
            };
            merged.insert((loc, key_name), resolved);
        }
    }
    Ok(merged.into_values().collect())
}

/// Deterministically extracts read (`GET`) operations from `doc` (`ING-010`, `CMP-030`, `CMP-032`).
pub fn extract_operations(doc: &OpenApiDocument) -> Result<Extraction, ExtractError> {
    let Some(paths_obj) = doc.root().get("paths").and_then(Value::as_object) else {
        return Err(ExtractError::MissingField("paths"));
    };

    let mut operations = Vec::new();
    let mut skipped = Vec::new();
    let mut used_names = BTreeSet::new();

    for (path_template, path_item_val) in paths_obj {
        let resolved_item = doc.inline_refs(path_item_val)?;
        let Some(path_obj) = resolved_item.as_object() else {
            return Err(ExtractError::InvalidStructure(format!(
                "path item `{path_template}` must be an object"
            )));
        };

        for &method in HTTP_METHODS {
            if method == "get" {
                continue;
            }
            if let Some(non_get_val) = path_obj.get(method) {
                let op_id = non_get_val
                    .as_object()
                    .and_then(|o| o.get("operationId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let upper = method.to_ascii_uppercase();
                skipped.push(SkippedOperation {
                    path: path_template.clone(),
                    method: upper.clone(),
                    operation_id: op_id,
                    reason: format!("non-GET method `{upper}` is unsupported in v0"),
                });
            }
        }

        let Some(get_val) = path_obj.get("get") else {
            continue;
        };
        let Some(op_obj) = get_val.as_object() else {
            return Err(ExtractError::InvalidStructure(format!(
                "`GET {path_template}` operation must be an object"
            )));
        };
        let raw_op_id = op_obj
            .get("operationId")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let segments = match parse_path_template(path_template) {
            Ok(s) => s,
            Err(reason) => {
                skipped.push(SkippedOperation {
                    path: path_template.clone(),
                    method: "GET".to_owned(),
                    operation_id: raw_op_id,
                    reason,
                });
                continue;
            }
        };

        let raw_params = collect_merged_raw_params(doc, path_obj, op_obj)?;
        let mut skip_reason: Option<String> = None;
        let mut path_params_by_name: BTreeMap<String, ParameterSpec> = BTreeMap::new();
        let mut query_params: Vec<ParameterSpec> = Vec::new();
        let mut header_params: Vec<ParameterSpec> = Vec::new();
        let mut seen_arg_wire_names: BTreeSet<String> = BTreeSet::new();
        let mut used_sdl_args: BTreeSet<String> = BTreeSet::new();

        for p_val in raw_params {
            let Some(p_obj) = p_val.as_object() else {
                skip_reason = Some("parameter is not an object".to_owned());
                break;
            };
            let Some(p_name) = p_obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                skip_reason = Some("parameter is missing a non-empty `name`".to_owned());
                break;
            };
            let Some(p_in) = p_obj.get("in").and_then(Value::as_str) else {
                skip_reason = Some(format!("parameter `{p_name}` is missing `in`"));
                break;
            };

            if p_in == "cookie" {
                skip_reason = Some(format!("cookie parameter `{p_name}` is unsupported in v0"));
                break;
            }

            if p_in == "header" {
                let lower = p_name.to_ascii_lowercase();
                if IGNORED_OPENAPI_HEADERS.contains(&lower.as_str()) {
                    continue;
                }
                if FORBIDDEN_OPENAPI_HEADERS.contains(&lower.as_str()) {
                    skip_reason = Some(format!(
                        "forbidden header parameter `{p_name}` cannot be set by connector"
                    ));
                    break;
                }
                if !is_rfc7230_token(p_name) {
                    skip_reason = Some(format!(
                        "header parameter `{p_name}` is not a valid RFC 7230 token"
                    ));
                    break;
                }
            }

            if p_obj.contains_key("content") {
                skip_reason = Some(format!(
                    "parameter `{p_name}` uses `content` serialization, unsupported in v0"
                ));
                break;
            }

            let raw_style = p_obj.get("style").and_then(Value::as_str);
            if raw_style == Some("deepObject") {
                skip_reason = Some(format!(
                    "parameter `{p_name}` uses unsupported `deepObject` style"
                ));
                break;
            }

            let Some(schema_val) = p_obj.get("schema") else {
                skip_reason = Some(format!("parameter `{p_name}` is missing `schema`"));
                break;
            };
            let shape = match parse_param_shape(schema_val) {
                Ok(s) => s,
                Err(reason) => {
                    skip_reason = Some(format!("parameter `{p_name}`: {reason}"));
                    break;
                }
            };

            let required_flag = p_obj
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            match p_in {
                "path" => {
                    if !matches!(raw_style, None | Some("simple")) {
                        skip_reason =
                            Some(format!("path parameter `{p_name}` uses unsupported style"));
                        break;
                    }
                    if p_obj.get("required").and_then(Value::as_bool) == Some(false) {
                        skip_reason = Some(format!(
                            "path parameter `{p_name}` must have `required: true`"
                        ));
                        break;
                    }
                    if !matches!(shape, ParamShape::Scalar(_)) {
                        skip_reason = Some(format!(
                            "path parameter `{p_name}` must have a scalar schema"
                        ));
                        break;
                    }
                    if !seen_arg_wire_names.insert(p_name.to_owned()) {
                        skip_reason = Some(format!(
                            "duplicate parameter name `{p_name}` across locations"
                        ));
                        break;
                    }
                    let sdl_arg_name = sanitize_graphql_arg_name(p_name, &mut used_sdl_args);
                    path_params_by_name.insert(
                        p_name.to_owned(),
                        ParameterSpec {
                            name: p_name.to_owned(),
                            sdl_arg_name,
                            location: ParamLocation::Path,
                            style: ParamStyle::Simple,
                            required: true,
                            shape,
                            schema: schema_val.clone(),
                        },
                    );
                }
                "query" => {
                    if !matches!(raw_style, None | Some("form")) {
                        skip_reason =
                            Some(format!("query parameter `{p_name}` uses unsupported style"));
                        break;
                    }
                    let explode = p_obj
                        .get("explode")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if !seen_arg_wire_names.insert(p_name.to_owned()) {
                        skip_reason = Some(format!(
                            "duplicate parameter name `{p_name}` across locations"
                        ));
                        break;
                    }
                    let sdl_arg_name = sanitize_graphql_arg_name(p_name, &mut used_sdl_args);
                    query_params.push(ParameterSpec {
                        name: p_name.to_owned(),
                        sdl_arg_name,
                        location: ParamLocation::Query,
                        style: ParamStyle::Form { explode },
                        required: required_flag,
                        shape,
                        schema: schema_val.clone(),
                    });
                }
                "header" => {
                    if !matches!(raw_style, None | Some("simple")) {
                        skip_reason = Some(format!(
                            "header parameter `{p_name}` uses unsupported style"
                        ));
                        break;
                    }
                    if !matches!(shape, ParamShape::Scalar(_)) {
                        skip_reason = Some(format!(
                            "header parameter `{p_name}` must have a scalar schema"
                        ));
                        break;
                    }
                    if !seen_arg_wire_names.insert(p_name.to_owned()) {
                        skip_reason = Some(format!(
                            "duplicate parameter name `{p_name}` across locations"
                        ));
                        break;
                    }
                    let sdl_arg_name = sanitize_graphql_arg_name(p_name, &mut used_sdl_args);
                    header_params.push(ParameterSpec {
                        name: p_name.to_owned(),
                        sdl_arg_name,
                        location: ParamLocation::Header,
                        style: ParamStyle::Simple,
                        required: required_flag,
                        shape,
                        schema: schema_val.clone(),
                    });
                }
                other => {
                    skip_reason = Some(format!("unsupported parameter location `{other}`"));
                    break;
                }
            }
        }

        if let Some(reason) = skip_reason {
            skipped.push(SkippedOperation {
                path: path_template.clone(),
                method: "GET".to_owned(),
                operation_id: raw_op_id,
                reason,
            });
            continue;
        }

        // Order path parameters strictly by path-template appearance and verify 1-to-1 match.
        let mut ordered_path_params = Vec::new();
        let mut key_fields = Vec::new();
        let mut template_mismatch: Option<String> = None;
        for seg in &segments {
            if let PathSegment::Param(slot) = seg {
                if let Some(spec) = path_params_by_name.remove(slot) {
                    key_fields.push(slot.clone());
                    ordered_path_params.push(spec);
                } else {
                    template_mismatch = Some(format!(
                        "path placeholder `{{{slot}}}` has no declared path parameter"
                    ));
                    break;
                }
            }
        }
        if template_mismatch.is_none() && !path_params_by_name.is_empty() {
            template_mismatch =
                Some("declared path parameter does not appear in path template".to_owned());
        }
        if let Some(reason) = template_mismatch {
            skipped.push(SkippedOperation {
                path: path_template.clone(),
                method: "GET".to_owned(),
                operation_id: raw_op_id,
                reason,
            });
            continue;
        }

        query_params.sort_by(|a, b| a.name.cmp(&b.name));
        header_params.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });

        let has_limit = query_params
            .iter()
            .any(|p| p.name == "limit" && p.shape == ParamShape::Scalar(ParamPrimitive::Integer));
        let has_offset = query_params
            .iter()
            .any(|p| p.name == "offset" && p.shape == ParamShape::Scalar(ParamPrimitive::Integer));
        let has_offset_pagination = has_limit && has_offset;

        let sort_param = query_params
            .iter()
            .find(|p| p.name == "sort" && p.shape == ParamShape::Scalar(ParamPrimitive::String));
        let has_sort = sort_param.is_some();
        let sort_allowed_values = sort_param
            .and_then(|p| p.schema.get("enum"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            });

        let native_batch = match op_obj.get("x-gap-in-param") {
            Some(ext) => {
                let parsed = parse_native_batch(ext)?;
                if key_fields.len() != 1 {
                    return Err(ExtractError::InvalidStructure(format!(
                        "`x-gap-in-param` on `{path_template}` requires a single-keyed path read"
                    )));
                }
                Some(parsed)
            }
            None => None,
        };

        let (response_type_hint, response_schema) = extract_response_schema(doc, op_obj)?;

        let mut parameters = Vec::with_capacity(
            ordered_path_params.len() + query_params.len() + header_params.len(),
        );
        parameters.extend(ordered_path_params);
        parameters.extend(query_params);
        parameters.extend(header_params);

        let raw_summary = op_obj
            .get("summary")
            .and_then(Value::as_str)
            .or_else(|| op_obj.get("description").and_then(Value::as_str))
            .unwrap_or("");
        let summary = clip_summary(raw_summary);
        let name = make_operation_name(raw_op_id.as_deref(), &segments, &mut used_names);

        operations.push(ExtractedOperation {
            name,
            raw_operation_id: raw_op_id,
            path_template: path_template.clone(),
            segments,
            summary,
            parameters,
            key_fields,
            has_offset_pagination,
            has_sort,
            sort_allowed_values,
            native_batch,
            response_type_hint,
            response_schema,
        });
    }

    // Cross-validate every `x-gap-in-param` target against extracted GET operations and resolve
    // `limit` capping for server-side paginated batch endpoints (`CMP-032`).
    let params_by_path: BTreeMap<String, Vec<ParameterSpec>> = operations
        .iter()
        .map(|op| (op.path_template.clone(), op.parameters.clone()))
        .collect();

    for op in &mut operations {
        if let Some(nb) = &mut op.native_batch {
            let Some(target_params) = params_by_path.get(&nb.path) else {
                return Err(ExtractError::InvalidStructure(format!(
                    "`x-gap-in-param` on `{}` references missing GET path `{}`",
                    op.path_template, nb.path
                )));
            };
            let has_array_param = target_params.iter().any(|p| {
                p.location == ParamLocation::Query
                    && p.name == nb.param
                    && matches!(p.shape, ParamShape::Array(_))
            });
            if !has_array_param {
                return Err(ExtractError::InvalidStructure(format!(
                    "`x-gap-in-param` target `{}` lacks array query parameter `{}`",
                    nb.path, nb.param
                )));
            }
            if let Some(limit_param) = target_params.iter().find(|p| {
                p.location == ParamLocation::Query
                    && p.name == "limit"
                    && p.shape == ParamShape::Scalar(ParamPrimitive::Integer)
            }) {
                nb.target_has_limit = true;
                if let Some(max_f) = limit_param
                    .schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .filter(|f| f.is_finite())
                {
                    let max_int = max_f.floor();
                    if max_int < 1.0 {
                        return Err(ExtractError::InvalidStructure(format!(
                            "`x-gap-in-param` target `{}` has `limit.maximum` < 1",
                            nb.path
                        )));
                    }
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let cap = if max_int >= f64::from(u32::MAX) {
                        u32::MAX
                    } else {
                        max_int as u32
                    };
                    nb.max_batch_size = nb.max_batch_size.min(cap);
                }
            }
        }
    }

    Ok(Extraction {
        operations,
        report: ExtractionReport {
            openapi_version: doc.openapi_version().to_owned(),
            spec_hash: doc.spec_hash().to_owned(),
            servers: doc.servers().to_vec(),
            skipped,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn shop_bytes() -> Vec<u8> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/openapi/shop.openapi.json");
        std::fs::read(p).expect("read shop.openapi.json")
    }

    // spec: ING-010, CMP-030, CMP-032
    #[test]
    fn golden_extraction_of_shop_document_is_deterministic_and_complete() {
        let bytes = shop_bytes();
        let doc1 = OpenApiDocument::parse(&bytes).expect("parse 1");
        let doc2 = OpenApiDocument::parse(&bytes).expect("parse 2");
        let ext1 = extract_operations(&doc1).expect("extract 1");
        let ext2 = extract_operations(&doc2).expect("extract 2");
        assert_eq!(ext1, ext2, "same bytes must produce identical Extraction");

        let names: Vec<&str> = ext1.operations.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "blobs",
                "customers",
                "customer",
                "orders",
                "order",
                "product",
                "slow"
            ]
        );

        let customer = ext1
            .operations
            .iter()
            .find(|o| o.name == "customer")
            .expect("customer op");
        assert_eq!(customer.key_fields, vec!["id".to_owned()]);
        assert_eq!(
            customer.native_batch,
            Some(NativeBatchSpec {
                path: "/customers".to_owned(),
                param: "ids".to_owned(),
                key_field: "id".to_owned(),
                max_batch_size: 50,
                target_has_limit: true,
            })
        );
        // `Authorization` header parameter in OpenAPI must be ignored, while `X-Request-Tag` is kept.
        let param_names: Vec<&str> = customer
            .parameters
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(param_names, vec!["id", "X-Request-Tag"]);

        let customers = ext1
            .operations
            .iter()
            .find(|o| o.name == "customers")
            .expect("customers op");
        assert!(customers.has_offset_pagination);
        assert!(customers.has_sort);
        assert!(customers.key_fields.is_empty());

        let order = ext1
            .operations
            .iter()
            .find(|o| o.name == "order")
            .expect("order op");
        assert_eq!(order.key_fields, vec!["id".to_owned()]);
        assert!(order.native_batch.is_none());

        // Non-GET `POST /orders` is recorded as skipped, never exposed as an operation.
        assert_eq!(ext1.report.servers, vec!["https://api.shop.internal/v1"]);
        assert_eq!(ext1.report.skipped.len(), 1);
        assert_eq!(ext1.report.skipped[0].path, "/orders");
        assert_eq!(ext1.report.skipped[0].method, "POST");
    }

    // spec: ING-010, NGC-012, GQL-004, CMP-030
    #[test]
    fn sanitizes_names_resolves_collisions_and_skips_unsupported_parameters() {
        let raw = json!({
            "openapi": "3.1.0",
            "info": { "title": "EdgeCases", "version": "1.0.0" },
            "paths": {
                "/v1/items-list": {
                    "get": {
                        "operationId": "123-bad!id",
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/v2/items-list": {
                    "get": {
                        "operationId": "123_bad_id",
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/reserved-gap": {
                    "get": {
                        "operationId": "gap",
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/dunder": {
                    "get": {
                        "operationId": "__secret",
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/unnamed/{item_id}": {
                    "get": {
                        "parameters": [
                            { "name": "item_id", "in": "path", "required": true, "schema": { "type": "string" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/skip-cookie": {
                    "get": {
                        "parameters": [
                            { "name": "sid", "in": "cookie", "schema": { "type": "string" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/skip-deep": {
                    "get": {
                        "parameters": [
                            { "name": "filter", "in": "query", "style": "deepObject", "schema": { "type": "object" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                },
                "/skip-host": {
                    "get": {
                        "parameters": [
                            { "name": "Host", "in": "header", "schema": { "type": "string" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                }
            }
        });
        let doc = OpenApiDocument::parse(&serde_json::to_vec(&raw).expect("json")).expect("parse");
        let ext = extract_operations(&doc).expect("extract");

        let names: Vec<&str> = ext.operations.iter().map(|o| o.name.as_str()).collect();
        assert!(names.contains(&"op_secret"), "got {names:?}");
        assert!(names.contains(&"op_gap"), "got {names:?}");
        assert!(names.contains(&"get_unnamed_by_item_id"), "got {names:?}");
        assert!(names.contains(&"op_123_bad_id"), "got {names:?}");
        assert!(names.contains(&"op_123_bad_id_2"), "got {names:?}");

        let skipped_paths: Vec<&str> = ext.report.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            skipped_paths,
            vec!["/skip-cookie", "/skip-deep", "/skip-host"]
        );
    }

    // spec: CMP-030, ING-010
    #[test]
    fn t3_hostile_path_literals_are_skipped_with_unsafe_path_literal() {
        let hostile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/openapi/hostile/path_literals.json");
        let bytes = std::fs::read(hostile_path).expect("read hostile/path_literals.json");
        let doc = OpenApiDocument::parse(&bytes).expect("parse hostile path_literals.json");
        let ext = extract_operations(&doc).expect("extract hostile path_literals.json");

        assert!(
            ext.operations.is_empty(),
            "all 5 unsafe literal paths must be skipped: {:?}",
            ext.operations
        );
        let skipped: Vec<(&str, &str)> = ext
            .report
            .skipped
            .iter()
            .map(|s| (s.path.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            skipped,
            vec![
                ("/a/../b", "unsafe_path_literal"),
                ("/c/%2e%2e/d", "unsafe_path_literal"),
                ("/e\\f", "unsafe_path_literal"),
                ("/g?x", "unsafe_path_literal"),
                ("/h#i", "unsafe_path_literal"),
            ]
        );

        // Additional dot-spelling, space, and control-character literal segments must also skip.
        let extra = json!({
            "openapi": "3.0.3",
            "info": { "title": "extra", "version": "1" },
            "paths": {
                "/.": { "get": { "operationId": "d1", "responses": { "200": { "description": "ok" } } } },
                "/.%2e": { "get": { "operationId": "d2", "responses": { "200": { "description": "ok" } } } },
                "/%2e.": { "get": { "operationId": "d3", "responses": { "200": { "description": "ok" } } } },
                "/has space": { "get": { "operationId": "sp", "responses": { "200": { "description": "ok" } } } },
                "/ctrl\u{0007}bell": { "get": { "operationId": "ct", "responses": { "200": { "description": "ok" } } } }
            },
            "components": { "schemas": {} }
        });
        let extra_doc =
            OpenApiDocument::parse(&serde_json::to_vec(&extra).expect("json")).expect("parse");
        let extra_ext = extract_operations(&extra_doc).expect("extract");
        assert!(extra_ext.operations.is_empty());
        assert_eq!(extra_ext.report.skipped.len(), 5);
        for item in &extra_ext.report.skipped {
            assert_eq!(item.reason, "unsafe_path_literal", "path={}", item.path);
        }

        // Shop golden extraction remains identical.
        let shop_doc = OpenApiDocument::parse(&shop_bytes()).expect("parse shop");
        let shop_ext = extract_operations(&shop_doc).expect("extract shop");
        let shop_names: Vec<&str> = shop_ext
            .operations
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(
            shop_names,
            vec![
                "blobs",
                "customers",
                "customer",
                "orders",
                "order",
                "product",
                "slow"
            ]
        );
    }
}
