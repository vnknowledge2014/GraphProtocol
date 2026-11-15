//! Typed GAP-QL Task API models matching `crates/gap-ql/schema/builtin.graphql` (`RUN-080`..`RUN-086`).

use gap_core::clock::Timestamp;
use gap_core::ids::TaskId;
use gap_store::tasks::{StoredArtifact, TaskEventRecord, TaskRecord, TaskStatus};
use serde::{Deserialize, Serialize};

/// Execution scheduling strategy for a submitted task (`RUN-082`, `ADR-016`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStrategy {
    /// Single executable step.
    #[default]
    Single,
    /// Independent steps executed concurrently; results merged into an array.
    Parallel,
    /// Ordered steps where step `i` waits for step `i - 1`.
    Sequential,
    /// Directed acyclic graph defined by explicit `depends_on` edges.
    Dag,
}

/// Kind of work performed by a task step (`RUN-082`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskStepKind {
    /// Executes a GAP-QL document through the normal execution pipeline *as the submitting principal*.
    Gapql {
        /// GAP-QL query or mutation document.
        document: String,
        /// Optional JSON variable bindings.
        #[serde(default)]
        variables: serde_json::Value,
    },
    /// Delegates execution to a remote branch (`StartTask`) and mirrors its states.
    Delegate {
        /// Target branch identifier.
        branch: String,
        /// Target operation or skill identifier.
        operation: String,
        /// JSON message payload forwarded to the remote branch.
        #[serde(default)]
        message: serde_json::Value,
        /// Expected artifact media types or names.
        #[serde(default)]
        expect_artifacts: Vec<String>,
    },
}

/// Declarative definition of one step inside a [`SubmitTaskInput`] (`RUN-081`, `RUN-082`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStepSpec {
    /// Unique step identifier within the task.
    pub id: String,
    /// Upstream step identifiers that must reach `COMPLETED` before this step runs.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Execution payload (`Gapql` or `Delegate`).
    pub kind: TaskStepKind,
    /// Whether this step is checkpointed / idempotent and safe to resume after a worker crash (`RUN-081`).
    #[serde(default)]
    pub resumable: bool,
    /// Optional idempotency key carried by this step (`RUN-081`, `RUN-072`).
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Whether the backing connector guarantees replay-safety for `idempotency_key` (`RUN-081`).
    #[serde(default)]
    pub connector_replay_safe: bool,
}

impl TaskStepSpec {
    /// Constructs a GAP-QL step with default non-resumable crash recovery metadata.
    #[must_use]
    pub fn gapql(
        id: impl Into<String>,
        document: impl Into<String>,
        variables: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            depends_on: Vec::new(),
            kind: TaskStepKind::Gapql {
                document: document.into(),
                variables,
            },
            resumable: false,
            idempotency_key: None,
            connector_replay_safe: false,
        }
    }

    /// Constructs a delegated peer/A2A step.
    #[must_use]
    pub fn delegate(
        id: impl Into<String>,
        branch: impl Into<String>,
        operation: impl Into<String>,
        message: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            depends_on: Vec::new(),
            kind: TaskStepKind::Delegate {
                branch: branch.into(),
                operation: operation.into(),
                message,
                expect_artifacts: Vec::new(),
            },
            resumable: false,
            idempotency_key: None,
            connector_replay_safe: false,
        }
    }

    /// Appends an explicit upstream dependency step ID (`DAG` / `SEQUENTIAL`).
    #[must_use]
    pub fn with_depends_on(mut self, deps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends_on.extend(deps.into_iter().map(Into::into));
        self
    }

    /// Marks this step as resumable across worker crash recovery (`RUN-081`).
    #[must_use]
    pub const fn with_resumable(mut self, resumable: bool) -> Self {
        self.resumable = resumable;
        self
    }

    /// Attaches an idempotency key and connector replay-safety declaration (`RUN-081`).
    #[must_use]
    pub fn with_idempotent_replay(mut self, key: impl Into<String>, replay_safe: bool) -> Self {
        self.idempotency_key = Some(key.into());
        self.connector_replay_safe = replay_safe;
        self
    }

    /// Returns `true` if a worker crash while this step is `WORKING` can be safely resumed (`RUN-081`).
    #[must_use]
    pub const fn is_crash_resumable(&self) -> bool {
        self.resumable || (self.idempotency_key.is_some() && self.connector_replay_safe)
    }
}

