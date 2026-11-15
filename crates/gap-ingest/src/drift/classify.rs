//! Connector schema and metadata drift classification (`WP-M4-09`, `ING-052`, `SEC-082`).
//!
//! Compares an incoming [`DescribeResponse`] against the administrator-approved pinned baseline:
//! - Strips comments, descriptions, and definition ordering before classifying SDL changes into
//!   [`DriftKind::NotModified`], [`DriftKind::Cosmetic`], [`DriftKind::SchemaCompatible`],
//!   [`DriftKind::SchemaBreaking`], or [`DriftKind::DescriptionChange`].
//! - Recomputes GAP's own operation description hashes (`desc_hash`, `SEC-082`) from sanitized
//!   [`UntrustedText`] rather than trusting the connector's self-reported `spec_hash`.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;

use gap_proto::ngc::v1::{
    ConnectorDescription, DescribeResponse, OperationDescriptor, SideEffectHint,
};
use gap_ql::schema::{validate_branch_sdl_reserved_names, with_gap_builtins};
use gap_ql::sha256_bytes;

use crate::cisg::UntrustedText;

/// Deterministic classification of connector drift across `Describe` polls (`ING-052`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DriftKind {
    /// Connector returned `not_modified = true` or byte-identical schema + pinned descriptions.
    NotModified,
    /// Only whitespace, comments, ordering, or non-semantic metadata changed (auto-accepted).
    Cosmetic,
    /// Additive non-breaking schema extension (emits `PROPOSAL`; branch remains `ACTIVE`).
    SchemaCompatible,
    /// Breaking schema or contract change (sets `DRIFT`, emits breaking `PROPOSAL`).
    SchemaBreaking,
    /// Operation description, examples, or untrusted annotations diverged from `pinned_desc_hash` (`INV-08`, `SEC-082`).
    DescriptionChange,
}

impl DriftKind {
    /// Wire / log label matching `spec/05-ingestion.md` (`ING-052`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotModified => "NOT_MODIFIED",
            Self::Cosmetic => "COSMETIC",
            Self::SchemaCompatible => "SCHEMA_COMPATIBLE",
            Self::SchemaBreaking => "SCHEMA_BREAKING",
            Self::DescriptionChange => "DESCRIPTION_CHANGE",
        }
    }

    /// Returns `true` iff this classification forces the branch into `DRIFT` (`ING-052`, `SEC-082`).
    #[must_use]
    pub const fn triggers_drift(self) -> bool {
        matches!(self, Self::SchemaBreaking | Self::DescriptionChange)
    }

    /// Returns `true` iff this classification should emit an ingestion `Proposal` (`ING-052`).
    #[must_use]
    pub const fn emits_proposal(self) -> bool {
        matches!(
            self,
            Self::SchemaCompatible | Self::SchemaBreaking | Self::DescriptionChange
        )
    }
}

impl fmt::Display for DriftKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed errors emitted while pinning or classifying connector descriptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftClassificationError {
    /// Connector returned `not_modified = false` without a `ConnectorDescription` payload.
    MissingDescription,
    /// Incoming `schema_sdl` failed reserved-name or GraphQL SDL validation (`GQL-004`).
    InvalidSchema(String),
}

impl fmt::Display for DriftClassificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDescription => {
                write!(
                    f,
                    "connector returned not_modified=false without description"
                )
            }
            Self::InvalidSchema(msg) => write!(f, "invalid connector schema_sdl: {msg}"),
        }
    }
}

impl StdError for DriftClassificationError {}

/// Approved, sanitized operation metadata pinned at branch approval (`INV-08`, `SEC-082`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedOperation {
    /// Operation name inside the branch namespace.
    pub name: String,
    /// Raw proto operation kind (`OpKind as i32`).
    pub kind: i32,
    /// Verified/sanitized operation description cached for pinned serving (`INV-08`).
    pub sanitized_description: UntrustedText,
    /// Sanitized positive/negative intent examples.
    pub sanitized_examples: Vec<(String, String, bool)>,
    /// Sanitized untrusted annotation pairs sorted by key.
    pub sanitized_annotations: BTreeMap<String, String>,
    /// Deterministic side-effect hint rank recorded at approval.
    pub side_effect_rank: u8,
    /// Deterministic structural schema fingerprint of `input_schema` and `output_schema`.
    pub signature_hash: [u8; 32],
    /// GAP-computed `desc_hash` covering description, examples, and annotations (`SEC-082`).
    pub desc_hash: [u8; 32],
}

/// Complete administrator-approved pin for a connector's `Describe` payload (`INV-08`, `SEC-082`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedConnectorSnapshot {
    /// Upstream `spec_hash` advertised at approval (sent as `known_spec_hash` on polls).
    pub advertised_spec_hash: String,
    /// GAP-computed raw digest over `(schema_sdl, operations)` to detect cosmetic diffs.
    pub raw_payload_hash: [u8; 32],
    /// GAP-computed canonical structural schema digest (whitespace/comment/order independent).
    pub canonical_schema_hash: [u8; 32],
    /// GAP-computed combined `pinned_desc_hash` across all operation descriptions & SDL comments/docs (`SEC-082`).
    pub pinned_desc_hash: [u8; 32],
    /// Sanitized connector synopsis cached from approval (`INV-08`).
    pub sanitized_synopsis: UntrustedText,
    /// Per-operation pinned descriptors keyed by operation name.
    pub operations: BTreeMap<String, PinnedOperation>,
    /// Parsed canonical GraphQL SDL model for breaking vs compatible diffing.
    parsed_sdl: ParsedSdl,
    /// Raw normalized schema signatures keyed by operation name for JSON Schema comparison.
    op_schemas: BTreeMap<String, OpJsonSchemas>,
}

/// Detailed result produced by [`classify_connector_drift`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftClassification {
    /// Primary classification (`ING-052`).
    pub kind: DriftKind,
    /// Human-readable evidence strings explaining every detected divergence (never raw injection).
    pub reasons: Vec<String>,
    /// Operations whose `desc_hash` changed relative to `pinned_desc_hash` (`SEC-082`).
    pub drifted_operations: Vec<String>,
    /// Operations added compatibly compared to the pinned baseline.
    pub added_operations: Vec<String>,
    /// Operations removed or broken relative to the pinned baseline.
    pub breaking_operations: Vec<String>,
    /// Candidate pinned state if an administrator later approves this snapshot (`approve_repin`).
    pub candidate: Option<PinnedConnectorSnapshot>,
}

