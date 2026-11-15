//! Domain types, configuration, constants, 13-state FSM, and canonical JSON helpers for ingestion
//! proposals (`WP-M4-07`; `ING-001`, `ING-002`, `ING-003`, `ING-050`, `ING-051`, `spec/05-ingestion.md` §1).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::str::FromStr;

use gap_core::clock::Timestamp;
use gap_core::error::{ErrorCode, GapError};
use gap_core::ids::{BranchId, ProposalId, TenantId};
use gap_core::snapshot::{Protocol, TrustTier};
use gap_store::error::StoreError;

use crate::cisg::CisgValue;
use crate::proposals::audit_adapter::parse_cisg_json;

/// Required scope to approve `T2` extraction or decide proposals (`ING-003`).
pub const ADMIN_INGEST_SCOPE: &str = "admin:ingest";

/// Required scope to inspect shadow snapshots via `X-Gap-Snapshot: shadow:<id>` (`ING-050`).
pub const ADMIN_SCHEMA_SCOPE: &str = "admin:schema";

/// Default approval window in seconds before `AWAITING_APPROVAL` expires to `REJECTED` (`14 days`, `spec/05-ingestion.md` §1).
pub const PROPOSAL_APPROVAL_TTL_SECS: u64 = 14 * 86_400;

/// Default post-activation watch window in seconds (`60 s`, `spec/05-ingestion.md` §1).
pub const PROPOSAL_WATCH_WINDOW_SECS: u64 = 60;

/// Maximum retention window for stored `query_redacted` events (`7 days`, `RTE-100`).
pub const UNMAPPED_QUERY_MAX_TTL_SECS: u64 = 7 * 86_400;

/// Singleflight coalescing window for identical `(tenant, query_hash)` events (`10 min`, `RTE-101`).
pub const UNMAPPED_SINGLEFLIGHT_WINDOW_SECS: u64 = 10 * 60;

/// Per-tenant distinct unmapped event rate limit per minute (`10 / min / tenant`, `RTE-101`).
pub const UNMAPPED_RATE_LIMIT_PER_MIN: usize = 10;

/// Maximum page size allowed for `gap.proposals(first: ...)` (`spec/02-gap-ql.md` §3).
pub const MAX_PROPOSALS_PAGE_SIZE: u32 = 1_000;

/// Default page size for `gap.proposals` (`spec/02-gap-ql.md` §3).
pub const DEFAULT_PROPOSALS_PAGE_SIZE: u32 = 20;

/// Proposal lifecycle state (`spec/02-gap-ql.md` §3 `ProposalStatus`, `spec/05-ingestion.md` §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProposalStatus {
    /// Initial proposed state (`PROPOSED`).
    Proposed,
    /// Deterministic source extraction running (`EXTRACTING`).
    Extracting,
    /// Optional bounded LLM enrichment (`ENRICHING`).
    Enriching,
    /// Verification gates `G1..G9` running (`VERIFYING`).
    Verifying,
    /// Shadow snapshot built; ready for policy evaluation (`STAGED`).
    Staged,
    /// Waiting for `admin:ingest` decision(s) (`AWAITING_APPROVAL`).
    AwaitingApproval,
    /// Prewarm and atomic snapshot swap in progress (`PROMOTING`).
    Promoting,
    /// Live in active snapshot (`ACTIVE`).
    Active,
    /// Post-activation 60 s canary/regression watch window (`WATCHING`).
    Watching,
    /// Confirmed healthy after watch window (`STABLE`).
    Stable,
    /// Explicitly denied or TTL-expired (`REJECTED`).
    Rejected,
    /// Extraction, verification gate, or promotion failed (`FAILED`).
    Failed,
    /// Reverted after watch regression (`ROLLED_BACK`).
    RolledBack,
}

