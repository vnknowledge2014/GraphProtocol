//! System-1 (`LayaCandle`) vs `BaselineRouter` comparison evaluator and `RTE-043` / `RTE-069`
//! decision metrics (`WP-M5-07`, consumed by `WP-M5-08`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use gap_core::error::GapResult;
use gap_core::ids::{TenantId, TenantScope};
use gap_core::snapshot::{SideEffectLevel, Snapshot};
use gap_core::traits::BoxFuture;
use gap_ql::AllowAllVisibility;
use gap_route::calibration_job::{compute_ece_15_from_logits, softmax_scaled};
use gap_route::debias::evaluate_debias_tradeoff;
use gap_route::groups::{GroupOption, SINK_OPTION_KEY, build_group_options};
use gap_route::s1_adapter::{
    BaselineRouter, RankCtx, RankOutput, RoutingModel, S1AdapterConfig, S1RouterAdapter,
};
use gap_route::sanitize::SanitizedQuery;
use gap_route::{DEFAULT_RETRIEVAL_MIN_SCORE, DEFAULT_TOP_K_GROUPS};
use gap_s1::{LayaCandle, LayaTokenizer, Precision};
use serde::{Deserialize, Serialize};

use crate::dataset::{GoldTarget, RouteBenchDataset, RouteBenchExample, SplitKind};
use crate::metrics::{
    BucketCalibrationSummary, EvalError, EvaluatedBenchmark, GateGS1Evaluation, SplitEvaluation,
    evaluate_dataset,
};
use crate::mining::poll_future;

/// Weight source metadata captured in `S1ComparisonReport` (`ADR-021` provenance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeightSourceInfo {
    /// Backend identifier (`s1:laya-candle:f32`, etc.).
    pub model_id: String,
    /// `"checkpoint:<path>"` when external safetensors checkpoint was supplied, or
    /// `"builtin:calibrated_default_f32"` when the built-in deterministic reference weights ran.
    pub source: String,
}

/// Raw uncalibrated (`T = 1.0`) accuracy and latency tail summary on one dataset split.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RawSplitSummary {
    /// Evaluated examples count.
    pub example_count: usize,
    /// Top-1 capability group accuracy across in-domain examples.
    pub top1_accuracy: f64,
    /// Uncalibrated 15-bin Expected Calibration Error (`T = 1.0`).
    pub raw_ece_15: f64,
    /// Median end-to-end discovery latency in milliseconds.
    pub p50_latency_ms: f64,
    /// 95th-percentile end-to-end discovery latency in milliseconds.
    pub p95_latency_ms: f64,
    /// 99th-percentile end-to-end discovery latency in milliseconds.
    pub p99_latency_ms: f64,
}

/// Full serialized model entry (`Baseline`, `S1 forward`, or `S1 debiased`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluatedRouterEntry {
    /// Router identifier.
    pub router_id: String,
    /// Per-bucket fitted calibration diagnostics.
    pub calibration_buckets: Vec<BucketCalibrationSummary>,
    /// Uncalibrated raw `T = 1.0` summary on `Test`.
    pub raw_test: RawSplitSummary,
    /// Calibrated 14 `RTE-061` metrics + 95% CIs on `Test`.
    pub test_metrics: SplitEvaluation,
    /// Calibrated 14 `RTE-061` metrics + 95% CIs on `HeldOutBranch`.
    pub held_out_metrics: SplitEvaluation,
    /// Gate `G-S1` check against `BaselineRouter` (`RTE-069`).
    pub gate_gs1: GateGS1Evaluation,
}

/// Serializable `RTE-043` forward-vs-debiased decision report.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DebiasReportJson {
    /// Forward-only (`debias = false`) Top-1 accuracy.
    pub forward_top1: f64,
    /// Forward+reverse (`debias = true`) Top-1 accuracy.
    pub debiased_top1: f64,
    /// Observed Top-1 accuracy delta.
    pub top1_gain: f64,
    /// Forward-only p95 latency (ms).
    pub forward_p95_ms: f64,
    /// Debiased p95 latency (ms).
    pub debiased_p95_ms: f64,
    /// Latency multiplier (`debiased_p95_ms / forward_p95_ms`).
    pub latency_ratio: f64,
    /// Recommended default for `s1.debias` per `RTE-043`.
    pub enable_by_default: bool,
}

/// Comprehensive S1 vs Baseline evaluation artifact written by `WP-M5-07`/`WP-M5-08`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct S1ComparisonReport {
    /// Seed & bootstrap configuration used.
    pub seed: u64,
    /// Bootstrap resample count.
    pub bootstrap_iters: usize,
    /// Weight provenance metadata.
    pub weights: WeightSourceInfo,
    /// Process resident set size in KiB observed after running S1 forward passes (`0` if unavailable).
    pub process_rss_kib: u64,
    /// `BaselineRouter` calibrated benchmark results.
    pub baseline: EvaluatedRouterEntry,
    /// `S1RouterAdapter` (`debias = false`) results (`raw` + calibrated).
    pub s1_forward: EvaluatedRouterEntry,
    /// `S1RouterAdapter` (`debias = true`) calibrated results.
    pub s1_debiased: EvaluatedRouterEntry,
    /// Data-driven `RTE-043` `s1.debias` tradeoff verdict.
    pub debias_decision: DebiasReportJson,
}

