//! ConnectorLayer v1 (`NGC-020`, `NGC-025`, `NGC-071`, `RTE-073`..`RTE-093`, `INV-02`, `INV-12`, `INV-15`):
//! object-safe [`BranchClient`], [`InProcess`] adapter, and lock-free [`ConnectorRegistry`] with
//! P2C replica choice, circuit-breaker admission, retries with jittered backoff, health polling,
//! per-branch metrics, and non-queueing per-replica bulkheads.
//!
//! # Call accounting rule
//!
//! Every completed call is recorded **exactly once** against its telemetry slot.
//! A call is *complete* when its stream returns `Poll::Ready(None)` (success) or a stream `Err`
//! item (failure). A call *dropped* by the caller — via cancel token, deadline expiry, or future
//! drop — is **not** an outcome and produces **no** record; probe permits released this way do not
//! count as either success or failure, so a cancel storm cannot stall a half-open episode.
//!
//! # Why retries never replay delivered rows
//!
//! A retry wraps the entire `BranchClient::query` call: it is started from scratch against
//! a **different** replica (the failed one is excluded). Rows already delivered to the caller by
//! a previous attempt's stream are already in the caller's buffer; the retry opens a fresh stream
//! and the caller reads new rows from that. Mutations are **never** retried — only `Query` is
//! idempotent.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::Stream;
use gap_balance::breaker::Admission;
use gap_balance::slot::{Outcome, TelemetrySlot};
use gap_balance::time::Now;
use gap_balance::{Candidate, InFlightGuard, ProbePermit, XorShift64, select_excluding};
use gap_core::clock::{Clock, SystemClock};
use gap_core::error::{ErrorCode, GapError, GapResult, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::BranchUid;
use gap_core::snapshot::{BranchDescriptor, BranchLimits, Snapshot};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::grpc::GrpcClient;
use gap_proto::ngc::v1::{
    HealthRequest, HealthResponse, QueryRequest, QueryResponse, ServingStatus,
};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant as TokioInstant, Sleep, sleep, sleep_until};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

pub mod health;
pub mod replica;
pub mod resilience;
pub mod stats;

pub use health::HealthPollers;
pub use replica::{Replica, ReplicaSpec};
pub use resilience::ResilienceConfig;
pub use stats::{AtomicCounters, BranchStats, ReplicaStats};

use resilience::{classify_error_code, is_retryable, jittered_backoff};

// ─── Streams ─────────────────────────────────────────────────────────────────

/// Stream wrapper enforcing `CallScope::cancel` and `CallScope::deadline` on a streamed query.
struct ScopedQueryStream {
    inner: Option<BoxStream<QueryResponse>>,
    cancel_fut: Pin<Box<WaitForCancellationFutureOwned>>,
    sleep: Pin<Box<Sleep>>,
}

impl ScopedQueryStream {
    fn new(
        inner: BoxStream<QueryResponse>,
        cancel: CancellationToken,
        deadline: TokioInstant,
    ) -> Self {
        Self {
            inner: Some(inner),
            cancel_fut: Box::pin(cancel.cancelled_owned()),
            sleep: Box::pin(sleep_until(deadline)),
        }
    }

    fn terminate(&mut self) {
        self.inner = None;
    }
}

impl Stream for ScopedQueryStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.inner.is_none() {
            return Poll::Ready(None);
        }
        if this.cancel_fut.as_mut().poll(cx).is_ready() {
            this.terminate();
            return Poll::Ready(Some(Err(NgcErrorInfo::new(
                NgcErrorCode::Cancelled,
                "call cancelled",
            ))));
        }
        if this.sleep.as_mut().poll(cx).is_ready() {
            this.terminate();
            return Poll::Ready(Some(Err(NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "call deadline exceeded",
            ))));
        }
        let Some(ref mut stream) = this.inner else {
            return Poll::Ready(None);
        };
        match stream.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.inner = None;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                this.inner = None;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(Ok(item))),
        }
    }
}

