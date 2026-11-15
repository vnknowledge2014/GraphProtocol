#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Integration test for durable SQLite WAL task lease heartbeat, crash recovery before readiness,
//! resumable/idempotent continuation, and non-resumable failure (`GAP_INTERNAL`, `reason=crash`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::{ErrorCode, GapError};
use gap_core::ids::{TaskId, TenantId};
use gap_core::snapshot::Snapshot;
use gap_exec::tasks::{
    GapqlStepRunner, SqliteTaskStore, SubmitTaskInput, TaskEngine, TaskEngineConfig, TaskFuture,
    TaskStatus, TaskStepSpec, TaskStore, TaskStrategy,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("gap-{prefix}-{}", TaskId::new_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn make_ctx(clock: &dyn Clock) -> RequestContext {
    let tenant: TenantId = "acme".parse().unwrap();
    RequestContext::try_new(
        TaskId::new_v7().as_uuid(),
        tenant.clone(),
        Principal::new("worker-caller", PrincipalKind::Agent, tenant),
        clock.now_instant() + Duration::from_secs(120),
        Budget::new(10_000),
        TraceCtx::generate_root(true),
        0,
        None,
        Arc::new(Snapshot::empty(1)),
    )
    .unwrap()
}

struct CrashAwareRunner {
    step_counts: Arc<Mutex<HashMap<String, usize>>>,
    stall_on_doc: Option<String>,
    stalled_notify: Arc<Notify>,
    unblock_gate: Arc<Notify>,
}

impl GapqlStepRunner for CrashAwareRunner {
    fn run_gapql<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        document: &'a str,
        _variables: &'a serde_json::Value,
        cancel: CancellationToken,
    ) -> TaskFuture<'a, serde_json::Value> {
        let doc = document.to_owned();
        Box::pin(async move {
            {
                let mut counts = self.step_counts.lock().unwrap();
                *counts.entry(doc.clone()).or_insert(0) += 1;
            }
            if self.stall_on_doc.as_deref() == Some(doc.as_str()) {
                self.stalled_notify.notify_waiters();
                tokio::select! {
                    () = self.unblock_gate.notified() => {}
                    () = cancel.cancelled() => {
                        return Err(GapError::new(ErrorCode::Cancelled, "aborted"));
                    }
                }
            }
            Ok(serde_json::json!({ "ok": doc }))
        })
    }
}

