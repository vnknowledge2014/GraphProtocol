//! Property and unit tests for `gap_ngc_sdk::foreach` and `gap_ngc_sdk::mock` (`NGC-015`, `NGC-020`, `NGC-026`, `NGC-090`, `NGC-096`, `INV-18`).
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector};
use gap_ngc_sdk::error::ngc_error_from_proto;
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_ngc_sdk::mock::{MockConnector, RpcMethod};
use gap_proto::ngc::v1::{
    DescribeRequest, DescribeResponse, HealthRequest, QueryRequest, QueryResponse, RowSetChunk,
    VariableSet, query_response,
};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use tokio::sync::Notify;
use tokio::time::Instant;

const FOREACH_PROPTEST_CASES: u32 = 256;
const _: () = assert!(FOREACH_PROPTEST_CASES >= 256);

fn make_struct(
    entries: impl IntoIterator<Item = (&'static str, prost_types::value::Kind)>,
) -> prost_types::Struct {
    let mut fields = BTreeMap::new();
    for (k, kind) in entries {
        fields.insert(k.to_owned(), prost_types::Value { kind: Some(kind) });
    }
    prost_types::Struct { fields }
}

fn num_kind(n: f64) -> prost_types::value::Kind {
    prost_types::value::Kind::NumberValue(n)
}

fn str_kind(s: &str) -> prost_types::value::Kind {
    prost_types::value::Kind::StringValue(s.to_owned())
}

#[derive(Debug, Clone)]
enum SyntheticOutcome {
    Rows(usize),
    ElemError(NgcErrorCode, String),
}

fn canonical_key_for_reference(st: Option<&prost_types::Struct>) -> String {
    fn fmt_val(v: &prost_types::Value) -> String {
        match &v.kind {
            None | Some(prost_types::value::Kind::NullValue(_)) => "null".to_owned(),
            Some(prost_types::value::Kind::BoolValue(b)) => format!("b:{b}"),
            Some(prost_types::value::Kind::NumberValue(n)) => {
                let bits = if *n == 0.0 {
                    0.0_f64.to_bits()
                } else {
                    n.to_bits()
                };
                format!("n:{bits:016x}")
            }
            Some(prost_types::value::Kind::StringValue(s)) => format!("s:{s:?}"),
            Some(prost_types::value::Kind::ListValue(l)) => {
                let items: Vec<String> = l.values.iter().map(fmt_val).collect();
                format!("[{}]", items.join(","))
            }
            Some(prost_types::value::Kind::StructValue(s)) => fmt_struct(s),
        }
    }
    fn fmt_struct(s: &prost_types::Struct) -> String {
        let mut pairs: Vec<(&String, String)> =
            s.fields.iter().map(|(k, v)| (k, fmt_val(v))).collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        let body: Vec<String> = pairs
            .into_iter()
            .map(|(k, v)| format!("{k:?}:{v}"))
            .collect();
        format!("{{{}}}", body.join(","))
    }
    match st {
        None => "{}".to_owned(),
        Some(s) => fmt_struct(s),
    }
}

fn build_rows_for_key(key: &str, count: usize) -> Vec<prost_types::Struct> {
    (0..count)
        .map(|seq| {
            let mut fields = BTreeMap::new();
            fields.insert(
                "key".to_owned(),
                prost_types::Value {
                    kind: Some(str_kind(key)),
                },
            );
            fields.insert(
                "seq".to_owned(),
                prost_types::Value {
                    kind: Some(num_kind(seq as f64)),
                },
            );
            prost_types::Struct { fields }
        })
        .collect()
}

/// Naive per-index reference evaluator (`m1-03-brief.md` §4): evaluates each caller `VariableSet`
/// independently without deduplication or batching.
fn naive_reference_evaluate(
    sets: &[VariableSet],
    outcomes: &BTreeMap<String, SyntheticOutcome>,
    failing_batch_idx: Option<(usize, usize)>, // (batch_index, max_batch_size) if batch-level failure injected
) -> BTreeMap<u32, Result<Vec<prost_types::Struct>, NgcErrorCode>> {
    // Determine which canonical keys land in the failing batch (if any) so the reference knows
    // which distinct keys experience a batch-level upstream error.
    let mut distinct_order: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for vs in sets {
        let ck = canonical_key_for_reference(vs.values.as_ref());
        if seen.insert(ck.clone()) {
            distinct_order.push(ck);
        }
    }
    let mut batch_failed_keys = BTreeSet::new();
    if let Some((fail_b, batch_sz)) = failing_batch_idx {
        for (b_idx, chunk) in distinct_order.chunks(batch_sz).enumerate() {
            if b_idx == fail_b {
                for ck in chunk {
                    batch_failed_keys.insert(ck.clone());
                }
            }
        }
    }

    let mut reference = BTreeMap::new();
    for vs in sets {
        let ck = canonical_key_for_reference(vs.values.as_ref());
        if batch_failed_keys.contains(&ck) {
            reference.insert(vs.index, Err(NgcErrorCode::UpstreamError));
            continue;
        }
        match outcomes.get(&ck) {
            Some(SyntheticOutcome::Rows(n)) => {
                reference.insert(vs.index, Ok(build_rows_for_key(&ck, *n)));
            }
            Some(SyntheticOutcome::ElemError(code, _)) => {
                reference.insert(vs.index, Err(*code));
            }
            None => {
                reference.insert(vs.index, Ok(Vec::new()));
            }
        }
    }
    reference
}

#[derive(Debug, Default)]
struct CollectedIndex {
    rows: Vec<prost_types::Struct>,
    closing_chunks: usize,
    saw_chunk_after_close: bool,
    error: Option<NgcErrorInfo>,
}

fn collect_chunks(chunks: Vec<RowSetChunk>) -> BTreeMap<u32, CollectedIndex> {
    let mut by_index: BTreeMap<u32, CollectedIndex> = BTreeMap::new();
    for ch in chunks {
        let entry = by_index.entry(ch.foreach_index).or_default();
        if entry.closing_chunks > 0 {
            entry.saw_chunk_after_close = true;
        }
        entry.rows.extend(ch.rows);
        if ch.last_for_index {
            entry.closing_chunks += 1;
        }
        if let Some(err) = ch.error {
            assert!(
                ch.last_for_index,
                "RowSetChunk.error must only appear on closing chunk (index {})",
                ch.foreach_index
            );
            entry.error = Some(ngc_error_from_proto(&err));
        }
    }
    by_index
}

// spec: NGC-015, NGC-096
#[tokio::test(start_paused = true)]
async fn foreach_rejects_zero_batch_size_and_duplicate_indices() {
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(30));

    let zero_batch = foreach(
        vec![VariableSet {
            index: 0,
            values: None,
        }],
        ForeachConfig::new(0, 4),
        &scope,
        |_batch| async { Ok(Vec::new()) },
    );
    assert_eq!(
        zero_batch.expect_err("max_batch_size = 0 rejected").code,
        NgcErrorCode::InvalidArgument
    );

    let zero_inflight = foreach(
        vec![VariableSet {
            index: 0,
            values: None,
        }],
        ForeachConfig::new(4, 0),
        &scope,
        |_batch| async { Ok(Vec::new()) },
    );
    assert_eq!(
        zero_inflight.expect_err("max_in_flight = 0 rejected").code,
        NgcErrorCode::InvalidArgument
    );

    let upstream_invocations = Arc::new(AtomicUsize::new(0));
    let inv_clone = Arc::clone(&upstream_invocations);
    let dup = foreach(
        vec![
            VariableSet {
                index: 7,
                values: Some(make_struct([("k", str_kind("a"))])),
            },
            VariableSet {
                index: 7,
                values: Some(make_struct([("k", str_kind("b"))])),
            },
        ],
        ForeachConfig::new(4, 2),
        &scope,
        move |_batch| {
            let inv = Arc::clone(&inv_clone);
            async move {
                inv.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        },
    );
    assert_eq!(
        dup.expect_err("duplicate VariableSet.index rejected").code,
        NgcErrorCode::InvalidArgument
    );
    assert_eq!(upstream_invocations.load(Ordering::SeqCst), 0);
}

// spec: NGC-020, NGC-026, NGC-096, INV-18
#[tokio::test(start_paused = true)]
async fn inv_18_foreach_deduplicates_canonical_json_negative_zero_and_empty_structs() {
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(30));

    // Indices 10, 20, 30 all represent the exact same canonical JSON:
    // - 10: {"a": -0.0, "b": {"x": 1.0, "y": "hi"}}
    // - 20: {"b": {"y": "hi", "x": 1.0}, "a": 0.0}
    // - 30: {"a": 0.0, "b": {"x": 1.0, "y": "hi"}}
    // Indices 40 (None) and 50 (Some(empty Struct)) both represent `{}` (0 rows returned).
    let mut nested_1 = BTreeMap::new();
    nested_1.insert(
        "x".to_owned(),
        prost_types::Value {
            kind: Some(num_kind(1.0)),
        },
    );
    nested_1.insert(
        "y".to_owned(),
        prost_types::Value {
            kind: Some(str_kind("hi")),
        },
    );

    let mut nested_2 = BTreeMap::new();
    nested_2.insert(
        "y".to_owned(),
        prost_types::Value {
            kind: Some(str_kind("hi")),
        },
    );
    nested_2.insert(
        "x".to_owned(),
        prost_types::Value {
            kind: Some(num_kind(1.0)),
        },
    );

    let s1 = make_struct([
        ("a", num_kind(-0.0)),
        (
            "b",
            prost_types::value::Kind::StructValue(prost_types::Struct { fields: nested_1 }),
        ),
    ]);
    let s2 = make_struct([
        (
            "b",
            prost_types::value::Kind::StructValue(prost_types::Struct { fields: nested_2 }),
        ),
        ("a", num_kind(0.0)),
    ]);

    let sets = vec![
        VariableSet {
            index: 10,
            values: Some(s1.clone()),
        },
        VariableSet {
            index: 20,
            values: Some(s2),
        },
        VariableSet {
            index: 30,
            values: Some(s1),
        },
        VariableSet {
            index: 40,
            values: None,
        },
        VariableSet {
            index: 50,
            values: Some(prost_types::Struct::default()),
        },
    ];

    let observed_batches = Arc::new(AtomicUsize::new(0));
    let observed_distinct_items = Arc::new(AtomicUsize::new(0));
    let ob = Arc::clone(&observed_batches);
    let od = Arc::clone(&observed_distinct_items);

    let mut run = foreach(
        sets,
        ForeachConfig::new(10, 2).with_max_rows_per_chunk(2),
        &scope,
        move |batch| {
            let ob = Arc::clone(&ob);
            let od = Arc::clone(&od);
            async move {
                ob.fetch_add(1, Ordering::SeqCst);
                od.fetch_add(batch.len(), Ordering::SeqCst);
                let out = batch
                    .into_iter()
                    .map(|st| {
                        if st.fields.is_empty() {
                            Ok(Vec::new()) // empty index case
                        } else {
                            Ok(vec![
                                make_struct([("r", num_kind(1.0))]),
                                make_struct([("r", num_kind(2.0))]),
                                make_struct([("r", num_kind(3.0))]),
                            ])
                        }
                    })
                    .collect();
                Ok(out)
            }
        },
    )
    .expect("foreach starts");

    let mut chunks = Vec::new();
    while let Some(item) = run.stream_mut().next().await {
        chunks.push(item.expect("no fatal call-level error"));
    }

    // 5 caller indices collapsed into 2 distinct keys -> 1 batch call!
    assert_eq!(observed_batches.load(Ordering::SeqCst), 1);
    assert_eq!(observed_distinct_items.load(Ordering::SeqCst), 2);
    assert_eq!(run.upstream_calls(), 1);

    let collected = collect_chunks(chunks);
    assert_eq!(collected.len(), 5);
    for idx in [10_u32, 20, 30] {
        let entry = &collected[&idx];
        assert_eq!(entry.closing_chunks, 1);
        assert!(!entry.saw_chunk_after_close);
        assert!(entry.error.is_none());
        assert_eq!(entry.rows.len(), 3);
    }
    for empty_idx in [40_u32, 50] {
        let entry = &collected[&empty_idx];
        assert_eq!(
            entry.closing_chunks, 1,
            "empty index {empty_idx} must emit closing chunk"
        );
        assert!(!entry.saw_chunk_after_close);
        assert!(entry.error.is_none());
        assert!(entry.rows.is_empty());
    }
}

