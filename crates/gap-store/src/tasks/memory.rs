//! Thread-safe in-memory [`TaskStore`] reference backend (`RUN-080`..`RUN-086`, `INV-09`, `INV-13`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};

use crate::error::StoreError;
use crate::sqlite::lock_unpoisoned;

use super::{
    TaskEventRecord, TaskQueryFilter, TaskRecord, TaskStatus, TaskStore, TaskTransitionRequest,
};

#[derive(Debug, Clone)]
struct StoredTask {
    record: TaskRecord,
    events: Vec<TaskEventRecord>,
}

/// In-memory tenant-partitioned [`TaskStore`] (`INV-09`, `INV-13`, `RUN-080`).
#[derive(Debug, Default)]
pub struct InMemoryTaskStore {
    tenants: Mutex<HashMap<String, BTreeMap<TaskId, StoredTask>>>,
}

impl InMemoryTaskStore {
    /// Creates an empty [`InMemoryTaskStore`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TaskStore for InMemoryTaskStore {
    fn put_task(
        &self,
        tenant: &TenantScope,
        record: &TaskRecord,
        event: &TaskEventRecord,
    ) -> Result<(), StoreError> {
        if event.task_id != record.task_id {
            return Err(StoreError::Invalid {
                field: "task_id",
                reason: "event task_id must match record task_id",
            });
        }
        if event.status != record.status {
            return Err(StoreError::Invalid {
                field: "status",
                reason: "event status must match record status",
            });
        }
        let mut guard = lock_unpoisoned(&self.tenants);
        let bucket = guard
            .entry(tenant.tenant().as_str().to_owned())
            .or_default();

        match bucket.get_mut(&record.task_id) {
            Some(existing) => {
                if !existing.record.status.can_transition_to(record.status) {
                    return Err(StoreError::Conflict);
                }
                let expected_seq = existing
                    .events
                    .last()
                    .map_or(1, |e| e.seq.saturating_add(1));
                if event.seq != expected_seq {
                    return Err(StoreError::Conflict);
                }
                existing.record = record.clone();
                existing.events.push(event.clone());
            }
            None => {
                if event.seq != 1 {
                    return Err(StoreError::Invalid {
                        field: "seq",
                        reason: "initial task event must start at seq 1",
                    });
                }
                bucket.insert(
                    record.task_id,
                    StoredTask {
                        record: record.clone(),
                        events: vec![event.clone()],
                    },
                );
            }
        }
        Ok(())
    }

