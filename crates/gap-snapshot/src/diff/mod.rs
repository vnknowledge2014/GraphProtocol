//! Schema and branch-set diffing across snapshot generations (`WP-M4-09`, `ING-052`, `SEC-082`,
//! `RUN-009`).

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use gap_core::ids::{BranchId, BranchUid, GroupId, OperationId};
use gap_core::joingraph::{JoinEdge, JoinStatus};
use gap_core::snapshot::{
    BranchDescriptor, BranchStatus, FieldCoordinate, OperationDescriptor, SideEffectLevel,
    Snapshot, TrustTier,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::canonical::{CanonicalError, canonical_json};

/// Lifecycle diff classification for a branch across two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BranchChangeKind {
    /// Branch is present in `after` and absent in `before`.
    Added,
    /// Branch was present in `before` and removed from `after`.
    Removed,
    /// Branch exists in both snapshots with changed status, trust, or hashes.
    Modified,
}

impl BranchChangeKind {
    /// Canonical string label for CLI and JSON diff output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "ADDED",
            Self::Removed => "REMOVED",
            Self::Modified => "MODIFIED",
        }
    }
}

/// Diff record describing a single changed [`BranchDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDiffEntry {
    /// Stable branch UID.
    pub uid: BranchUid,
    /// Branch slug.
    pub id: BranchId,
    /// Added, removed, or modified kind.
    pub change: BranchChangeKind,
    /// Status transition if modified (`(before, after)`).
    pub status_change: Option<(BranchStatus, BranchStatus)>,
    /// Trust tier transition if modified (`(before, after)`).
    pub trust_change: Option<(TrustTier, TrustTier)>,
    /// Whether `spec_hash` changed.
    pub spec_hash_changed: bool,
    /// Whether `pinned_desc_hash` changed (`SEC-082`).
    pub pinned_desc_hash_changed: bool,
}

/// Kind of change affecting a single operation across two snapshot generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperationChangeKind {
    /// Operation newly introduced.
    Added,
    /// Operation removed from branch (breaking).
    Removed,
    /// Rendered GraphQL SDL signature changed (breaking).
    SignatureChanged,
    /// Pinned operation `desc_hash` changed (`SEC-082`, `INV-08`).
    DescriptionChanged,
    /// Verified side-effect classification changed.
    SideEffectChanged,
    /// Capability group assignment changed (`ADR-006`).
    GroupChanged,
}

impl OperationChangeKind {
    /// Wire / display label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "ADDED",
            Self::Removed => "REMOVED",
            Self::SignatureChanged => "SIGNATURE_CHANGED",
            Self::DescriptionChanged => "DESCRIPTION_CHANGED",
            Self::SideEffectChanged => "SIDE_EFFECT_CHANGED",
            Self::GroupChanged => "GROUP_CHANGED",
        }
    }
}

/// Diff record describing a single changed [`OperationDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDiffEntry {
    /// Qualified `<branch>.<op>` identifier.
    pub operation_id: OperationId,
    /// Owning branch slug.
    pub branch_id: BranchId,
    /// Sorted list of detected changes on this operation.
    pub changes: Vec<OperationChangeKind>,
    /// `true` iff any change breaks existing callers (removed, signature change, or side-effect escalation).
    pub breaking: bool,
    /// Previous operation SDL signature if present in `before`.
    pub before_sdl: Option<String>,
    /// New operation SDL signature if present in `after`.
    pub after_sdl: Option<String>,
    /// Previous verified side-effect level if present in `before`.
    pub before_side_effect: Option<SideEffectLevel>,
    /// New verified side-effect level if present in `after`.
    pub after_side_effect: Option<SideEffectLevel>,
}

/// Diff record describing changes to a [`CapabilityGroup`](gap_core::snapshot::CapabilityGroup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDiffEntry {
    /// Stable capability group identifier.
    pub group_id: GroupId,
    /// Operations newly added to the group in `after`.
    pub added_members: Vec<OperationId>,
    /// Operations removed from the group relative to `before`.
    pub removed_members: Vec<OperationId>,
    /// Whether the canonical group signature SDL changed.
    pub signature_changed: bool,
}