impl ProposalStatus {
    /// Wire GraphQL / persistence representation matching `spec/02-gap-ql.md` §3.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "PROPOSED",
            Self::Extracting => "EXTRACTING",
            Self::Enriching => "ENRICHING",
            Self::Verifying => "VERIFYING",
            Self::Staged => "STAGED",
            Self::AwaitingApproval => "AWAITING_APPROVAL",
            Self::Promoting => "PROMOTING",
            Self::Active => "ACTIVE",
            Self::Watching => "WATCHING",
            Self::Stable => "STABLE",
            Self::Rejected => "REJECTED",
            Self::Failed => "FAILED",
            Self::RolledBack => "ROLLED_BACK",
        }
    }

    /// Returns `true` iff `self` is an in-progress open proposal where `(tenant, location, kind)`
    /// idempotency collapses repeated `proposeSource` calls (`ING-002`).
    ///
    /// Terminal outcomes (`STABLE`, `REJECTED`, `FAILED`, `ROLLED_BACK`) allow a fresh proposal.
    #[must_use]
    pub const fn is_open(self) -> bool {
        !matches!(
            self,
            Self::Stable | Self::Rejected | Self::Failed | Self::RolledBack
        )
    }

    /// Returns `true` iff transitioning `self -> next` is legal per `spec/05-ingestion.md` §1.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Proposed, Self::Extracting | Self::Rejected)
                | (Self::Extracting, Self::Enriching | Self::Failed)
                | (Self::Enriching, Self::Verifying | Self::Failed)
                | (Self::Verifying, Self::Staged | Self::Failed)
                | (Self::Staged, Self::AwaitingApproval | Self::Promoting)
                | (Self::AwaitingApproval, Self::Promoting | Self::Rejected)
                | (Self::Promoting, Self::Active | Self::Failed)
                | (Self::Active, Self::Watching)
                | (Self::Watching, Self::Stable | Self::RolledBack)
                | (Self::Stable, Self::Proposed)
        )
    }
}

impl fmt::Display for ProposalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProposalStatus {
    type Err = ProposalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "PROPOSED" => Ok(Self::Proposed),
            "EXTRACTING" => Ok(Self::Extracting),
            "ENRICHING" => Ok(Self::Enriching),
            "VERIFYING" => Ok(Self::Verifying),
            "STAGED" => Ok(Self::Staged),
            "AWAITING_APPROVAL" => Ok(Self::AwaitingApproval),
            "PROMOTING" => Ok(Self::Promoting),
            "ACTIVE" => Ok(Self::Active),
            "WATCHING" => Ok(Self::Watching),
            "STABLE" => Ok(Self::Stable),
            "REJECTED" => Ok(Self::Rejected),
            "FAILED" => Ok(Self::Failed),
            "ROLLED_BACK" => Ok(Self::RolledBack),
            other => Err(ProposalError::Validation(format!(
                "unknown proposal status `{other}`"
            ))),
        }
    }
}

/// Administrative decision (`spec/02-gap-ql.md` §3 `ApprovalDecision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Approve progression / activation.
    Approve,
    /// Deny and transition to `REJECTED`.
    Deny,
}

impl ApprovalDecision {
    /// Wire GraphQL / CLI representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::Deny => "DENY",
        }
    }
}

impl FromStr for ApprovalDecision {
    type Err = ProposalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "APPROVE" => Ok(Self::Approve),
            "DENY" => Ok(Self::Deny),
            _ => Err(ProposalError::Validation(format!(
                "invalid approval decision `{s}`: expected APPROVE or DENY"
            ))),
        }
    }
}

/// Authenticated control-plane principal invoking proposal or admin operations (`ING-003`, `ING-050`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminCaller {
    /// Principal subject identifier (non-empty).
    pub principal: String,
    /// Granted authorization scopes (`admin:ingest`, `admin:schema`, ...).
    pub scopes: BTreeSet<String>,
}

impl AdminCaller {
    /// Constructs an [`AdminCaller`] with explicit scopes.
    #[must_use]
    pub fn new<I, S>(principal: impl Into<String>, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            principal: principal.into(),
            scopes: scopes.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns `true` iff the principal holds `scope`.
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

/// Config-based interim approval policy (`WP-M4-07`, `ING-003`, `ING-051`; Cedar replaces/extends in M6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPolicyConfig {
    /// Allow `T0` sources to skip `AWAITING_APPROVAL` when manifest verified & non-breaking & non-degraded (`ING-003`).
    pub auto_activate_t0: bool,
    /// Require two distinct `admin:ingest` principals when activating a `T2` source (`ING-051`).
    pub require_two_person_for_t2: bool,
    /// Require two distinct `admin:ingest` principals when proposal adds cross-trust joins (`ING-051`).
    pub require_two_person_for_cross_trust: bool,
    /// Require two distinct `admin:ingest` principals when source contains mutating ops (`ING-051`).
    pub require_two_person_for_mutating: bool,
    /// Maximum age of an `AWAITING_APPROVAL` proposal before TTL rejection (`spec/05-ingestion.md` §1).
    pub approval_ttl_secs: u64,
}

impl Default for ApprovalPolicyConfig {
    fn default() -> Self {
        Self {
            auto_activate_t0: false,
            require_two_person_for_t2: true,
            require_two_person_for_cross_trust: true,
            require_two_person_for_mutating: true,
            approval_ttl_secs: PROPOSAL_APPROVAL_TTL_SECS,
        }
    }
}

/// Single verification gate evidence entry recorded in `ProposalReport` (`ING-001`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateCheckResult {
    /// Gate identifier (`G1`..`G9`).
    pub gate: String,
    /// Whether the gate passed.
    pub passed: bool,
    /// Sanitized evidence summary (must not contain raw secrets).
    pub evidence: String,
}