/// Loads `LayaCandle` from `GAP_LAYA_CHECKPOINT` if set and readable, otherwise falls back to
/// deterministic built-in `LayaCandle::calibrated_default(Precision::F32)`.
#[must_use]
pub fn load_s1_candle_for_bench() -> (LayaCandle, WeightSourceInfo) {
    if let Some(raw_path) = std::env::var_os("GAP_LAYA_CHECKPOINT") {
        let path = PathBuf::from(&raw_path);
        if path.exists()
            && let Ok(model) = LayaCandle::from_safetensors_mmap(
                &path,
                LayaTokenizer::builtin_default(),
                Precision::F32,
            )
        {
            let info = WeightSourceInfo {
                model_id: gap_route::DecisionBackend::model_id(&model).to_owned(),
                source: format!("checkpoint:{}", path.display()),
            };
            return (model, info);
        }
    }
    let model = LayaCandle::calibrated_default(Precision::F32);
    let info = WeightSourceInfo {
        model_id: gap_route::DecisionBackend::model_id(&model).to_owned(),
        source: "builtin:calibrated_default_f32".to_owned(),
    };
    (model, info)
}

#[derive(Debug)]
struct BenchTimeoutFloor<M: RoutingModel> {
    inner: M,
}

impl<M: RoutingModel> RoutingModel for BenchTimeoutFloor<M> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn rank<'a>(
        &'a self,
        sanitized_query: &'a SanitizedQuery,
        opts: &'a [GroupOption],
        ctx: &'a RankCtx,
    ) -> BoxFuture<'a, GapResult<RankOutput>> {
        if cfg!(debug_assertions) {
            Box::pin(async move {
                let raised = RankCtx {
                    max_side_effect: ctx.max_side_effect,
                    request_id: ctx.request_id.clone(),
                    timeout_ms: ctx.timeout_ms.max(10_000),
                };
                self.inner.rank(sanitized_query, opts, &raised).await
            })
        } else {
            self.inner.rank(sanitized_query, opts, ctx)
        }
    }
}

/// Evaluates `BaselineRouter`, `S1RouterAdapter` (`debias = false`), and `S1RouterAdapter`
/// (`debias = true`) across `dataset` (`RTE-043`, `RTE-061`, `RTE-069`).
pub fn evaluate_s1_vs_baseline(
    dataset: &RouteBenchDataset,
    seed: u64,
    bootstrap_iters: usize,
) -> Result<S1ComparisonReport, EvalError> {
    let baseline_router = BaselineRouter::default();
    let baseline_eval = evaluate_dataset(dataset, &baseline_router, seed, bootstrap_iters, None)?;
    let baseline_raw = score_raw_split(
        dataset.examples_for(SplitKind::Test),
        &dataset.in_domain_snapshot,
        &baseline_router,
    )?;

    let (candle_fwd, weights) = load_s1_candle_for_bench();
    let s1_fwd = BenchTimeoutFloor {
        inner: S1RouterAdapter::new(
            candle_fwd,
            S1AdapterConfig {
                debias: false,
                cache_ttl_secs: 0,
                ..S1AdapterConfig::default()
            },
        ),
    };
    let s1_fwd_eval = evaluate_dataset(
        dataset,
        &s1_fwd,
        seed,
        bootstrap_iters,
        Some(&baseline_eval.test_metrics),
    )?;
    let s1_fwd_raw = score_raw_split(
        dataset.examples_for(SplitKind::Test),
        &dataset.in_domain_snapshot,
        &s1_fwd,
    )?;

    let (candle_deb, _) = load_s1_candle_for_bench();
    let s1_deb = BenchTimeoutFloor {
        inner: S1RouterAdapter::new(
            candle_deb,
            S1AdapterConfig {
                debias: true,
                cache_ttl_secs: 0,
                ..S1AdapterConfig::default()
            },
        ),
    };
    let s1_deb_eval = evaluate_dataset(
        dataset,
        &s1_deb,
        seed,
        bootstrap_iters,
        Some(&baseline_eval.test_metrics),
    )?;
    let s1_deb_raw = score_raw_split(
        dataset.examples_for(SplitKind::Test),
        &dataset.in_domain_snapshot,
        &s1_deb,
    )?;

    let debias_eval = evaluate_debias_tradeoff(
        s1_fwd_eval.test_metrics.top1.estimate,
        s1_deb_eval.test_metrics.top1.estimate,
        s1_fwd_eval.test_metrics.p95_latency_ms,
        s1_deb_eval.test_metrics.p95_latency_ms,
    );

    Ok(S1ComparisonReport {
        seed,
        bootstrap_iters,
        weights,
        process_rss_kib: current_process_rss_kib(),
        baseline: build_entry(&baseline_router, baseline_raw, baseline_eval),
        s1_forward: build_entry(&s1_fwd, s1_fwd_raw, s1_fwd_eval),
        s1_debiased: build_entry(&s1_deb, s1_deb_raw, s1_deb_eval),
        debias_decision: DebiasReportJson {
            forward_top1: debias_eval.forward_top1,
            debiased_top1: debias_eval.debiased_top1,
            top1_gain: debias_eval.top1_gain,
            forward_p95_ms: debias_eval.forward_p95_ms,
            debiased_p95_ms: debias_eval.debiased_p95_ms,
            latency_ratio: debias_eval.latency_ratio,
            enable_by_default: debias_eval.enable_by_default,
        },
    })
}

