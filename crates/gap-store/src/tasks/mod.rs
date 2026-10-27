//! Durable A2A-compatible task state and lease persistence (`RUN-080`, `RUN-081`, `INV-09`, `INV-13`).
//!
//! **Provisional declaration (`m2-01-design.md` §9):** [`TaskStore`] and its companion record types
//! establish the object-safe `L1` trait boundary and tenant-first signature discipline (`INV-09`).
//! `WP-M6-02` implements the store and may extend these provisional types.

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Provisional task lifecycle states matching the `spec/06-runtime.md` §8 FSM (`RUN-080`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Provisional durable snapshot of a task row (`RUN-080`, `RUN-081`, `WP-M6-02`).
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

/// Provisional monotonic per-task event log entry (`RUN-080`, `INV-13`, `WP-M6-02`).
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

/// Provisional object-safe durable task store (`RUN-080`, `RUN-081`, `INV-09`, `WP-M6-02`).
///
/// Every method takes [`TenantScope`] first so no cross-tenant read or write is expressible.
/// Synchronous (`L1`); implemented and extended in `WP-M6-02`.
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
}

#[cfg(test)]
mod tests {
    use super::TaskStore;

    // spec: RUN-080, RUN-081, INV-09
    #[test]
    fn task_store_trait_is_object_safe() {
        fn _assert_dyn(_store: &dyn TaskStore) {}
    }
}
