//! Immutable `Snapshot`, branch/operation descriptors, and tenant-scoped cache/telemetry keys
//! (`spec/01-architecture.md` §4, `ARC-005`, `ARC-006`, `INV-09`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use apollo_compiler::Schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::ids::{BranchId, BranchUid, GroupId, OperationId, ProposalId, TenantId};
use crate::joingraph::JoinGraph;
use crate::traits::{ElementSearch, EmptyElementSearch};

/// Current binary/in-memory `Snapshot` format version (`ADR-024`).
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// Upstream protocol spoken by a registered branch (`proto/gap/ngc/v1/ngc.proto`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Protocol {
    /// Relational SQL backend (`SQL`).
    Sql,
    /// OpenAPI / HTTP REST backend (`REST`).
    Rest,
    /// gRPC service (`GRPC`).
    Grpc,
    /// Upstream GraphQL endpoint (`GRAPHQL`).
    Graphql,
    /// Outbound MCP server (`MCP`).
    Mcp,
    /// Peer A2A / GAP agent (`A2A`).
    A2a,
    /// Sandboxed local CLI binary (`CLI`).
    Cli,
    /// Tree-sitter / document / symbol graph (`CODE_GRAPH`).
    CodeGraph,
    /// Custom NGC connector (`CUSTOM`).
    Custom,
}

/// Branch trust tier (`ADR-013`, `spec/07-security.md` §8).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum TrustTier {
    /// Organization-signed manifest (`SEC-060`).
    T0,
    /// Admin-approved with pinned hashes (`SEC-061`).
    T1,
    /// Untrusted / discovered / drifted (`SEC-061`, `INV-07`).
    T2,
}

/// Lifecycle state of a branch inside a [`Snapshot`] (`spec/01-architecture.md` §4).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BranchStatus {
    /// Healthy and eligible for routing.
    Active,
    /// Connector description diverged from `pinned_desc_hash` (`ING-053`, `INV-08`).
    Drift,
    /// Quarantined pending review (`ADR-013`).
    Quarantined,
    /// Administratively disabled.
    Disabled,
}

/// Callable operation kind (`spec/01-architecture.md` §4, `proto/gap/ngc/v1/ngc.proto`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OpKind {
    /// Side-effect-free read (`QUERY`).
    Query,
    /// State-mutating call (`MUTATION`).
    Mutation,
    /// Event stream (`SUBSCRIPTION`).
    Subscription,
    /// Long-running A2A-compatible task (`TASK`).
    Task,
}

/// Verified side-effect classification level (`ADR-015`, `spec/07-security.md` §5).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SideEffectLevel {
    /// Read-only / pure (`NONE`).
    None,
    /// State change with a compensating operation (`REVERSIBLE`).
    Reversible,
    /// External or permanent state change (`IRREVERSIBLE`).
    Irreversible,
    /// Unclassified; treated conservatively by policy (`UNKNOWN`).
    Unknown,
}

/// Approved side-effect metadata attached to an [`OperationDescriptor`] (`ADR-015`, `INV-11`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SideEffect {
    /// Verified side-effect severity.
    pub level: SideEffectLevel,
    /// Whether classification was approved rather than raw connector hint (`NGC-014`).
    pub verified: bool,
    /// Whether policy requires explicit human approval regardless of session flags.
    pub requires_approval: bool,
}

/// Static cost parameters for `GQL-041` query complexity calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CostHint {
    /// Base field weight `w(F)` (defaults to `1`).
    pub weight: u32,
    /// Expected list cardinality `@cost.listSize` when caller omits `first`/`limit`.
    pub default_list_size: Option<u32>,
    /// Additional cost per joined key (`perKey`).
    pub per_key_weight: u32,
}

impl Default for CostHint {
    fn default() -> Self {
        Self {
            weight: 1,
            default_list_size: None,
            per_key_weight: 1,
        }
    }
}

/// Sanitized positive or negative usage example for an operation (`NGC-016`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Example {
    /// Natural-language intent phrase.
    pub intent: String,
    /// `true` for positive match examples, `false` for negative disambiguation examples.
    pub positive: bool,
    /// Optional minimal GAP-QL selection snippet.
    pub query_snippet: Option<String>,
}

/// Sanitized connector endpoint metadata (`INV-15`: never stores credentials).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Target URI or unix socket path (must contain no userinfo/secrets, `SEC-075`, `INV-15`).
    pub uri: String,
    /// Binding transport (`"grpc"`, `"http"`, `"stdio"`).
    pub transport: String,
}

