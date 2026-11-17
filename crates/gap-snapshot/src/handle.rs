//! Wait-free `SnapshotHandle` over `ArcSwap` and delta-sharing `SnapshotBuilder` (`RUN-003`, `RUN-004`, `ARC-005`, `INV-01`, `INV-02`, `INV-16`).

use std::collections::BTreeSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use gap_core::clock::Timestamp;
use gap_core::ids::{BranchUid, ProposalId};
use gap_core::snapshot::{CapabilityGroup, OperationDescriptor, SNAPSHOT_FORMAT_VERSION, Snapshot};
use thiserror::Error;

use crate::compose::Composition;

/// Error returned when [`SnapshotHandle::publish`] rejects a candidate [`Snapshot`] (`RUN-003`, `RUN-004`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PublishError {
    /// Candidate `next.generation` was not equal to `current.generation + 1` (`RUN-003`).
    #[error("non-monotonic snapshot generation: expected {expected}, got {got}")]
    NonMonotonic {
        /// Expected consecutive generation (`current + 1`).
        expected: u64,
        /// Rejected candidate generation.
        got: u64,
    },
    /// Candidate `next.format_version` is not supported by this runtime (`RUN-004`).
    #[error("unsupported snapshot format_version {got} (supported: {supported})")]
    UnsupportedFormat {
        /// Candidate format version.
        got: u32,
        /// Supported format version ([`SNAPSHOT_FORMAT_VERSION`]).
        supported: u32,
    },
}

/// Wait-free snapshot holder serving pinned [`Arc<Snapshot>`] handles on the request path (`INV-01`, `INV-02`).
#[derive(Debug)]
pub struct SnapshotHandle {
    cell: ArcSwap<Snapshot>,
}

impl SnapshotHandle {
    /// Initializes a [`SnapshotHandle`] holding `initial`.
    #[must_use]
    pub fn new(initial: Snapshot) -> Self {
        Self {
            cell: ArcSwap::from_pointee(initial),
        }
    }

    // BEGIN_READ_PATH
    /// Pins the active immutable [`Snapshot`] for the duration of a request (`INV-01`, `INV-02`).
    #[inline]
    #[must_use]
    pub fn pin(&self) -> Arc<Snapshot> {
        self.cell.load_full()
    }

    /// Returns the active snapshot generation (`INV-02`).
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.cell.load().generation
    }
    // END_READ_PATH

    /// Atomically publishes `next` if `next.format_version == SNAPSHOT_FORMAT_VERSION` and
    /// `next.generation == current.generation + 1` (`RUN-003`, `RUN-004`, `INV-16`).
    ///
    /// # Errors
    ///
    /// Returns [`PublishError::UnsupportedFormat`] or [`PublishError::NonMonotonic`] without
    /// mutating the active snapshot when validation fails.
    pub fn publish(&self, next: Snapshot) -> Result<u64, PublishError> {
        if next.format_version != SNAPSHOT_FORMAT_VERSION {
            return Err(PublishError::UnsupportedFormat {
                got: next.format_version,
                supported: SNAPSHOT_FORMAT_VERSION,
            });
        }

        let next_gen = next.generation;
        let candidate = Arc::new(next);
        let mut cur = self.cell.load();
        loop {
            let Some(expected) = cur.generation.checked_add(1) else {
                return Err(PublishError::NonMonotonic {
                    expected: u64::MAX,
                    got: next_gen,
                });
            };
            if next_gen != expected {
                return Err(PublishError::NonMonotonic {
                    expected,
                    got: next_gen,
                });
            }

            let cur_ptr = Arc::as_ptr(&cur);
            let prev = self.cell.compare_and_swap(&cur, Arc::clone(&candidate));
            if std::ptr::eq(Arc::as_ptr(&prev), cur_ptr) {
                return Ok(next_gen);
            }
            cur = prev;
        }
    }
}

fn make_capability_group(branch_uid: BranchUid, op: &OperationDescriptor) -> Arc<CapabilityGroup> {
    Arc::new(CapabilityGroup {
        id: op.group.clone(),
        signature_sdl: Arc::clone(&op.sdl),
        members: vec![(branch_uid, op.id.clone())],
    })
}

