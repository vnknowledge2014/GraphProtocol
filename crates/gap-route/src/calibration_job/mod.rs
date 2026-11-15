//! Temperature scaling (`RTE-062`), split-conformal LAC calibration and decision rules
//! (`RTE-064`, `RTE-065`, `RTE-066`), and snapshot calibration versioning/invalidation (`RTE-067`),
//! plus offline dataset pipeline & online ECE job (`RTE-068`, `RTE-111`, `WP-M5-04`).

pub mod online;
pub mod pipeline;
#[cfg(test)]
mod tests;

pub use online::{
    DEFAULT_ABSTENTION_SHIFT_MULTIPLIER, DEFAULT_ONLINE_ECE_ALERT_THRESHOLD,
    DEFAULT_ONLINE_WINDOW_SIZE, OnlineCalibrationMonitor, OnlineCalibrationSample, OnlineEceAlert,
    OnlineMonitorConfig, OnlineWindowEvaluation, ScheduledRecalibrationJob,
};
pub use pipeline::{
    BucketCalibrationSummary, CalibrationExample, CalibrationPipelineReport, CalibrationSplit,
    ReliabilityBin, WinsorizedConfig, compute_reliability_diagram, run_calibration_pipeline,
};

use std::collections::BTreeMap;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::{CalibrationTable, SideEffectLevel};
use gap_core::traits::DiscoveryOutcome;
use gap_ql::sha256_hex;

use crate::feedback::AbstentionReason;
use crate::groups::{GroupOption, SINK_OPTION_KEY, cmp_f32_desc, side_effect_as_str};

/// Minimum temperature bound permitted by `RTE-041` / `RTE-062`.
pub const TEMP_MIN: f32 = 0.5;

/// Maximum temperature bound permitted by `RTE-041` / `RTE-062`.
pub const TEMP_MAX: f32 = 5.0;

/// Minimum held-out calibration samples required for conformal prediction sets (`RTE-064`).
pub const MIN_CONFORMAL_SAMPLES: usize = 500;

/// Default miscoverage level for read-only (`NONE`) discovery (`RTE-066`).
pub const ALPHA_READ_ONLY: f32 = 0.10;

/// Tighter miscoverage level when mutating (`REVERSIBLE` / `IRREVERSIBLE` / `UNKNOWN`) options
/// remain eligible (`RTE-066`).
pub const ALPHA_MUTATING: f32 = 0.05;

/// Choice-cardinality bucket family defined by `RTE-062` / `RTE-064`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptionsBucket {
    /// Exactly `2` options (1 group + sink).
    Two,
    /// `3..=5` options.
    ThreeToFive,
    /// `6..=10` options.
    SixToTen,
    /// `11..=16` options.
    ElevenToSixteen,
}

impl OptionsBucket {
    /// Classifies option count `n` into its calibration bucket family (`RTE-062`).
    #[must_use]
    pub const fn from_option_count(n: usize) -> Self {
        if n <= 2 {
            Self::Two
        } else if n <= 5 {
            Self::ThreeToFive
        } else if n <= 10 {
            Self::SixToTen
        } else {
            Self::ElevenToSixteen
        }
    }

    /// Stable bucket label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Two => "2",
            Self::ThreeToFive => "3-5",
            Self::SixToTen => "6-10",
            Self::ElevenToSixteen => "11-16",
        }
    }

    /// Fixed index `0..=3` in compact arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Two => 0,
            Self::ThreeToFive => 1,
            Self::SixToTen => 2,
            Self::ElevenToSixteen => 3,
        }
    }

    /// All four bucket families in canonical ascending order.
    pub const ALL: [Self; 4] = [
        Self::Two,
        Self::ThreeToFive,
        Self::SixToTen,
        Self::ElevenToSixteen,
    ];

    /// Fallback `max p` confidence threshold when `n < 500` (`RTE-064`).
    #[must_use]
    pub const fn fallback_threshold(self) -> f32 {
        match self {
            Self::Two => 0.75,
            Self::ThreeToFive => 0.60,
            Self::SixToTen => 0.45,
            Self::ElevenToSixteen => 0.35,
        }
    }
}

