// Federation chaos and fault-injection scenarios (`WP-M3-08`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::Clock;
use gap_core::error::{ErrorCode, NgcErrorCode};
use gap_core::ids::BranchUid;
use gap_testkit::chaos::{
    BreakerState, ChaosFederationHarness, ChaosReplica, ChaosReplicaConfig, FaultInjector,
    FaultProfile,
};
use gap_testkit::clock::{advance_paused, test_clock};
use gap_testkit::mock_ngc::{MockNgcConnector, ScriptStep};
use serde_json::json;

fn branch_uid(n: u8) -> BranchUid {
    BranchUid::from_unix_ms(1_700_000_000_000, [n; 10])
}

fn make_healthy_replica(uid_byte: u8, now_ms: u64, base_latency_ms: u64) -> Arc<ChaosReplica> {
    let mock = MockNgcConnector::new();
    mock.on_operation("catalog.lookup", move |idx, args, _var| {
        ScriptStep::ok(json!({
            "replica": uid_byte,
            "idx": idx,
            "args": args
        }))
        .with_latency(Duration::from_millis(base_latency_ms))
    });
    ChaosReplica::new(
        branch_uid(uid_byte),
        mock,
        FaultInjector::new(u64::from(uid_byte) * 97 + 1),
        now_ms,
        ChaosReplicaConfig::default(),
    )
}

fn make_p2c_replica(uid_byte: u8, now_ms: u64, base_latency_ms: u64) -> Arc<ChaosReplica> {
    let mock = MockNgcConnector::new();
    mock.on_operation("catalog.lookup", move |idx, args, _var| {
        ScriptStep::ok(json!({
            "replica": uid_byte,
            "idx": idx,
            "args": args
        }))
        .with_latency(Duration::from_millis(base_latency_ms))
    });
    ChaosReplica::new(
        branch_uid(uid_byte),
        mock,
        FaultInjector::new(u64::from(uid_byte) * 97 + 1),
        now_ms,
        ChaosReplicaConfig {
            capacity: 8,
            default_timeout: Duration::from_millis(500),
            ewma_half_life: Duration::from_millis(500),
        },
    )
}

async fn run_concurrent_wave(
    harness: &Arc<ChaosFederationHarness>,
    clock: &Arc<gap_testkit::clock::ManualClock>,
    wave_size: usize,
    base_seq: usize,
) -> Vec<BranchUid> {
    let mut join_set = tokio::task::JoinSet::new();
    for offset in 0..wave_size {
        let h = Arc::clone(harness);
        let seq = base_seq + offset;
        join_set.spawn(async move {
            h.execute_unary(
                "catalog.lookup",
                json!({"seq": seq}),
                Duration::from_secs(2),
            )
            .await
            .expect("healthy replica serves concurrent wave call")
            .branch_uid
        });
    }

    let mut served_by = Vec::with_capacity(wave_size);
    while !join_set.is_empty() {
        tokio::select! {
            biased;
            joined = join_set.join_next() => {
                if let Some(res) = joined {
                    served_by.push(res.expect("wave task completes"));
                }
            }
            () = tokio::time::sleep(Duration::from_millis(5)) => {
                clock.advance(Duration::from_millis(5));
            }
        }
    }
    served_by
}