/// Stream wrapper that records a telemetry outcome exactly once at the first terminal event.
/// Dropped-without-completion = no record + probe permit released without outcome.
struct AccountedStream {
    inner: Option<BoxStream<QueryResponse>>,
    slot: Arc<TelemetrySlot>,
    counters: Arc<AtomicCounters>,
    probe_permit: Option<ProbePermit>,
    _in_flight: Option<InFlightGuard>,
    start: Instant,
    epoch: Instant,
    clock: Arc<dyn Clock>,
    recorded: bool,
}

impl AccountedStream {
    fn elapsed_now(&self) -> Now {
        let wall = self.clock.now_instant();
        let elapsed = wall.saturating_duration_since(self.epoch);
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        Now::from_ms(ms)
    }

    fn record(&mut self, outcome: Outcome) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let latency = self.start.elapsed();
        let now = self.elapsed_now();
        if let Some(permit) = self.probe_permit.take() {
            permit.record(outcome, latency, now);
        } else {
            self.slot.record(outcome, latency, now);
        }
        match outcome {
            Outcome::Ok => {
                self.counters.calls_ok.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::Failure => {
                self.counters.calls_failed.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::ClientError => {
                self.counters
                    .calls_client_error
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Stream for AccountedStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(ref mut stream) = this.inner else {
            return Poll::Ready(None);
        };
        match stream.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.inner = None;
                this._in_flight = None;
                this.record(Outcome::Ok);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                let outcome = classify_error_code(err.code);
                this.inner = None;
                this._in_flight = None;
                this.record(outcome);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(Ok(item))),
        }
    }
}

// ─── BranchClient ────────────────────────────────────────────────────────────

/// Object-safe NGC branch client trait (`NGC-020`, `NGC-025`).
pub trait BranchClient: Send + Sync + 'static {
    /// Opens a streaming `Query` RPC against this branch under `scope`.
    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>>;

    /// Reports the branch's health status.
    /// Default returns `Serving` so existing clients keep working without change.
    fn health(
        &self,
        _request: HealthRequest,
    ) -> Pin<Box<dyn Future<Output = NgcResult<HealthResponse>> + Send + '_>> {
        Box::pin(async {
            Ok(HealthResponse {
                status: ServingStatus::Serving as i32,
                version: String::new(),
                details: None,
            })
        })
    }
}

impl BranchClient for GrpcClient {
    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>> {
        Box::pin(async move {
            if scope.cancel.is_cancelled() {
                return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
            }
            if TokioInstant::now() >= scope.deadline {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "call deadline exceeded",
                ));
            }
            let cancel = scope.cancel.clone();
            let deadline = scope.deadline;
            let metadata = scope.metadata;
            let stream = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
                }
                () = sleep_until(deadline) => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "call deadline exceeded"));
                }
                res = self.query_with_metadata(request, &metadata) => res?,
            };
            Ok(Box::pin(ScopedQueryStream::new(stream, cancel, deadline)) as BoxStream<_>)
        })
    }

    fn health(
        &self,
        request: HealthRequest,
    ) -> Pin<Box<dyn Future<Output = NgcResult<HealthResponse>> + Send + '_>> {
        Box::pin(async move { self.health(request).await })
    }
}

/// In-process [`BranchClient`] adapter for socket-free unit and integration tests.
pub struct InProcess<C> {
    core: Arc<ServerCore<C>>,
}

impl<C> Clone for InProcess<C> {
    fn clone(&self) -> Self {
        Self {
            core: Arc::clone(&self.core),
        }
    }
}

impl<C> fmt::Debug for InProcess<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InProcess").finish_non_exhaustive()
    }
}

impl<C: Connector> InProcess<C> {
    /// Wraps an existing [`Arc<ServerCore<C>>`].
    #[must_use]
    pub const fn new(core: Arc<ServerCore<C>>) -> Self {
        Self { core }
    }