/// Diff record for a [`FieldCoordinate`] owner change inside `Snapshot.ownership` (`RUN-011`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipDiffEntry {
    /// Changed `Type.field` coordinate.
    pub coordinate: FieldCoordinate,
    /// Previous owning branch UID (if previously owned).
    pub before_owner: Option<BranchUid>,
    /// New owning branch UID (if still owned).
    pub after_owner: Option<BranchUid>,
}

/// Diff record for a cross-branch join edge inside `Snapshot.joins` (`INV-06`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinDiffEntry {
    /// Stable join edge ID.
    pub edge_id: String,
    /// Source object type.
    pub source_type: String,
    /// Synthesized field name.
    pub field_name: String,
    /// Target RHS operation ID.
    pub target_operation: OperationId,
    /// Previous join lifecycle status.
    pub before_status: Option<JoinStatus>,
    /// New join lifecycle status.
    pub after_status: Option<JoinStatus>,
}

/// Deterministic structural diff between two immutable [`Snapshot`] generations (`gap snapshot diff`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotDiff {
    /// Baseline generation (`before.generation`).
    pub from_generation: u64,
    /// Target generation (`after.generation`).
    pub to_generation: u64,
    /// Whether `Snapshot.branch_set_hash` differs between `before` and `after`.
    pub branch_set_hash_changed: bool,
    /// Aggregate breaking flag across branch removals, op signatures, side effects, ownership, and joins.
    pub has_breaking_changes: bool,
    /// Per-branch diff entries sorted by `(id, uid)`.
    pub branches: Vec<BranchDiffEntry>,
    /// Per-operation diff entries sorted by `operation_id`.
    pub operations: Vec<OperationDiffEntry>,
    /// Capability group diffs sorted by `group_id`.
    pub groups: Vec<GroupDiffEntry>,
    /// Field ownership changes sorted by `(type_name, field)`.
    pub ownership: Vec<OwnershipDiffEntry>,
    /// Join graph edge transitions sorted by `edge_id`.
    pub joins: Vec<JoinDiffEntry>,
}

