//! Per-branch live telemetry slot and cancel-safe [`InFlightGuard`] (`RTE-070`..`RTE-072`, `RTE-084`, `RTE-086`).

use std::time::Duration;

use crossbeam_utils::CachePadded;

use crate::breaker::{Breaker, BreakerState};
use crate::ewma::DecayedEwma;
use crate::retry::RetryBudget;
use crate::sync::{Arc, AtomicU32, Ordering};
use crate::time::Now;

/// Half-life of the per-branch latency EWMA in milliseconds (`10 s`, `RTE §8.1`).
pub(crate) const LATENCY_HALF_LIFE_MS: f32 = 10_000.0;

/// Half-life of the per-branch error-rate EWMA in milliseconds (`30 s`, `RTE §8.1`).
pub(crate) const ERROR_HALF_LIFE_MS: f32 = 30_000.0;

/// Registration parameters configuring a [`TelemetrySlot`] (`RTE-078`, `RTE-080`, `RTE-086`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotParams {
    /// Baseline maximum concurrent in-flight calls (`BranchLimits::max_in_flight`).
    pub max_in_flight: u32,
    /// Per-call connector timeout in milliseconds (`BranchLimits::timeout_ms`).
    pub default_timeout_ms: u32,
    /// Tenant-free metric/event branch label (`1..=64` bytes, no control chars, `OPS-020`, `RTE-078`).
    pub label: Arc<str>,
}

/// Completed call classification fed into [`TelemetrySlot::record`] and `ProbePermit::record` (`RTE-080`, `RTE-084`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Call succeeded.
    Ok,
    /// Connector failed with `retryable = true` or timed out (`RTE-080`).
    Failure,
    /// Deterministic client error (`INVALID_ARGUMENT`, `PERMISSION_DENIED`, `NOT_FOUND`); not a
    /// branch failure and excluded from latency EWMA (`RTE-080`, `RTE-084`).
    ClientError,
}

/// Upstream connector health status reported by NGC `Health` polling (`NGC-070`, `RTE-083`, `RTE-086`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthStatus {
    /// Healthy and serving at normal (`1.0`) degraded factor.
    Serving,
    /// Serving with capacity halved (`0.5` factor, `RTE-086`).
    Degraded,
    /// Unhealthy; forces the circuit breaker `Open` (`RTE-083`, `RTE-085`).
    NotServing,
}

/// Point-in-time diagnostic snapshot of a [`TelemetrySlot`] (`m3-01-design.md` §4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotView {
    /// Time-decayed latency EWMA in microseconds (`10 s` half-life), or `None` if unknown.
    pub latency_us: Option<f32>,
    /// Time-decayed error-rate EWMA in `[0.0, 1.0]` (`30 s` half-life), or `None` if unknown.
    pub error_rate: Option<f32>,
    /// Current number of active in-flight calls held by [`InFlightGuard`]s.
    pub in_flight: u32,
    /// Effective concurrency capacity (`RTE-086`).
    pub capacity: u32,
    /// Circuit-breaker state observed at `now`.
    pub state: BreakerState,
}

/// Live telemetry state for one branch slot outside `Snapshot` (`ARC-006`, `INV-10`, `RTE §8.1`).
#[derive(Debug)]
pub struct TelemetrySlot {
    pub(crate) latency_us: CachePadded<DecayedEwma>,
    pub(crate) error_rate: CachePadded<DecayedEwma>,
    pub(crate) in_flight: CachePadded<AtomicU32>,
    pub(crate) capacity: AtomicU32,
    pub(crate) max_in_flight: u32,
    pub(crate) breaker: Breaker,
    pub(crate) retry_budget: RetryBudget,
    pub(crate) label: Arc<str>,
}

impl TelemetrySlot {
    /// Creates a fresh [`TelemetrySlot`] initialized from `params`.
    #[must_use]
    pub fn new(params: SlotParams) -> Self {
        let initial_capacity = params.max_in_flight.max(1);
        let label = Arc::clone(&params.label);
        Self {
            latency_us: CachePadded::new(DecayedEwma::new()),
            error_rate: CachePadded::new(DecayedEwma::new()),
            in_flight: CachePadded::new(AtomicU32::new(0)),
            capacity: AtomicU32::new(initial_capacity),
            max_in_flight: params.max_in_flight,
            breaker: Breaker::new(params.default_timeout_ms, params.label),
            retry_budget: RetryBudget::new(),
            label,
        }
    }

