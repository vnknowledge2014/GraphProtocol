//! GAP-RouteBench dataset builder and routing evaluation harness (`L5`, `WP-M2-06`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::path::PathBuf;
use std::process::ExitCode;

use gap_route::s1_adapter::{BaselineRouter, RoutingModel};

use crate::dataset::{
    DatasetConfig, DeterministicSeededGenerator, RouteBenchDataset, SplitKind, build_dataset,
};
use crate::metrics::evaluate_dataset;
use crate::report::{DatasetSummary, RouteBenchReport, write_reports};

pub mod dataset;
pub mod metrics;
pub mod mining;
pub mod report;

/// Command-line configuration for the `routebench` binary.
#[derive(Debug, Clone)]
pub struct CliConfig {
    /// Directory containing RouteBench `.json` / `.openapi.json` specs.
    pub data_dir: PathBuf,
    /// Output directory where `report.json` and `report.md` are written.
    pub out_dir: PathBuf,
    /// Optional disk cache directory for deterministic paraphrase/hard-negative synthesis.
    pub cache_dir: Option<PathBuf>,
    /// Deterministic PRNG seed (`RTE-060`).
    pub seed: u64,
    /// Number of bootstrap resamples per metric (`RTE-061`).
    pub bootstrap_iters: usize,
    /// Explicit bootstrap override flag.
    pub bootstrap_overridden: bool,
    /// Compact fast mode suitable for smoke checks.
    pub smoke: bool,
    /// Print help text and exit without running.
    pub help: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data"),
            out_dir: PathBuf::from("target/routebench"),
            cache_dir: None,
            seed: 20_261_115,
            bootstrap_iters: 1_000,
            bootstrap_overridden: false,
            smoke: false,
            help: false,
        }
    }
}

fn parse_args<I>(args: I) -> Result<CliConfig, String>
where
    I: IntoIterator<Item = String>,
{
    let mut cfg = CliConfig::default();
    let mut iter = args.into_iter().skip(1);
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--smoke" => cfg.smoke = true,
            "--help" | "-h" => cfg.help = true,
            "--out" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value after `--out`".to_owned())?;
                cfg.out_dir = PathBuf::from(val);
            }
            "--data-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value after `--data-dir`".to_owned())?;
                cfg.data_dir = PathBuf::from(val);
            }
            "--cache-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "missing value after `--cache-dir`".to_owned())?;
                cfg.cache_dir = Some(PathBuf::from(val));
            }
            "--seed" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "missing value after `--seed`".to_owned())?;
                cfg.seed = raw
                    .parse::<u64>()
                    .map_err(|e| format!("invalid `--seed` value `{raw}`: {e}"))?;
            }
            "--bootstrap" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "missing value after `--bootstrap`".to_owned())?;
                cfg.bootstrap_iters = raw
                    .parse::<usize>()
                    .map_err(|e| format!("invalid `--bootstrap` value `{raw}`: {e}"))?;
                cfg.bootstrap_overridden = true;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if cfg.smoke && !cfg.bootstrap_overridden {
        cfg.bootstrap_iters = 200;
    }
    Ok(cfg)
}

fn build_dataset_summary(dataset: &RouteBenchDataset) -> DatasetSummary {
    let mut fit_ops = 0usize;
    let mut cal_ops = 0usize;
    let mut test_ops = 0usize;
    for split in dataset.operation_splits.values() {
        match split {
            SplitKind::Fit => fit_ops += 1,
            SplitKind::Calibration => cal_ops += 1,
            SplitKind::Test => test_ops += 1,
            SplitKind::HeldOutBranch => {}
        }
    }

    DatasetSummary {
        total_branches: dataset.all_branches.len(),
        in_domain_branches: dataset.in_domain_branch_uids.len(),
        held_out_branches: dataset.held_out_branch_uids.len(),
        fit_operations: fit_ops,
        calibration_operations: cal_ops,
        test_operations: test_ops,
        fit_examples: dataset.examples_for(SplitKind::Fit).count(),
        calibration_examples: dataset.examples_for(SplitKind::Calibration).count(),
        test_examples: dataset.examples_for(SplitKind::Test).count(),
        held_out_examples: dataset.examples_for(SplitKind::HeldOutBranch).count(),
    }
}

