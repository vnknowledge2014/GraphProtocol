//! SQLite WAL-backed durable [`IdempotencyStore`] partitioned by `(tenant, key)` (`RUN-072`, `INV-09`).

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::ids::TenantScope;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::error::StoreError;
use crate::sqlite::{DEFAULT_BUSY_TIMEOUT_MS, lock_unpoisoned, map_sqlite_err};

use super::{
    IdempotencyOutcome, IdempotencyRecord, IdempotencyStatus, IdempotencyStore,
    validate_idempotency_key,
};

const IDEMPOTENCY_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS idempotency_keys (
    tenant TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash BLOB NOT NULL,
    status TEXT NOT NULL,
    result_ref TEXT,
    expires_at_rfc3339 TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    PRIMARY KEY (tenant, idempotency_key)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_idempotency_tenant_expiry
    ON idempotency_keys (tenant, expires_at_ms);
";

/// SQLite WAL + `synchronous = FULL` durable [`IdempotencyStore`] (`RUN-072`, `INV-09`).
#[derive(Debug)]
pub struct SqliteIdempotencyStore {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
}

impl SqliteIdempotencyStore {
    /// Opens or initializes a WAL-mode SQLite idempotency database at `path` using [`SystemClock`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if opening or configuring SQLite fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_clock(path, Arc::new(SystemClock))
    }

    /// Opens or initializes a WAL-mode SQLite idempotency database at `path` with custom `clock`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if opening or configuring SQLite fails.
    pub fn open_with_clock(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| StoreError::Backend)?;
        }
        let conn = Connection::open(path).map_err(map_sqlite_err)?;
        configure_file_connection(&conn)?;
        conn.execute_batch(IDEMPOTENCY_SCHEMA_SQL)
            .map_err(map_sqlite_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// Creates an in-memory SQLite idempotency store for tests using [`SystemClock`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if initialization fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_in_memory_with_clock(Arc::new(SystemClock))
    }

    /// Creates an in-memory SQLite idempotency store with custom `clock` for deterministic TTL tests.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if initialization fails.
    pub fn open_in_memory_with_clock(clock: Arc<dyn Clock>) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(map_sqlite_err)?;
        conn.busy_timeout(Duration::from_millis(u64::from(DEFAULT_BUSY_TIMEOUT_MS)))
            .map_err(map_sqlite_err)?;
        conn.execute_batch(
            "PRAGMA trusted_schema = OFF;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = FULL;",
        )
        .map_err(map_sqlite_err)?;
        conn.execute_batch(IDEMPOTENCY_SCHEMA_SQL)
            .map_err(map_sqlite_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }
}

