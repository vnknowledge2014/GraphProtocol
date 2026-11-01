//! Lock-free Prometheus text exposition metrics with bounded label cardinality
//! (`Brief §3.6`, `OPS-020`, `ARC-012`, `INV-02`, `INV-15`).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_plan::PlanCacheStatus;

/// Fixed Prometheus histogram upper bounds in seconds for `gap_http_request_duration_seconds`.
pub const DURATION_BUCKETS_SECS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

const DURATION_BUCKET_LABELS: [&str; 12] = [
    "0.001", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10",
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
    /// A path served by a mounted protocol facade (`HttpModule`: `/mcp`, `/.well-known/...`); one bounded label for all.
    Module = 6,
}

impl RouteLabel {
    /// Total number of closed [`RouteLabel`] variants.
    pub const COUNT: usize = 7;

    /// Ordered slice of all [`RouteLabel`] variants.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Graphql,
        Self::GraphqlStream,
        Self::Healthz,
        Self::Readyz,
        Self::Metrics,
        Self::Unmatched,
        Self::Module,
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
            Self::Module => "module",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum StatusSlot {
    S200 = 0,
    S400 = 1,
    S401 = 2,
    S403 = 3,
    S404 = 4,
    S405 = 5,
    S406 = 6,
    S413 = 7,
    S415 = 8,
    S422 = 9,
    S429 = 10,
    S500 = 11,
    S503 = 12,
    Other = 13,
}

impl StatusSlot {
    const COUNT: usize = 14;

    const ALL: [Self; Self::COUNT] = [
        Self::S200,
        Self::S400,
        Self::S401,
        Self::S403,
        Self::S404,
        Self::S405,
        Self::S406,
        Self::S413,
        Self::S415,
        Self::S422,
        Self::S429,
        Self::S500,
        Self::S503,
        Self::Other,
    ];

    const fn from_status(status: u16) -> Self {
        match status {
            200 => Self::S200,
            400 => Self::S400,
            401 => Self::S401,
            403 => Self::S403,
            404 => Self::S404,
            405 => Self::S405,
            406 => Self::S406,
            413 => Self::S413,
            415 => Self::S415,
            422 => Self::S422,
            429 => Self::S429,
            500 => Self::S500,
            503 => Self::S503,
            _ => Self::Other,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::S200 => "200",
            Self::S400 => "400",
            Self::S401 => "401",
            Self::S403 => "403",
            Self::S404 => "404",
            Self::S405 => "405",
            Self::S406 => "406",
            Self::S413 => "413",
            Self::S415 => "415",
            Self::S422 => "422",
            Self::S429 => "429",
            Self::S500 => "500",
            Self::S503 => "503",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Default)]
struct RouteHistogram {
    buckets: [AtomicU64; 12],
    inf_overflow: AtomicU64,
    sum_nanos: AtomicU64,
}

impl RouteHistogram {
    fn observe(&self, duration: Duration) {
        let secs = duration.as_secs_f64();
        let mut placed = false;
        for (idx, &bound) in DURATION_BUCKETS_SECS.iter().enumerate() {
            if secs <= bound {
                self.buckets[idx].fetch_add(1, Ordering::Relaxed);
                placed = true;
                break;
            }
        }
        if !placed {
            self.inf_overflow.fetch_add(1, Ordering::Relaxed);
        }
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
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
    request_counts: [[AtomicU64; StatusSlot::COUNT]; RouteLabel::COUNT],
    histograms: [RouteHistogram; RouteLabel::COUNT],
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
    pub fn observe_request(&self, route: RouteLabel, status: u16, duration: Duration) {
        let route_idx = route as usize;
        let status_idx = StatusSlot::from_status(status) as usize;
        self.request_counts[route_idx][status_idx].fetch_add(1, Ordering::Relaxed);
        self.histograms[route_idx].observe(duration);
    }

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
    pub fn render_prometheus(&self, snapshot_generation: u64) -> String {
        let mut out = String::with_capacity(2048);

        let _ = writeln!(
            out,
            "# HELP gap_http_requests_total Total HTTP requests handled by gap-transport."
        );
        let _ = writeln!(out, "# TYPE gap_http_requests_total counter");
        for route in RouteLabel::ALL {
            let route_str = route.as_str();
            let row = &self.request_counts[route as usize];
            for slot in StatusSlot::ALL {
                let count = row[slot as usize].load(Ordering::Relaxed);
                if count > 0 {
                    let _ = writeln!(
                        out,
                        "gap_http_requests_total{{route=\"{route_str}\",status=\"{}\"}} {count}",
                        slot.as_str()
                    );
                }
            }
        }

        let in_flight = self.in_flight.load(Ordering::Acquire);
        let _ = writeln!(
            out,
            "# HELP gap_http_in_flight Currently executing HTTP requests."
        );
        let _ = writeln!(out, "# TYPE gap_http_in_flight gauge");
        let _ = writeln!(out, "gap_http_in_flight {in_flight}");

        let _ = writeln!(
            out,
            "# HELP gap_http_request_duration_seconds HTTP request latency in seconds."
        );
        let _ = writeln!(out, "# TYPE gap_http_request_duration_seconds histogram");
        for route in RouteLabel::ALL {
            let hist = &self.histograms[route as usize];
            let mut snapshot_buckets = [0_u64; 12];
            let mut cumulative = 0_u64;
            for (idx, slot) in snapshot_buckets.iter_mut().enumerate() {
                cumulative = cumulative.saturating_add(hist.buckets[idx].load(Ordering::Relaxed));
                *slot = cumulative;
            }
            let total_count = cumulative.saturating_add(hist.inf_overflow.load(Ordering::Relaxed));
            if total_count == 0 {
                continue;
            }
            let route_str = route.as_str();
            for (idx, bound_label) in DURATION_BUCKET_LABELS.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "gap_http_request_duration_seconds_bucket{{route=\"{route_str}\",le=\"{bound_label}\"}} {}",
                    snapshot_buckets[idx]
                );
            }
            let _ = writeln!(
                out,
                "gap_http_request_duration_seconds_bucket{{route=\"{route_str}\",le=\"+Inf\"}} {total_count}"
            );
            let sum_secs = hist.sum_nanos.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
            let _ = writeln!(
                out,
                "gap_http_request_duration_seconds_sum{{route=\"{route_str}\"}} {sum_secs:.6}"
            );
            let _ = writeln!(
                out,
                "gap_http_request_duration_seconds_count{{route=\"{route_str}\"}} {total_count}"
            );
        }

