//! Deterministic builders for `gap-core` [`Snapshot`], [`BranchDescriptor`], and
//! [`OperationDescriptor`] values (`ARC-005`, `ARC-006`, `INV-09`).

use std::collections::BTreeMap;
use std::sync::Arc;

use gap_core::clock::Timestamp;
use gap_core::ids::{
    BranchId, BranchUid, GroupId, IdValidationError, OperationId, ProposalId, TenantId,
};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, BuildProvenance, CalibrationTable, Capabilities,
    CapabilityGroup, CostHint, Endpoint, Example, OpKind, OperationDescriptor, Protocol,
    SideEffect, SideEffectLevel, Snapshot, TrustTier,
};
use thiserror::Error;

/// Validation error returned when a snapshot, branch, or operation builder receives invalid inputs.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SnapshotBuilderError {
    /// Identifier validation failed (`TenantId`, `BranchId`, `OperationId`, `GroupId`, etc.).
    #[error(transparent)]
    InvalidId(#[from] IdValidationError),
    /// An operation attached to a branch carried a `<branch>.<name>` prefix for a different branch.
    #[error(
        "operation `{operation_id}` belongs to branch `{operation_branch}`, expected `{branch_id}`"
    )]
    MismatchedOperationBranch {
        /// Branch slug being built.
        branch_id: String,
        /// Operation's `<branch>` prefix.
        operation_branch: String,
        /// Full `<branch>.<name>` operation ID.
        operation_id: String,
    },
    /// Two branches in the same snapshot shared the same [`BranchUid`].
    #[error("duplicate BranchUid `{0}` in SnapshotBuilder")]
    DuplicateBranchUid(BranchUid),
    /// Two branches in the same snapshot shared the same [`BranchId`] slug.
    #[error("duplicate BranchId `{0}` in SnapshotBuilder")]
    DuplicateBranchId(BranchId),
}

/// Fluent builder for [`OperationDescriptor`] with sensible read-only defaults.
#[derive(Debug, Clone)]
pub struct OperationDescriptorBuilder {
    qualified_id: String,
    group: Option<GroupId>,
    kind: OpKind,
    side_effect: SideEffect,
    idempotent: bool,
    cost: CostHint,
    desc_hash: Option<[u8; 32]>,
    sdl: Option<Arc<str>>,
    examples: Vec<Example>,
}

impl OperationDescriptorBuilder {
    /// Creates an [`OperationDescriptorBuilder`] for `<branch>.<name>` (e.g. `"billing.getInvoice"`).
    #[must_use]
    pub fn new(qualified_id: impl Into<String>) -> Self {
        Self {
            qualified_id: qualified_id.into(),
            group: None,
            kind: OpKind::Query,
            side_effect: SideEffect {
                level: SideEffectLevel::None,
                verified: true,
                requires_approval: false,
            },
            idempotent: true,
            cost: CostHint::default(),
            desc_hash: None,
            sdl: None,
            examples: Vec::new(),
        }
    }

    /// Creates an [`OperationDescriptorBuilder`] from separate `branch_slug` and `operation_name` parts.
    #[must_use]
    pub fn for_branch(branch_slug: &str, operation_name: &str) -> Self {
        Self::new(format!("{branch_slug}.{operation_name}"))
    }

    /// Overrides the [`GroupId`] (otherwise derived deterministically from the operation ID).
    #[must_use]
    pub fn group(mut self, group: GroupId) -> Self {
        self.group = Some(group);
        self
    }

