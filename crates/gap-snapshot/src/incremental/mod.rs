//! Structural-sharing incremental supergraph composition, descriptor pooling, and branch
//! removal grace-period integration (`WP-M4-09`, `ADR-009`, `ARC-005`, `RUN-009`, `ING-055`).

pub mod grace;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use apollo_compiler::Schema;
use apollo_compiler::validation::Valid;
use gap_core::clock::Timestamp;
use gap_core::ids::{BranchUid, OperationId, ProposalId};
use gap_core::snapshot::{BranchDescriptor, FieldOwnership, OperationDescriptor, Snapshot};
use prost_types::value::Kind as ProstValueKind;

use crate::compose::{BranchInput, Composition, CompositionError, CompositionWarning, compose};
use crate::handle::SnapshotBuilder;

pub use grace::{
    BranchRemovalTracker, DEFAULT_BRANCH_REMOVAL_GRACE, DeprecatedOperationEntry,
    RemovedOperationResolution,
};

/// Pool reuse counters reported by [`IncrementalComposer`] for observability and testing (`ADR-009`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IncrementalStats {
    /// Total number of branches in the target composition.
    pub total_branches: usize,
    /// Number of branches whose existing `Arc<BranchDescriptor>` pointer was reused cleanly.
    pub reused_branches: usize,
    /// Number of branches that required rebuilding a fresh `BranchDescriptor`.
    pub recompiled_branches: usize,
    /// Number of individual `Arc<OperationDescriptor>` pointers reused from the pool.
    pub reused_operations: usize,
    /// Number of individual `Arc<OperationDescriptor>` objects freshly allocated.
    pub recompiled_operations: usize,
    /// `true` iff the entire supergraph `Valid<Schema>` was hit cleanly with zero branch diffs.
    pub schema_reused: bool,
}

#[derive(Debug, Clone)]
struct CachedFullComposition {
    sorted_branch_digests: Vec<(BranchUid, [u8; 32])>,
    schema: Valid<Schema>,
    sdl: String,
    branches: Vec<Arc<BranchDescriptor>>,
    ownership: FieldOwnership,
    warnings: Vec<CompositionWarning>,
    branch_set_hash: [u8; 32],
}

/// Shared allocation pool retaining `Arc<str>`, `Arc<OperationDescriptor>`, and
/// `Arc<BranchDescriptor>` handles across snapshot generations (`ADR-009`, `ARC-005`).
#[derive(Debug, Clone, Default)]
pub struct IncrementalPool {
    sdl_strings: BTreeMap<[u8; 32], Arc<str>>,
    ops: BTreeMap<OperationId, Arc<OperationDescriptor>>,
    branches: BTreeMap<BranchUid, Arc<BranchDescriptor>>,
    last_composition: Option<CachedFullComposition>,
}

impl IncrementalPool {
    /// Creates an empty [`IncrementalPool`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of cached branch descriptors currently held in the pool.
    #[must_use]
    pub fn cached_branch_count(&self) -> usize {
        self.branches.len()
    }

    /// Number of cached operation descriptors currently held in the pool.
    #[must_use]
    pub fn cached_operation_count(&self) -> usize {
        self.ops.len()
    }

    fn intern_sdl(&mut self, sdl: &Arc<str>) -> Arc<str> {
        let hash = *blake3::hash(sdl.as_bytes()).as_bytes();
        if let Some(existing) = self.sdl_strings.get(&hash) {
            return Arc::clone(existing);
        }
        let cloned = Arc::clone(sdl);
        self.sdl_strings.insert(hash, Arc::clone(&cloned));
        cloned
    }

    fn reuse_or_update_operation(
        &mut self,
        op: Arc<OperationDescriptor>,
    ) -> (Arc<OperationDescriptor>, bool) {
        if let Some(cached) = self.ops.get(&op.id)
            && cached.as_ref() == op.as_ref()
        {
            return (Arc::clone(cached), true);
        }
        let mut owned = (*op).clone();
        owned.sdl = self.intern_sdl(&op.sdl);
        let interned = Arc::new(owned);
        self.ops.insert(interned.id.clone(), Arc::clone(&interned));
        (interned, false)
    }
}

