//! RouteBench v1 evaluation runner, temperature/conformal calibration fitters, 14 `RTE-061`
//! metrics with 95% bootstrap confidence intervals, reliability diagram, and `RTE-069` Gate G-S1
//! check (`WP-M2-06`).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use gap_core::error::GapError;
use gap_core::ids::{TenantId, TenantScope};
use gap_core::snapshot::{SideEffectLevel, Snapshot};
use gap_core::traits::DiscoveryOutcome;
use gap_ql::AllowAllVisibility;
use gap_route::calibration_job::{
    ALPHA_READ_ONLY, CalibrationMode, ConformalDecision, OptionsBucket,
    alpha_for_side_effect_constraint, conformal_decide, fit_conformal_lac,
    fit_temperature_golden_section, softmax_scaled,
};
use gap_route::groups::{GroupOption, SINK_OPTION_KEY, build_group_options};
use gap_route::s1_adapter::{RankCtx, RoutingModel};
use gap_route::sanitize::SanitizedQuery;
use gap_route::{DEFAULT_RETRIEVAL_MIN_SCORE, DEFAULT_TOP_K_GROUPS};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::dataset::{GoldTarget, RouteBenchDataset, RouteBenchExample, SplitKind, SplitMix64};

/// Number of equal-width bins in reliability diagrams (`RTE-061`).
pub const RELIABILITY_BINS: usize = 15;

/// Minimum required accuracy gain vs baseline for Gate `G-S1` (`RTE-069`).
pub const GATE_GS1_MIN_ACCURACY_GAIN: f64 = 0.03;

/// Maximum permitted ECE_15 for Gate `G-S1` (`RTE-069`).
pub const GATE_GS1_MAX_ECE_15: f64 = 0.05;

/// Conformal coverage relaxation tolerance for Gate `G-S1` (`RTE-069`).
pub const GATE_GS1_COVERAGE_SLACK: f64 = 0.02;

/// Default p95 routing latency SLO in milliseconds (`RTE-069`).
pub const DEFAULT_SLO_P95_MS: f64 = 40.0;

/// Errors raised during RouteBench evaluation.
#[derive(Debug, Error)]
pub enum EvalError {
    /// Underlying core/router error.
    #[error("routing evaluation error: {0}")]
    Gap(#[from] GapError),
}

/// Point estimate paired with a 95% two-sided bootstrap confidence interval (`RTE-061`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricWithCi {
    /// Point estimate on the evaluated split.
    pub estimate: f64,
    /// Lower 2.5th bootstrap percentile.
    pub ci_low_95: f64,
    /// Upper 97.5th bootstrap percentile.
    pub ci_high_95: f64,
}

impl MetricWithCi {
    /// Helper for synthetic unit-test construction or degenerate fixed values.
    #[must_use]
    pub const fn fixed(value: f64) -> Self {
        Self {
            estimate: value,
            ci_low_95: value,
            ci_high_95: value,
        }
    }
}

/// Single bin of a 15-bin reliability diagram (`RTE-061`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReliabilityBin {
    /// Zero-based bin index (`0..15`).
    pub bin_index: usize,
    /// Inclusive lower confidence bound.
    pub low: f64,
    /// Exclusive upper confidence bound (inclusive at 1.0 for bin 14).
    pub high: f64,
    /// Sample count falling into this bin.
    pub count: usize,
    /// Mean predicted confidence in this bin.
    pub avg_confidence: f64,
    /// Empirical accuracy in this bin.
    pub avg_accuracy: f64,
}

/// Fitted calibration diagnostics for a single [`OptionsBucket`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BucketCalibrationSummary {
    /// Bucket label (`"two"`, `"three_to_five"`, `"six_to_ten"`, `"eleven_to_sixteen"`).
    pub bucket: String,
    /// Calibration mode (`"conformal"` or `"fallback"`).
    pub mode: String,
    /// Number of `Fit` samples contributing to temperature scaling.
    pub fit_count: usize,
    /// Number of `Calibration` samples contributing to conformal quantile estimation.
    pub cal_count: usize,
    /// Fitted temperature scaling factor `T`.
    pub temperature: f32,
    /// Representative miscoverage `alpha` for summary reporting.
    pub alpha: f32,
    /// Fitted conformal quantile `q_hat` (or `None` in `Fallback` mode when `< 500` samples).
    pub q_hat: Option<f32>,
    /// Pre-temperature 15-bin ECE measured on `Fit` samples.
    pub ece_before: f32,
    /// Post-temperature 15-bin ECE measured on `Fit` samples.
    pub ece_after: f32,
}