/// Input accepted by `Mutation.gap.submitTask` (`RUN-082`, `RUN-083`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitTaskInput {
    /// Optional conversation/thread identifier propagated across peer hops (`CMP-023`).
    #[serde(default)]
    pub context_id: Option<String>,
    /// Optional human/agent goal summary.
    #[serde(default)]
    pub goal: Option<String>,
    /// Execution strategy (`SINGLE`, `PARALLEL`, `SEQUENTIAL`, `DAG`).
    #[serde(default)]
    pub strategy: TaskStrategy,
    /// Ordered step declarations.
    pub steps: Vec<TaskStepSpec>,
    /// Relative execution deadline budget in milliseconds from submission (`RUN-083`).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// Optional client idempotency key for task submission / mutation replay (`RUN-072`).
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Structured error attached to a task or step (`spec/02-gap-ql.md`, `RUN-081`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskErrorView {
    /// Canonical `GAP_*` error code string.
    pub code: String,
    /// Sanitized error explanation.
    pub message: String,
    /// Whether the failure is retryable.
    pub retryable: bool,
    /// Optional machine-readable sub-reason (e.g. `"crash"`, `"approval_expired"`, `"canceled"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Output artifact produced by a task (`RUN-086`, `proto/gap/ngc/v1/ngc.proto`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArtifactView {
    /// Unique artifact identifier.
    pub id: String,
    /// Optional logical name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Declared MIME media type.
    pub media_type: String,
    /// Inline JSON payload when `<= 1 MiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// External blob URI (`file://...`) when `> 1 MiB` (`RUN-086`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Integrity hex digest paired with `uri` when offloaded (`RUN-086`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl TaskArtifactView {
    /// Builds a [`TaskArtifactView`] from a persisted [`StoredArtifact`] (`RUN-086`).
    #[must_use]
    pub fn from_stored(
        id: impl Into<String>,
        name: Option<String>,
        stored: StoredArtifact,
    ) -> Self {
        match stored {
            StoredArtifact::Inline {
                data, mime_type, ..
            } => {
                let parsed = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => serde_json::Value::String(data),
                };
                Self {
                    id: id.into(),
                    name,
                    media_type: mime_type,
                    data: Some(parsed),
                    uri: None,
                    sha256: None,
                }
            }
            StoredArtifact::Reference(r) => Self {
                id: id.into(),
                name,
                media_type: r.mime_type,
                data: None,
                uri: Some(r.uri),
                sha256: Some(r.sha256),
            },
        }
    }
}

/// Pending human approval descriptor attached when a task enters `INPUT_REQUIRED` (`RUN-071`, `RUN-085`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApprovalView {
    /// Approval request identifier.
    pub id: String,
    /// Operation / action awaiting approval.
    pub action: String,
    /// Deterministic server-generated summary (`RUN-071`, `SEC-085`).
    pub summary: String,
    /// Declared side-effect classification.
    pub side_effect: String,
    /// Expiration instant (`24 h` default TTL, `RUN-085`).
    pub expires_at: Timestamp,
}

/// Runtime snapshot of a single step within a [`TaskStatePayload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStepSnapshot {
    /// Step identifier matching [`TaskStepSpec::id`].
    pub id: String,
    /// Current step FSM state.
    pub state: TaskStatus,
    /// Step output payload when `COMPLETED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Step error when `FAILED` or `REJECTED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskErrorView>,
    /// Downstream branch remote task ID when `Delegate` (`RUN-082`, `RUN-084`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_task_id: Option<String>,
}

/// Durable JSON state persisted in [`TaskRecord::payload_json`] (`RUN-080`, `RUN-081`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatePayload {
    /// Optional conversation context ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Optional goal description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Execution strategy.
    pub strategy: TaskStrategy,
    /// Submitting principal identifier (`RUN-082`).
    pub principal_id: String,
    /// Current peer hop count (`RUN-083`, `INV-20`).
    pub hop_count: u8,
    /// Peer trace path (`RUN-083`, `INV-20`).
    #[serde(default)]
    pub trace_path: Vec<String>,
    /// Step definitions retained for crash recovery (`RUN-081`).
    pub spec_steps: Vec<TaskStepSpec>,
    /// Per-step runtime snapshots.
    pub steps: Vec<TaskStepSnapshot>,
    /// Collected task artifacts.
    #[serde(default)]
    pub artifacts: Vec<TaskArtifactView>,
    /// Pending approval descriptor if `INPUT_REQUIRED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApprovalView>,
    /// Task creation timestamp.
    pub created_at: Timestamp,
    /// Task deadline instant (`RUN-083`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<Timestamp>,
    /// Task-level error when `FAILED`, `REJECTED`, or `CANCELED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskErrorView>,
    /// Latest persisted event sequence number (`INV-13`).
    pub last_seq: u64,
}