/// Stateful incremental supergraph composer pairing [`IncrementalPool`] structural sharing
/// (`ADR-009`) with [`BranchRemovalTracker`] `24 h` `DEPRECATED` grace tombstones (`ING-055`).
#[derive(Debug, Clone, Default)]
pub struct IncrementalComposer {
    pool: IncrementalPool,
    removal_tracker: BranchRemovalTracker,
}

impl IncrementalComposer {
    /// Creates an [`IncrementalComposer`] with default `24 h` branch removal grace (`ING-055`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an [`IncrementalComposer`] backed by a custom [`BranchRemovalTracker`].
    #[must_use]
    pub fn with_removal_tracker(removal_tracker: BranchRemovalTracker) -> Self {
        Self {
            pool: IncrementalPool::new(),
            removal_tracker,
        }
    }

    /// Borrows the internal [`IncrementalPool`] metrics/cache.
    #[must_use]
    pub const fn pool(&self) -> &IncrementalPool {
        &self.pool
    }

    /// Borrows the [`BranchRemovalTracker`].
    #[must_use]
    pub const fn removal_tracker(&self) -> &BranchRemovalTracker {
        &self.removal_tracker
    }

    /// Mutably borrows the [`BranchRemovalTracker`] (for example to call `purge_expired`).
    pub const fn removal_tracker_mut(&mut self) -> &mut BranchRemovalTracker {
        &mut self.removal_tracker
    }

    /// Seeds the internal pool from an already-published [`Snapshot`] (for example on startup).
    pub fn warm_from_snapshot(&mut self, snapshot: &Snapshot) {
        for (uid, branch) in &snapshot.branches {
            for op in &branch.operations {
                let _ = self.pool.intern_sdl(&op.sdl);
                self.pool.ops.insert(op.id.clone(), Arc::clone(op));
            }
            self.pool.branches.insert(*uid, Arc::clone(branch));
        }
    }

    /// Incrementally composes `inputs`, reusing cached `Valid<Schema>` when zero branches changed
    /// and canonicalizing unchanged `Arc<BranchDescriptor>` / `Arc<OperationDescriptor>` pointers
    /// via `self.pool` when a subset of branches changed (`ADR-009`, `RUN-009`).
    ///
    /// # Errors
    ///
    /// Propagates any validation or ownership collision error from [`compose`].
    pub fn compose_incremental(
        &mut self,
        inputs: &[BranchInput],
    ) -> Result<(Composition, IncrementalStats), CompositionError> {
        let mut sorted_digests: Vec<(BranchUid, [u8; 32])> = inputs
            .iter()
            .map(|input| (input.uid, digest_branch_input(input)))
            .collect();
        sorted_digests.sort_by_key(|(uid, _)| *uid);

        if let Some(cached) = &self.pool.last_composition
            && cached.sorted_branch_digests == sorted_digests
        {
            let total_ops: usize = cached.branches.iter().map(|b| b.operations.len()).sum();
            return Ok((
                Composition {
                    schema: cached.schema.clone(),
                    sdl: cached.sdl.clone(),
                    branches: cached.branches.clone(),
                    ownership: cached.ownership.clone(),
                    warnings: cached.warnings.clone(),
                    branch_set_hash: cached.branch_set_hash,
                },
                IncrementalStats {
                    total_branches: cached.branches.len(),
                    reused_branches: cached.branches.len(),
                    recompiled_branches: 0,
                    reused_operations: total_ops,
                    recompiled_operations: 0,
                    schema_reused: true,
                },
            ));
        }

        let mut composed = compose(inputs)?;
        let active_uids: BTreeSet<BranchUid> = inputs.iter().map(|i| i.uid).collect();

        let mut reused_branches = 0usize;
        let mut recompiled_branches = 0usize;
        let mut reused_operations = 0usize;
        let mut recompiled_operations = 0usize;
        let mut active_op_ids = BTreeSet::new();

        for branch_slot in &mut composed.branches {
            let uid = branch_slot.uid;

            if let Some(prev_desc) = self.pool.branches.get(&uid)
                && prev_desc.as_ref() == branch_slot.as_ref()
            {
                reused_branches += 1;
                reused_operations += prev_desc.operations.len();
                for op in &prev_desc.operations {
                    active_op_ids.insert(op.id.clone());
                }
                *branch_slot = Arc::clone(prev_desc);
                continue;
            }

            recompiled_branches += 1;
            let mut updated_branch = (**branch_slot).clone();
            let mut canonical_ops = Vec::with_capacity(updated_branch.operations.len());
            for op in updated_branch.operations {
                active_op_ids.insert(op.id.clone());
                let (interned_op, was_reused) = self.pool.reuse_or_update_operation(op);
                if was_reused {
                    reused_operations += 1;
                } else {
                    recompiled_operations += 1;
                }
                canonical_ops.push(interned_op);
            }
            updated_branch.operations = canonical_ops;
            let canonical_branch = Arc::new(updated_branch);
            self.pool
                .branches
                .insert(uid, Arc::clone(&canonical_branch));
            *branch_slot = canonical_branch;
        }

        // Retain only branches and ops belonging to the active generation so long-lived pools stay
        // strictly bounded to active supergraph size.
        self.pool
            .branches
            .retain(|uid, _| active_uids.contains(uid));
        self.pool
            .ops
            .retain(|op_id, _| active_op_ids.contains(op_id));

        let stats = IncrementalStats {
            total_branches: composed.branches.len(),
            reused_branches,
            recompiled_branches,
            reused_operations,
            recompiled_operations,
            schema_reused: false,
        };

        self.pool.last_composition = Some(CachedFullComposition {
            sorted_branch_digests: sorted_digests,
            schema: composed.schema.clone(),
            sdl: composed.sdl.clone(),
            branches: composed.branches.clone(),
            ownership: composed.ownership.clone(),
            warnings: composed.warnings.clone(),
            branch_set_hash: composed.branch_set_hash,
        });

        Ok((composed, stats))
    }