/// All 14 `RTE-061` evaluation metrics and 15-bin reliability diagram for one dataset split.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitEvaluation {
    /// Total examples evaluated in this split.
    pub total_examples: usize,
    /// In-domain examples (`GoldTarget::Group`).
    pub in_domain_examples: usize,
    /// Out-of-domain examples (`GoldTarget::InsufficientEvidence`).
    pub ood_examples: usize,
    /// Median discovery + ranking latency in milliseconds.
    pub p50_latency_ms: f64,
    /// 95th-percentile discovery + ranking latency in milliseconds.
    pub p95_latency_ms: f64,
    /// Top-1 capability group accuracy across in-domain examples.
    pub top1: MetricWithCi,
    /// Top-3 capability group recall across in-domain examples.
    pub top3: MetricWithCi,
    /// Mean reciprocal rank across in-domain examples.
    pub mrr: MetricWithCi,
    /// 15-bin Expected Calibration Error (`ECE_15`).
    pub ece_15: MetricWithCi,
    /// Multi-class Brier score across candidate options.
    pub brier: MetricWithCi,
    /// Negative log-likelihood (`NLL`).
    pub nll: MetricWithCi,
    /// Selective accuracy at 50% answering coverage.
    pub selective_accuracy_50: MetricWithCi,
    /// Selective accuracy at 70% answering coverage.
    pub selective_accuracy_70: MetricWithCi,
    /// Selective accuracy at 90% answering coverage.
    pub selective_accuracy_90: MetricWithCi,
    /// Abstention AUROC discriminating examples where abstention is needed.
    pub abstention_auroc: MetricWithCi,
    /// Empirical conformal prediction set coverage (`Y in C(X)`) on in-domain examples.
    pub conformal_empirical_coverage: MetricWithCi,
    /// Mean prediction set cardinality `|C(X)|`.
    pub mean_set_size: MetricWithCi,
    /// Fraction of queries triggering [`DiscoveryOutcome::Ambiguous`].
    pub ambiguity_rate: MetricWithCi,
    /// Precision of [`DiscoveryOutcome::Abstained`] on true OOD examples.
    pub ood_abstention_precision: MetricWithCi,
    /// 15-bin reliability diagram.
    pub reliability_bins: Vec<ReliabilityBin>,
}

/// Gate `G-S1` pass/fail decision criteria (`RTE-069`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GateGS1Evaluation {
    /// Candidate gains `>= +0.03` over baseline in either top-1 accuracy or selective@70.
    pub accuracy_gain_ok: bool,
    /// `ECE_15 <= 0.05`.
    pub ece_ok: bool,
    /// Conformal empirical coverage `>= 1 - alpha - 0.02`.
    pub coverage_ok: bool,
    /// Target minimum empirical coverage (`1 - alpha - 0.02`).
    pub coverage_target: f64,
    /// Latency p95 `<= slo_p95_ms`.
    pub latency_ok: bool,
    /// Configured p95 latency SLO (ms).
    pub slo_p95_ms: f64,
    /// Aggregate conjunction of all four Gate `G-S1` conditions.
    pub overall_pass: bool,
}

/// Full output of running RouteBench calibration + evaluation over a [`RouteBenchDataset`].
#[derive(Debug, Clone)]
pub struct EvaluatedBenchmark {
    /// Per-bucket fitted calibration metadata.
    pub calibration_buckets: Vec<BucketCalibrationSummary>,
    /// Evaluation on the held-out in-domain `Test` split.
    pub test_metrics: SplitEvaluation,
    /// Evaluation on cold-start `HeldOutBranch` operations.
    pub held_out_metrics: SplitEvaluation,
    /// Gate `G-S1` check against SLOs and calibration requirements.
    pub gate_gs1: GateGS1Evaluation,
}

/// Pre-scored single-example observation decoupled from routing execution for fast metric +
/// bootstrap calculations (`RTE-061`).
#[derive(Debug, Clone)]
pub struct ScoredPrediction {
    /// Whether ground truth expects an in-domain group (`true`) or `insufficient_evidence` (`false`).
    pub is_in_domain: bool,
    /// 1-based rank of the gold group among candidate groups (`None` if unretrieved or OOD).
    pub group_rank_1based: Option<usize>,
    /// Predicted probability assigned to the true target option (`0.0` if gold group omitted).
    pub p_gold: f64,
    /// Sum of squared probabilities of all non-gold candidate options (`sum_{j != gold} p_j^2`),
    /// used directly in multi-class Brier: `brier = sum_non_gold_sq + (p_gold - 1)^2`.
    pub brier_score: f64,
    /// Maximum calibrated probability across all options (including sink).
    pub max_prob: f64,
    /// Whether overall top-1 argmax (including sink for OOD) matched ground truth.
    pub argmax_correct: bool,
    /// Confidence score for selective non-sink answering (`max_non_sink_prob`).
    pub selective_confidence: f64,
    /// Whether top-1 non-sink group matches `gold_group` (always `false` for OOD).
    pub top1_group_correct: bool,
    /// Continuous score favoring abstention (`1.0 - max_non_sink_prob`).
    pub abstention_score: f64,
    /// Conformal decision outcome.
    pub conformal_outcome: DiscoveryOutcome,
    /// Whether conformal output covered ground truth (`Y in C(X)` for in-domain).
    pub conformal_covered: bool,
    /// Conformal prediction set size.
    pub prediction_set_size: usize,
    /// Measured per-example latency in milliseconds.
    pub latency_ms: f64,
}

