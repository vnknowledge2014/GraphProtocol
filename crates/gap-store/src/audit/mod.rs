//! Append-only BLAKE3 hash-chained audit persistence (`SEC-090`, `SEC-092`, `INV-09`, `INV-14`, `TM-17`).
//!
//! Provides independent per-tenant and gateway-system BLAKE3 hash chains (`AuditChain::Tenant`,
//! `AuditChain::System`), canonical JSON hashing (`blake3(prev_hash || canonical_json(record_without_hash))`),
//! synchronous durability for privileged events vs `<= 100 ms` batching for standard events,
//! startup/nightly verification (`SEC-092`), and both in-memory and durable SQLite WAL backends.

pub mod canonical;
pub mod memory;
pub mod sqlite;
#[cfg(test)]
mod tests;

pub use canonical::{
    AuditEventClass, DEFAULT_BATCH_WINDOW_MS, GENESIS_HASH, STARTUP_VERIFY_LAST_N,
    canonical_record_without_hash_bytes, chain_storage_key, classify_audit_action,
    compute_record_hash, hex_encode_32, validate_entry, verify_ordered_records,
};
pub use memory::InMemoryAuditLog;
pub use sqlite::{SqliteAuditConfig, SqliteAuditLog};

use gap_core::clock::Timestamp;
use gap_core::ids::TenantScope;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Identifies which independent hash chain an audit operation targets (`SEC-090`).
///
/// `SEC-090` mandates one hash chain per tenant plus one gateway system chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditChain {
    /// Tenant-isolated audit chain carrying the non-forgeable [`TenantScope`] token (`INV-09`).
    Tenant(TenantScope),
    /// Gateway-wide system chain (startup, admin reload, snapshot activation, anchoring).
    System,
}

/// Unhashed audit event fields prior to chain sequencing (`SEC-090`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Event timestamp (`UTC`).
    pub ts: Timestamp,
    /// Principal / delegation chain (`actor_chain` in `SEC-090`, outermost actor first).
    pub actor_chain: Vec<String>,
    /// Action identifier (operation or control-plane verb).
    pub action: String,
    /// Target resource identifier.
    pub resource: String,
    /// Authorization / execution decision (`ALLOW`, `DENY`, etc.).
    pub decision: String,
    /// Canonical JSON details payload (`args_hash`, `approval_id`, `outcome`, etc.).
    pub details_json: String,
}

/// Sequenced and BLAKE3-chained audit record (`SEC-090`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Strictly monotonic per-chain sequence number (`SEC-090`).
    pub seq: u64,
    /// Recorded event fields.
    pub entry: AuditEntry,
    /// Previous record's 32-byte BLAKE3 digest (`[0u8; 32]` at chain genesis).
    pub prev_hash: [u8; 32],
    /// `blake3(prev_hash || canonical_json(record_without_hash))` (`SEC-090`).
    pub hash: [u8; 32],
}

/// Result of recomputing a hash chain (`SEC-092`, `TM-17`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditVerifyReport {
    /// Chain verified cleanly through `head_seq`.
    Intact {
        /// Highest verified sequence number (`0` when empty).
        head_seq: u64,
        /// Head digest (`[0u8; 32]` when empty).
        head_hash: [u8; 32],
    },
    /// First detected sequence gap, reordering, `prev_hash` break, or digest mismatch (`SEC-092`, `TM-17`).
    Inconsistent {
        /// Sequence number of the first inconsistent record.
        seq: u64,
        /// Diagnostic description suitable for operator review.
        reason: String,
    },
}

/// Object-safe append-only hash-chained audit log (`SEC-090`..`SEC-092`, `INV-09`).
///
/// Tenant-scoped helpers take [`TenantScope`] first; [`AuditChain`] methods additionally allow
/// addressing the single gateway [`AuditChain::System`] chain (`SEC-090`).
pub trait AuditLog: Send + Sync {
    /// Appends `entry` to the chain selected by `chain` (`SEC-090`).
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError>;

    /// Returns `(head_seq, head_hash)` of `chain` for Ed25519 anchoring (`SEC-091`).
    fn chain_head(&self, chain: &AuditChain) -> Result<Option<(u64, [u8; 32])>, StoreError>;

    /// Recomputes `chain` (optionally bounded to the last `last_n` records at startup, `SEC-092`)
    /// and reports the first inconsistency if any.
    fn verify_chain(
        &self,
        chain: &AuditChain,
        last_n: Option<u64>,
    ) -> Result<AuditVerifyReport, StoreError>;

    /// Appends `entry` to `tenant`'s hash chain (`INV-09`, `SEC-090`).
    fn append(&self, tenant: &TenantScope, entry: &AuditEntry) -> Result<AuditRecord, StoreError> {
        self.append_chain(&AuditChain::Tenant(tenant.clone()), entry)
    }

    /// Returns `(head_seq, head_hash)` of `tenant`'s hash chain (`INV-09`, `SEC-091`).
    fn head(&self, tenant: &TenantScope) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        self.chain_head(&AuditChain::Tenant(tenant.clone()))
    }

    /// Verifies `tenant`'s hash chain (`INV-09`, `SEC-092`).
    fn verify(
        &self,
        tenant: &TenantScope,
        last_n: Option<u64>,
    ) -> Result<AuditVerifyReport, StoreError> {
        self.verify_chain(&AuditChain::Tenant(tenant.clone()), last_n)
    }
}
