//! Serialized `Publisher` actor, pre-warm gate, coalescer, shadow staging, 5-generation ring,
//! and dedicated off-thread generation reclaimer (`WP-M4-08`).
//!
//! Normative references: `RUN-003`, `RUN-004`, `RUN-005`, `RUN-006`, `INV-01`, `INV-02`,
//! `INV-16`, `ING-050`, `ADR-009`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use gap_core::clock::Timestamp;
use gap_core::ids::{BranchUid, ProposalId, TenantId};
use gap_core::joingraph::{JoinGraph, JoinStatus};
use gap_core::snapshot::{BranchDescriptor, BranchStatus, SNAPSHOT_FORMAT_VERSION, Snapshot};
use gap_plan::{PlanConfig, plan_query};
use thiserror::Error;

use crate::compose::{BranchInput, Composition, CompositionError, compose};
use crate::handle::{PublishError, SnapshotBuilder, SnapshotHandle};

/// Default number of top hot queries re-planned during pre-warm (`RUN-004`, `ADR-009` §6).
pub const DEFAULT_PREWARM_TOP_N: usize = 100;
/// Default minimum pre-warm pass fraction (`95%` pass gate, `RUN-004`, `ADR-009` §6).
pub const DEFAULT_MIN_PREWARM_PASS_RATE: f64 = 0.95;
/// Default number of snapshot generations retained in the structural-sharing ring (`ADR-009` §4).
pub const DEFAULT_RING_CAPACITY: usize = 5;
/// Default coalescing window for batching rapid proposals (`RUN-006`).
pub const DEFAULT_COALESCE_WINDOW_MS: u64 = 250;
/// Default maximum bounded channel depth for off-thread snapshot reclamation.
pub const DEFAULT_RECLAIMER_QUEUE_CAP: usize = 256;

/// Required RBAC scope to inspect staged shadow snapshots via `X-Gap-Snapshot: shadow:<id>` (`ING-050`).
pub const ADMIN_SCHEMA_SCOPE: &str = "admin:schema";
/// Header prefix identifying shadow snapshots (`ING-050`).
pub const SHADOW_HEADER_PREFIX: &str = "shadow:";

/// Hot cached/persisted GraphQL query signature replayed during pre-warm (`RUN-004`, `ADR-009` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotQuery {
    /// Stable identifier (e.g. cache key hash).
    pub id: String,
    /// GraphQL query document string.
    pub query: String,
    /// Optional GraphQL operation name.
    pub operation_name: Option<String>,
}

/// Single pre-warm failure diagnostic for a hot query (`RUN-004`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrewarmFailure {
    /// Identifier of the failing [`HotQuery`].
    pub query_id: String,
    /// Stable GAP error code tag (`gap_core::error::ErrorCode`).
    pub code: String,
    /// Sanitized planning error summary.
    pub reason: String,
}

/// Aggregated pre-warm outcome before `ArcSwap` pointer promotion (`RUN-004`, `ADR-009` §6).
#[derive(Debug, Clone, PartialEq)]
pub struct PrewarmReport {
    /// Total queries re-planned (at most `top_n`).
    pub total: usize,
    /// Number of queries that planned cleanly against the candidate snapshot.
    pub passed: usize,
    /// Failing queries with error diagnostics.
    pub failures: Vec<PrewarmFailure>,
    /// Fraction `passed / total` (`1.0` when `total == 0`).
    pub pass_rate: f64,
}

/// Audited admin override for controlled breaking changes (`RUN-004`, `ADR-009` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakingChangeAck {
    /// Admin principal ID.
    pub actor: String,
    /// Non-empty justification recorded in publish audit logs.
    pub reason: String,
}

impl BreakingChangeAck {
    /// Returns `true` when both `actor` and `reason` are non-empty after trimming.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.actor.trim().is_empty() && !self.reason.trim().is_empty()
    }
}

/// Per-publish options passed to [`PublisherActor::publish`] (`RUN-004`, `ADR-009` §5–6).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PublishOptions {
    /// Optional explicit breaking-change acknowledgment overriding the 95% pre-warm gate.
    pub breaking_change_ack: Option<BreakingChangeAck>,
    /// Allow re-promoting a `branch_set_hash` / generation previously marked `bad = true` (`ADR-009` §5).
    pub force: bool,
    /// Optional `HotQuery` workload overriding the publisher's default hot query table.
    pub hot_queries_override: Option<Vec<HotQuery>>,
}

/// Configuration for [`PublisherActor`] (`RUN-003`, `RUN-004`, `ADR-009`).
#[derive(Debug, Clone, PartialEq)]
pub struct PublisherConfig {
    /// Number of retained generations in the structural-sharing ring (default `5`, `ADR-009` §4).
    pub ring_capacity: usize,
    /// Top-N hot queries evaluated during pre-warm (default `100`, `RUN-004`).
    pub prewarm_top_n: usize,
    /// Minimum fraction of pre-warm queries that must succeed unless `breaking_change_ack` is valid (`0.95`).
    pub min_prewarm_pass_rate: f64,
    /// Coalescing window in milliseconds for batching concurrent proposals (default `250`).
    pub coalesce_window_ms: u64,
    /// Planner limits & cost settings used during pre-warm.
    pub plan_config: PlanConfig,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            ring_capacity: DEFAULT_RING_CAPACITY,
            prewarm_top_n: DEFAULT_PREWARM_TOP_N,
            min_prewarm_pass_rate: DEFAULT_MIN_PREWARM_PASS_RATE,
            coalesce_window_ms: DEFAULT_COALESCE_WINDOW_MS,
            plan_config: PlanConfig::default(),
        }
    }
}

/// Structured audit record emitted for every committed publish or rollback (`RUN-004`, `RUN-007`).
#[derive(Debug, Clone, PartialEq)]
pub struct PublishAuditRecord {
    /// Monotonically assigned snapshot generation (`INV-16`).
    pub generation: u64,
    /// Previous active generation, if any.
    pub parent_generation: Option<u64>,
    /// Resulting `branch_set_hash`.
    pub branch_set_hash: [u8; 32],
    /// Origin proposals coalesced into this generation.
    pub proposals: Vec<ProposalId>,
    /// Pre-warm outcome recorded before swapping.
    pub prewarm: PrewarmReport,
    /// Whether this publication was a rollback (`RUN-007`).
    pub is_rollback: bool,
    /// Optional audit note (e.g. rollback target or breaking change ack actor).
    pub note: String,
    /// End-to-end swap duration of the `SnapshotHandle::publish` step in microseconds.
    pub swap_us: u64,
}

