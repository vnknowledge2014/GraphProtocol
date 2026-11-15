//! Drift polling scheduler, `DRIFT` serving guard, pinned signature probing, and re-approval
//! clearing (`WP-M4-09`, `ING-052`, `ING-053`, `SEC-082`, `INV-08`).

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::error::{ErrorCode, GapError, Suggestion};
use gap_core::ids::{BranchId, BranchUid, ProposalId, TenantScope};
use gap_core::snapshot::{BranchStatus, Protocol, TrustTier};
use gap_proto::ngc::v1::{ConnectorDescription, DescribeRequest, DescribeResponse};

use crate::cisg::UntrustedText;
use crate::proposals::service::ProposalService;
use crate::proposals::types::{ProposalError, ProposeSourceInput};

use super::classify::{
    DriftClassification, DriftClassificationError, DriftKind, PinnedConnectorSnapshot,
    PinnedOperation, classify_connector_drift, pin_connector_description,
};

/// Default background connector `Describe` polling interval (`15 min`, `ING-052`).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 15 * 60;

/// Default symmetric polling jitter (`±10%`, expressed in basis points).
pub const DEFAULT_POLL_JITTER_BPS: u32 = 1_000;

/// Configuration governing periodic `Describe` drift polling (`ING-052`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftPollConfig {
    /// Nominal interval between polls per branch (`default = 15 min`, `ING-052`).
    pub base_interval: Duration,
    /// Symmetric jitter band in basis points (`1000` = `±10%`).
    pub jitter_bps: u32,
}

impl Default for DriftPollConfig {
    fn default() -> Self {
        Self {
            base_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            jitter_bps: DEFAULT_POLL_JITTER_BPS,
        }
    }
}

/// Computes a deterministic jittered polling interval around `base_interval` (`ING-052`).
///
/// The offset lies strictly within `[-jitter_bps, +jitter_bps]` of `base_interval` and is seeded by
/// `(branch_uid, poll_seq)` so retries and peer branches spread cleanly without non-deterministic RNG.
#[must_use]
pub fn compute_jittered_interval(
    base_interval: Duration,
    branch_uid: BranchUid,
    poll_seq: u64,
    jitter_bps: u32,
) -> Duration {
    if jitter_bps == 0 || base_interval.is_zero() {
        return base_interval;
    }
    let clamped_bps = u128::from(jitter_bps.min(9_000));
    let base_ms = base_interval.as_millis();
    if base_ms == 0 {
        return base_interval;
    }

    let uid_bytes = branch_uid.as_uuid().into_bytes();
    let lo = u64::from_le_bytes([
        uid_bytes[0],
        uid_bytes[1],
        uid_bytes[2],
        uid_bytes[3],
        uid_bytes[4],
        uid_bytes[5],
        uid_bytes[6],
        uid_bytes[7],
    ]);
    let hi = u64::from_le_bytes([
        uid_bytes[8],
        uid_bytes[9],
        uid_bytes[10],
        uid_bytes[11],
        uid_bytes[12],
        uid_bytes[13],
        uid_bytes[14],
        uid_bytes[15],
    ]);
    let mixed = splitmix64(lo ^ hi.rotate_left(17) ^ poll_seq.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let span = clamped_bps * 2 + 1;
    let offset_bps = (u128::from(mixed) % span) as i128 - clamped_bps as i128;

    let base_i128 = i128::try_from(base_ms).unwrap_or(i128::MAX);
    let delta_ms = base_i128.saturating_mul(offset_bps) / 10_000;
    let adjusted_ms = base_i128.saturating_add(delta_ms).max(1);
    Duration::from_millis(u64::try_from(adjusted_ms).unwrap_or(u64::MAX))
}

const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Transport abstraction fetching `DescribeResponse` from a registered connector (`ING-052`).
pub trait DescribePoller {
    /// Polls `branch_uid` passing GAP's cached upstream `known_spec_hash`.
    ///
    /// # Errors
    ///
    /// Returns [`DriftError::PollTransport`] when the connector cannot be reached or fails.
    fn describe(
        &self,
        branch_uid: BranchUid,
        request: DescribeRequest,
    ) -> Result<DescribeResponse, DriftError>;
}

/// Result of probing whether an operation's pinned signature still executes cleanly (`ING-053`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureProbeResult {
    /// `true` iff the pinned operation signature resolved and remained executable.
    pub compatible: bool,
    /// Optional human-readable diagnostic explaining incompatibility.
    pub diagnostic: Option<String>,
}

