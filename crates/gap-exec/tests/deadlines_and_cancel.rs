//! `RUN-030` deadline propagation (5% margin, 50 ms floor, branch `timeout_ms` cap, expired
//! deadline short-circuit) and `RUN-031` / `INV-12` paused-time stream cancellation within 50 ms.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream;
use gap_core::clock::{Clock as _, ManualClock};
use gap_core::error::ErrorCode;
use gap_core::snapshot::BranchLimits;
use gap_exec::{BranchClient, ConnectorRegistry, ExecLayer, ExecRequest};
use gap_ngc_sdk::connector::{BoxStream, CallScope, NgcResult};
use gap_ngc_sdk::mock::MockConnector;
use gap_proto::ngc::v1::{QueryRequest, QueryResponse};
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use common::{
    build_two_branch_snapshot, build_two_branch_snapshot_with_limits, compile_plan, crm_uid,
    make_in_process_branch, rows_response, shop_uid, spawn_grpc_branch, stream_rows,
};

struct ObservingClient {
    inner: Arc<dyn BranchClient>,
    recorded: Arc<Mutex<Vec<(i64, Instant)>>>,
}

impl BranchClient for ObservingClient {
    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>> {
        let deadline_unix_ms = request.ctx.as_ref().map_or(0, |c| c.deadline_unix_ms);
        self.recorded
            .lock()
            .expect("test mutex")
            .push((deadline_unix_ms, scope.deadline));
        self.inner.query(request, scope)
    }
}

// spec: RUN-030, INV-12, NGC-025
#[tokio::test(start_paused = true)]
async fn run_030_deadline_reaches_connector_for_three_remaining_time_cases() {
    let base_unix_ms: u64 = 1_700_000_000_000;
    let clock = Arc::new(ManualClock::new(base_unix_ms));

    let shop_limits = BranchLimits {
        max_batch_size: 100,
        max_in_flight: 32,
        timeout_ms: 5_000,
        max_response_bytes: 4 * 1024 * 1024,
    };
    let (snapshot, _) =
        build_two_branch_snapshot_with_limits(shop_limits, BranchLimits::default(), 1);

    let connector_observed = Arc::new(Mutex::new(Vec::<(i64, Instant)>::new()));
    let co = Arc::clone(&connector_observed);
    let shop_mock = MockConnector::new().on_query(move |req, scope| {
        let dl_ms = req.ctx.as_ref().map_or(0, |c| c.deadline_unix_ms);
        co.lock().expect("test mutex").push((dl_ms, scope.deadline));
        Ok(stream_rows(vec![json!({ "value": "pong" })]))
    });

    let (grpc_client, _handle) = spawn_grpc_branch(shop_mock, clock.clone());
    let client_observed = Arc::new(Mutex::new(Vec::<(i64, Instant)>::new()));
    let observing = Arc::new(ObservingClient {
        inner: grpc_client,
        recorded: Arc::clone(&client_observed),
    });

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), observing, 32)
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock.clone());
    let plan = compile_plan(&snapshot, "{ ping }", &serde_json::Map::new());

    // Case 1: remaining = 2,000 ms -> 5% margin = 100 ms (> 50 ms floor) -> call budget = 1,900 ms
    //Case 2: remaining = 400 ms   -> 5% margin = 20 ms (< 50 ms floor)  -> call budget = 350 ms
    // Case 3: remaining = 20,000 ms -> 5% margin = 1,000 ms -> 19,000 ms capped by branch timeout_ms = 5,000 ms -> 5,000 ms
    let cases = [
        (Duration::from_millis(2_000), Duration::from_millis(1_900)),
        (Duration::from_millis(400), Duration::from_millis(350)),
        (Duration::from_millis(20_000), Duration::from_millis(5_000)),
    ];

    for (idx, (remaining, expected_budget)) in cases.into_iter().enumerate() {
        let t0_instant = Instant::now();
        let t0_unix_ms = clock.unix_millis();
        let expected_unix_ms =
            t0_unix_ms + i64::try_from(expected_budget.as_millis()).expect("budget i64");
        let expected_deadline_instant = t0_instant + expected_budget;

        let resp = exec
            .execute(ExecRequest::new(
                snapshot.clone(),
                plan.clone(),
                t0_instant + remaining,
            ))
            .await;

        assert!(
            resp.errors.is_empty(),
            "case {idx} failed with errors: {:?}",
            resp.errors
        );

        let (client_ms, client_inst) = client_observed.lock().expect("mutex")[idx];
        assert_eq!(
            client_ms, expected_unix_ms,
            "case {idx}: BranchClient deadline_unix_ms mismatch"
        );
        assert_eq!(
            client_inst, expected_deadline_instant,
            "case {idx}: BranchClient CallScope.deadline mismatch"
        );

        let (conn_ms, conn_inst) = connector_observed.lock().expect("mutex")[idx];
        assert_eq!(
            conn_ms, expected_unix_ms,
            "case {idx}: Connector CallContext.deadline_unix_ms mismatch"
        );
        assert_eq!(
            conn_inst, expected_deadline_instant,
            "case {idx}: Connector CallScope.deadline mismatch"
        );
    }
}

