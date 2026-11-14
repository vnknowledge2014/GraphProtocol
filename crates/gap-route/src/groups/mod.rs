//! Replica-invariant capability group selection, side-effect filtering, and deterministic
//! GAP-QL candidate skeleton synthesis (`spec/04-routing.md` §5, `spec/02-gap-ql.md` §3;
//! `RTE-030`, `RTE-031`, `RTE-032`, `RTE-033`, `RTE-042`, `GQL-012`, `WP-M2-05`, `WP-M2-08`).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{BranchId, BranchUid, GroupId};
use gap_core::snapshot::{
    BranchDescriptor, BranchStatus, OpKind, OperationDescriptor, SideEffectLevel, Snapshot,
    TrustTier,
};
use gap_core::traits::{ElementKind, SearchHit};
use gap_ql::{
    Schema, Valid, Visibility, minimal_selection_template, parse_executable_ast,
    project_visible_schema, validate_executable_reserved_names,
    validate_unique_input_object_fields,
};

use crate::sanitize::{lint_branch_text, sanitize_default};

/// Default top `K` capability groups surfaced to any routing model (`RTE-030`).
pub const DEFAULT_TOP_K_GROUPS: usize = 15;

/// Minimum allowed `K` limit for capability grouping (`RTE-030`).
pub const MIN_TOP_K_GROUPS: usize = 4;

/// Maximum allowed `K` limit for capability grouping (`RTE-030`).
pub const MAX_TOP_K_GROUPS: usize = 15;

/// Fixed sink key appended after the `<= K` candidate capability groups (`RTE-030`).
pub const SINK_OPTION_KEY: &str = "insufficient_evidence";

/// Fixed sink text specified by `RTE-042` ("None of the other options can fulfil the request.").
pub const SINK_OPTION_TEXT: &str = "None of the other options can fulfil the request.";

/// Single candidate option passed to [`crate::s1_adapter::RoutingModel`] (`RTE-030`, `RTE-042`).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupOption {
    /// Option slot key (`"g0".."g14"` or `"insufficient_evidence"`).
    pub key: String,
    /// Sanitized description text containing NO branch/operation IDs or trust labels (`RTE-042`).
    pub text: String,
    /// Normalized hybrid retrieval prior in `[0, 1]`.
    pub prior: f32,
}

/// Visible, healthy capability group candidate paired with its deterministically chosen primary
/// replica operation (`RTE-030`, `RTE-031`).
#[derive(Debug, Clone)]
pub struct ResolvedGroup {
    /// Capability group identifier.
    pub group_id: GroupId,
    /// Option slot key (`"g0".."g14"`).
    pub option_key: String,
    /// Sanitized model-facing option text (free of IDs/trust labels, `RTE-042`).
    pub option_text: String,
    /// Maximum retrieval score across visible group members (`RTE-030`).
    pub prior: f32,
    /// Selected primary healthy branch ID.
    pub primary_branch: BranchId,
    /// Selected primary healthy branch UID.
    pub primary_branch_uid: BranchUid,
    /// Trust tier of `primary_branch`.
    pub primary_trust: TrustTier,
    /// Effective side-effect level (`TrustTier::T2` forced to `SideEffectLevel::Unknown`, `GQL-032`).
    pub effective_side_effect: SideEffectLevel,
    /// Selected primary operation descriptor.
    pub primary_op: Arc<OperationDescriptor>,
    /// Minimal validated GAP-QL skeleton + variables schema + SDL fragment (`RTE-031`, `GQL-012`).
    pub skeleton: Option<BuiltSkeleton>,
    /// Count of visible eligible members in this group.
    pub member_count: usize,
}

/// Deterministic validated GAP-QL query skeleton and matching JSON Schema (`RTE-031`, `GQL-012`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltSkeleton {
    /// Executable GraphQL document validated against the caller-visible snapshot schema.
    pub skeleton: String,
    /// JSON Schema describing declared skeleton variables (`variablesSchema`).
    pub variables_schema_json: String,
    /// Minimal SDL declaration fragment for progressive disclosure.
    pub field_sdl: String,
}