/// Runs the RouteBench dataset builder + evaluation pipeline and writes `report.json` +
/// `report.md` to `cfg.out_dir`.
pub fn execute_routebench(cfg: &CliConfig) -> Result<RouteBenchReport, String> {
    let ds_cfg = DatasetConfig {
        seed: cfg.seed,
        cache_dir: cfg.cache_dir.clone(),
        smoke: cfg.smoke,
        ..DatasetConfig::default()
    };
    let generator = DeterministicSeededGenerator::new(cfg.cache_dir.clone());
    let dataset = build_dataset(&cfg.data_dir, &ds_cfg, &generator).map_err(|e| e.to_string())?;
    let router = BaselineRouter::default();
    let eval = evaluate_dataset(&dataset, &router, cfg.seed, cfg.bootstrap_iters, None)
        .map_err(|e| e.to_string())?;

    let report = RouteBenchReport {
        seed: cfg.seed,
        smoke: cfg.smoke,
        bootstrap_iters: cfg.bootstrap_iters,
        router_id: router.id().to_owned(),
        dataset: build_dataset_summary(&dataset),
        calibration_buckets: eval.calibration_buckets,
        test_metrics: eval.test_metrics,
        held_out_metrics: eval.held_out_metrics,
        gate_gs1: eval.gate_gs1,
    };
    write_reports(&cfg.out_dir, &report).map_err(|e| e.to_string())?;
    Ok(report)
}

fn print_usage() {
    println!(
        "routebench — GAP RouteBench v1 harness\n\
         Usage: routebench [--smoke] [--out <dir>] [--data-dir <dir>] [--cache-dir <dir>] [--seed <u64>] [--bootstrap <n>]"
    );
}

