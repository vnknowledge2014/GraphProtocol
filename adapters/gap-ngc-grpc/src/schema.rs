//! Protobuf descriptor -> NGC `OperationDescriptor`s, JSON Schema 2020-12, and GAP-QL SDL extraction (`spec/09-compat.md` §5.3, `NGC-012`, `NGC-013`, `GQL-004`, `ADR-015`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{is_allowed_graphql_identifier, is_reserved_builtin_type};
use gap_ngc_sdk::connector::NgcResult;
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_proto::ngc::v1::{OpKind, OperationDescriptor, PaginationKind, SideEffectHint};
use http::uri::PathAndQuery;
use prost::Message as _;
use prost_reflect::{
    DescriptorPool, EnumDescriptor, FieldDescriptor, Kind, MessageDescriptor, MethodDescriptor,
};
use prost_types::method_options::IdempotencyLevel;
use serde_json::{Map, Value, json};

use crate::config::{DeclaredSideEffect, GrpcConfig};

/// JSON Schema 2020-12 dialect URI (`NGC-013`).
pub const JSON_SCHEMA_DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

const BUILTIN_SDL_NAMES: &[&str] = &[
    "Query",
    "Mutation",
    "Subscription",
    "String",
    "Int",
    "Float",
    "Boolean",
    "ID",
    "JSON",
];

/// Compiled runtime dispatch + metadata for one extracted gRPC method.
#[derive(Debug, Clone)]
pub struct ExtractedGrpcMethod {
    /// Operation identifier exposed to GAP (`[A-Za-z_][A-Za-z0-9_]*`).
    pub operation_name: String,
    /// Full gRPC service/method identifier (`"pkg.Service/Method"`).
    pub full_grpc_name: String,
    /// Pre-validated HTTP/2 `:path` (`"/pkg.Service/Method"`).
    pub grpc_path: PathAndQuery,
    /// Classified GAP operation kind (`Query`, `Mutation`, or `Subscription`).
    pub kind: OpKind,
    /// Protobuf input request message descriptor.
    pub input_desc: MessageDescriptor,
    /// Protobuf output response message descriptor.
    pub output_desc: MessageDescriptor,
    /// NGC wire descriptor advertised by `Describe`.
    pub descriptor: OperationDescriptor,
}

/// Extracted gRPC schema bundle produced from a [`DescriptorPool`] (`NGC-010`, `NGC-012`, `NGC-013`).
#[derive(Debug, Clone)]
pub struct GrpcSchemaBundle {
    /// Deterministic `blake3` hash of the source descriptors.
    pub spec_hash: String,
    /// Valid GAP-QL branch SDL (`schema_sdl`) excluding GAP built-ins.
    pub schema_sdl: String,
    /// Lookup map keyed by NGC operation name.
    pub methods: BTreeMap<String, ExtractedGrpcMethod>,
    /// Deterministic list of NGC operation descriptors.
    pub descriptors: Vec<OperationDescriptor>,
}