struct RawRankedExample {
    bucket: OptionsBucket,
    is_in_domain: bool,
    gold_opt_idx: Option<usize>,
    group_rank_1based: Option<usize>,
    opts: Vec<GroupOption>,
    logits: Vec<f32>,
    top1_group_correct: bool,
    alpha: f32,
    latency_ms: f64,
}

struct FittedCalibration {
    temperatures: BTreeMap<OptionsBucket, f32>,
    quantiles: BTreeMap<(OptionsBucket, u16), Option<f32>>,
    summaries: Vec<BucketCalibrationSummary>,
}

fn poll_sync<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut pinned = std::pin::pin!(fut);
    loop {
        match Pin::as_mut(&mut pinned).poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Evaluates `model` (typically [`gap_route::s1_adapter::BaselineRouter`]) on `dataset`
/// (`RTE-060`, `RTE-061`, `RTE-069`).
pub fn evaluate_dataset(
    dataset: &RouteBenchDataset,
    model: &dyn RoutingModel,
    seed: u64,
    bootstrap_iters: usize,
    baseline_reference: Option<&SplitEvaluation>,
) -> Result<EvaluatedBenchmark, EvalError> {
    let fit_raw = score_split(
        dataset.examples_for(SplitKind::Fit),
        &dataset.in_domain_snapshot,
        model,
    )?;
    let cal_raw = score_split(
        dataset.examples_for(SplitKind::Calibration),
        &dataset.in_domain_snapshot,
        model,
    )?;
    let test_raw = score_split(
        dataset.examples_for(SplitKind::Test),
        &dataset.in_domain_snapshot,
        model,
    )?;
    let held_out_raw = score_split(
        dataset.examples_for(SplitKind::HeldOutBranch),
        &dataset.full_snapshot,
        model,
    )?;

    let calibration = fit_per_bucket_calibration(&fit_raw, &cal_raw)?;

    let test_preds = apply_calibration(&test_raw, &calibration);
    let held_out_preds = apply_calibration(&held_out_raw, &calibration);

    let test_metrics = compute_split_evaluation(&test_preds, seed ^ 0x1111_2222, bootstrap_iters);
    let held_out_metrics =
        compute_split_evaluation(&held_out_preds, seed ^ 0x3333_4444, bootstrap_iters);

    let mean_alpha = if test_raw.is_empty() {
        ALPHA_READ_ONLY as f64
    } else {
        test_raw.iter().map(|r| r.alpha as f64).sum::<f64>() / (test_raw.len() as f64)
    };
    let gate_gs1 = evaluate_gate_gs1(
        &test_metrics,
        baseline_reference,
        mean_alpha,
        DEFAULT_SLO_P95_MS,
    );

    Ok(EvaluatedBenchmark {
        calibration_buckets: calibration.summaries,
        test_metrics,
        held_out_metrics,
        gate_gs1,
    })
}

fn bucket_for_option_count(len: usize) -> OptionsBucket {
    match len {
        0..=2 => OptionsBucket::Two,
        3..=5 => OptionsBucket::ThreeToFive,
        6..=10 => OptionsBucket::SixToTen,
        _ => OptionsBucket::ElevenToSixteen,
    }
}

fn bucket_label(bucket: OptionsBucket) -> &'static str {
    match bucket {
        OptionsBucket::Two => "2",
        OptionsBucket::ThreeToFive => "3..5",
        OptionsBucket::SixToTen => "6..10",
        OptionsBucket::ElevenToSixteen => "11..16",
    }
}

fn alpha_milli(alpha: f32) -> u16 {
    (alpha * 1000.0).round() as u16
}

