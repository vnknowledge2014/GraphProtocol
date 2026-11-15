//! Lock-free online routing Prometheus metrics and server-observed attribution helpers
//! (`WP-M5-05`; `RTE-110`, `RTE-111`, `TM-14`, `INV-02`).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use gap_core::ids::GroupId;

use super::{FeedbackOutcome, RoutingFeedbackRow};

/// Maximum 5-minute window (`300_000 ms`) between discovery and server execution (`RTE-110`).
pub const FIVE_MINUTE_WINDOW_MS: i64 = 300_000;

/// Cumulative upper bounds (`<= b`) for `gap_discovery_set_size` histogram (`RTE-111`).
pub const SET_SIZE_HISTOGRAM_BUCKETS: [usize; 7] = [0, 1, 2, 3, 4, 8, 16];

/// Discovery decision outcome label on `gap_discovery_decisions_total{outcome, model}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionOutcomeLabel {
    /// Confident top-1 or singleton conformal prediction.
    Confident = 0,
    /// Multi-element conformal prediction set requiring disambiguation or top-K fanout.
    Ambiguous = 1,
    /// Calibrated abstention (`insufficient evidence`).
    Abstained = 2,
    /// Degraded baseline fallback (`INV-17`).
    Fallback = 3,
}

impl DecisionOutcomeLabel {
    const ALL: [Self; 4] = [
        Self::Confident,
        Self::Ambiguous,
        Self::Abstained,
        Self::Fallback,
    ];

    /// Prometheus `outcome` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confident => "confident",
            Self::Ambiguous => "ambiguous",
            Self::Abstained => "abstained",
            Self::Fallback => "fallback",
        }
    }
}

/// Active routing model label on `gap_discovery_decisions_total{outcome, model}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouterModelLabel {
    /// Deterministic retrieval + BM25 baseline router.
    Baseline = 0,
    /// Calibrated local System-1 classifier.
    S1 = 1,
}

impl RouterModelLabel {
    const ALL: [Self; 2] = [Self::Baseline, Self::S1];

    /// Prometheus `model` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::S1 => "s1",
        }
    }
}

/// Fallback reason label on `gap_router_fallback_total{reason}` (`RTE-111`, `INV-17`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FallbackReasonLabel {
    /// Per-call S1 inference exceeded `s1_timeout_ms`.
    Timeout = 0,
    /// S1 backend returned an error or malformed output.
    BackendError = 1,
    /// Missing/invalid calibration table degraded decision to uncalibrated baseline.
    Uncalibrated = 2,
    /// Router disabled via runtime flag or snapshot degradation.
    Disabled = 3,
    /// S1 candidate token/option budget exceeded.
    BudgetExceeded = 4,
}

impl FallbackReasonLabel {
    const ALL: [Self; 5] = [
        Self::Timeout,
        Self::BackendError,
        Self::Uncalibrated,
        Self::Disabled,
        Self::BudgetExceeded,
    ];

    /// Prometheus `reason` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::BackendError => "backend_error",
            Self::Uncalibrated => "uncalibrated",
            Self::Disabled => "disabled",
            Self::BudgetExceeded => "budget_exceeded",
        }
    }
}

/// Lock-free telemetry counters, histogram, and online ECE gauge (`INV-02`, `RTE-111`).
///
/// Exposed metrics:
/// - `gap_discovery_decisions_total{outcome,model}`
/// - `gap_discovery_set_size` (histogram)
/// - `gap_router_fallback_total{reason}`
/// - `gap_s1_online_ece` (gauge)
#[derive(Debug)]
pub struct RoutingMetrics {
    decisions: [[AtomicU64; 2]; 4],
    fallbacks: [AtomicU64; 5],
    set_size_buckets: [AtomicU64; 8], // 7 finite upper bounds + `+Inf`
    set_size_sum: AtomicU64,
    set_size_count: AtomicU64,
    online_ece_bits: AtomicU64,
}

