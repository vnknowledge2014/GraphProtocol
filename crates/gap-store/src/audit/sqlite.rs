//! Durable SQLite WAL BLAKE3 hash-chained [`AuditLog`] backend (`SEC-090`, `SEC-092`, `INV-09`, `INV-14`, `INV-15`).

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gap_core::clock::{Clock, Timestamp};
use rusqlite::{Connection, OptionalExtension, params};

use crate::audit::canonical::{
    AuditEventClass, DEFAULT_BATCH_WINDOW_MS, GENESIS_HASH, STARTUP_VERIFY_LAST_N,
    chain_storage_key, classify_audit_action, compute_record_hash, validate_entry,
    verify_ordered_records,
};
use crate::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
use crate::error::StoreError;
use crate::sqlite::{DEFAULT_BUSY_TIMEOUT_MS, Durability, lock_unpoisoned, map_sqlite_err};

/// Configuration for [`SqliteAuditLog`] (`SEC-090`, `SEC-092`).
#[derive(Debug, Clone)]
pub struct SqliteAuditConfig {
    /// Filesystem path of the SQLite database file, or `None` for private in-memory storage.
    pub path: Option<PathBuf>,
    /// WAL durability mode (default [`Durability::Full`]).
    pub durability: Durability,
    /// SQLite `busy_timeout` in milliseconds (default `5,000`).
    pub busy_timeout_ms: u32,
    /// Maximum buffering delay (`<= 100 ms`, default `0` for immediate write unless batching enabled).
    pub batch_window_ms: u64,
    /// Maximum buffered standard records before automatic batch commit.
    pub max_batch_len: usize,
}

impl Default for SqliteAuditConfig {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SqliteAuditConfig {
    /// Creates a file-backed [`SqliteAuditConfig`] with [`Durability::Full`] and immediate synchronous writes.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            durability: Durability::Full,
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            batch_window_ms: 0,
            max_batch_len: 64,
        }
    }

    /// Creates a private in-memory [`SqliteAuditConfig`].
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            durability: Durability::Full,
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            batch_window_ms: 0,
            max_batch_len: 64,
        }
    }

    /// Enables `<= 100 ms` batching for non-privileged standard audit events (`SEC-090`).
    #[must_use]
    pub fn with_batch_window_ms(mut self, ms: u64) -> Self {
        self.batch_window_ms = ms.min(DEFAULT_BATCH_WINDOW_MS);
        self
    }
}

#[derive(Debug)]
struct PendingSqliteRecord {
    chain_key: String,
    enqueued_ms: i64,
    record: AuditRecord,
    event_class: AuditEventClass,
}

struct SqliteAuditInner {
    conn: Connection,
    pending_standard: Vec<PendingSqliteRecord>,
    sync_flush_count: u64,
    batched_flush_count: u64,
}

/// Persistent SQLite WAL BLAKE3 hash-chained [`AuditLog`] (`SEC-090`, `SEC-092`, `INV-14`).
pub struct SqliteAuditLog {
    clock: Option<Arc<dyn Clock>>,
    config: SqliteAuditConfig,
    inner: Mutex<SqliteAuditInner>,
}

impl std::fmt::Debug for SqliteAuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteAuditLog")
            .field("is_file_backed", &self.config.path.is_some())
            .field("batch_window_ms", &self.config.batch_window_ms)
            .finish_non_exhaustive()
    }
}

impl Drop for SqliteAuditLog {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl SqliteAuditLog {
    /// Opens a [`SqliteAuditLog`] from `config` without batch clock override.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if opening or initializing the SQLite audit schema fails.
    pub fn open(config: SqliteAuditConfig) -> Result<Self, StoreError> {
        Self::open_internal(config, None)
    }

    /// Opens a [`SqliteAuditLog`] using `clock` for `<= 100 ms` standard-event batching (`SEC-090`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if opening or initializing the SQLite audit schema fails.
    pub fn open_with_clock(
        config: SqliteAuditConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StoreError> {
        Self::open_internal(config, Some(clock))
    }

    fn open_internal(
        mut config: SqliteAuditConfig,
        clock: Option<Arc<dyn Clock>>,
    ) -> Result<Self, StoreError> {
        config.batch_window_ms = config.batch_window_ms.min(DEFAULT_BATCH_WINDOW_MS);
        config.max_batch_len = config.max_batch_len.max(1);

        if let Some(ref path) = config.path
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| StoreError::Backend)?;
        }

        let conn = match config.path {
            Some(ref path) => Connection::open(path).map_err(map_sqlite_err)?,
            None => Connection::open_in_memory().map_err(map_sqlite_err)?,
        };