fn build_entry(
    model: &dyn RoutingModel,
    raw_test: RawSplitSummary,
    eval: EvaluatedBenchmark,
) -> EvaluatedRouterEntry {
    EvaluatedRouterEntry {
        router_id: model.id().to_owned(),
        calibration_buckets: eval.calibration_buckets,
        raw_test,
        test_metrics: eval.test_metrics,
        held_out_metrics: eval.held_out_metrics,
        gate_gs1: eval.gate_gs1,
    }
}

fn score_raw_split<'a>(
    examples: impl Iterator<Item = &'a RouteBenchExample>,
    snapshot: &Snapshot,
    model: &dyn RoutingModel,
) -> Result<RawSplitSummary, EvalError> {
    let scope = TenantScope::new(TenantId::default_tenant());
    let vis = AllowAllVisibility::default();

    let mut latencies_ms = Vec::new();
    let mut in_domain_total = 0usize;
    let mut in_domain_top1 = 0usize;
    let mut ece_samples: Vec<(Vec<f32>, usize)> = Vec::new();

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
            request_id: format!("raw-{idx:05}"),
            timeout_ms: 2_000,
        };
        let ranked = poll_future(model.rank(&sanitized, &opts, &ctx))?;
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        let logits = if ranked.logits.len() == opts.len() {
            ranked.logits
        } else {
            opts.iter().map(|o| o.prior).collect()
        };
        let probs = softmax_scaled(&logits, 1.0);
        let sink_idx = opts
            .iter()
            .position(|o| o.key == SINK_OPTION_KEY)
            .unwrap_or_else(|| opts.len().saturating_sub(1));

        match &ex.gold {
            GoldTarget::Group(gold_gid) => {
                in_domain_total = in_domain_total.saturating_add(1);
                if let Some(gold_pos) = resolved_groups
                    .iter()
                    .position(|rg| rg.group_id.as_str() == gold_gid)
                {
                    let mut best_pos = 0usize;
                    let mut best_p = f32::NEG_INFINITY;
                    for (pos, _) in resolved_groups.iter().enumerate() {
                        let p = probs.get(pos).copied().unwrap_or(0.0);
                        if p > best_p {
                            best_p = p;
                            best_pos = pos;
                        }
                    }
                    if best_pos == gold_pos && !resolved_groups.is_empty() {
                        in_domain_top1 = in_domain_top1.saturating_add(1);
                    }
                    if gold_pos < logits.len() {
                        ece_samples.push((logits, gold_pos));
                    }
                }
            }
            GoldTarget::InsufficientEvidence => {
                if sink_idx < logits.len() {
                    ece_samples.push((logits, sink_idx));
                }
            }
        }
    }

    let raw_ece_15 = if ece_samples.is_empty() {
        0.0
    } else {
        f64::from(compute_ece_15_from_logits(&ece_samples, 1.0))
    };
    let top1_accuracy = if in_domain_total == 0 {
        0.0
    } else {
        (in_domain_top1 as f64) / (in_domain_total as f64)
    };

    Ok(RawSplitSummary {
        example_count: latencies_ms.len(),
        top1_accuracy,
        raw_ece_15,
        p50_latency_ms: percentile(&latencies_ms, 0.50),
        p95_latency_ms: percentile(&latencies_ms, 0.95),
        p99_latency_ms: percentile(&latencies_ms, 0.99),
    })
}

fn percentile(samples: &[f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let idx = ((sorted.len().saturating_sub(1) as f64) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn current_process_rss_kib() -> u64 {
    let pid = std::process::id().to_string();
    let Ok(out) = Command::new("ps").args(["-o", "rss=", "-p", &pid]).output() else {
        return 0;
    };
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

/// Writes `report` as pretty JSON to `path`, creating parent directories when needed.
pub fn write_s1_comparison_report(path: &Path, report: &S1ComparisonReport) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(report)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, body)
}
