//! Injection-safe HTTP request builder and input schema validator (`CMP-030`, `NGC-060`).

use axum::http::{HeaderName, HeaderValue};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::RegexBuilder;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::extract::{
    ExtractedOperation, ParamLocation, ParamPrimitive, ParamShape, ParamStyle, PathSegment,
    is_rfc7230_token,
};

/// Maximum compiled NFA/DFA size in bytes for a JSON Schema `pattern` regular expression (`ING-012`).
const MAX_PATTERN_REGEX_BYTES: usize = 1 << 20;

/// Exact integer boundary representable without precision loss in IEEE-754 `f64` (`2^53`).
const MAX_EXACT_F64_INT: f64 = 9_007_199_254_740_992.0;

/// Percent-encode set leaving only RFC 3986 `unreserved` (`ALPHA / DIGIT / "-" / "." / "_" / "~"`) raw (`CMP-030`).
const RFC3986_NON_UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Relative HTTP request path-and-query and headers produced by [`build_request`] (`CMP-030`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltRequest {
    /// Relative path and optional `?query`, always starting with `'/'` and never carrying scheme or authority.
    pub path_and_query: String,
    /// Validated HTTP request headers in deterministic order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

/// Validation or encoding failure produced by [`validate_arguments`] or [`build_request`] (`CMP-030`, `NGC-060`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RequestBuildError {
    /// Argument failed JSON Schema validation (`NGC-060`).
    #[error("invalid argument: {0}")]
    SchemaViolation(String),
    /// Path parameter resolved to an empty string (`CMP-030`).
    #[error("path parameter `{0}` must not be empty")]
    EmptyPathParameter(String),
    /// Header name was not a valid RFC 7230 token (`CMP-030`).
    #[error("header parameter `{0}` is not a valid HTTP header name")]
    InvalidHeaderName(String),
    /// Header value contained CR, LF, NUL, or invalid header bytes (`CMP-030`).
    #[error("header parameter `{0}` contains forbidden control characters or invalid bytes")]
    InvalidHeaderValue(String),
}

impl RequestBuildError {
    /// Converts this error into an [`NgcErrorInfo`] with [`NgcErrorCode::InvalidArgument`] (`NGC-060`).
    #[must_use]
    pub fn into_ngc_error(self) -> NgcErrorInfo {
        NgcErrorInfo::new(NgcErrorCode::InvalidArgument, self.to_string())
    }
}

/// Percent-encodes every byte outside RFC 3986 `unreserved` (`CMP-030`).
#[must_use]
pub fn encode_rfc3986_unreserved(raw: &str) -> String {
    utf8_percent_encode(raw, RFC3986_NON_UNRESERVED).to_string()
}

/// Encodes a single path parameter value so it cannot introduce `/`, `?`, `#`, empty segments, or `.`/`..` (`CMP-030`).
pub fn encode_path_segment_value(param_name: &str, raw: &str) -> Result<String, RequestBuildError> {
    if raw.is_empty() {
        return Err(RequestBuildError::EmptyPathParameter(param_name.to_owned()));
    }
    let encoded = encode_rfc3986_unreserved(raw);
    match encoded.as_str() {
        "." => Ok("%2E".to_owned()),
        ".." => Ok("%2E%2E".to_owned()),
        _ => Ok(encoded),
    }
}

/// Appends one percent-encoded `name=value` pair onto `path_and_query` (`CMP-030`).
pub fn append_encoded_query_pair(path_and_query: &mut String, name: &str, value: &str) {
    let sep = if path_and_query.contains('?') {
        '&'
    } else {
        '?'
    };
    path_and_query.push(sep);
    path_and_query.push_str(&encode_rfc3986_unreserved(name));
    path_and_query.push('=');
    path_and_query.push_str(&encode_rfc3986_unreserved(value));
}

/// Validates and constructs an HTTP header pair, rejecting CR, LF, NUL, and control characters (`CMP-030`).
pub fn build_safe_header(
    name: &str,
    value: &str,
) -> Result<(HeaderName, HeaderValue), RequestBuildError> {
    if !is_rfc7230_token(name) {
        return Err(RequestBuildError::InvalidHeaderName(name.to_owned()));
    }
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| RequestBuildError::InvalidHeaderName(name.to_owned()))?;
    if value
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == 0 || (b < 0x20 && b != b'\t') || b == 0x7f)
    {
        return Err(RequestBuildError::InvalidHeaderValue(name.to_owned()));
    }
    let header_value = HeaderValue::from_str(value)
        .map_err(|_| RequestBuildError::InvalidHeaderValue(name.to_owned()))?;
    Ok((header_name, header_value))
}

