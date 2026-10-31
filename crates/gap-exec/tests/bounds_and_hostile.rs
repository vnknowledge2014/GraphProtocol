//! Concurrency bounds (`RUN-015` `max_parallel_fetches`), non-queueing per-branch bulkhead
//! (`NGC-071`, `GAP_OVERLOADED`), `INV-02` lock-free registry read path, `RUN-060` incremental
//! `max_response_bytes` early stop + exact-fit boundary, and hostile connectors.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::stream;
use gap_core::clock::ManualClock;
use gap_core::error::ErrorCode;
use gap_exec::{ConnectorRegistry, ExecLayer, ExecLimits, ExecRequest};
use gap_ngc_sdk::mock::MockConnector;
use serde_json::json;
use tokio::time::Instant;

use common::{
    build_two_branch_snapshot, compile_plan, complete_response, make_in_process_branch,
    rows_response, shop_uid, spawn_grpc_branch, stream_rows,
};

// spec: RUN-015
#[tokio::test(start_paused = true)]
async fn run_015_max_parallel_fetches_reaches_cap_and_never_exceeds_it() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    let shop_mock = MockConnector::new().on_query_async(|_req, _scope| async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        Ok(stream_rows(vec![json!({ "value": "pong" })]))
    });

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(
                shop_uid(),
                make_in_process_branch(shop_mock.clone(), clock.clone()),
                64,
            )
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);

    // 8 independent root fetches in a single Parallel plan, bounded by max_parallel_fetches = 3.
    let query = "{ p1: ping p2: ping p3: ping p4: ping p5: ping p6: ping p7: ping p8: ping }";
    let plan = compile_plan(&snapshot, query, &serde_json::Map::new());

    let limits = ExecLimits {
        max_parallel_fetches: 3,
        ..ExecLimits::default()
    };
    let resp = exec
        .execute(
            ExecRequest::new(snapshot, plan, Instant::now() + Duration::from_secs(5))
                .with_limits(limits),
        )
        .await;

    assert!(resp.errors.is_empty(), "errors: {:?}", resp.errors);
    assert_eq!(shop_mock.total_calls(), 8);
    assert_eq!(
        shop_mock.max_in_flight_observed(),
        3,
        "observed concurrency must reach cap (3) and NEVER exceed max_parallel_fetches (3)"
    );
    assert_eq!(shop_mock.in_flight(), 0);
}

// spec: NGC-071, RUN-015, INV-02
#[tokio::test(start_paused = true)]
async fn bulkhead_saturation_fails_fast_with_gap_overloaded_and_never_queues() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    // Each accepted call holds its stream open for 40 ms of virtual time.
    let shop_mock = MockConnector::new().on_query_async(|_req, _scope| async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        Ok(stream_rows(vec![json!({ "value": "ok" })]))
    });

    // Register shop branch with bulkhead capacity = 2 while ExecLimits allows 8 parallel fetches!
    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(
                shop_uid(),
                make_in_process_branch(shop_mock.clone(), clock.clone()),
                2,
            )
            .build(),
    );
    let exec = ExecLayer::new(registry.clone()).with_clock(clock);

    // Use 4 nullable root fields (`optionalNote`) so 2 succeed and 2 fail fast with GAP_OVERLOADED (`partial = true`)!
    let query = "{ n1: optionalNote n2: optionalNote n3: optionalNote n4: optionalNote }";
    let plan = compile_plan(&snapshot, query, &serde_json::Map::new());

    let t0 = Instant::now();
    let resp = exec
        .execute(ExecRequest::new(
            snapshot,
            plan,
            Instant::now() + Duration::from_secs(5),
        ))
        .await;

    // If the bulkhead queued instead of failing fast with `try_acquire`, all 4 would succeed in 80 ms!
    assert_eq!(
        t0.elapsed(),
        Duration::from_millis(40),
        "saturated bulkhead must fail fast without queueing (took {:?})",
        t0.elapsed()
    );
    assert_eq!(shop_mock.max_in_flight_observed(), 2);
    assert_eq!(shop_mock.total_calls(), 2);
    assert_eq!(resp.errors.len(), 2);
    for err in &resp.errors {
        assert_eq!(err.code(), ErrorCode::Overloaded);
        assert!(err.extensions.retry.expect("retry").retryable);
    }
    assert!(resp.extensions.partial);
    assert_eq!(registry.available_permits(&shop_uid()), Some(2));
}

fn collect_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
}