    /// Sets [`OpKind`].
    #[must_use]
    pub const fn kind(mut self, kind: OpKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets [`SideEffect`].
    #[must_use]
    pub const fn side_effect(mut self, side_effect: SideEffect) -> Self {
        self.side_effect = side_effect;
        self
    }

    /// Sets `idempotent`.
    #[must_use]
    pub const fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// Sets [`CostHint`].
    #[must_use]
    pub const fn cost(mut self, cost: CostHint) -> Self {
        self.cost = cost;
        self
    }

    /// Overrides the 32-byte `desc_hash`.
    #[must_use]
    pub const fn desc_hash(mut self, desc_hash: [u8; 32]) -> Self {
        self.desc_hash = Some(desc_hash);
        self
    }

    /// Overrides the rendered GraphQL field SDL signature.
    #[must_use]
    pub fn sdl(mut self, sdl: impl Into<Arc<str>>) -> Self {
        self.sdl = Some(sdl.into());
        self
    }

    /// Appends a sanitized [`Example`].
    #[must_use]
    pub fn example(mut self, example: Example) -> Self {
        self.examples.push(example);
        self
    }

    /// Validates identifiers and produces an [`OperationDescriptor`].
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotBuilderError::InvalidId`] if `qualified_id` violates `<branch>.<name>` or
    /// `GQL-004` naming rules.
    pub fn build(self) -> Result<OperationDescriptor, SnapshotBuilderError> {
        let id = OperationId::new(self.qualified_id)?;
        let sdl: Arc<str> = self
            .sdl
            .unwrap_or_else(|| Arc::from(format!("{}: JSON", id.name())));
        let group = self.group.unwrap_or_else(|| {
            GroupId::from_bytes(deterministic_bytes_16("group", sdl.as_bytes()))
        });
        let desc_hash = self.desc_hash.unwrap_or_else(|| {
            let mut material = Vec::with_capacity(id.as_str().len() + 1 + sdl.len());
            material.extend_from_slice(id.as_str().as_bytes());
            material.push(0);
            material.extend_from_slice(sdl.as_bytes());
            deterministic_bytes_32("op-desc", &material)
        });

        Ok(OperationDescriptor {
            id,
            group,
            kind: self.kind,
            side_effect: self.side_effect,
            idempotent: self.idempotent,
            cost: self.cost,
            desc_hash,
            sdl,
            examples: self.examples,
        })
    }

    /// Convenience wrapper returning `Arc<OperationDescriptor>`.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::build`] errors.
    pub fn build_arc(self) -> Result<Arc<OperationDescriptor>, SnapshotBuilderError> {
        self.build().map(Arc::new)
    }
}

/// Fluent builder for [`BranchDescriptor`] with deterministic defaults.
#[derive(Debug, Clone)]
pub struct BranchDescriptorBuilder {
    slug: String,
    uid: Option<BranchUid>,
    protocol: Protocol,
    trust: TrustTier,
    endpoint: Option<Endpoint>,
    spec_hash: Option<[u8; 32]>,
    pinned_desc_hash: Option<[u8; 32]>,
    capabilities: Capabilities,
    limits: BranchLimits,
    synopsis: Option<String>,
    operations: Vec<Arc<OperationDescriptor>>,
    deferred_error: Option<SnapshotBuilderError>,
    status: BranchStatus,
}

impl BranchDescriptorBuilder {
    /// Creates a [`BranchDescriptorBuilder`] for `branch_slug` (e.g. `"billing"`).
    #[must_use]
    pub fn new(branch_slug: impl Into<String>) -> Self {
        Self {
            slug: branch_slug.into(),
            uid: None,
            protocol: Protocol::Grpc,
            trust: TrustTier::T1,
            endpoint: None,
            spec_hash: None,
            pinned_desc_hash: None,
            capabilities: Capabilities {
                foreach_batch: true,
                idempotency: true,
                ..Capabilities::default()
            },
            limits: BranchLimits::default(),
            synopsis: None,
            operations: Vec::new(),
            deferred_error: None,
            status: BranchStatus::Active,
        }
    }

    /// Overrides the [`BranchUid`] (otherwise derived deterministically from `branch_slug`).
    #[must_use]
    pub const fn uid(mut self, uid: BranchUid) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Sets [`Protocol`].
    #[must_use]
    pub const fn protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// Sets [`TrustTier`].
    #[must_use]
    pub const fn trust(mut self, trust: TrustTier) -> Self {
        self.trust = trust;
        self
    }

    /// Overrides [`Endpoint`].
    #[must_use]
    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// Overrides `spec_hash`.
    #[must_use]
    pub const fn spec_hash(mut self, spec_hash: [u8; 32]) -> Self {
        self.spec_hash = Some(spec_hash);
        self
    }

    /// Overrides `pinned_desc_hash`.
    #[must_use]
    pub const fn pinned_desc_hash(mut self, pinned_desc_hash: [u8; 32]) -> Self {
        self.pinned_desc_hash = Some(pinned_desc_hash);
        self
    }