/// Incremental [`Snapshot`] builder that reuses unchanged [`Arc`] descriptors and `gap_core::snapshot::OrdMap`
/// B-tree nodes from a parent [`Snapshot`] (`ARC-005`).
#[derive(Debug)]
pub struct SnapshotBuilder<'a> {
    current: &'a Snapshot,
    composition: Composition,
    built_at: Timestamp,
    builder: String,
    proposal_id: Option<ProposalId>,
}

impl<'a> SnapshotBuilder<'a> {
    /// Starts building generation `current.generation + 1` from `current` and `composition`.
    #[must_use]
    pub fn new(current: &'a Snapshot, composition: Composition) -> Self {
        Self {
            current,
            composition,
            built_at: Timestamp::UNIX_EPOCH,
            builder: "gap-snapshot".to_owned(),
            proposal_id: None,
        }
    }

    /// Sets the UTC build timestamp recorded on the resulting [`Snapshot`].
    #[must_use]
    pub fn built_at(mut self, built_at: Timestamp) -> Self {
        self.built_at = built_at;
        self
    }

    /// Sets the `BuildProvenance.builder` label.
    #[must_use]
    pub fn builder(mut self, builder: impl Into<String>) -> Self {
        self.builder = builder.into();
        self
    }

    /// Sets the optional originating `ProposalId` on `BuildProvenance`.
    #[must_use]
    pub fn proposal_id(mut self, proposal_id: Option<ProposalId>) -> Self {
        self.proposal_id = proposal_id;
        self
    }

    /// Produces the next immutable [`Snapshot`] generation (`ARC-005`).
    #[must_use]
    pub fn build(self) -> Snapshot {
        let next_gen = self.current.generation.saturating_add(1);
        let mut next =
            self.current
                .next_generation(next_gen, self.built_at, self.builder, self.proposal_id);

        let new_uids: BTreeSet<BranchUid> =
            self.composition.branches.iter().map(|b| b.uid).collect();
        let removed_uids: Vec<BranchUid> = self
            .current
            .branches
            .keys()
            .copied()
            .filter(|uid| !new_uids.contains(uid))
            .collect();

        for removed_uid in removed_uids {
            if let Some(removed_branch) = next.branches.remove(&removed_uid) {
                for op in &removed_branch.operations {
                    next.groups.remove(&op.group);
                }
            }
        }

        for new_branch in self.composition.branches {
            let branch_uid = new_branch.uid;
            if let Some(old_branch) = self.current.branches.get(&branch_uid) {
                if old_branch.as_ref() == new_branch.as_ref() {
                    // Unchanged branch: skip `next.branches.insert` so `gap_core::snapshot::OrdMap` leaf nodes
                    // and the `Arc<BranchDescriptor>` stay shared by pointer (`ARC-005`).
                    for op in &old_branch.operations {
                        if !next.groups.contains_key(&op.group) {
                            next.groups
                                .insert(op.group.clone(), make_capability_group(branch_uid, op));
                        }
                    }
                    continue;
                }

                for old_op in &old_branch.operations {
                    let still_present = new_branch
                        .operations
                        .iter()
                        .any(|o| o.group == old_op.group);
                    if !still_present {
                        next.groups.remove(&old_op.group);
                    }
                }
            }

            for op in &new_branch.operations {
                let desired = CapabilityGroup {
                    id: op.group.clone(),
                    signature_sdl: Arc::clone(&op.sdl),
                    members: vec![(branch_uid, op.id.clone())],
                };
                let already_same = next
                    .groups
                    .get(&op.group)
                    .is_some_and(|existing| existing.as_ref() == &desired);
                if !already_same {
                    next.groups.insert(op.group.clone(), Arc::new(desired));
                }
            }

            next.branches.insert(branch_uid, new_branch);
        }

        next.schema = Arc::new(self.composition.schema.into_inner());
        if self.current.ownership.as_ref() != &self.composition.ownership {
            next.ownership = Arc::new(self.composition.ownership);
        }
        next.branch_set_hash = self.composition.branch_set_hash;
        next
    }
}