/// Validates and builds a pinned baseline from an administrator-approved [`ConnectorDescription`].
///
/// # Errors
///
/// Returns [`DriftClassificationError::InvalidSchema`] when `description.schema_sdl` violates
/// `GQL-004` reserved names or GraphQL SDL validation.
pub fn pin_connector_description(
    description: &ConnectorDescription,
) -> Result<PinnedConnectorSnapshot, DriftClassificationError> {
    if !description.schema_sdl.trim().is_empty() {
        validate_branch_sdl_reserved_names(&description.schema_sdl)
            .map_err(|err| DriftClassificationError::InvalidSchema(err.message().to_owned()))?;
        with_gap_builtins(&description.schema_sdl)
            .map_err(|err| DriftClassificationError::InvalidSchema(err.message().to_owned()))?;
    }

    let parsed_sdl = parse_sdl(&description.schema_sdl);
    let sanitized_synopsis = UntrustedText::from_raw(
        description
            .meta
            .as_ref()
            .map_or("", |m| m.synopsis_hint.as_str()),
    );

    let mut operations = BTreeMap::new();
    let mut op_schemas = BTreeMap::new();
    let mut canonical_op_sigs = Vec::with_capacity(description.operations.len());
    let mut desc_hash_inputs = Vec::with_capacity(description.operations.len() + 2);
    desc_hash_inputs.push(format!("synopsis:{}", sanitized_synopsis.hash));
    desc_hash_inputs.push(format!("sdl_docs:{}", parsed_sdl.descriptions_digest_hex()));

    let mut sorted_ops: Vec<&OperationDescriptor> = description.operations.iter().collect();
    sorted_ops.sort_by(|a, b| a.name.cmp(&b.name));

    for op in sorted_ops {
        let pinned_op = build_pinned_operation(op);
        let parsed_io = parse_op_json_schemas(op);
        canonical_op_sigs.push(format!(
            "op:{}:{}:{}:{}:{}",
            pinned_op.name,
            pinned_op.kind,
            pinned_op.side_effect_rank,
            op.idempotent_hint,
            hex32(&pinned_op.signature_hash)
        ));
        desc_hash_inputs.push(format!(
            "op_desc:{}:{}",
            pinned_op.name,
            hex32(&pinned_op.desc_hash)
        ));
        op_schemas.insert(pinned_op.name.clone(), parsed_io);
        operations.insert(pinned_op.name.clone(), pinned_op);
    }

    let canonical_schema_payload = format!(
        "sdl={}\nops={}",
        parsed_sdl.canonical_structural_text(),
        canonical_op_sigs.join("\n")
    );
    let canonical_schema_hash = sha256_bytes(canonical_schema_payload.as_bytes());
    let pinned_desc_hash = sha256_bytes(desc_hash_inputs.join("\n").as_bytes());
    let raw_payload_hash = compute_raw_payload_hash(description);

    Ok(PinnedConnectorSnapshot {
        advertised_spec_hash: description.spec_hash.clone(),
        raw_payload_hash,
        canonical_schema_hash,
        pinned_desc_hash,
        sanitized_synopsis,
        operations,
        parsed_sdl,
        op_schemas,
    })
}

/// Classifies drift between a pinned baseline and an incoming [`DescribeResponse`] (`ING-052`, `SEC-082`).
///
/// # Errors
///
/// Returns [`DriftClassificationError`] if `response` omits `description` when `not_modified`
/// is `false` or contains invalid `schema_sdl`.
pub fn classify_connector_drift(
    pinned: &PinnedConnectorSnapshot,
    response: &DescribeResponse,
) -> Result<DriftClassification, DriftClassificationError> {
    if response.not_modified {
        return Ok(DriftClassification {
            kind: DriftKind::NotModified,
            reasons: Vec::new(),
            drifted_operations: Vec::new(),
            added_operations: Vec::new(),
            breaking_operations: Vec::new(),
            candidate: None,
        });
    }

    let desc = response
        .description
        .as_ref()
        .ok_or(DriftClassificationError::MissingDescription)?;
    let incoming = pin_connector_description(desc)?;

    if incoming.raw_payload_hash == pinned.raw_payload_hash
        && incoming.advertised_spec_hash == pinned.advertised_spec_hash
        && incoming.pinned_desc_hash == pinned.pinned_desc_hash
    {
        return Ok(DriftClassification {
            kind: DriftKind::NotModified,
            reasons: Vec::new(),
            drifted_operations: Vec::new(),
            added_operations: Vec::new(),
            breaking_operations: Vec::new(),
            candidate: Some(incoming),
        });
    }

    let mut breaking_reasons = Vec::new();
    let mut compatible_reasons = Vec::new();
    let mut description_reasons = Vec::new();
    let mut drifted_operations = Vec::new();
    let mut added_operations = Vec::new();
    let mut breaking_operations = BTreeSet::new();

    diff_sdl(
        &pinned.parsed_sdl,
        &incoming.parsed_sdl,
        &mut breaking_reasons,
        &mut compatible_reasons,
        &mut breaking_operations,
    );

    diff_operations(
        pinned,
        &incoming,
        &mut breaking_reasons,
        &mut compatible_reasons,
        &mut description_reasons,
        &mut drifted_operations,
        &mut added_operations,
        &mut breaking_operations,
    );

    if pinned.sanitized_synopsis.hash != incoming.sanitized_synopsis.hash {
        description_reasons.push("connector meta.synopsis_hint changed".to_owned());
    }
    if pinned.parsed_sdl.descriptions != incoming.parsed_sdl.descriptions {
        description_reasons.push("GraphQL schema SDL description text changed".to_owned());
    }

    let kind = if !breaking_reasons.is_empty() {
        DriftKind::SchemaBreaking
    } else if !description_reasons.is_empty() {
        DriftKind::DescriptionChange
    } else if !compatible_reasons.is_empty() {
        DriftKind::SchemaCompatible
    } else {
        DriftKind::Cosmetic
    };

    let mut reasons = Vec::new();
    reasons.extend(breaking_reasons);
    reasons.extend(description_reasons);
    reasons.extend(compatible_reasons);
    if kind == DriftKind::Cosmetic && reasons.is_empty() {
        reasons.push("cosmetic formatting, ordering, or comment change only".to_owned());
    }

    Ok(DriftClassification {
        kind,
        reasons,
        drifted_operations,
        added_operations,
        breaking_operations: breaking_operations.into_iter().collect(),
        candidate: Some(incoming),
    })
}