    /// Overrides [`Capabilities`].
    #[must_use]
    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Overrides [`BranchLimits`].
    #[must_use]
    pub const fn limits(mut self, limits: BranchLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Overrides `synopsis`.
    #[must_use]
    pub fn synopsis(mut self, synopsis: impl Into<String>) -> Self {
        self.synopsis = Some(synopsis.into());
        self
    }

    /// Sets [`BranchStatus`].
    #[must_use]
    pub const fn status(mut self, status: BranchStatus) -> Self {
        self.status = status;
        self
    }

    /// Appends a pre-built `Arc<OperationDescriptor>`.
    #[must_use]
    pub fn operation_arc(mut self, op: Arc<OperationDescriptor>) -> Self {
        self.operations.push(op);
        self
    }

    /// Appends an [`OperationDescriptor`].
    #[must_use]
    pub fn operation(self, op: OperationDescriptor) -> Self {
        self.operation_arc(Arc::new(op))
    }

    /// Builds and appends an [`OperationDescriptorBuilder`], deferring any validation error to [`Self::build`].
    #[must_use]
    pub fn with_operation(mut self, builder: OperationDescriptorBuilder) -> Self {
        match builder.build_arc() {
            Ok(op) => self.operations.push(op),
            Err(err) => {
                if self.deferred_error.is_none() {
                    self.deferred_error = Some(err);
                }
            }
        }
        self
    }

    /// Validates the branch slug and operation prefixes and returns a [`BranchDescriptor`].
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotBuilderError`] if the branch slug is invalid, a deferred operation build
    /// failed, or an attached operation belongs to a different branch slug.
    pub fn build(self) -> Result<BranchDescriptor, SnapshotBuilderError> {
        if let Some(err) = self.deferred_error {
            return Err(err);
        }
        let id = BranchId::new(self.slug)?;
        for op in &self.operations {
            if op.id.branch_str() != id.as_str() {
                return Err(SnapshotBuilderError::MismatchedOperationBranch {
                    branch_id: id.as_str().to_owned(),
                    operation_branch: op.id.branch_str().to_owned(),
                    operation_id: op.id.as_str().to_owned(),
                });
            }
        }

        let uid = self.uid.unwrap_or_else(|| {
            let rand_bytes = deterministic_bytes_10("branch-uid", id.as_str().as_bytes());
            BranchUid::from_unix_ms(1_700_000_000_000, rand_bytes)
        });

        let endpoint = self.endpoint.unwrap_or_else(|| Endpoint {
            uri: format!("http://127.0.0.1:50051/{id}"),
            transport: transport_label(self.protocol).to_owned(),
        });

        let spec_hash = self
            .spec_hash
            .unwrap_or_else(|| deterministic_bytes_32("branch-spec", id.as_str().as_bytes()));

        let pinned_desc_hash = self.pinned_desc_hash.unwrap_or_else(|| {
            let mut material = Vec::new();
            material.extend_from_slice(id.as_str().as_bytes());
            for op in &self.operations {
                material.extend_from_slice(&op.desc_hash);
            }
            deterministic_bytes_32("branch-pinned-desc", &material)
        });

        let synopsis = self
            .synopsis
            .unwrap_or_else(|| format!("Test branch `{id}`"));

        Ok(BranchDescriptor {
            uid,
            id,
            protocol: self.protocol,
            trust: self.trust,
            endpoint,
            spec_hash,
            pinned_desc_hash,
            capabilities: self.capabilities,
            limits: self.limits,
            synopsis,
            operations: self.operations,
            status: self.status,
        })
    }

    /// Convenience wrapper returning `Arc<BranchDescriptor>`.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::build`] errors.
    pub fn build_arc(self) -> Result<Arc<BranchDescriptor>, SnapshotBuilderError> {
        self.build().map(Arc::new)
    }
}

/// Fluent builder for [`Snapshot`] with sensible defaults and tenant customization (`INV-09`).
#[derive(Debug, Clone)]
pub struct SnapshotBuilder {
    generation: u64,
    tenant_id: TenantId,
    deferred_error: Option<SnapshotBuilderError>,
    branches: Vec<Arc<BranchDescriptor>>,
    explicit_groups: Vec<Arc<CapabilityGroup>>,
    policy_version: u64,
    calibration: Option<Arc<CalibrationTable>>,
    branch_set_hash: Option<[u8; 32]>,
    built_at: Timestamp,
    built_by: BuildProvenance,
}