/// Extracts NGC operations, JSON schemas, and GAP-QL SDL from `pool` according to `config` trust policy (`spec/09-compat.md` §5.3).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if descriptor method names produce invalid gRPC paths.
pub fn extract_grpc_schema(
    pool: &DescriptorPool,
    spec_hash: String,
    config: &GrpcConfig,
) -> NgcResult<GrpcSchemaBundle> {
    let mut services: Vec<_> = pool.services().collect();
    services.sort_by(|a, b| a.full_name().cmp(b.full_name()));

    // Detect if multiple services share the same unqualified short name.
    let mut short_name_counts: BTreeMap<String, usize> = BTreeMap::new();
    for svc in &services {
        if is_internal_grpc_service(svc.full_name()) {
            continue;
        }
        *short_name_counts
            .entry(sanitize_ident(svc.name()))
            .or_insert(0) += 1;
    }

    let mut used_op_names = BTreeSet::new();
    let mut methods = BTreeMap::new();
    let mut descriptors = Vec::new();

    for svc in services {
        if is_internal_grpc_service(svc.full_name()) {
            continue;
        }
        let use_qualified_service = short_name_counts
            .get(&sanitize_ident(svc.name()))
            .copied()
            .unwrap_or(0)
            > 1;

        let mut svc_methods: Vec<_> = svc.methods().collect();
        svc_methods.sort_by(|a, b| a.name().cmp(b.name()));

        for method in svc_methods {
            // Skip client-streaming and bidirectional streaming application RPCs;
            // server-streaming maps to SUBSCRIPTION, unary maps to QUERY/MUTATION.
            if method.is_client_streaming() {
                continue;
            }

            let op_name = make_operation_name(
                svc.full_name(),
                svc.name(),
                method.name(),
                use_qualified_service,
                &mut used_op_names,
            );
            let full_grpc_name = format!("{}/{}", svc.full_name(), method.name());
            let raw_path = format!("/{}/{}/", svc.full_name(), method.name());
            let clean_path = raw_path.trim_end_matches('/').to_owned();
            let grpc_path = clean_path.parse::<PathAndQuery>().map_err(|err| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("invalid gRPC method path `{clean_path}`: {err}"),
                )
            })?;

            let (kind, side_effect_hint, idempotent_hint, proto_idem) =
                classify_method(&method, &op_name, &full_grpc_name, config);

            let input_json_schema = message_to_json_schema(&method.input(), true);
            let output_json_schema = message_to_json_schema(&method.output(), true);

            let input_struct = match &input_json_schema {
                Value::Object(m) => json_map_to_prost_struct(m),
                _ => prost_types::Struct::default(),
            };
            let output_struct = match &output_json_schema {
                Value::Object(m) => json_map_to_prost_struct(m),
                _ => prost_types::Struct::default(),
            };

            let mut annotations = HashMap::new();
            annotations.insert("grpc.method".to_owned(), full_grpc_name.clone());
            annotations.insert(
                "grpc.idempotency_level".to_owned(),
                proto_idem.as_str_name().to_owned(),
            );
            annotations.insert(
                "grpc.server_streaming".to_owned(),
                method.is_server_streaming().to_string(),
            );
            annotations.insert("batch".to_owned(), "emulated".to_owned());

            let descriptor = OperationDescriptor {
                name: op_name.clone(),
                kind: kind.into(),
                description: format!("gRPC RPC {full_grpc_name}"),
                input_schema: Some(input_struct),
                output_schema: Some(output_struct),
                side_effect_hint: side_effect_hint.into(),
                idempotent_hint,
                supports_dry_run: matches!(kind, OpKind::Mutation) && config.supports_dry_run,
                compensation_operation: String::new(),
                examples: Vec::new(),
                cost_weight: 2,
                pagination: PaginationKind::PaginationNone.into(),
                key_fields: infer_key_fields(&method.input()),
                annotations,
            };

            descriptors.push(descriptor.clone());
            methods.insert(
                op_name.clone(),
                ExtractedGrpcMethod {
                    operation_name: op_name,
                    full_grpc_name,
                    grpc_path,
                    kind,
                    input_desc: method.input(),
                    output_desc: method.output(),
                    descriptor,
                },
            );
        }
    }

    let schema_sdl = build_schema_sdl(&methods);

    Ok(GrpcSchemaBundle {
        spec_hash,
        schema_sdl,
        methods,
        descriptors,
    })
}

fn is_internal_grpc_service(full_name: &str) -> bool {
    full_name.starts_with("grpc.reflection.") || full_name.starts_with("grpc.health.")
}

fn read_idempotency_level(method: &MethodDescriptor) -> IdempotencyLevel {
    let encoded = method.options().encode_to_vec();
    match prost_types::MethodOptions::decode(encoded.as_slice()) {
        Ok(opts) => opts.idempotency_level(),
        Err(_) => IdempotencyLevel::IdempotencyUnknown,
    }
}

fn classify_method(
    method: &MethodDescriptor,
    op_name: &str,
    full_grpc_name: &str,
    config: &GrpcConfig,
) -> (OpKind, SideEffectHint, bool, IdempotencyLevel) {
    let proto_idem = read_idempotency_level(method);

    if method.is_server_streaming() {
        return (
            OpKind::Subscription,
            SideEffectHint::SeNone,
            false,
            proto_idem,
        );
    }

    let admin_trusted = config.trust_tier.is_admin_trusted();
    let policy = &config.admin_policy;

    let explicit_query = admin_trusted
        && (policy.approved_query_methods.contains(op_name)
            || policy.approved_query_methods.contains(full_grpc_name));

    let idempotency_promotes = admin_trusted
        && policy.accept_idempotency_options
        && matches!(
            proto_idem,
            IdempotencyLevel::NoSideEffects | IdempotencyLevel::Idempotent
        );

    if explicit_query || idempotency_promotes {
        return (OpKind::Query, SideEffectHint::SeNone, true, proto_idem);
    }

    let side_effect = if admin_trusted {
        match policy
            .approved_side_effects
            .get(op_name)
            .or_else(|| policy.approved_side_effects.get(full_grpc_name))
        {
            Some(DeclaredSideEffect::None) => SideEffectHint::SeNone,
            Some(DeclaredSideEffect::Reversible) => SideEffectHint::SeReversible,
            Some(DeclaredSideEffect::Irreversible) => SideEffectHint::SeIrreversible,
            None => SideEffectHint::SideEffectUnspecified,
        }
    } else {
        SideEffectHint::SideEffectUnspecified
    };

    (OpKind::Mutation, side_effect, false, proto_idem)
}