// spec: NGC-020, NGC-096
#[tokio::test(start_paused = true)]
async fn foreach_upstream_length_mismatch_marks_only_that_batch_internal() {
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(30));
    let sets = vec![
        VariableSet {
            index: 0,
            values: Some(make_struct([("id", num_kind(1.0))])),
        },
        VariableSet {
            index: 1,
            values: Some(make_struct([("id", num_kind(2.0))])),
        },
        VariableSet {
            index: 2,
            values: Some(make_struct([("id", num_kind(3.0))])),
        },
        VariableSet {
            index: 3,
            values: Some(make_struct([("id", num_kind(4.0))])),
        },
    ];

    let mut run = foreach(sets, ForeachConfig::new(2, 1), &scope, |batch| async move {
        let first_id = batch[0]
            .fields
            .get("id")
            .and_then(|v| match v.kind {
                Some(prost_types::value::Kind::NumberValue(n)) => Some(n),
                _ => None,
            })
            .unwrap_or(0.0);
        if (first_id - 1.0).abs() < f64::EPSILON {
            // Wrong length: batch had 2 distinct sets, upstream returned only 1!
            Ok(vec![Ok(vec![make_struct([("ok", num_kind(1.0))])])])
        } else {
            Ok(batch.into_iter().map(|st| Ok(vec![st])).collect())
        }
    })
    .expect("foreach starts");

    let mut chunks = Vec::new();
    while let Some(res) = run.stream_mut().next().await {
        chunks.push(res.expect("per-batch mismatch stays per-index"));
    }
    assert_eq!(run.upstream_calls(), 2);

    let collected = collect_chunks(chunks);
    for bad_idx in [0_u32, 1] {
        let e = collected[&bad_idx].error.as_ref().expect("batch 0 failed");
        assert_eq!(e.code, NgcErrorCode::Internal);
        assert_eq!(collected[&bad_idx].closing_chunks, 1);
    }
    for ok_idx in [2_u32, 3] {
        assert!(collected[&ok_idx].error.is_none());
        assert_eq!(collected[&ok_idx].rows.len(), 1);
        assert_eq!(collected[&ok_idx].closing_chunks, 1);
    }
}

