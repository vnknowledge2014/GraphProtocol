//! Cold-path branch statistics snapshot for metrics emission (`OPS-020`, `RTE-073`).

use std::sync::atomic::Ordering;

use gap_balance::breaker::BreakerState;

/// Monotonic call counters for one replica (all increase from zero; never reset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplicaCounters {
    pub calls_ok: u64,
    pub calls_failed: u64,
    pub calls_client_error: u64,
    pub retries: u64,
    pub breaker_rejections: u64,
    pub bulkhead_rejections: u64,
    pub health_failures: u64,
}

/// Point-in-time statistics snapshot for one replica.
#[derive(Debug, Clone)]
pub struct ReplicaStats {
    /// Current circuit-breaker state.
    pub breaker_state: BreakerState,
    /// Active in-flight calls.
    pub in_flight: u32,
    /// Effective concurrency capacity (after health scaling).
    pub capacity: u32,
    /// Time-decayed latency EWMA in microseconds, or `None` if unknown.
    pub latency_us: Option<f32>,
    /// Time-decayed error-rate EWMA in `[0.0, 1.0]`, or `None` if unknown.
    pub error_rate: Option<f32>,
    /// Remaining retry budget in milli-tokens.
    pub retry_budget_millis: u32,
    /// Monotonic call counters.
    pub counters: ReplicaCounters,
}

/// Cold-path statistics snapshot for one registered branch (`OPS-020`, `INV-15`).
#[derive(Debug, Clone)]
pub struct BranchStats {
    /// Tenant-free branch label (bounded cardinality, `INV-15`).
    pub label: String,
    /// Per-replica stats, ordered by replica index.
    pub replicas: Vec<ReplicaStats>,
}

/// Per-replica monotonic counters, stored atomically alongside the telemetry slot.
#[derive(Debug, Default)]
pub struct AtomicCounters {
    pub calls_ok: std::sync::atomic::AtomicU64,
    pub calls_failed: std::sync::atomic::AtomicU64,
    pub calls_client_error: std::sync::atomic::AtomicU64,
    pub retries: std::sync::atomic::AtomicU64,
    pub breaker_rejections: std::sync::atomic::AtomicU64,
    pub bulkhead_rejections: std::sync::atomic::AtomicU64,
    pub health_failures: std::sync::atomic::AtomicU64,
}

impl AtomicCounters {
    /// Loads a snapshot of all counters (Relaxed; monotonic counters, no synchronisation needed).
    pub fn snapshot(&self) -> ReplicaCounters {
        ReplicaCounters {
            calls_ok: self.calls_ok.load(Ordering::Relaxed),
            calls_failed: self.calls_failed.load(Ordering::Relaxed),
            calls_client_error: self.calls_client_error.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            breaker_rejections: self.breaker_rejections.load(Ordering::Relaxed),
            bulkhead_rejections: self.bulkhead_rejections.load(Ordering::Relaxed),
            health_failures: self.health_failures.load(Ordering::Relaxed),
        }
    }
}