    /// Wraps `connector` in a fresh [`ServerCore`] with `config`.
    #[must_use]
    pub fn from_connector(connector: C, config: ServerConfig) -> Self {
        Self {
            core: Arc::new(ServerCore::new(connector, config)),
        }
    }

    /// Borrows the underlying [`ServerCore`].
    #[must_use]
    pub const fn core(&self) -> &Arc<ServerCore<C>> {
        &self.core
    }
}

impl<C: Connector> BranchClient for InProcess<C> {
    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>> {
        Box::pin(async move {
            if scope.cancel.is_cancelled() {
                return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
            }
            if TokioInstant::now() >= scope.deadline {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "call deadline exceeded",
                ));
            }
            let cancel = scope.cancel.clone();
            let deadline = scope.deadline;
            let metadata = scope.metadata;
            let core_stream = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
                }
                () = sleep_until(deadline) => {
                    return Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "call deadline exceeded"));
                }
                res = self.core.query(request, metadata, cancel.clone()) => res?,
            };
            let boxed: BoxStream<QueryResponse> = Box::pin(core_stream);
            Ok(Box::pin(ScopedQueryStream::new(boxed, cancel, deadline)) as BoxStream<_>)
        })
    }

    fn health(
        &self,
        request: HealthRequest,
    ) -> Pin<Box<dyn Future<Output = NgcResult<HealthResponse>> + Send + '_>> {
        Box::pin(async move {
            self.core
                .health(request, BTreeMap::new(), CancellationToken::new())
                .await
        })
    }
}

// ─── RegisteredBranch ────────────────────────────────────────────────────────

struct ReplicaEntry {
    replica: Replica,
    counters: Arc<AtomicCounters>,
}

/// Immutable per-branch client entry inside [`ConnectorRegistry`] (`INV-02`, `NGC-071`).
#[derive(Clone)]
pub struct RegisteredBranch {
    replicas: Vec<Arc<ReplicaEntry>>,
}

impl fmt::Debug for RegisteredBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredBranch")
            .field("replica_count", &self.replicas.len())
            .field("max_in_flight", &self.replicas[0].replica.max_in_flight)
            .field(
                "available_permits",
                &self.replicas[0].replica.bulkhead.available_permits(),
            )
            .finish()
    }
}

impl RegisteredBranch {
    fn new_single(replica: Replica) -> Self {
        Self {
            replicas: vec![Arc::new(ReplicaEntry {
                replica,
                counters: Arc::new(AtomicCounters::default()),
            })],
        }
    }

    fn new_multi(replicas: Vec<Replica>) -> Self {
        Self {
            replicas: replicas
                .into_iter()
                .map(|r| {
                    Arc::new(ReplicaEntry {
                        replica: r,
                        counters: Arc::new(AtomicCounters::default()),
                    })
                })
                .collect(),
        }
    }

    /// V0-compatible: client for the first replica.
    #[must_use]
    pub fn client(&self) -> &Arc<dyn BranchClient> {
        &self.replicas[0].replica.client
    }

    /// V0-compatible: `max_in_flight` for the first replica.
    #[must_use]
    pub fn max_in_flight(&self) -> u32 {
        self.replicas[0].replica.max_in_flight
    }

    /// V0-compatible: available permits for the first replica.
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.replicas[0].replica.bulkhead.available_permits()
    }

    /// Sensitive values across all replicas.
    pub fn sensitive_values(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for entry in &self.replicas {
            out.extend(entry.replica.sensitive_values());
        }
        out
    }
}

// ─── BranchQueryError ────────────────────────────────────────────────────────

/// Failure when opening a branch query through [`ConnectorRegistry::open_query`].
#[derive(Debug)]
pub enum BranchQueryError {
    /// Registry lookup, breaker rejection, or bulkhead saturation.
    Registry(GapError),
    /// Connector-returned `NgcErrorInfo`.
    Connector(NgcErrorInfo),
}

