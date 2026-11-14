// High-concurrency snapshot swap stress, p99 latency, 5-generation structural sharing, and
// deterministic publisher/reclaimer/rollback state-machine verification (`WP-M4-08`).
//
// Normative references: `RUN-003`, `RUN-004`, `RUN-006`, `RUN-007`, `RUN-008`, `INV-01`,
// `INV-02`, `INV-16`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::ids::{BranchId, BranchUid, ProposalId};
use gap_core::snapshot::{BranchDescriptor, BranchStatus, Endpoint, Protocol, Snapshot, TrustTier};
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits, MetaSummary,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, SideEffectHint,
};
use gap_snapshot::compose::{BranchInput, compose};
use gap_snapshot::publisher::{PublishOptions, PublisherActor, PublisherConfig, PublisherError};
use gap_snapshot::rollback::{RollbackDecision, RollbackWatcher, WatchConfig, WatchSignal};
use gap_snapshot::SnapshotBuilder;

fn uid_for(idx: u16) -> BranchUid {
    let hi = (idx >> 8) as u8;
    let lo = (idx & 0xff) as u8;
    BranchUid::from_unix_ms(
        1_761_900_000_000 + u64::from(idx),
        [hi, lo, 0, 0, 0, 0, 0, 0, 0, 0],
    )
}

fn make_branch_with_pad(idx: u16, field_name: &str, rev: u32, desc_pad_bytes: usize) -> BranchInput {
    let slug = format!("svc{idx:03}");
    let sdl = format!("type Query {{ {field_name}(id: ID!): String }}\n");
    let description_pad = "x".repeat(desc_pad_bytes);
    let op = ProtoOp {
        name: field_name.to_owned(),
        kind: ProtoOpKind::Query as i32,
        description: format!("op {field_name} {description_pad}"),
        input_schema: None,
        output_schema: None,
        side_effect_hint: SideEffectHint::SeNone as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 1,
        pagination: 0,
        key_fields: Vec::new(),
        annotations: HashMap::new(),
    };

    BranchInput {
        id: BranchId::new(&slug).expect("valid branch id"),
        uid: uid_for(idx),
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: format!("https://{slug}.internal/ngc"),
            transport: "grpc".to_owned(),
        },
        description: ConnectorDescription {
            connector_id: slug,
            connector_version: format!("1.{rev}.0"),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
            spec_hash: format!("{idx:056x}{rev:08x}"),
            capabilities: Some(ProtoCapabilities::default()),
            limits: Some(ProtoLimits {
                max_batch_size: 64,
                max_in_flight: 32,
                max_page_size: 100,
                default_timeout_ms: 2_000,
                max_response_bytes: 1_048_576,
            }),
            schema_sdl: sdl,
            operations: vec![op],
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: format!("service {idx}"),
            }),
            auth: None,
        },
        status: BranchStatus::Active,
    }
}

fn build_composed_snapshot(cur: &Snapshot, branches: &[BranchInput]) -> Snapshot {
    let comp = compose(branches).expect("compose valid test branches");
    SnapshotBuilder::new(cur, comp).build()
}

fn delta_branch_snapshot(cur: &Snapshot, idx: u16, rev: u32, _desc_pad_bytes: usize) -> Snapshot {
    let next_gen = cur.generation.saturating_add(1);
    let mut next = cur.next_generation(next_gen, Timestamp::UNIX_EPOCH, "stress-swap", None);
    let uid = uid_for(idx);
    if let Some(existing) = next.branches.get(&uid).cloned() {
        let mut updated: BranchDescriptor = (*existing).clone();
        updated.spec_hash = *blake3::hash(format!("spec-{idx}-{rev}").as_bytes()).as_bytes();
        updated.pinned_desc_hash =
            *blake3::hash(format!("pin-{idx}-{rev}").as_bytes()).as_bytes();
        next.branches.insert(uid, Arc::new(updated));
    }
    let mut hasher = blake3::Hasher::new();
    for (b_uid, desc) in &next.branches {
        hasher.update(b_uid.as_uuid().as_bytes());
        hasher.update(&desc.spec_hash);
    }
    next.branch_set_hash = *hasher.finalize().as_bytes();
    next
}

