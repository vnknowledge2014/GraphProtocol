//! Lock-free, allocation-free, time-decayed EWMA packed into one `AtomicU64` (`RTE-070`, `RTE-077`).

use std::fmt;

use crate::sync::{AtomicU64, Ordering};

/// Duration of one `now_q` tick in milliseconds (`RTE-077`).
const TICK_MS: f32 = 4.0;
const TICK_MS_F64: f64 = 4.0;

/// Staleness horizon in half-lives (`RTE-070`, `RTE-077`).
const STALE_HALF_LIVES: f64 = 5.0;

/// Maximum tolerated out-of-order lag (`< 2^16` ticks ≈ `4.37 min`, `RTE-077(b)`).
const MAX_OUT_OF_ORDER_TICKS: u32 = 1 << 16;

/// Half-range of the `u32` tick ring (`2^31` ticks ≈ `99.4 days`, `RTE-077`).
const HALF_RING_TICKS: u32 = 1 << 31;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickRelation {
    /// `now_q` equals `stored_t` or trails it by `< 2^16` ticks (`dt = 0`, keep `stored_t`).
    SameOrBehindWithinWindow,
    /// `now_q` is ahead of `stored_t` by `1..2^31` ticks and within `<= 5 * half_life_ms`.
    FreshAdvance { age_ticks: u32 },
    /// Stored value is older than `5` half-lives or ahead of `now_q` by `>= 2^16` ticks.
    StaleOrInvalidAhead,
}

#[inline]
fn classify_tick(stored_t: u32, now_q: u32, half_life_ms: f32) -> TickRelation {
    if now_q == stored_t {
        return TickRelation::SameOrBehindWithinWindow;
    }
    let age = now_q.wrapping_sub(stored_t);
    if age < HALF_RING_TICKS {
        let dt_ms = f64::from(age) * TICK_MS_F64;
        if dt_ms > STALE_HALF_LIVES * f64::from(half_life_ms) {
            TickRelation::StaleOrInvalidAhead
        } else {
            TickRelation::FreshAdvance { age_ticks: age }
        }
    } else {
        let ahead = stored_t.wrapping_sub(now_q);
        if ahead < MAX_OUT_OF_ORDER_TICKS {
            TickRelation::SameOrBehindWithinWindow
        } else {
            TickRelation::StaleOrInvalidAhead
        }
    }
}

#[inline]
fn pack(value: f32, tick_q: u32) -> u64 {
    // Normalize `-0.0` to `+0.0` so zero has a single canonical bit pattern (`0x0000_0000`).
    // Finite `f32` values never set all exponent bits (`0xFF`), so `to_bits() != u32::MAX` and
    // `pack(value, tick_q) != DecayedEwma::UNSET` (`u64::MAX`) for all `tick_q`.
    let canonical = if value == 0.0 { 0.0_f32 } else { value };
    (u64::from(canonical.to_bits()) << 32) | u64::from(tick_q)
}

#[inline]
fn unpack(word: u64) -> (f32, u32) {
    let value = f32::from_bits((word >> 32) as u32);
    let tick_q = word as u32;
    (value, tick_q)
}

/// Time-decayed exponentially weighted moving average stored in one `AtomicU64` (`RTE-070`, `RTE-077`).
///
/// Upper 32 bits store `f32::to_bits(value)` (`finite >= 0.0`); lower 32 bits store `now_q` in
/// `4 ms` ticks. [`Self::UNSET`] (`u64::MAX`) represents an unknown cell.
pub struct DecayedEwma(AtomicU64);

impl DecayedEwma {
    /// Sentinel raw word representing an uninitialized or reset cell (`RTE-070`).
    #[doc(hidden)]
    pub const UNSET: u64 = u64::MAX;

