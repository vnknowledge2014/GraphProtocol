//! Deterministic OpenAPI 3.0/3.1 and Swagger 2.0 extractor (`WP-M4-02`).
//!
//! Converts OpenAPI/Swagger JSON specifications into a canonical [`Cisg`] within
//! [`ExtractLimits`] (`ING-010`, `ING-012`, `ING-020`, `ING-021`).
//!
//! Security & determinism rules (`spec/05-ingestion.md` §2):
//! - Zero network or ambient filesystem access (`ING-011`): `servers[]` / `host` URLs are
//!   recorded in metadata/`DocBlob`s and NEVER followed.
//! - Local JSON Pointer `$ref` resolution (`#/...`) enforces cycle detection and
//!   `limits.max_depth`; any external `$ref` (`http://`, `file://`, relative file path) is
//!   rejected with [`ExtractError::Malformed`].
//! - Side-effect classification is derived strictly from HTTP method (`ADR-015`), never
//!   from untrusted description text (`ING-020`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use gap_core::snapshot::{OpKind, Protocol, SideEffectLevel};

use super::mcp::{
    CisgValueExt, allocate_unique_name, check_deadline, enforce_cisg_item_budget, expect_array,
    expect_object, expect_string, named, non_null, optional_string, parse_bounded_json,
    sanitize_identifier, to_pascal_case,
};
use crate::cisg::{
    ArgDef, CandidateJoin, Cisg, CisgValue, DocBlob, ExtractError, ExtractLimits, Extractor,
    ExtractorInfo, FieldDef, FieldRef, JoinCardinality, JoinEvidence, KeyDef, OperationDef,
    Pagination, SourceHandle, SourceRef, TypeDef, TypeRef, UntrustedText,
};

/// Canonical identifier for [`OpenApiExtractor`] (`ING-010`).
pub const OPENAPI_EXTRACTOR_ID: &str = "gap-ingest.openapi";
/// Pinned semantic version of [`OpenApiExtractor`] (`ING-010`).
pub const OPENAPI_EXTRACTOR_VERSION: &str = "0.1.0";

const HTTP_METHODS: &[&str] = &[
    "get", "head", "options", "post", "put", "patch", "delete", "trace",
];

/// Deterministic OpenAPI 3.0/3.1 and Swagger 2.0 extractor (`ING-010`, `ING-012`).
#[derive(Debug, Clone, Default)]
pub struct OpenApiExtractor;

impl OpenApiExtractor {
    /// Creates an [`OpenApiExtractor`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Extractor for OpenApiExtractor {
    fn kind(&self) -> Protocol {
        Protocol::Rest
    }

    fn extractor_info(&self) -> ExtractorInfo {
        ExtractorInfo {
            id: OPENAPI_EXTRACTOR_ID.to_owned(),
            version: OPENAPI_EXTRACTOR_VERSION.to_owned(),
        }
    }

