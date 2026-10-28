//! Standalone release tail-percentile runner (`p50`, `p99`, `p99.9`) using `hdrhistogram` (`m3-01-design.md` §7).
//!
//! Ignores all command-line arguments (`--quick`, `--bench`, filters), never asserts timing, and
//! prints both raw nanosecond percentiles and timer-overhead-subtracted percentiles over 100,000
//! warm-up and 1,000,000 timed operations per scenario.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gap_balance::{
    Candidate, DecayedEwma, InFlightGuard, Now, Outcome, RetryBudget, SlotParams, TelemetrySlot,
    XorShift64, select,
};
use gap_core::ids::BranchUid;
use hdrhistogram::Histogram;

const WARMUP_ITERS: u64 = 100_000;
const TIMED_ITERS: u64 = 1_000_000;

struct Stats {
    p50: u64,
    p99: u64,
    p999: u64,
}

fn measure<F: FnMut()>(mut op: F) -> Stats {
    for _ in 0..WARMUP_ITERS {
        op();
    }

    let mut hist = match Histogram::<u64>::new(3) {
        Ok(h) => h,
        Err(_) => {
            return Stats {
                p50: 0,
                p99: 0,
                p999: 0,
            };
        }
    };

    for _ in 0..TIMED_ITERS {
        let start = Instant::now();
        op();
        let ns = start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let _ = hist.record(ns);
    }

    Stats {
        p50: hist.value_at_quantile(0.50),
        p99: hist.value_at_quantile(0.99),
        p999: hist.value_at_quantile(0.999),
    }
}

fn print_row(name: &str, s: &Stats, overhead_p50: Option<u64>) {
    match overhead_p50 {
        None => {
            println!(
                "{name} n={TIMED_ITERS} p50={}ns p99={}ns p99.9={}ns",
                s.p50, s.p99, s.p999
            );
        }
        Some(ovh) => {
            println!(
                "{name} n={TIMED_ITERS} p50={}ns p99={}ns p99.9={}ns (net p50={}ns p99={}ns p99.9={}ns)",
                s.p50,
                s.p99,
                s.p999,
                s.p50.saturating_sub(ovh),
                s.p99.saturating_sub(ovh),
                s.p999.saturating_sub(ovh),
            );
        }
    }
}

fn make_uid(seed: usize) -> BranchUid {
    BranchUid::from_unix_ms(1_700_000_000_000 + (seed as u64), [seed as u8; 10])
}

fn make_candidate_group(n: usize) -> (Vec<BranchUid>, Vec<Arc<TelemetrySlot>>) {
    let now = Now::from_ms(1_000);
    let mut uids = Vec::with_capacity(n);
    let mut slots = Vec::with_capacity(n);
    for i in 0..n {
        uids.push(make_uid(i + 1));
        let slot = Arc::new(TelemetrySlot::new(SlotParams {
            max_in_flight: 128,
            default_timeout_ms: 1_000,
            label: Arc::from("pct-branch"),
        }));
        if i % 4 != 3 {
            slot.record(Outcome::Ok, Duration::from_micros(80 + (i as u64) * 7), now);
        }
        slots.push(slot);
    }
    (uids, slots)
}