/// Extracts an exact `i64` from a JSON number, accepting protobuf `Struct` whole-number `f64` values (`1.0` -> `1`).
#[must_use]
pub fn json_number_as_exact_i64(val: &Value) -> Option<i64> {
    let num = val.as_number()?;
    if let Some(i) = num.as_i64() {
        return Some(i);
    }
    if let Some(u) = num.as_u64() {
        return i64::try_from(u).ok();
    }
    let f = num.as_f64()?;
    if f.is_finite() && f.fract() == 0.0 && (-MAX_EXACT_F64_INT..=MAX_EXACT_F64_INT).contains(&f) {
        #[allow(clippy::cast_possible_truncation)]
        return Some(f as i64);
    }
    None
}

fn json_values_equal(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(fa), Some(fb)) if a.is_number() && b.is_number() => (fa - fb).abs() == 0.0,
        _ => a == b,
    }
}

fn value_matches_single_type(type_name: &str, val: &Value) -> bool {
    match type_name {
        "null" => val.is_null(),
        "boolean" => val.is_boolean(),
        "string" => val.is_string(),
        "integer" => json_number_as_exact_i64(val).is_some(),
        "number" => val.as_f64().is_some_and(f64::is_finite),
        "array" => val.is_array(),
        "object" => val.is_object(),
        _ => true,
    }
}

fn validate_value_against_schema(
    path: &str,
    schema: &Value,
    val: &Value,
) -> Result<(), RequestBuildError> {
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };

    if let Some(type_val) = obj.get("type") {
        let matched = match type_val {
            Value::String(t) => value_matches_single_type(t, val),
            Value::Array(arr) => arr
                .iter()
                .filter_map(Value::as_str)
                .any(|t| value_matches_single_type(t, val)),
            _ => true,
        };
        if !matched {
            return Err(RequestBuildError::SchemaViolation(format!(
                "argument `{path}` has wrong type"
            )));
        }
    }

    if let Some(enum_arr) = obj.get("enum").and_then(Value::as_array)
        && !enum_arr
            .iter()
            .any(|allowed| json_values_equal(allowed, val))
    {
        return Err(RequestBuildError::SchemaViolation(format!(
            "argument `{path}` is not one of the allowed enum values"
        )));
    }

    if let Some(num) = val.as_f64()
        && val.is_number()
    {
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64)
            && num < min
        {
            return Err(RequestBuildError::SchemaViolation(format!(
                "argument `{path}` is below minimum {min}"
            )));
        }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64)
            && num > max
        {
            return Err(RequestBuildError::SchemaViolation(format!(
                "argument `{path}` exceeds maximum {max}"
            )));
        }
    }

    if let Some(s) = val.as_str() {
        let char_len = s.chars().count();
        if let Some(min_len) = obj
            .get("minLength")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            && char_len < min_len
        {
            return Err(RequestBuildError::SchemaViolation(format!(
                "argument `{path}` is shorter than minLength {min_len}"
            )));
        }
        if let Some(max_len) = obj
            .get("maxLength")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            && char_len > max_len
        {
            return Err(RequestBuildError::SchemaViolation(format!(
                "argument `{path}` exceeds maxLength {max_len}"
            )));
        }
        if let Some(pat) = obj.get("pattern").and_then(Value::as_str) {
            let re = RegexBuilder::new(pat)
                .size_limit(MAX_PATTERN_REGEX_BYTES)
                .build()
                .map_err(|_| {
                    RequestBuildError::SchemaViolation(format!(
                        "schema pattern on `{path}` could not be compiled"
                    ))
                })?;
            if !re.is_match(s) {
                return Err(RequestBuildError::SchemaViolation(format!(
                    "argument `{path}` does not match required pattern"
                )));
            }
        }
    }

    if let Some(items_val) = val.as_array()
        && let Some(item_schema) = obj.get("items")
    {
        for (idx, elem) in items_val.iter().enumerate() {
            validate_value_against_schema(&format!("{path}[{idx}]"), item_schema, elem)?;
        }
    }

    if let Some(map_val) = val.as_object() {
        validate_object_map(path, obj, map_val)?;
    }

    Ok(())
}

