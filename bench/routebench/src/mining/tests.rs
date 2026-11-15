#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use gap_route::s1_adapter::BaselineRouter;

use crate::dataset::{DatasetConfig, DeterministicSeededGenerator, build_dataset};
use crate::mining::eval_s1::{evaluate_s1_vs_baseline, write_s1_comparison_report};
use crate::mining::{mine_hard_negatives, verify_zero_split_leakage, write_mined_corpus};

fn manifest_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
}

// spec: RTE-060
#[test]
fn mining_extracts_hard_negatives_without_cross_split_leakage() {
    let cfg = DatasetConfig {
        seed: 20_261_115,
        smoke: true,
        ..DatasetConfig::default()
    };
    let generator = DeterministicSeededGenerator::new(None);
    let dataset = build_dataset(&manifest_data_dir(), &cfg, &generator).expect("build dataset");
    let baseline = BaselineRouter::default();

    let corpus = mine_hard_negatives(&dataset, &baseline).expect("mine hard negatives");
    assert_eq!(corpus.schema_version, 2);
    assert_eq!(corpus.mined_count, corpus.records.len());
    verify_zero_split_leakage(&dataset, &corpus).expect("zero split leakage");

    let dir = tempfile::tempdir().expect("tempdir");
    let out_path = dir.path().join("mined_v2.json");
    write_mined_corpus(&out_path, &corpus).expect("write mined corpus");
    assert!(out_path.exists());
}

// spec: RTE-043, RTE-060
#[test]
fn mining_s1_vs_baseline_evaluation_and_optional_artifact_export() {
    let full_mode = std::env::var_os("GAP_ROUTEBENCH_FULL").is_some();
    let cfg = if full_mode {
        DatasetConfig {
            seed: 20_261_115,
            smoke: false,
            ..DatasetConfig::default()
        }
    } else {
        DatasetConfig {
            seed: 20_261_115,
            smoke: true,
            paraphrases_per_op: if cfg!(debug_assertions) { 0 } else { 2 },
            hard_negatives_per_op: if cfg!(debug_assertions) { 0 } else { 1 },
            ood_negative_count: if cfg!(debug_assertions) { 6 } else { 20 },
            ..DatasetConfig::default()
        }
    };
    let bootstrap_iters = if full_mode { 1_000 } else { 40 };
    let generator = DeterministicSeededGenerator::new(None);
    let dataset = build_dataset(&manifest_data_dir(), &cfg, &generator).expect("build dataset");

    let report =
        evaluate_s1_vs_baseline(&dataset, cfg.seed, bootstrap_iters).expect("evaluate s1 vs base");
    assert!(!report.baseline.router_id.is_empty());
    assert!(report.s1_forward.router_id.starts_with("s1:"));
    assert!(report.s1_debiased.router_id.starts_with("s1:"));
    assert!(report.s1_forward.raw_test.p95_latency_ms >= 0.0);
    assert!(report.s1_forward.raw_test.p99_latency_ms >= report.s1_forward.raw_test.p95_latency_ms);
    assert!(report.debias_decision.latency_ratio.is_finite());

    let baseline = BaselineRouter::default();
    let mined = mine_hard_negatives(&dataset, &baseline).expect("mine corpus");
    verify_zero_split_leakage(&dataset, &mined).expect("zero split leakage");

    if let Some(out_dir_os) = std::env::var_os("GAP_ROUTEBENCH_S1_OUT") {
        let out_dir = PathBuf::from(out_dir_os);
        fs::create_dir_all(&out_dir).expect("create out dir");
        write_s1_comparison_report(&out_dir.join("s1_comparison.json"), &report)
            .expect("write s1 comparison json");
        let debias_json = serde_json::to_string_pretty(&report.debias_decision).unwrap();
        fs::write(out_dir.join("debias_eval.json"), debias_json).expect("write debias json");
        write_mined_corpus(&out_dir.join("mined_hard_negatives.json"), &mined)
            .expect("write mined hard negatives");
    }
}