    /// Records a completed call into the error-rate EWMA, latency EWMA, and (when `Closed`)
    /// the circuit-breaker window (`RTE-084`, `RTE-085`).
    pub fn record(&self, outcome: Outcome, latency: Duration, now: Now) {
        let err_sample = match outcome {
            Outcome::Failure => 1.0_f32,
            Outcome::Ok | Outcome::ClientError => 0.0_f32,
        };
        let _ = self
            .error_rate
            .update(err_sample, now.q, ERROR_HALF_LIFE_MS);

        if matches!(outcome, Outcome::Ok | Outcome::Failure) {
            let micros = (latency.as_secs_f64() * 1_000_000.0).min(f64::from(f32::MAX)) as f32;
            let _ = self.latency_us.update(micros, now.q, LATENCY_HALF_LIFE_MS);
        }

        self.breaker.record_call(outcome, latency, now);
    }

    /// Deposits `0.1` token (`100` milli-tokens) into the slot's [`RetryBudget`] (`RTE-091`).
    pub fn on_first_attempt(&self) {
        self.retry_budget.deposit_first_attempt();
    }

    /// Applies an NGC `Health` report, recomputing `capacity` (`RTE-086`, `NGC-072`) and updating
    /// the circuit breaker (`RTE-083`, `RTE-085`).
    pub fn apply_health(&self, status: HealthStatus, capacity_weight: Option<f64>, now: Now) {
        let weight = match capacity_weight {
            Some(w) if w.is_finite() && w > 0.0 && w <= 1.0 => w,
            Some(invalid) => {
                tracing::warn!(
                    branch = &*self.label,
                    capacity_weight = ?invalid,
                    "ignoring out-of-range capacity_weight outside (0.0, 1.0]; using 1.0"
                );
                1.0
            }
            None => 1.0,
        };
        let degraded_factor = match status {
            HealthStatus::Degraded => 0.5,
            HealthStatus::Serving | HealthStatus::NotServing => 1.0,
        };
        let scaled = (f64::from(self.max_in_flight) * weight * degraded_factor).floor();
        let next_cap = if scaled >= f64::from(u32::MAX) {
            u32::MAX
        } else if scaled < 1.0 {
            1
        } else {
            scaled as u32
        };
        self.capacity.store(next_cap, Ordering::Release);
        self.breaker.on_health(status, now);
    }

    /// Returns a reference to the slot's [`Breaker`].
    #[must_use]
    pub fn breaker(&self) -> &Breaker {
        &self.breaker
    }

    /// Returns a reference to the slot's [`RetryBudget`].
    #[must_use]
    pub fn retry_budget(&self) -> &RetryBudget {
        &self.retry_budget
    }

    /// Returns the current active in-flight call count.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Returns the current effective capacity (`>= 1`, `RTE-086`).
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity.load(Ordering::Acquire)
    }

    /// Captures a point-in-time [`SlotView`] at `now` (cold path).
    #[must_use]
    pub fn view(&self, now: Now) -> SlotView {
        SlotView {
            latency_us: self.latency_us.read(now.q, LATENCY_HALF_LIFE_MS),
            error_rate: self.error_rate.read(now.q, ERROR_HALF_LIFE_MS),
            in_flight: self.in_flight.load(Ordering::Acquire),
            capacity: self.capacity.load(Ordering::Acquire),
            state: self.breaker.state(now),
        }
    }

    /// Resets stale EWMAs (`> 5` half-lives) and stale breaker buckets (`RTE-077`, `RTE-084`).
    pub(crate) fn maintain(&self, now: Now) {
        let _ = self.latency_us.reset_if_stale(now.q, LATENCY_HALF_LIFE_MS);
        let _ = self.error_rate.reset_if_stale(now.q, ERROR_HALF_LIFE_MS);
        self.breaker.clear_stale_buckets(now);
    }
}

/// Cancel-safe RAII guard tracking one active in-flight request on a [`TelemetrySlot`] (`RTE-071`).
///
/// [`Drop`] decrements `in_flight` exactly once, including on task/future cancellation and panic
/// unwinding. Passing a guard to [`std::mem::forget`] permanently leaks its in-flight increment.
#[must_use = "if unused, InFlightGuard drops immediately and releases its in-flight count"]
#[derive(Debug)]
pub struct InFlightGuard {
    slot: Arc<TelemetrySlot>,
}

