//! TDD integration and stress tests for `gap_snapshot::handle` (`INV-01`, `INV-02`, `INV-16`, `RUN-003`, `RUN-004`, `ARC-005`, `RUN-009`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use apollo_compiler::schema::ExtendedType;
use gap_core::clock::Timestamp;
use gap_core::ids::{BranchId, BranchUid, ProposalId};
use gap_core::snapshot::{
    BranchStatus, Endpoint, FieldCoordinate, Protocol, SNAPSHOT_FORMAT_VERSION, Snapshot, TrustTier,
};
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits, MetaSummary,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, SideEffectHint,
};
use gap_snapshot::{BranchInput, PublishError, SnapshotBuilder, SnapshotHandle, compose};

fn uid_for(idx: u16) -> BranchUid {
    let hi = (idx >> 8) as u8;
    let lo = (idx & 0xff) as u8;
    BranchUid::from_unix_ms(
        1_761_900_000_000 + u64::from(idx),
        [hi, lo, 0, 0, 0, 0, 0, 0, 0, 0],
    )
}

fn make_branch_input(idx: u16, rev: u32) -> BranchInput {
    let slug = format!("b{idx:04}");
    let branch_id = BranchId::new(&slug).expect("valid branch id");
    let field_name = format!("opB{idx:04}R{rev}");
    let type_name = format!("ItemR{rev}");
    let synopsis = format!("branch-{idx}-rev-{rev}");
    let sdl = format!(
        "type Query {{ {field_name}(id: ID!): {type_name} }}\ntype {type_name} {{ id: ID! rev: Int! }}\n"
    );

    let mut annotations = HashMap::new();
    annotations.insert("rev".to_owned(), rev.to_string());

    let op = ProtoOp {
        name: field_name,
        kind: ProtoOpKind::Query as i32,
        description: format!("Fetch item for {slug} rev {rev}"),
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
        annotations,
    };

    BranchInput {
        id: branch_id,
        uid: uid_for(idx),
        protocol: Protocol::Rest,
        trust: TrustTier::T1,
        endpoint: Endpoint {
            uri: format!("https://{slug}.internal/ngc"),
            transport: "grpc".to_owned(),
        },
        description: ConnectorDescription {
            connector_id: slug,
            connector_version: format!("0.{rev}.0"),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
            spec_hash: format!("{idx:064x}"),
            capabilities: Some(ProtoCapabilities::default()),
            limits: Some(ProtoLimits {
                max_batch_size: 64,
                max_in_flight: 32,
                max_page_size: 200,
                default_timeout_ms: 2_500,
                max_response_bytes: 1_048_576,
            }),
            schema_sdl: sdl,
            operations: vec![op],
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: synopsis,
            }),
            auth: None,
        },
        status: BranchStatus::Active,
    }
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(ms).expect("valid timestamp")
}

fn query_has_field(snap: &Snapshot, field: &str) -> bool {
    match snap.schema.types.get("Query") {
        Some(ExtendedType::Object(obj)) => obj.fields.contains_key(field),
        _ => false,
    }
}