/// Advertised connector capabilities (`spec/03-ngc.md` §§2–6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Supports batched `foreach` variable sets (`NGC-020`).
    pub foreach_batch: bool,
    /// Supports `ctx.idempotency_key` deduplication (`NGC-030`).
    pub idempotency: bool,
    /// Supports `Mutate(dry_run = true)` (`NGC-031`).
    pub dry_run: bool,
    /// Supports `Subscribe` streaming (`NGC-040`).
    pub subscriptions: bool,
    /// Supports `StartTask`/`GetTask`/`CancelTask`/`WatchTask` (`NGC-041`).
    pub tasks: bool,
    /// Supports privacy-preserving `Sketch` (`NGC-050`).
    pub sketch: bool,
    /// Supports `Explain` (`ngc.proto`).
    pub explain: bool,
    /// Supported remote predicate operators (`NGC-023`).
    pub predicate_ops: Vec<String>,
}

/// Per-branch concurrency, batching, timeout, and payload limits (`spec/01-architecture.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BranchLimits {
    /// Maximum `foreach` variable sets per call (`0` = batching unsupported, `NGC-015`).
    pub max_batch_size: u32,
    /// Maximum concurrent in-flight requests to this connector (`NGC-071`).
    pub max_in_flight: u32,
    /// Per-call connector timeout in milliseconds (`*Ms`).
    pub timeout_ms: u32,
    /// Maximum response payload size in bytes (`NGC-027`).
    pub max_response_bytes: u64,
}

impl Default for BranchLimits {
    fn default() -> Self {
        Self {
            max_batch_size: 100,
            max_in_flight: 64,
            timeout_ms: 5_000,
            max_response_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Immutable operation descriptor inside a [`BranchDescriptor`] (`spec/01-architecture.md` §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDescriptor {
    /// Qualified `<branch>.<name>` identifier.
    pub id: OperationId,
    /// Interchangeable capability group (`ADR-006`).
    pub group: GroupId,
    /// Operation category.
    pub kind: OpKind,
    /// Verified side-effect classification (`ADR-015`).
    pub side_effect: SideEffect,
    /// Whether repeated execution with identical inputs is idempotent (`NGC-030`).
    pub idempotent: bool,
    /// Static cost parameters (`GQL-041`).
    pub cost: CostHint,
    /// `blake3-256` digest over canonical JCS operation metadata (`NGC-011`, `INV-08`).
    pub desc_hash: [u8; 32],
    /// Rendered GraphQL SDL signature shared across snapshots (`ARC-005`).
    pub sdl: Arc<str>,
    /// Sanitized intent examples (`NGC-016`).
    pub examples: Vec<Example>,
}

/// Immutable branch descriptor inside a [`Snapshot`] (`spec/01-architecture.md` §4, `ARC-005`).
///
/// Live latency, error EWMA, in-flight count, and circuit-breaker state NEVER live here (`ARC-006`,
/// `INV-10`); they reside in an external telemetry slab keyed by [`TelemetrySlotKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDescriptor {
    /// Stable UUIDv7 across generations (`spec/00-conventions.md` §7).
    pub uid: BranchUid,
    /// Human-readable branch slug.
    pub id: BranchId,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Trust tier (`T0`/`T1`/`T2`).
    pub trust: TrustTier,
    /// Sanitized connector endpoint.
    pub endpoint: Endpoint,
    /// `blake3-256` digest of upstream raw specification.
    pub spec_hash: [u8; 32],
    /// Approved `blake3-256` digest over all operation `desc_hash` values (`INV-08`).
    pub pinned_desc_hash: [u8; 32],
    /// Connector capability flags.
    pub capabilities: Capabilities,
    /// Batch, concurrency, timeout, and size limits.
    pub limits: BranchLimits,
    /// Short `<= 24`-token progressive-disclosure summary (`spec/01-architecture.md` §4).
    pub synopsis: String,
    /// Operations exposed by this branch.
    pub operations: Vec<Arc<OperationDescriptor>>,
    /// Administrative/drift lifecycle state.
    pub status: BranchStatus,
}

/// Group of interchangeable replica operations across branches (`ADR-006`, `spec/01-architecture.md` §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGroup {
    /// `blake3-128` hex identifier of canonical group signature.
    pub id: GroupId,
    /// Canonical GraphQL signature shared by every replica in the group.
    pub signature_sdl: Arc<str>,
    /// Member `(branch_uid, operation_id)` replicas.
    pub members: Vec<(BranchUid, OperationId)>,
}

/// Conformal calibration parameters pinned to a [`Snapshot`] (`ADR-008`, `spec/01-architecture.md` §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalibrationTable {
    /// Routing model identifier to which this calibration applies.
    pub model_id: String,
    /// Temperature scaling parameter (`> 0.0`).
    pub temperature: f32,
    /// Calibrated abstention threshold (`GAP_INSUFFICIENT_EVIDENCE`).
    pub abstention_threshold: f32,
    /// Nonconformity quantile scores for conformal prediction sets.
    pub quantile_scores: Vec<f32>,
    /// Number of held-out / observed samples used to fit this table.
    pub sample_count: u64,
}

/// Audit provenance of a compiled [`Snapshot`] (`spec/01-architecture.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildProvenance {
    /// Component or identity that produced the snapshot (e.g. `"bootstrap"`, `"gap-snapshot"`).
    pub builder: String,
    /// Optional ingestion proposal that triggered this generation.
    pub proposal_id: Option<ProposalId>,
    /// Previous generation from which unchanged `Arc` structures were shared (`ARC-005`).
    pub parent_generation: Option<u64>,
}