// ─── ConnectorRegistryBuilder ────────────────────────────────────────────────

/// Builder for an immutable [`ConnectorRegistry`].
pub struct ConnectorRegistryBuilder {
    entries: BTreeMap<BranchUid, RegisteredBranch>,
    clock: Arc<dyn Clock>,
    rng_seed: Option<u64>,
    resilience: ResilienceConfig,
}

impl Default for ConnectorRegistryBuilder {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            clock: Arc::new(SystemClock),
            rng_seed: None,
            resilience: ResilienceConfig::default(),
        }
    }
}

impl fmt::Debug for ConnectorRegistryBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectorRegistryBuilder")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl ConnectorRegistryBuilder {
    /// Injects a custom [`Clock`] for `Now` computation (default: `SystemClock`).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Injects a deterministic RNG seed for P2C candidate sampling.
    #[must_use]
    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = Some(seed);
        self
    }

    /// Sets the resilience configuration (breaker/retry/health params).
    #[must_use]
    pub fn with_resilience(mut self, cfg: ResilienceConfig) -> Self {
        self.resilience = cfg;
        self
    }

    fn sanitize_label(uid: &BranchUid, label: Option<&str>) -> Arc<str> {
        if let Some(l) = label
            && !l.is_empty()
            && l.len() <= 64
            && !l.chars().any(char::is_control)
        {
            return Arc::from(l);
        }
        let raw = uid.to_string();
        let s: String = raw
            .chars()
            .map(|c| if c.is_control() { '_' } else { c })
            .collect();
        Arc::from(s.as_str())
    }

    /// Registers `client` for `uid` (single replica, v0 compat).
    #[must_use]
    pub fn register(
        self,
        uid: BranchUid,
        client: Arc<dyn BranchClient>,
        max_in_flight: u32,
    ) -> Self {
        self.register_with_metadata(uid, client, max_in_flight, BTreeMap::new())
    }

    /// Registers `client` for `uid` with static call `metadata` (single replica, v0 compat).
    #[must_use]
    pub fn register_with_metadata(
        mut self,
        uid: BranchUid,
        client: Arc<dyn BranchClient>,
        max_in_flight: u32,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        let label = Self::sanitize_label(&uid, None);
        let params = gap_balance::slot::SlotParams {
            max_in_flight,
            default_timeout_ms: 5_000,
            label,
        };
        let replica = Replica::new(uid, params, ReplicaSpec { client, metadata });
        self.entries
            .insert(uid, RegisteredBranch::new_single(replica));
        self
    }

    /// Registers `client` for `branch` (single replica, v0 compat).
    #[must_use]
    pub fn register_branch(self, branch: &BranchDescriptor, client: Arc<dyn BranchClient>) -> Self {
        self.register(branch.uid, client, branch.limits.max_in_flight)
    }

    /// Registers multiple replicas for `uid` with P2C load balancing (`RTE-073`, `RTE-074`).
    #[must_use]
    pub fn register_replicas(
        mut self,
        uid: BranchUid,
        label: &str,
        default_timeout_ms: u32,
        max_in_flight: u32,
        specs: Vec<ReplicaSpec>,
    ) -> Self {
        let label_arc = Self::sanitize_label(&uid, Some(label));
        let replicas: Vec<Replica> = specs
            .into_iter()
            .enumerate()
            .map(|(i, spec)| {
                // Derive a stable tiebreak uid: encode (uid_bytes XOR index) into random_bytes.
                let uid_bytes = *uid.as_uuid().as_bytes();
                let idx_u32 = i as u32;
                let mut rand_bytes = [0u8; 10];
                rand_bytes[..8].copy_from_slice(&uid_bytes[8..]);
                rand_bytes[6] ^= (idx_u32 >> 8) as u8;
                rand_bytes[7] ^= idx_u32 as u8;
                // Extract unix_ms from UUIDv7 (top 48 bits of the 128-bit layout).
                let unix_ms_hi = u64::from(uid_bytes[0]) << 40
                    | u64::from(uid_bytes[1]) << 32
                    | u64::from(uid_bytes[2]) << 24
                    | u64::from(uid_bytes[3]) << 16
                    | u64::from(uid_bytes[4]) << 8
                    | u64::from(uid_bytes[5]);
                let tiebreak = BranchUid::from_unix_ms(unix_ms_hi, rand_bytes);
                let params = gap_balance::slot::SlotParams {
                    max_in_flight,
                    default_timeout_ms,
                    label: Arc::clone(&label_arc),
                };
                Replica::new(tiebreak, params, spec)
            })
            .collect();
        self.entries
            .insert(uid, RegisteredBranch::new_multi(replicas));
        self
    }

    /// Finalizes the immutable [`ConnectorRegistry`].
    #[must_use]
    pub fn build(self) -> ConnectorRegistry {
        let epoch = TokioInstant::now();
        ConnectorRegistry {
            entries: self.entries,
            clock: self.clock,
            epoch,
            rng_seed: self.rng_seed,
            resilience: self.resilience,
        }
    }
}