/// Calibration mode active during discovery decision (`RTE-064`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalibrationMode {
    /// Split-conformal LAC thresholds fit from `>= 500` held-out samples.
    Conformal,
    /// Small-sample fallback thresholds `{2: 0.75, 3-5: 0.60, 6-10: 0.45, 11-16: 0.35}` (`RTE-064`).
    Fallback,
    /// Baseline uncalibrated or invalidated table fallback (`RTE-067`).
    InvalidatedBaseline,
}

/// Clamps fitted/configured temperature into `[0.5, 5.0]` (`RTE-041`).
#[must_use]
pub fn clamp_temperature(t: f32) -> f32 {
    if !t.is_finite() {
        1.0
    } else {
        t.clamp(TEMP_MIN, TEMP_MAX)
    }
}

/// Computes `softmax(logits / clamp_temperature(temperature))` safely (`RTE-041`).
#[must_use]
pub fn softmax_scaled(logits: &[f32], temperature: f32) -> Vec<f32> {
    if logits.is_empty() {
        return Vec::new();
    }
    let t = clamp_temperature(temperature);
    let max_logit = logits
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold(f32::NEG_INFINITY, f32::max);
    if !max_logit.is_finite() {
        let uniform = 1.0 / (logits.len() as f32);
        return vec![uniform; logits.len()];
    }
    let inv_t = 1.0 / f64::from(t);
    let mut exps: Vec<f64> = Vec::with_capacity(logits.len());
    let mut sum = 0.0_f64;
    for &z in logits {
        let val = if z.is_finite() {
            ((f64::from(z) - f64::from(max_logit)) * inv_t).exp()
        } else {
            0.0
        };
        sum += val;
        exps.push(val);
    }
    if sum <= 0.0 || !sum.is_finite() {
        let uniform = 1.0 / (logits.len() as f32);
        return vec![uniform; logits.len()];
    }
    exps.into_iter().map(|e| (e / sum) as f32).collect()
}

/// Selects per-request miscoverage level `alpha` per `RTE-066`:
/// `0.05` if any non-`NONE` option is in play, else `0.10`.
#[must_use]
pub const fn alpha_for_side_effect_constraint(has_non_read_only_option: bool) -> f32 {
    if has_non_read_only_option {
        ALPHA_MUTATING
    } else {
        ALPHA_READ_ONLY
    }
}

/// Temperature scaling fit statistics stored per bucket (`RTE-062`).
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureFitResult {
    /// Clamped fitted temperature in `[0.5, 5.0]`.
    pub temperature: f32,
    /// Number of samples in the fit split.
    pub sample_count: usize,
    /// Mean negative log-likelihood before fitting (`T = 1.0`).
    pub nll_before: f64,
    /// Mean negative log-likelihood at `temperature`.
    pub nll_after: f64,
    /// 15-bin Expected Calibration Error before fitting (`T = 1.0`).
    pub ece_before: f32,
    /// 15-bin Expected Calibration Error at `temperature`.
    pub ece_after: f32,
}