fn score_split<'a>(
    examples: impl Iterator<Item = &'a RouteBenchExample>,
    snapshot: &Snapshot,
    model: &dyn RoutingModel,
) -> Result<Vec<RawRankedExample>, EvalError> {
    let scope = TenantScope::new(TenantId::default_tenant());
    let vis = AllowAllVisibility::default();
    let mut out = Vec::new();

    for (idx, ex) in examples.enumerate() {
        let t0 = Instant::now();
        let sanitized = SanitizedQuery::new(&ex.query);
        let hits = if sanitized.is_empty() {
            Vec::new()
        } else {
            snapshot
                .index
                .search(&scope, sanitized.as_str(), None, 64)?
        };
        let filtered_hits: Vec<_> = hits
            .into_iter()
            .filter(|h| h.fused_score >= DEFAULT_RETRIEVAL_MIN_SCORE)
            .collect();
        let (resolved_groups, opts) = build_group_options(
            snapshot,
            &filtered_hits,
            None,
            SideEffectLevel::Irreversible,
            DEFAULT_TOP_K_GROUPS,
            false,
            &vis,
            0.05,
        );
        let ctx = RankCtx {
            max_side_effect: SideEffectLevel::Irreversible,
            request_id: format!("rb-{idx:05}"),
            timeout_ms: 400,
        };
        let ranked = poll_sync(model.rank(&sanitized, &opts, &ctx))?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let logits = if ranked.logits.len() == opts.len() {
            ranked.logits
        } else {
            opts.iter().map(|o| o.prior).collect()
        };
        let probs_raw = softmax_scaled(&logits, 1.0);
        let bucket = bucket_for_option_count(opts.len());
        let has_mutating = resolved_groups
            .iter()
            .any(|rg| !matches!(rg.effective_side_effect, SideEffectLevel::None))
            || !matches!(ex.gold_side_effect, SideEffectLevel::None);
        let alpha = alpha_for_side_effect_constraint(has_mutating);

        let sink_idx = opts
            .iter()
            .position(|o| o.key == SINK_OPTION_KEY)
            .unwrap_or_else(|| opts.len().saturating_sub(1));

        // Rank non-sink candidate groups by raw probability descending.
        let mut group_indices: Vec<usize> = (0..resolved_groups.len()).collect();
        group_indices.sort_by(|&a, &b| {
            let pa = probs_raw.get(a).copied().unwrap_or(0.0);
            let pb = probs_raw.get(b).copied().unwrap_or(0.0);
            pb.total_cmp(&pa).then_with(|| a.cmp(&b))
        });

        let (is_in_domain, gold_opt_idx, group_rank_1based, top1_group_correct) = match &ex.gold {
            GoldTarget::Group(gid) => {
                let opt_pos = resolved_groups
                    .iter()
                    .position(|rg| rg.group_id.as_str() == gid);
                let rank = opt_pos.and_then(|pos| {
                    group_indices
                        .iter()
                        .position(|&candidate| candidate == pos)
                        .map(|r| r + 1)
                });
                let top1 = rank == Some(1);
                (true, opt_pos, rank, top1)
            }
            GoldTarget::InsufficientEvidence => (false, Some(sink_idx), None, false),
        };

        out.push(RawRankedExample {
            bucket,
            is_in_domain,
            gold_opt_idx,
            group_rank_1based,
            opts,
            logits,
            top1_group_correct,
            alpha,
            latency_ms: elapsed_ms.max(ranked.latency.as_secs_f64() * 1000.0),
        });
    }

    Ok(out)
}