impl Default for SnapshotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotBuilder {
    /// Creates a [`SnapshotBuilder`] at generation `1` for `TenantId::default_tenant()`.
    #[must_use]
    pub fn new() -> Self {
        let default_ts =
            Timestamp::from_unix_millis(1_700_000_000_000).unwrap_or(Timestamp::UNIX_EPOCH);
        Self {
            generation: 1,
            tenant_id: TenantId::default_tenant(),
            deferred_error: None,
            branches: Vec::new(),
            explicit_groups: Vec::new(),
            policy_version: 1,
            calibration: None,
            branch_set_hash: None,
            built_at: default_ts,
            built_by: BuildProvenance {
                builder: "gap-testkit".to_owned(),
                proposal_id: None,
                parent_generation: None,
            },
        }
    }

    /// Sets the snapshot `generation`.
    #[must_use]
    pub const fn generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Sets the owning [`TenantId`] (`INV-09`).
    #[must_use]
    pub fn tenant(mut self, tenant_id: TenantId) -> Self {
        self.tenant_id = tenant_id;
        self
    }

    /// Parses and sets the owning tenant from `tenant_slug` (`INV-09`), deferring any slug
    /// validation error to [`Self::build`].
    #[must_use]
    pub fn tenant_slug(mut self, tenant_slug: impl Into<String>) -> Self {
        match TenantId::new(tenant_slug) {
            Ok(tid) => self.tenant_id = tid,
            Err(err) => {
                if self.deferred_error.is_none() {
                    self.deferred_error = Some(SnapshotBuilderError::InvalidId(err));
                }
            }
        }
        self
    }

    /// Appends a pre-built `Arc<BranchDescriptor>` (preserving pointer identity for `ARC-005` tests).
    #[must_use]
    pub fn branch_arc(mut self, branch: Arc<BranchDescriptor>) -> Self {
        self.branches.push(branch);
        self
    }

    /// Appends a [`BranchDescriptor`].
    #[must_use]
    pub fn branch(self, branch: BranchDescriptor) -> Self {
        self.branch_arc(Arc::new(branch))
    }

    /// Builds and appends a [`BranchDescriptorBuilder`], deferring any validation error to [`Self::build`].
    #[must_use]
    pub fn with_branch(mut self, builder: BranchDescriptorBuilder) -> Self {
        match builder.build_arc() {
            Ok(b) => self.branches.push(b),
            Err(err) => {
                if self.deferred_error.is_none() {
                    self.deferred_error = Some(err);
                }
            }
        }
        self
    }

    /// Adds an explicit [`CapabilityGroup`] (in addition to groups synthesized from branch operations).
    #[must_use]
    pub fn capability_group(mut self, group: CapabilityGroup) -> Self {
        self.explicit_groups.push(Arc::new(group));
        self
    }

    /// Sets `policy_version`.
    #[must_use]
    pub const fn policy_version(mut self, policy_version: u64) -> Self {
        self.policy_version = policy_version;
        self
    }

    /// Attaches conformal [`CalibrationTable`] parameters.
    #[must_use]
    pub fn calibration(mut self, calibration: CalibrationTable) -> Self {
        self.calibration = Some(Arc::new(calibration));
        self
    }

    /// Overrides `branch_set_hash`.
    #[must_use]
    pub const fn branch_set_hash(mut self, branch_set_hash: [u8; 32]) -> Self {
        self.branch_set_hash = Some(branch_set_hash);
        self
    }

    /// Sets `built_at`.
    #[must_use]
    pub const fn built_at(mut self, built_at: Timestamp) -> Self {
        self.built_at = built_at;
        self
    }

    /// Sets [`BuildProvenance`] (including optional `ProposalId` and `parent_generation`).
    #[must_use]
    pub fn provenance(
        mut self,
        builder: impl Into<String>,
        proposal_id: Option<ProposalId>,
        parent_generation: Option<u64>,
    ) -> Self {
        self.built_by = BuildProvenance {
            builder: builder.into(),
            proposal_id,
            parent_generation,
        };
        self
    }

