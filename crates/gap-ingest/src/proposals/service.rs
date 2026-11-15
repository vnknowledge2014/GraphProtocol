//! Persisted ingestion proposal service and 13-state FSM (`WP-M4-07`; `ING-001`..`ING-003`,
//! `ING-035`, `ING-050`, `ING-051`, `INV-09`, `INV-14`, `RUN-025`, `SEC-090`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gap_core::clock::{Clock, Timestamp};
use gap_core::cursor::{CursorBinding, CursorEnvelope, CursorKey, sign_cursor, verify_cursor};
use gap_core::ids::{BranchId, ProposalId, TenantId, TenantScope};
use gap_core::snapshot::{Protocol, TrustTier};
use gap_store::audit::{AuditChain, AuditEntry, AuditLog};
use gap_store::store::ContextStore;
use gap_store::types::{
    Caller, Direction, EdgeId, EdgeResolution, EdgeType, MAX_TRAVERSE_FIRST, NewEdge, NewNode,
    Node, NodeId, NodeType, Provenance, ScopeSpec, TraverseQuery, TraverseStrategy,
};

use crate::cisg::{CisgValue, sha256_hex};
use crate::proposals::audit_adapter::{ContextStoreAuditLog, parse_cisg_json};
use crate::proposals::types::{
    ADMIN_INGEST_SCOPE, ADMIN_SCHEMA_SCOPE, AdminCaller, ApprovalDecision, ApprovalPolicyConfig,
    MAX_PROPOSALS_PAGE_SIZE, PageInfo, Proposal, ProposalConnection, ProposalError, ProposalFilter,
    ProposalReport, ProposalStatus, ProposeSourceInput,
};

const INDEX_ROOT_ID: &str = "proposal_index:root";
const INDEX_ROOT_NODE_TYPE: &str = "proposal_index";
const PROPOSAL_NODE_TYPE: &str = "proposal";
const HAS_PROPOSAL_EDGE_TYPE: &str = "HAS_PROPOSAL";
const PROPOSAL_PROVENANCE_SOURCE: &str = "gap-ingest.proposals";
const PROPOSAL_WRITER_PRINCIPAL: &str = "gap-ingest-proposals";
const PROPOSAL_CURSOR_FAMILY: &str = "gap.proposals";
const PROPOSAL_CURSOR_SCHEMA_HASH: &str = "proposal.v1";

static PROPOSAL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Formats [`Protocol`] using GraphQL / CLI uppercase convention (`spec/02-gap-ql.md` §3).
#[must_use]
pub const fn format_protocol(kind: Protocol) -> &'static str {
    match kind {
        Protocol::Sql => "SQL",
        Protocol::Rest => "REST",
        Protocol::Grpc => "GRPC",
        Protocol::Graphql => "GRAPHQL",
        Protocol::Mcp => "MCP",
        Protocol::A2a => "A2A",
        Protocol::Cli => "CLI",
        Protocol::CodeGraph => "CODE_GRAPH",
        Protocol::Custom => "CUSTOM",
    }
}

/// Parses a protocol string (`SQL`, `REST`, `GRPC`, `GRAPHQL`, `MCP`, `A2A`, `CLI`, `CODE_GRAPH`, `CUSTOM`).
pub fn parse_protocol(raw: &str) -> Result<Protocol, ProposalError> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "SQL" => Ok(Protocol::Sql),
        "REST" => Ok(Protocol::Rest),
        "GRPC" => Ok(Protocol::Grpc),
        "GRAPHQL" => Ok(Protocol::Graphql),
        "MCP" => Ok(Protocol::Mcp),
        "A2A" => Ok(Protocol::A2a),
        "CLI" => Ok(Protocol::Cli),
        "CODE_GRAPH" | "CODE-GRAPH" | "CODEGRAPH" => Ok(Protocol::CodeGraph),
        "CUSTOM" => Ok(Protocol::Custom),
        other => Err(ProposalError::Validation(format!(
            "unknown protocol kind `{other}`"
        ))),
    }
}

/// Formats [`TrustTier`] using GraphQL / CLI convention (`T0`, `T1`, `T2`).
#[must_use]
pub const fn format_trust_tier(tier: TrustTier) -> &'static str {
    match tier {
        TrustTier::T0 => "T0",
        TrustTier::T1 => "T1",
        TrustTier::T2 => "T2",
    }
}

/// Parses a trust tier string (`T0`, `T1`, `T2`).
pub fn parse_trust_tier(raw: &str) -> Result<TrustTier, ProposalError> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "T0" => Ok(TrustTier::T0),
        "T1" => Ok(TrustTier::T1),
        "T2" => Ok(TrustTier::T2),
        other => Err(ProposalError::Validation(format!(
            "unknown trust tier `{other}` (expected T0, T1, or T2)"
        ))),
    }
}

/// Canonicalizes a source URI or filesystem path for `(tenant, location, kind)` idempotency (`ING-002`).
///
/// For URIs (`scheme://...`), lowercases scheme and host, strips default ports (`:80` on `http`,
/// `:443` on `https`), drops `#fragment`, collapses redundant slashes and `.` segments in the
/// path, rejects `..` path traversal, and trims trailing slashes on non-root paths.
/// For filesystem paths, converts `\` to `/`, collapses redundant slashes and `.` segments,
/// rejects `..` traversal, and trims trailing slashes unless `/`.
pub fn canonicalize_location(raw: &str) -> Result<String, ProposalError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProposalError::Validation(
            "proposal location must not be empty".to_owned(),
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(ProposalError::Validation(
            "proposal location must not contain control characters".to_owned(),
        ));
    }

    if let Some(scheme_end) = trimmed.find("://") {
        canonicalize_uri(trimmed, scheme_end)
    } else {
        canonicalize_fs_path(trimmed)
    }
}

