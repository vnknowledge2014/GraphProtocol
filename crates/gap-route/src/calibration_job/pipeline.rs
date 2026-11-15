//! Offline calibration dataset pipeline (`Fit` -> `Calibration` -> `Test`), reliability
//! diagrams, winsorized robust NLL option (`TM-14`), and bucketed summary reporting
//! (`RTE-062`, `RTE-064`, `RTE-065`, `RTE-066`, `RTE-067`, `WP-M5-04`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::CalibrationTable;

use crate::groups::{GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT};

use super::{
    ALPHA_MUTATING, ALPHA_READ_ONLY, CalibrationMode, CalibrationVersionKey, OptionsBucket,
    VersionedCalibrationRecord, clamp_temperature, conformal_decide, fit_conformal_lac,
    fit_temperature_golden_section, softmax_scaled,
};

/// Dataset partition assigned to a calibration example without `operation_id` leakage (`RTE-060`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalibrationSplit {
    /// Held-out temperature scaling fit partition (`RTE-062`).
    Fit,
    /// Held-out split-conformal LAC quantile calibration partition (`RTE-064`).
    Calibration,
    /// Disjoint evaluation partition measuring ECE, empirical coverage, and set sizes.
    Test,
}

/// Optional robust logit winsorization to mitigate poisoned feedback outliers (`TM-14`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WinsorizedConfig {
    /// When `Some(cap)` (with `cap > 0.0` finite), clamps logits into `[-cap, cap]` prior to
    /// temperature fitting and calibration scoring.
    pub clip_logit_abs: Option<f32>,
}

impl WinsorizedConfig {
    /// Applies configured symmetric winsorization clip to `logits`.
    #[must_use]
    pub fn apply(self, logits: &[f32]) -> Vec<f32> {
        match self.clip_logit_abs {
            Some(cap) if cap.is_finite() && cap > 0.0 => logits
                .iter()
                .map(|&z| {
                    if z.is_finite() {
                        z.clamp(-cap, cap)
                    } else {
                        0.0
                    }
                })
                .collect(),
            _ => logits
                .iter()
                .map(|&z| if z.is_finite() { z } else { 0.0 })
                .collect(),
        }
    }
}

/// One labeled choice example consumed by [`run_calibration_pipeline`].
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationExample {
    /// Dataset split assignment (`Fit`, `Calibration`, or `Test`).
    pub split: CalibrationSplit,
    /// Raw uncalibrated router logits aligned with the `n_options` presented choices.
    pub logits: Vec<f32>,
    /// Ground-truth choice index inside `logits`.
    pub true_index: usize,
    /// Optional index of the trailing `insufficient_evidence` sink choice if present.
    pub sink_index: Option<usize>,
    /// Whether any eligible option carried `side_effect != NONE` (selects `alpha = 0.05` vs `0.10`, `RTE-066`).
    pub has_mutating_option: bool,
}

/// One bin (`1 / 15` width) in a 15-bin reliability diagram (`RTE-061`, `RTE-062`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReliabilityBin {
    /// Zero-based bin index (`0..15`).
    pub bin_index: usize,
    /// Lower confidence edge inclusive (`bin / 15.0`).
    pub lower: f32,
    /// Upper confidence edge (`(bin + 1) / 15.0`).
    pub upper: f32,
    /// Number of test examples landing in this confidence bin.
    pub count: usize,
    /// Mean predicted top-1 probability in this bin.
    pub mean_confidence: f32,
    /// Empirical top-1 accuracy observed in this bin.
    pub empirical_accuracy: f32,
}

