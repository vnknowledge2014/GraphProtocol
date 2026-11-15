//! Durable [`TaskStore`] implementation backed by the SurrealDB 3.x state model (`ADR-020`,
//! `RUN-080`..`RUN-086`, `INV-09`).

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};

use crate::error::StoreError;
use crate::surreal::context::SurrealContextStore;
use crate::surreal::transport::{SharedEngine, StoredTask};
use crate::tasks::{
    TaskEventRecord, TaskQueryFilter, TaskRecord, TaskStatus, TaskStore, TaskTransitionRequest,
};

/// Standalone SurrealDB-backed [`TaskStore`] handle sharing the transactional engine with
/// [`SurrealContextStore`] (`ADR-020`).
#[derive(Debug, Clone)]
pub struct SurrealTaskStore {
    engine: SharedEngine,
}

impl SurrealTaskStore {
    /// Creates an isolated in-memory [`SurrealTaskStore`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: std::sync::Arc::new(crate::surreal::transport::InMemorySurrealEngine::new()),
        }
    }

    /// Creates a [`SurrealTaskStore`] attached to an existing [`SurrealContextStore`].
    #[must_use]
    pub fn from_context_store(context: &SurrealContextStore) -> Self {
        Self {
            engine: context.shared_engine(),
        }
    }
}

impl Default for SurrealTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskStore for SurrealTaskStore {
    fn put_task(
        &self,
        tenant: &TenantScope,
        record: &TaskRecord,
        event: &TaskEventRecord,
    ) -> Result<(), StoreError> {
        put_task_on_engine(&self.engine, tenant, record, event)
    }

    fn get_task(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<Option<TaskRecord>, StoreError> {
        get_task_on_engine(&self.engine, tenant, task_id)
    }

    fn events_from(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        from_seq: u64,
    ) -> Result<Vec<TaskEventRecord>, StoreError> {
        events_from_on_engine(&self.engine, tenant, task_id, from_seq)
    }

    fn renew_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError> {
        renew_lease_on_engine(&self.engine, tenant, task_id, worker_id, expires_at)
    }

    fn transition_task(
        &self,
        tenant: &TenantScope,
        req: &TaskTransitionRequest,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
        transition_task_on_engine(&self.engine, tenant, req)
    }

    fn list_tasks(
        &self,
        tenant: &TenantScope,
        filter: &TaskQueryFilter,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        list_tasks_on_engine(&self.engine, tenant, filter)
    }

    fn claim_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError> {
        claim_lease_on_engine(&self.engine, tenant, task_id, worker_id, now, expires_at)
    }

    fn list_recoverable_tasks(&self, tenant: &TenantScope) -> Result<Vec<TaskRecord>, StoreError> {
        list_recoverable_tasks_on_engine(&self.engine, tenant)
    }

    fn expire_stale_approvals(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        approval_ttl_ms: i64,
    ) -> Result<Vec<(TaskRecord, TaskEventRecord)>, StoreError> {
        expire_stale_approvals_on_engine(&self.engine, tenant, now, approval_ttl_ms)
    }

    fn prune_terminal_tasks(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        retention_ms: i64,
    ) -> Result<usize, StoreError> {
        prune_terminal_tasks_on_engine(&self.engine, tenant, now, retention_ms)
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
        checkpoint_progress_on_engine(
            &self.engine,
            tenant,
            task_id,
            worker_id,
            payload_json,
            event_payload_json,
            now,
        )
    }
}

impl TaskStore for SurrealContextStore {
    fn put_task(
        &self,
        tenant: &TenantScope,
        record: &TaskRecord,
        event: &TaskEventRecord,
    ) -> Result<(), StoreError> {
        put_task_on_engine(&self.engine, tenant, record, event)
    }