impl InFlightGuard {
    /// Atomically increments `slot.in_flight` and returns a guard that decrements it on [`Drop`].
    pub fn acquire(slot: &Arc<TelemetrySlot>) -> Self {
        slot.in_flight.fetch_add(1, Ordering::AcqRel);
        Self {
            slot: Arc::clone(slot),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.slot.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    fn test_slot() -> Arc<TelemetrySlot> {
        Arc::new(TelemetrySlot::new(SlotParams {
            max_in_flight: 64,
            default_timeout_ms: 1_000,
            label: Arc::from("billing-primary"),
        }))
    }

    // spec: RTE-071
    #[test]
    fn in_flight_guard_acquire_drop_nested_and_panic_unwind() {
        let slot = test_slot();
        assert_eq!(slot.in_flight(), 0);

        {
            let _g1 = InFlightGuard::acquire(&slot);
            assert_eq!(slot.in_flight(), 1);
            {
                let g2 = InFlightGuard::acquire(&slot);
                let g3 = InFlightGuard::acquire(&slot);
                assert_eq!(slot.in_flight(), 3);
                drop(g2);
                assert_eq!(slot.in_flight(), 2);
                drop(g3);
                assert_eq!(slot.in_flight(), 1);
            }
            assert_eq!(slot.in_flight(), 1);
        }
        assert_eq!(slot.in_flight(), 0);

        let unwind_res = catch_unwind(AssertUnwindSafe(|| {
            let _g_outer = InFlightGuard::acquire(&slot);
            assert_eq!(slot.in_flight(), 1);
            let inner = catch_unwind(AssertUnwindSafe(|| {
                let _g_inner = InFlightGuard::acquire(&slot);
                assert_eq!(slot.in_flight(), 2);
                panic!("simulated connector task panic");
            }));
            assert!(inner.is_err());
            assert_eq!(slot.in_flight(), 1);
            panic!("outer panic");
        }));
        assert!(unwind_res.is_err());
        assert_eq!(slot.in_flight(), 0);
    }

    // spec: RTE-071
    #[test]
    fn in_flight_guard_10000_cancelled_timeout_and_unpolled_futures() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("build paused current-thread runtime");

        rt.block_on(async {
            let slot = test_slot();

            // 1. Futures dropped before first poll never acquire a guard.
            let mut unpolled = Vec::with_capacity(256);
            for _ in 0..256 {
                let s = Arc::clone(&slot);
                unpolled.push(Box::pin(async move {
                    let _guard = InFlightGuard::acquire(&s);
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                }));
            }
            assert_eq!(slot.in_flight(), 0);
            drop(unpolled);
            assert_eq!(slot.in_flight(), 0);

            // 2. 10,000 tasks each hold a guard across sleep(1 h).await, then all are aborted.
            let mut handles = Vec::with_capacity(10_000);
            for _ in 0..10_000 {
                let s = Arc::clone(&slot);
                handles.push(tokio::spawn(async move {
                    let _guard = InFlightGuard::acquire(&s);
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                }));
            }
            while slot.in_flight() < 10_000 {
                tokio::task::yield_now().await;
            }
            assert_eq!(slot.in_flight(), 10_000);

            for h in &handles {
                h.abort();
            }
            for h in handles {
                let err = h.await.expect_err("aborted task must return JoinError");
                assert!(err.is_cancelled());
            }
            assert_eq!(slot.in_flight(), 0);

            // 3. Futures dropped mid-flight by tokio::time::timeout release their guards.
            let mut timeout_handles = Vec::with_capacity(1_000);
            for _ in 0..1_000 {
                let s = Arc::clone(&slot);
                timeout_handles.push(tokio::spawn(async move {
                    tokio::time::timeout(Duration::from_secs(1), async move {
                        let _guard = InFlightGuard::acquire(&s);
                        tokio::time::sleep(Duration::from_secs(3_600)).await;
                    })
                    .await
                }));
            }
            while slot.in_flight() < 1_000 {
                tokio::task::yield_now().await;
            }
            assert_eq!(slot.in_flight(), 1_000);

            tokio::time::advance(Duration::from_secs(1)).await;
            for h in timeout_handles {
                let res = h.await.expect("timeout wrapper task completes cleanly");
                assert!(res.is_err(), "inner sleep(1h) must time out at 1s");
            }
            assert_eq!(slot.in_flight(), 0);
        });
    }

    // spec: RTE-084
    #[test]
    fn slot_record_updates_ewmas_and_ignores_client_error_latency() {
        let slot = test_slot();
        let t0 = Now::from_ms(0);
        assert_eq!(
            slot.view(t0),
            SlotView {
                latency_us: None,
                error_rate: None,
                in_flight: 0,
                capacity: 64,
                state: BreakerState::Closed,
            }
        );

        // ClientError updates error_rate (0.0) but leaves latency_us unknown.
        slot.record(Outcome::ClientError, Duration::from_millis(50), t0);
        let v0 = slot.view(t0);
        assert_eq!(v0.latency_us, None);
        assert_eq!(v0.error_rate, Some(0.0));

        // Ok seeds latency_us in microseconds (2 ms = 2,000 us).
        let t1 = Now::from_ms(4);
        slot.record(Outcome::Ok, Duration::from_millis(2), t1);
        let v1 = slot.view(t1);
        assert_eq!(v1.latency_us, Some(2_000.0));
        assert_eq!(v1.error_rate, Some(0.0));

        // Failure updates both latency_us and error_rate toward 1.0 (after 30s = 1 error half-life -> 0.5).
        let t2 = Now::from_ms(30_004);
        slot.record(Outcome::Failure, Duration::from_millis(10), t2);
        let v2 = slot.view(t2);
        let err = v2.error_rate.expect("error rate known");
        assert!((err - 0.5).abs() <= 1e-3, "expected ~0.5, got {err}");
        assert!(v2.latency_us.expect("latency known") > 2_000.0);

        slot.on_first_attempt();
        assert_eq!(slot.retry_budget().balance_millis(), 10_100);
    }

    struct WarnCounter(Arc<crate::sync::AtomicU32>);

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    // spec: RTE-083, RTE-086, NGC-070, NGC-072 (ADR-025)
    #[test]
    fn health_and_capacity_weights_degraded_multiplication_invalid_and_not_serving() {
        let now = Now::from_ms(1_000);

        // Initial capacity floor at 1 when max_in_flight == 0.
        let zero_slot = TelemetrySlot::new(SlotParams {
            max_in_flight: 0,
            default_timeout_ms: 1_000,
            label: Arc::from("zero-cap"),
        });
        assert_eq!(zero_slot.capacity(), 1);
        zero_slot.apply_health(HealthStatus::Degraded, None, now);
        assert_eq!(zero_slot.capacity(), 1);

        // 1. max_in_flight = 10 x weight 0.3 -> 3; Serving with None restores 10.
        let slot10 = TelemetrySlot::new(SlotParams {
            max_in_flight: 10,
            default_timeout_ms: 1_000,
            label: Arc::from("cap-10"),
        });
        assert_eq!(slot10.capacity(), 10);

        slot10.apply_health(HealthStatus::Serving, Some(0.3), now);
        assert_eq!(slot10.capacity(), 3);
        assert_eq!(slot10.view(now).capacity, 3);

        // 2. Degraded halves (floor, minimum 1).
        slot10.apply_health(HealthStatus::Degraded, None, now);
        assert_eq!(slot10.capacity(), 5);

        let slot9 = TelemetrySlot::new(SlotParams {
            max_in_flight: 9,
            default_timeout_ms: 1_000,
            label: Arc::from("cap-9"),
        });
        slot9.apply_health(HealthStatus::Degraded, None, now);
        assert_eq!(slot9.capacity(), 4); // floor(9 * 0.5) = 4

        let slot1 = TelemetrySlot::new(SlotParams {
            max_in_flight: 1,
            default_timeout_ms: 1_000,
            label: Arc::from("cap-1"),
        });
        slot1.apply_health(HealthStatus::Degraded, None, now);
        assert_eq!(slot1.capacity(), 1); // max(1, floor(1 * 0.5)) = 1

        // 3. Multiplicative combination of capacity_weight and Degraded factor (0.5).
        let slot20 = TelemetrySlot::new(SlotParams {
            max_in_flight: 20,
            default_timeout_ms: 1_000,
            label: Arc::from("cap-20"),
        });
        slot20.apply_health(HealthStatus::Degraded, Some(0.3), now);
        assert_eq!(slot20.capacity(), 3); // floor(20 * 0.3 * 0.5) = 3

        slot10.apply_health(HealthStatus::Degraded, Some(0.3), now);
        assert_eq!(slot10.capacity(), 1); // floor(10 * 0.3 * 0.5) = floor(1.5) = 1

        slot10.apply_health(HealthStatus::Degraded, Some(0.05), now);
        assert_eq!(slot10.capacity(), 1); // max(1, floor(10 * 0.05 * 0.5)) = 1

        // Serving restores full capacity (10).
        slot10.apply_health(HealthStatus::Serving, None, now);
        assert_eq!(slot10.capacity(), 10);

        // 4. Valid upper/lower bounds emit 0 warnings; out-of-range / non-finite weights act as 1.0 and log warn!
        let warns = Arc::new(crate::sync::AtomicU32::new(0));
        let sub = WarnCounter(Arc::clone(&warns));
        tracing::subscriber::with_default(sub, || {
            slot10.apply_health(HealthStatus::Serving, Some(1.0), now);
            assert_eq!(slot10.capacity(), 10);
            slot10.apply_health(HealthStatus::Serving, Some(f64::MIN_POSITIVE), now);
            assert_eq!(slot10.capacity(), 1);
            slot10.apply_health(HealthStatus::Serving, None, now);
            assert_eq!(slot10.capacity(), 10);
            assert_eq!(
                warns.load(Ordering::Relaxed),
                0,
                "valid weights and None must not log warnings"
            );

            let invalid_weights = [
                0.0,
                -0.0,
                -1.0,
                f64::NAN,
                1.000_001,
                1.5,
                f64::INFINITY,
                f64::NEG_INFINITY,
            ];
            for (idx, bad) in invalid_weights.iter().copied().enumerate() {
                // First shrink capacity to 3 so we prove the invalid weight actively resets to 1.0 (10 / 5).
                slot10.apply_health(HealthStatus::Serving, Some(0.3), now);
                assert_eq!(slot10.capacity(), 3);

                slot10.apply_health(HealthStatus::Serving, Some(bad), now);
                assert_eq!(
                    slot10.capacity(),
                    10,
                    "invalid weight #{idx} ({bad:?}) with Serving must act as 1.0"
                );

                slot10.apply_health(HealthStatus::Degraded, Some(bad), now);
                assert_eq!(
                    slot10.capacity(),
                    5,
                    "invalid weight #{idx} ({bad:?}) with Degraded must act as 1.0 * 0.5"
                );
            }
            assert_eq!(
                warns.load(Ordering::Relaxed),
                (invalid_weights.len() * 2) as u32,
                "each invalid Some(weight) call must log one warning"
            );
        });

        // 5. NotServing opens the circuit immediately with retry_after = remaining open time (5,000 ms for k = 0).
        let health_slot = TelemetrySlot::new(SlotParams {
            max_in_flight: 10,
            default_timeout_ms: 1_000,
            label: Arc::from("health-open"),
        });
        health_slot.apply_health(HealthStatus::NotServing, Some(0.4), Now::from_ms(1_000));
        assert_eq!(health_slot.capacity(), 4);
        assert_eq!(
            health_slot.view(Now::from_ms(1_000)).state,
            BreakerState::Open
        );
        match health_slot.breaker().admit(Now::from_ms(1_000)) {
            crate::breaker::Admission::Rejected { retry_after } => {
                assert_eq!(retry_after, Duration::from_millis(5_000));
            }
            other => panic!("expected Rejected(5_000ms), got {other:?}"),
        }
        match health_slot.breaker().admit(Now::from_ms(2_750)) {
            crate::breaker::Admission::Rejected { retry_after } => {
                assert_eq!(retry_after, Duration::from_millis(3_250));
            }
            other => panic!("expected Rejected(3_250ms), got {other:?}"),
        }
    }

    // spec: RTE-070, RTE-077, RTE-084
    #[test]
    fn slot_latency_ewma_10s_half_life_reads_exact_midpoint() {
        let slot = test_slot();
        slot.record(Outcome::Ok, Duration::from_millis(100), Now::from_ms(0));
        assert_eq!(slot.view(Now::from_ms(0)).latency_us, Some(100_000.0));

        slot.record(
            Outcome::Ok,
            Duration::from_millis(200),
            Now::from_ms(10_000),
        );
        let lat = slot
            .view(Now::from_ms(10_000))
            .latency_us
            .expect("latency known at 10 s");
        assert!(
            (lat - 150_000.0).abs() <= 1.0,
            "expected 150,000 us after 10 s half-life, got {lat}"
        );
    }
}
