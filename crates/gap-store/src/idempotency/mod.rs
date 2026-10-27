//! Mutation idempotency-key ledger (`RUN-072`, `INV-09`).
//!
//! **Provisional declaration (`m2-01-design.md` §9):** [`IdempotencyStore`] and its companion types
//! establish the object-safe `L1` trait boundary mapping
//! `(tenant, key) -> (request_hash, status, result_ref, expires_at)` (`RUN-072`). `WP-M6-04`
//! implements the 24 h ledger and may extend these provisional types.

use gap_core::clock::Timestamp;
use gap_core::ids::TenantScope;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Provisional execution state of a reserved idempotency key (`RUN-072`, `WP-M6-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IdempotencyStatus {
    /// Mutation is currently executing under this key and `request_hash`.
    InProgress,
    /// Mutation finished; `result_ref` holds the replayable result reference.
    Completed,
}

/// Provisional durable ledger row for `(tenant, key)` (`RUN-072`, `WP-M6-04`).
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

/// Provisional outcome of checking or reserving `(tenant, key)` (`RUN-072`, `WP-M6-04`).
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

/// Provisional object-safe mutation idempotency ledger (`RUN-072`, `INV-09`, `WP-M6-04`).
///
/// Every method takes [`TenantScope`] first so idempotency keys are strictly partitioned per
/// tenant (`INV-09`). Implemented and extended in `WP-M6-04`.
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
}

#[cfg(test)]
mod tests {
    use super::IdempotencyStore;

    // spec: RUN-072, INV-09
    #[test]
    fn idempotency_store_trait_is_object_safe() {
        fn _assert_dyn(_store: &dyn IdempotencyStore) {}
    }
}