// spec: RTE-074
#[tokio::test(start_paused = true)]
async fn rte_074_latency_step_300ms_shifts_p2c_traffic_away_from_degraded_replica_without_herding()
{
    let clock = test_clock();
    let t0 = clock.unix_millis() as u64;
    let r0 = make_p2c_replica(1, t0, 20);
    let r1 = make_p2c_replica(2, t0, 20);
    let r2 = make_p2c_replica(3, t0, 20);

    let harness = Arc::new(ChaosFederationHarness::new(
        clock.clone(),
        vec![Arc::clone(&r0), Arc::clone(&r1), Arc::clone(&r2)],
        1,
        2026,
    ));

    // Phase 1: Warm baseline EWMA across all 3 replicas using concurrent waves (`in_flight` P2C).
    for wave in 0..10 {
        let _ = run_concurrent_wave(&harness, &clock, 6, wave * 6).await;
        advance_paused(&clock, Duration::from_millis(40)).await;
    }

    let before_r0 = r0.routed_calls();
    let before_r1 = r1.routed_calls();
    let before_r2 = r2.routed_calls();
    assert!(
        before_r0 > 5 && before_r1 > 5 && before_r2 > 5,
        "expected concurrent warmup across all replicas: r0={before_r0}, r1={before_r1}, r2={before_r2}"
    );

    // Inject +300 ms latency shift on r0 (`+300 ms` degraded replica).
    r0.injector().inject_latency_shift_ms(300);

    // Send 15 concurrent waves of 6 requests (90 total) so EWMA reflects the 320 ms latency step on r0.
    let mut degraded_phase_counts = [0_u64; 3];
    for wave in 0..15 {
        let uids = run_concurrent_wave(&harness, &clock, 6, 1_000 + wave * 6).await;
        for uid in uids {
            if uid == r0.branch_uid() {
                degraded_phase_counts[0] += 1;
            } else if uid == r1.branch_uid() {
                degraded_phase_counts[1] += 1;
            } else {
                degraded_phase_counts[2] += 1;
            }
        }
        advance_paused(&clock, Duration::from_millis(40)).await;
    }

    let now_ms = clock.unix_millis() as u64;
    let r0_ewma = r0.latency_ewma_ms(now_ms).unwrap_or(0.0);
    assert!(
        r0_ewma > 80.0,
        "expected degraded replica r0 EWMA to rise after +300 ms shift, got {r0_ewma:.2}"
    );
    assert!(
        degraded_phase_counts[0] < degraded_phase_counts[1]
            && degraded_phase_counts[0] < degraded_phase_counts[2],
        "expected P2C to shift traffic away from degraded r0: counts={degraded_phase_counts:?}"
    );
    // Healthy replicas both stay utilized without herding all traffic onto one node.
    assert!(
        degraded_phase_counts[1] >= 25 && degraded_phase_counts[2] >= 25,
        "expected balanced split across healthy replicas r1/r2: counts={degraded_phase_counts:?}"
    );
    assert_eq!(r0.in_flight() + r1.in_flight() + r2.in_flight(), 0);
}

// spec: RTE-080, RTE-081
#[tokio::test(start_paused = true)]
async fn rte_080_081_hundred_percent_error_opens_breaker_within_30s_and_recovers_via_half_open_probes()
 {
    let clock = test_clock();
    let t0 = clock.unix_millis() as u64;
    let r0 = make_healthy_replica(10, t0, 15);
    // Inject 100% retryable Upstream Unavailable errors on the sole replica.
    r0.injector()
        .inject_error_rate_bps(10_000, NgcErrorCode::Unavailable, true);

    let harness = ChaosFederationHarness::new(clock.clone(), vec![Arc::clone(&r0)], 0, 7);

    // Run 20 failing calls well inside <= 30 s rolling window (`RTE-080`).
    for i in 0..20 {
        let fut = harness.execute_unary("catalog.lookup", json!({"i": i}), Duration::from_secs(1));
        tokio::pin!(fut);
        let err = loop {
            tokio::select! {
                biased;
                res = &mut fut => break res.expect_err("100% error injects failure"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {
                    clock.advance(Duration::from_millis(5));
                }
            }
        };
        assert_eq!(err.code(), ErrorCode::BranchError);
        advance_paused(&clock, Duration::from_millis(200)).await;
    }

    let opened_at = clock.unix_millis() as u64;
    assert!(
        opened_at - t0 <= 30_000,
        "expected breaker to open within <= 30 s, elapsed={} ms",
        opened_at - t0
    );
    assert!(matches!(
        r0.breaker_state(opened_at),
        BreakerState::Open { .. }
    ));

    // Call #21 must fail fast with `GAP_BRANCH_UNAVAILABLE` + `retry.afterMs` without dialing mock (`RTE-081`).
    let calls_before = r0.connector().recorded_calls().len();
    let fast_err = harness
        .execute_unary(
            "catalog.lookup",
            json!({"fast": true}),
            Duration::from_secs(1),
        )
        .await
        .expect_err("open breaker fails fast");
    assert_eq!(fast_err.code(), ErrorCode::BranchUnavailable);
    assert!(fast_err.retry().after_ms.is_some());
    assert_eq!(r0.connector().recorded_calls().len(), calls_before);

    // Heal replica and advance past initial 5 s Open timer -> HalfOpen recovery via 2 successful probes (`RTE-080`).
    r0.injector().set_profile(FaultProfile::default());
    advance_paused(&clock, Duration::from_millis(5_050)).await;
    let t_half = clock.unix_millis() as u64;
    assert!(matches!(
        r0.breaker_state(t_half),
        BreakerState::HalfOpen { .. }
    ));

    for probe_i in 0..2 {
        let fut = harness.execute_unary(
            "catalog.lookup",
            json!({"probe": probe_i}),
            Duration::from_secs(1),
        );
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                res = &mut fut => {
                    let _ = res.expect("probe succeeds");
                    break;
                }
                () = tokio::time::sleep(Duration::from_millis(5)) => {
                    clock.advance(Duration::from_millis(5));
                }
            }
        }
    }

    assert_eq!(
        r0.breaker_state(clock.unix_millis() as u64),
        BreakerState::Closed
    );
    assert_eq!(r0.in_flight(), 0);
}