impl Default for BuildProvenance {
    fn default() -> Self {
        Self {
            builder: "bootstrap".to_owned(),
            proposal_id: None,
            parent_generation: None,
        }
    }
}

/// GraphQL `(type_name, field)` coordinate inside a composed supergraph (`RUN-009`, `RUN-011`).
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct FieldCoordinate {
    /// Enclosing GraphQL object type name (e.g. `"Query"`, `"shop_Customer"`).
    pub type_name: String,
    /// Field name on `type_name`.
    pub field: String,
}

impl FieldCoordinate {
    /// Constructs a [`FieldCoordinate`] for `type_name.field`.
    #[must_use]
    pub fn new(type_name: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            type_name: type_name.into(),
            field: field.into(),
        }
    }
}

impl fmt::Display for FieldCoordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.type_name, self.field)
    }
}

/// Immutable field-ownership map from [`FieldCoordinate`] to owning [`BranchUid`] (`RUN-009`, `RUN-011`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldOwnership {
    owners: BTreeMap<FieldCoordinate, BranchUid>,
}

impl FieldOwnership {
    /// Returns an empty [`FieldOwnership`] map.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            owners: BTreeMap::new(),
        }
    }

    /// Wraps an immutable `BTreeMap` of [`FieldCoordinate`] -> [`BranchUid`] entries.
    #[must_use]
    pub const fn new(owners: BTreeMap<FieldCoordinate, BranchUid>) -> Self {
        Self { owners }
    }

    /// Resolves the owning [`BranchUid`] for `coord`, if owned by a branch (`RUN-011`).
    #[must_use]
    pub fn owner(&self, coord: &FieldCoordinate) -> Option<BranchUid> {
        self.owners.get(coord).copied()
    }

    /// Iterates in deterministic `(type_name, field)` order over coordinates owned by `branch_uid`.
    pub fn fields_of(&self, branch_uid: BranchUid) -> impl Iterator<Item = &FieldCoordinate> + '_ {
        self.owners
            .iter()
            .filter_map(move |(coord, &uid)| (uid == branch_uid).then_some(coord))
    }

    /// Number of branch-owned field coordinates in this map.
    #[must_use]
    pub fn len(&self) -> usize {
        self.owners.len()
    }

    /// Returns `true` if no field coordinates are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// Iterates over all `(FieldCoordinate, BranchUid)` pairs in deterministic order.
    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, FieldCoordinate, BranchUid> {
        self.owners.iter()
    }

    /// Borrows the underlying sorted map.
    #[must_use]
    pub const fn as_map(&self) -> &BTreeMap<FieldCoordinate, BranchUid> {
        &self.owners
    }
}