impl SnapshotDiff {
    /// Returns `true` iff no branch, operation, group, ownership, join, or branch-set hash changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.branch_set_hash_changed
            && self.branches.is_empty()
            && self.operations.is_empty()
            && self.groups.is_empty()
            && self.ownership.is_empty()
            && self.joins.is_empty()
    }

    /// Formats a deterministic human-readable diff suitable for `gap snapshot diff` CLI output.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "snapshot diff g{} -> g{} (breaking={}, branches={}, ops={}, groups={}, ownership={}, joins={})",
            self.from_generation,
            self.to_generation,
            self.has_breaking_changes,
            self.branches.len(),
            self.operations.len(),
            self.groups.len(),
            self.ownership.len(),
            self.joins.len()
        );
        if self.is_empty() {
            let _ = writeln!(out, "  (no changes)");
            return out;
        }
        for b in &self.branches {
            let prefix = match b.change {
                BranchChangeKind::Added => '+',
                BranchChangeKind::Removed => '-',
                BranchChangeKind::Modified => '~',
            };
            let _ = write!(
                out,
                "  {prefix} branch {} ({}): {}",
                b.id,
                b.uid,
                b.change.as_str()
            );
            if let Some((from, to)) = b.status_change {
                let _ = write!(out, " status={from:?}->{to:?}");
            }
            if let Some((from, to)) = b.trust_change {
                let _ = write!(out, " trust={from:?}->{to:?}");
            }
            if b.spec_hash_changed {
                let _ = write!(out, " spec_hash");
            }
            if b.pinned_desc_hash_changed {
                let _ = write!(out, " desc_hash");
            }
            out.push('\n');
        }
        for op in &self.operations {
            let prefix = if op.changes.contains(&OperationChangeKind::Added) {
                '+'
            } else if op.changes.contains(&OperationChangeKind::Removed) {
                '-'
            } else {
                '~'
            };
            let tags: Vec<&'static str> = op.changes.iter().map(|c| c.as_str()).collect();
            let _ = writeln!(
                out,
                "  {prefix} op {} [{}] breaking={}",
                op.operation_id,
                tags.join(","),
                op.breaking
            );
        }
        for g in &self.groups {
            let _ = writeln!(
                out,
                "  ~ group {} (+{}, -{}, sig_changed={})",
                g.group_id,
                g.added_members.len(),
                g.removed_members.len(),
                g.signature_changed
            );
        }
        for o in &self.ownership {
            let prefix = match (o.before_owner, o.after_owner) {
                (None, Some(_)) => '+',
                (Some(_), None) => '-',
                _ => '~',
            };
            let _ = writeln!(
                out,
                "  {prefix} field {}: {:?} -> {:?}",
                o.coordinate, o.before_owner, o.after_owner
            );
        }
        for j in &self.joins {
            let prefix = match (j.before_status, j.after_status) {
                (None, Some(_)) => '+',
                (Some(_), None) => '-',
                _ => '~',
            };
            let _ = writeln!(
                out,
                "  {prefix} join {} ({}.{} -> {}): {:?} -> {:?}",
                j.edge_id,
                j.source_type,
                j.field_name,
                j.target_operation,
                j.before_status,
                j.after_status
            );
        }
        out
    }

    /// Converts the diff into a structured [`serde_json::Value`].
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let mut root = JsonMap::new();
        root.insert(
            "from_generation".to_owned(),
            JsonValue::Number(self.from_generation.into()),
        );
        root.insert(
            "to_generation".to_owned(),
            JsonValue::Number(self.to_generation.into()),
        );
        root.insert(
            "branch_set_hash_changed".to_owned(),
            JsonValue::Bool(self.branch_set_hash_changed),
        );
        root.insert(
            "has_breaking_changes".to_owned(),
            JsonValue::Bool(self.has_breaking_changes),
        );
        root.insert(
            "branches".to_owned(),
            JsonValue::Array(self.branches.iter().map(branch_entry_json).collect()),
        );
        root.insert(
            "operations".to_owned(),
            JsonValue::Array(self.operations.iter().map(op_entry_json).collect()),
        );
        root.insert(
            "groups".to_owned(),
            JsonValue::Array(self.groups.iter().map(group_entry_json).collect()),
        );
        root.insert(
            "ownership".to_owned(),
            JsonValue::Array(self.ownership.iter().map(ownership_entry_json).collect()),
        );
        root.insert(
            "joins".to_owned(),
            JsonValue::Array(self.joins.iter().map(join_entry_json).collect()),
        );
        JsonValue::Object(root)
    }

    /// Serializes this diff into RFC 8785 canonical JSON (`crate::canonical::canonical_json`).
    ///
    /// # Errors
    ///
    /// Propagates [`CanonicalError`] from [`canonical_json`] if serialization fails.
    pub fn to_canonical_json(&self) -> Result<String, CanonicalError> {
        canonical_json(&self.to_json_value())
    }
}