/// Structured report attached to a proposal at `STAGED` or `FAILED` (`ING-001`, `spec/05-ingestion.md` §1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposalReport {
    /// Canonical input URL or path.
    pub source: String,
    /// SHA-256 hex digest of extracted input bytes (`spec_hash`).
    pub spec_hash: String,
    /// Deterministic extractor/tool versions keyed by tool identifier.
    pub tool_versions: BTreeMap<String, String>,
    /// Model identifiers consulted during optional enrichment (if any).
    pub model_ids: Vec<String>,
    /// Ordered gate results `G1..G9` with sanitized evidence.
    pub gate_results: Vec<GateCheckResult>,
    /// Human-readable summary of schema/descriptor diffs vs previous generation.
    pub diffs_summary: Option<String>,
    /// `true` iff LLM enrichment degraded to `NullEnricher` (`ING-035`; blocks auto-activation).
    pub enrichment_degraded: bool,
    /// `true` iff breaking schema/behaviour change was detected (`ING-051`, `ING-052`).
    pub breaking: bool,
    /// `true` iff proposal introduces mutating operations (`ING-051`).
    pub mutating: bool,
    /// `true` iff proposal introduces cross-trust joins (`ING-051`).
    pub cross_trust_join: bool,
    /// `true` iff `T0` signed manifest verification succeeded (`ING-003`, `SEC-060`).
    pub manifest_verified: bool,
}

impl ProposalReport {
    /// Encodes the report as a deterministic [`CisgValue`].
    #[must_use]
    pub fn to_cisg_value(&self) -> CisgValue {
        let mut root = BTreeMap::new();
        root.insert("source".to_owned(), CisgValue::String(self.source.clone()));
        root.insert(
            "spec_hash".to_owned(),
            CisgValue::String(self.spec_hash.clone()),
        );

        let mut tv = BTreeMap::new();
        for (k, v) in &self.tool_versions {
            tv.insert(k.clone(), CisgValue::String(v.clone()));
        }
        root.insert("tool_versions".to_owned(), CisgValue::Object(tv));

        root.insert(
            "model_ids".to_owned(),
            CisgValue::Array(
                self.model_ids
                    .iter()
                    .map(|m| CisgValue::String(m.clone()))
                    .collect(),
            ),
        );

        let gates = self
            .gate_results
            .iter()
            .map(|g| {
                let mut obj = BTreeMap::new();
                obj.insert("gate".to_owned(), CisgValue::String(g.gate.clone()));
                obj.insert("passed".to_owned(), CisgValue::Bool(g.passed));
                obj.insert("evidence".to_owned(), CisgValue::String(g.evidence.clone()));
                CisgValue::Object(obj)
            })
            .collect();
        root.insert("gate_results".to_owned(), CisgValue::Array(gates));

        if let Some(diffs) = &self.diffs_summary {
            root.insert("diffs_summary".to_owned(), CisgValue::String(diffs.clone()));
        }
        root.insert(
            "enrichment_degraded".to_owned(),
            CisgValue::Bool(self.enrichment_degraded),
        );
        root.insert("breaking".to_owned(), CisgValue::Bool(self.breaking));
        root.insert("mutating".to_owned(), CisgValue::Bool(self.mutating));
        root.insert(
            "cross_trust_join".to_owned(),
            CisgValue::Bool(self.cross_trust_join),
        );
        root.insert(
            "manifest_verified".to_owned(),
            CisgValue::Bool(self.manifest_verified),
        );
        CisgValue::Object(root)
    }

    /// Serializes the report to canonical RFC 8785 JSON.
    pub fn to_canonical_json(&self) -> Result<String, ProposalError> {
        self.to_cisg_value()
            .to_canonical_json()
            .map_err(|e| ProposalError::Validation(format!("invalid proposal report: {e}")))
    }

