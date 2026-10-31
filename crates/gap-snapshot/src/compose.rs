//! Deterministic supergraph composer, branch type namespacing, and field ownership (`RUN-009`, `RUN-011`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use apollo_compiler::ast::{
    self, Argument, Definition, Directive, DirectiveList as AstDirectiveList, FieldDefinition,
    InputValueDefinition, Type, Value as AstValue,
};
use apollo_compiler::parser::{SourceMap, SourceSpan};
use apollo_compiler::schema::{DirectiveList as SchemaDirectiveList, ExtendedType};
use apollo_compiler::validation::Valid;
use apollo_compiler::{Name, Node, Schema, name};
use gap_core::error::{ErrorCode, GapError, SourceLocation};
use gap_core::ids::{
    BranchId, BranchUid, GroupId, OperationId, RESERVED_BUILTIN_TYPES, is_valid_graphql_name,
};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CostHint, Endpoint, Example,
    FieldCoordinate, FieldOwnership, OpKind, OperationDescriptor, Protocol, SideEffect,
    SideEffectLevel, TrustTier,
};
use gap_proto::ngc::v1::{
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOperationDescriptor, PredicateOp,
    SideEffectHint,
};
use gap_ql::schema::{
    BUILTIN_SDL, EXECUTABLE_DIRECTIVES_SDL, TYPE_SYSTEM_DIRECTIVES_SDL,
    inject_gap_introspection_fields, inject_gap_introspection_types, is_root_operation_object_name,
    parse_sdl_ast, span_to_source_location, validate_branch_ast_reserved_names,
    validate_oneof_schema_definitions,
};
use prost_types::value::Kind as ProstValueKind;
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;

use crate::canonical::canonical_json;

/// Maximum GraphQL list/non-null wrapper depth accepted on any field/argument/input type (`RUN-009`).
const MAX_TYPE_WRAPPER_DEPTH: usize = 64;

/// Maximum recursion depth when converting Protobuf `Struct` JSON schemas (`NGC-011`).
const MAX_SCHEMA_STRUCT_DEPTH: usize = 128;

/// Maximum byte length of `BranchDescriptor.synopsis` (`spec/01-architecture.md` §4, brief §3.3).
const MAX_SYNOPSIS_BYTES: usize = 90;

/// Built-in GraphQL scalar names shared across all branches (`RUN-009(b)`, `GQL-004`).
const STANDARD_GRAPHQL_SCALARS: &[&str] = &["Int", "Float", "String", "Boolean", "ID"];

/// Single branch input supplied to [`compose`] (`RUN-009`).
#[derive(Debug, Clone, PartialEq)]
pub struct BranchInput {
    /// Human-readable branch slug (`[a-z][a-z0-9_-]{1,62}`).
    pub id: BranchId,
    /// Generation-stable branch UUIDv7.
    pub uid: BranchUid,
    /// Upstream wire protocol.
    pub protocol: Protocol,
    /// Branch trust tier (`T0`, `T1`, `T2`).
    pub trust: TrustTier,
    /// Sanitized connector endpoint (`INV-15`).
    pub endpoint: Endpoint,
    /// Connector `Describe` payload (`schema_sdl`, `operations`, `capabilities`, `limits`, `meta`).
    pub description: gap_proto::ngc::v1::ConnectorDescription,
    /// Administrative / drift status for this branch in the target snapshot.
    pub status: BranchStatus,
}

/// Non-fatal warning emitted during branch SDL sanitization and composition (`GQL-030`, `GQL-032`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionWarning {
    /// Branch that triggered the warning.
    pub branch_id: BranchId,
    /// Stable GAP error/warning code (e.g. [`ErrorCode::DirectiveIgnored`]).
    pub code: ErrorCode,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Optional 1-based SDL source locations.
    pub locations: Vec<SourceLocation>,
}

impl CompositionWarning {
    /// Converts this warning into a structured [`GapError`].
    #[must_use]
    pub fn into_gap_error(self) -> GapError {
        let err = GapError::new(self.code, self.message);
        if self.locations.is_empty() {
            err
        } else {
            err.with_locations(self.locations)
        }
    }
}

/// Composition failure returned by [`compose`] (`RUN-009`, `RUN-011`, `GQL-004`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{error}")]
pub struct CompositionError {
    /// Branch primarily responsible for the error, when attributable to a single branch.
    pub branch_id: Option<BranchId>,
    /// Underlying structured [`GapError`] carrying [`ErrorCode`] and source locations.
    #[source]
    pub error: GapError,
}

impl CompositionError {
    /// Constructs a new [`CompositionError`].
    #[must_use]
    pub fn new(branch_id: Option<BranchId>, error: GapError) -> Self {
        Self { branch_id, error }
    }

    /// Returns the canonical [`ErrorCode`] of this failure.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.error.code()
    }

    /// Borrows the human-readable failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        self.error.message()
    }

    /// Unwraps the inner [`GapError`].
    #[must_use]
    pub fn into_gap_error(self) -> GapError {
        self.error
    }
}

impl From<CompositionError> for GapError {
    fn from(err: CompositionError) -> Self {
        err.error
    }
}

/// Pure, deterministic supergraph composition output (`RUN-009`).
#[derive(Debug, Clone)]
pub struct Composition {
    /// Validated supergraph schema including GAP built-ins and programmatic `__search`/`__metaGraph`.
    pub schema: Valid<Schema>,
    /// Canonical standalone supergraph SDL text (valid GraphQL SDL containing GAP built-ins once).
    pub sdl: String,
    /// Compiled immutable branch descriptors sorted by `BranchId`.
    pub branches: Vec<Arc<BranchDescriptor>>,
    /// Deterministic `(type_name, field) -> BranchUid` ownership map (`RUN-009(f)`, `RUN-011`).
    pub ownership: FieldOwnership,
    /// Non-fatal directive sanitization warnings (`GQL-030`, `GQL-032`).
    pub warnings: Vec<CompositionWarning>,
    /// `blake3-256` digest over RFC 8785 JCS of sorted `[(uid, pinned_desc_hash, status)]`.
    pub branch_set_hash: [u8; 32],
}

