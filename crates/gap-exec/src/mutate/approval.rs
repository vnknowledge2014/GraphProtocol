//! Mutation approval summaries, `TaskStore` `INPUT_REQUIRED` gate (`RUN-071`, `RUN-085`),
//! and Ed25519 `ApprovalProof` signing/verification (`NGC-032`).

use std::sync::Arc;

use gap_core::clock::{Clock, Timestamp};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{ApprovalId, TaskId, TenantScope};
use gap_core::snapshot::SideEffectLevel;
use gap_proto::ngc::v1::ApprovalProof;
use gap_sec::crypto::{
    Ed25519SigningKey, Ed25519VerifyingKey, base64url_decode, base64url_encode, blake3_hash,
    hex_encode,
};
use gap_store::tasks::{
    DEFAULT_APPROVAL_TTL_MS, TaskEventRecord, TaskRecord, TaskStatus, TaskStore,
    TaskTransitionRequest,
};
use serde::{Deserialize, Serialize};

const APPROVAL_PROOF_DOMAIN: &[u8] = b"gap-approval-v1";

/// Checks whether a JSON key name represents secret/credential material (`RUN-071`).
#[must_use]
fn is_sensitive_arg_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("token")
        || lower.contains("credential")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("authorization")
        || lower.contains("cookie")
        || lower == "key"
}

/// Recursively canonicalizes and clones `val` with sorted object keys (`INV-04`, `NGC-032`).
fn canonicalize_json(val: &serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                if let Some(v) = map.get(k) {
                    out.insert(k.clone(), canonicalize_json(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json).collect())
        }
        other => other.clone(),
    }
}

/// Recursively clones `val` while masking sensitive keys (`RUN-071`, `INV-15`).
fn redact_sensitive_json(val: &serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                if is_sensitive_arg_key(k) {
                    out.insert(
                        k.clone(),
                        serde_json::Value::String("[REDACTED]".to_owned()),
                    );
                } else if let Some(v) = map.get(k) {
                    out.insert(k.clone(), redact_sensitive_json(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_sensitive_json).collect())
        }
        other => other.clone(),
    }
}

/// Computes the deterministic BLAKE3 digest binding `(operation, canonical_arguments)` (`NGC-032`).
#[must_use]
pub fn canonical_args_hash(operation: &str, args: &serde_json::Value) -> [u8; 32] {
    let canonical = canonicalize_json(args);
    let serialized = canonical.to_string();
    let mut payload = Vec::with_capacity(operation.len() + 1 + serialized.len());
    payload.extend_from_slice(operation.as_bytes());
    payload.push(0);
    payload.extend_from_slice(serialized.as_bytes());
    blake3_hash(&payload)
}

/// Generates a deterministic, template-only human approval summary without model text and with
/// sensitive arguments redacted (`RUN-071`, `SEC-083`).
#[must_use]
pub fn deterministic_approval_summary(
    operation: &str,
    target_branch: &str,
    side_effect: SideEffectLevel,
    args: &serde_json::Value,
) -> String {
    let redacted = redact_sensitive_json(args);
    let side_effect_label = match side_effect {
        SideEffectLevel::None => "NONE",
        SideEffectLevel::Reversible => "REVERSIBLE",
        SideEffectLevel::Irreversible => "IRREVERSIBLE",
        SideEffectLevel::Unknown => "UNKNOWN",
    };
    format!(
        "Approval required for mutation `{operation}` on branch `{target_branch}` (sideEffect={side_effect_label}): arguments={redacted}"
    )
}