    /// Incrementally composes `inputs`, builds the next [`Snapshot`] generation via
    /// [`SnapshotBuilder`], and records any removed branches in [`BranchRemovalTracker`] (`ADR-009`,
    /// `ING-055`).
    ///
    /// # Errors
    ///
    /// Propagates any error returned by [`IncrementalComposer::compose_incremental`].
    pub fn build_next_snapshot(
        &mut self,
        current: &Snapshot,
        inputs: &[BranchInput],
        built_at: Timestamp,
        builder: impl Into<String>,
        proposal_id: Option<ProposalId>,
    ) -> Result<(Snapshot, IncrementalStats), CompositionError> {
        let (composition, stats) = self.compose_incremental(inputs)?;
        let next = SnapshotBuilder::new(current, composition)
            .built_at(built_at)
            .builder(builder)
            .proposal_id(proposal_id)
            .build();
        let _ = self
            .removal_tracker
            .record_snapshot_transition(current, &next, built_at);
        Ok((next, stats))
    }
}

fn digest_branch_input(input: &BranchInput) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let _ = hasher.update(input.uid.to_string().as_bytes());
    let _ = hasher.update(b"\x00");
    let _ = hasher.update(input.id.as_str().as_bytes());
    let _ = hasher.update(format!("{:?}:{:?}", input.protocol, input.trust).as_bytes());
    let _ = hasher.update(input.endpoint.uri.as_bytes());
    let _ = hasher.update(input.endpoint.transport.as_bytes());
    let _ = hasher.update(format!("{:?}", input.status).as_bytes());

    let desc = &input.description;
    let _ = hasher.update(desc.connector_id.as_bytes());
    let _ = hasher.update(desc.connector_version.as_bytes());
    let _ = hasher.update(desc.ngc_version.as_bytes());
    let _ = hasher.update(&desc.protocol.to_le_bytes());
    let _ = hasher.update(desc.spec_hash.as_bytes());
    let _ = hasher.update(desc.schema_sdl.as_bytes());
    if let Some(caps) = &desc.capabilities {
        let mut pred_ops = caps.predicate_ops.clone();
        pred_ops.sort_unstable();
        let _ = hasher.update(
            format!(
                "{}:{}:{}:{:?}:{}:{}:{}:{}:{}:{}:{}:{}",
                caps.foreach_batch,
                caps.order_by,
                caps.pagination,
                pred_ops,
                caps.native_traverse,
                caps.subscribe,
                caps.tasks,
                caps.dry_run,
                caps.idempotency,
                caps.sketch,
                caps.explain,
                caps.as_of
            )
            .as_bytes(),
        );
    }
    if let Some(limits) = &desc.limits {
        let _ = hasher.update(&limits.max_batch_size.to_le_bytes());
        let _ = hasher.update(&limits.max_in_flight.to_le_bytes());
        let _ = hasher.update(&limits.max_page_size.to_le_bytes());
        let _ = hasher.update(&limits.default_timeout_ms.to_le_bytes());
        let _ = hasher.update(&limits.max_response_bytes.to_le_bytes());
    }
    if let Some(meta) = &desc.meta {
        let mut nodes = meta.node_categories.clone();
        let mut edges = meta.edge_predicates.clone();
        nodes.sort();
        edges.sort();
        let _ = hasher.update(format!("{nodes:?}{edges:?}{}", meta.synopsis_hint).as_bytes());
    }
    if let Some(auth) = &desc.auth {
        let mut scopes = auth.scopes.clone();
        scopes.sort();
        let _ = hasher.update(
            format!(
                "{scopes:?}:{}:{}",
                auth.needs_end_user_delegation, auth.resource_indicator
            )
            .as_bytes(),
        );
    }

    let mut ops: Vec<&gap_proto::ngc::v1::OperationDescriptor> = desc.operations.iter().collect();
    ops.sort_by(|a, b| a.name.cmp(&b.name));
    for op in ops {
        let mut buf = String::new();
        let _ = write!(
            buf,
            "op={}:{}:{}:{}:{}:{}:{}:{}:{}:{}:",
            op.name,
            op.kind,
            op.description,
            op.side_effect_hint,
            op.idempotent_hint,
            op.supports_dry_run,
            op.compensation_operation,
            op.cost_weight,
            op.pagination,
            op.key_fields.join(",")
        );
        let mut exs: Vec<(String, String, bool)> = op
            .examples
            .iter()
            .map(|e| (e.intent.clone(), e.gapql.clone(), e.negative))
            .collect();
        exs.sort();
        for (i, q, neg) in exs {
            let _ = write!(buf, "ex={neg}:{i}:{q};");
        }
        let sorted_anns: BTreeMap<&String, &String> = op.annotations.iter().collect();
        for (k, v) in sorted_anns {
            let _ = write!(buf, "ann={k}:{v};");
        }
        let _ = hasher.update(buf.as_bytes());
        if let Some(in_schema) = &op.input_schema {
            hash_prost_struct(&mut hasher, in_schema);
        }
        if let Some(out_schema) = &op.output_schema {
            hash_prost_struct(&mut hasher, out_schema);
        }
    }

    *hasher.finalize().as_bytes()
}