fn sanitize_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    if out.starts_with("__") {
        out = format!("grpc_{}", out.trim_start_matches('_'));
    }
    out
}

fn make_operation_name(
    svc_full: &str,
    svc_short: &str,
    method_name: &str,
    use_qualified: bool,
    used: &mut BTreeSet<String>,
) -> String {
    let svc_part = if use_qualified {
        sanitize_ident(svc_full)
    } else {
        sanitize_ident(svc_short)
    };
    let base = format!("{svc_part}_{}", sanitize_ident(method_name));
    let base = if is_allowed_graphql_identifier(&base)
        && !gap_core::ids::RESERVED_ROOT_FIELDS.contains(&base.as_str())
    {
        base
    } else {
        format!("grpc_{base}")
    };
    if used.insert(base.clone()) {
        return base;
    }
    for idx in 2_u32.. {
        let cand = format!("{base}_{idx}");
        if used.insert(cand.clone()) {
            return cand;
        }
    }
    base
}

fn infer_key_fields(input_desc: &MessageDescriptor) -> Vec<String> {
    let mut keys = Vec::new();
    for field in input_desc.fields() {
        if !field.is_list()
            && !field.is_map()
            && matches!(
                field.kind(),
                Kind::String
                    | Kind::Int32
                    | Kind::Sint32
                    | Kind::Uint32
                    | Kind::Int64
                    | Kind::Uint64
            )
        {
            keys.push(field.name().to_owned());
        }
    }
    keys
}

fn message_to_json_schema(desc: &MessageDescriptor, root: bool) -> Value {
    let mut visiting = BTreeSet::new();
    let mut obj = message_schema_inner(desc, &mut visiting);
    if root {
        obj.insert(
            "$schema".to_owned(),
            Value::String(JSON_SCHEMA_DRAFT_2020_12.to_owned()),
        );
        obj.insert(
            "title".to_owned(),
            Value::String(desc.full_name().to_owned()),
        );
    }
    Value::Object(obj)
}

fn message_schema_inner(
    desc: &MessageDescriptor,
    visiting: &mut BTreeSet<String>,
) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("type".to_owned(), Value::String("object".to_owned()));

    if !visiting.insert(desc.full_name().to_owned()) {
        return obj;
    }

    let mut props = Map::new();
    for field in desc.fields() {
        let f_schema = field_to_json_schema(&field, visiting);
        props.insert(field.name().to_owned(), f_schema);
    }
    obj.insert("properties".to_owned(), Value::Object(props));
    obj.insert("additionalProperties".to_owned(), Value::Bool(false));

    visiting.remove(desc.full_name());
    obj
}

fn field_to_json_schema(field: &FieldDescriptor, visiting: &mut BTreeSet<String>) -> Value {
    if field.is_map() {
        let mut map_schema = Map::new();
        map_schema.insert("type".to_owned(), Value::String("object".to_owned()));
        if let Kind::Message(entry_msg) = field.kind()
            && let Some(val_field) = entry_msg.get_field_by_name("value")
        {
            map_schema.insert(
                "additionalProperties".to_owned(),
                kind_to_json_schema(&val_field.kind(), visiting),
            );
        }
        return Value::Object(map_schema);
    }

    let inner = kind_to_json_schema(&field.kind(), visiting);
    if field.is_list() {
        json!({
            "type": "array",
            "items": inner,
        })
    } else {
        inner
    }
}

