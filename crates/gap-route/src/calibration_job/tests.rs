#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::snapshot::Snapshot;

use super::{
    ALPHA_MUTATING, ALPHA_READ_ONLY, CalibrationExample, CalibrationMode, CalibrationSplit,
    CalibrationVersionKey, OnlineCalibrationMonitor, OnlineCalibrationSample, OnlineEceAlert,
    OnlineMonitorConfig, OptionsBucket, ScheduledRecalibrationJob, WinsorizedConfig,
    is_snapshot_calibration_valid, lookup_calibration_params, run_calibration_pipeline,
    snapshot_calibration_id,
};

struct XorShift64(u64);

impl XorShift64 {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_unit(&mut self) -> f32 {
        let top = (self.next_u64() >> 40) as u32;
        (top as f32) / ((1u32 << 24) as f32)
    }

    fn next_normal_approx(&mut self) -> f32 {
        // Sum of 6 uniforms centered cleanly without external crates.
        let mut s = 0.0_f32;
        for _ in 0..6 {
            s += self.next_unit();
        }
        (s - 3.0) * std::f32::consts::SQRT_2
    }
}

fn generate_exchangeable_bucket(
    rng: &mut XorShift64,
    n_options: usize,
    fit_n: usize,
    cal_n: usize,
    test_n: usize,
    examples: &mut Vec<CalibrationExample>,
) {
    let total = fit_n + cal_n + test_n;
    for idx in 0..total {
        let split = if idx < fit_n {
            CalibrationSplit::Fit
        } else if idx < fit_n + cal_n {
            CalibrationSplit::Calibration
        } else {
            CalibrationSplit::Test
        };

        // True latent signal vector per option with overconfident temperature gain (~3.0x).
        let true_idx = (rng.next_u64() as usize) % n_options;
        let mut logits = Vec::with_capacity(n_options);
        for opt_idx in 0..n_options {
            let signal = if opt_idx == true_idx { 1.5 } else { 0.0 };
            let noise = rng.next_normal_approx() * 0.90;
            // Overconfident logit scaling (pre-temperature ECE > post-temperature ECE).
            logits.push((signal + noise) * 3.0);
        }

        examples.push(CalibrationExample {
            split,
            logits,
            true_index: true_idx,
            sink_index: Some(n_options - 1),
            has_mutating_option: (idx % 3) == 0,
        });
    }
}

fn version_key() -> CalibrationVersionKey {
    CalibrationVersionKey {
        router_model_id: "s1:laya-candle:f32:abc123def456".to_owned(),
        embedding_model_id: Some("hash-embed-384".to_owned()),
        option_format_version: 1,
    }
}