        conn.busy_timeout(std::time::Duration::from_millis(u64::from(
            config.busy_timeout_ms,
        )))
        .map_err(map_sqlite_err)?;

        let sync_sql = match config.durability {
            Durability::Full => "PRAGMA synchronous = FULL;",
            Durability::Normal => "PRAGMA synchronous = NORMAL;",
            Durability::Off => "PRAGMA synchronous = OFF;",
        };
        conn.execute_batch(sync_sql).map_err(map_sqlite_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS audit (
                 chain_key TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 ts_unix_nanos TEXT NOT NULL,
                 actor_chain_json TEXT NOT NULL,
                 action TEXT NOT NULL,
                 resource TEXT NOT NULL,
                 decision TEXT NOT NULL,
                 details_json TEXT NOT NULL,
                 prev_hash BLOB NOT NULL,
                 hash BLOB NOT NULL,
                 event_class TEXT NOT NULL,
                 PRIMARY KEY (chain_key, seq)
             );
             CREATE INDEX IF NOT EXISTS idx_audit_chain_ts ON audit (chain_key, ts_unix_nanos);",
        )
        .map_err(map_sqlite_err)?;

        Ok(Self {
            clock,
            config,
            inner: Mutex::new(SqliteAuditInner {
                conn,
                pending_standard: Vec::new(),
                sync_flush_count: 0,
                batched_flush_count: 0,
            }),
        })
    }

    fn current_ms(&self) -> i64 {
        self.clock
            .as_ref()
            .map_or(0, |c| c.now_timestamp().unix_millis())
    }

