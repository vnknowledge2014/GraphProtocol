#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use gap_core::error::ErrorCode;
use gap_route::s1_adapter::{DecisionBackend, S1_CHOICE_QUESTION};
use gap_route::{GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT};

use super::{
    LayaOrt, OrtExecutionProvider, OrtOptimizationLevel, OrtSessionOptions, PINNED_ORT_VERSION,
    S1_ORT_ENABLED, measure_ort_vs_candle,
};
use crate::candle::{LayaCandle, Precision};

fn block_on_ready<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(val) => val,
        Poll::Pending => panic!("expected synchronous LayaOrt future to complete immediately"),
    }
}

// spec: RTE-041
// Design ref: ADR-005
#[test]
fn ort_parity_matches_laya_candle_logits_within_tolerance() {
    const { assert!(S1_ORT_ENABLED) };
    assert_eq!(PINNED_ORT_VERSION, "=2.0.0-rc.15");

    for precision in [Precision::F32, Precision::F16] {
        let candle = LayaCandle::calibrated_default(precision);
        let ort_cpu = LayaOrt::calibrated_default(
            precision,
            OrtSessionOptions {
                provider: OrtExecutionProvider::Cpu,
                intra_threads: 4,
                optimization_level: OrtOptimizationLevel::Extended,
            },
        );
        let ort_coreml = LayaOrt::calibrated_default(
            precision,
            OrtSessionOptions {
                provider: OrtExecutionProvider::CoreMl,
                intra_threads: 4,
                optimization_level: OrtOptimizationLevel::All,
            },
        );

        assert!(ort_cpu.model_id().starts_with("laya-ort:cpu:"));
        assert!(ort_coreml.model_id().starts_with("laya-ort:coreml:"));
        assert_eq!(ort_cpu.head_max_len(), candle.head_max_len());

        let state = r#"{"constraints":{"max_side_effect":"NONE"},"query":"list billing invoices"}"#;
        let opts = vec![
            GroupOption {
                key: "g0".to_owned(),
                text: "Fetch invoices from billing service".to_owned(),
                prior: 0.90,
            },
            GroupOption {
                key: "g1".to_owned(),
                text: "Read customer support tickets".to_owned(),
                prior: 0.40,
            },
            GroupOption {
                key: SINK_OPTION_KEY.to_owned(),
                text: SINK_OPTION_TEXT.to_owned(),
                prior: 0.15,
            },
        ];

        let expected = candle
            .forward(state, S1_CHOICE_QUESTION, &opts)
            .expect("candle reference")
            .choice_logits;
        let cpu_logits = block_on_ready(ort_cpu.score_choice(state, S1_CHOICE_QUESTION, &opts))
            .expect("ort cpu score");
        let coreml_logits =
            block_on_ready(ort_coreml.score_choice(state, S1_CHOICE_QUESTION, &opts))
                .expect("ort coreml score");

        assert_eq!(cpu_logits.len(), expected.len());
        assert_eq!(coreml_logits.len(), expected.len());
        for i in 0..expected.len() {
            assert!(
                (cpu_logits[i] - expected[i]).abs() <= 1e-3,
                "cpu logit divergence at {i}: {} vs {}",
                cpu_logits[i],
                expected[i]
            );
            assert!(
                (coreml_logits[i] - expected[i]).abs() <= 1e-3,
                "coreml logit divergence at {i}: {} vs {}",
                coreml_logits[i],
                expected[i]
            );
        }
    }
}

// spec: RTE-041, RTE-044
#[test]
fn ort_respects_timeout_budget_and_reports_latency_comparison() {
    let ort_cpu = LayaOrt::calibrated_default(Precision::F32, OrtSessionOptions::default());
    let state = r#"{"constraints":{"max_side_effect":"NONE"},"query":"refund invoice"}"#;
    let opts = vec![
        GroupOption {
            key: "g0".to_owned(),
            text: "Issue customer invoice refund".to_owned(),
            prior: 0.88,
        },
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: 0.15,
        },
    ];

    let err = block_on_ready(ort_cpu.score_choice_with_timeout(
        state,
        S1_CHOICE_QUESTION,
        &opts,
        Duration::ZERO,
    ))
    .expect_err("zero timeout budget must degrade cleanly");
    assert_eq!(err.code(), ErrorCode::DiscoveryDegraded);

    let cmp = measure_ort_vs_candle(Precision::F32, 10).expect("measure ort vs candle");
    assert_eq!(cmp.iterations, 10);
    assert!(cmp.max_abs_diff_cpu <= 1e-3);
    assert!(cmp.max_abs_diff_coreml <= 1e-3);
    assert!(cmp.candle_mean_us > 0.0);
    assert!(cmp.ort_cpu_mean_us > 0.0);
    assert!(cmp.ort_coreml_mean_us > 0.0);
    println!(
        "ORT_BENCH candle_us={:.2} ort_cpu_us={:.2} ort_coreml_us={:.2} max_diff_cpu={:.6} max_diff_coreml={:.6}",
        cmp.candle_mean_us,
        cmp.ort_cpu_mean_us,
        cmp.ort_coreml_mean_us,
        cmp.max_abs_diff_cpu,
        cmp.max_abs_diff_coreml
    );
}