/// Computes a deterministic structural diff between `before` and `after` [`Snapshot`]s.
#[must_use]
pub fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> SnapshotDiff {
    let mut has_breaking = false;
    let mut branch_diffs = Vec::new();
    let mut op_diffs = Vec::new();

    let all_uids: BTreeSet<BranchUid> = before
        .branches
        .keys()
        .chain(after.branches.keys())
        .copied()
        .collect();

    for uid in all_uids {
        match (before.branches.get(&uid), after.branches.get(&uid)) {
            (None, Some(new_br)) => {
                branch_diffs.push(BranchDiffEntry {
                    uid,
                    id: new_br.id.clone(),
                    change: BranchChangeKind::Added,
                    status_change: None,
                    trust_change: None,
                    spec_hash_changed: false,
                    pinned_desc_hash_changed: false,
                });
                for op in &new_br.operations {
                    op_diffs.push(OperationDiffEntry {
                        operation_id: op.id.clone(),
                        branch_id: new_br.id.clone(),
                        changes: vec![OperationChangeKind::Added],
                        breaking: false,
                        before_sdl: None,
                        after_sdl: Some((*op.sdl).to_owned()),
                        before_side_effect: None,
                        after_side_effect: Some(op.side_effect.level),
                    });
                }
            }
            (Some(old_br), None) => {
                has_breaking = true;
                branch_diffs.push(BranchDiffEntry {
                    uid,
                    id: old_br.id.clone(),
                    change: BranchChangeKind::Removed,
                    status_change: None,
                    trust_change: None,
                    spec_hash_changed: false,
                    pinned_desc_hash_changed: false,
                });
                for op in &old_br.operations {
                    op_diffs.push(OperationDiffEntry {
                        operation_id: op.id.clone(),
                        branch_id: old_br.id.clone(),
                        changes: vec![OperationChangeKind::Removed],
                        breaking: true,
                        before_sdl: Some((*op.sdl).to_owned()),
                        after_sdl: None,
                        before_side_effect: Some(op.side_effect.level),
                        after_side_effect: None,
                    });
                }
            }
            (Some(old_br), Some(new_br)) => {
                if old_br.as_ref() != new_br.as_ref() {
                    let status_change =
                        (old_br.status != new_br.status).then_some((old_br.status, new_br.status));
                    if old_br.status == BranchStatus::Active
                        && matches!(
                            new_br.status,
                            BranchStatus::Drift
                                | BranchStatus::Quarantined
                                | BranchStatus::Disabled
                        )
                    {
                        has_breaking = true;
                    }
                    let trust_change =
                        (old_br.trust != new_br.trust).then_some((old_br.trust, new_br.trust));
                    let spec_hash_changed = old_br.spec_hash != new_br.spec_hash;
                    let pinned_desc_hash_changed =
                        old_br.pinned_desc_hash != new_br.pinned_desc_hash;

                    if status_change.is_some()
                        || trust_change.is_some()
                        || spec_hash_changed
                        || pinned_desc_hash_changed
                        || old_br.id != new_br.id
                    {
                        branch_diffs.push(BranchDiffEntry {
                            uid,
                            id: new_br.id.clone(),
                            change: BranchChangeKind::Modified,
                            status_change,
                            trust_change,
                            spec_hash_changed,
                            pinned_desc_hash_changed,
                        });
                    }
                    diff_branch_ops(old_br, new_br, &mut op_diffs, &mut has_breaking);
                }
            }
            (None, None) => {}
        }
    }

    branch_diffs.sort_by(|a, b| (&a.id, a.uid).cmp(&(&b.id, b.uid)));
    op_diffs.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));

    let group_diffs = diff_groups(before, after);
    let ownership_diffs = diff_ownership(before, after, &mut has_breaking);
    let join_diffs = diff_joins(before, after, &mut has_breaking);

    SnapshotDiff {
        from_generation: before.generation,
        to_generation: after.generation,
        branch_set_hash_changed: before.branch_set_hash != after.branch_set_hash,
        has_breaking_changes: has_breaking,
        branches: branch_diffs,
        operations: op_diffs,
        groups: group_diffs,
        ownership: ownership_diffs,
        joins: join_diffs,
    }
}