fn main() -> ExitCode {
    let cfg = match parse_args(std::env::args()) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("routebench argument error: {err}");
            print_usage();
            return ExitCode::from(2);
        }
    };
    if cfg.help {
        print_usage();
        return ExitCode::SUCCESS;
    }

    match execute_routebench(&cfg) {
        Ok(report) => {
            println!(
                "RouteBench complete: out={}, router={}, top1={:.3}, ece_15={:.4}, cov={:.3}, gate_gs1={}",
                cfg.out_dir.display(),
                report.router_id,
                report.test_metrics.top1.estimate,
                report.test_metrics.ece_15.estimate,
                report.test_metrics.conformal_empirical_coverage.estimate,
                report.gate_gs1.overall_pass,
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("routebench failed: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gap_core::ids::OperationId;
    use gap_core::snapshot::{BranchDescriptor, OperationDescriptor};
    use gap_core::traits::DiscoveryOutcome;

    use super::*;
    use crate::dataset::{DatasetError, ParaphraseGenerator};
    use crate::metrics::{
        MetricWithCi, ScoredPrediction, SplitEvaluation, compute_split_evaluation,
        evaluate_gate_gs1,
    };

    fn fixture_data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
    }

    #[test]
    // spec: RTE-060
    fn rte_060_splits_have_zero_operation_leakage_and_held_out_branch() {
        let cfg = DatasetConfig {
            seed: 42,
            smoke: false,
            ..DatasetConfig::default()
        };
        let generator = DeterministicSeededGenerator::default();
        let ds = build_dataset(&fixture_data_dir(), &cfg, &generator).unwrap();

        assert!(
            !ds.held_out_branch_uids.is_empty(),
            "expected at least one held-out cold-start branch (~10%)"
        );
        assert!(
            !ds.in_domain_branch_uids.is_empty(),
            "expected non-empty in-domain branches"
        );

        let held_out_names: BTreeSet<String> = ds
            .all_branches
            .iter()
            .filter(|b| ds.held_out_branch_uids.contains(&b.uid))
            .map(|b| b.id.as_str().to_owned())
            .collect();

        let mut op_splits_seen: BTreeMap<String, SplitKind> = BTreeMap::new();
        for ex in &ds.examples {
            if let Some(branch_name) = &ex.source_branch {
                if held_out_names.contains(branch_name) {
                    assert_eq!(ex.split, SplitKind::HeldOutBranch);
                } else {
                    assert_ne!(ex.split, SplitKind::HeldOutBranch);
                }
            }
            if let Some(op_id) = &ex.source_op
                && let Some(prev) = op_splits_seen.insert(op_id.clone(), ex.split)
            {
                assert_eq!(
                    prev, ex.split,
                    "operation `{op_id}` leaked across splits {prev:?} and {:?}",
                    ex.split
                );
            }
        }

        // Verify all three in-domain splits exist and have disjoint operation sets.
        assert!(ds.examples_for(SplitKind::Fit).next().is_some());
        assert!(ds.examples_for(SplitKind::Calibration).next().is_some());
        assert!(ds.examples_for(SplitKind::Test).next().is_some());
        assert!(ds.examples_for(SplitKind::HeldOutBranch).next().is_some());

        // Held-out branch UIDs must not appear inside `in_domain_snapshot`.
        for uid in &ds.held_out_branch_uids {
            assert!(!ds.in_domain_snapshot.branches.contains_key(uid));
            assert!(ds.full_snapshot.branches.contains_key(uid));
        }
    }

    struct CountingGenerator {
        inner: DeterministicSeededGenerator,
        calls: AtomicUsize,
    }

    impl ParaphraseGenerator for CountingGenerator {
        fn generate_paraphrases(
            &self,
            op: &OperationDescriptor,
            branch: &BranchDescriptor,
            count: usize,
            seed: u64,
        ) -> Result<Vec<String>, DatasetError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner.generate_paraphrases(op, branch, count, seed)
        }

        fn generate_hard_negatives(
            &self,
            op: &OperationDescriptor,
            branch: &BranchDescriptor,
            siblings: &[(&OperationDescriptor, &BranchDescriptor)],
            count: usize,
            seed: u64,
        ) -> Result<Vec<(String, OperationId)>, DatasetError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inner
                .generate_hard_negatives(op, branch, siblings, count, seed)
        }
    }

    #[test]
    // spec: RTE-060
    fn rte_060_generator_is_seed_deterministic_and_uses_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = DatasetConfig {
            seed: 99_001,
            cache_dir: Some(tmp.path().to_path_buf()),
            smoke: true,
            ..DatasetConfig::default()
        };
        let gen1 = CountingGenerator {
            inner: DeterministicSeededGenerator::new(Some(tmp.path().to_path_buf())),
            calls: AtomicUsize::new(0),
        };
        let ds1 = build_dataset(&fixture_data_dir(), &cfg, &gen1).unwrap();
        assert!(gen1.calls.load(Ordering::Relaxed) > 0);
        let cache_files: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert!(
            !cache_files.is_empty(),
            "cache directory should persist generated paraphrase entries"
        );

        let gen2 = DeterministicSeededGenerator::new(Some(tmp.path().to_path_buf()));
        let ds2 = build_dataset(&fixture_data_dir(), &cfg, &gen2).unwrap();
        assert_eq!(ds1.examples, ds2.examples);

        let cfg_diff_seed = DatasetConfig {
            seed: 99_002,
            cache_dir: Some(tmp.path().to_path_buf()),
            smoke: true,
            ..DatasetConfig::default()
        };
        let ds3 = build_dataset(&fixture_data_dir(), &cfg_diff_seed, &gen2).unwrap();
        assert_ne!(ds1.examples, ds3.examples);
    }

    #[test]
    // spec: RTE-061
    fn rte_061_metrics_and_bootstrap_cis_match_known_predictions() {
        let preds = vec![
            // High-confidence correct in-domain (0.90)
            ScoredPrediction {
                is_in_domain: true,
                group_rank_1based: Some(1),
                p_gold: 0.90,
                brier_score: (0.90_f64 - 1.0).powi(2) + 0.10_f64.powi(2),
                max_prob: 0.90,
                argmax_correct: true,
                selective_confidence: 0.90,
                top1_group_correct: true,
                abstention_score: 0.10,
                conformal_outcome: DiscoveryOutcome::Resolved,
                conformal_covered: true,
                prediction_set_size: 1,
                latency_ms: 4.0,
            },
            // Mid-confidence rank-2 ambiguous in-domain (0.60 gold, covered in set of 2)
            ScoredPrediction {
                is_in_domain: true,
                group_rank_1based: Some(2),
                p_gold: 0.60,
                brier_score: (0.60_f64 - 1.0).powi(2) + 0.40_f64.powi(2),
                max_prob: 0.60,
                argmax_correct: false,
                selective_confidence: 0.60,
                top1_group_correct: false,
                abstention_score: 0.40,
                conformal_outcome: DiscoveryOutcome::Ambiguous,
                conformal_covered: true,
                prediction_set_size: 2,
                latency_ms: 8.0,
            },
            // Confident OOD abstained correctly
            ScoredPrediction {
                is_in_domain: false,
                group_rank_1based: None,
                p_gold: 0.85,
                brier_score: (0.85_f64 - 1.0).powi(2) + 0.15_f64.powi(2),
                max_prob: 0.85,
                argmax_correct: true,
                selective_confidence: 0.15,
                top1_group_correct: false,
                abstention_score: 0.85,
                conformal_outcome: DiscoveryOutcome::Abstained,
                conformal_covered: true,
                prediction_set_size: 0,
                latency_ms: 12.0,
            },
        ];

        let eval = compute_split_evaluation(&preds, 7, 100);
        assert_eq!(eval.total_examples, 3);
        assert_eq!(eval.in_domain_examples, 2);
        assert_eq!(eval.ood_examples, 1);
        assert!((eval.top1.estimate - 0.5).abs() < 1e-6);
        assert!((eval.top3.estimate - 1.0).abs() < 1e-6);
        assert!((eval.mrr.estimate - 0.75).abs() < 1e-6);
        assert!((eval.conformal_empirical_coverage.estimate - 1.0).abs() < 1e-6);
        assert!((eval.mean_set_size.estimate - 1.0).abs() < 1e-6);
        assert!((eval.ambiguity_rate.estimate - (1.0 / 3.0)).abs() < 1e-6);
        assert!((eval.ood_abstention_precision.estimate - 1.0).abs() < 1e-6);
        assert!((eval.abstention_auroc.estimate - 1.0).abs() < 1e-6);
        assert_eq!(eval.reliability_bins.len(), 15);
        assert!(
            eval.top1.ci_low_95 <= eval.top1.estimate && eval.top1.estimate <= eval.top1.ci_high_95
        );
        assert!(eval.nll.estimate > 0.0);
        assert!(eval.brier.estimate > 0.0);
        assert!(eval.selective_accuracy_50.estimate >= eval.selective_accuracy_90.estimate);
    }

    #[test]
    // spec: RTE-060
    // spec: RTE-061
    fn rte_060_061_smoke_cli_writes_valid_json_and_markdown_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CliConfig {
            data_dir: fixture_data_dir(),
            out_dir: tmp.path().to_path_buf(),
            cache_dir: Some(tmp.path().join("cache")),
            seed: 1234,
            bootstrap_iters: 50,
            bootstrap_overridden: true,
            smoke: true,
            help: false,
        };
        let report = execute_routebench(&cfg).unwrap();
        assert_eq!(report.seed, 1234);
        assert!(report.smoke);
        assert!(report.dataset.total_branches >= 5);
        assert!(report.test_metrics.total_examples > 0);
        assert_eq!(report.test_metrics.reliability_bins.len(), 15);

        let json_path = tmp.path().join("report.json");
        let md_path = tmp.path().join("report.md");
        assert!(json_path.exists());
        assert!(md_path.exists());

        let parsed: RouteBenchReport =
            serde_json::from_str(&fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(parsed.router_id, report.router_id);
        let md = fs::read_to_string(&md_path).unwrap();
        assert!(md.contains("RouteBench v1 Report"));
        assert!(md.contains("Gate G-S1 Summary"));
    }

    #[test]
    // spec: RTE-069
    fn rte_069_gate_gs1_checks_delta_ece_coverage_and_latency() {
        let mk_eval = |top1: f64, sel70: f64, ece: f64, cov: f64, p95: f64| SplitEvaluation {
            total_examples: 100,
            in_domain_examples: 90,
            ood_examples: 10,
            p50_latency_ms: 10.0,
            p95_latency_ms: p95,
            top1: MetricWithCi::fixed(top1),
            top3: MetricWithCi::fixed(0.95),
            mrr: MetricWithCi::fixed(0.88),
            ece_15: MetricWithCi::fixed(ece),
            brier: MetricWithCi::fixed(0.10),
            nll: MetricWithCi::fixed(0.30),
            selective_accuracy_50: MetricWithCi::fixed(0.96),
            selective_accuracy_70: MetricWithCi::fixed(sel70),
            selective_accuracy_90: MetricWithCi::fixed(0.88),
            abstention_auroc: MetricWithCi::fixed(0.92),
            conformal_empirical_coverage: MetricWithCi::fixed(cov),
            mean_set_size: MetricWithCi::fixed(1.2),
            ambiguity_rate: MetricWithCi::fixed(0.1),
            ood_abstention_precision: MetricWithCi::fixed(0.9),
            reliability_bins: Vec::new(),
        };

        let baseline = mk_eval(0.75, 0.85, 0.08, 0.89, 15.0);
        // Passes via +0.04 top1 gain, ECE 0.04 <= 0.05, coverage 0.90 >= 0.88, p95 25 <= 40.
        let good = mk_eval(0.79, 0.86, 0.04, 0.90, 25.0);
        let g_pass = evaluate_gate_gs1(&good, Some(&baseline), 0.10, 40.0);
        assert!(g_pass.overall_pass);
        assert!(g_pass.accuracy_gain_ok);
        assert!(g_pass.ece_ok);
        assert!(g_pass.coverage_ok);
        assert!(g_pass.latency_ok);

        // Fails if ECE exceeds 0.05 even with accuracy gain.
        let bad_ece = mk_eval(0.82, 0.90, 0.06, 0.92, 20.0);
        assert!(!evaluate_gate_gs1(&bad_ece, Some(&baseline), 0.10, 40.0).overall_pass);

        // Fails if conformal coverage violates 1 - alpha - 0.02 (0.88 for alpha=0.10).
        let bad_cov = mk_eval(0.82, 0.90, 0.03, 0.85, 20.0);
        assert!(!evaluate_gate_gs1(&bad_cov, Some(&baseline), 0.10, 40.0).overall_pass);

        // Fails if latency breaches SLO.
        let bad_lat = mk_eval(0.82, 0.90, 0.03, 0.92, 45.0);
        assert!(!evaluate_gate_gs1(&bad_lat, Some(&baseline), 0.10, 40.0).overall_pass);
    }
}