/// Immutable compiled supergraph snapshot (`spec/01-architecture.md` §4, `ARC-005`, `ARC-006`).
///
/// Persistent `im::OrdMap` collections and [`Arc`]-wrapped artifacts allow publishing generation
/// `g + 1` to structurally share unchanged [`BranchDescriptor`]s, [`CapabilityGroup`]s,
/// [`FieldOwnership`], [`JoinGraph`], and [`apollo_compiler::Schema`] without copying (`ARC-005`),
/// while containing zero live telemetry state (`ARC-006`).
#[derive(Clone)]
pub struct Snapshot {
    /// Monotonically increasing generation number (`spec/00-conventions.md` §6).
    pub generation: u64,
    /// Snapshot binary/layout version (`ADR-024`).
    pub format_version: u32,
    /// Owning tenant (`INV-09`, `ADR-018`).
    pub tenant_id: TenantId,
    /// Registered branches keyed by stable [`BranchUid`] (`ARC-005`).
    pub branches: im::OrdMap<BranchUid, Arc<BranchDescriptor>>,
    /// Interchangeable capability groups keyed by [`GroupId`] (`ARC-005`).
    pub groups: im::OrdMap<GroupId, Arc<CapabilityGroup>>,
    /// Composed supergraph GraphQL schema (`apollo_compiler::Schema`).
    pub schema: Arc<Schema>,
    /// Deterministic field-ownership table consistent with `@branch` directives (`RUN-009`, `RUN-011`).
    pub ownership: Arc<FieldOwnership>,
    /// Cross-branch join graph (`INV-06`).
    pub joins: Arc<JoinGraph>,
    /// Hybrid lexical/vector discovery index (`ADR-010`).
    pub index: Arc<dyn ElementSearch>,
    /// Monotonic/digest version of the active Cedar policy set (`TM-22`).
    pub policy_version: u64,
    /// Optional conformal calibration parameters (`ADR-008`).
    pub calibration: Option<Arc<CalibrationTable>>,
    /// `blake3-256` hash of the active branch set (part of [`PlanCacheKey`]).
    pub branch_set_hash: [u8; 32],
    /// UTC timestamp when this snapshot was built.
    pub built_at: Timestamp,
    /// Provenance metadata for auditability.
    pub built_by: BuildProvenance,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("generation", &self.generation)
            .field("format_version", &self.format_version)
            .field("tenant_id", &self.tenant_id)
            .field("branch_count", &self.branches.len())
            .field("group_count", &self.groups.len())
            .field("owned_fields", &self.ownership.len())
            .field("schema_type_count", &self.schema.types.len())
            .field("active_joins", &self.joins.active_edge_count())
            .field("index_elements", &self.index.element_count())
            .field("policy_version", &self.policy_version)
            .field("has_calibration", &self.calibration.is_some())
            .field("branch_set_hash", &self.branch_set_hash)
            .field("built_at", &self.built_at)
            .field("built_by", &self.built_by)
            .finish()
    }
}

impl Snapshot {
    /// Constructs a minimal valid empty [`Snapshot`] for `TenantId::default_tenant()`.
    #[must_use]
    pub fn empty(generation: u64) -> Self {
        Self::empty_for_tenant(generation, TenantId::default_tenant())
    }

    /// Constructs a minimal valid empty [`Snapshot`] scoped to `tenant_id` (`INV-09`).
    #[must_use]
    pub fn empty_for_tenant(generation: u64, tenant_id: TenantId) -> Self {
        Self {
            generation,
            format_version: SNAPSHOT_FORMAT_VERSION,
            tenant_id,
            branches: im::OrdMap::new(),
            groups: im::OrdMap::new(),
            schema: Arc::new(Schema::new()),
            ownership: Arc::new(FieldOwnership::empty()),
            joins: Arc::new(JoinGraph::empty()),
            index: Arc::new(EmptyElementSearch),
            policy_version: 0,
            calibration: None,
            branch_set_hash: [0_u8; 32],
            built_at: Timestamp::UNIX_EPOCH,
            built_by: BuildProvenance::default(),
        }
    }

    /// Derives a new generation that structurally shares every [`Arc`] field with `self` (`ARC-005`)
    /// until specific fields are replaced by a builder/publisher.
    #[must_use]
    pub fn next_generation(
        &self,
        generation: u64,
        built_at: Timestamp,
        builder: impl Into<String>,
        proposal_id: Option<ProposalId>,
    ) -> Self {
        let mut next = self.clone();
        next.generation = generation;
        next.built_at = built_at;
        next.built_by = BuildProvenance {
            builder: builder.into(),
            proposal_id,
            parent_generation: Some(self.generation),
        };
        next
    }
}