    fn get_task(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<Option<TaskRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.tenants);
        Ok(guard
            .get(tenant.tenant().as_str())
            .and_then(|m| m.get(&task_id))
            .map(|t| t.record.clone()))
    }

    fn events_from(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        from_seq: u64,
    ) -> Result<Vec<TaskEventRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.tenants);
        let Some(entry) = guard
            .get(tenant.tenant().as_str())
            .and_then(|m| m.get(&task_id))
        else {
            return Ok(Vec::new());
        };
        Ok(entry
            .events
            .iter()
            .filter(|e| e.seq >= from_seq)
            .cloned()
            .collect())
    }

    fn renew_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError> {
        if worker_id.trim().is_empty() {
            return Err(StoreError::Invalid {
                field: "worker_id",
                reason: "worker_id must not be empty",
            });
        }
        let mut guard = lock_unpoisoned(&self.tenants);
        let Some(entry) = guard
            .get_mut(tenant.tenant().as_str())
            .and_then(|m| m.get_mut(&task_id))
        else {
            return Ok(false);
        };
        if entry.record.status != TaskStatus::Working {
            return Ok(false);
        }
        if entry.record.lease_owner.as_deref() != Some(worker_id) {
            return Ok(false);
        }
        entry.record.lease_expires_at = Some(expires_at);
        Ok(true)
    }

    fn transition_task(
        &self,
        tenant: &TenantScope,
        req: &TaskTransitionRequest,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
        let mut guard = lock_unpoisoned(&self.tenants);
        let entry = guard
            .get_mut(tenant.tenant().as_str())
            .and_then(|m| m.get_mut(&req.task_id))
            .ok_or(StoreError::NotFound)?;

        if !entry.record.status.can_transition_to(req.to_status) {
            return Err(StoreError::Conflict);
        }

        let next_seq = entry.events.last().map_or(1, |e| e.seq.saturating_add(1));

        let mut next_record = entry.record.clone();
        next_record.status = req.to_status;
        if let Some(ref payload_json) = req.payload_json {
            next_record.payload_json = payload_json.clone();
        }
        if let Some(ref owner) = req.lease_owner {
            next_record.lease_owner = owner.clone();
        } else if req.to_status != TaskStatus::Working {
            next_record.lease_owner = None;
        }
        if let Some(exp) = req.lease_expires_at {
            next_record.lease_expires_at = exp;
        } else if req.to_status != TaskStatus::Working {
            next_record.lease_expires_at = None;
        }
        next_record.updated_at = req.now;

        let event = TaskEventRecord {
            task_id: req.task_id,
            seq: next_seq,
            status: req.to_status,
            payload_json: req.event_payload_json.clone(),
            emitted_at: req.now,
        };

        entry.record = next_record.clone();
        entry.events.push(event.clone());
        Ok((next_record, event))
    }

    fn list_tasks(
        &self,
        tenant: &TenantScope,
        filter: &TaskQueryFilter,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.tenants);
        let Some(bucket) = guard.get(tenant.tenant().as_str()) else {
            return Ok(Vec::new());
        };
        let mut items: Vec<TaskRecord> = bucket
            .values()
            .map(|t| t.record.clone())
            .filter(|rec| filter.status.is_none_or(|s| rec.status == s))
            .collect();
        if let Some(limit) = filter.limit {
            items.truncate(limit);
        }
        Ok(items)
    }

    fn claim_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError> {
        if worker_id.trim().is_empty() {
            return Err(StoreError::Invalid {
                field: "worker_id",
                reason: "worker_id must not be empty",
            });
        }
        let mut guard = lock_unpoisoned(&self.tenants);
        let Some(entry) = guard
            .get_mut(tenant.tenant().as_str())
            .and_then(|m| m.get_mut(&task_id))
        else {
            return Ok(false);
        };
        if entry.record.status != TaskStatus::Working {
            return Ok(false);
        }
        let can_claim = match (&entry.record.lease_owner, entry.record.lease_expires_at) {
            (None, _) => true,
            (Some(owner), _) if owner == worker_id => true,
            (Some(_), Some(exp)) if exp <= now => true,
            (Some(_), None) => true,
            (Some(_), Some(_)) => false,
        };
        if !can_claim {
            return Ok(false);
        }
        entry.record.lease_owner = Some(worker_id.to_owned());
        entry.record.lease_expires_at = Some(expires_at);
        Ok(true)
    }

    fn list_recoverable_tasks(&self, tenant: &TenantScope) -> Result<Vec<TaskRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.tenants);
        let Some(bucket) = guard.get(tenant.tenant().as_str()) else {
            return Ok(Vec::new());
        };
        Ok(bucket
            .values()
            .map(|t| t.record.clone())
            .filter(|rec| matches!(rec.status, TaskStatus::Submitted | TaskStatus::Working))
            .collect())
    }

    fn expire_stale_approvals(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        approval_ttl_ms: i64,
    ) -> Result<Vec<(TaskRecord, TaskEventRecord)>, StoreError> {
        if approval_ttl_ms <= 0 {
            return Err(StoreError::Invalid {
                field: "approval_ttl_ms",
                reason: "approval TTL must be positive",
            });
        }
        let cutoff_ms = now.unix_millis().saturating_sub(approval_ttl_ms);
        let mut guard = lock_unpoisoned(&self.tenants);
        let Some(bucket) = guard.get_mut(tenant.tenant().as_str()) else {
            return Ok(Vec::new());
        };

        let mut expired = Vec::new();
        for entry in bucket.values_mut() {
            if entry.record.status == TaskStatus::InputRequired
                && entry.record.updated_at.unix_millis() <= cutoff_ms
            {
                let next_seq = entry.events.last().map_or(1, |e| e.seq.saturating_add(1));
                let payload = r#"{"reason":"approval_expired"}"#.to_owned();
                entry.record.status = TaskStatus::Rejected;
                entry.record.lease_owner = None;
                entry.record.lease_expires_at = None;
                entry.record.updated_at = now;
                let event = TaskEventRecord {
                    task_id: entry.record.task_id,
                    seq: next_seq,
                    status: TaskStatus::Rejected,
                    payload_json: payload,
                    emitted_at: now,
                };
                entry.events.push(event.clone());
                expired.push((entry.record.clone(), event));
            }
        }
        Ok(expired)
    }

    fn prune_terminal_tasks(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        retention_ms: i64,
    ) -> Result<usize, StoreError> {
        if retention_ms <= 0 {
            return Err(StoreError::Invalid {
                field: "retention_ms",
                reason: "retention duration must be positive",
            });
        }
        let cutoff_ms = now.unix_millis().saturating_sub(retention_ms);
        let mut guard = lock_unpoisoned(&self.tenants);
        let Some(bucket) = guard.get_mut(tenant.tenant().as_str()) else {
            return Ok(0);
        };
        let before = bucket.len();
        bucket.retain(|_, entry| {
            !(entry.record.status.is_terminal()
                && entry.record.updated_at.unix_millis() <= cutoff_ms)
        });
        Ok(before.saturating_sub(bucket.len()))
    }

    fn checkpoint_progress(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        payload_json: &str,
        event_payload_json: &str,
        now: Timestamp,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
        let mut guard = lock_unpoisoned(&self.tenants);
        let entry = guard
            .get_mut(tenant.tenant().as_str())
            .and_then(|m| m.get_mut(&task_id))
            .ok_or(StoreError::NotFound)?;

        if entry.record.status != TaskStatus::Working
            || entry.record.lease_owner.as_deref() != Some(worker_id)
        {
            return Err(StoreError::Conflict);
        }

        let next_seq = entry.events.last().map_or(1, |e| e.seq.saturating_add(1));
        entry.record.payload_json = payload_json.to_owned();
        entry.record.updated_at = now;
        let event = TaskEventRecord {
            task_id,
            seq: next_seq,
            status: TaskStatus::Working,
            payload_json: event_payload_json.to_owned(),
            emitted_at: now,
        };
        entry.events.push(event.clone());
        Ok((entry.record.clone(), event))
    }
}