/// Typed errors emitted by [`PublisherActor`] and [`ShadowRegistry`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PublisherError {
    /// Underlying `SnapshotHandle::publish` failed (`UnsupportedFormat` or `NonMonotonic`).
    #[error(transparent)]
    Handle(#[from] PublishError),
    /// Branch composition error while building a candidate snapshot.
    #[error("composition failed: {0}")]
    Composition(#[from] CompositionError),
    /// Pre-warm pass rate fell below threshold without a valid `breaking_change_ack` (`RUN-004`).
    #[error(
        "prewarm regression: {passed}/{total} (rate {pass_rate:.3}) below minimum {min_rate:.3} without breaking_change_ack"
    )]
    PrewarmRegression {
        /// Successful query count.
        passed: usize,
        /// Total pre-warmed queries.
        total: usize,
        /// Achieved pass fraction.
        pass_rate: f64,
        /// Required pass fraction.
        min_rate: f64,
    },
    /// Candidate `branch_set_hash` matches a generation marked `bad = true` and `force` was false (`ADR-009` §5).
    #[error("snapshot with bad generation {bad_generation} cannot be re-promoted without --force")]
    BadGenerationBlocked {
        /// Generation marked bad.
        bad_generation: u64,
    },
    /// Requested rollback target generation is no longer retained in the ring (`RUN-007`).
    #[error("rollback target generation {target} is not in the active ring (min={oldest:?})")]
    GenerationNotRetained {
        /// Requested rollback target generation.
        target: u64,
        /// Oldest generation currently retained in the ring.
        oldest: Option<u64>,
    },
    /// Attempted to roll back to a generation itself marked `bad = true` without `force = true`.
    #[error("rollback target generation {target} is marked bad and requires force")]
    RollbackTargetMarkedBad {
        /// Target generation.
        target: u64,
    },
    /// Reclaimer worker queue saturated or closed.
    #[error("off-thread reclaimer unavailable: {0}")]
    ReclaimerUnavailable(String),
    /// Staged shadow lookup rejected due to missing `admin:schema` scope or invalid header (`ING-050`).
    #[error("shadow snapshot access error: {0}")]
    ShadowAccess(String),
}

enum ReclaimMsg {
    Retire(Arc<Snapshot>),
    Flush(mpsc::SyncSender<()>),
    Shutdown,
}

/// Metrics snapshot exposed by [`ReclaimerHandle`] for monitoring and verification tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimerMetrics {
    /// Total retired snapshots queued for off-thread drop.
    pub enqueued: u64,
    /// Total retired snapshots whose final `Arc<Snapshot>` dropped on the reclaimer thread.
    pub reclaimed: u64,
    /// Retired snapshots still awaiting release by pinned requests/tasks (`RUN-006`).
    pub pending: usize,
    /// OS `ThreadId` of the dedicated reclaimer thread (proves drops occur off-thread).
    pub thread_id: ThreadId,
}

/// Dedicated background OS thread handle reclaiming retired [`Arc<Snapshot>`] generations once
/// every pinned request/task has released its reference (`RUN-004`, `RUN-006`, `ADR-009` §2).
#[derive(Debug)]
pub struct ReclaimerHandle {
    tx: Mutex<Option<mpsc::SyncSender<ReclaimMsg>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    enqueued: Arc<AtomicU64>,
    reclaimed: Arc<AtomicU64>,
    pending: Arc<AtomicUsize>,
    thread_id: ThreadId,
    shutdown: Arc<AtomicBool>,
}