/// Per-bucket offline calibration fit and held-out test metrics (`RTE-062`..`RTE-066`).
#[derive(Debug, Clone, PartialEq)]
pub struct BucketCalibrationSummary {
    /// Option cardinality bucket (`2`, `3-5`, `6-10`, `11-16`).
    pub bucket: OptionsBucket,
    /// Number of `Fit` split samples in this bucket.
    pub fit_count: usize,
    /// Number of `Calibration` split samples in this bucket.
    pub cal_count: usize,
    /// Number of `Test` split samples in this bucket.
    pub test_count: usize,
    /// Fitted clamped scalar temperature in `[0.5, 5.0]` (`1.0` when `fit_count == 0`).
    pub temperature: f32,
    /// NLL on `Fit` before scaling (`T = 1.0`).
    pub fit_nll_before: f64,
    /// NLL on `Fit` after scaling (`T = temperature`).
    pub fit_nll_after: f64,
    /// Active mode at `alpha = 0.10` (`Conformal` when `cal_count >= 500`, else `Fallback`).
    pub mode_read_only: CalibrationMode,
    /// Fitted conformal quantile `q_hat` at `alpha = 0.10` (`None` when `Fallback`).
    pub q_hat_read_only: Option<f32>,
    /// Active mode at `alpha = 0.05`.
    pub mode_mutating: CalibrationMode,
    /// Fitted conformal quantile `q_hat` at `alpha = 0.05` (`None` when `Fallback`).
    pub q_hat_mutating: Option<f32>,
    /// 15-bin ECE on `Test` (or `Fit` when `Test` empty) before temperature scaling.
    pub ece_before: f32,
    /// 15-bin ECE on `Test` (or `Fit` when `Test` empty) after temperature scaling.
    pub ece_after: f32,
    /// Observed empirical coverage on `Test` under `alpha = 0.10`.
    pub empirical_coverage_read_only: Option<f32>,
    /// Observed empirical coverage on `Test` under `alpha = 0.05`.
    pub empirical_coverage_mutating: Option<f32>,
    /// Mean prediction set size `|C(x) \ {sink}|` on `Test` under `alpha = 0.10`.
    pub mean_set_size_read_only: Option<f32>,
    /// Mean prediction set size `|C(x) \ {sink}|` on `Test` under `alpha = 0.05`.
    pub mean_set_size_mutating: Option<f32>,
    /// Abstention rate on `Test` under `alpha = 0.10`.
    pub abstention_rate_read_only: Option<f32>,
    /// Abstention rate on `Test` under `alpha = 0.05`.
    pub abstention_rate_mutating: Option<f32>,
    /// Observed empirical coverage matching each test item's `has_mutating_option` policy (`RTE-066`).
    pub policy_empirical_coverage: Option<f32>,
    /// 15-bin reliability diagram before temperature scaling (`T = 1.0`).
    pub reliability_before: Vec<ReliabilityBin>,
    /// 15-bin reliability diagram after temperature scaling (`T = temperature`).
    pub reliability_after: Vec<ReliabilityBin>,
}

/// Complete report emitted alongside [`VersionedCalibrationRecord`] (`WP-M5-04`).
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationPipelineReport {
    /// Canonical SHA-256 calibration table identifier (`RTE-067`).
    pub calibration_id: String,
    /// Bound version tuple `(router, embedder, option_format_version)` (`RTE-067`).
    pub version_key: CalibrationVersionKey,
    /// Ready-to-publish snapshot [`CalibrationTable`].
    pub table: CalibrationTable,
    /// Total examples consumed across splits.
    pub fit_total: usize,
    /// Total `Calibration` split examples.
    pub cal_total: usize,
    /// Total `Test` split examples.
    pub test_total: usize,
    /// Weighted `ECE_15` before temperature scaling across test (or fit) buckets.
    pub overall_ece_before: f32,
    /// Weighted `ECE_15` after temperature scaling across test (or fit) buckets.
    pub overall_ece_after: f32,
    /// Ordered per-bucket summaries (`2`, `3-5`, `6-10`, `11-16`).
    pub buckets: Vec<BucketCalibrationSummary>,
}

impl CalibrationPipelineReport {
    /// Renders a human-readable Markdown report containing per-bucket calibration parameters,
    /// ECE improvements, conformal coverage vs `1 - alpha`, and reliability diagram summaries.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::with_capacity(2048);
        let _ = writeln!(out, "# GAP S1 Calibration Report");
        let _ = writeln!(out);
        let _ = writeln!(out, "- **Calibration ID:** `{}`", self.calibration_id);
        let _ = writeln!(out, "- **Binding:** `{}`", self.table.model_id);
        let _ = writeln!(
            out,
            "- **Splits:** fit={}, calibration={}, test={}",
            self.fit_total, self.cal_total, self.test_total
        );
        let _ = writeln!(
            out,
            "- **Overall ECE_15:** before=`{:.4}` -> after=`{:.4}`",
            self.overall_ece_before, self.overall_ece_after
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "| Bucket | Fit | Cal | Test | Temp | q_hat(0.10) | q_hat(0.05) | ECE before | ECE after | Cov@0.10 | Cov@0.05 | SetSize@0.10 | Abstain@0.10 |"
        );
        let _ = writeln!(
            out,
            "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for b in &self.buckets {
            let q10 = b
                .q_hat_read_only
                .map_or_else(|| "fallback".to_owned(), |v| format!("{v:.4}"));
            let q05 = b
                .q_hat_mutating
                .map_or_else(|| "fallback".to_owned(), |v| format!("{v:.4}"));
            let cov10 = b
                .empirical_coverage_read_only
                .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.4}"));
            let cov05 = b
                .empirical_coverage_mutating
                .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.4}"));
            let sz10 = b
                .mean_set_size_read_only
                .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.2}"));
            let ab10 = b
                .abstention_rate_read_only
                .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.4}"));
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {:.4} | {} | {} | {:.4} | {:.4} | {} | {} | {} | {} |",
                b.bucket.as_str(),
                b.fit_count,
                b.cal_count,
                b.test_count,
                b.temperature,
                q10,
                q05,
                b.ece_before,
                b.ece_after,
                cov10,
                cov05,
                sz10,
                ab10
            );
        }
        out
    }
}

