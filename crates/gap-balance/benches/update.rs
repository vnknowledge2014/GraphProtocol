//! Criterion microbenchmarks for `gap-balance` slot update primitives (`m3-01-design.md` §7).
//!
//! Covers single-thread and 8-contending-threads latency for:
//! - `DecayedEwma::update`
//! - `TelemetrySlot::record` (`Outcome::Ok` and `Outcome::Failure`)
//! - `InFlightGuard::acquire` + `Drop`
//! - `RetryBudget::deposit_first_attempt` and `RetryBudget::try_withdraw`

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use gap_balance::{
    DecayedEwma, InFlightGuard, Now, Outcome, RetryBudget, SlotParams, TelemetrySlot,
};

fn make_slot() -> Arc<TelemetrySlot> {
    Arc::new(TelemetrySlot::new(SlotParams {
        max_in_flight: 256,
        default_timeout_ms: 1_000,
        label: Arc::from("bench-update"),
    }))
}

struct ContentionPool {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl ContentionPool {
    fn spawn<F>(n_bg: usize, f: F) -> Self
    where
        F: Fn(usize) + Send + Sync + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let task = Arc::new(f);
        let mut workers = Vec::with_capacity(n_bg);
        for tid in 0..n_bg {
            let stop_flag = Arc::clone(&stop);
            let body = Arc::clone(&task);
            workers.push(thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    body(tid);
                }
            }));
        }
        Self { stop, workers }
    }
}

impl Drop for ContentionPool {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn bench_ewma_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma_update");

    let ewma_1t = DecayedEwma::new();
    let mut tick = 0_u32;
    group.bench_function("1t", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            black_box(ewma_1t.update(black_box(125.0), black_box(tick), 10_000.0));
        });
    });

    let ewma_8t = Arc::new(DecayedEwma::new());
    let bg_ewma = Arc::clone(&ewma_8t);
    let _pool = ContentionPool::spawn(7, move |tid| {
        let sample = 100.0 + (tid as f32);
        let _ = bg_ewma.update(sample, 1_000, 10_000.0);
    });
    let mut tick_8t = 1_000_u32;
    group.bench_function("8t", |b| {
        b.iter(|| {
            tick_8t = tick_8t.wrapping_add(1);
            black_box(ewma_8t.update(black_box(125.0), black_box(tick_8t), 10_000.0));
        });
    });

    group.finish();
}

fn bench_slot_record(c: &mut Criterion) {
    let mut group = c.benchmark_group("slot_record");

    let slot_ok = make_slot();
    let mut ms_ok = 0_u64;
    group.bench_function("ok_1t", |b| {
        b.iter(|| {
            ms_ok = ms_ok.wrapping_add(4);
            slot_ok.record(
                black_box(Outcome::Ok),
                black_box(Duration::from_micros(150)),
                black_box(Now::from_ms(ms_ok)),
            );
        });
    });

    let slot_fail = make_slot();
    // Refresh a 2,000-Ok baseline every 1,000 Failure calls across 30s epochs so `slot_fail`
    // remains `Closed` (`fail_rate <= 1000 / 3000 < 0.5`) and measures pure Failure recording.
    let mut epoch_ms = 0_u64;
    let mut fail_in_epoch = 1_000_u32;
    group.bench_function("failure_1t", |b| {
        b.iter(|| {
            if fail_in_epoch >= 1_000 {
                epoch_ms = epoch_ms.wrapping_add(30_000);
                let seed_now = Now::from_ms(epoch_ms);
                for _ in 0..2_000 {
                    slot_fail.record(Outcome::Ok, Duration::from_micros(100), seed_now);
                }
                fail_in_epoch = 0;
            }
            fail_in_epoch += 1;
            slot_fail.record(
                black_box(Outcome::Failure),
                black_box(Duration::from_micros(450)),
                black_box(Now::from_ms(epoch_ms)),
            );
        });
    });

    let slot_ok_8t = make_slot();
    let bg_ok = Arc::clone(&slot_ok_8t);
    let _pool_ok = ContentionPool::spawn(7, move |tid| {
        bg_ok.record(
            Outcome::Ok,
            Duration::from_micros(100 + (tid as u64)),
            Now::from_ms(1_000),
        );
    });
    group.bench_function("ok_8t", |b| {
        b.iter(|| {
            slot_ok_8t.record(
                black_box(Outcome::Ok),
                black_box(Duration::from_micros(150)),
                black_box(Now::from_ms(1_000)),
            );
        });
    });

    let slot_fail_8t = make_slot();
    let bg_fail = Arc::clone(&slot_fail_8t);
    let _pool_fail = ContentionPool::spawn(7, move |_| {
        bg_fail.record(Outcome::Ok, Duration::from_micros(100), Now::from_ms(1_000));
    });
    group.bench_function("failure_8t", |b| {
        b.iter(|| {
            slot_fail_8t.record(
                black_box(Outcome::Failure),
                black_box(Duration::from_micros(450)),
                black_box(Now::from_ms(1_000)),
            );
        });
    });

    group.finish();
}

fn bench_in_flight_guard(c: &mut Criterion) {
    let mut group = c.benchmark_group("in_flight_guard");

    let slot_1t = make_slot();
    group.bench_function("acquire_drop_1t", |b| {
        b.iter(|| {
            let guard = InFlightGuard::acquire(black_box(&slot_1t));
            black_box(&guard);
            drop(guard);
        });
    });

    let slot_8t = make_slot();
    let bg_slot = Arc::clone(&slot_8t);
    let _pool = ContentionPool::spawn(7, move |_| {
        let g = InFlightGuard::acquire(&bg_slot);
        black_box(&g);
        drop(g);
    });
    group.bench_function("acquire_drop_8t", |b| {
        b.iter(|| {
            let guard = InFlightGuard::acquire(black_box(&slot_8t));
            black_box(&guard);
            drop(guard);
        });
    });

    group.finish();
}

fn bench_retry_budget(c: &mut Criterion) {
    let mut group = c.benchmark_group("retry_budget");

    let budget_1t = RetryBudget::new();
    group.bench_function("deposit_1t", |b| {
        b.iter(|| {
            budget_1t.deposit_first_attempt();
            black_box(budget_1t.balance_millis());
        });
    });

    group.bench_function("withdraw_1t", |b| {
        b.iter(|| {
            if !budget_1t.try_withdraw() {
                for _ in 0..10 {
                    budget_1t.deposit_first_attempt();
                }
                black_box(budget_1t.try_withdraw());
            }
        });
    });

    let budget_8t = Arc::new(RetryBudget::new());
    let bg_budget = Arc::clone(&budget_8t);
    let _pool = ContentionPool::spawn(7, move |tid| {
        if tid % 2 == 0 {
            bg_budget.deposit_first_attempt();
        } else {
            let _ = bg_budget.try_withdraw();
        }
    });

    group.bench_function("deposit_8t", |b| {
        b.iter(|| {
            budget_8t.deposit_first_attempt();
        });
    });

    group.bench_function("withdraw_8t", |b| {
        b.iter(|| {
            black_box(budget_8t.try_withdraw());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_ewma_update,
    bench_slot_record,
    bench_in_flight_guard,
    bench_retry_budget
);
criterion_main!(benches);