fn kind_to_json_schema(kind: &Kind, visiting: &mut BTreeSet<String>) -> Value {
    match kind {
        Kind::Double | Kind::Float => json!({ "type": "number" }),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Uint32 | Kind::Fixed32 => {
            json!({ "type": "integer" })
        }
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 | Kind::Uint64 | Kind::Fixed64 => {
            // Proto3 canonical JSON serializes 64-bit ints as strings to avoid float precision loss.
            json!({ "type": "string", "format": "int64" })
        }
        Kind::Bool => json!({ "type": "boolean" }),
        Kind::String => json!({ "type": "string" }),
        Kind::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
        Kind::Enum(e) => {
            let variants: Vec<Value> = e
                .values()
                .map(|v| Value::String(v.name().to_owned()))
                .collect();
            json!({ "type": "string", "enum": variants })
        }
        Kind::Message(m) => Value::Object(message_schema_inner(m, visiting)),
    }
}

struct SdlCollector {
    enums: BTreeMap<String, String>,
    objects: BTreeMap<String, String>,
    inputs: BTreeMap<String, String>,
    emitted_objects: BTreeSet<String>,
    emitted_inputs: BTreeSet<String>,
}

impl SdlCollector {
    fn new() -> Self {
        Self {
            enums: BTreeMap::new(),
            objects: BTreeMap::new(),
            inputs: BTreeMap::new(),
            emitted_objects: BTreeSet::new(),
            emitted_inputs: BTreeSet::new(),
        }
    }

    fn safe_type_name(full_name: &str, suffix: &str) -> String {
        let mut base = String::with_capacity(full_name.len() + suffix.len());
        let mut cap = true;
        for ch in full_name.chars() {
            if ch.is_ascii_alphanumeric() {
                if cap {
                    base.push(ch.to_ascii_uppercase());
                    cap = false;
                } else {
                    base.push(ch);
                }
            } else {
                cap = true;
            }
        }
        if base.is_empty() || base.as_bytes()[0].is_ascii_digit() {
            base = format!("Grpc{base}");
        }
        let candidate = format!("{base}{suffix}");
        if candidate.starts_with("__")
            || is_reserved_builtin_type(&candidate)
            || BUILTIN_SDL_NAMES.contains(&candidate.as_str())
            || !is_allowed_graphql_identifier(&candidate)
        {
            format!("Grpc{candidate}")
        } else {
            candidate
        }
    }

    fn safe_field_name(raw: &str) -> String {
        let cleaned = sanitize_ident(raw);
        if cleaned.starts_with("__") || !is_allowed_graphql_identifier(&cleaned) {
            format!("f_{}", cleaned.trim_start_matches('_'))
        } else {
            cleaned
        }
    }

    fn register_enum(&mut self, enum_desc: &EnumDescriptor) -> String {
        let ty_name = Self::safe_type_name(enum_desc.full_name(), "");
        if !self.enums.contains_key(&ty_name) {
            let mut vals = Vec::new();
            for val in enum_desc.values() {
                let mut vname = sanitize_ident(val.name());
                if vname == "true" || vname == "false" || vname == "null" {
                    vname = format!("ENUM_{vname}");
                }
                vals.push(format!("  {vname}"));
            }
            if vals.is_empty() {
                vals.push("  UNSPECIFIED".to_owned());
            }
            let block = format!("enum {ty_name} {{\n{}\n}}\n\n", vals.join("\n"));
            self.enums.insert(ty_name.clone(), block);
        }
        ty_name
    }

    fn register_output_message(&mut self, msg: &MessageDescriptor) -> String {
        let ty_name = Self::safe_type_name(msg.full_name(), "");
        if !self.emitted_objects.insert(ty_name.clone()) {
            return ty_name;
        }
        let mut lines = Vec::new();
        for field in msg.fields() {
            let fname = Self::safe_field_name(field.name());
            let fty = if field.is_map() {
                "JSON".to_owned()
            } else {
                let base = match field.kind() {
                    Kind::Double | Kind::Float => "Float".to_owned(),
                    Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Uint32 | Kind::Fixed32 => {
                        "Int".to_owned()
                    }
                    Kind::Int64
                    | Kind::Sint64
                    | Kind::Sfixed64
                    | Kind::Uint64
                    | Kind::Fixed64
                    | Kind::String
                    | Kind::Bytes => "String".to_owned(),
                    Kind::Bool => "Boolean".to_owned(),
                    Kind::Enum(e) => self.register_enum(&e),
                    Kind::Message(m) => self.register_output_message(&m),
                };
                if field.is_list() {
                    format!("[{base}!]")
                } else {
                    base
                }
            };
            lines.push(format!("  {fname}: {fty}"));
        }
        if lines.is_empty() {
            lines.push("  _empty: Boolean".to_owned());
        }
        let block = format!("type {ty_name} {{\n{}\n}}\n\n", lines.join("\n"));
        self.objects.insert(ty_name.clone(), block);
        ty_name
    }

