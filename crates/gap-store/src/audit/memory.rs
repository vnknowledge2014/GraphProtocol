//! Thread-safe in-memory BLAKE3 hash-chained [`AuditLog`] implementation (`SEC-090`, `SEC-092`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gap_core::clock::Clock;

use crate::audit::canonical::{
    AuditEventClass, DEFAULT_BATCH_WINDOW_MS, GENESIS_HASH, STARTUP_VERIFY_LAST_N,
    chain_storage_key, classify_audit_action, compute_record_hash, validate_entry,
    verify_ordered_records,
};
use crate::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
use crate::error::StoreError;
use crate::sqlite::lock_unpoisoned;

#[derive(Debug)]
struct PendingRecord {
    chain_key: String,
    enqueued_ms: i64,
    record: AuditRecord,
}

#[derive(Debug, Default)]
struct MemoryAuditState {
    chains: BTreeMap<String, Vec<AuditRecord>>,
    pending_standard: Vec<PendingRecord>,
    sync_flush_count: u64,
    batched_flush_count: u64,
}

/// Append-only in-memory BLAKE3 hash-chained [`AuditLog`] supporting `<= 100 ms` batching for
/// standard events and immediate synchronous flushing for privileged events (`SEC-090`, `SEC-092`).
pub struct InMemoryAuditLog {
    clock: Option<Arc<dyn Clock>>,
    batch_window_ms: u64,
    state: Mutex<MemoryAuditState>,
}

impl std::fmt::Debug for InMemoryAuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryAuditLog")
            .field("batch_window_ms", &self.batch_window_ms)
            .finish_non_exhaustive()
    }
}

impl Default for InMemoryAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryAuditLog {
    /// Creates an [`InMemoryAuditLog`] that commits every entry immediately.
    #[must_use]
    pub fn new() -> Self {
        Self {
            clock: None,
            batch_window_ms: 0,
            state: Mutex::new(MemoryAuditState::default()),
        }
    }

    /// Creates an [`InMemoryAuditLog`] using `clock` with `<= 100 ms` standard-event batching (`SEC-090`).
    #[must_use]
    pub fn with_batching(clock: Arc<dyn Clock>, batch_window_ms: u64) -> Self {
        Self {
            clock: Some(clock),
            batch_window_ms: batch_window_ms.min(DEFAULT_BATCH_WINDOW_MS),
            state: Mutex::new(MemoryAuditState::default()),
        }
    }

    fn current_ms(&self) -> i64 {
        self.clock
            .as_ref()
            .map_or(0, |c| c.now_timestamp().unix_millis())
    }