    /// Decodes a [`ProposalReport`] from JSON stored in the context store.
    pub fn from_json_str(raw: &str) -> Result<Self, ProposalError> {
        let CisgValue::Object(obj) = parse_cisg_json(raw)? else {
            return Err(ProposalError::Corrupt(
                "proposal report JSON must be an object".to_owned(),
            ));
        };
        let source = obj_str(&obj, "source").unwrap_or_default().to_owned();
        let spec_hash = obj_str(&obj, "spec_hash").unwrap_or_default().to_owned();

        let mut tool_versions = BTreeMap::new();
        if let Some(CisgValue::Object(tv)) = obj.get("tool_versions") {
            for (k, v) in tv {
                if let CisgValue::String(sv) = v {
                    tool_versions.insert(k.clone(), sv.clone());
                }
            }
        }

        let mut model_ids = Vec::new();
        if let Some(CisgValue::Array(arr)) = obj.get("model_ids") {
            for item in arr {
                if let CisgValue::String(s) = item {
                    model_ids.push(s.clone());
                }
            }
        }

        let mut gate_results = Vec::new();
        if let Some(CisgValue::Array(arr)) = obj.get("gate_results") {
            for item in arr {
                if let CisgValue::Object(gobj) = item {
                    gate_results.push(GateCheckResult {
                        gate: obj_str(gobj, "gate").unwrap_or_default().to_owned(),
                        passed: obj_bool(gobj, "passed").unwrap_or(false),
                        evidence: obj_str(gobj, "evidence").unwrap_or_default().to_owned(),
                    });
                }
            }
        }

        Ok(Self {
            source,
            spec_hash,
            tool_versions,
            model_ids,
            gate_results,
            diffs_summary: obj_str(&obj, "diffs_summary").map(str::to_owned),
            enrichment_degraded: obj_bool(&obj, "enrichment_degraded").unwrap_or(false),
            breaking: obj_bool(&obj, "breaking").unwrap_or(false),
            mutating: obj_bool(&obj, "mutating").unwrap_or(false),
            cross_trust_join: obj_bool(&obj, "cross_trust_join").unwrap_or(false),
            manifest_verified: obj_bool(&obj, "manifest_verified").unwrap_or(false),
        })
    }
}

