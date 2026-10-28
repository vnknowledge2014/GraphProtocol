//! Dedicated zero-allocation verification for `gap-balance` hot paths (`RTE-070`, `RTE-079`, `ARC-006`).
//!
//! Why `#![allow(unsafe_code)]` is permitted in this single integration test file (`m3-01-design.md` §6.4):
//! Rust's `std::alloc::GlobalAlloc` trait is an `unsafe trait` whose `alloc`, `dealloc`,
//! `alloc_zeroed`, and `realloc` methods are `unsafe fn`s delegating directly to `std::alloc::System`.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use gap_balance::{
    Admission, Candidate, DecayedEwma, InFlightGuard, Now, Outcome, RetryBudget, SlotParams,
    TelemetryRegistry, XorShift64, select, select_excluding,
};
use gap_core::clock::ManualClock;
use gap_core::ids::{BranchUid, TenantId};
use gap_core::snapshot::TelemetrySlotKey;

struct ThreadLocalCountingAlloc;

thread_local! {
    static COUNTING_ENABLED: Cell<bool> = const { Cell::new(false) };
    static ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[inline]
fn record_alloc() {
    let _ = COUNTING_ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = ALLOC_COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

// SAFETY: Every allocator method delegates unmodified arguments directly to `std::alloc::System`.
unsafe impl GlobalAlloc for ThreadLocalCountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_alloc();
        // SAFETY: Delegated verbatim to `System::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Delegated verbatim to `System::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_alloc();
        // SAFETY: Delegated verbatim to `System::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_alloc();
        // SAFETY: Delegated verbatim to `System::realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: ThreadLocalCountingAlloc = ThreadLocalCountingAlloc;

fn count_allocs<R>(f: impl FnOnce() -> R) -> (usize, R) {
    // Warm thread-local slots before enabling counting.
    COUNTING_ENABLED.with(|e| e.set(false));
    ALLOC_COUNT.with(|c| c.set(0));
    COUNTING_ENABLED.with(|e| e.set(true));
    let out = black_box(f());
    COUNTING_ENABLED.with(|e| e.set(false));
    let n = ALLOC_COUNT.with(Cell::get);
    (n, out)
}

fn assert_zero_allocs<R>(name: &str, f: impl FnOnce() -> R) -> R {
    let (n, out) = count_allocs(f);
    assert_eq!(
        n, 0,
        "hot-path `{name}` performed {n} heap allocations (expected 0)"
    );
    out
}

fn make_uid(seq: u64) -> BranchUid {
    let mut b = [0_u8; 10];
    b[2..10].copy_from_slice(&seq.to_be_bytes());
    BranchUid::from_unix_ms(1_700_000_000_000 + seq, b)
}

// spec: RTE-070, RTE-079, ARC-006
#[test]
fn hot_path_operations_perform_zero_heap_allocations() {
    // Sanity check: prove the thread-local counting allocator actually catches allocations.
    let (detected, vec_val) = count_allocs(|| {
        let mut v = Vec::<u64>::with_capacity(16);
        v.push(42);
        v
    });
    assert!(
        detected >= 1,
        "counting allocator must detect Vec heap allocation"
    );
    drop(vec_val);

    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let registry = TelemetryRegistry::new(clock, 256);
    let tenant = TenantId::new("acme").expect("tenant");

    // Pre-register 80 slots (> 64 stack scratch cap so both fast-path and >64 fallback are tested!).
    let mut slot_ids = Vec::with_capacity(80);
    let mut slot_arcs = Vec::with_capacity(80);
    let mut uids = Vec::with_capacity(80);
    for i in 0..80_u64 {
        let uid = make_uid(i + 1);
        let key = TelemetrySlotKey::new(tenant.clone(), uid);
        let id = registry
            .register(
                key,
                SlotParams {
                    max_in_flight: 64,
                    default_timeout_ms: 1_000,
                    label: Arc::from("hot-branch"),
                },
            )
            .expect("register");
        let arc = registry.get(id).expect("slot");
        if i % 2 == 0 {
            arc.record(
                Outcome::Ok,
                Duration::from_micros(120 + i * 5),
                Now::from_ms(0),
            );
        }
        slot_ids.push(id);
        slot_arcs.push(arc);
        uids.push(uid);
    }

    let cands_8: Vec<Candidate<'_>> = (0..8)
        .map(|i| Candidate {
            uid: uids[i],
            slot: &slot_arcs[i],
        })
        .collect();
    let cands_80: Vec<Candidate<'_>> = (0..80)
        .map(|i| Candidate {
            uid: uids[i],
            slot: &slot_arcs[i],
        })
        .collect();

    // 1. DecayedEwma::update and read
    let ewma = DecayedEwma::new();
    assert_zero_allocs("DecayedEwma::update+read", || {
        for i in 0..1_000_u32 {
            black_box(ewma.update(50.0 + (i % 10) as f32, i, 10_000.0));
            black_box(ewma.read(i, 10_000.0));
        }
    });

    // 2. TelemetrySlot::record (Ok, Failure, ClientError)
    let slot0 = &slot_arcs[0];
    assert_zero_allocs("TelemetrySlot::record", || {
        for i in 1..=500_u64 {
            let now = Now::from_ms(i * 4);
            slot0.record(Outcome::Ok, Duration::from_micros(150), now);
            slot0.record(Outcome::ClientError, Duration::from_micros(90), now);
        }
    });

    // 3. InFlightGuard::acquire + drop
    assert_zero_allocs("InFlightGuard::acquire+drop", || {
        for _ in 0..1_000 {
            let guard = InFlightGuard::acquire(slot0);
            black_box(&guard);
            drop(guard);
        }
    });

    // 4. Breaker::admit (Closed, Open, and HalfOpen Probe + drop)
    let breaker_slot = &slot_arcs[1];
    assert_zero_allocs("Breaker::admit(Closed)", || {
        for i in 0..1_000_u64 {
            black_box(breaker_slot.breaker().admit(Now::from_ms(i)));
        }
    });
    // Trip breaker_slot (outside counting, since transition emits tracing/gauge), then test Open & HalfOpen admit!
    for _ in 0..20 {
        breaker_slot.record(
            Outcome::Failure,
            Duration::from_millis(10),
            Now::from_ms(1_000),
        );
    }
    assert_zero_allocs("Breaker::admit(Open)", || {
        for _ in 0..1_000 {
            black_box(breaker_slot.breaker().admit(Now::from_ms(2_000)));
        }
    });
    // Materialize Open -> HalfOpen once at t = 6,000, then verify Probe issue + drop allocates 0 bytes!
    let _ = breaker_slot.breaker().state(Now::from_ms(6_000));
    assert_zero_allocs("Breaker::admit(HalfOpen)+drop", || {
        for _ in 0..1_000 {
            match breaker_slot.breaker().admit(Now::from_ms(6_000)) {
                Admission::Probe(p) => drop(p),
                other => panic!("expected Probe, got {other:?}"),
            }
        }
    });

    // 5. RetryBudget ops
    let budget = RetryBudget::new();
    assert_zero_allocs("RetryBudget ops", || {
        for _ in 0..1_000 {
            budget.deposit_first_attempt();
            black_box(budget.balance_millis());
        }
        for _ in 0..20 {
            black_box(budget.try_withdraw());
        }
    });

    // 6. P2C select and select_excluding (both <= 64 and > 64 candidates, with mixed known/unknown latencies)
    let mut rng = XorShift64::new(123_456);
    assert_zero_allocs("p2c::select(n=8)", || {
        for _ in 0..1_000 {
            black_box(select(&cands_8, Now::from_ms(100), &mut rng));
            black_box(select_excluding(
                &cands_8,
                &[0, 2],
                Now::from_ms(100),
                &mut rng,
            ));
        }
    });
    assert_zero_allocs("p2c::select(n=80)", || {
        for _ in 0..200 {
            black_box(select(&cands_80, Now::from_ms(100), &mut rng));
        }
    });

    // 7. TelemetryRegistry::get and now
    assert_zero_allocs("TelemetryRegistry::get+now", || {
        for &id in &slot_ids {
            black_box(registry.now());
            let arc = registry.get(id);
            black_box(&arc);
            drop(arc);
        }
    });

    // 8. XorShift64::with_thread_local (including initial lazy per-thread seeding) + select
    assert_zero_allocs("XorShift64::with_thread_local+select", || {
        for _ in 0..200 {
            black_box(XorShift64::with_thread_local(|tls| {
                select(&cands_8, Now::from_ms(100), tls)
            }));
        }
    });
}