impl SignatureProbeResult {
    /// Successful signature probe.
    #[must_use]
    pub const fn ok() -> Self {
        Self {
            compatible: true,
            diagnostic: None,
        }
    }

    /// Failed signature probe carrying a sanitized diagnostic.
    #[must_use]
    pub fn incompatible(reason: impl Into<String>) -> Self {
        Self {
            compatible: false,
            diagnostic: Some(reason.into()),
        }
    }
}

/// Probe abstraction verifying whether pinned operation signatures still resolve during `DRIFT` (`ING-053`).
pub trait SignatureProber {
    /// Verifies that `op_name` on `branch_uid` matches `pinned_signature_hash`.
    ///
    /// # Errors
    ///
    /// Returns [`DriftError::ProbeFailed`] if the probe transport itself errors out.
    fn probe_operation_signature(
        &self,
        branch_uid: BranchUid,
        op_name: &str,
        pinned_signature_hash: [u8; 32],
    ) -> Result<SignatureProbeResult, DriftError>;
}

/// Registration metadata required by [`DriftMonitor`] for a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoredBranchConfig {
    /// Immutable UUIDv7 identifying this branch (`spec/00-conventions.md` §7).
    pub uid: BranchUid,
    /// Human-readable branch slug.
    pub id: BranchId,
    /// Upstream protocol (`ProposalService` deduplication key part).
    pub protocol: Protocol,
    /// Canonical endpoint location (`ProposalService` deduplication key part).
    pub location: String,
    /// Current trust tier (`T0`/`T1`/`T2`).
    pub trust: TrustTier,
}

/// Sanitized pinned operation view served to routers and planners even during `DRIFT` (`INV-08`, `SEC-082`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedOperationView {
    /// Qualified `<branch>.<op>` or branch-local operation name.
    pub name: String,
    /// Sanitized approved description (`INV-08` — never contaminated by unapproved drift text).
    pub description: UntrustedText,
    /// Approved `desc_hash` (`SEC-082`).
    pub desc_hash: [u8; 32],
    /// Sanitized approved intent examples.
    pub examples: Vec<(String, String, bool)>,
    /// Sanitized approved annotations.
    pub annotations: BTreeMap<String, String>,
    /// Current branch lifecycle status (`Active` or `Drift`).
    pub branch_status: BranchStatus,
}

/// Optional proposal-emission context passed into [`DriftMonitor::poll_once`].
pub struct ProposalEmissionContext<'a> {
    /// Persistent proposal service receiving `PROPOSED` drift proposals (`ING-052`).
    pub service: &'a ProposalService,
    /// Tenant scope owning the branch (`INV-09`).
    pub tenant: &'a TenantScope,
    /// Principal attributed in the hash-chained audit log.
    pub actor: &'a str,
}

/// Outcome returned by [`DriftMonitor::poll_once`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftPollOutcome {
    /// Branch polled.
    pub branch_uid: BranchUid,
    /// Branch lifecycle status after evaluating this poll (`Active` or `Drift`).
    pub status: BranchStatus,
    /// Drift classification computed from the response (`ING-052`).
    pub classification: DriftClassification,
    /// Scheduled next poll timestamp including jitter (`ING-052`).
    pub next_poll_at: Timestamp,
    /// Optional [`ProposalId`] created or deduplicated via [`ProposalService`] (`ING-052`).
    pub emitted_proposal_id: Option<ProposalId>,
}