// spec: INV-01, RUN-003
#[test]
fn inv_01_request_pins_single_generation() {
    let base = Snapshot::empty(0);
    let uid_1 = uid_for(1);

    let comp1 = compose(&[make_branch_input(1, 1)]).expect("compose gen 1");
    let gen1 = SnapshotBuilder::new(&base, comp1)
        .built_at(ts(1_761_900_001_000))
        .builder("test-inv-01")
        .build();
    assert_eq!(gen1.generation, 1);

    // Pre-build 100 successor generations (`2..=101`), each with distinct schema/branches/ownership.
    let mut successors = Vec::with_capacity(100);
    let mut prev = gen1.clone();
    for g in 2_u32..=101 {
        let comp_g = compose(&[make_branch_input(1, g)]).expect("compose successor");
        let snap_g = SnapshotBuilder::new(&prev, comp_g)
            .built_at(ts(1_761_900_000_000 + i64::from(g) * 1_000))
            .builder("test-inv-01")
            .build();
        prev = snap_g.clone();
        successors.push(snap_g);
    }

    let handle = Arc::new(SnapshotHandle::new(gen1));

    // A simulated in-flight request pins ONCE before the writer publishes 100 generations.
    let pinned = handle.pin();
    assert_eq!(pinned.generation, 1);

    let start_barrier = Arc::new(Barrier::new(2));
    let writer_done = Arc::new(AtomicBool::new(false));

    let writer_handle = {
        let handle = Arc::clone(&handle);
        let barrier = Arc::clone(&start_barrier);
        let done = Arc::clone(&writer_done);
        std::thread::spawn(move || {
            barrier.wait();
            for (idx, snap_g) in successors.into_iter().enumerate() {
                let expected_gen = (idx as u64) + 2;
                let got_gen = handle.publish(snap_g).expect("publish successor");
                assert_eq!(got_gen, expected_gen);
            }
            done.store(true, Ordering::Release);
        })
    };

    start_barrier.wait();

    // Repeatedly read `branches`, `schema`, and `ownership` from `pinned` while writer publishes
    // and continue for another 200 iterations after writer completes.
    let mut post_done_checks = 0_usize;
    loop {
        assert_eq!(pinned.generation, 1);
        assert_eq!(pinned.branches.len(), 1);
        let b1 = pinned.branches.get(&uid_1).expect("b0001 in pinned");
        assert_eq!(b1.synopsis, "branch-1-rev-1");
        assert_eq!(b1.operations.len(), 1);
        assert_eq!(b1.operations[0].id.as_str(), "b0001.opB0001R1");

        assert!(query_has_field(&pinned, "opB0001R1"));
        assert!(!query_has_field(&pinned, "opB0001R2"));
        assert!(!query_has_field(&pinned, "opB0001R101"));
        assert!(pinned.schema.types.contains_key("b0001_ItemR1"));
        assert!(!pinned.schema.types.contains_key("b0001_ItemR101"));

        assert_eq!(
            pinned
                .ownership
                .owner(&FieldCoordinate::new("Query", "opB0001R1")),
            Some(uid_1)
        );
        assert_eq!(
            pinned
                .ownership
                .owner(&FieldCoordinate::new("Query", "opB0001R2")),
            None
        );
        assert_eq!(
            pinned
                .ownership
                .owner(&FieldCoordinate::new("Query", "opB0001R101")),
            None
        );

        if writer_done.load(Ordering::Acquire) {
            post_done_checks += 1;
            if post_done_checks >= 200 {
                break;
            }
        }
    }

    writer_handle.join().expect("writer thread join");

    // Handle has advanced to generation 101, while `pinned` stays alive and holds generation 1.
    assert_eq!(handle.generation(), 101);
    let latest = handle.pin();
    assert_eq!(latest.generation, 101);
    assert!(query_has_field(&latest, "opB0001R101"));
    assert!(!query_has_field(&latest, "opB0001R1"));

    assert_eq!(Arc::strong_count(&pinned), 1);
    assert_eq!(pinned.generation, 1);
    assert_eq!(
        pinned.branches.get(&uid_1).expect("b0001").synopsis,
        "branch-1-rev-1"
    );
}