    fn register_input_message(&mut self, msg: &MessageDescriptor) -> String {
        let ty_name = Self::safe_type_name(msg.full_name(), "Input");
        if !self.emitted_inputs.insert(ty_name.clone()) {
            return ty_name;
        }
        let mut lines = Vec::new();
        for field in msg.fields() {
            let fname = Self::safe_field_name(field.name());
            let fty = if field.is_map() {
                "JSON".to_owned()
            } else {
                let base = match field.kind() {
                    Kind::Double | Kind::Float => "Float".to_owned(),
                    Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Uint32 | Kind::Fixed32 => {
                        "Int".to_owned()
                    }
                    Kind::Int64
                    | Kind::Sint64
                    | Kind::Sfixed64
                    | Kind::Uint64
                    | Kind::Fixed64
                    | Kind::String
                    | Kind::Bytes => "String".to_owned(),
                    Kind::Bool => "Boolean".to_owned(),
                    Kind::Enum(e) => self.register_enum(&e),
                    Kind::Message(m) => self.register_input_message(&m),
                };
                if field.is_list() {
                    format!("[{base}!]")
                } else {
                    base
                }
            };
            lines.push(format!("  {fname}: {fty}"));
        }
        if lines.is_empty() {
            lines.push("  _empty: Boolean".to_owned());
        }
        let block = format!("input {ty_name} {{\n{}\n}}\n\n", lines.join("\n"));
        self.inputs.insert(ty_name.clone(), block);
        ty_name
    }

    fn field_args(&mut self, msg: &MessageDescriptor) -> String {
        let mut args = Vec::new();
        for field in msg.fields() {
            let fname = Self::safe_field_name(field.name());
            let fty = if field.is_map() {
                "JSON".to_owned()
            } else {
                let base = match field.kind() {
                    Kind::Double | Kind::Float => "Float".to_owned(),
                    Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Uint32 | Kind::Fixed32 => {
                        "Int".to_owned()
                    }
                    Kind::Int64
                    | Kind::Sint64
                    | Kind::Sfixed64
                    | Kind::Uint64
                    | Kind::Fixed64
                    | Kind::String
                    | Kind::Bytes => "String".to_owned(),
                    Kind::Bool => "Boolean".to_owned(),
                    Kind::Enum(e) => self.register_enum(&e),
                    Kind::Message(m) => self.register_input_message(&m),
                };
                if field.is_list() {
                    format!("[{base}!]")
                } else {
                    base
                }
            };
            args.push(format!("{fname}: {fty}"));
        }
        if args.is_empty() {
            String::new()
        } else {
            format!("({})", args.join(", "))
        }
    }
}

fn build_schema_sdl(methods: &BTreeMap<String, ExtractedGrpcMethod>) -> String {
    let mut collector = SdlCollector::new();
    let mut query_fields = Vec::new();
    let mut mutation_fields = Vec::new();
    let mut sub_fields = Vec::new();

    for m in methods.values() {
        let out_ty = collector.register_output_message(&m.output_desc);
        let args_str = collector.field_args(&m.input_desc);
        let entry = format!("  {}{args_str}: {out_ty}", m.operation_name);
        match m.kind {
            OpKind::Query => query_fields.push(entry),
            OpKind::Subscription => sub_fields.push(entry),
            OpKind::Mutation | OpKind::Task | OpKind::Unspecified => mutation_fields.push(entry),
        }
    }

    let mut sdl = String::new();
    for block in collector.enums.values() {
        sdl.push_str(block);
    }
    for block in collector.inputs.values() {
        sdl.push_str(block);
    }
    for block in collector.objects.values() {
        sdl.push_str(block);
    }
    if !query_fields.is_empty() {
        sdl.push_str(&format!(
            "type Query {{\n{}\n}}\n\n",
            query_fields.join("\n")
        ));
    }
    if !mutation_fields.is_empty() {
        sdl.push_str(&format!(
            "type Mutation {{\n{}\n}}\n\n",
            mutation_fields.join("\n")
        ));
    }
    if !sub_fields.is_empty() {
        sdl.push_str(&format!(
            "type Subscription {{\n{}\n}}\n\n",
            sub_fields.join("\n")
        ));
    }
    sdl
}