fn fit_per_bucket_calibration(
    fit_raw: &[RawRankedExample],
    cal_raw: &[RawRankedExample],
) -> Result<FittedCalibration, EvalError> {
    let mut temperatures = BTreeMap::new();
    let mut quantiles = BTreeMap::new();
    let mut summaries = Vec::with_capacity(OptionsBucket::ALL.len());

    for bucket in OptionsBucket::ALL {
        let fit_bucket: Vec<&RawRankedExample> =
            fit_raw.iter().filter(|r| r.bucket == bucket).collect();
        let fit_samples: Vec<(Vec<f32>, usize)> = fit_bucket
            .iter()
            .filter_map(|r| r.gold_opt_idx.map(|idx| (r.logits.clone(), idx)))
            .collect();

        let (temp, ece_before, ece_after) = if fit_samples.is_empty() {
            (1.0, 0.0, 0.0)
        } else {
            let fit = fit_temperature_golden_section(&fit_samples)?;
            (fit.temperature, fit.ece_before, fit.ece_after)
        };
        temperatures.insert(bucket, temp);

        let cal_bucket: Vec<&RawRankedExample> =
            cal_raw.iter().filter(|r| r.bucket == bucket).collect();
        let mut cal_by_alpha: BTreeMap<u16, Vec<(Vec<f32>, usize)>> = BTreeMap::new();
        for item in &cal_bucket {
            if item.is_in_domain
                && let Some(gold_idx) = item.gold_opt_idx
            {
                let scaled = softmax_scaled(&item.logits, temp);
                cal_by_alpha
                    .entry(alpha_milli(item.alpha))
                    .or_default()
                    .push((scaled, gold_idx));
            }
        }

        let default_alpha_key = alpha_milli(ALPHA_READ_ONLY);
        let mut bucket_mode = CalibrationMode::Fallback;
        let mut rep_q_hat = None;

        for alpha in [ALPHA_READ_ONLY, 0.05_f32] {
            let key = alpha_milli(alpha);
            let samples = cal_by_alpha.get(&key).map_or(&[][..], |v| v.as_slice());
            let fit_res = fit_conformal_lac(samples, alpha)?;
            quantiles.insert((bucket, key), fit_res.q_hat);
            if key == default_alpha_key {
                bucket_mode = fit_res.mode;
                rep_q_hat = fit_res.q_hat;
            }
        }
        let mode_str = match bucket_mode {
            CalibrationMode::Conformal => "conformal",
            CalibrationMode::Fallback => "fallback",
            CalibrationMode::InvalidatedBaseline => "invalidated_baseline",
        };
        summaries.push(BucketCalibrationSummary {
            bucket: bucket_label(bucket).to_owned(),
            mode: mode_str.to_owned(),
            fit_count: fit_samples.len(),
            cal_count: cal_bucket.len(),
            temperature: temp,
            alpha: ALPHA_READ_ONLY,
            q_hat: rep_q_hat,
            ece_before,
            ece_after,
        });
    }

    Ok(FittedCalibration {
        temperatures,
        quantiles,
        summaries,
    })
}

fn apply_calibration(
    raw: &[RawRankedExample],
    calibration: &FittedCalibration,
) -> Vec<ScoredPrediction> {
    raw.iter()
        .map(|r| {
            let temp = calibration
                .temperatures
                .get(&r.bucket)
                .copied()
                .unwrap_or(1.0);
            let probs = softmax_scaled(&r.logits, temp);
            let sink_idx = probs.len().saturating_sub(1);

            let (argmax_idx, max_prob_f32) = probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map_or((0, 0.0), |(i, &p)| (i, p));

            let max_non_sink = if probs.len() > 1 {
                probs[..sink_idx].iter().copied().fold(0.0_f32, f32::max) as f64
            } else {
                0.0
            };

            let p_gold = r
                .gold_opt_idx
                .and_then(|idx| probs.get(idx).copied())
                .unwrap_or(0.0) as f64;
            let brier_score = match r.gold_opt_idx {
                Some(gold) => probs
                    .iter()
                    .enumerate()
                    .map(|(j, &p)| {
                        let target = if j == gold { 1.0_f64 } else { 0.0_f64 };
                        let diff = (p as f64) - target;
                        diff * diff
                    })
                    .sum::<f64>(),
                None => probs.iter().map(|&p| (p as f64) * (p as f64)).sum::<f64>() + 1.0,
            };

            let q_hat = calibration
                .quantiles
                .get(&(r.bucket, alpha_milli(r.alpha)))
                .copied()
                .flatten();
            let decision: ConformalDecision = conformal_decide(&probs, &r.opts, r.bucket, q_hat, 6);

            let conformal_covered = if r.is_in_domain {
                r.gold_opt_idx
                    .is_some_and(|idx| decision.selected_indices.contains(&idx))
            } else {
                decision.outcome == DiscoveryOutcome::Abstained
            };

            ScoredPrediction {
                is_in_domain: r.is_in_domain,
                group_rank_1based: r.group_rank_1based,
                p_gold,
                brier_score,
                max_prob: max_prob_f32 as f64,
                argmax_correct: r.gold_opt_idx == Some(argmax_idx),
                selective_confidence: max_non_sink,
                top1_group_correct: r.top1_group_correct,
                abstention_score: (1.0 - max_non_sink).clamp(0.0, 1.0),
                conformal_outcome: decision.outcome,
                conformal_covered,
                prediction_set_size: decision.prediction_set_size,
                latency_ms: r.latency_ms,
            }
        })
        .collect()
}