/// Computes a 15-bin reliability diagram and `ECE_15` over `(logits, gold_idx)` pairs.
#[must_use]
pub fn compute_reliability_diagram(
    samples: &[(Vec<f32>, usize)],
    temperature: f32,
) -> (f32, Vec<ReliabilityBin>) {
    let mut counts = [0usize; 15];
    let mut conf_sums = [0.0_f64; 15];
    let mut acc_sums = [0.0_f64; 15];

    for (logits, gold) in samples {
        let probs = softmax_scaled(logits, temperature);
        if let Some((pred_idx, &conf)) = probs
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(&a.0)))
        {
            let bin = (((f64::from(conf) * 15.0).floor() as isize).clamp(0, 14)) as usize;
            counts[bin] += 1;
            conf_sums[bin] += f64::from(conf);
            if pred_idx == *gold {
                acc_sums[bin] += 1.0;
            }
        }
    }

    let total = samples.len().max(1) as f64;
    let mut bins = Vec::with_capacity(15);
    let mut ece = 0.0_f64;
    for b in 0..15 {
        let c = counts[b];
        let (mean_conf, mean_acc) = if c > 0 {
            let mc = conf_sums[b] / (c as f64);
            let ma = acc_sums[b] / (c as f64);
            if !samples.is_empty() {
                ece += ((c as f64) / total) * (ma - mc).abs();
            }
            (mc as f32, ma as f32)
        } else {
            (0.0, 0.0)
        };
        bins.push(ReliabilityBin {
            bin_index: b,
            lower: (b as f32) / 15.0,
            upper: ((b + 1) as f32) / 15.0,
            count: c,
            mean_confidence: mean_conf,
            empirical_accuracy: mean_acc,
        });
    }
    let ece_checked = if samples.is_empty() { 0.0 } else { ece as f32 };
    (ece_checked, bins)
}