// spec: RUN-024, RUN-030
#[tokio::test(start_paused = true)]
async fn run_024_030_mid_stream_crash_and_deadline_cancellation_isolate_partial_results_and_release_in_flight()
 {
    let clock = test_clock();
    let t0 = clock.unix_millis() as u64;

    // 1. Mid-stream `foreach` crash isolation (`RUN-024`): 4 items, each has 1 final chunk, crash after 2 chunks.
    let stream_mock = MockNgcConnector::new();
    stream_mock.on_operation("orders.batch", |idx, _base, _var| {
        ScriptStep::ok(json!({"item": idx, "status": "ok"})).with_latency(Duration::from_millis(10))
    });
    let r_stream = ChaosReplica::new(
        branch_uid(20),
        stream_mock,
        FaultInjector::new(11),
        t0,
        ChaosReplicaConfig::default(),
    );
    r_stream.injector().inject_mid_stream_crash(Some(2));

    let harness_stream =
        ChaosFederationHarness::new(clock.clone(), vec![Arc::clone(&r_stream)], 0, 11);
    let foreach_fut = harness_stream.execute_foreach(
        "orders.batch",
        json!({}),
        vec![json!(0), json!(1), json!(2), json!(3)],
        Duration::from_secs(2),
    );
    tokio::pin!(foreach_fut);
    let report = loop {
        tokio::select! {
            biased;
            res = &mut foreach_fut => break res.expect("partial foreach report returned"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {
                clock.advance(Duration::from_millis(5));
            }
        }
    };

    assert!(report.partial);
    assert_eq!(report.delivered_chunks.len(), 2);
    assert_eq!(report.items[0], Some(json!({"item": 0, "status": "ok"})));
    assert_eq!(report.items[1], Some(json!({"item": 1, "status": "ok"})));
    assert_eq!(report.items[2], None);
    assert_eq!(report.items[3], None);
    assert_eq!(report.affected_indexes, vec![2, 3]);
    assert!(!report.errors.is_empty());
    assert_eq!(report.errors[0].code(), ErrorCode::BranchError);
    assert_eq!(r_stream.in_flight(), 0);

    // 2. Per-hop deadline formula (`RUN-030`) & mid-call cancellation releasing RAII `in_flight` (`RTE-071`).
    assert_eq!(
        ChaosFederationHarness::per_hop_deadline(
            Duration::from_millis(2_000),
            Duration::from_millis(500)
        ),
        Some(Duration::from_millis(500))
    );
    assert_eq!(
        ChaosFederationHarness::per_hop_deadline(
            Duration::from_millis(200),
            Duration::from_millis(500)
        ),
        Some(Duration::from_millis(150)) // 200 - max(50, 10)
    );
    assert_eq!(
        ChaosFederationHarness::per_hop_deadline(
            Duration::from_millis(40),
            Duration::from_millis(500)
        ),
        None
    );

    let slow_mock = MockNgcConnector::new();
    slow_mock.on_operation("slow.op", |_, _, _| {
        ScriptStep::ok(json!({"ok": true})).with_latency(Duration::from_millis(400))
    });
    let r_slow = ChaosReplica::new(
        branch_uid(21),
        slow_mock,
        FaultInjector::new(22),
        clock.unix_millis() as u64,
        ChaosReplicaConfig::default(),
    );
    let harness_slow = ChaosFederationHarness::new(clock.clone(), vec![Arc::clone(&r_slow)], 0, 22);

    // Drop in-flight future mid-execution after 50 ms -> RAII permit decrements `in_flight` to 0 immediately.
    {
        let inflight_fut = harness_slow.execute_unary("slow.op", json!({}), Duration::from_secs(2));
        tokio::pin!(inflight_fut);
        let cancel_timer = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(cancel_timer);
        tokio::select! {
            biased;
            _ = &mut cancel_timer => {}
            _ = &mut inflight_fut => panic!("slow call should still be waiting at 50 ms"),
        }
        assert_eq!(r_slow.in_flight(), 1);
        clock.advance(Duration::from_millis(50));
    }
    assert_eq!(
        r_slow.in_flight(),
        0,
        "dropped future must immediately decrement in_flight to 0"
    );

    // Verify per-hop deadline cancellation returns `GAP_DEADLINE_EXCEEDED` / `GAP_BRANCH_TIMEOUT` cleanly.
    let tight_fut = harness_slow.execute_unary("slow.op", json!({}), Duration::from_millis(200));
    tokio::pin!(tight_fut);
    let deadline_err = loop {
        tokio::select! {
            biased;
            res = &mut tight_fut => break res.expect_err("per-hop 150 ms deadline aborts 400 ms call"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {
                clock.advance(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(deadline_err.code(), ErrorCode::BranchTimeout);
    assert_eq!(r_slow.in_flight(), 0);
}

// spec: RTE-081, OPS-030
#[tokio::test(start_paused = true)]
async fn ops_030_network_partition_and_retry_budget_cap_retries_under_ten_percent() {
    let clock = test_clock();
    let t0 = clock.unix_millis() as u64;

    // 2 replicas: r0 is partitioned (packet blackhole until deadline), r1 intermittently fails at 60% error rate.
    let r0 = make_healthy_replica(30, t0, 10);
    let r1 = make_healthy_replica(31, t0, 10);
    r0.injector().set_partitioned(true);
    r1.injector()
        .inject_error_rate_bps(6_000, NgcErrorCode::Unavailable, true);

    let cfg = ChaosReplicaConfig {
        capacity: 32,
        default_timeout: Duration::from_millis(80),
        ewma_half_life: Duration::from_millis(2_000),
    };
    let r0 = ChaosReplica::new(
        branch_uid(30),
        r0.connector().clone(),
        r0.injector().clone(),
        t0,
        cfg,
    );
    let r1 = ChaosReplica::new(
        branch_uid(31),
        r1.connector().clone(),
        r1.injector().clone(),
        t0,
        cfg,
    );

    let harness =
        ChaosFederationHarness::new(clock.clone(), vec![Arc::clone(&r0), Arc::clone(&r1)], 2, 99);

    let total_requests = 200_u64;
    for seq in 0..total_requests {
        let fut = harness.execute_unary(
            "catalog.lookup",
            json!({"seq": seq}),
            Duration::from_millis(300),
        );
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                _res = &mut fut => break,
                () = tokio::time::sleep(Duration::from_millis(5)) => {
                    clock.advance(Duration::from_millis(5));
                }
            }
        }
        advance_paused(&clock, Duration::from_millis(150)).await;
    }

    // Retry budget (`initial 10 + 0.1 * 200 = 30`) strictly bounds retry storms (`OPS-030`, `RTE-091`).
    let budget = harness.retry_budget();
    assert_eq!(budget.first_attempts(), total_requests);
    assert!(
        budget.retries_used() <= 30,
        "expected retry count <= initial(10) + 10% of 200 (30), got {}",
        budget.retries_used()
    );
    assert!(
        budget.retries_rejected() > 0,
        "expected exhausted retry budget to reject excess retries during partition storm"
    );
    // Partitioned blackhole replica r0 must have tripped Open cleanly.
    let now_ms = clock.unix_millis() as u64;
    assert!(
        r0.breaker_transitions() >= 1
            || matches!(r0.breaker_state(now_ms), BreakerState::Open { .. })
    );
    assert_eq!(r0.in_flight() + r1.in_flight(), 0);
}
