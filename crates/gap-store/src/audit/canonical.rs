//! Deterministic canonical JSON encoding and BLAKE3 audit hash verification (`SEC-090`, `SEC-092`, `INV-14`).

use std::fmt::Write as _;

use crate::audit::{AuditChain, AuditEntry, AuditRecord, AuditVerifyReport};
use crate::error::StoreError;

/// Genesis `prev_hash` (`[0u8; 32]`) used by record `seq = 1` on every audit chain (`SEC-090`).
pub const GENESIS_HASH: [u8; 32] = [0u8; 32];

/// Startup verification lookback count for the gateway `System` chain (`SEC-092`).
pub const STARTUP_VERIFY_LAST_N: u64 = 10_000;

/// Maximum buffering delay (`100 ms`) for non-privileged audit writes (`SEC-090`).
pub const DEFAULT_BATCH_WINDOW_MS: u64 = 100;

/// Durability class of an audit event (`SEC-090`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditEventClass {
    /// Privileged event (mutation, approval, admin, policy, ingestion activation, anchoring):
    /// flushed synchronously and durably before returning (`SEC-090`).
    Privileged,
    /// Non-privileged event (read execution, discovery, introspection): batchable `<= 100 ms` (`SEC-090`).
    Standard,
}

impl AuditEventClass {
    /// Stable lowercase database/telemetry label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Privileged => "privileged",
            Self::Standard => "standard",
        }
    }
}

/// Maps an audit `action` verb to [`AuditEventClass`] per `SEC-090`.
#[must_use]
pub fn classify_audit_action(action: &str) -> AuditEventClass {
    let lower = action.to_ascii_lowercase();
    let privileged_prefixes = [
        "mutate",
        "mutation",
        "approval",
        "approve",
        "decide_approval",
        "admin",
        "policy",
        "ingest.activate",
        "ingestion.activate",
        "proposal",
        "user_label",
        "routing.feedback.user_label",
        "purge",
        "audit.anchor",
        "system.startup",
        "system.reload",
        "snapshot.activate",
        "trust.",
        "revoke",
    ];
    if privileged_prefixes.iter().any(|pat| {
        lower == *pat
            || lower.starts_with(pat)
            || lower.contains(".admin")
            || lower.contains(".mutate")
    }) {
        AuditEventClass::Privileged
    } else {
        AuditEventClass::Standard
    }
}

/// Maps an [`AuditChain`] selector onto its isolated persistent chain key (`SEC-090`, `INV-09`).
#[must_use]
pub fn chain_storage_key(chain: &AuditChain) -> String {
    match chain {
        AuditChain::System => "system".to_owned(),
        AuditChain::Tenant(scope) => format!("tenant:{}", scope.tenant().as_str()),
    }
}

/// Validates required fields on an [`AuditEntry`] before persistence.
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] if `action`, `resource`, or `decision` is empty, or if
/// `details_json` is non-empty yet malformed JSON.
pub fn validate_entry(entry: &AuditEntry) -> Result<(), StoreError> {
    if entry.action.trim().is_empty() {
        return Err(StoreError::Invalid {
            field: "action",
            reason: "must be non-empty",
        });
    }
    if entry.resource.trim().is_empty() {
        return Err(StoreError::Invalid {
            field: "resource",
            reason: "must be non-empty",
        });
    }
    if entry.decision.trim().is_empty() {
        return Err(StoreError::Invalid {
            field: "decision",
            reason: "must be non-empty",
        });
    }
    if !entry.details_json.is_empty()
        && serde_json::from_str::<serde_json::Value>(&entry.details_json).is_err()
    {
        return Err(StoreError::Invalid {
            field: "details_json",
            reason: "must be valid JSON",
        });
    }
    Ok(())
}