    fn insert_batch(
        conn: &mut Connection,
        items: &[PendingSqliteRecord],
    ) -> Result<(), StoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO audit (
                         chain_key, seq, ts_unix_nanos, actor_chain_json, action, resource,
                         decision, details_json, prev_hash, hash, event_class
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .map_err(map_sqlite_err)?;
            for item in items {
                let seq_i64 =
                    i64::try_from(item.record.seq).map_err(|_| StoreError::QuotaExceeded)?;
                let ts_str = item.record.entry.ts.unix_nanos().to_string();
                let actors_json =
                    serde_json::to_string(&item.record.entry.actor_chain).map_err(|_| {
                        StoreError::Invalid {
                            field: "actor_chain",
                            reason: "JSON encoding failed",
                        }
                    })?;
                stmt.execute(params![
                    item.chain_key,
                    seq_i64,
                    ts_str,
                    actors_json,
                    item.record.entry.action,
                    item.record.entry.resource,
                    item.record.entry.decision,
                    item.record.entry.details_json,
                    item.record.prev_hash.as_slice(),
                    item.record.hash.as_slice(),
                    item.event_class.as_str(),
                ])
                .map_err(map_sqlite_err)?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    fn flush_pending_locked(
        inner: &mut SqliteAuditInner,
        count_batched: bool,
    ) -> Result<(), StoreError> {
        if inner.pending_standard.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut inner.pending_standard);
        if let Err(err) = Self::insert_batch(&mut inner.conn, &items) {
            inner.pending_standard = items;
            return Err(err);
        }
        if count_batched {
            inner.batched_flush_count = inner.batched_flush_count.saturating_add(1);
        }
        Ok(())
    }

    fn logical_head_locked(
        inner: &SqliteAuditInner,
        chain_key: &str,
    ) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        if let Some(last_pending) = inner
            .pending_standard
            .iter()
            .rev()
            .find(|item| item.chain_key == chain_key)
        {
            return Ok(Some((last_pending.record.seq, last_pending.record.hash)));
        }
        let row: Option<(i64, Vec<u8>)> = inner
            .conn
            .query_row(
                "SELECT seq, hash FROM audit WHERE chain_key = ?1 ORDER BY rowid DESC LIMIT 1",
                params![chain_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let Some((seq_i64, hash_blob)) = row else {
            return Ok(None);
        };
        let seq = u64::try_from(seq_i64).map_err(|_| StoreError::Corrupt)?;
        let hash: [u8; 32] = hash_blob.try_into().map_err(|_| StoreError::Corrupt)?;
        Ok(Some((seq, hash)))
    }

    /// Appends `entry` with explicit [`AuditEventClass`] durability control (`SEC-090`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if validation fails or persistence returns an error.
    pub fn append_with_class(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
        class: AuditEventClass,
    ) -> Result<AuditRecord, StoreError> {
        validate_entry(entry)?;
        let chain_key = chain_storage_key(chain);
        let now_ms = self.current_ms();

        let mut guard = lock_unpoisoned(&self.inner);
        let window_i64 = i64::try_from(self.config.batch_window_ms).unwrap_or(i64::MAX);
        if !guard.pending_standard.is_empty()
            && (guard.pending_standard.len() >= self.config.max_batch_len
                || guard
                    .pending_standard
                    .first()
                    .is_some_and(|first| now_ms.saturating_sub(first.enqueued_ms) >= window_i64))
        {
            Self::flush_pending_locked(&mut guard, true)?;
        }

        let (prev_seq, prev_hash) =
            Self::logical_head_locked(&guard, &chain_key)?.unwrap_or((0, GENESIS_HASH));
        let seq = prev_seq.checked_add(1).ok_or(StoreError::QuotaExceeded)?;
        let hash = compute_record_hash(&chain_key, seq, entry, &prev_hash)?;
        let record = AuditRecord {
            seq,
            entry: entry.clone(),
            prev_hash,
            hash,
        };

        let pending_item = PendingSqliteRecord {
            chain_key,
            enqueued_ms: now_ms,
            record: record.clone(),
            event_class: class,
        };

        if class == AuditEventClass::Privileged || self.config.batch_window_ms == 0 {
            let mut items = std::mem::take(&mut guard.pending_standard);
            items.push(pending_item);
            if let Err(err) = Self::insert_batch(&mut guard.conn, &items) {
                let last = items.pop();
                guard.pending_standard = items;
                drop(last);
                return Err(err);
            }
            guard.sync_flush_count = guard.sync_flush_count.saturating_add(1);
        } else {
            guard.pending_standard.push(pending_item);
        }

        Ok(record)
    }

    /// Flushes any pending batched standard records to SQLite synchronously.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if SQLite batch insertion fails.
    pub fn flush(&self) -> Result<(), StoreError> {
        let mut guard = lock_unpoisoned(&self.inner);
        Self::flush_pending_locked(&mut guard, true)
    }

    /// Number of currently buffered standard events awaiting batch flush.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        lock_unpoisoned(&self.inner).pending_standard.len()
    }

    /// Cumulative synchronous commits performed (`Privileged` events + immediate flushes).
    #[must_use]
    pub fn sync_flush_count(&self) -> u64 {
        lock_unpoisoned(&self.inner).sync_flush_count
    }

    /// Cumulative batched commits performed (`Standard` events within `<= 100 ms`).
    #[must_use]
    pub fn batched_flush_count(&self) -> u64 {
        lock_unpoisoned(&self.inner).batched_flush_count
    }

    /// Reads ordered records from `chain` starting at `from_seq` up to `limit`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on invalid `from_seq` or corrupt persisted row data.
    pub fn list_records(
        &self,
        chain: &AuditChain,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        if from_seq == 0 {
            return Err(StoreError::Invalid {
                field: "from_seq",
                reason: "audit sequence numbers are 1-based",
            });
        }
        let mut guard = lock_unpoisoned(&self.inner);
        Self::flush_pending_locked(&mut guard, true)?;
        let chain_key = chain_storage_key(chain);
        let from_i64 = i64::try_from(from_seq).map_err(|_| StoreError::QuotaExceeded)?;
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        Self::load_records_since_seq(&guard.conn, &chain_key, from_i64, limit_i64)
    }

    /// Runs startup verification (`System` chain, last `10_000` records, `SEC-092`).
    ///
    /// # Errors
    ///
    /// Propagates any [`StoreError`].
    pub fn verify_startup(&self) -> Result<AuditVerifyReport, StoreError> {
        self.verify_chain(&AuditChain::System, Some(STARTUP_VERIFY_LAST_N))
    }

    /// Runs full nightly verification across the system chain and every stored tenant chain (`SEC-092`).
    ///
    /// # Errors
    ///
    /// Propagates any [`StoreError`].
    pub fn verify_nightly_all(&self) -> Result<Vec<(String, AuditVerifyReport)>, StoreError> {
        let mut guard = lock_unpoisoned(&self.inner);
        Self::flush_pending_locked(&mut guard, true)?;
        let chain_keys = Self::distinct_chain_keys(&guard.conn)?;
        let mut out = Vec::with_capacity(chain_keys.len());
        for key in chain_keys {
            let report = Self::verify_chain_key_locked(&guard.conn, &key, None)?;
            out.push((key, report));
        }
        Ok(out)
    }

    fn distinct_chain_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
        let mut stmt = conn
            .prepare("SELECT DISTINCT chain_key FROM audit ORDER BY chain_key ASC")
            .map_err(map_sqlite_err)?;
        let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
        let mut seen = BTreeSet::new();
        while let Some(row) = rows.next().map_err(map_sqlite_err)? {
            let key: String = row.get(0).map_err(map_sqlite_err)?;
            seen.insert(key);
        }
        Ok(seen.into_iter().collect())
    }

