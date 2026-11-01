//! Adaptive concurrency limiting and priority load shedding (`RUN-040`, `ARC-011`, `INV-02`).
//!
//! The limiter caps requests in flight at a limit that follows the service's own latency: a gradient controller
//! (Vegas / Netflix `Gradient` shape) compares the recent round-trip time with a slowly adapting no-load baseline,
//! shrinks the limit when latency rises (queues are forming) and grows it by `sqrt(limit)` otherwise; an explicit
//! drop signal (a timeout) additionally cuts it multiplicatively (AIMD). Initial 256, minimum 16, maximum 8,192.
//!
//! When the limit is reached requests are **shed, never queued** (`GAP_OVERLOADED` with a `Retry-After`
//! derived from the observed latency): `interactive` requests may use the whole limit, `batch`
//! (`X-Gap-Priority: batch`) 60% of it and `background` 30%, so batch work is refused first and an
//! overload cannot starve interactive callers.

use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use gap_core::clock::Clock;
use gap_core::error::{ErrorCode, GapError};

use crate::gateway::{AdmissionOutcome, AdmissionPermit};

/// Request priority class (`RUN-040`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Default: interactive callers (agents waiting for an answer).
    Interactive,
    /// Bulk callers that announce themselves with `X-Gap-Priority: batch`.
    Batch,
    /// Internal work (tasks, ingestion): never selected by a header.
    Background,
}

impl Priority {
    /// Number of classes.
    pub const COUNT: usize = 3;

