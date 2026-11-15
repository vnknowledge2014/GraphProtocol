#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gap_core::clock::{Clock, ManualClock};
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::{ErrorCode, GapError};
use gap_core::ids::{TaskId, TenantId};
use gap_core::snapshot::Snapshot;
use gap_store::tasks::{FsArtifactBlobStore, InMemoryTaskStore, TaskStatus};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{
    DelegateStartResult, DelegatedStepClient, GapqlStepRunner, SubmitTaskInput, TaskEngine,
    TaskEngineConfig, TaskFuture, TaskListFilter, TaskStepSpec, TaskStrategy,
};

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

fn make_ctx(tenant_slug: &str, principal_id: &str, clock: &dyn Clock) -> RequestContext {
    let tenant: TenantId = tenant_slug.parse().unwrap();
    RequestContext::try_new(
        TaskId::new_v7().as_uuid(),
        tenant.clone(),
        Principal::new(principal_id, PrincipalKind::Agent, tenant),
        clock.now_instant() + Duration::from_secs(60),
        Budget::new(10_000),
        TraceCtx::generate_root(true),
        0,
        None,
        Arc::new(Snapshot::empty(1)),
    )
    .unwrap()
}

struct RecordingGapqlRunner {
    calls: Arc<Mutex<Vec<(String, String)>>>,
    gate: Option<Arc<Notify>>,
    barrier_count: Option<(Arc<std::sync::atomic::AtomicUsize>, usize, Arc<Notify>)>,
    large_output_bytes: Option<usize>,
}

impl RecordingGapqlRunner {
    fn simple() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            barrier_count: None,
            large_output_bytes: None,
        }
    }
}

impl GapqlStepRunner for RecordingGapqlRunner {
    fn run_gapql<'a>(
        &'a self,
        ctx: &'a RequestContext,
        document: &'a str,
        _variables: &'a serde_json::Value,
        cancel: CancellationToken,
    ) -> TaskFuture<'a, serde_json::Value> {
        let doc = document.to_owned();
        let principal = ctx.principal.id.clone();
        Box::pin(async move {
            if let Some((ref count, target, ref barrier)) = self.barrier_count {
                let prev = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if prev + 1 >= target {
                    barrier.notify_waiters();
                } else {
                    tokio::select! {
                        () = barrier.notified() => {}
                        () = cancel.cancelled() => {
                            return Err(GapError::new(ErrorCode::Cancelled, "cancelled at barrier"));
                        }
                    }
                }
            }
            if let Some(ref gate) = self.gate {
                tokio::select! {
                    () = gate.notified() => {}
                    () = cancel.cancelled() => {
                        return Err(GapError::new(ErrorCode::Cancelled, "cancelled while waiting"));
                    }
                }
            }
            self.calls
                .lock()
                .unwrap()
                .push((principal.clone(), doc.clone()));
            if let Some(bytes) = self.large_output_bytes {
                let payload = "x".repeat(bytes);
                return Ok(serde_json::json!({ "blob": payload }));
            }
            Ok(serde_json::json!({ "doc": doc, "principal": principal }))
        })
    }
}

type StartedRecord = (String, u8, Vec<String>, Option<String>);
type CanceledRecord = (String, String, Option<String>);

struct MockDelegatedClient {
    started: Arc<Mutex<Vec<StartedRecord>>>,
    polls: Arc<Mutex<VecDeque<DelegateStartResult>>>,
    canceled: Arc<Mutex<Vec<CanceledRecord>>>,
    remote_id: String,
    initial_status: TaskStatus,
}

impl Default for MockDelegatedClient {
    fn default() -> Self {
        Self {
            started: Arc::new(Mutex::new(Vec::new())),
            polls: Arc::new(Mutex::new(VecDeque::new())),
            canceled: Arc::new(Mutex::new(Vec::new())),
            remote_id: String::new(),
            initial_status: TaskStatus::Submitted,
        }
    }
}