// spec: RTE-062, RTE-064, RTE-065, RTE-066, RTE-067
#[test]
fn calibration_synthetic_conformal_coverage_exceeds_target_across_all_buckets() {
    let mut rng = XorShift64(0x9e37_79b9_7f4a_7c15);
    let mut examples = Vec::new();
    for n_opts in [2usize, 4, 8, 12] {
        generate_exchangeable_bucket(&mut rng, n_opts, 350, 700, 1_000, &mut examples);
    }

    let key = version_key();
    let (record, report) =
        run_calibration_pipeline(key.clone(), &examples, WinsorizedConfig::default())
            .expect("calibration pipeline succeeds");

    assert_eq!(report.calibration_id, record.calibration_id);
    assert_eq!(
        report.calibration_id,
        run_calibration_pipeline(key.clone(), &examples, WinsorizedConfig::default())
            .unwrap()
            .0
            .calibration_id
    );
    assert!(
        report.overall_ece_after <= report.overall_ece_before,
        "temperature scaling must reduce or maintain ECE ({} -> {})",
        report.overall_ece_before,
        report.overall_ece_after
    );

    for b in &report.buckets {
        assert_eq!(b.mode_read_only, CalibrationMode::Conformal);
        assert_eq!(b.mode_mutating, CalibrationMode::Conformal);
        assert!(
            b.temperature >= 1.1,
            "expected T > 1.1 on overconfident logits for bucket {:?}",
            b.bucket
        );
        assert!(b.ece_after <= b.ece_before + 1e-4);

        let cov_ro = b
            .empirical_coverage_read_only
            .expect("read-only test coverage");
        let cov_mut = b
            .empirical_coverage_mutating
            .expect("mutating test coverage");
        let pol_cov = b.policy_empirical_coverage.expect("policy coverage");

        assert!(
            cov_ro >= 1.0 - ALPHA_READ_ONLY - 0.02,
            "bucket {:?} alpha=0.10 coverage {cov_ro:.4} < 0.88",
            b.bucket
        );
        assert!(
            cov_mut >= 1.0 - ALPHA_MUTATING - 0.02,
            "bucket {:?} alpha=0.05 coverage {cov_mut:.4} < 0.93",
            b.bucket
        );
        assert!(
            pol_cov >= 1.0 - ALPHA_READ_ONLY - 0.02,
            "bucket {:?} policy coverage {pol_cov:.4} < 0.88",
            b.bucket
        );
        assert!(b.mean_set_size_mutating.unwrap() >= b.mean_set_size_read_only.unwrap() - 0.05);
        assert_eq!(b.reliability_before.len(), 15);
        assert_eq!(b.reliability_after.len(), 15);
    }

    let md = report.render_markdown();
    assert!(md.contains("Overall ECE_15"));
    assert!(is_snapshot_calibration_valid(&report.table, &key));
    let _ = snapshot_calibration_id(&report.table);
    let (_t, q_mut) =
        lookup_calibration_params(&report.table, OptionsBucket::ThreeToFive, ALPHA_MUTATING);
    let (_t2, q_ro) =
        lookup_calibration_params(&report.table, OptionsBucket::ThreeToFive, ALPHA_READ_ONLY);
    assert!(q_mut.is_some());
    assert!(q_ro.is_some());
}

// spec: RTE-064, RTE-067, TM-14, INV-17
#[test]
fn calibration_invalidation_small_sample_fallback_and_winsorized_fit() {
    let mut rng = XorShift64(0x1234_5678_9abc_def0);
    let mut small_ds = Vec::new();
    // Only 120 calibration samples (< 500) -> Fallback thresholds must trigger cleanly.
    generate_exchangeable_bucket(&mut rng, 4, 80, 120, 200, &mut small_ds);
    // Inject extreme poisoned logits on Fit rows to test TM-14 winsorization resilience.
    let idx0 = small_ds[0].true_index;
    small_ds[0].logits[idx0] = 1_000.0;
    small_ds[1].logits[0] = -1_000.0;

    let key = version_key();
    let (_record, report) = run_calibration_pipeline(
        key.clone(),
        &small_ds,
        WinsorizedConfig {
            clip_logit_abs: Some(12.0),
        },
    )
    .expect("small sample pipeline succeeds");

    let bucket_summary = report
        .buckets
        .iter()
        .find(|b| b.bucket == OptionsBucket::ThreeToFive)
        .unwrap();
    assert_eq!(bucket_summary.mode_read_only, CalibrationMode::Fallback);
    assert_eq!(bucket_summary.q_hat_read_only, None);
    assert!(bucket_summary.temperature.is_finite());

    // Changing any component of CalibrationVersionKey MUST invalidate snapshot calibration (`RTE-067`).
    let wrong_router = CalibrationVersionKey {
        router_model_id: "s1:laya-candle:f32:different_sha".to_owned(),
        ..key.clone()
    };
    let wrong_embedder = CalibrationVersionKey {
        embedding_model_id: Some("bge-small-v2".to_owned()),
        ..key.clone()
    };
    let wrong_fmt = CalibrationVersionKey {
        option_format_version: 2,
        ..key.clone()
    };
    assert!(!is_snapshot_calibration_valid(&report.table, &wrong_router));
    assert!(!is_snapshot_calibration_valid(
        &report.table,
        &wrong_embedder
    ));
    assert!(!is_snapshot_calibration_valid(&report.table, &wrong_fmt));
}