/// Runs the complete per-bucket temperature scaling (`RTE-062`), split-conformal LAC quantile
/// fitting (`RTE-064`..`RTE-066`), and `Test` split evaluation pipeline, returning the versioned
/// snapshot table and full calibration report (`RTE-067`).
///
/// # Errors
///
/// Returns [`GapError`] with [`ErrorCode::ValidationError`] when `examples` is empty or any entry
/// has fewer than `2` options or an out-of-range `true_index`/`sink_index`.
pub fn run_calibration_pipeline(
    version_key: CalibrationVersionKey,
    examples: &[CalibrationExample],
    winsor: WinsorizedConfig,
) -> GapResult<(VersionedCalibrationRecord, CalibrationPipelineReport)> {
    if examples.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "cannot run calibration pipeline on empty dataset",
        ));
    }

    let mut fit_by_bucket: BTreeMap<OptionsBucket, Vec<(Vec<f32>, usize)>> = BTreeMap::new();
    let mut cal_by_bucket: BTreeMap<OptionsBucket, Vec<(Vec<f32>, usize)>> = BTreeMap::new();
    let mut test_by_bucket: BTreeMap<OptionsBucket, Vec<&CalibrationExample>> = BTreeMap::new();

    let mut fit_total = 0usize;
    let mut cal_total = 0usize;
    let mut test_total = 0usize;

    for (row_idx, ex) in examples.iter().enumerate() {
        if ex.logits.len() < 2 || ex.true_index >= ex.logits.len() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "calibration example {row_idx} invalid: len={}, true_index={}",
                    ex.logits.len(),
                    ex.true_index
                ),
            ));
        }
        if let Some(sink_idx) = ex.sink_index
            && sink_idx >= ex.logits.len()
        {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "calibration example {row_idx} sink_index {sink_idx} out of range {}",
                    ex.logits.len()
                ),
            ));
        }
        let bucket = OptionsBucket::from_option_count(ex.logits.len());
        let clipped = winsor.apply(&ex.logits);
        match ex.split {
            CalibrationSplit::Fit => {
                fit_total += 1;
                fit_by_bucket
                    .entry(bucket)
                    .or_default()
                    .push((clipped, ex.true_index));
            }
            CalibrationSplit::Calibration => {
                cal_total += 1;
                cal_by_bucket
                    .entry(bucket)
                    .or_default()
                    .push((clipped, ex.true_index));
            }
            CalibrationSplit::Test => {
                test_total += 1;
                test_by_bucket.entry(bucket).or_default().push(ex);
            }
        }
    }

    let mut temperatures = BTreeMap::new();
    let mut conformal_q_hat = BTreeMap::new();
    let mut summaries = Vec::with_capacity(OptionsBucket::ALL.len());
    let mut weighted_ece_before = 0.0_f64;
    let mut weighted_ece_after = 0.0_f64;
    let mut weight_sum = 0usize;

    for bucket in OptionsBucket::ALL {
        let fit_samples = fit_by_bucket.get(&bucket).map_or(&[][..], Vec::as_slice);
        let (temp, fit_nll_before, fit_nll_after) = if fit_samples.is_empty() {
            (1.0, 0.0, 0.0)
        } else {
            let res = fit_temperature_golden_section(fit_samples)?;
            (res.temperature, res.nll_before, res.nll_after)
        };
        let clamped_t = clamp_temperature(temp);
        temperatures.insert(bucket, clamped_t);

        let cal_logits = cal_by_bucket.get(&bucket).map_or(&[][..], Vec::as_slice);
        let cal_probs: Vec<(Vec<f32>, usize)> = cal_logits
            .iter()
            .map(|(z, gold)| (softmax_scaled(z, clamped_t), *gold))
            .collect();

        let lac_ro = fit_conformal_lac(&cal_probs, ALPHA_READ_ONLY)?;
        let lac_mut = fit_conformal_lac(&cal_probs, ALPHA_MUTATING)?;
        if let Some(q) = lac_ro.q_hat {
            conformal_q_hat.insert((bucket, 100), q);
        }
        if let Some(q) = lac_mut.q_hat {
            conformal_q_hat.insert((bucket, 50), q);
        }

        let test_slice = test_by_bucket.get(&bucket).map_or(&[][..], Vec::as_slice);
        let test_pairs: Vec<(Vec<f32>, usize)> = test_slice
            .iter()
            .map(|ex| (winsor.apply(&ex.logits), ex.true_index))
            .collect();

        let diag_source = if test_pairs.is_empty() {
            fit_samples
        } else {
            &test_pairs
        };
        let (ece_before, reliability_before) = compute_reliability_diagram(diag_source, 1.0);
        let (ece_after, reliability_after) = compute_reliability_diagram(diag_source, clamped_t);

        if !diag_source.is_empty() {
            let w = diag_source.len();
            weight_sum += w;
            weighted_ece_before += (w as f64) * f64::from(ece_before);
            weighted_ece_after += (w as f64) * f64::from(ece_after);
        }

        let eval_ro = evaluate_test_slice(test_slice, clamped_t, bucket, lac_ro.q_hat, winsor);
        let eval_mut = evaluate_test_slice(test_slice, clamped_t, bucket, lac_mut.q_hat, winsor);
        let policy_cov = evaluate_policy_coverage(
            test_slice,
            clamped_t,
            bucket,
            lac_ro.q_hat,
            lac_mut.q_hat,
            winsor,
        );

        summaries.push(BucketCalibrationSummary {
            bucket,
            fit_count: fit_samples.len(),
            cal_count: cal_logits.len(),
            test_count: test_slice.len(),
            temperature: clamped_t,
            fit_nll_before,
            fit_nll_after,
            mode_read_only: lac_ro.mode,
            q_hat_read_only: lac_ro.q_hat,
            mode_mutating: lac_mut.mode,
            q_hat_mutating: lac_mut.q_hat,
            ece_before,
            ece_after,
            empirical_coverage_read_only: eval_ro.map(|m| m.0),
            empirical_coverage_mutating: eval_mut.map(|m| m.0),
            mean_set_size_read_only: eval_ro.map(|m| m.1),
            mean_set_size_mutating: eval_mut.map(|m| m.1),
            abstention_rate_read_only: eval_ro.map(|m| m.2),
            abstention_rate_mutating: eval_mut.map(|m| m.2),
            policy_empirical_coverage: policy_cov,
            reliability_before,
            reliability_after,
        });
    }

    let record = VersionedCalibrationRecord::new(
        version_key.clone(),
        temperatures,
        conformal_q_hat,
        cal_total as u64,
    );
    let table = record.to_snapshot_calibration();

    let (overall_ece_before, overall_ece_after) = if weight_sum > 0 {
        (
            (weighted_ece_before / (weight_sum as f64)) as f32,
            (weighted_ece_after / (weight_sum as f64)) as f32,
        )
    } else {
        (0.0, 0.0)
    };

    let report = CalibrationPipelineReport {
        calibration_id: record.calibration_id.clone(),
        version_key,
        table,
        fit_total,
        cal_total,
        test_total,
        overall_ece_before,
        overall_ece_after,
        buckets: summaries,
    };

    Ok((record, report))
}

