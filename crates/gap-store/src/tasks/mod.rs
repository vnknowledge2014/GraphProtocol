//! Durable A2A-compatible task state, monotonic event log, leases, and blob persistence
//! (`RUN-080`..`RUN-086`, `INV-09`, `INV-13`).

pub mod blob;
pub mod memory;
pub mod sqlite;

#[cfg(test)]
mod tests;

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

pub use blob::{
    ArtifactRef, FsArtifactBlobStore, INLINE_ARTIFACT_MAX_BYTES, MAX_ARTIFACT_BLOB_BYTES,
    StoredArtifact,
};
pub use memory::InMemoryTaskStore;
pub use sqlite::SqliteTaskStore;

/// Default worker lease duration in seconds (`30 s`, `RUN-081`).
pub const DEFAULT_TASK_LEASE_SECS: i64 = 30;

/// Default worker heartbeat interval in seconds (`10 s`, `RUN-081`).
pub const DEFAULT_TASK_HEARTBEAT_SECS: i64 = 10;

/// Default approval expiry TTL in milliseconds (`24 h`, `RUN-085`).
pub const DEFAULT_APPROVAL_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

/// Default terminal task retention period in milliseconds (`30 d`, `RUN-086`).
pub const DEFAULT_TASK_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

/// Task lifecycle states matching the `spec/06-runtime.md` §8 FSM (`RUN-080`, `ADR-016`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    /// Accepted, awaiting worker pickup.
    Submitted,
    /// Actively leased by a worker (`RUN-081`).
    Working,
    /// Paused awaiting caller input.
    InputRequired,
    /// Paused awaiting out-of-band authorization / human approval (`RUN-071`, `RUN-085`).
    AuthRequired,
    /// Terminal success.
    Completed,
    /// Terminal failure.
    Failed,
    /// Terminal cancellation (`RUN-084`).
    Canceled,
    /// Terminal policy or approval rejection.
    Rejected,
}

impl TaskStatus {
    /// Exhaustive list of all 8 task lifecycle states in `spec/06-runtime.md` §8.
    pub const ALL: &'static [Self] = &[
        Self::Submitted,
        Self::Working,
        Self::InputRequired,
        Self::AuthRequired,
        Self::Completed,
        Self::Failed,
        Self::Canceled,
        Self::Rejected,
    ];

    /// Canonical wire string in `SCREAMING_SNAKE_CASE`.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Submitted => "SUBMITTED",
            Self::Working => "WORKING",
            Self::InputRequired => "INPUT_REQUIRED",
            Self::AuthRequired => "AUTH_REQUIRED",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Canceled => "CANCELED",
            Self::Rejected => "REJECTED",
        }
    }

    /// Parses canonical `SCREAMING_SNAKE_CASE` wire status text.
    #[must_use]
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "SUBMITTED" => Some(Self::Submitted),
            "WORKING" => Some(Self::Working),
            "INPUT_REQUIRED" => Some(Self::InputRequired),
            "AUTH_REQUIRED" => Some(Self::AuthRequired),
            "COMPLETED" => Some(Self::Completed),
            "FAILED" => Some(Self::Failed),
            "CANCELED" => Some(Self::Canceled),
            "REJECTED" => Some(Self::Rejected),
            _ => None,
        }
    }

    /// Returns `true` iff this state is terminal (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    /// Returns `true` iff transitioning `self -> next` is valid under `spec/06-runtime.md` §8 (`RUN-080`).
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Submitted => matches!(next, Self::Working | Self::Rejected | Self::Canceled),
            Self::Working => matches!(
                next,
                Self::InputRequired
                    | Self::AuthRequired
                    | Self::Completed
                    | Self::Failed
                    | Self::Canceled
            ),
            Self::InputRequired => {
                matches!(
                    next,
                    Self::Working | Self::Canceled | Self::Rejected | Self::Failed
                )
            }
            Self::AuthRequired => {
                matches!(next, Self::Working | Self::Canceled | Self::Failed)
            }
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected => false,
        }
    }
}

/// Durable snapshot of a task row (`RUN-080`, `RUN-081`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Durable UUIDv7 task identifier.
    pub task_id: TaskId,
    /// Current FSM state.
    pub status: TaskStatus,
    /// Serialized task state / step ledger JSON.
    pub payload_json: String,
    /// Identifier of the worker currently holding the lease when `Working` (`RUN-081`).
    pub lease_owner: Option<String>,
    /// Lease expiry instant (`RUN-081`).
    pub lease_expires_at: Option<Timestamp>,
    /// Timestamp of the last persisted transition.
    pub updated_at: Timestamp,
}

