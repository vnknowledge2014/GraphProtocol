#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::groups::{GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT};

use super::{
    average_non_sink_reversed_logits, average_reversed_logits, evaluate_debias_tradeoff,
    reverse_non_sink_options, reverse_options,
};

fn sample_options() -> Vec<GroupOption> {
    vec![
        GroupOption {
            key: "g0".to_owned(),
            text: "Create billing invoice".to_owned(),
            prior: 0.72,
        },
        GroupOption {
            key: "g1".to_owned(),
            text: "List refund transactions".to_owned(),
            prior: 0.65,
        },
        GroupOption {
            key: "g2".to_owned(),
            text: "Cancel active subscription".to_owned(),
            prior: 0.51,
        },
        GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: 0.15,
        },
    ]
}

// spec: RTE-043
#[test]
fn debias_sink_preserving_permutation_cancels_positional_logit_bias() {
    let opts = sample_options();
    let rev = reverse_non_sink_options(&opts);
    let keys: Vec<&str> = rev.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, vec!["g2", "g1", "g0", SINK_OPTION_KEY]);

    // True option is `g1`, but positional bias adds `[+1.2, 0.0, -0.4]` depending on slot index.
    let forward = [2.2_f32, 2.0_f32, 0.6_f32, -1.0_f32]; // argmax=0 (`g0`) due to bias!
    // On reversed pass `[g2, g1, g0, sink]`: bias puts +1.2 on slot 0 (`g2`), 0 on slot 1 (`g1`), -0.4 on slot 2 (`g0`).
    let reversed = [2.2_f32, 2.0_f32, 0.6_f32, -1.0_f32];
    let avg = average_non_sink_reversed_logits(&forward, &reversed, true);
    // g0: (2.2 + 0.6)/2 = 1.4, g1: (2.0 + 2.0)/2 = 2.0, g2: (0.6 + 2.2)/2 = 1.4 -> g1 wins cleanly!
    assert!((avg[0] - 1.4).abs() < 1e-6);
    assert!((avg[1] - 2.0).abs() < 1e-6);
    assert!((avg[2] - 1.4).abs() < 1e-6);
    assert!((avg[3] - (-1.0)).abs() < 1e-6);

    // Full reversal helpers remain equivariant too.
    let full_rev = reverse_options(&[10, 20, 30]);
    assert_eq!(full_rev, vec![30, 20, 10]);
    let full_avg = average_reversed_logits(&[2.0, 4.0], &[6.0, 2.0]);
    assert_eq!(full_avg, vec![2.0, 5.0]);
}

// spec: RTE-043
#[test]
fn debias_rte_043_default_decision_respects_accuracy_and_latency_thresholds() {
    let accept = evaluate_debias_tradeoff(0.81, 0.825, 4.0, 7.8);
    assert!(accept.top1_gain >= 0.01);
    assert!(accept.latency_ratio <= 2.0);
    assert!(accept.enable_by_default);

    let gain_too_small = evaluate_debias_tradeoff(0.81, 0.815, 4.0, 5.5);
    assert!(!gain_too_small.enable_by_default);

    let too_slow = evaluate_debias_tradeoff(0.81, 0.85, 4.0, 8.2);
    assert!(!too_slow.enable_by_default);
}
