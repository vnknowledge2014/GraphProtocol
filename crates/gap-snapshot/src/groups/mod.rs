//! Snapshot-time capability group assembly, canonical signature validation,
//! unverified isolation, and deduplicated text union (`ADR-006`, `RTE-030`,
//! `RUN-011`, `WP-M2-08`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use apollo_compiler::ast::{self, Definition};
use gap_core::error::GapResult;
use gap_core::ids::{BranchId, BranchUid, GroupId, OperationId, RESERVED_BUILTIN_TYPES, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchStatus, CapabilityGroup, OpKind, OperationDescriptor, SideEffectLevel,
    Snapshot, TrustTier,
};
use gap_core::traits::ElementKind;
use gap_index::{ElementDoc, HybridElementIndex, HybridIndexConfig};
use gap_ql::schema::parse_sdl_ast;
use thiserror::Error;

const STANDARD_GRAPHQL_SCALARS: &[&str] = &["Int", "Float", "String", "Boolean", "ID"];

/// Explicit operation-level `replica_of` equivalence assertion (`ADR-006`, `WP-M2-08`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaDeclaration {
    /// Replica operation being mapped onto `canonical_op`.
    pub replica_op: OperationId,
    /// Canonical primary operation representing the capability group.
    pub canonical_op: OperationId,
    /// Only `verified == true` declarations permit merging; `false` forces separate groups (`ADR-006`).
    pub verified: bool,
}

/// Branch-wide `replica_of` declaration pairing operations with identical normalized names (`ADR-006`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchReplicaDeclaration {
    /// Replica branch slug.
    pub replica_branch: BranchId,
    /// Canonical target branch slug.
    pub canonical_branch: BranchId,
    /// Only `verified == true` declarations permit merging; `false` forces separate groups.
    pub verified: bool,
}

/// Ingestion/admin replica declarations supplied during snapshot group assembly (`WP-M2-08`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupAssignmentConfig {
    /// Explicit operation-to-operation replica declarations.
    pub op_replicas: Vec<ReplicaDeclaration>,
    /// Branch-level replica declarations pairing same-named operations across two branches.
    pub branch_replicas: Vec<BranchReplicaDeclaration>,
}

impl GroupAssignmentConfig {
    /// Creates an empty group assignment configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares a verified interchangeable replica between `replica_op` and `canonical_op`.
    #[must_use]
    pub fn with_verified_op_replica(
        mut self,
        replica_op: OperationId,
        canonical_op: OperationId,
    ) -> Self {
        self.op_replicas.push(ReplicaDeclaration {
            replica_op,
            canonical_op,
            verified: true,
        });
        self
    }

    /// Records an unverified candidate replica pair (kept isolated in separate groups per `ADR-006`).
    #[must_use]
    pub fn with_unverified_op_replica(
        mut self,
        replica_op: OperationId,
        canonical_op: OperationId,
    ) -> Self {
        self.op_replicas.push(ReplicaDeclaration {
            replica_op,
            canonical_op,
            verified: false,
        });
        self
    }

    /// Declares a verified branch-level replica mapping between `replica_branch` and `canonical_branch`.
    #[must_use]
    pub fn with_verified_branch_replica(
        mut self,
        replica_branch: BranchId,
        canonical_branch: BranchId,
    ) -> Self {
        self.branch_replicas.push(BranchReplicaDeclaration {
            replica_branch,
            canonical_branch,
            verified: true,
        });
        self
    }

    /// Records an unverified branch-level candidate replica pair (kept in separate groups).
    #[must_use]
    pub fn with_unverified_branch_replica(
        mut self,
        replica_branch: BranchId,
        canonical_branch: BranchId,
    ) -> Self {
        self.branch_replicas.push(BranchReplicaDeclaration {
            replica_branch,
            canonical_branch,
            verified: false,
        });
        self
    }
}

/// Deterministic error raised when replica declarations violate contract equivalence (`ADR-006`,
/// `RUN-011`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GroupAssemblyError {
    /// Referenced operation ID does not exist in the snapshot branch inventory.
    #[error("replica declaration references unknown operation `{0}`")]
    UnknownOperation(OperationId),
    /// Referenced branch slug does not exist in the snapshot branch inventory.
    #[error("branch replica declaration references unknown branch `{0}`")]
    UnknownBranch(BranchId),
    /// Verified `replica_of` graph contains a cycle.
    #[error("replica_of declaration forms a cycle involving `{0}`")]
    ReplicaCycle(OperationId),
    /// Replicas declared as interchangeable do not share the same normalized input/output signature.
    #[error(
        "divergent signature between replica `{replica}` (`{replica_sig}`) and canonical `{canonical}` (`{canonical_sig}`)"
    )]
    SignatureMismatch {
        /// Replica operation identifier.
        replica: OperationId,
        /// Canonical operation identifier.
        canonical: OperationId,
        /// Normalized signature of `replica`.
        replica_sig: String,
        /// Normalized signature of `canonical`.
        canonical_sig: String,
    },
    /// Replicas declared as interchangeable differ on operation kind or effective side effect (`GQL-032`).
    #[error("incompatible metadata between `{replica}` and `{canonical}`: {reason}")]
    IncompatibleMetadata {
        /// Replica operation identifier.
        replica: OperationId,
        /// Canonical operation identifier.
        canonical: OperationId,
        /// Human-readable reason.
        reason: String,
    },
    /// Multiple non-disabled branches define the exact same composed GraphQL root coordinate (`RUN-011`).
    #[error("ambiguous root field ownership on `{coordinate}` across branches: {branches:?}")]
    AmbiguousFieldOwnership {
        /// Root field coordinate (`<RootType>.<field>`).
        coordinate: String,
        /// Sorted list of conflicting branches.
        branches: Vec<BranchId>,
    },
}