    fn extract(&self, src: &SourceHandle, limits: &ExtractLimits) -> Result<Cisg, ExtractError> {
        let started = Instant::now();
        if src.kind != Protocol::Rest {
            return Err(ExtractError::Unsupported {
                reason: format!("expected REST/OpenAPI protocol source, got {:?}", src.kind),
            });
        }
        if src.as_bytes().len() > limits.max_bytes {
            return Err(ExtractError::TooLarge {
                bytes: src.as_bytes().len(),
                limit: limits.max_bytes,
            });
        }

        let root_val = parse_bounded_json(src.as_bytes(), limits, started)?;
        let root_obj = expect_object(&root_val, "OpenAPI root document")?;
        let version_mode = detect_spec_version(root_obj)?;

        let mut cisg = Cisg {
            source: SourceRef {
                kind: Protocol::Rest,
                location_canonical: src.location_canonical.clone(),
                source_hash: src.source_hash().to_owned(),
            },
            extractor: self.extractor_info(),
            types: Vec::new(),
            operations: Vec::new(),
            node_categories: Vec::new(),
            edge_predicates: Vec::new(),
            keys: Vec::new(),
            candidate_joins: Vec::new(),
            docs: Vec::new(),
        };

        // 1. Validate & record untrusted info + servers (never auto-followed per ING-011).
        record_info_and_servers(root_obj, version_mode, &mut cisg)?;

        // 2. Scan schemas from `#/components/schemas` (OpenAPI 3) or `#/definitions` (Swagger 2).
        let schemas_map = extract_named_schemas_map(root_obj, version_mode)?;
        let mut known_schema_names = BTreeSet::new();
        for name in schemas_map.keys() {
            known_schema_names.insert(to_pascal_case(name));
        }

        let mut entity_id_types: BTreeMap<String, String> = BTreeMap::new();
        let mut raw_schema_docs = Vec::new();
        for (raw_name, schema_val) in &schemas_map {
            check_deadline(started, limits)?;
            let type_name = to_pascal_case(raw_name);
            let resolved = resolve_local_ref_chain(&root_val, schema_val, limits, 0)?;
            let resolved_obj = match resolved {
                CisgValue::Object(map) => map,
                CisgValue::Bool(_) => continue,
                _ => {
                    return Err(ExtractError::Malformed {
                        reason: format!("OpenAPI schema `{raw_name}` must be an object or boolean"),
                    });
                }
            };
            let td = build_schema_typedef(&root_val, &type_name, resolved_obj, limits)?;
            if td.fields.iter().any(|f| f.name == "id") {
                cisg.keys.push(KeyDef {
                    type_name: type_name.clone(),
                    fields: vec!["id".to_owned()],
                    primary: true,
                });
                entity_id_types.insert(to_lower_snake(&type_name), type_name.clone());
            }
            if !td.description.sanitized.is_empty() {
                raw_schema_docs.push(DocBlob {
                    id: format!("openapi://schemas/{raw_name}"),
                    source: format!("openapi:schema#{raw_name}"),
                    content: td.description.clone(),
                });
            }
            cisg.types.push(td);
            enforce_cisg_item_budget(&cisg, limits)?;
        }

        // Infer deterministic `NAME_TYPE` candidate joins (`<entity>_id` -> `<Entity>.id`, ING-021).
        for td in &cisg.types {
            for field in &td.fields {
                if let Some(stem) = field.name.strip_suffix("_id")
                    && let Some(target_type) = entity_id_types.get(stem)
                    && target_type != &td.name
                {
                    cisg.candidate_joins.push(CandidateJoin {
                        from: FieldRef {
                            type_name: td.name.clone(),
                            field_path: field.name.clone(),
                        },
                        to: FieldRef {
                            type_name: target_type.clone(),
                            field_path: "id".to_owned(),
                        },
                        cardinality: JoinCardinality::ManyToOne,
                        evidence: JoinEvidence::NameType,
                    });
                }
            }
        }
        cisg.docs.extend(raw_schema_docs);
        enforce_cisg_item_budget(&cisg, limits)?;

        // 3. Extract paths x HTTP methods into deterministic operations.
        let paths_val = root_obj
            .get("paths")
            .ok_or_else(|| ExtractError::Malformed {
                reason: "OpenAPI document missing required `paths` object".to_owned(),
            })?;
        let paths_obj = expect_object(paths_val, "OpenAPI.paths")?;
        let mut used_ops = BTreeSet::new();

        for (path, path_item_val) in paths_obj {
            check_deadline(started, limits)?;
            let path_item_resolved = resolve_local_ref_chain(&root_val, path_item_val, limits, 0)?;
            let path_item = expect_object(path_item_resolved, &format!("paths.{path}"))?;

            let path_params =
                parse_parameter_list(&root_val, path_item.get("parameters"), limits, 0)?;

            for &method in HTTP_METHODS {
                let Some(op_val) = path_item.get(method) else {
                    continue;
                };
                if matches!(op_val, CisgValue::Null) {
                    continue;
                }
                let op_obj = expect_object(op_val, &format!("paths.{path}.{method}"))?;
                add_openapi_operation(
                    &root_val,
                    version_mode,
                    path,
                    method,
                    &path_params,
                    op_obj,
                    limits,
                    &mut cisg,
                    &mut used_ops,
                )?;
                enforce_cisg_item_budget(&cisg, limits)?;
            }
        }

        enforce_cisg_item_budget(&cisg, limits)?;
        check_deadline(started, limits)?;
        cisg.canonicalize();
        let _canonical = cisg.to_canonical_json_bytes()?;
        Ok(cisg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenApiVersion {
    OpenApi3,
    Swagger2,
}

fn detect_spec_version(root: &BTreeMap<String, CisgValue>) -> Result<OpenApiVersion, ExtractError> {
    if let Some(v) = root.get("openapi") {
        let ver = expect_string(v, "openapi")?;
        if ver.starts_with("3.0.") || ver.starts_with("3.1.") {
            return Ok(OpenApiVersion::OpenApi3);
        }
        return Err(ExtractError::Unsupported {
            reason: format!("unsupported OpenAPI version `{ver}`; expected 3.0.x or 3.1.x"),
        });
    }
    if let Some(v) = root.get("swagger") {
        let ver = expect_string(v, "swagger")?;
        if ver == "2.0" {
            return Ok(OpenApiVersion::Swagger2);
        }
        return Err(ExtractError::Unsupported {
            reason: format!("unsupported Swagger version `{ver}`; expected 2.0"),
        });
    }
    Err(ExtractError::Malformed {
        reason: "OpenAPI document must declare `openapi` (3.0/3.1) or `swagger` (2.0)".to_owned(),
    })
}

fn record_info_and_servers(
    root: &BTreeMap<String, CisgValue>,
    version: OpenApiVersion,
    cisg: &mut Cisg,
) -> Result<(), ExtractError> {
    let info_obj = root
        .get("info")
        .ok_or_else(|| ExtractError::Malformed {
            reason: "OpenAPI document missing required `info`".to_owned(),
        })
        .and_then(|v| expect_object(v, "OpenAPI.info"))?;
    let title = optional_string(info_obj.get("title"), "info.title")?;
    let api_ver = optional_string(info_obj.get("version"), "info.version")?;
    let desc = optional_string(info_obj.get("description"), "info.description")?;

    let mut summary = format!("title={title} version={api_ver}");
    if !desc.is_empty() {
        summary.push_str(": ");
        summary.push_str(desc);
    }
    cisg.docs.push(DocBlob {
        id: "openapi://info".to_owned(),
        source: "openapi:info".to_owned(),
        content: UntrustedText::from_raw(summary),
    });

    match version {
        OpenApiVersion::OpenApi3 => {
            if let Some(servers_val) = root.get("servers") {
                let servers = expect_array(servers_val, "OpenAPI.servers")?;
                for (idx, srv) in servers.iter().enumerate() {
                    let srv_obj = expect_object(srv, "OpenAPI.servers[]")?;
                    let url = optional_string(srv_obj.get("url"), "servers[].url")?;
                    let srv_desc =
                        optional_string(srv_obj.get("description"), "servers[].description")?;
                    let text = if srv_desc.is_empty() {
                        format!("server_url={url}")
                    } else {
                        format!("server_url={url} ({srv_desc})")
                    };
                    cisg.docs.push(DocBlob {
                        id: format!("openapi://servers/{idx}"),
                        source: format!("openapi:servers[{idx}]"),
                        content: UntrustedText::from_raw(text),
                    });
                }
            }
        }
        OpenApiVersion::Swagger2 => {
            let host = optional_string(root.get("host"), "swagger.host")?;
            let base_path = optional_string(root.get("basePath"), "swagger.basePath")?;
            if !host.is_empty() || !base_path.is_empty() {
                cisg.docs.push(DocBlob {
                    id: "openapi://servers/0".to_owned(),
                    source: "swagger:host_basePath".to_owned(),
                    content: UntrustedText::from_raw(format!("host={host} basePath={base_path}")),
                });
            }
        }
    }
    Ok(())
}

fn extract_named_schemas_map(
    root: &BTreeMap<String, CisgValue>,
    version: OpenApiVersion,
) -> Result<BTreeMap<String, CisgValue>, ExtractError> {
    match version {
        OpenApiVersion::OpenApi3 => {
            let Some(components_val) = root.get("components") else {
                return Ok(BTreeMap::new());
            };
            let components = expect_object(components_val, "OpenAPI.components")?;
            let Some(schemas_val) = components.get("schemas") else {
                return Ok(BTreeMap::new());
            };
            Ok(expect_object(schemas_val, "OpenAPI.components.schemas")?.clone())
        }
        OpenApiVersion::Swagger2 => {
            let Some(defs_val) = root.get("definitions") else {
                return Ok(BTreeMap::new());
            };
            Ok(expect_object(defs_val, "Swagger.definitions")?.clone())
        }
    }
}

/// Resolves a chain of local `#/...` JSON Pointers cleanly, rejecting external `$ref` URLs,
/// direct `$ref` cycles (`Malformed`), and excessive `$ref` chains (`DepthExceeded`) (`ING-011`, `ING-012`).
fn resolve_local_ref_chain<'a>(
    root: &'a CisgValue,
    mut current: &'a CisgValue,
    limits: &ExtractLimits,
    base_depth: usize,
) -> Result<&'a CisgValue, ExtractError> {
    let mut visited = BTreeSet::new();
    let mut hop = base_depth;
    while let Some(obj) = current.as_object() {
        let Some(ref_val) = obj.get("$ref") else {
            return Ok(current);
        };
        let ref_str = expect_string(ref_val, "$ref")?;
        hop = hop.saturating_add(1);
        if hop > limits.max_depth {
            return Err(ExtractError::DepthExceeded {
                depth: hop,
                limit: limits.max_depth,
            });
        }
        if !visited.insert(ref_str.to_owned()) {
            return Err(ExtractError::Malformed {
                reason: format!("cyclic local `$ref` chain at `{ref_str}`"),
            });
        }
        current = lookup_local_json_pointer(root, ref_str)?;
    }
    Ok(current)
}

fn lookup_local_json_pointer<'a>(
    root: &'a CisgValue,
    pointer: &str,
) -> Result<&'a CisgValue, ExtractError> {
    if !pointer.starts_with("#/") {
        return Err(ExtractError::Malformed {
            reason: format!("external or non-local `$ref` rejected (ING-011): `{pointer}`"),
        });
    }
    let mut cur = root;
    for raw_seg in pointer[2..].split('/') {
        let decoded = decode_json_pointer_segment(raw_seg);
        cur = match cur {
            CisgValue::Object(map) => map.get(&decoded).ok_or_else(|| ExtractError::Malformed {
                reason: format!("dangling local `$ref` pointer `{pointer}`"),
            })?,
            CisgValue::Array(arr) => {
                let idx: usize = decoded.parse().map_err(|_| ExtractError::Malformed {
                    reason: format!("invalid array index in `$ref` pointer `{pointer}`"),
                })?;
                arr.get(idx).ok_or_else(|| ExtractError::Malformed {
                    reason: format!("out-of-bounds array index in `$ref` pointer `{pointer}`"),
                })?
            }
            _ => {
                return Err(ExtractError::Malformed {
                    reason: format!("cannot traverse non-container along `$ref` `{pointer}`"),
                });
            }
        };
    }
    Ok(cur)
}

