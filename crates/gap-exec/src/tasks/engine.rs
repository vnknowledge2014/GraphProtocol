//! Durable task execution engine, lease/heartbeat manager, strategy coordinator, cancellation
//! propagator, and startup crash recovery (`RUN-080`..`RUN-086`, `RUN-091`, `INV-02`, `INV-13`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::context::RequestContext;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{TaskId, TenantScope};
use gap_store::StoreError;
use gap_store::tasks::{
    DEFAULT_APPROVAL_TTL_MS, DEFAULT_TASK_HEARTBEAT_SECS, DEFAULT_TASK_LEASE_SECS,
    DEFAULT_TASK_RETENTION_MS, FsArtifactBlobStore, TaskEventRecord, TaskQueryFilter, TaskRecord,
    TaskStatus, TaskStore, TaskTransitionRequest,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::dag::{advance_delegated_hop, validate_submission_steps};
use super::events::{TaskEventHub, TaskEventStream, parse_sse_last_event_id};
use super::types::{
    PendingApprovalView, SubmitTaskInput, TaskArtifactView, TaskConnectionView, TaskErrorView,
    TaskEventPayload, TaskEventView, TaskStatePayload, TaskStepKind, TaskStepSnapshot,
    TaskStepSpec, TaskStrategy, TaskView,
};

/// Boxed future alias for async task step traits.
pub type TaskFuture<'a, T> = Pin<Box<dyn Future<Output = GapResult<T>> + Send + 'a>>;

/// Executes a `Gapql` task step through the normal GAP execution pipeline as the submitting
/// [`Principal`](gap_core::context::Principal) (`RUN-082`).
pub trait GapqlStepRunner: Send + Sync {
    /// Runs `document` with `variables` under `ctx` and cancellation token `cancel`.
    fn run_gapql<'a>(
        &'a self,
        ctx: &'a RequestContext,
        document: &'a str,
        variables: &'a serde_json::Value,
        cancel: CancellationToken,
    ) -> TaskFuture<'a, serde_json::Value>;
}

/// No-op [`GapqlStepRunner`] failing with [`ErrorCode::UnsupportedFeature`] when no GAP-QL runner
/// is wired.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopGapqlStepRunner;

impl GapqlStepRunner for NoopGapqlStepRunner {
    fn run_gapql<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _document: &'a str,
        _variables: &'a serde_json::Value,
        _cancel: CancellationToken,
    ) -> TaskFuture<'a, serde_json::Value> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "no GAP-QL step runner configured for TaskEngine",
            ))
        })
    }
}

/// Result returned by a remote branch `StartTask` / `GetTask` poll (`RUN-082`, `NGC-041`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateStartResult {
    /// Remote task identifier assigned by the target branch.
    pub remote_task_id: String,
    /// Current remote task FSM status.
    pub status: TaskStatus,
    /// Optional structured output when `Completed`.
    pub result: Option<serde_json::Value>,
    /// Optional error when `Failed` / `Rejected` / `Canceled`.
    pub error: Option<TaskErrorView>,
    /// Optional remote artifacts.
    pub artifacts: Vec<TaskArtifactView>,
}

/// Outbound branch delegation client used by `TaskStepKind::Delegate` (`RUN-082`, `RUN-083`, `RUN-084`).
pub trait DelegatedStepClient: Send + Sync {
    /// Starts a delegated remote task on `branch` carrying hopped `ctx`, `context_id`, and `message`.
    #[allow(clippy::too_many_arguments)]
    fn start_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        branch: &'a str,
        operation: &'a str,
        context_id: Option<&'a str>,
        message: &'a serde_json::Value,
        expect_artifacts: &'a [String],
        cancel: CancellationToken,
    ) -> TaskFuture<'a, DelegateStartResult>;

    /// Polls a non-terminal remote task (`RUN-082`, `CMP-022`).
    fn get_remote_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        branch: &'a str,
        remote_task_id: &'a str,
    ) -> TaskFuture<'a, DelegateStartResult>;

    /// Propagates `CancelTask` to an active delegated remote task (`RUN-084`).
    fn cancel_remote_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        branch: &'a str,
        remote_task_id: &'a str,
        reason: Option<&'a str>,
    ) -> TaskFuture<'a, TaskStatus>;
}

/// No-op [`DelegatedStepClient`] returning [`ErrorCode::UnsupportedFeature`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopDelegatedStepClient;

impl DelegatedStepClient for NoopDelegatedStepClient {
    fn start_task<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _branch: &'a str,
        _operation: &'a str,
        _context_id: Option<&'a str>,
        _message: &'a serde_json::Value,
        _expect_artifacts: &'a [String],
        _cancel: CancellationToken,
    ) -> TaskFuture<'a, DelegateStartResult> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "no delegated step client configured for TaskEngine",
            ))
        })
    }

    fn get_remote_task<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _branch: &'a str,
        _remote_task_id: &'a str,
    ) -> TaskFuture<'a, DelegateStartResult> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "no delegated step client configured for TaskEngine",
            ))
        })
    }

    fn cancel_remote_task<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _branch: &'a str,
        _remote_task_id: &'a str,
        _reason: Option<&'a str>,
    ) -> TaskFuture<'a, TaskStatus> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "no delegated step client configured for TaskEngine",
            ))
        })
    }
}