/// Monotonic per-task event log entry (`RUN-080`, `INV-13`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEventRecord {
    /// Owning task identifier.
    pub task_id: TaskId,
    /// Strictly increasing per-task sequence number (`INV-13`).
    pub seq: u64,
    /// FSM state resulting from this transition.
    pub status: TaskStatus,
    /// Serialized event payload JSON.
    pub payload_json: String,
    /// Event emission timestamp.
    pub emitted_at: Timestamp,
}

/// Atomic state-transition request passed to [`TaskStore::transition_task`] (`RUN-080`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTransitionRequest {
    /// Target task identifier.
    pub task_id: TaskId,
    /// Desired next FSM state.
    pub to_status: TaskStatus,
    /// Optional updated durable task payload JSON (`None` preserves existing payload).
    pub payload_json: Option<String>,
    /// Optional lease owner update (`Some(None)` clears lease, `Some(Some(w))` sets it, `None` clears unless `Working`).
    pub lease_owner: Option<Option<String>>,
    /// Optional lease expiry update (`None` clears unless `Working`).
    pub lease_expires_at: Option<Option<Timestamp>>,
    /// Event-specific JSON metadata persisted on [`TaskEventRecord::payload_json`].
    pub event_payload_json: String,
    /// Transition timestamp.
    pub now: Timestamp,
}

/// Filter options for [`TaskStore::list_tasks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskQueryFilter {
    /// Optional status filter.
    pub status: Option<TaskStatus>,
    /// Optional upper bound on returned rows.
    pub limit: Option<usize>,
}

/// Object-safe durable task store (`RUN-080`..`RUN-086`, `INV-09`, `INV-13`).
///
/// Every method takes [`TenantScope`] first so no cross-tenant read or write is expressible.
pub trait TaskStore: Send + Sync {
    /// Atomically persists `record` and appends `event` before acknowledgement (`RUN-080`).
    fn put_task(
        &self,
        tenant: &TenantScope,
        record: &TaskRecord,
        event: &TaskEventRecord,
    ) -> Result<(), StoreError>;

    /// Loads the latest persisted [`TaskRecord`] for `task_id` in `tenant`.
    fn get_task(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<Option<TaskRecord>, StoreError>;

    /// Replays persisted [`TaskEventRecord`]s for `task_id` in `tenant` with `seq >= from_seq` (`RUN-080`).
    fn events_from(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        from_seq: u64,
    ) -> Result<Vec<TaskEventRecord>, StoreError>;

    /// Acquires or renews a worker lease on a `WORKING` task (`RUN-081`).
    fn renew_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError>;

    /// Atomically transitions a task under `spec/06-runtime.md` §8 FSM rules and appends the next
    /// strictly increasing `seq` event (`RUN-080`, `INV-13`).
    fn transition_task(
        &self,
        tenant: &TenantScope,
        req: &TaskTransitionRequest,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError>;

    /// Lists tasks in `tenant` matching `filter`.
    fn list_tasks(
        &self,
        tenant: &TenantScope,
        filter: &TaskQueryFilter,
    ) -> Result<Vec<TaskRecord>, StoreError>;

    /// Claims an unleased or expired lease on a `WORKING` task for `worker_id` (`RUN-081`).
    fn claim_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError>;

    /// Returns all non-terminal `SUBMITTED` or `WORKING` tasks in `tenant` for startup recovery (`RUN-081`).
    fn list_recoverable_tasks(&self, tenant: &TenantScope) -> Result<Vec<TaskRecord>, StoreError>;

    /// Expires `INPUT_REQUIRED` tasks in `tenant` older than `approval_ttl_ms` into `REJECTED` (`RUN-085`).
    fn expire_stale_approvals(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        approval_ttl_ms: i64,
    ) -> Result<Vec<(TaskRecord, TaskEventRecord)>, StoreError>;

    /// Deletes terminal tasks in `tenant` older than `retention_ms` (`RUN-086`).
    fn prune_terminal_tasks(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        retention_ms: i64,
    ) -> Result<usize, StoreError>;

    /// Atomically checkpoints updated payload JSON for an active `WORKING` task owned by
    /// `worker_id` and appends the next strictly increasing `WORKING` progress event (`RUN-080`,
    /// `RUN-081`, `INV-13`).
    fn checkpoint_progress(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        payload_json: &str,
        event_payload_json: &str,
        now: Timestamp,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError>;
}