/// Converts [`SideEffectLevel`] to an explicit severity rank treating `Unknown` as equivalent to
/// `Irreversible` (`RTE-032`).
#[must_use]
pub const fn side_effect_rank(level: SideEffectLevel) -> u8 {
    match level {
        SideEffectLevel::None => 0,
        SideEffectLevel::Reversible => 1,
        SideEffectLevel::Irreversible | SideEffectLevel::Unknown => 2,
    }
}

/// Effective side-effect classification taking branch trust into account (`GQL-032`, `ADR-015`).
#[must_use]
pub const fn effective_side_effect(trust: TrustTier, declared: SideEffectLevel) -> SideEffectLevel {
    if matches!(trust, TrustTier::T2) {
        SideEffectLevel::Unknown
    } else {
        declared
    }
}

/// Returns `true` iff an operation's effective side-effect severity fits within
/// `constraints.max_side_effect` (`RTE-032`).
#[must_use]
pub const fn side_effect_allows(max_allowed: SideEffectLevel, op_level: SideEffectLevel) -> bool {
    side_effect_rank(op_level) <= side_effect_rank(max_allowed)
}

/// String representation matching `spec/02-gap-ql.md` & `RTE-042`.
#[must_use]
pub const fn side_effect_as_str(level: SideEffectLevel) -> &'static str {
    match level {
        SideEffectLevel::None => "NONE",
        SideEffectLevel::Reversible => "REVERSIBLE",
        SideEffectLevel::Irreversible => "IRREVERSIBLE",
        SideEffectLevel::Unknown => "UNKNOWN",
    }
}

/// String representation of [`OpKind`].
#[must_use]
pub const fn op_kind_as_str(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Query => "QUERY",
        OpKind::Mutation => "MUTATION",
        OpKind::Subscription => "SUBSCRIPTION",
        OpKind::Task => "TASK",
    }
}

/// String representation of [`TrustTier`].
#[must_use]
pub const fn trust_as_str(trust: TrustTier) -> &'static str {
    match trust {
        TrustTier::T0 => "T0",
        TrustTier::T1 => "T1",
        TrustTier::T2 => "T2",
    }
}

/// Returns the GraphQL root object name for `kind`.
#[must_use]
pub const fn root_type_for_op_kind(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Query => "Query",
        OpKind::Mutation => "Mutation",
        OpKind::Subscription => "Subscription",
        OpKind::Task => "GapMutation",
    }
}

/// Evaluates whether a branch operation is visible to the current caller under `visibility` (`INV-05`).
#[must_use]
pub fn is_operation_visible(op: &OperationDescriptor, visibility: &dyn Visibility) -> bool {
    let root = root_type_for_op_kind(op.kind);
    visibility.can_see_field(root, op.id.name())
}