fn diff_branch_ops(
    old_br: &BranchDescriptor,
    new_br: &BranchDescriptor,
    out: &mut Vec<OperationDiffEntry>,
    has_breaking: &mut bool,
) {
    let old_ops: BTreeMap<&OperationId, &OperationDescriptor> = old_br
        .operations
        .iter()
        .map(|o| (&o.id, o.as_ref()))
        .collect();
    let new_ops: BTreeMap<&OperationId, &OperationDescriptor> = new_br
        .operations
        .iter()
        .map(|o| (&o.id, o.as_ref()))
        .collect();

    let all_ids: BTreeSet<&OperationId> = old_ops.keys().chain(new_ops.keys()).copied().collect();
    for op_id in all_ids {
        match (old_ops.get(op_id), new_ops.get(op_id)) {
            (None, Some(new_op)) => {
                out.push(OperationDiffEntry {
                    operation_id: (*op_id).clone(),
                    branch_id: new_br.id.clone(),
                    changes: vec![OperationChangeKind::Added],
                    breaking: false,
                    before_sdl: None,
                    after_sdl: Some((*new_op.sdl).to_owned()),
                    before_side_effect: None,
                    after_side_effect: Some(new_op.side_effect.level),
                });
            }
            (Some(old_op), None) => {
                *has_breaking = true;
                out.push(OperationDiffEntry {
                    operation_id: (*op_id).clone(),
                    branch_id: old_br.id.clone(),
                    changes: vec![OperationChangeKind::Removed],
                    breaking: true,
                    before_sdl: Some((*old_op.sdl).to_owned()),
                    after_sdl: None,
                    before_side_effect: Some(old_op.side_effect.level),
                    after_side_effect: None,
                });
            }
            (Some(old_op), Some(new_op)) => {
                let mut changes = Vec::new();
                let mut breaking = false;

                if old_op.sdl != new_op.sdl || old_op.kind != new_op.kind {
                    changes.push(OperationChangeKind::SignatureChanged);
                    breaking = true;
                }
                if old_op.desc_hash != new_op.desc_hash {
                    changes.push(OperationChangeKind::DescriptionChanged);
                }
                if old_op.side_effect.level != new_op.side_effect.level {
                    changes.push(OperationChangeKind::SideEffectChanged);
                    if side_effect_rank(new_op.side_effect.level)
                        > side_effect_rank(old_op.side_effect.level)
                    {
                        breaking = true;
                    }
                }
                if old_op.group != new_op.group {
                    changes.push(OperationChangeKind::GroupChanged);
                }

                if !changes.is_empty() {
                    if breaking {
                        *has_breaking = true;
                    }
                    out.push(OperationDiffEntry {
                        operation_id: (*op_id).clone(),
                        branch_id: new_br.id.clone(),
                        changes,
                        breaking,
                        before_sdl: Some((*old_op.sdl).to_owned()),
                        after_sdl: Some((*new_op.sdl).to_owned()),
                        before_side_effect: Some(old_op.side_effect.level),
                        after_side_effect: Some(new_op.side_effect.level),
                    });
                }
            }
            (None, None) => {}
        }
    }
}

const fn side_effect_rank(level: SideEffectLevel) -> u8 {
    match level {
        SideEffectLevel::None => 0,
        SideEffectLevel::Reversible => 1,
        SideEffectLevel::Irreversible => 2,
        SideEffectLevel::Unknown => 3,
    }
}

fn diff_groups(before: &Snapshot, after: &Snapshot) -> Vec<GroupDiffEntry> {
    let all_ids: BTreeSet<&GroupId> = before.groups.keys().chain(after.groups.keys()).collect();
    let mut diffs = Vec::new();
    for gid in all_ids {
        let old_grp = before.groups.get(gid);
        let new_grp = after.groups.get(gid);
        let old_members: BTreeSet<OperationId> = old_grp
            .map(|g| g.members.iter().map(|(_, op)| op.clone()).collect())
            .unwrap_or_default();
        let new_members: BTreeSet<OperationId> = new_grp
            .map(|g| g.members.iter().map(|(_, op)| op.clone()).collect())
            .unwrap_or_default();

        let added_members: Vec<OperationId> =
            new_members.difference(&old_members).cloned().collect();
        let removed_members: Vec<OperationId> =
            old_members.difference(&new_members).cloned().collect();
        let signature_changed = match (old_grp, new_grp) {
            (Some(a), Some(b)) => a.signature_sdl != b.signature_sdl,
            _ => false,
        };
        if !added_members.is_empty() || !removed_members.is_empty() || signature_changed {
            diffs.push(GroupDiffEntry {
                group_id: gid.clone(),
                added_members,
                removed_members,
                signature_changed,
            });
        }
    }
    diffs
}