    fn drain_pending_locked(state: &mut MemoryAuditState, count_batched: bool) {
        if state.pending_standard.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut state.pending_standard);
        for item in pending {
            state
                .chains
                .entry(item.chain_key)
                .or_default()
                .push(item.record);
        }
        if count_batched {
            state.batched_flush_count = state.batched_flush_count.saturating_add(1);
        }
    }

    fn logical_head_locked(state: &MemoryAuditState, chain_key: &str) -> Option<(u64, [u8; 32])> {
        if let Some(last_pending) = state
            .pending_standard
            .iter()
            .rev()
            .find(|item| item.chain_key == chain_key)
        {
            return Some((last_pending.record.seq, last_pending.record.hash));
        }
        state
            .chains
            .get(chain_key)
            .and_then(|vec| vec.last())
            .map(|rec| (rec.seq, rec.hash))
    }

    /// Appends `entry` with explicit [`AuditEventClass`] durability control (`SEC-090`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `entry` fails validation.
    pub fn append_with_class(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
        class: AuditEventClass,
    ) -> Result<AuditRecord, StoreError> {
        validate_entry(entry)?;
        let chain_key = chain_storage_key(chain);
        let now_ms = self.current_ms();

        let mut guard = lock_unpoisoned(&self.state);
        if let Some(first) = guard.pending_standard.first() {
            let elapsed_ms = now_ms.saturating_sub(first.enqueued_ms);
            if elapsed_ms >= i64::try_from(self.batch_window_ms).unwrap_or(i64::MAX) {
                Self::drain_pending_locked(&mut guard, true);
            }
        }

        let (prev_seq, prev_hash) =
            Self::logical_head_locked(&guard, &chain_key).unwrap_or((0, GENESIS_HASH));
        let seq = prev_seq.checked_add(1).ok_or(StoreError::QuotaExceeded)?;
        let hash = compute_record_hash(&chain_key, seq, entry, &prev_hash)?;
        let record = AuditRecord {
            seq,
            entry: entry.clone(),
            prev_hash,
            hash,
        };

        if class == AuditEventClass::Privileged || self.batch_window_ms == 0 {
            Self::drain_pending_locked(&mut guard, false);
            guard
                .chains
                .entry(chain_key)
                .or_default()
                .push(record.clone());
            guard.sync_flush_count = guard.sync_flush_count.saturating_add(1);
        } else {
            guard.pending_standard.push(PendingRecord {
                chain_key,
                enqueued_ms: now_ms,
                record: record.clone(),
            });
        }

        Ok(record)
    }

    /// Flushes all pending batched non-privileged records synchronously.
    pub fn flush(&self) {
        let mut guard = lock_unpoisoned(&self.state);
        Self::drain_pending_locked(&mut guard, true);
    }

    /// Number of currently buffered standard events awaiting batch flush.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        lock_unpoisoned(&self.state).pending_standard.len()
    }

    /// Cumulative synchronous durable flushes performed (`Privileged` events + immediate flushes).
    #[must_use]
    pub fn sync_flush_count(&self) -> u64 {
        lock_unpoisoned(&self.state).sync_flush_count
    }

    /// Cumulative batched flushes performed (`Standard` events within `<= 100 ms`).
    #[must_use]
    pub fn batched_flush_count(&self) -> u64 {
        lock_unpoisoned(&self.state).batched_flush_count
    }

    /// Returns ordered records in `chain` starting at `from_seq` (`>= 1`) up to `limit` items.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] if `from_seq == 0`.
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
        let mut guard = lock_unpoisoned(&self.state);
        Self::drain_pending_locked(&mut guard, true);
        let key = chain_storage_key(chain);
        let Some(rows) = guard.chains.get(&key) else {
            return Ok(Vec::new());
        };
        Ok(rows
            .iter()
            .filter(|rec| rec.seq >= from_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    /// Runs startup verification (`System` chain, last `10_000` records, `SEC-092`).
    ///
    /// # Errors
    ///
    /// Propagates any [`StoreError`].
    pub fn verify_startup(&self) -> Result<AuditVerifyReport, StoreError> {
        self.verify_chain(&AuditChain::System, Some(STARTUP_VERIFY_LAST_N))
    }

    /// Runs full nightly verification across the system chain and every recorded tenant chain (`SEC-092`).
    ///
    /// # Errors
    ///
    /// Propagates any [`StoreError`].
    pub fn verify_nightly_all(&self) -> Result<Vec<(String, AuditVerifyReport)>, StoreError> {
        let mut guard = lock_unpoisoned(&self.state);
        Self::drain_pending_locked(&mut guard, true);
        let keys: Vec<String> = guard.chains.keys().cloned().collect();
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let rows = guard.chains.get(&key).cloned().unwrap_or_default();
            let report = verify_ordered_records(&key, &rows, 1, GENESIS_HASH);
            results.push((key, report));
        }
        Ok(results)
    }

    #[cfg(test)]
    pub(crate) fn tamper_modify_entry(&self, chain: &AuditChain, seq: u64, new_decision: &str) {
        self.flush();
        let mut guard = lock_unpoisoned(&self.state);
        let key = chain_storage_key(chain);
        if let Some(rows) = guard.chains.get_mut(&key)
            && let Some(row) = rows.iter_mut().find(|r| r.seq == seq)
        {
            row.entry.decision = new_decision.to_owned();
        }
    }

    #[cfg(test)]
    pub(crate) fn tamper_delete_seq(&self, chain: &AuditChain, seq: u64) {
        self.flush();
        let mut guard = lock_unpoisoned(&self.state);
        let key = chain_storage_key(chain);
        if let Some(rows) = guard.chains.get_mut(&key) {
            rows.retain(|r| r.seq != seq);
        }
    }

    #[cfg(test)]
    pub(crate) fn tamper_swap_seqs(&self, chain: &AuditChain, seq_a: u64, seq_b: u64) {
        self.flush();
        let mut guard = lock_unpoisoned(&self.state);
        let key = chain_storage_key(chain);
        if let Some(rows) = guard.chains.get_mut(&key)
            && let (Some(idx_a), Some(idx_b)) = (
                rows.iter().position(|r| r.seq == seq_a),
                rows.iter().position(|r| r.seq == seq_b),
            )
        {
            rows.swap(idx_a, idx_b);
        }
    }
}

impl AuditLog for InMemoryAuditLog {
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError> {
        let class = classify_audit_action(&entry.action);
        self.append_with_class(chain, entry, class)
    }

    fn chain_head(&self, chain: &AuditChain) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        let guard = lock_unpoisoned(&self.state);
        let key = chain_storage_key(chain);
        Ok(Self::logical_head_locked(&guard, &key))
    }

    fn verify_chain(
        &self,
        chain: &AuditChain,
        last_n: Option<u64>,
    ) -> Result<AuditVerifyReport, StoreError> {
        let mut guard = lock_unpoisoned(&self.state);
        Self::drain_pending_locked(&mut guard, true);
        let key = chain_storage_key(chain);
        let Some(rows) = guard.chains.get(&key) else {
            return Ok(AuditVerifyReport::Intact {
                head_seq: 0,
                head_hash: GENESIS_HASH,
            });
        };
        if rows.is_empty() {
            return Ok(AuditVerifyReport::Intact {
                head_seq: 0,
                head_hash: GENESIS_HASH,
            });
        }

        let max_seq = rows.iter().map(|r| r.seq).max().unwrap_or(0);
        let (start_seq, initial_prev) = match last_n {
            Some(0) => {
                let last = rows.last().ok_or(StoreError::Corrupt)?;
                return Ok(AuditVerifyReport::Intact {
                    head_seq: last.seq,
                    head_hash: last.hash,
                });
            }
            Some(n) if max_seq > n => {
                let start = max_seq - n + 1;
                let pred_seq = start - 1;
                let Some(pred) = rows.iter().find(|r| r.seq == pred_seq) else {
                    return Ok(AuditVerifyReport::Inconsistent {
                        seq: pred_seq,
                        reason: format!(
                            "missing predecessor seq {pred_seq} required to anchor last_n window"
                        ),
                    });
                };
                (start, pred.hash)
            }
            _ => (1u64, GENESIS_HASH),
        };

        let slice: Vec<AuditRecord> = rows
            .iter()
            .filter(|r| r.seq >= start_seq)
            .cloned()
            .collect();
        Ok(verify_ordered_records(
            &key,
            &slice,
            start_seq,
            initial_prev,
        ))
    }
}