fn obj_str<'a>(obj: &'a BTreeMap<String, CisgValue>, key: &str) -> Option<&'a str> {
    match obj.get(key) {
        Some(CisgValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn obj_bool(obj: &BTreeMap<String, CisgValue>, key: &str) -> Option<bool> {
    match obj.get(key) {
        Some(CisgValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Input accepted by `gapMutation.proposeSource` (`spec/02-gap-ql.md` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposeSourceInput {
    /// Upstream protocol kind.
    pub kind: Protocol,
    /// Raw source URL or filesystem path.
    pub location: String,
    /// Optional target branch slug.
    pub branch: Option<BranchId>,
    /// Requested trust tier (`spec/02-gap-ql.md` defaults to `T2`).
    pub trust: TrustTier,
    /// Optional caller notes or discovery evidence.
    pub notes: Option<String>,
}

/// Pagination and status filter for `gap.proposals` (`spec/02-gap-ql.md` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalFilter {
    /// Optional status allow-list (`None` returns all statuses).
    pub statuses: Option<Vec<ProposalStatus>>,
    /// Requested page size (`1..=1,000`, defaults to `20`).
    pub first: u32,
    /// Optional signed continuation cursor.
    pub after: Option<String>,
}

impl Default for ProposalFilter {
    fn default() -> Self {
        Self {
            statuses: None,
            first: DEFAULT_PROPOSALS_PAGE_SIZE,
            after: None,
        }
    }
}

/// Relay-style pagination metadata (`spec/02-gap-ql.md` `PageInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    /// Whether additional records exist after this page.
    pub has_next_page: bool,
    /// Signed cursor referencing the final node in this page.
    pub end_cursor: Option<String>,
}

/// Paginated list of proposals returned by `gap.proposals` (`spec/02-gap-ql.md` §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalConnection {
    /// Ordered proposals for this page.
    pub nodes: Vec<Proposal>,
    /// Cursor metadata.
    pub page_info: PageInfo,
}

/// Persisted ingestion proposal (`spec/02-gap-ql.md` §3 `Proposal`, `ING-001`..`ING-003`, `ING-050`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Unique RFC 9562 UUIDv7 proposal identifier.
    pub id: ProposalId,
    /// Owning tenant identifier (`INV-09`).
    pub tenant: TenantId,
    /// Current 13-state FSM status.
    pub status: ProposalStatus,
    /// Associated branch slug if assigned.
    pub branch: Option<BranchId>,
    /// Protocol kind.
    pub kind: Protocol,
    /// Canonicalized location used as part of the `(tenant, canonical_location, kind)` idempotency key (`ING-002`).
    pub location: String,
    /// Trust tier (`T0`, `T1`, `T2`).
    pub trust: TrustTier,
    /// Caller / discovery notes or transition rationale.
    pub notes: Option<String>,
    /// Structured verification report (`ING-001`).
    pub report: Option<ProposalReport>,
    /// Distinct `admin:ingest` principals who approved at `AWAITING_APPROVAL` (`ING-051` two-person tracking).
    pub approvals: Vec<String>,
    /// Shadow snapshot identifier built at `STAGED` (hidden from normal traffic until `ACTIVE`, `ING-050`).
    pub shadow_snapshot_id: Option<String>,
    /// Creation UTC timestamp (`RFC 3339`).
    pub created_at: Timestamp,
    /// Last transition UTC timestamp (`RFC 3339`).
    pub updated_at: Timestamp,
}

/// Typed errors returned by proposal FSM, persistence, discovery worker, and CLI (`INV-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalError {
    /// Invalid input or illegal state machine transition.
    Validation(String),
    /// Referenced proposal does not exist in caller's tenant.
    NotFound(ProposalId),
    /// Caller lacks required scope (`admin:ingest` / `admin:schema`).
    Forbidden(String),
    /// Approval requires explicit `breaking_change_ack = true` (`ING-051`).
    BreakingChangeAckRequired(ProposalId),
    /// Pending approval TTL (`14 days`) expired; proposal transitioned to `REJECTED`.
    ApprovalExpired(ProposalId),
    /// Backing storage operation failed.
    Store(StoreError),
    /// Cursor or core validation error.
    Core(GapError),
    /// Persisted proposal or audit record is corrupt.
    Corrupt(String),
}

impl fmt::Display for ProposalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
            Self::NotFound(id) => write!(f, "proposal `{id}` not found"),
            Self::Forbidden(msg) => write!(f, "forbidden: {msg}"),
            Self::BreakingChangeAckRequired(id) => {
                write!(
                    f,
                    "breaking change acknowledgement required before promoting `{id}`"
                )
            }
            Self::ApprovalExpired(id) => {
                write!(
                    f,
                    "approval window expired for proposal `{id}`; moved to REJECTED"
                )
            }
            Self::Store(err) => write!(f, "context store failure: {err}"),
            Self::Core(err) => write!(f, "protocol error: {err}"),
            Self::Corrupt(msg) => write!(f, "corrupt proposal state: {msg}"),
        }
    }
}

impl StdError for ProposalError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Core(err) => Some(err),
            _ => None,
        }
    }
}

impl From<StoreError> for ProposalError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<GapError> for ProposalError {
    fn from(value: GapError) -> Self {
        Self::Core(value)
    }
}

impl ProposalError {
    /// Maps this domain error into a canonical [`GapError`] (`spec/02-gap-ql.md` §10).
    #[must_use]
    pub fn to_gap_error(&self, correlation_id: impl Into<String>) -> GapError {
        let cid = correlation_id.into();
        match self {
            Self::Validation(msg) => {
                GapError::new(ErrorCode::ValidationError, msg.clone()).with_correlation_id(cid)
            }
            Self::NotFound(_) => GapError::new(ErrorCode::ValidationError, "proposal not found")
                .with_correlation_id(cid),
            Self::Forbidden(msg) => {
                GapError::new(ErrorCode::Forbidden, msg.clone()).with_correlation_id(cid)
            }
            Self::BreakingChangeAckRequired(id) => GapError::new(
                ErrorCode::ValidationError,
                format!("breakingChangeAck=true is required to approve proposal `{id}`"),
            )
            .with_correlation_id(cid),
            Self::ApprovalExpired(id) => GapError::new(
                ErrorCode::ApprovalRequired,
                format!("approval window expired for proposal `{id}`; moved to REJECTED"),
            )
            .with_correlation_id(cid),
            Self::Store(err) => err.clone().into_gap_error(cid),
            Self::Core(err) => err.clone().with_correlation_id(cid),
            Self::Corrupt(_) => GapError::new(ErrorCode::Internal, "corrupt proposal state")
                .with_correlation_id(cid),
        }
    }
}