    /// Constructs an uninitialized (`UNSET`) [`DecayedEwma`].
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(Self::UNSET))
    }

    /// Updates the EWMA with `sample` at tick `now_q` using `half_life_ms` (`RTE-070`, `RTE-077`).
    ///
    /// Returns `false` without modifying state if `sample` is non-finite or negative (or if
    /// `half_life_ms` is non-finite or `<= 0.0`).
    pub fn update(&self, sample: f32, now_q: u32, half_life_ms: f32) -> bool {
        self.update_traced(sample, now_q, half_life_ms).is_some()
    }

    /// Reads the current EWMA value if initialized, within `<= 5` half-lives, and not ahead of
    /// `now_q` by `>= 2^16` ticks (`RTE-070`, `RTE-077`).
    #[must_use]
    pub fn read(&self, now_q: u32, half_life_ms: f32) -> Option<f32> {
        if !half_life_ms.is_finite() || half_life_ms <= 0.0 {
            return None;
        }
        let cur = self.0.load(Ordering::Relaxed);
        if cur == Self::UNSET {
            return None;
        }
        let (value, stored_t) = unpack(cur);
        match classify_tick(stored_t, now_q, half_life_ms) {
            TickRelation::SameOrBehindWithinWindow | TickRelation::FreshAdvance { .. } => {
                Some(value)
            }
            TickRelation::StaleOrInvalidAhead => None,
        }
    }

    /// Atomically resets the cell to `UNSET` if its stored value is older than `5` half-lives
    /// (or ahead of `now_q` by `>= 2^16` ticks), returning `true` when a reset occurred (`RTE-077`).
    pub fn reset_if_stale(&self, now_q: u32, half_life_ms: f32) -> bool {
        if !half_life_ms.is_finite() || half_life_ms <= 0.0 {
            return false;
        }
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            if cur == Self::UNSET {
                return false;
            }
            let (_value, stored_t) = unpack(cur);
            if !matches!(
                classify_tick(stored_t, now_q, half_life_ms),
                TickRelation::StaleOrInvalidAhead
            ) {
                return false;
            }
            match self.0.compare_exchange_weak(
                cur,
                Self::UNSET,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Commits a lock-free EWMA update and returns `Some((old_raw, new_raw))` of the winning CAS,
    /// or `None` when `sample` / `half_life_ms` is rejected (`RTE-070`, `RTE-077`).
    #[doc(hidden)]
    pub fn update_traced(&self, sample: f32, now_q: u32, half_life_ms: f32) -> Option<(u64, u64)> {
        if !sample.is_finite() || sample < 0.0 || !half_life_ms.is_finite() || half_life_ms <= 0.0 {
            return None;
        }
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let next = if cur == Self::UNSET {
                pack(sample, now_q)
            } else {
                let (value, stored_t) = unpack(cur);
                match classify_tick(stored_t, now_q, half_life_ms) {
                    TickRelation::SameOrBehindWithinWindow => cur,
                    TickRelation::StaleOrInvalidAhead => pack(sample, now_q),
                    TickRelation::FreshAdvance { age_ticks } => {
                        let dt_ms = (age_ticks as f32) * TICK_MS;
                        let x = dt_ms * core::f32::consts::LN_2 / half_life_ms;
                        let alpha = (-(-x).exp_m1()).clamp(0.0, 1.0);
                        let low = value.min(sample);
                        let high = value.max(sample);
                        let blended = (value + alpha * (sample - value)).clamp(low, high);
                        pack(blended, now_q)
                    }
                }
            };
            match self
                .0
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Some((cur, next)),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Returns the raw packed `u64` word (`UNSET == u64::MAX`) for concurrency verification.
    #[doc(hidden)]
    #[must_use]
    pub fn raw(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for DecayedEwma {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DecayedEwma {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw = self.0.load(Ordering::Relaxed);
        if raw == Self::UNSET {
            f.debug_tuple("DecayedEwma").field(&"UNSET").finish()
        } else {
            let (value, tick_q) = unpack(raw);
            f.debug_struct("DecayedEwma")
                .field("value", &value)
                .field("tick_q", &tick_q)
                .finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::{Arc, Barrier};

    use super::*;

    // spec: RTE-070, RTE-077
    #[test]
    fn ewma_unset_and_first_sample_seed() {
        let ewma = DecayedEwma::new();
        assert_eq!(ewma.raw(), DecayedEwma::UNSET);
        assert_eq!(ewma.read(0, 10_000.0), None);
        assert!(!ewma.reset_if_stale(0, 10_000.0));

        assert!(ewma.update(42.5, 100, 10_000.0));
        assert_eq!(ewma.read(100, 10_000.0), Some(42.5));

        // Verify `-0.0` normalizes to canonical `+0.0` and `u32::MAX` tick never collides with `UNSET`.
        let zero_max = DecayedEwma::new();
        assert!(zero_max.update(-0.0, u32::MAX, 10_000.0));
        assert_ne!(zero_max.raw(), DecayedEwma::UNSET);
        assert_eq!(zero_max.raw() >> 32, 0);
        assert_eq!(zero_max.read(u32::MAX, 10_000.0), Some(0.0));
    }

    // spec: RTE-070, RTE-077
    #[test]
    fn ewma_small_step_exp_m1_precision() {
        let ewma = DecayedEwma::new();
        assert!(ewma.update(0.0, 0, 500_000.0));
        assert!(ewma.update(1_000.0, 1, 500_000.0));

        let got = f64::from(ewma.read(1, 500_000.0).expect("value must be known"));
        let expected = 1_000.0_f64 * (1.0 - 2.0_f64.powf(-4.0 / 500_000.0));
        let rel_err = (got - expected).abs() / expected;
        assert!(
            rel_err <= 1e-4,
            "expected {expected} within relative 1e-4, got {got} (rel_err={rel_err})"
        );

        // Exact 1 and 2 half-life steps with 500,000 ms half-life stay exact.
        let hl1 = DecayedEwma::new();
        assert!(hl1.update(0.0, 0, 500_000.0));
        assert!(hl1.update(1_000.0, 125_000, 500_000.0));
        assert_eq!(hl1.read(125_000, 500_000.0), Some(500.0));

        let hl2 = DecayedEwma::new();
        assert!(hl2.update(0.0, 0, 500_000.0));
        assert!(hl2.update(1_000.0, 250_000, 500_000.0));
        assert_eq!(hl2.read(250_000, 500_000.0), Some(750.0));
    }

    // spec: RTE-070, RTE-077
    #[test]
    fn ewma_half_life_decay_and_zero_dt() {
        let ewma = DecayedEwma::new();
        assert!(ewma.update(0.0, 0, 10_000.0));

        // 2,500 ticks * 4 ms = 10,000 ms = exactly 1 half-life: 0 -> 100 yields 50.0.
        assert!(ewma.update(100.0, 2_500, 10_000.0));
        let v1 = ewma.read(2_500, 10_000.0).expect("value must be known");
        assert!((v1 - 50.0).abs() <= 1e-3, "expected ~50.0, got {v1}");

        // dt = 0 leaves the value unchanged and still returns true (committing a self-loop).
        let raw_before = ewma.raw();
        let traced = ewma
            .update_traced(999.0, 2_500, 10_000.0)
            .expect("dt=0 sample is valid");
        assert_eq!(traced, (raw_before, raw_before));
        assert!(ewma.update(0.0, 2_500, 10_000.0));
        let v_same = ewma.read(2_500, 10_000.0).expect("value must be known");
        assert_eq!(v_same, v1);

        // Two half-lives later (5,000 ticks = 20,000 ms) from 0 -> 100 yields 75.0.
        let ewma2 = DecayedEwma::new();
        assert!(ewma2.update(0.0, 10, 10_000.0));
        assert!(ewma2.update(100.0, 5_010, 10_000.0));
        let v2 = ewma2.read(5_010, 10_000.0).expect("value must be known");
        assert!((v2 - 75.0).abs() <= 1e-3, "expected ~75.0, got {v2}");
    }

    // spec: RTE-077
    #[test]
    fn ewma_rejects_non_finite_negative_and_bad_half_life() {
        let ewma = DecayedEwma::new();
        for bad_sample in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.000_1,
            -1.0,
            -f32::MIN_POSITIVE,
        ] {
            assert!(!ewma.update(bad_sample, 10, 10_000.0));
            assert_eq!(ewma.raw(), DecayedEwma::UNSET);
        }

        assert!(ewma.update(12.5, 50, 10_000.0));
        let seeded_raw = ewma.raw();
        for bad_sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5, -100.0] {
            assert!(!ewma.update(bad_sample, 100, 10_000.0));
            assert_eq!(ewma.raw(), seeded_raw);
        }
        for bad_hl in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!ewma.update(20.0, 100, bad_hl));
            assert_eq!(ewma.raw(), seeded_raw);
            assert_eq!(ewma.read(50, bad_hl), None);
            assert!(!ewma.reset_if_stale(50_000, bad_hl));
        }
    }

    // spec: RTE-077
    #[test]
    fn ewma_out_of_order_ticks_and_ahead_boundary() {
        let ewma = DecayedEwma::new();
        assert!(ewma.update(25.0, 100_000, 10_000.0));
        let stored_raw = ewma.raw();

        // Behind by 3 ticks acts as dt = 0 and preserves the later timestamp 100_000.
        assert!(ewma.update(900.0, 99_997, 10_000.0));
        assert_eq!(ewma.raw(), stored_raw);
        assert_eq!(ewma.read(99_997, 10_000.0), Some(25.0));

        // Behind by 2^16 - 1 (65,535) ticks is still inside the out-of-order window.
        assert_eq!(ewma.read(100_000 - 65_535, 10_000.0), Some(25.0));
        assert!(ewma.update(900.0, 100_000 - 65_535, 10_000.0));
        assert_eq!(ewma.raw(), stored_raw);

        // Behind by 2^16 (65,536) ticks or more is unknown on read.
        assert_eq!(ewma.read(100_000 - 65_536, 10_000.0), None);
        assert_eq!(ewma.read(100_000 - 70_000, 10_000.0), None);
    }

    // spec: RTE-070, RTE-077
    #[test]
    fn ewma_staleness_five_half_lives_reseed_and_reset() {
        // For half_life_ms = 10,000 ms, 5 half-lives = 50,000 ms = 12,500 ticks.
        let ewma = DecayedEwma::new();
        assert!(ewma.update(10.0, 1_000, 10_000.0));

        // Exactly 5 half-lives old (age = 12,500 ticks) is still known and not reset.
        assert_eq!(ewma.read(13_500, 10_000.0), Some(10.0));
        assert!(!ewma.reset_if_stale(13_500, 10_000.0));

        // 5 half-lives + 1 tick (age = 12,501 ticks = 50,004 ms) is unknown on read.
        assert_eq!(ewma.read(13_501, 10_000.0), None);

        // Updating at exact 5 half-lives decays (does NOT re-seed: 10 + 31/32 * 100 = 106.875).
        let exact_boundary = DecayedEwma::new();
        assert!(exact_boundary.update(10.0, 1_000, 10_000.0));
        assert!(exact_boundary.update(110.0, 13_500, 10_000.0));
        let decayed = exact_boundary
            .read(13_500, 10_000.0)
            .expect("known after update");
        assert!(
            (decayed - 106.875).abs() <= 1e-2,
            "expected ~106.875, got {decayed}"
        );

        // Updating an older (> 5 half-lives) cell re-seeds directly to sample.
        assert!(ewma.update(110.0, 13_501, 10_000.0));
        assert_eq!(ewma.read(13_501, 10_000.0), Some(110.0));

        // reset_if_stale clears a stale cell and keeps a fresh cell.
        let stale_cell = DecayedEwma::new();
        let fresh_cell = DecayedEwma::new();
        assert!(stale_cell.update(33.0, 100, 30_000.0)); // 5 * 30,000 ms = 150,000 ms = 37,500 ticks
        assert!(fresh_cell.update(44.0, 101, 30_000.0));

        assert!(!stale_cell.reset_if_stale(37_600, 30_000.0)); // age = 37,500 -> fresh
        assert!(stale_cell.reset_if_stale(37_601, 30_000.0)); // age = 37,501 -> cleared
        assert_eq!(stale_cell.raw(), DecayedEwma::UNSET);
        assert!(!stale_cell.reset_if_stale(37_601, 30_000.0)); // already UNSET -> false

        assert!(!fresh_cell.reset_if_stale(37_601, 30_000.0)); // age = 37,500 -> kept
        assert_eq!(fresh_cell.read(37_601, 30_000.0), Some(44.0));
    }

    // spec: RTE-077
    #[test]
    fn ewma_u32_tick_wraparound_behaves_as_16_ticks() {
        let ewma = DecayedEwma::new();
        let t0 = u32::MAX - 10;
        let t1 = 5_u32; // wrapping age = (u32::MAX - (u32::MAX - 10)) + 1 + 5 = 16 ticks = 64 ms
        assert!(ewma.update(0.0, t0, 64.0));
        assert_eq!(ewma.read(t1, 64.0), Some(0.0));

        // With half_life_ms = 64.0 ms, 16 ticks (64 ms) is exactly 1 half-life -> 50.0.
        assert!(ewma.update(100.0, t1, 64.0));
        let val = ewma.read(t1, 64.0).expect("wrapped cell must be known");
        assert!((val - 50.0).abs() <= 1e-3, "expected ~50.0, got {val}");
    }

    // spec: RTE-070
    #[test]
    fn ewma_64_threads_cas_transitions_form_single_trail() {
        const THREADS: usize = 64;
        const UPDATES_PER_THREAD: usize = 5_000;
        const TOTAL_EDGES: usize = THREADS * UPDATES_PER_THREAD;
        const HALF_LIFE_MS: f32 = 500_000.0; // 5 * 500s = 625,000 ticks > 320,000 ticks

        let ewma = Arc::new(DecayedEwma::new());
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);

        let mut global_min = f32::INFINITY;
        let mut global_max = f32::NEG_INFINITY;
        for tid in 0..THREADS {
            for step in 0..UPDATES_PER_THREAD {
                let s = 10.0 + (tid as f32) * 2.5 + ((step % 17) as f32);
                global_min = global_min.min(s);
                global_max = global_max.max(s);
            }
        }

        for tid in 0..THREADS {
            let ewma = Arc::clone(&ewma);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let mut local_edges = Vec::with_capacity(UPDATES_PER_THREAD);
                barrier.wait();
                for step in 0..UPDATES_PER_THREAD {
                    let tick = (step as u32) * (THREADS as u32) + (tid as u32) + 1;
                    let sample = 10.0 + (tid as f32) * 2.5 + ((step % 17) as f32);
                    let edge = ewma
                        .update_traced(sample, tick, HALF_LIFE_MS)
                        .expect("valid sample must commit");
                    local_edges.push(edge);
                }
                local_edges
            }));
        }

        let mut edges = Vec::with_capacity(TOTAL_EDGES);
        for h in handles {
            edges.extend(h.join().expect("worker thread must not panic"));
        }
        assert_eq!(edges.len(), 320_000);

        let final_raw = ewma.raw();
        assert_ne!(final_raw, DecayedEwma::UNSET);

        let mut net_out_minus_in: HashMap<u64, i64> = HashMap::new();
        let mut undirected: HashMap<u64, Vec<u64>> = HashMap::new();
        for &(old_raw, new_raw) in &edges {
            *net_out_minus_in.entry(old_raw).or_insert(0) += 1;
            *net_out_minus_in.entry(new_raw).or_insert(0) -= 1;
            undirected.entry(old_raw).or_default().push(new_raw);
            undirected.entry(new_raw).or_default().push(old_raw);
        }

        assert_eq!(
            net_out_minus_in.get(&DecayedEwma::UNSET).copied(),
            Some(1),
            "UNSET must have out_degree - in_degree == +1"
        );
        assert_eq!(
            net_out_minus_in.get(&final_raw).copied(),
            Some(-1),
            "final raw word must have out_degree - in_degree == -1"
        );
        for (&node, &diff) in &net_out_minus_in {
            if node != DecayedEwma::UNSET && node != final_raw {
                assert_eq!(
                    diff, 0,
                    "intermediate word {node:#x} had out - in = {diff} (lost update!)"
                );
            }
        }

        // Verify all vertices touched by any edge belong to one weakly connected component.
        let mut visited: HashSet<u64> = HashSet::with_capacity(undirected.len());
        let mut queue = VecDeque::new();
        visited.insert(DecayedEwma::UNSET);
        queue.push_back(DecayedEwma::UNSET);
        while let Some(cur) = queue.pop_front() {
            if let Some(neighbours) = undirected.get(&cur) {
                for &nxt in neighbours {
                    if visited.insert(nxt) {
                        queue.push_back(nxt);
                    }
                }
            }
        }
        assert_eq!(
            visited.len(),
            undirected.len(),
            "multigraph must form a single weakly connected component"
        );

        let (final_val, _final_tick) = unpack(final_raw);
        assert!(final_val.is_finite());
        assert!(
            final_val >= global_min && final_val <= global_max,
            "final value {final_val} outside [{global_min}, {global_max}]"
        );
    }
}