/// Fits scalar temperature `T` in `[0.5, 5.0]` via 1-D golden-section search over `ln(T)` minimizing
/// mean NLL (`RTE-062`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if `samples` is empty or contains an out-of-range label index.
pub fn fit_temperature_golden_section(
    samples: &[(Vec<f32>, usize)],
) -> GapResult<TemperatureFitResult> {
    if samples.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "cannot fit temperature on empty sample slice",
        ));
    }
    for (logits, gold) in samples {
        if logits.is_empty() || *gold >= logits.len() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "calibration sample has empty logits or out-of-bounds gold index",
            ));
        }
    }

    let nll_before = mean_nll(samples, 1.0);
    let ece_before = compute_ece_15_from_logits(samples, 1.0);

    let mut lo = f64::from(TEMP_MIN).ln();
    let mut hi = f64::from(TEMP_MAX).ln();
    let phi = 0.5 * (5.0_f64.sqrt() - 1.0);

    let mut c = hi - phi * (hi - lo);
    let mut d = lo + phi * (hi - lo);
    let mut fc = mean_nll(samples, c.exp() as f32);
    let mut fd = mean_nll(samples, d.exp() as f32);

    for _ in 0..64 {
        if fc < fd {
            hi = d;
            d = c;
            fd = fc;
            c = hi - phi * (hi - lo);
            fc = mean_nll(samples, c.exp() as f32);
        } else {
            lo = c;
            c = d;
            fc = fd;
            d = lo + phi * (hi - lo);
            fd = mean_nll(samples, d.exp() as f32);
        }
    }

    let mid_t = clamp_temperature((0.5 * (lo + hi)).exp() as f32);
    let nll_after = mean_nll(samples, mid_t);
    let (best_t, best_nll) = if nll_before < nll_after {
        (1.0, nll_before)
    } else {
        (mid_t, nll_after)
    };
    let ece_after = compute_ece_15_from_logits(samples, best_t);

    Ok(TemperatureFitResult {
        temperature: best_t,
        sample_count: samples.len(),
        nll_before,
        nll_after: best_nll,
        ece_before,
        ece_after,
    })
}

fn mean_nll(samples: &[(Vec<f32>, usize)], temperature: f32) -> f64 {
    let mut total = 0.0_f64;
    for (logits, gold) in samples {
        let probs = softmax_scaled(logits, temperature);
        let p = probs.get(*gold).copied().unwrap_or(0.0).max(1e-7);
        total -= f64::from(p).ln();
    }
    total / (samples.len() as f64)
}

/// Computes 15-bin Expected Calibration Error (`ECE_15`) on top-1 predictions (`RTE-061`, `RTE-062`).
#[must_use]
pub fn compute_ece_15_from_logits(samples: &[(Vec<f32>, usize)], temperature: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut bin_counts = [0usize; 15];
    let mut bin_conf_sum = [0.0_f64; 15];
    let mut bin_acc_sum = [0.0_f64; 15];

    for (logits, gold) in samples {
        let probs = softmax_scaled(logits, temperature);
        if let Some((pred_idx, &conf)) = probs
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(&a.0)))
        {
            let bin = (((f64::from(conf) * 15.0).floor() as isize).clamp(0, 14)) as usize;
            bin_counts[bin] += 1;
            bin_conf_sum[bin] += f64::from(conf);
            if pred_idx == *gold {
                bin_acc_sum[bin] += 1.0;
            }
        }
    }

    let n = samples.len() as f64;
    let mut ece = 0.0_f64;
    for b in 0..15 {
        let count = bin_counts[b];
        if count > 0 {
            let avg_conf = bin_conf_sum[b] / (count as f64);
            let avg_acc = bin_acc_sum[b] / (count as f64);
            ece += ((count as f64) / n) * (avg_acc - avg_conf).abs();
        }
    }
    ece as f32
}

/// Split-conformal fit result for one `(bucket, alpha)` slice (`RTE-064`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConformalQuantile {
    /// Target miscoverage rate `alpha`.
    pub alpha: f32,
    /// Sample count evaluated.
    pub sample_count: usize,
    /// Mode selected (`Conformal` when `sample_count >= 500`, else `Fallback`).
    pub mode: CalibrationMode,
    /// Quantile threshold `q_hat` (`Some` iff `mode == Conformal`).
    pub q_hat: Option<f32>,
}