impl DelegatedStepClient for MockDelegatedClient {
    fn start_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        branch: &'a str,
        operation: &'a str,
        context_id: Option<&'a str>,
        _message: &'a serde_json::Value,
        _expect_artifacts: &'a [String],
        _cancel: CancellationToken,
    ) -> TaskFuture<'a, DelegateStartResult> {
        let trace_path = ctx.trace.path.clone();
        let hop = ctx.hop;
        let branch_s = branch.to_owned();
        let op_s = operation.to_owned();
        let ctx_id = context_id.map(str::to_owned);
        Box::pin(async move {
            self.started.lock().unwrap().push((
                format!("{branch_s}/{op_s}"),
                hop,
                trace_path,
                ctx_id,
            ));
            Ok(DelegateStartResult {
                remote_task_id: self.remote_id.clone(),
                status: self.initial_status,
                result: if self.initial_status == TaskStatus::Completed {
                    Some(serde_json::json!({ "delegated": true }))
                } else {
                    None
                },
                error: None,
                artifacts: Vec::new(),
            })
        })
    }

    fn get_remote_task<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _branch: &'a str,
        _remote_task_id: &'a str,
    ) -> TaskFuture<'a, DelegateStartResult> {
        Box::pin(async move {
            let next = self
                .polls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DelegateStartResult {
                    remote_task_id: self.remote_id.clone(),
                    status: TaskStatus::Working,
                    result: None,
                    error: None,
                    artifacts: Vec::new(),
                });
            Ok(next)
        })
    }

    fn cancel_remote_task<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        branch: &'a str,
        remote_task_id: &'a str,
        reason: Option<&'a str>,
    ) -> TaskFuture<'a, TaskStatus> {
        let branch_s = branch.to_owned();
        let remote_s = remote_task_id.to_owned();
        let reason_s = reason.map(str::to_owned);
        Box::pin(async move {
            self.canceled
                .lock()
                .unwrap()
                .push((branch_s, remote_s, reason_s));
            Ok(TaskStatus::Canceled)
        })
    }
}

// spec: RUN-082
#[tokio::test]
async fn task_strategies_parallel_sequential_dag_and_cycle_rejection() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = Arc::new(InMemoryTaskStore::new());
    let ctx = make_ctx("acme", "agent-alpha", clock.as_ref());

    // 1. PARALLEL runs concurrently (enforced by 2-party barrier) and merges array result.
    let barrier = Arc::new(Notify::new());
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let par_runner = Arc::new(RecordingGapqlRunner {
        calls: Arc::new(Mutex::new(Vec::new())),
        gate: None,
        barrier_count: Some((counter, 2, barrier)),
        large_output_bytes: None,
    });
    let engine = TaskEngine::new(store.clone(), TaskEngineConfig::default())
        .with_clock(clock.clone())
        .with_gapql_runner(par_runner.clone());

    let view = engine
        .submit_and_run(
            &ctx,
            SubmitTaskInput {
                context_id: Some("ctx-par".to_owned()),
                goal: Some("parallel queries".to_owned()),
                strategy: TaskStrategy::Parallel,
                steps: vec![
                    TaskStepSpec::gapql("s1", "{ a }", serde_json::Value::Null),
                    TaskStepSpec::gapql("s2", "{ b }", serde_json::Value::Null),
                ],
                deadline_ms: Some(10_000),
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(view.state, TaskStatus::Completed);
    assert_eq!(view.artifacts.len(), 1);
    let arr = view.artifacts[0].data.as_ref().unwrap().as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["principal"], "agent-alpha");

    // 2. SEQUENTIAL executes in strict order 0 -> 1 -> 2.
    let seq_runner = Arc::new(RecordingGapqlRunner::simple());
    let seq_engine = TaskEngine::new(store.clone(), TaskEngineConfig::default())
        .with_clock(clock.clone())
        .with_gapql_runner(seq_runner.clone());
    let seq_view = seq_engine
        .submit_and_run(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Sequential,
                steps: vec![
                    TaskStepSpec::gapql("one", "q1", serde_json::Value::Null),
                    TaskStepSpec::gapql("two", "q2", serde_json::Value::Null),
                    TaskStepSpec::gapql("three", "q3", serde_json::Value::Null),
                ],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(seq_view.state, TaskStatus::Completed);
    let recorded_docs: Vec<String> = seq_runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, d)| d.clone())
        .collect();
    assert_eq!(recorded_docs, vec!["q1", "q2", "q3"]);

    // 3. Cyclic DAG is rejected at submit time with CycleUnbounded.
    let err = seq_engine
        .gap_submit_task(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Dag,
                steps: vec![
                    TaskStepSpec::gapql("a", "qa", serde_json::Value::Null).with_depends_on(["b"]),
                    TaskStepSpec::gapql("b", "qb", serde_json::Value::Null).with_depends_on(["a"]),
                ],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CycleUnbounded);

    // 4. Acyclic diamond DAG: root -> [left, right] -> join.
    let dag_runner = Arc::new(RecordingGapqlRunner::simple());
    let dag_engine = TaskEngine::new(store, TaskEngineConfig::default())
        .with_clock(clock)
        .with_gapql_runner(dag_runner.clone());
    let dag_view = dag_engine
        .submit_and_run(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: Some("diamond".to_owned()),
                strategy: TaskStrategy::Dag,
                steps: vec![
                    TaskStepSpec::gapql("root", "r", serde_json::Value::Null),
                    TaskStepSpec::gapql("left", "l", serde_json::Value::Null)
                        .with_depends_on(["root"]),
                    TaskStepSpec::gapql("right", "rg", serde_json::Value::Null)
                        .with_depends_on(["root"]),
                    TaskStepSpec::gapql("join", "j", serde_json::Value::Null)
                        .with_depends_on(["left", "right"]),
                ],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(dag_view.state, TaskStatus::Completed);
    let dag_order: Vec<String> = dag_runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, d)| d.clone())
        .collect();
    assert_eq!(dag_order[0], "r");
    assert_eq!(dag_order[3], "j");
}