fn decode_json_pointer_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    let bytes = seg.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'~' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'0' => {
                    out.push('~');
                    i += 2;
                    continue;
                }
                b'1' => {
                    out.push('/');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // Only push ASCII-safe UTF-8 char slices without splitting multi-byte chars.
        if let Some(ch) = seg[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn schema_ref_type_name(ref_str: &str) -> Option<String> {
    if let Some(rest) = ref_str.strip_prefix("#/components/schemas/") {
        return Some(to_pascal_case(rest));
    }
    if let Some(rest) = ref_str.strip_prefix("#/definitions/") {
        return Some(to_pascal_case(rest));
    }
    None
}

fn build_schema_typedef(
    root: &CisgValue,
    name: &str,
    schema_obj: &BTreeMap<String, CisgValue>,
    limits: &ExtractLimits,
) -> Result<TypeDef, ExtractError> {
    let desc = optional_string(schema_obj.get("description"), "schema.description")?;
    let json_schema = CisgValue::Object(schema_obj.clone())
        .to_canonical_json()
        .ok();

    if let Some(enum_val) = schema_obj.get("enum") {
        let enum_arr = expect_array(enum_val, "schema.enum")?;
        if enum_arr.len() > limits.max_items {
            return Err(ExtractError::TooManyItems {
                items: enum_arr.len(),
                limit: limits.max_items,
            });
        }
        let mut enum_values = Vec::with_capacity(enum_arr.len());
        for item in enum_arr {
            match item {
                CisgValue::String(s) => enum_values.push(s.clone()),
                CisgValue::Int(i) => enum_values.push(i.to_string()),
                _ => {}
            }
        }
        return Ok(TypeDef {
            name: name.to_owned(),
            kind: "ENUM".to_owned(),
            fields: Vec::new(),
            enum_values,
            json_schema,
            description: UntrustedText::from_raw(desc),
        });
    }

    let required: BTreeSet<String> = match schema_obj.get("required") {
        Some(v) => expect_array(v, "schema.required")?
            .iter()
            .filter_map(CisgValue::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        None => BTreeSet::new(),
    };

    let mut fields = Vec::new();
    if let Some(props_val) = schema_obj.get("properties") {
        let props = expect_object(props_val, "schema.properties")?;
        for (prop_name, prop_val) in props {
            let ty = openapi_schema_to_typeref(root, prop_val, limits, 0)?;
            let field_type = if required.contains(prop_name) {
                non_null(ty)
            } else {
                ty
            };
            let prop_desc = prop_val
                .as_object()
                .and_then(|o| o.get("description"))
                .and_then(CisgValue::as_str)
                .unwrap_or("");
            fields.push(FieldDef {
                name: sanitize_identifier(prop_name, "field"),
                field_type,
                is_pii: false,
                description: UntrustedText::from_raw(prop_desc),
            });
        }
    }

    let kind = if fields.is_empty()
        && !matches!(
            schema_obj.get("type").and_then(CisgValue::as_str),
            Some("object")
        ) {
        "SCALAR"
    } else {
        "OBJECT"
    };

    Ok(TypeDef {
        name: name.to_owned(),
        kind: kind.to_owned(),
        fields,
        enum_values: Vec::new(),
        json_schema,
        description: UntrustedText::from_raw(desc),
    })
}

fn openapi_schema_to_typeref(
    root: &CisgValue,
    val: &CisgValue,
    limits: &ExtractLimits,
    depth: usize,
) -> Result<TypeRef, ExtractError> {
    if depth > limits.max_depth {
        return Err(ExtractError::DepthExceeded {
            depth,
            limit: limits.max_depth,
        });
    }
    let Some(obj) = val.as_object() else {
        return Ok(named("JSON"));
    };

    if let Some(ref_val) = obj.get("$ref") {
        let ref_str = expect_string(ref_val, "schema.$ref")?;
        if !ref_str.starts_with("#/") {
            return Err(ExtractError::Malformed {
                reason: format!("external `$ref` rejected: `{ref_str}`"),
            });
        }
        // Validate pointer target exists + reject unbounded chains (`long_ref_chain.json`),
        // returning symbolic named type if this refers to a component schema or continuing
        // resolution for direct aliases.
        if let Some(type_name) = schema_ref_type_name(ref_str) {
            let _target = resolve_local_ref_chain(root, val, limits, depth)?;
            return Ok(named(&type_name));
        }
        let target = resolve_local_ref_chain(root, val, limits, depth)?;
        return openapi_schema_to_typeref(root, target, limits, depth + 1);
    }

    let ty = match obj.get("type") {
        Some(CisgValue::String(s)) => Some(s.as_str()),
        Some(CisgValue::Array(arr)) => arr
            .iter()
            .filter_map(CisgValue::as_str)
            .find(|t| *t != "null"),
        _ => None,
    };
    let fmt = obj.get("format").and_then(CisgValue::as_str);

    let mapped = match ty {
        Some("string") => match fmt {
            Some("uuid") => named("ID"),
            _ => named("String"),
        },
        Some("integer") => named("Int"),
        Some("number") => named("Float"),
        Some("boolean") => named("Boolean"),
        Some("array") => match obj.get("items") {
            Some(items_val) => {
                let inner = openapi_schema_to_typeref(root, items_val, limits, depth + 1)?;
                TypeRef::List(Box::new(inner))
            }
            None => TypeRef::List(Box::new(named("JSON"))),
        },
        _ => named("JSON"),
    };
    Ok(mapped)
}

fn parse_parameter_list(
    root: &CisgValue,
    val: Option<&CisgValue>,
    limits: &ExtractLimits,
    depth: usize,
) -> Result<Vec<BTreeMap<String, CisgValue>>, ExtractError> {
    let Some(v) = val else {
        return Ok(Vec::new());
    };
    let arr = expect_array(v, "parameters")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let resolved = resolve_local_ref_chain(root, item, limits, depth)?;
        out.push(expect_object(resolved, "parameter")?.clone());
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn add_openapi_operation(
    root: &CisgValue,
    version: OpenApiVersion,
    path: &str,
    method: &str,
    path_params: &[BTreeMap<String, CisgValue>],
    op_obj: &BTreeMap<String, CisgValue>,
    limits: &ExtractLimits,
    cisg: &mut Cisg,
    used_ops: &mut BTreeSet<String>,
) -> Result<(), ExtractError> {
    let raw_op_id = op_obj.get("operationId").and_then(CisgValue::as_str);
    let base_name = match raw_op_id {
        Some(id) if !id.trim().is_empty() => sanitize_identifier(id, "op"),
        _ => sanitize_identifier(&format!("{method}_{path}"), "op"),
    };
    let op_name = allocate_unique_name(&base_name, used_ops);

    let (kind, side_effect, idempotent) = match method {
        "get" | "head" | "options" => (OpKind::Query, SideEffectLevel::None, Some(true)),
        "put" => (OpKind::Mutation, SideEffectLevel::Reversible, Some(true)),
        "delete" => (OpKind::Mutation, SideEffectLevel::Irreversible, Some(true)),
        "post" | "patch" => (OpKind::Mutation, SideEffectLevel::Unknown, Some(false)),
        _ => (OpKind::Mutation, SideEffectLevel::Unknown, None),
    };

    // Merge path-level and operation-level parameters by `(name, in)`.
    let op_params = parse_parameter_list(root, op_obj.get("parameters"), limits, 0)?;
    let mut merged: BTreeMap<(String, String), BTreeMap<String, CisgValue>> = BTreeMap::new();
    for p in path_params.iter().chain(op_params.iter()) {
        let pname = p
            .get("name")
            .and_then(CisgValue::as_str)
            .unwrap_or("")
            .to_owned();
        let pin = p
            .get("in")
            .and_then(CisgValue::as_str)
            .unwrap_or("query")
            .to_owned();
        if !pname.is_empty() {
            merged.insert((pname, pin), p.clone());
        }
    }

    let mut args = Vec::new();
    let mut seen_args = BTreeSet::new();
    for ((pname, pin), p) in merged {
        if version == OpenApiVersion::Swagger2 && pin == "body" {
            if let Some(schema_val) = p.get("schema") {
                extract_body_args(root, schema_val, limits, &mut args, &mut seen_args)?;
            }
            continue;
        }
        let arg_name = allocate_unique_name(&sanitize_identifier(&pname, "arg"), &mut seen_args);
        let req = p
            .get("required")
            .and_then(CisgValue::as_bool)
            .unwrap_or(false)
            || pin == "path";
        let ty = match p.get("schema") {
            Some(sv) => openapi_schema_to_typeref(root, sv, limits, 0)?,
            None => match p.get("type").and_then(CisgValue::as_str) {
                Some("integer") => named("Int"),
                Some("number") => named("Float"),
                Some("boolean") => named("Boolean"),
                _ => named("String"),
            },
        };
        let arg_type = if req { non_null(ty) } else { ty };
        let desc = p
            .get("description")
            .and_then(CisgValue::as_str)
            .unwrap_or("");
        args.push(ArgDef {
            name: arg_name,
            arg_type,
            default_value: None,
            description: UntrustedText::from_raw(desc),
        });
    }

    // OpenAPI 3.x requestBody (`application/json`).
    if let Some(rb_val) = op_obj.get("requestBody") {
        let rb_resolved = resolve_local_ref_chain(root, rb_val, limits, 0)?;
        let rb_obj = expect_object(rb_resolved, "operation.requestBody")?;
        if let Some(json_schema_val) = rb_obj
            .get("content")
            .and_then(CisgValue::as_object)
            .and_then(|c| c.get("application/json"))
            .and_then(CisgValue::as_object)
            .and_then(|m| m.get("schema"))
        {
            extract_body_args(root, json_schema_val, limits, &mut args, &mut seen_args)?;
        }
    }

    let result = extract_response_type(root, op_obj, limits)?;
    let pagination = detect_openapi_pagination(&args);

    let summary = optional_string(op_obj.get("summary"), "operation.summary")?;
    let desc = optional_string(op_obj.get("description"), "operation.description")?;
    let raw_doc = match (summary.is_empty(), desc.is_empty()) {
        (true, true) => String::new(),
        (false, true) => summary.to_owned(),
        (true, false) => desc.to_owned(),
        (false, false) => format!("{summary}\n{desc}"),
    };
    let untrusted_desc = UntrustedText::from_raw(&raw_doc);
    if !untrusted_desc.sanitized.is_empty() {
        cisg.docs.push(DocBlob {
            id: format!("openapi://ops/{op_name}"),
            source: format!("openapi:{}#{path}", method.to_ascii_uppercase()),
            content: untrusted_desc.clone(),
        });
    }

    let mut native_ref = BTreeMap::new();
    native_ref.insert(
        "method".to_owned(),
        CisgValue::String(method.to_ascii_uppercase()),
    );
    native_ref.insert("path".to_owned(), CisgValue::String(path.to_owned()));
    if let Some(batch_ext) = op_obj.get("x-gap-batch") {
        native_ref.insert("x_gap_batch".to_owned(), batch_ext.clone());
    }
    if let Some(in_param) = op_obj.get("x-gap-in-param") {
        native_ref.insert("x_gap_in_param".to_owned(), in_param.clone());
    }

    cisg.operations.push(OperationDef {
        name: op_name,
        kind,
        args,
        result,
        description: untrusted_desc,
        side_effect,
        idempotent,
        pagination,
        key_fields: Vec::new(),
        examples: Vec::new(),
        native_ref: CisgValue::Object(native_ref),
    });

    Ok(())
}

fn extract_body_args(
    root: &CisgValue,
    schema_val: &CisgValue,
    limits: &ExtractLimits,
    args: &mut Vec<ArgDef>,
    seen_args: &mut BTreeSet<String>,
) -> Result<(), ExtractError> {
    let resolved = resolve_local_ref_chain(root, schema_val, limits, 0)?;
    let Some(obj) = resolved.as_object() else {
        return Ok(());
    };
    let required: BTreeSet<String> = obj
        .get("required")
        .and_then(CisgValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(CisgValue::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    if let Some(props) = obj.get("properties").and_then(CisgValue::as_object) {
        for (prop_name, prop_val) in props {
            let arg_name = allocate_unique_name(&sanitize_identifier(prop_name, "arg"), seen_args);
            let ty = openapi_schema_to_typeref(root, prop_val, limits, 0)?;
            let arg_type = if required.contains(prop_name) {
                non_null(ty)
            } else {
                ty
            };
            let desc = prop_val
                .as_object()
                .and_then(|m| m.get("description"))
                .and_then(CisgValue::as_str)
                .unwrap_or("");
            args.push(ArgDef {
                name: arg_name,
                arg_type,
                default_value: None,
                description: UntrustedText::from_raw(desc),
            });
        }
    } else {
        let arg_name = allocate_unique_name("body", seen_args);
        let ty = openapi_schema_to_typeref(root, resolved, limits, 0)?;
        args.push(ArgDef {
            name: arg_name,
            arg_type: ty,
            default_value: None,
            description: UntrustedText::from_raw("JSON request body"),
        });
    }
    Ok(())
}

fn extract_response_type(
    root: &CisgValue,
    op_obj: &BTreeMap<String, CisgValue>,
    limits: &ExtractLimits,
) -> Result<TypeRef, ExtractError> {
    let Some(responses) = op_obj.get("responses").and_then(CisgValue::as_object) else {
        return Ok(named("JSON"));
    };
    for status in ["200", "201", "default"] {
        let Some(resp_val) = responses.get(status) else {
            continue;
        };
        let resolved = resolve_local_ref_chain(root, resp_val, limits, 0)?;
        let Some(resp_obj) = resolved.as_object() else {
            continue;
        };
        if let Some(schema_val) = resp_obj
            .get("content")
            .and_then(CisgValue::as_object)
            .and_then(|c| c.get("application/json"))
            .and_then(CisgValue::as_object)
            .and_then(|m| m.get("schema"))
            .or_else(|| resp_obj.get("schema"))
        {
            return openapi_schema_to_typeref(root, schema_val, limits, 0);
        }
    }
    Ok(named("JSON"))
}

fn detect_openapi_pagination(args: &[ArgDef]) -> Pagination {
    let has = |name: &str| args.iter().any(|a| a.name == name);
    if has("cursor") {
        return Pagination::Cursor {
            cursor_arg: "cursor".to_owned(),
            next_field: "nextCursor".to_owned(),
        };
    }
    if has("after") {
        return Pagination::Cursor {
            cursor_arg: "after".to_owned(),
            next_field: "endCursor".to_owned(),
        };
    }
    if has("offset") && has("limit") {
        return Pagination::OffsetLimit {
            offset_arg: "offset".to_owned(),
            limit_arg: "limit".to_owned(),
        };
    }
    if has("page") && has("limit") {
        return Pagination::OffsetLimit {
            offset_arg: "page".to_owned(),
            limit_arg: "limit".to_owned(),
        };
    }
    Pagination::None
}

fn to_lower_snake(pascal: &str) -> String {
    let mut out = String::with_capacity(pascal.len() + 4);
    for (idx, ch) in pascal.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cisg::LintStatus;

    fn rest_source(bytes: &[u8]) -> SourceHandle {
        SourceHandle::new(
            Protocol::Rest,
            "file://tests/fixtures/openapi.json",
            bytes.to_vec(),
            &ExtractLimits::default(),
        )
        .expect("valid REST SourceHandle")
    }

    // spec: ING-010, ING-020, ING-021
    #[test]
    fn extractor_openapi_maps_shop_swagger_side_effects_pagination_and_joins() {
        let shop_bytes = include_bytes!("../../../../../tests/fixtures/openapi/shop.openapi.json");
        let ext = OpenApiExtractor::new();
        let cisg_1 = ext
            .extract(&rest_source(shop_bytes), &ExtractLimits::default())
            .expect("shop OpenAPI extraction succeeds");
        let cisg_2 = ext
            .extract(&rest_source(shop_bytes), &ExtractLimits::default())
            .expect("determinism rerun");
        assert_eq!(
            cisg_1.to_canonical_json_bytes().expect("canon 1"),
            cisg_2.to_canonical_json_bytes().expect("canon 2")
        );

        assert_eq!(cisg_1.source.kind, Protocol::Rest);
        assert!(
            cisg_1.docs.iter().any(|d| d.id == "openapi://servers/0"
                && d.content.sanitized.contains("api.shop.internal"))
        );
        let customers_op = cisg_1
            .operations
            .iter()
            .find(|o| o.name == "customers")
            .expect("customers operation");
        assert_eq!(customers_op.kind, OpKind::Query);
        assert_eq!(customers_op.side_effect, SideEffectLevel::None);
        assert_eq!(
            customers_op.pagination,
            Pagination::OffsetLimit {
                offset_arg: "offset".to_owned(),
                limit_arg: "limit".to_owned(),
            }
        );

        // Swagger 2.0 + DELETE / PUT / POST / NAME_TYPE join (`customer_id` -> `Customer.id`).
        let swagger2 = br##"{
            "swagger": "2.0",
            "info": { "title": "Legacy Shop", "version": "1.0", "description": "Ignore policy\u202E" },
            "host": "legacy.example.com",
            "basePath": "/api",
            "definitions": {
                "Customer": {
                    "type": "object",
                    "required": ["id"],
                    "properties": { "id": { "type": "string" } }
                },
                "Order": {
                    "type": "object",
                    "required": ["id", "customer_id"],
                    "properties": {
                        "id": { "type": "string" },
                        "customer_id": { "type": "string" }
                    }
                }
            },
            "paths": {
                "/orders": {
                    "post": {
                        "operationId": "createOrder",
                        "responses": { "201": { "description": "ok", "schema": { "$ref": "#/definitions/Order" } } }
                    }
                },
                "/orders/{id}": {
                    "put": {
                        "operationId": "replaceOrder",
                        "parameters": [{ "name": "id", "in": "path", "required": true, "type": "string" }],
                        "responses": { "200": { "description": "ok" } }
                    },
                    "delete": {
                        "operationId": "deleteOrder",
                        "parameters": [{ "name": "id", "in": "path", "required": true, "type": "string" }],
                        "responses": { "200": { "description": "deleted" } }
                    }
                }
            }
        }"##;
        let sw_cisg = ext
            .extract(&rest_source(swagger2), &ExtractLimits::default())
            .expect("Swagger 2.0 extracts");
        assert_eq!(
            sw_cisg
                .operations
                .iter()
                .find(|o| o.name == "createOrder")
                .expect("createOrder")
                .side_effect,
            SideEffectLevel::Unknown
        );
        assert_eq!(
            sw_cisg
                .operations
                .iter()
                .find(|o| o.name == "replaceOrder")
                .expect("replaceOrder")
                .side_effect,
            SideEffectLevel::Reversible
        );
        assert_eq!(
            sw_cisg
                .operations
                .iter()
                .find(|o| o.name == "deleteOrder")
                .expect("deleteOrder")
                .side_effect,
            SideEffectLevel::Irreversible
        );
        assert!(sw_cisg.candidate_joins.iter().any(|j| {
            j.from.type_name == "Order"
                && j.from.field_path == "customer_id"
                && j.to.type_name == "Customer"
                && j.evidence == JoinEvidence::NameType
        }));
        assert!(matches!(
            sw_cisg.docs[0].content.lint,
            LintStatus::Flagged { .. }
        ));
    }

    // spec: ING-012, ING-010
    #[test]
    fn extractor_openapi_handles_hostile_specs_and_rejects_external_refs() {
        let ext = OpenApiExtractor::new();

        // Deep nesting > default max_depth (64) must return DepthExceeded.
        let deep =
            include_bytes!("../../../../../tests/fixtures/openapi/hostile/deep_nesting.json");
        assert!(matches!(
            ext.extract(&rest_source(deep), &ExtractLimits::default()),
            Err(ExtractError::DepthExceeded { .. })
        ));

        // Long $ref chain (40 hops > max_depth 16) must return DepthExceeded.
        let chain =
            include_bytes!("../../../../../tests/fixtures/openapi/hostile/long_ref_chain.json");
        assert!(matches!(
            ext.extract(
                &rest_source(chain),
                &ExtractLimits {
                    max_depth: 16,
                    ..ExtractLimits::default()
                }
            ),
            Err(ExtractError::DepthExceeded { limit: 16, .. })
        ));

        // Cyclic named schema references via properties complete cleanly without stack overflow.
        let cyclic =
            include_bytes!("../../../../../tests/fixtures/openapi/hostile/cyclic_schema.json");
        let cyc_cisg = ext
            .extract(&rest_source(cyclic), &ExtractLimits::default())
            .expect("cyclic property schemas stay bounded via symbolic TypeRef::Named");
        assert!(cyc_cisg.types.iter().any(|t| t.name == "NodeA"));

        // Direct $ref self-cycle fails cleanly.
        let direct_cycle = br##"{
            "openapi": "3.1.0",
            "info": { "title": "Self", "version": "1" },
            "components": { "schemas": { "A": { "$ref": "#/components/schemas/B" }, "B": { "$ref": "#/components/schemas/A" } } },
            "paths": {}
        }"##;
        assert!(matches!(
            ext.extract(&rest_source(direct_cycle), &ExtractLimits::default()),
            Err(ExtractError::Malformed { .. })
        ));

        // External $ref MUST be rejected (`ING-011`).
        let ext_ref = br#"{
            "openapi": "3.1.0",
            "info": { "title": "ExtRef", "version": "1" },
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "bad",
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "https://evil.example/schema.json#/Foo" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        assert!(matches!(
            ext.extract(&rest_source(ext_ref), &ExtractLimits::default()),
            Err(ExtractError::Malformed { .. })
        ));

        // Internal servers are recorded in docs, never fetched, and path literals sanitize cleanly.
        let internal =
            include_bytes!("../../../../../tests/fixtures/openapi/hostile/internal_servers.json");
        let int_cisg = ext
            .extract(&rest_source(internal), &ExtractLimits::default())
            .expect("internal servers recorded without fetch");
        assert!(
            int_cisg
                .docs
                .iter()
                .any(|d| d.content.sanitized.contains("169.254.169.254"))
        );

        let literals =
            include_bytes!("../../../../../tests/fixtures/openapi/hostile/path_literals.json");
        let lit_cisg = ext
            .extract(&rest_source(literals), &ExtractLimits::default())
            .expect("path literals extract deterministically");
        assert!(!lit_cisg.operations.is_empty());
    }
}
