//! Criterion microbenchmarks for `gap-balance` P2C selection (`n = 2, 8, 64`, with and without contention).

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use gap_balance::{
    Candidate, InFlightGuard, Now, Outcome, SlotParams, TelemetrySlot, XorShift64, select,
};
use gap_core::ids::BranchUid;

fn make_uid(seed: usize) -> BranchUid {
    BranchUid::from_unix_ms(1_700_000_000_000 + (seed as u64), [seed as u8; 10])
}

fn make_slots(n: usize) -> (Vec<BranchUid>, Vec<Arc<TelemetrySlot>>) {
    let now = Now::from_ms(1_000);
    let mut uids = Vec::with_capacity(n);
    let mut slots = Vec::with_capacity(n);
    for i in 0..n {
        uids.push(make_uid(i + 1));
        let slot = Arc::new(TelemetrySlot::new(SlotParams {
            max_in_flight: 128,
            default_timeout_ms: 1_000,
            label: Arc::from("bench-select"),
        }));
        // Leave 1 in 4 slots with unknown latency so lower-median computation is exercised too.
        if i % 4 != 3 {
            slot.record(Outcome::Ok, Duration::from_micros(80 + (i as u64) * 7), now);
        }
        slots.push(slot);
    }
    (uids, slots)
}

struct WriterPool {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl WriterPool {
    fn start(slots: &[Arc<TelemetrySlot>], num_threads: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let shared: Arc<[Arc<TelemetrySlot>]> = Arc::from(slots.to_vec());
        let mut workers = Vec::with_capacity(num_threads);
        for tid in 0..num_threads {
            let stop_flag = Arc::clone(&stop);
            let target_slots = Arc::clone(&shared);
            workers.push(thread::spawn(move || {
                let mut idx = tid % target_slots.len();
                let mut tick_ms = 1_000_u64;
                while !stop_flag.load(Ordering::Relaxed) {
                    let s = &target_slots[idx];
                    let g = InFlightGuard::acquire(s);
                    tick_ms = tick_ms.wrapping_add(4);
                    s.record(
                        Outcome::Ok,
                        Duration::from_micros(90 + (idx as u64) * 5),
                        Now::from_ms(tick_ms),
                    );
                    drop(g);
                    idx = (idx + 1) % target_slots.len();
                }
            }));
        }
        Self { stop, workers }
    }
}

impl Drop for WriterPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn bench_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("p2c_select");
    let now = Now::from_ms(1_500);

    for &n in &[2_usize, 8, 64] {
        let (uids, slots) = make_slots(n);
        let cands: Vec<Candidate<'_>> = uids
            .iter()
            .zip(slots.iter())
            .map(|(&uid, s)| Candidate { uid, slot: s })
            .collect();
        let mut rng = XorShift64::new(0x1234_5678_9abc_def0 ^ (n as u64));

        group.bench_with_input(BenchmarkId::new("uncontended", n), &n, |b, _| {
            b.iter(|| {
                black_box(select(black_box(&cands), black_box(now), &mut rng));
            });
        });

        let (uids_c, slots_c) = make_slots(n);
        let _writers = WriterPool::start(&slots_c, 4);
        let cands_c: Vec<Candidate<'_>> = uids_c
            .iter()
            .zip(slots_c.iter())
            .map(|(&uid, s)| Candidate { uid, slot: s })
            .collect();
        let mut rng_c = XorShift64::new(0xfedc_ba98_7654_3210 ^ (n as u64));

        group.bench_with_input(BenchmarkId::new("contended_4w", n), &n, |b, _| {
            b.iter(|| {
                black_box(select(black_box(&cands_c), black_box(now), &mut rng_c));
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_selection);
criterion_main!(benches);
