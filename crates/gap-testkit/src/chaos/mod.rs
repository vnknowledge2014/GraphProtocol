//! Fault-injection and chaos harness utilities (`WP-M3-08`, `RTE-074`, `RTE-080`, `RTE-081`, `RUN-024`, `RUN-030`, `OPS-030`).

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gap_core::clock::Clock;
use gap_core::error::{ErrorCode, GapError, GapResult, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::BranchUid;
use serde_json::{Value, json};

use crate::clock::ManualClock;
use crate::mock_ngc::{MockForeachChunk, MockNgcConnector};

/// Deterministic SplitMix64 + XorShift64 PRNG avoiding modulo bias (`RTE-079`).
#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Initializes PRNG from `seed` (forcing non-zero state).
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        let mixed = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: mixed }
    }

    /// Generates next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Generates an unbiased index in `0..upper` (`upper > 0`).
    pub fn next_bounded(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            return 0;
        }
        let u = upper as u64;
        let threshold = u64::MAX - (u64::MAX % u);
        loop {
            let r = self.next_u64();
            if r < threshold {
                return (r % u) as usize;
            }
        }
    }
}

/// Fault profile dynamically attached to a [`ChaosReplica`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultProfile {
    /// Artificial latency step injected before serving each call (ms).
    pub added_latency_ms: u64,
    /// Probability `[0..=10_000]` basis points of returning a synthetic error.
    pub error_rate_bps: u16,
    /// Error code emitted when `error_rate_bps` triggers.
    pub error_code: NgcErrorCode,
    /// Whether injected synthetic errors are marked `retryable` (`RTE-080`).
    pub retryable_errors: bool,
    /// If `Some(k)`, crashes a streaming `foreach` response mid-stream after `k` chunks (`RUN-024`).
    pub mid_stream_crash_after_chunks: Option<u32>,
    /// If `true`, drops/blackholes packets so calls stall until deadline (`OPS-030`).
    pub partitioned: bool,
}

impl Default for FaultProfile {
    fn default() -> Self {
        Self {
            added_latency_ms: 0,
            error_rate_bps: 0,
            error_code: NgcErrorCode::Unavailable,
            retryable_errors: true,
            mid_stream_crash_after_chunks: None,
            partitioned: false,
        }
    }
}

#[derive(Debug)]
struct InjectorInner {
    profile: FaultProfile,
    rng: XorShift64,
}

/// Thread-safe fault controller attached to a replica (`WP-M3-08`).
#[derive(Debug, Clone)]
pub struct FaultInjector {
    inner: Arc<Mutex<InjectorInner>>,
}

impl FaultInjector {
    /// Creates a healthy fault injector seeded deterministically with `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InjectorInner {
                profile: FaultProfile::default(),
                rng: XorShift64::new(seed),
            })),
        }
    }

    /// Replaces the entire [`FaultProfile`].
    pub fn set_profile(&self, profile: FaultProfile) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.profile = profile;
        }
    }

    /// Returns a snapshot of the active [`FaultProfile`].
    #[must_use]
    pub fn profile(&self) -> FaultProfile {
        match self.inner.lock() {
            Ok(guard) => guard.profile.clone(),
            Err(poisoned) => poisoned.into_inner().profile.clone(),
        }
    }

    /// Injects an artificial latency shift (`+added_latency_ms`).
    pub fn inject_latency_shift_ms(&self, added_latency_ms: u64) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.profile.added_latency_ms = added_latency_ms;
        }
    }

    /// Sets synthetic error injection rate in basis points (`0..=10_000`) and retryability.
    pub fn inject_error_rate_bps(&self, bps: u16, code: NgcErrorCode, retryable: bool) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.profile.error_rate_bps = bps.min(10_000);
            guard.profile.error_code = code;
            guard.profile.retryable_errors = retryable;
        }
    }

    /// Configures mid-stream `foreach` crash after `after_chunks` chunks (`RUN-024`).
    pub fn inject_mid_stream_crash(&self, after_chunks: Option<u32>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.profile.mid_stream_crash_after_chunks = after_chunks;
        }
    }

    /// Enables or clears network partition (blackhole until per-hop deadline, `OPS-030`).
    pub fn set_partitioned(&self, partitioned: bool) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.profile.partitioned = partitioned;
        }
    }

    fn should_inject_error(&self) -> Option<(NgcErrorCode, bool)> {
        let mut guard = self.inner.lock().ok()?;
        let bps = guard.profile.error_rate_bps;
        if bps == 0 {
            return None;
        }
        let code = guard.profile.error_code;
        let retryable = guard.profile.retryable_errors;
        if bps >= 10_000 {
            return Some((code, retryable));
        }
        let sample = guard.rng.next_bounded(10_000) as u16;
        if sample < bps {
            Some((code, retryable))
        } else {
            None
        }
    }
}

