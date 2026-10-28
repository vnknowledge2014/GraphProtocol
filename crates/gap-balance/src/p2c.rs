//! Allocation-free power-of-two-choices (`P2C`) replica selection (`RTE-073`..`RTE-075`, `RTE-079`, `ADR-006`, `ADR-011`).

use core::cell::Cell;
use core::cmp::Ordering as CmpOrdering;

use gap_core::ids::BranchUid;

use crate::slot::{ERROR_HALF_LIFE_MS, LATENCY_HALF_LIFE_MS, TelemetrySlot};
use crate::sync::Ordering;
use crate::time::Now;

/// Stack scratch capacity covering standard group sizes in one pass without heap allocation.
const STACK_CAP: usize = 64;

/// Minimum `1.0 - err` denominator floor preventing division by zero at `err -> 1.0` (`RTE-074`, `RTE-079`).
const MIN_SUCCESS_FLOOR: f64 = 0.05;

/// Fallback latency (`1.0 us`) when no eligible candidate has a known latency EWMA (`RTE-079(b)`).
const UNKNOWN_GROUP_LATENCY_FALLBACK: f64 = 1.0;

/// Non-zero default state if SplitMix64 ever outputs `0`.
const NONZERO_FALLBACK_STATE: u64 = 0x9e37_79b9_7f4a_7c15;

/// Process-wide seed counter mixed via SplitMix64 so each thread's local RNG draws an independent stream (`RTE-079(d)`).
static NEXT_TLS_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    static TLS_RNG: Cell<u64> = const { Cell::new(0) };
}

/// Deterministic `xorshift64` pseudo-random generator for unbiased P2C candidate sampling (`RTE-074`, `RTE-079(d)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Initializes a deterministic [`XorShift64`] from `seed` (bijectively mixed via SplitMix64 so
    /// every `u64` seed, including `0`, maps to a high-entropy non-zero state).
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        if z == 0 {
            z = NONZERO_FALLBACK_STATE;
        }
        Self { state: z }
    }

    /// Advances the generator and returns the next `u64`.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Samples an integer uniformly from `0..bound` (`bound >= 1`) using threshold rejection so
    /// there is zero modulo bias (`RTE-079(d)`).
    #[inline]
    pub fn gen_index(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        let b = bound as u64;
        let threshold = b.wrapping_neg() % b;
        loop {
            let r = self.next_u64();
            if r >= threshold {
                return (r % b) as usize;
            }
        }
    }

    /// Runs `f` with a fast thread-local [`XorShift64`] lazily seeded per thread from a process-wide
    /// counter passed through SplitMix64 (`RTE-079(d)`).
    pub fn with_thread_local<R>(f: impl FnOnce(&mut Self) -> R) -> R {
        TLS_RNG.with(|cell| {
            let state = cell.get();
            let mut rng = if state == 0 {
                // Relaxed fetch_add suffices because each thread only needs a unique initial seed.
                let seed = NEXT_TLS_SEED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Self::new(seed)
            } else {
                Self { state }
            };
            let out = f(&mut rng);
            cell.set(rng.state);
            out
        })
    }
}

impl Default for XorShift64 {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Replica candidate passed to [`select`] and [`select_excluding`] (`RTE-074`, `RTE-079`).
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// Stable branch UUIDv7 used as the final deterministic tie-breaker (`RTE-074`, `RTE-079(c)`).
    pub uid: BranchUid,
    /// Borrowed live telemetry slot for this candidate.
    pub slot: &'a TelemetrySlot,
}

#[inline]
fn is_excluded(idx: usize, excluded: &[usize]) -> bool {
    let mut i = 0;
    while i < excluded.len() {
        if excluded[i] == idx {
            return true;
        }
        i += 1;
    }
    false
}