fn approval_proof_signing_bytes(
    approval_id: &str,
    decided_by: &str,
    decided_at: Option<&prost_types::Timestamp>,
    operation: &str,
    args_hash: [u8; 32],
    idempotency_key: &str,
) -> Vec<u8> {
    let args_hex = hex_encode(&args_hash);
    let (secs, nanos) = decided_at.map_or((0_i64, 0_i32), |t| (t.seconds, t.nanos));
    let mut msg = Vec::with_capacity(
        APPROVAL_PROOF_DOMAIN.len()
            + approval_id.len()
            + decided_by.len()
            + operation.len()
            + args_hex.len()
            + idempotency_key.len()
            + 32,
    );
    msg.extend_from_slice(APPROVAL_PROOF_DOMAIN);
    msg.push(0);
    msg.extend_from_slice(approval_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(decided_by.as_bytes());
    msg.push(0);
    msg.extend_from_slice(&secs.to_le_bytes());
    msg.extend_from_slice(&nanos.to_le_bytes());
    msg.push(0);
    msg.extend_from_slice(operation.as_bytes());
    msg.push(0);
    msg.extend_from_slice(args_hex.as_bytes());
    msg.push(0);
    msg.extend_from_slice(idempotency_key.as_bytes());
    msg
}

fn timestamp_to_prost(ts: Timestamp) -> prost_types::Timestamp {
    let total_nanos = ts.unix_nanos();
    let seconds = i64::try_from(total_nanos.div_euclid(1_000_000_000)).unwrap_or(0);
    let nanos = i32::try_from(total_nanos.rem_euclid(1_000_000_000)).unwrap_or(0);
    prost_types::Timestamp { seconds, nanos }
}

/// Signs an NGC [`ApprovalProof`] using the gateway Ed25519 key bound to `(operation, args_hash, idempotency_key)` (`NGC-032`, `RUN-071`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if identifiers/keys are empty, or [`ErrorCode::Internal`]
/// on cryptographic signing failure.
pub fn sign_approval_proof(
    signing_key: &Ed25519SigningKey,
    approval_id: &str,
    decided_by: &str,
    decided_at: Timestamp,
    operation: &str,
    args_hash: [u8; 32],
    idempotency_key: &str,
) -> GapResult<ApprovalProof> {
    if approval_id.is_empty()
        || decided_by.is_empty()
        || operation.is_empty()
        || idempotency_key.is_empty()
    {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "approval_id, decided_by, operation, and idempotency_key must be non-empty",
        ));
    }
    let prost_ts = timestamp_to_prost(decided_at);
    let message = approval_proof_signing_bytes(
        approval_id,
        decided_by,
        Some(&prost_ts),
        operation,
        args_hash,
        idempotency_key,
    );
    let sig = signing_key.sign(&message)?;
    Ok(ApprovalProof {
        approval_id: approval_id.to_owned(),
        decided_by: decided_by.to_owned(),
        decided_at: Some(prost_ts),
        signature: base64url_encode(&sig),
    })
}

/// Verifies an [`ApprovalProof`] signature, `(operation, args_hash, idempotency_key)` binding,
/// and anti-replay tracker (`NGC-032`).
///
/// # Errors
///
/// Returns [`ErrorCode::Forbidden`] when the proof is missing timestamp/signature, fails Ed25519
/// verification, binds different `(operation, args_hash, idempotency_key)`, or was already replayed.
pub fn verify_approval_proof(
    verifying_key: &Ed25519VerifyingKey,
    proof: &ApprovalProof,
    expected_operation: &str,
    expected_args_hash: [u8; 32],
    expected_idempotency_key: &str,
    replay_cache: Option<&ApprovalReplayGuard>,
) -> GapResult<()> {
    if proof.approval_id.is_empty() || proof.decided_by.is_empty() || proof.decided_at.is_none() {
        return Err(GapError::new(
            ErrorCode::Forbidden,
            "incomplete approval proof metadata",
        ));
    }
    let raw_sig = base64url_decode(&proof.signature)
        .map_err(|_| GapError::new(ErrorCode::Forbidden, "malformed approval proof signature"))?;
    let msg = approval_proof_signing_bytes(
        &proof.approval_id,
        &proof.decided_by,
        proof.decided_at.as_ref(),
        expected_operation,
        expected_args_hash,
        expected_idempotency_key,
    );
    verifying_key.verify(&msg, &raw_sig).map_err(|_| {
        GapError::new(
            ErrorCode::Forbidden,
            "invalid approval proof signature or argument binding",
        )
    })?;

    if let Some(guard) = replay_cache
        && !guard.consume_once(&proof.approval_id, expected_idempotency_key)
    {
        return Err(GapError::new(
            ErrorCode::Forbidden,
            "approval proof replay rejected",
        ));
    }
    Ok(())
}

