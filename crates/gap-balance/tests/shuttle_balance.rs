//! Randomized and PCT concurrency tests for `gap-balance` under `shuttle` (`cfg(gap_shuttle)`).
//!
//! Run with:
//! `RUSTFLAGS="--cfg gap_shuttle" cargo test -p gap-balance --test 'shuttle_*' --locked --offline --target-dir target/shuttle`
#![cfg(gap_shuttle)]

use std::fmt;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use gap_balance::{
    Admission, BreakerState, HealthStatus, InFlightGuard, Now, Outcome, RetryBudget, SlotParams,
    TelemetryRegistry, TelemetrySlot,
};
use gap_core::clock::ManualClock;
use gap_core::ids::{BranchUid, TenantId};
use gap_core::snapshot::TelemetrySlotKey;
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::sync::Arc;
use shuttle::sync::atomic::{AtomicU32, Ordering};
use shuttle::{Config, FailurePersistence, Runner, thread};
use tracing::Dispatch;
use tracing::field::{Field, Visit};

const RANDOM_ITERS: usize = 10_000;
const PCT_ITERS: usize = 250;
const PCT_DEPTH: usize = 3;

fn run_shuttle_suite<F>(seed: u64, f: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let mut cfg = Config::new();
    cfg.failure_persistence = FailurePersistence::Print;
    cfg.silence_warnings = true;

    let body = std::sync::Arc::new(f);
    {
        let b = std::sync::Arc::clone(&body);
        let sched = RandomScheduler::new_from_seed(seed, RANDOM_ITERS);
        let runner = Runner::new(sched, cfg.clone());
        runner.run(move || b());
    }
    {
        let b = std::sync::Arc::clone(&body);
        let sched = PctScheduler::new_from_seed(seed ^ 0x9e37_79b9_7f4a_7c15, PCT_DEPTH, PCT_ITERS);
        let runner = Runner::new(sched, cfg);
        runner.run(move || b());
    }
}

fn make_slot(max_in_flight: u32) -> Arc<TelemetrySlot> {
    Arc::new(TelemetrySlot::new(SlotParams {
        max_in_flight,
        default_timeout_ms: 1_000,
        label: Arc::from("shuttle-branch"),
    }))
}

#[derive(Default)]
struct TransitionVisitor {
    from: Option<String>,
    to: Option<String>,
}

impl Visit for TransitionVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "from" => self.from = Some(value.to_owned()),
            "to" => self.to = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "from" if self.from.is_none() => self.from = Some(format!("{value:?}")),
            "to" if self.to.is_none() => self.to = Some(format!("{value:?}")),
            _ => {}
        }
    }
}

struct EdgeRecorder {
    edges: std::sync::Arc<StdMutex<Vec<(String, String)>>>,
}