fn hash_prost_struct(hasher: &mut blake3::Hasher, st: &prost_types::Struct) {
    let _ = hasher.update(b"{");
    for (k, v) in &st.fields {
        let _ = hasher.update(k.as_bytes());
        let _ = hasher.update(b":");
        hash_prost_value(hasher, v);
        let _ = hasher.update(b",");
    }
    let _ = hasher.update(b"}");
}

fn hash_prost_value(hasher: &mut blake3::Hasher, val: &prost_types::Value) {
    match val.kind.as_ref() {
        None | Some(ProstValueKind::NullValue(_)) => {
            let _ = hasher.update(b"null");
        }
        Some(ProstValueKind::BoolValue(b)) => {
            let _ = hasher.update(if *b { b"true" } else { b"false" });
        }
        Some(ProstValueKind::NumberValue(n)) => {
            let _ = hasher.update(&n.to_bits().to_le_bytes());
        }
        Some(ProstValueKind::StringValue(s)) => {
            let _ = hasher.update(s.as_bytes());
        }
        Some(ProstValueKind::StructValue(st)) => hash_prost_struct(hasher, st),
        Some(ProstValueKind::ListValue(list)) => {
            let _ = hasher.update(b"[");
            for elem in &list.values {
                hash_prost_value(hasher, elem);
                let _ = hasher.update(b",");
            }
            let _ = hasher.update(b"]");
        }
    }
}