impl Default for RoutingMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl RoutingMetrics {
    /// Creates zero-initialized atomic routing metrics (`gap_s1_online_ece = 0.0`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            decisions: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            fallbacks: std::array::from_fn(|_| AtomicU64::new(0)),
            set_size_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            set_size_sum: AtomicU64::new(0),
            set_size_count: AtomicU64::new(0),
            online_ece_bits: AtomicU64::new(0.0_f64.to_bits()),
        }
    }

    /// Records a discovery routing outcome and prediction set size without locks (`INV-02`).
    pub fn record_decision(
        &self,
        outcome: DecisionOutcomeLabel,
        model: RouterModelLabel,
        set_size: usize,
    ) {
        self.decisions[outcome as usize][model as usize].fetch_add(1, Ordering::Relaxed);
        self.set_size_count.fetch_add(1, Ordering::Relaxed);
        self.set_size_sum
            .fetch_add(set_size as u64, Ordering::Relaxed);

        for (idx, &bound) in SET_SIZE_HISTOGRAM_BUCKETS.iter().enumerate() {
            if set_size <= bound {
                self.set_size_buckets[idx].fetch_add(1, Ordering::Relaxed);
            }
        }
        // Always increment the `+Inf` bucket.
        self.set_size_buckets[SET_SIZE_HISTOGRAM_BUCKETS.len()].fetch_add(1, Ordering::Relaxed);
    }

    /// Increments `gap_router_fallback_total{reason}` (`RTE-111`, `INV-17`).
    pub fn record_fallback(&self, reason: FallbackReasonLabel) {
        self.fallbacks[reason as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Updates `gap_s1_online_ece` gauge (`[0.0, 1.0]`), ignoring non-finite inputs (`RTE-111`).
    pub fn set_online_ece(&self, ece: f64) {
        if ece.is_finite() {
            let clamped = ece.clamp(0.0, 1.0);
            self.online_ece_bits
                .store(clamped.to_bits(), Ordering::Relaxed);
        }
    }

    /// Reads the current `gap_s1_online_ece` gauge value.
    #[must_use]
    pub fn online_ece(&self) -> f64 {
        f64::from_bits(self.online_ece_bits.load(Ordering::Relaxed))
    }

    /// Reads the total count for `(outcome, model)`.
    #[must_use]
    pub fn decisions_count(&self, outcome: DecisionOutcomeLabel, model: RouterModelLabel) -> u64 {
        self.decisions[outcome as usize][model as usize].load(Ordering::Relaxed)
    }

    /// Reads the fallback counter for `reason`.
    #[must_use]
    pub fn fallback_count(&self, reason: FallbackReasonLabel) -> u64 {
        self.fallbacks[reason as usize].load(Ordering::Relaxed)
    }

    /// Reads total observations in `gap_discovery_set_size`.
    #[must_use]
    pub fn set_size_count(&self) -> u64 {
        self.set_size_count.load(Ordering::Relaxed)
    }

    /// Reads cumulative sum of `gap_discovery_set_size`.
    #[must_use]
    pub fn set_size_sum(&self) -> u64 {
        self.set_size_sum.load(Ordering::Relaxed)
    }

    /// Reads cumulative count for bucket index `idx` (`0..=6` finite bounds, `7` = `+Inf`).
    #[must_use]
    pub fn set_size_le_bucket(&self, idx: usize) -> Option<u64> {
        self.set_size_buckets
            .get(idx)
            .map(|c| c.load(Ordering::Relaxed))
    }

    /// Renders all metrics in standard Prometheus text exposition format (`0.0.4`).
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let mut out = String::with_capacity(1024);
        let _ = writeln!(
            out,
            "# HELP gap_discovery_decisions_total Total discovery routing decisions by outcome and model."
        );
        let _ = writeln!(out, "# TYPE gap_discovery_decisions_total counter");
        for outcome in DecisionOutcomeLabel::ALL {
            for model in RouterModelLabel::ALL {
                let val = self.decisions_count(outcome, model);
                let _ = writeln!(
                    out,
                    "gap_discovery_decisions_total{{outcome=\"{}\",model=\"{}\"}} {val}",
                    outcome.as_str(),
                    model.as_str()
                );
            }
        }

        let _ = writeln!(
            out,
            "# HELP gap_discovery_set_size Conformal prediction set cardinality |C(x) \\ {{sink}}|."
        );
        let _ = writeln!(out, "# TYPE gap_discovery_set_size histogram");
        for (idx, bound) in SET_SIZE_HISTOGRAM_BUCKETS.iter().enumerate() {
            let count = self.set_size_buckets[idx].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "gap_discovery_set_size_bucket{{le=\"{bound}\"}} {count}"
            );
        }
        let inf = self.set_size_buckets[SET_SIZE_HISTOGRAM_BUCKETS.len()].load(Ordering::Relaxed);
        let _ = writeln!(out, "gap_discovery_set_size_bucket{{le=\"+Inf\"}} {inf}");
        let _ = writeln!(out, "gap_discovery_set_size_sum {}", self.set_size_sum());
        let _ = writeln!(
            out,
            "gap_discovery_set_size_count {}",
            self.set_size_count()
        );

        let _ = writeln!(
            out,
            "# HELP gap_router_fallback_total Total S1 router fallbacks to deterministic baseline."
        );
        let _ = writeln!(out, "# TYPE gap_router_fallback_total counter");
        for reason in FallbackReasonLabel::ALL {
            let val = self.fallback_count(reason);
            let _ = writeln!(
                out,
                "gap_router_fallback_total{{reason=\"{}\"}} {val}",
                reason.as_str()
            );
        }

        let _ = writeln!(
            out,
            "# HELP gap_s1_online_ece Sliding-window 15-bin Expected Calibration Error for System-1 routing."
        );
        let _ = writeln!(out, "# TYPE gap_s1_online_ece gauge");
        let _ = writeln!(out, "gap_s1_online_ece {:.6}", self.online_ece());
        out
    }
}