/// Serializes the canonical JSON representation of a record excluding `hash` (`SEC-090`).
///
/// Keys appear in strict lexicographical order so hash verification is byte-deterministic across
/// `InMemoryAuditLog` and `SqliteAuditLog`.
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] if string JSON serialization fails.
pub fn canonical_record_without_hash_bytes(
    chain_key: &str,
    seq: u64,
    entry: &AuditEntry,
    prev_hash: &[u8; 32],
) -> Result<Vec<u8>, StoreError> {
    let json_err = |_| StoreError::Invalid {
        field: "audit_entry",
        reason: "canonical JSON encoding failed",
    };
    let chain_q = serde_json::to_string(chain_key).map_err(json_err)?;
    let action_q = serde_json::to_string(&entry.action).map_err(json_err)?;
    let actor_q = serde_json::to_string(&entry.actor_chain).map_err(json_err)?;
    let decision_q = serde_json::to_string(&entry.decision).map_err(json_err)?;
    let details_q = serde_json::to_string(&entry.details_json).map_err(json_err)?;
    let prev_hex = hex_encode_32(prev_hash);
    let prev_q = serde_json::to_string(&prev_hex).map_err(json_err)?;
    let resource_q = serde_json::to_string(&entry.resource).map_err(json_err)?;
    let ts_str = entry.ts.unix_nanos().to_string();
    let ts_q = serde_json::to_string(&ts_str).map_err(json_err)?;

    let canonical = format!(
        "{{\"action\":{action_q},\"actor_chain\":{actor_q},\"chain\":{chain_q},\"decision\":{decision_q},\"details_json\":{details_q},\"prev_hash\":{prev_q},\"resource\":{resource_q},\"seq\":{seq},\"ts_unix_nanos\":{ts_q}}}"
    );
    Ok(canonical.into_bytes())
}

/// Computes `blake3(prev_hash || canonical_json(record_without_hash))` per `SEC-090`.
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] if canonical encoding fails.
pub fn compute_record_hash(
    chain_key: &str,
    seq: u64,
    entry: &AuditEntry,
    prev_hash: &[u8; 32],
) -> Result<[u8; 32], StoreError> {
    let canonical = canonical_record_without_hash_bytes(chain_key, seq, entry, prev_hash)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(prev_hash);
    hasher.update(&canonical);
    Ok(*hasher.finalize().as_bytes())
}

/// Encodes a 32-byte digest into 64 lowercase hexadecimal characters.
#[must_use]
pub fn hex_encode_32(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Recomputes and verifies a contiguous slice of ordered audit records (`SEC-092`, `TM-17`).
///
/// Reports [`AuditVerifyReport::Inconsistent`] at the first sequence gap, reordering,
/// `prev_hash` mismatch, or recomputed digest mismatch.
#[must_use]
pub fn verify_ordered_records(
    chain_key: &str,
    records: &[AuditRecord],
    start_seq: u64,
    initial_prev_hash: [u8; 32],
) -> AuditVerifyReport {
    if records.is_empty() {
        return AuditVerifyReport::Intact {
            head_seq: 0,
            head_hash: GENESIS_HASH,
        };
    }

    let mut expected_seq = start_seq;
    let mut expected_prev = initial_prev_hash;
    let mut head_seq = 0u64;
    let mut head_hash = GENESIS_HASH;

    for record in records {
        if record.seq != expected_seq {
            return AuditVerifyReport::Inconsistent {
                seq: expected_seq,
                reason: format!(
                    "sequence continuity violated: expected seq {expected_seq}, found {}",
                    record.seq
                ),
            };
        }
        if record.prev_hash != expected_prev {
            return AuditVerifyReport::Inconsistent {
                seq: record.seq,
                reason: format!(
                    "prev_hash mismatch at seq {}: expected {}, found {}",
                    record.seq,
                    hex_encode_32(&expected_prev),
                    hex_encode_32(&record.prev_hash)
                ),
            };
        }
        let recomputed =
            match compute_record_hash(chain_key, record.seq, &record.entry, &expected_prev) {
                Ok(digest) => digest,
                Err(_) => {
                    return AuditVerifyReport::Inconsistent {
                        seq: record.seq,
                        reason: format!(
                            "canonical hash recomputation failed at seq {}",
                            record.seq
                        ),
                    };
                }
            };
        if record.hash != recomputed {
            return AuditVerifyReport::Inconsistent {
                seq: record.seq,
                reason: format!(
                    "digest mismatch at seq {}: expected {}, found {}",
                    record.seq,
                    hex_encode_32(&recomputed),
                    hex_encode_32(&record.hash)
                ),
            };
        }

        head_seq = record.seq;
        head_hash = record.hash;
        expected_prev = record.hash;
        expected_seq = record.seq.saturating_add(1);
    }

    AuditVerifyReport::Intact {
        head_seq,
        head_hash,
    }
}