fn validate_object_map(
    prefix: &str,
    schema_obj: &Map<String, Value>,
    arguments: &Map<String, Value>,
) -> Result<(), RequestBuildError> {
    let props = schema_obj.get("properties").and_then(Value::as_object);
    let disallow_extra = schema_obj
        .get("additionalProperties")
        .and_then(Value::as_bool)
        == Some(false);

    if let Some(req_arr) = schema_obj.get("required").and_then(Value::as_array) {
        for req_key in req_arr.iter().filter_map(Value::as_str) {
            let allows_null = props.and_then(|p| p.get(req_key)).is_some_and(|sub| {
                value_matches_single_type("null", &Value::Null) && {
                    sub.as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(Value::as_array)
                        .is_some_and(|arr| arr.iter().any(|t| t.as_str() == Some("null")))
                }
            });
            match arguments.get(req_key) {
                None => {
                    return Err(RequestBuildError::SchemaViolation(format!(
                        "missing required argument `{req_key}`"
                    )));
                }
                Some(Value::Null) if !allows_null => {
                    return Err(RequestBuildError::SchemaViolation(format!(
                        "required argument `{req_key}` must not be null"
                    )));
                }
                Some(_) => {}
            }
        }
    }

    for (key, val) in arguments {
        let field_path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match props.and_then(|p| p.get(key)) {
            Some(prop_schema) => {
                if val.is_null() {
                    let is_req = schema_obj
                        .get("required")
                        .and_then(Value::as_array)
                        .is_some_and(|arr| arr.iter().any(|r| r.as_str() == Some(key.as_str())));
                    if !is_req {
                        continue;
                    }
                }
                validate_value_against_schema(&field_path, prop_schema, val)?;
            }
            None if disallow_extra => {
                return Err(RequestBuildError::SchemaViolation(format!(
                    "unknown argument `{field_path}` is not allowed"
                )));
            }
            None => {}
        }
    }
    Ok(())
}

/// Validates `arguments` against `input_schema` (`NGC-060`).
pub fn validate_arguments(
    input_schema: &Value,
    arguments: &Map<String, Value>,
) -> Result<(), RequestBuildError> {
    let Some(schema_obj) = input_schema.as_object() else {
        return Err(RequestBuildError::SchemaViolation(
            "operation input_schema must be an object".to_owned(),
        ));
    };
    validate_object_map("", schema_obj, arguments)
}

/// Formats a validated scalar JSON value according to `prim` (`CMP-030`).
pub fn format_scalar_value(
    param_name: &str,
    prim: ParamPrimitive,
    val: &Value,
) -> Result<String, RequestBuildError> {
    match prim {
        ParamPrimitive::String => val.as_str().map(str::to_owned).ok_or_else(|| {
            RequestBuildError::SchemaViolation(format!("argument `{param_name}` must be a string"))
        }),
        ParamPrimitive::Integer => json_number_as_exact_i64(val)
            .map(|i| i.to_string())
            .ok_or_else(|| {
                RequestBuildError::SchemaViolation(format!(
                    "argument `{param_name}` must be an integer"
                ))
            }),
        ParamPrimitive::Number => val
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| {
                if f.fract() == 0.0 && (-MAX_EXACT_F64_INT..=MAX_EXACT_F64_INT).contains(&f) {
                    #[allow(clippy::cast_possible_truncation)]
                    let i = f as i64;
                    i.to_string()
                } else {
                    f.to_string()
                }
            })
            .ok_or_else(|| {
                RequestBuildError::SchemaViolation(format!(
                    "argument `{param_name}` must be a finite number"
                ))
            }),
        ParamPrimitive::Boolean => val.as_bool().map(|b| b.to_string()).ok_or_else(|| {
            RequestBuildError::SchemaViolation(format!("argument `{param_name}` must be a boolean"))
        }),
    }
}