    /// Builds a validated [`Snapshot`] with deterministic `branches`, `groups`, and `branch_set_hash`.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotBuilderError`] if any deferred identifier check failed or if duplicate
    /// branch UIDs/slugs were supplied.
    pub fn build(self) -> Result<Snapshot, SnapshotBuilderError> {
        if let Some(err) = self.deferred_error {
            return Err(err);
        }

        let mut branches_map: BTreeMap<BranchUid, Arc<BranchDescriptor>> = BTreeMap::new();
        let mut seen_slugs: std::collections::BTreeSet<BranchId> =
            std::collections::BTreeSet::new();
        let mut synthesized_groups: BTreeMap<GroupId, CapabilityGroup> = BTreeMap::new();

        for branch in self.branches {
            if branches_map.contains_key(&branch.uid) {
                return Err(SnapshotBuilderError::DuplicateBranchUid(branch.uid));
            }
            if !seen_slugs.insert(branch.id.clone()) {
                return Err(SnapshotBuilderError::DuplicateBranchId(branch.id.clone()));
            }
            for op in &branch.operations {
                let entry = synthesized_groups
                    .entry(op.group.clone())
                    .or_insert_with(|| CapabilityGroup {
                        id: op.group.clone(),
                        signature_sdl: Arc::clone(&op.sdl),
                        members: Vec::new(),
                    });
                entry.members.push((branch.uid, op.id.clone()));
            }
            branches_map.insert(branch.uid, branch);
        }

        let mut groups_map: BTreeMap<GroupId, Arc<CapabilityGroup>> = synthesized_groups
            .into_iter()
            .map(|(gid, grp)| (gid, Arc::new(grp)))
            .collect();
        for explicit in self.explicit_groups {
            groups_map.insert(explicit.id.clone(), explicit);
        }

        let branch_set_hash = self.branch_set_hash.unwrap_or_else(|| {
            if branches_map.is_empty() {
                [0_u8; 32]
            } else {
                let mut material = Vec::new();
                for (uid, branch) in &branches_map {
                    material.extend_from_slice(uid.as_uuid().as_bytes());
                    material.extend_from_slice(&branch.pinned_desc_hash);
                }
                deterministic_bytes_32("branch-set", &material)
            }
        });

        let mut snap = Snapshot::empty_for_tenant(self.generation, self.tenant_id);
        snap.branches = Arc::new(branches_map);
        snap.groups = Arc::new(groups_map);
        snap.policy_version = self.policy_version;
        snap.calibration = self.calibration;
        snap.branch_set_hash = branch_set_hash;
        snap.built_at = self.built_at;
        snap.built_by = self.built_by;
        Ok(snap)
    }
}

const fn transport_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Grpc | Protocol::Sql | Protocol::CodeGraph | Protocol::Custom => "grpc",
        Protocol::Rest | Protocol::Graphql | Protocol::Mcp | Protocol::A2a => "http",
        Protocol::Cli => "stdio",
    }
}

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn lane_digest(seed: u64, domain: &str, bytes: &[u8]) -> u64 {
    let mut acc = seed ^ 0xcbf2_9ce4_8422_2325;
    for &b in domain.as_bytes() {
        acc ^= u64::from(b);
        acc = acc.wrapping_mul(0x0100_0000_01b3);
    }
    acc ^= 0xff;
    acc = acc.wrapping_mul(0x0100_0000_01b3);
    for &b in bytes {
        acc ^= u64::from(b);
        acc = acc.wrapping_mul(0x0100_0000_01b3);
    }
    mix64(acc)
}

fn deterministic_bytes_32(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (lane_idx, chunk) in out.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        let word = lane_digest((lane_idx as u64).wrapping_add(1), domain, bytes);
        *chunk = word.to_le_bytes();
    }
    out
}