/// Runtime policy and lease timing configuration for [`TaskEngine`] (`RUN-081`..`RUN-086`).
#[derive(Debug, Clone)]
pub struct TaskEngineConfig {
    /// Stable worker identifier stamped onto task leases.
    pub worker_id: String,
    /// Lease duration on a `WORKING` task (`30 s` default, `RUN-081`).
    pub lease_duration: Duration,
    /// Lease heartbeat renewal interval (`10 s` default, `RUN-081`).
    pub heartbeat_interval: Duration,
    /// Approval expiration window in milliseconds (`24 h` default, `RUN-085`).
    pub approval_ttl_ms: i64,
    /// Terminal task retention window in milliseconds (`30 d` default, `RUN-086`).
    pub retention_ms: i64,
    /// Maximum allowed peer hops (`INV-20`).
    pub max_hops: u8,
    /// Maximum allowed client deadline window in milliseconds (`RUN-030`, `RUN-083`).
    pub max_deadline_ms: u64,
    /// Poll interval while mirroring a non-terminal delegated remote task (`RUN-082`).
    pub delegate_poll_interval: Duration,
}

impl Default for TaskEngineConfig {
    fn default() -> Self {
        Self {
            worker_id: "gap-worker-1".to_owned(),
            lease_duration: Duration::from_secs(
                u64::try_from(DEFAULT_TASK_LEASE_SECS).unwrap_or(30),
            ),
            heartbeat_interval: Duration::from_secs(
                u64::try_from(DEFAULT_TASK_HEARTBEAT_SECS).unwrap_or(10),
            ),
            approval_ttl_ms: DEFAULT_APPROVAL_TTL_MS,
            retention_ms: DEFAULT_TASK_RETENTION_MS,
            max_hops: gap_core::context::DEFAULT_MAX_HOPS,
            max_deadline_ms: 300_000,
            delegate_poll_interval: Duration::from_millis(25),
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveTaskControl {
    cancel: CancellationToken,
    delegated: Vec<(String, String)>,
}

/// Filter options for [`TaskEngine::gap_tasks`].
#[derive(Debug, Clone, Default)]
pub struct TaskListFilter {
    /// Optional subset of states to include.
    pub states: Option<Vec<TaskStatus>>,
    /// Optional context identifier filter.
    pub context_id: Option<String>,
    /// Maximum number of tasks to return (default `20`, capped at `200`).
    pub first: Option<usize>,
    /// Opaque pagination cursor from a previous [`TaskConnectionView::end_cursor`].
    pub after: Option<String>,
}

/// Coordinates durable task persistence, step strategies, heartbeats, cancellation, SSE replay,
/// and startup crash recovery (`RUN-080`..`RUN-086`, `RUN-091`).
#[derive(Clone)]
pub struct TaskEngine {
    store: Arc<dyn TaskStore>,
    blobs: Option<Arc<FsArtifactBlobStore>>,
    clock: Arc<dyn Clock>,
    gapql_runner: Arc<dyn GapqlStepRunner>,
    delegated_client: Arc<dyn DelegatedStepClient>,
    hub: TaskEventHub,
    config: TaskEngineConfig,
    controls: Arc<Mutex<HashMap<(String, TaskId), ActiveTaskControl>>>,
}

impl std::fmt::Debug for TaskEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskEngine")
            .field("config", &self.config)
            .field("has_blob_store", &self.blobs.is_some())
            .finish_non_exhaustive()
    }
}

impl TaskEngine {
    /// Constructs a [`TaskEngine`] with default [`SystemClock`] and no-op step runners.
    #[must_use]
    pub fn new(store: Arc<dyn TaskStore>, config: TaskEngineConfig) -> Self {
        Self {
            store,
            blobs: None,
            clock: Arc::new(SystemClock),
            gapql_runner: Arc::new(NoopGapqlStepRunner),
            delegated_client: Arc::new(NoopDelegatedStepClient),
            hub: TaskEventHub::new(),
            config,
            controls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attaches a filesystem artifact blob store for `> 1 MiB` offloading (`RUN-086`).
    #[must_use]
    pub fn with_blob_store(mut self, blobs: Arc<FsArtifactBlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Injects a custom [`Clock`] implementation (e.g. `ManualClock` for deterministic lease/TTL tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Injects the [`GapqlStepRunner`] used for `Gapql` steps (`RUN-082`).
    #[must_use]
    pub fn with_gapql_runner(mut self, runner: Arc<dyn GapqlStepRunner>) -> Self {
        self.gapql_runner = runner;
        self
    }

    /// Injects the [`DelegatedStepClient`] used for `Delegate` steps (`RUN-082`, `RUN-084`).
    #[must_use]
    pub fn with_delegated_client(mut self, client: Arc<dyn DelegatedStepClient>) -> Self {
        self.delegated_client = client;
        self
    }

    /// Returns a reference to the underlying [`TaskStore`].
    #[must_use]
    pub fn store(&self) -> &dyn TaskStore {
        self.store.as_ref()
    }

    fn lock_controls(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<(String, TaskId), ActiveTaskControl>> {
        match self.controls.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn register_control(&self, tenant: &TenantScope, task_id: TaskId) -> CancellationToken {
        let token = CancellationToken::new();
        let mut guard = self.lock_controls();
        guard.insert(
            (tenant.tenant().as_str().to_owned(), task_id),
            ActiveTaskControl {
                cancel: token.clone(),
                delegated: Vec::new(),
            },
        );
        token
    }

    fn record_delegated_target(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        branch: String,
        remote_task_id: String,
    ) {
        let mut guard = self.lock_controls();
        if let Some(ctrl) = guard.get_mut(&(tenant.tenant().as_str().to_owned(), task_id))
            && !ctrl
                .delegated
                .iter()
                .any(|(b, r)| b == &branch && r == &remote_task_id)
        {
            ctrl.delegated.push((branch, remote_task_id));
        }
    }

    fn clear_control(&self, tenant: &TenantScope, task_id: TaskId) {
        let mut guard = self.lock_controls();
        guard.remove(&(tenant.tenant().as_str().to_owned(), task_id));
    }

    fn lease_expires_at(&self, now: Timestamp) -> Timestamp {
        let lease_ms = i64::try_from(self.config.lease_duration.as_millis()).unwrap_or(30_000);
        add_millis_clamped(now, lease_ms)
    }

    /// Validates, persists (`SUBMITTED`), and spawns asynchronous execution for `input`
    /// (`gap.submitTask`, `RUN-080`, `RUN-082`, `RUN-083`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::CycleUnbounded`] if a `DAG` plan contains a cycle, or
    /// [`ErrorCode::ValidationError`] / [`ErrorCode::Internal`] on invalid input or store failure.
    pub fn gap_submit_task(
        &self,
        ctx: &RequestContext,
        input: SubmitTaskInput,
    ) -> GapResult<TaskView> {
        let (task_view, task_id, waves, payload) = self.persist_submitted_task(ctx, input)?;
        let engine = self.clone();
        let owned_ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = engine
                .run_task_from_state(&owned_ctx, task_id, payload, waves)
                .await;
        });
        Ok(task_view)
    }

    /// Validates, persists (`SUBMITTED`), and executes a task to completion/suspension before
    /// returning (`RUN-082`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::CycleUnbounded`] on cyclic DAGs or step/store errors.
    pub async fn submit_and_run(
        &self,
        ctx: &RequestContext,
        input: SubmitTaskInput,
    ) -> GapResult<TaskView> {
        let (_, task_id, waves, payload) = self.persist_submitted_task(ctx, input)?;
        self.run_task_from_state(ctx, task_id, payload, waves).await
    }

    fn persist_submitted_task(
        &self,
        ctx: &RequestContext,
        input: SubmitTaskInput,
    ) -> GapResult<(TaskView, TaskId, Vec<Vec<usize>>, TaskStatePayload)> {
        let waves = validate_submission_steps(input.strategy, &input.steps)?;
        let tenant = ctx.tenant_scope();
        let now = self.clock.now_timestamp();
        let task_id = TaskId::new_v7();

        let ctx_remaining_ms = u64::try_from(
            ctx.deadline
                .saturating_duration_since(self.clock.now_instant())
                .as_millis(),
        )
        .unwrap_or(self.config.max_deadline_ms);
        let bounded_ms = input
            .deadline_ms
            .map_or(ctx_remaining_ms, |ms| ms.min(ctx_remaining_ms))
            .min(self.config.max_deadline_ms);
        let deadline_at = if bounded_ms > 0 {
            Some(add_millis_clamped(
                now,
                i64::try_from(bounded_ms).unwrap_or(i64::MAX),
            ))
        } else {
            Some(now)
        };

        let snapshots = input
            .steps
            .iter()
            .map(|s| TaskStepSnapshot {
                id: s.id.clone(),
                state: TaskStatus::Submitted,
                result: None,
                error: None,
                remote_task_id: None,
            })
            .collect();

        let payload = TaskStatePayload {
            context_id: input.context_id,
            goal: input.goal,
            strategy: input.strategy,
            principal_id: ctx.principal.id.clone(),
            hop_count: ctx.hop,
            trace_path: ctx.trace.path.clone(),
            spec_steps: input.steps,
            steps: snapshots,
            artifacts: Vec::new(),
            pending_approval: None,
            created_at: now,
            deadline_at,
            error: None,
            last_seq: 1,
        };
        let payload_json = serialize_json(&payload)?;
        let evt_payload_json = serialize_json(&TaskEventPayload {
            step_id: None,
            message: Some("task submitted".to_owned()),
            artifact: None,
            reason: None,
        })?;

        let record = TaskRecord {
            task_id,
            status: TaskStatus::Submitted,
            payload_json,
            lease_owner: None,
            lease_expires_at: None,
            updated_at: now,
        };
        let event = TaskEventRecord {
            task_id,
            seq: 1,
            status: TaskStatus::Submitted,
            payload_json: evt_payload_json,
            emitted_at: now,
        };

        // RUN-080: Persist to WAL TaskStore BEFORE publishing/acknowledging.
        self.store
            .put_task(&tenant, &record, &event)
            .map_err(map_store_err)?;
        self.hub
            .publish(&tenant, TaskEventView::from_record(&event));

        let view = TaskView::from_record(&record)
            .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}")))?;
        Ok((view, task_id, waves, payload))
    }

    async fn run_task_from_state(
        &self,
        ctx: &RequestContext,
        task_id: TaskId,
        mut payload: TaskStatePayload,
        waves: Vec<Vec<usize>>,
    ) -> GapResult<TaskView> {
        let tenant = ctx.tenant_scope();
        let cancel = self.register_control(&tenant, task_id);

        // Move Submitted -> Working and acquire lease before executing any step.
        let now = self.clock.now_timestamp();
        let lease_exp = self.lease_expires_at(now);
        let current_rec = self
            .store
            .get_task(&tenant, task_id)
            .map_err(map_store_err)?
            .ok_or_else(|| GapError::new(ErrorCode::TaskNotFound, "task missing before start"))?;

        if current_rec.status == TaskStatus::Canceled {
            self.clear_control(&tenant, task_id);
            return TaskView::from_record(&current_rec)
                .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt task: {e}")));
        }
        if current_rec.status == TaskStatus::Submitted {
            let view = self.persist_transition(
                &tenant,
                task_id,
                TaskStatus::Working,
                &mut payload,
                Some(Some(self.config.worker_id.clone())),
                Some(Some(lease_exp)),
                TaskEventPayload {
                    step_id: None,
                    message: Some("worker claimed task lease".to_owned()),
                    artifact: None,
                    reason: None,
                },
            )?;
            let _ = view;
        }

        // Spawn periodic lease heartbeat task (INV-02: never holds locks across await).
        let heartbeat_cancel = cancel.child_token();
        let hb_store = Arc::clone(&self.store);
        let hb_clock = Arc::clone(&self.clock);
        let hb_tenant = tenant.clone();
        let hb_worker = self.config.worker_id.clone();
        let hb_interval = self.config.heartbeat_interval.max(Duration::from_millis(5));
        let hb_lease_ms = i64::try_from(self.config.lease_duration.as_millis()).unwrap_or(30_000);
        let hb_cancel = heartbeat_cancel.clone();
        let hb_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(hb_interval);
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = hb_cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let ts = hb_clock.now_timestamp();
                        let exp = add_millis_clamped(ts, hb_lease_ms);
                        match hb_store.renew_lease(&hb_tenant, task_id, &hb_worker, exp) {
                            Ok(true) => {}
                            Ok(false) | Err(_) => break,
                        }
                    }
                }
            }
        });

