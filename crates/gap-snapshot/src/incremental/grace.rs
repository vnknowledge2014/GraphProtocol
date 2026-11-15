//! Branch removal `24 h` `DEPRECATED` grace-period tracking and group-sibling alternative
//! suggestions (`WP-M4-09`, `ING-055`).

use std::collections::BTreeMap;
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::error::{ErrorCode, GapError, Suggestion};
use gap_core::ids::{BranchId, BranchUid, GroupId, OperationId};
use gap_core::snapshot::{BranchStatus, Snapshot};

/// Default retention window during which operations of a removed branch remain visible as
/// `DEPRECATED` (`24 h`, `ING-055`).
pub const DEFAULT_BRANCH_REMOVAL_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Metadata recorded for a single operation from a removed branch during its grace period (`ING-055`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeprecatedOperationEntry {
    /// Original `<branch>.<op>` identifier.
    pub operation_id: OperationId,
    /// UUIDv7 of the removed branch.
    pub removed_branch_uid: BranchUid,
    /// Slug of the removed branch.
    pub removed_branch_id: BranchId,
    /// Capability group identifier used to suggest active replica alternatives (`ADR-006`).
    pub group_id: GroupId,
    /// UTC timestamp when the branch removal snapshot was published.
    pub deprecated_at: Timestamp,
    /// UTC timestamp when the operation disappears cleanly (`deprecated_at + grace_period`).
    pub expires_at: Timestamp,
}

/// Resolution result for a query targeting an operation of a recently removed branch (`ING-055`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovedOperationResolution {
    /// Within the `24 h` grace window: operation is `DEPRECATED` and returns
    /// `GAP_BRANCH_UNAVAILABLE` with optional `suggestion.alternative`.
    Deprecated {
        /// Recorded grace-period metadata.
        entry: DeprecatedOperationEntry,
        /// Active sibling operation in the same capability group, if any.
        alternative: Option<OperationId>,
    },
    /// Grace window elapsed (or operation was never tracked): operation has disappeared.
    Expired,
}

/// Stateful tracker governing `24 h` `DEPRECATED` tombstones after branch removal (`ING-055`).
#[derive(Debug, Clone)]
pub struct BranchRemovalTracker {
    grace_period: Duration,
    entries: BTreeMap<OperationId, DeprecatedOperationEntry>,
}

impl Default for BranchRemovalTracker {
    fn default() -> Self {
        Self::new(DEFAULT_BRANCH_REMOVAL_GRACE)
    }
}

impl BranchRemovalTracker {
    /// Creates a [`BranchRemovalTracker`] with a custom grace duration (`default = 24 h`, `ING-055`).
    #[must_use]
    pub const fn new(grace_period: Duration) -> Self {
        Self {
            grace_period,
            entries: BTreeMap::new(),
        }
    }

    /// Returns the configured grace window duration.
    #[must_use]
    pub const fn grace_period(&self) -> Duration {
        self.grace_period
    }

    /// Inspects a snapshot transition `previous -> next` at `removed_at`, marking all operations
    /// of removed branches as `DEPRECATED` until `removed_at + grace_period` and clearing any
    /// operations that re-appeared in `next`.
    pub fn record_snapshot_transition(
        &mut self,
        previous: &Snapshot,
        next: &Snapshot,
        removed_at: Timestamp,
    ) -> usize {
        // Re-added active operations leave the deprecated tombstone table immediately.
        for branch in next.branches.values() {
            for op in &branch.operations {
                self.entries.remove(&op.id);
            }
        }

        let expires_at = add_duration_saturating(removed_at, self.grace_period);
        let mut recorded = 0usize;

        for (uid, prev_branch) in &previous.branches {
            if next.branches.contains_key(uid) {
                continue;
            }
            for op in &prev_branch.operations {
                self.entries.insert(
                    op.id.clone(),
                    DeprecatedOperationEntry {
                        operation_id: op.id.clone(),
                        removed_branch_uid: *uid,
                        removed_branch_id: prev_branch.id.clone(),
                        group_id: op.group.clone(),
                        deprecated_at: removed_at,
                        expires_at,
                    },
                );
                recorded += 1;
            }
        }
        recorded
    }