// spec: INV-02
#[tokio::test]
async fn inv_02_connector_registry_has_no_locks_on_read_path() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let mock = MockConnector::new();
    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), make_in_process_branch(mock, clock), 32)
            .build(),
    );

    let mut handles = Vec::new();
    for _ in 0..16 {
        let reg = Arc::clone(&registry);
        handles.push(tokio::spawn(async move {
            for _ in 0..500 {
                assert!(reg.client(&shop_uid()).is_some());
                assert_eq!(reg.max_in_flight(&shop_uid()), Some(32));
            }
        }));
    }
    for h in handles {
        h.await.expect("concurrent reader");
    }

    // Static verification that `crates/gap-exec/src/**` contains zero Mutex, RwLock, or DashMap!
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src_dir, &mut files);
    assert!(!files.is_empty());
    for file in files {
        let text = fs::read_to_string(&file).expect("read src file");
        let code_only: String = text
            .lines()
            .filter(|line| {
                let t = line.trim_start();
                !t.starts_with("//")
            })
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["Mutex", "RwLock", "DashMap"] {
            assert!(
                !code_only.contains(forbidden),
                "INV-02 violation: found `{forbidden}` in {}",
                file.display()
            );
        }
    }
}

// spec: RUN-060
#[tokio::test(start_paused = true)]
async fn run_060_max_response_bytes_stops_assembly_early_and_enforces_exact_fit_boundary() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    // Part A: Early stop on 5,000-row stream — must stop after touching only a handful of rows!
    let rows_touched = Arc::new(AtomicUsize::new(0));
    let rt = Arc::clone(&rows_touched);
    let streaming_mock = MockConnector::new().on_query(move |_req, _scope| {
        let rt_inner = Arc::clone(&rt);
        let s = stream::unfold(0_usize, move |idx| {
            let rt_step = Arc::clone(&rt_inner);
            async move {
                if idx >= 5_000 {
                    return None;
                }
                if idx == 4_999 {
                    return Some((Ok(complete_response()), 5_000));
                }
                rt_step.fetch_add(1, Ordering::SeqCst);
                let chunk = gap_proto::ngc::v1::QueryResponse {
                    chunk: Some(gap_proto::ngc::v1::query_response::Chunk::Rows(
                        gap_proto::ngc::v1::RowSetChunk {
                            foreach_index: 0,
                            rows: vec![common::json_obj_to_prost_struct(&json!({
                                "id": format!("order-{idx:04}"),
                                "sku": "SKU-LARGE-ITEM",
                                "totalCents": 1000
                            }))],
                            last_for_index: false,
                            error: None,
                        },
                    )),
                };
                Some((Ok(chunk), idx + 1))
            }
        });
        Ok(Box::pin(s))
    });

    let exec_stream = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(
                shop_uid(),
                make_in_process_branch(streaming_mock, clock.clone()),
                16,
            )
            .build(),
    ))
    .with_clock(clock.clone());

    let orders_plan = compile_plan(&snapshot, "{ orders { id sku } }", &serde_json::Map::new());
    let early_resp = exec_stream
        .execute(
            ExecRequest::new(
                snapshot.clone(),
                orders_plan.clone(),
                Instant::now() + Duration::from_secs(5),
            )
            .with_limits(ExecLimits {
                max_response_bytes: 200,
                ..ExecLimits::default()
            }),
        )
        .await;

    assert_eq!(early_resp.data, None);
    assert!(!early_resp.extensions.partial);
    assert_eq!(early_resp.errors.len(), 1);
    assert_eq!(early_resp.errors[0].code(), ErrorCode::ResponseTooLarge);
    assert!(
        early_resp.errors[0].path.is_some(),
        "GAP_RESPONSE_TOO_LARGE must report the truncation field path"
    );
    let touched = rows_touched.load(Ordering::SeqCst);
    assert!(
        (1..=10).contains(&touched),
        "incremental size check must stop early without building full 5,000-row body (touched {touched} rows)"
    );

    // Part B: Exact-fit boundary (`== max_response_bytes` PASSES, `max_response_bytes - 1` FAILS, and 1 extra byte at `max_response_bytes` FAILS)!
    let exact_mock = MockConnector::new().on_query(|_req, _scope| {
        Ok(stream_rows(vec![
            json!({ "id": "o-1", "sku": "ABC", "totalCents": 42 }),
            json!({ "id": "o-2", "sku": "XYZ", "totalCents": 99 }),
        ]))
    });
    let (grpc_client, _h) = spawn_grpc_branch(exact_mock, clock.clone());
    let exec_exact = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), grpc_client, 16)
            .build(),
    ))
    .with_clock(clock);

    let baseline = exec_exact
        .execute(ExecRequest::new(
            snapshot.clone(),
            orders_plan.clone(),
            Instant::now() + Duration::from_secs(5),
        ))
        .await;
    let exact_bytes = serde_json::to_vec(baseline.data.as_ref().expect("baseline data"))
        .expect("serialize data")
        .len();

    // Equal to exact_bytes MUST pass:
    let pass_at_cap = exec_exact
        .execute(
            ExecRequest::new(
                snapshot.clone(),
                orders_plan.clone(),
                Instant::now() + Duration::from_secs(5),
            )
            .with_limits(ExecLimits {
                max_response_bytes: exact_bytes,
                ..ExecLimits::default()
            }),
        )
        .await;
    assert!(
        pass_at_cap.errors.is_empty(),
        "response of exact max_response_bytes ({exact_bytes}) must pass, got {:?}",
        pass_at_cap.errors
    );
    assert_eq!(pass_at_cap.data, baseline.data);

    // One byte tighter (`exact_bytes - 1`, i.e. body is 1 byte over cap) MUST fail:
    let fail_one_over = exec_exact
        .execute(
            ExecRequest::new(
                snapshot,
                orders_plan,
                Instant::now() + Duration::from_secs(5),
            )
            .with_limits(ExecLimits {
                max_response_bytes: exact_bytes - 1,
                ..ExecLimits::default()
            }),
        )
        .await;
    assert_eq!(fail_one_over.data, None);
    assert_eq!(fail_one_over.errors.len(), 1);
    assert_eq!(fail_one_over.errors[0].code(), ErrorCode::ResponseTooLarge);
}