    fn load_records_since_seq(
        conn: &Connection,
        chain_key: &str,
        from_seq: i64,
        limit: i64,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts_unix_nanos, actor_chain_json, action, resource,
                        decision, details_json, prev_hash, hash
                 FROM audit
                 WHERE chain_key = ?1 AND seq >= ?2
                 ORDER BY rowid ASC
                 LIMIT ?3",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![chain_key, from_seq, limit], Self::decode_audit_row)
            .map_err(map_sqlite_err)?;
        let mut records = Vec::new();
        for row_res in rows {
            records.push(row_res.map_err(map_sqlite_err)?);
        }
        Ok(records)
    }

    fn load_records_after_rowid(
        conn: &Connection,
        chain_key: &str,
        after_rowid: i64,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts_unix_nanos, actor_chain_json, action, resource,
                        decision, details_json, prev_hash, hash
                 FROM audit
                 WHERE chain_key = ?1 AND rowid > ?2
                 ORDER BY rowid ASC",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![chain_key, after_rowid], Self::decode_audit_row)
            .map_err(map_sqlite_err)?;
        let mut records = Vec::new();
        for row_res in rows {
            records.push(row_res.map_err(map_sqlite_err)?);
        }
        Ok(records)
    }

    fn decode_audit_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
        let seq_i64: i64 = r.get(0)?;
        let ts_str: String = r.get(1)?;
        let actor_chain_json: String = r.get(2)?;
        let action: String = r.get(3)?;
        let resource: String = r.get(4)?;
        let decision: String = r.get(5)?;
        let details_json: String = r.get(6)?;
        let prev_blob: Vec<u8> = r.get(7)?;
        let hash_blob: Vec<u8> = r.get(8)?;