/// Computes all 14 `RTE-061` metrics, 95% bootstrap CIs, and 15-bin reliability diagram over
/// pre-scored predictions.
#[must_use]
pub fn compute_split_evaluation(
    preds: &[ScoredPrediction],
    seed: u64,
    bootstrap_iters: usize,
) -> SplitEvaluation {
    let total_examples = preds.len();
    let in_domain_examples = preds.iter().filter(|p| p.is_in_domain).count();
    let ood_examples = total_examples.saturating_sub(in_domain_examples);

    let mut latencies: Vec<f64> = preds.iter().map(|p| p.latency_ms).collect();
    latencies.sort_by(f64::total_cmp);
    let p50_latency_ms = percentile_sorted(&latencies, 0.50);
    let p95_latency_ms = percentile_sorted(&latencies, 0.95);

    let point = summarize_slice(preds);
    let (_, reliability_bins) = compute_ece_and_bins(
        &preds
            .iter()
            .map(|p| (p.max_prob, p.argmax_correct))
            .collect::<Vec<_>>(),
    );

    if preds.is_empty() || bootstrap_iters == 0 {
        return SplitEvaluation {
            total_examples,
            in_domain_examples,
            ood_examples,
            p50_latency_ms,
            p95_latency_ms,
            top1: MetricWithCi::fixed(point.top1),
            top3: MetricWithCi::fixed(point.top3),
            mrr: MetricWithCi::fixed(point.mrr),
            ece_15: MetricWithCi::fixed(point.ece_15),
            brier: MetricWithCi::fixed(point.brier),
            nll: MetricWithCi::fixed(point.nll),
            selective_accuracy_50: MetricWithCi::fixed(point.sel50),
            selective_accuracy_70: MetricWithCi::fixed(point.sel70),
            selective_accuracy_90: MetricWithCi::fixed(point.sel90),
            abstention_auroc: MetricWithCi::fixed(point.auroc),
            conformal_empirical_coverage: MetricWithCi::fixed(point.coverage),
            mean_set_size: MetricWithCi::fixed(point.set_size),
            ambiguity_rate: MetricWithCi::fixed(point.ambiguity),
            ood_abstention_precision: MetricWithCi::fixed(point.ood_prec),
            reliability_bins,
        };
    }

    let mut rng = SplitMix64(seed);
    let mut b_top1 = Vec::with_capacity(bootstrap_iters);
    let mut b_top3 = Vec::with_capacity(bootstrap_iters);
    let mut b_mrr = Vec::with_capacity(bootstrap_iters);
    let mut b_ece = Vec::with_capacity(bootstrap_iters);
    let mut b_brier = Vec::with_capacity(bootstrap_iters);
    let mut b_nll = Vec::with_capacity(bootstrap_iters);
    let mut b_sel50 = Vec::with_capacity(bootstrap_iters);
    let mut b_sel70 = Vec::with_capacity(bootstrap_iters);
    let mut b_sel90 = Vec::with_capacity(bootstrap_iters);
    let mut b_auroc = Vec::with_capacity(bootstrap_iters);
    let mut b_cov = Vec::with_capacity(bootstrap_iters);
    let mut b_size = Vec::with_capacity(bootstrap_iters);
    let mut b_amb = Vec::with_capacity(bootstrap_iters);
    let mut b_ood = Vec::with_capacity(bootstrap_iters);

    let mut sample: Vec<ScoredPrediction> = Vec::with_capacity(preds.len());
    for _ in 0..bootstrap_iters {
        sample.clear();
        for _ in 0..preds.len() {
            let pick = rng.next_usize(preds.len());
            sample.push(preds[pick].clone());
        }
        let s = summarize_slice(&sample);
        b_top1.push(s.top1);
        b_top3.push(s.top3);
        b_mrr.push(s.mrr);
        b_ece.push(s.ece_15);
        b_brier.push(s.brier);
        b_nll.push(s.nll);
        b_sel50.push(s.sel50);
        b_sel70.push(s.sel70);
        b_sel90.push(s.sel90);
        b_auroc.push(s.auroc);
        b_cov.push(s.coverage);
        b_size.push(s.set_size);
        b_amb.push(s.ambiguity);
        b_ood.push(s.ood_prec);
    }

    SplitEvaluation {
        total_examples,
        in_domain_examples,
        ood_examples,
        p50_latency_ms,
        p95_latency_ms,
        top1: make_ci(point.top1, b_top1),
        top3: make_ci(point.top3, b_top3),
        mrr: make_ci(point.mrr, b_mrr),
        ece_15: make_ci(point.ece_15, b_ece),
        brier: make_ci(point.brier, b_brier),
        nll: make_ci(point.nll, b_nll),
        selective_accuracy_50: make_ci(point.sel50, b_sel50),
        selective_accuracy_70: make_ci(point.sel70, b_sel70),
        selective_accuracy_90: make_ci(point.sel90, b_sel90),
        abstention_auroc: make_ci(point.auroc, b_auroc),
        conformal_empirical_coverage: make_ci(point.coverage, b_cov),
        mean_set_size: make_ci(point.set_size, b_size),
        ambiguity_rate: make_ci(point.ambiguity, b_amb),
        ood_abstention_precision: make_ci(point.ood_prec, b_ood),
        reliability_bins,
    }
}