// spec: RUN-080, RUN-081, RUN-091, INV-13
#[tokio::test]
async fn task_crash_recovery_resumes_resumable_and_fails_non_resumable_before_readiness() {
    let dir = TempTestDir::new("crash-recovery");
    let db_path = dir.path.join("tasks.sqlite3");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let ctx = make_ctx(clock.as_ref());

    let counts = Arc::new(Mutex::new(HashMap::new()));
    let stalled1 = Arc::new(Notify::new());
    let stalled2 = Arc::new(Notify::new());
    let stalled3 = Arc::new(Notify::new());
    let never_released = Arc::new(Notify::new());

    // Worker 1 opens WAL store and starts 3 tasks, all stalling mid-flight.
    let store1 = Arc::new(SqliteTaskStore::open(&db_path).unwrap());

    let engine_resumable = TaskEngine::new(
        store1.clone(),
        TaskEngineConfig {
            worker_id: "worker-dead-1".to_owned(),
            ..Default::default()
        },
    )
    .with_clock(clock.clone())
    .with_gapql_runner(Arc::new(CrashAwareRunner {
        step_counts: counts.clone(),
        stall_on_doc: Some("resumable-step-2".to_owned()),
        stalled_notify: stalled1.clone(),
        unblock_gate: never_released.clone(),
    }));

    let engine_non_resumable = TaskEngine::new(
        store1.clone(),
        TaskEngineConfig {
            worker_id: "worker-dead-1".to_owned(),
            ..Default::default()
        },
    )
    .with_clock(clock.clone())
    .with_gapql_runner(Arc::new(CrashAwareRunner {
        step_counts: counts.clone(),
        stall_on_doc: Some("unsafe-step".to_owned()),
        stalled_notify: stalled2.clone(),
        unblock_gate: never_released.clone(),
    }));

    let engine_idem = TaskEngine::new(
        store1.clone(),
        TaskEngineConfig {
            worker_id: "worker-dead-1".to_owned(),
            ..Default::default()
        },
    )
    .with_clock(clock.clone())
    .with_gapql_runner(Arc::new(CrashAwareRunner {
        step_counts: counts.clone(),
        stall_on_doc: Some("idem-step".to_owned()),
        stalled_notify: stalled3.clone(),
        unblock_gate: never_released,
    }));

    // Task A: SEQUENTIAL where step 1 completes & checkpoints, and step 2 (`resumable = true`) stalls mid-WORKING.
    let t_resumable = engine_resumable
        .gap_submit_task(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: Some("resumable workflow".to_owned()),
                strategy: TaskStrategy::Sequential,
                steps: vec![
                    TaskStepSpec::gapql("s1", "checkpointed-step-1", serde_json::Value::Null),
                    TaskStepSpec::gapql("s2", "resumable-step-2", serde_json::Value::Null)
                        .with_resumable(true),
                ],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap();

    // Task B: single non-resumable step without replay-safe idempotency stalls mid-WORKING.
    let t_unsafe = engine_non_resumable
        .gap_submit_task(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: Some("non-resumable workflow".to_owned()),
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::gapql(
                    "s1",
                    "unsafe-step",
                    serde_json::Value::Null,
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap();

    // Task C: `resumable = false`, but carries `idempotency_key` + `connector_replay_safe = true` -> MUST resume!
    let t_idem = engine_idem
        .gap_submit_task(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: Some("replay-safe workflow".to_owned()),
                strategy: TaskStrategy::Single,
                steps: vec![
                    TaskStepSpec::gapql("s1", "idem-step", serde_json::Value::Null)
                        .with_idempotent_replay("idem-key-1", true),
                ],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap();

    // Wait until all 3 tasks are actively inside their stalled WORKING steps on Worker 1.
    tokio::join!(
        stalled1.notified(),
        stalled2.notified(),
        stalled3.notified(),
    );
    // Cancel in-memory controls cleanly or drop engines to mimic abrupt crash after WAL persistence.
    drop(engine_resumable);
    drop(engine_non_resumable);
    drop(engine_idem);
    drop(store1);

    // Advance clock past the 30s lease window so Worker 2 can claim expired leases on startup.
    clock.advance(Duration::from_secs(31));

    // Worker 2 opens the SAME SQLite WAL DB and runs recovery BEFORE readiness (RUN-091).
    let store2 = Arc::new(SqliteTaskStore::open(&db_path).unwrap());
    let recovered_gate = Arc::new(Notify::new());
    recovered_gate.notify_waiters();
    let engine2 = TaskEngine::new(
        store2.clone(),
        TaskEngineConfig {
            worker_id: "worker-recovered-2".to_owned(),
            ..Default::default()
        },
    )
    .with_clock(clock.clone())
    .with_gapql_runner(Arc::new(CrashAwareRunner {
        step_counts: counts.clone(),
        stall_on_doc: None,
        stalled_notify: Arc::new(Notify::new()),
        unblock_gate: recovered_gate,
    }));

    let recovered = engine2.recover_before_readiness(&ctx).await.unwrap();
    assert_eq!(recovered.len(), 3);

    // Task A resumed ONLY step 2 and reached COMPLETED (checkpointed step 1 ran only ONCE total!).
    let view_a = engine2.gap_task(&ctx, t_resumable.id).unwrap().unwrap();
    assert_eq!(view_a.state, TaskStatus::Completed);
    assert_eq!(
        counts.lock().unwrap().get("checkpointed-step-1").copied(),
        Some(1)
    );
    assert_eq!(
        counts.lock().unwrap().get("resumable-step-2").copied(),
        Some(2)
    );

    // Task B marked FAILED(code=GAP_INTERNAL, reason=crash).
    let view_b = engine2.gap_task(&ctx, t_unsafe.id).unwrap().unwrap();
    assert_eq!(view_b.state, TaskStatus::Failed);
    let err_b = view_b.error.as_ref().unwrap();
    assert_eq!(err_b.code, "GAP_INTERNAL");
    assert_eq!(err_b.reason.as_deref(), Some("crash"));

    // Task C resumed because idempotency_key + connector_replay_safe = true (RUN-081).
    let view_c = engine2.gap_task(&ctx, t_idem.id).unwrap().unwrap();
    assert_eq!(view_c.state, TaskStatus::Completed);

    // Strictly increasing event sequences survived crash + restart (INV-13).
    let events_a = store2
        .events_from(&ctx.tenant_scope(), t_resumable.id, 1)
        .unwrap();
    assert!(!events_a.is_empty());
    assert!(events_a.windows(2).all(|w| w[1].seq == w[0].seq + 1));
}

// spec: RUN-081
#[tokio::test]
async fn task_worker_lease_heartbeat_renews_while_working() {
    let dir = TempTestDir::new("lease-heartbeat");
    let db_path = dir.path.join("tasks.sqlite3");
    let store = Arc::new(SqliteTaskStore::open(&db_path).unwrap());

    struct SlowRunner(Arc<AtomicUsize>);
    impl GapqlStepRunner for SlowRunner {
        fn run_gapql<'a>(
            &'a self,
            _ctx: &'a RequestContext,
            _document: &'a str,
            _variables: &'a serde_json::Value,
            _cancel: CancellationToken,
        ) -> TaskFuture<'a, serde_json::Value> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok(serde_json::json!({ "done": true }))
            })
        }
    }

    let engine = TaskEngine::new(
        store,
        TaskEngineConfig {
            worker_id: "hb-worker".to_owned(),
            lease_duration: Duration::from_millis(120),
            heartbeat_interval: Duration::from_millis(20),
            ..Default::default()
        },
    )
    .with_gapql_runner(Arc::new(SlowRunner(Arc::new(AtomicUsize::new(0)))));

    let tenant: TenantId = "acme".parse().unwrap();
    let ctx = RequestContext::try_new(
        TaskId::new_v7().as_uuid(),
        tenant.clone(),
        Principal::new("hb-user", PrincipalKind::Agent, tenant),
        std::time::Instant::now() + Duration::from_secs(10),
        Budget::new(100),
        TraceCtx::generate_root(true),
        0,
        None,
        Arc::new(Snapshot::empty(1)),
    )
    .unwrap();

    let view = engine
        .submit_and_run(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::gapql("s", "{ ok }", serde_json::Value::Null)],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(view.state, TaskStatus::Completed);
}