impl ReclaimerHandle {
    /// Spawns a bounded background reclaimer thread (`DEFAULT_RECLAIMER_QUEUE_CAP`).
    #[must_use]
    pub fn start(queue_cap: usize) -> Self {
        let cap = queue_cap.max(8);
        let (tx, rx) = mpsc::sync_channel::<ReclaimMsg>(cap);
        let (tid_tx, tid_rx) = mpsc::sync_channel::<ThreadId>(1);
        let enqueued = Arc::new(AtomicU64::new(0));
        let reclaimed = Arc::new(AtomicU64::new(0));
        let pending = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let reclaimed_c = Arc::clone(&reclaimed);
        let pending_c = Arc::clone(&pending);
        let shutdown_c = Arc::clone(&shutdown);

        let handle = thread::Builder::new()
            .name("gap-snapshot-reclaimer".to_owned())
            .spawn(move || {
                let _ = tid_tx.send(thread::current().id());
                let mut holding: Vec<Arc<Snapshot>> = Vec::new();

                let reap_ready = |holding: &mut Vec<Arc<Snapshot>>| {
                    let before = holding.len();
                    // Drop ONLY entries whose refcount reached 1 (this vector holds the sole Arc),
                    // ensuring no reader thread or swap thread pays the ~29 ms snapshot dealloc!
                    holding.retain(|snap| Arc::strong_count(snap) > 1);
                    let dropped = before.saturating_sub(holding.len());
                    if dropped > 0 {
                        reclaimed_c.fetch_add(dropped as u64, Ordering::Relaxed);
                        pending_c.store(holding.len(), Ordering::Release);
                    }
                };

                loop {
                    reap_ready(&mut holding);
                    match rx.recv_timeout(Duration::from_millis(2)) {
                        Ok(ReclaimMsg::Retire(snap)) => {
                            holding.push(snap);
                            pending_c.store(holding.len(), Ordering::Release);
                            reap_ready(&mut holding);
                        }
                        Ok(ReclaimMsg::Flush(ack)) => {
                            reap_ready(&mut holding);
                            let _ = ack.send(());
                        }
                        Ok(ReclaimMsg::Shutdown) => {
                            shutdown_c.store(true, Ordering::Release);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }

                // Final drain on shutdown so memory doesn't leak if unpinned.
                reap_ready(&mut holding);
                holding.clear();
                pending_c.store(0, Ordering::Release);
            });

        let (worker, thread_id) = match handle {
            Ok(h) => {
                let tid = tid_rx.recv().unwrap_or_else(|_| thread::current().id());
                (Some(h), tid)
            }
            Err(_) => (None, thread::current().id()),
        };

        Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(worker),
            enqueued,
            reclaimed,
            pending,
            thread_id,
            shutdown,
        }
    }

    /// Hands a retired `Arc<Snapshot>` to the background reclaimer thread without blocking on drop.
    pub fn retire(&self, snap: Arc<Snapshot>) -> Result<(), PublisherError> {
        let guard = self
            .tx
            .lock()
            .map_err(|_| PublisherError::ReclaimerUnavailable("sender lock poisoned".to_owned()))?;
        let Some(ref tx) = *guard else {
            return Err(PublisherError::ReclaimerUnavailable(
                "reclaimer already shut down".to_owned(),
            ));
        };
        match tx.try_send(ReclaimMsg::Retire(snap)) {
            Ok(()) => {
                self.enqueued.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::TrySendError::Full(ReclaimMsg::Retire(snap))) => {
                // Bounded backpressure in control plane only; never stalls reader threads.
                tx.send(ReclaimMsg::Retire(snap)).map_err(|e| {
                    PublisherError::ReclaimerUnavailable(format!("reclaimer closed: {e}"))
                })?;
                self.enqueued.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(PublisherError::ReclaimerUnavailable(format!(
                "reclaimer channel error: {e}"
            ))),
        }
    }

    /// Synchronizes one pass of the reclaimer loop (useful in deterministic tests/benchmarks).
    pub fn sync_pass(&self) {
        let (ack_tx, ack_rx) = mpsc::sync_channel::<()>(1);
        if let Ok(guard) = self.tx.lock()
            && let Some(ref tx) = *guard
            && tx.send(ReclaimMsg::Flush(ack_tx)).is_ok()
        {
            let _ = ack_rx.recv_timeout(Duration::from_secs(1));
        }
    }

    /// Returns a point-in-time [`ReclaimerMetrics`] snapshot.
    #[must_use]
    pub fn metrics(&self) -> ReclaimerMetrics {
        ReclaimerMetrics {
            enqueued: self.enqueued.load(Ordering::Relaxed),
            reclaimed: self.reclaimed.load(Ordering::Relaxed),
            pending: self.pending.load(Ordering::Acquire),
            thread_id: self.thread_id,
        }
    }
}

impl Drop for ReclaimerHandle {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.tx.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(ReclaimMsg::Shutdown);
        }
        if let Ok(mut guard) = self.worker.lock()
            && let Some(worker) = guard.take()
        {
            let _ = worker.join();
        }
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Runs deterministic pre-warm across up to `top_n` hot queries against `candidate`
/// (`RUN-004`, `ADR-009` §6).
#[must_use]
pub fn prewarm_snapshot(
    candidate: &Snapshot,
    hot_queries: &[HotQuery],
    top_n: usize,
    plan_config: &PlanConfig,
) -> PrewarmReport {
    let empty_vars = serde_json::Map::new();
    let total = hot_queries.len().min(top_n);
    let mut passed = 0_usize;
    let mut failures = Vec::new();

    for q in hot_queries.iter().take(top_n) {
        match plan_query(
            candidate,
            &q.query,
            q.operation_name.as_deref(),
            &empty_vars,
            plan_config,
        ) {
            Ok(_) => passed += 1,
            Err(err) => failures.push(PrewarmFailure {
                query_id: q.id.clone(),
                code: err.code().as_str().to_owned(),
                reason: err.message().to_owned(),
            }),
        }
    }

    let pass_rate = if total == 0 {
        1.0
    } else {
        passed as f64 / total as f64
    };

    PrewarmReport {
        total,
        passed,
        failures,
        pass_rate,
    }
}

/// Entry retained in the bounded structural-sharing generation ring (`ADR-009` §4, `RUN-007`).
#[derive(Debug, Clone)]
pub struct RingGeneration {
    /// Retained immutable snapshot (sharing unchanged `Arc`s / `gap_core::snapshot::OrdMap` nodes).
    pub snapshot: Arc<Snapshot>,
    /// Marked `true` after automatic or explicit rollback so it cannot be re-promoted without `--force`.
    pub bad: bool,
    /// Recorded diagnostic reason if `bad == true`.
    pub bad_reason: Option<String>,
}

/// Registry holding staged shadow snapshots keyed by `(TenantId, ProposalId)` (`ING-050`).
///
/// Shadow snapshots are isolated from live client `SnapshotHandle::pin()` and are reachable
/// only with `X-Gap-Snapshot: shadow:<proposal_id>` by callers carrying `admin:schema`.
#[derive(Debug, Default)]
pub struct ShadowRegistry {
    staged: Mutex<BTreeMap<(TenantId, ProposalId), Arc<Snapshot>>>,
}

impl ShadowRegistry {
    /// Creates an empty [`ShadowRegistry`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stages a shadow snapshot for `(tenant_id, proposal_id)` (`ING-050`).
    pub fn stage(&self, tenant_id: TenantId, proposal_id: ProposalId, snap: Arc<Snapshot>) {
        if let Ok(mut guard) = self.staged.lock() {
            guard.insert((tenant_id, proposal_id), snap);
        }
    }

    /// Removes a shadow snapshot once promoted or discarded.
    pub fn remove(&self, tenant_id: &TenantId, proposal_id: ProposalId) {
        if let Ok(mut guard) = self.staged.lock() {
            guard.remove(&(tenant_id.clone(), proposal_id));
        }
    }

    /// Resolves `X-Gap-Snapshot: shadow:<proposal_id>` iff caller scopes contain `admin:schema` (`ING-050`).
    pub fn resolve_header(
        &self,
        tenant_id: &TenantId,
        header_value: &str,
        caller_scopes: &[String],
    ) -> Result<Arc<Snapshot>, PublisherError> {
        if !caller_scopes.iter().any(|s| s == ADMIN_SCHEMA_SCOPE) {
            return Err(PublisherError::ShadowAccess(
                "missing required `admin:schema` scope for shadow snapshot inspection (ING-050)"
                    .to_owned(),
            ));
        }
        let Some(id_str) = header_value.trim().strip_prefix(SHADOW_HEADER_PREFIX) else {
            return Err(PublisherError::ShadowAccess(format!(
                "expected `{SHADOW_HEADER_PREFIX}<proposal_id>` header"
            )));
        };
        let proposal_id: ProposalId = id_str.parse().map_err(|_| {
            PublisherError::ShadowAccess(format!("invalid proposal_id `{id_str}` in shadow header"))
        })?;
        let guard = self
            .staged
            .lock()
            .map_err(|_| PublisherError::ShadowAccess("shadow registry poisoned".to_owned()))?;
        guard
            .get(&(tenant_id.clone(), proposal_id))
            .cloned()
            .ok_or_else(|| {
                PublisherError::ShadowAccess(format!("shadow snapshot `{proposal_id}` not found"))
            })
    }
}

/// Queued change entry accepted by [`CoalescingPublishQueue`] (`RUN-006`).
#[derive(Debug, Clone)]
pub struct PendingBranchUpdate {
    /// Optional ingestion proposal triggering this change.
    pub proposal_id: Option<ProposalId>,
    /// Branch inputs to compose (or replace/upsert).
    pub inputs: Vec<BranchInput>,
    /// Timestamp when the update was queued.
    pub queued_at: Timestamp,
}

/// Bounded coalescing queue merging rapid proposal updates within `coalesce_window` (`RUN-006`).
#[derive(Debug, Default)]
pub struct CoalescingPublishQueue {
    pending: Mutex<Vec<PendingBranchUpdate>>,
}

impl CoalescingPublishQueue {
    /// Creates an empty [`CoalescingPublishQueue`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a branch update batch for coalesced composition and publication.
    pub fn enqueue(&self, update: PendingBranchUpdate) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.push(update);
        }
    }