/// Evaluates Gate `G-S1` promotion criteria (`RTE-069`).
///
/// When `baseline_reference` is `None` (evaluating the standalone baseline itself),
/// `accuracy_gain_ok` evaluates to `false` until compared against the baseline run.
#[must_use]
pub fn evaluate_gate_gs1(
    candidate: &SplitEvaluation,
    baseline_reference: Option<&SplitEvaluation>,
    mean_alpha: f64,
    slo_p95_ms: f64,
) -> GateGS1Evaluation {
    let accuracy_gain_ok = baseline_reference.is_some_and(|base| {
        let top1_gain = candidate.top1.estimate - base.top1.estimate;
        let sel70_gain =
            candidate.selective_accuracy_70.estimate - base.selective_accuracy_70.estimate;
        top1_gain >= GATE_GS1_MIN_ACCURACY_GAIN || sel70_gain >= GATE_GS1_MIN_ACCURACY_GAIN
    });
    let ece_ok = candidate.ece_15.estimate <= GATE_GS1_MAX_ECE_15;
    let coverage_target = (1.0 - mean_alpha - GATE_GS1_COVERAGE_SLACK).clamp(0.0, 1.0);
    let coverage_ok = candidate.conformal_empirical_coverage.estimate >= coverage_target;
    let latency_ok = candidate.p95_latency_ms <= slo_p95_ms;
    let overall_pass = accuracy_gain_ok && ece_ok && coverage_ok && latency_ok;

    GateGS1Evaluation {
        accuracy_gain_ok,
        ece_ok,
        coverage_ok,
        coverage_target,
        latency_ok,
        slo_p95_ms,
        overall_pass,
    }
}

struct ScalarMetrics {
    top1: f64,
    top3: f64,
    mrr: f64,
    ece_15: f64,
    brier: f64,
    nll: f64,
    sel50: f64,
    sel70: f64,
    sel90: f64,
    auroc: f64,
    coverage: f64,
    set_size: f64,
    ambiguity: f64,
    ood_prec: f64,
}

fn summarize_slice(preds: &[ScoredPrediction]) -> ScalarMetrics {
    if preds.is_empty() {
        return ScalarMetrics {
            top1: 0.0,
            top3: 0.0,
            mrr: 0.0,
            ece_15: 0.0,
            brier: 0.0,
            nll: 0.0,
            sel50: 0.0,
            sel70: 0.0,
            sel90: 0.0,
            auroc: 0.5,
            coverage: 0.0,
            set_size: 0.0,
            ambiguity: 0.0,
            ood_prec: 0.0,
        };
    }

    let in_domain: Vec<&ScoredPrediction> = preds.iter().filter(|p| p.is_in_domain).collect();
    let in_n = in_domain.len() as f64;
    let (top1, top3, mrr, coverage) = if in_domain.is_empty() {
        (0.0, 0.0, 0.0, 1.0)
    } else {
        let t1 = in_domain
            .iter()
            .filter(|p| p.group_rank_1based == Some(1))
            .count() as f64
            / in_n;
        let t3 = in_domain
            .iter()
            .filter(|p| p.group_rank_1based.is_some_and(|r| r <= 3))
            .count() as f64
            / in_n;
        let rr_sum: f64 = in_domain
            .iter()
            .filter_map(|p| p.group_rank_1based.map(|r| 1.0 / (r as f64)))
            .sum();
        let cov = in_domain.iter().filter(|p| p.conformal_covered).count() as f64 / in_n;
        (t1, t3, rr_sum / in_n, cov)
    };

    let (ece_15, _) = compute_ece_and_bins(
        &preds
            .iter()
            .map(|p| (p.max_prob, p.argmax_correct))
            .collect::<Vec<_>>(),
    );

    let total_n = preds.len() as f64;
    let brier = preds.iter().map(|p| p.brier_score).sum::<f64>() / total_n;
    let nll = preds
        .iter()
        .map(|p| -p.p_gold.clamp(1e-7, 1.0).ln())
        .sum::<f64>()
        / total_n;

    let sel50 = selective_accuracy(preds, 0.50);
    let sel70 = selective_accuracy(preds, 0.70);
    let sel90 = selective_accuracy(preds, 0.90);

    let auroc = compute_abstention_auroc(preds);

    let set_size = preds
        .iter()
        .map(|p| p.prediction_set_size as f64)
        .sum::<f64>()
        / total_n;
    let ambiguity = preds
        .iter()
        .filter(|p| p.conformal_outcome == DiscoveryOutcome::Ambiguous)
        .count() as f64
        / total_n;

    let abstained: Vec<&ScoredPrediction> = preds
        .iter()
        .filter(|p| p.conformal_outcome == DiscoveryOutcome::Abstained)
        .collect();
    let ood_prec = if abstained.is_empty() {
        0.0
    } else {
        let ood_tp = abstained.iter().filter(|p| !p.is_in_domain).count() as f64;
        ood_tp / (abstained.len() as f64)
    };

    ScalarMetrics {
        top1,
        top3,
        mrr,
        ece_15,
        brier,
        nll,
        sel50,
        sel70,
        sel90,
        auroc,
        coverage,
        set_size,
        ambiguity,
        ood_prec,
    }
}