/// Tenant-scoped query plan cache key (`INV-09`, `ADR-009` §6, `ADR-018`, `TM-22`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanCacheKey {
    /// Mandatory tenant isolation prefix (`INV-09`).
    pub tenant: TenantId,
    /// Snapshot generation (`INV-01`).
    pub snapshot_generation: u64,
    /// Active branch set digest (`ADR-009`).
    pub branch_set_hash: [u8; 32],
    /// Cedar policy version (`TM-22`).
    pub policy_version: u64,
    /// `blake3-256` digest of normalized GAP-QL document.
    pub document_hash: [u8; 32],
    /// Selected GraphQL operation name, if any.
    pub operation_name: Option<String>,
}

impl PlanCacheKey {
    /// Constructs a [`PlanCacheKey`] bound to `snapshot` (inheriting its `tenant_id`, `generation`,
    /// `branch_set_hash`, and `policy_version`).
    #[must_use]
    pub fn from_snapshot(
        snapshot: &Snapshot,
        document_hash: [u8; 32],
        operation_name: Option<String>,
    ) -> Self {
        Self {
            tenant: snapshot.tenant_id.clone(),
            snapshot_generation: snapshot.generation,
            branch_set_hash: snapshot.branch_set_hash,
            policy_version: snapshot.policy_version,
            document_hash,
            operation_name,
        }
    }
}

/// Tenant-scoped key for live branch telemetry slabs outside [`Snapshot`] (`ARC-006`, `INV-09`,
/// `INV-10`).
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TelemetrySlotKey {
    /// Mandatory tenant isolation prefix (`INV-09`).
    pub tenant: TenantId,
    /// Stable branch UID across generations.
    pub branch_uid: BranchUid,
}