// spec: RUN-060, INV-12, INV-15
#[tokio::test(start_paused = true)]
async fn hostile_connectors_endless_stream_and_giant_row_bounded_safely() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    // Hostile 1: Endless stream never sending `Complete`
    let endless_mock = MockConnector::new().on_query(|_req, _scope| {
        let s = stream::unfold(0_usize, |idx| async move {
            if idx >= 2_000 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Some((
                Ok(rows_response(vec![json!({
                    "id": "inf",
                    "sku": "ENDLESS-STREAM",
                    "totalCents": 1
                })])),
                idx + 1,
            ))
        });
        Ok(Box::pin(s))
    });
    let (endless_grpc, _eh) = spawn_grpc_branch(endless_mock.clone(), clock.clone());
    let exec_endless = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), endless_grpc, 8)
            .build(),
    ))
    .with_clock(clock.clone());

    let orders_plan = compile_plan(&snapshot, "{ orders { id sku } }", &serde_json::Map::new());
    let resp_endless = exec_endless
        .execute(
            ExecRequest::new(
                snapshot.clone(),
                orders_plan,
                Instant::now() + Duration::from_secs(10),
            )
            .with_limits(ExecLimits {
                max_response_bytes: 512,
                ..ExecLimits::default()
            }),
        )
        .await;
    assert_eq!(resp_endless.data, None);
    assert_eq!(resp_endless.errors[0].code(), ErrorCode::ResponseTooLarge);
    endless_mock.wait_for_drops(1).await;
    assert_eq!(endless_mock.in_flight(), 0);

    // Hostile 2: Single giant 2 MiB string row against 4 KiB max_response_bytes
    let giant_mock = MockConnector::new().on_query(|_req, _scope| {
        let huge = "G".repeat(2 * 1024 * 1024);
        Ok(stream_rows(vec![json!({ "value": huge })]))
    });
    let exec_giant = ExecLayer::new(Arc::new(
        ConnectorRegistry::builder()
            .register(
                shop_uid(),
                make_in_process_branch(giant_mock, clock.clone()),
                8,
            )
            .build(),
    ))
    .with_clock(clock);

    let ping_plan = compile_plan(&snapshot, "{ ping }", &serde_json::Map::new());
    let resp_giant = exec_giant
        .execute(
            ExecRequest::new(snapshot, ping_plan, Instant::now() + Duration::from_secs(5))
                .with_limits(ExecLimits {
                    max_response_bytes: 4_096,
                    ..ExecLimits::default()
                }),
        )
        .await;
    assert_eq!(resp_giant.data, None);
    assert_eq!(resp_giant.errors[0].code(), ErrorCode::ResponseTooLarge);
    assert_eq!(
        serde_json::to_value(&resp_giant.errors[0].path).unwrap(),
        json!(["ping"])
    );
}