fn selective_accuracy(preds: &[ScoredPrediction], coverage: f64) -> f64 {
    if preds.is_empty() {
        return 0.0;
    }
    let mut ranked: Vec<&ScoredPrediction> = preds.iter().collect();
    ranked.sort_by(|a, b| b.selective_confidence.total_cmp(&a.selective_confidence));
    let k = ((preds.len() as f64) * coverage).ceil() as usize;
    let take_k = k.clamp(1, preds.len());
    let correct = ranked[..take_k]
        .iter()
        .filter(|p| p.is_in_domain && p.top1_group_correct)
        .count();
    (correct as f64) / (take_k as f64)
}

fn compute_abstention_auroc(preds: &[ScoredPrediction]) -> f64 {
    // Prefer distinguishing explicit OOD examples when present; otherwise use misclassification.
    let has_ood = preds.iter().any(|p| !p.is_in_domain);
    let pairs: Vec<(f64, bool)> = if has_ood {
        preds
            .iter()
            .map(|p| (p.abstention_score, !p.is_in_domain))
            .collect()
    } else {
        preds
            .iter()
            .map(|p| (p.abstention_score, !p.top1_group_correct))
            .collect()
    };

    let pos: Vec<f64> = pairs.iter().filter(|&&(_, y)| y).map(|&(s, _)| s).collect();
    let neg: Vec<f64> = pairs
        .iter()
        .filter(|&&(_, y)| !y)
        .map(|&(s, _)| s)
        .collect();
    if pos.is_empty() || neg.is_empty() {
        return 1.0;
    }

    let mut wins = 0.0_f64;
    for &p_score in &pos {
        for &n_score in &neg {
            if p_score > n_score {
                wins += 1.0;
            } else if (p_score - n_score).abs() <= f64::EPSILON {
                wins += 0.5;
            }
        }
    }
    wins / ((pos.len() * neg.len()) as f64)
}

fn compute_ece_and_bins(pairs: &[(f64, bool)]) -> (f64, Vec<ReliabilityBin>) {
    let mut sums_conf = [0.0_f64; RELIABILITY_BINS];
    let mut sums_acc = [0.0_f64; RELIABILITY_BINS];
    let mut counts = [0usize; RELIABILITY_BINS];

    for &(conf_raw, correct) in pairs {
        let conf = conf_raw.clamp(0.0, 1.0);
        let raw_bin = (conf * (RELIABILITY_BINS as f64)).floor() as usize;
        let bin = raw_bin.min(RELIABILITY_BINS - 1);
        counts[bin] += 1;
        sums_conf[bin] += conf;
        if correct {
            sums_acc[bin] += 1.0;
        }
    }

    let total = pairs.len().max(1) as f64;
    let mut ece = 0.0_f64;
    let mut bins = Vec::with_capacity(RELIABILITY_BINS);

    for i in 0..RELIABILITY_BINS {
        let low = (i as f64) / (RELIABILITY_BINS as f64);
        let high = ((i + 1) as f64) / (RELIABILITY_BINS as f64);
        let c = counts[i];
        let (avg_conf, avg_acc) = if c == 0 {
            (0.0, 0.0)
        } else {
            (sums_conf[i] / (c as f64), sums_acc[i] / (c as f64))
        };
        if c > 0 {
            ece += ((c as f64) / total) * (avg_acc - avg_conf).abs();
        }
        bins.push(ReliabilityBin {
            bin_index: i,
            low,
            high,
            count: c,
            avg_confidence: avg_conf,
            avg_accuracy: avg_acc,
        });
    }

    (ece, bins)
}

fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn make_ci(estimate: f64, mut boot: Vec<f64>) -> MetricWithCi {
    if boot.is_empty() {
        return MetricWithCi::fixed(estimate);
    }
    boot.sort_by(f64::total_cmp);
    let low = percentile_sorted(&boot, 0.025);
    let high = percentile_sorted(&boot, 0.975);
    MetricWithCi {
        estimate,
        ci_low_95: low,
        ci_high_95: high,
    }
}