// spec: INV-02, RUN-003
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn inv_02_no_lock_on_read_path() {
    // Part (a): Static source inspection between explicit read-path markers in `src/handle.rs`.
    let handle_src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/handle.rs"))
        .expect("read crates/gap-snapshot/src/handle.rs");

    let begin_marker = "// BEGIN_READ_PATH";
    let end_marker = "// END_READ_PATH";
    let begin_pos = handle_src
        .find(begin_marker)
        .expect("BEGIN_READ_PATH marker must exist in src/handle.rs");
    let end_pos = handle_src
        .find(end_marker)
        .expect("END_READ_PATH marker must exist in src/handle.rs");
    assert!(
        begin_pos < end_pos,
        "BEGIN_READ_PATH must precede END_READ_PATH"
    );
    assert_eq!(
        handle_src.matches(begin_marker).count(),
        1,
        "expected exactly one BEGIN_READ_PATH marker"
    );
    assert_eq!(
        handle_src.matches(end_marker).count(),
        1,
        "expected exactly one END_READ_PATH marker"
    );

    let read_path_src = &handle_src[begin_pos + begin_marker.len()..end_pos];
    assert!(
        read_path_src.contains("pub fn pin("),
        "pin(&self) must stay inside BEGIN_READ_PATH..END_READ_PATH"
    );
    assert!(
        read_path_src.contains("pub fn generation("),
        "generation(&self) must stay inside BEGIN_READ_PATH..END_READ_PATH"
    );

    for forbidden in ["Mutex", "RwLock", ".lock(", ".read(", ".write(", "await"] {
        assert!(
            !read_path_src.contains(forbidden),
            "read path in src/handle.rs must not contain `{forbidden}`; got:\n{read_path_src}"
        );
    }

    // Part (b): Multi-thread stress test — 32 concurrent reader threads tight-looping `pin()`
    // alongside async Tokio reader tasks while a writer publishes 64 consecutive generations.
    const PUBLISHED_MAX: u32 = 65;
    let uid_1 = uid_for(1);
    let base = Snapshot::empty(0);
    let comp1 = compose(&[make_branch_input(1, 1)]).expect("compose gen 1");
    let gen1 = SnapshotBuilder::new(&base, comp1).build();

    let mut expected_hashes = vec![[0_u8; 32]; (PUBLISHED_MAX as usize) + 1];
    expected_hashes[1] = gen1.branch_set_hash;

    let mut to_publish = Vec::with_capacity((PUBLISHED_MAX - 1) as usize);
    let mut prev = gen1.clone();
    for g in 2..=PUBLISHED_MAX {
        let comp_g = compose(&[make_branch_input(1, g)]).expect("compose g");
        let snap_g = SnapshotBuilder::new(&prev, comp_g).build();
        expected_hashes[g as usize] = snap_g.branch_set_hash;
        prev = snap_g.clone();
        to_publish.push(snap_g);
    }

    let handle = Arc::new(SnapshotHandle::new(gen1));
    let expected_hashes = Arc::new(expected_hashes);

    // Spawn 16 async tasks on the multi-thread Tokio runtime that also exercise `pin()`.
    let mut async_tasks = Vec::with_capacity(16);
    for _ in 0..16 {
        let h = Arc::clone(&handle);
        let hashes = Arc::clone(&expected_hashes);
        async_tasks.push(tokio::spawn(async move {
            for _ in 0..500 {
                let t0 = Instant::now();
                let snap = h.pin();
                let elapsed = t0.elapsed();
                assert!(
                    elapsed < Duration::from_millis(50),
                    "async reader pin() blocked for {elapsed:?}"
                );
                let g = snap.generation;
                assert!(
                    (1..=u64::from(PUBLISHED_MAX)).contains(&g),
                    "unexpected generation {g}"
                );
                assert_eq!(snap.branch_set_hash, hashes[g as usize]);
                tokio::task::yield_now().await;
            }
        }));
    }

    // Spawn 32 OS reader threads + 1 writer thread synchronized at a barrier.
    let barrier = Barrier::new(33);
    std::thread::scope(|s| {
        for reader_id in 0..32_usize {
            let h = &handle;
            let hashes = &expected_hashes;
            let bar = &barrier;
            s.spawn(move || {
                bar.wait();
                for iter in 0..2_000_usize {
                    let t0 = Instant::now();
                    let snap = h.pin();
                    let elapsed = t0.elapsed();
                    assert!(
                        elapsed < Duration::from_millis(50),
                        "reader {reader_id} iter {iter}: pin() took {elapsed:?} (>= 50ms)"
                    );

                    let g = snap.generation;
                    assert!(
                        (1..=u64::from(PUBLISHED_MAX)).contains(&g),
                        "reader {reader_id}: pinned unpublished generation {g}"
                    );
                    assert_eq!(
                        snap.branch_set_hash, hashes[g as usize],
                        "torn branch_set_hash for generation {g}"
                    );
                    let field_name = format!("opB0001R{g}");
                    assert_eq!(
                        snap.ownership
                            .owner(&FieldCoordinate::new("Query", &field_name)),
                        Some(uid_1),
                        "torn ownership for generation {g}"
                    );
                    assert_eq!(
                        snap.branches.get(&uid_1).expect("branch").synopsis,
                        format!("branch-1-rev-{g}"),
                        "torn branch synopsis for generation {g}"
                    );
                }
            });
        }

        let h = &handle;
        let bar = &barrier;
        s.spawn(move || {
            bar.wait();
            for (idx, snap_g) in to_publish.into_iter().enumerate() {
                let want = (idx as u64) + 2;
                let got = h.publish(snap_g).expect("publish under 32-reader load");
                assert_eq!(got, want);
            }
        });
    });

    for task in async_tasks {
        task.await.expect("async reader task panicked");
    }

    assert_eq!(handle.generation(), u64::from(PUBLISHED_MAX));
}