/// Groups visibility-filtered, side-effect-permitted search hits by [`GroupId`], keeps the top `K`
/// groups (`clamp(top_k_groups, 4, 15)`), selects each group's deterministic primary replica, and
/// appends `insufficient_evidence` (`RTE-030`, `RTE-032`, `RTE-042`).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_group_options(
    snapshot: &Snapshot,
    hits: &[SearchHit],
    allowed_branches: Option<&BTreeSet<BranchId>>,
    max_side_effect: SideEffectLevel,
    top_k_groups: usize,
    exclude_linted_from_s1: bool,
    visibility: &dyn Visibility,
    sink_prior: f32,
) -> (Vec<ResolvedGroup>, Vec<GroupOption>) {
    let clamped_k = top_k_groups.clamp(MIN_TOP_K_GROUPS, MAX_TOP_K_GROUPS);

    // Map visible operations inside Active branches by `(BranchUid, OperationId)` and `OperationId`.
    struct CandidateReplica {
        score: f32,
        branch: Arc<BranchDescriptor>,
        op: Arc<OperationDescriptor>,
        eff_side_effect: SideEffectLevel,
    }

    let mut per_group: BTreeMap<GroupId, Vec<CandidateReplica>> = BTreeMap::new();

    for hit in hits {
        if hit.kind != ElementKind::Operation && hit.kind != ElementKind::Example {
            continue;
        }
        let Some((branch, op)) = resolve_hit_operation(snapshot, hit) else {
            continue;
        };
        if branch.status != BranchStatus::Active {
            continue;
        }
        if let Some(filter) = allowed_branches
            && !filter.contains(&branch.id)
        {
            continue;
        }
        if !is_operation_visible(&op, visibility) {
            continue;
        }
        let eff = effective_side_effect(branch.trust, op.side_effect.level);
        if !side_effect_allows(max_side_effect, eff) {
            continue;
        }
        if exclude_linted_from_s1 && op_has_lint_findings(&branch, &op) {
            continue;
        }

        let clean_score = if hit.fused_score.is_finite() {
            hit.fused_score.clamp(0.0, 1.0)
        } else {
            0.0
        };

        per_group
            .entry(op.group.clone())
            .or_default()
            .push(CandidateReplica {
                score: clean_score,
                branch,
                op,
                eff_side_effect: eff,
            });
    }

    // Deduplicate replicas per group and pick the deterministic primary replica per group.
    let mut ranked_groups: Vec<(GroupId, f32, CandidateReplica, usize)> =
        Vec::with_capacity(per_group.len());

    for (group_id, mut members) in per_group {
        members.sort_by(|a, b| {
            cmp_f32_desc(a.score, b.score)
                .then_with(|| a.op.id.as_str().cmp(b.op.id.as_str()))
                .then_with(|| a.branch.uid.cmp(&b.branch.uid))
        });

        let mut unique_members: Vec<CandidateReplica> = Vec::with_capacity(members.len());
        let mut seen_ops: BTreeSet<(BranchUid, String)> = BTreeSet::new();
        for m in members {
            let key = (m.branch.uid, m.op.id.as_str().to_owned());
            if seen_ops.insert(key) {
                unique_members.push(m);
            }
        }
        let member_count = unique_members.len();
        if let Some(primary) = unique_members.into_iter().next() {
            let best_score = primary.score;
            ranked_groups.push((group_id, best_score, primary, member_count));
        }
    }

    // Sort groups by descending retrieval score, tie-breaking deterministically by GroupId.
    ranked_groups.sort_by(|a, b| cmp_f32_desc(a.1, b.1).then_with(|| a.0.cmp(&b.0)));
    ranked_groups.truncate(clamped_k);

    let mut resolved = Vec::with_capacity(ranked_groups.len());
    let mut options = Vec::with_capacity(ranked_groups.len() + 1);

    for (slot_idx, (group_id, score, primary, member_count)) in
        ranked_groups.into_iter().enumerate()
    {
        let key = format!("g{slot_idx}");
        let opt_text = build_sanitized_option_text(&primary.branch, &primary.op);
        options.push(GroupOption {
            key: key.clone(),
            text: opt_text.clone(),
            prior: score,
        });
        resolved.push(ResolvedGroup {
            group_id,
            option_key: key,
            option_text: opt_text,
            prior: score,
            primary_branch: primary.branch.id.clone(),
            primary_branch_uid: primary.branch.uid,
            primary_trust: primary.branch.trust,
            effective_side_effect: primary.eff_side_effect,
            primary_op: primary.op,
            skeleton: None,
            member_count,
        });
    }

    options.push(GroupOption {
        key: SINK_OPTION_KEY.to_owned(),
        text: SINK_OPTION_TEXT.to_owned(),
        prior: sink_prior.clamp(0.0, 1.0),
    });

    (resolved, options)
}

fn op_has_lint_findings(branch: &BranchDescriptor, op: &OperationDescriptor) -> bool {
    if !lint_branch_text(&branch.synopsis).is_empty() {
        return true;
    }
    op.examples
        .iter()
        .any(|ex| !lint_branch_text(&ex.intent).is_empty())
}