    /// Removes entries whose `24 h` grace window has elapsed (`now >= expires_at`).
    pub fn purge_expired(&mut self, now: Timestamp) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| now < entry.expires_at);
        before.saturating_sub(self.entries.len())
    }

    /// Returns all currently active `DEPRECATED` operations (`now < expires_at`) in sorted order.
    #[must_use]
    pub fn active_deprecated_operations(&self, now: Timestamp) -> Vec<&DeprecatedOperationEntry> {
        self.entries
            .values()
            .filter(|entry| now < entry.expires_at)
            .collect()
    }

    /// Classifies an incoming request for `operation_id` against `active_snapshot` at `now`.
    #[must_use]
    pub fn inspect_removed_operation(
        &self,
        active_snapshot: &Snapshot,
        operation_id: &OperationId,
        now: Timestamp,
    ) -> RemovedOperationResolution {
        let Some(entry) = self.entries.get(operation_id) else {
            return RemovedOperationResolution::Expired;
        };
        if now >= entry.expires_at {
            return RemovedOperationResolution::Expired;
        }

        let alternative =
            find_active_group_sibling(active_snapshot, &entry.group_id, entry.removed_branch_uid);
        RemovedOperationResolution::Deprecated {
            entry: entry.clone(),
            alternative,
        }
    }

    /// Builds the `GAP_BRANCH_UNAVAILABLE` error with `suggestion.alternative` when `operation_id`
    /// is inside its `DEPRECATED` grace window, or returns `None` once expired (`ING-055`).
    #[must_use]
    pub fn resolve_removed_operation(
        &self,
        active_snapshot: &Snapshot,
        operation_id: &OperationId,
        now: Timestamp,
    ) -> Option<GapError> {
        match self.inspect_removed_operation(active_snapshot, operation_id, now) {
            RemovedOperationResolution::Deprecated { entry, alternative } => {
                let mut err = GapError::new(
                    ErrorCode::BranchUnavailable,
                    format!(
                        "operation `{operation_id}` is DEPRECATED because branch `{}` was removed (available until `{}`)",
                        entry.removed_branch_id, entry.expires_at
                    ),
                );
                if let Some(alt) = alternative {
                    err = err.with_suggestion(Suggestion {
                        alternative: Some(alt.as_str().to_owned()),
                        ..Suggestion::default()
                    });
                }
                Some(err)
            }
            RemovedOperationResolution::Expired => None,
        }
    }
}

fn find_active_group_sibling(
    snapshot: &Snapshot,
    group_id: &GroupId,
    removed_branch_uid: BranchUid,
) -> Option<OperationId> {
    let mut candidates: Vec<OperationId> = Vec::new();

    if let Some(group) = snapshot.groups.get(group_id) {
        for (member_uid, member_op) in &group.members {
            if *member_uid != removed_branch_uid
                && snapshot
                    .branches
                    .get(member_uid)
                    .is_some_and(|b| b.status == BranchStatus::Active)
            {
                candidates.push(member_op.clone());
            }
        }
    }

    // Fallback scan over active branch operations when group membership was attached directly on
    // `OperationDescriptor.group`.
    for (uid, branch) in &snapshot.branches {
        if *uid == removed_branch_uid || branch.status != BranchStatus::Active {
            continue;
        }
        for op in &branch.operations {
            if &op.group == group_id {
                candidates.push(op.id.clone());
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates.into_iter().next()
}

fn add_duration_saturating(ts: Timestamp, delta: Duration) -> Timestamp {
    let add_nanos = i128::try_from(delta.as_nanos()).unwrap_or(i128::MAX);
    let total = ts.unix_nanos().saturating_add(add_nanos);
    Timestamp::from_unix_nanos(total).unwrap_or(ts)
}