/// Thread-safe anti-replay set tracking `(approval_id -> idempotency_key)` (`NGC-032`).
///
/// Reusing the same `approval_id` across a different `idempotency_key` (or distinct mutation)
/// is rejected as a replay attack, while matching an already-committed `idempotency_key` for
/// idempotent response replay does not need connector re-verification.
#[derive(Debug, Default)]
pub struct ApprovalReplayGuard {
    consumed: crate::lockfree::LockFreeLog<String>,
}

impl ApprovalReplayGuard {
    /// Creates an empty replay guard.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `approval_id` as consumed for `idempotency_key`; returns `false` on replay.
    #[must_use]
    pub fn consume_once(&self, approval_id: &str, idempotency_key: &str) -> bool {
        let token = format!("{approval_id}|{idempotency_key}");
        self.consumed.insert_unique(token)
    }

    /// Records `approval_id` unconditionally (single-use regardless of key).
    #[must_use]
    pub fn record_approval_used(&self, approval_id: &str) -> bool {
        self.consumed.insert_unique(approval_id.to_owned())
    }
}

/// Durable metadata attached to a pending approval task (`RUN-071`, `RUN-085`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// Approval identifier (`UUIDv7`).
    pub approval_id: ApprovalId,
    /// Task identifier held in `TaskStatus::InputRequired`.
    pub task_id: TaskId,
    /// Requesting principal ID (checked against four-eyes approver).
    pub requester_id: String,
    /// Target operation (`<branch>.<name>`).
    pub operation: String,
    /// Deterministic hex digest of canonical arguments.
    pub args_hash_hex: String,
    /// Idempotency key bound to the pending mutation.
    pub idempotency_key: String,
    /// Deterministic, redacted summary presented to human reviewers.
    pub summary: String,
    /// Verified side-effect level.
    pub side_effect: SideEffectLevel,
    /// Whether `decided_by != requester_id` is mandatory.
    pub four_eyes: bool,
    /// Approval creation timestamp.
    pub created_at: Timestamp,
    /// Expiration timestamp (`created_at + 24h`, `RUN-085`).
    pub expires_at: Timestamp,
}

/// Human decision on a pending approval task (`RUN-071`, `RUN-085`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecisionOutcome {
    /// Approved with gateway Ed25519 signature ready to attach to the mutation retry.
    Approved {
        /// Task transitioned back to `WORKING`.
        task: TaskRecord,
        /// Signed proof bound to `(operation, args_hash, idempotency_key)`.
        proof: ApprovalProof,
    },
    /// Rejected either explicitly by reviewer or due to 24 h expiry (`reason = "approval_expired"`).
    Rejected {
        /// Terminal `REJECTED` task record.
        task: TaskRecord,
        /// Rejection reason (`"approval_expired"` or caller rejection reason).
        reason: String,
    },
}

/// Manages `TaskStore`-backed mutation approval tasks (`RUN-071`, `RUN-085`, `NGC-032`).
pub struct ApprovalGate {
    store: Arc<dyn TaskStore>,
    clock: Arc<dyn Clock>,
    signing_key: Ed25519SigningKey,
    ttl_ms: i64,
}

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("ttl_ms", &self.ttl_ms)
            .finish()
    }
}

impl ApprovalGate {
    /// Creates an [`ApprovalGate`] with default 24 h `approval.ttl` (`RUN-085`).
    #[must_use]
    pub fn new(
        store: Arc<dyn TaskStore>,
        clock: Arc<dyn Clock>,
        signing_key: Ed25519SigningKey,
    ) -> Self {
        Self {
            store,
            clock,
            signing_key,
            ttl_ms: DEFAULT_APPROVAL_TTL_MS,
        }
    }

