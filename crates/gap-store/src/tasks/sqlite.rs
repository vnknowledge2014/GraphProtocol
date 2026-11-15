//! SQLite WAL-backed durable [`TaskStore`] (`RUN-080`..`RUN-086`, `INV-09`, `INV-13`).

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::error::StoreError;
use crate::sqlite::{DEFAULT_BUSY_TIMEOUT_MS, lock_unpoisoned, map_sqlite_err};

use super::{
    TaskEventRecord, TaskQueryFilter, TaskRecord, TaskStatus, TaskStore, TaskTransitionRequest,
};

const TASKS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    tenant TEXT NOT NULL,
    task_id TEXT NOT NULL,
    status TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    lease_owner TEXT,
    lease_expires_at_rfc3339 TEXT,
    lease_expires_at_ms INTEGER,
    updated_at_rfc3339 TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    last_seq INTEGER NOT NULL,
    PRIMARY KEY (tenant, task_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_tasks_tenant_status_updated
    ON tasks (tenant, status, updated_at_ms);

CREATE TABLE IF NOT EXISTS task_events (
    tenant TEXT NOT NULL,
    task_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    status TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    emitted_at_rfc3339 TEXT NOT NULL,
    PRIMARY KEY (tenant, task_id, seq)
) WITHOUT ROWID;
";

/// SQLite WAL + `synchronous = FULL` durable [`TaskStore`] partitioned by `(tenant, task_id)` (`RUN-080`, `INV-09`, `INV-13`).
#[derive(Debug)]
pub struct SqliteTaskStore {
    conn: Mutex<Connection>,
}

impl SqliteTaskStore {
    /// Opens or initializes a WAL-mode SQLite task database at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if parent directory creation, WAL configuration, or DDL migration fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| StoreError::Backend)?;
        }
        let conn = Connection::open(path).map_err(map_sqlite_err)?;
        configure_task_connection(&conn)?;
        conn.execute_batch(TASKS_SCHEMA_SQL)
            .map_err(map_sqlite_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Creates an in-memory SQLite WAL/synchronous-equivalent task store for fast unit tests.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if initialization fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(map_sqlite_err)?;
        conn.busy_timeout(Duration::from_millis(u64::from(DEFAULT_BUSY_TIMEOUT_MS)))
            .map_err(map_sqlite_err)?;
        conn.execute_batch(
            "PRAGMA trusted_schema = OFF;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = FULL;",
        )
        .map_err(map_sqlite_err)?;
        conn.execute_batch(TASKS_SCHEMA_SQL)
            .map_err(map_sqlite_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

fn configure_task_connection(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(u64::from(DEFAULT_BUSY_TIMEOUT_MS)))
        .map_err(map_sqlite_err)?;
    conn.execute_batch(
        "PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(map_sqlite_err)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Backend);
    }
    conn.execute_batch("PRAGMA synchronous = FULL;")
        .map_err(map_sqlite_err)?;
    Ok(())
}

fn parse_status(raw: &str) -> Result<TaskStatus, StoreError> {
    TaskStatus::from_wire_str(raw).ok_or(StoreError::Corrupt)
}

fn parse_timestamp(raw: &str) -> Result<Timestamp, StoreError> {
    raw.parse::<Timestamp>().map_err(|_| StoreError::Corrupt)
}

fn parse_task_id(raw: &str) -> Result<TaskId, StoreError> {
    raw.parse::<TaskId>().map_err(|_| StoreError::Corrupt)
}

fn seq_to_i64(seq: u64) -> Result<i64, StoreError> {
    i64::try_from(seq).map_err(|_| StoreError::Invalid {
        field: "seq",
        reason: "sequence number exceeds SQLite integer range",
    })
}

fn row_to_task_record(row: &rusqlite::Row<'_>) -> Result<TaskRecord, rusqlite::Error> {
    let task_id_raw: String = row.get(0)?;
    let status_raw: String = row.get(1)?;
    let payload_json: String = row.get(2)?;
    let lease_owner: Option<String> = row.get(3)?;
    let lease_exp_raw: Option<String> = row.get(4)?;
    let updated_raw: String = row.get(5)?;

    let task_id = task_id_raw.parse::<TaskId>().map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, "task_id".to_owned(), rusqlite::types::Type::Text)
    })?;
    let status = TaskStatus::from_wire_str(&status_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(1, "status".to_owned(), rusqlite::types::Type::Text)
    })?;
    let lease_expires_at = match lease_exp_raw {
        Some(s) => Some(s.parse::<Timestamp>().map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                4,
                "lease_expires_at".to_owned(),
                rusqlite::types::Type::Text,
            )
        })?),
        None => None,
    };
    let updated_at = updated_raw.parse::<Timestamp>().map_err(|_| {
        rusqlite::Error::InvalidColumnType(5, "updated_at".to_owned(), rusqlite::types::Type::Text)
    })?;

    Ok(TaskRecord {
        task_id,
        status,
        payload_json,
        lease_owner,
        lease_expires_at,
        updated_at,
    })
}