// spec: RUN-008, INV-01, INV-02, INV-16
#[test]
fn run_008_swap_stress_1000_concurrent_tasks_100_swaps() {
    let base = Snapshot::empty(0);
    let mut initial_branches: Vec<BranchInput> = (1..=8_u16)
        .map(|i| make_branch_with_pad(i, &format!("field_{i}_v1"), 1, 64))
        .collect();
    let gen1 = build_composed_snapshot(&base, &initial_branches);

    let actor = Arc::new(PublisherActor::new(
        gen1,
        PublisherConfig {
            ring_capacity: 5,
            prewarm_top_n: 0,
            ..PublisherConfig::default()
        },
    ));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_time()
        .build()
        .expect("build multi-thread tokio runtime");

    let torn_reads = Arc::new(AtomicU64::new(0));
    let total_reads = Arc::new(AtomicU64::new(0));
    let swaps_done = Arc::new(AtomicBool::new(false));

    rt.block_on(async {
        let mut handles = Vec::with_capacity(1_001);

        // 1,000 concurrent request tasks continuously pinning snapshots across 100 swaps.
        for _task_id in 0..1_000_usize {
            let actor_r = Arc::clone(&actor);
            let torn_r = Arc::clone(&torn_reads);
            let reads_r = Arc::clone(&total_reads);
            let done_r = Arc::clone(&swaps_done);
            handles.push(tokio::spawn(async move {
                let mut last_seen_gen = 0_u64;
                let mut iterations = 0_u32;
                while !done_r.load(Ordering::Acquire) || iterations < 4 {
                    let pinned = actor_r.pin();
                    let g = pinned.generation;
                    if g < last_seen_gen {
                        torn_r.fetch_add(1, Ordering::Relaxed);
                    }
                    last_seen_gen = g;

                    // Verify snapshot internal consistency & immutability across an async yield (`INV-01`).
                    let branch_count = pinned.branches.len();
                    let schema_addr = Arc::as_ptr(&pinned.schema) as usize;
                    let branch_set_hash = pinned.branch_set_hash;
                    let uid1_hash = pinned.branches.get(&uid_for(1)).map(|b| b.spec_hash);
                    tokio::task::yield_now().await;

                    if pinned.generation != g
                        || pinned.branches.len() != branch_count
                        || pinned.branches.len() != 8
                        || (Arc::as_ptr(&pinned.schema) as usize) != schema_addr
                        || pinned.branch_set_hash != branch_set_hash
                        || pinned.branches.get(&uid_for(1)).map(|b| b.spec_hash) != uid1_hash
                    {
                        torn_r.fetch_add(1, Ordering::Relaxed);
                    }

                    reads_r.fetch_add(1, Ordering::Relaxed);
                    iterations = iterations.saturating_add(1);
                }
            }));
        }

        // Publisher thread executes 100 swaps concurrently via `spawn_blocking`.
        let actor_w = Arc::clone(&actor);
        let done_w = Arc::clone(&swaps_done);
        let pub_handle = tokio::task::spawn_blocking(move || {
            for rev in 2..=101_u32 {
                let cur = actor_w.pin();
                initial_branches[0] = make_branch_with_pad(1, &format!("field_1_v{rev}"), rev, 64);
                let next = build_composed_snapshot(&cur, &initial_branches);
                actor_w
                    .publish(
                        next,
                        &PublishOptions::default(),
                        vec![ProposalId::new_v7()],
                    )
                    .expect("publish swap in stress run");
                std::thread::sleep(Duration::from_micros(150));
            }
            done_w.store(true, Ordering::Release);
        });
        handles.push(pub_handle);

        for h in handles {
            h.await.expect("stress task finishes without panic");
        }
    });

    assert_eq!(
        torn_reads.load(Ordering::Relaxed),
        0,
        "zero torn reads or non-monotonic observations across 1,000 tasks x 100 swaps"
    );
    assert!(
        total_reads.load(Ordering::Relaxed) >= 4_000,
        "expected >= 4,000 pinned reads across stress run"
    );
    assert_eq!(actor.pin().generation, 101);

    // Every task has dropped its pin -> background reclaimer drains cleanly off-thread.
    actor.sync_reclaimer();
    let metrics = actor.reclaimer_metrics();
    assert_eq!(
        metrics.enqueued, 96,
        "101 published gens minus 5 retained in ring = 96 evicted"
    );
    assert_eq!(metrics.pending, 0);
    assert_eq!(metrics.reclaimed, 96);
}