fn build_pinned_operation(op: &OperationDescriptor) -> PinnedOperation {
    let sanitized_description = UntrustedText::from_raw(&op.description);

    let mut sanitized_examples: Vec<(String, String, bool)> = op
        .examples
        .iter()
        .map(|ex| {
            (
                UntrustedText::from_raw(&ex.intent).sanitized,
                UntrustedText::from_raw(&ex.gapql).sanitized,
                ex.negative,
            )
        })
        .collect();
    sanitized_examples.sort();

    let mut sanitized_annotations = BTreeMap::new();
    for (k, v) in &op.annotations {
        sanitized_annotations.insert(k.clone(), UntrustedText::from_raw(v).sanitized);
    }

    let side_effect_rank = side_effect_severity_rank(op.side_effect_hint);
    let mut desc_lines = Vec::new();
    desc_lines.push(format!("name={}", op.name));
    desc_lines.push(format!("desc={}", sanitized_description.hash));
    for (intent, gapql, neg) in &sanitized_examples {
        desc_lines.push(format!("ex={neg}:{intent}=>{gapql}"));
    }
    for (k, v) in &sanitized_annotations {
        desc_lines.push(format!("ann={k}={v}"));
    }
    let desc_hash = sha256_bytes(desc_lines.join("\n").as_bytes());

    let signature_payload = format!(
        "name={};kind={};side={};idem={};in={:?};out={:?}",
        op.name, op.kind, side_effect_rank, op.idempotent_hint, op.input_schema, op.output_schema
    );
    let signature_hash = sha256_bytes(signature_payload.as_bytes());

    PinnedOperation {
        name: op.name.clone(),
        kind: op.kind,
        sanitized_description,
        sanitized_examples,
        sanitized_annotations,
        side_effect_rank,
        signature_hash,
        desc_hash,
    }
}

fn side_effect_severity_rank(raw_hint: i32) -> u8 {
    match SideEffectHint::try_from(raw_hint) {
        Ok(SideEffectHint::SeNone) => 0,
        Ok(SideEffectHint::SeReversible) => 1,
        Ok(SideEffectHint::SeIrreversible) => 2,
        Ok(SideEffectHint::SideEffectUnspecified) | Err(_) => 3,
    }
}

fn compute_raw_payload_hash(desc: &ConnectorDescription) -> [u8; 32] {
    let mut parts = Vec::with_capacity(desc.operations.len() + 4);
    parts.push(format!("id={}", desc.connector_id));
    parts.push(format!("proto={}", desc.protocol));
    parts.push(format!("sdl={}", desc.schema_sdl));
    parts.push(format!(
        "synopsis={}",
        desc.meta.as_ref().map_or("", |m| m.synopsis_hint.as_str())
    ));
    for op in &desc.operations {
        parts.push(format!("{op:?}"));
    }
    sha256_bytes(parts.join("\n").as_bytes())
}