// spec: RTE-068, RTE-111, INV-01
#[test]
fn calibration_online_ece_monitor_alerts_and_scheduled_recalibration_staging() {
    let mut monitor = OnlineCalibrationMonitor::new(OnlineMonitorConfig {
        window_size: 500,
        ece_threshold: 0.08,
        baseline_abstention_rate: 0.10,
        abstention_shift_multiplier: 2.0,
    });

    // Window < 500 returns None.
    for i in 0..499 {
        assert!(
            monitor
                .observe(OnlineCalibrationSample {
                    p_pred: 0.90,
                    correct: true,
                    abstained: false,
                    timestamp_secs: i as u64,
                })
                .is_none()
        );
    }
    assert_eq!(monitor.online_ece_gauge(), None);

    // Sample 500 completes first high-ECE window (p_pred = 0.95, all incorrect -> ECE ~0.95 > 0.08),
    // but only ONE consecutive breach -> no ConsecutiveHighEce yet; however abstention 0.26 > 2 * 0.10.
    let mut mon2 = OnlineCalibrationMonitor::new(OnlineMonitorConfig {
        window_size: 500,
        ece_threshold: 0.08,
        baseline_abstention_rate: 0.10,
        abstention_shift_multiplier: 2.0,
    });
    let mut first_eval = None;
    for i in 0..500 {
        let ev = mon2.observe(OnlineCalibrationSample {
            p_pred: 0.95,
            correct: false,
            abstained: i < 130, // 26% > 20%
            timestamp_secs: i as u64,
        });
        if ev.is_some() {
            first_eval = ev;
        }
    }
    let w0 = first_eval.expect("first window completed at 500 samples");
    assert_eq!(w0.window_index, 0);
    assert!(w0.online_ece > 0.80);
    assert!(mon2.online_ece_gauge().is_some());
    assert_eq!(w0.alerts.len(), 1);
    assert!(matches!(
        w0.alerts[0],
        OnlineEceAlert::AbstentionRateShift { .. }
    ));

    // Second consecutive high-ECE window triggers ConsecutiveHighEce (`RTE-111`).
    let mut second_eval = None;
    for i in 0..500 {
        let ev = mon2.observe(OnlineCalibrationSample {
            p_pred: 0.92,
            correct: false,
            abstained: false, // normal abstention
            timestamp_secs: 1000 + i as u64,
        });
        if ev.is_some() {
            second_eval = ev;
        }
    }
    let w1 = second_eval.expect("second window completed");
    assert_eq!(w1.window_index, 1);
    assert!(
        w1.alerts
            .iter()
            .any(|a| matches!(a, OnlineEceAlert::ConsecutiveHighEce { .. })),
        "two consecutive windows > 0.08 must trigger ConsecutiveHighEce"
    );

    // ScheduledRecalibrationJob stages generation + 1 without mutating existing snapshot (`INV-01`).
    let mut rng = XorShift64(0xface_b00c);
    let mut ds = Vec::new();
    generate_exchangeable_bucket(&mut rng, 4, 200, 550, 200, &mut ds);
    let key = version_key();
    let mut job = ScheduledRecalibrationJob::new(
        Duration::from_secs(3600),
        key.clone(),
        WinsorizedConfig::default(),
    );
    assert!(job.is_due(1_000));
    let (_rec, rep) = job.run_once(&ds, 1_000).expect("scheduled run succeeds");
    assert!(!job.is_due(2_000));
    assert!(job.is_due(4_600));

    let base_snap = Snapshot::empty(4);
    let next_snap = ScheduledRecalibrationJob::stage_next_snapshot(
        &base_snap,
        rep.table,
        Timestamp::UNIX_EPOCH,
    );
    assert!(base_snap.calibration.is_none());
    assert_eq!(next_snap.generation, 5);
    assert!(is_snapshot_calibration_valid(
        next_snap.calibration.as_deref().unwrap(),
        &key
    ));
}