    fn get_task(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<Option<TaskRecord>, StoreError> {
        get_task_on_engine(&self.engine, tenant, task_id)
    }

    fn events_from(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        from_seq: u64,
    ) -> Result<Vec<TaskEventRecord>, StoreError> {
        events_from_on_engine(&self.engine, tenant, task_id, from_seq)
    }

    fn renew_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError> {
        renew_lease_on_engine(&self.engine, tenant, task_id, worker_id, expires_at)
    }

    fn transition_task(
        &self,
        tenant: &TenantScope,
        req: &TaskTransitionRequest,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
        transition_task_on_engine(&self.engine, tenant, req)
    }

    fn list_tasks(
        &self,
        tenant: &TenantScope,
        filter: &TaskQueryFilter,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        list_tasks_on_engine(&self.engine, tenant, filter)
    }

    fn claim_lease(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        worker_id: &str,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<bool, StoreError> {
        claim_lease_on_engine(&self.engine, tenant, task_id, worker_id, now, expires_at)
    }

    fn list_recoverable_tasks(&self, tenant: &TenantScope) -> Result<Vec<TaskRecord>, StoreError> {
        list_recoverable_tasks_on_engine(&self.engine, tenant)
    }

    fn expire_stale_approvals(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        approval_ttl_ms: i64,
    ) -> Result<Vec<(TaskRecord, TaskEventRecord)>, StoreError> {
        expire_stale_approvals_on_engine(&self.engine, tenant, now, approval_ttl_ms)
    }

    fn prune_terminal_tasks(
        &self,
        tenant: &TenantScope,
        now: Timestamp,
        retention_ms: i64,
    ) -> Result<usize, StoreError> {
        prune_terminal_tasks_on_engine(&self.engine, tenant, now, retention_ms)
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
        let _ = self.clock.unix_millis();
        checkpoint_progress_on_engine(
            &self.engine,
            tenant,
            task_id,
            worker_id,
            payload_json,
            event_payload_json,
            now,
        )
    }
}

fn put_task_on_engine(
    engine: &SharedEngine,
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
    let _ = i64::try_from(event.seq).map_err(|_| StoreError::Invalid {
        field: "seq",
        reason: "sequence number exceeds i64 range",
    })?;

    let mut state = engine.write()?;
    let tenant_str = tenant.tenant().as_str().to_owned();
    let tasks = state.tasks_by_tenant.entry(tenant_str.clone()).or_default();

    if let Some(existing) = tasks.get_mut(&record.task_id) {
        if !existing.record.status.can_transition_to(record.status) {
            return Err(StoreError::Conflict);
        }
        let expected_seq = existing
            .last_seq
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        if event.seq != expected_seq {
            return Err(StoreError::Conflict);
        }
        existing.record = record.clone();
        existing.last_seq = event.seq;
    } else {
        if event.seq != 1 {
            return Err(StoreError::Invalid {
                field: "seq",
                reason: "initial task event must start at seq 1",
            });
        }
        tasks.insert(
            record.task_id,
            StoredTask {
                record: record.clone(),
                last_seq: event.seq,
            },
        );
    }

    state
        .task_events_by_tenant
        .entry(tenant_str)
        .or_default()
        .entry(record.task_id)
        .or_default()
        .push(event.clone());
    Ok(())
}

fn get_task_on_engine(
    engine: &SharedEngine,
    tenant: &TenantScope,
    task_id: TaskId,
) -> Result<Option<TaskRecord>, StoreError> {
    let state = engine.read()?;
    Ok(state
        .tasks_by_tenant
        .get(tenant.tenant().as_str())
        .and_then(|m| m.get(&task_id))
        .map(|s| s.record.clone()))
}

fn events_from_on_engine(
    engine: &SharedEngine,
    tenant: &TenantScope,
    task_id: TaskId,
    from_seq: u64,
) -> Result<Vec<TaskEventRecord>, StoreError> {
    let _ = i64::try_from(from_seq).map_err(|_| StoreError::Invalid {
        field: "seq",
        reason: "sequence number exceeds i64 range",
    })?;
    let state = engine.read()?;
    let Some(events) = state
        .task_events_by_tenant
        .get(tenant.tenant().as_str())
        .and_then(|m| m.get(&task_id))
    else {
        return Ok(Vec::new());
    };
    let mut out: Vec<TaskEventRecord> = events
        .iter()
        .filter(|e| e.seq >= from_seq)
        .cloned()
        .collect();
    out.sort_by_key(|e| e.seq);
    Ok(out)
}

fn renew_lease_on_engine(
    engine: &SharedEngine,
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
    let mut state = engine.write()?;
    let Some(stored) = state
        .tasks_by_tenant
        .get_mut(tenant.tenant().as_str())
        .and_then(|m| m.get_mut(&task_id))
    else {
        return Ok(false);
    };
    if stored.record.status == TaskStatus::Working
        && stored.record.lease_owner.as_deref() == Some(worker_id)
    {
        stored.record.lease_expires_at = Some(expires_at);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn transition_task_on_engine(
    engine: &SharedEngine,
    tenant: &TenantScope,
    req: &TaskTransitionRequest,
) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
    let mut state = engine.write()?;
    let tenant_str = tenant.tenant().as_str().to_owned();
    let Some(tasks) = state.tasks_by_tenant.get_mut(&tenant_str) else {
        return Err(StoreError::NotFound);
    };
    let Some(stored) = tasks.get_mut(&req.task_id) else {
        return Err(StoreError::NotFound);
    };
    if !stored.record.status.can_transition_to(req.to_status) {
        return Err(StoreError::Conflict);
    }
    let next_seq = stored.last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
    stored.last_seq = next_seq;

    stored.record.status = req.to_status;
    if let Some(ref payload) = req.payload_json {
        stored.record.payload_json = payload.clone();
    }
    if let Some(ref owner) = req.lease_owner {
        stored.record.lease_owner = owner.clone();
    } else if req.to_status != TaskStatus::Working {
        stored.record.lease_owner = None;
    }
    if let Some(exp) = req.lease_expires_at {
        stored.record.lease_expires_at = exp;
    } else if req.to_status != TaskStatus::Working {
        stored.record.lease_expires_at = None;
    }
    stored.record.updated_at = req.now;
    let updated_record = stored.record.clone();

    let event = TaskEventRecord {
        task_id: req.task_id,
        seq: next_seq,
        status: req.to_status,
        payload_json: req.event_payload_json.clone(),
        emitted_at: req.now,
    };
    state
        .task_events_by_tenant
        .entry(tenant_str)
        .or_default()
        .entry(req.task_id)
        .or_default()
        .push(event.clone());
    Ok((updated_record, event))
}

fn list_tasks_on_engine(
    engine: &SharedEngine,
    tenant: &TenantScope,
    filter: &TaskQueryFilter,
) -> Result<Vec<TaskRecord>, StoreError> {
    let state = engine.read()?;
    let Some(tasks) = state.tasks_by_tenant.get(tenant.tenant().as_str()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<TaskRecord> = tasks
        .values()
        .filter(|s| filter.status.is_none_or(|st| s.record.status == st))
        .map(|s| s.record.clone())
        .collect();
    out.sort_by_key(|r| r.task_id.to_string());
    if let Some(limit) = filter.limit {
        out.truncate(limit);
    }
    Ok(out)
}

fn claim_lease_on_engine(
    engine: &SharedEngine,
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
    let mut state = engine.write()?;
    let Some(stored) = state
        .tasks_by_tenant
        .get_mut(tenant.tenant().as_str())
        .and_then(|m| m.get_mut(&task_id))
    else {
        return Ok(false);
    };
    let can_claim = stored.record.status == TaskStatus::Working
        && (stored.record.lease_owner.is_none()
            || stored.record.lease_owner.as_deref() == Some(worker_id)
            || stored
                .record
                .lease_expires_at
                .is_none_or(|exp| exp.unix_millis() <= now.unix_millis()));
    if can_claim {
        stored.record.lease_owner = Some(worker_id.to_owned());
        stored.record.lease_expires_at = Some(expires_at);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn list_recoverable_tasks_on_engine(
    engine: &SharedEngine,
    tenant: &TenantScope,
) -> Result<Vec<TaskRecord>, StoreError> {
    let state = engine.read()?;
    let Some(tasks) = state.tasks_by_tenant.get(tenant.tenant().as_str()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<TaskRecord> = tasks
        .values()
        .filter(|s| matches!(s.record.status, TaskStatus::Submitted | TaskStatus::Working))
        .map(|s| s.record.clone())
        .collect();
    out.sort_by_key(|r| r.task_id.to_string());
    Ok(out)
}

fn expire_stale_approvals_on_engine(
    engine: &SharedEngine,
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
    let mut state = engine.write()?;
    let tenant_str = tenant.tenant().as_str().to_owned();
    let Some(tasks) = state.tasks_by_tenant.get_mut(&tenant_str) else {
        return Ok(Vec::new());
    };

    let mut expired_ids: Vec<(String, TaskId)> = tasks
        .iter()
        .filter(|(_, s)| {
            s.record.status == TaskStatus::InputRequired
                && s.record.updated_at.unix_millis() <= cutoff_ms
        })
        .map(|(tid, _)| (tid.to_string(), *tid))
        .collect();
    expired_ids.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::with_capacity(expired_ids.len());
    for (_, tid) in expired_ids {
        if let Some(stored) = tasks.get_mut(&tid) {
            let next_seq = stored.last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
            stored.last_seq = next_seq;
            stored.record.status = TaskStatus::Rejected;
            stored.record.lease_owner = None;
            stored.record.lease_expires_at = None;
            stored.record.updated_at = now;
            let ev = TaskEventRecord {
                task_id: tid,
                seq: next_seq,
                status: TaskStatus::Rejected,
                payload_json: r#"{"reason":"approval_expired"}"#.to_owned(),
                emitted_at: now,
            };
            out.push((stored.record.clone(), ev));
        }
    }

    let events_map = state.task_events_by_tenant.entry(tenant_str).or_default();
    for (rec, ev) in &out {
        events_map.entry(rec.task_id).or_default().push(ev.clone());
    }
    Ok(out)
}

fn prune_terminal_tasks_on_engine(
    engine: &SharedEngine,
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
    let mut state = engine.write()?;
    let tenant_str = tenant.tenant().as_str();
    let mut pruned_ids = Vec::new();
    if let Some(tasks) = state.tasks_by_tenant.get_mut(tenant_str) {
        tasks.retain(|tid, s| {
            let terminal = matches!(
                s.record.status,
                TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Canceled
                    | TaskStatus::Rejected
            );
            if terminal && s.record.updated_at.unix_millis() <= cutoff_ms {
                pruned_ids.push(*tid);
                false
            } else {
                true
            }
        });
    }
    if let Some(events) = state.task_events_by_tenant.get_mut(tenant_str) {
        for tid in &pruned_ids {
            events.remove(tid);
        }
    }
    Ok(pruned_ids.len())
}

fn checkpoint_progress_on_engine(
    engine: &SharedEngine,
    tenant: &TenantScope,
    task_id: TaskId,
    worker_id: &str,
    payload_json: &str,
    event_payload_json: &str,
    now: Timestamp,
) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
    let mut state = engine.write()?;
    let tenant_str = tenant.tenant().as_str().to_owned();
    let Some(tasks) = state.tasks_by_tenant.get_mut(&tenant_str) else {
        return Err(StoreError::NotFound);
    };
    let Some(stored) = tasks.get_mut(&task_id) else {
        return Err(StoreError::NotFound);
    };
    if stored.record.status != TaskStatus::Working
        || stored.record.lease_owner.as_deref() != Some(worker_id)
    {
        return Err(StoreError::Conflict);
    }
    let next_seq = stored.last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
    stored.last_seq = next_seq;
    stored.record.payload_json = payload_json.to_owned();
    stored.record.updated_at = now;
    let rec = stored.record.clone();
    let ev = TaskEventRecord {
        task_id,
        seq: next_seq,
        status: TaskStatus::Working,
        payload_json: event_payload_json.to_owned(),
        emitted_at: now,
    };
    state
        .task_events_by_tenant
        .entry(tenant_str)
        .or_default()
        .entry(task_id)
        .or_default()
        .push(ev.clone());
    Ok((rec, ev))
}