impl TelemetrySlotKey {
    /// Constructs a tenant-scoped [`TelemetrySlotKey`] (`INV-09`).
    #[must_use]
    pub const fn new(tenant: TenantId, branch_uid: BranchUid) -> Self {
        Self { tenant, branch_uid }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_branch(slug: &str) -> Arc<BranchDescriptor> {
        let group: GroupId = "0123456789abcdef0123456789abcdef"
            .parse()
            .expect("valid group id");
        let op = Arc::new(OperationDescriptor {
            id: format!("{slug}.getInvoice").parse().expect("valid op id"),
            group,
            kind: OpKind::Query,
            side_effect: SideEffect {
                level: SideEffectLevel::None,
                verified: true,
                requires_approval: false,
            },
            idempotent: true,
            cost: CostHint::default(),
            desc_hash: [1_u8; 32],
            sdl: Arc::from("getInvoice(id: ID!): Invoice"),
            examples: vec![Example {
                intent: "fetch invoice by id".to_owned(),
                positive: true,
                query_snippet: Some("{ billing { getInvoice(id: \"1\") { id } } }".to_owned()),
            }],
        });

        Arc::new(BranchDescriptor {
            uid: BranchUid::new_v7(),
            id: slug.parse().expect("valid branch slug"),
            protocol: Protocol::Grpc,
            trust: TrustTier::T0,
            endpoint: Endpoint {
                uri: "https://billing.internal:8443".to_owned(),
                transport: "grpc".to_owned(),
            },
            spec_hash: [2_u8; 32],
            pinned_desc_hash: [3_u8; 32],
            capabilities: Capabilities {
                foreach_batch: true,
                idempotency: true,
                ..Capabilities::default()
            },
            limits: BranchLimits::default(),
            synopsis: "Billing invoices and ledger".to_owned(),
            operations: vec![op],
            status: BranchStatus::Active,
        })
    }

    // spec: ARC-005, ARC-006
    #[test]
    fn arc_005_snapshot_structurally_shares_unchanged_branches_across_generations() {
        let billing = sample_branch("billing");
        let crm = sample_branch("crm");
        let mut map_g1 = im::OrdMap::new();
        map_g1.insert(billing.uid, Arc::clone(&billing));
        map_g1.insert(crm.uid, Arc::clone(&crm));

        let mut g1 = Snapshot::empty(1);
        g1.branches = map_g1;
        g1.schema = Arc::new(
            Schema::parse("type Query { ok: Boolean }", "supergraph.graphql").expect("valid SDL"),
        );

        // Build g2 updating only a new branch while sharing `billing`, `crm`, and `schema` Arcs.
        let mut g2 = g1.next_generation(
            2,
            "2026-10-26T23:00:00Z".parse().expect("valid ts"),
            "gap-snapshot",
            Some(ProposalId::new_v7()),
        );
        let shipping = sample_branch("shipping");
        g2.branches.insert(shipping.uid, Arc::clone(&shipping));

        assert_eq!(g1.generation, 1);
        assert_eq!(g2.generation, 2);
        assert_eq!(g1.branches.len(), 2);
        assert_eq!(g2.branches.len(), 3);
        assert_eq!(g2.built_by.parent_generation, Some(1));

        // Pointer equality proves structural sharing without deep-cloning unchanged descriptors.
        assert!(Arc::ptr_eq(
            g1.branches.get(&billing.uid).expect("billing in g1"),
            g2.branches.get(&billing.uid).expect("billing in g2"),
        ));
        assert!(Arc::ptr_eq(&g1.schema, &g2.schema));
        assert!(Arc::ptr_eq(&g1.ownership, &g2.ownership));

        // Large OrdMap test: cloning and inserting 1 branch into a 500-branch OrdMap does not
        // clone untouched leaf nodes (most descriptors keep strong_count == 2: vec + shared leaf).
        let many: Vec<Arc<BranchDescriptor>> = (0..500)
            .map(|i| sample_branch(&format!("br-{i:04}")))
            .collect();
        let mut ord_g1 = im::OrdMap::new();
        for b in &many {
            ord_g1.insert(b.uid, Arc::clone(b));
        }
        let mut ord_g2 = ord_g1.clone();
        let extra = sample_branch("br-extra");
        ord_g2.insert(extra.uid, extra);

        let untouched_leaves = many.iter().filter(|b| Arc::strong_count(b) == 2).count();
        assert!(
            untouched_leaves > 400,
            "im::OrdMap must share untouched B-tree leaf nodes (got {untouched_leaves}/500)"
        );
        for b in &many {
            assert!(Arc::ptr_eq(
                ord_g1.get(&b.uid).expect("in g1"),
                ord_g2.get(&b.uid).expect("in g2"),
            ));
        }
    }

    // spec: RUN-009, RUN-011, ARC-005
    #[test]
    fn run_011_field_ownership_lookup_fields_of_and_snapshot_default() {
        let empty_snap = Snapshot::empty(1);
        assert!(empty_snap.ownership.is_empty());
        assert_eq!(empty_snap.ownership.len(), 0);

        let shop_uid = BranchUid::new_v7();
        let crm_uid = BranchUid::new_v7();
        let mut entries = BTreeMap::new();
        entries.insert(FieldCoordinate::new("Query", "customer"), shop_uid);
        entries.insert(FieldCoordinate::new("shop_Customer", "id"), shop_uid);
        entries.insert(FieldCoordinate::new("Query", "lead"), crm_uid);

        let ownership = FieldOwnership::new(entries);
        assert!(!ownership.is_empty());
        assert_eq!(ownership.len(), 3);
        assert_eq!(
            ownership.owner(&FieldCoordinate::new("Query", "customer")),
            Some(shop_uid)
        );
        assert_eq!(
            ownership.owner(&FieldCoordinate::new("Query", "unknown")),
            None
        );

        let shop_fields: Vec<FieldCoordinate> = ownership.fields_of(shop_uid).cloned().collect();
        assert_eq!(
            shop_fields,
            vec![
                FieldCoordinate::new("Query", "customer"),
                FieldCoordinate::new("shop_Customer", "id"),
            ]
        );
        assert_eq!(ownership.fields_of(BranchUid::new_v7()).count(), 0);
    }

    // spec: INV-09, ARC-006 (ADR-018)
    #[test]
    fn inv_09_plan_cache_key_and_telemetry_slot_key_isolate_tenants() {
        let acme: TenantId = "acme".parse().expect("acme");
        let globex: TenantId = "globex".parse().expect("globex");
        let snap_acme = Snapshot::empty_for_tenant(5, acme.clone());
        let snap_globex = Snapshot::empty_for_tenant(5, globex.clone());

        let doc_hash = [9_u8; 32];
        let k_acme = PlanCacheKey::from_snapshot(&snap_acme, doc_hash, None);
        let k_globex = PlanCacheKey::from_snapshot(&snap_globex, doc_hash, None);
        assert_ne!(k_acme, k_globex);

        let shared_uid = BranchUid::new_v7();
        let slot_acme = TelemetrySlotKey::new(acme, shared_uid);
        let slot_globex = TelemetrySlotKey::new(globex, shared_uid);
        assert_ne!(slot_acme, slot_globex);
    }
}