/// Computes split-conformal Least Ambiguous Classifier (LAC) quantile `q_hat` (`RTE-064`).
///
/// Requires `prob_samples.len() >= 500` for [`CalibrationMode::Conformal`]; otherwise emits
/// [`CalibrationMode::Fallback`].
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if `alpha` is outside `(0, 1)` or sample indices are invalid.
pub fn fit_conformal_lac(
    prob_samples: &[(Vec<f32>, usize)],
    alpha: f32,
) -> GapResult<ConformalQuantile> {
    if !alpha.is_finite() || alpha <= 0.0 || alpha >= 1.0 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "conformal alpha must lie strictly inside (0.0, 1.0)",
        ));
    }
    if prob_samples.len() < MIN_CONFORMAL_SAMPLES {
        return Ok(ConformalQuantile {
            alpha,
            sample_count: prob_samples.len(),
            mode: CalibrationMode::Fallback,
            q_hat: None,
        });
    }

    let mut scores = Vec::with_capacity(prob_samples.len());
    for (probs, gold) in prob_samples {
        let Some(&p_gold) = probs.get(*gold) else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "conformal sample gold index out of bounds",
            ));
        };
        let p = if p_gold.is_finite() {
            p_gold.clamp(0.0, 1.0)
        } else {
            0.0
        };
        scores.push(1.0 - p);
    }

    scores.sort_by(f32::total_cmp);
    let n = scores.len();
    let rank_1based = (((n + 1) as f64) * (1.0 - f64::from(alpha))).ceil() as usize;
    let idx = rank_1based.clamp(1, n) - 1;
    let q_hat = scores.get(idx).copied().unwrap_or(1.0);

    Ok(ConformalQuantile {
        alpha,
        sample_count: n,
        mode: CalibrationMode::Conformal,
        q_hat: Some(q_hat),
    })
}

/// Outcome of split-conformal / fallback prediction set construction (`RTE-065`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConformalDecision {
    /// Discovery outcome (`Resolved`, `Ambiguous`, or `Abstained`).
    pub outcome: DiscoveryOutcome,
    /// Structured abstention reason when `outcome == Abstained`.
    pub abstention_reason: Option<AbstentionReason>,
    /// Selected option indices into `options` (excluding sink), ordered by probability desc.
    pub selected_indices: Vec<usize>,
    /// True size of non-sink prediction set `|C(x) \ {sink}|` prior to `max_candidates` truncation.
    pub prediction_set_size: usize,
    /// Whether `selected_indices` was truncated to `max_candidates`.
    pub truncated: bool,
    /// Calibration mode actually used for this decision.
    pub mode: CalibrationMode,
}