fn is_shared_type_name(name: &str) -> bool {
    STANDARD_GRAPHQL_SCALARS.contains(&name)
        || RESERVED_BUILTIN_TYPES.contains(&name)
        || is_root_operation_object_name(name)
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

fn make_branch_directive(branch: &BranchInput) -> Node<Directive> {
    Node::new(Directive {
        name: name!("branch"),
        arguments: vec![
            Node::new(Argument {
                name: name!("id"),
                value: branch.id.as_str().into(),
            }),
            Node::new(Argument {
                name: name!("uid"),
                value: branch.uid.to_string().into(),
            }),
            Node::new(Argument {
                name: name!("protocol"),
                value: Node::new(AstValue::Enum(protocol_enum_name(branch.protocol))),
            }),
            Node::new(Argument {
                name: name!("trust"),
                value: Node::new(AstValue::Enum(trust_enum_name(branch.trust))),
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
                value: Node::new(AstValue::Enum(name!("UNKNOWN"))),
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

fn make_warning(
    branch_id: &BranchId,
    message: String,
    span: Option<SourceSpan>,
    sources: &SourceMap,
) -> CompositionWarning {
    let locations = span_to_source_location(span, sources).into_iter().collect();
    CompositionWarning {
        branch_id: branch_id.clone(),
        code: ErrorCode::DirectiveIgnored,
        message,
        locations,
    }
}

fn err_with_span(
    branch_id: Option<&BranchId>,
    code: ErrorCode,
    message: String,
    span: Option<SourceSpan>,
    sources: &SourceMap,
) -> CompositionError {
    let mut err = GapError::new(code, message);
    if let Some(loc) = span_to_source_location(span, sources) {
        err = err.with_locations(vec![loc]);
    }
    CompositionError::new(branch_id.cloned(), err)
}

fn namespace_name(
    ns: &str,
    raw: &Name,
    branch_id: &BranchId,
    sources: &SourceMap,
) -> Result<Name, CompositionError> {
    if is_shared_type_name(raw.as_str()) {
        return Ok(raw.clone());
    }
    let candidate = format!("{ns}_{raw}");
    Name::new(&candidate).map_err(|_| {
        err_with_span(
            Some(branch_id),
            ErrorCode::ValidationError,
            format!(
                "branch `{branch_id}`: namespaced identifier `{candidate}` is not a valid GraphQL name"
            ),
            raw.location(),
            sources,
        )
    })
}

fn rewrite_type(
    ty: &mut Type,
    ns: &str,
    branch_id: &BranchId,
    sources: &SourceMap,
    depth: usize,
) -> Result<(), CompositionError> {
    if depth > MAX_TYPE_WRAPPER_DEPTH {
        return Err(CompositionError::new(
            Some(branch_id.clone()),
            GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "branch `{branch_id}`: GraphQL type wrapper depth exceeds maximum ({MAX_TYPE_WRAPPER_DEPTH})"
                ),
            ),
        ));
    }
    match ty {
        Type::Named(named) | Type::NonNullNamed(named) => {
            *named = namespace_name(ns, named, branch_id, sources)?;
            Ok(())
        }
        Type::List(inner) | Type::NonNullList(inner) => {
            rewrite_type(inner, ns, branch_id, sources, depth + 1)
        }
    }
}

fn sanitize_directives(
    directives: &mut AstDirectiveList,
    target_label: &str,
    branch: &BranchInput,
    is_field: bool,
    force_t2_side_effect_if_absent: bool,
    stamp_branch: bool,
    sources: &SourceMap,
    warnings: &mut Vec<CompositionWarning>,
) {
    let mut kept = AstDirectiveList::new();
    for dir in directives.iter() {
        let dname = dir.name.as_str();
        if matches!(dname, "branch" | "capabilityGroup" | "provenance") {
            warnings.push(make_warning(
                &branch.id,
                format!(
                    "ignored authored `@{dname}` on `{target_label}` in branch `{}` (GQL-030)",
                    branch.id
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
                warnings.push(make_warning(
                    &branch.id,
                    format!(
                        "ignored authored `@join(status:)` on `{target_label}` in branch `{}`; reset to PROPOSED (GQL-030)",
                        branch.id
                    ),
                    arg_span.or_else(|| dir.location()),
                    sources,
                ));
                new_args.push(Node::new(Argument {
                    name: name!("status"),
                    value: Node::new(AstValue::Enum(name!("PROPOSED"))),
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

    if is_field && branch.trust == TrustTier::T2 {
        let mut had_side_effect = false;
        let mut authored_non_unknown = None;
        for dir in directives.iter() {
            if dir.name.as_str() == "sideEffect" {
                had_side_effect = true;
                let is_pure_unknown = dir.arguments.iter().any(|a| {
                    a.name.as_str() == "level"
                        && matches!(&*a.value, AstValue::Enum(v) if v.as_str() == "UNKNOWN")
                }) && dir.arguments.iter().all(|a| match a.name.as_str() {
                    "idempotent" | "dryRun" => matches!(&*a.value, AstValue::Boolean(false)),
                    _ => true,
                });
                if !is_pure_unknown {
                    authored_non_unknown = Some(dir.location());
                }
            }
        }
        if let Some(span) = authored_non_unknown {
            warnings.push(make_warning(
                &branch.id,
                format!(
                    "forced `@sideEffect(level: UNKNOWN)` on T2 field `{target_label}` in branch `{}` (GQL-032)",
                    branch.id
                ),
                span,
                sources,
            ));
        }
        if had_side_effect || force_t2_side_effect_if_absent {
            directives.retain(|d| d.name.as_str() != "sideEffect");
            directives.push(make_forced_unknown_side_effect());
        }
    }

    if stamp_branch && !directives.has("branch") {
        directives.push(make_branch_directive(branch));
    }
}

fn rewrite_and_sanitize_input_values(
    inputs: &mut [Node<InputValueDefinition>],
    parent_label: &str,
    ns: &str,
    branch: &BranchInput,
    sources: &SourceMap,
    warnings: &mut Vec<CompositionWarning>,
) -> Result<(), CompositionError> {
    for arg_node in inputs {
        let arg = arg_node.make_mut();
        rewrite_type(arg.ty.make_mut(), ns, &branch.id, sources, 0)?;
        let label = format!("{parent_label}({})", arg.name);
        sanitize_directives(
            &mut arg.directives,
            &label,
            branch,
            false,
            false,
            false,
            sources,
            warnings,
        );
    }
    Ok(())
}

fn render_clean_field_signature(field: &FieldDefinition) -> String {
    let stripped = FieldDefinition {
        description: None,
        name: field.name.clone(),
        arguments: field
            .arguments
            .iter()
            .map(|arg| {
                Node::new(InputValueDefinition {
                    description: None,
                    name: arg.name.clone(),
                    ty: arg.ty.clone(),
                    default_value: arg.default_value.clone(),
                    directives: AstDirectiveList::new(),
                })
            })
            .collect(),
        ty: field.ty.clone(),
        directives: AstDirectiveList::new(),
    };
    stripped
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn root_kind_for_name(name: &str) -> Option<OpKind> {
    match name {
        "Query" => Some(OpKind::Query),
        "Mutation" => Some(OpKind::Mutation),
        "Subscription" => Some(OpKind::Subscription),
        _ => None,
    }
}

fn root_name_for_kind(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Query => "Query",
        OpKind::Mutation => "Mutation",
        OpKind::Subscription => "Subscription",
        OpKind::Task => "Task",
    }
}

fn extract_sdl_side_effect_level(directives: &AstDirectiveList) -> Option<SideEffectLevel> {
    let dir = directives.get("sideEffect")?;
    let val = dir.specified_argument_by_name("level")?;
    let AstValue::Enum(level_name) = &**val else {
        return None;
    };
    match level_name.as_str() {
        "NONE" => Some(SideEffectLevel::None),
        "REVERSIBLE" => Some(SideEffectLevel::Reversible),
        "IRREVERSIBLE" => Some(SideEffectLevel::Irreversible),
        "UNKNOWN" => Some(SideEffectLevel::Unknown),
        _ => None,
    }
}

fn extract_sdl_cost_hint(directives: &AstDirectiveList, proto_weight: u32) -> CostHint {
    let mut hint = CostHint {
        weight: if proto_weight == 0 { 1 } else { proto_weight },
        default_list_size: None,
        per_key_weight: 1,
    };
    if let Some(cost_dir) = directives.get("cost") {
        if proto_weight == 0
            && let Some(AstValue::Int(i)) =
                cost_dir.specified_argument_by_name("weight").map(|v| &**v)
            && let Ok(parsed) = i.try_to_i32()
            && let Ok(u) = u32::try_from(parsed)
        {
            hint.weight = u;
        }
        if let Some(AstValue::Int(i)) = cost_dir
            .specified_argument_by_name("listSize")
            .map(|v| &**v)
            && let Ok(parsed) = i.try_to_i32()
            && let Ok(u) = u32::try_from(parsed)
        {
            hint.default_list_size = Some(u);
        }
        if let Some(AstValue::Int(i)) = cost_dir.specified_argument_by_name("perKey").map(|v| &**v)
            && let Ok(parsed) = i.try_to_i32()
            && let Ok(u) = u32::try_from(parsed)
        {
            hint.per_key_weight = u;
        }
    }
    hint
}

fn prost_struct_to_json(
    st: &prost_types::Struct,
    branch_id: &BranchId,
    op_name: &str,
    depth: usize,
) -> Result<JsonValue, CompositionError> {
    if depth > MAX_SCHEMA_STRUCT_DEPTH {
        return Err(CompositionError::new(
            Some(branch_id.clone()),
            GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "branch `{branch_id}`: JSON schema nesting exceeds maximum depth ({MAX_SCHEMA_STRUCT_DEPTH}) on operation `{op_name}`"
                ),
            ),
        ));
    }
    let mut map = JsonMap::with_capacity(st.fields.len());
    for (k, v) in &st.fields {
        let converted = prost_value_to_json(v, branch_id, op_name, depth + 1)?;
        map.insert(k.clone(), converted);
    }
    Ok(JsonValue::Object(map))
}

fn prost_value_to_json(
    val: &prost_types::Value,
    branch_id: &BranchId,
    op_name: &str,
    depth: usize,
) -> Result<JsonValue, CompositionError> {
    if depth > MAX_SCHEMA_STRUCT_DEPTH {
        return Err(CompositionError::new(
            Some(branch_id.clone()),
            GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "branch `{branch_id}`: JSON schema nesting exceeds maximum depth ({MAX_SCHEMA_STRUCT_DEPTH}) on operation `{op_name}`"
                ),
            ),
        ));
    }
    match val.kind.as_ref() {
        None | Some(ProstValueKind::NullValue(_)) => Ok(JsonValue::Null),
        Some(ProstValueKind::BoolValue(b)) => Ok(JsonValue::Bool(*b)),
        Some(ProstValueKind::StringValue(s)) => Ok(JsonValue::String(s.clone())),
        Some(ProstValueKind::NumberValue(n)) => {
            let num = serde_json::Number::from_f64(*n).ok_or_else(|| {
                CompositionError::new(
                    Some(branch_id.clone()),
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{branch_id}`: non-finite float in schema of operation `{op_name}`"
                        ),
                    ),
                )
            })?;
            Ok(JsonValue::Number(num))
        }
        Some(ProstValueKind::StructValue(s)) => {
            prost_struct_to_json(s, branch_id, op_name, depth + 1)
        }
        Some(ProstValueKind::ListValue(list)) => {
            let mut items = Vec::with_capacity(list.values.len());
            for elem in &list.values {
                items.push(prost_value_to_json(elem, branch_id, op_name, depth + 1)?);
            }
            Ok(JsonValue::Array(items))
        }
    }
}

fn compute_operation_desc_hash(
    branch_id: &BranchId,
    op: &ProtoOperationDescriptor,
) -> Result<[u8; 32], CompositionError> {
    let input_json = match op.input_schema.as_ref() {
        Some(st) => prost_struct_to_json(st, branch_id, &op.name, 0)?,
        None => JsonValue::Null,
    };
    let output_json = match op.output_schema.as_ref() {
        Some(st) => prost_struct_to_json(st, branch_id, &op.name, 0)?,
        None => JsonValue::Null,
    };
    let mut annotations_map = JsonMap::with_capacity(op.annotations.len());
    for (k, v) in &op.annotations {
        annotations_map.insert(k.clone(), JsonValue::String(v.clone()));
    }

    let mut payload = JsonMap::with_capacity(5);
    payload.insert("name".to_owned(), JsonValue::String(op.name.clone()));
    payload.insert(
        "description".to_owned(),
        JsonValue::String(op.description.clone()),
    );
    payload.insert("input_schema".to_owned(), input_json);
    payload.insert("output_schema".to_owned(), output_json);
    payload.insert("annotations".to_owned(), JsonValue::Object(annotations_map));

    let jcs = canonical_json(&JsonValue::Object(payload)).map_err(|err| {
        CompositionError::new(
            Some(branch_id.clone()),
            GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "branch `{branch_id}`: failed to canonicalize operation `{}` metadata: {err}",
                    op.name
                ),
            ),
        )
    })?;
    Ok(*blake3::hash(jcs.as_bytes()).as_bytes())
}

fn compute_per_operation_group_id(op_id: &OperationId, kind: OpKind, field_sdl: &str) -> GroupId {
    let canonical_sig = format!(
        "{}:{}:{field_sdl}",
        op_id.as_str(),
        root_name_for_kind(kind)
    );
    let digest = blake3::hash(canonical_sig.as_bytes());
    let mut bytes16 = [0_u8; 16];
    bytes16.copy_from_slice(&digest.as_bytes()[..16]);
    GroupId::from_bytes(bytes16)
}

fn parse_spec_hash(raw: &str) -> [u8; 32] {
    if raw.is_empty() {
        return [0_u8; 32];
    }
    if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        let ascii = raw.as_bytes();
        let mut out = [0_u8; 32];
        for (idx, slot) in out.iter_mut().enumerate() {
            let hi = hex_nibble(ascii[idx * 2]);
            let lo = hex_nibble(ascii[idx * 2 + 1]);
            *slot = (hi << 4) | lo;
        }
        return out;
    }
    *blake3::hash(raw.as_bytes()).as_bytes()
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn hex_encode_32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn clip_utf8_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn convert_capabilities(proto: Option<&gap_proto::ngc::v1::Capabilities>) -> Capabilities {
    let Some(cap) = proto else {
        return Capabilities::default();
    };
    let mut predicate_ops = Vec::with_capacity(cap.predicate_ops.len());
    for &raw_op in &cap.predicate_ops {
        let maybe_name = match PredicateOp::try_from(raw_op) {
            Ok(PredicateOp::PEq) => Some("EQ"),
            Ok(PredicateOp::PNe) => Some("NE"),
            Ok(PredicateOp::PGt) => Some("GT"),
            Ok(PredicateOp::PGte) => Some("GTE"),
            Ok(PredicateOp::PLt) => Some("LT"),
            Ok(PredicateOp::PLte) => Some("LTE"),
            Ok(PredicateOp::PIn) => Some("IN"),
            Ok(PredicateOp::PContains) => Some("CONTAINS"),
            Ok(PredicateOp::PStartsWith) => Some("STARTS_WITH"),
            Ok(PredicateOp::PExists) => Some("EXISTS"),
            Ok(PredicateOp::PAnd) => Some("AND"),
            Ok(PredicateOp::POr) => Some("OR"),
            Ok(PredicateOp::PNot) => Some("NOT"),
            Ok(PredicateOp::Unspecified) | Err(_) => None,
        };
        if let Some(op_str) = maybe_name {
            predicate_ops.push(op_str.to_owned());
        }
    }
    Capabilities {
        foreach_batch: cap.foreach_batch,
        idempotency: cap.idempotency,
        dry_run: cap.dry_run,
        subscriptions: cap.subscribe,
        tasks: cap.tasks,
        sketch: cap.sketch,
        explain: cap.explain,
        predicate_ops,
    }
}

fn convert_limits(proto: Option<&gap_proto::ngc::v1::Limits>) -> BranchLimits {
    let Some(lim) = proto else {
        return BranchLimits::default();
    };
    BranchLimits {
        max_batch_size: lim.max_batch_size,
        max_in_flight: lim.max_in_flight,
        timeout_ms: lim.default_timeout_ms,
        max_response_bytes: lim.max_response_bytes,
    }
}

fn convert_side_effect(
    trust: TrustTier,
    hint_raw: i32,
    sdl_level: Option<SideEffectLevel>,
) -> SideEffect {
    let level = if trust == TrustTier::T2 {
        SideEffectLevel::Unknown
    } else {
        match SideEffectHint::try_from(hint_raw) {
            Ok(SideEffectHint::SeNone) => SideEffectLevel::None,
            Ok(SideEffectHint::SeReversible) => SideEffectLevel::Reversible,
            Ok(SideEffectHint::SeIrreversible) => SideEffectLevel::Irreversible,
            Ok(SideEffectHint::SideEffectUnspecified) | Err(_) => {
                sdl_level.unwrap_or(SideEffectLevel::Unknown)
            }
        }
    };
    let requires_approval = matches!(
        level,
        SideEffectLevel::Irreversible | SideEffectLevel::Unknown
    );
    SideEffect {
        level,
        verified: false,
        requires_approval,
    }
}

fn convert_proto_op_kind(
    raw: i32,
    branch_id: &BranchId,
    op_name: &str,
) -> Result<OpKind, CompositionError> {
    match ProtoOpKind::try_from(raw) {
        Ok(ProtoOpKind::Query) => Ok(OpKind::Query),
        Ok(ProtoOpKind::Mutation) => Ok(OpKind::Mutation),
        Ok(ProtoOpKind::Subscription) => Ok(OpKind::Subscription),
        Ok(ProtoOpKind::Task) => Err(CompositionError::new(
            Some(branch_id.clone()),
            GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "branch `{branch_id}`: operation `{op_name}` has kind TASK without a GraphQL root operation type (RUN-009)"
                ),
            ),
        )),
        Ok(ProtoOpKind::Unspecified) | Err(_) => Err(CompositionError::new(
            Some(branch_id.clone()),
            GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "branch `{branch_id}`: operation `{op_name}` has unspecified or invalid OpKind ({raw})"
                ),
            ),
        )),
    }
}

fn compute_branch_set_hash(
    branches: &[Arc<BranchDescriptor>],
) -> Result<[u8; 32], CompositionError> {
    let mut tuples: Vec<(BranchUid, [u8; 32], BranchStatus)> = branches
        .iter()
        .map(|b| (b.uid, b.pinned_desc_hash, b.status))
        .collect();
    tuples.sort_by_key(|(uid, _, _)| *uid);

    let mut json_rows = Vec::with_capacity(tuples.len());
    for (uid, pinned, status) in tuples {
        let status_val = serde_json::to_value(status).map_err(|err| {
            CompositionError::new(
                None,
                GapError::new(
                    ErrorCode::Internal,
                    format!("failed to serialize BranchStatus: {err}"),
                ),
            )
        })?;
        json_rows.push(JsonValue::Array(vec![
            JsonValue::String(uid.to_string()),
            JsonValue::String(hex_encode_32(&pinned)),
            status_val,
        ]));
    }

    let jcs = canonical_json(&JsonValue::Array(json_rows)).map_err(|err| {
        CompositionError::new(
            None,
            GapError::new(
                ErrorCode::Internal,
                format!("failed to canonicalize branch_set_hash payload: {err}"),
            ),
        )
    })?;
    Ok(*blake3::hash(jcs.as_bytes()).as_bytes())
}

fn extract_uid_from_schema_directives(directives: &SchemaDirectiveList) -> Option<BranchUid> {
    let dir = directives.get("branch")?;
    let AstValue::String(raw_uid) = dir.specified_argument_by_name("uid").map(|v| &**v)? else {
        return None;
    };
    BranchUid::parse(raw_uid).ok()
}

fn extract_uid_from_ast_directives(directives: &AstDirectiveList) -> Option<BranchUid> {
    let dir = directives.get("branch")?;
    let AstValue::String(raw_uid) = dir.specified_argument_by_name("uid").map(|v| &**v)? else {
        return None;
    };
    BranchUid::parse(raw_uid).ok()
}

fn derive_ownership_from_schema(schema: &Schema) -> FieldOwnership {
    let mut owners = BTreeMap::new();
    for (type_name, ext_ty) in &schema.types {
        if type_name.as_str().starts_with("__") {
            continue;
        }
        let ExtendedType::Object(obj) = ext_ty else {
            continue;
        };
        let obj_owner = extract_uid_from_schema_directives(&obj.directives);
        for (field_name, field_def) in &obj.fields {
            if field_name.as_str().starts_with("__") {
                continue;
            }
            if let Some(uid) = extract_uid_from_ast_directives(&field_def.directives).or(obj_owner)
            {
                owners.insert(
                    FieldCoordinate::new(type_name.as_str(), field_name.as_str()),
                    uid,
                );
            }
        }
    }
    FieldOwnership::new(owners)
}

#[derive(Clone)]
struct RootFieldRecord {
    sdl_signature: String,
    sdl_side_effect: Option<SideEffectLevel>,
    directives: AstDirectiveList,
}

fn fold_same_branch_extensions(definitions: Vec<Definition>) -> Vec<Definition> {
    let mut out: Vec<Definition> = Vec::with_capacity(definitions.len());
    let mut extensions: Vec<Definition> = Vec::new();

    for def in definitions {
        match def {
            Definition::ObjectTypeExtension(_)
            | Definition::InterfaceTypeExtension(_)
            | Definition::UnionTypeExtension(_)
            | Definition::EnumTypeExtension(_)
            | Definition::InputObjectTypeExtension(_)
            | Definition::ScalarTypeExtension(_) => extensions.push(def),
            other => out.push(other),
        }
    }

    for ext in extensions {
        match ext {
            Definition::ObjectTypeExtension(ext_node) => {
                if let Some(Definition::ObjectTypeDefinition(base_node)) = out
                    .iter_mut()
                    .find(|d| matches!(d, Definition::ObjectTypeDefinition(b) if b.name == ext_node.name))
                {
                    let base = base_node.make_mut();
                    base.implements_interfaces
                        .extend(ext_node.implements_interfaces.iter().cloned());
                    base.fields.extend(ext_node.fields.iter().cloned());
                } else {
                    out.push(Definition::ObjectTypeExtension(ext_node));
                }
            }
            Definition::InterfaceTypeExtension(ext_node) => {
                if let Some(Definition::InterfaceTypeDefinition(base_node)) = out
                    .iter_mut()
                    .find(|d| matches!(d, Definition::InterfaceTypeDefinition(b) if b.name == ext_node.name))
                {
                    let base = base_node.make_mut();
                    base.implements_interfaces
                        .extend(ext_node.implements_interfaces.iter().cloned());
                    base.fields.extend(ext_node.fields.iter().cloned());
                } else {
                    out.push(Definition::InterfaceTypeExtension(ext_node));
                }
            }
            Definition::UnionTypeExtension(ext_node) => {
                if let Some(Definition::UnionTypeDefinition(base_node)) = out
                    .iter_mut()
                    .find(|d| matches!(d, Definition::UnionTypeDefinition(b) if b.name == ext_node.name))
                {
                    let base = base_node.make_mut();
                    base.members.extend(ext_node.members.iter().cloned());
                } else {
                    out.push(Definition::UnionTypeExtension(ext_node));
                }
            }
            Definition::EnumTypeExtension(ext_node) => {
                if let Some(Definition::EnumTypeDefinition(base_node)) = out
                    .iter_mut()
                    .find(|d| matches!(d, Definition::EnumTypeDefinition(b) if b.name == ext_node.name))
                {
                    let base = base_node.make_mut();
                    base.values.extend(ext_node.values.iter().cloned());
                } else {
                    out.push(Definition::EnumTypeExtension(ext_node));
                }
            }
            Definition::InputObjectTypeExtension(ext_node) => {
                if let Some(Definition::InputObjectTypeDefinition(base_node)) = out
                    .iter_mut()
                    .find(|d| matches!(d, Definition::InputObjectTypeDefinition(b) if b.name == ext_node.name))
                {
                    let base = base_node.make_mut();
                    base.fields.extend(ext_node.fields.iter().cloned());
                } else {
                    out.push(Definition::InputObjectTypeExtension(ext_node));
                }
            }
            other => out.push(other),
        }
    }

    out
}

/// Composes a slice of [`BranchInput`]s into a validated supergraph [`Composition`] (`RUN-009`).
///
/// # Errors
///
/// Returns [`CompositionError`] on duplicate branch identifiers, SDL parse/validation failures,
/// `GQL-004` reserved-name violations (`GAP_RESERVED_NAME`), root-field or namespaced-type
/// collisions (`GAP_AMBIGUOUS_FIELD`), or operation/field mismatches (`GAP_VALIDATION_ERROR`).
pub fn compose(branches: &[BranchInput]) -> Result<Composition, CompositionError> {
    // RUN-009(a): reject duplicate BranchId or BranchUid and sort deterministically by BranchId.
    let mut seen_ids: BTreeMap<&BranchId, &BranchUid> = BTreeMap::new();
    let mut seen_uids: BTreeMap<BranchUid, &BranchId> = BTreeMap::new();
    for b in branches {
        if seen_ids.insert(&b.id, &b.uid).is_some() {
            return Err(CompositionError::new(
                Some(b.id.clone()),
                GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "duplicate BranchId `{}` in composition input (RUN-009)",
                        b.id
                    ),
                ),
            ));
        }
        if let Some(prev_id) = seen_uids.insert(b.uid, &b.id) {
            return Err(CompositionError::new(
                Some(b.id.clone()),
                GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "duplicate BranchUid `{}` shared by branches `{prev_id}` and `{}` (RUN-009)",
                        b.uid, b.id
                    ),
                ),
            ));
        }
    }

    let mut sorted_branches: Vec<&BranchInput> = branches.iter().collect();
    sorted_branches.sort_by(|a, b| a.id.cmp(&b.id));

    let mut builtin_ast = parse_sdl_ast(BUILTIN_SDL, "builtin.graphql")
        .map_err(|e| CompositionError::new(None, e))?;
    let ts_directives_ast =
        parse_sdl_ast(TYPE_SYSTEM_DIRECTIVES_SDL, "type_system_directives.graphql")
            .map_err(|e| CompositionError::new(None, e))?;
    let exec_directives_ast =
        parse_sdl_ast(EXECUTABLE_DIRECTIVES_SDL, "executable_directives.graphql")
            .map_err(|e| CompositionError::new(None, e))?;

    let mut merged_query_fields: Vec<Node<FieldDefinition>> = Vec::new();
    let mut merged_mutation_fields: Vec<Node<FieldDefinition>> = Vec::new();
    let mut merged_subscription_fields: Vec<Node<FieldDefinition>> = Vec::new();
    let mut branch_non_root_asts: Vec<ast::Document> = Vec::with_capacity(sorted_branches.len());

    let mut root_field_owners: BTreeMap<(OpKind, String), BranchId> = BTreeMap::new();
    let mut produced_types: BTreeMap<String, BranchId> = BTreeMap::new();
    let mut warnings: Vec<CompositionWarning> = Vec::new();
    let mut compiled_branches: Vec<Arc<BranchDescriptor>> =
        Vec::with_capacity(sorted_branches.len());

    for branch in sorted_branches {
        let source_name = format!("{}.graphql", branch.id);
        let mut ast_doc = parse_sdl_ast(&branch.description.schema_sdl, &source_name)
            .map_err(|e| CompositionError::new(Some(branch.id.clone()), e))?;

        // GQL-004 reserved-name checks on raw branch AST.
        validate_branch_ast_reserved_names(&ast_doc)
            .map_err(|e| CompositionError::new(Some(branch.id.clone()), e))?;

        let sources = ast_doc.sources.clone();
        let ns = branch.id.as_str().replace('-', "_");

        // Reject standard GraphQL scalar redefinitions and collect within-branch base type names.
        let mut local_defined_types: BTreeSet<String> = BTreeSet::new();
        for def in &ast_doc.definitions {
            let maybe_base_type = match def {
                Definition::ScalarTypeDefinition(d) => Some((&d.name, false)),
                Definition::ObjectTypeDefinition(d) => {
                    Some((&d.name, is_root_operation_object_name(d.name.as_str())))
                }
                Definition::InterfaceTypeDefinition(d) => Some((&d.name, false)),
                Definition::UnionTypeDefinition(d) => Some((&d.name, false)),
                Definition::EnumTypeDefinition(d) => Some((&d.name, false)),
                Definition::InputObjectTypeDefinition(d) => Some((&d.name, false)),
                Definition::ScalarTypeExtension(d) => {
                    if STANDARD_GRAPHQL_SCALARS.contains(&d.name.as_str()) {
                        return Err(err_with_span(
                            Some(&branch.id),
                            ErrorCode::ReservedName,
                            format!(
                                "branch `{}` cannot extend standard GraphQL scalar `{}` (GQL-004)",
                                branch.id, d.name
                            ),
                            d.name.location(),
                            &sources,
                        ));
                    }
                    None
                }
                Definition::SchemaDefinition(sd) => {
                    for root_op in &sd.root_operations {
                        let (op_ty, target_name) = &**root_op;
                        let expected = op_ty.default_type_name();
                        if *target_name != expected {
                            return Err(err_with_span(
                                Some(&branch.id),
                                ErrorCode::ValidationError,
                                format!(
                                    "branch `{}`: custom root operation type `{target_name}` is not supported; use `{expected}`",
                                    branch.id
                                ),
                                target_name.location(),
                                &sources,
                            ));
                        }
                    }
                    None
                }
                Definition::SchemaExtension(se) => {
                    for root_op in &se.root_operations {
                        let (op_ty, target_name) = &**root_op;
                        let expected = op_ty.default_type_name();
                        if *target_name != expected {
                            return Err(err_with_span(
                                Some(&branch.id),
                                ErrorCode::ValidationError,
                                format!(
                                    "branch `{}`: custom root operation type `{target_name}` is not supported; use `{expected}`",
                                    branch.id
                                ),
                                target_name.location(),
                                &sources,
                            ));
                        }
                    }
                    None
                }
                _ => None,
            };

            if let Some((type_ident, is_root)) = maybe_base_type
                && !is_root
            {
                if STANDARD_GRAPHQL_SCALARS.contains(&type_ident.as_str()) {
                    return Err(err_with_span(
                        Some(&branch.id),
                        ErrorCode::ReservedName,
                        format!(
                            "branch `{}` cannot redefine standard GraphQL scalar `{type_ident}` (GQL-004)",
                            branch.id
                        ),
                        type_ident.location(),
                        &sources,
                    ));
                }
                if !local_defined_types.insert(type_ident.as_str().to_owned()) {
                    return Err(err_with_span(
                        Some(&branch.id),
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{}` defines duplicate type `{type_ident}`",
                            branch.id
                        ),
                        type_ident.location(),
                        &sources,
                    ));
                }
                let namespaced = format!("{ns}_{type_ident}");
                if let Some(prev_branch) = produced_types.get(&namespaced) {
                    return Err(err_with_span(
                        Some(&branch.id),
                        ErrorCode::AmbiguousField,
                        format!(
                            "namespaced type `{namespaced}` is produced by both branch `{prev_branch}` and branch `{}` (RUN-009)",
                            branch.id
                        ),
                        type_ident.location(),
                        &sources,
                    ));
                }
                produced_types.insert(namespaced, branch.id.clone());
            }
        }

        let mut local_root_fields: BTreeMap<(OpKind, String), RootFieldRecord> = BTreeMap::new();
        let mut rewritten_non_root_defs: Vec<Definition> =
            Vec::with_capacity(ast_doc.definitions.len());

        for def in std::mem::take(&mut ast_doc.definitions) {
            match def {
                Definition::SchemaDefinition(_) | Definition::SchemaExtension(_) => {}
                Definition::ObjectTypeDefinition(mut obj_node)
                    if is_root_operation_object_name(obj_node.name.as_str()) =>
                {
                    let obj = obj_node.make_mut();
                    let root_name = obj.name.as_str().to_owned();
                    let Some(root_kind) = root_kind_for_name(&root_name) else {
                        continue;
                    };
                    if !obj.implements_interfaces.is_empty() {
                        return Err(err_with_span(
                            Some(&branch.id),
                            ErrorCode::ValidationError,
                            format!(
                                "branch `{}`: root type `{root_name}` must not implement interfaces",
                                branch.id
                            ),
                            obj.name.location(),
                            &sources,
                        ));
                    }
                    sanitize_directives(
                        &mut obj.directives,
                        &root_name,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    for mut field_node in obj.fields.drain(..) {
                        let field = field_node.make_mut();
                        let fname = field.name.as_str().to_owned();
                        if local_root_fields.contains_key(&(root_kind, fname.clone())) {
                            return Err(err_with_span(
                                Some(&branch.id),
                                ErrorCode::ValidationError,
                                format!(
                                    "branch `{}` defines duplicate root field `{root_name}.{fname}`",
                                    branch.id
                                ),
                                field.name.location(),
                                &sources,
                            ));
                        }
                        if let Some(prev_owner) = root_field_owners.get(&(root_kind, fname.clone()))
                        {
                            return Err(err_with_span(
                                Some(&branch.id),
                                ErrorCode::AmbiguousField,
                                format!(
                                    "root field `{root_name}.{fname}` is defined by both branch `{prev_owner}` and branch `{}` (RUN-009)",
                                    branch.id
                                ),
                                field.name.location(),
                                &sources,
                            ));
                        }
                        root_field_owners.insert((root_kind, fname.clone()), branch.id.clone());

                        rewrite_type(&mut field.ty, &ns, &branch.id, &sources, 0)?;
                        let coord = format!("{root_name}.{fname}");
                        rewrite_and_sanitize_input_values(
                            &mut field.arguments,
                            &coord,
                            &ns,
                            branch,
                            &sources,
                            &mut warnings,
                        )?;
                        let sdl_signature = render_clean_field_signature(field);
                        sanitize_directives(
                            &mut field.directives,
                            &coord,
                            branch,
                            true,
                            true,
                            true,
                            &sources,
                            &mut warnings,
                        );
                        let sdl_side_effect = extract_sdl_side_effect_level(&field.directives);
                        local_root_fields.insert(
                            (root_kind, fname),
                            RootFieldRecord {
                                sdl_signature,
                                sdl_side_effect,
                                directives: field.directives.clone(),
                            },
                        );
                        match root_kind {
                            OpKind::Query => merged_query_fields.push(field_node),
                            OpKind::Mutation => merged_mutation_fields.push(field_node),
                            OpKind::Subscription => merged_subscription_fields.push(field_node),
                            OpKind::Task => {}
                        }
                    }
                }
                Definition::ObjectTypeExtension(mut obj_node)
                    if is_root_operation_object_name(obj_node.name.as_str()) =>
                {
                    let obj = obj_node.make_mut();
                    let root_name = obj.name.as_str().to_owned();
                    let Some(root_kind) = root_kind_for_name(&root_name) else {
                        continue;
                    };
                    if !obj.implements_interfaces.is_empty() {
                        return Err(err_with_span(
                            Some(&branch.id),
                            ErrorCode::ValidationError,
                            format!(
                                "branch `{}`: root type extension `{root_name}` must not implement interfaces",
                                branch.id
                            ),
                            obj.name.location(),
                            &sources,
                        ));
                    }
                    sanitize_directives(
                        &mut obj.directives,
                        &root_name,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    for mut field_node in obj.fields.drain(..) {
                        let field = field_node.make_mut();
                        let fname = field.name.as_str().to_owned();
                        if local_root_fields.contains_key(&(root_kind, fname.clone())) {
                            return Err(err_with_span(
                                Some(&branch.id),
                                ErrorCode::ValidationError,
                                format!(
                                    "branch `{}` defines duplicate root field `{root_name}.{fname}`",
                                    branch.id
                                ),
                                field.name.location(),
                                &sources,
                            ));
                        }
                        if let Some(prev_owner) = root_field_owners.get(&(root_kind, fname.clone()))
                        {
                            return Err(err_with_span(
                                Some(&branch.id),
                                ErrorCode::AmbiguousField,
                                format!(
                                    "root field `{root_name}.{fname}` is defined by both branch `{prev_owner}` and branch `{}` (RUN-009)",
                                    branch.id
                                ),
                                field.name.location(),
                                &sources,
                            ));
                        }
                        root_field_owners.insert((root_kind, fname.clone()), branch.id.clone());

                        rewrite_type(&mut field.ty, &ns, &branch.id, &sources, 0)?;
                        let coord = format!("{root_name}.{fname}");
                        rewrite_and_sanitize_input_values(
                            &mut field.arguments,
                            &coord,
                            &ns,
                            branch,
                            &sources,
                            &mut warnings,
                        )?;
                        let sdl_signature = render_clean_field_signature(field);
                        sanitize_directives(
                            &mut field.directives,
                            &coord,
                            branch,
                            true,
                            true,
                            true,
                            &sources,
                            &mut warnings,
                        );
                        let sdl_side_effect = extract_sdl_side_effect_level(&field.directives);
                        local_root_fields.insert(
                            (root_kind, fname),
                            RootFieldRecord {
                                sdl_signature,
                                sdl_side_effect,
                                directives: field.directives.clone(),
                            },
                        );
                        match root_kind {
                            OpKind::Query => merged_query_fields.push(field_node),
                            OpKind::Mutation => merged_mutation_fields.push(field_node),
                            OpKind::Subscription => merged_subscription_fields.push(field_node),
                            OpKind::Task => {}
                        }
                    }
                }
                Definition::ObjectTypeDefinition(mut obj_node) => {
                    let obj = obj_node.make_mut();
                    obj.name = namespace_name(&ns, &obj.name, &branch.id, &sources)?;
                    let obj_label = obj.name.as_str().to_owned();
                    for iface in &mut obj.implements_interfaces {
                        *iface = namespace_name(&ns, iface, &branch.id, &sources)?;
                    }
                    sanitize_directives(
                        &mut obj.directives,
                        &obj_label,
                        branch,
                        false,
                        false,
                        true,
                        &sources,
                        &mut warnings,
                    );
                    for field_node in &mut obj.fields {
                        let field = field_node.make_mut();
                        rewrite_type(&mut field.ty, &ns, &branch.id, &sources, 0)?;
                        let coord = format!("{obj_label}.{}", field.name);
                        rewrite_and_sanitize_input_values(
                            &mut field.arguments,
                            &coord,
                            &ns,
                            branch,
                            &sources,
                            &mut warnings,
                        )?;
                        sanitize_directives(
                            &mut field.directives,
                            &coord,
                            branch,
                            true,
                            false,
                            false,
                            &sources,
                            &mut warnings,
                        );
                    }
                    rewritten_non_root_defs.push(Definition::ObjectTypeDefinition(obj_node));
                }
                Definition::ObjectTypeExtension(mut obj_node) => {
                    let obj = obj_node.make_mut();
                    obj.name = namespace_name(&ns, &obj.name, &branch.id, &sources)?;
                    let obj_label = obj.name.as_str().to_owned();
                    for iface in &mut obj.implements_interfaces {
                        *iface = namespace_name(&ns, iface, &branch.id, &sources)?;
                    }
                    sanitize_directives(
                        &mut obj.directives,
                        &obj_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    for field_node in &mut obj.fields {
                        let field = field_node.make_mut();
                        rewrite_type(&mut field.ty, &ns, &branch.id, &sources, 0)?;
                        let coord = format!("{obj_label}.{}", field.name);
                        rewrite_and_sanitize_input_values(
                            &mut field.arguments,
                            &coord,
                            &ns,
                            branch,
                            &sources,
                            &mut warnings,
                        )?;
                        sanitize_directives(
                            &mut field.directives,
                            &coord,
                            branch,
                            true,
                            false,
                            false,
                            &sources,
                            &mut warnings,
                        );
                    }
                    rewritten_non_root_defs.push(Definition::ObjectTypeExtension(obj_node));
                }
                Definition::InterfaceTypeDefinition(mut iface_node) => {
                    let iface = iface_node.make_mut();
                    iface.name = namespace_name(&ns, &iface.name, &branch.id, &sources)?;
                    let iface_label = iface.name.as_str().to_owned();
                    for parent_iface in &mut iface.implements_interfaces {
                        *parent_iface = namespace_name(&ns, parent_iface, &branch.id, &sources)?;
                    }
                    sanitize_directives(
                        &mut iface.directives,
                        &iface_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    for field_node in &mut iface.fields {
                        let field = field_node.make_mut();
                        rewrite_type(&mut field.ty, &ns, &branch.id, &sources, 0)?;
                        let coord = format!("{iface_label}.{}", field.name);
                        rewrite_and_sanitize_input_values(
                            &mut field.arguments,
                            &coord,
                            &ns,
                            branch,
                            &sources,
                            &mut warnings,
                        )?;
                        sanitize_directives(
                            &mut field.directives,
                            &coord,
                            branch,
                            true,
                            false,
                            false,
                            &sources,
                            &mut warnings,
                        );
                    }
                    rewritten_non_root_defs.push(Definition::InterfaceTypeDefinition(iface_node));
                }
                Definition::InterfaceTypeExtension(mut iface_node) => {
                    let iface = iface_node.make_mut();
                    iface.name = namespace_name(&ns, &iface.name, &branch.id, &sources)?;
                    let iface_label = iface.name.as_str().to_owned();
                    for parent_iface in &mut iface.implements_interfaces {
                        *parent_iface = namespace_name(&ns, parent_iface, &branch.id, &sources)?;
                    }
                    sanitize_directives(
                        &mut iface.directives,
                        &iface_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    for field_node in &mut iface.fields {
                        let field = field_node.make_mut();
                        rewrite_type(&mut field.ty, &ns, &branch.id, &sources, 0)?;
                        let coord = format!("{iface_label}.{}", field.name);
                        rewrite_and_sanitize_input_values(
                            &mut field.arguments,
                            &coord,
                            &ns,
                            branch,
                            &sources,
                            &mut warnings,
                        )?;
                        sanitize_directives(
                            &mut field.directives,
                            &coord,
                            branch,
                            true,
                            false,
                            false,
                            &sources,
                            &mut warnings,
                        );
                    }
                    rewritten_non_root_defs.push(Definition::InterfaceTypeExtension(iface_node));
                }
                Definition::UnionTypeDefinition(mut union_node) => {
                    let u = union_node.make_mut();
                    u.name = namespace_name(&ns, &u.name, &branch.id, &sources)?;
                    let u_label = u.name.as_str().to_owned();
                    for member in &mut u.members {
                        *member = namespace_name(&ns, member, &branch.id, &sources)?;
                    }
                    sanitize_directives(
                        &mut u.directives,
                        &u_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewritten_non_root_defs.push(Definition::UnionTypeDefinition(union_node));
                }
                Definition::UnionTypeExtension(mut union_node) => {
                    let u = union_node.make_mut();
                    u.name = namespace_name(&ns, &u.name, &branch.id, &sources)?;
                    let u_label = u.name.as_str().to_owned();
                    for member in &mut u.members {
                        *member = namespace_name(&ns, member, &branch.id, &sources)?;
                    }
                    sanitize_directives(
                        &mut u.directives,
                        &u_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewritten_non_root_defs.push(Definition::UnionTypeExtension(union_node));
                }
                Definition::EnumTypeDefinition(mut enum_node) => {
                    let e = enum_node.make_mut();
                    e.name = namespace_name(&ns, &e.name, &branch.id, &sources)?;
                    let e_label = e.name.as_str().to_owned();
                    sanitize_directives(
                        &mut e.directives,
                        &e_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewritten_non_root_defs.push(Definition::EnumTypeDefinition(enum_node));
                }
                Definition::EnumTypeExtension(mut enum_node) => {
                    let e = enum_node.make_mut();
                    e.name = namespace_name(&ns, &e.name, &branch.id, &sources)?;
                    let e_label = e.name.as_str().to_owned();
                    sanitize_directives(
                        &mut e.directives,
                        &e_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewritten_non_root_defs.push(Definition::EnumTypeExtension(enum_node));
                }
                Definition::InputObjectTypeDefinition(mut input_node) => {
                    let inp = input_node.make_mut();
                    inp.name = namespace_name(&ns, &inp.name, &branch.id, &sources)?;
                    let inp_label = inp.name.as_str().to_owned();
                    sanitize_directives(
                        &mut inp.directives,
                        &inp_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewrite_and_sanitize_input_values(
                        &mut inp.fields,
                        &inp_label,
                        &ns,
                        branch,
                        &sources,
                        &mut warnings,
                    )?;
                    rewritten_non_root_defs.push(Definition::InputObjectTypeDefinition(input_node));
                }
                Definition::InputObjectTypeExtension(mut input_node) => {
                    let inp = input_node.make_mut();
                    inp.name = namespace_name(&ns, &inp.name, &branch.id, &sources)?;
                    let inp_label = inp.name.as_str().to_owned();
                    sanitize_directives(
                        &mut inp.directives,
                        &inp_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewrite_and_sanitize_input_values(
                        &mut inp.fields,
                        &inp_label,
                        &ns,
                        branch,
                        &sources,
                        &mut warnings,
                    )?;
                    rewritten_non_root_defs.push(Definition::InputObjectTypeExtension(input_node));
                }
                Definition::ScalarTypeDefinition(mut scalar_node) => {
                    let s = scalar_node.make_mut();
                    s.name = namespace_name(&ns, &s.name, &branch.id, &sources)?;
                    let s_label = s.name.as_str().to_owned();
                    sanitize_directives(
                        &mut s.directives,
                        &s_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewritten_non_root_defs.push(Definition::ScalarTypeDefinition(scalar_node));
                }
                Definition::ScalarTypeExtension(mut scalar_node) => {
                    let s = scalar_node.make_mut();
                    s.name = namespace_name(&ns, &s.name, &branch.id, &sources)?;
                    let s_label = s.name.as_str().to_owned();
                    sanitize_directives(
                        &mut s.directives,
                        &s_label,
                        branch,
                        false,
                        false,
                        false,
                        &sources,
                        &mut warnings,
                    );
                    rewritten_non_root_defs.push(Definition::ScalarTypeExtension(scalar_node));
                }
                Definition::DirectiveDefinition(mut dir_node) => {
                    let d = dir_node.make_mut();
                    let d_label = format!("@{}", d.name);
                    rewrite_and_sanitize_input_values(
                        &mut d.arguments,
                        &d_label,
                        &ns,
                        branch,
                        &sources,
                        &mut warnings,
                    )?;
                    rewritten_non_root_defs.push(Definition::DirectiveDefinition(dir_node));
                }
                Definition::OperationDefinition(_) | Definition::FragmentDefinition(_) => {
                    return Err(CompositionError::new(
                        Some(branch.id.clone()),
                        GapError::new(
                            ErrorCode::ValidationError,
                            format!(
                                "branch `{}`: executable definitions are forbidden in branch SDL",
                                branch.id
                            ),
                        ),
                    ));
                }
            }
        }

        ast_doc.definitions = fold_same_branch_extensions(rewritten_non_root_defs);
        branch_non_root_asts.push(ast_doc);

        // RUN-009(d): 1-to-1 correspondence between OperationDescriptors and branch root fields.
        let mut matched_root_fields: BTreeSet<(OpKind, String)> = BTreeSet::new();
        let mut compiled_ops: Vec<Arc<OperationDescriptor>> =
            Vec::with_capacity(branch.description.operations.len());

        for proto_op in &branch.description.operations {
            if !is_valid_graphql_name(&proto_op.name) {
                return Err(CompositionError::new(
                    Some(branch.id.clone()),
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{}`: operation name `{}` is not a valid GraphQL identifier",
                            branch.id, proto_op.name
                        ),
                    ),
                ));
            }
            let op_id = OperationId::from_parts(&branch.id, &proto_op.name).map_err(|err| {
                CompositionError::new(
                    Some(branch.id.clone()),
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{}`: invalid operation `{}`: {err}",
                            branch.id, proto_op.name
                        ),
                    ),
                )
            })?;
            let op_kind = convert_proto_op_kind(proto_op.kind, &branch.id, &proto_op.name)?;
            let key = (op_kind, proto_op.name.clone());

            if !matched_root_fields.insert(key.clone()) {
                return Err(CompositionError::new(
                    Some(branch.id.clone()),
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{}`: duplicate OperationDescriptor for `{}.{}` (RUN-009)",
                            branch.id,
                            root_name_for_kind(op_kind),
                            proto_op.name
                        ),
                    ),
                ));
            }

            let Some(field_rec) = local_root_fields.get(&key) else {
                return Err(CompositionError::new(
                    Some(branch.id.clone()),
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{}`: operation `{}` ({:?}) has no matching root field `{}.{}` in branch SDL (RUN-009)",
                            branch.id,
                            proto_op.name,
                            op_kind,
                            root_name_for_kind(op_kind),
                            proto_op.name
                        ),
                    ),
                ));
            };

            let desc_hash = compute_operation_desc_hash(&branch.id, proto_op)?;
            let group = compute_per_operation_group_id(&op_id, op_kind, &field_rec.sdl_signature);
            let side_effect = convert_side_effect(
                branch.trust,
                proto_op.side_effect_hint,
                field_rec.sdl_side_effect,
            );
            let idempotent = if branch.trust == TrustTier::T2 {
                false
            } else {
                proto_op.idempotent_hint
            };
            let cost = extract_sdl_cost_hint(&field_rec.directives, proto_op.cost_weight);
            let examples = proto_op
                .examples
                .iter()
                .map(|ex| Example {
                    intent: ex.intent.clone(),
                    positive: !ex.negative,
                    query_snippet: if ex.gapql.is_empty() {
                        None
                    } else {
                        Some(ex.gapql.clone())
                    },
                })
                .collect();

            compiled_ops.push(Arc::new(OperationDescriptor {
                id: op_id,
                group,
                kind: op_kind,
                side_effect,
                idempotent,
                cost,
                desc_hash,
                sdl: Arc::from(field_rec.sdl_signature.as_str()),
                examples,
            }));
        }

        for (op_kind, field_name) in local_root_fields.keys() {
            let key = (*op_kind, field_name.clone());
            if !matched_root_fields.contains(&key) {
                return Err(CompositionError::new(
                    Some(branch.id.clone()),
                    GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "branch `{}`: root field `{}.{field_name}` has no matching OperationDescriptor (RUN-009)",
                            branch.id,
                            root_name_for_kind(*op_kind)
                        ),
                    ),
                ));
            }
        }

        compiled_ops.sort_by(|a, b| a.id.cmp(&b.id));

        let mut sorted_desc_hashes: Vec<[u8; 32]> =
            compiled_ops.iter().map(|o| o.desc_hash).collect();
        sorted_desc_hashes.sort_unstable();
        let mut pinned_hasher = blake3::Hasher::new();
        for h in &sorted_desc_hashes {
            pinned_hasher.update(h);
        }
        let pinned_desc_hash: [u8; 32] = *pinned_hasher.finalize().as_bytes();

        let synopsis = branch
            .description
            .meta
            .as_ref()
            .map(|m| clip_utf8_char_boundary(&m.synopsis_hint, MAX_SYNOPSIS_BYTES).to_owned())
            .unwrap_or_default();

        compiled_branches.push(Arc::new(BranchDescriptor {
            uid: branch.uid,
            id: branch.id.clone(),
            protocol: branch.protocol,
            trust: branch.trust,
            endpoint: branch.endpoint.clone(),
            spec_hash: parse_spec_hash(&branch.description.spec_hash),
            pinned_desc_hash,
            capabilities: convert_capabilities(branch.description.capabilities.as_ref()),
            limits: convert_limits(branch.description.limits.as_ref()),
            synopsis,
            operations: compiled_ops,
            status: branch.status,
        }));
    }

    // Merge collected root fields directly onto GAP's built-in Query / Mutation / Subscription definitions.
    for def in &mut builtin_ast.definitions {
        if let Definition::ObjectTypeDefinition(obj_node) = def {
            match obj_node.name.as_str() {
                "Query" if !merged_query_fields.is_empty() => {
                    obj_node
                        .make_mut()
                        .fields
                        .extend(merged_query_fields.drain(..));
                }
                "Mutation" if !merged_mutation_fields.is_empty() => {
                    obj_node
                        .make_mut()
                        .fields
                        .extend(merged_mutation_fields.drain(..));
                }
                "Subscription" if !merged_subscription_fields.is_empty() => {
                    obj_node
                        .make_mut()
                        .fields
                        .extend(merged_subscription_fields.drain(..));
                }
                _ => {}
            }
        }
    }

    let mut builder = Schema::builder()
        .add_ast(&builtin_ast)
        .add_ast(&ts_directives_ast)
        .add_ast(&exec_directives_ast);
    for branch_ast in &branch_non_root_asts {
        builder = builder.add_ast(branch_ast);
    }

    let mut schema = builder.build().map_err(|with_errs| {
        let gap_err = if let Some(diag) = with_errs.errors.iter().next() {
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
                "failed to build composed supergraph schema",
            )
        };
        CompositionError::new(None, gap_err)
    })?;

    inject_gap_introspection_types(&mut schema);

    let valid_standalone = schema.validate().map_err(|with_errs| {
        let gap_err = if let Some(diag) = with_errs.errors.iter().next() {
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
                "composed supergraph schema failed GraphQL validation",
            )
        };
        CompositionError::new(None, gap_err)
    })?;

    let mut inner_schema = valid_standalone.into_inner();
    validate_oneof_schema_definitions(&inner_schema).map_err(|e| CompositionError::new(None, e))?;
    gap_ql::cost::validate_join_cycles(&inner_schema)
        .map_err(|e| CompositionError::new(None, e))?;

    // Render standalone supergraph SDL BEFORE injecting programmatic `__search` / `__metaGraph`
    // so `Composition.sdl` is 100% valid standalone GraphQL SDL (`GQL-004`).
    let sdl = inner_schema.to_string();

    inject_gap_introspection_fields(&mut inner_schema)
        .map_err(|e| CompositionError::new(None, e))?;
    let ownership = derive_ownership_from_schema(&inner_schema);
    let branch_set_hash = compute_branch_set_hash(&compiled_branches)?;

    Ok(Composition {
        schema: Valid::assume_valid(inner_schema),
        sdl,
        branches: compiled_branches,
        ownership,
        warnings,
        branch_set_hash,
    })
}