/// Assembled capability group metadata produced by [`assign_capability_groups`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledGroup {
    /// Stable 128-bit `blake3` group identifier (`compute_stable_group_id`).
    pub id: GroupId,
    /// Canonical primary operation identifier anchoring `id`.
    pub canonical_op: OperationId,
    /// Canonical normalized signature shared by every member of this group.
    pub canonical_signature: String,
    /// Sorted interchangeable members `(branch_uid, operation_id)`.
    pub members: Vec<(BranchUid, OperationId)>,
    /// Deterministic deduplicated union of descriptions and examples across members (`ADR-006`).
    pub union_text: String,
    /// Deduplicated positive intent examples across members.
    pub union_examples: Vec<String>,
}

/// Complete capability group assembly output for a set of branches (`ADR-006`, `WP-M2-08`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledGroups {
    /// Assembled groups keyed by [`GroupId`].
    pub groups: BTreeMap<GroupId, AssembledGroup>,
    /// Operation-to-group lookup table covering every operation in the input branches.
    pub op_to_group: BTreeMap<OperationId, GroupId>,
    /// Unverified replica declarations intentionally isolated into separate groups (`ADR-006`).
    pub isolated_unverified: Vec<(OperationId, OperationId)>,
}

impl AssembledGroups {
    /// Builds one deduplicated [`ElementDoc`] per [`AssembledGroup`] (plus deduplicated example docs)
    /// so adding healthy replicas never dilutes BM25 IDF or crowds RRF ranks (`ADR-006`, `RTE-030`).
    #[must_use]
    pub fn build_group_element_docs(
        &self,
        branches: &[Arc<BranchDescriptor>],
        tenant: TenantId,
    ) -> Vec<ElementDoc> {
        let mut by_op: BTreeMap<&str, (&BranchDescriptor, &OperationDescriptor)> = BTreeMap::new();
        for branch in branches {
            for op in &branch.operations {
                by_op.insert(op.id.as_str(), (branch.as_ref(), op.as_ref()));
            }
        }

        let mut docs = Vec::with_capacity(self.groups.len() * 2);
        for group in self.groups.values() {
            let Some((root_branch, root_op)) = by_op.get(group.canonical_op.as_str()).copied()
            else {
                continue;
            };
            let path = format!("{}.{}", root_name_for_kind(root_op.kind), root_op.id.name());
            let mut op_doc = ElementDoc::new(
                group.id.as_str(),
                ElementKind::Operation,
                root_op.id.name().to_owned(),
            )
            .with_tenant(tenant.clone())
            .with_branch_uid(root_branch.uid)
            .with_operation_id(root_op.id.clone())
            .with_path(&path)
            .with_text(group.union_text.clone());

            if !group.union_examples.is_empty() {
                op_doc = op_doc.with_examples(group.union_examples.clone());
            }
            docs.push(op_doc);
        }
        docs
    }

    /// Rewrites [`Snapshot.groups`], operation [`GroupId`]s inside `branches`, and rebuilds
    /// [`Snapshot.index`] using group-level [`ElementDoc`] entries (`ADR-006`).
    ///
    /// # Errors
    ///
    /// Propagates any hybrid index build error from [`HybridElementIndex::build`].
    pub fn apply_to_snapshot(
        &self,
        snapshot: &Snapshot,
        tenant: TenantId,
        index_config: HybridIndexConfig,
    ) -> GapResult<Snapshot> {
        let mut next = snapshot.clone();
        let mut updated_branches = im::OrdMap::new();
        let mut branch_list: Vec<Arc<BranchDescriptor>> = Vec::with_capacity(next.branches.len());

        for (&uid, branch_arc) in &next.branches {
            let mut branch_cloned = (**branch_arc).clone();
            let mut ops_changed = false;
            for op_arc in &mut branch_cloned.operations {
                if let Some(new_gid) = self.op_to_group.get(&op_arc.id)
                    && &op_arc.group != new_gid
                {
                    let mut op_cloned = (**op_arc).clone();
                    op_cloned.group = new_gid.clone();
                    *op_arc = Arc::new(op_cloned);
                    ops_changed = true;
                }
            }
            let final_arc = if ops_changed {
                Arc::new(branch_cloned)
            } else {
                Arc::clone(branch_arc)
            };
            branch_list.push(Arc::clone(&final_arc));
            updated_branches.insert(uid, final_arc);
        }

        next.branches = updated_branches;
        next.groups.clear();

        let mut sdl_by_op: BTreeMap<String, Arc<str>> = BTreeMap::new();
        for branch in &branch_list {
            for op in &branch.operations {
                sdl_by_op.insert(op.id.as_str().to_owned(), Arc::clone(&op.sdl));
            }
        }

        for (gid, grp) in &self.groups {
            let sig_sdl = sdl_by_op
                .get(grp.canonical_op.as_str())
                .cloned()
                .unwrap_or_else(|| Arc::from(grp.canonical_signature.as_str()));
            next.groups.insert(
                gid.clone(),
                Arc::new(CapabilityGroup {
                    id: gid.clone(),
                    signature_sdl: sig_sdl,
                    members: grp.members.clone(),
                }),
            );
        }

        let docs = self.build_group_element_docs(&branch_list, tenant);
        let hybrid = HybridElementIndex::build(docs, index_config)?;
        next.index = Arc::new(hybrid);
        Ok(next)
    }
}