        let hits = self.plan_cache_hits.load(Ordering::Relaxed);
        let misses = self.plan_cache_misses.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP gap_plan_cache_total PlanCache lookups partitioned by hit or miss."
        );
        let _ = writeln!(out, "# TYPE gap_plan_cache_total counter");
        let _ = writeln!(out, "gap_plan_cache_total{{result=\"hit\"}} {hits}");
        let _ = writeln!(out, "gap_plan_cache_total{{result=\"miss\"}} {misses}");

        let _ = writeln!(
            out,
            "# HELP gap_snapshot_generation Active supergraph snapshot generation."
        );
        let _ = writeln!(out, "# TYPE gap_snapshot_generation gauge");
        let _ = writeln!(out, "gap_snapshot_generation {snapshot_generation}");

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: OPS-020, INV-02, INV-15
    #[test]
    fn inv_15_route_label_closed_set_and_raii_guards_are_deterministic() {
        assert_eq!(RouteLabel::from_path("/graphql"), RouteLabel::Graphql);
        assert_eq!(
            RouteLabel::from_path("/graphql/stream"),
            RouteLabel::GraphqlStream
        );
        assert_eq!(RouteLabel::from_path("/healthz"), RouteLabel::Healthz);
        assert_eq!(RouteLabel::from_path("/readyz"), RouteLabel::Readyz);
        assert_eq!(RouteLabel::from_path("/metrics"), RouteLabel::Metrics);
        assert_eq!(
            RouteLabel::from_path("/user/secret-token-123"),
            RouteLabel::Unmatched
        );

        let metrics = Arc::new(TransportMetrics::new());
        {
            let _g1 = metrics.track_in_flight();
            let _g2 = metrics.track_sse_producer();
            assert_eq!(metrics.in_flight(), 1);
            assert_eq!(metrics.sse_producers_in_flight(), 1);
        }
        assert_eq!(metrics.in_flight(), 0);
        assert_eq!(metrics.sse_producers_in_flight(), 0);

        metrics.observe_request(RouteLabel::Graphql, 200, Duration::from_millis(3));
        metrics.observe_request(RouteLabel::Unmatched, 404, Duration::from_micros(400));
        metrics.record_plan_cache(PlanCacheStatus::Miss);
        metrics.record_plan_cache(PlanCacheStatus::Hit);

        let rendered_a = metrics.render_prometheus(7);
        let rendered_b = metrics.render_prometheus(7);
        assert_eq!(rendered_a, rendered_b);
        assert!(rendered_a.contains(r#"gap_http_requests_total{route="/graphql",status="200"} 1"#));
        assert!(
            rendered_a.contains(r#"gap_http_requests_total{route="unmatched",status="404"} 1"#)
        );
        assert!(rendered_a.contains("gap_snapshot_generation 7"));
    }
}