// spec: RUN-080, INV-13
#[tokio::test]
async fn task_events_replay_follow_and_sse_last_event_id() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = Arc::new(InMemoryTaskStore::new());
    let gate = Arc::new(Notify::new());
    let runner = Arc::new(RecordingGapqlRunner {
        calls: Arc::new(Mutex::new(Vec::new())),
        gate: Some(gate.clone()),
        barrier_count: None,
        large_output_bytes: None,
    });
    let engine = TaskEngine::new(store, TaskEngineConfig::default())
        .with_clock(clock.clone())
        .with_gapql_runner(runner);
    let ctx = make_ctx("acme", "agent-stream", clock.as_ref());

    let submitted = engine
        .gap_submit_task(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::gapql(
                    "step-1",
                    "{ ok }",
                    serde_json::Value::Null,
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap();

    let mut live_stream = engine.gap_task_events(&ctx, submitted.id, 1, None).unwrap();
    // Unblock execution after live subscriber is registered.
    tokio::task::yield_now().await;
    gate.notify_waiters();

    let mut seen_seqs = Vec::new();
    let mut final_state = None;
    while let Some(evt) = tokio::time::timeout(Duration::from_secs(2), live_stream.recv())
        .await
        .unwrap()
    {
        seen_seqs.push(evt.seq);
        final_state = Some(evt.state);
    }
    assert_eq!(final_state, Some(TaskStatus::Completed));
    assert!(seen_seqs.windows(2).all(|w| w[1] > w[0]));
    assert!(seen_seqs.len() >= 3);

    // SSE Last-Event-ID = 2 resumes strictly at seq >= 3.
    let mut replay_only = engine
        .gap_task_events(&ctx, submitted.id, 1, Some("2"))
        .unwrap();
    let mut replayed_seqs = Vec::new();
    while let Some(evt) = replay_only.recv().await {
        replayed_seqs.push(evt.seq);
    }
    assert_eq!(replayed_seqs.first().copied(), Some(3));
    assert_eq!(replayed_seqs.last().copied(), seen_seqs.last().copied());

    let bad = engine.gap_task_events(&ctx, submitted.id, 1, Some("not-a-num"));
    assert_eq!(bad.unwrap_err().code(), ErrorCode::ValidationError);
}

// spec: RUN-083, INV-20
#[tokio::test]
async fn task_deadline_hop_limit_and_trace_loop_detection() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = Arc::new(InMemoryTaskStore::new());
    let delegated = Arc::new(MockDelegatedClient {
        remote_id: "remote-1".to_owned(),
        initial_status: TaskStatus::Completed,
        ..Default::default()
    });
    let engine = TaskEngine::new(
        store,
        TaskEngineConfig {
            max_hops: 2,
            ..Default::default()
        },
    )
    .with_clock(clock.clone())
    .with_delegated_client(delegated.clone());

    // Valid hop advances hop + 1 and appends target branch to trace_path.
    let ctx = make_ctx("acme", "agent-peer", clock.as_ref());
    let view = engine
        .submit_and_run(
            &ctx,
            SubmitTaskInput {
                context_id: Some("conv-42".to_owned()),
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::delegate(
                    "d1",
                    "peer-b",
                    "search",
                    serde_json::json!({"q":"hi"}),
                )],
                deadline_ms: Some(5_000),
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(view.state, TaskStatus::Completed);
    {
        let recorded = delegated.started.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].1, 1);
        assert_eq!(recorded[0].2, vec!["peer-b".to_owned()]);
        assert_eq!(recorded[0].3.as_deref(), Some("conv-42"));
    }

    // Re-visiting peer-b in trace.path triggers LoopDetected (INV-20).
    let mut looped_ctx = ctx.clone();
    looped_ctx.trace.path.push("peer-b".to_owned());
    let loop_view = engine
        .submit_and_run(
            &looped_ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::delegate(
                    "d1",
                    "peer-b",
                    "search",
                    serde_json::json!({}),
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(loop_view.state, TaskStatus::Failed);
    assert_eq!(loop_view.error.as_ref().unwrap().code, "GAP_LOOP_DETECTED");

    // Exceeding max_hops triggers HopLimit (INV-20).
    let mut max_hop_ctx = ctx.clone();
    max_hop_ctx.hop = 2;
    let hop_view = engine
        .submit_and_run(
            &max_hop_ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::delegate(
                    "d1",
                    "peer-c",
                    "search",
                    serde_json::json!({}),
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(hop_view.state, TaskStatus::Failed);
    assert_eq!(hop_view.error.as_ref().unwrap().code, "GAP_HOP_LIMIT");

    // Expired ctx deadline -> DEADLINE_EXCEEDED.
    let mut expired_ctx = ctx;
    expired_ctx.deadline = Instant::now() - Duration::from_secs(1);
    let dl_view = engine
        .submit_and_run(
            &expired_ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::delegate(
                    "d1",
                    "peer-d",
                    "search",
                    serde_json::json!({}),
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(dl_view.state, TaskStatus::Failed);
    assert_eq!(
        dl_view.error.as_ref().unwrap().code,
        "GAP_DEADLINE_EXCEEDED"
    );
}

// spec: RUN-084
#[tokio::test]
async fn task_cancel_propagates_to_delegated_branch_and_is_idempotent() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = Arc::new(InMemoryTaskStore::new());
    let delegated = Arc::new(MockDelegatedClient {
        remote_id: "remote-working-9".to_owned(),
        initial_status: TaskStatus::Working,
        ..Default::default()
    });
    let engine = TaskEngine::new(
        store,
        TaskEngineConfig {
            delegate_poll_interval: Duration::from_millis(20),
            ..Default::default()
        },
    )
    .with_clock(clock.clone())
    .with_delegated_client(delegated.clone());

    let ctx = make_ctx("acme", "agent-cancel", clock.as_ref());
    let submitted = engine
        .gap_submit_task(
            &ctx,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::delegate(
                    "d1",
                    "peer-slow",
                    "longOp",
                    serde_json::json!({}),
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap();

    // Wait until start_task registered the remote task id.
    for _ in 0..50 {
        if !delegated.started.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let canceled1 = engine
        .gap_cancel_task(&ctx, submitted.id, Some("caller aborted"))
        .await
        .unwrap();
    assert_eq!(canceled1.state, TaskStatus::Canceled);

    let canceled_targets = delegated.canceled.lock().unwrap().clone();
    assert!(
        canceled_targets
            .iter()
            .any(|(b, r, _)| b == "peer-slow" && r == "remote-working-9")
    );

    // Second cancel is idempotent.
    let canceled2 = engine
        .gap_cancel_task(&ctx, submitted.id, Some("repeat"))
        .await
        .unwrap();
    assert_eq!(canceled2.state, TaskStatus::Canceled);
}

// spec: RUN-085, RUN-086, INV-09
#[tokio::test]
async fn task_approval_expiry_retention_and_large_artifact_blob_spill() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let dir = TempTestDir::new("exec-blobs");
    let blobs = Arc::new(FsArtifactBlobStore::new(dir.path.join("blobs")).unwrap());
    let store = Arc::new(InMemoryTaskStore::new());

    // 1. Artifact > 1 MiB spills to FsArtifactBlobStore with uri + sha256.
    let large_runner = Arc::new(RecordingGapqlRunner {
        calls: Arc::new(Mutex::new(Vec::new())),
        gate: None,
        barrier_count: None,
        large_output_bytes: Some(gap_store::tasks::INLINE_ARTIFACT_MAX_BYTES + 256),
    });
    let engine = TaskEngine::new(store, TaskEngineConfig::default())
        .with_clock(clock.clone())
        .with_blob_store(blobs)
        .with_gapql_runner(large_runner);
    let ctx_a = make_ctx("tenant-a", "user-1", clock.as_ref());
    let ctx_b = make_ctx("tenant-b", "user-2", clock.as_ref());

    let done = engine
        .submit_and_run(
            &ctx_a,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::gapql(
                    "big",
                    "{ large }",
                    serde_json::Value::Null,
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(done.state, TaskStatus::Completed);
    let art = &done.artifacts[0];
    assert!(art.data.is_none());
    assert!(art.uri.as_deref().unwrap().starts_with("file://"));
    assert_eq!(art.sha256.as_deref().unwrap().len(), 64);

    // Tenant B cannot view or list Tenant A's task (INV-09).
    assert!(engine.gap_task(&ctx_b, done.id).unwrap().is_none());
    assert!(
        engine
            .gap_tasks(&ctx_b, &TaskListFilter::default())
            .unwrap()
            .nodes
            .is_empty()
    );

    // 2. Approval expiry -> REJECTED(reason=approval_expired) after 24 h.
    let gate = Arc::new(Notify::new());
    let paused_engine = TaskEngine::new(
        Arc::new(InMemoryTaskStore::new()),
        TaskEngineConfig::default(),
    )
    .with_clock(clock.clone())
    .with_gapql_runner(Arc::new(RecordingGapqlRunner {
        calls: Arc::new(Mutex::new(Vec::new())),
        gate: Some(gate),
        barrier_count: None,
        large_output_bytes: None,
    }));
    let pending = paused_engine
        .gap_submit_task(
            &ctx_a,
            SubmitTaskInput {
                context_id: None,
                goal: None,
                strategy: TaskStrategy::Single,
                steps: vec![TaskStepSpec::gapql(
                    "mut",
                    "mutation { x }",
                    serde_json::Value::Null,
                )],
                deadline_ms: None,
                idempotency_key: None,
            },
        )
        .unwrap();
    for _ in 0..50 {
        if paused_engine
            .gap_task(&ctx_a, pending.id)
            .unwrap()
            .unwrap()
            .state
            == TaskStatus::Working
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    paused_engine
        .pause_for_approval(
            &ctx_a,
            pending.id,
            super::PendingApprovalView {
                id: "appr-1".to_owned(),
                action: "deleteCustomer".to_owned(),
                summary: "deleteCustomer(id=42)".to_owned(),
                side_effect: "IRREVERSIBLE".to_owned(),
                expires_at: gap_core::clock::Timestamp::from_unix_millis(
                    clock.now_timestamp().unix_millis() + gap_store::tasks::DEFAULT_APPROVAL_TTL_MS,
                )
                .unwrap(),
            },
        )
        .unwrap();
    assert!(
        paused_engine
            .expire_stale_approvals(&ctx_a.tenant_scope())
            .unwrap()
            .is_empty()
    );

    clock.advance(Duration::from_millis(
        u64::try_from(gap_store::tasks::DEFAULT_APPROVAL_TTL_MS).unwrap() + 1,
    ));
    let expired = paused_engine
        .expire_stale_approvals(&ctx_a.tenant_scope())
        .unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].state, TaskStatus::Rejected);
    assert_eq!(
        expired[0].error.as_ref().and_then(|e| e.reason.as_deref()),
        Some("approval_expired")
    );

    // 3. Retention pruning removes terminal task after 30 d (RUN-086).
    clock.advance(Duration::from_millis(
        u64::try_from(gap_store::tasks::DEFAULT_TASK_RETENTION_MS).unwrap() + 1,
    ));
    assert_eq!(
        engine.prune_retained_tasks(&ctx_a.tenant_scope()).unwrap(),
        1
    );
    assert!(engine.gap_task(&ctx_a, done.id).unwrap().is_none());
}