// spec: RUN-030, INV-12
#[tokio::test(start_paused = true)]
async fn run_030_expired_deadline_never_calls_connector() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();
    let shop_mock = MockConnector::new().on_query(|_req, _scope| {
        Ok(stream_rows(vec![json!({ "value": "should-never-run" })]))
    });

    let (grpc_client, _handle) = spawn_grpc_branch(shop_mock.clone(), clock.clone());
    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), grpc_client, 16)
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);
    let plan = compile_plan(&snapshot, "{ ping }", &serde_json::Map::new());

    // Subcase A: already at/past `now`
    let resp_past = exec
        .execute(ExecRequest::new(
            snapshot.clone(),
            plan.clone(),
            Instant::now(),
        ))
        .await;
    assert_eq!(resp_past.data, None);
    assert_eq!(resp_past.errors.len(), 1);
    assert_eq!(resp_past.errors[0].code(), ErrorCode::DeadlineExceeded);
    assert_eq!(shop_mock.total_calls(), 0);

    // Subcase B: remaining = 30 ms <= 50 ms margin_floor -> net call budget <= 0 -> expired!
    let resp_within_margin = exec
        .execute(ExecRequest::new(
            snapshot,
            plan,
            Instant::now() + Duration::from_millis(30),
        ))
        .await;
    assert_eq!(resp_within_margin.data, None);
    assert_eq!(resp_within_margin.errors.len(), 1);
    assert_eq!(
        resp_within_margin.errors[0].code(),
        ErrorCode::DeadlineExceeded
    );
    assert_eq!(
        shop_mock.total_calls(),
        0,
        "connector must NEVER be called when request.deadline - margin <= now"
    );
}