/// Per-branch tracked drift state inside [`DriftMonitor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftBranchState {
    /// Static registration info.
    pub config: MonitoredBranchConfig,
    /// Lifecycle status (`Active`, `Drift`, `Quarantined`, `Disabled`).
    pub status: BranchStatus,
    /// Hash-verified pinned snapshot (`INV-08`, `SEC-082`).
    pub pinned: PinnedConnectorSnapshot,
    /// Unapproved drifted candidate held outside serving (`INV-08`) until [`DriftMonitor::approve_repin`].
    pub unapproved_candidate: Option<PinnedConnectorSnapshot>,
    /// Last recorded drift classification.
    pub last_classification: Option<DriftKind>,
    /// Reasons observed on the latest non-clean poll.
    pub last_reasons: Vec<String>,
    /// Per-operation cached probe result recorded when `DRIFT` entered (`ING-053`).
    pub probe_results: BTreeMap<String, SignatureProbeResult>,
    /// Monotonic poll counter feeding deterministic jitter.
    pub poll_seq: u64,
    /// Next scheduled poll timestamp.
    pub next_poll_at: Timestamp,
    /// Timestamp when `DRIFT` was first entered for the active incident.
    pub drifted_at: Option<Timestamp>,
    /// Timestamp when admin approval last pinned the baseline.
    pub approved_at: Timestamp,
}

/// Typed errors returned by [`DriftMonitor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftError {
    /// Branch is not registered in this monitor.
    UnknownBranch(BranchUid),
    /// Invalid schema or response encountered while pinning/classifying.
    Classification(DriftClassificationError),
    /// Transport failure talking to connector `Describe`.
    PollTransport(String),
    /// Transport failure running a signature probe.
    ProbeFailed(String),
    /// Timestamp arithmetic overflowed `Timestamp` bounds.
    InvalidTimestamp(String),
    /// Emitting a drift `Proposal` via [`ProposalService`] failed.
    Proposal(ProposalError),
}

impl fmt::Display for DriftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBranch(uid) => write!(f, "unknown branch `{uid}` in drift monitor"),
            Self::Classification(err) => write!(f, "drift classification error: {err}"),
            Self::PollTransport(msg) => write!(f, "describe poll failed: {msg}"),
            Self::ProbeFailed(msg) => write!(f, "signature probe failed: {msg}"),
            Self::InvalidTimestamp(msg) => write!(f, "invalid timestamp: {msg}"),
            Self::Proposal(err) => write!(f, "failed to emit drift proposal: {err}"),
        }
    }
}

impl StdError for DriftError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Classification(err) => Some(err),
            Self::Proposal(err) => Some(err),
            Self::UnknownBranch(_)
            | Self::PollTransport(_)
            | Self::ProbeFailed(_)
            | Self::InvalidTimestamp(_) => None,
        }
    }
}

impl From<DriftClassificationError> for DriftError {
    fn from(value: DriftClassificationError) -> Self {
        Self::Classification(value)
    }
}

impl From<ProposalError> for DriftError {
    fn from(value: ProposalError) -> Self {
        Self::Proposal(value)
    }
}

/// Stateful per-branch connector drift detector and execution guard (`ING-052`, `ING-053`, `INV-08`).
#[derive(Debug, Clone)]
pub struct DriftMonitor {
    config: DriftPollConfig,
    branches: BTreeMap<BranchUid, DriftBranchState>,
}

impl DriftMonitor {
    /// Creates an empty [`DriftMonitor`] with the provided polling configuration.
    #[must_use]
    pub fn new(config: DriftPollConfig) -> Self {
        Self {
            config,
            branches: BTreeMap::new(),
        }
    }