fn resolve_hit_operation(
    snapshot: &Snapshot,
    hit: &SearchHit,
) -> Option<(Arc<BranchDescriptor>, Arc<OperationDescriptor>)> {
    if let Some(b_uid) = hit.branch_uid
        && let Some(branch) = snapshot.branches.get(&b_uid)
    {
        if let Some(ref target_op) = hit.operation_id
            && let Some(op) = branch.operations.iter().find(|op| &op.id == target_op)
        {
            return Some((Arc::clone(branch), Arc::clone(op)));
        }
        if let Some(op) = branch
            .operations
            .iter()
            .find(|op| op.id.as_str() == hit.element_id)
        {
            return Some((Arc::clone(branch), Arc::clone(op)));
        }
    }

    // Fallback scan by `operation_id` or `element_id` across branches sorted by BranchUid.
    for branch in snapshot.branches.values() {
        if let Some(ref target_op) = hit.operation_id
            && let Some(op) = branch.operations.iter().find(|op| &op.id == target_op)
        {
            return Some((Arc::clone(branch), Arc::clone(op)));
        }
        if let Some(op) = branch
            .operations
            .iter()
            .find(|op| op.id.as_str() == hit.element_id)
        {
            return Some((Arc::clone(branch), Arc::clone(op)));
        }
    }
    None
}

/// Builds sanitized option description text strictly omitting branch IDs, operation IDs, and
/// trust tier labels (`RTE-042`).
#[must_use]
pub fn build_sanitized_option_text(branch: &BranchDescriptor, op: &OperationDescriptor) -> String {
    let mut parts: Vec<String> = Vec::new();
    let op_phrase = humanize_field_name(op.id.name());
    if !op_phrase.is_empty() {
        parts.push(op_phrase);
    }

    for ex in op.examples.iter().filter(|e| e.positive).take(2) {
        let clean = sanitize_default(&ex.intent);
        if !clean.is_empty() {
            parts.push(clean);
        }
    }

    if parts.len() <= 1 {
        let syn = sanitize_default(&branch.synopsis);
        if !syn.is_empty() {
            parts.push(syn);
        }
    }

    let joined = parts.join(" — ");
    sanitize_and_scrub_ids(&joined, branch, op)
}

fn sanitize_and_scrub_ids(
    raw: &str,
    branch: &BranchDescriptor,
    op: &OperationDescriptor,
) -> String {
    let sanitized = sanitize_default(raw);
    let b_id = branch.id.as_str();
    let o_id = op.id.as_str();
    let tokens: Vec<&str> = sanitized
        .split(' ')
        .filter(|tok| {
            !tok.is_empty()
                && !tok.eq_ignore_ascii_case(b_id)
                && !tok.eq_ignore_ascii_case(o_id)
                && !matches!(*tok, "T0" | "T1" | "T2")
        })
        .collect();
    if tokens.is_empty() {
        "schema operation".to_owned()
    } else {
        tokens.join(" ")
    }
}

fn humanize_field_name(field_name: &str) -> String {
    let mut out = String::with_capacity(field_name.len() + 8);
    for (idx, ch) in field_name.chars().enumerate() {
        if ch == '_' {
            out.push(' ');
        } else if ch.is_ascii_uppercase() && idx > 0 {
            out.push(' ');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    sanitize_default(&out)
}

/// Synthesizes a deterministic, snapshot-validated GAP-QL skeleton query and matching JSON Schema
/// `variablesSchema` for `op` (`RTE-031`, `GQL-012`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if the field is missing/hidden in the projected schema or
/// cannot produce a valid GAP-QL executable document.
pub fn build_candidate_skeleton(
    schema: &Valid<Schema>,
    op: &OperationDescriptor,
    visibility: &dyn Visibility,
) -> GapResult<BuiltSkeleton> {
    let projected = project_visible_schema(schema, visibility);
    let root_name = root_type_for_op_kind(op.kind);
    let field_name = op.id.name();

    let Some(root_obj) = projected.get_object(root_name) else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("root type `{root_name}` unavailable in visible schema"),
        ));
    };
    let Some(field_def) = root_obj.fields.get(field_name) else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("field `{root_name}.{field_name}` not visible or missing in schema"),
        ));
    };

    let op_keyword = match op.kind {
        OpKind::Query => "query",
        OpKind::Mutation | OpKind::Task => "mutation",
        OpKind::Subscription => "subscription",
    };

    let mut var_defs = Vec::new();
    let mut call_args = Vec::new();
    let mut schema_props = Vec::new();
    let mut required_names = Vec::new();

    for arg in &field_def.arguments {
        if arg.ty.is_non_null() && arg.default_value.is_none() {
            let arg_name = arg.name.as_str();
            let gql_ty = arg.ty.to_string();
            var_defs.push(format!("${arg_name}: {gql_ty}"));
            call_args.push(format!("{arg_name}: ${arg_name}"));
            required_names.push(format!("\"{}\"", escape_json_str(arg_name)));
            let prop_schema = graphql_type_to_json_schema(&projected, &gql_ty);
            schema_props.push(format!("\"{}\":{prop_schema}", escape_json_str(arg_name)));
        }
    }

    let var_header = if var_defs.is_empty() {
        String::new()
    } else {
        format!("({})", var_defs.join(", "))
    };
    let args_call = if call_args.is_empty() {
        String::new()
    } else {
        format!("({})", call_args.join(", "))
    };

    let return_inner = field_def.ty.inner_named_type().as_str();
    let selection = build_selection_set(&projected, return_inner)?;
    let skeleton = if selection.is_empty() {
        format!("{op_keyword}{var_header} {{ {field_name}{args_call} }}")
    } else {
        format!("{op_keyword}{var_header} {{ {field_name}{args_call} {selection} }}")
    };

    validate_skeleton_against_projected(&projected, &skeleton)?;

    let variables_schema_json = format!(
        "{{\"type\":\"object\",\"properties\":{{{}}},\"required\":[{}],\"additionalProperties\":false}}",
        schema_props.join(","),
        required_names.join(","),
    );

    Ok(BuiltSkeleton {
        skeleton,
        variables_schema_json,
        field_sdl: field_def.serialize().to_string(),
    })
}