    /// Drains all currently queued updates, deduplicating branch UIDs (last-writer wins per UID)
    /// and collecting every coalesced [`ProposalId`].
    #[must_use]
    pub fn drain_coalesced(&self) -> (Vec<BranchInput>, Vec<ProposalId>) {
        let items = match self.pending.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => return (Vec::new(), Vec::new()),
        };

        let mut by_uid: BTreeMap<BranchUid, BranchInput> = BTreeMap::new();
        let mut proposals = Vec::new();
        for item in items {
            if let Some(pid) = item.proposal_id {
                proposals.push(pid);
            }
            for branch in item.inputs {
                by_uid.insert(branch.uid, branch);
            }
        }
        (by_uid.into_values().collect(), proposals)
    }

    /// Composes `coalesced` inputs (optionally preserving untouched branches from `current`)
    /// into a candidate next-generation [`Snapshot`] using incremental `SnapshotBuilder` (`ARC-005`).
    pub fn build_coalesced_snapshot(
        current: &Snapshot,
        updates: Vec<BranchInput>,
        proposals: &[ProposalId],
        built_at: Timestamp,
    ) -> Result<Snapshot, PublisherError> {
        let mut sorted_inputs = updates;
        sorted_inputs.sort_by(|a, b| a.id.cmp(&b.id));
        let composition: Composition = compose(&sorted_inputs)?;
        let builder_label = if proposals.len() > 1 {
            format!("gap-publisher(coalesced:{})", proposals.len())
        } else {
            "gap-publisher".to_owned()
        };
        let next = SnapshotBuilder::new(current, composition)
            .built_at(built_at)
            .builder(builder_label)
            .proposal_id(proposals.last().copied())
            .build();
        Ok(next)
    }
}

#[derive(Debug)]
struct PublisherInner {
    ring: VecDeque<RingGeneration>,
    bad_hashes: BTreeMap<[u8; 32], u64>,
    hot_queries: Vec<HotQuery>,
    audit_log: Vec<PublishAuditRecord>,
}

/// Control-plane actor serializing all snapshot publications, pre-warms, rollbacks, and ring
/// management (`RUN-003`, `RUN-004`, `RUN-007`, `INV-16`, `ADR-009`).
///
/// Readers never touch the mutex inside `PublisherActor`; they call [`Self::pin`] which forwards
/// directly to [`SnapshotHandle::pin`] (`ArcSwap::load_full`, `INV-01`, `INV-02`).
#[derive(Debug)]
pub struct PublisherActor {
    handle: Arc<SnapshotHandle>,
    config: PublisherConfig,
    reclaimer: Arc<ReclaimerHandle>,
    shadows: Arc<ShadowRegistry>,
    state: Mutex<PublisherInner>,
}

impl PublisherActor {
    /// Initializes a [`PublisherActor`] wrapping `initial` and spawning a dedicated background
    /// reclaimer thread.
    #[must_use]
    pub fn new(initial: Snapshot, config: PublisherConfig) -> Self {
        let handle = Arc::new(SnapshotHandle::new(initial));
        let initial_arc = handle.pin();
        let mut ring = VecDeque::with_capacity(config.ring_capacity.max(1));
        ring.push_back(RingGeneration {
            snapshot: initial_arc,
            bad: false,
            bad_reason: None,
        });

        Self {
            handle,
            config,
            reclaimer: Arc::new(ReclaimerHandle::start(DEFAULT_RECLAIMER_QUEUE_CAP)),
            shadows: Arc::new(ShadowRegistry::new()),
            state: Mutex::new(PublisherInner {
                ring,
                bad_hashes: BTreeMap::new(),
                hot_queries: Vec::new(),
                audit_log: Vec::new(),
            }),
        }
    }

    /// Wait-free request-path handle to pin the current generation (`INV-01`, `INV-02`).
    #[inline]
    #[must_use]
    pub fn pin(&self) -> Arc<Snapshot> {
        self.handle.pin()
    }

    /// Returns a shared reference to the underlying wait-free [`SnapshotHandle`].
    #[must_use]
    pub fn handle(&self) -> Arc<SnapshotHandle> {
        Arc::clone(&self.handle)
    }

    /// Returns the staged shadow snapshot registry (`ING-050`).
    #[must_use]
    pub fn shadows(&self) -> Arc<ShadowRegistry> {
        Arc::clone(&self.shadows)
    }

    /// Returns a reference to the background generation reclaimer metrics (`RUN-004`).
    #[must_use]
    pub fn reclaimer_metrics(&self) -> ReclaimerMetrics {
        self.reclaimer.metrics()
    }

    /// Triggers one synchronous flush of the background reclaimer loop.
    pub fn sync_reclaimer(&self) {
        self.reclaimer.sync_pass();
    }

    /// Registers default hot queries for pre-warm verification (`RUN-004`).
    pub fn set_hot_queries(&self, queries: Vec<HotQuery>) {
        if let Ok(mut guard) = self.state.lock() {
            guard.hot_queries = queries;
        }
    }