fn main() {
    // Explicitly ignore all CLI arguments (`--quick`, `--bench`, etc.).
    for arg in std::env::args_os() {
        let _ = black_box(arg);
    }

    let overhead = measure(|| {
        black_box(());
    });
    print_row("timer_overhead", &overhead, None);
    let ovh = Some(overhead.p50);

    // 1. DecayedEwma::update
    let ewma = DecayedEwma::new();
    let mut tick = 0_u32;
    let s_ewma = measure(|| {
        tick = tick.wrapping_add(1);
        black_box(ewma.update(black_box(125.0), black_box(tick), 10_000.0));
    });
    print_row("ewma_update_1t", &s_ewma, ovh);

    // 2. TelemetrySlot::record (Ok, 1 thread)
    let slot_ok = Arc::new(TelemetrySlot::new(SlotParams {
        max_in_flight: 256,
        default_timeout_ms: 1_000,
        label: Arc::from("pct-slot-ok"),
    }));
    let mut ms_ok = 0_u64;
    let s_rec_ok = measure(|| {
        ms_ok = ms_ok.wrapping_add(4);
        slot_ok.record(
            black_box(Outcome::Ok),
            black_box(Duration::from_micros(150)),
            black_box(Now::from_ms(ms_ok)),
        );
    });
    print_row("slot_record_ok_1t", &s_rec_ok, ovh);

    // 2b. TelemetrySlot::record (pure Failure in Closed window, 1 thread; baseline seeded outside timer)
    let slot_fail = Arc::new(TelemetrySlot::new(SlotParams {
        max_in_flight: 256,
        default_timeout_ms: 1_000,
        label: Arc::from("pct-slot-fail"),
    }));
    let s_rec_fail = {
        let mut hist = match Histogram::<u64>::new(3) {
            Ok(h) => h,
            Err(_) => unreachable!(),
        };
        let mut epoch_ms = 0_u64;
        let mut fail_in_epoch = 1_000_u32;
        for iter in 0..(WARMUP_ITERS + TIMED_ITERS) {
            if fail_in_epoch >= 1_000 {
                epoch_ms = epoch_ms.wrapping_add(30_000);
                let seed_now = Now::from_ms(epoch_ms);
                for _ in 0..2_000 {
                    slot_fail.record(Outcome::Ok, Duration::from_micros(100), seed_now);
                }
                fail_in_epoch = 0;
            }
            fail_in_epoch += 1;
            let now = Now::from_ms(epoch_ms);
            let start = Instant::now();
            slot_fail.record(
                black_box(Outcome::Failure),
                black_box(Duration::from_micros(450)),
                black_box(now),
            );
            let ns = start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            if iter >= WARMUP_ITERS {
                let _ = hist.record(ns);
            }
        }
        Stats {
            p50: hist.value_at_quantile(0.50),
            p99: hist.value_at_quantile(0.99),
            p999: hist.value_at_quantile(0.999),
        }
    };
    print_row("slot_record_failure_1t", &s_rec_fail, ovh);

    // 3. TelemetrySlot::record (Ok, 8 contending threads)
    let slot_ok_8t = Arc::new(TelemetrySlot::new(SlotParams {
        max_in_flight: 256,
        default_timeout_ms: 1_000,
        label: Arc::from("pct-slot-8t"),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::with_capacity(7);
    for tid in 0..7_u64 {
        let s = Arc::clone(&slot_ok_8t);
        let flag = Arc::clone(&stop);
        workers.push(thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                s.record(
                    Outcome::Ok,
                    Duration::from_micros(110 + tid),
                    Now::from_ms(1_000),
                );
            }
        }));
    }
    let s_rec_ok_8t = measure(|| {
        slot_ok_8t.record(
            black_box(Outcome::Ok),
            black_box(Duration::from_micros(150)),
            black_box(Now::from_ms(1_000)),
        );
    });
    stop.store(true, Ordering::Release);
    for w in workers {
        let _ = w.join();
    }
    print_row("slot_record_ok_8t", &s_rec_ok_8t, ovh);

    // 4. InFlightGuard acquire + drop
    let s_guard = measure(|| {
        let g = InFlightGuard::acquire(black_box(&slot_ok));
        black_box(&g);
        drop(g);
    });
    print_row("in_flight_guard_1t", &s_guard, ovh);

    // 5. RetryBudget deposit + withdraw
    let budget = RetryBudget::new();
    let s_deposit = measure(|| {
        budget.deposit_first_attempt();
        black_box(budget.balance_millis());
    });
    print_row("retry_budget_deposit_1t", &s_deposit, ovh);

    let s_withdraw = measure(|| {
        if !budget.try_withdraw() {
            for _ in 0..10 {
                budget.deposit_first_attempt();
            }
            black_box(budget.try_withdraw());
        }
    });
    print_row("retry_budget_withdraw_1t", &s_withdraw, ovh);

    // 6. P2C select (n = 2, 8, 64)
    let now = Now::from_ms(1_500);
    for &(name, n) in &[
        ("select_n2_1t", 2_usize),
        ("select_n8_1t", 8),
        ("select_n64_1t", 64),
    ] {
        let (uids, slots) = make_candidate_group(n);
        let cands: Vec<Candidate<'_>> = uids
            .iter()
            .zip(slots.iter())
            .map(|(&uid, s)| Candidate { uid, slot: s })
            .collect();
        let mut rng = XorShift64::new(0x9876_5432_10fe_dcba ^ (n as u64));
        let st = measure(|| {
            black_box(select(black_box(&cands), black_box(now), &mut rng));
        });
        print_row(name, &st, ovh);
    }

    // 7. P2C select (n = 64, 4 contending writer threads)
    let (uids_c, slots_c) = make_candidate_group(64);
    let stop_sel = Arc::new(AtomicBool::new(false));
    let shared_slots: Arc<[Arc<TelemetrySlot>]> = Arc::from(slots_c.clone());
    let mut sel_workers = Vec::with_capacity(4);
    for tid in 0..4_usize {
        let flag = Arc::clone(&stop_sel);
        let group = Arc::clone(&shared_slots);
        sel_workers.push(thread::spawn(move || {
            let mut idx = tid;
            let mut tick_ms = 1_000_u64;
            while !flag.load(Ordering::Relaxed) {
                let s = &group[idx];
                let g = InFlightGuard::acquire(s);
                tick_ms = tick_ms.wrapping_add(4);
                s.record(
                    Outcome::Ok,
                    Duration::from_micros(90 + (idx as u64) * 5),
                    Now::from_ms(tick_ms),
                );
                drop(g);
                idx = (idx + 1) % group.len();
            }
        }));
    }
    let cands_c: Vec<Candidate<'_>> = uids_c
        .iter()
        .zip(slots_c.iter())
        .map(|(&uid, s)| Candidate { uid, slot: s })
        .collect();
    let mut rng_c = XorShift64::new(0x1122_3344_5566_7788);
    let st_c = measure(|| {
        black_box(select(black_box(&cands_c), black_box(now), &mut rng_c));
    });
    stop_sel.store(true, Ordering::Release);
    for w in sel_workers {
        let _ = w.join();
    }
    print_row("select_n64_contended_4w", &st_c, ovh);
}