#[allow(clippy::too_many_arguments)]
fn diff_operations(
    pinned: &PinnedConnectorSnapshot,
    incoming: &PinnedConnectorSnapshot,
    breaking_reasons: &mut Vec<String>,
    compatible_reasons: &mut Vec<String>,
    description_reasons: &mut Vec<String>,
    drifted_operations: &mut Vec<String>,
    added_operations: &mut Vec<String>,
    breaking_operations: &mut BTreeSet<String>,
) {
    for (op_name, old_op) in &pinned.operations {
        let Some(new_op) = incoming.operations.get(op_name) else {
            breaking_reasons.push(format!("operation `{op_name}` was removed"));
            breaking_operations.insert(op_name.clone());
            continue;
        };

        if new_op.kind != old_op.kind {
            breaking_reasons.push(format!(
                "operation `{op_name}` changed OpKind ({} -> {})",
                old_op.kind, new_op.kind
            ));
            breaking_operations.insert(op_name.clone());
        }

        if new_op.side_effect_rank > old_op.side_effect_rank {
            breaking_reasons.push(format!(
                "operation `{op_name}` escalated side_effect_hint severity ({} -> {})",
                old_op.side_effect_rank, new_op.side_effect_rank
            ));
            breaking_operations.insert(op_name.clone());
        }

        if new_op.desc_hash != old_op.desc_hash {
            description_reasons.push(format!(
                "operation `{op_name}` description, examples, or annotations diverged from pinned_desc_hash"
            ));
            drifted_operations.push(op_name.clone());
        }

        if let (Some(old_io), Some(new_io)) = (
            pinned.op_schemas.get(op_name),
            incoming.op_schemas.get(op_name),
        ) {
            diff_op_json_schemas(
                op_name,
                old_io,
                new_io,
                breaking_reasons,
                compatible_reasons,
                breaking_operations,
            );
        }
    }

    for op_name in incoming.operations.keys() {
        if !pinned.operations.contains_key(op_name) {
            compatible_reasons.push(format!("operation `{op_name}` added"));
            added_operations.push(op_name.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic pure-Rust JSON Schema (`prost_types::Struct` Debug) diffing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct JsonSchemaObj {
    properties: BTreeMap<String, String>,
    required: BTreeSet<String>,
    raw_kind_digest: String,
    present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct OpJsonSchemas {
    input: JsonSchemaObj,
    output: JsonSchemaObj,
}

fn parse_op_json_schemas(op: &OperationDescriptor) -> OpJsonSchemas {
    OpJsonSchemas {
        input: op
            .input_schema
            .as_ref()
            .map_or_else(JsonSchemaObj::default, |s| {
                parse_prost_struct_schema(&format!("{s:?}"))
            }),
        output: op
            .output_schema
            .as_ref()
            .map_or_else(JsonSchemaObj::default, |s| {
                parse_prost_struct_schema(&format!("{s:?}"))
            }),
    }
}

fn parse_prost_struct_schema(debug_str: &str) -> JsonSchemaObj {
    let normalized_full = normalize_prost_debug_types(debug_str);
    let mut obj = JsonSchemaObj {
        properties: BTreeMap::new(),
        required: BTreeSet::new(),
        raw_kind_digest: hex32(&sha256_bytes(normalized_full.as_bytes())),
        present: true,
    };

    if let Some(props_body) = extract_named_struct_fields(debug_str, "\"properties\"") {
        for (prop_name, prop_val) in split_top_level_map_entries(props_body) {
            let prop_name = unquote(&prop_name);
            let canonical_prop = normalize_prost_debug_types(&prop_val);
            obj.properties.insert(prop_name, canonical_prop);
        }
    }

    if let Some(req_body) = extract_named_list_values(debug_str, "\"required\"") {
        for req_item in extract_string_values(req_body) {
            obj.required.insert(req_item);
        }
    }

    obj
}

fn normalize_prost_debug_types(raw: &str) -> String {
    // Exclude `"description"` key inside property schema from structural comparison so prose edits
    // don't mask schema equivalence.
    if let Some(fields_body) = extract_struct_fields_slice(raw) {
        let mut kept = Vec::new();
        for (k, v) in split_top_level_map_entries(fields_body) {
            let clean_k = unquote(&k);
            if clean_k == "description" || clean_k == "title" || clean_k == "examples" {
                continue;
            }
            kept.push(format!("{clean_k}:{}", normalize_prost_debug_types(&v)));
        }
        return format!("{{{}}}", kept.join(","));
    }
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_struct_fields_slice(input: &str) -> Option<&str> {
    let marker = "fields: {";
    let idx = input.find(marker)?;
    let start = idx + marker.len();
    find_matching_delim(&input[start..], '{', '}').map(|end| &input[start..start + end])
}

fn extract_named_struct_fields<'a>(input: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}: Value {{ kind: Some(StructValue(Struct {{ fields: {{");
    let idx = input.find(&needle)?;
    let start = idx + needle.len();
    let len = find_matching_delim(&input[start..], '{', '}')?;
    Some(&input[start..start + len])
}

fn extract_named_list_values<'a>(input: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}: Value {{ kind: Some(ListValue(ListValue {{ values: [");
    let idx = input.find(&needle)?;
    let start = idx + needle.len();
    let len = find_matching_delim(&input[start..], '[', ']')?;
    Some(&input[start..start + len])
}

fn extract_string_values(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let marker = "StringValue(\"";
    let mut rest = input;
    while let Some(idx) = rest.find(marker) {
        let after = &rest[idx + marker.len()..];
        let mut s = String::new();
        let mut escaped = false;
        let mut end_offset = 0;
        for (off, ch) in after.char_indices() {
            if escaped {
                s.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                end_offset = off + ch.len_utf8();
                break;
            } else {
                s.push(ch);
            }
        }
        out.push(s);
        rest = &after[end_offset..];
    }
    out
}

fn split_top_level_map_entries(body: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut entry_start = 0usize;

    for (idx, ch) in body.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            ',' if brace_depth == 0 && bracket_depth == 0 && paren_depth == 0 => {
                let slice = body[entry_start..idx].trim();
                if let Some((k, v)) = split_key_value_once(slice) {
                    entries.push((k, v));
                }
                entry_start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = body[entry_start..].trim();
    if !tail.is_empty()
        && let Some((k, v)) = split_key_value_once(tail)
    {
        entries.push((k, v));
    }
    entries
}

fn split_key_value_once(slice: &str) -> Option<(String, String)> {
    let mut in_str = false;
    let mut escaped = false;
    for (idx, ch) in slice.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
        } else if ch == ':' {
            return Some((
                slice[..idx].trim().to_owned(),
                slice[idx + 1..].trim().to_owned(),
            ));
        }
    }
    None
}

fn find_matching_delim(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 1usize;
    let mut in_str = false;
    let mut escaped = false;
    for (idx, ch) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
        } else if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn diff_op_json_schemas(
    op_name: &str,
    old_io: &OpJsonSchemas,
    new_io: &OpJsonSchemas,
    breaking_reasons: &mut Vec<String>,
    compatible_reasons: &mut Vec<String>,
    breaking_ops: &mut BTreeSet<String>,
) {
    // Input schema diff
    match (old_io.input.present, new_io.input.present) {
        (true, false) => {
            breaking_reasons.push(format!("operation `{op_name}` removed input_schema"));
            breaking_ops.insert(op_name.to_owned());
        }
        (false, true) => {
            if new_io.input.required.is_empty() {
                compatible_reasons
                    .push(format!("operation `{op_name}` added optional input_schema"));
            } else {
                breaking_reasons.push(format!(
                    "operation `{op_name}` added input_schema with required fields"
                ));
                breaking_ops.insert(op_name.to_owned());
            }
        }
        (true, true) => {
            for (prop, old_ty) in &old_io.input.properties {
                match new_io.input.properties.get(prop) {
                    None => {
                        breaking_reasons.push(format!(
                            "operation `{op_name}` removed input property `{prop}`"
                        ));
                        breaking_ops.insert(op_name.to_owned());
                    }
                    Some(new_ty) if new_ty != old_ty => {
                        breaking_reasons.push(format!(
                            "operation `{op_name}` changed input property `{prop}` schema"
                        ));
                        breaking_ops.insert(op_name.to_owned());
                    }
                    Some(_) => {}
                }
            }
            if old_io.input.properties.is_empty()
                && new_io.input.properties.is_empty()
                && old_io.input.raw_kind_digest != new_io.input.raw_kind_digest
            {
                breaking_reasons.push(format!(
                    "operation `{op_name}` changed top-level input_schema shape"
                ));
                breaking_ops.insert(op_name.to_owned());
            }
            for prop in new_io.input.properties.keys() {
                if !old_io.input.properties.contains_key(prop) {
                    if new_io.input.required.contains(prop) {
                        breaking_reasons.push(format!(
                            "operation `{op_name}` added required input property `{prop}`"
                        ));
                        breaking_ops.insert(op_name.to_owned());
                    } else {
                        compatible_reasons.push(format!(
                            "operation `{op_name}` added optional input property `{prop}`"
                        ));
                    }
                }
            }
            for req in &new_io.input.required {
                if old_io.input.properties.contains_key(req) && !old_io.input.required.contains(req)
                {
                    breaking_reasons.push(format!(
                        "operation `{op_name}` tightened input property `{req}` to required"
                    ));
                    breaking_ops.insert(op_name.to_owned());
                }
            }
        }
        (false, false) => {}
    }

    // Output schema diff
    match (old_io.output.present, new_io.output.present) {
        (true, false) => {
            breaking_reasons.push(format!("operation `{op_name}` removed output_schema"));
            breaking_ops.insert(op_name.to_owned());
        }
        (false, true) => {
            compatible_reasons.push(format!("operation `{op_name}` added output_schema"));
        }
        (true, true) => {
            for (prop, old_ty) in &old_io.output.properties {
                match new_io.output.properties.get(prop) {
                    None => {
                        breaking_reasons.push(format!(
                            "operation `{op_name}` removed output property `{prop}`"
                        ));
                        breaking_ops.insert(op_name.to_owned());
                    }
                    Some(new_ty) if new_ty != old_ty => {
                        breaking_reasons.push(format!(
                            "operation `{op_name}` changed output property `{prop}` schema"
                        ));
                        breaking_ops.insert(op_name.to_owned());
                    }
                    Some(_) => {}
                }
            }
            if old_io.output.properties.is_empty()
                && new_io.output.properties.is_empty()
                && old_io.output.raw_kind_digest != new_io.output.raw_kind_digest
            {
                breaking_reasons.push(format!(
                    "operation `{op_name}` changed top-level output_schema shape"
                ));
                breaking_ops.insert(op_name.to_owned());
            }
            for prop in new_io.output.properties.keys() {
                if !old_io.output.properties.contains_key(prop) {
                    compatible_reasons.push(format!(
                        "operation `{op_name}` added output property `{prop}`"
                    ));
                }
            }
        }
        (false, false) => {}
    }
}

// ---------------------------------------------------------------------------
// Deterministic pure-Rust GraphQL SDL structural parser & differ
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedArg {
    arg_type: String,
    has_default: bool,
    directives: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedField {
    args: BTreeMap<String, ParsedArg>,
    return_type: String,
    directives: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedObject {
    kind: &'static str,
    implements: BTreeSet<String>,
    fields: BTreeMap<String, ParsedField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedInputObject {
    fields: BTreeMap<String, ParsedArg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedSdl {
    scalars: BTreeSet<String>,
    enums: BTreeMap<String, BTreeSet<String>>,
    unions: BTreeMap<String, BTreeSet<String>>,
    objects: BTreeMap<String, ParsedObject>,
    inputs: BTreeMap<String, ParsedInputObject>,
    descriptions: BTreeMap<String, String>,
}

impl ParsedSdl {
    fn canonical_structural_text(&self) -> String {
        let mut lines = Vec::new();
        for s in &self.scalars {
            lines.push(format!("scalar:{s}"));
        }
        for (name, vals) in &self.enums {
            let joined: Vec<&str> = vals.iter().map(String::as_str).collect();
            lines.push(format!("enum:{name}={}", joined.join(",")));
        }
        for (name, members) in &self.unions {
            let joined: Vec<&str> = members.iter().map(String::as_str).collect();
            lines.push(format!("union:{name}={}", joined.join("|")));
        }
        for (name, obj) in &self.objects {
            let impls: Vec<&str> = obj.implements.iter().map(String::as_str).collect();
            lines.push(format!(
                "obj:{}:{}:impl=[{}]",
                obj.kind,
                name,
                impls.join(",")
            ));
            for (fname, f) in &obj.fields {
                let args: Vec<String> = f
                    .args
                    .iter()
                    .map(|(an, a)| format!("{an}:{}:def={}", a.arg_type, a.has_default))
                    .collect();
                lines.push(format!(
                    "  field:{}.{}({})->{}",
                    name,
                    fname,
                    args.join(","),
                    f.return_type
                ));
            }
        }
        for (name, inp) in &self.inputs {
            for (fname, f) in &inp.fields {
                lines.push(format!(
                    "  input:{}.{}={}:def={}",
                    name, fname, f.arg_type, f.has_default
                ));
            }
        }
        lines.join("\n")
    }

    fn descriptions_digest_hex(&self) -> String {
        let lines: Vec<String> = self
            .descriptions
            .iter()
            .map(|(k, v)| format!("{k}={}", UntrustedText::from_raw(v).hash))
            .collect();
        hex32(&sha256_bytes(lines.join("\n").as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Name(String),
    StringLit(String),
    Punct(char),
}

fn tokenize_sdl(src: &str) -> Vec<Tok> {
    let mut tokens = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() || b == b',' {
            i += 1;
            continue;
        }
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"\"\"\"") {
            i += 3;
            let start = i;
            while i + 2 < bytes.len() && !bytes[i..].starts_with(b"\"\"\"") {
                i += 1;
            }
            let end = if i + 2 < bytes.len() { i } else { bytes.len() };
            let raw = String::from_utf8_lossy(&bytes[start..end])
                .trim()
                .to_owned();
            tokens.push(Tok::StringLit(raw));
            i = if i + 2 < bytes.len() {
                i + 3
            } else {
                bytes.len()
            };
            continue;
        }
        if b == b'"' {
            i += 1;
            let mut out = String::new();
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    let ch = src[i..].chars().next().unwrap_or('\u{FFFD}');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            tokens.push(Tok::StringLit(out));
            continue;
        }
        if b == b'.' && bytes[i..].starts_with(b"...") {
            tokens.push(Tok::Name("...".to_owned()));
            i += 3;
            continue;
        }
        if matches!(
            b,
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b':' | b'!' | b'=' | b'|' | b'&' | b'@'
        ) {
            tokens.push(Tok::Punct(b as char));
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(
                bytes[i],
                b',' | b'#'
                    | b'"'
                    | b'{'
                    | b'}'
                    | b'('
                    | b')'
                    | b'['
                    | b']'
                    | b':'
                    | b'!'
                    | b'='
                    | b'|'
                    | b'&'
                    | b'@'
            )
        {
            i += 1;
        }
        let word = String::from_utf8_lossy(&bytes[start..i]).into_owned();
        if !word.is_empty() {
            tokens.push(Tok::Name(word));
        }
    }
    tokens
}

fn parse_sdl(src: &str) -> ParsedSdl {
    let toks = tokenize_sdl(src);
    let mut idx = 0usize;
    let mut sdl = ParsedSdl::default();

    while idx < toks.len() {
        let mut pending_desc = None;
        while let Some(Tok::StringLit(s)) = toks.get(idx) {
            pending_desc = Some(s.clone());
            idx += 1;
        }
        let Some(Tok::Name(kw)) = toks.get(idx) else {
            idx += 1;
            continue;
        };

        let is_extend = kw == "extend";
        if is_extend {
            idx += 1;
        }
        let Some(Tok::Name(kind)) = toks.get(idx).cloned() else {
            idx += 1;
            continue;
        };
        idx += 1;

        match kind.as_str() {
            "scalar" => {
                if let Some(Tok::Name(name)) = toks.get(idx).cloned() {
                    if let Some(desc) = pending_desc {
                        sdl.descriptions.insert(format!("scalar:{name}"), desc);
                    }
                    sdl.scalars.insert(name);
                    idx += 1;
                    skip_directives(&toks, &mut idx);
                }
            }
            "enum" => {
                if let Some(Tok::Name(name)) = toks.get(idx).cloned() {
                    if let Some(desc) = pending_desc {
                        sdl.descriptions.insert(format!("enum:{name}"), desc);
                    }
                    idx += 1;
                    skip_directives(&toks, &mut idx);
                    let entry = sdl.enums.entry(name.clone()).or_default();
                    if toks.get(idx) == Some(&Tok::Punct('{')) {
                        idx += 1;
                        while idx < toks.len() && toks.get(idx) != Some(&Tok::Punct('}')) {
                            let mut val_desc = None;
                            while let Some(Tok::StringLit(s)) = toks.get(idx) {
                                val_desc = Some(s.clone());
                                idx += 1;
                            }
                            if let Some(Tok::Name(val)) = toks.get(idx).cloned() {
                                if let Some(d) = val_desc {
                                    sdl.descriptions.insert(format!("enum:{name}.{val}"), d);
                                }
                                entry.insert(val);
                                idx += 1;
                                skip_directives(&toks, &mut idx);
                            } else {
                                idx += 1;
                            }
                        }
                        if toks.get(idx) == Some(&Tok::Punct('}')) {
                            idx += 1;
                        }
                    }
                }
            }
            "union" => {
                if let Some(Tok::Name(name)) = toks.get(idx).cloned() {
                    if let Some(desc) = pending_desc {
                        sdl.descriptions.insert(format!("union:{name}"), desc);
                    }
                    idx += 1;
                    skip_directives(&toks, &mut idx);
                    let entry = sdl.unions.entry(name).or_default();
                    if toks.get(idx) == Some(&Tok::Punct('=')) {
                        idx += 1;
                        if toks.get(idx) == Some(&Tok::Punct('|')) {
                            idx += 1;
                        }
                        while let Some(Tok::Name(member)) = toks.get(idx).cloned() {
                            entry.insert(member);
                            idx += 1;
                            if toks.get(idx) == Some(&Tok::Punct('|')) {
                                idx += 1;
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
            "type" | "interface" => {
                let kind_tag = if kind == "interface" {
                    "interface"
                } else {
                    "type"
                };
                if let Some(Tok::Name(name)) = toks.get(idx).cloned() {
                    if let Some(desc) = pending_desc {
                        sdl.descriptions.insert(format!("{kind_tag}:{name}"), desc);
                    }
                    idx += 1;
                    let obj = sdl
                        .objects
                        .entry(name.clone())
                        .or_insert_with(|| ParsedObject {
                            kind: kind_tag,
                            implements: BTreeSet::new(),
                            fields: BTreeMap::new(),
                        });
                    if toks.get(idx) == Some(&Tok::Name("implements".to_owned())) {
                        idx += 1;
                        if toks.get(idx) == Some(&Tok::Punct('&')) {
                            idx += 1;
                        }
                        while let Some(Tok::Name(iface)) = toks.get(idx).cloned() {
                            obj.implements.insert(iface);
                            idx += 1;
                            if toks.get(idx) == Some(&Tok::Punct('&')) {
                                idx += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    skip_directives(&toks, &mut idx);
                    if toks.get(idx) == Some(&Tok::Punct('{')) {
                        idx += 1;
                        parse_field_block(&toks, &mut idx, &name, obj, &mut sdl.descriptions);
                    }
                }
            }
            "input" => {
                if let Some(Tok::Name(name)) = toks.get(idx).cloned() {
                    if let Some(desc) = pending_desc {
                        sdl.descriptions.insert(format!("input:{name}"), desc);
                    }
                    idx += 1;
                    skip_directives(&toks, &mut idx);
                    let inp = sdl.inputs.entry(name.clone()).or_default();
                    if toks.get(idx) == Some(&Tok::Punct('{')) {
                        idx += 1;
                        parse_input_block(&toks, &mut idx, &name, inp, &mut sdl.descriptions);
                    }
                }
            }
            "directive" | "schema" => {
                // Skip block/line of schema/directive definition cleanly.
                while idx < toks.len() {
                    if toks.get(idx) == Some(&Tok::Punct('{')) {
                        skip_braced(&toks, &mut idx);
                        break;
                    }
                    if matches!(
                        toks.get(idx),
                        Some(Tok::Name(w)) if matches!(
                            w.as_str(),
                            "type" | "interface" | "input" | "enum" | "union" | "scalar" | "extend"
                        )
                    ) {
                        break;
                    }
                    idx += 1;
                }
            }
            _ => {}
        }
    }

    sdl
}

fn parse_field_block(
    toks: &[Tok],
    idx: &mut usize,
    type_name: &str,
    obj: &mut ParsedObject,
    descriptions: &mut BTreeMap<String, String>,
) {
    while *idx < toks.len() && toks.get(*idx) != Some(&Tok::Punct('}')) {
        let mut f_desc = None;
        while let Some(Tok::StringLit(s)) = toks.get(*idx) {
            f_desc = Some(s.clone());
            *idx += 1;
        }
        let Some(Tok::Name(field_name)) = toks.get(*idx).cloned() else {
            *idx += 1;
            continue;
        };
        if let Some(d) = f_desc {
            descriptions.insert(format!("field:{type_name}.{field_name}"), d);
        }
        *idx += 1;

        let mut args = BTreeMap::new();
        if toks.get(*idx) == Some(&Tok::Punct('(')) {
            *idx += 1;
            parse_args_block(
                toks,
                idx,
                &format!("arg:{type_name}.{field_name}"),
                &mut args,
                descriptions,
            );
        }

        let return_type = if toks.get(*idx) == Some(&Tok::Punct(':')) {
            *idx += 1;
            parse_type_ref(toks, idx)
        } else {
            String::new()
        };
        let directives = skip_directives(toks, idx);
        obj.fields.insert(
            field_name,
            ParsedField {
                args,
                return_type,
                directives,
            },
        );
    }
    if toks.get(*idx) == Some(&Tok::Punct('}')) {
        *idx += 1;
    }
}

fn parse_input_block(
    toks: &[Tok],
    idx: &mut usize,
    input_name: &str,
    inp: &mut ParsedInputObject,
    descriptions: &mut BTreeMap<String, String>,
) {
    while *idx < toks.len() && toks.get(*idx) != Some(&Tok::Punct('}')) {
        let mut f_desc = None;
        while let Some(Tok::StringLit(s)) = toks.get(*idx) {
            f_desc = Some(s.clone());
            *idx += 1;
        }
        let Some(Tok::Name(field_name)) = toks.get(*idx).cloned() else {
            *idx += 1;
            continue;
        };
        if let Some(d) = f_desc {
            descriptions.insert(format!("input:{input_name}.{field_name}"), d);
        }
        *idx += 1;
        let arg_type = if toks.get(*idx) == Some(&Tok::Punct(':')) {
            *idx += 1;
            parse_type_ref(toks, idx)
        } else {
            String::new()
        };
        let has_default = skip_default_value(toks, idx);
        let directives = skip_directives(toks, idx);
        inp.fields.insert(
            field_name,
            ParsedArg {
                arg_type,
                has_default,
                directives,
            },
        );
    }
    if toks.get(*idx) == Some(&Tok::Punct('}')) {
        *idx += 1;
    }
}

fn parse_args_block(
    toks: &[Tok],
    idx: &mut usize,
    prefix: &str,
    args: &mut BTreeMap<String, ParsedArg>,
    descriptions: &mut BTreeMap<String, String>,
) {
    while *idx < toks.len() && toks.get(*idx) != Some(&Tok::Punct(')')) {
        let mut a_desc = None;
        while let Some(Tok::StringLit(s)) = toks.get(*idx) {
            a_desc = Some(s.clone());
            *idx += 1;
        }
        let Some(Tok::Name(arg_name)) = toks.get(*idx).cloned() else {
            *idx += 1;
            continue;
        };
        if let Some(d) = a_desc {
            descriptions.insert(format!("{prefix}.{arg_name}"), d);
        }
        *idx += 1;
        let arg_type = if toks.get(*idx) == Some(&Tok::Punct(':')) {
            *idx += 1;
            parse_type_ref(toks, idx)
        } else {
            String::new()
        };
        let has_default = skip_default_value(toks, idx);
        let directives = skip_directives(toks, idx);
        args.insert(
            arg_name,
            ParsedArg {
                arg_type,
                has_default,
                directives,
            },
        );
    }
    if toks.get(*idx) == Some(&Tok::Punct(')')) {
        *idx += 1;
    }
}

fn parse_type_ref(toks: &[Tok], idx: &mut usize) -> String {
    let mut out = String::new();
    if toks.get(*idx) == Some(&Tok::Punct('[')) {
        out.push('[');
        *idx += 1;
        out.push_str(&parse_type_ref(toks, idx));
        if toks.get(*idx) == Some(&Tok::Punct(']')) {
            out.push(']');
            *idx += 1;
        }
    } else if let Some(Tok::Name(n)) = toks.get(*idx) {
        out.push_str(n);
        *idx += 1;
    }
    if toks.get(*idx) == Some(&Tok::Punct('!')) {
        out.push('!');
        *idx += 1;
    }
    out
}

fn skip_default_value(toks: &[Tok], idx: &mut usize) -> bool {
    if toks.get(*idx) != Some(&Tok::Punct('=')) {
        return false;
    }
    *idx += 1;
    match toks.get(*idx) {
        Some(Tok::Punct('{')) => skip_braced(toks, idx),
        Some(Tok::Punct('[')) => skip_bracketed(toks, idx),
        Some(_) => *idx += 1,
        None => {}
    }
    true
}

fn skip_directives(toks: &[Tok], idx: &mut usize) -> Vec<String> {
    let mut dirs = Vec::new();
    while toks.get(*idx) == Some(&Tok::Punct('@')) {
        *idx += 1;
        if let Some(Tok::Name(dname)) = toks.get(*idx).cloned() {
            let mut rendered = format!("@{dname}");
            *idx += 1;
            if toks.get(*idx) == Some(&Tok::Punct('(')) {
                let start = *idx;
                skip_parens(toks, idx);
                let inner: Vec<String> = toks[start..*idx]
                    .iter()
                    .map(|t| match t {
                        Tok::Name(s) | Tok::StringLit(s) => s.clone(),
                        Tok::Punct(c) => c.to_string(),
                    })
                    .collect();
                rendered.push_str(&inner.join(" "));
            }
            dirs.push(rendered);
        }
    }
    dirs.sort();
    dirs
}

fn skip_parens(toks: &[Tok], idx: &mut usize) {
    skip_pair(toks, idx, '(', ')');
}

fn skip_braced(toks: &[Tok], idx: &mut usize) {
    skip_pair(toks, idx, '{', '}');
}

fn skip_bracketed(toks: &[Tok], idx: &mut usize) {
    skip_pair(toks, idx, '[', ']');
}

fn skip_pair(toks: &[Tok], idx: &mut usize, open: char, close: char) {
    if toks.get(*idx) != Some(&Tok::Punct(open)) {
        return;
    }
    let mut depth = 0usize;
    while *idx < toks.len() {
        if toks.get(*idx) == Some(&Tok::Punct(open)) {
            depth += 1;
        } else if toks.get(*idx) == Some(&Tok::Punct(close)) {
            depth = depth.saturating_sub(1);
            *idx += 1;
            if depth == 0 {
                return;
            }
            continue;
        }
        *idx += 1;
    }
}

fn diff_sdl(
    old: &ParsedSdl,
    new: &ParsedSdl,
    breaking_reasons: &mut Vec<String>,
    compatible_reasons: &mut Vec<String>,
    breaking_ops: &mut BTreeSet<String>,
) {
    for scalar in &old.scalars {
        if !new.scalars.contains(scalar) {
            breaking_reasons.push(format!("scalar `{scalar}` removed from SDL"));
        }
    }
    for scalar in &new.scalars {
        if !old.scalars.contains(scalar) {
            compatible_reasons.push(format!("scalar `{scalar}` added to SDL"));
        }
    }

    for (enum_name, old_vals) in &old.enums {
        let Some(new_vals) = new.enums.get(enum_name) else {
            breaking_reasons.push(format!("enum `{enum_name}` removed from SDL"));
            continue;
        };
        for val in old_vals {
            if !new_vals.contains(val) {
                breaking_reasons.push(format!("enum `{enum_name}` removed value `{val}`"));
            }
        }
        for val in new_vals {
            if !old_vals.contains(val) {
                compatible_reasons.push(format!("enum `{enum_name}` added value `{val}`"));
            }
        }
    }
    for enum_name in new.enums.keys() {
        if !old.enums.contains_key(enum_name) {
            compatible_reasons.push(format!("enum `{enum_name}` added to SDL"));
        }
    }

    for (union_name, old_members) in &old.unions {
        let Some(new_members) = new.unions.get(union_name) else {
            breaking_reasons.push(format!("union `{union_name}` removed from SDL"));
            continue;
        };
        for member in old_members {
            if !new_members.contains(member) {
                breaking_reasons.push(format!("union `{union_name}` removed member `{member}`"));
            }
        }
        for member in new_members {
            if !old_members.contains(member) {
                compatible_reasons.push(format!("union `{union_name}` added member `{member}`"));
            }
        }
    }
    for union_name in new.unions.keys() {
        if !old.unions.contains_key(union_name) {
            compatible_reasons.push(format!("union `{union_name}` added to SDL"));
        }
    }

    for (type_name, old_obj) in &old.objects {
        let Some(new_obj) = new.objects.get(type_name) else {
            breaking_reasons.push(format!("{} `{type_name}` removed from SDL", old_obj.kind));
            if matches!(type_name.as_str(), "Query" | "Mutation" | "Subscription") {
                for fname in old_obj.fields.keys() {
                    breaking_ops.insert(fname.clone());
                }
            }
            continue;
        };
        if new_obj.kind != old_obj.kind {
            breaking_reasons.push(format!(
                "type `{type_name}` changed kind ({} -> {})",
                old_obj.kind, new_obj.kind
            ));
        }
        for iface in &old_obj.implements {
            if !new_obj.implements.contains(iface) {
                breaking_reasons.push(format!(
                    "type `{type_name}` stopped implementing interface `{iface}`"
                ));
            }
        }
        let is_root = matches!(type_name.as_str(), "Query" | "Mutation" | "Subscription");
        for (field_name, old_field) in &old_obj.fields {
            let Some(new_field) = new_obj.fields.get(field_name) else {
                breaking_reasons.push(format!("field `{type_name}.{field_name}` removed"));
                if is_root {
                    breaking_ops.insert(field_name.clone());
                }
                continue;
            };
            if new_field.return_type != old_field.return_type {
                breaking_reasons.push(format!(
                    "field `{type_name}.{field_name}` changed return type (`{}` -> `{}`)",
                    old_field.return_type, new_field.return_type
                ));
                if is_root {
                    breaking_ops.insert(field_name.clone());
                }
            }
            for (arg_name, old_arg) in &old_field.args {
                let Some(new_arg) = new_field.args.get(arg_name) else {
                    breaking_reasons.push(format!(
                        "argument `{type_name}.{field_name}({arg_name})` removed"
                    ));
                    if is_root {
                        breaking_ops.insert(field_name.clone());
                    }
                    continue;
                };
                if new_arg.arg_type != old_arg.arg_type {
                    breaking_reasons.push(format!(
                        "argument `{type_name}.{field_name}({arg_name})` changed type (`{}` -> `{}`)",
                        old_arg.arg_type, new_arg.arg_type
                    ));
                    if is_root {
                        breaking_ops.insert(field_name.clone());
                    }
                }
            }
            for (arg_name, new_arg) in &new_field.args {
                if !old_field.args.contains_key(arg_name) {
                    if new_arg.arg_type.ends_with('!') && !new_arg.has_default {
                        breaking_reasons.push(format!(
                            "required non-null argument `{type_name}.{field_name}({arg_name}: {})` added without default",
                            new_arg.arg_type
                        ));
                        if is_root {
                            breaking_ops.insert(field_name.clone());
                        }
                    } else {
                        compatible_reasons.push(format!(
                            "optional argument `{type_name}.{field_name}({arg_name}: {})` added",
                            new_arg.arg_type
                        ));
                    }
                }
            }
        }
        for field_name in new_obj.fields.keys() {
            if !old_obj.fields.contains_key(field_name) {
                compatible_reasons.push(format!("field `{type_name}.{field_name}` added"));
            }
        }
    }
    for (type_name, new_obj) in &new.objects {
        if !old.objects.contains_key(type_name) {
            compatible_reasons.push(format!("{} `{type_name}` added to SDL", new_obj.kind));
        }
    }

    for (inp_name, old_inp) in &old.inputs {
        let Some(new_inp) = new.inputs.get(inp_name) else {
            breaking_reasons.push(format!("input `{inp_name}` removed from SDL"));
            continue;
        };
        for (fname, old_f) in &old_inp.fields {
            let Some(new_f) = new_inp.fields.get(fname) else {
                breaking_reasons.push(format!("input field `{inp_name}.{fname}` removed"));
                continue;
            };
            if new_f.arg_type != old_f.arg_type {
                breaking_reasons.push(format!(
                    "input field `{inp_name}.{fname}` changed type (`{}` -> `{}`)",
                    old_f.arg_type, new_f.arg_type
                ));
            }
        }
        for (fname, new_f) in &new_inp.fields {
            if !old_inp.fields.contains_key(fname) {
                if new_f.arg_type.ends_with('!') && !new_f.has_default {
                    breaking_reasons.push(format!(
                        "required input field `{inp_name}.{fname}: {}` added without default",
                        new_f.arg_type
                    ));
                } else {
                    compatible_reasons.push(format!(
                        "optional input field `{inp_name}.{fname}: {}` added",
                        new_f.arg_type
                    ));
                }
            }
        }
    }
    for inp_name in new.inputs.keys() {
        if !old.inputs.contains_key(inp_name) {
            compatible_reasons.push(format!("input `{inp_name}` added to SDL"));
        }
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        let _ = std::fmt::Write::write_str(&mut out, &format!("{b:02x}"));
    }
    out
}