fn canonicalize_uri(trimmed: &str, scheme_end: usize) -> Result<String, ProposalError> {
    let scheme_raw = &trimmed[..scheme_end];
    if scheme_raw.is_empty()
        || !scheme_raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return Err(ProposalError::Validation(format!(
            "invalid URI scheme in proposal location `{trimmed}`"
        )));
    }
    let scheme = scheme_raw.to_ascii_lowercase();
    let after_scheme = &trimmed[scheme_end + 3..];
    let without_fragment = after_scheme.split('#').next().unwrap_or(after_scheme);
    let (before_query, query_opt) = match without_fragment.split_once('?') {
        Some((before, query)) => (before, Some(query)),
        None => (without_fragment, None),
    };
    let (authority_raw, path_raw) = match before_query.find('/') {
        Some(idx) => (&before_query[..idx], &before_query[idx..]),
        None => (before_query, ""),
    };

    if authority_raw.is_empty() && scheme != "file" {
        return Err(ProposalError::Validation(format!(
            "URI proposal location `{trimmed}` is missing host authority"
        )));
    }
    let authority = canonicalize_authority(&scheme, authority_raw)?;
    let norm_path = normalize_uri_path(path_raw)?;

    let mut out = String::with_capacity(trimmed.len());
    out.push_str(&scheme);
    out.push_str("://");
    out.push_str(&authority);
    if scheme == "file" {
        if norm_path.is_empty() {
            out.push('/');
        } else {
            out.push_str(&norm_path);
        }
    } else if !norm_path.is_empty() && norm_path != "/" {
        out.push_str(&norm_path);
    }
    if let Some(query) = query_opt
        && !query.is_empty()
    {
        out.push('?');
        out.push_str(query);
    }
    Ok(out)
}

fn canonicalize_authority(scheme: &str, authority_raw: &str) -> Result<String, ProposalError> {
    if authority_raw.is_empty() {
        return Ok(String::new());
    }
    let (userinfo_opt, hostport) = match authority_raw.rsplit_once('@') {
        Some((userinfo, hp)) => (Some(userinfo), hp),
        None => (None, authority_raw),
    };
    if hostport.is_empty() {
        return Err(ProposalError::Validation(
            "URI host authority must not be empty".to_owned(),
        ));
    }

    let hostport_lower = if hostport.starts_with('[') {
        if let Some(close) = hostport.find(']') {
            let addr = hostport[..=close].to_ascii_lowercase();
            let port_part = &hostport[close + 1..];
            strip_default_port(scheme, &addr, port_part)?
        } else {
            return Err(ProposalError::Validation(
                "unterminated IPv6 literal in URI authority".to_owned(),
            ));
        }
    } else if let Some((host, port)) = hostport.rsplit_once(':') {
        if host.contains(':') {
            return Err(ProposalError::Validation(
                "invalid host:port syntax in URI authority".to_owned(),
            ));
        }
        strip_default_port(scheme, &host.to_ascii_lowercase(), &format!(":{port}"))?
    } else {
        hostport.to_ascii_lowercase()
    };

    match userinfo_opt {
        Some(userinfo) => Ok(format!("{userinfo}@{hostport_lower}")),
        None => Ok(hostport_lower),
    }
}

fn strip_default_port(scheme: &str, host: &str, port_part: &str) -> Result<String, ProposalError> {
    if host.is_empty() {
        return Err(ProposalError::Validation(
            "URI host must not be empty".to_owned(),
        ));
    }
    if port_part.is_empty() {
        return Ok(host.to_owned());
    }
    let Some(port_digits) = port_part.strip_prefix(':') else {
        return Err(ProposalError::Validation(format!(
            "unexpected authority suffix `{port_part}`"
        )));
    };
    if port_digits.is_empty() || !port_digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProposalError::Validation(format!(
            "invalid URI port `{port_digits}`"
        )));
    }
    let port_num: u16 = port_digits
        .parse()
        .map_err(|_| ProposalError::Validation(format!("port out of range `{port_digits}`")))?;
    let is_default = (scheme == "http" && port_num == 80)
        || (scheme == "https" && port_num == 443)
        || (scheme == "ws" && port_num == 80)
        || (scheme == "wss" && port_num == 443);
    if is_default {
        Ok(host.to_owned())
    } else {
        Ok(format!("{host}:{port_num}"))
    }
}

fn normalize_uri_path(path_raw: &str) -> Result<String, ProposalError> {
    if path_raw.is_empty() || path_raw == "/" {
        return Ok(String::new());
    }
    let mut segments: Vec<&str> = Vec::new();
    for seg in path_raw.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(ProposalError::Validation(
                "parent directory segment `..` is not allowed in proposal location".to_owned(),
            ));
        }
        segments.push(seg);
    }
    if segments.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("/{}", segments.join("/")))
    }
}

fn canonicalize_fs_path(raw: &str) -> Result<String, ProposalError> {
    let slash_norm = raw.replace('\\', "/");
    let is_abs = slash_norm.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for seg in slash_norm.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(ProposalError::Validation(
                "parent directory segment `..` is not allowed in proposal path".to_owned(),
            ));
        }
        segments.push(seg);
    }
    if is_abs {
        if segments.is_empty() {
            Ok("/".to_owned())
        } else {
            Ok(format!("/{}", segments.join("/")))
        }
    } else if segments.is_empty() {
        Ok(".".to_owned())
    } else {
        Ok(segments.join("/"))
    }
}