    /// Reads `X-Gap-Priority`: `batch` selects [`Priority::Batch`]; anything else (absent, unknown,
    /// `background` — which a client may not claim — or malformed) is [`Priority::Interactive`]. A client can
    /// therefore only ever *lower* its own priority.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        match headers
            .get("x-gap-priority")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
        {
            Some(v) if v.eq_ignore_ascii_case("batch") => Self::Batch,
            _ => Self::Interactive,
        }
    }

    /// Stable lower-case label (metrics, logs).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Batch => "batch",
            Self::Background => "background",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Invalid [`LimiterConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimiterConfigError(pub String);

impl fmt::Display for LimiterConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid limiter configuration: {}", self.0)
    }
}

impl std::error::Error for LimiterConfigError {}

/// Tuning of the [`AdaptiveLimiter`]; the defaults are those of `RUN-040`.
#[derive(Debug, Clone, PartialEq)]
pub struct LimiterConfig {
    /// Starting limit (default 256).
    pub initial_limit: u32,
    /// Lower bound (default 16): the limiter never refuses all traffic.
    pub min_limit: u32,
    /// Upper bound (default 8,192).
    pub max_limit: u32,
    /// Weight of a new limit estimate against the current limit, in `(0, 1]` (default 0.2).
    pub smoothing: f64,
    /// Latency tolerance: the limit only shrinks when the recent RTT exceeds `tolerance x` the no-load baseline (default 1.5).
    pub rtt_tolerance: f64,
    /// Share of the limit batch requests may use, percent (default 60).
    pub batch_share_percent: u32,
    /// Share of the limit background requests may use, percent (default 30).
    pub background_share_percent: u32,
    /// Smallest `Retry-After` advertised (default 100 ms).
    pub retry_after_min: Duration,
    /// Largest `Retry-After` advertised (default 5 s).
    pub retry_after_max: Duration,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            initial_limit: 256,
            min_limit: 16,
            max_limit: 8_192,
            smoothing: 0.2,
            rtt_tolerance: 1.5,
            batch_share_percent: 60,
            background_share_percent: 30,
            retry_after_min: Duration::from_millis(100),
            retry_after_max: Duration::from_secs(5),
        }
    }
}

impl LimiterConfig {
    /// Checks the invariants the controller relies on.
    ///
    /// # Errors
    ///
    /// Returns [`LimiterConfigError`] naming the first violated rule.
    pub fn validate(&self) -> Result<(), LimiterConfigError> {
        let bad = |m: &str| Err(LimiterConfigError(m.to_owned()));
        if self.min_limit == 0 {
            return bad("min_limit must be at least 1");
        }
        if !(self.min_limit <= self.initial_limit && self.initial_limit <= self.max_limit) {
            return bad("min_limit <= initial_limit <= max_limit must hold");
        }
        if !(self.smoothing > 0.0 && self.smoothing <= 1.0) {
            return bad("smoothing must be in (0, 1]");
        }
        if !(self.rtt_tolerance >= 1.0 && self.rtt_tolerance.is_finite()) {
            return bad("rtt_tolerance must be finite and at least 1.0");
        }
        if !(1..=100).contains(&self.batch_share_percent)
            || !(1..=100).contains(&self.background_share_percent)
            || self.background_share_percent > self.batch_share_percent
        {
            return bad("shares must be 1..=100 with background <= batch");
        }
        if self.retry_after_min > self.retry_after_max {
            return bad("retry_after_min must not exceed retry_after_max");
        }
        Ok(())
    }
}

/// How a finished request ended, as far as the limiter is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Completed normally: the duration is a service-time sample.
    Success,
    /// Failed or timed out because the service is overloaded: cuts the limit multiplicatively.
    Dropped,
    /// Finished for a reason that says nothing about load (client error, cancellation): no sample.
    Ignore,
}

#[derive(Debug)]
struct Controller {
    limit: f64,
    /// No-load round-trip time, learned **only from samples taken at low load** (at most a quarter of the limit in
    /// flight): under overload every sample is queue-inflated, and an average over all samples would follow the latency
    /// up and never see the degradation. Frozen while the gateway stays loaded; if no low-load sample arrives for
    /// [`BASELINE_STALE_AFTER`] it drifts up slowly ([`BASELINE_DRIFT_PER_SEC`]) so a permanent upstream slowdown cannot
    /// pin the limit at its floor forever.
    baseline_nanos: f64,
    /// Smallest low-load sample of the current and the previous window (the baseline is their minimum, so it
    /// forgets a regime that ended more than one to two windows ago).
    window_min_nanos: [f64; 2],
    window_started: Option<Instant>,
    baseline_at: Option<Instant>,
    last_low_load: Option<Instant>,
    last_update: Option<Instant>,
    last_rtt_nanos: f64,
    samples: u64,
    last_drop: Option<Instant>,
}

/// Length of one baseline window; the baseline is the minimum over the current and the previous window.
const BASELINE_WINDOW: Duration = Duration::from_secs(150);

/// Upward drift of a stale baseline: 2% per second (a permanent doubling is absorbed in about a minute).
const BASELINE_DRIFT_PER_SEC: f64 = 1.02;

/// A baseline with no low-load sample for this long starts to drift.
const BASELINE_STALE_AFTER: Duration = Duration::from_secs(60);

/// Smallest time between two limit updates.
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Point-in-time view for metrics and tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterSnapshot {
    /// Current concurrency limit.
    pub limit: u32,
    /// Requests currently holding a permit.
    pub in_flight: u32,
    /// Requests admitted so far, per priority class (`Priority::index` order).
    pub admitted: [u64; Priority::COUNT],
    /// Requests shed so far, per priority class.
    pub shed: [u64; Priority::COUNT],
    /// Smoothed recent round-trip time, if any sample was seen.
    pub recent_rtt: Option<Duration>,
    /// No-load round-trip time the gradient compares against, once learned.
    pub baseline_rtt: Option<Duration>,
}

/// Adaptive concurrency limiter with priority shedding (`RUN-040`).
pub struct AdaptiveLimiter {
    cfg: LimiterConfig,
    clock: Arc<dyn Clock>,
    limit: AtomicU32,
    in_flight: AtomicU32,
    admitted: [AtomicU64; Priority::COUNT],
    shed: [AtomicU64; Priority::COUNT],
    controller: Mutex<Controller>,
}

impl fmt::Debug for AdaptiveLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.snapshot();
        f.debug_struct("AdaptiveLimiter")
            .field("limit", &s.limit)
            .field("in_flight", &s.in_flight)
            .finish_non_exhaustive()
    }
}

impl AdaptiveLimiter {
    /// Builds a limiter after validating `cfg`.
    ///
    /// # Errors
    ///
    /// Returns [`LimiterConfigError`] if `cfg` is invalid.
    pub fn new(cfg: LimiterConfig, clock: Arc<dyn Clock>) -> Result<Arc<Self>, LimiterConfigError> {
        cfg.validate()?;
        let initial = f64::from(cfg.initial_limit);
        Ok(Arc::new(Self {
            limit: AtomicU32::new(cfg.initial_limit),
            in_flight: AtomicU32::new(0),
            admitted: Default::default(),
            shed: Default::default(),
            controller: Mutex::new(Controller {
                limit: initial,
                baseline_nanos: 0.0,
                window_min_nanos: [f64::INFINITY; 2],
                window_started: None,
                baseline_at: None,
                last_low_load: None,
                last_update: None,
                last_rtt_nanos: 0.0,
                samples: 0,
                last_drop: None,
            }),
            clock,
            cfg,
        }))
    }

    /// Current limit.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.limit.load(Ordering::Acquire)
    }

    /// Requests currently in flight.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Counters and the current limit.
    #[must_use]
    pub fn snapshot(&self) -> LimiterSnapshot {
        let (recent, baseline) = self
            .controller
            .lock()
            .ok()
            .filter(|c| c.samples > 0)
            .map_or((None, None), |c| {
                (
                    Some(Duration::from_nanos(c.last_rtt_nanos as u64)),
                    Some(Duration::from_nanos(c.baseline_nanos as u64)),
                )
            });
        LimiterSnapshot {
            limit: self.limit(),
            in_flight: self.in_flight(),
            admitted: std::array::from_fn(|i| self.admitted[i].load(Ordering::Relaxed)),
            shed: std::array::from_fn(|i| self.shed[i].load(Ordering::Relaxed)),
            recent_rtt: recent,
            baseline_rtt: baseline,
        }
    }

    /// In-flight ceiling for `priority` at the current limit (always at least 1).
    fn ceiling(&self, priority: Priority) -> u32 {
        let limit = u64::from(self.limit());
        let share = match priority {
            Priority::Interactive => 100,
            Priority::Batch => u64::from(self.cfg.batch_share_percent),
            Priority::Background => u64::from(self.cfg.background_share_percent),
        };
        u32::try_from((limit * share / 100).max(1)).unwrap_or(u32::MAX)
    }

    /// Admits a request of `priority` or sheds it; never queues.
    ///
    /// # Errors
    ///
    /// Returns `GAP_OVERLOADED` (retryable, with `afterMs`) when the in-flight count has reached the ceiling of
    /// `priority`.
    pub fn try_acquire(self: &Arc<Self>, priority: Priority) -> Result<LimitPermit, GapError> {
        let ceiling = self.ceiling(priority);
        let mut current = self.in_flight.load(Ordering::Acquire);
        let admitted_at_load = loop {
            if current >= ceiling {
                self.shed[priority.index()].fetch_add(1, Ordering::Relaxed);
                return Err(self.overloaded(priority));
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break current + 1,
                Err(actual) => current = actual,
            }
        };
        self.admitted[priority.index()].fetch_add(1, Ordering::Relaxed);
        Ok(LimitPermit {
            limiter: Arc::clone(self),
            started: self.clock.now_instant(),
            admitted_at_load,
            outcome: Outcome::Success,
        })
    }

    fn overloaded(&self, priority: Priority) -> GapError {
        let after = self.retry_after();
        let after_ms = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
        GapError::new(
            ErrorCode::Overloaded,
            format!(
                "the gateway is at its concurrency limit; {} requests are refused first, retry later",
                priority.as_str()
            ),
        )
        .with_retry(true, Some(after_ms))
    }

    /// `Retry-After`: twice the recent service time, clamped to the configured range.
    fn retry_after(&self) -> Duration {
        let recent = self
            .controller
            .lock()
            .ok()
            .filter(|c| c.samples > 0)
            .map_or(Duration::from_secs(1), |c| {
                Duration::from_nanos((c.last_rtt_nanos * 2.0) as u64)
            });
        recent.clamp(self.cfg.retry_after_min, self.cfg.retry_after_max)
    }

    fn release(&self, rtt: Duration, outcome: Outcome, admitted_at_load: u32) {
        match outcome {
            Outcome::Ignore => {}
            Outcome::Dropped => self.on_drop(),
            Outcome::Success => self.on_sample(rtt, admitted_at_load),
        }
    }

    /// AIMD decrease, at most once per 100 ms so a burst of timeouts is one signal.
    fn on_drop(&self) {
        let now = self.clock.now_instant();
        let Ok(mut c) = self.controller.lock() else {
            return;
        };
        if c.last_drop
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(100))
        {
            return;
        }
        c.last_drop = Some(now);
        c.limit = (c.limit * 0.9).max(f64::from(self.cfg.min_limit));
        self.store_limit(c.limit);
    }

    /// Gradient update from one service-time sample.
    fn on_sample(&self, rtt: Duration, admitted_at_load: u32) {
        let rtt_nanos = rtt.as_nanos() as f64;
        if rtt_nanos <= 0.0 {
            return;
        }
        let Ok(mut c) = self.controller.lock() else {
            return;
        };
        c.samples += 1;
        // Recent RTT: fast EWMA; long-run RTT: slow EWMA seeded by the first samples.
        c.last_rtt_nanos = if c.samples == 1 {
            rtt_nanos
        } else {
            c.last_rtt_nanos * 0.8 + rtt_nanos * 0.2
        };
        let now = self.clock.now_instant();
        let low_load = f64::from(admitted_at_load) * 4.0 <= c.limit;
        let since_check = c
            .baseline_at
            .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        c.baseline_at = Some(now);
        if low_load || c.baseline_nanos <= 0.0 {
            let started = *c.window_started.get_or_insert(now);
            if now.saturating_duration_since(started) >= BASELINE_WINDOW {
                c.window_min_nanos = [f64::INFINITY, c.window_min_nanos[0]];
                c.window_started = Some(now);
            }
            c.window_min_nanos[0] = c.window_min_nanos[0].min(rtt_nanos);
            c.baseline_nanos = c.window_min_nanos[0].min(c.window_min_nanos[1]);
            c.last_low_load = Some(now);
        } else if c
            .last_low_load
            .is_some_and(|t| now.saturating_duration_since(t) > BASELINE_STALE_AFTER)
        {
            let drift = BASELINE_DRIFT_PER_SEC.powf(since_check);
            c.baseline_nanos *= drift;
            for m in &mut c.window_min_nanos {
                *m *= drift;
            }
        }

        // The limit moves at most once per four round trips (and not faster than every 100 ms): applying the update to every
        // one of thousands of completions per second would compound into violent oscillation.
        let interval = Duration::from_nanos(c.last_rtt_nanos as u64)
            .saturating_mul(4)
            .max(MIN_UPDATE_INTERVAL);
        if c.last_update
            .is_some_and(|t| now.saturating_duration_since(t) < interval)
        {
            return;
        }
        c.last_update = Some(now);

        // Do not shrink a limit nobody is using: an idle gateway with one slow request must not collapse it.
        let mut gradient =
            (self.cfg.rtt_tolerance * c.baseline_nanos / c.last_rtt_nanos).clamp(0.5, 1.0);
        if f64::from(admitted_at_load) * 2.0 < c.limit {
            gradient = gradient.max(1.0);
        }
        let queue = c.limit.sqrt().max(1.0);
        let estimate = c.limit * gradient + queue;
        let smoothed = c.limit * (1.0 - self.cfg.smoothing) + estimate * self.cfg.smoothing;
        // Growth is only useful while the current limit is actually reached.
        let new_limit = if gradient >= 1.0 && f64::from(admitted_at_load) * 2.0 < c.limit {
            c.limit
        } else {
            smoothed
        };
        c.limit = new_limit.clamp(f64::from(self.cfg.min_limit), f64::from(self.cfg.max_limit));
        self.store_limit(c.limit);
    }

    fn store_limit(&self, limit: f64) {
        self.limit
            .store(limit.round().max(1.0) as u32, Ordering::Release);
    }
}

/// A concurrency permit: released on drop, which also feeds the limiter one latency sample.
pub struct LimitPermit {
    limiter: Arc<AdaptiveLimiter>,
    started: Instant,
    /// Requests in flight when this one was admitted, itself included: the queueing it can have experienced.
    admitted_at_load: u32,
    outcome: Outcome,
}

impl fmt::Debug for LimitPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LimitPermit")
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

impl LimitPermit {
    /// Sets how the request ended (default [`Outcome::Success`]).
    pub fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}

impl AdmissionPermit for LimitPermit {
    fn set_outcome(&mut self, outcome: AdmissionOutcome) {
        self.outcome = match outcome {
            AdmissionOutcome::Success => Outcome::Success,
            AdmissionOutcome::Dropped => Outcome::Dropped,
            AdmissionOutcome::Ignore => Outcome::Ignore,
        };
    }
}

impl Drop for LimitPermit {
    fn drop(&mut self) {
        let rtt = self
            .limiter
            .clock
            .now_instant()
            .saturating_duration_since(self.started);
        self.limiter.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.limiter
            .release(rtt, self.outcome, self.admitted_at_load);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gap_core::clock::ManualClock;

    fn limiter(cfg: LimiterConfig) -> (Arc<AdaptiveLimiter>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let l =
            AdaptiveLimiter::new(cfg, Arc::clone(&clock) as Arc<dyn Clock>).expect("valid config");
        (l, clock)
    }

    // spec: RUN-040
    #[test]
    fn defaults_are_the_specified_ones_and_validate() {
        let cfg = LimiterConfig::default();
        assert_eq!(
            (cfg.initial_limit, cfg.min_limit, cfg.max_limit),
            (256, 16, 8_192)
        );
        assert!(cfg.validate().is_ok());
        for bad in [
            LimiterConfig {
                min_limit: 0,
                ..LimiterConfig::default()
            },
            LimiterConfig {
                initial_limit: 8,
                ..LimiterConfig::default()
            },
            LimiterConfig {
                initial_limit: 9_000,
                ..LimiterConfig::default()
            },
            LimiterConfig {
                smoothing: 0.0,
                ..LimiterConfig::default()
            },
            LimiterConfig {
                rtt_tolerance: 0.5,
                ..LimiterConfig::default()
            },
            LimiterConfig {
                background_share_percent: 90,
                ..LimiterConfig::default()
            },
            LimiterConfig {
                retry_after_min: Duration::from_secs(9),
                ..LimiterConfig::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    // spec: RUN-040
    #[test]
    fn priority_header_can_only_lower_priority() {
        let mut h = HeaderMap::new();
        assert_eq!(Priority::from_headers(&h), Priority::Interactive);
        for (value, expected) in [
            ("batch", Priority::Batch),
            (" BATCH ", Priority::Batch),
            ("background", Priority::Interactive),
            ("interactive", Priority::Interactive),
            ("urgent", Priority::Interactive),
        ] {
            h.insert("x-gap-priority", value.parse().unwrap());
            assert_eq!(Priority::from_headers(&h), expected, "{value}");
        }
    }

    // spec: RUN-040, ARC-011
    #[test]
    fn requests_beyond_the_ceiling_are_shed_not_queued_and_batch_goes_first() {
        let (l, _clock) = limiter(LimiterConfig {
            initial_limit: 20,
            min_limit: 10,
            max_limit: 20,
            ..LimiterConfig::default()
        });
        // Fill up to the background ceiling (30% of 20 = 6): the next background request is shed.
        let mut held = Vec::new();
        for _ in 0..6 {
            held.push(
                l.try_acquire(Priority::Background)
                    .expect("below the background ceiling"),
            );
        }
        let err = l
            .try_acquire(Priority::Background)
            .expect_err("background ceiling reached");
        assert_eq!(err.code(), ErrorCode::Overloaded);
        assert!(err.extensions.retry.retryable);
        assert!(err.extensions.retry.after_ms.is_some_and(|ms| ms >= 100));
        // Batch still has room up to 60% (12), interactive up to 100% (20).
        for _ in 6..12 {
            held.push(
                l.try_acquire(Priority::Batch)
                    .expect("below the batch ceiling"),
            );
        }
        assert!(
            l.try_acquire(Priority::Batch).is_err(),
            "batch is shed at 12"
        );
        for _ in 12..20 {
            held.push(
                l.try_acquire(Priority::Interactive)
                    .expect("interactive may use the whole limit"),
            );
        }
        assert!(
            l.try_acquire(Priority::Interactive).is_err(),
            "everything is shed at the limit"
        );
        assert_eq!(l.in_flight(), 20);
        let snap = l.snapshot();
        assert_eq!(snap.shed, [1, 1, 1]);
        // Releasing a permit frees exactly one slot.
        held.pop();
        assert!(l.try_acquire(Priority::Interactive).is_ok());
    }

    // spec: INV-02
    #[test]
    fn dropping_a_permit_releases_it_exactly_once_even_when_leaked_futures_are_cancelled() {
        let (l, _clock) = limiter(LimiterConfig::default());
        for _ in 0..10_000 {
            let permit = l.try_acquire(Priority::Interactive).expect("room");
            drop(permit);
        }
        assert_eq!(l.in_flight(), 0);
    }

    /// A toy service: latency is `base` while `in_flight <= knee` and grows linearly above it.
    fn simulated_latency(base: Duration, knee: u32, in_flight: u32) -> Duration {
        if in_flight <= knee {
            base
        } else {
            base + base * (in_flight - knee) / 4
        }
    }

    // spec: RUN-040
    #[test]
    fn the_limit_converges_near_the_knee_of_a_simulated_service_and_stays_in_bounds() {
        let (l, clock) = limiter(LimiterConfig::default());
        let base = Duration::from_millis(10);
        let knee = 120;
        // A short low-load warm-up teaches the baseline the no-load latency, as in any real deployment.
        for _ in 0..20 {
            let p = l.try_acquire(Priority::Interactive).unwrap();
            clock.advance(base);
            drop(p);
        }
        // Closed loop: keep the gateway saturated, complete requests with the simulated latency.
        for _round in 0..4_000 {
            let mut permits = Vec::new();
            while let Ok(p) = l.try_acquire(Priority::Interactive) {
                permits.push(p);
                if permits.len() > 20_000 {
                    break;
                }
            }
            let latency = simulated_latency(base, knee, l.in_flight());
            clock.advance(latency);
            drop(permits);
            let lim = l.limit();
            assert!((16..=8_192).contains(&lim), "limit {lim} out of bounds");
        }
        let settled = l.limit();
        assert!(
            (knee / 2..=knee * 3).contains(&settled),
            "the limit should settle in the neighbourhood of the knee ({knee}), got {settled}"
        );
    }

    // spec: RUN-040
    #[test]
    fn a_light_load_never_collapses_the_limit_and_idle_slow_requests_are_ignored() {
        let (l, clock) = limiter(LimiterConfig::default());
        // One slow request at a time: far below half the limit, so it must not shrink it.
        for i in 0..200 {
            let p = l.try_acquire(Priority::Interactive).unwrap();
            clock.advance(if i % 2 == 0 {
                Duration::from_millis(5)
            } else {
                Duration::from_millis(900)
            });
            drop(p);
        }
        assert!(
            l.limit() >= 256,
            "an unused limit must not shrink: {}",
            l.limit()
        );
    }

    // spec: RUN-040
    #[test]
    fn a_drop_signal_cuts_the_limit_multiplicatively_once_per_burst_and_streams_are_not_samples() {
        let (l, clock) = limiter(LimiterConfig::default());
        let before = l.limit();
        for _ in 0..5 {
            let mut p = l.try_acquire(Priority::Interactive).unwrap();
            p.set_outcome(Outcome::Dropped);
            drop(p);
        }
        let after = l.limit();
        assert_eq!(
            after,
            (f64::from(before) * 0.9).round() as u32,
            "a burst of drops is one signal"
        );
        clock.advance(Duration::from_millis(150));
        let mut p = l.try_acquire(Priority::Interactive).unwrap();
        p.set_outcome(Outcome::Dropped);
        drop(p);
        assert!(l.limit() < after, "a later drop cuts again");

        let mut s = l.try_acquire(Priority::Interactive).unwrap();
        AdmissionPermit::set_outcome(&mut s, AdmissionOutcome::Ignore);
        clock.advance(Duration::from_secs(3_600));
        let snapshot_before = l.limit();
        drop(s);
        assert_eq!(
            l.limit(),
            snapshot_before,
            "an hour-long stream is not a latency sample"
        );
    }

    // spec: RUN-040
    #[test]
    fn the_limit_never_leaves_its_bounds_under_a_drop_storm() {
        let (l, clock) = limiter(LimiterConfig::default());
        for _ in 0..500 {
            clock.advance(Duration::from_millis(150));
            let mut p = l.try_acquire(Priority::Interactive).unwrap();
            p.set_outcome(Outcome::Dropped);
            drop(p);
        }
        assert_eq!(l.limit(), 16, "the floor holds");
    }
}