// spec: NGC-025, NGC-096, INV-12
#[tokio::test(start_paused = true)]
async fn inv_12_foreach_cancel_and_deadline_stop_issuing_further_upstream_calls() {
    // 1. Cancellation stops further batches from being issued (`max_in_flight = 1`, 5 batches).
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(60));
    let cancel_token = scope.cancel.clone();
    let started = Arc::new(Notify::new());
    let started_clone = Arc::clone(&started);

    let sets: Vec<VariableSet> = (0..5_u32)
        .map(|i| VariableSet {
            index: i,
            values: Some(make_struct([("i", num_kind(f64::from(i)))])),
        })
        .collect();

    let mut run = foreach(
        sets.clone(),
        ForeachConfig::new(1, 1),
        &scope,
        move |_batch| {
            let s = Arc::clone(&started_clone);
            async move {
                s.notify_one();
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(vec![Ok(Vec::new())])
            }
        },
    )
    .expect("foreach cancel run");

    let cancel_driver = tokio::spawn(async move {
        started.notified().await;
        cancel_token.cancel();
    });

    let mut fatal_err = None;
    while let Some(item) = run.stream_mut().next().await {
        if let Err(err) = item {
            fatal_err = Some(err);
            break;
        }
    }
    cancel_driver.await.expect("driver");
    assert_eq!(fatal_err.expect("cancelled").code, NgcErrorCode::Cancelled);
    assert_eq!(
        run.upstream_calls(),
        1,
        "only batch 0 was started before cancel; batches 1..4 must never be issued"
    );

    // 2. Deadline expiration mid-run stops issuing further batches.
    let deadline_scope = CallScope::for_test(Instant::now() + Duration::from_millis(50));
    let mut deadline_run = foreach(
        sets,
        ForeachConfig::new(1, 1),
        &deadline_scope,
        |_batch| async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(vec![Ok(Vec::new())])
        },
    )
    .expect("foreach deadline run");

    let mut deadline_err = None;
    while let Some(item) = deadline_run.stream_mut().next().await {
        if let Err(err) = item {
            deadline_err = Some(err);
            break;
        }
    }
    assert_eq!(
        deadline_err.expect("deadline exceeded").code,
        NgcErrorCode::DeadlineExceeded
    );
    // Batch 0 finishes at t=30ms, batch 1 starts at t=30ms and is cut off at t=50ms -> 2 calls, never 5!
    assert_eq!(deadline_run.upstream_calls(), 2);
}