// spec: INV-16, ARC-005, RUN-003
#[test]
fn inv_16_publish_1000_branches_single_delta_under_1ms() {
    let mut inputs: Vec<BranchInput> = (0..1_000_u16).map(|i| make_branch_input(i, 1)).collect();

    let base = Snapshot::empty(0);
    let comp1 = compose(&inputs).expect("compose 1,000 branches gen 1");
    let gen1 = SnapshotBuilder::new(&base, comp1)
        .built_at(ts(1_761_900_010_000))
        .build();
    assert_eq!(gen1.generation, 1);
    assert_eq!(gen1.branches.len(), 1_000);

    let handle = SnapshotHandle::new(gen1);
    let pinned_reader = handle.pin();
    assert_eq!(pinned_reader.generation, 1);

    // Mutate exactly 1 branch out of 1,000 (999 unchanged + 1 changed).
    inputs[999] = make_branch_input(999, 2);
    let comp2 = compose(&inputs).expect("compose 1,000 branches gen 2");
    let gen2 = SnapshotBuilder::new(&pinned_reader, comp2)
        .built_at(ts(1_761_900_011_000))
        .build();

    let t0 = Instant::now();
    let published_gen = handle.publish(gen2).expect("publish gen 2");
    let publish_elapsed = t0.elapsed();

    assert_eq!(published_gen, 2);
    assert_eq!(handle.generation(), 2);
    assert!(
        publish_elapsed < Duration::from_millis(1),
        "INV-16 smoke check: publish of 1-branch delta in 1,000-branch snapshot took {publish_elapsed:?} (>= 1ms)"
    );

    let after = handle.pin();
    assert_eq!(after.generation, 2);
    assert!(Arc::ptr_eq(
        pinned_reader.branches.get(&uid_for(0)).expect("gen1 b0"),
        after.branches.get(&uid_for(0)).expect("gen2 b0"),
    ));
    assert!(!Arc::ptr_eq(
        pinned_reader
            .branches
            .get(&uid_for(999))
            .expect("gen1 b999"),
        after.branches.get(&uid_for(999)).expect("gen2 b999"),
    ));
}

// spec: RUN-003, RUN-004
#[test]
fn publish_rejects_non_monotonic_generation_and_unsupported_format_version() {
    let handle = SnapshotHandle::new(Snapshot::empty(1));
    assert_eq!(handle.generation(), 1);

    // Same generation (`got == 1`, `expected == 2`).
    let err_same = handle
        .publish(Snapshot::empty(1))
        .expect_err("duplicate generation must fail");
    assert_eq!(
        err_same,
        PublishError::NonMonotonic {
            expected: 2,
            got: 1,
        }
    );
    assert_eq!(handle.generation(), 1);

    // Regressing generation (`got == 0`, `expected == 2`).
    let err_regress = handle
        .publish(Snapshot::empty(0))
        .expect_err("regressing generation must fail");
    assert_eq!(
        err_regress,
        PublishError::NonMonotonic {
            expected: 2,
            got: 0,
        }
    );
    assert_eq!(handle.generation(), 1);

    // Non-consecutive gap (`got == 3`, `expected == 2`).
    let err_gap = handle
        .publish(Snapshot::empty(3))
        .expect_err("non-consecutive generation must fail");
    assert_eq!(
        err_gap,
        PublishError::NonMonotonic {
            expected: 2,
            got: 3,
        }
    );
    assert_eq!(handle.generation(), 1);

    // Unsupported format_version (`SNAPSHOT_FORMAT_VERSION + 1`).
    let mut bad_fmt = Snapshot::empty(2);
    bad_fmt.format_version = SNAPSHOT_FORMAT_VERSION + 1;
    let err_fmt = handle
        .publish(bad_fmt)
        .expect_err("unsupported format_version must fail");
    assert_eq!(
        err_fmt,
        PublishError::UnsupportedFormat {
            got: SNAPSHOT_FORMAT_VERSION + 1,
            supported: SNAPSHOT_FORMAT_VERSION,
        }
    );
    assert_eq!(handle.generation(), 1);

    // Unsupported format_version (`0`).
    let mut zero_fmt = Snapshot::empty(2);
    zero_fmt.format_version = 0;
    let err_zero_fmt = handle
        .publish(zero_fmt)
        .expect_err("zero format_version must fail");
    assert_eq!(
        err_zero_fmt,
        PublishError::UnsupportedFormat {
            got: 0,
            supported: SNAPSHOT_FORMAT_VERSION,
        }
    );
    assert_eq!(handle.generation(), 1);

    // Valid consecutive generation (`2`) succeeds.
    let ok_gen = handle
        .publish(Snapshot::empty(2))
        .expect("consecutive generation with supported format_version must succeed");
    assert_eq!(ok_gen, 2);
    assert_eq!(handle.generation(), 2);
}