/// Atomic counter tracking active requests with RAII drop-decrement semantics (`RTE-071`).
#[derive(Debug, Default)]
pub struct InFlightCounter {
    count: AtomicU32,
}

impl InFlightCounter {
    /// Acquires an [`InFlightPermit`], incrementing `in_flight` until dropped.
    #[must_use]
    pub fn acquire(self: &Arc<Self>) -> InFlightPermit {
        self.count.fetch_add(1, Ordering::AcqRel);
        InFlightPermit {
            counter: Arc::clone(self),
        }
    }

    /// Current live in-flight request count.
    #[must_use]
    pub fn current(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }
}

/// RAII guard decrementing [`InFlightCounter`] on every exit path including cancellation (`RTE-071`).
#[derive(Debug)]
pub struct InFlightPermit {
    counter: Arc<InFlightCounter>,
}

impl Drop for InFlightPermit {
    fn drop(&mut self) {
        self.counter.count.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone)]
struct DecayedEwma {
    value: Option<f64>,
    last_ms: u64,
    half_life_ms: f64,
}

impl DecayedEwma {
    const fn new(half_life_ms: f64) -> Self {
        Self {
            value: None,
            last_ms: 0,
            half_life_ms,
        }
    }

    fn read(&self, now_ms: u64) -> Option<f64> {
        let val = self.value?;
        if now_ms < self.last_ms {
            return Some(val);
        }
        let age_ms = (now_ms - self.last_ms) as f64;
        if age_ms > 5.0 * self.half_life_ms {
            None
        } else {
            Some(val)
        }
    }