    /// Creates a [`DriftMonitor`] using the spec default `15 min` polling interval (`ING-052`).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(DriftPollConfig::default())
    }

    /// Registers an approved branch and pins its description hashes (`SEC-082`).
    ///
    /// # Errors
    ///
    /// Returns [`DriftError::Classification`] or [`DriftError::InvalidTimestamp`] if the
    /// connector's SDL is invalid or the next poll timestamp overflows.
    pub fn register_branch(
        &mut self,
        config: MonitoredBranchConfig,
        approved_description: &ConnectorDescription,
        approved_at: Timestamp,
    ) -> Result<&DriftBranchState, DriftError> {
        let pinned = pin_connector_description(approved_description)?;
        let initial_delay = compute_jittered_interval(
            self.config.base_interval,
            config.uid,
            0,
            self.config.jitter_bps,
        );
        let next_poll_at = add_duration(approved_at, initial_delay)?;
        let uid = config.uid;

        let state = DriftBranchState {
            config,
            status: BranchStatus::Active,
            pinned,
            unapproved_candidate: None,
            last_classification: None,
            last_reasons: Vec::new(),
            probe_results: BTreeMap::new(),
            poll_seq: 0,
            next_poll_at,
            drifted_at: None,
            approved_at,
        };
        self.branches.insert(uid, state);
        self.branches
            .get(&uid)
            .ok_or(DriftError::UnknownBranch(uid))
    }

    /// Returns tracked state for `branch_uid`, if registered.
    #[must_use]
    pub fn branch_state(&self, branch_uid: BranchUid) -> Option<&DriftBranchState> {
        self.branches.get(&branch_uid)
    }

    /// Lists branches whose scheduled jittered `next_poll_at <= now` (`ING-052`).
    #[must_use]
    pub fn due_branches(&self, now: Timestamp) -> Vec<BranchUid> {
        self.branches
            .iter()
            .filter_map(|(&uid, state)| (state.next_poll_at <= now).then_some(uid))
            .collect()
    }

    /// Executes one `Describe` poll for `branch_uid`, classifies drift, updates `DRIFT` status,
    /// runs signature probes when entering `DRIFT`, and optionally emits an ingestion `Proposal`
    /// (`ING-052`, `ING-053`, `SEC-082`).
    ///
    /// Note that when `DRIFT` occurs, [`DriftBranchState::pinned`] stays untouched so planners
    /// continue serving the hash-verified approved descriptions (`INV-08`).
    ///
    /// # Errors
    ///
    /// Returns [`DriftError`] when the branch is unknown, polling/classification fails, probing
    /// errors out, or proposal emission fails.
    pub fn poll_once<P, S>(
        &mut self,
        branch_uid: BranchUid,
        poller: &P,
        prober: &S,
        now: Timestamp,
        proposal_ctx: Option<ProposalEmissionContext<'_>>,
    ) -> Result<DriftPollOutcome, DriftError>
    where
        P: DescribePoller,
        S: SignatureProber,
    {
        let (known_spec_hash, poll_seq, branch_cfg, pinned_snapshot) = {
            let state = self
                .branches
                .get(&branch_uid)
                .ok_or(DriftError::UnknownBranch(branch_uid))?;
            (
                state.pinned.advertised_spec_hash.clone(),
                state.poll_seq,
                state.config.clone(),
                state.pinned.clone(),
            )
        };

        let response = poller.describe(branch_uid, DescribeRequest { known_spec_hash })?;
        let classification = classify_connector_drift(&pinned_snapshot, &response)?;

        let mut probe_results = BTreeMap::new();
        if classification.kind.triggers_drift() {
            for (op_name, pinned_op) in &pinned_snapshot.operations {
                let res = prober.probe_operation_signature(
                    branch_uid,
                    op_name,
                    pinned_op.signature_hash,
                )?;
                probe_results.insert(op_name.clone(), res);
            }
        }

        let emitted_proposal_id = if classification.kind.emits_proposal()
            && let Some(ctx) = proposal_ctx
        {
            let notes = build_drift_proposal_notes(classification.kind, &classification.reasons);
            let proposal = ctx.service.propose_source(
                ctx.tenant,
                ctx.actor,
                ProposeSourceInput {
                    kind: branch_cfg.protocol,
                    location: branch_cfg.location,
                    branch: Some(branch_cfg.id),
                    trust: branch_cfg.trust,
                    notes: Some(notes),
                },
            )?;
            Some(proposal.id)
        } else {
            None
        };

        let next_seq = poll_seq.saturating_add(1);
        let delay = compute_jittered_interval(
            self.config.base_interval,
            branch_uid,
            next_seq,
            self.config.jitter_bps,
        );
        let next_poll_at = add_duration(now, delay)?;

        let state = self
            .branches
            .get_mut(&branch_uid)
            .ok_or(DriftError::UnknownBranch(branch_uid))?;

        state.poll_seq = next_seq;
        state.next_poll_at = next_poll_at;
        state.last_classification = Some(classification.kind);
        state.last_reasons.clone_from(&classification.reasons);

        match classification.kind {
            DriftKind::SchemaBreaking | DriftKind::DescriptionChange => {
                // `pinned` MUST remain frozen at the approved baseline (`INV-08`, `SEC-082`).
                state.status = BranchStatus::Drift;
                state
                    .unapproved_candidate
                    .clone_from(&classification.candidate);
                state.probe_results = probe_results;
                if state.drifted_at.is_none() {
                    state.drifted_at = Some(now);
                }
            }
            DriftKind::SchemaCompatible => {
                // Pending approval via proposal; branch keeps serving pinned descriptions cleanly.
                state
                    .unapproved_candidate
                    .clone_from(&classification.candidate);
            }
            DriftKind::NotModified | DriftKind::Cosmetic => {
                // Auto-accepted without altering pinned hashes or snapshot state (`ING-052`).
                state.unapproved_candidate = None;
            }
        }

        Ok(DriftPollOutcome {
            branch_uid,
            status: state.status,
            classification,
            next_poll_at,
            emitted_proposal_id,
        })
    }

    /// Returns the hash-verified **pinned** operation view (`INV-08`, `SEC-082`), never unapproved
    /// drifted text, even while [`BranchStatus::Drift`] is active.
    #[must_use]
    pub fn served_operation(
        &self,
        branch_uid: BranchUid,
        op_name: &str,
    ) -> Option<PinnedOperationView> {
        let state = self.branches.get(&branch_uid)?;
        let op: &PinnedOperation = state.pinned.operations.get(op_name)?;
        Some(PinnedOperationView {
            name: op.name.clone(),
            description: op.sanitized_description.clone(),
            desc_hash: op.desc_hash,
            examples: op.sanitized_examples.clone(),
            annotations: op.sanitized_annotations.clone(),
            branch_status: state.status,
        })
    }

    /// Returns the hash-verified pinned branch synopsis (`INV-08`).
    #[must_use]
    pub fn served_synopsis(&self, branch_uid: BranchUid) -> Option<&UntrustedText> {
        self.branches
            .get(&branch_uid)
            .map(|state| &state.pinned.sanitized_synopsis)
    }

    /// Enforces `ING-053` execution gating against the branch's recorded probe state.
    ///
    /// - `Active` branches with the requested pinned operation are permitted.
    /// - `Drift` branches permit execution **only** if the pinned operation signature probe passed;
    ///   otherwise they fail deterministically with `GAP_BRANCH_DRIFT` (`ErrorCode::BranchDrift`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] with `ErrorCode::BranchDrift` or `ErrorCode::BranchUnavailable`.
    pub fn authorize_execution(
        &self,
        branch_uid: BranchUid,
        op_name: &str,
    ) -> Result<(), GapError> {
        let state = self.branches.get(&branch_uid).ok_or_else(|| {
            GapError::new(
                ErrorCode::BranchUnavailable,
                format!("branch `{branch_uid}` is not registered"),
            )
        })?;

        check_status_and_pinned_op(state, op_name)?;

        if state.status == BranchStatus::Drift {
            let probe = state.probe_results.get(op_name);
            match probe {
                Some(res) if res.compatible => Ok(()),
                Some(res) => Err(branch_drift_error(
                    &state.config.id,
                    op_name,
                    res.diagnostic
                        .as_deref()
                        .unwrap_or("pinned operation signature probe failed"),
                )),
                None => Err(branch_drift_error(
                    &state.config.id,
                    op_name,
                    "no passing pinned signature probe recorded for drifted branch",
                )),
            }
        } else {
            Ok(())
        }
    }

    /// Enforces `ING-053` execution gating using a live [`SignatureProber`] during `DRIFT`.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] with `ErrorCode::BranchDrift` when the signature probe fails or the
    /// branch/operation is unavailable.
    pub fn authorize_execution_with_prober<S: SignatureProber>(
        &mut self,
        branch_uid: BranchUid,
        op_name: &str,
        prober: &S,
    ) -> Result<(), GapError> {
        let (branch_id, status, sig_hash) = {
            let state = self.branches.get(&branch_uid).ok_or_else(|| {
                GapError::new(
                    ErrorCode::BranchUnavailable,
                    format!("branch `{branch_uid}` is not registered"),
                )
            })?;
            let op = check_status_and_pinned_op(state, op_name)?;
            (state.config.id.clone(), state.status, op.signature_hash)
        };

        if status != BranchStatus::Drift {
            return Ok(());
        }

        let probe_res = prober
            .probe_operation_signature(branch_uid, op_name, sig_hash)
            .map_err(|err| branch_drift_error(&branch_id, op_name, &err.to_string()))?;

        let compatible = probe_res.compatible;
        let reason = probe_res.diagnostic.clone();
        if let Some(state) = self.branches.get_mut(&branch_uid) {
            state.probe_results.insert(op_name.to_owned(), probe_res);
        }

        if compatible {
            Ok(())
        } else {
            Err(branch_drift_error(
                &branch_id,
                op_name,
                reason
                    .as_deref()
                    .unwrap_or("pinned operation signature failed live probe"),
            ))
        }
    }

    /// Re-pins an approved description and clears `BranchStatus::Drift` (`ING-053`, `SEC-082`).
    ///
    /// # Errors
    ///
    /// Returns [`DriftError`] if the branch is unknown or `approved_description` fails validation.
    pub fn approve_repin(
        &mut self,
        branch_uid: BranchUid,
        approved_description: &ConnectorDescription,
        approved_at: Timestamp,
    ) -> Result<&DriftBranchState, DriftError> {
        let new_pinned = pin_connector_description(approved_description)?;
        let state = self
            .branches
            .get_mut(&branch_uid)
            .ok_or(DriftError::UnknownBranch(branch_uid))?;

        state.pinned = new_pinned;
        state.unapproved_candidate = None;
        state.status = BranchStatus::Active;
        state.probe_results.clear();
        state.last_reasons.clear();
        state.last_classification = None;
        state.drifted_at = None;
        state.approved_at = approved_at;
        Ok(state)
    }
}