        let exec_outcome = self
            .execute_waves(ctx, &tenant, task_id, &mut payload, &waves, &cancel)
            .await;

        heartbeat_cancel.cancel();
        let _ = hb_handle.await;

        let final_view = match exec_outcome {
            Ok(()) => {
                // Re-read store to check whether cancel_task transitioned concurrently while awaiting.
                let latest = self
                    .store
                    .get_task(&tenant, task_id)
                    .map_err(map_store_err)?
                    .ok_or_else(|| GapError::new(ErrorCode::TaskNotFound, "task vanished"))?;
                if latest.status.is_terminal() {
                    TaskView::from_record(&latest).map_err(|e| {
                        GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}"))
                    })?
                } else {
                    self.finalize_completed_task(&tenant, task_id, &mut payload)?
                }
            }
            Err(err) => {
                let latest = self
                    .store
                    .get_task(&tenant, task_id)
                    .map_err(map_store_err)?
                    .ok_or_else(|| GapError::new(ErrorCode::TaskNotFound, "task vanished"))?;
                if latest.status.is_terminal() {
                    TaskView::from_record(&latest).map_err(|e| {
                        GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}"))
                    })?
                } else {
                    let reason = if err.code() == ErrorCode::Cancelled {
                        Some("canceled".to_owned())
                    } else if err.code() == ErrorCode::DeadlineExceeded {
                        Some("deadline_exceeded".to_owned())
                    } else {
                        None
                    };
                    let target_status = if err.code() == ErrorCode::Cancelled {
                        TaskStatus::Canceled
                    } else {
                        TaskStatus::Failed
                    };
                    payload.error = Some(TaskErrorView {
                        code: err.code().as_str().to_owned(),
                        message: err.message().to_owned(),
                        retryable: err.retry().retryable,
                        reason: reason.clone(),
                    });
                    self.persist_transition(
                        &tenant,
                        task_id,
                        target_status,
                        &mut payload,
                        Some(None),
                        Some(None),
                        TaskEventPayload {
                            step_id: None,
                            message: Some(err.message().to_owned()),
                            artifact: None,
                            reason,
                        },
                    )?
                }
            }
        };

        self.clear_control(&tenant, task_id);
        Ok(final_view)
    }

    async fn execute_waves(
        &self,
        ctx: &RequestContext,
        tenant: &TenantScope,
        task_id: TaskId,
        payload: &mut TaskStatePayload,
        waves: &[Vec<usize>],
        cancel: &CancellationToken,
    ) -> GapResult<()> {
        for wave in waves {
            self.check_deadline_and_cancel(payload, cancel)?;

            // Mark pending steps in this wave as WORKING and persist checkpoint before dispatch.
            let mut runnable_indices = Vec::new();
            for &idx in wave {
                if payload.steps[idx].state == TaskStatus::Completed {
                    continue;
                }
                payload.steps[idx].state = TaskStatus::Working;
                runnable_indices.push(idx);
            }
            if runnable_indices.is_empty() {
                continue;
            }
            self.checkpoint_working_payload(tenant, task_id, payload)?;

            if runnable_indices.len() == 1 {
                let idx = runnable_indices[0];
                let step_spec = payload.spec_steps[idx].clone();
                let outcome = self
                    .execute_single_step(ctx, tenant, task_id, &step_spec, payload, cancel)
                    .await;
                self.apply_step_outcome(idx, &step_spec.id, payload, outcome)?;
                self.checkpoint_working_payload(tenant, task_id, payload)?;
            } else {
                let mut set = JoinSet::new();
                for idx in runnable_indices {
                    let engine = self.clone();
                    let ctx_owned = ctx.clone();
                    let tenant_owned = tenant.clone();
                    let step_spec = payload.spec_steps[idx].clone();
                    let payload_ctx_id = payload.context_id.clone();
                    let cancel_child = cancel.child_token();
                    set.spawn(async move {
                        let res = engine
                            .execute_step_kind(
                                &ctx_owned,
                                &tenant_owned,
                                task_id,
                                &step_spec,
                                payload_ctx_id.as_deref(),
                                cancel_child,
                            )
                            .await;
                        (idx, step_spec.id, res)
                    });
                }
                while let Some(joined) = set.join_next().await {
                    let (idx, step_id, res) = joined.map_err(|e| {
                        GapError::new(ErrorCode::Internal, format!("step task join error: {e}"))
                    })?;
                    self.apply_step_outcome(idx, &step_id, payload, res)?;
                }
                self.checkpoint_working_payload(tenant, task_id, payload)?;
            }
        }
        Ok(())
    }

    fn apply_step_outcome(
        &self,
        idx: usize,
        step_id: &str,
        payload: &mut TaskStatePayload,
        outcome: GapResult<(serde_json::Value, Option<String>, Vec<TaskArtifactView>)>,
    ) -> GapResult<()> {
        match outcome {
            Ok((val, remote_id, artifacts)) => {
                payload.steps[idx].state = TaskStatus::Completed;
                payload.steps[idx].result = Some(val);
                payload.steps[idx].error = None;
                if remote_id.is_some() {
                    payload.steps[idx].remote_task_id = remote_id;
                }
                payload.artifacts.extend(artifacts);
                Ok(())
            }
            Err(err) => {
                payload.steps[idx].state = if err.code() == ErrorCode::Cancelled {
                    TaskStatus::Canceled
                } else {
                    TaskStatus::Failed
                };
                payload.steps[idx].error = Some(TaskErrorView {
                    code: err.code().as_str().to_owned(),
                    message: format!("step `{step_id}` failed: {}", err.message()),
                    retryable: err.retry().retryable,
                    reason: None,
                });
                Err(err)
            }
        }
    }

    async fn execute_single_step(
        &self,
        ctx: &RequestContext,
        tenant: &TenantScope,
        task_id: TaskId,
        step: &TaskStepSpec,
        payload: &TaskStatePayload,
        cancel: &CancellationToken,
    ) -> GapResult<(serde_json::Value, Option<String>, Vec<TaskArtifactView>)> {
        self.execute_step_kind(
            ctx,
            tenant,
            task_id,
            step,
            payload.context_id.as_deref(),
            cancel.child_token(),
        )
        .await
    }

    async fn execute_step_kind(
        &self,
        ctx: &RequestContext,
        tenant: &TenantScope,
        task_id: TaskId,
        step: &TaskStepSpec,
        context_id: Option<&str>,
        cancel: CancellationToken,
    ) -> GapResult<(serde_json::Value, Option<String>, Vec<TaskArtifactView>)> {
        match &step.kind {
            TaskStepKind::Gapql {
                document,
                variables,
            } => {
                let res = tokio::select! {
                    () = cancel.cancelled() => {
                        return Err(GapError::new(ErrorCode::Cancelled, "task cancelled"));
                    }
                    r = self.gapql_runner.run_gapql(ctx, document, variables, cancel.clone()) => r?,
                };
                Ok((res, None, Vec::new()))
            }
            TaskStepKind::Delegate {
                branch,
                operation,
                message,
                expect_artifacts,
            } => {
                let hopped_ctx = advance_delegated_hop(ctx, branch, self.config.max_hops)?;
                let mut current = self
                    .delegated_client
                    .start_task(
                        &hopped_ctx,
                        branch,
                        operation,
                        context_id,
                        message,
                        expect_artifacts,
                        cancel.clone(),
                    )
                    .await?;
                let remote_id = current.remote_task_id.clone();
                self.record_delegated_target(tenant, task_id, branch.clone(), remote_id.clone());

                while !current.status.is_terminal() {
                    if cancel.is_cancelled() {
                        let _ = self
                            .delegated_client
                            .cancel_remote_task(
                                &hopped_ctx,
                                branch,
                                &remote_id,
                                Some("task canceled"),
                            )
                            .await;
                        return Err(GapError::new(
                            ErrorCode::Cancelled,
                            "delegated step canceled",
                        ));
                    }
                    tokio::select! {
                        () = cancel.cancelled() => {
                            let _ = self
                                .delegated_client
                                .cancel_remote_task(&hopped_ctx, branch, &remote_id, Some("task canceled"))
                                .await;
                            return Err(GapError::new(ErrorCode::Cancelled, "delegated step canceled"));
                        }
                        () = tokio::time::sleep(self.config.delegate_poll_interval) => {}
                    }
                    current = self
                        .delegated_client
                        .get_remote_task(&hopped_ctx, branch, &remote_id)
                        .await?;
                }

                match current.status {
                    TaskStatus::Completed => Ok((
                        current.result.unwrap_or(serde_json::Value::Null),
                        Some(remote_id),
                        current.artifacts,
                    )),
                    TaskStatus::Canceled => Err(GapError::new(
                        ErrorCode::Cancelled,
                        "delegated branch canceled remote task",
                    )),
                    TaskStatus::Rejected => Err(GapError::new(
                        ErrorCode::Forbidden,
                        current
                            .error
                            .map_or_else(|| "delegated step rejected".to_owned(), |e| e.message),
                    )),
                    _ => Err(GapError::new(
                        ErrorCode::BranchError,
                        current
                            .error
                            .map_or_else(|| "delegated step failed".to_owned(), |e| e.message),
                    )),
                }
            }
        }
    }

    fn finalize_completed_task(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        payload: &mut TaskStatePayload,
    ) -> GapResult<TaskView> {
        let result_value = match payload.strategy {
            TaskStrategy::Parallel => {
                // RUN-082: PARALLEL merges step results as an ordered array of step results.
                let merged: Vec<serde_json::Value> = payload
                    .steps
                    .iter()
                    .map(|s| s.result.clone().unwrap_or(serde_json::Value::Null))
                    .collect();
                serde_json::Value::Array(merged)
            }
            TaskStrategy::Single | TaskStrategy::Sequential | TaskStrategy::Dag => payload
                .steps
                .last()
                .and_then(|s| s.result.clone())
                .unwrap_or(serde_json::Value::Null),
        };

        let artifact = if result_value.is_null() {
            None
        } else {
            let bytes = serde_json::to_vec(&result_value).map_err(|e| {
                GapError::new(ErrorCode::Internal, format!("serialize task output: {e}"))
            })?;
            let art = match &self.blobs {
                Some(blob_store) => {
                    let stored = blob_store
                        .put_artifact(tenant, task_id, "application/json", &bytes)
                        .map_err(map_store_err)?;
                    TaskArtifactView::from_stored(
                        format!("art-{task_id}"),
                        Some("result".to_owned()),
                        stored,
                    )
                }
                None => TaskArtifactView {
                    id: format!("art-{task_id}"),
                    name: Some("result".to_owned()),
                    media_type: "application/json".to_owned(),
                    data: Some(result_value),
                    uri: None,
                    sha256: None,
                },
            };
            payload.artifacts.push(art.clone());
            Some(art)
        };

        self.persist_transition(
            tenant,
            task_id,
            TaskStatus::Completed,
            payload,
            Some(None),
            Some(None),
            TaskEventPayload {
                step_id: None,
                message: Some("task completed".to_owned()),
                artifact,
                reason: None,
            },
        )
    }

    fn check_deadline_and_cancel(
        &self,
        payload: &TaskStatePayload,
        cancel: &CancellationToken,
    ) -> GapResult<()> {
        if cancel.is_cancelled() {
            return Err(GapError::new(ErrorCode::Cancelled, "task cancelled"));
        }
        if let Some(deadline_at) = payload.deadline_at
            && self.clock.now_timestamp() >= deadline_at
        {
            return Err(GapError::new(
                ErrorCode::DeadlineExceeded,
                "task deadline exceeded",
            ));
        }
        Ok(())
    }

    fn checkpoint_working_payload(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        payload: &mut TaskStatePayload,
    ) -> GapResult<()> {
        let now = self.clock.now_timestamp();
        payload.last_seq = payload.last_seq.saturating_add(1);
        let payload_json = serialize_json(payload)?;
        let evt_meta = serialize_json(&TaskEventPayload {
            step_id: payload
                .steps
                .iter()
                .find(|s| s.state == TaskStatus::Working)
                .map(|s| s.id.clone()),
            message: Some("checkpointed task step state".to_owned()),
            artifact: None,
            reason: None,
        })?;
        let (_, evt) = self
            .store
            .checkpoint_progress(
                tenant,
                task_id,
                &self.config.worker_id,
                &payload_json,
                &evt_meta,
                now,
            )
            .map_err(map_store_err)?;
        payload.last_seq = evt.seq;
        self.hub.publish(tenant, TaskEventView::from_record(&evt));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_transition(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        to_status: TaskStatus,
        payload: &mut TaskStatePayload,
        lease_owner: Option<Option<String>>,
        lease_expires_at: Option<Option<Timestamp>>,
        event_meta: TaskEventPayload,
    ) -> GapResult<TaskView> {
        let now = self.clock.now_timestamp();
        let next_seq_est = payload.last_seq.saturating_add(1);
        payload.last_seq = next_seq_est;
        let payload_json = serialize_json(payload)?;
        let event_payload_json = serialize_json(&event_meta)?;

        let (rec, evt) = self
            .store
            .transition_task(
                tenant,
                &TaskTransitionRequest {
                    task_id,
                    to_status,
                    payload_json: Some(payload_json),
                    lease_owner,
                    lease_expires_at,
                    event_payload_json,
                    now,
                },
            )
            .map_err(map_store_err)?;

        payload.last_seq = evt.seq;
        // Persist first (above), then publish to subscribers (RUN-080).
        self.hub.publish(tenant, TaskEventView::from_record(&evt));
        TaskView::from_record(&rec)
            .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}")))
    }

    /// Reads a single task in the caller's tenant (`Query.gap.task`, `INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] on store/serialization failures.
    pub fn gap_task(&self, ctx: &RequestContext, task_id: TaskId) -> GapResult<Option<TaskView>> {
        let tenant = ctx.tenant_scope();
        let Some(rec) = self
            .store
            .get_task(&tenant, task_id)
            .map_err(map_store_err)?
        else {
            return Ok(None);
        };
        let view = TaskView::from_record(&rec)
            .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}")))?;
        Ok(Some(view))
    }

    /// Lists tasks in the caller's tenant with cursor pagination (`Query.gap.tasks`, `INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] on invalid `after` cursors or [`ErrorCode::Internal`]
    /// on store errors.
    pub fn gap_tasks(
        &self,
        ctx: &RequestContext,
        filter: &TaskListFilter,
    ) -> GapResult<TaskConnectionView> {
        let tenant = ctx.tenant_scope();
        let all = self
            .store
            .list_tasks(
                &tenant,
                &TaskQueryFilter {
                    status: None,
                    limit: None,
                },
            )
            .map_err(map_store_err)?;

        let after_id = match filter.after.as_deref() {
            Some(cur) if !cur.trim().is_empty() => Some(cur.parse::<TaskId>().map_err(|_| {
                GapError::new(
                    ErrorCode::ValidationError,
                    format!("invalid task pagination cursor `{cur}`"),
                )
            })?),
            _ => None,
        };

        let page_size = filter.first.unwrap_or(20).clamp(1, 200);
        let mut filtered = Vec::new();
        for rec in &all {
            if let Some(after) = after_id
                && rec.task_id <= after
            {
                continue;
            }
            if let Some(ref states) = filter.states
                && !states.contains(&rec.status)
            {
                continue;
            }
            let view = TaskView::from_record(rec).map_err(|e| {
                GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}"))
            })?;
            if let Some(ref ctx_id) = filter.context_id
                && view.context_id.as_deref() != Some(ctx_id.as_str())
            {
                continue;
            }
            filtered.push(view);
        }

        let has_next_page = filtered.len() > page_size;
        if has_next_page {
            filtered.truncate(page_size);
        }
        let end_cursor = filtered.last().map(|t| t.id.to_string());
        Ok(TaskConnectionView {
            nodes: filtered,
            has_next_page,
            end_cursor,
        })
    }

    /// Idempotently cancels a task, firing its local [`CancellationToken`] and propagating
    /// `CancelTask` to any active delegated branch tasks (`Mutation.gap.cancelTask`, `RUN-084`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::TaskNotFound`] if missing or [`ErrorCode::TaskInvalidState`] if the task
    /// already finished in `COMPLETED`, `FAILED`, or `REJECTED`.
    pub async fn gap_cancel_task(
        &self,
        ctx: &RequestContext,
        task_id: TaskId,
        reason: Option<&str>,
    ) -> GapResult<TaskView> {
        let tenant = ctx.tenant_scope();
        let rec = self
            .store
            .get_task(&tenant, task_id)
            .map_err(map_store_err)?
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                )
            })?;

        if rec.status == TaskStatus::Canceled {
            return TaskView::from_record(&rec).map_err(|e| {
                GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}"))
            });
        }
        if rec.status.is_terminal() {
            return Err(GapError::new(
                ErrorCode::TaskInvalidState,
                format!(
                    "cannot cancel task `{task_id}` in terminal state {:?}",
                    rec.status
                ),
            ));
        }

        let mut payload: TaskStatePayload = serde_json::from_str(&rec.payload_json)
            .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt payload: {e}")))?;

        let mut remote_targets = Vec::new();
        {
            let guard = self.lock_controls();
            if let Some(ctrl) = guard.get(&(tenant.tenant().as_str().to_owned(), task_id)) {
                ctrl.cancel.cancel();
                remote_targets.extend(ctrl.delegated.clone());
            }
        }
        for (idx, step_snap) in payload.steps.iter().enumerate() {
            if !step_snap.state.is_terminal()
                && let Some(ref remote_id) = step_snap.remote_task_id
                && let TaskStepKind::Delegate { branch, .. } = &payload.spec_steps[idx].kind
                && !remote_targets
                    .iter()
                    .any(|(b, r)| b == branch && r == remote_id)
            {
                remote_targets.push((branch.clone(), remote_id.clone()));
            }
        }

        for (branch, remote_id) in remote_targets {
            let _ = self
                .delegated_client
                .cancel_remote_task(ctx, &branch, &remote_id, reason)
                .await;
        }

        for step in &mut payload.steps {
            if !step.state.is_terminal() {
                step.state = TaskStatus::Canceled;
            }
        }
        let reason_text = reason.unwrap_or("canceled").to_owned();
        payload.error = Some(TaskErrorView {
            code: ErrorCode::Cancelled.as_str().to_owned(),
            message: format!("task canceled: {reason_text}"),
            retryable: false,
            reason: Some("canceled".to_owned()),
        });

        let view = match self.persist_transition(
            &tenant,
            task_id,
            TaskStatus::Canceled,
            &mut payload,
            Some(None),
            Some(None),
            TaskEventPayload {
                step_id: None,
                message: Some(reason_text),
                artifact: None,
                reason: Some("canceled".to_owned()),
            },
        ) {
            Ok(v) => v,
            Err(err) if err.code() == ErrorCode::TaskInvalidState => {
                // Re-read in case concurrent worker finished or canceled just before transition.
                let latest = self
                    .store
                    .get_task(&tenant, task_id)
                    .map_err(map_store_err)?
                    .ok_or_else(|| GapError::new(ErrorCode::TaskNotFound, "task vanished"))?;
                if latest.status == TaskStatus::Canceled {
                    return TaskView::from_record(&latest).map_err(|e| {
                        GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}"))
                    });
                }
                return Err(err);
            }
            Err(err) => return Err(err),
        };
        self.clear_control(&tenant, task_id);
        Ok(view)
    }

    /// Subscribes to persisted replay (`seq >= effective_from_seq`) followed by live task events
    /// (`Subscription.gapTaskEvents`, `RUN-080`, `INV-13`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] for malformed `Last-Event-ID`,
    /// [`ErrorCode::TaskNotFound`] if missing, or [`ErrorCode::Internal`] on store failures.
    pub fn gap_task_events(
        &self,
        ctx: &RequestContext,
        task_id: TaskId,
        from_seq: u64,
        sse_last_event_id: Option<&str>,
    ) -> GapResult<TaskEventStream> {
        let tenant = ctx.tenant_scope();
        let effective_from = parse_sse_last_event_id(sse_last_event_id, from_seq.max(1))?;
        self.hub
            .subscribe(self.store.as_ref(), &tenant, task_id, effective_from)
    }

    /// Pauses a `WORKING` task in `INPUT_REQUIRED` awaiting approval (`RUN-071`, `RUN-085`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::TaskNotFound`] or [`ErrorCode::TaskInvalidState`] on FSM mismatch.
    pub fn pause_for_approval(
        &self,
        ctx: &RequestContext,
        task_id: TaskId,
        approval: PendingApprovalView,
    ) -> GapResult<TaskView> {
        let tenant = ctx.tenant_scope();
        let rec = self
            .store
            .get_task(&tenant, task_id)
            .map_err(map_store_err)?
            .ok_or_else(|| GapError::new(ErrorCode::TaskNotFound, "task not found"))?;
        let mut payload: TaskStatePayload = serde_json::from_str(&rec.payload_json)
            .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt payload: {e}")))?;
        let msg = approval.summary.clone();
        payload.pending_approval = Some(approval);
        self.persist_transition(
            &tenant,
            task_id,
            TaskStatus::InputRequired,
            &mut payload,
            Some(None),
            Some(None),
            TaskEventPayload {
                step_id: None,
                message: Some(msg),
                artifact: None,
                reason: Some("input_required".to_owned()),
            },
        )
    }

    /// Expires stale `INPUT_REQUIRED` tasks past `approval_ttl_ms` into `REJECTED(reason=approval_expired)`
    /// (`RUN-085`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if the store operation fails.
    pub fn expire_stale_approvals(&self, tenant: &TenantScope) -> GapResult<Vec<TaskView>> {
        let now = self.clock.now_timestamp();
        let expired = self
            .store
            .expire_stale_approvals(tenant, now, self.config.approval_ttl_ms)
            .map_err(map_store_err)?;
        let mut views = Vec::with_capacity(expired.len());
        for (rec, evt) in expired {
            self.hub.publish(tenant, TaskEventView::from_record(&evt));
            let view = TaskView::from_record(&rec).map_err(|e| {
                GapError::new(ErrorCode::Internal, format!("corrupt task view: {e}"))
            })?;
            views.push(view);
        }
        Ok(views)
    }

    /// Prunes terminal tasks older than `retention_ms` (`RUN-086`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] on store errors.
    pub fn prune_retained_tasks(&self, tenant: &TenantScope) -> GapResult<usize> {
        let now = self.clock.now_timestamp();
        self.store
            .prune_terminal_tasks(tenant, now, self.config.retention_ms)
            .map_err(map_store_err)
    }

    /// Runs before gateway readiness (`RUN-081`, `RUN-091`): claims recoverable `SUBMITTED`/`WORKING`
    /// tasks with expired leases (or matching this worker's identity), resumes steps marked
    /// `resumable` (or carrying an `idempotency_key` + `connector_replay_safe`), and marks
    /// non-resumable in-flight tasks `FAILED(code=GAP_INTERNAL, reason=crash)`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] or [`ErrorCode::CycleUnbounded`] on recovery failure.
    pub async fn recover_before_readiness(&self, ctx: &RequestContext) -> GapResult<Vec<TaskView>> {
        let tenant = ctx.tenant_scope();
        let recoverable = self
            .store
            .list_recoverable_tasks(&tenant)
            .map_err(map_store_err)?;
        let now = self.clock.now_timestamp();
        let lease_exp = self.lease_expires_at(now);
        let mut outcomes = Vec::new();

        for rec in recoverable {
            let mut payload: TaskStatePayload = serde_json::from_str(&rec.payload_json)
                .map_err(|e| GapError::new(ErrorCode::Internal, format!("corrupt payload: {e}")))?;

            if rec.status == TaskStatus::Working {
                let claimed = self
                    .store
                    .claim_lease(&tenant, rec.task_id, &self.config.worker_id, now, lease_exp)
                    .map_err(map_store_err)?;
                if !claimed {
                    continue;
                }
            }

            let mut crash_blocked = false;
            for (idx, snap) in payload.steps.iter_mut().enumerate() {
                if snap.state == TaskStatus::Working {
                    if payload.spec_steps[idx].is_crash_resumable() {
                        snap.state = TaskStatus::Submitted;
                    } else {
                        crash_blocked = true;
                        snap.state = TaskStatus::Failed;
                        snap.error = Some(TaskErrorView {
                            code: ErrorCode::Internal.as_str().to_owned(),
                            message: "in-flight step interrupted by worker crash".to_owned(),
                            retryable: false,
                            reason: Some("crash".to_owned()),
                        });
                    }
                }
            }

            if crash_blocked {
                payload.error = Some(TaskErrorView {
                    code: ErrorCode::Internal.as_str().to_owned(),
                    message: "in-flight non-resumable step interrupted by worker crash".to_owned(),
                    retryable: false,
                    reason: Some("crash".to_owned()),
                });
                let failed_view = self.persist_transition(
                    &tenant,
                    rec.task_id,
                    TaskStatus::Failed,
                    &mut payload,
                    Some(None),
                    Some(None),
                    TaskEventPayload {
                        step_id: None,
                        message: Some("worker crash recovery marked task failed".to_owned()),
                        artifact: None,
                        reason: Some("crash".to_owned()),
                    },
                )?;
                outcomes.push(failed_view);
            } else {
                let waves = validate_submission_steps(payload.strategy, &payload.spec_steps)?;
                let view = self
                    .run_task_from_state(ctx, rec.task_id, payload, waves)
                    .await?;
                outcomes.push(view);
            }
        }

        Ok(outcomes)
    }
}

fn add_millis_clamped(base: Timestamp, delta_ms: i64) -> Timestamp {
    Timestamp::from_unix_millis(base.unix_millis().saturating_add(delta_ms)).unwrap_or(base)
}

fn serialize_json<T: serde::Serialize>(val: &T) -> GapResult<String> {
    serde_json::to_string(val).map_err(|e| {
        GapError::new(
            ErrorCode::Internal,
            format!("task JSON serialize error: {e}"),
        )
    })
}

fn map_store_err(err: StoreError) -> GapError {
    match err {
        StoreError::NotFound => GapError::new(ErrorCode::TaskNotFound, "task not found"),
        StoreError::Conflict => {
            GapError::new(ErrorCode::TaskInvalidState, "invalid task state transition")
        }
        StoreError::Invalid { field, reason } => GapError::new(
            ErrorCode::ValidationError,
            format!("invalid task store field `{field}`: {reason}"),
        ),
        other => GapError::new(ErrorCode::Internal, format!("task store failure: {other}")),
    }
}