fn validate_skeleton_against_projected(projected: &Valid<Schema>, skeleton: &str) -> GapResult<()> {
    let ast_doc = parse_executable_ast(skeleton, "skeleton.graphql").map_err(|mut errs| {
        errs.pop()
            .unwrap_or_else(|| GapError::new(ErrorCode::ValidationError, "skeleton parse failed"))
    })?;
    validate_executable_reserved_names(&ast_doc).map_err(|mut errs| {
        errs.pop().unwrap_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "skeleton reserved name check")
        })
    })?;
    validate_unique_input_object_fields(&ast_doc).map_err(|mut errs| {
        errs.pop().unwrap_or_else(|| {
            GapError::new(ErrorCode::ValidationError, "skeleton input field check")
        })
    })?;
    ast_doc.to_executable_validate(projected).map_err(|errs| {
        GapError::new(
            ErrorCode::ValidationError,
            format!(
                "generated skeleton failed schema validation: {}",
                errs.errors
            ),
        )
    })?;
    Ok(())
}

fn build_selection_set(projected: &Schema, return_type_name: &str) -> GapResult<String> {
    if projected.get_scalar(return_type_name).is_some()
        || projected.get_enum(return_type_name).is_some()
        || matches!(
            return_type_name,
            "Int" | "Float" | "String" | "Boolean" | "ID"
        )
    {
        return Ok(String::new());
    }

    let mut chosen: Vec<String> = Vec::new();
    let mut chosen_set: BTreeSet<String> = BTreeSet::new();

    if let Some(obj) = projected.get_object(return_type_name) {
        let is_leaf_no_req = |inner_name: &str, args_empty: bool| -> bool {
            args_empty
                && (projected.get_scalar(inner_name).is_some()
                    || projected.get_enum(inner_name).is_some()
                    || matches!(inner_name, "Int" | "Float" | "String" | "Boolean" | "ID"))
        };

        // 1. Collect declared `@key(fields: "...")` leaf fields first (`RTE-031`).
        for dir in obj.directives.get_all("key") {
            for arg in &dir.arguments {
                if arg.name.as_str() == "fields"
                    && let Some(raw_fields) = arg.value.as_str()
                {
                    for key_field in raw_fields.split_whitespace() {
                        if let Some(fdef) = obj.fields.get(key_field) {
                            let no_req = !fdef
                                .arguments
                                .iter()
                                .any(|a| a.ty.is_non_null() && a.default_value.is_none());
                            if is_leaf_no_req(fdef.ty.inner_named_type().as_str(), no_req)
                                && chosen_set.insert(key_field.to_owned())
                            {
                                chosen.push(key_field.to_owned());
                            }
                        }
                    }
                }
            }
        }

        // 2. Next add up to 3 fields flagged with `@primary` (else first 3 leaf scalar/enum fields).
        let mut extra_count = 0usize;
        for (fname, fdef) in &obj.fields {
            if extra_count >= 3 {
                break;
            }
            let fstr = fname.as_str();
            if fstr.starts_with("__") || chosen_set.contains(fstr) {
                continue;
            }
            let no_req = !fdef
                .arguments
                .iter()
                .any(|a| a.ty.is_non_null() && a.default_value.is_none());
            if fdef.directives.has("primary")
                && is_leaf_no_req(fdef.ty.inner_named_type().as_str(), no_req)
            {
                chosen_set.insert(fstr.to_owned());
                chosen.push(fstr.to_owned());
                extra_count += 1;
            }
        }
        if extra_count == 0 {
            for (fname, fdef) in &obj.fields {
                if extra_count >= 3 {
                    break;
                }
                let fstr = fname.as_str();
                if fstr.starts_with("__") || chosen_set.contains(fstr) {
                    continue;
                }
                let no_req = !fdef
                    .arguments
                    .iter()
                    .any(|a| a.ty.is_non_null() && a.default_value.is_none());
                if is_leaf_no_req(fdef.ty.inner_named_type().as_str(), no_req) {
                    chosen_set.insert(fstr.to_owned());
                    chosen.push(fstr.to_owned());
                    extra_count += 1;
                }
            }
        }
    }

    if !chosen.is_empty() {
        return Ok(format!("{{ {} }}", chosen.join(" ")));
    }

    minimal_selection_template(projected, return_type_name).ok_or_else(|| {
        GapError::new(
            ErrorCode::ValidationError,
            format!("unable to synthesize valid selection set for `{return_type_name}`"),
        )
    })
}