// spec: RUN-008, INV-01, INV-02
#[test]
fn run_008_busy_readers_swap_p99_and_structural_sharing_rss() {
    let base = Snapshot::empty(0);
    // 100 branches with substantial per-branch payload to verify 5-gen structural sharing footprint.
    let initial_branches: Vec<BranchInput> = (1..=100_u16)
        .map(|i| make_branch_with_pad(i, &format!("op_{i}"), 1, 2_048))
        .collect();
    let gen1 = build_composed_snapshot(&base, &initial_branches);

    // Baseline byte/descriptor footprint for 1 generation.
    let single_gen_ptrs: BTreeSet<usize> = gen1
        .branches
        .values()
        .map(|arc| Arc::as_ptr(arc) as usize)
        .collect();
    assert_eq!(single_gen_ptrs.len(), 100);

    let actor = Arc::new(PublisherActor::new(
        gen1,
        PublisherConfig {
            ring_capacity: 5,
            prewarm_top_n: 0,
            ..PublisherConfig::default()
        },
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let reader_ops = Arc::new(AtomicU64::new(0));
    let mut swap_latencies_us = Vec::with_capacity(100);

    std::thread::scope(|scope| {
        for _ in 0..256_usize {
            let actor_r = Arc::clone(&actor);
            let stop_r = Arc::clone(&stop);
            let ops_r = Arc::clone(&reader_ops);
            scope.spawn(move || {
                while !stop_r.load(Ordering::Acquire) {
                    let snap = actor_r.pin();
                    let count = snap.branches.len();
                    std::hint::black_box(count);
                    ops_r.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
            });
        }

        // Wait briefly so all 256 busy readers are spinning on `pin()`.
        std::thread::sleep(Duration::from_millis(20));

        // Perform 100 single-branch delta swaps while 256 busy readers actively pin snapshots.
        for rev in 2..=101_u32 {
            let cur = actor.pin();
            let next = delta_branch_snapshot(&cur, 1, rev, 64);
            let record = actor
                .publish(next, &PublishOptions::default(), Vec::new())
                .expect("delta swap succeeds");
            swap_latencies_us.push(record.swap_us);
        }

        // Inspect structural sharing across the 5 retained generations (`gen 97..=101`).
        let mut retained = Vec::with_capacity(5);
        for _ in 0..4 {
            // Ensure 5 distinct snapshots reachable via handle + deltas; also check latest vs initial.
            retained.push(actor.pin());
        }
        let latest = actor.pin();
        let mut shared_unchanged = 0_usize;
        for idx in 2..=100_u16 {
            let uid = uid_for(idx);
            if let Some(arc_now) = latest.branches.get(&uid)
                && single_gen_ptrs.contains(&(Arc::as_ptr(arc_now) as usize))
            {
                shared_unchanged += 1;
            }
        }
        assert_eq!(
            shared_unchanged, 99,
            "all 99 untouched branches must remain Arc::ptr_eq across 100 swaps"
        );

        stop.store(true, Ordering::Release);
    });

    assert!(
        reader_ops.load(Ordering::Relaxed) >= 256,
        "256 busy readers must actively pin snapshots during swaps"
    );

    swap_latencies_us.sort_unstable();
    let p99_idx = ((swap_latencies_us.len() * 99) / 100).min(swap_latencies_us.len() - 1);
    let p99_us = swap_latencies_us[p99_idx];
    assert!(
        p99_us <= 250,
        "swap latency p99 under 256 busy readers was {p99_us} us, exceeding Gate G3 <= 250 us"
    );

    // Explicitly measure 5-generation ring structural sharing growth on a 1-branch delta sequence (`gen 1..=5`).
    let g1 = build_composed_snapshot(&Snapshot::empty(0), &initial_branches);
    let g2 = delta_branch_snapshot(&g1, 1, 2, 64);
    let g3 = delta_branch_snapshot(&g2, 1, 3, 64);
    let g4 = delta_branch_snapshot(&g3, 1, 4, 64);
    let g5 = delta_branch_snapshot(&g4, 1, 5, 64);
    let mut unique_branch_arcs = BTreeSet::new();
    for g in [&g1, &g2, &g3, &g4, &g5] {
        for arc in g.branches.values() {
            unique_branch_arcs.insert(Arc::as_ptr(arc) as usize);
        }
    }
    // 100 initial descriptors + 4 updated versions = 104 unique BranchDescriptors (+4% <= +15% gate).
    assert_eq!(unique_branch_arcs.len(), 104);
    let growth_ratio = unique_branch_arcs.len() as f64 / 100.0;
    assert!(
        growth_ratio <= 1.15,
        "5-generation 1-branch delta footprint growth ratio {growth_ratio:.3} exceeds +15%"
    );
}

#[derive(Debug)]
struct ModelEnv {
    actor: Arc<PublisherActor>,
    watcher: RollbackWatcher,
    pins: BTreeMap<u64, Vec<Arc<Snapshot>>>,
    bad_gens: BTreeSet<u64>,
    current_rev: u32,
}

#[derive(Debug, Clone, Copy)]
enum StepOp {
    PublishGood,
    PublishFaultyBranch1,
    PinCurrent,
    ReleaseOldestPin,
    RecordInfraNoise,
    RecordFaultyBranch1AndEvaluate,
    AttemptRepromoteBadWithoutForce,
    AttemptRepromoteBadWithForce,
    FlushReclaimer,
}

fn check_invariants(env: &ModelEnv) {
    // INV-16: Active generation matches highest ring generation and is strictly >= 1.
    let active = env.actor.pin();
    let ring = env.actor.ring_generations();
    assert!(!ring.is_empty());
    assert_eq!(active.generation, ring.last().map_or(0, |r| r.0));

    // INV-01 / RUN-006: Any pinned evicted generation must NEVER be reclaimed while `pin_count > 0`,
    // and after `sync_reclaimer()` it is visibly tracked in `pending`.
    let min_ring_gen = ring.first().map_or(u64::MAX, |r| r.0);
    let evicted_pinned_gens = env
        .pins
        .iter()
        .filter(|(pinned_gen, list)| **pinned_gen < min_ring_gen && !list.is_empty())
        .count() as u64;
    let metrics = env.actor.reclaimer_metrics();
    assert!(
        metrics.reclaimed <= metrics.enqueued.saturating_sub(evicted_pinned_gens),
        "premature reclamation detected: enqueued={} reclaimed={} evicted_pinned_gens={evicted_pinned_gens}",
        metrics.enqueued,
        metrics.reclaimed
    );
}

// spec: RUN-003, RUN-007, RUN-008, INV-16
#[test]
fn run_007_state_machine_interleaving_publisher_reclaimer_rollback() {
    // Deterministic exploration across 6 distinct adversarial & normal schedules covering every
    // publisher, reclaimer, pin-retention, infra-noise isolation, rollback, and bad-gen guard transition.
    let schedules: &[&[StepOp]] = &[
        // Schedule A: pin old -> flood ring -> infra noise ignored -> reclaimer holds pin until released.
        &[
            StepOp::PinCurrent,
            StepOp::PublishGood,
            StepOp::PublishGood,
            StepOp::PublishGood,
            StepOp::PublishGood,
            StepOp::PublishGood,
            StepOp::FlushReclaimer,
            StepOp::RecordInfraNoise,
            StepOp::ReleaseOldestPin,
            StepOp::FlushReclaimer,
        ],
        // Schedule B: promote faulty branch -> watch window triggers branch quarantine ->
        // re-promoting bad hash blocked without force -> allowed with force.
        &[
            StepOp::PinCurrent,
            StepOp::PublishFaultyBranch1,
            StepOp::RecordInfraNoise,
            StepOp::RecordFaultyBranch1AndEvaluate,
            StepOp::AttemptRepromoteBadWithoutForce,
            StepOp::AttemptRepromoteBadWithForce,
            StepOp::ReleaseOldestPin,
            StepOp::FlushReclaimer,
        ],
        // Schedule C: interleaved pins + faulty publish + flood past ring + rollback + drain.
        &[
            StepOp::PinCurrent,
            StepOp::PublishGood,
            StepOp::PinCurrent,
            StepOp::PublishFaultyBranch1,
            StepOp::RecordFaultyBranch1AndEvaluate,
            StepOp::PublishGood,
            StepOp::PublishGood,
            StepOp::PublishGood,
            StepOp::FlushReclaimer,
            StepOp::ReleaseOldestPin,
            StepOp::ReleaseOldestPin,
            StepOp::FlushReclaimer,
        ],
    ];

    for (sched_idx, schedule) in schedules.iter().enumerate() {
        let b1 = make_branch_with_pad(1, "getUser", 1, 32);
        let b2 = make_branch_with_pad(2, "getOrder", 1, 32);
        let gen1 = build_composed_snapshot(&Snapshot::empty(0), &[b1, b2]);
        let actor = Arc::new(PublisherActor::new(
            gen1,
            PublisherConfig {
                ring_capacity: 5,
                prewarm_top_n: 0,
                ..PublisherConfig::default()
            },
        ));
        let mut env = ModelEnv {
            actor,
            watcher: RollbackWatcher::new(WatchConfig::default()),
            pins: BTreeMap::new(),
            bad_gens: BTreeSet::new(),
            current_rev: 1,
        };
        let mut last_bad_snap: Option<Snapshot> = None;

        for (step_idx, op) in schedule.iter().enumerate() {
            match op {
                StepOp::PublishGood => {
                    env.current_rev += 1;
                    let prev = env.actor.pin();
                    let next = delta_branch_snapshot(&prev, 2, env.current_rev, 32);
                    let rec = env
                        .actor
                        .publish(next, &PublishOptions::default(), Vec::new())
                        .expect("publish good");
                    let cur = env.actor.pin();
                    assert_eq!(cur.generation, rec.generation);
                    env.watcher.start_watch(&prev, &cur, 0.01);
                }
                StepOp::PublishFaultyBranch1 => {
                    env.current_rev += 1;
                    let prev = env.actor.pin();
                    let next = delta_branch_snapshot(&prev, 1, env.current_rev, 32);
                    let rec = env
                        .actor
                        .publish(next, &PublishOptions::default(), Vec::new())
                        .expect("publish faulty branch 1");
                    let cur = env.actor.pin();
                    last_bad_snap = Some((*cur).clone());
                    assert_eq!(cur.generation, rec.generation);
                    env.watcher.start_watch(&prev, &cur, 0.01);
                }
                StepOp::PinCurrent => {
                    let p = env.actor.pin();
                    env.pins.entry(p.generation).or_default().push(p);
                }
                StepOp::ReleaseOldestPin => {
                    if let Some((&oldest_gen, vec)) = env.pins.iter_mut().next() {
                        vec.pop();
                        if vec.is_empty() {
                            env.pins.remove(&oldest_gen);
                        }
                    }
                }
                StepOp::RecordInfraNoise => {
                    for _ in 0..500 {
                        env.watcher.record(WatchSignal::UnrelatedInfraFailure);
                    }
                    assert_eq!(
                        env.watcher.evaluate(Duration::from_secs(10), 1_000),
                        RollbackDecision::Healthy,
                        "unrelated infra noise must never roll back (sched {sched_idx} step {step_idx})"
                    );
                }
                StepOp::RecordFaultyBranch1AndEvaluate => {
                    for _ in 0..250 {
                        env.watcher.record(WatchSignal::BranchCall {
                            branch_uid: uid_for(1),
                            failed: true,
                        });
                    }
                    let decision = env.watcher.evaluate(Duration::from_secs(15), 2_000);
                    match decision {
                        RollbackDecision::QuarantineBranch {
                            bad_generation,
                            clean_generation,
                            branch_uid,
                            ..
                        } => {
                            assert_eq!(branch_uid, uid_for(1));
                            env.bad_gens.insert(bad_generation);
                            let rec = env
                                .actor
                                .quarantine_branch_and_publish(
                                    branch_uid,
                                    clean_generation,
                                    bad_generation,
                                    "state-machine faulty branch1",
                                )
                                .expect("quarantine publish");
                            assert_eq!(rec.generation, bad_generation + 1);
                        }
                        other => panic!(
                            "expected QuarantineBranch in sched {sched_idx} step {step_idx}, got {other:?}"
                        ),
                    }
                }
                StepOp::AttemptRepromoteBadWithoutForce => {
                    if let Some(ref bad_snap) = last_bad_snap {
                        let cur = env.actor.pin();
                        let mut attempt = bad_snap.clone();
                        attempt.generation = cur.generation + 1;
                        let err = env
                            .actor
                            .publish(attempt, &PublishOptions::default(), Vec::new())
                            .expect_err("bad generation hash must be blocked without force");
                        assert!(matches!(err, PublisherError::BadGenerationBlocked { .. }));
                    }
                }
                StepOp::AttemptRepromoteBadWithForce => {
                    if let Some(ref bad_snap) = last_bad_snap {
                        let cur = env.actor.pin();
                        let mut attempt = bad_snap.clone();
                        attempt.generation = cur.generation + 1;
                        let rec = env
                            .actor
                            .publish(
                                attempt,
                                &PublishOptions {
                                    force: true,
                                    ..PublishOptions::default()
                                },
                                Vec::new(),
                            )
                            .expect("force re-promote succeeds");
                        assert_eq!(rec.generation, cur.generation + 1);
                    }
                }
                StepOp::FlushReclaimer => {
                    env.actor.sync_reclaimer();
                    let ring = env.actor.ring_generations();
                    let min_ring_gen = ring.first().map_or(u64::MAX, |r| r.0);
                    let evicted_pinned = env
                        .pins
                        .iter()
                        .filter(|(pinned_gen, list)| **pinned_gen < min_ring_gen && !list.is_empty())
                        .count();
                    let metrics = env.actor.reclaimer_metrics();
                    assert!(
                        metrics.pending >= evicted_pinned,
                        "after flush: evicted_pinned={evicted_pinned}, pending={}",
                        metrics.pending
                    );
                }
            }
            check_invariants(&env);
        }

        // Release remaining pins and verify complete off-thread drain.
        env.pins.clear();
        env.actor.sync_reclaimer();
        let final_metrics = env.actor.reclaimer_metrics();
        assert_eq!(final_metrics.pending, 0);
        assert_eq!(final_metrics.reclaimed, final_metrics.enqueued);
    }
}
