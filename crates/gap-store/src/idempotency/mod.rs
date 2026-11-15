//! Mutation idempotency-key ledger (`RUN-072`, `INV-09`).
//!
//! Maps `(tenant, key) -> (request_hash, status, result_ref, expires_at)` with a default 24 h
//! retention TTL (`DEFAULT_IDEMPOTENCY_TTL_MS`), providing in-memory and SQLite WAL-backed
//! implementations partitioned strictly by [`TenantScope`] (`INV-09`).

pub mod memory;
pub mod sqlite;

#[cfg(test)]
mod tests;

use gap_core::clock::Timestamp;
use gap_core::ids::TenantScope;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

pub use memory::InMemoryIdempotencyStore;
pub use sqlite::SqliteIdempotencyStore;

/// Default retention TTL for idempotency keys in milliseconds (`24 h`, `RUN-072`).
pub const DEFAULT_IDEMPOTENCY_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

/// Maximum byte length accepted for an idempotency key in storage.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

/// Validates caller-supplied idempotency keys prior to persistence (`RUN-072`).
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] if `key` is empty or exceeds [`MAX_IDEMPOTENCY_KEY_BYTES`].
pub fn validate_idempotency_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(StoreError::Invalid {
            field: "idempotency_key",
            reason: "must be 1..=256 bytes",
        });
    }
    Ok(())
}

/// Execution state of a reserved idempotency key (`RUN-072`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IdempotencyStatus {
    /// Mutation is currently executing under this key and `request_hash`.
    InProgress,
    /// Mutation finished; `result_ref` holds the replayable serialized outcome/reference.
    Completed,
}

impl IdempotencyStatus {
    /// Canonical uppercase wire/storage representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "IN_PROGRESS",
            Self::Completed => "COMPLETED",
        }
    }

    /// Parses canonical status string from storage.
    #[must_use]
    pub fn from_storage_str(raw: &str) -> Option<Self> {
        match raw {
            "IN_PROGRESS" => Some(Self::InProgress),
            "COMPLETED" => Some(Self::Completed),
            _ => None,
        }
    }
}

/// Durable ledger row for `(tenant, key)` (`RUN-072`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    /// Client-provided or server-minted idempotency key (`RUN-072`).
    pub key: String,
    /// 32-byte digest of the normalized mutation request.
    pub request_hash: [u8; 32],
    /// Current reservation status.
    pub status: IdempotencyStatus,
    /// Stored replay reference once [`IdempotencyStatus::Completed`].
    pub result_ref: Option<String>,
    /// Entry expiration timestamp (`24 h` TTL, `RUN-072`).
    pub expires_at: Timestamp,
}

/// Outcome of checking or reserving `(tenant, key)` (`RUN-072`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdempotencyOutcome {
    /// Key was absent (or expired); caller now owns the `InProgress` reservation.
    Reserved,
    /// Key is currently `InProgress` with an identical `request_hash`.
    InProgress,
    /// Key already `Completed` with an identical `request_hash`; replay `result_ref`.
    Replay {
        /// Stored result reference (`RUN-072`).
        result_ref: Option<String>,
    },
    /// Same key was previously bound to a different `request_hash` (`GAP_IDEMPOTENCY_CONFLICT`).
    Conflict,
}

/// Object-safe mutation idempotency ledger (`RUN-072`, `INV-09`).
///
/// Every method takes [`TenantScope`] first so idempotency keys are strictly partitioned per
/// tenant (`INV-09`).
pub trait IdempotencyStore: Send + Sync {
    /// Atomically checks or reserves `(tenant, key)` for `request_hash` until `expires_at` (`RUN-072`).
    fn check_or_reserve(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
        expires_at: Timestamp,
    ) -> Result<IdempotencyOutcome, StoreError>;

    /// Marks an `InProgress` reservation as [`IdempotencyStatus::Completed`] with `result_ref` (`RUN-072`).
    fn complete(
        &self,
        tenant: &TenantScope,
        key: &str,
        request_hash: [u8; 32],
        result_ref: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Looks up a non-expired [`IdempotencyRecord`] for `(tenant, key)`.
    fn get(&self, tenant: &TenantScope, key: &str)
    -> Result<Option<IdempotencyRecord>, StoreError>;

    /// Releases an `InProgress` reservation matching `(tenant, key, request_hash)` when a request
    /// fails *prior* to side-effecting dispatch, allowing safe subsequent retry.
    fn release_in_progress(
        &self,
        _tenant: &TenantScope,
        _key: &str,
        _request_hash: [u8; 32],
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// Removes expired records belonging to `tenant` at `now`, returning deleted row count.
    fn purge_expired(&self, _tenant: &TenantScope, _now: Timestamp) -> Result<u64, StoreError> {
        Ok(0)
    }
}