/// Applies server-observed execution attribution to an in-memory [`RoutingFeedbackRow`] if and only
/// if the row is unattributed (`FeedbackOutcome::None`) and the server observed execution within the
/// `5-minute` window (`RTE-110`, `TM-14`).
///
/// Returns `true` when `row.outcome` transitioned from `None`.
#[must_use]
pub fn attribute_row_from_server_execution(
    row: &mut RoutingFeedbackRow,
    decision_ts_ms: i64,
    observed_ts_ms: i64,
    executed_group: &GroupId,
    execution_succeeded: bool,
    candidate_prediction_set: &[GroupId],
) -> bool {
    if row.outcome != FeedbackOutcome::None {
        return false;
    }
    let delta = observed_ts_ms - decision_ts_ms;
    if !(0..=FIVE_MINUTE_WINDOW_MS).contains(&delta) {
        return false;
    }
    let matches_predicted = row.predicted_group.as_ref() == Some(executed_group)
        || candidate_prediction_set.iter().any(|g| g == executed_group);
    row.outcome = if matches_predicted {
        if execution_succeeded {
            FeedbackOutcome::ExecutedOk
        } else {
            FeedbackOutcome::ExecutedError
        }
    } else {
        FeedbackOutcome::ExecutedOther
    };
    true
}

/// Transitions `row.outcome` to [`FeedbackOutcome::UserLabel`] only after audit log confirmation
/// (`audit_seq > 0`) and non-empty `admin_principal` (`RTE-110`, `TM-14`).
///
/// # Errors
///
/// Returns a static error reason if audit verification is missing.
pub fn apply_audited_admin_label(
    row: &mut RoutingFeedbackRow,
    admin_principal: &str,
    audit_seq: u64,
) -> Result<(), &'static str> {
    if admin_principal.trim().is_empty() || audit_seq == 0 {
        return Err("admin principal and committed audit sequence (> 0) are required");
    }
    row.outcome = FeedbackOutcome::UserLabel;
    Ok(())
}