/// Evaluates `RTE-065` split-conformal decision sets `C(x) = { o : 1 - p_o <= q_hat }`,
/// or `RTE-064` fallback `max p >= threshold_b` when `q_hat` is `None`.
#[must_use]
pub fn conformal_decide(
    probs: &[f32],
    opts: &[GroupOption],
    bucket: OptionsBucket,
    q_hat: Option<f32>,
    max_candidates: usize,
) -> ConformalDecision {
    let cap = max_candidates.clamp(1, 10);
    if probs.is_empty() || probs.len() != opts.len() {
        return ConformalDecision {
            outcome: DiscoveryOutcome::Abstained,
            abstention_reason: Some(AbstentionReason::LowConfidence),
            selected_indices: Vec::new(),
            prediction_set_size: 0,
            truncated: false,
            mode: CalibrationMode::Fallback,
        };
    }

    let sink_idx = opts.iter().position(|o| o.key == SINK_OPTION_KEY);

    if let Some(q) = q_hat {
        let mut in_set: Vec<usize> = Vec::new();
        let mut sink_in_set = false;
        for (idx, &p) in probs.iter().enumerate() {
            if 1.0 - p <= q {
                if Some(idx) == sink_idx {
                    sink_in_set = true;
                } else {
                    in_set.push(idx);
                }
            }
        }

        if in_set.is_empty() {
            let reason = if sink_in_set {
                AbstentionReason::NoMatch
            } else {
                AbstentionReason::LowConfidence
            };
            return ConformalDecision {
                outcome: DiscoveryOutcome::Abstained,
                abstention_reason: Some(reason),
                selected_indices: Vec::new(),
                prediction_set_size: 0,
                truncated: false,
                mode: CalibrationMode::Conformal,
            };
        }

        in_set.sort_by(|&a, &b| {
            let pa = probs.get(a).copied().unwrap_or(0.0);
            let pb = probs.get(b).copied().unwrap_or(0.0);
            cmp_f32_desc(pa, pb).then_with(|| {
                let ka = opts.get(a).map_or("", |o| o.key.as_str());
                let kb = opts.get(b).map_or("", |o| o.key.as_str());
                ka.cmp(kb)
            })
        });

        let total_set_size = in_set.len();
        if total_set_size == 1 && !sink_in_set {
            return ConformalDecision {
                outcome: DiscoveryOutcome::Resolved,
                abstention_reason: None,
                selected_indices: in_set,
                prediction_set_size: 1,
                truncated: false,
                mode: CalibrationMode::Conformal,
            };
        }

        let truncated = total_set_size > cap;
        in_set.truncate(cap);
        return ConformalDecision {
            outcome: DiscoveryOutcome::Ambiguous,
            abstention_reason: None,
            selected_indices: in_set,
            prediction_set_size: total_set_size,
            truncated,
            mode: CalibrationMode::Conformal,
        };
    }

    // Small-sample / uncalibrated fallback using bucket `max p` thresholds (`RTE-064`).
    let threshold = bucket.fallback_threshold();
    let sink_p = sink_idx.and_then(|i| probs.get(i).copied()).unwrap_or(0.0);

    let mut non_sink_ranked: Vec<usize> = opts
        .iter()
        .enumerate()
        .filter_map(|(idx, o)| (o.key != SINK_OPTION_KEY).then_some(idx))
        .collect();
    non_sink_ranked.sort_by(|&a, &b| {
        let pa = probs.get(a).copied().unwrap_or(0.0);
        let pb = probs.get(b).copied().unwrap_or(0.0);
        cmp_f32_desc(pa, pb).then_with(|| {
            let ka = opts.get(a).map_or("", |o| o.key.as_str());
            let kb = opts.get(b).map_or("", |o| o.key.as_str());
            ka.cmp(kb)
        })
    });

    let Some(&best_idx) = non_sink_ranked.first() else {
        return ConformalDecision {
            outcome: DiscoveryOutcome::Abstained,
            abstention_reason: Some(AbstentionReason::NoMatch),
            selected_indices: Vec::new(),
            prediction_set_size: 0,
            truncated: false,
            mode: CalibrationMode::Fallback,
        };
    };
    let best_p = probs.get(best_idx).copied().unwrap_or(0.0);

    if sink_p >= best_p {
        return ConformalDecision {
            outcome: DiscoveryOutcome::Abstained,
            abstention_reason: Some(AbstentionReason::NoMatch),
            selected_indices: Vec::new(),
            prediction_set_size: 0,
            truncated: false,
            mode: CalibrationMode::Fallback,
        };
    }

    if best_p < threshold {
        // When candidate priors remain distinct and positive below the single-winner threshold,
        // inspect whether any ambiguity candidate exists or abstain cleanly.
        return ConformalDecision {
            outcome: DiscoveryOutcome::Abstained,
            abstention_reason: Some(AbstentionReason::LowConfidence),
            selected_indices: Vec::new(),
            prediction_set_size: 0,
            truncated: false,
            mode: CalibrationMode::Fallback,
        };
    }

    // Check near-tie ambiguity within threshold margin (`0.05`).
    let above_runner_up: Vec<usize> = non_sink_ranked
        .iter()
        .copied()
        .filter(|&idx| {
            let p = probs.get(idx).copied().unwrap_or(0.0);
            idx == best_idx || (best_p - p) <= 0.05
        })
        .collect();

    if above_runner_up.len() > 1 {
        let total = above_runner_up.len();
        let mut chosen = above_runner_up;
        let truncated = total > cap;
        chosen.truncate(cap);
        ConformalDecision {
            outcome: DiscoveryOutcome::Ambiguous,
            abstention_reason: None,
            selected_indices: chosen,
            prediction_set_size: total,
            truncated,
            mode: CalibrationMode::Fallback,
        }
    } else {
        ConformalDecision {
            outcome: DiscoveryOutcome::Resolved,
            abstention_reason: None,
            selected_indices: vec![best_idx],
            prediction_set_size: 1,
            truncated: false,
            mode: CalibrationMode::Fallback,
        }
    }
}