    fn update(&mut self, sample: f64, now_ms: u64) {
        if !sample.is_finite() || sample < 0.0 {
            return;
        }
        match self.value {
            None => {
                self.value = Some(sample);
                self.last_ms = now_ms;
            }
            Some(prev) => {
                let dt_ms = now_ms.saturating_sub(self.last_ms) as f64;
                if dt_ms > 5.0 * self.half_life_ms {
                    self.value = Some(sample);
                    self.last_ms = now_ms;
                } else if dt_ms > 0.0 {
                    let alpha = 1.0 - (-dt_ms * std::f64::consts::LN_2 / self.half_life_ms).exp();
                    self.value = Some(prev + alpha * (sample - prev));
                    self.last_ms = now_ms;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RollingBucket {
    epoch: u64,
    ok: u32,
    fail: u32,
    slow: u32,
}

/// Circuit breaker FSM state (`RTE-080`, `RTE-081`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Healthy closed state admitting requests normally.
    Closed,
    /// Tripped open rejecting calls until `opened_at_ms + duration_ms`.
    Open {
        /// Millisecond timestamp when the breaker opened.
        opened_at_ms: u64,
        /// Configured open backoff duration (`min(5s * 2^k, 60s)`).
        duration_ms: u64,
        /// Exponential backoff exponent `k`.
        k: u32,
    },
    /// Probing recovery state admitting at most 3 probe requests.
    HalfOpen {
        /// Number of probes currently executing.
        probes_in_flight: u32,
        /// Probe successes observed in this episode.
        successes: u32,
        /// Probe failures observed in this episode.
        failures: u32,
        /// Backoff exponent carried from `Open`.
        k: u32,
    },
}

/// Outcome category for breaker and telemetry accounting (`RTE-080`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// Successful upstream response.
    Ok,
    /// Non-retryable client error (`INVALID_ARGUMENT`, `PERMISSION_DENIED`, `NOT_FOUND`): NOT a failure.
    ClientError,
    /// Retryable upstream failure or timeout.
    Failure,
}

#[derive(Debug)]
struct BreakerInner {
    state: BreakerState,
    buckets: [RollingBucket; 10],
    last_closed_ms: u64,
    transitions: u32,
}

impl BreakerInner {
    fn new(now_ms: u64) -> Self {
        Self {
            state: BreakerState::Closed,
            buckets: [RollingBucket::default(); 10],
            last_closed_ms: now_ms,
            transitions: 0,
        }
    }

    fn advance_state(&mut self, now_ms: u64) {
        if let BreakerState::Open {
            opened_at_ms,
            duration_ms,
            k,
        } = self.state
            && now_ms >= opened_at_ms.saturating_add(duration_ms)
        {
            self.state = BreakerState::HalfOpen {
                probes_in_flight: 0,
                successes: 0,
                failures: 0,
                k,
            };
            self.transitions = self.transitions.saturating_add(1);
        }
    }

    fn record_bucket(
        &mut self,
        now_ms: u64,
        outcome: CallOutcome,
        latency_ms: u64,
        default_timeout_ms: u64,
    ) {
        let epoch = now_ms / 3_000;
        let idx = (epoch as usize) % 10;
        if self.buckets[idx].epoch != epoch {
            self.buckets[idx] = RollingBucket {
                epoch,
                ok: 0,
                fail: 0,
                slow: 0,
            };
        }
        match outcome {
            CallOutcome::Ok | CallOutcome::ClientError => {
                self.buckets[idx].ok = self.buckets[idx].ok.saturating_add(1);
            }
            CallOutcome::Failure => {
                self.buckets[idx].fail = self.buckets[idx].fail.saturating_add(1);
            }
        }
        if latency_ms.saturating_mul(10) > default_timeout_ms.saturating_mul(8) {
            self.buckets[idx].slow = self.buckets[idx].slow.saturating_add(1);
        }
    }

    fn window_totals(&self, now_ms: u64) -> (u32, u32, u32) {
        let cur_epoch = now_ms / 3_000;
        let min_epoch = cur_epoch.saturating_sub(9);
        let mut ok = 0_u32;
        let mut fail = 0_u32;
        let mut slow = 0_u32;
        for b in &self.buckets {
            if b.epoch >= min_epoch
                && b.epoch <= cur_epoch
                && (b.ok > 0 || b.fail > 0 || b.slow > 0)
            {
                ok = ok.saturating_add(b.ok);
                fail = fail.saturating_add(b.fail);
                slow = slow.saturating_add(b.slow);
            }
        }
        (ok, fail, slow)
    }

    fn clear_window(&mut self) {
        self.buckets = [RollingBucket::default(); 10];
    }

    fn open_duration_ms(k: u32) -> u64 {
        let shift = k.min(6);
        (5_000_u64.saturating_mul(1_u64 << shift)).min(60_000)
    }
}

/// RAII probe permit ensuring cancelled `HalfOpen` probes release their probe slot (`RTE-085`).
#[derive(Debug)]
pub struct ProbePermit {
    breaker: Arc<Mutex<BreakerInner>>,
    recorded: bool,
}

impl ProbePermit {
    fn settle(mut self, now_ms: u64, outcome: CallOutcome) {
        self.recorded = true;
        if let Ok(mut guard) = self.breaker.lock()
            && let BreakerState::HalfOpen {
                probes_in_flight,
                successes,
                failures,
                k,
            } = guard.state
        {
            let next_in_flight = probes_in_flight.saturating_sub(1);
            let (next_succ, next_fail) = match outcome {
                CallOutcome::Ok | CallOutcome::ClientError => {
                    (successes.saturating_add(1), failures)
                }
                CallOutcome::Failure => (successes, failures.saturating_add(1)),
            };
            if next_succ >= 2 {
                guard.state = BreakerState::Closed;
                guard.last_closed_ms = now_ms;
                guard.clear_window();
                guard.transitions = guard.transitions.saturating_add(1);
            } else if next_fail >= 2 {
                let next_k = (k.saturating_add(1)).min(6);
                let duration_ms = BreakerInner::open_duration_ms(next_k);
                guard.state = BreakerState::Open {
                    opened_at_ms: now_ms,
                    duration_ms,
                    k: next_k,
                };
                guard.transitions = guard.transitions.saturating_add(1);
            } else {
                guard.state = BreakerState::HalfOpen {
                    probes_in_flight: next_in_flight,
                    successes: next_succ,
                    failures: next_fail,
                    k,
                };
            }
        }
    }
}

impl Drop for ProbePermit {
    fn drop(&mut self) {
        if !self.recorded
            && let Ok(mut guard) = self.breaker.lock()
            && let BreakerState::HalfOpen {
                probes_in_flight,
                successes,
                failures,
                k,
            } = guard.state
        {
            guard.state = BreakerState::HalfOpen {
                probes_in_flight: probes_in_flight.saturating_sub(1),
                successes,
                failures,
                k,
            };
        }
    }
}

/// Fixed-point milli-token retry budget (`RTE-091`, `RTE-093`).
#[derive(Debug)]
pub struct RetryBudgetTracker {
    balance_milli: AtomicI64,
    first_attempts: AtomicU64,
    retries_used: AtomicU64,
    retries_rejected: AtomicU64,
}

impl RetryBudgetTracker {
    /// Initial milli-tokens (`10.0` retries = `10_000` milli-tokens).
    pub const INITIAL_MILLI: i64 = 10_000;
    /// Maximum milli-token balance (`100.0` retries = `100_000` milli-tokens).
    pub const CAP_MILLI: i64 = 100_000;
    /// Deposit per first attempt (`0.1` retry = `100` milli-tokens).
    pub const DEPOSIT_MILLI: i64 = 100;
    /// Cost per retry attempt (`1.0` retry = `1_000` milli-tokens).
    pub const WITHDRAW_MILLI: i64 = 1_000;

    /// Creates a new retry budget starting at `10_000` milli-tokens.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            balance_milli: AtomicI64::new(Self::INITIAL_MILLI),
            first_attempts: AtomicU64::new(0),
            retries_used: AtomicU64::new(0),
            retries_rejected: AtomicU64::new(0),
        }
    }

    /// Deposits `100` milli-tokens on a first attempt, saturating at `CAP_MILLI`.
    pub fn record_first_attempt(&self) {
        self.first_attempts.fetch_add(1, Ordering::Relaxed);
        let mut cur = self.balance_milli.load(Ordering::Relaxed);
        loop {
            let next = (cur + Self::DEPOSIT_MILLI).min(Self::CAP_MILLI);
            match self.balance_milli.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Atomically withdraws `1_000` milli-tokens if `balance >= 1_000`.
    #[must_use]
    pub fn try_withdraw_retry(&self) -> bool {
        let mut cur = self.balance_milli.load(Ordering::Relaxed);
        loop {
            if cur < Self::WITHDRAW_MILLI {
                self.retries_rejected.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let next = cur - Self::WITHDRAW_MILLI;
            match self.balance_milli.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.retries_used.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Err(actual) => cur = actual,
            }
        }
    }

    /// Total first attempts observed.
    #[must_use]
    pub fn first_attempts(&self) -> u64 {
        self.first_attempts.load(Ordering::Relaxed)
    }

    /// Total retries permitted by this budget.
    #[must_use]
    pub fn retries_used(&self) -> u64 {
        self.retries_used.load(Ordering::Relaxed)
    }

    /// Total retry attempts denied because the budget was empty.
    #[must_use]
    pub fn retries_rejected(&self) -> u64 {
        self.retries_rejected.load(Ordering::Relaxed)
    }

    /// Current milli-token balance.
    #[must_use]
    pub fn balance_milli(&self) -> i64 {
        self.balance_milli.load(Ordering::Relaxed)
    }
}

impl Default for RetryBudgetTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Tunables for [`ChaosReplica`] and [`ChaosFederationHarness`].
#[derive(Debug, Clone, Copy)]
pub struct ChaosReplicaConfig {
    /// Replica concurrency capacity for P2C cost calculation (`max(1, capacity)`).
    pub capacity: u32,
    /// Per-branch default timeout (`RUN-030`, `RTE-080`).
    pub default_timeout: Duration,
    /// Time-decay half-life for EWMA (`RTE-070`).
    pub ewma_half_life: Duration,
}

impl Default for ChaosReplicaConfig {
    fn default() -> Self {
        Self {
            capacity: 64,
            default_timeout: Duration::from_millis(500),
            ewma_half_life: Duration::from_millis(5_000),
        }
    }
}

/// Fault-injectable downstream branch replica with RAII in-flight tracking, EWMAs, and circuit breaker.
#[derive(Debug)]
pub struct ChaosReplica {
    branch_uid: BranchUid,
    connector: MockNgcConnector,
    injector: FaultInjector,
    in_flight: Arc<InFlightCounter>,
    ewma: Mutex<(DecayedEwma, DecayedEwma)>,
    breaker: Arc<Mutex<BreakerInner>>,
    routed_calls: AtomicU64,
    config: ChaosReplicaConfig,
}

impl ChaosReplica {
    /// Creates a healthy replica initialized at `now_ms`.
    #[must_use]
    pub fn new(
        branch_uid: BranchUid,
        connector: MockNgcConnector,
        injector: FaultInjector,
        now_ms: u64,
        config: ChaosReplicaConfig,
    ) -> Arc<Self> {
        let hl_ms = config.ewma_half_life.as_secs_f64() * 1_000.0;
        Arc::new(Self {
            branch_uid,
            connector,
            injector,
            in_flight: Arc::new(InFlightCounter::default()),
            ewma: Mutex::new((DecayedEwma::new(hl_ms), DecayedEwma::new(hl_ms))),
            breaker: Arc::new(Mutex::new(BreakerInner::new(now_ms))),
            routed_calls: AtomicU64::new(0),
            config,
        })
    }

    /// Stable branch UID.
    #[must_use]
    pub const fn branch_uid(&self) -> BranchUid {
        self.branch_uid
    }

    /// Underlying mock NGC connector.
    #[must_use]
    pub const fn connector(&self) -> &MockNgcConnector {
        &self.connector
    }

    /// Attached fault controller.
    #[must_use]
    pub const fn injector(&self) -> &FaultInjector {
        &self.injector
    }

    /// Current live in-flight count (`RTE-071`).
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.in_flight.current()
    }

    /// Total calls routed to this replica.
    #[must_use]
    pub fn routed_calls(&self) -> u64 {
        self.routed_calls.load(Ordering::Relaxed)
    }

    /// Reads current decayed latency EWMA in milliseconds if fresh (`<= 5` half-lives).
    #[must_use]
    pub fn latency_ewma_ms(&self, now_ms: u64) -> Option<f64> {
        self.ewma.lock().ok().and_then(|g| g.0.read(now_ms))
    }

    /// Reads current decayed error rate EWMA in `[0.0, 1.0]`.
    #[must_use]
    pub fn error_ewma(&self, now_ms: u64) -> f64 {
        self.ewma
            .lock()
            .ok()
            .and_then(|g| g.1.read(now_ms))
            .unwrap_or(0.0)
    }

    /// Current breaker state after advancing timers to `now_ms`.
    #[must_use]
    pub fn breaker_state(&self, now_ms: u64) -> BreakerState {
        match self.breaker.lock() {
            Ok(mut guard) => {
                guard.advance_state(now_ms);
                guard.state
            }
            Err(_) => BreakerState::Closed,
        }
    }

    /// Total breaker state transitions recorded.
    #[must_use]
    pub fn breaker_transitions(&self) -> u32 {
        self.breaker.lock().map_or(0, |g| g.transitions)
    }

    fn try_admit(&self, now_ms: u64) -> Result<Option<ProbePermit>, u32> {
        let mut guard = match self.breaker.lock() {
            Ok(g) => g,
            Err(_) => return Err(1_000),
        };
        guard.advance_state(now_ms);
        match guard.state {
            BreakerState::Closed => Ok(None),
            BreakerState::Open {
                opened_at_ms,
                duration_ms,
                ..
            } => {
                let rem = opened_at_ms
                    .saturating_add(duration_ms)
                    .saturating_sub(now_ms)
                    .max(1);
                Err(u32::try_from(rem).unwrap_or(u32::MAX))
            }
            BreakerState::HalfOpen {
                probes_in_flight,
                successes,
                failures,
                k,
            } => {
                if probes_in_flight + successes + failures < 3 && probes_in_flight < 3 {
                    guard.state = BreakerState::HalfOpen {
                        probes_in_flight: probes_in_flight + 1,
                        successes,
                        failures,
                        k,
                    };
                    Ok(Some(ProbePermit {
                        breaker: Arc::clone(&self.breaker),
                        recorded: false,
                    }))
                } else {
                    Err(1_000)
                }
            }
        }
    }

    fn record_completion(
        &self,
        now_ms: u64,
        outcome: CallOutcome,
        latency_ms: u64,
        probe: Option<ProbePermit>,
    ) {
        if let Ok(mut guard) = self.ewma.lock() {
            match outcome {
                CallOutcome::Ok => {
                    guard.0.update(latency_ms as f64, now_ms);
                    guard.1.update(0.0, now_ms);
                }
                CallOutcome::Failure => {
                    guard.0.update(latency_ms as f64, now_ms);
                    guard.1.update(1.0, now_ms);
                }
                CallOutcome::ClientError => {
                    guard.1.update(0.0, now_ms);
                }
            }
        }

        if let Some(permit) = probe {
            permit.settle(now_ms, outcome);
            return;
        }

        if let Ok(mut guard) = self.breaker.lock()
            && guard.state == BreakerState::Closed
        {
            let timeout_ms = self.config.default_timeout.as_millis() as u64;
            guard.record_bucket(now_ms, outcome, latency_ms, timeout_ms);
            let (ok, fail, slow) = guard.window_totals(now_ms);
            let calls = ok.saturating_add(fail);
            if calls >= 20
                && (fail.saturating_mul(2) >= calls
                    || slow.saturating_mul(5) >= calls.saturating_mul(4))
            {
                let within_5min = now_ms.saturating_sub(guard.last_closed_ms) <= 300_000;
                let k = if within_5min && guard.transitions > 0 {
                    1
                } else {
                    0
                };
                let duration_ms = BreakerInner::open_duration_ms(k);
                guard.state = BreakerState::Open {
                    opened_at_ms: now_ms,
                    duration_ms,
                    k,
                };
                guard.transitions = guard.transitions.saturating_add(1);
            }
        }
    }
}

/// Response returned by [`ChaosFederationHarness::execute_unary`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChaosUnaryResponse {
    /// Selected replica `BranchUid`.
    pub branch_uid: BranchUid,
    /// Upstream payload.
    pub data: Value,
    /// Retry attempts spent before success (`0` on first-attempt success).
    pub retries_used: u32,
}

/// Partial-failure-aware report returned by [`ChaosFederationHarness::execute_foreach`] (`RUN-024`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChaosForeachReport {
    /// Selected replica serving the batch.
    pub branch_uid: BranchUid,
    /// Per-index outputs aligned with input `foreach_sets` (`None` -> nulled on partial error).
    pub items: Vec<Option<Value>>,
    /// Delivered pre-crash stream chunks.
    pub delivered_chunks: Vec<MockForeachChunk>,
    /// `true` iff any index failed or mid-stream crash truncated remaining items (`RUN-024`).
    pub partial: bool,
    /// Zero-based item indexes affected by partial failures or mid-stream crash.
    pub affected_indexes: Vec<u32>,
    /// Structured GAP errors covering affected items (`GAP_BRANCH_ERROR` / `GAP_PARTIAL_RESULT`).
    pub errors: Vec<GapError>,
}