    /// Publishes `next` exclusively through the serialized actor (`RUN-003`, `RUN-004`, `INV-16`):
    /// 1. Validates `format_version == SNAPSHOT_FORMAT_VERSION` and `generation == current + 1`.
    /// 2. Rejects any `branch_set_hash` previously marked `bad` unless `opts.force == true`.
    /// 3. Pre-warms up to `top_n` hot plans (`>= 95%` pass gate unless `breaking_change_ack` valid).
    /// 4. Atomically swaps `SnapshotHandle`, retires evicted ring entries to the off-thread reclaimer,
    ///    and appends a [`PublishAuditRecord`].
    pub fn publish(
        &self,
        next: Snapshot,
        opts: &PublishOptions,
        proposals: Vec<ProposalId>,
    ) -> Result<PublishAuditRecord, PublisherError> {
        let mut inner = self.state.lock().map_err(|_| {
            PublisherError::ReclaimerUnavailable("publisher lock poisoned".to_owned())
        })?;

        let current_arc = self.handle.pin();
        let current_gen = current_arc.generation;

        if next.format_version != SNAPSHOT_FORMAT_VERSION {
            return Err(PublisherError::Handle(PublishError::UnsupportedFormat {
                got: next.format_version,
                supported: SNAPSHOT_FORMAT_VERSION,
            }));
        }
        let expected_gen = current_gen.saturating_add(1);
        if next.generation != expected_gen {
            return Err(PublisherError::Handle(PublishError::NonMonotonic {
                expected: expected_gen,
                got: next.generation,
            }));
        }

        if !opts.force
            && let Some(&bad_gen) = inner.bad_hashes.get(&next.branch_set_hash)
        {
            return Err(PublisherError::BadGenerationBlocked {
                bad_generation: bad_gen,
            });
        }

        let hot_queries = opts
            .hot_queries_override
            .as_deref()
            .unwrap_or(&inner.hot_queries);
        let prewarm = prewarm_snapshot(
            &next,
            hot_queries,
            self.config.prewarm_top_n,
            &self.config.plan_config,
        );

        let ack_valid = opts
            .breaking_change_ack
            .as_ref()
            .is_some_and(BreakingChangeAck::is_valid);
        if prewarm.total > 0 && prewarm.pass_rate < self.config.min_prewarm_pass_rate && !ack_valid
        {
            return Err(PublisherError::PrewarmRegression {
                passed: prewarm.passed,
                total: prewarm.total,
                pass_rate: prewarm.pass_rate,
                min_rate: self.config.min_prewarm_pass_rate,
            });
        }

        let next_gen = next.generation;
        let next_hash = next.branch_set_hash;

        let swap_start = Instant::now();
        // Publish directly so `handle` and `inner.ring` share the exact same `Arc<Snapshot>`;
        // because `inner.ring` holds the prior generation's `Arc`, `SnapshotHandle::publish`
        // never drops a retired generation on the swap thread, and the off-thread reclaimer
        // drops retired generations only once all reader pins release (`ADR-009` §2).
        let _ = self.handle.publish(next)?;
        let swap_us = u64::try_from(swap_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let next_arc = self.handle.pin();

        let ring_cap = self.config.ring_capacity.max(1);
        inner.ring.push_back(RingGeneration {
            snapshot: next_arc,
            bad: false,
            bad_reason: None,
        });
        while inner.ring.len() > ring_cap {
            if let Some(evicted) = inner.ring.pop_front() {
                self.reclaimer.retire(evicted.snapshot)?;
            }
        }

        let note = match opts.breaking_change_ack.as_ref().filter(|a| a.is_valid()) {
            Some(ack) => format!("breaking_change_ack({}) : {}", ack.actor, ack.reason),
            None => "normal publish".to_owned(),
        };

        let record = PublishAuditRecord {
            generation: next_gen,
            parent_generation: Some(current_gen),
            branch_set_hash: next_hash,
            proposals,
            prewarm,
            is_rollback: false,
            note,
            swap_us,
        };
        inner.audit_log.push(record.clone());
        Ok(record)
    }

    /// Marks `bad_gen` as `bad = true` and publishes a new monotonic generation (`current + 1`)
    /// restoring the state of `target_gen` retained in the ring (`RUN-007`, `INV-16`, `ADR-009` §5).
    pub fn rollback_to_generation(
        &self,
        target_gen: u64,
        bad_gen: u64,
        reason: &str,
        force: bool,
    ) -> Result<PublishAuditRecord, PublisherError> {
        let mut inner = self.state.lock().map_err(|_| {
            PublisherError::ReclaimerUnavailable("publisher lock poisoned".to_owned())
        })?;

        let oldest = inner.ring.front().map(|e| e.snapshot.generation);
        let target_entry = inner
            .ring
            .iter()
            .find(|e| e.snapshot.generation == target_gen)
            .cloned()
            .ok_or(PublisherError::GenerationNotRetained {
                target: target_gen,
                oldest,
            })?;

        if target_entry.bad && !force {
            return Err(PublisherError::RollbackTargetMarkedBad { target: target_gen });
        }

        let mut recorded_bad_hashes = Vec::new();
        for entry in &mut inner.ring {
            if entry.snapshot.generation == bad_gen {
                entry.bad = true;
                entry.bad_reason = Some(reason.to_owned());
                recorded_bad_hashes.push(entry.snapshot.branch_set_hash);
            }
        }
        for bad_hash in recorded_bad_hashes {
            inner.bad_hashes.insert(bad_hash, bad_gen);
        }

        let current_arc = self.handle.pin();
        let current_gen = current_arc.generation;
        let new_gen = current_gen.saturating_add(1);

        let mut restored = target_entry.snapshot.next_generation(
            new_gen,
            Timestamp::UNIX_EPOCH,
            format!("rollback-to-gen-{target_gen}"),
            None,
        );
        restored.parent_set_restore_preserve(&target_entry.snapshot);

        let prewarm = prewarm_snapshot(
            &restored,
            &inner.hot_queries,
            self.config.prewarm_top_n,
            &self.config.plan_config,
        );

        let next_hash = restored.branch_set_hash;
        let swap_start = Instant::now();
        let _ = self.handle.publish(restored)?;
        let swap_us = u64::try_from(swap_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let next_arc = self.handle.pin();

        let ring_cap = self.config.ring_capacity.max(1);
        inner.ring.push_back(RingGeneration {
            snapshot: next_arc,
            bad: false,
            bad_reason: None,
        });
        while inner.ring.len() > ring_cap {
            if let Some(evicted) = inner.ring.pop_front() {
                self.reclaimer.retire(evicted.snapshot)?;
            }
        }

        let record = PublishAuditRecord {
            generation: new_gen,
            parent_generation: Some(current_gen),
            branch_set_hash: next_hash,
            proposals: Vec::new(),
            prewarm,
            is_rollback: true,
            note: format!("rollback(target={target_gen}, bad={bad_gen}): {reason}"),
            swap_us,
        };
        inner.audit_log.push(record.clone());
        Ok(record)
    }

    /// Publishes `G+2` isolating and quarantining ONLY `bad_branch` while preserving all other
    /// healthy branch updates from `bad_gen` (`RUN-007`, `ADR-009` §5).
    pub fn quarantine_branch_and_publish(
        &self,
        bad_branch: BranchUid,
        fallback_clean_gen: u64,
        bad_gen: u64,
        reason: &str,
    ) -> Result<PublishAuditRecord, PublisherError> {
        let mut inner = self.state.lock().map_err(|_| {
            PublisherError::ReclaimerUnavailable("publisher lock poisoned".to_owned())
        })?;

        let mut recorded_bad_hashes = Vec::new();
        for entry in &mut inner.ring {
            if entry.snapshot.generation == bad_gen {
                entry.bad = true;
                entry.bad_reason = Some(reason.to_owned());
                recorded_bad_hashes.push(entry.snapshot.branch_set_hash);
            }
        }
        for bad_hash in recorded_bad_hashes {
            inner.bad_hashes.insert(bad_hash, bad_gen);
        }

        let current_arc = self.handle.pin();
        let current_gen = current_arc.generation;
        let new_gen = current_gen.saturating_add(1);

        let mut next = current_arc.next_generation(
            new_gen,
            Timestamp::UNIX_EPOCH,
            "rollback-quarantine-branch",
            None,
        );

        let clean_branch = inner
            .ring
            .iter()
            .find(|e| e.snapshot.generation == fallback_clean_gen)
            .and_then(|e| e.snapshot.branches.get(&bad_branch))
            .cloned();

        // Mark `bad_branch` Quarantined (or fall back to clean descriptor marked Quarantined so no traffic routes to the broken version).
        if let Some(existing) = next.branches.get(&bad_branch).cloned() {
            let mut updated: BranchDescriptor = (*existing).clone();
            updated.status = BranchStatus::Quarantined;
            next.branches.insert(bad_branch, Arc::new(updated));
        } else if let Some(prev_desc) = clean_branch {
            let mut updated: BranchDescriptor = (*prev_desc).clone();
            updated.status = BranchStatus::Quarantined;
            next.branches.insert(bad_branch, Arc::new(updated));
        }

        // Disable active join edges originating on `bad_branch`.
        let filtered_joins: Vec<_> = next
            .joins
            .all_edges()
            .iter()
            .map(|e| {
                let mut cloned = e.clone();
                if cloned.source_branch == bad_branch && cloned.status == JoinStatus::Active {
                    cloned.status = JoinStatus::Quarantined;
                }
                cloned
            })
            .collect();
        next.joins = Arc::new(JoinGraph::new(filtered_joins));
        next.branch_set_hash = compute_branch_set_digest(&next.branches);

        let prewarm = prewarm_snapshot(
            &next,
            &inner.hot_queries,
            self.config.prewarm_top_n,
            &self.config.plan_config,
        );

        let next_hash = next.branch_set_hash;
        let swap_start = Instant::now();
        let _ = self.handle.publish(next)?;
        let swap_us = u64::try_from(swap_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let next_arc = self.handle.pin();

        let ring_cap = self.config.ring_capacity.max(1);
        inner.ring.push_back(RingGeneration {
            snapshot: next_arc,
            bad: false,
            bad_reason: None,
        });
        while inner.ring.len() > ring_cap {
            if let Some(evicted) = inner.ring.pop_front() {
                self.reclaimer.retire(evicted.snapshot)?;
            }
        }

        let record = PublishAuditRecord {
            generation: new_gen,
            parent_generation: Some(current_gen),
            branch_set_hash: next_hash,
            proposals: Vec::new(),
            prewarm,
            is_rollback: true,
            note: format!("quarantined branch `{bad_branch}` in gen {bad_gen}: {reason}"),
            swap_us,
        };
        inner.audit_log.push(record.clone());
        Ok(record)
    }

    /// Publishes `G+2` quarantining ONLY a faulty `JoinEdge` while leaving all branches intact
    /// (`ADR-009` §5).
    pub fn quarantine_join_and_publish(
        &self,
        bad_join_id: &str,
        bad_gen: u64,
        reason: &str,
    ) -> Result<PublishAuditRecord, PublisherError> {
        let mut inner = self.state.lock().map_err(|_| {
            PublisherError::ReclaimerUnavailable("publisher lock poisoned".to_owned())
        })?;

        for entry in &mut inner.ring {
            if entry.snapshot.generation == bad_gen {
                entry.bad = true;
                entry.bad_reason = Some(reason.to_owned());
            }
        }

        let current_arc = self.handle.pin();
        let current_gen = current_arc.generation;
        let new_gen = current_gen.saturating_add(1);

        let mut next = current_arc.next_generation(
            new_gen,
            Timestamp::UNIX_EPOCH,
            "rollback-quarantine-join",
            None,
        );
        let filtered_joins: Vec<_> = next
            .joins
            .all_edges()
            .iter()
            .map(|e| {
                let mut cloned = e.clone();
                if cloned.id == bad_join_id {
                    cloned.status = JoinStatus::Quarantined;
                }
                cloned
            })
            .collect();
        next.joins = Arc::new(JoinGraph::new(filtered_joins));

        let prewarm = prewarm_snapshot(
            &next,
            &inner.hot_queries,
            self.config.prewarm_top_n,
            &self.config.plan_config,
        );

        let next_hash = next.branch_set_hash;
        let swap_start = Instant::now();
        let _ = self.handle.publish(next)?;
        let swap_us = u64::try_from(swap_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let next_arc = self.handle.pin();

        let ring_cap = self.config.ring_capacity.max(1);
        inner.ring.push_back(RingGeneration {
            snapshot: next_arc,
            bad: false,
            bad_reason: None,
        });
        while inner.ring.len() > ring_cap {
            if let Some(evicted) = inner.ring.pop_front() {
                self.reclaimer.retire(evicted.snapshot)?;
            }
        }

        let record = PublishAuditRecord {
            generation: new_gen,
            parent_generation: Some(current_gen),
            branch_set_hash: next_hash,
            proposals: Vec::new(),
            prewarm,
            is_rollback: true,
            note: format!("quarantined join `{bad_join_id}` in gen {bad_gen}: {reason}"),
            swap_us,
        };
        inner.audit_log.push(record.clone());
        Ok(record)
    }

    /// Returns a snapshot of the ring buffer generation metadata (ordered oldest to newest).
    #[must_use]
    pub fn ring_generations(&self) -> Vec<(u64, bool)> {
        self.state.lock().map_or_else(
            |_| Vec::new(),
            |g| {
                g.ring
                    .iter()
                    .map(|e| (e.snapshot.generation, e.bad))
                    .collect()
            },
        )
    }

    /// Returns a cloned copy of the publish audit log.
    #[must_use]
    pub fn audit_log(&self) -> Vec<PublishAuditRecord> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.audit_log.clone())
    }
}

trait SnapshotRestoreExt {
    fn parent_set_restore_preserve(&mut self, source: &Snapshot);
}

impl SnapshotRestoreExt for Snapshot {
    fn parent_set_restore_preserve(&mut self, source: &Snapshot) {
        // `next_generation` already copies `branches`, `groups`, `schema`, `ownership`, `joins`,
        // `index`, `branch_set_hash`; explicitly preserve `branch_set_hash` and provenance parent.
        self.branch_set_hash = source.branch_set_hash;
    }
}

fn compute_branch_set_digest(
    branches: &gap_core::snapshot::OrdMap<BranchUid, Arc<BranchDescriptor>>,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for (uid, desc) in branches {
        hasher.update(uid.as_uuid().as_bytes());
        hasher.update(&desc.spec_hash);
        hasher.update(&[match desc.status {
            BranchStatus::Active => 0,
            BranchStatus::Drift => 1,
            BranchStatus::Quarantined => 2,
            BranchStatus::Disabled => 3,
        }]);
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use gap_core::ids::BranchId;
    use gap_core::snapshot::{Endpoint, Protocol, TrustTier};
    use gap_proto::ngc::v1::{
        Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits,
        MetaSummary, OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, SideEffectHint,
    };

    fn uid_for(idx: u16) -> BranchUid {
        let hi = (idx >> 8) as u8;
        let lo = (idx & 0xff) as u8;
        BranchUid::from_unix_ms(
            1_761_900_000_000 + u64::from(idx),
            [hi, lo, 0, 0, 0, 0, 0, 0, 0, 0],
        )
    }

    fn make_branch(idx: u16, field_name: &str, rev: u32) -> BranchInput {
        let slug = format!("svc{idx:03}");
        let sdl = format!("type Query {{ {field_name}(id: ID!): String }}\n");
        let op = ProtoOp {
            name: field_name.to_owned(),
            kind: ProtoOpKind::Query as i32,
            description: format!("op {field_name}"),
            input_schema: None,
            output_schema: None,
            side_effect_hint: SideEffectHint::SeNone as i32,
            idempotent_hint: true,
            supports_dry_run: false,
            compensation_operation: String::new(),
            examples: Vec::new(),
            cost_weight: 1,
            pagination: 0,
            key_fields: Vec::new(),
            annotations: HashMap::new(),
        };

        BranchInput {
            id: BranchId::new(&slug).expect("valid branch id"),
            uid: uid_for(idx),
            protocol: Protocol::Rest,
            trust: TrustTier::T1,
            endpoint: Endpoint {
                uri: format!("https://{slug}.internal/ngc"),
                transport: "grpc".to_owned(),
            },
            description: ConnectorDescription {
                connector_id: slug,
                connector_version: format!("1.{rev}.0"),
                ngc_version: "gap/0.1".to_owned(),
                protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
                spec_hash: format!("{idx:063x}{rev:x}"),
                capabilities: Some(ProtoCapabilities::default()),
                limits: Some(ProtoLimits {
                    max_batch_size: 64,
                    max_in_flight: 32,
                    max_page_size: 100,
                    default_timeout_ms: 2_000,
                    max_response_bytes: 1_048_576,
                }),
                schema_sdl: sdl,
                operations: vec![op],
                meta: Some(MetaSummary {
                    node_categories: Vec::new(),
                    edge_predicates: Vec::new(),
                    synopsis_hint: format!("service {idx}"),
                }),
                auth: None,
            },
            status: BranchStatus::Active,
        }
    }

    fn build_next(cur: &Snapshot, branches: &[BranchInput]) -> Snapshot {
        let comp = compose(branches).expect("compose succeeds");
        SnapshotBuilder::new(cur, comp).build()
    }

    // spec: RUN-003, RUN-004, INV-16
    #[test]
    fn publisher_prewarm_95_percent_gate_and_breaking_change_ack() {
        let base = Snapshot::empty(0);
        let gen1 = build_next(&base, &[make_branch(1, "getUser", 1)]);
        let actor = PublisherActor::new(gen1, PublisherConfig::default());

        // Register 20 hot queries: 19 target `getUser`, 1 targets `getOrder` (will fail if removed).
        let mut hot = Vec::new();
        for i in 0..19 {
            hot.push(HotQuery {
                id: format!("q-user-{i}"),
                query: "{ getUser(id: \"1\") }".to_owned(),
                operation_name: None,
            });
        }
        hot.push(HotQuery {
            id: "q-order-0".to_owned(),
            query: "{ getOrder(id: \"1\") }".to_owned(),
            operation_name: None,
        });
        // Add a second query that keeps `getOrder` missing in gen2 -> 18/20 = 90% < 95%.
        hot.push(HotQuery {
            id: "q-order-1".to_owned(),
            query: "{ getOrder(id: \"2\") }".to_owned(),
            operation_name: None,
        });
        actor.set_hot_queries(hot);

        let cur = actor.pin();
        // Candidate gen2 removes `getOrder` (19/21 = 90.47% < 95%).
        let gen2 = build_next(&cur, &[make_branch(1, "getUser", 2)]);
        let err = actor
            .publish(gen2.clone(), &PublishOptions::default(), Vec::new())
            .expect_err("prewarm < 95% without breaking_change_ack must abort");
        assert!(matches!(err, PublisherError::PrewarmRegression { .. }));
        assert_eq!(actor.pin().generation, 1);

        // With valid `breaking_change_ack`, publication succeeds cleanly (`gen = 2`).
        let ack_opts = PublishOptions {
            breaking_change_ack: Some(BreakingChangeAck {
                actor: "admin@acme.test".to_owned(),
                reason: "planned v2 field deprecation".to_owned(),
            }),
            ..PublishOptions::default()
        };
        let rec = actor
            .publish(gen2, &ack_opts, vec![ProposalId::new_v7()])
            .expect("publish with breaking_change_ack succeeds");
        assert_eq!(rec.generation, 2);
        assert_eq!(actor.pin().generation, 2);
    }

    // spec: RUN-003, RUN-004, RUN-006
    #[test]
    fn publisher_off_thread_reclaimer_holds_pinned_arcs_and_retires_ring() {
        let base = Snapshot::empty(0);
        let gen1 = build_next(&base, &[make_branch(1, "ping", 1)]);
        let actor = PublisherActor::new(
            gen1,
            PublisherConfig {
                ring_capacity: 5,
                ..PublisherConfig::default()
            },
        );
        assert_ne!(actor.reclaimer_metrics().thread_id, thread::current().id());

        // Pin generation 1 like a long-running Task (`RUN-006`).
        let pinned_gen1 = actor.pin();
        assert_eq!(pinned_gen1.generation, 1);

        // Publish generations 2..=8 so gen 1, 2, 3 are evicted from the 5-gen ring to the reclaimer.
        for rev in 2..=8_u32 {
            let cur = actor.pin();
            let next = build_next(&cur, &[make_branch(1, &format!("pingV{rev}"), rev)]);
            actor
                .publish(next, &PublishOptions::default(), Vec::new())
                .expect("publish next generation");
        }
        assert_eq!(actor.pin().generation, 8);
        assert_eq!(
            actor.ring_generations(),
            vec![(4, false), (5, false), (6, false), (7, false), (8, false)]
        );

        // Note: `handle.cell` also holds one extra clone of the *latest* gen (`8`), while
        // retired gen `1` stays pinned in `pinned_gen1` (`strong_count > 1` inside reclaimer).
        actor.sync_reclaimer();
        let before_drop = actor.reclaimer_metrics();
        assert_eq!(before_drop.enqueued, 3);
        assert!(
            before_drop.pending >= 1,
            "pinned gen1 must remain held off-thread until request drops pin"
        );

        // Dropping the pinned request allows the background reclaimer thread to drop gen 1.
        drop(pinned_gen1);
        actor.sync_reclaimer();
        let after_drop = actor.reclaimer_metrics();
        assert_eq!(after_drop.reclaimed, 3);
        assert_eq!(after_drop.pending, 0);
    }

    // spec: ING-050, INV-16, RUN-003
    #[test]
    fn publisher_coalescing_queue_bad_generation_block_and_shadow_isolation() {
        let base = Snapshot::empty(0);
        let gen1 = build_next(&base, &[make_branch(1, "getAlpha", 1)]);
        let actor = PublisherActor::new(gen1, PublisherConfig::default());

        // Stage shadow snapshot under `shadow:<proposal_id>` (`ING-050`): invisible to live `pin()`.
        let proposal_a = ProposalId::new_v7();
        let proposal_b = ProposalId::new_v7();
        let queue = CoalescingPublishQueue::new();
        queue.enqueue(PendingBranchUpdate {
            proposal_id: Some(proposal_a),
            inputs: vec![make_branch(1, "getAlpha", 2)],
            queued_at: Timestamp::UNIX_EPOCH,
        });
        queue.enqueue(PendingBranchUpdate {
            proposal_id: Some(proposal_b),
            inputs: vec![make_branch(1, "getAlpha", 2), make_branch(2, "getBeta", 1)],
            queued_at: Timestamp::UNIX_EPOCH,
        });

        let (coalesced_inputs, proposals) = queue.drain_coalesced();
        assert_eq!(proposals.len(), 2);
        let cur = actor.pin();
        let shadow_snap = CoalescingPublishQueue::build_coalesced_snapshot(
            &cur,
            coalesced_inputs,
            &proposals,
            Timestamp::UNIX_EPOCH,
        )
        .expect("coalesced build succeeds");
        actor.shadows().stage(
            cur.tenant_id.clone(),
            proposal_b,
            Arc::new(shadow_snap.clone()),
        );

        // Live pin still sees generation 1 (`ING-050`).
        assert_eq!(actor.pin().generation, 1);

        // Unauthorized caller without `admin:schema` is rejected (`ING-050`).
        let header = format!("shadow:{proposal_b}");
        let denied =
            actor
                .shadows()
                .resolve_header(&cur.tenant_id, &header, &["query:read".to_owned()]);
        assert!(matches!(denied, Err(PublisherError::ShadowAccess(_))));

        // Authorized caller with `admin:schema` resolves staged shadow snapshot.
        let resolved = actor
            .shadows()
            .resolve_header(&cur.tenant_id, &header, &[ADMIN_SCHEMA_SCOPE.to_owned()])
            .expect("admin:schema resolves staged shadow");
        assert_eq!(resolved.generation, 2);

        // Publish gen 2, roll back to gen 1 (marking gen 2 bad), and verify gen 2 hash is blocked without --force.
        actor
            .publish(shadow_snap.clone(), &PublishOptions::default(), proposals)
            .expect("publish gen 2");
        let bad_hash = shadow_snap.branch_set_hash;
        let rb = actor
            .rollback_to_generation(1, 2, "regression in Beta", false)
            .expect("rollback produces gen 3");
        assert_eq!(rb.generation, 3);
        assert!(rb.is_rollback);
        assert_eq!(actor.pin().generation, 3);

        let mut re_promote = shadow_snap;
        re_promote.generation = 4;
        re_promote.branch_set_hash = bad_hash;
        let blocked = actor.publish(re_promote.clone(), &PublishOptions::default(), Vec::new());
        assert!(matches!(
            blocked,
            Err(PublisherError::BadGenerationBlocked { bad_generation: 2 })
        ));

        // Re-promoting with `force = true` succeeds explicitly (`ADR-009` §5).
        let forced = actor
            .publish(
                re_promote,
                &PublishOptions {
                    force: true,
                    ..PublishOptions::default()
                },
                Vec::new(),
            )
            .expect("forced publish succeeds");
        assert_eq!(forced.generation, 4);
    }
}