/// Validates an `X-Gap-Snapshot: shadow:<proposal_id>` preview request (`ING-050`).
///
/// Requires `admin:schema`, matching `proposal.id`, an active shadow lifecycle status
/// (`STAGED`, `AWAITING_APPROVAL`, or `PROMOTING`), and a recorded `shadow_snapshot_id`.
pub fn authorize_shadow_header(
    header_val: &str,
    caller: &AdminCaller,
    proposal: &Proposal,
) -> Result<String, ProposalError> {
    let trimmed = header_val.trim();
    let raw_id = trimmed.strip_prefix("shadow:").ok_or_else(|| {
        ProposalError::Validation(
            "X-Gap-Snapshot header must have form `shadow:<proposal_id>`".to_owned(),
        )
    })?;
    let parsed_id = ProposalId::parse(raw_id).map_err(|_| {
        ProposalError::Validation(format!("invalid proposal id in shadow header `{raw_id}`"))
    })?;

    if !caller.has_scope(ADMIN_SCHEMA_SCOPE) {
        return Err(ProposalError::Forbidden(
            "scope `admin:schema` is required to query a shadow snapshot".to_owned(),
        ));
    }
    if proposal.id != parsed_id {
        return Err(ProposalError::Validation(format!(
            "shadow header targets proposal `{parsed_id}` rather than `{}`",
            proposal.id
        )));
    }
    if !matches!(
        proposal.status,
        ProposalStatus::Staged | ProposalStatus::AwaitingApproval | ProposalStatus::Promoting
    ) {
        return Err(ProposalError::Validation(format!(
            "proposal `{}` in status `{}` does not expose a shadow snapshot",
            proposal.id,
            proposal.status.as_str()
        )));
    }
    proposal.shadow_snapshot_id.clone().ok_or_else(|| {
        ProposalError::Validation(format!(
            "proposal `{}` has no recorded shadow snapshot",
            proposal.id
        ))
    })
}

/// Persistent lifecycle manager for ingestion proposals (`ING-001`..`ING-003`, `ING-050`, `ING-051`).
///
/// Backed by any tenant-isolated [`ContextStore`] and [`AuditLog`]. Uses control-plane serialization
/// (`Mutex<()>`) outside the query hot path so `(tenant, location, kind)` idempotency and multi-person
/// approval tallies remain atomic across concurrent administrative calls.
#[derive(Clone)]
pub struct ProposalService {
    store: Arc<dyn ContextStore>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    policy: ApprovalPolicyConfig,
    cursor_key: CursorKey,
    mutation_guard: Arc<Mutex<()>>,
    transitions_total: Arc<Mutex<BTreeMap<(ProposalStatus, ProposalStatus), u64>>>,
}