impl TaskStore for SqliteTaskStore {
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
        let seq_i64 = seq_to_i64(event.seq)?;
        let tenant_str = tenant.tenant().as_str();
        let task_id_str = record.task_id.to_string();
        let status_str = record.status.as_wire_str();
        let lease_exp_rfc = record.lease_expires_at.map(|t| t.to_string());
        let lease_exp_ms = record.lease_expires_at.map(Timestamp::unix_millis);
        let updated_rfc = record.updated_at.to_string();
        let updated_ms = record.updated_at.unix_millis();
        let emitted_rfc = event.emitted_at.to_string();

        let mut guard = lock_unpoisoned(&self.conn);
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT status, last_seq FROM tasks WHERE tenant = ?1 AND task_id = ?2",
                params![tenant_str, &task_id_str],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        match existing {
            Some((prev_status_raw, last_seq)) => {
                let prev_status = parse_status(&prev_status_raw)?;
                if !prev_status.can_transition_to(record.status) {
                    return Err(StoreError::Conflict);
                }
                let expected_seq = last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
                if seq_i64 != expected_seq {
                    return Err(StoreError::Conflict);
                }
                tx.execute(
                    "UPDATE tasks
                     SET status = ?1, payload_json = ?2, lease_owner = ?3,
                         lease_expires_at_rfc3339 = ?4, lease_expires_at_ms = ?5,
                         updated_at_rfc3339 = ?6, updated_at_ms = ?7, last_seq = ?8
                     WHERE tenant = ?9 AND task_id = ?10",
                    params![
                        status_str,
                        &record.payload_json,
                        &record.lease_owner,
                        lease_exp_rfc,
                        lease_exp_ms,
                        updated_rfc,
                        updated_ms,
                        seq_i64,
                        tenant_str,
                        &task_id_str,
                    ],
                )
                .map_err(map_sqlite_err)?;
            }
            None => {
                if seq_i64 != 1 {
                    return Err(StoreError::Invalid {
                        field: "seq",
                        reason: "initial task event must start at seq 1",
                    });
                }
                tx.execute(
                    "INSERT INTO tasks (
                        tenant, task_id, status, payload_json, lease_owner,
                        lease_expires_at_rfc3339, lease_expires_at_ms,
                        updated_at_rfc3339, updated_at_ms, last_seq
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        tenant_str,
                        &task_id_str,
                        status_str,
                        &record.payload_json,
                        &record.lease_owner,
                        lease_exp_rfc,
                        lease_exp_ms,
                        updated_rfc,
                        updated_ms,
                        seq_i64,
                    ],
                )
                .map_err(map_sqlite_err)?;
            }
        }

        tx.execute(
            "INSERT INTO task_events (tenant, task_id, seq, status, payload_json, emitted_at_rfc3339)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                tenant_str,
                &task_id_str,
                seq_i64,
                status_str,
                &event.payload_json,
                emitted_rfc,
            ],
        )
        .map_err(map_sqlite_err)?;

        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    fn get_task(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<Option<TaskRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.conn);
        let tenant_str = tenant.tenant().as_str();
        let task_id_str = task_id.to_string();
        guard
            .query_row(
                "SELECT task_id, status, payload_json, lease_owner,
                        lease_expires_at_rfc3339, updated_at_rfc3339
                 FROM tasks
                 WHERE tenant = ?1 AND task_id = ?2",
                params![tenant_str, &task_id_str],
                row_to_task_record,
            )
            .optional()
            .map_err(map_sqlite_err)
    }

    fn events_from(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        from_seq: u64,
    ) -> Result<Vec<TaskEventRecord>, StoreError> {
        let seq_i64 = seq_to_i64(from_seq)?;
        let guard = lock_unpoisoned(&self.conn);
        let tenant_str = tenant.tenant().as_str();
        let task_id_str = task_id.to_string();
        let mut stmt = guard
            .prepare_cached(
                "SELECT task_id, seq, status, payload_json, emitted_at_rfc3339
                 FROM task_events
                 WHERE tenant = ?1 AND task_id = ?2 AND seq >= ?3
                 ORDER BY seq ASC",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![tenant_str, &task_id_str, seq_i64], |row| {
                let tid_raw: String = row.get(0)?;
                let seq: i64 = row.get(1)?;
                let status_raw: String = row.get(2)?;
                let payload_json: String = row.get(3)?;
                let emitted_raw: String = row.get(4)?;
                Ok((tid_raw, seq, status_raw, payload_json, emitted_raw))
            })
            .map_err(map_sqlite_err)?;

        let mut events = Vec::new();
        for item in rows {
            let (tid_raw, seq, status_raw, payload_json, emitted_raw) =
                item.map_err(map_sqlite_err)?;
            let seq_u64 = u64::try_from(seq).map_err(|_| StoreError::Corrupt)?;
            events.push(TaskEventRecord {
                task_id: parse_task_id(&tid_raw)?,
                seq: seq_u64,
                status: parse_status(&status_raw)?,
                payload_json,
                emitted_at: parse_timestamp(&emitted_raw)?,
            });
        }
        Ok(events)
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
        let guard = lock_unpoisoned(&self.conn);
        let changed = guard
            .execute(
                "UPDATE tasks
                 SET lease_expires_at_rfc3339 = ?1, lease_expires_at_ms = ?2
                 WHERE tenant = ?3 AND task_id = ?4 AND status = ?5 AND lease_owner = ?6",
                params![
                    expires_at.to_string(),
                    expires_at.unix_millis(),
                    tenant.tenant().as_str(),
                    task_id.to_string(),
                    TaskStatus::Working.as_wire_str(),
                    worker_id,
                ],
            )
            .map_err(map_sqlite_err)?;
        Ok(changed > 0)
    }

    fn transition_task(
        &self,
        tenant: &TenantScope,
        req: &TaskTransitionRequest,
    ) -> Result<(TaskRecord, TaskEventRecord), StoreError> {
        let tenant_str = tenant.tenant().as_str();
        let task_id_str = req.task_id.to_string();
        let mut guard = lock_unpoisoned(&self.conn);
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let row: Option<(TaskRecord, i64)> = tx
            .query_row(
                "SELECT task_id, status, payload_json, lease_owner,
                        lease_expires_at_rfc3339, updated_at_rfc3339, last_seq
                 FROM tasks
                 WHERE tenant = ?1 AND task_id = ?2",
                params![tenant_str, &task_id_str],
                |row| {
                    let rec = row_to_task_record(row)?;
                    let last_seq: i64 = row.get(6)?;
                    Ok((rec, last_seq))
                },
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let (mut record, last_seq) = row.ok_or(StoreError::NotFound)?;
        if !record.status.can_transition_to(req.to_status) {
            return Err(StoreError::Conflict);
        }

        let next_seq_i64 = last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
        let next_seq_u64 = u64::try_from(next_seq_i64).map_err(|_| StoreError::Corrupt)?;

        record.status = req.to_status;
        if let Some(ref payload) = req.payload_json {
            record.payload_json = payload.clone();
        }
        if let Some(ref owner) = req.lease_owner {
            record.lease_owner = owner.clone();
        } else if req.to_status != TaskStatus::Working {
            record.lease_owner = None;
        }
        if let Some(exp) = req.lease_expires_at {
            record.lease_expires_at = exp;
        } else if req.to_status != TaskStatus::Working {
            record.lease_expires_at = None;
        }
        record.updated_at = req.now;

        let lease_exp_rfc = record.lease_expires_at.map(|t| t.to_string());
        let lease_exp_ms = record.lease_expires_at.map(Timestamp::unix_millis);
        let updated_rfc = record.updated_at.to_string();
        let updated_ms = record.updated_at.unix_millis();

        tx.execute(
            "UPDATE tasks
             SET status = ?1, payload_json = ?2, lease_owner = ?3,
                 lease_expires_at_rfc3339 = ?4, lease_expires_at_ms = ?5,
                 updated_at_rfc3339 = ?6, updated_at_ms = ?7, last_seq = ?8
             WHERE tenant = ?9 AND task_id = ?10",
            params![
                record.status.as_wire_str(),
                &record.payload_json,
                &record.lease_owner,
                lease_exp_rfc,
                lease_exp_ms,
                &updated_rfc,
                updated_ms,
                next_seq_i64,
                tenant_str,
                &task_id_str,
            ],
        )
        .map_err(map_sqlite_err)?;

        let event = TaskEventRecord {
            task_id: req.task_id,
            seq: next_seq_u64,
            status: req.to_status,
            payload_json: req.event_payload_json.clone(),
            emitted_at: req.now,
        };

        tx.execute(
            "INSERT INTO task_events (tenant, task_id, seq, status, payload_json, emitted_at_rfc3339)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                tenant_str,
                &task_id_str,
                next_seq_i64,
                event.status.as_wire_str(),
                &event.payload_json,
                &updated_rfc,
            ],
        )
        .map_err(map_sqlite_err)?;

        tx.commit().map_err(map_sqlite_err)?;
        Ok((record, event))
    }

    fn list_tasks(
        &self,
        tenant: &TenantScope,
        filter: &TaskQueryFilter,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.conn);
        let tenant_str = tenant.tenant().as_str();
        let limit_i64 = filter
            .limit
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
            .unwrap_or(-1);

        let mut out = Vec::new();
        match filter.status {
            Some(status) => {
                let mut stmt = guard
                    .prepare_cached(
                        "SELECT task_id, status, payload_json, lease_owner,
                                lease_expires_at_rfc3339, updated_at_rfc3339
                         FROM tasks
                         WHERE tenant = ?1 AND status = ?2
                         ORDER BY task_id ASC
                         LIMIT ?3",
                    )
                    .map_err(map_sqlite_err)?;
                let rows = stmt
                    .query_map(
                        params![tenant_str, status.as_wire_str(), limit_i64],
                        row_to_task_record,
                    )
                    .map_err(map_sqlite_err)?;
                for row in rows {
                    out.push(row.map_err(map_sqlite_err)?);
                }
            }
            None => {
                let mut stmt = guard
                    .prepare_cached(
                        "SELECT task_id, status, payload_json, lease_owner,
                                lease_expires_at_rfc3339, updated_at_rfc3339
                         FROM tasks
                         WHERE tenant = ?1
                         ORDER BY task_id ASC
                         LIMIT ?2",
                    )
                    .map_err(map_sqlite_err)?;
                let rows = stmt
                    .query_map(params![tenant_str, limit_i64], row_to_task_record)
                    .map_err(map_sqlite_err)?;
                for row in rows {
                    out.push(row.map_err(map_sqlite_err)?);
                }
            }
        }
        Ok(out)
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
        let guard = lock_unpoisoned(&self.conn);
        let changed = guard
            .execute(
                "UPDATE tasks
                 SET lease_owner = ?1, lease_expires_at_rfc3339 = ?2, lease_expires_at_ms = ?3
                 WHERE tenant = ?4 AND task_id = ?5 AND status = ?6
                   AND (
                       lease_owner IS NULL
                       OR lease_owner = ?1
                       OR lease_expires_at_ms IS NULL
                       OR lease_expires_at_ms <= ?7
                   )",
                params![
                    worker_id,
                    expires_at.to_string(),
                    expires_at.unix_millis(),
                    tenant.tenant().as_str(),
                    task_id.to_string(),
                    TaskStatus::Working.as_wire_str(),
                    now.unix_millis(),
                ],
            )
            .map_err(map_sqlite_err)?;
        Ok(changed > 0)
    }

    fn list_recoverable_tasks(&self, tenant: &TenantScope) -> Result<Vec<TaskRecord>, StoreError> {
        let guard = lock_unpoisoned(&self.conn);
        let mut stmt = guard
            .prepare_cached(
                "SELECT task_id, status, payload_json, lease_owner,
                        lease_expires_at_rfc3339, updated_at_rfc3339
                 FROM tasks
                 WHERE tenant = ?1 AND status IN (?2, ?3)
                 ORDER BY task_id ASC",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(
                params![
                    tenant.tenant().as_str(),
                    TaskStatus::Submitted.as_wire_str(),
                    TaskStatus::Working.as_wire_str(),
                ],
                row_to_task_record,
            )
            .map_err(map_sqlite_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(map_sqlite_err)?);
        }
        Ok(out)
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
        let tenant_str = tenant.tenant().as_str();
        let now_rfc = now.to_string();
        let now_ms = now.unix_millis();

        let mut guard = lock_unpoisoned(&self.conn);
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let mut candidates: Vec<(TaskRecord, i64)> = Vec::new();
        {
            let mut stmt = tx
                .prepare_cached(
                    "SELECT task_id, status, payload_json, lease_owner,
                            lease_expires_at_rfc3339, updated_at_rfc3339, last_seq
                     FROM tasks
                     WHERE tenant = ?1 AND status = ?2 AND updated_at_ms <= ?3
                     ORDER BY task_id ASC",
                )
                .map_err(map_sqlite_err)?;
            let rows = stmt
                .query_map(
                    params![
                        tenant_str,
                        TaskStatus::InputRequired.as_wire_str(),
                        cutoff_ms
                    ],
                    |row| Ok((row_to_task_record(row)?, row.get::<_, i64>(6)?)),
                )
                .map_err(map_sqlite_err)?;
            for item in rows {
                candidates.push(item.map_err(map_sqlite_err)?);
            }
        }

        let mut out = Vec::with_capacity(candidates.len());
        let rejected_str = TaskStatus::Rejected.as_wire_str();
        let event_payload = r#"{"reason":"approval_expired"}"#;

        for (mut rec, last_seq) in candidates {
            let next_seq_i64 = last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
            let next_seq_u64 = u64::try_from(next_seq_i64).map_err(|_| StoreError::Corrupt)?;
            let tid_str = rec.task_id.to_string();

            rec.status = TaskStatus::Rejected;
            rec.lease_owner = None;
            rec.lease_expires_at = None;
            rec.updated_at = now;

            tx.execute(
                "UPDATE tasks
                 SET status = ?1, lease_owner = NULL, lease_expires_at_rfc3339 = NULL,
                     lease_expires_at_ms = NULL, updated_at_rfc3339 = ?2,
                     updated_at_ms = ?3, last_seq = ?4
                 WHERE tenant = ?5 AND task_id = ?6",
                params![
                    rejected_str,
                    &now_rfc,
                    now_ms,
                    next_seq_i64,
                    tenant_str,
                    &tid_str
                ],
            )
            .map_err(map_sqlite_err)?;

            let event = TaskEventRecord {
                task_id: rec.task_id,
                seq: next_seq_u64,
                status: TaskStatus::Rejected,
                payload_json: event_payload.to_owned(),
                emitted_at: now,
            };

            tx.execute(
                "INSERT INTO task_events (tenant, task_id, seq, status, payload_json, emitted_at_rfc3339)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    tenant_str,
                    &tid_str,
                    next_seq_i64,
                    rejected_str,
                    &event.payload_json,
                    &now_rfc,
                ],
            )
            .map_err(map_sqlite_err)?;

            out.push((rec, event));
        }

        tx.commit().map_err(map_sqlite_err)?;
        Ok(out)
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
        let tenant_str = tenant.tenant().as_str();
        let mut guard = lock_unpoisoned(&self.conn);
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        tx.execute(
            "DELETE FROM task_events
             WHERE tenant = ?1 AND task_id IN (
                 SELECT task_id FROM tasks
                 WHERE tenant = ?1
                   AND status IN (?2, ?3, ?4, ?5)
                   AND updated_at_ms <= ?6
             )",
            params![
                tenant_str,
                TaskStatus::Completed.as_wire_str(),
                TaskStatus::Failed.as_wire_str(),
                TaskStatus::Canceled.as_wire_str(),
                TaskStatus::Rejected.as_wire_str(),
                cutoff_ms,
            ],
        )
        .map_err(map_sqlite_err)?;

        let deleted = tx
            .execute(
                "DELETE FROM tasks
                 WHERE tenant = ?1
                   AND status IN (?2, ?3, ?4, ?5)
                   AND updated_at_ms <= ?6",
                params![
                    tenant_str,
                    TaskStatus::Completed.as_wire_str(),
                    TaskStatus::Failed.as_wire_str(),
                    TaskStatus::Canceled.as_wire_str(),
                    TaskStatus::Rejected.as_wire_str(),
                    cutoff_ms,
                ],
            )
            .map_err(map_sqlite_err)?;

        tx.commit().map_err(map_sqlite_err)?;
        Ok(deleted)
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
        let tenant_str = tenant.tenant().as_str();
        let tid_str = task_id.to_string();
        let now_rfc = now.to_string();
        let now_ms = now.unix_millis();

        let mut guard = lock_unpoisoned(&self.conn);
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let row_opt: Option<(TaskRecord, i64)> = tx
            .query_row(
                "SELECT task_id, status, payload_json, lease_owner, lease_expires_at_rfc3339,
                        updated_at_rfc3339, last_seq
                 FROM tasks
                 WHERE tenant = ?1 AND task_id = ?2",
                params![tenant_str, &tid_str],
                |row| Ok((row_to_task_record(row)?, row.get(6)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let (mut rec, last_seq) = row_opt.ok_or(StoreError::NotFound)?;
        if rec.status != TaskStatus::Working || rec.lease_owner.as_deref() != Some(worker_id) {
            return Err(StoreError::Conflict);
        }

        let next_seq_i64 = last_seq.checked_add(1).ok_or(StoreError::Conflict)?;
        let next_seq_u64 = u64::try_from(next_seq_i64).map_err(|_| StoreError::Corrupt)?;

        rec.payload_json = payload_json.to_owned();
        rec.updated_at = now;

        tx.execute(
            "UPDATE tasks
             SET payload_json = ?1, updated_at_rfc3339 = ?2, updated_at_ms = ?3, last_seq = ?4
             WHERE tenant = ?5 AND task_id = ?6",
            params![
                &rec.payload_json,
                &now_rfc,
                now_ms,
                next_seq_i64,
                tenant_str,
                &tid_str,
            ],
        )
        .map_err(map_sqlite_err)?;

        let event = TaskEventRecord {
            task_id,
            seq: next_seq_u64,
            status: TaskStatus::Working,
            payload_json: event_payload_json.to_owned(),
            emitted_at: now,
        };
        tx.execute(
            "INSERT INTO task_events (tenant, task_id, seq, status, payload_json, emitted_at_rfc3339)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                tenant_str,
                &tid_str,
                next_seq_i64,
                TaskStatus::Working.as_wire_str(),
                &event.payload_json,
                &now_rfc,
            ],
        )
        .map_err(map_sqlite_err)?;

        tx.commit().map_err(map_sqlite_err)?;
        Ok((rec, event))
    }
}