#[derive(Debug, Clone)]
struct GeneratedForeachCase {
    sets: Vec<VariableSet>,
    max_batch_size: usize,
    max_in_flight: usize,
    max_rows_per_chunk: usize,
    outcomes: BTreeMap<String, SyntheticOutcome>,
    failing_batch: Option<usize>,
}

fn foreach_case_strategy() -> impl Strategy<Value = GeneratedForeachCase> {
    (
        0_usize..=28,
        1_usize..=7,
        1_usize..=4,
        1_usize..=4,
        proptest::option::of(0_usize..=6),
    )
        .prop_flat_map(
            |(num_sets, max_batch_size, max_in_flight, max_rows_per_chunk, failing_batch)| {
                let indices_strat = proptest::collection::btree_set(0_u32..500, num_sets)
                    .prop_map(|s| s.into_iter().collect::<Vec<u32>>())
                    .prop_shuffle();
                // Small key space (`0..10`) intentionally creates frequent duplicates + `-0.0` vs `+0.0`.
                let key_choices =
                    proptest::collection::vec((0_u8..10, any::<bool>(), 0_u8..6), num_sets);
                (
                    indices_strat,
                    key_choices,
                    Just(max_batch_size),
                    Just(max_in_flight),
                    Just(max_rows_per_chunk),
                    Just(failing_batch),
                )
            },
        )
        .prop_map(
            |(
                indices,
                key_choices,
                max_batch_size,
                max_in_flight,
                max_rows_per_chunk,
                failing_batch,
            )| {
                let mut sets = Vec::new();
                let mut outcomes = BTreeMap::new();

                for (idx, (bucket, neg_zero_or_flip, outcome_tag)) in
                    indices.into_iter().zip(key_choices)
                {
                    let val_opt = if bucket == 0 {
                        if neg_zero_or_flip {
                            None
                        } else {
                            Some(prost_types::Struct::default())
                        }
                    } else {
                        let z = if neg_zero_or_flip { -0.0 } else { 0.0 };
                        let mut fields = BTreeMap::new();
                        if neg_zero_or_flip {
                            fields.insert(
                                "k".to_owned(),
                                prost_types::Value {
                                    kind: Some(num_kind(f64::from(bucket))),
                                },
                            );
                            fields.insert(
                                "z".to_owned(),
                                prost_types::Value {
                                    kind: Some(num_kind(z)),
                                },
                            );
                        } else {
                            fields.insert(
                                "z".to_owned(),
                                prost_types::Value {
                                    kind: Some(num_kind(z)),
                                },
                            );
                            fields.insert(
                                "k".to_owned(),
                                prost_types::Value {
                                    kind: Some(num_kind(f64::from(bucket))),
                                },
                            );
                        }
                        Some(prost_types::Struct { fields })
                    };

                    let ck = canonical_key_for_reference(val_opt.as_ref());
                    outcomes.entry(ck).or_insert_with(|| match outcome_tag {
                        0 => SyntheticOutcome::Rows(0), // empty index
                        1 => SyntheticOutcome::Rows(1),
                        2 => SyntheticOutcome::Rows(3),
                        3 => SyntheticOutcome::Rows(6),
                        4 => SyntheticOutcome::ElemError(
                            NgcErrorCode::NotFound,
                            format!("missing bucket {bucket}"),
                        ),
                        _ => SyntheticOutcome::ElemError(
                            NgcErrorCode::PermissionDenied,
                            format!("denied bucket {bucket}"),
                        ),
                    });

                    sets.push(VariableSet {
                        index: idx,
                        values: val_opt,
                    });
                }

                GeneratedForeachCase {
                    sets,
                    max_batch_size,
                    max_in_flight,
                    max_rows_per_chunk,
                    outcomes,
                    failing_batch,
                }
            },
        )
}

