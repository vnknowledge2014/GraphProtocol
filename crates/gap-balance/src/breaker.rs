//! Lock-free per-branch circuit breaker with 10x3 s rolling bucket window and 3-probe `HalfOpen`
//! majority FSM (`RTE-080`..`RTE-085`, `OPS-020`).

use std::fmt;
use std::time::Duration;

use crate::slot::{HealthStatus, Outcome};
use crate::sync::{Arc, AtomicU64, Ordering};
use crate::time::Now;

/// Width of each rolling-window bucket in milliseconds (`3 s`, `RTE-080`).
const BUCKET_MS: u64 = 3_000;

/// Number of buckets in the `30 s` rolling window (`10 * 3 s`, `RTE-080`).
const NUM_BUCKETS: usize = 10;

/// Minimum calls in the active window before trip evaluation (`RTE-080`, `RTE-084`).
const MIN_WINDOW_CALLS: u64 = 20;

/// Maximum per-bucket 12-bit counter before proportional halving (`RTE-084`).
const BUCKET_COUNTER_CAP: u64 = 4_094;
const BUCKET_COUNTER_MASK: u64 = 0xFFF;
const BUCKET_CYCLE_BITS: u32 = 22;
const BUCKET_CYCLE_MASK: u64 = (1_u64 << BUCKET_CYCLE_BITS) - 1;
const BUCKET_EMPTY: u64 = u64::MAX;

/// Modulus for `closed_gen` (`0..63`, never `63` so packed bucket words never equal `u64::MAX`).
const CLOSED_GEN_MOD: u8 = 63;

/// Continuous `Closed` duration (`5 min`) that resets backoff level `k` to `0` (`RTE-085`).
const K_RESET_CLOSED_MS: u64 = 300_000;

/// Maximum backoff exponent `k` (`Open(6)` = `60 s`, `RTE-085`).
const MAX_K: u8 = 6;

/// Maximum concurrent and total probes per `HalfOpen` episode (`RTE-085`).
const MAX_EPISODE_PROBES: u8 = 3;

/// Probe majority needed to close or reopen an episode (`2` of `3`, `RTE-085`).
const PROBE_MAJORITY: u8 = 2;

/// `retry_after` returned when `HalfOpen` has all probe slots occupied (`1 s`, `RTE-085`).
const HALF_OPEN_RETRY_AFTER: Duration = Duration::from_secs(1);

/// `retry_after` returned when `NotServing` holds the breaker `Open` past `open_until` (`1 s`, `RTE-085`).
const NOT_SERVING_HOLD_RETRY_AFTER: Duration = Duration::from_millis(1_000);

/// Maximum out-of-order lag in `30 s` cycles (`2` cycles = `60 s`) treated as behind a newer
/// concurrent bucket write rather than a wrapped newer epoch (`RTE-084`).
const MAX_OUT_OF_ORDER_CYCLES: u64 = 2;

/// Flapping window duration (`10 min`, `RTE-082`, `RTE-085`).
const FLAPPING_WINDOW_MS: u64 = 600_000;

/// Transition count threshold (`> 6` inside `10 min`) indicating flapping (`RTE-082`, `RTE-085`).
const FLAPPING_THRESHOLD: usize = 6;

/// Size of the recent-transition timestamp ring (`RTE-085`).
const TRANSITION_RING_LEN: usize = 8;
const TRANSITION_UNSET: u64 = u64::MAX;

const PAYLOAD_BITS: u32 = 51;
const PAYLOAD_MASK: u64 = (1_u64 << PAYLOAD_BITS) - 1;
const EPISODE_BITS: u32 = 45;
const EPISODE_MASK: u64 = (1_u64 << EPISODE_BITS) - 1;

/// Returns the `Open(k)` duration in milliseconds: `5, 10, 20, 40, 60, 60, 60 s` (`RTE-085`).
#[inline]
const fn open_duration_ms(k: u8) -> u64 {
    match k {
        0 => 5_000,
        1 => 10_000,
        2 => 20_000,
        3 => 40_000,
        _ => 60_000,
    }
}

/// Observable circuit-breaker state (`RTE-080`, `RTE-082`, `RTE-085`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakerState {
    /// Normal operation; calls admitted and counted in the `30 s` window (`0` on gauge).
    Closed,
    /// Recovery trial; up to `3` probes admitted per episode (`1` on gauge).
    HalfOpen,
    /// Tripped or forced `NOT_SERVING`; calls rejected immediately (`2` on gauge).
    Open,
}

impl BreakerState {
    #[inline]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "Closed",
            Self::HalfOpen => "HalfOpen",
            Self::Open => "Open",
        }
    }

    #[inline]
    const fn gauge_value(self) -> f64 {
        match self {
            Self::Closed => 0.0,
            Self::HalfOpen => 1.0,
            Self::Open => 2.0,
        }
    }
}

/// Result of [`Breaker::admit`] (`RTE-081`, `RTE-085`).
#[derive(Debug)]
pub enum Admission {
    /// Breaker is [`BreakerState::Closed`]; normal call admitted.
    Allowed,
    /// Breaker is [`BreakerState::HalfOpen`]; caller holds one of the episode's probe slots.
    Probe(ProbePermit),
    /// Breaker is [`BreakerState::Open`] or all `HalfOpen` probe slots are taken (`RTE-085`).
    Rejected {
        /// Suggested client retry delay (`retry.afterMs`).
        retry_after: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnpackedBucket {
    closed_gen: u8,
    cycle: u64,
    ok: u64,
    fail: u64,
    slow: u64,
}

#[inline]
fn pack_bucket(b: UnpackedBucket) -> u64 {
    ((u64::from(b.closed_gen) & 0x3F) << 58)
        | ((b.cycle & BUCKET_CYCLE_MASK) << 36)
        | ((b.ok & BUCKET_COUNTER_MASK) << 24)
        | ((b.fail & BUCKET_COUNTER_MASK) << 12)
        | (b.slow & BUCKET_COUNTER_MASK)
}

#[inline]
fn unpack_bucket(word: u64) -> Option<UnpackedBucket> {
    if word == BUCKET_EMPTY {
        return None;
    }
    Some(UnpackedBucket {
        closed_gen: ((word >> 58) & 0x3F) as u8,
        cycle: (word >> 36) & BUCKET_CYCLE_MASK,
        ok: (word >> 24) & BUCKET_COUNTER_MASK,
        fail: (word >> 12) & BUCKET_COUNTER_MASK,
        slow: word & BUCKET_COUNTER_MASK,
    })
}

/// Returns `true` iff a bucket at `slot_idx` (`0..10`) carrying 22-bit `bucket_cycle_tag` lies
/// within the active 10-epoch window `[cur_epoch.saturating_sub(9), cur_epoch]` (`RTE-084`).
#[inline]
fn is_bucket_in_window(slot_idx: usize, bucket_cycle_tag: u64, cur_epoch: u64) -> bool {
    let cur_full_cycle = cur_epoch / (NUM_BUCKETS as u64);
    let cur_idx = (cur_epoch % (NUM_BUCKETS as u64)) as usize;
    let cur_cycle_tag = cur_full_cycle & BUCKET_CYCLE_MASK;
    let cycle_dist = cur_cycle_tag.wrapping_sub(bucket_cycle_tag) & BUCKET_CYCLE_MASK;
    if slot_idx <= cur_idx {
        cycle_dist == 0
    } else {
        cur_full_cycle > 0 && cycle_dist == 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnpackedState {
    state: BreakerState,
    k: u8,
    not_serving: bool,
    has_opened: bool,
    closed_gen: u8,
    /// `closed_since_ms` in `Closed`, `open_until_ms` in `Open`, unused in `HalfOpen`.
    timestamp_ms: u64,
    in_flight_probes: u8,
    ok_probes: u8,
    fail_probes: u8,
    episode: u64,
}

#[inline]
fn pack_state(s: UnpackedState) -> u64 {
    let state_bits: u64 = match s.state {
        BreakerState::Closed => 0,
        BreakerState::HalfOpen => 1,
        BreakerState::Open => 2,
    };
    let payload = match s.state {
        BreakerState::Closed | BreakerState::Open => s.timestamp_ms.min(PAYLOAD_MASK),
        BreakerState::HalfOpen => {
            (u64::from(s.in_flight_probes) & 0x3)
                | ((u64::from(s.ok_probes) & 0x3) << 2)
                | ((u64::from(s.fail_probes) & 0x3) << 4)
                | ((s.episode & EPISODE_MASK) << 6)
        }
    };
    (state_bits << 62)
        | (u64::from(s.k & 0x7) << 59)
        | (u64::from(s.not_serving) << 58)
        | (u64::from(s.has_opened) << 57)
        | (u64::from(s.closed_gen & 0x3F) << 51)
        | payload
}

#[inline]
fn unpack_state(word: u64) -> UnpackedState {
    let state = match (word >> 62) & 0x3 {
        0 => BreakerState::Closed,
        1 => BreakerState::HalfOpen,
        _ => BreakerState::Open,
    };
    let k = ((word >> 59) & 0x7) as u8;
    let not_serving = ((word >> 58) & 0x1) != 0;
    let has_opened = ((word >> 57) & 0x1) != 0;
    let closed_gen = ((word >> 51) & 0x3F) as u8;
    let payload = word & PAYLOAD_MASK;

    match state {
        BreakerState::Closed | BreakerState::Open => UnpackedState {
            state,
            k,
            not_serving,
            has_opened,
            closed_gen,
            timestamp_ms: payload,
            in_flight_probes: 0,
            ok_probes: 0,
            fail_probes: 0,
            episode: 0,
        },
        BreakerState::HalfOpen => UnpackedState {
            state,
            k,
            not_serving,
            has_opened,
            closed_gen,
            timestamp_ms: 0,
            in_flight_probes: (payload & 0x3) as u8,
            ok_probes: ((payload >> 2) & 0x3) as u8,
            fail_probes: ((payload >> 4) & 0x3) as u8,
            episode: (payload >> 6) & EPISODE_MASK,
        },
    }
}

struct BreakerInner {
    default_timeout_ms: u32,
    label: Arc<str>,
    state_word: AtomicU64,
    next_episode: AtomicU64,
    buckets: [AtomicU64; NUM_BUCKETS],
    transitions: [AtomicU64; TRANSITION_RING_LEN],
    transition_seq: AtomicU64,
}

impl fmt::Debug for BreakerInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Acquire synchronizes with the latest Release/AcqRel transition on `state_word`.
        let unpacked = unpack_state(self.state_word.load(Ordering::Acquire));
        f.debug_struct("BreakerInner")
            .field("default_timeout_ms", &self.default_timeout_ms)
            .field("label", &self.label)
            .field("state", &unpacked.state)
            .field("k", &unpacked.k)
            .field("not_serving", &unpacked.not_serving)
            .finish()
    }
}

impl BreakerInner {
    fn new(default_timeout_ms: u32, label: Arc<str>) -> Self {
        let initial = UnpackedState {
            state: BreakerState::Closed,
            k: 0,
            not_serving: false,
            has_opened: false,
            closed_gen: 0,
            timestamp_ms: 0,
            in_flight_probes: 0,
            ok_probes: 0,
            fail_probes: 0,
            episode: 0,
        };
        Self {
            default_timeout_ms,
            label,
            state_word: AtomicU64::new(pack_state(initial)),
            next_episode: AtomicU64::new(1),
            buckets: std::array::from_fn(|_| AtomicU64::new(BUCKET_EMPTY)),
            transitions: std::array::from_fn(|_| AtomicU64::new(TRANSITION_UNSET)),
            transition_seq: AtomicU64::new(0),
        }
    }

    fn emit_transition(
        &self,
        from: BreakerState,
        to: BreakerState,
        k: u8,
        reason: &'static str,
        now: Now,
    ) {
        // Relaxed fetch_add suffices to assign each concurrent transition a distinct ring slot.
        let seq = self.transition_seq.fetch_add(1, Ordering::Relaxed);
        let slot = (seq as usize) % TRANSITION_RING_LEN;
        // Release publishes the transition timestamp to subsequent `Acquire` loads in `flapping`.
        self.transitions[slot].store(now.ms, Ordering::Release);

        tracing::info!(
            branch = &*self.label,
            from = from.as_str(),
            to = to.as_str(),
            k = k,
            reason = reason,
            "circuit breaker state transition"
        );
        metrics::gauge!(
            "gap_breaker_state",
            "branch" => metrics::SharedString::from(Arc::clone(&self.label))
        )
        .set(to.gauge_value());
    }

    fn clear_buckets_except_gen(&self, active_gen: u8) {
        for bucket in &self.buckets {
            // Acquire observes prior bucket writes before deciding whether to clear stale generations.
            let mut cur = bucket.load(Ordering::Acquire);
            while let Some(unpacked) = unpack_bucket(cur) {
                if unpacked.closed_gen == active_gen {
                    break;
                }
                // AcqRel/Acquire ensures clearing an old generation never overwrites a concurrent active_gen write.
                match bucket.compare_exchange_weak(
                    cur,
                    BUCKET_EMPTY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }
        }
    }

    /// Materializes `Open -> HalfOpen` if the open timer has elapsed and Health is not `NotServing`.
    fn advance_timer_if_due(&self, now: Now) -> u64 {
        // Acquire observes the most recent state transition and open_until deadline.
        let mut cur_raw = self.state_word.load(Ordering::Acquire);
        loop {
            let cur = unpack_state(cur_raw);
            if cur.state != BreakerState::Open || cur.not_serving || now.ms < cur.timestamp_ms {
                return cur_raw;
            }
            // Relaxed fetch_add suffices to mint a unique 45-bit episode tag for the candidate CAS.
            let episode = self.next_episode.fetch_add(1, Ordering::Relaxed) & EPISODE_MASK;
            let next = UnpackedState {
                state: BreakerState::HalfOpen,
                k: cur.k,
                not_serving: false,
                has_opened: true,
                closed_gen: cur.closed_gen,
                timestamp_ms: 0,
                in_flight_probes: 0,
                ok_probes: 0,
                fail_probes: 0,
                episode,
            };
            let next_raw = pack_state(next);
            // AcqRel/Acquire guarantees only one thread wins Open -> HalfOpen and publishes episode = 0 probes.
            match self.state_word.compare_exchange_weak(
                cur_raw,
                next_raw,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.emit_transition(
                        BreakerState::Open,
                        BreakerState::HalfOpen,
                        cur.k,
                        "open_timer_elapsed",
                        now,
                    );
                    return next_raw;
                }
                Err(actual) => cur_raw = actual,
            }
        }
    }

    #[inline]
    fn compute_closed_open_k(cur: UnpackedState, now: Now) -> u8 {
        if !cur.has_opened || now.ms.saturating_sub(cur.timestamp_ms) >= K_RESET_CLOSED_MS {
            0
        } else {
            (cur.k + 1).min(MAX_K)
        }
    }
}

/// RAII probe token issued by [`Breaker::admit`] during [`BreakerState::HalfOpen`] (`RTE-085`).
///
/// Calling [`ProbePermit::record`] records the probe outcome toward the 2-of-3 majority decision.
/// Dropping the permit without calling `record` releases the probe slot and counts as neither
/// success nor failure so cancelled futures cannot stall recovery.
#[must_use = "dropping ProbePermit without calling record() releases the probe slot as cancelled"]
#[derive(Debug)]
pub struct ProbePermit {
    inner: Arc<BreakerInner>,
    episode: u64,
    recorded: bool,
}

impl ProbePermit {
    /// Consumes the permit and records its outcome (`Ok`/`ClientError` = success, `Failure` = failure)
    /// against the `HalfOpen` episode (`RTE-085`).
    pub fn record(mut self, outcome: Outcome, _latency: Duration, now: Now) {
        self.recorded = true;
        // Acquire loads the active HalfOpen episode word and probe tally.
        let mut cur_raw = self.inner.state_word.load(Ordering::Acquire);
        loop {
            let cur = unpack_state(cur_raw);
            if cur.state != BreakerState::HalfOpen || cur.episode != self.episode {
                return;
            }
            let next_in_flight = cur.in_flight_probes.saturating_sub(1);
            let is_success = matches!(outcome, Outcome::Ok | Outcome::ClientError);

            if is_success {
                let next_ok = cur.ok_probes.saturating_add(1);
                if next_ok >= PROBE_MAJORITY {
                    let next_gen = (cur.closed_gen + 1) % CLOSED_GEN_MOD;
                    let closed = UnpackedState {
                        state: BreakerState::Closed,
                        k: cur.k,
                        not_serving: false,
                        has_opened: true,
                        closed_gen: next_gen,
                        timestamp_ms: now.ms,
                        in_flight_probes: 0,
                        ok_probes: 0,
                        fail_probes: 0,
                        episode: 0,
                    };
                    // AcqRel/Acquire atomically closes the episode and bumps closed_gen to invalidate old buckets.
                    match self.inner.state_word.compare_exchange_weak(
                        cur_raw,
                        pack_state(closed),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            self.inner.clear_buckets_except_gen(next_gen);
                            self.inner.emit_transition(
                                BreakerState::HalfOpen,
                                BreakerState::Closed,
                                cur.k,
                                "probe_succeeded",
                                now,
                            );
                            return;
                        }
                        Err(actual) => cur_raw = actual,
                    }
                } else {
                    let stay = UnpackedState {
                        in_flight_probes: next_in_flight,
                        ok_probes: next_ok,
                        ..cur
                    };
                    // AcqRel/Acquire serializes probe tally updates across concurrent probes.
                    match self.inner.state_word.compare_exchange_weak(
                        cur_raw,
                        pack_state(stay),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return,
                        Err(actual) => cur_raw = actual,
                    }
                }
            } else {
                let next_fail = cur.fail_probes.saturating_add(1);
                if next_fail >= PROBE_MAJORITY {
                    let next_k = (cur.k + 1).min(MAX_K);
                    let open_until = now.ms.saturating_add(open_duration_ms(next_k));
                    let reopened = UnpackedState {
                        state: BreakerState::Open,
                        k: next_k,
                        not_serving: cur.not_serving,
                        has_opened: true,
                        closed_gen: cur.closed_gen,
                        timestamp_ms: open_until,
                        in_flight_probes: 0,
                        ok_probes: 0,
                        fail_probes: 0,
                        episode: 0,
                    };
                    // AcqRel/Acquire atomically reopens the breaker on the 2nd failed probe.
                    match self.inner.state_word.compare_exchange_weak(
                        cur_raw,
                        pack_state(reopened),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            self.inner.emit_transition(
                                BreakerState::HalfOpen,
                                BreakerState::Open,
                                next_k,
                                "probe_failed",
                                now,
                            );
                            return;
                        }
                        Err(actual) => cur_raw = actual,
                    }
                } else {
                    let stay = UnpackedState {
                        in_flight_probes: next_in_flight,
                        fail_probes: next_fail,
                        ..cur
                    };
                    // AcqRel/Acquire serializes probe failure tally updates across concurrent probes.
                    match self.inner.state_word.compare_exchange_weak(
                        cur_raw,
                        pack_state(stay),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return,
                        Err(actual) => cur_raw = actual,
                    }
                }
            }
        }
    }
}