/// Computes the exact lower median `sorted[(m - 1) / 2]` of known latencies among eligible
/// non-excluded candidates without any heap allocation (`RTE-079(b)(f)`).
fn lower_median_known_latency(cands: &[Candidate<'_>], excluded: &[usize], now: Now) -> f64 {
    let mut stack_vals = [0.0_f32; STACK_CAP];
    let mut count: usize = 0;

    for (idx, cand) in cands.iter().enumerate() {
        if is_excluded(idx, excluded) || !cand.slot.breaker.is_selectable_readonly(now) {
            continue;
        }
        if let Some(lat) = cand.slot.latency_us.read(now.q, LATENCY_HALF_LIFE_MS) {
            if count < STACK_CAP {
                stack_vals[count] = lat;
            }
            count += 1;
        }
    }

    if count == 0 {
        return UNKNOWN_GROUP_LATENCY_FALLBACK;
    }

    let target_rank = (count - 1) / 2;
    if count <= STACK_CAP {
        let slice = &mut stack_vals[..count];
        slice.sort_unstable_by(f32::total_cmp);
        return f64::from(slice[target_rank]);
    }

    // Zero-allocation exact fallback for `count > 64`: binary-search IEEE-754 `u32` bit pattern
    // over `[+0.0..=f32::MAX]` (`0x0000_0000..=0x7f7f_ffff`), which preserves `f32::total_cmp`
    // monotonically for all finite non-negative floats.
    let mut lo: u32 = 0;
    let mut hi: u32 = f32::MAX.to_bits();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mut le_count: usize = 0;
        for (idx, cand) in cands.iter().enumerate() {
            if is_excluded(idx, excluded) || !cand.slot.breaker.is_selectable_readonly(now) {
                continue;
            }
            if let Some(lat) = cand.slot.latency_us.read(now.q, LATENCY_HALF_LIFE_MS) {
                let canonical = if lat == 0.0 { 0.0_f32 } else { lat };
                if canonical.to_bits() <= mid {
                    le_count += 1;
                }
            }
        }
        if le_count > target_rank {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    f64::from(f32::from_bits(lo))
}

#[derive(Clone, Copy)]
struct ScoredCandidate {
    idx: usize,
    cost: f64,
    in_flight: u32,
    uid: BranchUid,
}

#[inline]
fn score_candidate(
    idx: usize,
    cand: &Candidate<'_>,
    now: Now,
    fallback_lat: Option<f64>,
) -> ScoredCandidate {
    let lat = match cand.slot.latency_us.read(now.q, LATENCY_HALF_LIFE_MS) {
        Some(v) => f64::from(v),
        None => fallback_lat.unwrap_or(UNKNOWN_GROUP_LATENCY_FALLBACK),
    };
    let in_flight = cand.slot.in_flight.load(Ordering::Acquire);
    let cap = f64::from(cand.slot.capacity.load(Ordering::Acquire).max(1));
    let err = cand
        .slot
        .error_rate
        .read(now.q, ERROR_HALF_LIFE_MS)
        .map_or(0.0, f64::from);

    let load_ratio = 1.0 + (f64::from(in_flight) / cap);
    let denom = (1.0 - err).max(MIN_SUCCESS_FLOOR);
    let cost = lat * (load_ratio * load_ratio) / denom;

    ScoredCandidate {
        idx,
        cost,
        in_flight,
        uid: cand.uid,
    }
}

#[inline]
fn pick_better(a: ScoredCandidate, b: ScoredCandidate) -> usize {
    match a.cost.total_cmp(&b.cost) {
        CmpOrdering::Less => a.idx,
        CmpOrdering::Greater => b.idx,
        CmpOrdering::Equal => match a.in_flight.cmp(&b.in_flight) {
            CmpOrdering::Less => a.idx,
            CmpOrdering::Greater => b.idx,
            CmpOrdering::Equal => {
                if a.uid.as_uuid().as_bytes() <= b.uid.as_uuid().as_bytes() {
                    a.idx
                } else {
                    b.idx
                }
            }
        },
    }
}

/// Selects an eligible candidate index from `cands` using power-of-two-choices (`RTE-074`, `RTE-079`).
///
/// Never mutates circuit-breaker state or `HalfOpen` probe tallies, and performs zero heap allocations.
pub fn select(cands: &[Candidate<'_>], now: Now, rng: &mut XorShift64) -> Option<usize> {
    select_excluding(cands, &[], now, rng)
}

/// Selects an eligible candidate index from `cands` while skipping slice indices listed in
/// `excluded` (`RTE-074`, `RTE-079`).
///
/// Never mutates circuit-breaker state or `HalfOpen` probe tallies, and performs zero heap allocations.
pub fn select_excluding(
    cands: &[Candidate<'_>],
    excluded: &[usize],
    now: Now,
    rng: &mut XorShift64,
) -> Option<usize> {
    let mut stack_elig = [0_usize; STACK_CAP];
    let mut eligible_count: usize = 0;

    for (idx, cand) in cands.iter().enumerate() {
        if is_excluded(idx, excluded) {
            continue;
        }
        if cand.slot.breaker.is_selectable_readonly(now) {
            if eligible_count < STACK_CAP {
                stack_elig[eligible_count] = idx;
            }
            eligible_count += 1;
        }
    }

    if eligible_count == 0 {
        return None;
    }
    if eligible_count == 1 {
        return Some(stack_elig[0]);
    }

    let first_rank = rng.gen_index(eligible_count);
    let second_raw = rng.gen_index(eligible_count - 1);
    let second_rank = if second_raw >= first_rank {
        second_raw + 1
    } else {
        second_raw
    };

    let (idx_a, idx_b) = if eligible_count <= STACK_CAP {
        (stack_elig[first_rank], stack_elig[second_rank])
    } else {
        let mut found_a: Option<usize> = None;
        let mut found_b: Option<usize> = None;
        let mut seen = 0_usize;
        for (idx, cand) in cands.iter().enumerate() {
            if is_excluded(idx, excluded) || !cand.slot.breaker.is_selectable_readonly(now) {
                continue;
            }
            if seen < STACK_CAP {
                stack_elig[seen] = idx;
            }
            if seen == first_rank {
                found_a = Some(idx);
            }
            if seen == second_rank {
                found_b = Some(idx);
            }
            seen += 1;
        }
        if seen == 0 {
            return None;
        }
        if seen == 1 {
            return Some(stack_elig[0]);
        }
        match (found_a, found_b) {
            (Some(a), Some(b)) => (a, b),
            (Some(a), None) | (None, Some(a)) => {
                let pool = seen.min(STACK_CAP);
                let pick = stack_elig[rng.gen_index(pool)];
                let b = if pick != a {
                    pick
                } else if stack_elig[0] != a {
                    stack_elig[0]
                } else {
                    stack_elig[1]
                };
                (a, b)
            }
            (None, None) => {
                let pool = seen.min(STACK_CAP);
                let r1 = rng.gen_index(pool);
                let r2_raw = rng.gen_index(pool - 1);
                let r2 = if r2_raw >= r1 { r2_raw + 1 } else { r2_raw };
                (stack_elig[r1], stack_elig[r2])
            }
        }
    };

    let cand_a = &cands[idx_a];
    let cand_b = &cands[idx_b];
    let a_unknown = cand_a
        .slot
        .latency_us
        .read(now.q, LATENCY_HALF_LIFE_MS)
        .is_none();
    let b_unknown = cand_b
        .slot
        .latency_us
        .read(now.q, LATENCY_HALF_LIFE_MS)
        .is_none();

    let median_lat = if a_unknown || b_unknown {
        Some(lower_median_known_latency(cands, excluded, now))
    } else {
        None
    };

    let scored_a = score_candidate(idx_a, cand_a, now, median_lat);
    let scored_b = score_candidate(idx_b, cand_b, now, median_lat);
    Some(pick_better(scored_a, scored_b))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::breaker::{Admission, BreakerState};
    use crate::slot::{HealthStatus, InFlightGuard, Outcome, SlotParams};
    use crate::sync::Arc;

    fn uid_with_byte(b: u8) -> BranchUid {
        BranchUid::from_unix_ms(1_700_000_000_000, [b; 10])
    }

    fn make_slot(max_in_flight: u32) -> Arc<TelemetrySlot> {
        Arc::new(TelemetrySlot::new(SlotParams {
            max_in_flight,
            default_timeout_ms: 1_000,
            label: Arc::from("p2c-branch"),
        }))
    }

    // spec: RTE-073, RTE-074, RTE-075, RTE-079
    #[test]
    fn p2c_single_two_candidates_and_literal_cost_formula() {
        let now = Now::from_ms(0);
        let s0 = make_slot(4);
        let s1 = make_slot(4);
        let uid0 = uid_with_byte(1);
        let uid1 = uid_with_byte(2);

        // Empty -> None; single candidate -> Some(0).
        let mut rng = XorShift64::new(7);
        assert_eq!(select(&[], now, &mut rng), None);
        assert_eq!(
            select(
                &[Candidate {
                    uid: uid0,
                    slot: &s0
                }],
                now,
                &mut rng
            ),
            Some(0)
        );

        // Two candidates: with s0 better for seeds under 1 setup and s1 better when s0 has load,
        // verify each of the two can be chosen.
        {
            let _g0 = InFlightGuard::acquire(&s0);
            let chosen = select(
                &[
                    Candidate {
                        uid: uid0,
                        slot: &s0,
                    },
                    Candidate {
                        uid: uid1,
                        slot: &s1,
                    },
                ],
                now,
                &mut rng,
            );
            assert_eq!(chosen, Some(1));
        }
        {
            let _g1 = InFlightGuard::acquire(&s1);
            let chosen = select(
                &[
                    Candidate {
                        uid: uid0,
                        slot: &s0,
                    },
                    Candidate {
                        uid: uid1,
                        slot: &s1,
                    },
                ],
                now,
                &mut rng,
            );
            assert_eq!(chosen, Some(0));
        }

        // Literal cost formula check (m3-01-design.md §6.8):
        // Candidate A: lat = 100 us, in_flight = 2, cap = 4, err = 0.5
        //   => cost = 100 * (1 + 2/4)^2 / max(0.05, 1 - 0.5) = 100 * 2.25 / 0.5 = 450.0
        let a = make_slot(4);
        a.record(Outcome::Ok, Duration::from_micros(100), Now::from_ms(0));
        // Move error_rate to exact 0.5 after 1 half-life (30,000 ms), keeping latency = 100 us.
        let t_eval = Now::from_ms(30_000);
        a.record(Outcome::Failure, Duration::from_micros(100), t_eval);
        let _ga1 = InFlightGuard::acquire(&a);
        let _ga2 = InFlightGuard::acquire(&a);

        let scored_a = score_candidate(
            0,
            &Candidate {
                uid: uid0,
                slot: &a,
            },
            t_eval,
            None,
        );
        assert!(
            (scored_a.cost - 450.0).abs() <= 1e-3,
            "expected literal cost 450.0, got {}",
            scored_a.cost
        );

        // Compare against Candidate B with lat = 451 us, in_flight = 0, err = 0.0 (cost = 451.0) -> A wins;
        // and Candidate C with lat = 449 us, in_flight = 0, err = 0.0 (cost = 449.0) -> C wins!
        let b = make_slot(4);
        b.record(Outcome::Ok, Duration::from_micros(451), t_eval);
        assert_eq!(
            select(
                &[
                    Candidate {
                        uid: uid1,
                        slot: &a
                    },
                    Candidate {
                        uid: uid0,
                        slot: &b
                    },
                ],
                t_eval,
                &mut rng,
            ),
            Some(0)
        );

        let c = make_slot(4);
        c.record(Outcome::Ok, Duration::from_micros(449), t_eval);
        assert_eq!(
            select(
                &[
                    Candidate {
                        uid: uid0,
                        slot: &a
                    },
                    Candidate {
                        uid: uid1,
                        slot: &c
                    },
                ],
                t_eval,
                &mut rng,
            ),
            Some(1)
        );
    }

    // spec: RTE-074, RTE-079
    #[test]
    fn p2c_error_floor_zero_cap_median_and_tie_break_order() {
        let now = Now::from_ms(1_000);
        let uid_low = uid_with_byte(1);
        let uid_high = uid_with_byte(9);

        // 1. max(0.05, 1 - err) floor: err = 0.99 and err = 1.0 produce the exact same cost!
        let s_err99 = make_slot(10);
        let s_err100 = make_slot(10);
        // Seed error_rate directly on the EWMA with 0.99 vs 1.0 and equal latency 100 us:
        let _ = s_err99
            .latency_us
            .update(100.0, now.q, LATENCY_HALF_LIFE_MS);
        let _ = s_err99.error_rate.update(0.99, now.q, ERROR_HALF_LIFE_MS);
        let _ = s_err100
            .latency_us
            .update(100.0, now.q, LATENCY_HALF_LIFE_MS);
        let _ = s_err100.error_rate.update(1.0, now.q, ERROR_HALF_LIFE_MS);

        let sc99 = score_candidate(
            0,
            &Candidate {
                uid: uid_high,
                slot: &s_err99,
            },
            now,
            None,
        );
        let sc100 = score_candidate(
            1,
            &Candidate {
                uid: uid_low,
                slot: &s_err100,
            },
            now,
            None,
        );
        assert_eq!(sc99.cost, 2_000.0); // 100.0 * 1.0 / 0.05 = 2,000.0
        assert_eq!(sc99.cost, sc100.cost);
        // Because costs are equal and in_flight == 0 on both, lower BranchUid (`uid_low`, index 1) wins!
        let mut rng = XorShift64::new(42);
        assert_eq!(
            select(
                &[
                    Candidate {
                        uid: uid_high,
                        slot: &s_err99,
                    },
                    Candidate {
                        uid: uid_low,
                        slot: &s_err100,
                    },
                ],
                now,
                &mut rng,
            ),
            Some(1)
        );

        // 2. cap = 0 behaves as cap = 1: in_flight = 1, lat = 50, err = 0 => 50 * (1 + 1/1)^2 = 200.0.
        let s_zero_cap = make_slot(0);
        s_zero_cap.capacity.store(0, Ordering::Release);
        s_zero_cap.record(Outcome::Ok, Duration::from_micros(50), now);
        let _g = InFlightGuard::acquire(&s_zero_cap);
        let sc_zero = score_candidate(
            0,
            &Candidate {
                uid: uid_low,
                slot: &s_zero_cap,
            },
            now,
            None,
        );
        assert_eq!(sc_zero.cost, 200.0);

        // 3. Unknown latency lower median:
        //    - 4 known values [80, 10, 40, 20] -> sorted [10, 20, 40, 80] -> index (4-1)/2 = 1 => 20.0
        //    - 3 known values [90, 10, 30] -> sorted [10, 30, 90] -> index (3-1)/2 = 1 => 30.0
        //    - 0 known values -> 1.0
        let k0 = make_slot(10);
        let k1 = make_slot(10);
        let k2 = make_slot(10);
        let k3 = make_slot(10);
        let unk = make_slot(10);
        k0.record(Outcome::Ok, Duration::from_micros(80), now);
        k1.record(Outcome::Ok, Duration::from_micros(10), now);
        k2.record(Outcome::Ok, Duration::from_micros(40), now);
        k3.record(Outcome::Ok, Duration::from_micros(20), now);

        let cands_4_known = [
            Candidate {
                uid: uid_with_byte(1),
                slot: &k0,
            },
            Candidate {
                uid: uid_with_byte(2),
                slot: &k1,
            },
            Candidate {
                uid: uid_with_byte(3),
                slot: &k2,
            },
            Candidate {
                uid: uid_with_byte(4),
                slot: &k3,
            },
            Candidate {
                uid: uid_with_byte(5),
                slot: &unk,
            },
        ];
        assert_eq!(lower_median_known_latency(&cands_4_known, &[], now), 20.0);
        assert_eq!(
            lower_median_known_latency(&cands_4_known[..4], &[0], now),
            20.0
        );

        let c0 = make_slot(10);
        let c1 = make_slot(10);
        let c2 = make_slot(10);
        c0.record(Outcome::Ok, Duration::from_micros(90), now);
        c1.record(Outcome::Ok, Duration::from_micros(10), now);
        c2.record(Outcome::Ok, Duration::from_micros(30), now);
        let cands_3_known = [
            Candidate {
                uid: uid_with_byte(1),
                slot: &c0,
            },
            Candidate {
                uid: uid_with_byte(2),
                slot: &c1,
            },
            Candidate {
                uid: uid_with_byte(3),
                slot: &c2,
            },
            Candidate {
                uid: uid_with_byte(4),
                slot: &unk,
            },
        ];
        assert_eq!(lower_median_known_latency(&cands_3_known, &[], now), 30.0);

        let unk2 = make_slot(10);
        let cands_none_known = [
            Candidate {
                uid: uid_low,
                slot: &unk,
            },
            Candidate {
                uid: uid_high,
                slot: &unk2,
            },
        ];
        assert_eq!(lower_median_known_latency(&cands_none_known, &[], now), 1.0);

        // 4. Tie-break order: equal cost -> lower `in_flight` wins BEFORE `BranchUid`;
        //    equal cost AND equal `in_flight` -> lower `BranchUid` byte order wins!
        let tie_low_inflight = make_slot(4); // lat = 225, in_flight = 0 => cost = 225 * 1.0 = 225.0
        let tie_high_inflight = make_slot(4); // lat = 100, in_flight = 2 => cost = 100 * 2.25 = 225.0
        tie_low_inflight.record(Outcome::Ok, Duration::from_micros(225), now);
        tie_high_inflight.record(Outcome::Ok, Duration::from_micros(100), now);
        let _gi1 = InFlightGuard::acquire(&tie_high_inflight);
        let _gi2 = InFlightGuard::acquire(&tie_high_inflight);

        // Give tie_low_inflight a HIGHER BranchUid so a swapped tie-breaker (UID before in_flight) FAILS!
        assert_eq!(
            select(
                &[
                    Candidate {
                        uid: uid_high,
                        slot: &tie_low_inflight,
                    },
                    Candidate {
                        uid: uid_low,
                        slot: &tie_high_inflight,
                    },
                ],
                now,
                &mut rng,
            ),
            Some(0)
        );

        // Equal cost AND equal in_flight -> lower BranchUid wins regardless of slice order.
        let eq_a = make_slot(4);
        let eq_b = make_slot(4);
        eq_a.record(Outcome::Ok, Duration::from_micros(50), now);
        eq_b.record(Outcome::Ok, Duration::from_micros(50), now);
        assert_eq!(
            select(
                &[
                    Candidate {
                        uid: uid_high,
                        slot: &eq_a,
                    },
                    Candidate {
                        uid: uid_low,
                        slot: &eq_b,
                    },
                ],
                now,
                &mut rng,
            ),
            Some(1)
        );
        assert_eq!(
            select(
                &[
                    Candidate {
                        uid: uid_low,
                        slot: &eq_b,
                    },
                    Candidate {
                        uid: uid_high,
                        slot: &eq_a,
                    },
                ],
                now,
                &mut rng,
            ),
            Some(0)
        );
    }

    // spec: RTE-073, RTE-079
    #[test]
    fn p2c_breaker_eligibility_no_side_effects_and_select_excluding() {
        let mut rng = XorShift64::new(99);
        let s_open = make_slot(10);
        let s_half = make_slot(10);
        let s_closed = make_slot(10);

        // Force s_open into Open via NotServing at t = 0.
        s_open.apply_health(HealthStatus::NotServing, None, Now::from_ms(0));

        // Trip s_half at t = 0 and advance to t = 5,000 so it enters HalfOpen.
        for _ in 0..20 {
            s_half.record(Outcome::Failure, Duration::from_millis(5), Now::from_ms(0));
        }
        let t5000 = Now::from_ms(5_000);
        assert_eq!(s_half.breaker().state(t5000), BreakerState::HalfOpen);

        // Make a 4th Closed candidate slower so when P2C pairs (s_half, s_slow_closed), s_half wins.
        let s_slow_closed = make_slot(10);
        s_slow_closed.record(Outcome::Ok, Duration::from_millis(500), t5000);

        let cands = [
            Candidate {
                uid: uid_with_byte(1),
                slot: &s_open,
            },
            Candidate {
                uid: uid_with_byte(2),
                slot: &s_half,
            },
            Candidate {
                uid: uid_with_byte(3),
                slot: &s_closed,
            },
            Candidate {
                uid: uid_with_byte(4),
                slot: &s_slow_closed,
            },
        ];

        // Open candidate (index 0) is never selected; HalfOpen with free probe slots IS selectable;
        // and calling `select` 500 times NEVER mutates breaker state or consumes any probe slot!
        let mut saw_half = false;
        for _ in 0..500 {
            let pick = select(&cands, t5000, &mut rng).expect("eligible candidate exists");
            assert_ne!(pick, 0, "Open candidate must never be selected");
            if pick == 1 {
                saw_half = true;
            }
        }
        assert!(
            saw_half,
            "HalfOpen candidate with free probe slots must be eligible"
        );
        assert_eq!(s_half.breaker().state(t5000), BreakerState::HalfOpen);

        // Verify all 3 probe slots on s_half are STILL untouched after 500 `select` calls!
        let p1 = match s_half.breaker().admit(t5000) {
            Admission::Probe(p) => p,
            other => panic!("expected Probe 1, got {other:?}"),
        };
        let p2 = match s_half.breaker().admit(t5000) {
            Admission::Probe(p) => p,
            other => panic!("expected Probe 2, got {other:?}"),
        };
        let p3 = match s_half.breaker().admit(t5000) {
            Admission::Probe(p) => p,
            other => panic!("expected Probe 3, got {other:?}"),
        };

        // Now all 3 probe slots on s_half are occupied -> s_half is excluded too!
        for _ in 0..100 {
            let pick = select(&cands, t5000, &mut rng).expect("closed candidates remain");
            assert!(pick == 2 || pick == 3, "expected 2 or 3, got {pick}");
        }

        // select_excluding(&cands, &[2, 3], ...) now has zero eligible candidates -> None!
        assert_eq!(select_excluding(&cands, &[2, 3], t5000, &mut rng), None);

        // Dropping 1 probe permit makes s_half eligible again -> select_excluding(&cands, &[2, 3]) returns Some(1)!
        drop(p3);
        assert_eq!(select_excluding(&cands, &[2, 3], t5000, &mut rng), Some(1));
        assert_eq!(select_excluding(&cands, &[1, 2, 3], t5000, &mut rng), None);
        drop((p1, p2));
    }

    // spec: RTE-074, RTE-079
    #[test]
    fn p2c_distinct_pair_uniformity_and_seed_determinism() {
        // 1. 100,000 draws: the two sampled indexes are ALWAYS distinct and uniform over all
        //    5 * 4 = 20 ordered pairs for n = 5 within +-10% of expected 5,000 ([4,500..=5,500]).
        let mut rng = XorShift64::new(0x1234_5678_9abc_def0);
        let mut pair_counts = [[0_u32; 5]; 5];
        for _ in 0..100_000 {
            let a = rng.gen_index(5);
            let b_raw = rng.gen_index(4);
            let b = if b_raw >= a { b_raw + 1 } else { b_raw };
            assert_ne!(a, b, "sampled pair must always be distinct");
            pair_counts[a][b] += 1;
        }
        for (i, row) in pair_counts.iter().enumerate() {
            for (j, &count) in row.iter().enumerate() {
                if i == j {
                    assert_eq!(count, 0);
                } else {
                    assert!(
                        (4_500..=5_500).contains(&count),
                        "ordered pair ({i}, {j}) count {count} outside literal +-10% [4500, 5500]"
                    );
                }
            }
        }

        // 2. Seed determinism: same seed -> identical choice sequence; different seed -> different sequence.
        let slots: Vec<Arc<TelemetrySlot>> = (0..5).map(|_| make_slot(10)).collect();
        let now = Now::from_ms(100);
        for (idx, s) in slots.iter().enumerate() {
            s.record(
                Outcome::Ok,
                Duration::from_micros(100 + (idx as u64) * 10),
                now,
            );
        }
        let cands: Vec<Candidate<'_>> = slots
            .iter()
            .enumerate()
            .map(|(i, s)| Candidate {
                uid: uid_with_byte(i as u8 + 1),
                slot: s,
            })
            .collect();

        let mut rng_a = XorShift64::new(2026);
        let mut rng_b = XorShift64::new(2026);
        let mut rng_c = XorShift64::new(2027);

        let seq_a: Vec<usize> = (0..128)
            .map(|_| select(&cands, now, &mut rng_a).expect("choice"))
            .collect();
        let seq_b: Vec<usize> = (0..128)
            .map(|_| select(&cands, now, &mut rng_b).expect("choice"))
            .collect();
        let seq_c: Vec<usize> = (0..128)
            .map(|_| select(&cands, now, &mut rng_c).expect("choice"))
            .collect();
        assert_eq!(seq_a, seq_b);
        assert_ne!(seq_a, seq_c);
    }

    // ---- Test Group 9: Herding simulation + global-argmin control (RTE-074, ADR-011) ----

    fn select_global_argmin(cands: &[Candidate<'_>], now: Now) -> usize {
        let median = Some(lower_median_known_latency(cands, &[], now));
        let mut best = score_candidate(0, &cands[0], now, median);
        for (idx, cand) in cands.iter().enumerate().skip(1) {
            let next = score_candidate(idx, cand, now, median);
            if pick_better(best, next) == idx {
                best = next;
            }
        }
        best.idx
    }

    struct SimStats {
        max_consecutive_window_swing: f64,
        total_counts: [u32; 5],
    }

    /// Runs a deterministic 100,000-request discrete-time simulation with a 1-tick feedback delay:
    /// all `BATCH_PER_TICK` requests arriving in `tick` choose a replica concurrently from the
    /// telemetry snapshot at the start of `tick` (reflecting `tick - 1`'s in-flight load and
    /// earlier completed latencies), before `tick`'s own requests take effect.
    fn run_herding_sim(base_lat_us: [f64; 5], use_p2c: bool) -> SimStats {
        const TOTAL_REQUESTS: usize = 100_000;
        const BATCH_PER_TICK: usize = 250; // 4 ticks per 1,000-request window (400 ticks total)
        const WINDOW_SIZE: usize = 1_000;
        const TICK_DT_MS: u64 = 1_500;

        let slots: [Arc<TelemetrySlot>; 5] = std::array::from_fn(|_| make_slot(100));
        let uids: [BranchUid; 5] = std::array::from_fn(|i| uid_with_byte((i as u8) + 1));
        let mut rng = XorShift64::new(0x5eed_cafe_d00d_0001);

        // Seed initial baseline latency at t = 0.
        for i in 0..5 {
            slots[i].record(
                Outcome::Ok,
                Duration::from_micros(base_lat_us[i] as u64),
                Now::from_ms(0),
            );
        }

        let mut prev_guards: Vec<InFlightGuard> = Vec::new();
        let mut prev_hits = [0_u32; 5];
        let mut total_counts = [0_u32; 5];
        let mut window_counts = [0_u32; 5];
        let mut prev_shares: Option<[f64; 5]> = None;
        let mut max_swing: f64 = 0.0;
        let mut reqs_in_window: usize = 0;

        let ticks = TOTAL_REQUESTS / BATCH_PER_TICK;
        for tick in 0..ticks {
            let now = Now::from_ms(((tick + 1) as u64) * TICK_DT_MS);

            let cands: [Candidate<'_>; 5] = std::array::from_fn(|i| Candidate {
                uid: uids[i],
                slot: &slots[i],
            });

            // All requests in `tick` observe the stale state from `tick - 1` (1-tick delay).
            let mut tick_hits = [0_u32; 5];
            for _ in 0..BATCH_PER_TICK {
                let chosen = if use_p2c {
                    select(&cands, now, &mut rng).expect("5 closed replicas")
                } else {
                    select_global_argmin(&cands, now)
                };
                tick_hits[chosen] += 1;
                total_counts[chosen] += 1;
                window_counts[chosen] += 1;
                reqs_in_window += 1;

                if reqs_in_window == WINDOW_SIZE {
                    let shares: [f64; 5] =
                        std::array::from_fn(|i| f64::from(window_counts[i]) / (WINDOW_SIZE as f64));
                    if let Some(prev) = prev_shares {
                        for i in 0..5 {
                            let swing = (shares[i] - prev[i]).abs();
                            if swing > max_swing {
                                max_swing = swing;
                            }
                        }
                    }
                    prev_shares = Some(shares);
                    window_counts = [0; 5];
                    reqs_in_window = 0;
                }
            }

            // Complete `tick - 1`'s in-flight batch and record its load-dependent latencies.
            prev_guards.clear();
            for i in 0..5 {
                if prev_hits[i] > 0 {
                    let queue_multiplier = 1.0 + 0.5 * (f64::from(prev_hits[i]) / 100.0);
                    let observed_us = (base_lat_us[i] * queue_multiplier).round() as u64;
                    slots[i].record(Outcome::Ok, Duration::from_micros(observed_us), now);
                }
            }

            // Put `tick`'s batch in flight so `tick + 1` observes it after the 1-tick delay.
            for (i, &hits) in tick_hits.iter().enumerate() {
                for _ in 0..hits {
                    prev_guards.push(InFlightGuard::acquire(&slots[i]));
                }
            }
            prev_hits = tick_hits;
        }

        drop(prev_guards);

        SimStats {
            max_consecutive_window_swing: max_swing,
            total_counts,
        }
    }

    // spec: RTE-070, RTE-074 (ADR-011)
    #[test]
    fn herding_simulation_100k_requests_p2c_vs_global_argmin_and_slow_replica() {
        // 1. 5 identical replicas (100 us base latency):
        //    - P2C keeps max consecutive-window share swing <= 0.20.
        //    - Control: global-argmin herds under 1-tick delayed feedback with swing > 0.20.
        let p2c_equal = run_herding_sim([100.0; 5], true);
        let argmin_equal = run_herding_sim([100.0; 5], false);

        assert!(
            p2c_equal.max_consecutive_window_swing <= 0.20,
            "P2C consecutive-window swing {} exceeded 0.20",
            p2c_equal.max_consecutive_window_swing
        );
        assert!(
            argmin_equal.max_consecutive_window_swing > 0.20,
            "global-argmin control failed to exhibit herding: max swing = {}",
            argmin_equal.max_consecutive_window_swing
        );

        // 2. Replica 0 is 3x slower (300 us vs 100 us):
        //    - Faster replicas receive higher share than replica 0.
        //    - Max consecutive-window share swing stays <= 0.20.
        //    - Slow replica 0 never drops to 0 share (exploration, RTE-070).
        let p2c_slow = run_herding_sim([300.0, 100.0, 100.0, 100.0, 100.0], true);
        assert!(
            p2c_slow.max_consecutive_window_swing <= 0.20,
            "P2C with 3x slow replica swung by {} (> 0.20)",
            p2c_slow.max_consecutive_window_swing
        );
        assert!(
            p2c_slow.total_counts[0] > 0,
            "3x slower replica must retain > 0 exploration share"
        );
        for fast_idx in 1..5 {
            assert!(
                p2c_slow.total_counts[fast_idx] > p2c_slow.total_counts[0],
                "faster replica {fast_idx} ({}) should serve more than 3x slow replica 0 ({})",
                p2c_slow.total_counts[fast_idx],
                p2c_slow.total_counts[0]
            );
        }
    }

    // spec: RTE-074, RTE-079
    #[test]
    fn p2c_thread_local_rng_distinct_across_eight_threads_and_seed_deterministic() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::with_capacity(8);
        for _ in 0..8 {
            let b = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                b.wait();
                let mut seq = [0_u64; 16];
                for out in &mut seq {
                    *out = XorShift64::with_thread_local(XorShift64::next_u64);
                }
                seq
            }));
        }

        let mut seqs = Vec::with_capacity(8);
        for h in handles {
            seqs.push(h.join().expect("tls rng worker"));
        }
        for i in 0..seqs.len() {
            for j in (i + 1)..seqs.len() {
                assert_ne!(
                    seqs[i], seqs[j],
                    "threads {i} and {j} produced identical thread-local RNG streams"
                );
            }
        }

        // Explicit XorShift64::new(seed) remains strictly deterministic across runs and threads.
        for seed in [0_u64, 1, 42, 0xdead_beef_cafe_babe] {
            let mut r1 = XorShift64::new(seed);
            let mut r2 = XorShift64::new(seed);
            for _ in 0..32 {
                assert_eq!(r1.next_u64(), r2.next_u64());
            }
        }
    }

    // spec: RTE-079
    #[test]
    fn p2c_select_excluding_over_64_concurrent_eligibility_never_returns_excluded() {
        let t0 = Now::from_ms(0);
        let now = Now::from_ms(5_000);
        let slots: Vec<Arc<TelemetrySlot>> = (0..100).map(|_| make_slot(16)).collect();
        for s in &slots {
            for _ in 0..20 {
                s.record(Outcome::Failure, Duration::from_millis(10), t0);
            }
        }

        let toggler_slots: Vec<Arc<TelemetrySlot>> =
            slots[50..100].iter().map(Arc::clone).collect();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_bg = std::sync::Arc::clone(&stop);

        let toggler = std::thread::spawn(move || {
            while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
                for s in &toggler_slots {
                    // Use `t0` (< open_until = 5,000) so `s` stays `Open(open_until = 5_000)` while
                    // toggling `not_serving` between `true` (49 eligible at `now`) and `false` (99 > 64 eligible at `now`).
                    s.apply_health(HealthStatus::NotServing, None, t0);
                }
                for s in &toggler_slots {
                    s.apply_health(HealthStatus::Serving, None, t0);
                }
            }
        });

        let cands: Vec<Candidate<'_>> = slots
            .iter()
            .enumerate()
            .map(|(i, s)| Candidate {
                uid: uid_with_byte(i as u8),
                slot: s,
            })
            .collect();

        let mut rng = XorShift64::new(0x55aa_1234_9876_fedc);
        for _ in 0..200_000 {
            let picked = select_excluding(&cands, &[0], now, &mut rng);
            let idx = picked.expect("members 1..50 stay eligible");
            assert_ne!(idx, 0, "excluded index 0 must never be returned");
            assert!(idx < 100);
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        toggler.join().expect("toggler thread clean");
    }
}
