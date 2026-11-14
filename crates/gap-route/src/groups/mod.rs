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

/// Groups visibility-filtered, side-effect-permitted search hits by [`GroupId`], expands each
/// matched capability group across all healthy visible replicas registered in
/// `Snapshot.groups`, keeps the top `K` groups (`clamp(top_k_groups, 4, 15)`), selects each
/// group's deterministic primary replica, builds deduplicated scrubbed option text, and appends
/// `insufficient_evidence` (`RTE-030`, `RTE-032`, `RTE-033`, `RTE-042`).
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

    struct CandidateReplica {
        hit_score: f32,
        branch: Arc<BranchDescriptor>,
        op: Arc<OperationDescriptor>,
        eff_side_effect: SideEffectLevel,
    }

    type MemberPair = (Arc<BranchDescriptor>, Arc<OperationDescriptor>);
    let mut group_scores: BTreeMap<GroupId, f32> = BTreeMap::new();
    let mut per_op_hit_score: BTreeMap<(BranchUid, String), f32> = BTreeMap::new();
    let mut direct_members: BTreeMap<GroupId, Vec<MemberPair>> = BTreeMap::new();

    for hit in hits {
        if hit.kind != ElementKind::Operation && hit.kind != ElementKind::Example {
            continue;
        }
        let Some((branch, op)) = resolve_hit_operation(snapshot, hit) else {
            continue;
        };
        let clean_score = if hit.fused_score.is_finite() {
            hit.fused_score.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let gid = op.group.clone();
        group_scores
            .entry(gid.clone())
            .and_modify(|prev| *prev = prev.max(clean_score))
            .or_insert(clean_score);
        per_op_hit_score
            .entry((branch.uid, op.id.as_str().to_owned()))
            .and_modify(|prev| *prev = prev.max(clean_score))
            .or_insert(clean_score);
        direct_members.entry(gid).or_default().push((branch, op));
    }

    struct BuiltGroupRow {
        group_id: GroupId,
        best_score: f32,
        primary: CandidateReplica,
        member_count: usize,
        option_text: String,
    }

    let mut ranked_groups: Vec<BuiltGroupRow> = Vec::with_capacity(group_scores.len());

    for (group_id, best_score) in group_scores {
        let mut raw_pairs: Vec<MemberPair> = direct_members.remove(&group_id).unwrap_or_default();

        // Also expand all replicas registered in `snapshot.groups` so if an indexed representative
        // branch is disabled/filtered out, other healthy visible replicas still serve the group.
        if let Some(cap_group) = snapshot.groups.get(&group_id) {
            for (member_uid, member_op_id) in &cap_group.members {
                if let Some(branch) = snapshot.branches.get(member_uid)
                    && let Some(op) = branch.operations.iter().find(|o| &o.id == member_op_id)
                {
                    raw_pairs.push((Arc::clone(branch), Arc::clone(op)));
                }
            }
        }

        let mut seen_keys: BTreeSet<(BranchUid, String)> = BTreeSet::new();
        let mut eligible: Vec<CandidateReplica> = Vec::new();
        for (branch, op) in raw_pairs {
            let key = (branch.uid, op.id.as_str().to_owned());
            if !seen_keys.insert(key.clone()) {
                continue;
            }
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
            let hit_score = per_op_hit_score.get(&key).copied().unwrap_or(best_score);
            eligible.push(CandidateReplica {
                hit_score,
                branch,
                op,
                eff_side_effect: eff,
            });
        }

        if eligible.is_empty() {
            continue;
        }

        eligible.sort_by(|a, b| {
            cmp_f32_desc(a.hit_score, b.hit_score)
                .then_with(|| a.op.id.as_str().cmp(b.op.id.as_str()))
                .then_with(|| a.branch.uid.cmp(&b.branch.uid))
        });

        let member_refs: Vec<(&BranchDescriptor, &OperationDescriptor)> = eligible
            .iter()
            .map(|c| (c.branch.as_ref(), c.op.as_ref()))
            .collect();
        let option_text = build_group_option_text_union(&member_refs);
        let member_count = eligible.len();
        if let Some(primary) = eligible.into_iter().next() {
            ranked_groups.push(BuiltGroupRow {
                group_id,
                best_score,
                primary,
                member_count,
                option_text,
            });
        }
    }

    // Sort groups by descending retrieval score, tie-breaking deterministically by GroupId (`RTE-033`).
    ranked_groups.sort_by(|a, b| {
        cmp_f32_desc(a.best_score, b.best_score).then_with(|| a.group_id.cmp(&b.group_id))
    });
    ranked_groups.truncate(clamped_k);

    let mut resolved = Vec::with_capacity(ranked_groups.len());
    let mut options = Vec::with_capacity(ranked_groups.len() + 1);

    for (slot_idx, row) in ranked_groups.into_iter().enumerate() {
        let key = format!("g{slot_idx}");
        options.push(GroupOption {
            key: key.clone(),
            text: row.option_text.clone(),
            prior: row.best_score,
        });
        resolved.push(ResolvedGroup {
            group_id: row.group_id,
            option_key: key,
            option_text: row.option_text,
            prior: row.best_score,
            primary_branch: row.primary.branch.id.clone(),
            primary_branch_uid: row.primary.branch.uid,
            primary_trust: row.primary.branch.trust,
            effective_side_effect: row.primary.eff_side_effect,
            primary_op: row.primary.op,
            skeleton: None,
            member_count: row.member_count,
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
    build_group_option_text_union(&[(branch, op)])
}

/// Builds deduplicated, sanitized option text across all eligible members of a capability group,
/// omitting every member's branch ID, operation ID, and trust tier label (`RTE-030`, `RTE-042`).
#[must_use]
pub fn build_group_option_text_union(
    members: &[(&BranchDescriptor, &OperationDescriptor)],
) -> String {
    if members.is_empty() {
        return "schema operation".to_owned();
    }

    let mut seen_parts: BTreeSet<String> = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();
    let mut branch_ids: Vec<&str> = Vec::with_capacity(members.len());
    let mut op_ids: Vec<&str> = Vec::with_capacity(members.len());

    for (branch, op) in members {
        branch_ids.push(branch.id.as_str());
        op_ids.push(op.id.as_str());

        let op_phrase = humanize_field_name(op.id.name());
        if !op_phrase.is_empty() && seen_parts.insert(op_phrase.clone()) {
            parts.push(op_phrase);
        }

        let syn = sanitize_default(&branch.synopsis);
        if !syn.is_empty() && seen_parts.insert(syn.clone()) {
            parts.push(syn);
        }

        for ex in op.examples.iter().filter(|e| e.positive).take(3) {
            let clean = sanitize_default(&ex.intent);
            if !clean.is_empty() && seen_parts.insert(clean.clone()) {
                parts.push(clean);
            }
        }
    }

    let joined = parts.join(" — ");
    sanitize_and_scrub_member_ids(&joined, &branch_ids, &op_ids)
}

fn sanitize_and_scrub_member_ids(raw: &str, branch_ids: &[&str], op_ids: &[&str]) -> String {
    let sanitized = sanitize_default(raw);
    let tokens: Vec<&str> = sanitized
        .split(' ')
        .filter(|tok| {
            if tok.is_empty() {
                return false;
            }
            let core = tok.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.'
            });
            if core.is_empty() {
                return true;
            }
            if matches!(core, "T0" | "T1" | "T2") {
                return false;
            }
            let matches_branch = branch_ids.iter().any(|b| core.eq_ignore_ascii_case(b));
            let matches_op = op_ids.iter().any(|o| core.eq_ignore_ascii_case(o));
            !(matches_branch || matches_op)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{DiscoveryConfig, ResolveConstraints, RouteDiscoveryService};
    use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
    use gap_core::ids::{OperationId, TenantId};
    use gap_core::snapshot::{
        BranchLimits, Capabilities, CapabilityGroup, CostHint, Endpoint, Example, Protocol,
        SideEffect,
    };
    use gap_core::traits::DiscoveryOutcome;
    use gap_index::{ElementDoc, HybridElementIndex, HybridIndexConfig};
    use gap_ql::{AllowAllVisibility, with_gap_builtins};

    fn poll_ready<F: Future>(fut: F) -> F::Output {
        let mut boxed: Pin<Box<F>> = Box::pin(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match boxed.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("expected ready future in group tests"),
        }
    }

    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn next_usize(&mut self, bound: usize) -> usize {
            (self.next_u64() as usize) % bound
        }
    }

    fn mk_group_id(label: &str) -> GroupId {
        let mut bytes = [0u8; 16];
        for (i, b) in label.bytes().take(16).enumerate() {
            bytes[i] = b;
        }
        GroupId::from_bytes(bytes)
    }

    fn mk_op(
        branch: &BranchId,
        name: &str,
        gid: GroupId,
        sdl: &str,
        intents: &[&str],
    ) -> Arc<OperationDescriptor> {
        let id = OperationId::from_parts(branch, name).unwrap();
        Arc::new(OperationDescriptor {
            id,
            group: gid,
            kind: OpKind::Query,
            side_effect: SideEffect {
                level: SideEffectLevel::None,
                verified: true,
                requires_approval: false,
            },
            idempotent: true,
            cost: CostHint::default(),
            desc_hash: [3u8; 32],
            sdl: Arc::from(sdl),
            examples: intents
                .iter()
                .map(|s| Example {
                    intent: (*s).to_owned(),
                    positive: true,
                    query_snippet: None,
                })
                .collect(),
        })
    }

    fn mk_branch(
        slug: &str,
        status: BranchStatus,
        synopsis: &str,
        ops: Vec<Arc<OperationDescriptor>>,
    ) -> Arc<BranchDescriptor> {
        Arc::new(BranchDescriptor {
            uid: BranchUid::new_v7(),
            id: BranchId::new(slug).unwrap(),
            protocol: Protocol::Rest,
            trust: TrustTier::T0,
            endpoint: Endpoint {
                uri: format!("https://{slug}.example.internal"),
                transport: "http".to_owned(),
            },
            spec_hash: [4u8; 32],
            pinned_desc_hash: [4u8; 32],
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: synopsis.to_owned(),
            operations: ops,
            status,
        })
    }

    fn mk_ctx(snap: Arc<Snapshot>) -> RequestContext {
        let tenant = TenantId::default_tenant();
        RequestContext::try_new(
            BranchUid::new_v7().as_uuid(),
            tenant.clone(),
            Principal::new("agent-group-test", PrincipalKind::Agent, tenant),
            Instant::now() + Duration::from_secs(30),
            Budget::new(10_000),
            TraceCtx::generate_root(false),
            0,
            None,
            snap,
        )
        .unwrap()
    }

    // spec: RTE-030, RTE-033
    #[test]
    fn group_text_union_scrubs_all_member_branch_and_operation_ids() {
        let b1 = BranchId::new("crm-us").unwrap();
        let b2 = BranchId::new("crm-eu").unwrap();
        let gid = mk_group_id("grp-crm");

        let op1 = mk_op(
            &b1,
            "getOrder",
            gid.clone(),
            "getOrder(id: ID!): String",
            &["Fetch crm-us T0 order via crm-us.getOrder"],
        );
        let op2 = mk_op(
            &b2,
            "lookupOrder",
            gid,
            "lookupOrder(id: ID!): String",
            &["Retrieve (crm-eu) T1 billing ledger via crm-eu.lookupOrder"],
        );

        let d1 = mk_branch(
            "crm-us",
            BranchStatus::Active,
            "crm-us service",
            vec![Arc::clone(&op1)],
        );
        let d2 = mk_branch(
            "crm-eu",
            BranchStatus::Active,
            "crm-eu mirror T2",
            vec![Arc::clone(&op2)],
        );

        let joined = build_group_option_text_union(&[
            (d1.as_ref(), op1.as_ref()),
            (d2.as_ref(), op2.as_ref()),
        ]);

        assert!(joined.contains("billing ledger"));
        assert!(!joined.contains("crm-us"));
        assert!(!joined.contains("crm-eu"));
        assert!(!joined.contains("crm-us.getOrder"));
        assert!(!joined.contains("crm-eu.lookupOrder"));
        assert!(
            !joined
                .split(' ')
                .any(|tok| matches!(tok, "T0" | "T1" | "T2"))
        );
    }

    // spec: RTE-030, RTE-033
    #[test]
    fn group_failover_selects_healthy_replica_when_primary_disabled() {
        let sdl = r#"
            type Query {
                crm_a_getOrder(id: ID!): String
                crm_b_getOrder(id: ID!): String
            }
        "#;
        let schema = with_gap_builtins(sdl).unwrap();
        let tenant = TenantId::default_tenant();
        let gid = mk_group_id("grp-failover");

        let b_prim = BranchId::new("crm-a").unwrap();
        let b_sec = BranchId::new("crm-b").unwrap();

        let op_prim = mk_op(
            &b_prim,
            "crm_a_getOrder",
            gid.clone(),
            "crm_a_getOrder(id: ID!): String",
            &["fetch order invoice"],
        );
        let op_sec = mk_op(
            &b_sec,
            "crm_b_getOrder",
            gid.clone(),
            "crm_b_getOrder(id: ID!): String",
            &["fetch order invoice", "lookup secondary replica"],
        );

        // Primary replica is Disabled, secondary is Active.
        let br_prim = mk_branch(
            "crm-a",
            BranchStatus::Disabled,
            "Primary disabled",
            vec![Arc::clone(&op_prim)],
        );
        let br_sec = mk_branch(
            "crm-b",
            BranchStatus::Active,
            "Secondary healthy",
            vec![Arc::clone(&op_sec)],
        );

        let doc = ElementDoc::new(gid.as_str(), ElementKind::Operation, "crm_a_getOrder")
            .with_tenant(tenant)
            .with_branch_uid(br_prim.uid)
            .with_operation_id(op_prim.id.clone())
            .with_path("Query.crm_a_getOrder")
            .with_text("fetch order invoice billing record")
            .with_examples(vec!["fetch order invoice".to_owned()]);
        let index = HybridElementIndex::build(vec![doc], HybridIndexConfig::default()).unwrap();

        let mut snap = Snapshot::empty(3);
        snap.schema = Arc::new(schema.into_inner());
        snap.index = Arc::new(index);
        snap.branches.insert(br_prim.uid, Arc::clone(&br_prim));
        snap.branches.insert(br_sec.uid, Arc::clone(&br_sec));
        snap.groups.insert(
            gid.clone(),
            Arc::new(CapabilityGroup {
                id: gid.clone(),
                signature_sdl: Arc::from("getOrder(id: ID!): String"),
                members: vec![
                    (br_prim.uid, op_prim.id.clone()),
                    (br_sec.uid, op_sec.id.clone()),
                ],
            }),
        );

        let hits = vec![SearchHit {
            element_id: gid.as_str().to_owned(),
            kind: ElementKind::Operation,
            branch_uid: Some(br_prim.uid),
            operation_id: Some(op_prim.id.clone()),
            lexical_score: 0.92,
            dense_score: None,
            fused_score: 0.92,
        }];
        let vis = AllowAllVisibility::default();
        let (groups, _) = build_group_options(
            &snap,
            &hits,
            None,
            SideEffectLevel::None,
            15,
            false,
            &vis,
            0.0,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].primary_branch, b_sec);
        assert_eq!(groups[0].member_count, 1);

        let ctx = mk_ctx(Arc::new(snap));
        let svc = RouteDiscoveryService::baseline(DiscoveryConfig::default());
        let res = poll_ready(svc.resolve_detailed(
            &ctx,
            ResolveConstraints::default(),
            "fetch order invoice",
        ))
        .unwrap();
        assert_ne!(res.outcome, DiscoveryOutcome::Abstained);
        assert_eq!(res.candidates.len(), 1);
        assert_eq!(res.candidates[0].branch, b_sec);
        assert!(res.candidates[0].skeleton.contains("crm_b_getOrder"));
    }

    // spec: RTE-030, RTE-033
    #[test]
    fn group_unverified_duplicates_remain_distinct_candidates_sorted_by_group_id() {
        let b1 = BranchId::new("inv-a").unwrap();
        let b2 = BranchId::new("inv-b").unwrap();
        let b3 = BranchId::new("inv-c").unwrap();

        // Construct ordered GroupIds with equal score to verify deterministic GroupId asc tie-breaking.
        let g1 = GroupId::from_bytes([0x01u8; 16]);
        let g2 = GroupId::from_bytes([0x02u8; 16]);
        let g3 = GroupId::from_bytes([0x03u8; 16]);

        let o1 = mk_op(&b1, "getBill", g1.clone(), "getBill: String", &["get bill"]);
        let o2 = mk_op(&b2, "getBill", g2.clone(), "getBill: String", &["get bill"]);
        let o3 = mk_op(&b3, "getBill", g3.clone(), "getBill: String", &["get bill"]);

        let d1 = mk_branch("inv-a", BranchStatus::Active, "A", vec![Arc::clone(&o1)]);
        let d2 = mk_branch("inv-b", BranchStatus::Active, "B", vec![Arc::clone(&o2)]);
        let d3 = mk_branch("inv-c", BranchStatus::Active, "C", vec![Arc::clone(&o3)]);

        let mut snap = Snapshot::empty(4);
        snap.branches.insert(d1.uid, Arc::clone(&d1));
        snap.branches.insert(d2.uid, Arc::clone(&d2));
        snap.branches.insert(d3.uid, Arc::clone(&d3));

        // Feed hits in reverse order (`g3`, `g1`, `g2`) with identical scores.
        let hits = vec![
            SearchHit {
                element_id: o3.id.as_str().to_owned(),
                kind: ElementKind::Operation,
                branch_uid: Some(d3.uid),
                operation_id: Some(o3.id.clone()),
                lexical_score: 0.85,
                dense_score: None,
                fused_score: 0.85,
            },
            SearchHit {
                element_id: o1.id.as_str().to_owned(),
                kind: ElementKind::Operation,
                branch_uid: Some(d1.uid),
                operation_id: Some(o1.id.clone()),
                lexical_score: 0.85,
                dense_score: None,
                fused_score: 0.85,
            },
            SearchHit {
                element_id: o2.id.as_str().to_owned(),
                kind: ElementKind::Operation,
                branch_uid: Some(d2.uid),
                operation_id: Some(o2.id.clone()),
                lexical_score: 0.85,
                dense_score: None,
                fused_score: 0.85,
            },
        ];

        let (resolved, _) = build_group_options(
            &snap,
            &hits,
            None,
            SideEffectLevel::None,
            15,
            false,
            &AllowAllVisibility::default(),
            0.0,
        );
        let ordered_gids: Vec<GroupId> = resolved.into_iter().map(|g| g.group_id).collect();
        assert_eq!(ordered_gids, vec![g1, g2, g3]);
    }

    // spec: RTE-030, RTE-033
    #[test]
    fn group_replica_addition_invariance_never_lowers_score_or_triggers_abstention() {
        let mut sdl = String::from("type Query {\n");
        for i in 0..6 {
            sdl.push_str(&format!("  crm_{i}_getOrder(id: ID!): String\n"));
        }
        sdl.push_str(
            "  billing_getInvoice(id: ID!): String\n  shipping_trackParcel(id: ID!): String\n}\n",
        );
        let schema = Arc::new(with_gap_builtins(&sdl).unwrap().into_inner());
        let tenant = TenantId::default_tenant();

        let target_gid = mk_group_id("grp-crm-order");
        let billing_gid = mk_group_id("grp-billing-inv");
        let ship_gid = mk_group_id("grp-ship-trk");

        let b_bill = BranchId::new("billing").unwrap();
        let b_ship = BranchId::new("shipping").unwrap();
        let op_bill = mk_op(
            &b_bill,
            "billing_getInvoice",
            billing_gid.clone(),
            "billing_getInvoice(id: ID!): String",
            &["get billing invoice statement"],
        );
        let op_ship = mk_op(
            &b_ship,
            "shipping_trackParcel",
            ship_gid.clone(),
            "shipping_trackParcel(id: ID!): String",
            &["track shipping parcel delivery"],
        );
        let d_bill = mk_branch(
            "billing",
            BranchStatus::Active,
            "Billing service",
            vec![Arc::clone(&op_bill)],
        );
        let d_ship = mk_branch(
            "shipping",
            BranchStatus::Active,
            "Shipping service",
            vec![Arc::clone(&op_ship)],
        );

        let queries = [
            "fetch customer order status",
            "lookup customer order details",
            "retrieve order record by identifier",
            "check customer order status",
        ];

        let mut rng = SplitMix64(0x4752_4F55_5053_0208);
        let svc = RouteDiscoveryService::baseline(DiscoveryConfig::default());

        for trial in 0..220usize {
            let replica_count = 1 + rng.next_usize(6); // 1..=6 replicas
            let add_synonym = (trial % 2) == 1;

            let mut target_branches = Vec::with_capacity(replica_count);
            let mut target_members = Vec::with_capacity(replica_count);
            let mut text_union = String::from("fetch customer order lookup retrieve record");

            for r in 0..replica_count {
                let slug = format!("crm-{r}");
                let bid = BranchId::new(&slug).unwrap();
                let op_name = format!("crm_{r}_getOrder");
                let mut exs = vec!["fetch customer order status"];
                if add_synonym && r > 0 {
                    exs.push("lookup customer order details");
                    text_union.push_str(" lookup customer order details");
                }
                let op = mk_op(
                    &bid,
                    &op_name,
                    target_gid.clone(),
                    &format!("{op_name}(id: ID!): String"),
                    &exs,
                );
                let br = mk_branch(
                    &slug,
                    BranchStatus::Active,
                    "Customer CRM orders",
                    vec![Arc::clone(&op)],
                );
                target_members.push((br.uid, op.id.clone()));
                target_branches.push((br, op));
            }

            let build_snapshot = |count: usize| -> Snapshot {
                let active_members = &target_members[..count];
                let mut union_examples = vec!["fetch customer order status".to_owned()];
                if count > 1 && add_synonym {
                    union_examples.push("lookup customer order details".to_owned());
                }
                let target_doc = ElementDoc::new(
                    target_gid.as_str(),
                    ElementKind::Operation,
                    target_branches[0].1.id.name(),
                )
                .with_tenant(tenant.clone())
                .with_branch_uid(target_branches[0].0.uid)
                .with_operation_id(target_branches[0].1.id.clone())
                .with_path("Query.crm_0_getOrder")
                .with_text(text_union.clone())
                .with_examples(union_examples);
                let bill_doc = ElementDoc::new(
                    billing_gid.as_str(),
                    ElementKind::Operation,
                    "billing_getInvoice",
                )
                .with_tenant(tenant.clone())
                .with_branch_uid(d_bill.uid)
                .with_operation_id(op_bill.id.clone())
                .with_path("Query.billing_getInvoice")
                .with_text("get billing invoice statement")
                .with_examples(vec!["get billing invoice".to_owned()]);
                let ship_doc = ElementDoc::new(
                    ship_gid.as_str(),
                    ElementKind::Operation,
                    "shipping_trackParcel",
                )
                .with_tenant(tenant.clone())
                .with_branch_uid(d_ship.uid)
                .with_operation_id(op_ship.id.clone())
                .with_path("Query.shipping_trackParcel")
                .with_text("track shipping parcel delivery")
                .with_examples(vec!["track shipping parcel".to_owned()]);

                let index = HybridElementIndex::build(
                    vec![target_doc, bill_doc, ship_doc],
                    HybridIndexConfig::default(),
                )
                .unwrap();
                let mut snap = Snapshot::empty((trial + count) as u64);
                snap.schema = Arc::clone(&schema);
                snap.index = Arc::new(index);
                snap.branches.insert(d_bill.uid, Arc::clone(&d_bill));
                snap.branches.insert(d_ship.uid, Arc::clone(&d_ship));
                for (br, _) in &target_branches[..count] {
                    snap.branches.insert(br.uid, Arc::clone(br));
                }
                snap.groups.insert(
                    target_gid.clone(),
                    Arc::new(CapabilityGroup {
                        id: target_gid.clone(),
                        signature_sdl: Arc::from("getOrder(id: ID!): String"),
                        members: active_members.to_vec(),
                    }),
                );
                snap.groups.insert(
                    billing_gid.clone(),
                    Arc::new(CapabilityGroup {
                        id: billing_gid.clone(),
                        signature_sdl: Arc::from("getInvoice(id: ID!): String"),
                        members: vec![(d_bill.uid, op_bill.id.clone())],
                    }),
                );
                snap.groups.insert(
                    ship_gid.clone(),
                    Arc::new(CapabilityGroup {
                        id: ship_gid.clone(),
                        signature_sdl: Arc::from("trackParcel(id: ID!): String"),
                        members: vec![(d_ship.uid, op_ship.id.clone())],
                    }),
                );
                snap
            };

            let q = queries[rng.next_usize(queries.len())];
            let snap_1 = Arc::new(build_snapshot(1));
            let snap_n = Arc::new(build_snapshot(replica_count));

            let res_1 =
                poll_ready(svc.resolve_detailed(&mk_ctx(snap_1), ResolveConstraints::default(), q))
                    .unwrap();
            let res_n =
                poll_ready(svc.resolve_detailed(&mk_ctx(snap_n), ResolveConstraints::default(), q))
                    .unwrap();

            assert_ne!(
                res_1.outcome,
                DiscoveryOutcome::Abstained,
                "single-replica query `{q}` unexpectedly abstained on trial {trial}"
            );
            assert_ne!(
                res_n.outcome,
                DiscoveryOutcome::Abstained,
                "adding {replica_count} healthy replicas must never trigger abstention on trial {trial}"
            );

            let c1 = res_1
                .candidates
                .iter()
                .find(|c| c.group_id == target_gid)
                .expect("target group in 1-replica");
            let cn = res_n
                .candidates
                .iter()
                .find(|c| c.group_id == target_gid)
                .expect("target group in N-replica");
            assert!(
                cn.score + 1e-6 >= c1.score,
                "adding healthy replicas lowered group score ({} -> {}) on trial {trial}",
                c1.score,
                cn.score
            );
        }
    }
}