        let seq = u64::try_from(seq_i64).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(StoreError::Corrupt),
            )
        })?;
        let ts_nanos: i128 = ts_str.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(StoreError::Corrupt),
            )
        })?;
        let ts = Timestamp::from_unix_nanos(ts_nanos).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(StoreError::Corrupt),
            )
        })?;
        let actor_chain: Vec<String> = serde_json::from_str(&actor_chain_json).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(StoreError::Corrupt),
            )
        })?;
        let prev_hash: [u8; 32] = prev_blob.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Blob,
                Box::new(StoreError::Corrupt),
            )
        })?;
        let hash: [u8; 32] = hash_blob.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Blob,
                Box::new(StoreError::Corrupt),
            )
        })?;

        Ok(AuditRecord {
            seq,
            entry: AuditEntry {
                ts,
                actor_chain,
                action,
                resource,
                decision,
                details_json,
            },
            prev_hash,
            hash,
        })
    }

    fn verify_chain_key_locked(
        conn: &Connection,
        chain_key: &str,
        last_n: Option<u64>,
    ) -> Result<AuditVerifyReport, StoreError> {
        let max_row: Option<(i64, Vec<u8>)> = conn
            .query_row(
                "SELECT seq, hash FROM audit WHERE chain_key = ?1 ORDER BY rowid DESC LIMIT 1",
                params![chain_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let Some((max_seq_i64, head_blob)) = max_row else {
            return Ok(AuditVerifyReport::Intact {
                head_seq: 0,
                head_hash: GENESIS_HASH,
            });
        };
        let max_seq = u64::try_from(max_seq_i64).map_err(|_| StoreError::Corrupt)?;
        if matches!(last_n, Some(0)) {
            let head_hash: [u8; 32] = head_blob.try_into().map_err(|_| StoreError::Corrupt)?;
            return Ok(AuditVerifyReport::Intact {
                head_seq: max_seq,
                head_hash,
            });
        }

        let (start_seq, initial_prev, after_rowid) = match last_n {
            Some(n) if max_seq > n => {
                let start = max_seq - n + 1;
                let pred_seq = start - 1;
                let pred_i64 = i64::try_from(pred_seq).map_err(|_| StoreError::Corrupt)?;
                let pred: Option<(i64, Vec<u8>)> = conn
                    .query_row(
                        "SELECT rowid, hash FROM audit WHERE chain_key = ?1 AND seq = ?2",
                        params![chain_key, pred_i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?;
                let Some((pred_rowid, pred_blob)) = pred else {
                    return Ok(AuditVerifyReport::Inconsistent {
                        seq: pred_seq,
                        reason: format!(
                            "missing predecessor seq {pred_seq} required to anchor last_n window"
                        ),
                    });
                };
                let pred_hash: [u8; 32] = pred_blob.try_into().map_err(|_| StoreError::Corrupt)?;
                (start, pred_hash, pred_rowid)
            }
            _ => (1u64, GENESIS_HASH, -1_i64),
        };

        let records = Self::load_records_after_rowid(conn, chain_key, after_rowid)?;
        Ok(verify_ordered_records(
            chain_key,
            &records,
            start_seq,
            initial_prev,
        ))
    }

    #[cfg(test)]
    pub(crate) fn tamper_modify_decision(
        &self,
        chain: &AuditChain,
        seq: u64,
        new_decision: &str,
    ) -> Result<(), StoreError> {
        self.flush()?;
        let guard = lock_unpoisoned(&self.inner);
        let key = chain_storage_key(chain);
        let seq_i64 = i64::try_from(seq).map_err(|_| StoreError::QuotaExceeded)?;
        guard
            .conn
            .execute(
                "UPDATE audit SET decision = ?1 WHERE chain_key = ?2 AND seq = ?3",
                params![new_decision, key, seq_i64],
            )
            .map_err(map_sqlite_err)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn tamper_delete_seq(&self, chain: &AuditChain, seq: u64) -> Result<(), StoreError> {
        self.flush()?;
        let guard = lock_unpoisoned(&self.inner);
        let key = chain_storage_key(chain);
        let seq_i64 = i64::try_from(seq).map_err(|_| StoreError::QuotaExceeded)?;
        guard
            .conn
            .execute(
                "DELETE FROM audit WHERE chain_key = ?1 AND seq = ?2",
                params![key, seq_i64],
            )
            .map_err(map_sqlite_err)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn tamper_corrupt_prev_hash(
        &self,
        chain: &AuditChain,
        seq: u64,
        corrupt_prev: &[u8; 32],
    ) -> Result<(), StoreError> {
        self.flush()?;
        let guard = lock_unpoisoned(&self.inner);
        let key = chain_storage_key(chain);
        let seq_i64 = i64::try_from(seq).map_err(|_| StoreError::QuotaExceeded)?;
        guard
            .conn
            .execute(
                "UPDATE audit SET prev_hash = ?1 WHERE chain_key = ?2 AND seq = ?3",
                params![corrupt_prev.as_slice(), key, seq_i64],
            )
            .map_err(map_sqlite_err)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn tamper_swap_seqs(
        &self,
        chain: &AuditChain,
        seq_a: u64,
        seq_b: u64,
    ) -> Result<(), StoreError> {
        self.flush()?;
        let mut guard = lock_unpoisoned(&self.inner);
        let key = chain_storage_key(chain);
        let a_i64 = i64::try_from(seq_a).map_err(|_| StoreError::QuotaExceeded)?;
        let b_i64 = i64::try_from(seq_b).map_err(|_| StoreError::QuotaExceeded)?;
        let tx = guard.conn.transaction().map_err(map_sqlite_err)?;
        tx.execute(
            "UPDATE audit SET seq = -1 WHERE chain_key = ?1 AND seq = ?2",
            params![key, a_i64],
        )
        .map_err(map_sqlite_err)?;
        tx.execute(
            "UPDATE audit SET seq = ?2 WHERE chain_key = ?1 AND seq = ?3",
            params![key, a_i64, b_i64],
        )
        .map_err(map_sqlite_err)?;
        tx.execute(
            "UPDATE audit SET seq = ?2 WHERE chain_key = ?1 AND seq = -1",
            params![key, b_i64],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }
}

impl AuditLog for SqliteAuditLog {
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError> {
        let class = classify_audit_action(&entry.action);
        self.append_with_class(chain, entry, class)
    }

    fn chain_head(&self, chain: &AuditChain) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        let guard = lock_unpoisoned(&self.inner);
        let key = chain_storage_key(chain);
        Self::logical_head_locked(&guard, &key)
    }

    fn verify_chain(
        &self,
        chain: &AuditChain,
        last_n: Option<u64>,
    ) -> Result<AuditVerifyReport, StoreError> {
        let mut guard = lock_unpoisoned(&self.inner);
        Self::flush_pending_locked(&mut guard, true)?;
        let key = chain_storage_key(chain);
        Self::verify_chain_key_locked(&guard.conn, &key, last_n)
    }
}