/// Validates `arguments` against `input_schema` and builds a relative [`BuiltRequest`] (`CMP-030`, `NGC-060`).
pub fn build_request(
    op: &ExtractedOperation,
    input_schema: &Value,
    arguments: &Map<String, Value>,
) -> Result<BuiltRequest, RequestBuildError> {
    validate_arguments(input_schema, arguments)?;

    let mut rendered_segments = Vec::with_capacity(op.segments.len());
    for seg in &op.segments {
        match seg {
            PathSegment::Literal(lit) => rendered_segments.push(lit.clone()),
            PathSegment::Param(slot) => {
                let Some(param) = op
                    .parameters
                    .iter()
                    .find(|p| p.location == ParamLocation::Path && p.name == *slot)
                else {
                    return Err(RequestBuildError::SchemaViolation(format!(
                        "missing path parameter definition `{slot}`"
                    )));
                };
                let Some(val) = arguments.get(slot) else {
                    return Err(RequestBuildError::SchemaViolation(format!(
                        "missing required path argument `{slot}`"
                    )));
                };
                let ParamShape::Scalar(prim) = param.shape else {
                    return Err(RequestBuildError::SchemaViolation(format!(
                        "path parameter `{slot}` must be scalar"
                    )));
                };
                let raw_str = format_scalar_value(slot, prim, val)?;
                rendered_segments.push(encode_path_segment_value(slot, &raw_str)?);
            }
        }
    }

    let mut path_and_query = if rendered_segments.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", rendered_segments.join("/"))
    };

    let mut headers = Vec::new();
    for param in &op.parameters {
        let Some(val) = arguments.get(&param.name) else {
            continue;
        };
        if val.is_null() {
            continue;
        }
        match param.location {
            ParamLocation::Path => {}
            ParamLocation::Query => {
                let explode = match param.style {
                    ParamStyle::Form { explode } => explode,
                    ParamStyle::Simple => true,
                };
                match param.shape {
                    ParamShape::Scalar(prim) => {
                        let raw_val = format_scalar_value(&param.name, prim, val)?;
                        append_encoded_query_pair(&mut path_and_query, &param.name, &raw_val);
                    }
                    ParamShape::Array(prim) => {
                        let Some(arr) = val.as_array() else {
                            return Err(RequestBuildError::SchemaViolation(format!(
                                "argument `{}` must be an array",
                                param.name
                            )));
                        };
                        if arr.is_empty() {
                            continue;
                        }
                        if explode {
                            for item in arr {
                                let raw_item = format_scalar_value(&param.name, prim, item)?;
                                append_encoded_query_pair(
                                    &mut path_and_query,
                                    &param.name,
                                    &raw_item,
                                );
                            }
                        } else {
                            let mut encoded_items = Vec::with_capacity(arr.len());
                            for item in arr {
                                let raw_item = format_scalar_value(&param.name, prim, item)?;
                                encoded_items.push(encode_rfc3986_unreserved(&raw_item));
                            }
                            let sep = if path_and_query.contains('?') {
                                '&'
                            } else {
                                '?'
                            };
                            path_and_query.push(sep);
                            path_and_query.push_str(&encode_rfc3986_unreserved(&param.name));
                            path_and_query.push('=');
                            path_and_query.push_str(&encoded_items.join(","));
                        }
                    }
                }
            }
            ParamLocation::Header => {
                let ParamShape::Scalar(prim) = param.shape else {
                    return Err(RequestBuildError::SchemaViolation(format!(
                        "header parameter `{}` must be scalar",
                        param.name
                    )));
                };
                let raw_val = format_scalar_value(&param.name, prim, val)?;
                headers.push(build_safe_header(&param.name, &raw_val)?);
            }
        }
    }

    Ok(BuiltRequest {
        path_and_query,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::document::OpenApiDocument;
    use crate::extract::extract_operations;
    use crate::schema::{SchemaBundle, build_schema_bundle};

    fn load_shop() -> (Vec<ExtractedOperation>, SchemaBundle) {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/openapi/shop.openapi.json");
        let bytes = std::fs::read(p).expect("read shop");
        let doc = OpenApiDocument::parse(&bytes).expect("parse");
        let ext = extract_operations(&doc).expect("extract");
        let bundle = build_schema_bundle(&doc, &ext).expect("bundle");
        (ext.operations, bundle)
    }

    // spec: NGC-060, CMP-030
    #[test]
    fn schema_validator_accepts_valid_inputs_and_rejects_every_bad_class() {
        let (ops, bundle) = load_shop();
        let customer = ops.iter().find(|o| o.name == "customer").expect("customer");
        let schema = &bundle.operations["customer"].input_json_schema;

        // Valid integer and protobuf-style 1.0 float representation of integer
        let ok_args = json!({ "id": 1.0, "X-Request-Tag": "req_01" })
            .as_object()
            .cloned()
            .expect("obj");
        let built = build_request(customer, schema, &ok_args).expect("valid request");
        assert_eq!(built.path_and_query, "/customers/1");
        assert_eq!(built.headers.len(), 1);
        assert_eq!(built.headers[0].0.as_str(), "x-request-tag");
        assert_eq!(built.headers[0].1.to_str().expect("str"), "req_01");

        // Missing required `id`
        let missing = Map::new();
        assert!(build_request(customer, schema, &missing).is_err());

        // Wrong type (`id: "abc"` or fractional `1.5`), and never echoes caller value
        let canary = "SECRET-ARG-CANARY-99";
        for bad in [
            json!({ "id": canary }),
            json!({ "id": 1.5 }),
            json!({ "id": 0 }),
        ] {
            let m = bad.as_object().cloned().expect("obj");
            let err = build_request(customer, schema, &m).expect_err("expected failure");
            assert!(
                !err.to_string().contains(canary),
                "validator must never echo caller argument values: {err}"
            );
        }

        // Unknown argument (`additionalProperties: false`)
        let unknown = json!({ "id": 1, "extra": true })
            .as_object()
            .cloned()
            .expect("obj");
        assert!(build_request(customer, schema, &unknown).is_err());

        // Exact `maxLength` (64 chars ok, 65 chars > 64 rejected), `minimum`/`maximum` on `limit` (1 and 200 ok, 0 and 201 rejected), and failing `pattern`
        let exact_len = json!({ "id": 1, "X-Request-Tag": "a".repeat(64) })
            .as_object()
            .cloned()
            .expect("obj");
        assert!(build_request(customer, schema, &exact_len).is_ok());

        let too_long = "a".repeat(65);
        let over_len = json!({ "id": 1, "X-Request-Tag": too_long })
            .as_object()
            .cloned()
            .expect("obj");
        assert!(build_request(customer, schema, &over_len).is_err());

        let customers = ops
            .iter()
            .find(|o| o.name == "customers")
            .expect("customers");
        let cust_schema = &bundle.operations["customers"].input_json_schema;
        for ok_lim in [1, 200] {
            let m = json!({ "limit": ok_lim })
                .as_object()
                .cloned()
                .expect("obj");
            assert!(build_request(customers, cust_schema, &m).is_ok());
        }
        for bad_lim in [0, 201] {
            let m = json!({ "limit": bad_lim })
                .as_object()
                .cloned()
                .expect("obj");
            assert!(build_request(customers, cust_schema, &m).is_err());
        }

        let bad_pat = json!({ "id": 1, "X-Request-Tag": "bad space!" })
            .as_object()
            .cloned()
            .expect("obj");
        assert!(build_request(customer, schema, &bad_pat).is_err());
    }

    // spec: CMP-030
    #[test]
    fn encodes_dot_segments_slashes_queries_fragments_arrays_and_rejects_crlf_headers() {
        let (ops, bundle) = load_shop();
        let product = ops.iter().find(|o| o.name == "product").expect("product");
        let prod_schema = &bundle.operations["product"].input_json_schema;

        for (sku, expected) in [
            (".", "/products/%2E"),
            ("..", "/products/%2E%2E"),
            ("../admin?x=1#frag", "/products/..%2Fadmin%3Fx%3D1%23frag"),
            ("%2e%2e/secret", "/products/%252e%252e%2Fsecret"),
            ("a/b/c", "/products/a%2Fb%2Fc"),
        ] {
            let args = json!({ "sku": sku }).as_object().cloned().expect("obj");
            let built = build_request(product, prod_schema, &args).expect("build");
            assert_eq!(built.path_and_query, expected, "sku={sku:?}");
        }

        // Empty path string rejected so segment count cannot collapse
        let raw_empty = json!({
            "openapi": "3.1.0",
            "info": { "title": "EmptyPath", "version": "1.0.0" },
            "paths": {
                "/p/{k}": {
                    "get": {
                        "operationId": "p",
                        "parameters": [
                            { "name": "k", "in": "path", "required": true, "schema": { "type": "string" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                }
            }
        });
        let ep_doc =
            OpenApiDocument::parse(&serde_json::to_vec(&raw_empty).expect("json")).expect("parse");
        let ep_ext = extract_operations(&ep_doc).expect("extract");
        let ep_bundle = build_schema_bundle(&ep_doc, &ep_ext).expect("bundle");
        let empty_args = json!({ "k": "" }).as_object().cloned().expect("obj");
        assert!(matches!(
            build_request(
                &ep_ext.operations[0],
                &ep_bundle.operations["p"].input_json_schema,
                &empty_args
            ),
            Err(RequestBuildError::EmptyPathParameter(_))
        ));

        // Array query encoding: `ids` has explode=false (`ids=1,2,3`), `status` on `orders` has explode=true
        let customers = ops
            .iter()
            .find(|o| o.name == "customers")
            .expect("customers");
        let cust_schema = &bundle.operations["customers"].input_json_schema;
        let args = json!({ "ids": [1, 2, 3], "region": "eu" })
            .as_object()
            .cloned()
            .expect("obj");
        let built = build_request(customers, cust_schema, &args).expect("customers req");
        assert_eq!(built.path_and_query, "/customers?ids=1,2,3&region=eu");

        let orders = ops.iter().find(|o| o.name == "orders").expect("orders");
        let ord_schema = &bundle.operations["orders"].input_json_schema;
        let args = json!({ "status": ["pending", "paid"] })
            .as_object()
            .cloned()
            .expect("obj");
        let built = build_request(orders, ord_schema, &args).expect("orders req");
        assert_eq!(built.path_and_query, "/orders?status=pending&status=paid");

        // Header CR/LF/NUL injection check on an unrestricted header schema
        let raw_hdr_doc = json!({
            "openapi": "3.1.0",
            "info": { "title": "Hdr", "version": "1.0.0" },
            "paths": {
                "/h": {
                    "get": {
                        "operationId": "h",
                        "parameters": [
                            { "name": "X-Free", "in": "header", "required": true, "schema": { "type": "string" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                }
            }
        });
        let doc = OpenApiDocument::parse(&serde_json::to_vec(&raw_hdr_doc).expect("json"))
            .expect("parse");
        let ext = extract_operations(&doc).expect("extract");
        let h_bundle = build_schema_bundle(&doc, &ext).expect("bundle");
        let h_op = &ext.operations[0];
        let h_schema = &h_bundle.operations["h"].input_json_schema;

        for injected in ["ok\r\nX-Injected: evil", "ok\nevil", "ok\revil", "ok\0evil"] {
            let args = json!({ "X-Free": injected })
                .as_object()
                .cloned()
                .expect("obj");
            let err = build_request(h_op, h_schema, &args).expect_err("CRLF/NUL must fail");
            assert!(
                matches!(err, RequestBuildError::InvalidHeaderValue(_)),
                "got {err:?} for {injected:?}"
            );
        }
    }

    fn build_prop_fixture() -> (ExtractedOperation, Value) {
        let raw = json!({
            "openapi": "3.1.0",
            "info": { "title": "Prop", "version": "1.0.0" },
            "paths": {
                "/items/{sku}/detail": {
                    "get": {
                        "operationId": "detail",
                        "parameters": [
                            { "name": "sku", "in": "path", "required": true, "schema": { "type": "string", "minLength": 1, "maxLength": 4096 } },
                            { "name": "q", "in": "query", "required": false, "schema": { "type": "string" } },
                            { "name": "X-Custom", "in": "header", "required": false, "schema": { "type": "string" } }
                        ],
                        "responses": { "200": { "description": "ok" } }
                    }
                }
            }
        });
        let doc = OpenApiDocument::parse(&serde_json::to_vec(&raw).expect("json")).expect("parse");
        let ext = extract_operations(&doc).expect("extract");
        let bundle = build_schema_bundle(&doc, &ext).expect("bundle");
        (
            ext.operations[0].clone(),
            bundle.operations["detail"].input_json_schema.clone(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        // spec: CMP-030
        #[test]
        fn proptest_hostile_strings_never_escape_path_query_or_origin(
            sku in prop_oneof![
                Just(".".to_owned()),
                Just("..".to_owned()),
                Just("../etc/passwd".to_owned()),
                Just("%2e%2e%2f".to_owned()),
                Just("a?b=1&c=2#frag".to_owned()),
                Just("line\r\nbreak\0nul".to_owned()),
                Just("x".repeat(4096)),
                ".{1,256}",
            ],
            qval in ".*",
            hval in ".*",
        ) {
            thread_local! {
                static PROP_FIXTURE: (ExtractedOperation, Value) = build_prop_fixture();
            }

            let args = json!({ "sku": sku, "q": qval, "X-Custom": hval })
                .as_object()
                .cloned()
                .expect("obj");

            match PROP_FIXTURE.with(|(op, schema)| build_request(op, schema, &args)) {
                Ok(built) => {
                    prop_assert!(built.path_and_query.starts_with('/'));
                    prop_assert!(!built.path_and_query.contains('#'));
                    let (path_part, query_part) = match built.path_and_query.split_once('?') {
                        Some((p, q)) => (p, Some(q)),
                        None => (built.path_and_query.as_str(), None),
                    };
                    let segs: Vec<&str> = path_part.strip_prefix('/').expect("slash").split('/').collect();
                    prop_assert_eq!(segs.len(), 3);
                    prop_assert_eq!(segs[0], "items");
                    prop_assert_eq!(segs[2], "detail");
                    prop_assert_ne!(segs[1], "");
                    prop_assert_ne!(segs[1], ".");
                    prop_assert_ne!(segs[1], "..");

                    // Verify origin preservation when joined against a base URL
                    let full = format!("http://127.0.0.1:9999{}", built.path_and_query);
                    let parsed = url::Url::parse(&full).expect("valid URL");
                    prop_assert_eq!(parsed.scheme(), "http");
                    prop_assert_eq!(parsed.host_str(), Some("127.0.0.1"));
                    prop_assert_eq!(parsed.port(), Some(9999));
                    prop_assert!(parsed.fragment().is_none());

                    if let Some(q) = query_part {
                        let pairs: Vec<&str> = q.split('&').collect();
                        prop_assert_eq!(pairs.len(), 1);
                        prop_assert!(pairs[0].starts_with("q="));
                    }
                    for (_, hv) in &built.headers {
                        let bytes = hv.as_bytes();
                        prop_assert!(!bytes.contains(&b'\r'));
                        prop_assert!(!bytes.contains(&b'\n'));
                        prop_assert!(!bytes.contains(&0));
                    }
                }
                Err(RequestBuildError::InvalidHeaderValue(_)) => {
                    // Expected whenever `hval` contains CR, LF, NUL, or non-visible header bytes.
                }
                Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
            }
        }
    }
}