/// Multi-replica P2C federation router + fault-injection chaos harness (`WP-M3-08`).
#[derive(Debug)]
pub struct ChaosFederationHarness {
    clock: Arc<ManualClock>,
    replicas: Vec<Arc<ChaosReplica>>,
    retry_budget: RetryBudgetTracker,
    rng: Mutex<XorShift64>,
    max_retries: u32,
}

impl ChaosFederationHarness {
    /// Constructs a chaos harness backed by `clock` and `replicas`.
    #[must_use]
    pub fn new(
        clock: Arc<ManualClock>,
        replicas: Vec<Arc<ChaosReplica>>,
        max_retries: u32,
        rng_seed: u64,
    ) -> Self {
        Self {
            clock,
            replicas,
            retry_budget: RetryBudgetTracker::new(),
            rng: Mutex::new(XorShift64::new(rng_seed)),
            max_retries,
        }
    }

    /// Accesses replicas managed by this harness.
    #[must_use]
    pub fn replicas(&self) -> &[Arc<ChaosReplica>] {
        &self.replicas
    }

    /// Accesses shared retry budget tracker (`RTE-091`).
    #[must_use]
    pub const fn retry_budget(&self) -> &RetryBudgetTracker {
        &self.retry_budget
    }

    /// Selects a replica index using Power-of-Two-Choices (`RTE-074`, `RTE-079`).
    fn select_replica(
        &self,
        now_ms: u64,
        excluded: &[usize],
    ) -> GapResult<(usize, Option<ProbePermit>)> {
        let mut eligible: Vec<usize> = Vec::new();
        let mut soonest_retry_ms: Option<u32> = None;

        for (idx, r) in self.replicas.iter().enumerate() {
            if excluded.contains(&idx) {
                continue;
            }
            match r.try_admit(now_ms) {
                Ok(permit) => {
                    // Do not hold probe tokens unless this replica actually wins P2C (`RTE-079(e)`).
                    drop(permit);
                    eligible.push(idx);
                }
                Err(after_ms) => {
                    soonest_retry_ms = Some(soonest_retry_ms.map_or(after_ms, |m| m.min(after_ms)));
                }
            }
        }

        if eligible.is_empty() {
            let after = soonest_retry_ms.unwrap_or(1_000);
            return Err(GapError::new(
                ErrorCode::BranchUnavailable,
                "all group members unavailable (circuit open)",
            )
            .with_retry(true, Some(after)));
        }

        let median_lat = self.group_median_latency_ms(now_ms, &eligible);
        let chosen_idx = if eligible.len() == 1 {
            eligible[0]
        } else {
            let mut rng = self
                .rng
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "harness rng mutex poisoned"))?;
            let i_pos = rng.next_bounded(eligible.len());
            let mut j_pos = rng.next_bounded(eligible.len() - 1);
            if j_pos >= i_pos {
                j_pos += 1;
            }
            drop(rng);