fn diff_ownership(
    before: &Snapshot,
    after: &Snapshot,
    has_breaking: &mut bool,
) -> Vec<OwnershipDiffEntry> {
    let old_map = before.ownership.as_map();
    let new_map = after.ownership.as_map();
    let all_coords: BTreeSet<&FieldCoordinate> = old_map.keys().chain(new_map.keys()).collect();
    let mut diffs = Vec::new();
    for coord in all_coords {
        let before_owner = old_map.get(coord).copied();
        let after_owner = new_map.get(coord).copied();
        if before_owner != after_owner {
            if before_owner.is_some() && after_owner.is_none() {
                *has_breaking = true;
            }
            diffs.push(OwnershipDiffEntry {
                coordinate: coord.clone(),
                before_owner,
                after_owner,
            });
        }
    }
    diffs
}

fn diff_joins(before: &Snapshot, after: &Snapshot, has_breaking: &mut bool) -> Vec<JoinDiffEntry> {
    let old_edges: BTreeMap<&str, &JoinEdge> = before
        .joins
        .all_edges()
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();
    let new_edges: BTreeMap<&str, &JoinEdge> = after
        .joins
        .all_edges()
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();
    let all_ids: BTreeSet<&str> = old_edges.keys().chain(new_edges.keys()).copied().collect();
    let mut diffs = Vec::new();
    for id in all_ids {
        match (old_edges.get(id), new_edges.get(id)) {
            (None, Some(new_e)) => {
                diffs.push(JoinDiffEntry {
                    edge_id: new_e.id.clone(),
                    source_type: new_e.source_type.clone(),
                    field_name: new_e.field_name.clone(),
                    target_operation: new_e.target_operation.clone(),
                    before_status: None,
                    after_status: Some(new_e.status),
                });
            }
            (Some(old_e), None) => {
                if old_e.status == JoinStatus::Active {
                    *has_breaking = true;
                }
                diffs.push(JoinDiffEntry {
                    edge_id: old_e.id.clone(),
                    source_type: old_e.source_type.clone(),
                    field_name: old_e.field_name.clone(),
                    target_operation: old_e.target_operation.clone(),
                    before_status: Some(old_e.status),
                    after_status: None,
                });
            }
            (Some(old_e), Some(new_e)) if old_e != new_e => {
                if old_e.status == JoinStatus::Active && new_e.status != JoinStatus::Active {
                    *has_breaking = true;
                }
                diffs.push(JoinDiffEntry {
                    edge_id: new_e.id.clone(),
                    source_type: new_e.source_type.clone(),
                    field_name: new_e.field_name.clone(),
                    target_operation: new_e.target_operation.clone(),
                    before_status: Some(old_e.status),
                    after_status: Some(new_e.status),
                });
            }
            _ => {}
        }
    }
    diffs
}

fn branch_entry_json(entry: &BranchDiffEntry) -> JsonValue {
    let mut obj = JsonMap::new();
    obj.insert("uid".to_owned(), JsonValue::String(entry.uid.to_string()));
    obj.insert(
        "id".to_owned(),
        JsonValue::String(entry.id.as_str().to_owned()),
    );
    obj.insert(
        "change".to_owned(),
        JsonValue::String(entry.change.as_str().to_owned()),
    );
    obj.insert(
        "status_change".to_owned(),
        match entry.status_change {
            Some((a, b)) => JsonValue::Array(vec![
                JsonValue::String(format!("{a:?}")),
                JsonValue::String(format!("{b:?}")),
            ]),
            None => JsonValue::Null,
        },
    );
    obj.insert(
        "trust_change".to_owned(),
        match entry.trust_change {
            Some((a, b)) => JsonValue::Array(vec![
                JsonValue::String(format!("{a:?}")),
                JsonValue::String(format!("{b:?}")),
            ]),
            None => JsonValue::Null,
        },
    );
    obj.insert(
        "spec_hash_changed".to_owned(),
        JsonValue::Bool(entry.spec_hash_changed),
    );
    obj.insert(
        "pinned_desc_hash_changed".to_owned(),
        JsonValue::Bool(entry.pinned_desc_hash_changed),
    );
    JsonValue::Object(obj)
}