/// Effective side-effect classification enforcing `TrustTier::T2` -> `UNKNOWN` (`GQL-032`).
#[must_use]
pub const fn effective_side_effect(trust: TrustTier, declared: SideEffectLevel) -> SideEffectLevel {
    if matches!(trust, TrustTier::T2) {
        SideEffectLevel::Unknown
    } else {
        declared
    }
}

/// Checks that no two non-disabled branches define the exact same unnamespaced composed GraphQL
/// root coordinate (`Query.<name>`, `Mutation.<name>`, `Subscription.<name>`) (`RUN-011`).
///
/// # Errors
///
/// Returns [`GroupAssemblyError::AmbiguousFieldOwnership`] listing every conflicting branch slug.
pub fn validate_unambiguous_root_ownership(
    branches: &[Arc<BranchDescriptor>],
) -> Result<(), GroupAssemblyError> {
    let mut owners: BTreeMap<String, BTreeSet<BranchId>> = BTreeMap::new();
    for branch in branches {
        if branch.status == BranchStatus::Disabled {
            continue;
        }
        for op in &branch.operations {
            let coord = format!("{}.{}", root_name_for_kind(op.kind), op.id.name());
            owners.entry(coord).or_default().insert(branch.id.clone());
        }
    }

    for (coordinate, branch_set) in owners {
        if branch_set.len() > 1 {
            return Err(GroupAssemblyError::AmbiguousFieldOwnership {
                coordinate,
                branches: branch_set.into_iter().collect(),
            });
        }
    }
    Ok(())
}

/// Computes the normalized canonical signature string for `op` on `branch` (`ADR-006`, `WP-M2-08`):
/// strips `<ns>_` branch namespace prefixes from the field name and non-builtin types, sorts named
/// GraphQL arguments lexicographically, and binds `(OpKind, effective_side_effect)`.
#[must_use]
pub fn canonical_operation_signature(
    branch: &BranchDescriptor,
    op: &OperationDescriptor,
) -> String {
    let ns_prefix = format!("{}_", branch.id.as_str().replace('-', "_"));
    let eff_se = effective_side_effect(branch.trust, op.side_effect.level);
    let kind_tag = op_kind_tag(op.kind);
    let se_tag = side_effect_tag(eff_se);

    let trimmed_sdl = op.sdl.trim();
    let doc_src = format!("type Query {{ {trimmed_sdl} }}");
    if let Ok(ast_doc) = parse_sdl_ast(&doc_src, "op_signature.graphql") {
        for def in &ast_doc.definitions {
            if let Definition::ObjectTypeDefinition(obj) = def
                && let Some(field) = obj.fields.first()
            {
                let norm_name = strip_ns_prefix(field.name.as_str(), &ns_prefix);
                let mut args: Vec<(String, String, String)> = field
                    .arguments
                    .iter()
                    .map(|arg| {
                        let a_name = arg.name.as_str().to_owned();
                        let a_ty = normalize_ast_type(&arg.ty, &ns_prefix);
                        let a_def = arg
                            .default_value
                            .as_ref()
                            .map(|v| collapse_ws(&v.serialize().to_string()))
                            .unwrap_or_default();
                        (a_name, a_ty, a_def)
                    })
                    .collect();
                args.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                let args_rendered = if args.is_empty() {
                    String::new()
                } else {
                    let inner: Vec<String> = args
                        .into_iter()
                        .map(|(n, t, d)| {
                            if d.is_empty() {
                                format!("{n}:{t}")
                            } else {
                                format!("{n}:{t}={d}")
                            }
                        })
                        .collect();
                    format!("({})", inner.join(","))
                };
                let ret_ty = normalize_ast_type(&field.ty, &ns_prefix);
                return format!("{kind_tag}:{se_tag}:{norm_name}{args_rendered}:{ret_ty}");
            }
        }
    }

    let fallback_name = strip_ns_prefix(op.id.name(), &ns_prefix);
    let collapsed = collapse_ws(trimmed_sdl);
    format!("{kind_tag}:{se_tag}:{fallback_name}:{collapsed}")
}

/// Derives a deterministic, replica-count-invariant 128-bit `GroupId` anchored to `canonical_op`
/// and its normalized `canonical_signature` (`ADR-006`).
#[must_use]
pub fn compute_stable_group_id(canonical_op: &OperationId, canonical_signature: &str) -> GroupId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gap.group.v1\0");
    hasher.update(canonical_op.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_signature.as_bytes());
    let digest = hasher.finalize();
    let mut bytes16 = [0u8; 16];
    bytes16.copy_from_slice(&digest.as_bytes()[..16]);
    GroupId::from_bytes(bytes16)
}

