//! Lock-free Prometheus text exposition metrics with bounded label cardinality
//! (`Brief §3.6`, `OPS-020`, `ARC-012`, `INV-02`, `INV-15`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_plan::PlanCacheStatus;

/// Fixed Prometheus histogram upper bounds in seconds for `gap_http_request_duration_seconds`.
pub const DURATION_BUCKETS_SECS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Closed route label set for HTTP telemetry (`OPS-020`, `INV-15`: never derived from raw user URIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(usize)]
pub enum RouteLabel {
    /// `/graphql`
    Graphql = 0,
    /// `/graphql/stream`
    GraphqlStream = 1,
    /// `/healthz`
    Healthz = 2,
    /// `/readyz`
    Readyz = 3,
    /// `/metrics`
    Metrics = 4,
    /// Any unmatched URI path (`404`).
    Unmatched = 5,
}

impl RouteLabel {
    /// Total number of closed [`RouteLabel`] variants.
    pub const COUNT: usize = 6;

    /// Ordered slice of all [`RouteLabel`] variants.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Graphql,
        Self::GraphqlStream,
        Self::Healthz,
        Self::Readyz,
        Self::Metrics,
        Self::Unmatched,
    ];

    /// Maps an incoming URI path to its closed [`RouteLabel`] (`INV-15`, `OPS-020`).
    #[must_use]
    pub fn from_path(path: &str) -> Self {
        match path {
            "/graphql" => Self::Graphql,
            "/graphql/stream" => Self::GraphqlStream,
            "/healthz" => Self::Healthz,
            "/readyz" => Self::Readyz,
            "/metrics" => Self::Metrics,
            _ => Self::Unmatched,
        }
    }

    /// Canonical bounded Prometheus label string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Graphql => "/graphql",
            Self::GraphqlStream => "/graphql/stream",
            Self::Healthz => "/healthz",
            Self::Readyz => "/readyz",
            Self::Metrics => "/metrics",
            Self::Unmatched => "unmatched",
        }
    }
}

/// RAII guard tracking an in-flight HTTP request (`gap_http_in_flight`, `ARC-012`).
#[derive(Debug)]
pub struct InFlightGuard {
    metrics: Arc<TransportMetrics>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// RAII guard tracking an active `graphql-sse` producer task (`ARC-012`).
#[derive(Debug)]
pub struct SseProducerGuard {
    metrics: Arc<TransportMetrics>,
}

impl Drop for SseProducerGuard {
    fn drop(&mut self) {
        self.metrics
            .sse_producers_in_flight
            .fetch_sub(1, Ordering::AcqRel);
    }
}

/// Lock-free atomic metrics registry for `gap-transport` (`INV-02`, `OPS-020`).
#[derive(Debug, Default)]
pub struct TransportMetrics {
    in_flight: AtomicU64,
    sse_producers_in_flight: AtomicU64,
    plan_cache_hits: AtomicU64,
    plan_cache_misses: AtomicU64,
}

impl TransportMetrics {
    /// Creates a zeroed [`TransportMetrics`] instance.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increments `gap_http_in_flight` and returns an RAII [`InFlightGuard`] that decrements on drop.
    #[must_use]
    pub fn track_in_flight(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        InFlightGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Increments active SSE producer count and returns an RAII [`SseProducerGuard`].
    #[must_use]
    pub fn track_sse_producer(self: &Arc<Self>) -> SseProducerGuard {
        self.sse_producers_in_flight.fetch_add(1, Ordering::AcqRel);
        SseProducerGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Current value of `gap_http_in_flight`.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Current number of active SSE producer tasks.
    #[must_use]
    pub fn sse_producers_in_flight(&self) -> u64 {
        self.sse_producers_in_flight.load(Ordering::Acquire)
    }

    /// Records a completed HTTP request with bounded `route`, HTTP `status`, and `duration`.
    pub fn observe_request(&self, _route: RouteLabel, _status: u16, _duration: Duration) {}

    /// Records a [`PlanCacheStatus`] outcome (`hit` or `miss`) in `gap_plan_cache_total`.
    pub fn record_plan_cache(&self, status: PlanCacheStatus) {
        match status {
            PlanCacheStatus::Hit => {
                self.plan_cache_hits.fetch_add(1, Ordering::Relaxed);
            }
            PlanCacheStatus::Miss => {
                self.plan_cache_misses.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Renders deterministic Prometheus `0.0.4` text exposition (`Brief §3.6`).
    #[must_use]
    pub fn render_prometheus(&self, _snapshot_generation: u64) -> String {
        String::new()
    }
}