fn op_entry_json(entry: &OperationDiffEntry) -> JsonValue {
    let mut obj = JsonMap::new();
    obj.insert(
        "operation_id".to_owned(),
        JsonValue::String(entry.operation_id.as_str().to_owned()),
    );
    obj.insert(
        "branch_id".to_owned(),
        JsonValue::String(entry.branch_id.as_str().to_owned()),
    );
    obj.insert(
        "changes".to_owned(),
        JsonValue::Array(
            entry
                .changes
                .iter()
                .map(|c| JsonValue::String(c.as_str().to_owned()))
                .collect(),
        ),
    );
    obj.insert("breaking".to_owned(), JsonValue::Bool(entry.breaking));
    JsonValue::Object(obj)
}

fn group_entry_json(entry: &GroupDiffEntry) -> JsonValue {
    let mut obj = JsonMap::new();
    obj.insert(
        "group_id".to_owned(),
        JsonValue::String(entry.group_id.to_string()),
    );
    obj.insert(
        "added_members".to_owned(),
        JsonValue::Array(
            entry
                .added_members
                .iter()
                .map(|m| JsonValue::String(m.as_str().to_owned()))
                .collect(),
        ),
    );
    obj.insert(
        "removed_members".to_owned(),
        JsonValue::Array(
            entry
                .removed_members
                .iter()
                .map(|m| JsonValue::String(m.as_str().to_owned()))
                .collect(),
        ),
    );
    obj.insert(
        "signature_changed".to_owned(),
        JsonValue::Bool(entry.signature_changed),
    );
    JsonValue::Object(obj)
}

fn ownership_entry_json(entry: &OwnershipDiffEntry) -> JsonValue {
    let mut obj = JsonMap::new();
    obj.insert(
        "coordinate".to_owned(),
        JsonValue::String(entry.coordinate.to_string()),
    );
    obj.insert(
        "before_owner".to_owned(),
        entry
            .before_owner
            .map_or(JsonValue::Null, |u| JsonValue::String(u.to_string())),
    );
    obj.insert(
        "after_owner".to_owned(),
        entry
            .after_owner
            .map_or(JsonValue::Null, |u| JsonValue::String(u.to_string())),
    );
    JsonValue::Object(obj)
}

fn join_entry_json(entry: &JoinDiffEntry) -> JsonValue {
    let mut obj = JsonMap::new();
    obj.insert(
        "edge_id".to_owned(),
        JsonValue::String(entry.edge_id.clone()),
    );
    obj.insert(
        "source_type".to_owned(),
        JsonValue::String(entry.source_type.clone()),
    );
    obj.insert(
        "field_name".to_owned(),
        JsonValue::String(entry.field_name.clone()),
    );
    obj.insert(
        "target_operation".to_owned(),
        JsonValue::String(entry.target_operation.as_str().to_owned()),
    );
    obj.insert(
        "before_status".to_owned(),
        entry
            .before_status
            .map_or(JsonValue::Null, |s| JsonValue::String(format!("{s:?}"))),
    );
    obj.insert(
        "after_status".to_owned(),
        entry
            .after_status
            .map_or(JsonValue::Null, |s| JsonValue::String(format!("{s:?}"))),
    );
    JsonValue::Object(obj)
}