/// Computes the deterministic, deduplicated union of descriptions and examples across capability
/// group members (`ADR-006`, `WP-M2-08`).
#[must_use]
pub fn union_group_text(
    members: &[(&BranchDescriptor, &OperationDescriptor)],
    canonical_op: &OperationId,
) -> (String, Vec<String>) {
    let mut ordered: Vec<(&BranchDescriptor, &OperationDescriptor)> = members.to_vec();
    ordered.sort_by(|a, b| {
        let a_root = &a.1.id == canonical_op;
        let b_root = &b.1.id == canonical_op;
        b_root
            .cmp(&a_root)
            .then_with(|| a.1.id.as_str().cmp(b.1.id.as_str()))
            .then_with(|| a.0.uid.cmp(&b.0.uid))
    });

    let mut seen_phrases: BTreeSet<String> = BTreeSet::new();
    let mut phrases: Vec<String> = Vec::new();
    let mut seen_examples: BTreeSet<String> = BTreeSet::new();
    let mut examples: Vec<String> = Vec::new();

    for (branch, op) in ordered {
        let humanized = humanize_identifier(op.id.name());
        if !humanized.is_empty() && seen_phrases.insert(humanized.clone()) {
            phrases.push(humanized);
        }
        let synopsis = collapse_ws(branch.synopsis.trim());
        if !synopsis.is_empty() && seen_phrases.insert(synopsis.clone()) {
            phrases.push(synopsis);
        }
        for ex in &op.examples {
            if !ex.positive {
                continue;
            }
            let clean_ex = collapse_ws(ex.intent.trim());
            if !clean_ex.is_empty() && seen_examples.insert(clean_ex.clone()) {
                examples.push(clean_ex.clone());
                if seen_phrases.insert(clean_ex.clone()) {
                    phrases.push(clean_ex);
                }
            }
        }
    }

    (phrases.join(" | "), examples)
}