impl Drop for ProbePermit {
    fn drop(&mut self) {
        if self.recorded {
            return;
        }
        // Acquire loads the active HalfOpen episode word before releasing the cancelled probe slot.
        let mut cur_raw = self.inner.state_word.load(Ordering::Acquire);
        loop {
            let cur = unpack_state(cur_raw);
            if cur.state != BreakerState::HalfOpen || cur.episode != self.episode {
                return;
            }
            let next = UnpackedState {
                in_flight_probes: cur.in_flight_probes.saturating_sub(1),
                ..cur
            };
            // AcqRel/Acquire releases the slot without losing concurrent ok/fail tally updates.
            match self.inner.state_word.compare_exchange_weak(
                cur_raw,
                pack_state(next),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => cur_raw = actual,
            }
        }
    }
}

/// Lock-free per-branch circuit breaker (`RTE-080`..`RTE-085`).
#[derive(Debug)]
pub struct Breaker {
    inner: Arc<BreakerInner>,
}

impl Breaker {
    /// Constructs a new [`Breaker`] in [`BreakerState::Closed`].
    #[must_use]
    pub fn new(default_timeout_ms: u32, label: Arc<str>) -> Self {
        Self {
            inner: Arc::new(BreakerInner::new(default_timeout_ms, label)),
        }
    }