/// GraphQL/runtime view of a task (`crates/gap-ql/schema/builtin.graphql`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// Task identifier.
    pub id: TaskId,
    /// Optional context ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Current FSM state.
    pub state: TaskStatus,
    /// Execution strategy.
    pub strategy: TaskStrategy,
    /// Per-step execution snapshots.
    pub steps: Vec<TaskStepSnapshot>,
    /// Collected artifacts.
    pub artifacts: Vec<TaskArtifactView>,
    /// Pending approval metadata when `INPUT_REQUIRED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApprovalView>,
    /// Submission timestamp.
    pub created_at: Timestamp,
    /// Last transition timestamp.
    pub updated_at: Timestamp,
    /// Deadline instant if bounded (`RUN-083`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<Timestamp>,
    /// Highest emitted `TaskEvent` sequence number (`INV-13`).
    pub last_seq: u64,
    /// Task error if failed/rejected/canceled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskErrorView>,
}

impl TaskView {
    /// Materializes a [`TaskView`] from a persisted [`TaskRecord`] and optional highest `seq`.
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] if [`TaskRecord::payload_json`] is corrupted.
    pub fn from_record(record: &TaskRecord) -> Result<Self, serde_json::Error> {
        let payload: TaskStatePayload = serde_json::from_str(&record.payload_json)?;
        let error = match (&payload.error, record.status) {
            (Some(err), _) => Some(err.clone()),
            (None, TaskStatus::Rejected) => Some(TaskErrorView {
                code: "GAP_FORBIDDEN".to_owned(),
                message: "approval expired".to_owned(),
                retryable: false,
                reason: Some("approval_expired".to_owned()),
            }),
            _ => None,
        };
        Ok(Self {
            id: record.task_id,
            context_id: payload.context_id,
            state: record.status,
            strategy: payload.strategy,
            steps: payload.steps,
            artifacts: payload.artifacts,
            pending_approval: payload.pending_approval,
            created_at: payload.created_at,
            updated_at: record.updated_at,
            deadline_at: payload.deadline_at,
            last_seq: payload.last_seq,
            error,
        })
    }
}

/// Paginated list response returned by `Query.gap.tasks` (`builtin.graphql`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskConnectionView {
    /// Page items.
    pub nodes: Vec<TaskView>,
    /// `true` iff more items follow `end_cursor`.
    pub has_next_page: bool,
    /// Opaque pagination cursor for subsequent pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_cursor: Option<String>,
}

/// Compact metadata stored inside [`TaskEventRecord::payload_json`] (`RUN-080`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEventPayload {
    /// Associated step ID when event tracks step progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Human/log status message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Emitted artifact if this event attached an output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<TaskArtifactView>,
    /// Machine-readable transition reason (e.g. `"approval_expired"`, `"crash"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Streamed or replayed task lifecycle event matching `type TaskEvent` (`RUN-080`, `INV-13`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEventView {
    /// Strictly monotonic per-task sequence number (`INV-13`).
    pub seq: u64,
    /// Owning task ID.
    pub task_id: TaskId,
    /// Emission timestamp.
    pub at: Timestamp,
    /// FSM state resulting from or active during this event.
    pub state: TaskStatus,
    /// Optional associated step ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Optional human-readable message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional emitted artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<TaskArtifactView>,
    /// Optional structured reason (`approval_expired`, `crash`, `canceled`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TaskEventView {
    /// Converts a persisted [`TaskEventRecord`] into a [`TaskEventView`].
    #[must_use]
    pub fn from_record(record: &TaskEventRecord) -> Self {
        let meta: TaskEventPayload = serde_json::from_str(&record.payload_json).unwrap_or_default();
        Self {
            seq: record.seq,
            task_id: record.task_id,
            at: record.emitted_at,
            state: record.status,
            step_id: meta.step_id,
            message: meta.message,
            artifact: meta.artifact,
            reason: meta.reason,
        }
    }
}