impl tracing::Subscriber for EdgeRecorder {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::INFO
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut v = TransitionVisitor::default();
        event.record(&mut v);
        if let (Some(from), Some(to)) = (v.from, v.to) {
            self.edges
                .lock()
                .expect("edge recorder mutex")
                .push((from, to));
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

// spec: RTE-080, RTE-084
#[test]
fn shuttle_bucket_conservation_single_epoch() {
    // N = 3 threads x M = 2 calls = 6 calls in epoch 2 (6,500 ms), staying < 20 calls so Closed
    // never trips: each thread records 1 slow Ok call (900 ms > 800 ms) and 1 fast Failure call.
    // Exact totals MUST be ok = 3, fail = 3, slow = 3 (calls = 6) under every interleaving.
    run_shuttle_suite(0x5170_0001, || {
        let slot = make_slot(32);
        let now = Now::from_ms(6_500);
        let mut handles = Vec::with_capacity(3);
        for _ in 0..3 {
            let s = Arc::clone(&slot);
            handles.push(thread::spawn(move || {
                s.record(Outcome::Ok, Duration::from_millis(900), now);
                s.record(Outcome::Failure, Duration::from_millis(10), now);
            }));
        }
        for h in handles {
            h.join().expect("worker join");
        }
        assert_eq!(slot.breaker().window_totals(now), (3, 3, 3));
    });
}

// spec: RTE-080, RTE-084
#[test]
fn shuttle_epoch_change_race_no_stale_leak_and_no_lost_first_increment() {
    // Epoch e = 2 (6,000 ms) and epoch e + 10 = 12 (36,000 ms) map to the SAME bucket slot (2).
    // 3 threads record fast Ok at e = 2; 3 threads record slow Ok (900 ms > 800 ms) at e + 10 = 12.
    // Under EVERY interleaving:
    // - zero counts from e = 2 may survive into e + 10 = 12;
    // - none of the 3 increments at e + 10 = 12 (including the first lazy-reset CAS) may be lost.
    // Therefore window_totals at e + 10 MUST be exact (3, 0, 3).
    run_shuttle_suite(0x5170_0002, || {
        let slot = make_slot(32);
        let e_old = Now::from_ms(6_000);
        let e_new = Now::from_ms(36_000);

        let mut handles = Vec::with_capacity(6);
        for _ in 0..3 {
            let s = Arc::clone(&slot);
            handles.push(thread::spawn(move || {
                s.record(Outcome::Ok, Duration::from_millis(10), e_old);
            }));
        }
        for _ in 0..3 {
            let s = Arc::clone(&slot);
            handles.push(thread::spawn(move || {
                s.record(Outcome::Ok, Duration::from_millis(900), e_new);
            }));
        }
        for h in handles {
            h.join().expect("worker join");
        }
        assert_eq!(slot.breaker().window_totals(e_new), (3, 0, 3));
    });
}

// spec: RTE-085
#[test]
fn shuttle_half_open_probe_bounds_no_drop_episode_cap() {
    // No dropped permits -> at most 3 ProbePermits alive concurrently AND at most 3 issued in the
    // episode across 5 racing callers.
    run_shuttle_suite(0x5170_0003, || {
        let slot = make_slot(32);
        for _ in 0..20 {
            slot.record(Outcome::Failure, Duration::from_millis(10), Now::from_ms(0));
        }
        let t_probe = Now::from_ms(5_000);
        let alive = Arc::new(AtomicU32::new(0));
        let max_alive = Arc::new(AtomicU32::new(0));
        let issued = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::with_capacity(5);
        for i in 0..5 {
            let s = Arc::clone(&slot);
            let alive = Arc::clone(&alive);
            let max_alive = Arc::clone(&max_alive);
            let issued = Arc::clone(&issued);
            handles.push(thread::spawn(move || {
                if let Admission::Probe(permit) = s.breaker().admit(t_probe) {
                    issued.fetch_add(1, Ordering::AcqRel);
                    let cur = alive.fetch_add(1, Ordering::AcqRel) + 1;
                    max_alive.fetch_max(cur, Ordering::AcqRel);
                    assert!(cur <= 3, "more than 3 concurrent ProbePermits alive: {cur}");
                    thread::yield_now();
                    alive.fetch_sub(1, Ordering::AcqRel);
                    let out = if i % 2 == 0 {
                        Outcome::Ok
                    } else {
                        Outcome::Failure
                    };
                    permit.record(out, Duration::from_millis(10), t_probe);
                }
            }));
        }
        for h in handles {
            h.join().expect("probe worker join");
        }
        assert_eq!(alive.load(Ordering::Acquire), 0);
        assert!(max_alive.load(Ordering::Acquire) <= 3);
        assert!(issued.load(Ordering::Acquire) <= 3);
    });
}

// spec: RTE-085
#[test]
fn shuttle_half_open_probe_bounds_with_cancelled_permits() {
    // With cancelled/dropped permits -> at most 3 ProbePermits alive concurrently at any point,
    // and dropped permits release their slots cleanly.
    run_shuttle_suite(0x5170_0004, || {
        let slot = make_slot(32);
        for _ in 0..20 {
            slot.record(Outcome::Failure, Duration::from_millis(10), Now::from_ms(0));
        }
        let t_probe = Now::from_ms(5_000);
        let alive = Arc::new(AtomicU32::new(0));
        let max_alive = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::with_capacity(5);
        for i in 0..5 {
            let s = Arc::clone(&slot);
            let alive = Arc::clone(&alive);
            let max_alive = Arc::clone(&max_alive);
            handles.push(thread::spawn(move || {
                if let Admission::Probe(permit) = s.breaker().admit(t_probe) {
                    let cur = alive.fetch_add(1, Ordering::AcqRel) + 1;
                    max_alive.fetch_max(cur, Ordering::AcqRel);
                    assert!(cur <= 3, "more than 3 concurrent ProbePermits alive: {cur}");
                    thread::yield_now();
                    alive.fetch_sub(1, Ordering::AcqRel);
                    if i < 2 {
                        drop(permit);
                    } else {
                        permit.record(Outcome::Ok, Duration::from_millis(10), t_probe);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("drop/record probe worker join");
        }
        assert_eq!(alive.load(Ordering::Acquire), 0);
        assert!(max_alive.load(Ordering::Acquire) <= 3);
    });
}

// spec: RTE-082, RTE-085
#[test]
fn shuttle_single_thread_transitions_open_to_half_open() {
    let edges = std::sync::Arc::new(StdMutex::new(Vec::new()));
    let dispatch = Dispatch::new(EdgeRecorder {
        edges: std::sync::Arc::clone(&edges),
    });

    // Install the subscriber OUTSIDE `Runner::run` AND inside each spawned Shuttle task so Shuttle's
    // per-task tracing context inherits `EdgeRecorder` across every coroutine switch.
    tracing::dispatcher::with_default(&dispatch, || {
        let d_outer = dispatch.clone();
        run_shuttle_suite(0x5170_0005, move || {
            let d_main = d_outer.clone();
            let edges_inner = std::sync::Arc::clone(&edges);
            tracing::dispatcher::with_default(&d_main, || {
                edges_inner.lock().expect("reset edges").clear();
                let slot = make_slot(32);
                for _ in 0..20 {
                    slot.record(Outcome::Failure, Duration::from_millis(10), Now::from_ms(0));
                }
                assert_eq!(
                    edges_inner.lock().expect("check Closed -> Open").as_slice(),
                    &[("Closed".to_owned(), "Open".to_owned())]
                );
                edges_inner.lock().expect("clear initial open").clear();

                let t_due = Now::from_ms(5_000);
                let mut handles = Vec::with_capacity(4);
                for i in 0..4 {
                    let s = Arc::clone(&slot);
                    let d_task = d_main.clone();
                    handles.push(thread::spawn(move || {
                        tracing::dispatcher::with_default(&d_task, || {
                            if i % 2 == 0 {
                                let st = s.breaker().state(t_due);
                                assert_eq!(st, BreakerState::HalfOpen);
                            } else {
                                let adm = s.breaker().admit(t_due);
                                drop(adm);
                            }
                        });
                    }));
                }
                for h in handles {
                    h.join().expect("timer racer join");
                }

                let recorded = edges_inner.lock().expect("read edges");
                assert_eq!(
                    recorded.as_slice(),
                    &[("Open".to_owned(), "HalfOpen".to_owned())],
                    "exact one thread must perform Open -> HalfOpen; saw {recorded:?}"
                );
            });
        });
    });
}

// spec: RTE-083, RTE-085
#[test]
fn shuttle_legal_state_edges_and_transition_flow_conservation() {
    let edges = std::sync::Arc::new(StdMutex::new(Vec::new()));
    let dispatch = Dispatch::new(EdgeRecorder {
        edges: std::sync::Arc::clone(&edges),
    });

    tracing::dispatcher::with_default(&dispatch, || {
        let d_outer = dispatch.clone();
        run_shuttle_suite(0x5170_0006, move || {
            let d_main = d_outer.clone();
            let edges_inner = std::sync::Arc::clone(&edges);
            tracing::dispatcher::with_default(&d_main, || {
                edges_inner.lock().expect("reset edges").clear();

                let slot = make_slot(16);
                slot.apply_health(HealthStatus::NotServing, None, Now::from_ms(0));
                slot.apply_health(HealthStatus::Serving, None, Now::from_ms(0));

                let t1 = Now::from_ms(5_000);

                let s0 = Arc::clone(&slot);
                let d0 = d_main.clone();
                let h0 = thread::spawn(move || {
                    tracing::dispatcher::with_default(&d0, || {
                        if let Admission::Probe(p) = s0.breaker().admit(t1) {
                            thread::yield_now();
                            p.record(Outcome::Ok, Duration::from_millis(5), t1);
                        }
                    });
                });

                let s1 = Arc::clone(&slot);
                let d1 = d_main.clone();
                let h1 = thread::spawn(move || {
                    tracing::dispatcher::with_default(&d1, || {
                        if let Admission::Probe(p) = s1.breaker().admit(t1) {
                            thread::yield_now();
                            p.record(Outcome::Ok, Duration::from_millis(5), t1);
                        }
                    });
                });

                let s2 = Arc::clone(&slot);
                let d2 = d_main.clone();
                let h2 = thread::spawn(move || {
                    tracing::dispatcher::with_default(&d2, || {
                        s2.apply_health(HealthStatus::NotServing, None, t1);
                        thread::yield_now();
                        s2.apply_health(HealthStatus::Serving, None, t1);
                    });
                });

                let s3 = Arc::clone(&slot);
                let d3 = d_main.clone();
                let h3 = thread::spawn(move || {
                    tracing::dispatcher::with_default(&d3, || {
                        if let Admission::Probe(p) = s3.breaker().admit(t1) {
                            p.record(Outcome::Failure, Duration::from_millis(5), t1);
                        }
                    });
                });

                h0.join().expect("h0");
                h1.join().expect("h1");
                h2.join().expect("h2");
                h3.join().expect("h3");

                let final_state = match slot.breaker().state(t1) {
                    BreakerState::Closed => "Closed",
                    BreakerState::HalfOpen => "HalfOpen",
                    BreakerState::Open => "Open",
                };

                let recorded = edges_inner.lock().expect("edges");
                assert!(
                    recorded.len() >= 2,
                    "expected captured transitions across threads, got {recorded:?}"
                );

                let mut net_closed: i32 = 1; // initial state is Closed
                let mut net_half_open: i32 = 0;
                let mut net_open: i32 = 0;

                for (from, to) in recorded.iter() {
                    let legal = matches!(
                        (from.as_str(), to.as_str()),
                        ("Closed", "Open")
                            | ("Open", "HalfOpen")
                            | ("HalfOpen", "Closed")
                            | ("HalfOpen", "Open")
                    );
                    assert!(legal, "illegal breaker FSM edge: {from} -> {to}");

                    match from.as_str() {
                        "Closed" => net_closed -= 1,
                        "HalfOpen" => net_half_open -= 1,
                        "Open" => net_open -= 1,
                        other => panic!("unknown state {other}"),
                    }
                    match to.as_str() {
                        "Closed" => net_closed += 1,
                        "HalfOpen" => net_half_open += 1,
                        "Open" => net_open += 1,
                        other => panic!("unknown state {other}"),
                    }
                }

                assert_eq!(net_closed, i32::from(final_state == "Closed"));
                assert_eq!(net_half_open, i32::from(final_state == "HalfOpen"));
                assert_eq!(net_open, i32::from(final_state == "Open"));
            });
        });
    });
}

// spec: RTE-071
#[test]
fn shuttle_in_flight_guard_returns_to_zero() {
    run_shuttle_suite(0x5170_0007, || {
        let slot = make_slot(16);
        let mut handles = Vec::with_capacity(4);
        for _ in 0..4 {
            let s = Arc::clone(&slot);
            handles.push(thread::spawn(move || {
                let g1 = InFlightGuard::acquire(&s);
                assert!(s.in_flight() >= 1 && s.in_flight() <= 8);
                thread::yield_now();
                {
                    let _g2 = InFlightGuard::acquire(&s);
                    assert!(s.in_flight() >= 2 && s.in_flight() <= 8);
                    thread::yield_now();
                }
                drop(g1);
            }));
        }
        for h in handles {
            h.join().expect("guard worker join");
        }
        assert_eq!(slot.in_flight(), 0);
    });
}

// spec: RTE-091, RTE-093
#[test]
fn shuttle_retry_budget_11_concurrent_withdrawals_succeed_exact_10() {
    run_shuttle_suite(0x5170_0008, || {
        let budget = Arc::new(RetryBudget::new());
        let wins = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::with_capacity(11);
        for _ in 0..11 {
            let b = Arc::clone(&budget);
            let wins = Arc::clone(&wins);
            handles.push(thread::spawn(move || {
                if b.try_withdraw() {
                    wins.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for h in handles {
            h.join().expect("withdraw worker join");
        }
        assert_eq!(wins.load(Ordering::Acquire), 10);
        assert_eq!(budget.balance_millis(), 0);
    });
}

// spec: RTE-078, INV-09
#[test]
fn shuttle_registry_same_key_three_threads_idempotent() {
    let tenant = TenantId::new("acme").expect("tenant");
    let uid = BranchUid::from_unix_ms(1_700_000_000_000, [7_u8; 10]);
    let key = TelemetrySlotKey::new(tenant, uid);

    run_shuttle_suite(0x5170_0009, move || {
        let clock = std::sync::Arc::new(ManualClock::new(1_700_000_000_000));
        let reg = Arc::new(TelemetryRegistry::new(clock, 64));
        let mut handles = Vec::with_capacity(3);
        for tid in 0..3_u32 {
            let reg = Arc::clone(&reg);
            let k = key.clone();
            handles.push(thread::spawn(move || {
                reg.register(
                    k,
                    SlotParams {
                        max_in_flight: 10 + tid,
                        default_timeout_ms: 1_000,
                        label: Arc::from("shuttle-reg"),
                    },
                )
                .expect("idempotent register")
            }));
        }
        let mut issued = Vec::with_capacity(3);
        for h in handles {
            issued.push(h.join().expect("register worker"));
        }
        assert_eq!(reg.len(), 1);
        for s in issued {
            assert_eq!(s.index(), 0);
        }
    });
}

// spec: RTE-084, RTE-085
#[test]
fn shuttle_breaker_generation_race_stale_writer_never_reopens_closed() {
    // Thread 1 enters `record` in generation 0 on the 20th failure at `t0 = 0` (`bucket[0]`).
    // Thread 2 drives `Open -> HalfOpen -> Closed(gen 1)` via NotServing/Serving + 2 Ok probes at
    // `t1 = 30_000` (which also maps to `bucket[0]`, 10 epochs later) and records 5 Ok calls in `gen 1`.
    // Under every schedule:
    // - stale generation-0 trip CAS must never reopen `gen 1` (`state(t1) == Closed`);
    // - stale generation-0 bucket CAS must never overwrite `gen 1`'s `bucket[0]` (`window_totals(t1) == (5, 0, 0)`).
    run_shuttle_suite(0x5170_000a, || {
        let slot = make_slot(32);
        let t0 = Now::from_ms(0);
        let t1 = Now::from_ms(30_000);
        for _ in 0..19 {
            slot.record(Outcome::Failure, Duration::from_millis(10), t0);
        }

        let s_stale = Arc::clone(&slot);
        let writer = thread::spawn(move || {
            s_stale.record(Outcome::Failure, Duration::from_millis(10), t0);
        });

        let s_rec = Arc::clone(&slot);
        let recover = thread::spawn(move || {
            s_rec.apply_health(HealthStatus::NotServing, None, t0);
            s_rec.apply_health(HealthStatus::Serving, None, t0);
            match (s_rec.breaker().admit(t1), s_rec.breaker().admit(t1)) {
                (Admission::Probe(p1), Admission::Probe(p2)) => {
                    p1.record(Outcome::Ok, Duration::from_millis(5), t1);
                    p2.record(Outcome::Ok, Duration::from_millis(5), t1);
                }
                other => panic!("expected 2 recovery probes at t1, got {other:?}"),
            }
            for _ in 0..5 {
                s_rec.record(Outcome::Ok, Duration::from_millis(5), t1);
            }
        });

        writer.join().expect("stale writer join");
        recover.join().expect("recover worker join");

        assert_eq!(slot.breaker().state(t1), BreakerState::Closed);
        assert_eq!(slot.breaker().window_totals(t1), (5, 0, 0));
    });
}