            let a = eligible[i_pos];
            let b = eligible[j_pos];
            if self.p2c_less(a, b, now_ms, median_lat) {
                a
            } else {
                b
            }
        };

        // Now claim admit/probe permit for the selected winner (retrying if HalfOpen slot lost race).
        match self.replicas[chosen_idx].try_admit(now_ms) {
            Ok(permit) => Ok((chosen_idx, permit)),
            Err(_) => {
                let mut next_excluded = excluded.to_vec();
                next_excluded.push(chosen_idx);
                self.select_replica(now_ms, &next_excluded)
            }
        }
    }

    fn group_median_latency_ms(&self, now_ms: u64, eligible: &[usize]) -> f64 {
        let mut known: Vec<f64> = eligible
            .iter()
            .filter_map(|&idx| self.replicas[idx].latency_ewma_ms(now_ms))
            .collect();
        if known.is_empty() {
            return 1.0;
        }
        known.sort_by(f64::total_cmp);
        known[(known.len() - 1) / 2]
    }

    fn p2c_cost(&self, idx: usize, now_ms: u64, median_lat: f64) -> (f64, u32, BranchUid) {
        let r = &self.replicas[idx];
        let lat = r.latency_ewma_ms(now_ms).unwrap_or(median_lat);
        let in_flight = r.in_flight();
        let cap = f64::from(r.config.capacity.max(1));
        let load = 1.0 + f64::from(in_flight) / cap;
        let err = r.error_ewma(now_ms);
        let denom = (1.0 - err).max(0.05);
        let cost = lat * load * load / denom;
        (cost, in_flight, r.branch_uid)
    }

    fn p2c_less(&self, a: usize, b: usize, now_ms: u64, median_lat: f64) -> bool {
        let ca = self.p2c_cost(a, now_ms, median_lat);
        let cb = self.p2c_cost(b, now_ms, median_lat);
        match ca.0.total_cmp(&cb.0) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => match ca.1.cmp(&cb.1) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => ca.2 < cb.2,
            },
        }
    }

    /// Computes per-hop call deadline per `RUN-030`:
    /// `min(request_deadline - margin, branch.default_timeout)` with `margin = max(50 ms, 5% remaining)`.
    #[must_use]
    pub fn per_hop_deadline(remaining: Duration, branch_default: Duration) -> Option<Duration> {
        let five_pct = Duration::from_millis((remaining.as_millis() as u64) / 20);
        let margin = Duration::from_millis(50).max(five_pct);
        let budget = remaining.checked_sub(margin)?;
        if budget.is_zero() {
            return None;
        }
        Some(budget.min(branch_default))
    }

    /// Executes a unary RPC across replicas with P2C, breaker enforcement, per-hop deadlines, and retries.
    pub async fn execute_unary(
        &self,
        operation: &str,
        arguments: Value,
        request_deadline: Duration,
    ) -> GapResult<ChaosUnaryResponse> {
        let deadline_at: Instant = self.clock.now_instant() + request_deadline;
        self.retry_budget.record_first_attempt();
        let mut excluded: Vec<usize> = Vec::new();
        let mut retries_used = 0_u32;

        loop {
            let now_inst = self.clock.now_instant();
            let now_ms = self.clock.unix_millis() as u64;
            let remaining = deadline_at.saturating_duration_since(now_inst);
            let (idx, probe) = self.select_replica(now_ms, &excluded)?;
            let replica = Arc::clone(&self.replicas[idx]);

            let Some(hop_timeout) =
                Self::per_hop_deadline(remaining, replica.config.default_timeout)
            else {
                drop(probe);
                return Err(GapError::new(
                    ErrorCode::DeadlineExceeded,
                    "request deadline expired before branch hop",
                ));
            };

            let _permit = replica.in_flight.acquire();
            replica.routed_calls.fetch_add(1, Ordering::Relaxed);
            let start = self.clock.now_instant();
            let profile = replica.injector.profile();

            let outcome = if profile.partitioned {
                tokio::time::sleep(hop_timeout).await;
                Err(
                    NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "network partition timeout")
                        .with_retry(true, 0),
                )
            } else {
                let added = Duration::from_millis(profile.added_latency_ms);
                let fut = async {
                    if !added.is_zero() {
                        tokio::time::sleep(added).await;
                    }
                    if let Some((code, retryable)) = replica.injector.should_inject_error() {
                        return Err(NgcErrorInfo::new(code, "injected chaos fault")
                            .with_retry(retryable, 0));
                    }
                    replica.connector.call(operation, arguments.clone()).await
                };
                tokio::select! {
                    biased;
                    res = fut => res,
                    () = tokio::time::sleep(hop_timeout) => {
                        Err(NgcErrorInfo::new(
                            NgcErrorCode::DeadlineExceeded,
                            "branch hop deadline exceeded",
                        ).with_retry(true, 0))
                    }
                }
            };

            let end_ms = self.clock.unix_millis() as u64;
            let elapsed_ms = self
                .clock
                .now_instant()
                .saturating_duration_since(start)
                .as_millis() as u64;

            match outcome {
                Ok(data) => {
                    replica.record_completion(end_ms, CallOutcome::Ok, elapsed_ms, probe);
                    return Ok(ChaosUnaryResponse {
                        branch_uid: replica.branch_uid,
                        data,
                        retries_used,
                    });
                }
                Err(ngc_err) => {
                    let is_client_err = matches!(
                        ngc_err.code,
                        NgcErrorCode::InvalidArgument
                            | NgcErrorCode::PermissionDenied
                            | NgcErrorCode::NotFound
                    );
                    let call_outcome = if is_client_err {
                        CallOutcome::ClientError
                    } else {
                        CallOutcome::Failure
                    };
                    replica.record_completion(end_ms, call_outcome, elapsed_ms, probe);
                    excluded.push(idx);

                    if ngc_err.retryable
                        && retries_used < self.max_retries
                        && self.retry_budget.try_withdraw_retry()
                    {
                        retries_used += 1;
                        continue;
                    }
                    return Err(GapError::from(ngc_err));
                }
            }
        }
    }

    /// Executes a `foreach` call supporting mid-stream crashes and `RUN-024` partial isolation.
    pub async fn execute_foreach(
        &self,
        operation: &str,
        base_arguments: Value,
        foreach_sets: Vec<Value>,
        request_deadline: Duration,
    ) -> GapResult<ChaosForeachReport> {
        let now_inst = self.clock.now_instant();
        let now_ms = self.clock.unix_millis() as u64;
        let (idx, probe) = self.select_replica(now_ms, &[])?;
        let replica = Arc::clone(&self.replicas[idx]);

        let Some(hop_timeout) =
            Self::per_hop_deadline(request_deadline, replica.config.default_timeout)
        else {
            drop(probe);
            return Err(GapError::new(
                ErrorCode::DeadlineExceeded,
                "request deadline expired before foreach hop",
            ));
        };

        let _permit = replica.in_flight.acquire();
        replica.routed_calls.fetch_add(1, Ordering::Relaxed);
        let profile = replica.injector.profile();
        let n_items = foreach_sets.len();

        let foreach_res = if profile.partitioned {
            tokio::time::sleep(hop_timeout).await;
            let affected: Vec<u32> = (0..n_items).map(|i| i as u32).collect();
            let err = GapError::new(
                ErrorCode::BranchTimeout,
                "network partition during foreach stream",
            )
            .with_retry(true, Some(100));
            let end_ms = self.clock.unix_millis() as u64;
            let elapsed_ms = self
                .clock
                .now_instant()
                .saturating_duration_since(now_inst)
                .as_millis() as u64;
            replica.record_completion(end_ms, CallOutcome::Failure, elapsed_ms, probe);
            return Ok(ChaosForeachReport {
                branch_uid: replica.branch_uid,
                items: vec![None; n_items],
                delivered_chunks: Vec::new(),
                partial: true,
                affected_indexes: affected,
                errors: vec![err],
            });
        } else {
            let added = Duration::from_millis(profile.added_latency_ms);
            let fut = async {
                if !added.is_zero() {
                    tokio::time::sleep(added).await;
                }
                replica
                    .connector
                    .call_foreach(operation, base_arguments, foreach_sets)
                    .await
            };
            tokio::select! {
                biased;
                res = fut => Some(res),
                () = tokio::time::sleep(hop_timeout) => None,
            }
        };

        let end_ms = self.clock.unix_millis() as u64;
        let elapsed_ms = self
            .clock
            .now_instant()
            .saturating_duration_since(now_inst)
            .as_millis() as u64;

        let Some(mut result) = foreach_res else {
            replica.record_completion(end_ms, CallOutcome::Failure, elapsed_ms, probe);
            return Err(GapError::new(
                ErrorCode::BranchTimeout,
                "foreach hop deadline exceeded",
            ));
        };

        // Apply mid-stream crash truncation if configured (`RUN-024`).
        let mut crashed_mid_stream = false;
        if let Some(crash_after) = profile.mid_stream_crash_after_chunks
            && (result.chunks.len() as u32) > crash_after
        {
            result.chunks.truncate(crash_after as usize);
            crashed_mid_stream = true;
        }

        let mut items: Vec<Option<Value>> = vec![None; n_items];
        let mut closed_ok = vec![false; n_items];
        let mut affected_set = Vec::new();
        let mut errors = Vec::new();

        for chunk in &result.chunks {
            let i = chunk.foreach_index as usize;
            if i >= n_items {
                continue;
            }
            if let Some(ref val) = chunk.data
                && chunk.last_for_index
            {
                items[i] = Some(val.clone());
                closed_ok[i] = true;
            }
            if let Some(ref ngc_err) = chunk.error {
                items[i] = None;
                affected_set.push(chunk.foreach_index);
                errors.push(GapError::from(ngc_err.clone()));
            }
        }

        for (i, &ok) in closed_ok.iter().enumerate() {
            let idx_u32 = i as u32;
            if !ok && !affected_set.contains(&idx_u32) {
                items[i] = None;
                affected_set.push(idx_u32);
            }
        }
        affected_set.sort_unstable();

        if crashed_mid_stream {
            errors.push(GapError::new(
                ErrorCode::BranchError,
                format!(
                    "{} foreach items lost due to mid-stream branch crash; partial=true metadata: {}",
                    affected_set.len(),
                    json!({ "affectedIndexes": affected_set })
                ),
            ));
        }

        let partial = !affected_set.is_empty();
        let outcome = if crashed_mid_stream
            || partial && errors.iter().any(|e| e.code() == ErrorCode::BranchError)
        {
            CallOutcome::Failure
        } else {
            CallOutcome::Ok
        };
        replica.record_completion(end_ms, outcome, elapsed_ms, probe);

        Ok(ChaosForeachReport {
            branch_uid: replica.branch_uid,
            items,
            delivered_chunks: result.chunks,
            partial,
            affected_indexes: affected_set,
            errors,
        })
    }
}