// ─── ConnectorRegistry ───────────────────────────────────────────────────────

/// Immutable per-branch client registry with P2C, circuit-breaker, retries, and health polling
/// (`INV-02`: `BTreeMap<BranchUid, RegisteredBranch>`, zero locks on read path).
pub struct ConnectorRegistry {
    entries: BTreeMap<BranchUid, RegisteredBranch>,
    clock: Arc<dyn Clock>,
    epoch: TokioInstant,
    rng_seed: Option<u64>,
    resilience: ResilienceConfig,
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        ConnectorRegistryBuilder::default().build()
    }
}

impl Clone for ConnectorRegistry {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            clock: Arc::clone(&self.clock),
            epoch: self.epoch,
            rng_seed: self.rng_seed,
            resilience: self.resilience.clone(),
        }
    }
}

impl fmt::Debug for ConnectorRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectorRegistry")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl ConnectorRegistry {
    /// Returns an empty [`ConnectorRegistry`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns a [`ConnectorRegistryBuilder`].
    #[must_use]
    pub fn builder() -> ConnectorRegistryBuilder {
        ConnectorRegistryBuilder::default()
    }

    /// Builds a [`ConnectorRegistry`] from a snapshot + client map (single replica per uid).
    #[must_use]
    pub fn from_snapshot(
        snapshot: &Snapshot,
        clients: BTreeMap<BranchUid, Arc<dyn BranchClient>>,
    ) -> Self {
        let mut builder = ConnectorRegistryBuilder::default();
        for (uid, client) in clients {
            let max_in_flight = snapshot.branches.get(&uid).map_or_else(
                || BranchLimits::default().max_in_flight,
                |b| b.limits.max_in_flight,
            );
            builder = builder.register(uid, client, max_in_flight);
        }
        builder.build()
    }

    fn now(&self) -> Now {
        let elapsed = self.epoch.elapsed();
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        Now::from_ms(ms)
    }

    /// Looks up a [`RegisteredBranch`] by uid (lock-free, `INV-02`).
    #[must_use]
    pub fn get(&self, uid: &BranchUid) -> Option<&RegisteredBranch> {
        self.entries.get(uid)
    }

    /// V0-compat: returns the first replica's client for `uid`.
    #[must_use]
    pub fn client(&self, uid: &BranchUid) -> Option<&Arc<dyn BranchClient>> {
        self.entries.get(uid).map(RegisteredBranch::client)
    }

    /// V0-compat: available permits for the first replica.
    #[must_use]
    pub fn available_permits(&self, uid: &BranchUid) -> Option<usize> {
        self.entries
            .get(uid)
            .map(RegisteredBranch::available_permits)
    }

    /// V0-compat: `max_in_flight` for the first replica.
    #[must_use]
    pub fn max_in_flight(&self, uid: &BranchUid) -> Option<u32> {
        self.entries.get(uid).map(RegisteredBranch::max_in_flight)
    }

    /// Number of registered branches.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no branches are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// V0-compat: acquires a bulkhead permit for the first replica without queueing.
    pub fn try_acquire_branch(
        &self,
        uid: &BranchUid,
    ) -> GapResult<(&RegisteredBranch, OwnedSemaphorePermit)> {
        let Some(entry) = self.entries.get(uid) else {
            return Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!("no client registered for branch `{uid}`"),
            ));
        };
        let permit = Arc::clone(&entry.replicas[0].replica.bulkhead)
            .try_acquire_owned()
            .map_err(|_| {
                GapError::new(
                    ErrorCode::Overloaded,
                    format!(
                        "branch `{uid}` bulkhead saturated (max_in_flight={})",
                        entry.replicas[0].replica.max_in_flight
                    ),
                )
                .with_retry(true, None)
            })?;
        Ok((entry, permit))
    }

    /// Returns a statistics snapshot for all registered branches (`OPS-020`, `INV-15`).
    #[must_use]
    pub fn branch_stats(&self) -> Vec<BranchStats> {
        let now = self.now();
        let mut result: Vec<BranchStats> = self
            .entries
            .values()
            .map(|branch| {
                let label = branch.replicas[0].replica.label.to_string();
                let replicas: Vec<ReplicaStats> = branch
                    .replicas
                    .iter()
                    .map(|entry| {
                        let view = entry.replica.slot.view(now);
                        ReplicaStats {
                            breaker_state: view.state,
                            in_flight: view.in_flight,
                            capacity: view.capacity,
                            latency_us: view.latency_us,
                            error_rate: view.error_rate,
                            retry_budget_millis: entry.replica.slot.retry_budget().balance_millis(),
                            counters: entry.counters.snapshot(),
                        }
                    })
                    .collect();
                BranchStats { label, replicas }
            })
            .collect();
        result.sort_by(|a, b| a.label.cmp(&b.label));
        result
    }

    /// Spawns one health-poller task per replica.
    /// Dropping the returned [`HealthPollers`] cancels all pollers.
    pub fn spawn_health_pollers(&self, cancel: CancellationToken) -> HealthPollers {
        health::spawn_health_pollers(self, cancel)
    }

    /// Opens a streaming query for `uid` using P2C, circuit-breaker admission, and
    /// jittered-backoff retries for idempotent reads (`RTE-073`..`RTE-093`, `INV-02`, `INV-12`).
    pub async fn open_query(
        &self,
        uid: &BranchUid,
        request: QueryRequest,
        scope: CallScope,
    ) -> Result<BoxStream<QueryResponse>, BranchQueryError> {
        let Some(branch) = self.entries.get(uid) else {
            return Err(BranchQueryError::Registry(GapError::new(
                ErrorCode::BranchUnavailable,
                format!("no client registered for branch `{uid}`"),
            )));
        };

        // Deposit first-attempt token (`RTE-091`).
        branch.replicas[0].replica.slot.on_first_attempt();

        let call_start = Instant::now();

        let mut rng = match self.rng_seed {
            Some(seed) => XorShift64::new(seed),
            None => XorShift64::with_thread_local(|r| *r),
        };

        let mut excluded: Vec<usize> = Vec::new();
        let mut last_error: Option<NgcErrorInfo> = None;

        for attempt in 0..=self.resilience.max_retries {
            let now = self.now();

            // Build P2C candidate list from ALL replicas; indices align with branch.replicas.
            let candidates: Vec<Candidate<'_>> = branch
                .replicas
                .iter()
                .map(|entry| Candidate {
                    uid: entry.replica.tiebreak,
                    slot: &entry.replica.slot,
                })
                .collect();

            // All excluded?
            if excluded.len() >= candidates.len() {
                let min_after_ms = branch
                    .replicas
                    .iter()
                    .filter_map(|entry| {
                        if let Admission::Rejected { retry_after } =
                            entry.replica.slot.breaker().admit(now)
                        {
                            Some(retry_after.as_millis() as u64)
                        } else {
                            None
                        }
                    })
                    .min()
                    .unwrap_or(5_000);
                return Err(BranchQueryError::Registry(
                    GapError::new(
                        ErrorCode::BranchUnavailable,
                        format!("branch `{uid}` all replicas exhausted (attempt={attempt})"),
                    )
                    .with_retry(true, Some(u32::try_from(min_after_ms).unwrap_or(u32::MAX))),
                ));
            }

            // P2C selection — `excluded` indices are into `candidates` (= branch.replicas) (`RTE-074`, `RTE-079`).
            let Some(branch_idx) = select_excluding(&candidates, &excluded, now, &mut rng) else {
                // All non-excluded candidates have Open breakers.
                let min_after_ms = branch
                    .replicas
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| !excluded.contains(idx))
                    .filter_map(|(_, entry)| {
                        if let Admission::Rejected { retry_after } =
                            entry.replica.slot.breaker().admit(now)
                        {
                            Some(retry_after.as_millis() as u64)
                        } else {
                            None
                        }
                    })
                    .min()
                    .unwrap_or(5_000);
                for entry in branch.replicas.iter() {
                    entry
                        .counters
                        .breaker_rejections
                        .fetch_add(1, Ordering::Relaxed);
                }
                return Err(BranchQueryError::Registry(
                    GapError::new(
                        ErrorCode::BranchUnavailable,
                        format!("branch `{uid}` all replicas open (retry_after={min_after_ms}ms)"),
                    )
                    .with_retry(true, Some(u32::try_from(min_after_ms).unwrap_or(u32::MAX))),
                ));
            };

            let entry = &branch.replicas[branch_idx];

            // Breaker admission (`RTE-081`, `RTE-085`).
            let probe_permit = match entry.replica.slot.breaker().admit(now) {
                Admission::Allowed => None,
                Admission::Probe(permit) => Some(permit),
                Admission::Rejected { retry_after } => {
                    entry
                        .counters
                        .breaker_rejections
                        .fetch_add(1, Ordering::Relaxed);
                    excluded.push(branch_idx);
                    if excluded.len() >= branch.replicas.len() {
                        return Err(BranchQueryError::Registry(
                            GapError::new(
                                ErrorCode::BranchUnavailable,
                                format!(
                                    "branch `{uid}` breaker Open (retry_after={}ms)",
                                    retry_after.as_millis()
                                ),
                            )
                            .with_retry(
                                true,
                                Some(u32::try_from(retry_after.as_millis()).unwrap_or(u32::MAX)),
                            ),
                        ));
                    }
                    continue;
                }
            };

            // Non-queueing bulkhead (`INV-02`, `NGC-071`).
            let _bulkhead_permit = match Arc::clone(&entry.replica.bulkhead).try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    entry
                        .counters
                        .bulkhead_rejections
                        .fetch_add(1, Ordering::Relaxed);
                    excluded.push(branch_idx);
                    if excluded.len() >= branch.replicas.len() {
                        return Err(BranchQueryError::Registry(
                            GapError::new(
                                ErrorCode::Overloaded,
                                format!("branch `{uid}` all replicas saturated"),
                            )
                            .with_retry(true, None),
                        ));
                    }
                    continue;
                }
            };

            // In-flight guard (`RTE-071`).
            let in_flight_guard = InFlightGuard::acquire(&entry.replica.slot);

            // Merge branch metadata without overriding caller-supplied keys (`INV-15`).
            let mut merged_scope = scope.clone();
            for (k, v) in &entry.replica.metadata {
                merged_scope
                    .metadata
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }

            let stream_result = entry
                .replica
                .client
                .query(request.clone(), merged_scope)
                .await;

            // Bulkhead permit released here — no longer needed once stream is open.
            drop(_bulkhead_permit);

            match stream_result {
                Ok(stream) => {
                    let accounted = AccountedStream {
                        inner: Some(stream),
                        slot: Arc::clone(&entry.replica.slot),
                        counters: Arc::clone(&entry.counters),
                        probe_permit,
                        _in_flight: Some(in_flight_guard),
                        start: call_start,
                        epoch: std::time::Instant::now()
                            .checked_sub(self.epoch.elapsed())
                            .unwrap_or_else(std::time::Instant::now),
                        clock: Arc::clone(&self.clock),
                        recorded: false,
                    };
                    return Ok(Box::pin(accounted) as BoxStream<_>);
                }
                Err(err) => {
                    let outcome = classify_error_code(err.code);
                    let latency = call_start.elapsed();
                    let now_rec = self.now();
                    if let Some(permit) = probe_permit {
                        permit.record(outcome, latency, now_rec);
                    } else {
                        entry.replica.slot.record(outcome, latency, now_rec);
                    }
                    drop(in_flight_guard);
                    match outcome {
                        Outcome::Ok => {}
                        Outcome::Failure => {
                            entry.counters.calls_failed.fetch_add(1, Ordering::Relaxed);
                        }
                        Outcome::ClientError => {
                            entry
                                .counters
                                .calls_client_error
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    // Retry only if retryable, budget available, and not last attempt.
                    let retryable = is_retryable(&err)
                        && attempt < self.resilience.max_retries
                        && entry.replica.slot.retry_budget().try_withdraw();

                    if !retryable {
                        last_error = Some(err);
                        break;
                    }

                    let backoff = jittered_backoff(attempt, &self.resilience, &mut rng);
                    let deadline_minus_margin = scope
                        .deadline
                        .checked_sub(self.resilience.deadline_margin)
                        .unwrap_or(scope.deadline);
                    if TokioInstant::now() + backoff >= deadline_minus_margin {
                        last_error = Some(err);
                        break;
                    }

                    entry.counters.retries.fetch_add(1, Ordering::Relaxed);
                    excluded.push(branch_idx);

                    tokio::select! {
                        biased;
                        () = scope.cancel.cancelled() => {
                            return Err(BranchQueryError::Connector(NgcErrorInfo::new(
                                NgcErrorCode::Cancelled,
                                "call cancelled during retry backoff",
                            )));
                        }
                        () = sleep_until(deadline_minus_margin) => {
                            last_error = Some(err);
                            break;
                        }
                        () = sleep(backoff) => {}
                    }

                    last_error = Some(err);
                }
            }
        }

        Err(BranchQueryError::Connector(last_error.unwrap_or_else(
            || NgcErrorInfo::new(NgcErrorCode::Internal, "open_query exhausted retries"),
        )))
    }
}

// ─── Tests (v0 compat) ───────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt as _;
    use gap_ngc_sdk::mock::MockConnector;

    use super::*;

    // spec: INV-02, NGC-071
    #[tokio::test(start_paused = true)]
    async fn bulkhead_released_after_stream_opened() {
        let uid = BranchUid::parse("018bcfe5-6c01-7010-8101-010101010101").expect("uid");
        let mock = MockConnector::new()
            .on_query(|_req, _scope| Ok(Box::pin(futures_util::stream::pending())));
        let client: Arc<dyn BranchClient> =
            Arc::new(InProcess::from_connector(mock, ServerConfig::default()));
        let reg = ConnectorRegistry::builder()
            .register(uid, client, 1)
            .build();

        let _s1 = reg
            .open_query(
                &uid,
                QueryRequest::default(),
                CallScope::for_test(TokioInstant::now() + Duration::from_secs(5)),
            )
            .await
            .expect("s1 opens");
        // Bulkhead permit is released when open_query returns, not held by stream.
        assert_eq!(reg.available_permits(&uid), Some(1));
    }
}
