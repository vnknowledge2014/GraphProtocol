//! In-memory mutex-guarded [`IdempotencyStore`] backend for fast tests and single-node runs (`RUN-072`, `INV-09`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::ids::TenantScope;

use crate::error::StoreError;
use crate::sqlite::lock_unpoisoned;

use super::{
    IdempotencyOutcome, IdempotencyRecord, IdempotencyStatus, IdempotencyStore,
    validate_idempotency_key,
};

/// Thread-safe in-memory [`IdempotencyStore`] partitioned strictly by `(tenant, key)` (`RUN-072`, `INV-09`).
#[derive(Debug)]
pub struct InMemoryIdempotencyStore {
    clock: Arc<dyn Clock>,
    rows: Mutex<BTreeMap<(String, String), IdempotencyRecord>>,
}

impl Default for InMemoryIdempotencyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryIdempotencyStore {
    /// Creates an empty in-memory store backed by [`SystemClock`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// Creates an empty in-memory store using `clock` for TTL checks (`RUN-072`).
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            rows: Mutex::new(BTreeMap::new()),
        }
    }
}

impl IdempotencyStore for InMemoryIdempotencyStore {
    fn check_or_reserve(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
        expires_at: Timestamp,
    ) -> Result<IdempotencyOutcome, StoreError> {
        validate_idempotency_key(key)?;
        let now = self.clock.now_timestamp();
        let composite = (tenant.tenant().as_str().to_owned(), key.to_owned());

        let mut guard = lock_unpoisoned(&self.rows);
        if let Some(existing) = guard.get(&composite)
            && existing.expires_at > now
        {
            if existing.request_hash != request_hash {
                return Ok(IdempotencyOutcome::Conflict);
            }
            return Ok(match existing.status {
                IdempotencyStatus::InProgress => IdempotencyOutcome::InProgress,
                IdempotencyStatus::Completed => IdempotencyOutcome::Replay {
                    result_ref: existing.result_ref.clone(),
                },
            });
        }

        if expires_at <= now {
            return Err(StoreError::Invalid {
                field: "expires_at",
                reason: "must be in the future",
            });
        }

        guard.insert(
            composite,
            IdempotencyRecord {
                key: key.to_owned(),
                request_hash,
                status: IdempotencyStatus::InProgress,
                result_ref: None,
                expires_at,
            },
        );
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
        let now = self.clock.now_timestamp();
        let composite = (tenant.tenant().as_str().to_owned(), key.to_owned());
        let mut guard = lock_unpoisoned(&self.rows);
        let Some(existing) = guard.get_mut(&composite) else {
            return Err(StoreError::NotFound);
        };
        if existing.expires_at <= now {
            guard.remove(&composite);
            return Err(StoreError::NotFound);
        }
        if existing.request_hash != request_hash {
            return Err(StoreError::Conflict);
        }
        existing.status = IdempotencyStatus::Completed;
        existing.result_ref = result_ref.map(ToOwned::to_owned);
        Ok(())
    }

    fn get(
        &self,
        tenant: &TenantScope,
        key: &str,
    ) -> Result<Option<IdempotencyRecord>, StoreError> {
        validate_idempotency_key(key)?;
        let now = self.clock.now_timestamp();
        let composite = (tenant.tenant().as_str().to_owned(), key.to_owned());
        let mut guard = lock_unpoisoned(&self.rows);
        match guard.get(&composite) {
            Some(record) if record.expires_at > now => Ok(Some(record.clone())),
            Some(_) => {
                guard.remove(&composite);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    fn release_in_progress(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
    ) -> Result<(), StoreError> {
        validate_idempotency_key(key)?;
        let composite = (tenant.tenant().as_str().to_owned(), key.to_owned());
        let mut guard = lock_unpoisoned(&self.rows);
        if let Some(existing) = guard.get(&composite)
            && existing.status == IdempotencyStatus::InProgress
            && existing.request_hash == request_hash
        {
            guard.remove(&composite);
        }
        Ok(())
    }

    fn purge_expired(&self, tenant: &TenantScope, now: Timestamp) -> Result<u64, StoreError> {
        let tenant_key = tenant.tenant().as_str();
        let mut guard = lock_unpoisoned(&self.rows);
        let doomed: Vec<(String, String)> = guard
            .iter()
            .filter(|((t, _), rec)| t == tenant_key && rec.expires_at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        let removed = u64::try_from(doomed.len()).unwrap_or(u64::MAX);
        for key in doomed {
            guard.remove(&key);
        }
        Ok(removed)
    }
}