/// Composite version key binding a `CalibrationTable` to its exact router model, embedding model,
/// and option prompt format version (`RTE-067`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CalibrationVersionKey {
    /// Active routing model identifier (`"baseline"` or `"s1:<model_id>"`).
    pub router_model_id: String,
    /// Pinned snapshot embedding model identifier (`None` rendered as `"none"`).
    pub embedding_model_id: Option<String>,
    /// Option serialization format version (`crate::s1_adapter::OPTION_FORMAT_VERSION`).
    pub option_format_version: u32,
}

impl CalibrationVersionKey {
    /// Encodes this tuple deterministically for [`CalibrationTable::model_id`] (`RTE-067`).
    #[must_use]
    pub fn encode_binding(&self) -> String {
        let embed = self.embedding_model_id.as_deref().unwrap_or("none");
        format!(
            "fmt={}|router={}|embed={}",
            self.option_format_version, self.router_model_id, embed
        )
    }

    /// Computes a stable digest over the version key components (`RTE-067`).
    #[must_use]
    pub fn digest_hex(&self) -> String {
        sha256_hex(self.encode_binding().as_bytes())
    }
}

/// Full per-bucket calibrated parameters built offline or via scheduled recalibration (`RTE-062`,
/// `RTE-064`, `RTE-067`, `RTE-068`).
#[derive(Debug, Clone, PartialEq)]
pub struct VersionedCalibrationRecord {
    /// Deterministic SHA-256 calibration identifier (`RTE-067`).
    pub calibration_id: String,
    /// Bound version key (`router_model_id`, `embedding_model_id`, `option_format_version`).
    pub version_key: CalibrationVersionKey,
    /// Temperature per bucket family (`[0.5, 5.0]`).
    pub temperatures: BTreeMap<OptionsBucket, f32>,
    /// Fitted split-conformal `q_hat` quantiles keyed by `(bucket, alpha_milli)` (`50` = 0.05, `100` = 0.10).
    pub conformal_q_hat: BTreeMap<(OptionsBucket, u16), f32>,
    /// Calibration sample count.
    pub sample_count: u64,
}

impl VersionedCalibrationRecord {
    /// Compiles a deterministic [`Self`] from temperatures and conformal quantiles (`RTE-067`).
    #[must_use]
    pub fn new(
        version_key: CalibrationVersionKey,
        temperatures: BTreeMap<OptionsBucket, f32>,
        conformal_q_hat: BTreeMap<(OptionsBucket, u16), f32>,
        sample_count: u64,
    ) -> Self {
        let mut canonical = String::new();
        canonical.push_str(&version_key.encode_binding());
        canonical.push('\n');
        for bucket in OptionsBucket::ALL {
            let t = clamp_temperature(temperatures.get(&bucket).copied().unwrap_or(1.0));
            let q05 = conformal_q_hat
                .get(&(bucket, 50))
                .copied()
                .unwrap_or(f32::NAN);
            let q10 = conformal_q_hat
                .get(&(bucket, 100))
                .copied()
                .unwrap_or(f32::NAN);
            canonical.push_str(&format!(
                "{}:{:.6}:{:.6}:{:.6}\n",
                bucket.as_str(),
                t,
                q05,
                q10
            ));
        }
        let calibration_id = sha256_hex(canonical.as_bytes());
        Self {
            calibration_id,
            version_key,
            temperatures,
            conformal_q_hat,
            sample_count,
        }
    }