// spec: NGC-020, NGC-026, NGC-090, NGC-096, INV-18
#[test]
fn proptest_foreach_matches_naive_reference_order_chunking_dedupe_and_concurrency() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build current-thread runtime");

    let config = ProptestConfig {
        cases: FOREACH_PROPTEST_CASES,
        max_shrink_iters: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    let mut runner = TestRunner::new(config);

    let res = runner.run(&foreach_case_strategy(), |case| {
        rt.block_on(async {
            let distinct_keys: BTreeSet<String> = case
                .sets
                .iter()
                .map(|vs| canonical_key_for_reference(vs.values.as_ref()))
                .collect();
            let distinct_count = distinct_keys.len();
            let expected_batches = if distinct_count == 0 {
                0
            } else {
                distinct_count.div_ceil(case.max_batch_size)
            };

            let reference = naive_reference_evaluate(
                &case.sets,
                &case.outcomes,
                case.failing_batch.map(|b| (b, case.max_batch_size)),
            );

            let active = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let batch_seq = Arc::new(AtomicUsize::new(0));
            let seen_upstream_keys = Arc::new(std::sync::Mutex::new(BTreeSet::new()));

            let outcomes = Arc::new(case.outcomes.clone());
            let max_batch_size = case.max_batch_size;
            let failing_batch = case.failing_batch;

            let scope = CallScope::for_test(Instant::now() + Duration::from_secs(3_600));
            let cfg = ForeachConfig::new(case.max_batch_size, case.max_in_flight)
                .with_max_rows_per_chunk(case.max_rows_per_chunk);

            let active_c = Arc::clone(&active);
            let peak_c = Arc::clone(&peak);
            let seq_c = Arc::clone(&batch_seq);
            let seen_c = Arc::clone(&seen_upstream_keys);
            let out_c = Arc::clone(&outcomes);

            let mut run = foreach(case.sets.clone(), cfg, &scope, move |batch| {
                let active = Arc::clone(&active_c);
                let peak = Arc::clone(&peak_c);
                let seq = Arc::clone(&seq_c);
                let seen = Arc::clone(&seen_c);
                let outcomes = Arc::clone(&out_c);
                async move {
                    assert!(
                        !batch.is_empty() && batch.len() <= max_batch_size,
                        "invalid batch size {}",
                        batch.len()
                    );
                    let my_batch = seq.fetch_add(1, Ordering::SeqCst);
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);

                    for st in &batch {
                        let ck = canonical_key_for_reference(Some(st));
                        let newly_inserted = seen.lock().expect("seen mutex").insert(ck.clone());
                        assert!(
                            newly_inserted,
                            "duplicate canonical key `{ck}` sent to upstream more than once!"
                        );
                    }

                    // Yield via tokio task scheduling so concurrent batches interleave deterministically.
                    tokio::task::yield_now().await;

                    active.fetch_sub(1, Ordering::SeqCst);

                    if failing_batch == Some(my_batch) {
                        return Err(NgcErrorInfo::new(
                            NgcErrorCode::UpstreamError,
                            format!("batch {my_batch} failed"),
                        ));
                    }

                    let res = batch
                        .into_iter()
                        .map(|st| {
                            let ck = canonical_key_for_reference(Some(&st));
                            match outcomes.get(&ck) {
                                Some(SyntheticOutcome::Rows(n)) => Ok(build_rows_for_key(&ck, *n)),
                                Some(SyntheticOutcome::ElemError(code, msg)) => {
                                    Err(NgcErrorInfo::new(*code, msg.clone()))
                                }
                                None => Ok(Vec::new()),
                            }
                        })
                        .collect();
                    Ok(res)
                }
            })
            .map_err(|e| TestCaseError::fail(format!("foreach init failed: {e:?}")))?;

            let mut chunks = Vec::new();
            while let Some(item) = run.stream_mut().next().await {
                let ch =
                    item.map_err(|e| TestCaseError::fail(format!("unexpected stream err: {e:?}")))?;
                chunks.push(ch);
            }

            prop_assert_eq!(run.upstream_calls() as usize, expected_batches);
            prop_assert!(peak.load(Ordering::SeqCst) <= case.max_in_flight);

            let collected = collect_chunks(chunks);
            prop_assert_eq!(collected.len(), reference.len());

            for (idx, expected) in &reference {
                let actual = collected
                    .get(idx)
                    .ok_or_else(|| TestCaseError::fail(format!("missing index {idx}")))?;
                prop_assert_eq!(
                    actual.closing_chunks,
                    1,
                    "index {} closing chunks != 1",
                    idx
                );
                prop_assert!(
                    !actual.saw_chunk_after_close,
                    "chunk after close on {}",
                    idx
                );
                match expected {
                    Ok(expected_rows) => {
                        prop_assert!(actual.error.is_none(), "unexpected error on {}", idx);
                        prop_assert_eq!(&actual.rows, expected_rows, "row mismatch on {}", idx);
                    }
                    Err(expected_code) => {
                        let err = actual.error.as_ref().ok_or_else(|| {
                            TestCaseError::fail(format!("expected error on {idx}"))
                        })?;
                        prop_assert_eq!(err.code, *expected_code);
                        prop_assert!(actual.rows.is_empty());
                    }
                }
            }
            Ok(())
        })
    });

    if let Err(err) = res {
        panic!("foreach proptest failed: {err}");
    }
}