/// Assigns capability groups across `branches` from verified `replica_of` declarations (`config`),
/// keeping unverified candidates isolated in distinct solo groups (`ADR-006`, `RTE-030`).
///
/// # Errors
///
/// Returns [`GroupAssemblyError`] on unknown operations/branches, verified cycles, divergent
/// normalized signatures, or mismatched `(OpKind, effective_side_effect)`.
pub fn assign_capability_groups(
    branches: &[Arc<BranchDescriptor>],
    config: &GroupAssignmentConfig,
) -> Result<AssembledGroups, GroupAssemblyError> {
    let mut by_branch_slug: BTreeMap<BranchId, Arc<BranchDescriptor>> = BTreeMap::new();
    let mut by_op: BTreeMap<OperationId, (Arc<BranchDescriptor>, Arc<OperationDescriptor>)> =
        BTreeMap::new();

    for branch in branches {
        by_branch_slug.insert(branch.id.clone(), Arc::clone(branch));
        for op in &branch.operations {
            by_op.insert(op.id.clone(), (Arc::clone(branch), Arc::clone(op)));
        }
    }

    let mut all_decls: Vec<ReplicaDeclaration> = config.op_replicas.clone();

    // Expand branch-level replica declarations by normalized operation name.
    for b_decl in &config.branch_replicas {
        let Some(rep_br) = by_branch_slug.get(&b_decl.replica_branch) else {
            return Err(GroupAssemblyError::UnknownBranch(
                b_decl.replica_branch.clone(),
            ));
        };
        let Some(can_br) = by_branch_slug.get(&b_decl.canonical_branch) else {
            return Err(GroupAssemblyError::UnknownBranch(
                b_decl.canonical_branch.clone(),
            ));
        };

        let rep_ns = format!("{}_", rep_br.id.as_str().replace('-', "_"));
        let can_ns = format!("{}_", can_br.id.as_str().replace('-', "_"));

        let mut can_by_norm: BTreeMap<(OpKind, String), &OperationDescriptor> = BTreeMap::new();
        for op in &can_br.operations {
            let norm = strip_ns_prefix(op.id.name(), &can_ns);
            can_by_norm.insert((op.kind, norm), op.as_ref());
        }

        for rep_op in &rep_br.operations {
            let norm = strip_ns_prefix(rep_op.id.name(), &rep_ns);
            if let Some(can_op) = can_by_norm.get(&(rep_op.kind, norm)) {
                all_decls.push(ReplicaDeclaration {
                    replica_op: rep_op.id.clone(),
                    canonical_op: can_op.id.clone(),
                    verified: b_decl.verified,
                });
            }
        }
    }

    // Separate verified links from unverified links (unverified stay solo per ADR-006).
    let mut verified_parent: BTreeMap<OperationId, OperationId> = BTreeMap::new();
    let mut isolated_unverified: Vec<(OperationId, OperationId)> = Vec::new();

    for decl in all_decls {
        if !by_op.contains_key(&decl.replica_op) {
            return Err(GroupAssemblyError::UnknownOperation(decl.replica_op));
        }
        if !by_op.contains_key(&decl.canonical_op) {
            return Err(GroupAssemblyError::UnknownOperation(decl.canonical_op));
        }
        if !decl.verified {
            isolated_unverified.push((decl.replica_op, decl.canonical_op));
            continue;
        }
        if decl.replica_op != decl.canonical_op {
            verified_parent.insert(decl.replica_op, decl.canonical_op);
        }
    }

    // Compute canonical signature per operation and verify contract equivalence for all verified edges.
    let mut sig_by_op: BTreeMap<OperationId, String> = BTreeMap::new();
    for (op_id, (branch, op)) in &by_op {
        sig_by_op.insert(op_id.clone(), canonical_operation_signature(branch, op));
    }

    // Resolve each operation to its acyclic canonical root, validating equivalence along the link.
    let mut root_by_op: BTreeMap<OperationId, OperationId> = BTreeMap::new();
    for op_id in by_op.keys() {
        let mut cursor = op_id.clone();
        let mut visited: BTreeSet<OperationId> = BTreeSet::new();
        visited.insert(cursor.clone());

        while let Some(next_id) = verified_parent.get(&cursor) {
            if !visited.insert(next_id.clone()) {
                return Err(GroupAssemblyError::ReplicaCycle(op_id.clone()));
            }
            let (rep_b, rep_o) = &by_op[&cursor];
            let (can_b, can_o) = &by_op[next_id];

            if rep_o.kind != can_o.kind {
                return Err(GroupAssemblyError::IncompatibleMetadata {
                    replica: cursor,
                    canonical: next_id.clone(),
                    reason: format!(
                        "operation kind differs ({:?} vs {:?})",
                        rep_o.kind, can_o.kind
                    ),
                });
            }

            let rep_se = effective_side_effect(rep_b.trust, rep_o.side_effect.level);
            let can_se = effective_side_effect(can_b.trust, can_o.side_effect.level);
            if rep_se != can_se {
                return Err(GroupAssemblyError::IncompatibleMetadata {
                    replica: cursor,
                    canonical: next_id.clone(),
                    reason: format!("effective side effect differs ({rep_se:?} vs {can_se:?})"),
                });
            }

            let rep_sig = &sig_by_op[&cursor];
            let can_sig = &sig_by_op[next_id];
            if rep_sig != can_sig {
                return Err(GroupAssemblyError::SignatureMismatch {
                    replica: cursor,
                    canonical: next_id.clone(),
                    replica_sig: rep_sig.clone(),
                    canonical_sig: can_sig.clone(),
                });
            }

            cursor = next_id.clone();
        }
        root_by_op.insert(op_id.clone(), cursor);
    }

    // Group operations by resolved canonical root operation.
    let mut members_by_root: BTreeMap<OperationId, Vec<(BranchUid, OperationId)>> = BTreeMap::new();
    for (op_id, root_id) in &root_by_op {
        let (branch, _) = &by_op[op_id];
        members_by_root
            .entry(root_id.clone())
            .or_default()
            .push((branch.uid, op_id.clone()));
    }

    let mut groups: BTreeMap<GroupId, AssembledGroup> = BTreeMap::new();
    let mut op_to_group: BTreeMap<OperationId, GroupId> = BTreeMap::new();

    for (root_op, mut member_keys) in members_by_root {
        member_keys.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()).then_with(|| a.0.cmp(&b.0)));

        let can_sig = sig_by_op.get(&root_op).cloned().unwrap_or_default();
        let gid = compute_stable_group_id(&root_op, &can_sig);

        let mut member_refs: Vec<(&BranchDescriptor, &OperationDescriptor)> =
            Vec::with_capacity(member_keys.len());
        for (_, op_id) in &member_keys {
            op_to_group.insert(op_id.clone(), gid.clone());
            if let Some((b, o)) = by_op.get(op_id) {
                member_refs.push((b.as_ref(), o.as_ref()));
            }
        }

        let (union_text, union_examples) = union_group_text(&member_refs, &root_op);
        groups.insert(
            gid.clone(),
            AssembledGroup {
                id: gid,
                canonical_op: root_op,
                canonical_signature: can_sig,
                members: member_keys,
                union_text,
                union_examples,
            },
        );
    }

    isolated_unverified.sort();

    Ok(AssembledGroups {
        groups,
        op_to_group,
        isolated_unverified,
    })
}

fn strip_ns_prefix(name: &str, ns_prefix: &str) -> String {
    if !ns_prefix.is_empty() && name.starts_with(ns_prefix) && name.len() > ns_prefix.len() {
        name[ns_prefix.len()..].to_owned()
    } else {
        name.to_owned()
    }
}

fn is_shared_builtin_type(name: &str) -> bool {
    STANDARD_GRAPHQL_SCALARS.contains(&name) || RESERVED_BUILTIN_TYPES.contains(&name)
}

fn normalize_ast_type(ty: &ast::Type, ns_prefix: &str) -> String {
    match ty {
        ast::Type::Named(name) => {
            let raw = name.as_str();
            if is_shared_builtin_type(raw) {
                raw.to_owned()
            } else {
                strip_ns_prefix(raw, ns_prefix)
            }
        }
        ast::Type::NonNullNamed(name) => {
            let raw = name.as_str();
            let base = if is_shared_builtin_type(raw) {
                raw.to_owned()
            } else {
                strip_ns_prefix(raw, ns_prefix)
            };
            format!("{base}!")
        }
        ast::Type::List(inner) => format!("[{}]", normalize_ast_type(inner, ns_prefix)),
        ast::Type::NonNullList(inner) => format!("[{}]!", normalize_ast_type(inner, ns_prefix)),
    }
}