    /// Overrides the default 24 h approval TTL in milliseconds.
    #[must_use]
    pub const fn with_ttl_ms(mut self, ttl_ms: i64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Returns the [`Ed25519VerifyingKey`] distributed to high-risk connectors (`NGC-032`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if public key encoding fails.
    pub fn verifying_key(&self) -> GapResult<Ed25519VerifyingKey> {
        self.signing_key.verifying_key()
    }

    /// Opens an approval task in `SUBMITTED -> WORKING -> INPUT_REQUIRED` (`RUN-071`, `RUN-085`).
    ///
    /// # Errors
    ///
    /// Returns storage/validation errors mapped into [`GapError`].
    #[allow(clippy::too_many_arguments)]
    pub fn request_approval(
        &self,
        tenant: &TenantScope,
        requester_id: &str,
        target_branch: &str,
        operation: &str,
        side_effect: SideEffectLevel,
        args: &serde_json::Value,
        idempotency_key: &str,
        four_eyes: bool,
    ) -> GapResult<PendingApproval> {
        let now = self.clock.now_timestamp();
        let expires_at = Timestamp::from_unix_millis(now.unix_millis().saturating_add(self.ttl_ms))
            .map_err(|_| GapError::new(ErrorCode::ValidationError, "approval expiry overflow"))?;
        let args_hash = canonical_args_hash(operation, args);
        let args_hash_hex = hex_encode(&args_hash);
        let summary = deterministic_approval_summary(operation, target_branch, side_effect, args);
        let approval_id = ApprovalId::new_v7();
        let task_id = TaskId::new_v7();

        let pending = PendingApproval {
            approval_id,
            task_id,
            requester_id: requester_id.to_owned(),
            operation: operation.to_owned(),
            args_hash_hex,
            idempotency_key: idempotency_key.to_owned(),
            summary,
            side_effect,
            four_eyes,
            created_at: now,
            expires_at,
        };
        let payload_json = serde_json::to_string(&pending).map_err(|err| {
            GapError::new(
                ErrorCode::Internal,
                format!("failed to serialize pending approval: {err}"),
            )
        })?;

        let initial_record = TaskRecord {
            task_id,
            status: TaskStatus::Submitted,
            payload_json: payload_json.clone(),
            lease_owner: None,
            lease_expires_at: None,
            updated_at: now,
        };
        let initial_event = TaskEventRecord {
            task_id,
            seq: 1,
            status: TaskStatus::Submitted,
            payload_json: "{\"phase\":\"submitted\"}".to_owned(),
            emitted_at: now,
        };
        self.store
            .put_task(tenant, &initial_record, &initial_event)
            .map_err(|e| e.into_gap_error("approval-put"))?;

        self.store
            .transition_task(
                tenant,
                &TaskTransitionRequest {
                    task_id,
                    to_status: TaskStatus::Working,
                    payload_json: None,
                    lease_owner: Some(Some("approval-gate".to_owned())),
                    lease_expires_at: Some(Some(expires_at)),
                    event_payload_json: "{\"phase\":\"evaluating_approval\"}".to_owned(),
                    now,
                },
            )
            .map_err(|e| e.into_gap_error("approval-working"))?;

        self.store
            .transition_task(
                tenant,
                &TaskTransitionRequest {
                    task_id,
                    to_status: TaskStatus::InputRequired,
                    payload_json: Some(payload_json),
                    lease_owner: Some(None),
                    lease_expires_at: Some(None),
                    event_payload_json: serde_json::json!({
                        "phase": "input_required",
                        "approvalId": pending.approval_id.to_string(),
                        "expiresAt": pending.expires_at.to_string(),
                    })
                    .to_string(),
                    now,
                },
            )
            .map_err(|e| e.into_gap_error("approval-input-required"))?;

        Ok(pending)
    }

    /// Resolves a pending approval task with four-eyes and expiry enforcement (`RUN-071`, `RUN-085`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::TaskNotFound`] when missing, [`ErrorCode::TaskInvalidState`] if not in
    /// `INPUT_REQUIRED`, or [`ErrorCode::Forbidden`] when `four_eyes` self-approval is attempted.
    pub fn decide_approval(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
        approver_id: &str,
        approve: bool,
        rejection_reason: Option<&str>,
    ) -> GapResult<ApprovalDecisionOutcome> {
        let now = self.clock.now_timestamp();
        let record = self
            .store
            .get_task(tenant, task_id)
            .map_err(|e| e.into_gap_error("approval-get"))?
            .ok_or_else(|| GapError::new(ErrorCode::TaskNotFound, "approval task not found"))?;

        if record.status != TaskStatus::InputRequired {
            return Err(GapError::new(
                ErrorCode::TaskInvalidState,
                "task is not awaiting approval input",
            ));
        }

        let pending: PendingApproval =
            serde_json::from_str(&record.payload_json).map_err(|_| {
                GapError::new(ErrorCode::Internal, "corrupt pending approval task payload")
            })?;

        if now >= pending.expires_at {
            let expired_reason = "approval_expired".to_owned();
            let (rejected, _) = self
                .store
                .transition_task(
                    tenant,
                    &TaskTransitionRequest {
                        task_id,
                        to_status: TaskStatus::Rejected,
                        payload_json: None,
                        lease_owner: Some(None),
                        lease_expires_at: Some(None),
                        event_payload_json: serde_json::json!({
                            "phase": "rejected",
                            "reason": expired_reason,
                        })
                        .to_string(),
                        now,
                    },
                )
                .map_err(|e| e.into_gap_error("approval-expire"))?;
            return Ok(ApprovalDecisionOutcome::Rejected {
                task: rejected,
                reason: "approval_expired".to_owned(),
            });
        }

        if pending.four_eyes && approver_id == pending.requester_id {
            return Err(GapError::new(
                ErrorCode::Forbidden,
                "four-eyes policy prohibits self-approval by requester",
            ));
        }

        if !approve {
            let reason = rejection_reason.unwrap_or("denied_by_reviewer").to_owned();
            let (rejected, _) = self
                .store
                .transition_task(
                    tenant,
                    &TaskTransitionRequest {
                        task_id,
                        to_status: TaskStatus::Rejected,
                        payload_json: None,
                        lease_owner: Some(None),
                        lease_expires_at: Some(None),
                        event_payload_json: serde_json::json!({
                            "phase": "rejected",
                            "reason": reason,
                            "decidedBy": approver_id,
                        })
                        .to_string(),
                        now,
                    },
                )
                .map_err(|e| e.into_gap_error("approval-reject"))?;
            return Ok(ApprovalDecisionOutcome::Rejected {
                task: rejected,
                reason,
            });
        }

        let args_hash_bytes = gap_sec::crypto::hex_decode(&pending.args_hash_hex)?;
        let args_hash: [u8; 32] = args_hash_bytes.try_into().map_err(|_| {
            GapError::new(
                ErrorCode::Internal,
                "stored approval args_hash has wrong width",
            )
        })?;
        let approval_id_str = pending.approval_id.to_string();
        let proof = sign_approval_proof(
            &self.signing_key,
            &approval_id_str,
            approver_id,
            now,
            &pending.operation,
            args_hash,
            &pending.idempotency_key,
        )?;

        let (working, _) = self
            .store
            .transition_task(
                tenant,
                &TaskTransitionRequest {
                    task_id,
                    to_status: TaskStatus::Working,
                    payload_json: None,
                    lease_owner: Some(Some("mutation-pipeline".to_owned())),
                    lease_expires_at: Some(Some(pending.expires_at)),
                    event_payload_json: serde_json::json!({
                        "phase": "approved",
                        "approvalId": approval_id_str,
                        "decidedBy": approver_id,
                    })
                    .to_string(),
                    now,
                },
            )
            .map_err(|e| e.into_gap_error("approval-resume"))?;

        Ok(ApprovalDecisionOutcome::Approved {
            task: working,
            proof,
        })
    }
}
