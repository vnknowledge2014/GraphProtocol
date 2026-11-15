//! Permutation-averaging option-order debiasing and `RTE-043` cost-benefit evaluator
//! (`spec/04-routing.md` §6; `RTE-043`, `WP-M5-07`).

#[cfg(test)]
mod tests;

use crate::groups::{GroupOption, SINK_OPTION_KEY};

/// Minimum Top-1 accuracy improvement required by `RTE-043` to enable `s1.debias` by default.
pub const DEBIAS_MIN_TOP1_GAIN: f64 = 0.01;

/// Maximum p95 latency multiplier tolerated by `RTE-043` when enabling `s1.debias` by default.
pub const DEBIAS_MAX_LATENCY_RATIO: f64 = 2.0;

/// Averages `forward` logits with `reversed` logits aligned to the original forward option order
/// (`RTE-043`).
///
/// If lengths differ cleanly (should not happen with compliant backends), falls back to cloning `forward`.
#[must_use]
pub fn average_reversed_logits(forward: &[f32], reversed: &[f32]) -> Vec<f32> {
    if forward.len() != reversed.len() || forward.is_empty() {
        return forward.to_vec();
    }
    let len = forward.len();
    let mut out = Vec::with_capacity(len);
    for (idx, &f_val) in forward.iter().enumerate() {
        let rev_idx = len - 1 - idx;
        let r_val = reversed.get(rev_idx).copied().unwrap_or(f_val);
        out.push(0.5 * (f_val + r_val));
    }
    out
}

/// Permutes options in reverse order (`RTE-043`).
#[must_use]
pub fn reverse_options<T: Clone>(options: &[T]) -> Vec<T> {
    let mut reversed = options.to_vec();
    reversed.reverse();
    reversed
}

/// Reverses non-sink candidate [`GroupOption`] entries while keeping a trailing `SINK_OPTION_KEY`
/// option anchored in the final position (`RTE-042`, `RTE-043`).
#[must_use]
pub fn reverse_non_sink_options(options: &[GroupOption]) -> Vec<GroupOption> {
    let Some(last) = options.last() else {
        return Vec::new();
    };
    if last.key != SINK_OPTION_KEY || options.len() <= 2 {
        return reverse_options(options);
    }
    let non_sink_len = options.len() - 1;
    let mut out = Vec::with_capacity(options.len());
    out.extend(options[..non_sink_len].iter().rev().cloned());
    out.push(last.clone());
    out
}

/// Averages `forward` and `reversed` logits when `has_trailing_sink` kept the sink option fixed at
/// the final slot during the reversed pass (`RTE-043`).
#[must_use]
pub fn average_non_sink_reversed_logits(
    forward: &[f32],
    reversed: &[f32],
    has_trailing_sink: bool,
) -> Vec<f32> {
    if forward.len() != reversed.len() || forward.is_empty() {
        return forward.to_vec();
    }
    if !has_trailing_sink || forward.len() <= 2 {
        return average_reversed_logits(forward, reversed);
    }
    let non_sink_len = forward.len() - 1;
    let mut out = Vec::with_capacity(forward.len());
    for (idx, &f_val) in forward.iter().enumerate() {
        let rev_idx = if idx < non_sink_len {
            non_sink_len - 1 - idx
        } else {
            idx
        };
        let r_val = reversed.get(rev_idx).copied().unwrap_or(f_val);
        out.push(0.5 * (f_val + r_val));
    }
    out
}

/// Data-driven `RTE-043` measurement summary deciding whether `s1.debias` is enabled by default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebiasEvaluationSummary {
    /// Forward-only (`debias = false`) Top-1 accuracy on `Test`.
    pub forward_top1: f64,
    /// Forward+reverse (`debias = true`) Top-1 accuracy on `Test`.
    pub debiased_top1: f64,
    /// Signed Top-1 improvement (`debiased_top1 - forward_top1`).
    pub top1_gain: f64,
    /// Forward-only p95 latency in milliseconds.
    pub forward_p95_ms: f64,
    /// Debiased p95 latency in milliseconds.
    pub debiased_p95_ms: f64,
    /// Multiplicative p95 latency ratio (`debiased_p95_ms / forward_p95_ms`).
    pub latency_ratio: f64,
    /// True iff `top1_gain >= 0.01 && latency_ratio <= 2.0` (`RTE-043`).
    pub enable_by_default: bool,
}

/// Evaluates whether observed accuracy gain and latency cost satisfy `RTE-043` default criteria:
/// `top1_gain >= +1.0 percentage point` (`0.01`) and `latency_ratio <= 2.0x`.
#[must_use]
pub fn evaluate_debias_tradeoff(
    forward_top1: f64,
    debiased_top1: f64,
    forward_p95_ms: f64,
    debiased_p95_ms: f64,
) -> DebiasEvaluationSummary {
    let top1_gain = if forward_top1.is_finite() && debiased_top1.is_finite() {
        debiased_top1 - forward_top1
    } else {
        0.0
    };
    let latency_ratio = if forward_p95_ms > 0.0
        && forward_p95_ms.is_finite()
        && debiased_p95_ms.is_finite()
        && debiased_p95_ms >= 0.0
    {
        debiased_p95_ms / forward_p95_ms
    } else {
        f64::INFINITY
    };
    let enable_by_default =
        top1_gain >= DEBIAS_MIN_TOP1_GAIN && latency_ratio <= DEBIAS_MAX_LATENCY_RATIO;
    DebiasEvaluationSummary {
        forward_top1,
        debiased_top1,
        top1_gain,
        forward_p95_ms,
        debiased_p95_ms,
        latency_ratio,
        enable_by_default,
    }
}