// spec: RUN-031, INV-12, NGC-025
#[tokio::test(start_paused = true)]
async fn run_031_request_cancel_drops_in_flight_grpc_stream_within_50ms() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();
    let entered = Arc::new(Notify::new());
    let entered_sig = Arc::clone(&entered);

    let shop_mock = MockConnector::new().on_query(move |_req, _scope| {
        entered_sig.notify_waiters();
        let first = stream::once(async {
            Ok(rows_response(vec![json!({
                "id": "o-1",
                "sku": "A",
                "totalCents": 100
            })]))
        });
        let hang = stream::pending();
        Ok(Box::pin(futures_util::StreamExt::chain(first, hang)))
    });

    let (grpc_client, _handle) = spawn_grpc_branch(shop_mock.clone(), clock.clone());
    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), grpc_client, 16)
            .build(),
    );
    let exec = ExecLayer::new(registry).with_clock(clock);
    let plan = compile_plan(&snapshot, "{ orders { id sku } }", &serde_json::Map::new());

    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    let req = ExecRequest::new(snapshot, plan, Instant::now() + Duration::from_secs(30))
        .with_cancel(cancel);

    let exec_task = tokio::spawn(async move { exec.execute(req).await });

    entered.notified().await;
    // Allow first chunk to flow while stream remains open (`in_flight == 1`).
    tokio::task::yield_now().await;
    assert_eq!(shop_mock.in_flight(), 1);

    let cancel_t0 = Instant::now();
    cancel_trigger.cancel();

    let resp = tokio::time::timeout(Duration::from_millis(50), exec_task)
        .await
        .expect("ExecLayer must finish within 50 ms of cancel")
        .expect("join exec_task");

    assert_eq!(resp.data, None);
    assert!(!resp.errors.is_empty());
    assert_eq!(resp.errors[0].code(), ErrorCode::Cancelled);

    tokio::time::timeout(Duration::from_millis(50), shop_mock.wait_for_drops(1))
        .await
        .expect("connector stream drop guard must fire within 50 ms of cancel");

    assert!(
        cancel_t0.elapsed() <= Duration::from_millis(50),
        "cancellation propagation took {:?} (> 50 ms)",
        cancel_t0.elapsed()
    );
    assert_eq!(shop_mock.dropped_count(), 1);
    assert_eq!(
        shop_mock.in_flight(),
        0,
        "in-flight count must return to 0 after cancel"
    );
}

// spec: RUN-031, INV-12, NGC-025
#[tokio::test(start_paused = true)]
async fn run_031_caller_dropping_executor_future_drops_streams_and_leaves_zero_in_flight() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (snapshot, _) = build_two_branch_snapshot();

    let shop_ready = Arc::new(Notify::new());
    let crm_ready = Arc::new(Notify::new());

    let sr = Arc::clone(&shop_ready);
    let shop_mock = MockConnector::new().on_query(move |_req, _scope| {
        sr.notify_waiters();
        Ok(Box::pin(stream::pending()))
    });

    let cr = Arc::clone(&crm_ready);
    let crm_mock = MockConnector::new().on_query(move |_req, _scope| {
        cr.notify_waiters();
        Ok(Box::pin(stream::pending()))
    });

    let (shop_grpc, _sh) = spawn_grpc_branch(shop_mock.clone(), clock.clone());
    let crm_inproc = make_in_process_branch(crm_mock.clone(), clock.clone());

    let registry = Arc::new(
        ConnectorRegistry::builder()
            .register(shop_uid(), shop_grpc, 8)
            .register(crm_uid(), crm_inproc, 8)
            .build(),
    );
    let exec = ExecLayer::new(registry.clone()).with_clock(clock);
    let plan = compile_plan(
        &snapshot,
        r#"{ orders { id } leads { id } }"#,
        &serde_json::Map::new(),
    );

    let mut fut = Box::pin(exec.execute(ExecRequest::new(
        snapshot,
        plan,
        Instant::now() + Duration::from_secs(30),
    )));

    // Drive `fut` until both parallel streams are open and in flight.
    tokio::select! {
        _ = &mut fut => panic!("pending streams must not complete"),
        _ = async {
            tokio::join!(shop_ready.notified(), crm_ready.notified());
        } => {}
    }
    tokio::task::yield_now().await;
    assert_eq!(shop_mock.in_flight(), 1);
    assert_eq!(crm_mock.in_flight(), 1);

    // Caller drops the executor future directly!
    let drop_t0 = Instant::now();
    drop(fut);

    tokio::time::timeout(Duration::from_millis(50), async {
        shop_mock.wait_for_drops(1).await;
        crm_mock.wait_for_drops(1).await;
    })
    .await
    .expect("both gRPC and InProcess streams must drop within 50 ms when caller drops ExecLayer future");

    assert!(drop_t0.elapsed() <= Duration::from_millis(50));
    assert_eq!(shop_mock.in_flight(), 0);
    assert_eq!(crm_mock.in_flight(), 0);
    assert_eq!(registry.available_permits(&shop_uid()), Some(8));
    assert_eq!(registry.available_permits(&crm_uid()), Some(8));
}