fn configure_file_connection(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(u64::from(DEFAULT_BUSY_TIMEOUT_MS)))
        .map_err(map_sqlite_err)?;
    conn.execute_batch(
        "PRAGMA trusted_schema = OFF;
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

fn decode_hash(blob: Vec<u8>) -> Result<[u8; 32], StoreError> {
    blob.try_into().map_err(|_| StoreError::Corrupt)
}

fn decode_timestamp(rfc3339: &str) -> Result<Timestamp, StoreError> {
    rfc3339.parse().map_err(|_| StoreError::Corrupt)
}

impl IdempotencyStore for SqliteIdempotencyStore {
    fn check_or_reserve(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
        expires_at: Timestamp,
    ) -> Result<IdempotencyOutcome, StoreError> {
        validate_idempotency_key(key)?;
        let now = self.clock.now_timestamp();
        let now_ms = now.unix_millis();
        let tenant_str = tenant.tenant().as_str();

        let mut conn = lock_unpoisoned(&self.conn);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let existing: Option<(Vec<u8>, String, Option<String>, i64)> = tx
            .query_row(
                "SELECT request_hash, status, result_ref, expires_at_ms
                 FROM idempotency_keys
                 WHERE tenant = ?1 AND idempotency_key = ?2",
                params![tenant_str, key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        if let Some((hash_blob, status_raw, result_ref, expires_at_ms)) = existing
            && expires_at_ms > now_ms
        {
            let existing_hash = decode_hash(hash_blob)?;
            if existing_hash != request_hash {
                tx.commit().map_err(map_sqlite_err)?;
                return Ok(IdempotencyOutcome::Conflict);
            }
            let status =
                IdempotencyStatus::from_storage_str(&status_raw).ok_or(StoreError::Corrupt)?;
            tx.commit().map_err(map_sqlite_err)?;
            return Ok(match status {
                IdempotencyStatus::InProgress => IdempotencyOutcome::InProgress,
                IdempotencyStatus::Completed => IdempotencyOutcome::Replay { result_ref },
            });
        }

        if expires_at <= now {
            return Err(StoreError::Invalid {
                field: "expires_at",
                reason: "must be in the future",
            });
        }

        let expires_rfc3339 = expires_at.to_string();
        let expires_ms = expires_at.unix_millis();
        tx.execute(
            "INSERT INTO idempotency_keys (
                tenant, idempotency_key, request_hash, status, result_ref,
                expires_at_rfc3339, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)
             ON CONFLICT (tenant, idempotency_key) DO UPDATE SET
                request_hash = excluded.request_hash,
                status = excluded.status,
                result_ref = NULL,
                expires_at_rfc3339 = excluded.expires_at_rfc3339,
                expires_at_ms = excluded.expires_at_ms",
            params![
                tenant_str,
                key,
                request_hash.as_slice(),
                IdempotencyStatus::InProgress.as_str(),
                expires_rfc3339,
                expires_ms,
            ],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(IdempotencyOutcome::Reserved)
    }

    fn complete(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
        result_ref: Option<&str>,
    ) -> Result<(), StoreError> {
        validate_idempotency_key(key)?;
        let now_ms = self.clock.now_timestamp().unix_millis();
        let tenant_str = tenant.tenant().as_str();

        let mut conn = lock_unpoisoned(&self.conn);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let existing: Option<(Vec<u8>, i64)> = tx
            .query_row(
                "SELECT request_hash, expires_at_ms
                 FROM idempotency_keys
                 WHERE tenant = ?1 AND idempotency_key = ?2",
                params![tenant_str, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let Some((hash_blob, expires_ms)) = existing else {
            return Err(StoreError::NotFound);
        };
        if expires_ms <= now_ms {
            tx.execute(
                "DELETE FROM idempotency_keys WHERE tenant = ?1 AND idempotency_key = ?2",
                params![tenant_str, key],
            )
            .map_err(map_sqlite_err)?;
            tx.commit().map_err(map_sqlite_err)?;
            return Err(StoreError::NotFound);
        }

        let stored_hash = decode_hash(hash_blob)?;
        if stored_hash != request_hash {
            return Err(StoreError::Conflict);
        }

        tx.execute(
            "UPDATE idempotency_keys
             SET status = ?1, result_ref = ?2
             WHERE tenant = ?3 AND idempotency_key = ?4",
            params![
                IdempotencyStatus::Completed.as_str(),
                result_ref,
                tenant_str,
                key,
            ],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    fn get(
        &self,
        tenant: &TenantScope,
        key: &str,
    ) -> Result<Option<IdempotencyRecord>, StoreError> {
        validate_idempotency_key(key)?;
        let now_ms = self.clock.now_timestamp().unix_millis();
        let tenant_str = tenant.tenant().as_str();

        let conn = lock_unpoisoned(&self.conn);
        type StoredIdempotencyRow = (Vec<u8>, String, Option<String>, String, i64);
        let row: Option<StoredIdempotencyRow> = conn
            .query_row(
                "SELECT request_hash, status, result_ref, expires_at_rfc3339, expires_at_ms
                 FROM idempotency_keys
                 WHERE tenant = ?1 AND idempotency_key = ?2",
                params![tenant_str, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let Some((hash_blob, status_raw, result_ref, expires_rfc3339, expires_ms)) = row else {
            return Ok(None);
        };
        if expires_ms <= now_ms {
            conn.execute(
                "DELETE FROM idempotency_keys WHERE tenant = ?1 AND idempotency_key = ?2",
                params![tenant_str, key],
            )
            .map_err(map_sqlite_err)?;
            return Ok(None);
        }

        Ok(Some(IdempotencyRecord {
            key: key.to_owned(),
            request_hash: decode_hash(hash_blob)?,
            status: IdempotencyStatus::from_storage_str(&status_raw).ok_or(StoreError::Corrupt)?,
            result_ref,
            expires_at: decode_timestamp(&expires_rfc3339)?,
        }))
    }

    fn release_in_progress(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
    ) -> Result<(), StoreError> {
        validate_idempotency_key(key)?;
        let tenant_str = tenant.tenant().as_str();
        let mut conn = lock_unpoisoned(&self.conn);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;
        tx.execute(
            "DELETE FROM idempotency_keys
             WHERE tenant = ?1 AND idempotency_key = ?2
               AND status = ?3 AND request_hash = ?4",
            params![
                tenant_str,
                key,
                IdempotencyStatus::InProgress.as_str(),
                request_hash.as_slice(),
            ],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    fn purge_expired(&self, tenant: &TenantScope, now: Timestamp) -> Result<u64, StoreError> {
        let tenant_str = tenant.tenant().as_str();
        let conn = lock_unpoisoned(&self.conn);
        let deleted = conn
            .execute(
                "DELETE FROM idempotency_keys WHERE tenant = ?1 AND expires_at_ms <= ?2",
                params![tenant_str, now.unix_millis()],
            )
            .map_err(map_sqlite_err)?;
        Ok(u64::try_from(deleted).unwrap_or(u64::MAX))
    }
}