// spec: ARC-005, RUN-009
#[test]
fn snapshot_builder_reuses_unchanged_arcs_shares_ordmap_nodes_and_matches_permuted_hash() {
    let mut inputs: Vec<BranchInput> = (0..256_u16).map(|i| make_branch_input(i, 1)).collect();
    let proposal = ProposalId::from_unix_ms(1_761_900_020_000, [7_u8; 10]);

    let base = Snapshot::empty(0);
    let comp1 = compose(&inputs).expect("compose 256 branches gen 1");
    let gen1 = SnapshotBuilder::new(&base, comp1)
        .built_at(ts(1_761_900_020_000))
        .builder("builder-v0")
        .proposal_id(Some(proposal))
        .build();

    assert_eq!(gen1.generation, 1);
    assert_eq!(gen1.built_by.builder, "builder-v0");
    assert_eq!(gen1.built_by.proposal_id, Some(proposal));
    assert_eq!(gen1.built_by.parent_generation, Some(0));
    assert_eq!(gen1.branches.len(), 256);
    assert_eq!(gen1.groups.len(), 256);
    assert_eq!(gen1.ownership.len(), 768);

    // Change only branch 255; keep branches 0..255 untouched.
    inputs[255] = make_branch_input(255, 2);
    let mut permuted_inputs = inputs.clone();
    permuted_inputs.reverse();

    let comp2 = compose(&inputs).expect("compose 256 branches gen 2");
    let comp2_permuted = compose(&permuted_inputs).expect("compose permuted gen 2");

    let gen2 = SnapshotBuilder::new(&gen1, comp2).build();
    let gen2_permuted = SnapshotBuilder::new(&gen1, comp2_permuted).build();

    assert_eq!(gen2.generation, 2);
    assert_eq!(gen2.built_by.parent_generation, Some(1));
    assert_eq!(gen2.branch_set_hash, gen2_permuted.branch_set_hash);
    assert_ne!(gen2.branch_set_hash, gen1.branch_set_hash);

    // Every unchanged branch (0..255) shares the exact same `Arc<BranchDescriptor>`.
    for idx in 0..255_u16 {
        let u = uid_for(idx);
        let b1 = gen1.branches.get(&u).expect("in gen1");
        let b2 = gen2.branches.get(&u).expect("in gen2");
        assert!(
            Arc::ptr_eq(b1, b2),
            "unchanged branch {idx} must share Arc<BranchDescriptor>"
        );
    }

    // Changed branch (255) receives a new `Arc<BranchDescriptor>`.
    let u255 = uid_for(255);
    assert!(
        !Arc::ptr_eq(
            gen1.branches.get(&u255).expect("255 in gen1"),
            gen2.branches.get(&u255).expect("255 in gen2"),
        ),
        "changed branch 255 must be replaced with a new Arc<BranchDescriptor>"
    );

    // Drop `gen2_permuted` so only `gen1` and `gen2` hold references to the B-tree leaves.
    drop(gen2_permuted);

    // Because `SnapshotBuilder` delta-updates `gen1.branches` rather than rebuilding/cloning
    // `OrdMap` contents, untouched `OrdMap` B-tree leaf nodes are shared by pointer between
    // `gen1` and `gen2`, keeping `Arc::strong_count == 1` on descriptors in untouched leaves.
    let shared_leaf_descriptors = (0..255_u16)
        .filter(|&idx| {
            let b = gen2.branches.get(&uid_for(idx)).expect("in gen2");
            Arc::strong_count(b) == 1
        })
        .count();
    assert!(
        shared_leaf_descriptors >= 180,
        "expected >= 180/255 unchanged descriptors in shared OrdMap leaves (strong_count == 1), got {shared_leaf_descriptors}"
    );

    // Removing a branch in generation 3 prunes both `branches` and `groups` while keeping 0..255 shared.
    inputs.pop();
    let comp3 = compose(&inputs).expect("compose 255 branches gen 3");
    let gen3 = SnapshotBuilder::new(&gen2, comp3).build();
    assert_eq!(gen3.generation, 3);
    assert_eq!(gen3.branches.len(), 255);
    assert_eq!(gen3.groups.len(), 255);
    assert!(!gen3.branches.contains_key(&u255));
    assert!(Arc::ptr_eq(
        gen2.branches.get(&uid_for(0)).expect("0 in gen2"),
        gen3.branches.get(&uid_for(0)).expect("0 in gen3"),
    ));
}
