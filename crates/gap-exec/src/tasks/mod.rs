//! Durable task execution engine, strategies, leases, SSE event replay, and crash recovery
//! (`WP-M6-02`, `RUN-080`..`RUN-086`, `RUN-091`, `INV-09`, `INV-13`, `INV-20`).

pub mod dag;
pub mod engine;
pub mod events;
pub mod types;

#[cfg(test)]
mod tests;

pub use dag::{advance_delegated_hop, validate_submission_steps};
pub use engine::{
    DelegateStartResult, DelegatedStepClient, GapqlStepRunner, NoopDelegatedStepClient,
    NoopGapqlStepRunner, TaskEngine, TaskEngineConfig, TaskFuture, TaskListFilter,
};
pub use events::{
    TASK_EVENT_STREAM_BUFFER, TaskEventHub, TaskEventStream, parse_sse_last_event_id,
};
pub use gap_store::tasks::{
    ArtifactRef, DEFAULT_APPROVAL_TTL_MS, DEFAULT_TASK_HEARTBEAT_SECS, DEFAULT_TASK_LEASE_SECS,
    DEFAULT_TASK_RETENTION_MS, FsArtifactBlobStore, INLINE_ARTIFACT_MAX_BYTES, InMemoryTaskStore,
    SqliteTaskStore, StoredArtifact, TaskEventRecord, TaskRecord, TaskStatus, TaskStore,
};
pub use types::{
    PendingApprovalView, SubmitTaskInput, TaskArtifactView, TaskConnectionView, TaskErrorView,
    TaskEventPayload, TaskEventView, TaskStatePayload, TaskStepKind, TaskStepSnapshot,
    TaskStepSpec, TaskStrategy, TaskView,
};