fn collapse_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn humanize_identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for (idx, ch) in name.chars().enumerate() {
        if ch == '_' {
            out.push(' ');
        } else if ch.is_ascii_uppercase() && idx > 0 {
            out.push(' ');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    collapse_ws(&out)
}

const fn root_name_for_kind(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Query => "Query",
        OpKind::Mutation => "Mutation",
        OpKind::Subscription => "Subscription",
        OpKind::Task => "GapMutation",
    }
}

const fn op_kind_tag(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Query => "QUERY",
        OpKind::Mutation => "MUTATION",
        OpKind::Subscription => "SUBSCRIPTION",
        OpKind::Task => "TASK",
    }
}

const fn side_effect_tag(level: SideEffectLevel) -> &'static str {
    match level {
        SideEffectLevel::None => "NONE",
        SideEffectLevel::Reversible => "REVERSIBLE",
        SideEffectLevel::Irreversible => "IRREVERSIBLE",
        SideEffectLevel::Unknown => "UNKNOWN",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use gap_core::snapshot::{
        BranchLimits, Capabilities, CostHint, Endpoint, Example, Protocol, SideEffect,
    };
    use proptest::prelude::*;

    fn mk_op(
        branch: &BranchId,
        name: &str,
        kind: OpKind,
        se: SideEffectLevel,
        sdl: &str,
        examples: &[&str],
    ) -> Arc<OperationDescriptor> {
        let id = OperationId::from_parts(branch, name).expect("valid op id");
        Arc::new(OperationDescriptor {
            id,
            group: GroupId::from_bytes([0u8; 16]),
            kind,
            side_effect: SideEffect {
                level: se,
                verified: true,
                requires_approval: false,
            },
            idempotent: kind == OpKind::Query,
            cost: CostHint::default(),
            desc_hash: [1u8; 32],
            sdl: Arc::from(sdl),
            examples: examples
                .iter()
                .map(|ex| Example {
                    intent: (*ex).to_owned(),
                    positive: true,
                    query_snippet: None,
                })
                .collect(),
        })
    }

    fn mk_branch(
        slug: &str,
        trust: TrustTier,
        status: BranchStatus,
        synopsis: &str,
        ops: Vec<Arc<OperationDescriptor>>,
    ) -> Arc<BranchDescriptor> {
        Arc::new(BranchDescriptor {
            uid: BranchUid::new_v7(),
            id: BranchId::new(slug).expect("valid branch id"),
            protocol: Protocol::Rest,
            trust,
            endpoint: Endpoint {
                uri: format!("https://{slug}.internal"),
                transport: "http".to_owned(),
            },
            spec_hash: [2u8; 32],
            pinned_desc_hash: [2u8; 32],
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: synopsis.to_owned(),
            operations: ops,
            status,
        })
    }

    // spec: RTE-030
    #[test]
    fn group_id_is_stable_across_1_2_and_5_verified_replicas_with_arg_order_and_ns_normalization() {
        let b1 = BranchId::new("crm-us").unwrap();
        let b2 = BranchId::new("crm-eu").unwrap();
        let b3 = BranchId::new("crm-apac").unwrap();
        let b4 = BranchId::new("crm-latam").unwrap();
        let b5 = BranchId::new("crm-jp").unwrap();

        // Canonical operation uses argument order `(id: ID!, includeItems: Boolean)` and `crm_us_` prefix.
        let op1 = mk_op(
            &b1,
            "crm_us_getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "crm_us_getOrder(id: ID!, includeItems: Boolean): crm_us_Order",
            &["fetch customer order"],
        );
        // Replicas permute named GraphQL argument order and use their own `<ns>_` prefixes.
        let op2 = mk_op(
            &b2,
            "crm_eu_getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "crm_eu_getOrder(includeItems: Boolean, id: ID!): crm_eu_Order",
            &["retrieve eu order invoice"],
        );
        let op3 = mk_op(
            &b3,
            "crm_apac_getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "crm_apac_getOrder(id: ID!, includeItems: Boolean): crm_apac_Order",
            &["fetch customer order", "apac order lookup"],
        );
        let op4 = mk_op(
            &b4,
            "crm_latam_getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "crm_latam_getOrder(includeItems: Boolean, id: ID!): crm_latam_Order",
            &["latam order lookup"],
        );
        let op5 = mk_op(
            &b5,
            "crm_jp_getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "crm_jp_getOrder(id: ID!, includeItems: Boolean): crm_jp_Order",
            &["jp order status"],
        );

        let br1 = mk_branch(
            "crm-us",
            TrustTier::T0,
            BranchStatus::Active,
            "US CRM",
            vec![Arc::clone(&op1)],
        );
        let br2 = mk_branch(
            "crm-eu",
            TrustTier::T0,
            BranchStatus::Active,
            "EU CRM",
            vec![Arc::clone(&op2)],
        );
        let br3 = mk_branch(
            "crm-apac",
            TrustTier::T1,
            BranchStatus::Active,
            "APAC CRM",
            vec![Arc::clone(&op3)],
        );
        let br4 = mk_branch(
            "crm-latam",
            TrustTier::T1,
            BranchStatus::Active,
            "LATAM CRM",
            vec![Arc::clone(&op4)],
        );
        let br5 = mk_branch(
            "crm-jp",
            TrustTier::T1,
            BranchStatus::Active,
            "JP CRM",
            vec![Arc::clone(&op5)],
        );

        let out_1 =
            assign_capability_groups(&[Arc::clone(&br1)], &GroupAssignmentConfig::new()).unwrap();
        let gid_1 = out_1.op_to_group[&op1.id].clone();

        let cfg_2 =
            GroupAssignmentConfig::new().with_verified_op_replica(op2.id.clone(), op1.id.clone());
        let out_2 =
            assign_capability_groups(&[Arc::clone(&br1), Arc::clone(&br2)], &cfg_2).unwrap();
        assert_eq!(out_2.groups.len(), 1);
        assert_eq!(out_2.op_to_group[&op1.id], gid_1);
        assert_eq!(out_2.op_to_group[&op2.id], gid_1);

        // Build 5-replica set using mixed op-level + branch-level verified declarations.
        let cfg_5 = cfg_2
            .with_verified_op_replica(op3.id.clone(), op1.id.clone())
            .with_verified_branch_replica(b4.clone(), b1.clone())
            .with_verified_branch_replica(b5.clone(), b2.clone()); // chained b5 -> b2 -> b1
        let branches_5 = vec![br1, br2, br3, br4, br5];
        let out_5 = assign_capability_groups(&branches_5, &cfg_5).unwrap();
        assert_eq!(out_5.groups.len(), 1);
        for op in [&op1, &op2, &op3, &op4, &op5] {
            assert_eq!(
                out_5.op_to_group[&op.id], gid_1,
                "adding replicas must never change GroupId"
            );
        }

        // Apply to snapshot and verify group-level index creates 1 operation doc regardless of replica count.
        let mut base_snap = Snapshot::empty(7);
        for br in &branches_5 {
            base_snap.branches.insert(br.uid, Arc::clone(br));
        }
        let snap = out_5
            .apply_to_snapshot(
                &base_snap,
                TenantId::default_tenant(),
                HybridIndexConfig::default(),
            )
            .unwrap();
        assert_eq!(snap.groups.len(), 1);
        assert_eq!(snap.groups[&gid_1].members.len(), 5);
        assert_eq!(snap.index.element_count(), 1);
    }

    // spec: RTE-030
    #[test]
    fn group_assignment_rejects_divergent_signatures_t2_mismatch_and_isolates_unverified() {
        let b_main = BranchId::new("crm-main").unwrap();
        let b_bad_sig = BranchId::new("crm-bad").unwrap();
        let b_t2 = BranchId::new("crm-t2").unwrap();
        let b_unv = BranchId::new("crm-unverified").unwrap();
        let b_undecl = BranchId::new("crm-undeclared").unwrap();

        let op_main = mk_op(
            &b_main,
            "getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "getOrder(id: ID!): String",
            &["get order"],
        );
        let op_bad_sig = mk_op(
            &b_bad_sig,
            "getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "getOrder(id: Int!): String",
            &["get order"],
        );
        let op_t2 = mk_op(
            &b_t2,
            "getOrder",
            OpKind::Query,
            SideEffectLevel::None, // forced to Unknown because TrustTier::T2 (`GQL-032`)
            "getOrder(id: ID!): String",
            &["get order"],
        );
        let op_unv = mk_op(
            &b_unv,
            "getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "getOrder(id: ID!): String",
            &["get order"],
        );
        let op_undecl = mk_op(
            &b_undecl,
            "getOrder",
            OpKind::Query,
            SideEffectLevel::None,
            "getOrder(id: ID!): String",
            &["get order"],
        );

        let br_main = mk_branch(
            "crm-main",
            TrustTier::T0,
            BranchStatus::Active,
            "Main",
            vec![Arc::clone(&op_main)],
        );
        let br_bad = mk_branch(
            "crm-bad",
            TrustTier::T0,
            BranchStatus::Active,
            "BadSig",
            vec![Arc::clone(&op_bad_sig)],
        );
        let br_t2 = mk_branch(
            "crm-t2",
            TrustTier::T2,
            BranchStatus::Active,
            "Untrusted",
            vec![Arc::clone(&op_t2)],
        );
        let br_unv = mk_branch(
            "crm-unverified",
            TrustTier::T0,
            BranchStatus::Active,
            "Unv",
            vec![Arc::clone(&op_unv)],
        );
        let br_undecl = mk_branch(
            "crm-undeclared",
            TrustTier::T0,
            BranchStatus::Active,
            "Undec",
            vec![Arc::clone(&op_undecl)],
        );

        // Signature mismatch rejects verified merge.
        let err_sig = assign_capability_groups(
            &[Arc::clone(&br_main), Arc::clone(&br_bad)],
            &GroupAssignmentConfig::new()
                .with_verified_op_replica(op_bad_sig.id.clone(), op_main.id.clone()),
        )
        .unwrap_err();
        assert!(matches!(
            err_sig,
            GroupAssemblyError::SignatureMismatch { .. }
        ));

        // T2 branch has effective side effect Unknown != None and cannot share group with T0 (`GQL-032`).
        let err_t2 = assign_capability_groups(
            &[Arc::clone(&br_main), Arc::clone(&br_t2)],
            &GroupAssignmentConfig::new()
                .with_verified_op_replica(op_t2.id.clone(), op_main.id.clone()),
        )
        .unwrap_err();
        assert!(matches!(
            err_t2,
            GroupAssemblyError::IncompatibleMetadata { .. }
        ));

        // Unverified candidate and undeclared identical-signature branch MUST keep separate solo GroupIds (`ADR-006`).
        let out = assign_capability_groups(
            &[br_main, br_unv, br_undecl],
            &GroupAssignmentConfig::new()
                .with_unverified_op_replica(op_unv.id.clone(), op_main.id.clone()),
        )
        .unwrap();
        assert_eq!(out.groups.len(), 3);
        let g_main = &out.op_to_group[&op_main.id];
        let g_unv = &out.op_to_group[&op_unv.id];
        let g_undec = &out.op_to_group[&op_undecl.id];
        assert_ne!(g_main, g_unv);
        assert_ne!(g_main, g_undec);
        assert_ne!(g_unv, g_undec);
        assert_eq!(
            out.isolated_unverified,
            vec![(op_unv.id.clone(), op_main.id.clone())]
        );
    }

    // spec: RUN-011
    #[test]
    fn group_validate_unambiguous_root_ownership_rejects_duplicate_active_coordinates() {
        let b1 = BranchId::new("billing-a").unwrap();
        let b2 = BranchId::new("billing-b").unwrap();
        let b_disabled = BranchId::new("billing-old").unwrap();

        let op1 = mk_op(
            &b1,
            "getInvoice",
            OpKind::Query,
            SideEffectLevel::None,
            "getInvoice: String",
            &[],
        );
        let op2 = mk_op(
            &b2,
            "getInvoice",
            OpKind::Query,
            SideEffectLevel::None,
            "getInvoice: String",
            &[],
        );
        let op_dis = mk_op(
            &b_disabled,
            "getInvoice",
            OpKind::Query,
            SideEffectLevel::None,
            "getInvoice: String",
            &[],
        );

        let br1 = mk_branch(
            "billing-a",
            TrustTier::T0,
            BranchStatus::Active,
            "A",
            vec![op1],
        );
        let br2 = mk_branch(
            "billing-b",
            TrustTier::T0,
            BranchStatus::Active,
            "B",
            vec![op2],
        );
        let br_dis = mk_branch(
            "billing-old",
            TrustTier::T0,
            BranchStatus::Disabled,
            "Old",
            vec![op_dis],
        );

        // Active + Disabled does not conflict.
        assert!(validate_unambiguous_root_ownership(&[Arc::clone(&br1), br_dis]).is_ok());

        // Two active branches sharing `Query.getInvoice` triggers AmbiguousFieldOwnership (`RUN-011`).
        let err = validate_unambiguous_root_ownership(&[br1, br2]).unwrap_err();
        assert_eq!(
            err,
            GroupAssemblyError::AmbiguousFieldOwnership {
                coordinate: "Query.getInvoice".to_owned(),
                branches: vec![b1, b2],
            }
        );
    }

    proptest! {
        // spec: RTE-030
        #[test]
        fn prop_group_text_union_is_permutation_invariant_and_deduplicated(
            perm in proptest::sample::subsequence(vec![0usize, 1, 2], 3),
        ) {
            let b0 = BranchId::new("svc-0").unwrap();
            let b1 = BranchId::new("svc-1").unwrap();
            let b2 = BranchId::new("svc-2").unwrap();

            let o0 = mk_op(&b0, "findAccount", OpKind::Query, SideEffectLevel::None, "findAccount: String", &["lookup account", "search profile"]);
            let o1 = mk_op(&b1, "findAccount", OpKind::Query, SideEffectLevel::None, "findAccount: String", &["lookup account", "find customer ledger"]);
            let o2 = mk_op(&b2, "findAccount", OpKind::Query, SideEffectLevel::None, "findAccount: String", &["search profile", "query client"]);

            let d0 = mk_branch("svc-0", TrustTier::T0, BranchStatus::Active, "Core CRM", vec![Arc::clone(&o0)]);
            let d1 = mk_branch("svc-1", TrustTier::T0, BranchStatus::Active, "Core CRM", vec![Arc::clone(&o1)]);
            let d2 = mk_branch("svc-2", TrustTier::T0, BranchStatus::Active, "Edge CRM", vec![Arc::clone(&o2)]);

            let ordered_pairs = [
                (d0.as_ref(), o0.as_ref()),
                (d1.as_ref(), o1.as_ref()),
                (d2.as_ref(), o2.as_ref()),
            ];
            let permuted: Vec<_> = perm.iter().map(|&idx| ordered_pairs[idx]).collect();

            let (text_ref, ex_ref) = union_group_text(&ordered_pairs, &o0.id);
            let (text_perm, ex_perm) = union_group_text(&permuted, &o0.id);

            prop_assert_eq!(&text_ref, &text_perm);
            prop_assert_eq!(&ex_ref, &ex_perm);
            // "Core CRM" and "lookup account" only appear once.
            prop_assert_eq!(text_ref.matches("Core CRM").count(), 1);
            prop_assert_eq!(text_ref.matches("lookup account").count(), 1);
            prop_assert!(text_ref.contains("find customer ledger"));
            prop_assert!(text_ref.contains("query client"));
        }
    }
}