impl std::fmt::Debug for ProposalService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProposalService")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl ProposalService {
    /// Creates a [`ProposalService`] with an explicit [`ContextStore`] and [`AuditLog`].
    #[must_use]
    pub fn new(
        store: Arc<dyn ContextStore>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
        policy: ApprovalPolicyConfig,
        cursor_key: CursorKey,
    ) -> Self {
        Self {
            store,
            audit,
            clock,
            policy,
            cursor_key,
            mutation_guard: Arc::new(Mutex::new(())),
            transitions_total: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Creates a [`ProposalService`] backed by [`ContextStoreAuditLog`] in the same `store`.
    pub fn with_store_audit(
        store: Arc<dyn ContextStore>,
        clock: Arc<dyn Clock>,
        policy: ApprovalPolicyConfig,
        cursor_key: CursorKey,
    ) -> Result<Self, ProposalError> {
        let audit = Arc::new(ContextStoreAuditLog::new(
            Arc::clone(&store),
            Arc::clone(&clock),
        )?);
        Ok(Self::new(store, audit, clock, policy, cursor_key))
    }

    /// Returns a reference to the configured approval policy.
    #[must_use]
    pub const fn policy(&self) -> &ApprovalPolicyConfig {
        &self.policy
    }

    /// Returns the observed transition counter matching `gap_ingest_transitions_total{from,to}` (`ING-001`).
    #[must_use]
    pub fn transition_count(&self, from: ProposalStatus, to: ProposalStatus) -> u64 {
        let counts = self
            .transitions_total
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counts.get(&(from, to)).copied().unwrap_or(0)
    }

    /// Proposes a new source or returns the existing open proposal with matching `(tenant, canonical_location, kind)` (`ING-002`).
    pub fn propose_source(
        &self,
        tenant: &TenantScope,
        actor: &str,
        input: ProposeSourceInput,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let canonical = canonicalize_location(&input.location)?;
        if let Some(notes) = &input.notes
            && notes.chars().any(char::is_control)
        {
            return Err(ProposalError::Validation(
                "proposal notes must not contain control characters".to_owned(),
            ));
        }

        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let now = self.clock.now_timestamp();
        let _ = self.expire_stale_locked(tenant, actor_clean, now)?;

        let existing = self.load_all_proposals_locked(tenant)?;
        if let Some(open) = existing
            .into_iter()
            .find(|p| p.status.is_open() && p.kind == input.kind && p.location == canonical)
        {
            self.record_audit(
                tenant,
                actor_clean,
                "proposal.propose_source",
                &format!("proposal:{}", open.id),
                "DEDUPLICATED",
                &[
                    ("id".to_owned(), CisgValue::String(open.id.to_string())),
                    (
                        "kind".to_owned(),
                        CisgValue::String(format_protocol(open.kind).to_owned()),
                    ),
                    ("location".to_owned(), CisgValue::String(canonical)),
                    (
                        "status".to_owned(),
                        CisgValue::String(open.status.as_str().to_owned()),
                    ),
                ],
                now,
            )?;
            return Ok(open);
        }

        let id = mint_proposal_id(now);
        let proposal = Proposal {
            id,
            tenant: tenant.tenant().clone(),
            status: ProposalStatus::Proposed,
            branch: input.branch,
            kind: input.kind,
            location: canonical.clone(),
            trust: input.trust,
            notes: input.notes,
            report: None,
            approvals: Vec::new(),
            shadow_snapshot_id: None,
            created_at: now,
            updated_at: now,
        };
        self.persist_new_proposal(tenant, actor_clean, &proposal)?;
        self.record_audit(
            tenant,
            actor_clean,
            "proposal.propose_source",
            &format!("proposal:{}", proposal.id),
            "CREATED",
            &[
                ("id".to_owned(), CisgValue::String(proposal.id.to_string())),
                (
                    "kind".to_owned(),
                    CisgValue::String(format_protocol(proposal.kind).to_owned()),
                ),
                ("location".to_owned(), CisgValue::String(canonical)),
                (
                    "status".to_owned(),
                    CisgValue::String("PROPOSED".to_owned()),
                ),
                (
                    "trust".to_owned(),
                    CisgValue::String(format_trust_tier(proposal.trust).to_owned()),
                ),
            ],
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `PROPOSED -> EXTRACTING`, enforcing `admin:ingest` when `trust == T2` (`ING-003`).
    pub fn begin_extracting(
        &self,
        tenant: &TenantScope,
        caller: Option<&AdminCaller>,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;

        let actor = match (proposal.trust, caller) {
            (TrustTier::T2, Some(admin)) => {
                if !admin.has_scope(ADMIN_INGEST_SCOPE) {
                    return Err(ProposalError::Forbidden(
                        "scope `admin:ingest` is required before extracting a T2 proposal"
                            .to_owned(),
                    ));
                }
                admin.principal.clone()
            }
            (TrustTier::T2, None) => {
                return Err(ProposalError::Forbidden(
                    "explicit `admin:ingest` caller is required before extracting a T2 proposal"
                        .to_owned(),
                ));
            }
            (_, Some(admin)) => admin.principal.clone(),
            (_, None) => "system:ingestion-worker".to_owned(),
        };

        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Extracting,
            &actor,
            "extract_started",
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `EXTRACTING -> ENRICHING` (`spec/05-ingestion.md` §1).
    pub fn begin_enriching(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Enriching,
            actor_clean,
            "enrich_started",
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `ENRICHING -> VERIFYING` (`spec/05-ingestion.md` §1).
    pub fn begin_verifying(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Verifying,
            actor_clean,
            "verify_started",
            now,
        )?;
        Ok(proposal)
    }

    /// Records verification report + shadow snapshot (`VERIFYING -> STAGED`) and pauses at `STAGED`
    /// without evaluating auto-promotion yet (`ING-050`, `ING-001`).
    pub fn record_staged(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
        shadow_snapshot_id: String,
        report: ProposalReport,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let shadow_clean =
            validate_non_empty(&shadow_snapshot_id, "shadow_snapshot_id")?.to_owned();
        if report.gate_results.iter().any(|g| !g.passed) {
            return Err(ProposalError::Validation(
                "cannot stage proposal with failing verification gate(s); call fail_proposal"
                    .to_owned(),
            ));
        }

        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        proposal.shadow_snapshot_id = Some(shadow_clean);
        proposal.report = Some(report);
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Staged,
            actor_clean,
            "verification_staged",
            now,
        )?;
        Ok(proposal)
    }

    /// Evaluates approval policy from `STAGED`: auto-promotes eligible `T0` proposals
    /// (`STAGED -> PROMOTING`), or parks for `admin:ingest` (`STAGED -> AWAITING_APPROVAL`)
    /// (`ING-003`, `ING-035`, `ING-051`).
    pub fn evaluate_staged_policy(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        if proposal.status != ProposalStatus::Staged {
            return Err(ProposalError::Validation(format!(
                "proposal `{}` must be in STAGED to evaluate policy, found `{}`",
                proposal.id,
                proposal.status.as_str()
            )));
        }
        let now = self.clock.now_timestamp();
        let next = self.select_post_stage_status(&proposal);
        let rationale = if next == ProposalStatus::Promoting {
            "policy_t0_auto_activate"
        } else {
            "policy_awaiting_approval"
        };
        self.transition_locked(tenant, &mut proposal, next, actor_clean, rationale, now)?;
        Ok(proposal)
    }

    /// Convenience helper combining `VERIFYING -> STAGED` and immediate policy routing to
    /// `PROMOTING` (eligible signed non-breaking `T0` with intact enrichment) or
    /// `AWAITING_APPROVAL` (`ING-001`, `ING-003`, `ING-035`, `ING-050`, `ING-051`).
    pub fn stage_verified(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
        shadow_snapshot_id: String,
        report: ProposalReport,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let shadow_clean =
            validate_non_empty(&shadow_snapshot_id, "shadow_snapshot_id")?.to_owned();
        if report.gate_results.iter().any(|g| !g.passed) {
            return Err(ProposalError::Validation(
                "cannot stage proposal with failing verification gate(s); call fail_proposal"
                    .to_owned(),
            ));
        }

        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        proposal.shadow_snapshot_id = Some(shadow_clean);
        proposal.report = Some(report);

        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Staged,
            actor_clean,
            "verification_staged",
            now,
        )?;
        let next = self.select_post_stage_status(&proposal);
        let rationale = if next == ProposalStatus::Promoting {
            "policy_t0_auto_activate"
        } else {
            "policy_awaiting_approval"
        };
        self.transition_locked(tenant, &mut proposal, next, actor_clean, rationale, now)?;
        Ok(proposal)
    }

    /// Transitions an active ingestion stage (`EXTRACTING`, `ENRICHING`, `VERIFYING`, `PROMOTING`) to `FAILED`.
    pub fn fail_proposal(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
        reason: &str,
        report: Option<ProposalReport>,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let reason_clean = validate_non_empty(reason, "reason")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        if let Some(rep) = report {
            proposal.report = Some(rep);
        }
        proposal.notes = Some(reason_clean.to_owned());
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Failed,
            actor_clean,
            reason_clean,
            now,
        )?;
        Ok(proposal)
    }

    /// Records an administrative `APPROVE` or `DENY` decision (`ING-003`, `ING-051`, `spec/02-gap-ql.md` §3).
    pub fn decide_proposal(
        &self,
        tenant: &TenantScope,
        caller: &AdminCaller,
        id: ProposalId,
        decision: ApprovalDecision,
        breaking_change_ack: bool,
        reason: Option<String>,
    ) -> Result<Proposal, ProposalError> {
        if !caller.has_scope(ADMIN_INGEST_SCOPE) {
            return Err(ProposalError::Forbidden(
                "scope `admin:ingest` is required to decide proposals".to_owned(),
            ));
        }

        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        let now = self.clock.now_timestamp();

        if self.expire_one_if_stale_locked(tenant, &mut proposal, &caller.principal, now)? {
            return Err(ProposalError::ApprovalExpired(id));
        }

        match decision {
            ApprovalDecision::Deny => {
                if !matches!(
                    proposal.status,
                    ProposalStatus::Proposed | ProposalStatus::AwaitingApproval
                ) {
                    return Err(ProposalError::Validation(format!(
                        "cannot DENY proposal `{}` in status `{}`",
                        proposal.id,
                        proposal.status.as_str()
                    )));
                }
                if let Some(r) = reason
                    && !r.trim().is_empty()
                {
                    proposal.notes = Some(r);
                }
                self.transition_locked(
                    tenant,
                    &mut proposal,
                    ProposalStatus::Rejected,
                    &caller.principal,
                    "admin_denied",
                    now,
                )?;
                Ok(proposal)
            }
            ApprovalDecision::Approve => {
                if proposal.status != ProposalStatus::AwaitingApproval {
                    return Err(ProposalError::Validation(format!(
                        "proposal `{}` in status `{}` cannot receive promotion approval; expected AWAITING_APPROVAL",
                        proposal.id,
                        proposal.status.as_str()
                    )));
                }
                if proposal.report.as_ref().is_some_and(|r| r.breaking) && !breaking_change_ack {
                    return Err(ProposalError::BreakingChangeAckRequired(id));
                }

                let required = self.required_approvals(&proposal);
                if !proposal.approvals.iter().any(|p| p == &caller.principal) {
                    proposal.approvals.push(caller.principal.clone());
                }
                if let Some(r) = reason
                    && !r.trim().is_empty()
                {
                    proposal.notes = Some(r);
                }
                proposal.updated_at = now;

                if proposal.approvals.len() >= required {
                    self.transition_locked(
                        tenant,
                        &mut proposal,
                        ProposalStatus::Promoting,
                        &caller.principal,
                        "approval_threshold_met",
                        now,
                    )?;
                } else {
                    self.persist_updated_proposal(tenant, &caller.principal, &proposal)?;
                    self.record_audit(
                        tenant,
                        &caller.principal,
                        "proposal.approval_recorded",
                        &format!("proposal:{}", proposal.id),
                        "ALLOW",
                        &[
                            ("id".to_owned(), CisgValue::String(proposal.id.to_string())),
                            (
                                "approvals_count".to_owned(),
                                CisgValue::Int(proposal.approvals.len() as i64),
                            ),
                            ("required".to_owned(), CisgValue::Int(required as i64)),
                            (
                                "status".to_owned(),
                                CisgValue::String(proposal.status.as_str().to_owned()),
                            ),
                        ],
                        now,
                    )?;
                }
                Ok(proposal)
            }
        }
    }

    /// Transitions `PROMOTING -> ACTIVE` once the snapshot publisher swaps the generation in (`spec/05-ingestion.md` §1).
    pub fn mark_active(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Active,
            actor_clean,
            "snapshot_activated",
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `ACTIVE -> WATCHING` when post-activation watch starts (`spec/05-ingestion.md` §1).
    pub fn begin_watching(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Watching,
            actor_clean,
            "watch_window_started",
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `WATCHING -> STABLE` after the 60 s watch window completes cleanly (`spec/05-ingestion.md` §1).
    pub fn mark_stable(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Stable,
            actor_clean,
            "watch_window_stable",
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `WATCHING -> ROLLED_BACK` when regression triggers automatic rollback (`spec/05-ingestion.md` §1).
    pub fn rollback_proposal(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
        reason: &str,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let reason_clean = validate_non_empty(reason, "reason")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        proposal.notes = Some(reason_clean.to_owned());
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::RolledBack,
            actor_clean,
            reason_clean,
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions `STABLE -> PROPOSED` when upstream drift re-opens an existing branch source (`spec/05-ingestion.md` §1).
    pub fn reopen_stable(
        &self,
        tenant: &TenantScope,
        actor: &str,
        id: ProposalId,
        reason: Option<String>,
    ) -> Result<Proposal, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut proposal = self.load_required_proposal(tenant, id)?;
        if let Some(r) = reason
            && !r.trim().is_empty()
        {
            proposal.notes = Some(r);
        }
        proposal.approvals.clear();
        proposal.shadow_snapshot_id = None;
        let now = self.clock.now_timestamp();
        self.transition_locked(
            tenant,
            &mut proposal,
            ProposalStatus::Proposed,
            actor_clean,
            "drift_reopened",
            now,
        )?;
        Ok(proposal)
    }

    /// Transitions every expired `AWAITING_APPROVAL` proposal in `tenant` to `REJECTED` (`spec/05-ingestion.md` §1).
    pub fn expire_stale_approvals(
        &self,
        tenant: &TenantScope,
        actor: &str,
    ) -> Result<Vec<Proposal>, ProposalError> {
        let actor_clean = validate_non_empty(actor, "actor")?;
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = self.clock.now_timestamp();
        self.expire_stale_locked(tenant, actor_clean, now)
    }

    /// Fetches a single proposal within `tenant` (`INV-09`).
    pub fn get_proposal(
        &self,
        tenant: &TenantScope,
        id: ProposalId,
    ) -> Result<Option<Proposal>, ProposalError> {
        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.load_proposal_by_id_locked(tenant, id)
    }

    /// Lists proposals within `tenant` using signed continuation cursors (`RUN-025`, `INV-09`, `spec/02-gap-ql.md` §3).
    pub fn list_proposals(
        &self,
        tenant: &TenantScope,
        filter: ProposalFilter,
    ) -> Result<ProposalConnection, ProposalError> {
        if filter.first == 0 || filter.first > MAX_PROPOSALS_PAGE_SIZE {
            return Err(ProposalError::Validation(format!(
                "proposals `first` must be in 1..={MAX_PROPOSALS_PAGE_SIZE}, got {}",
                filter.first
            )));
        }

        let _guard = self
            .mutation_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = self.clock.now_timestamp();
        let _ = self.expire_stale_locked(tenant, "system:approval-ttl", now)?;

        let status_set: Option<BTreeSet<ProposalStatus>> = filter
            .statuses
            .as_ref()
            .map(|list| list.iter().copied().collect());

        let binding = build_list_cursor_binding(tenant.tenant(), status_set.as_ref());
        let after_key: Option<(i64, String)> = match &filter.after {
            Some(token) => {
                let env: CursorEnvelope<(i64, String)> =
                    verify_cursor(&self.cursor_key, token, &binding)?;
                Some(env.payload)
            }
            None => None,
        };

        let mut all = self.load_all_proposals_locked(tenant)?;
        if let Some(allowed) = &status_set {
            all.retain(|p| allowed.contains(&p.status));
        }
        all.sort_by(|a, b| {
            a.created_at
                .unix_micros()
                .cmp(&b.created_at.unix_micros())
                .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
        });

        let filtered: Vec<Proposal> = match &after_key {
            Some(resume) => all
                .into_iter()
                .filter(|p| (p.created_at.unix_micros(), p.id.to_string()) > *resume)
                .collect(),
            None => all,
        };

        let take_n = usize::try_from(filter.first).unwrap_or(usize::MAX);
        let has_next_page = filtered.len() > take_n;
        let nodes: Vec<Proposal> = filtered.into_iter().take(take_n).collect();
        let end_cursor = match nodes.last() {
            Some(last) => {
                let payload = (last.created_at.unix_micros(), last.id.to_string());
                let env = CursorEnvelope::new(&binding, Some(now), now, payload)?;
                Some(sign_cursor(&self.cursor_key, &env)?)
            }
            None => None,
        };

        Ok(ProposalConnection {
            nodes,
            page_info: PageInfo {
                has_next_page,
                end_cursor,
            },
        })
    }

    fn select_post_stage_status(&self, proposal: &Proposal) -> ProposalStatus {
        let Some(report) = proposal.report.as_ref() else {
            return ProposalStatus::AwaitingApproval;
        };
        let two_person_flag = (self.policy.require_two_person_for_cross_trust
            && report.cross_trust_join)
            || (self.policy.require_two_person_for_mutating && report.mutating);

        let eligible_t0 = proposal.trust == TrustTier::T0
            && self.policy.auto_activate_t0
            && report.manifest_verified
            && !report.breaking
            && !report.enrichment_degraded
            && !two_person_flag;

        if eligible_t0 {
            ProposalStatus::Promoting
        } else {
            ProposalStatus::AwaitingApproval
        }
    }

    fn required_approvals(&self, proposal: &Proposal) -> usize {
        let needs_two = (proposal.trust == TrustTier::T2 && self.policy.require_two_person_for_t2)
            || proposal.report.as_ref().is_some_and(|r| {
                (self.policy.require_two_person_for_cross_trust && r.cross_trust_join)
                    || (self.policy.require_two_person_for_mutating && r.mutating)
            });
        if needs_two { 2 } else { 1 }
    }

    fn expire_stale_locked(
        &self,
        tenant: &TenantScope,
        actor: &str,
        now: Timestamp,
    ) -> Result<Vec<Proposal>, ProposalError> {
        let all = self.load_all_proposals_locked(tenant)?;
        let mut expired = Vec::new();
        for mut proposal in all {
            if self.expire_one_if_stale_locked(tenant, &mut proposal, actor, now)? {
                expired.push(proposal);
            }
        }
        Ok(expired)
    }

    fn expire_one_if_stale_locked(
        &self,
        tenant: &TenantScope,
        proposal: &mut Proposal,
        actor: &str,
        now: Timestamp,
    ) -> Result<bool, ProposalError> {
        if proposal.status != ProposalStatus::AwaitingApproval {
            return Ok(false);
        }
        let elapsed_micros =
            i128::from(now.unix_micros()) - i128::from(proposal.updated_at.unix_micros());
        let ttl_micros = i128::from(self.policy.approval_ttl_secs) * 1_000_000;
        let expired = (self.policy.approval_ttl_secs == 0 && elapsed_micros > 0)
            || (self.policy.approval_ttl_secs > 0 && elapsed_micros >= ttl_micros);
        if !expired {
            return Ok(false);
        }
        self.transition_locked(
            tenant,
            proposal,
            ProposalStatus::Rejected,
            actor,
            "approval_ttl_expired",
            now,
        )?;
        Ok(true)
    }

    fn transition_locked(
        &self,
        tenant: &TenantScope,
        proposal: &mut Proposal,
        next: ProposalStatus,
        actor: &str,
        rationale: &str,
        now: Timestamp,
    ) -> Result<(), ProposalError> {
        let prev = proposal.status;
        if !prev.can_transition_to(next) {
            return Err(ProposalError::Validation(format!(
                "illegal proposal state transition `{} -> {}` for `{}`",
                prev.as_str(),
                next.as_str(),
                proposal.id
            )));
        }
        proposal.status = next;
        proposal.updated_at = now;
        self.persist_updated_proposal(tenant, actor, proposal)?;

        {
            let mut counts = self
                .transitions_total
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = counts.entry((prev, next)).or_insert(0);
            *entry = entry.saturating_add(1);
        }

        self.record_audit(
            tenant,
            actor,
            "proposal.transition",
            &format!("proposal:{}", proposal.id),
            "ALLOW",
            &[
                ("id".to_owned(), CisgValue::String(proposal.id.to_string())),
                (
                    "from".to_owned(),
                    CisgValue::String(prev.as_str().to_owned()),
                ),
                ("to".to_owned(), CisgValue::String(next.as_str().to_owned())),
                (
                    "rationale".to_owned(),
                    CisgValue::String(rationale.to_owned()),
                ),
            ],
            now,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_audit(
        &self,
        tenant: &TenantScope,
        actor: &str,
        action: &str,
        resource: &str,
        decision: &str,
        details: &[(String, CisgValue)],
        ts: Timestamp,
    ) -> Result<(), ProposalError> {
        let mut map = BTreeMap::new();
        for (k, v) in details {
            map.insert(k.clone(), v.clone());
        }
        let details_json = CisgValue::Object(map).to_canonical_json().map_err(|err| {
            ProposalError::Validation(format!("failed to serialize audit details: {err}"))
        })?;
        let entry = AuditEntry {
            ts,
            actor_chain: vec![actor.to_owned()],
            action: action.to_owned(),
            resource: resource.to_owned(),
            decision: decision.to_owned(),
            details_json,
        };
        let _ = self
            .audit
            .append_chain(&AuditChain::Tenant(tenant.clone()), &entry)?;
        Ok(())
    }

    fn load_required_proposal(
        &self,
        tenant: &TenantScope,
        id: ProposalId,
    ) -> Result<Proposal, ProposalError> {
        self.load_proposal_by_id_locked(tenant, id)?
            .ok_or(ProposalError::NotFound(id))
    }

    fn load_proposal_by_id_locked(
        &self,
        tenant: &TenantScope,
        id: ProposalId,
    ) -> Result<Option<Proposal>, ProposalError> {
        let node_id = proposal_node_id(id)?;
        let caller = Caller::new(PROPOSAL_WRITER_PRINCIPAL);
        let Some(node) = self.store.get_node(tenant, &caller, &node_id, None)? else {
            return Ok(None);
        };
        let decoded = decode_proposal_node(&node)?;
        if &decoded.tenant != tenant.tenant() {
            return Err(ProposalError::Corrupt(format!(
                "proposal `{id}` tenant mismatch `{}` vs `{}`",
                decoded.tenant,
                tenant.tenant()
            )));
        }
        Ok(Some(decoded))
    }

    fn load_all_proposals_locked(
        &self,
        tenant: &TenantScope,
    ) -> Result<Vec<Proposal>, ProposalError> {
        let root_id = NodeId::new(INDEX_ROOT_ID)?;
        let caller = Caller::new(PROPOSAL_WRITER_PRINCIPAL);
        if self
            .store
            .get_node(tenant, &caller, &root_id, None)?
            .is_none()
        {
            return Ok(Vec::new());
        }

        let edge_type = EdgeType::new(HAS_PROPOSAL_EDGE_TYPE)?;
        let mut proposals = Vec::new();
        let mut resume = None;
        loop {
            let query = TraverseQuery {
                from: root_id.clone(),
                edge_types: Some(vec![edge_type.clone()]),
                direction: Direction::Outbound,
                max_depth: 1,
                strategy: TraverseStrategy::Bfs,
                as_of: None,
                scopes: None,
                first: MAX_TRAVERSE_FIRST,
                visited_cap: None,
                after: resume,
            };
            let page = self.store.traverse(tenant, &caller, &query)?;
            for node in &page.nodes {
                if node.id == root_id {
                    continue;
                }
                let decoded = decode_proposal_node(node)?;
                if &decoded.tenant == tenant.tenant() {
                    proposals.push(decoded);
                }
            }
            match page.resume {
                Some(next) if page.truncated || !page.nodes.is_empty() => resume = Some(next),
                _ => break,
            }
        }
        Ok(proposals)
    }

    fn persist_new_proposal(
        &self,
        tenant: &TenantScope,
        actor: &str,
        proposal: &Proposal,
    ) -> Result<(), ProposalError> {
        let writer = Caller::new(actor);
        let root_id = NodeId::new(INDEX_ROOT_ID)?;
        let root_node = NewNode {
            id: Some(root_id.clone()),
            node_type: NodeType::new(INDEX_ROOT_NODE_TYPE)?,
            label: "proposal index root".to_owned(),
            summary: None,
            props: Default::default(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new(PROPOSAL_PROVENANCE_SOURCE),
        };
        let prop_id = proposal_node_id(proposal.id)?;
        let prop_node = encode_proposal_node(proposal, actor)?;
        let edge_id = EdgeId::new(format!("has_proposal:{}", proposal.id))?;
        let edge = NewEdge {
            id: Some(edge_id),
            edge_type: EdgeType::new(HAS_PROPOSAL_EDGE_TYPE)?,
            from: root_id,
            to: prop_id,
            props: Default::default(),
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: Provenance::new(PROPOSAL_PROVENANCE_SOURCE),
        };
        let _ = self
            .store
            .put_batch(tenant, &writer, &[root_node, prop_node], &[edge])?;
        Ok(())
    }

    fn persist_updated_proposal(
        &self,
        tenant: &TenantScope,
        actor: &str,
        proposal: &Proposal,
    ) -> Result<(), ProposalError> {
        let writer = Caller::new(actor);
        let prop_node = encode_proposal_node(proposal, actor)?;
        let _ = self.store.put_batch(tenant, &writer, &[prop_node], &[])?;
        Ok(())
    }
}

fn validate_non_empty<'a>(raw: &'a str, field: &str) -> Result<&'a str, ProposalError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProposalError::Validation(format!(
            "`{field}` must not be empty"
        )));
    }
    Ok(trimmed)
}

fn mint_proposal_id(now: Timestamp) -> ProposalId {
    let seq = PROPOSAL_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let os_bytes = ProposalId::new_v7().as_uuid().into_bytes();
    let mut rand = [0u8; 10];
    rand[0..8].copy_from_slice(&seq.to_be_bytes());
    rand[8] = os_bytes[14];
    rand[9] = os_bytes[15];
    let unix_ms = u64::try_from(now.unix_millis()).unwrap_or(0);
    ProposalId::from_unix_ms(unix_ms, rand)
}

fn proposal_node_id(id: ProposalId) -> Result<NodeId, ProposalError> {
    NodeId::new(format!("proposal:{id}")).map_err(ProposalError::from)
}

fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn encode_proposal_node(proposal: &Proposal, actor: &str) -> Result<NewNode, ProposalError> {
    let label = truncate_utf8_prefix(
        &format!(
            "proposal {}: {} {}",
            proposal.status.as_str(),
            format_protocol(proposal.kind),
            proposal.location
        ),
        512,
    );
    let mut node = NewNode {
        id: Some(proposal_node_id(proposal.id)?),
        node_type: NodeType::new(PROPOSAL_NODE_TYPE)?,
        label,
        summary: proposal
            .notes
            .as_deref()
            .map(|s| truncate_utf8_prefix(s, 1_024)),
        props: Default::default(),
        scope: ScopeSpec::Global,
        provenance: Provenance {
            source: PROPOSAL_PROVENANCE_SOURCE.to_owned(),
            desc_hash: None,
            confidence: None,
            extracted_at: Some(proposal.updated_at),
            agent: Some(truncate_utf8_prefix(actor, 256)),
        },
    };
    node.props
        .insert("proposal_id".to_owned(), proposal.id.to_string().into());
    node.props.insert(
        "tenant".to_owned(),
        proposal.tenant.as_str().to_owned().into(),
    );
    node.props.insert(
        "status".to_owned(),
        proposal.status.as_str().to_owned().into(),
    );
    if let Some(branch) = &proposal.branch {
        node.props
            .insert("branch".to_owned(), branch.as_str().to_owned().into());
    }
    node.props.insert(
        "kind".to_owned(),
        format_protocol(proposal.kind).to_owned().into(),
    );
    node.props
        .insert("location".to_owned(), proposal.location.clone().into());
    node.props.insert(
        "trust".to_owned(),
        format_trust_tier(proposal.trust).to_owned().into(),
    );
    if let Some(notes) = &proposal.notes {
        node.props.insert("notes".to_owned(), notes.clone().into());
    }
    if let Some(report) = &proposal.report {
        node.props
            .insert("report_json".to_owned(), report.to_canonical_json()?.into());
    }
    let approvals_arr = CisgValue::Array(
        proposal
            .approvals
            .iter()
            .cloned()
            .map(CisgValue::String)
            .collect(),
    )
    .to_canonical_json()
    .map_err(|err| ProposalError::Validation(format!("failed to encode approvals: {err}")))?;
    node.props
        .insert("approvals_json".to_owned(), approvals_arr.into());
    if let Some(shadow) = &proposal.shadow_snapshot_id {
        node.props
            .insert("shadow_snapshot_id".to_owned(), shadow.clone().into());
    }
    node.props.insert(
        "created_at".to_owned(),
        proposal.created_at.to_string().into(),
    );
    node.props.insert(
        "updated_at".to_owned(),
        proposal.updated_at.to_string().into(),
    );
    Ok(node)
}

fn decode_proposal_node(node: &Node) -> Result<Proposal, ProposalError> {
    let id_str = prop_str(node, "proposal_id")?;
    let id = ProposalId::parse(id_str)
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored proposal_id: {err}")))?;
    let tenant_str = prop_str(node, "tenant")?;
    let tenant = TenantId::new(tenant_str)
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored tenant: {err}")))?;
    let status: ProposalStatus = prop_str(node, "status")?
        .parse()
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored proposal status: {err}")))?;
    let branch = match node.props.get("branch").and_then(|v| v.as_str()) {
        Some(raw) => Some(BranchId::new(raw).map_err(|err| {
            ProposalError::Corrupt(format!("invalid stored branch `{raw}`: {err}"))
        })?),
        None => None,
    };
    let kind = parse_protocol(prop_str(node, "kind")?)
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored kind: {err}")))?;
    let location = prop_str(node, "location")?.to_owned();
    let trust = parse_trust_tier(prop_str(node, "trust")?)
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored trust tier: {err}")))?;
    let notes = node
        .props
        .get("notes")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    let report = match node.props.get("report_json").and_then(|v| v.as_str()) {
        Some(raw) => Some(ProposalReport::from_json_str(raw)?),
        None => None,
    };
    let approvals_raw = prop_str(node, "approvals_json")?;
    let CisgValue::Array(approvals_items) = parse_cisg_json(approvals_raw)? else {
        return Err(ProposalError::Corrupt(
            "stored approvals_json must be JSON array".to_owned(),
        ));
    };
    let mut approvals = Vec::with_capacity(approvals_items.len());
    for item in approvals_items {
        let CisgValue::String(s) = item else {
            return Err(ProposalError::Corrupt(
                "stored approvals_json item must be string".to_owned(),
            ));
        };
        approvals.push(s);
    }
    let shadow_snapshot_id = node
        .props
        .get("shadow_snapshot_id")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    let created_at: Timestamp = prop_str(node, "created_at")?
        .parse()
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored created_at: {err}")))?;
    let updated_at: Timestamp = prop_str(node, "updated_at")?
        .parse()
        .map_err(|err| ProposalError::Corrupt(format!("invalid stored updated_at: {err}")))?;

    Ok(Proposal {
        id,
        tenant,
        status,
        branch,
        kind,
        location,
        trust,
        notes,
        report,
        approvals,
        shadow_snapshot_id,
        created_at,
        updated_at,
    })
}

fn prop_str<'a>(node: &'a Node, key: &str) -> Result<&'a str, ProposalError> {
    node.props
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProposalError::Corrupt(format!("proposal node missing `{key}`")))
}

fn build_list_cursor_binding(
    tenant: &TenantId,
    statuses: Option<&BTreeSet<ProposalStatus>>,
) -> CursorBinding {
    let filter_desc = match statuses {
        Some(set) => set.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","),
        None => "*".to_owned(),
    };
    let op_hash = sha256_hex(format!("gap.proposals:{filter_desc}").as_bytes());
    CursorBinding {
        tenant: tenant.clone(),
        op_hash,
        generation_family: PROPOSAL_CURSOR_FAMILY.to_owned(),
        generation: 1,
        schema_hash: PROPOSAL_CURSOR_SCHEMA_HASH.to_owned(),
    }
}