fn deterministic_bytes_16(domain: &str, bytes: &[u8]) -> [u8; 16] {
    let full = deterministic_bytes_32(domain, bytes);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

fn deterministic_bytes_10(domain: &str, bytes: &[u8]) -> [u8; 10] {
    let full = deterministic_bytes_32(domain, bytes);
    let mut out = [0_u8; 10];
    out.copy_from_slice(&full[..10]);
    out
}

#[cfg(test)]
mod tests {
    use gap_core::snapshot::PlanCacheKey;

    use super::*;

    // spec: ARC-005, INV-09
    #[test]
    fn inv_09_snapshot_builder_populates_defaults_tenant_option_and_capability_groups() {
        let default_snap = SnapshotBuilder::new()
            .with_branch(
                BranchDescriptorBuilder::new("billing").with_operation(
                    OperationDescriptorBuilder::for_branch("billing", "getInvoice")
                        .sdl("getInvoice(id: ID!): Invoice"),
                ),
            )
            .build()
            .expect("build default-tenant snapshot");

        assert_eq!(default_snap.generation, 1);
        assert_eq!(default_snap.tenant_id, TenantId::default_tenant());
        assert_eq!(default_snap.branches.len(), 1);
        assert_eq!(default_snap.groups.len(), 1);
        assert_ne!(default_snap.branch_set_hash, [0_u8; 32]);

        let tenant_snap = SnapshotBuilder::new()
            .generation(4)
            .tenant_slug("acme-corp")
            .policy_version(7)
            .with_branch(
                BranchDescriptorBuilder::new("billing").with_operation(
                    OperationDescriptorBuilder::for_branch("billing", "getInvoice")
                        .sdl("getInvoice(id: ID!): Invoice"),
                ),
            )
            .build()
            .expect("build acme-corp snapshot");

        assert_eq!(tenant_snap.generation, 4);
        assert_eq!(tenant_snap.tenant_id.as_str(), "acme-corp");
        assert_eq!(tenant_snap.policy_version, 7);

        // Tenant isolation on PlanCacheKey (`INV-09`).
        let k_default = PlanCacheKey::from_snapshot(&default_snap, [5_u8; 32], None);
        let k_acme = PlanCacheKey::from_snapshot(&tenant_snap, [5_u8; 32], None);
        assert_ne!(k_default.tenant, k_acme.tenant);
    }

    // spec: ARC-005
    #[test]
    fn snapshot_builder_preserves_arc_sharing_and_rejects_invalid_or_mismatched_inputs() {
        let shared_branch = BranchDescriptorBuilder::new("crm")
            .with_operation(OperationDescriptorBuilder::new("crm.findContact"))
            .build_arc()
            .expect("build shared branch");

        let g1 = SnapshotBuilder::new()
            .generation(1)
            .branch_arc(Arc::clone(&shared_branch))
            .build()
            .expect("g1");
        let g2 = SnapshotBuilder::new()
            .generation(2)
            .branch_arc(Arc::clone(&shared_branch))
            .build()
            .expect("g2");

        assert!(Arc::ptr_eq(
            &g1.branches[&shared_branch.uid],
            &g2.branches[&shared_branch.uid]
        ));

        // Invalid tenant slug must fail.
        let bad_tenant = SnapshotBuilder::new().tenant_slug("BAD_UPPERCASE").build();
        assert!(matches!(
            bad_tenant,
            Err(SnapshotBuilderError::InvalidId(_))
        ));

        // Operation belonging to `"crm"` attached to `"billing"` must fail.
        let crm_op = OperationDescriptorBuilder::new("crm.findContact")
            .build()
            .expect("crm op");
        let mismatch = BranchDescriptorBuilder::new("billing")
            .operation(crm_op)
            .build();
        assert!(matches!(
            mismatch,
            Err(SnapshotBuilderError::MismatchedOperationBranch { .. })
        ));

        // Duplicate branch slug in the same snapshot must fail.
        let b1 = BranchDescriptorBuilder::new("billing")
            .uid(BranchUid::from_unix_ms(1, [1; 10]))
            .build()
            .expect("b1");
        let b2 = BranchDescriptorBuilder::new("billing")
            .uid(BranchUid::from_unix_ms(2, [2; 10]))
            .build()
            .expect("b2");
        let dup_slug = SnapshotBuilder::new().branch(b1).branch(b2).build();
        assert!(matches!(
            dup_slug,
            Err(SnapshotBuilderError::DuplicateBranchId(_))
        ));

        // Duplicate BranchUid in the same snapshot must fail.
        let same_uid = BranchUid::from_unix_ms(9, [9; 10]);
        let u1 = BranchDescriptorBuilder::new("billing")
            .uid(same_uid)
            .build()
            .expect("u1");
        let u2 = BranchDescriptorBuilder::new("shipping")
            .uid(same_uid)
            .build()
            .expect("u2");
        let dup_uid = SnapshotBuilder::new().branch(u1).branch(u2).build();
        assert!(matches!(
            dup_uid,
            Err(SnapshotBuilderError::DuplicateBranchUid(_))
        ));
    }
}