fn synthetic_options(len: usize, sink_index: Option<usize>) -> Vec<GroupOption> {
    (0..len)
        .map(|idx| {
            if Some(idx) == sink_index {
                GroupOption {
                    key: SINK_OPTION_KEY.to_owned(),
                    text: SINK_OPTION_TEXT.to_owned(),
                    prior: 0.15,
                }
            } else {
                GroupOption {
                    key: format!("g{idx}"),
                    text: format!("option {idx}"),
                    prior: 0.5,
                }
            }
        })
        .collect()
}

fn evaluate_test_slice(
    test_slice: &[&CalibrationExample],
    temperature: f32,
    bucket: OptionsBucket,
    q_hat: Option<f32>,
    winsor: WinsorizedConfig,
) -> Option<(f32, f32, f32)> {
    if test_slice.is_empty() {
        return None;
    }

    let mut covered = 0usize;
    let mut set_size_sum = 0usize;
    let mut abstained = 0usize;

    for ex in test_slice {
        let clipped = winsor.apply(&ex.logits);
        let probs = softmax_scaled(&clipped, temperature);
        let opts = synthetic_options(probs.len(), ex.sink_index);
        let decision = conformal_decide(&probs, &opts, bucket, q_hat, opts.len().min(10));
        if decision.abstention_reason.is_some() {
            abstained += 1;
        }
        set_size_sum += decision.prediction_set_size;

        let p_true = probs.get(ex.true_index).copied().unwrap_or(0.0);
        let is_covered = match q_hat {
            Some(q) => (1.0 - p_true) <= q,
            None => {
                if Some(ex.true_index) == ex.sink_index {
                    decision.abstention_reason.is_some()
                } else {
                    decision.selected_indices.contains(&ex.true_index)
                }
            }
        };
        if is_covered {
            covered += 1;
        }
    }

    let n = test_slice.len() as f32;
    Some((
        (covered as f32) / n,
        (set_size_sum as f32) / n,
        (abstained as f32) / n,
    ))
}

fn evaluate_policy_coverage(
    test_slice: &[&CalibrationExample],
    temperature: f32,
    bucket: OptionsBucket,
    q_ro: Option<f32>,
    q_mut: Option<f32>,
    winsor: WinsorizedConfig,
) -> Option<f32> {
    if test_slice.is_empty() {
        return None;
    }
    let mut covered = 0usize;
    for ex in test_slice {
        let q_hat = if ex.has_mutating_option { q_mut } else { q_ro };
        let clipped = winsor.apply(&ex.logits);
        let probs = softmax_scaled(&clipped, temperature);
        let p_true = probs.get(ex.true_index).copied().unwrap_or(0.0);
        let is_covered = match q_hat {
            Some(q) => (1.0 - p_true) <= q,
            None => {
                let opts = synthetic_options(probs.len(), ex.sink_index);
                let dec = conformal_decide(&probs, &opts, bucket, None, opts.len().min(10));
                if Some(ex.true_index) == ex.sink_index {
                    dec.abstention_reason.is_some()
                } else {
                    dec.selected_indices.contains(&ex.true_index)
                }
            }
        };
        if is_covered {
            covered += 1;
        }
    }
    Some((covered as f32) / (test_slice.len() as f32))
}
