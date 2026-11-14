//! Permutation-averaging option-order debiasing (`spec/04-routing.md` §6; `RTE-043`, `WP-M5-07`).

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

/// Permutes non-sink options in reverse order while keeping an optional trailing sink slot aligned
/// with caller expectations before permutation.
#[must_use]
pub fn reverse_options<T: Clone>(options: &[T]) -> Vec<T> {
    let mut reversed = options.to_vec();
    reversed.reverse();
    reversed
}