fn graphql_type_to_json_schema(schema: &Schema, gql_ty: &str) -> String {
    let trimmed = gql_ty.trim().trim_end_matches('!');
    if let Some(inner_list) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let item_schema = graphql_type_to_json_schema(schema, inner_list);
        return format!("{{\"type\":\"array\",\"items\":{item_schema}}}");
    }
    match trimmed {
        "Int" => "{\"type\":\"integer\"}".to_owned(),
        "Float" => "{\"type\":\"number\"}".to_owned(),
        "Boolean" => "{\"type\":\"boolean\"}".to_owned(),
        "ID" | "String" | "DateTime" | "Cursor" => "{\"type\":\"string\"}".to_owned(),
        "JSON" => "{}".to_owned(),
        other => {
            if let Some(enum_def) = schema.get_enum(other) {
                let vals: Vec<String> = enum_def
                    .values
                    .keys()
                    .map(|k| format!("\"{}\"", escape_json_str(k.as_str())))
                    .collect();
                format!("{{\"type\":\"string\",\"enum\":[{}]}}", vals.join(","))
            } else if let Some(input_def) = schema.get_input_object(other) {
                let mut props = Vec::new();
                let mut reqs = Vec::new();
                for (fname, fdef) in &input_def.fields {
                    let fstr = fname.as_str();
                    let sub_schema = graphql_type_to_json_schema(schema, &fdef.ty.to_string());
                    props.push(format!("\"{}\":{sub_schema}", escape_json_str(fstr)));
                    if fdef.ty.is_non_null() && fdef.default_value.is_none() {
                        reqs.push(format!("\"{}\"", escape_json_str(fstr)));
                    }
                }
                format!(
                    "{{\"type\":\"object\",\"properties\":{{{}}},\"required\":[{}]}}",
                    props.join(","),
                    reqs.join(",")
                )
            } else {
                "{\"type\":\"string\"}".to_owned()
            }
        }
    }
}

/// Escapes `s` for RFC 8259 / JCS JSON string literal output without external crates.
#[must_use]
pub fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Canonical deterministic descending `f32` ordering (`NaN` sorted last).
#[must_use]
pub fn cmp_f32_desc(a: f32, b: f32) -> Ordering {
    b.total_cmp(&a)
}