fn check_status_and_pinned_op<'a>(
    state: &'a DriftBranchState,
    op_name: &str,
) -> Result<&'a PinnedOperation, GapError> {
    if matches!(
        state.status,
        BranchStatus::Quarantined | BranchStatus::Disabled
    ) {
        return Err(GapError::new(
            ErrorCode::BranchUnavailable,
            format!(
                "branch `{}` is `{:?}` and unavailable",
                state.config.id, state.status
            ),
        ));
    }

    state.pinned.operations.get(op_name).ok_or_else(|| {
        GapError::new(
            ErrorCode::BranchDrift,
            format!(
                "operation `{op_name}` on branch `{}` is not present in pinned baseline",
                state.config.id
            ),
        )
    })
}

fn branch_drift_error(branch_id: &BranchId, op_name: &str, detail: &str) -> GapError {
    GapError::new(
        ErrorCode::BranchDrift,
        format!("branch `{branch_id}` operation `{op_name}` failed drift check: {detail}"),
    )
    .with_suggestion(Suggestion {
        alternative: None,
        ..Suggestion::default()
    })
}

fn build_drift_proposal_notes(kind: DriftKind, reasons: &[String]) -> String {
    let joined = reasons.join("; ");
    let sanitized_reasons = UntrustedText::from_raw(&joined).sanitized;
    let raw = if matches!(kind, DriftKind::SchemaBreaking) {
        format!("drift={kind}; breaking=true; reasons={sanitized_reasons}")
    } else {
        format!("drift={kind}; breaking=false; reasons={sanitized_reasons}")
    };
    let clean: String = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(512)
        .collect();
    clean
}

fn add_duration(ts: Timestamp, delta: Duration) -> Result<Timestamp, DriftError> {
    let nanos = i128::try_from(delta.as_nanos())
        .map_err(|_| DriftError::InvalidTimestamp("duration exceeds i128".to_owned()))?;
    let total = ts
        .unix_nanos()
        .checked_add(nanos)
        .ok_or_else(|| DriftError::InvalidTimestamp("poll timestamp overflow".to_owned()))?;
    Timestamp::from_unix_nanos(total)
        .map_err(|err| DriftError::InvalidTimestamp(format!("invalid poll timestamp: {err}")))
}