// spec: NGC-090
#[tokio::test(start_paused = true)]
async fn mock_connector_records_counters_high_water_delays_and_dropped_futures_and_streams() {
    let mock = Arc::new(
        MockConnector::new()
            .with_delay(Duration::from_millis(40))
            .on_describe(|req, _scope| {
                Ok(DescribeResponse {
                    not_modified: req.known_spec_hash == "same",
                    description: None,
                })
            })
            .on_query(|_req, _scope| {
                let s: BoxStream<QueryResponse> = Box::pin(stream::iter(vec![Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: Vec::new(),
                        last_for_index: true,
                        error: None,
                    })),
                })]));
                Ok(s)
            }),
    );

    // Drive 3 concurrent `describe` calls to verify `max_in_flight_observed == 3`.
    let scope = CallScope::for_test(Instant::now() + Duration::from_secs(10));
    let (r1, r2, r3) = tokio::join!(
        mock.describe(
            DescribeRequest {
                known_spec_hash: "same".to_owned()
            },
            scope.clone()
        ),
        mock.describe(DescribeRequest::default(), scope.clone()),
        mock.health(HealthRequest {}, scope.clone()),
    );
    assert!(r1.expect("r1").not_modified);
    assert!(!r2.expect("r2").not_modified);
    assert!(r3.is_ok());
    assert_eq!(mock.calls_for(RpcMethod::Describe), 2);
    assert_eq!(mock.calls_for(RpcMethod::Health), 1);
    assert_eq!(mock.total_calls(), 3);
    assert_eq!(mock.in_flight(), 0);
    assert_eq!(mock.max_in_flight_observed(), 3);
    assert_eq!(mock.dropped_count(), 0);

    // Drop an in-flight unary future mid-delay and verify drop guard records it immediately.
    {
        let mut fut = Box::pin(mock.describe(DescribeRequest::default(), scope.clone()));
        assert!(futures_util::poll!(&mut fut).is_pending());
        assert_eq!(mock.in_flight(), 1);
    }
    assert_eq!(mock.in_flight(), 0);
    assert_eq!(mock.dropped_count(), 1);

    // Drop an open stream before completion and verify drop guard records it.
    let endless_mock = MockConnector::new().on_query(|_req, _scope| {
        let s: BoxStream<QueryResponse> = Box::pin(stream::pending());
        Ok(s)
    });
    {
        let mut stream = endless_mock
            .query(QueryRequest::default(), scope)
            .await
            .expect("open endless stream");
        assert!(futures_util::poll!(stream.next()).is_pending());
        assert_eq!(endless_mock.in_flight(), 1);
    }
    assert_eq!(endless_mock.in_flight(), 0);
    assert_eq!(endless_mock.dropped_count(), 1);
}
