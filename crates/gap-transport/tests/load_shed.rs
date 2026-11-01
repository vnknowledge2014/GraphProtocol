//! Deterministic overload simulation (`RUN-040`): a fixed-capacity service behind the real
//! [`AdaptiveLimiter`] and a manual clock. Under 3x overload with 70% of the offered load being `batch`, the
//! interactive p99 must stay within 2x of the unloaded latency and batch traffic must be shed first.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_transport::shed::{AdaptiveLimiter, LimitPermit, LimiterConfig, Priority};

const WORKERS: usize = 50;
const SERVICE_MS: u64 = 10;

struct Waiting {
    arrived_ms: u64,
    priority: Priority,
    permit: LimitPermit,
}

struct Running {
    done_ms: u64,
    arrived_ms: u64,
    priority: Priority,
    permit: LimitPermit,
}

struct Report {
    interactive_latencies: Vec<u64>,
    offered: [u64; 3],
    shed: [u64; 3],
    final_limit: u32,
}

/// Runs `seconds` of simulated time in 1 ms steps. `interactive_per_ms` and `batch_per_ms` are arrival counts
/// per 100 ms (integer arithmetic keeps the run exactly reproducible).
fn simulate(
    seconds: u64,
    interactive_per_100ms: u64,
    batch_per_100ms: u64,
    cfg: LimiterConfig,
) -> Report {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let limiter = AdaptiveLimiter::new(cfg, Arc::clone(&clock) as Arc<dyn Clock>).unwrap();
    let mut waiting: VecDeque<Waiting> = VecDeque::new();
    let mut running: Vec<Running> = Vec::new();
    let mut report = Report {
        interactive_latencies: Vec::new(),
        offered: [0; 3],
        shed: [0; 3],
        final_limit: 0,
    };
    let warm_up_ms = 10_000;

    for now_ms in 0..seconds * 1_000 {
        // 1. completions at this instant
        let mut i = 0;
        while i < running.len() {
            if running[i].done_ms <= now_ms {
                let done = running.swap_remove(i);
                if done.priority == Priority::Interactive && now_ms >= warm_up_ms {
                    report.interactive_latencies.push(now_ms - done.arrived_ms);
                }
                drop(done.permit);
            } else {
                i += 1;
            }
        }
        // 2. arrivals (spread evenly over each 100 ms window)
        for (priority, per_100ms) in [
            (Priority::Interactive, interactive_per_100ms),
            (Priority::Batch, batch_per_100ms),
        ] {
            let slot = now_ms % 100;
            let arrivals = (per_100ms * (slot + 1)) / 100 - (per_100ms * slot) / 100;
            for _ in 0..arrivals {
                if now_ms >= warm_up_ms {
                    report.offered[priority as usize] += 1;
                }
                match limiter.try_acquire(priority) {
                    Ok(permit) => waiting.push_back(Waiting {
                        arrived_ms: now_ms,
                        priority,
                        permit,
                    }),
                    Err(_) => {
                        if now_ms >= warm_up_ms {
                            report.shed[priority as usize] += 1;
                        }
                    }
                }
            }
        }
        // 3. FIFO workers
        while running.len() < WORKERS {
            let Some(w) = waiting.pop_front() else { break };
            running.push(Running {
                done_ms: now_ms + SERVICE_MS,
                arrived_ms: w.arrived_ms,
                priority: w.priority,
                permit: w.permit,
            });
        }
        clock.advance(Duration::from_millis(1));
    }
    report.final_limit = limiter.limit();
    report
}

fn p99(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[(v.len() * 99 / 100).min(v.len() - 1)]
}

// spec: RUN-040
#[test]
fn interactive_p99_stays_within_twice_the_unloaded_latency_under_triple_overload() {
    // Capacity: 50 workers / 10 ms = 5,000 req/s = 500 per 100 ms.
    let unloaded = simulate(30, 100, 0, LimiterConfig::default());
    let unloaded_p99 = p99(unloaded.interactive_latencies);
    assert_eq!(
        unloaded_p99, SERVICE_MS,
        "an idle service answers in one service time"
    );

    // 3x overload: 1,500 per 100 ms offered, 30% interactive (450) and 70% batch (1,050).
    let loaded = simulate(60, 150, 1_350, LimiterConfig::default());
    let loaded_p99 = p99(loaded.interactive_latencies.clone());
    assert!(
        loaded_p99 <= 2 * unloaded_p99,
        "interactive p99 {loaded_p99} ms exceeds 2x the unloaded {unloaded_p99} ms"
    );

    let interactive_shed = loaded.shed[0] as f64 / loaded.offered[0] as f64;
    let batch_shed = loaded.shed[1] as f64 / loaded.offered[1] as f64;
    assert!(
        batch_shed > 0.5,
        "most batch traffic must be shed under 3x overload: {batch_shed:.2}"
    );
    assert!(
        interactive_shed < 0.05,
        "interactive traffic within capacity is (almost) never shed: {interactive_shed:.3}"
    );
    assert!(batch_shed > interactive_shed * 10.0, "batch is shed first");
}

// spec: RUN-040
#[test]
fn the_guarantee_holds_across_interactive_shares_and_the_gateway_recovers_after_the_overload() {
    // Interactive demand of 5%, 10% and 20% of capacity, batch filling the rest of a 3x overload.
    for interactive_per_100ms in [25_u64, 100, 300] {
        let batch = 1_500 - interactive_per_100ms;
        let report = simulate(60, interactive_per_100ms, batch, LimiterConfig::default());
        let p = p99(report.interactive_latencies.clone());
        assert!(
            p <= 2 * SERVICE_MS,
            "interactive {interactive_per_100ms}/100ms: p99 {p} ms exceeds 2x the unloaded latency"
        );
        assert_eq!(
            report.shed[0], 0,
            "interactive {interactive_per_100ms}/100ms: interactive traffic was shed"
        );
    }

    // After the overload ends the limit is high enough for the normal load: nothing is shed at 80% utilisation.
    let calm = simulate(60, 80, 320, LimiterConfig::default());
    assert_eq!(
        calm.shed,
        [0, 0, 0],
        "a gateway at 80% of capacity sheds nothing"
    );
    let p = p99(calm.interactive_latencies);
    assert!(p <= 2 * SERVICE_MS, "p99 {p} ms at 80% utilisation");
}