    /// Checks whether a call is admitted at `now`, issuing a [`ProbePermit`] in `HalfOpen`
    /// when a probe slot is free (`RTE-081`, `RTE-085`).
    pub fn admit(&self, now: Now) -> Admission {
        let mut cur_raw = self.inner.advance_timer_if_due(now);
        loop {
            let cur = unpack_state(cur_raw);
            match cur.state {
                BreakerState::Closed => return Admission::Allowed,
                BreakerState::Open => {
                    if !cur.not_serving && now.ms >= cur.timestamp_ms {
                        cur_raw = self.inner.advance_timer_if_due(now);
                        continue;
                    }
                    let rem_ms = cur.timestamp_ms.saturating_sub(now.ms);
                    let retry_after = if rem_ms == 0 {
                        NOT_SERVING_HOLD_RETRY_AFTER
                    } else {
                        Duration::from_millis(rem_ms)
                    };
                    return Admission::Rejected { retry_after };
                }
                BreakerState::HalfOpen => {
                    let used = cur
                        .in_flight_probes
                        .saturating_add(cur.ok_probes)
                        .saturating_add(cur.fail_probes);
                    if used >= MAX_EPISODE_PROBES {
                        return Admission::Rejected {
                            retry_after: HALF_OPEN_RETRY_AFTER,
                        };
                    }
                    let next = UnpackedState {
                        in_flight_probes: cur.in_flight_probes + 1,
                        ..cur
                    };
                    // AcqRel/Acquire ensures at most 3 ProbePermits can be reserved in an episode.
                    match self.inner.state_word.compare_exchange_weak(
                        cur_raw,
                        pack_state(next),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            return Admission::Probe(ProbePermit {
                                inner: Arc::clone(&self.inner),
                                episode: cur.episode,
                                recorded: false,
                            });
                        }
                        Err(actual) => cur_raw = actual,
                    }
                }
            }
        }
    }

    /// Returns the breaker's state at `now` (`RTE-080`, `RTE-085`).
    #[must_use]
    pub fn state(&self, now: Now) -> BreakerState {
        let raw = self.inner.advance_timer_if_due(now);
        unpack_state(raw).state
    }

    /// Returns `true` iff strictly more than `6` transitions occurred in the last `10` minutes
    /// (`600,000 ms`, `RTE-082`, `RTE-085`).
    #[must_use]
    pub fn flapping(&self, now: Now) -> bool {
        let _ = self.inner.advance_timer_if_due(now);
        let mut recent = 0_usize;
        for slot in &self.inner.transitions {
            // Acquire pairs with the Release store in `emit_transition`.
            let ts = slot.load(Ordering::Acquire);
            if ts != TRANSITION_UNSET
                && ts <= now.ms
                && now.ms.saturating_sub(ts) < FLAPPING_WINDOW_MS
            {
                recent += 1;
            }
        }
        recent > FLAPPING_THRESHOLD
    }

    /// Read-only eligibility check for P2C selection that NEVER mutates breaker state or probe
    /// tallies (`RTE-073`, `RTE-079(e)`).
    #[must_use]
    pub(crate) fn is_selectable_readonly(&self, now: Now) -> bool {
        // Acquire observes the latest state transition and probe reservation count without writing.
        let s = unpack_state(self.inner.state_word.load(Ordering::Acquire));
        match s.state {
            BreakerState::Closed => true,
            BreakerState::HalfOpen => {
                s.in_flight_probes
                    .saturating_add(s.ok_probes)
                    .saturating_add(s.fail_probes)
                    < MAX_EPISODE_PROBES
            }
            BreakerState::Open => !s.not_serving && now.ms >= s.timestamp_ms,
        }
    }

    /// Records a non-probe call completion into the rolling bucket window when `Closed` (`RTE-084`, `RTE-085`).
    pub(crate) fn record_call(&self, outcome: Outcome, latency: Duration, now: Now) {
        let raw = self.inner.advance_timer_if_due(now);
        let initial = unpack_state(raw);
        if initial.state != BreakerState::Closed {
            return;
        }
        let my_gen = initial.closed_gen;

        let epoch = now.ms / BUCKET_MS;
        let idx = (epoch % (NUM_BUCKETS as u64)) as usize;
        let cycle = (epoch / (NUM_BUCKETS as u64)) & BUCKET_CYCLE_MASK;

        let (inc_ok, inc_fail) = match outcome {
            Outcome::Ok | Outcome::ClientError => (1_u64, 0_u64),
            Outcome::Failure => (0_u64, 1_u64),
        };
        let is_slow = latency.as_nanos().saturating_mul(5)
            > u128::from(self.inner.default_timeout_ms) * 4_000_000;
        let inc_slow = u64::from(is_slow);

        let bucket = &self.inner.buckets[idx];
        // Acquire observes previous bucket state before atomic lazy-reset / increment.
        let mut cur_b = bucket.load(Ordering::Acquire);
        loop {
            // Acquire re-verifies the breaker is still in the same `Closed` generation.
            let check = unpack_state(self.inner.state_word.load(Ordering::Acquire));
            if check.state != BreakerState::Closed || check.closed_gen != my_gen {
                return;
            }

            let next_b = match unpack_bucket(cur_b) {
                Some(b) if b.closed_gen == my_gen => {
                    let cycle_dist = cycle.wrapping_sub(b.cycle) & BUCKET_CYCLE_MASK;
                    if cycle_dist > BUCKET_CYCLE_MASK - MAX_OUT_OF_ORDER_CYCLES {
                        return;
                    }
                    if cycle_dist == 0 {
                        let (mut ok, mut fail, mut slow) = (b.ok, b.fail, b.slow);
                        if ok >= BUCKET_COUNTER_CAP
                            || fail >= BUCKET_COUNTER_CAP
                            || slow >= BUCKET_COUNTER_CAP
                        {
                            ok >>= 1;
                            fail >>= 1;
                            slow >>= 1;
                        }
                        UnpackedBucket {
                            closed_gen: my_gen,
                            cycle,
                            ok: ok + inc_ok,
                            fail: fail + inc_fail,
                            slow: slow + inc_slow,
                        }
                    } else {
                        UnpackedBucket {
                            closed_gen: my_gen,
                            cycle,
                            ok: inc_ok,
                            fail: inc_fail,
                            slow: inc_slow,
                        }
                    }
                }
                _ => UnpackedBucket {
                    closed_gen: my_gen,
                    cycle,
                    ok: inc_ok,
                    fail: inc_fail,
                    slow: inc_slow,
                },
            };

            // AcqRel/Acquire makes lazy epoch reset + counter increment a single linearizable step.
            match bucket.compare_exchange_weak(
                cur_b,
                pack_bucket(next_b),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => cur_b = actual,
            }
        }

        // Sum buckets belonging to `my_gen` within `[epoch.saturating_sub(9), epoch]` modulo 2^22 cycles.
        let mut total_ok = 0_u64;
        let mut total_fail = 0_u64;
        let mut total_slow = 0_u64;
        for (slot_idx, b_atom) in self.inner.buckets.iter().enumerate() {
            // Acquire observes all committed increments before evaluating the trip threshold.
            if let Some(b) = unpack_bucket(b_atom.load(Ordering::Acquire))
                && b.closed_gen == my_gen
                && is_bucket_in_window(slot_idx, b.cycle, epoch)
            {
                total_ok += b.ok;
                total_fail += b.fail;
                total_slow += b.slow;
            }
        }

        let calls = total_ok + total_fail;
        let should_trip =
            calls >= MIN_WINDOW_CALLS && (total_fail * 2 >= calls || total_slow * 5 >= calls * 4);
        if !should_trip {
            return;
        }

        // Acquire loads the current state word to attempt Closed -> Open transition.
        let mut cur_s_raw = self.inner.state_word.load(Ordering::Acquire);
        loop {
            let cur_s = unpack_state(cur_s_raw);
            if cur_s.state != BreakerState::Closed || cur_s.closed_gen != my_gen {
                return;
            }
            let next_k = BreakerInner::compute_closed_open_k(cur_s, now);
            let open_until = now.ms.saturating_add(open_duration_ms(next_k));
            let opened = UnpackedState {
                state: BreakerState::Open,
                k: next_k,
                not_serving: cur_s.not_serving,
                has_opened: true,
                closed_gen: cur_s.closed_gen,
                timestamp_ms: open_until,
                in_flight_probes: 0,
                ok_probes: 0,
                fail_probes: 0,
                episode: 0,
            };
            // AcqRel/Acquire ensures at most one thread trips Closed -> Open for this generation.
            match self.inner.state_word.compare_exchange_weak(
                cur_s_raw,
                pack_state(opened),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.inner.emit_transition(
                        BreakerState::Closed,
                        BreakerState::Open,
                        next_k,
                        "window_threshold",
                        now,
                    );
                    return;
                }
                Err(actual) => cur_s_raw = actual,
            }
        }
    }

    /// Applies an upstream NGC `Health` signal (`RTE-083`, `RTE-085`).
    pub(crate) fn on_health(&self, status: HealthStatus, now: Now) {
        let mut cur_raw = self.inner.advance_timer_if_due(now);
        match status {
            HealthStatus::NotServing => loop {
                let cur = unpack_state(cur_raw);
                match cur.state {
                    BreakerState::Open => {
                        if cur.not_serving {
                            return;
                        }
                        let held = UnpackedState {
                            not_serving: true,
                            ..cur
                        };
                        // AcqRel/Acquire latches `not_serving` without changing `k` or `open_until`.
                        match self.inner.state_word.compare_exchange_weak(
                            cur_raw,
                            pack_state(held),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => return,
                            Err(actual) => cur_raw = actual,
                        }
                    }
                    BreakerState::Closed => {
                        let next_k = BreakerInner::compute_closed_open_k(cur, now);
                        let open_until = now.ms.saturating_add(open_duration_ms(next_k));
                        let opened = UnpackedState {
                            state: BreakerState::Open,
                            k: next_k,
                            not_serving: true,
                            has_opened: true,
                            closed_gen: cur.closed_gen,
                            timestamp_ms: open_until,
                            in_flight_probes: 0,
                            ok_probes: 0,
                            fail_probes: 0,
                            episode: 0,
                        };
                        // AcqRel/Acquire transitions Closed -> Open and latches the NotServing hold.
                        match self.inner.state_word.compare_exchange_weak(
                            cur_raw,
                            pack_state(opened),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                self.inner.emit_transition(
                                    BreakerState::Closed,
                                    BreakerState::Open,
                                    next_k,
                                    "health_not_serving",
                                    now,
                                );
                                return;
                            }
                            Err(actual) => cur_raw = actual,
                        }
                    }
                    BreakerState::HalfOpen => {
                        let next_k = (cur.k + 1).min(MAX_K);
                        let open_until = now.ms.saturating_add(open_duration_ms(next_k));
                        let opened = UnpackedState {
                            state: BreakerState::Open,
                            k: next_k,
                            not_serving: true,
                            has_opened: true,
                            closed_gen: cur.closed_gen,
                            timestamp_ms: open_until,
                            in_flight_probes: 0,
                            ok_probes: 0,
                            fail_probes: 0,
                            episode: 0,
                        };
                        // AcqRel/Acquire aborts the HalfOpen episode into Open(k + 1) with NotServing latched.
                        match self.inner.state_word.compare_exchange_weak(
                            cur_raw,
                            pack_state(opened),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                self.inner.emit_transition(
                                    BreakerState::HalfOpen,
                                    BreakerState::Open,
                                    next_k,
                                    "health_not_serving",
                                    now,
                                );
                                return;
                            }
                            Err(actual) => cur_raw = actual,
                        }
                    }
                }
            },
            HealthStatus::Serving | HealthStatus::Degraded => {
                loop {
                    let cur = unpack_state(cur_raw);
                    if !cur.not_serving {
                        break;
                    }
                    let cleared = UnpackedState {
                        not_serving: false,
                        ..cur
                    };
                    // AcqRel/Acquire releases the NotServing hold without closing the breaker directly.
                    match self.inner.state_word.compare_exchange_weak(
                        cur_raw,
                        pack_state(cleared),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break,
                        Err(actual) => cur_raw = actual,
                    }
                }
                let _ = self.inner.advance_timer_if_due(now);
            }
        }
    }

    /// Clears buckets outside the active `30 s` window or belonging to prior `Closed` generations (`RTE-084`).
    pub(crate) fn clear_stale_buckets(&self, now: Now) {
        // Acquire reads the current generation so live buckets of the active generation are preserved.
        let s = unpack_state(self.inner.state_word.load(Ordering::Acquire));
        let cur_epoch = now.ms / BUCKET_MS;

        for (slot_idx, bucket) in self.inner.buckets.iter().enumerate() {
            // Acquire observes bucket state before attempting stale cleanup.
            let mut cur = bucket.load(Ordering::Acquire);
            while let Some(b) = unpack_bucket(cur) {
                let is_live = s.state == BreakerState::Closed
                    && b.closed_gen == s.closed_gen
                    && is_bucket_in_window(slot_idx, b.cycle, cur_epoch);
                if is_live {
                    break;
                }
                // AcqRel/Acquire avoids clobbering a concurrent hot-path write to this bucket.
                match bucket.compare_exchange_weak(
                    cur,
                    BUCKET_EMPTY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }
        }
    }

    /// Returns summed `(ok, fail, slow)` in the active window at `now` for concurrency tests.
    #[doc(hidden)]
    #[must_use]
    pub fn window_totals(&self, now: Now) -> (u64, u64, u64) {
        // Acquire reads the current generation and active bucket words consistently.
        let s = unpack_state(self.inner.state_word.load(Ordering::Acquire));
        if s.state != BreakerState::Closed {
            return (0, 0, 0);
        }
        let epoch = now.ms / BUCKET_MS;
        let mut ok = 0;
        let mut fail = 0;
        let mut slow = 0;
        for (slot_idx, bucket) in self.inner.buckets.iter().enumerate() {
            // Acquire pairs with AcqRel bucket updates.
            if let Some(b) = unpack_bucket(bucket.load(Ordering::Acquire))
                && b.closed_gen == s.closed_gen
                && is_bucket_in_window(slot_idx, b.cycle, epoch)
            {
                ok += b.ok;
                fail += b.fail;
                slow += b.slow;
            }
        }
        (ok, fail, slow)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, RngAlgorithm, RngSeed};
    use tracing::field::{Field, Visit};

    use super::*;

    // ---- Hand-written minimal `tracing::Subscriber` for transition capture ----

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedTransitionEvent {
        branch: String,
        from: String,
        to: String,
        k: u64,
        reason: String,
    }

    #[derive(Default)]
    struct EventVisitor {
        fields: BTreeMap<String, String>,
        k: Option<u64>,
    }

    impl Visit for EventVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_owned(), value.to_owned());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "k" {
                self.k = Some(value);
            }
            self.fields
                .insert(field.name().to_owned(), value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    struct TransitionSubscriber {
        events: StdArc<StdMutex<Vec<RecordedTransitionEvent>>>,
    }

    impl tracing::Subscriber for TransitionSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = EventVisitor::default();
            event.record(&mut visitor);
            if let (Some(branch), Some(from), Some(to), Some(k), Some(reason)) = (
                visitor.fields.get("branch").cloned(),
                visitor.fields.get("from").cloned(),
                visitor.fields.get("to").cloned(),
                visitor.k,
                visitor.fields.get("reason").cloned(),
            ) {
                self.events
                    .lock()
                    .expect("event log lock")
                    .push(RecordedTransitionEvent {
                        branch,
                        from,
                        to,
                        k,
                        reason,
                    });
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// Runs `f` with `subscriber` as the thread-local default while keeping two anchor
    /// `Dispatch` instances alive so `tracing_core`'s `Rebuilder::JustOne` fast-path
    /// cannot race concurrent test threads that have no subscriber installed.
    fn with_transition_subscriber<T>(subscriber: TransitionSubscriber, f: impl FnOnce() -> T) -> T {
        static ANCHOR_DISPATCHES: std::sync::OnceLock<(tracing::Dispatch, tracing::Dispatch)> =
            std::sync::OnceLock::new();
        let _ = ANCHOR_DISPATCHES.get_or_init(|| {
            let d1 = tracing::Dispatch::new(TransitionSubscriber {
                events: StdArc::new(StdMutex::new(Vec::new())),
            });
            let d2 = tracing::Dispatch::new(TransitionSubscriber {
                events: StdArc::new(StdMutex::new(Vec::new())),
            });
            (d1, d2)
        });
        tracing::subscriber::with_default(subscriber, f)
    }

    // ---- Hand-written minimal `metrics::Recorder` for gauge capture ----

    #[derive(Debug, Clone, PartialEq)]
    struct RecordedGaugeSet {
        name: String,
        branch: String,
        value: f64,
    }

    struct CaptureGaugeHandle {
        name: String,
        branch: String,
        writes: StdArc<StdMutex<Vec<RecordedGaugeSet>>>,
    }

    impl metrics::GaugeFn for CaptureGaugeHandle {
        fn increment(&self, _value: f64) {}
        fn decrement(&self, _value: f64) {}
        fn set(&self, value: f64) {
            self.writes
                .lock()
                .expect("gauge log lock")
                .push(RecordedGaugeSet {
                    name: self.name.clone(),
                    branch: self.branch.clone(),
                    value,
                });
        }
    }

    struct TestMetricsRecorder {
        writes: StdArc<StdMutex<Vec<RecordedGaugeSet>>>,
    }

    impl metrics::Recorder for TestMetricsRecorder {
        fn describe_counter(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_gauge(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_histogram(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn register_counter(
            &self,
            _key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            metrics::Counter::noop()
        }

        fn register_gauge(
            &self,
            key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Gauge {
            let branch = key
                .labels()
                .find(|l| l.key() == "branch")
                .map(|l| l.value().to_owned())
                .unwrap_or_default();
            metrics::Gauge::from_arc(StdArc::new(CaptureGaugeHandle {
                name: key.name().to_owned(),
                branch,
                writes: StdArc::clone(&self.writes),
            }))
        }

        fn register_histogram(
            &self,
            _key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    // ---- Table-driven Breaker FSM (> 30 rows) ----

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ExpectedAdmit {
        Allowed,
        Probe,
        RejectedMs(u64),
    }

    #[derive(Debug, Clone)]
    enum Step {
        RecordMany {
            count: usize,
            outcome: Outcome,
            latency_ms: u64,
            at_ms: u64,
        },
        Health {
            status: HealthStatus,
            at_ms: u64,
        },
        AdmitProbe {
            slot: usize,
            at_ms: u64,
        },
        ResolveProbe {
            slot: usize,
            outcome: Outcome,
            latency_ms: u64,
            at_ms: u64,
        },
        DropProbe {
            slot: usize,
        },
        ObserveState {
            at_ms: u64,
            expected: BreakerState,
        },
    }

    struct FsmCase {
        name: &'static str,
        timeout_ms: u32,
        steps: Vec<Step>,
        check_ms: u64,
        expected_state: BreakerState,
        expected_admit: ExpectedAdmit,
        expected_flapping: bool,
        expected_transitions: usize,
    }

    fn trip_20_failures(at_ms: u64) -> Step {
        Step::RecordMany {
            count: 20,
            outcome: Outcome::Failure,
            latency_ms: 10,
            at_ms,
        }
    }

    fn close_via_two_probes(at_ms: u64) -> Vec<Step> {
        vec![
            Step::AdmitProbe { slot: 0, at_ms },
            Step::AdmitProbe { slot: 1, at_ms },
            Step::ResolveProbe {
                slot: 0,
                outcome: Outcome::Ok,
                latency_ms: 10,
                at_ms,
            },
            Step::ResolveProbe {
                slot: 1,
                outcome: Outcome::Ok,
                latency_ms: 10,
                at_ms,
            },
        ]
    }

    fn fail_two_probes(at_ms: u64) -> Vec<Step> {
        vec![
            Step::AdmitProbe { slot: 0, at_ms },
            Step::AdmitProbe { slot: 1, at_ms },
            Step::ResolveProbe {
                slot: 0,
                outcome: Outcome::Failure,
                latency_ms: 10,
                at_ms,
            },
            Step::ResolveProbe {
                slot: 1,
                outcome: Outcome::Failure,
                latency_ms: 10,
                at_ms,
            },
        ]
    }

    // spec: RTE-080, RTE-081, RTE-082, RTE-083, RTE-084, RTE-085, OPS-020
    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn breaker_fsm_table_driven_34_cases_and_telemetry_events() {
        let mut cases: Vec<FsmCase> = Vec::new();

        // 1. Fresh breaker is Closed and Allowed.
        cases.push(FsmCase {
            name: "01_fresh_closed",
            timeout_ms: 1_000,
            steps: vec![],
            check_ms: 0,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 2. 19 failures never trip (calls < 20).
        cases.push(FsmCase {
            name: "02_19_failures_do_not_trip",
            timeout_ms: 1_000,
            steps: vec![Step::RecordMany {
                count: 19,
                outcome: Outcome::Failure,
                latency_ms: 10,
                at_ms: 100,
            }],
            check_ms: 100,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 3. 20 calls with 9 failures + 11 fast Ok do NOT trip.
        cases.push(FsmCase {
            name: "03_20_calls_9_failures_stay_closed",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 9,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 200,
                },
                Step::RecordMany {
                    count: 11,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 200,
                },
            ],
            check_ms: 200,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 4. 20 calls with 10 failures + 10 fast Ok trip Open(k=0) with 5,000 ms retry_after.
        cases.push(FsmCase {
            name: "04_20_calls_10_failures_trip_open_k0",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 10,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 200,
                },
                Step::RecordMany {
                    count: 10,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 200,
                },
            ],
            check_ms: 200,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(5_000),
            expected_flapping: false,
            expected_transitions: 1,
        });

        // 5. Slow rate: 15 slow Ok (801 ms > 800 ms) + 5 boundary-fast Ok (800 ms) do NOT trip.
        cases.push(FsmCase {
            name: "05_20_calls_15_slow_stay_closed",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 15,
                    outcome: Outcome::Ok,
                    latency_ms: 801,
                    at_ms: 500,
                },
                Step::RecordMany {
                    count: 5,
                    outcome: Outcome::Ok,
                    latency_ms: 800,
                    at_ms: 500,
                },
            ],
            check_ms: 500,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 6. Slow rate: 16 slow Ok (801 ms) + 4 fast Ok (800 ms) trip Open(k=0).
        cases.push(FsmCase {
            name: "06_20_calls_16_slow_trip_open",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 4,
                    outcome: Outcome::Ok,
                    latency_ms: 800,
                    at_ms: 500,
                },
                Step::RecordMany {
                    count: 16,
                    outcome: Outcome::Ok,
                    latency_ms: 801,
                    at_ms: 500,
                },
            ],
            check_ms: 500,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(5_000),
            expected_flapping: false,
            expected_transitions: 1,
        });

        // 7. Exact 0.8 * timeout_ms (800 ms for 1,000 ms timeout) is NOT slow even across 20 calls.
        cases.push(FsmCase {
            name: "07_20_calls_at_exact_800ms_not_slow",
            timeout_ms: 1_000,
            steps: vec![Step::RecordMany {
                count: 20,
                outcome: Outcome::Ok,
                latency_ms: 800,
                at_ms: 500,
            }],
            check_ms: 500,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 8. ClientErrors count as ok (never failures): 25 ClientErrors + 9 Failures stay Closed.
        cases.push(FsmCase {
            name: "08_client_errors_never_count_as_failures",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 25,
                    outcome: Outcome::ClientError,
                    latency_ms: 10,
                    at_ms: 300,
                },
                Step::RecordMany {
                    count: 9,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 300,
                },
            ],
            check_ms: 300,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 9. Timeout (Failure with latency 1,000 ms > 800 ms) increments BOTH fail and slow:
        // 4 timeouts + 12 slow Ok + 4 fast Ok => fail=4 (<10), slow=16 (>=16) => trips!
        cases.push(FsmCase {
            name: "09_timeout_counts_as_both_fail_and_slow",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 4,
                    outcome: Outcome::Ok,
                    latency_ms: 100,
                    at_ms: 400,
                },
                Step::RecordMany {
                    count: 12,
                    outcome: Outcome::Ok,
                    latency_ms: 850,
                    at_ms: 400,
                },
                Step::RecordMany {
                    count: 4,
                    outcome: Outcome::Failure,
                    latency_ms: 1_000,
                    at_ms: 400,
                },
            ],
            check_ms: 400,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(5_000),
            expected_flapping: false,
            expected_transitions: 1,
        });

        // 10. Events at 0 ms remain inside the window at 29,999 ms (epoch 9, window [0..=9]).
        cases.push(FsmCase {
            name: "10_window_includes_0ms_at_29999ms",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 9,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 0,
                },
                Step::RecordMany {
                    count: 10,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 29_999,
                },
                Step::RecordMany {
                    count: 1,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 29_999,
                },
            ],
            check_ms: 29_999,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(5_000),
            expected_flapping: false,
            expected_transitions: 1,
        });

        // 11. Events at 0 ms fall outside the window at 30,000 ms (epoch 10, window [1..=10]).
        cases.push(FsmCase {
            name: "11_window_excludes_0ms_at_30000ms",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 10,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 0,
                },
                Step::RecordMany {
                    count: 11,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 30_000,
                },
                Step::RecordMany {
                    count: 9,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 30_000,
                },
            ],
            check_ms: 30_000,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 12. Epoch boundary at 3,000 ms: events at 3,000 ms (epoch 1) stay inside at 30,000 ms (epoch 10).
        cases.push(FsmCase {
            name: "12_epoch_boundary_3000ms_included_at_30000ms",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 9,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 3_000,
                },
                Step::RecordMany {
                    count: 10,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 30_000,
                },
                Step::RecordMany {
                    count: 1,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 30_000,
                },
            ],
            check_ms: 30_000,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(5_000),
            expected_flapping: false,
            expected_transitions: 1,
        });

        // 13. Epoch boundary at 2,999 ms (epoch 0) vs 30,000 ms (epoch 10): excluded!
        cases.push(FsmCase {
            name: "13_epoch_boundary_2999ms_excluded_at_30000ms",
            timeout_ms: 1_000,
            steps: vec![
                Step::RecordMany {
                    count: 10,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 2_999,
                },
                Step::RecordMany {
                    count: 11,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 30_000,
                },
                Step::RecordMany {
                    count: 9,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 30_000,
                },
            ],
            check_ms: 30_000,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 0,
        });

        // 14..21. Open durations for k = 0..6 and exact boundary ms (open_until - 1 vs open_until),
        // plus saturation at k = 6 (60_000 ms).
        let expected_open_ms: [u64; 8] = [
            5_000, 10_000, 20_000, 40_000, 60_000, 60_000, 60_000, 60_000,
        ];
        for (level_idx, &dur_ms) in expected_open_ms.iter().enumerate() {
            // Before boundary (open_until - 1): still Open with retry_after = 1 ms.
            let mut steps_before = vec![trip_20_failures(1_000)];
            let mut cursor = 1_000_u64;
            for prev_dur in expected_open_ms.iter().take(level_idx) {
                cursor += *prev_dur;
                steps_before.extend(fail_two_probes(cursor));
            }
            cases.push(FsmCase {
                name: match level_idx {
                    0 => "14_open_k0_1ms_before_boundary",
                    1 => "15_open_k1_1ms_before_boundary",
                    2 => "16_open_k2_1ms_before_boundary",
                    3 => "17_open_k3_1ms_before_boundary",
                    4 => "18_open_k4_1ms_before_boundary",
                    5 => "19_open_k5_1ms_before_boundary",
                    6 => "20_open_k6_1ms_before_boundary",
                    _ => "21_open_k6_saturated_1ms_before_boundary",
                },
                timeout_ms: 1_000,
                steps: steps_before,
                check_ms: cursor + dur_ms - 1,
                expected_state: BreakerState::Open,
                expected_admit: ExpectedAdmit::RejectedMs(1),
                expected_flapping: (1 + 2 * level_idx) > 6,
                expected_transitions: 1 + 2 * level_idx,
            });
        }

        // 22. Exact boundary millisecond transitions Open(k=0) -> HalfOpen and admits Probe.
        cases.push(FsmCase {
            name: "22_open_to_half_open_at_exact_5000ms",
            timeout_ms: 1_000,
            steps: vec![trip_20_failures(1_000)],
            check_ms: 6_000,
            expected_state: BreakerState::HalfOpen,
            expected_admit: ExpectedAdmit::Probe,
            expected_flapping: false,
            expected_transitions: 2,
        });

        // 23. 4th concurrent probe in HalfOpen is rejected with retry_after = 1,000 ms.
        cases.push(FsmCase {
            name: "23_half_open_4th_concurrent_probe_rejected_1s",
            timeout_ms: 1_000,
            steps: vec![
                trip_20_failures(0),
                Step::AdmitProbe {
                    slot: 0,
                    at_ms: 5_000,
                },
                Step::AdmitProbe {
                    slot: 1,
                    at_ms: 5_000,
                },
                Step::AdmitProbe {
                    slot: 2,
                    at_ms: 5_000,
                },
            ],
            check_ms: 5_000,
            expected_state: BreakerState::HalfOpen,
            expected_admit: ExpectedAdmit::RejectedMs(1_000),
            expected_flapping: false,
            expected_transitions: 2,
        });

        // 24. 1 probe success does NOT close yet; 2nd probe success (including ClientError) closes!
        cases.push(FsmCase {
            name: "24_half_open_one_success_stays_half_open",
            timeout_ms: 1_000,
            steps: vec![
                trip_20_failures(0),
                Step::AdmitProbe {
                    slot: 0,
                    at_ms: 5_000,
                },
                Step::ResolveProbe {
                    slot: 0,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 5_010,
                },
            ],
            check_ms: 5_010,
            expected_state: BreakerState::HalfOpen,
            expected_admit: ExpectedAdmit::Probe,
            expected_flapping: false,
            expected_transitions: 2,
        });

        // 25. 2 of 3 probes succeed (1 Failure + 1 Ok + 1 ClientError) -> Closed.
        cases.push(FsmCase {
            name: "25_half_open_majority_2_of_3_closes",
            timeout_ms: 1_000,
            steps: vec![
                trip_20_failures(0),
                Step::AdmitProbe {
                    slot: 0,
                    at_ms: 5_000,
                },
                Step::AdmitProbe {
                    slot: 1,
                    at_ms: 5_000,
                },
                Step::AdmitProbe {
                    slot: 2,
                    at_ms: 5_000,
                },
                Step::ResolveProbe {
                    slot: 0,
                    outcome: Outcome::Failure,
                    latency_ms: 10,
                    at_ms: 5_010,
                },
                Step::ResolveProbe {
                    slot: 1,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 5_020,
                },
                Step::ResolveProbe {
                    slot: 2,
                    outcome: Outcome::ClientError,
                    latency_ms: 10,
                    at_ms: 5_030,
                },
            ],
            check_ms: 5_030,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 3,
        });

        // 26. Dropped ProbePermit releases its slot without changing ok/fail tallies.
        cases.push(FsmCase {
            name: "26_dropped_probe_permit_frees_slot_unchanged_tally",
            timeout_ms: 1_000,
            steps: vec![
                trip_20_failures(0),
                Step::AdmitProbe {
                    slot: 0,
                    at_ms: 5_000,
                },
                Step::AdmitProbe {
                    slot: 1,
                    at_ms: 5_000,
                },
                Step::AdmitProbe {
                    slot: 2,
                    at_ms: 5_000,
                },
                Step::DropProbe { slot: 1 },
                Step::ResolveProbe {
                    slot: 0,
                    outcome: Outcome::Ok,
                    latency_ms: 10,
                    at_ms: 5_010,
                },
                Step::DropProbe { slot: 2 },
            ],
            check_ms: 5_010,
            expected_state: BreakerState::HalfOpen,
            expected_admit: ExpectedAdmit::Probe,
            expected_flapping: false,
            expected_transitions: 2,
        });

        // 27. Window is cleared on Closed: after closing at 5,000 ms, 19 new failures at 5,001 ms do NOT re-trip.
        let mut s27 = vec![trip_20_failures(0)];
        s27.extend(close_via_two_probes(5_000));
        s27.push(Step::RecordMany {
            count: 19,
            outcome: Outcome::Failure,
            latency_ms: 10,
            at_ms: 5_001,
        });
        cases.push(FsmCase {
            name: "27_window_cleared_on_closed_does_not_retrip",
            timeout_ms: 1_000,
            steps: s27,
            check_ms: 5_001,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 3,
        });

        // 28. Non-probe completions recorded while Open and HalfOpen do not touch window or probe tally.
        let mut s28 = vec![
            trip_20_failures(0),
            Step::RecordMany {
                count: 30,
                outcome: Outcome::Failure,
                latency_ms: 1_000,
                at_ms: 2_000,
            },
            Step::ObserveState {
                at_ms: 5_000,
                expected: BreakerState::HalfOpen,
            },
            Step::RecordMany {
                count: 30,
                outcome: Outcome::Ok,
                latency_ms: 5,
                at_ms: 5_001,
            },
            Step::RecordMany {
                count: 30,
                outcome: Outcome::Failure,
                latency_ms: 1_000,
                at_ms: 5_002,
            },
        ];
        s28.extend(close_via_two_probes(5_010));
        s28.push(Step::RecordMany {
            count: 1,
            outcome: Outcome::Failure,
            latency_ms: 10,
            at_ms: 5_011,
        });
        cases.push(FsmCase {
            name: "28_non_probe_records_ignored_in_open_and_half_open",
            timeout_ms: 1_000,
            steps: s28,
            check_ms: 5_011,
            expected_state: BreakerState::Closed,
            expected_admit: ExpectedAdmit::Allowed,
            expected_flapping: false,
            expected_transitions: 3,
        });

        // 29. Re-opening inside 5 minutes (299,999 ms Closed) escalates k from 0 -> 1 (10,000 ms).
        let mut s29 = vec![trip_20_failures(0)];
        s29.extend(close_via_two_probes(5_000));
        s29.push(trip_20_failures(5_000 + 299_999));
        cases.push(FsmCase {
            name: "29_reopen_within_5_min_escalates_k_to_1",
            timeout_ms: 1_000,
            steps: s29,
            check_ms: 304_999,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(10_000),
            expected_flapping: false,
            expected_transitions: 4,
        });

        // 30. Re-opening at exact 5 minutes (300,000 ms Closed) resets k to 0 (5,000 ms).
        let mut s30 = vec![trip_20_failures(0)];
        s30.extend(fail_two_probes(5_000)); // now Open(k=1) until 15_000
        s30.extend(close_via_two_probes(15_000)); // Closed(last_k=1) since 15_000
        s30.push(trip_20_failures(15_000 + 300_000)); // 315_000: >= 5 min Closed -> k=0 (5,000 ms)
        cases.push(FsmCase {
            name: "30_reopen_after_5_min_resets_k_to_0",
            timeout_ms: 1_000,
            steps: s30,
            check_ms: 315_000,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(5_000),
            expected_flapping: false,
            expected_transitions: 6,
        });

        // 31. Health = NotServing from Closed opens immediately and withholds Open -> HalfOpen past 5,000 ms,
        // returning retry_after = 1,000 ms once the open timer has elapsed (`RTE-085`).
        cases.push(FsmCase {
            name: "31_not_serving_opens_and_withholds_half_open",
            timeout_ms: 1_000,
            steps: vec![Step::Health {
                status: HealthStatus::NotServing,
                at_ms: 1_000,
            }],
            check_ms: 20_000,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(1_000),
            expected_flapping: false,
            expected_transitions: 1,
        });

        // 32. Health = Serving does NOT close Open directly: before timer (2,000 ms) stays Open;
        // after timer (6,000 ms) transitions Open -> HalfOpen (never Closed).
        cases.push(FsmCase {
            name: "32_serving_releases_hold_into_half_open_not_closed",
            timeout_ms: 1_000,
            steps: vec![
                Step::Health {
                    status: HealthStatus::NotServing,
                    at_ms: 0,
                },
                Step::Health {
                    status: HealthStatus::Serving,
                    at_ms: 2_000,
                },
                Step::ObserveState {
                    at_ms: 2_000,
                    expected: BreakerState::Open,
                },
                Step::Health {
                    status: HealthStatus::NotServing,
                    at_ms: 3_000,
                }, // already Open(k=0, open_until=5_000): k & open_until unchanged!
                Step::ObserveState {
                    at_ms: 6_000,
                    expected: BreakerState::Open,
                },
                Step::Health {
                    status: HealthStatus::Serving,
                    at_ms: 6_000,
                },
            ],
            check_ms: 6_000,
            expected_state: BreakerState::HalfOpen,
            expected_admit: ExpectedAdmit::Probe,
            expected_flapping: false,
            expected_transitions: 2,
        });

        // 33. Health = NotServing from HalfOpen reopens with k + 1 (10,000 ms).
        cases.push(FsmCase {
            name: "33_not_serving_from_half_open_reopens_k1",
            timeout_ms: 1_000,
            steps: vec![
                trip_20_failures(0),
                Step::ObserveState {
                    at_ms: 5_000,
                    expected: BreakerState::HalfOpen,
                },
                Step::Health {
                    status: HealthStatus::NotServing,
                    at_ms: 5_100,
                },
            ],
            check_ms: 5_100,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(10_000),
            expected_flapping: false,
            expected_transitions: 3,
        });

        // 34. Flapping: 7 transitions inside 10 min => true; after aging past 600,000 ms => false.
        let mut s34 = vec![trip_20_failures(0)]; // #1 Closed->Open at 0
        s34.extend(close_via_two_probes(5_000)); // #2 Open->HalfOpen, #3 HalfOpen->Closed at 5_000
        s34.push(trip_20_failures(6_000)); // #4 Closed->Open(k=1) at 6_000
        s34.extend(close_via_two_probes(16_000)); // #5 Open->HalfOpen, #6 HalfOpen->Closed at 16_000
        s34.push(trip_20_failures(17_000)); // #7 Closed->Open(k=2) at 17_000
        cases.push(FsmCase {
            name: "34_flapping_true_at_7_transitions_inside_10min",
            timeout_ms: 1_000,
            steps: s34.clone(),
            check_ms: 17_000,
            expected_state: BreakerState::Open,
            expected_admit: ExpectedAdmit::RejectedMs(20_000),
            expected_flapping: true,
            expected_transitions: 7,
        });

        // 35. Same 7 transitions checked at 605_000 ms (first 3 transitions at 0 & 5_000 aged >= 600,000 ms;
        // plus #8 Open->HalfOpen at 605_000 => only 5 transitions inside last 10 min) => flapping false!
        cases.push(FsmCase {
            name: "35_flapping_false_after_transitions_age_out",
            timeout_ms: 1_000,
            steps: s34,
            check_ms: 605_000,
            expected_state: BreakerState::HalfOpen,
            expected_admit: ExpectedAdmit::Probe,
            expected_flapping: false,
            expected_transitions: 8,
        });

        assert!(cases.len() >= 30);

        for case in cases {
            let events_buf = StdArc::new(StdMutex::new(Vec::new()));
            let gauges_buf = StdArc::new(StdMutex::new(Vec::new()));
            let subscriber = TransitionSubscriber {
                events: StdArc::clone(&events_buf),
            };
            let recorder = TestMetricsRecorder {
                writes: StdArc::clone(&gauges_buf),
            };

            with_transition_subscriber(subscriber, || {
                metrics::with_local_recorder(&recorder, || {
                    let breaker = Breaker::new(case.timeout_ms, Arc::from("orders-eu"));
                    let mut permits: [Option<ProbePermit>; 4] = [None, None, None, None];

                    for step in case.steps {
                        match step {
                            Step::RecordMany {
                                count,
                                outcome,
                                latency_ms,
                                at_ms,
                            } => {
                                let now = Now::from_ms(at_ms);
                                let dur = Duration::from_millis(latency_ms);
                                for _ in 0..count {
                                    breaker.record_call(outcome, dur, now);
                                }
                            }
                            Step::Health { status, at_ms } => {
                                breaker.on_health(status, Now::from_ms(at_ms));
                            }
                            Step::AdmitProbe { slot, at_ms } => {
                                match breaker.admit(Now::from_ms(at_ms)) {
                                    Admission::Probe(p) => permits[slot] = Some(p),
                                    other => panic!(
                                        "{}: expected Probe at {at_ms}, got {other:?}",
                                        case.name
                                    ),
                                }
                            }
                            Step::ResolveProbe {
                                slot,
                                outcome,
                                latency_ms,
                                at_ms,
                            } => {
                                let p = permits[slot].take().unwrap_or_else(|| {
                                    panic!("{}: missing permit {slot}", case.name)
                                });
                                p.record(
                                    outcome,
                                    Duration::from_millis(latency_ms),
                                    Now::from_ms(at_ms),
                                );
                            }
                            Step::DropProbe { slot } => {
                                let _ = permits[slot].take();
                            }
                            Step::ObserveState { at_ms, expected } => {
                                assert_eq!(
                                    breaker.state(Now::from_ms(at_ms)),
                                    expected,
                                    "case {} intermediate state at {at_ms}",
                                    case.name
                                );
                            }
                        }
                    }

                    let check_now = Now::from_ms(case.check_ms);
                    assert_eq!(
                        breaker.state(check_now),
                        case.expected_state,
                        "case {} state mismatch",
                        case.name
                    );
                    assert_eq!(
                        breaker.flapping(check_now),
                        case.expected_flapping,
                        "case {} flapping mismatch",
                        case.name
                    );

                    match (breaker.admit(check_now), case.expected_admit) {
                        (Admission::Allowed, ExpectedAdmit::Allowed) => {}
                        (Admission::Probe(p), ExpectedAdmit::Probe) => drop(p),
                        (
                            Admission::Rejected { retry_after },
                            ExpectedAdmit::RejectedMs(exp_ms),
                        ) => {
                            assert_eq!(
                                retry_after,
                                Duration::from_millis(exp_ms),
                                "case {} retry_after mismatch",
                                case.name
                            );
                        }
                        (actual, exp) => {
                            panic!("case {}: admit got {actual:?}, expected {exp:?}", case.name)
                        }
                    }

                    drop(permits);
                });
            });

            let recorded_events = events_buf.lock().expect("events lock");
            let recorded_gauges = gauges_buf.lock().expect("gauges lock");
            assert_eq!(
                recorded_events.len(),
                case.expected_transitions,
                "case {} transition event count mismatch: {recorded_events:?}",
                case.name
            );
            assert_eq!(
                recorded_gauges.len(),
                case.expected_transitions,
                "case {} gauge update count mismatch: {recorded_gauges:?}",
                case.name
            );

            for (ev, g) in recorded_events.iter().zip(recorded_gauges.iter()) {
                assert_eq!(ev.branch, "orders-eu");
                assert!(!ev.reason.is_empty());
                assert!(ev.k <= 6);
                assert_eq!(g.name, "gap_breaker_state");
                assert_eq!(g.branch, "orders-eu");
                let expected_g = match ev.to.as_str() {
                    "Closed" => 0.0,
                    "HalfOpen" => 1.0,
                    "Open" => 2.0,
                    other => panic!("unexpected `to` state {other}"),
                };
                assert_eq!(g.value, expected_g);
            }
        }
    }

    // ---- Differential Proptest against Brute-Force Reference Model (2,000+ cases) ----

    #[derive(Debug, Clone)]
    struct RefEvent {
        ms: u64,
        outcome: Outcome,
        latency: Duration,
    }

    #[derive(Debug, Clone)]
    struct RefBreaker {
        timeout_ms: u32,
        state: BreakerState,
        k: u8,
        not_serving: bool,
        has_opened: bool,
        closed_since_ms: u64,
        open_until_ms: u64,
        episode: u64,
        next_episode: u64,
        in_flight_probes: u8,
        ok_probes: u8,
        fail_probes: u8,
        events: Vec<RefEvent>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RefAdmit {
        Allowed,
        Probe(u64),
        Rejected(Duration),
    }

    impl RefBreaker {
        fn new(timeout_ms: u32) -> Self {
            Self {
                timeout_ms,
                state: BreakerState::Closed,
                k: 0,
                not_serving: false,
                has_opened: false,
                closed_since_ms: 0,
                open_until_ms: 0,
                episode: 0,
                next_episode: 1,
                in_flight_probes: 0,
                ok_probes: 0,
                fail_probes: 0,
                events: Vec::new(),
            }
        }

        fn advance_timer(&mut self, ms: u64) {
            if self.state == BreakerState::Open && !self.not_serving && ms >= self.open_until_ms {
                self.state = BreakerState::HalfOpen;
                self.episode = self.next_episode;
                self.next_episode += 1;
                self.in_flight_probes = 0;
                self.ok_probes = 0;
                self.fail_probes = 0;
            }
        }

        fn next_open_k_from_closed(&self, ms: u64) -> u8 {
            if !self.has_opened || ms.saturating_sub(self.closed_since_ms) >= 300_000 {
                0
            } else {
                (self.k + 1).min(6)
            }
        }

        fn record_call(&mut self, outcome: Outcome, latency: Duration, ms: u64) {
            self.advance_timer(ms);
            if self.state != BreakerState::Closed {
                return;
            }
            self.events.push(RefEvent {
                ms,
                outcome,
                latency,
            });
            let cur_epoch = ms / 3_000;
            let min_epoch = cur_epoch.saturating_sub(9);
            let mut ok = 0_u64;
            let mut fail = 0_u64;
            let mut slow = 0_u64;
            for ev in &self.events {
                let e = ev.ms / 3_000;
                if e >= min_epoch && e <= cur_epoch {
                    match ev.outcome {
                        Outcome::Ok | Outcome::ClientError => ok += 1,
                        Outcome::Failure => fail += 1,
                    }
                    if ev.latency.as_nanos().saturating_mul(5)
                        > u128::from(self.timeout_ms) * 4_000_000
                    {
                        slow += 1;
                    }
                }
            }
            let calls = ok + fail;
            if calls >= 20 && (fail * 2 >= calls || slow * 5 >= calls * 4) {
                let next_k = self.next_open_k_from_closed(ms);
                self.state = BreakerState::Open;
                self.k = next_k;
                self.has_opened = true;
                self.open_until_ms = ms + open_duration_ms(next_k);
            }
        }

        fn admit(&mut self, ms: u64) -> RefAdmit {
            self.advance_timer(ms);
            match self.state {
                BreakerState::Closed => RefAdmit::Allowed,
                BreakerState::Open => {
                    let rem_ms = self.open_until_ms.saturating_sub(ms);
                    let wait = if rem_ms == 0 {
                        Duration::from_millis(1_000)
                    } else {
                        Duration::from_millis(rem_ms)
                    };
                    RefAdmit::Rejected(wait)
                }
                BreakerState::HalfOpen => {
                    if self.in_flight_probes + self.ok_probes + self.fail_probes >= 3 {
                        RefAdmit::Rejected(Duration::from_secs(1))
                    } else {
                        self.in_flight_probes += 1;
                        RefAdmit::Probe(self.episode)
                    }
                }
            }
        }

        fn resolve_probe(&mut self, permit_ep: u64, outcome: Outcome, ms: u64) {
            if self.state != BreakerState::HalfOpen || self.episode != permit_ep {
                return;
            }
            self.in_flight_probes -= 1;
            if matches!(outcome, Outcome::Ok | Outcome::ClientError) {
                self.ok_probes += 1;
                if self.ok_probes >= 2 {
                    self.state = BreakerState::Closed;
                    self.has_opened = true;
                    self.closed_since_ms = ms;
                    self.events.clear();
                }
            } else {
                self.fail_probes += 1;
                if self.fail_probes >= 2 {
                    let next_k = (self.k + 1).min(6);
                    self.state = BreakerState::Open;
                    self.k = next_k;
                    self.has_opened = true;
                    self.open_until_ms = ms + open_duration_ms(next_k);
                }
            }
        }

        fn drop_probe(&mut self, permit_ep: u64) {
            if self.state == BreakerState::HalfOpen && self.episode == permit_ep {
                self.in_flight_probes -= 1;
            }
        }

        fn on_health(&mut self, status: HealthStatus, ms: u64) {
            self.advance_timer(ms);
            match status {
                HealthStatus::NotServing => match self.state {
                    BreakerState::Open => {
                        self.not_serving = true;
                    }
                    BreakerState::Closed => {
                        let next_k = self.next_open_k_from_closed(ms);
                        self.state = BreakerState::Open;
                        self.k = next_k;
                        self.not_serving = true;
                        self.has_opened = true;
                        self.open_until_ms = ms + open_duration_ms(next_k);
                    }
                    BreakerState::HalfOpen => {
                        let next_k = (self.k + 1).min(6);
                        self.state = BreakerState::Open;
                        self.k = next_k;
                        self.not_serving = true;
                        self.has_opened = true;
                        self.open_until_ms = ms + open_duration_ms(next_k);
                    }
                },
                HealthStatus::Serving | HealthStatus::Degraded => {
                    self.not_serving = false;
                    self.advance_timer(ms);
                }
            }
        }

        fn state(&mut self, ms: u64) -> BreakerState {
            self.advance_timer(ms);
            self.state
        }
    }

    #[derive(Debug, Clone)]
    enum DiffOp {
        Advance(u64),
        RecordBatch {
            n: u8,
            outcome: Outcome,
            latency_us: u64,
        },
        Admit,
        ResolvePermit {
            idx: usize,
            outcome: Outcome,
            latency_ms: u64,
        },
        DropPermit {
            idx: usize,
        },
        Health(HealthStatus),
    }

    fn arb_outcome() -> impl Strategy<Value = Outcome> {
        prop_oneof![
            Just(Outcome::Ok),
            Just(Outcome::Failure),
            Just(Outcome::ClientError),
        ]
    }

    fn arb_health() -> impl Strategy<Value = HealthStatus> {
        prop_oneof![
            Just(HealthStatus::Serving),
            Just(HealthStatus::Degraded),
            Just(HealthStatus::NotServing),
        ]
    }

    fn arb_diff_op() -> impl Strategy<Value = DiffOp> {
        prop_oneof![
            3 => prop_oneof![
                0_u64..=10,
                2_998_u64..=3_002,
                4_998_u64..=5_002,
                9_998_u64..=10_002,
                29_998_u64..=30_002,
                59_998_u64..=60_002,
                299_998_u64..=300_002,
            ].prop_map(DiffOp::Advance),
            4 => (
                1_u8..=8,
                arb_outcome(),
                prop_oneof![
                    50_000_u64..=799_000,
                    799_999_u64..=801_000,
                    850_000_u64..=1_500_000,
                ],
            )
                .prop_map(|(n, outcome, latency_us)| DiffOp::RecordBatch {
                    n,
                    outcome,
                    latency_us,
                }),
            3 => Just(DiffOp::Admit),
            2 => (0_usize..8, arb_outcome(), 10_u64..=1_200)
                .prop_map(|(idx, outcome, latency_ms)| DiffOp::ResolvePermit { idx, outcome, latency_ms }),
            2 => (0_usize..8).prop_map(|idx| DiffOp::DropPermit { idx }),
            1 => arb_health().prop_map(DiffOp::Health),
        ]
    }

    // spec: RTE-080, RTE-084, RTE-085
    #[test]
    fn breaker_differential_proptest_2000_cases_against_reference_model() {
        let seed: u64 = 0x4741_505F_4D33_3031;
        let config = ProptestConfig {
            cases: 2_000,
            rng_algorithm: RngAlgorithm::ChaCha,
            rng_seed: RngSeed::Fixed(seed),
            failure_persistence: None,
            ..ProptestConfig::default()
        };
        let mut runner = proptest::test_runner::TestRunner::new(config);
        let strategy = proptest::collection::vec(arb_diff_op(), 1..=60);

        let run_res = runner.run(&strategy, |ops| {
            let real = Breaker::new(1_000, Arc::from("diff-branch"));
            let mut model = RefBreaker::new(1_000);
            let mut real_permits: Vec<ProbePermit> = Vec::new();
            let mut ref_permits: Vec<u64> = Vec::new();
            let mut ms: u64 = 0;

            for (step_no, op) in ops.iter().enumerate() {
                match op {
                    DiffOp::Advance(delta) => {
                        ms = ms.saturating_add(*delta);
                    }
                    DiffOp::RecordBatch {
                        n,
                        outcome,
                        latency_us,
                    } => {
                        let now = Now::from_ms(ms);
                        let dur = Duration::from_micros(*latency_us);
                        for _ in 0..*n {
                            real.record_call(*outcome, dur, now);
                            model.record_call(*outcome, dur, ms);
                        }
                    }
                    DiffOp::Admit => {
                        let now = Now::from_ms(ms);
                        let r_adm = real.admit(now);
                        let m_adm = model.admit(ms);
                        match (r_adm, m_adm) {
                            (Admission::Allowed, RefAdmit::Allowed) => {}
                            (Admission::Probe(p), RefAdmit::Probe(ep)) => {
                                real_permits.push(p);
                                ref_permits.push(ep);
                            }
                            (Admission::Rejected { retry_after }, RefAdmit::Rejected(m_wait)) => {
                                prop_assert_eq!(
                                    retry_after,
                                    m_wait,
                                    "seed={:#x} step={} retry_after mismatch at ms={}",
                                    seed,
                                    step_no,
                                    ms
                                );
                            }
                            (r_other, m_other) => {
                                prop_assert!(
                                    false,
                                    "seed={seed:#x} step={step_no} admit kind mismatch at ms={ms}: real={r_other:?} model={m_other:?}"
                                );
                            }
                        }
                    }
                    DiffOp::ResolvePermit {
                        idx,
                        outcome,
                        latency_ms,
                    } => {
                        if !real_permits.is_empty() {
                            let pick = *idx % real_permits.len();
                            let rp = real_permits.swap_remove(pick);
                            let mp = ref_permits.swap_remove(pick);
                            rp.record(*outcome, Duration::from_millis(*latency_ms), Now::from_ms(ms));
                            model.resolve_probe(mp, *outcome, ms);
                        }
                    }
                    DiffOp::DropPermit { idx } => {
                        if !real_permits.is_empty() {
                            let pick = *idx % real_permits.len();
                            let rp = real_permits.swap_remove(pick);
                            let mp = ref_permits.swap_remove(pick);
                            drop(rp);
                            model.drop_probe(mp);
                        }
                    }
                    DiffOp::Health(h) => {
                        real.on_health(*h, Now::from_ms(ms));
                        model.on_health(*h, ms);
                    }
                }

                let r_state = real.state(Now::from_ms(ms));
                let m_state = model.state(ms);
                prop_assert_eq!(
                    r_state,
                    m_state,
                    "seed={:#x} step={} state mismatch at ms={}",
                    seed,
                    step_no,
                    ms
                );
            }
            Ok(())
        });

        if let Err(err) = run_res {
            panic!("breaker differential proptest failed (seed={seed:#x}): {err}");
        }
    }

    // spec: RTE-084
    #[test]
    fn breaker_bucket_saturation_halves_preserving_ratio() {
        // Test proportional halving at counter saturation (4,094):
        // Record 5,000 Ok calls and 200 Failure calls in one epoch (fail_rate = 200 / 5,200 < 0.5).
        // Without halving or with saturating clamp to 4,095, ratios or sums could corrupt; with halving,
        // breaker remains Closed because fail << ok.
        let b = Breaker::new(1_000, Arc::from("high-qps"));
        let now = Now::from_ms(0);
        for _ in 0..5_000 {
            b.record_call(Outcome::Ok, Duration::from_millis(5), now);
        }
        for _ in 0..200 {
            b.record_call(Outcome::Failure, Duration::from_millis(5), now);
        }
        assert_eq!(b.state(now), BreakerState::Closed);

        // Conversely, 5,000 Failure calls after 5,000 Ok calls exceed 50% and trip Open.
        for _ in 0..5_000 {
            b.record_call(Outcome::Failure, Duration::from_millis(5), now);
        }
        assert_eq!(b.state(now), BreakerState::Open);
    }

    // spec: RTE-080, RTE-084
    #[test]
    fn breaker_slow_threshold_exact_submillisecond_duration() {
        let now = Now::from_ms(0);
        for timeout_ms in [100_u32, 1_000_u32, 30_000_u32] {
            let exact_us = u64::from(timeout_ms) * 800; // 0.8 * timeout_ms in microseconds
            let cases: [(Duration, u64); 5] = [
                (Duration::from_micros(exact_us - 1), 0),
                (Duration::from_micros(exact_us), 0),
                (Duration::from_micros(exact_us + 1), 1),
                (Duration::from_micros(exact_us + 500), 1),
                (Duration::from_micros(exact_us + 1_000), 1),
            ];
            for (dur, expected_slow) in cases {
                let b = Breaker::new(timeout_ms, Arc::from("subms-slow"));
                b.record_call(Outcome::Ok, dur, now);
                assert_eq!(
                    b.window_totals(now),
                    (1, 0, expected_slow),
                    "timeout_ms={timeout_ms} dur={dur:?}"
                );
            }
        }
    }

    // spec: RTE-080, RTE-084
    #[test]
    fn breaker_bucket_cycle_wrap_after_2_pow_22_cycles() {
        const WRAP_MS: u64 = (1_u64 << 22) * 30_000;
        let bases = [
            0_u64,
            WRAP_MS - 100,
            WRAP_MS,
            WRAP_MS + 100,
            2 * WRAP_MS,
            2 * WRAP_MS + 5_000,
        ];

        for base in bases {
            let b = Breaker::new(1_000, Arc::from("cycle-wrap"));
            let before = Now::from_ms(base.saturating_sub(3_000));
            let after = Now::from_ms(base + 6_000);

            for _ in 0..10 {
                b.record_call(Outcome::Failure, Duration::from_millis(10), before);
            }
            assert_eq!(b.state(before), BreakerState::Closed);
            assert_eq!(b.window_totals(before), (0, 10, 0));

            for _ in 0..9 {
                b.record_call(Outcome::Failure, Duration::from_millis(10), after);
            }
            assert_eq!(b.state(after), BreakerState::Closed);
            assert_eq!(
                b.window_totals(after),
                (0, 19, 0),
                "base={base}: expected 19 failures across wrap"
            );

            b.record_call(Outcome::Failure, Duration::from_millis(10), after);
            assert_eq!(
                b.state(after),
                BreakerState::Open,
                "base={base}: 20th failure straddling wrap must trip Open"
            );
        }

        // Verify window_totals sums across the wrap and drops buckets once > 30 s old.
        let b_win = Breaker::new(1_000, Arc::from("wrap-aging"));
        let pre_wrap = Now::from_ms(WRAP_MS - 3_000); // epoch = 2^22 * 10 - 1 (bucket 9, cycle 2^22 - 1)
        let post_wrap = Now::from_ms(WRAP_MS + 6_000); // epoch = 2^22 * 10 + 2 (bucket 2, cycle 0)

        for _ in 0..4 {
            b_win.record_call(Outcome::Ok, Duration::from_millis(900), pre_wrap);
        }
        for _ in 0..3 {
            b_win.record_call(Outcome::Failure, Duration::from_millis(10), pre_wrap);
        }
        for _ in 0..5 {
            b_win.record_call(Outcome::Ok, Duration::from_millis(10), post_wrap);
        }
        for _ in 0..2 {
            b_win.record_call(Outcome::Failure, Duration::from_millis(10), post_wrap);
        }

        assert_eq!(b_win.window_totals(post_wrap), (9, 5, 4));
        // 27 s after `pre_wrap` (WRAP_MS + 24,000, 9 epochs later): `pre_wrap` is still inside window.
        assert_eq!(
            b_win.window_totals(Now::from_ms(WRAP_MS + 24_000)),
            (9, 5, 4)
        );
        // 30 s after `pre_wrap` (WRAP_MS + 27,000, 10 epochs later): `pre_wrap` aged out; only `post_wrap` counts.
        assert_eq!(
            b_win.window_totals(Now::from_ms(WRAP_MS + 27_000)),
            (5, 2, 0)
        );
        // 30 s after `post_wrap` (WRAP_MS + 36,000): both aged out.
        assert_eq!(
            b_win.window_totals(Now::from_ms(WRAP_MS + 36_000)),
            (0, 0, 0)
        );
    }

    // spec: RTE-083, RTE-085
    #[test]
    fn breaker_not_serving_hold_past_open_timer_rejects_with_1000ms() {
        let b = Breaker::new(1_000, Arc::from("hold-branch"));
        let t0 = Now::from_ms(0);
        for _ in 0..20 {
            b.record_call(Outcome::Failure, Duration::from_millis(10), t0);
        }
        assert_eq!(b.state(t0), BreakerState::Open);

        b.on_health(HealthStatus::NotServing, t0);

        // Before timer (open_until = 5,000 ms), retry_after is the exact remaining open time.
        match b.admit(Now::from_ms(1_800)) {
            Admission::Rejected { retry_after } => {
                assert_eq!(retry_after, Duration::from_millis(3_200));
            }
            other => panic!("expected Rejected(3_200 ms), got {other:?}"),
        }

        // At open_until (5,000 ms) and open_until + 10,000 (15,000 ms), NotServing hold keeps state Open
        // and rejects with 1,000 ms (never 0).
        for check_ms in [5_000_u64, 15_000_u64] {
            let t = Now::from_ms(check_ms);
            assert_eq!(b.state(t), BreakerState::Open);
            match b.admit(t) {
                Admission::Rejected { retry_after } => {
                    assert_eq!(retry_after, Duration::from_millis(1_000));
                }
                other => panic!("expected Rejected(1_000 ms) at {check_ms}, got {other:?}"),
            }
        }

        // Once Serving arrives, admit transitions Open -> HalfOpen and issues a ProbePermit.
        let t_recover = Now::from_ms(15_000);
        b.on_health(HealthStatus::Serving, t_recover);
        match b.admit(t_recover) {
            Admission::Probe(permit) => drop(permit),
            other => panic!("expected Probe after Serving, got {other:?}"),
        }
    }

    // spec: RTE-085
    #[test]
    fn breaker_stale_probe_permit_drop_and_record_ignore_later_episode() {
        // 1. Stale permit `drop` must not free a probe slot in episode 2.
        {
            let b = Breaker::new(1_000, Arc::from("stale-drop"));
            for _ in 0..20 {
                b.record_call(Outcome::Failure, Duration::from_millis(10), Now::from_ms(0));
            }
            let t_ep1 = Now::from_ms(5_000);
            let p_a = match b.admit(t_ep1) {
                Admission::Probe(p) => p,
                other => panic!("ep1 A: {other:?}"),
            };
            let p_b = match b.admit(t_ep1) {
                Admission::Probe(p) => p,
                other => panic!("ep1 B: {other:?}"),
            };
            let p_c = match b.admit(t_ep1) {
                Admission::Probe(p) => p,
                other => panic!("ep1 C: {other:?}"),
            };

            p_b.record(
                Outcome::Failure,
                Duration::from_millis(10),
                Now::from_ms(5_010),
            );
            p_c.record(
                Outcome::Failure,
                Duration::from_millis(10),
                Now::from_ms(5_010),
            );
            assert_eq!(b.state(Now::from_ms(5_010)), BreakerState::Open);

            // Episode 2 opens at 5,010 + 10,000 = 15,010 ms.
            let t_ep2 = Now::from_ms(15_010);
            let p_d = match b.admit(t_ep2) {
                Admission::Probe(p) => p,
                other => panic!("ep2 D: {other:?}"),
            };
            drop(p_a); // Stale episode-1 permit dropped while D is live in episode 2.

            let p_e = match b.admit(t_ep2) {
                Admission::Probe(p) => p,
                other => panic!("ep2 E: {other:?}"),
            };
            let p_f = match b.admit(t_ep2) {
                Admission::Probe(p) => p,
                other => panic!("ep2 F: {other:?}"),
            };
            match b.admit(t_ep2) {
                Admission::Rejected { retry_after } => {
                    assert_eq!(retry_after, Duration::from_millis(1_000));
                }
                other => panic!("expected 4th admit in ep2 to be Rejected(1000ms), got {other:?}"),
            }
            drop((p_d, p_e, p_f));
        }

        // 2. Stale permit `record(Ok)` and `record(Failure)` must not touch episode 2's slot count or tallies.
        for stale_outcome in [Outcome::Ok, Outcome::Failure] {
            let b = Breaker::new(1_000, Arc::from("stale-record"));
            for _ in 0..20 {
                b.record_call(Outcome::Failure, Duration::from_millis(10), Now::from_ms(0));
            }
            let t_ep1 = Now::from_ms(5_000);
            let p_a = match b.admit(t_ep1) {
                Admission::Probe(p) => p,
                other => panic!("ep1 A: {other:?}"),
            };
            let p_b = match b.admit(t_ep1) {
                Admission::Probe(p) => p,
                other => panic!("ep1 B: {other:?}"),
            };
            let p_c = match b.admit(t_ep1) {
                Admission::Probe(p) => p,
                other => panic!("ep1 C: {other:?}"),
            };

            p_b.record(
                Outcome::Failure,
                Duration::from_millis(10),
                Now::from_ms(5_010),
            );
            p_c.record(
                Outcome::Failure,
                Duration::from_millis(10),
                Now::from_ms(5_010),
            );

            let t_ep2 = Now::from_ms(15_010);
            let p_d = match b.admit(t_ep2) {
                Admission::Probe(p) => p,
                other => panic!("ep2 D: {other:?}"),
            };
            p_a.record(stale_outcome, Duration::from_millis(10), t_ep2);

            let p_e = match b.admit(t_ep2) {
                Admission::Probe(p) => p,
                other => panic!("ep2 E: {other:?}"),
            };
            let p_f = match b.admit(t_ep2) {
                Admission::Probe(p) => p,
                other => panic!("ep2 F: {other:?}"),
            };
            match b.admit(t_ep2) {
                Admission::Rejected { retry_after } => {
                    assert_eq!(retry_after, Duration::from_millis(1_000));
                }
                other => panic!(
                    "expected Rejected(1000ms) after stale record({stale_outcome:?}), got {other:?}"
                ),
            }

            // Verify episode 2's ok/fail tallies are still 0 (one real probe outcome must NOT reach majority 2).
            p_d.record(stale_outcome, Duration::from_millis(10), t_ep2);
            assert_eq!(
                b.state(t_ep2),
                BreakerState::HalfOpen,
                "stale {stale_outcome:?} must not contribute toward episode 2 majority"
            );
            drop((p_e, p_f));
        }
    }

    // spec: RTE-082, RTE-085
    #[test]
    fn breaker_flapping_window_10min_boundaries_and_future_timestamps() {
        let b = Breaker::new(1_000, Arc::from("flap-bounds"));

        // #1 Closed -> Open(0) at 100_000 ms
        for _ in 0..20 {
            b.record_call(
                Outcome::Failure,
                Duration::from_millis(10),
                Now::from_ms(100_000),
            );
        }
        // #2 Open -> HalfOpen, #3 HalfOpen -> Closed at 105_000 ms
        let t2 = Now::from_ms(105_000);
        match (b.admit(t2), b.admit(t2)) {
            (Admission::Probe(p1), Admission::Probe(p2)) => {
                p1.record(Outcome::Ok, Duration::from_millis(5), t2);
                p2.record(Outcome::Ok, Duration::from_millis(5), t2);
            }
            other => panic!("expected 2 probes at 105_000, got {other:?}"),
        }
        // #4 Closed -> Open(1) at 105_000 ms
        for _ in 0..20 {
            b.record_call(Outcome::Failure, Duration::from_millis(10), t2);
        }
        // #5 Open -> HalfOpen, #6 HalfOpen -> Closed at 115_000 ms (6 transitions so far!)
        let t3 = Now::from_ms(115_000);
        match (b.admit(t3), b.admit(t3)) {
            (Admission::Probe(p1), Admission::Probe(p2)) => {
                p1.record(Outcome::Ok, Duration::from_millis(5), t3);
                p2.record(Outcome::Ok, Duration::from_millis(5), t3);
            }
            other => panic!("expected 2 probes at 115_000, got {other:?}"),
        }
        assert!(!b.flapping(t3));

        // #7 Closed -> Open(2) with NotServing hold at 116_000 ms (7 transitions total:
        // [100_000, 105_000, 105_000, 105_000, 115_000, 115_000, 116_000]).
        b.on_health(HealthStatus::NotServing, Now::from_ms(116_000));
        assert!(b.flapping(Now::from_ms(116_000)));

        // Future timestamp (116_000 > 115_000) is NOT counted at now = 115_000 -> only 6 count -> false!
        assert!(
            !b.flapping(Now::from_ms(115_000)),
            "transition later than `now` must not be counted"
        );

        // 6 minutes after the latest transition (116_000 + 360_000 = 476_000 ms): all 7 are >= 6 min old
        // (> 5 min, < 10 min) and still count -> true!
        assert!(
            b.flapping(Now::from_ms(476_000)),
            "7 transitions 6 minutes ago must still count inside the 10-minute window"
        );

        // Exact 10-minute boundary on transition #1 (at 100_000 ms):
        // at 100_000 + 599_999 = 699_999 ms -> 7 transitions in window -> true;
        // at 100_000 + 600_000 = 700_000 ms -> #1 aged out (6 left) -> false.
        assert!(b.flapping(Now::from_ms(699_999)));
        assert!(!b.flapping(Now::from_ms(700_000)));
    }

    // spec: RTE-080, RTE-084
    #[test]
    fn breaker_counter_saturation_slow_and_ok_halving_preserves_ratio() {
        let now = Now::from_ms(0);

        // 1. 70% slow Ok + 30% fast Ok in one 3 s epoch across 10,000 calls (kills "halving skips slow").
        let b_ok = Breaker::new(1_000, Arc::from("sat-slow-ok"));
        for i in 0..10_000_u32 {
            let dur = if i % 10 < 3 {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(900)
            };
            b_ok.record_call(Outcome::Ok, dur, now);
        }
        assert_eq!(b_ok.state(now), BreakerState::Closed);
        let (ok, fail, slow) = b_ok.window_totals(now);
        assert!(ok <= 4_095 && fail == 0 && slow <= 4_095);
        let slow_ratio = (slow as f64) / (ok as f64);
        assert!(
            (slow_ratio - 0.70).abs() <= 0.01,
            "expected slow/calls within 1% of 0.70, got {slow_ratio} (ok={ok}, slow={slow})"
        );

        // 2. 30% fast Ok + 35% slow Ok + 35% slow Failure in each 20-call block in one 3 s epoch:
        //    slow = 70% > ok = 65% > fail = 35% (fast calls placed first in each block so cumulative
        //    ratios never overshoot 0.70 or 0.35), so `slow` reaches 4,094 BEFORE `ok` (at call 5,849)!
        //    Kills "saturation check ignores `slow`" as well as "halving skips `slow`".
        let b_slow_first = Breaker::new(1_000, Arc::from("sat-slow-first"));
        for i in 0..10_000_u32 {
            let m = i % 20;
            let (outcome, dur) = if m < 6 {
                (Outcome::Ok, Duration::from_millis(100))
            } else if m < 13 {
                (Outcome::Ok, Duration::from_millis(900))
            } else {
                (Outcome::Failure, Duration::from_millis(900))
            };
            b_slow_first.record_call(outcome, dur, now);

            // Check right past the first `slow` 12-bit wrap point (call 5,860) and at 10,000 calls.
            if i + 1 == 5_860 || i + 1 == 10_000 {
                assert_eq!(b_slow_first.state(now), BreakerState::Closed);
                let (w_ok, w_fail, w_slow) = b_slow_first.window_totals(now);
                assert!(w_ok <= 4_095 && w_fail <= 4_095 && w_slow <= 4_095);
                let calls = (w_ok + w_fail) as f64;
                let sr = (w_slow as f64) / calls;
                let fr = (w_fail as f64) / calls;
                assert!(
                    (sr - 0.70).abs() <= 0.01,
                    "at call {}: expected slow/calls ~0.70, got {sr} (ok={w_ok}, fail={w_fail}, slow={w_slow})",
                    i + 1
                );
                assert!(
                    (fr - 0.35).abs() <= 0.01,
                    "at call {}: expected fail/calls ~0.35, got {fr}",
                    i + 1
                );
            }
        }
    }

    // spec: RTE-082, RTE-085, OPS-020
    #[test]
    fn breaker_k_saturates_at_6_in_tracing_events_with_60s_duration() {
        let events_buf = StdArc::new(StdMutex::new(Vec::new()));
        let sub = TransitionSubscriber {
            events: StdArc::clone(&events_buf),
        };

        with_transition_subscriber(sub, || {
            let b = Breaker::new(1_000, Arc::from("k-sat-6"));
            let mut ms = 0_u64;

            // Initial trip: Closed -> Open(k = 0), 5,000 ms.
            for _ in 0..20 {
                b.record_call(
                    Outcome::Failure,
                    Duration::from_millis(10),
                    Now::from_ms(ms),
                );
            }

            let expected_steps: [(u64, u64); 7] = [
                (5_000, 1),  // Open(0)->HalfOpen, fail 2 probes -> Open(1) [10 s]
                (10_000, 2), // Open(1)->HalfOpen, fail 2 probes -> Open(2) [20 s]
                (20_000, 3), // Open(2)->HalfOpen, fail 2 probes -> Open(3) [40 s]
                (40_000, 4), // Open(3)->HalfOpen, fail 2 probes -> Open(4) [60 s]
                (60_000, 5), // Open(4)->HalfOpen, fail 2 probes -> Open(5) [60 s]
                (60_000, 6), // Open(5)->HalfOpen, fail 2 probes -> Open(6) [60 s]
                (60_000, 6), // Open(6)->HalfOpen, fail 2 probes -> Open(6) [saturates at 6, 60 s]
            ];

            for (wait_ms, _next_k) in expected_steps {
                assert_eq!(b.state(Now::from_ms(ms + wait_ms - 1)), BreakerState::Open);
                ms += wait_ms;
                let now = Now::from_ms(ms);
                match (b.admit(now), b.admit(now)) {
                    (Admission::Probe(p1), Admission::Probe(p2)) => {
                        p1.record(Outcome::Failure, Duration::from_millis(10), now);
                        p2.record(Outcome::Failure, Duration::from_millis(10), now);
                    }
                    other => panic!("expected 2 probes at ms={ms}, got {other:?}"),
                }
                match b.admit(now) {
                    Admission::Rejected { retry_after } => {
                        if _next_k >= 4 {
                            assert_eq!(retry_after, Duration::from_millis(60_000));
                        }
                    }
                    other => panic!("expected Rejected after reopening at ms={ms}, got {other:?}"),
                }
            }

            // Close at Open(6) + 60,000 ms, then re-trip within 5 minutes -> Closed -> Open(k = 6) lasting 60 s.
            assert_eq!(b.state(Now::from_ms(ms + 59_999)), BreakerState::Open);
            ms += 60_000;
            let t_close = Now::from_ms(ms);
            match (b.admit(t_close), b.admit(t_close)) {
                (Admission::Probe(p1), Admission::Probe(p2)) => {
                    p1.record(Outcome::Ok, Duration::from_millis(5), t_close);
                    p2.record(Outcome::Ok, Duration::from_millis(5), t_close);
                }
                other => panic!("expected 2 probes to close at ms={ms}, got {other:?}"),
            }
            assert_eq!(b.state(t_close), BreakerState::Closed);

            ms += 1_000;
            let t_retrip = Now::from_ms(ms);
            for _ in 0..20 {
                b.record_call(Outcome::Failure, Duration::from_millis(10), t_retrip);
            }
            match b.admit(t_retrip) {
                Admission::Rejected { retry_after } => {
                    assert_eq!(retry_after, Duration::from_millis(60_000));
                }
                other => panic!("expected Open(6) 60s rejection, got {other:?}"),
            }
        });

        let evs = events_buf.lock().expect("events");
        let observed_ks: Vec<(&str, &str, u64)> = evs
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.k))
            .collect();
        assert_eq!(
            observed_ks,
            vec![
                ("Closed", "Open", 0),
                ("Open", "HalfOpen", 0),
                ("HalfOpen", "Open", 1),
                ("Open", "HalfOpen", 1),
                ("HalfOpen", "Open", 2),
                ("Open", "HalfOpen", 2),
                ("HalfOpen", "Open", 3),
                ("Open", "HalfOpen", 3),
                ("HalfOpen", "Open", 4),
                ("Open", "HalfOpen", 4),
                ("HalfOpen", "Open", 5),
                ("Open", "HalfOpen", 5),
                ("HalfOpen", "Open", 6),
                ("Open", "HalfOpen", 6),
                ("HalfOpen", "Open", 6),
                ("Open", "HalfOpen", 6),
                ("HalfOpen", "Closed", 6),
                ("Closed", "Open", 6),
            ]
        );
    }
}