    /// Packs this record into [`CalibrationTable`] (`gap-core`) for snapshot publication (`RTE-067`).
    ///
    /// Layout in `quantile_scores` (length `12`): for each of the 4 buckets (`0..4`), stores
    /// `[T_b, q_hat(alpha=0.05) or -1.0, q_hat(alpha=0.10) or -1.0]`.
    #[must_use]
    pub fn to_snapshot_calibration(&self) -> CalibrationTable {
        let mut scores = Vec::with_capacity(12);
        for bucket in OptionsBucket::ALL {
            scores.push(clamp_temperature(
                self.temperatures.get(&bucket).copied().unwrap_or(1.0),
            ));
            scores.push(
                self.conformal_q_hat
                    .get(&(bucket, 50))
                    .copied()
                    .unwrap_or(-1.0),
            );
            scores.push(
                self.conformal_q_hat
                    .get(&(bucket, 100))
                    .copied()
                    .unwrap_or(-1.0),
            );
        }
        CalibrationTable {
            model_id: self.version_key.encode_binding(),
            temperature: clamp_temperature(
                self.temperatures
                    .get(&OptionsBucket::ThreeToFive)
                    .copied()
                    .unwrap_or(1.0),
            ),
            abstention_threshold: OptionsBucket::ThreeToFive.fallback_threshold(),
            quantile_scores: scores,
            sample_count: self.sample_count,
        }
    }
}

/// Validates that `table` strictly matches the active `(router_model_id, embedding_model_id,
/// option_format_version)` tuple (`RTE-067`).
#[must_use]
pub fn is_snapshot_calibration_valid(
    table: &CalibrationTable,
    expected: &CalibrationVersionKey,
) -> bool {
    table.model_id == expected.encode_binding()
        && table.temperature.is_finite()
        && table.temperature >= TEMP_MIN
        && table.temperature <= TEMP_MAX
}

/// Computes canonical SHA-256 `calibration_id` over `table` (`RTE-067`).
#[must_use]
pub fn snapshot_calibration_id(table: &CalibrationTable) -> String {
    let mut canonical = format!(
        "{}|{:.6}|{:.6}|{}|",
        table.model_id, table.temperature, table.abstention_threshold, table.sample_count
    );
    for q in &table.quantile_scores {
        canonical.push_str(&format!("{q:.6},"));
    }
    sha256_hex(canonical.as_bytes())
}

/// Resolves `(temperature, optional q_hat)` for a given bucket and `alpha` from a snapshot's
/// validated [`CalibrationTable`].
#[must_use]
pub fn lookup_calibration_params(
    table: &CalibrationTable,
    bucket: OptionsBucket,
    alpha: f32,
) -> (f32, Option<f32>) {
    if table.quantile_scores.len() >= 12 {
        let base = bucket.index() * 3;
        let t = clamp_temperature(
            table
                .quantile_scores
                .get(base)
                .copied()
                .unwrap_or(table.temperature),
        );
        let q_offset = if (alpha - ALPHA_MUTATING).abs() < 1e-4 {
            1
        } else {
            2
        };
        let raw_q = table
            .quantile_scores
            .get(base + q_offset)
            .copied()
            .unwrap_or(-1.0);
        let q_hat = (raw_q.is_finite() && (0.0..=1.0).contains(&raw_q)).then_some(raw_q);
        return (t, q_hat);
    }

    let t = clamp_temperature(table.temperature);
    let q_hat = if !table.quantile_scores.is_empty()
        && table.sample_count >= MIN_CONFORMAL_SAMPLES as u64
    {
        let idx = bucket.index().min(table.quantile_scores.len() - 1);
        let q = table.quantile_scores.get(idx).copied().unwrap_or(-1.0);
        (q.is_finite() && (0.0..=1.0).contains(&q)).then_some(q)
    } else {
        None
    };
    (t, q_hat)
}

/// Formats `side_effect` label helper for diagnostic summaries.
#[must_use]
pub const fn calibration_side_effect_tag(level: SideEffectLevel) -> &'static str {
    side_effect_as_str(level)
}
