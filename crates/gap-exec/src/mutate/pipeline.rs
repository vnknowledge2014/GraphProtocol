//! Deterministic mutation execution pipeline:
//! `authz -> side-effect policy -> approval gate -> idempotency -> dry-run -> connector.Mutate -> audit`
//! (`RUN-070`..`RUN-075`, `NGC-030`..`NGC-033`, `INV-11`, `INV-14`).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, Timestamp};
use gap_core::context::Principal;
use gap_core::error::{ErrorCode, GapError, GapResult, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{IdempotencyKey, TenantScope};
use gap_core::snapshot::SideEffectLevel;
use gap_ngc_sdk::connector::{CallScope, Connector, NgcResult};
use gap_proto::ngc::v1::{
    ApprovalProof, CallContext, MutateRequest, MutateResponse, MutationOutcome,
    Principal as NgcPrincipal, PrincipalKind as NgcPrincipalKind,
};
use gap_sec::authz::{
    AuthzAction, AuthzContext, CompiledPolicySet, DecisionKind, GENERIC_FORBIDDEN_MESSAGE,
    ResourceAttributes,
};
use gap_sec::crypto::{Ed25519VerifyingKey, blake3_hash, hex_encode};
use gap_store::audit::{AuditChain, AuditEntry, AuditLog};
use gap_store::idempotency::{
    DEFAULT_IDEMPOTENCY_TTL_MS, IdempotencyOutcome, IdempotencyStatus, IdempotencyStore,
};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant as TokioInstant, sleep};
use tokio_util::sync::CancellationToken;

use super::approval::{
    ApprovalGate, ApprovalReplayGuard, PendingApproval, canonical_args_hash, verify_approval_proof,
};

/// Default polling interval while waiting for an in-flight duplicate idempotency key (`RUN-072`).
pub const IDEMPOTENCY_POLL_INTERVAL_MS: u64 = 10;

/// Object-safe async connector interface for executing `Mutate` RPCs (`NGC-030`..`NGC-033`).
pub trait MutationTarget: Send + Sync {
    /// Invokes the underlying branch connector's `Mutate` operation under `scope`.
    fn mutate(
        &self,
        request: MutateRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<MutateResponse>> + Send + '_>>;
}

/// Blanket adapter wrapping any static [`Connector`] as a [`MutationTarget`].
#[derive(Debug, Clone)]
pub struct SdkMutationTarget<C> {
    connector: Arc<C>,
}

impl<C: Connector> SdkMutationTarget<C> {
    /// Wraps `connector` in a [`MutationTarget`].
    #[must_use]
    pub fn new(connector: Arc<C>) -> Self {
        Self { connector }
    }
}

impl<C: Connector> MutationTarget for SdkMutationTarget<C> {
    fn mutate(
        &self,
        request: MutateRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<MutateResponse>> + Send + '_>> {
        let connector = Arc::clone(&self.connector);
        Box::pin(async move { connector.mutate(request, scope).await })
    }
}

/// Input specification for executing a single mutation through [`MutationPipeline`].
#[derive(Clone)]
pub struct MutationExecutionRequest {
    /// Non-forgeable caller tenant scope (`INV-09`).
    pub tenant_scope: TenantScope,
    /// Correlation request identifier.
    pub request_id: String,
    /// Authenticated caller principal (`SEC-010`).
    pub principal: Principal,
    /// Delegation chain subjects (`SEC-012`).
    pub delegators: Vec<Principal>,
    /// Target resource attributes evaluated by Cedar.
    pub resource: ResourceAttributes,
    /// Cedar request evaluation context.
    pub authz_context: AuthzContext,
    /// Target branch slug.
    pub target_branch: String,
    /// Qualified mutation operation `<branch>.<name>`.
    pub operation: String,
    /// JSON mutation arguments.
    pub arguments: serde_json::Value,
    /// Verified side-effect level (`RUN-070`; untrusted hints already resolved to `UNKNOWN`).
    pub side_effect: SideEffectLevel,
    /// Declared `@compensate` operation name required for `REVERSIBLE` / saga mode (`RUN-070`, `RUN-074`).
    pub compensate_operation: Option<String>,
    /// Whether target connector advertises `capabilities.idempotency` (`NGC-030`).
    pub connector_supports_idempotency: bool,
    /// Whether target connector advertises `capabilities.dry_run` (`NGC-031`, `RUN-073`).
    pub connector_supports_dry_run: bool,
    /// Requested dry-run validation (`@dryRun` / `dryRun: true`, `RUN-073`).
    pub dry_run: bool,
    /// Whether human approval must come from a different principal than requester (`RUN-071`).
    pub four_eyes: bool,
    /// Client-supplied `Idempotency-Key` (`RUN-072`); minted automatically as UUIDv7 when absent.
    pub idempotency_key: Option<String>,
    /// Signed [`ApprovalProof`] presented when resuming after human approval (`NGC-032`).
    pub approval_proof: Option<ApprovalProof>,
    /// Optional compiled `policies/auto-approve.cedar` policy set (`RUN-070`).
    pub auto_approve_policies: Option<Arc<CompiledPolicySet>>,
    /// Per-call deadline budget in milliseconds (`RUN-030`, `RUN-072`).
    pub deadline_ms: u64,
    /// Cancellation token for cooperative cancellation (`INV-12`).
    pub cancel: CancellationToken,
}

/// Outcome returned by [`MutationPipeline::execute`] (`RUN-070`..`RUN-075`).
#[derive(Debug, Clone, PartialEq)]
pub enum MutationExecutionResult {
    /// Mutation applied (or replayed idempotently) by the branch connector.
    Applied {
        /// Parsed JSON result returned by connector.
        result: serde_json::Value,
        /// Effective idempotency key bound to this execution.
        idempotency_key: String,
        /// Whether this request is retryable (always `false` when connector lacks idempotency, `NGC-030`).
        idempotent: bool,
        /// `true` if replayed from [`IdempotencyStore`] or connector `was_replayed`, without re-mutating (`RUN-072`).
        was_replayed: bool,
        /// Optional connector compensation token preserved for saga rollbacks (`NGC-033`, `RUN-074`).
        compensation_token: Option<String>,
    },
    /// Connector verified validation/plan without changing state (`RUN-073`, `NGC-031`).
    DryRun {
        /// Dry-run report payload emitted by connector.
        dry_run_report: serde_json::Value,
        /// Effective idempotency key.
        idempotency_key: String,
    },
    /// Pipeline stopped at approval gate (`TaskStatus::InputRequired`, `RUN-071`, `GAP_APPROVAL_REQUIRED`).
    ApprovalRequired {
        /// Durable pending approval task record with redacted summary and expiry.
        pending: PendingApproval,
    },
}

/// Persisted JSON envelope inside [`IdempotencyStore`] `result_ref` for lossless replay (`RUN-072`).
#[derive(Debug, Serialize, Deserialize)]
struct StoredReplayPayload {
    result: serde_json::Value,
    compensation_token: Option<String>,
}

/// Dependencies required to construct [`MutationPipeline`].
pub struct MutationPipelineConfig {
    /// Clock used for timestamps, idempotency TTL, and approval checks.
    pub clock: Arc<dyn Clock>,
    /// Active Cedar policy set for `AuthzAction::Mutate`.
    pub policy: Arc<CompiledPolicySet>,
    /// Durable tenant-isolated idempotency store (`RUN-072`).
    pub idempotency: Arc<dyn IdempotencyStore>,
    /// Durable hash-chained audit log (`RUN-075`, `INV-14`).
    pub audit: Arc<dyn AuditLog>,
    /// Optional human-approval task gate (`RUN-071`, `RUN-085`).
    pub approval_gate: Option<Arc<ApprovalGate>>,
    /// Gateway public key used to verify `ApprovalProof` (`NGC-032`).
    pub approval_verifying_key: Option<Ed25519VerifyingKey>,
    /// Anti-replay tracker for `ApprovalProof` consumption (`NGC-032`).
    pub approval_replay_guard: Arc<ApprovalReplayGuard>,
}

/// Deterministic mutation execution pipeline enforcing `RUN-070`..`RUN-075` and `NGC-030`..`NGC-032`.
pub struct MutationPipeline {
    clock: Arc<dyn Clock>,
    policy: Arc<CompiledPolicySet>,
    idempotency: Arc<dyn IdempotencyStore>,
    audit: Arc<dyn AuditLog>,
    approval_gate: Option<Arc<ApprovalGate>>,
    approval_verifying_key: Option<Ed25519VerifyingKey>,
    approval_replay_guard: Arc<ApprovalReplayGuard>,
}

impl std::fmt::Debug for MutationPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutationPipeline").finish()
    }
}

struct MutationAuditEvent<'a> {
    tenant: &'a TenantScope,
    principal_chain: Vec<String>,
    operation: &'a str,
    args_hash_hex: &'a str,
    decision: &'a str,
    approval_id: Option<&'a str>,
    outcome: &'a str,
}

impl MutationPipeline {
    /// Constructs a [`MutationPipeline`] from its dependencies.
    #[must_use]
    pub fn new(cfg: MutationPipelineConfig) -> Self {
        Self {
            clock: cfg.clock,
            policy: cfg.policy,
            idempotency: cfg.idempotency,
            audit: cfg.audit,
            approval_gate: cfg.approval_gate,
            approval_verifying_key: cfg.approval_verifying_key,
            approval_replay_guard: cfg.approval_replay_guard,
        }
    }

    fn append_audit(&self, event: MutationAuditEvent<'_>) {
        let ts = self.clock.now_timestamp();
        let entry = AuditEntry {
            ts,
            actor_chain: event.principal_chain,
            action: event.operation.to_owned(),
            resource: event.operation.to_owned(),
            decision: event.decision.to_owned(),
            details_json: serde_json::json!({
                "args_hash": event.args_hash_hex,
                "approval_id": event.approval_id,
                "outcome": event.outcome,
            })
            .to_string(),
        };
        let _ = self
            .audit
            .append_chain(&AuditChain::Tenant(event.tenant.clone()), &entry);
    }

    /// Executes one mutation attempt end-to-end (`RUN-070`..`RUN-075`).
    ///
    /// # Errors
    ///
    /// Returns typed [`GapError`] codes (`GAP_FORBIDDEN`, `GAP_VALIDATION_ERROR`,
    /// `GAP_IDEMPOTENCY_CONFLICT`, `GAP_DRY_RUN_UNSUPPORTED`, `GAP_DEADLINE_EXCEEDED`, etc.)
    /// while recording an audit entry on every attempt (`RUN-075`, `INV-11`).
    #[allow(clippy::too_many_lines)]
    pub async fn execute(
        &self,
        req: MutationExecutionRequest,
        target: &dyn MutationTarget,
    ) -> GapResult<MutationExecutionResult> {
        let args_hash = canonical_args_hash(&req.operation, &req.arguments);
        let args_hash_hex = hex_encode(&args_hash);
        let idempotency_key = req
            .idempotency_key
            .clone()
            .unwrap_or_else(|| IdempotencyKey::generate_v7().to_string());
        let proof_id = req.approval_proof.as_ref().map(|p| p.approval_id.as_str());
        let requires_approval = matches!(
            req.side_effect,
            SideEffectLevel::Irreversible | SideEffectLevel::Unknown
        ) || req.authz_context.risk_score >= 100;
        let auto_approved = if requires_approval {
            req.auto_approve_policies.as_ref().is_some_and(|pol| {
                pol.evaluate_with_delegators(
                    &req.principal,
                    &req.delegators,
                    AuthzAction::Mutate,
                    &req.resource,
                    &req.authz_context,
                )
                .decision
                    == DecisionKind::Allow
            })
        } else {
            false
        };

        // 1. Cedar authorization check (including delegation intersection).
        let eval = self.policy.evaluate_with_delegators(
            &req.principal,
            &req.delegators,
            AuthzAction::Mutate,
            &req.resource,
            &req.authz_context,
        );
        let actor_chain = eval.evaluated_actors.clone();
        let defer_to_approval_gate = eval.decision == DecisionKind::Deny
            && requires_approval
            && !auto_approved
            && !req.dry_run
            && req.approval_proof.is_none()
            && !eval.determining_policies.is_empty()
            && eval.determining_policies.iter().all(|id| {
                id == "forbid-mutate-irreversible-or-unknown-unapproved"
                    || id == "forbid-mutate-rule-of-two-high-risk"
            })
            && {
                let mut pre_approval_ctx = req.authz_context.clone();
                pre_approval_ctx.approved = true;
                pre_approval_ctx.approver_is_different = true;
                self.policy
                    .evaluate_with_delegators(
                        &req.principal,
                        &req.delegators,
                        AuthzAction::Mutate,
                        &req.resource,
                        &pre_approval_ctx,
                    )
                    .decision
                    == DecisionKind::Allow
            };
        if eval.decision == DecisionKind::Deny && !defer_to_approval_gate {
            self.append_audit(MutationAuditEvent {
                tenant: &req.tenant_scope,
                principal_chain: actor_chain,
                operation: &req.operation,
                args_hash_hex: &args_hash_hex,
                decision: "DENY",
                approval_id: proof_id,
                outcome: "authz_denied",
            });
            return Err(GapError::new(
                ErrorCode::Forbidden,
                GENERIC_FORBIDDEN_MESSAGE,
            ));
        }

        // 2. Side-effect policy (RUN-070).
        if req.side_effect == SideEffectLevel::Reversible && req.compensate_operation.is_none() {
            self.append_audit(MutationAuditEvent {
                tenant: &req.tenant_scope,
                principal_chain: actor_chain,
                operation: &req.operation,
                args_hash_hex: &args_hash_hex,
                decision: "DENY",
                approval_id: proof_id,
                outcome: "missing_compensate_for_reversible",
            });
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "reversible mutation requires declared @compensate operation",
            ));
        }

        // 3. Approval gate / signed proof verification (RUN-071, NGC-032).
        if requires_approval && !auto_approved && !req.dry_run {
            if let Some(ref proof) = req.approval_proof {
                if req.four_eyes && proof.decided_by == req.principal.id {
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "DENY",
                        approval_id: Some(&proof.approval_id),
                        outcome: "four_eyes_violation",
                    });
                    return Err(GapError::new(
                        ErrorCode::Forbidden,
                        "four-eyes policy prohibits self-approval",
                    ));
                }
                let Some(ref vk) = self.approval_verifying_key else {
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "DENY",
                        approval_id: Some(&proof.approval_id),
                        outcome: "missing_verifying_key",
                    });
                    return Err(GapError::new(
                        ErrorCode::Internal,
                        "gateway approval verifying key is not configured",
                    ));
                };
                if let Err(err) = verify_approval_proof(
                    vk,
                    proof,
                    &req.operation,
                    args_hash,
                    &idempotency_key,
                    Some(&self.approval_replay_guard),
                ) {
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "DENY",
                        approval_id: Some(&proof.approval_id),
                        outcome: "invalid_or_replayed_approval_proof",
                    });
                    return Err(err);
                }
            } else {
                let Some(ref gate) = self.approval_gate else {
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "DENY",
                        approval_id: None,
                        outcome: "approval_required_no_gate",
                    });
                    return Err(GapError::new(
                        ErrorCode::ApprovalRequired,
                        "mutation requires human approval",
                    ));
                };
                let pending = gate.request_approval(
                    &req.tenant_scope,
                    &req.principal.id,
                    &req.target_branch,
                    &req.operation,
                    req.side_effect,
                    &req.arguments,
                    &idempotency_key,
                    req.four_eyes,
                )?;
                let pending_id = pending.approval_id.to_string();
                self.append_audit(MutationAuditEvent {
                    tenant: &req.tenant_scope,
                    principal_chain: actor_chain,
                    operation: &req.operation,
                    args_hash_hex: &args_hash_hex,
                    decision: "INPUT_REQUIRED",
                    approval_id: Some(&pending_id),
                    outcome: "awaiting_human_approval",
                });
                return Ok(MutationExecutionResult::ApprovalRequired { pending });
            }
        }

        // 4. Dry-run branch (RUN-073, NGC-031 — never simulated).
        if req.dry_run {
            if !req.connector_supports_dry_run {
                self.append_audit(MutationAuditEvent {
                    tenant: &req.tenant_scope,
                    principal_chain: actor_chain,
                    operation: &req.operation,
                    args_hash_hex: &args_hash_hex,
                    decision: "DENY",
                    approval_id: proof_id,
                    outcome: "dry_run_unsupported",
                });
                return Err(GapError::new(
                    ErrorCode::DryRunUnsupported,
                    "connector does not support dryRun",
                ));
            }
            let ngc_req = build_mutate_request(&req, &idempotency_key, true);
            let scope = build_call_scope(&req);
            match target.mutate(ngc_req, scope).await {
                Ok(resp) => {
                    if let Some(err) = resp.error {
                        let mapped = map_mutate_ngc_error(err.code, &err.message, true);
                        self.append_audit(MutationAuditEvent {
                            tenant: &req.tenant_scope,
                            principal_chain: actor_chain,
                            operation: &req.operation,
                            args_hash_hex: &args_hash_hex,
                            decision: "ALLOW",
                            approval_id: proof_id,
                            outcome: "dry_run_connector_error",
                        });
                        return Err(mapped);
                    }
                    let report = resp
                        .dry_run_report
                        .as_ref()
                        .map_or(serde_json::Value::Null, prost_struct_to_json);
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "ALLOW",
                        approval_id: proof_id,
                        outcome: "dry_run_ok",
                    });
                    return Ok(MutationExecutionResult::DryRun {
                        dry_run_report: report,
                        idempotency_key,
                    });
                }
                Err(ngc_err) => {
                    let mapped = ngc_err.into_dry_run_gap_error();
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "ALLOW",
                        approval_id: proof_id,
                        outcome: "dry_run_rpc_error",
                    });
                    return Err(mapped);
                }
            }
        }

        // 5. Idempotency reservation / wait / replay (RUN-072, NGC-030).
        let idemp_request_hash = compute_idempotency_request_hash(
            req.tenant_scope.tenant().as_str(),
            &req.operation,
            &args_hash,
        );
        let mut owns_reservation = false;
        if req.connector_supports_idempotency {
            let now_ms = self.clock.now_timestamp().unix_millis();
            let expires_at =
                Timestamp::from_unix_millis(now_ms.saturating_add(DEFAULT_IDEMPOTENCY_TTL_MS))
                    .map_err(|_| {
                        GapError::new(ErrorCode::Internal, "idempotency expiration overflow")
                    })?;

            match self
                .idempotency
                .check_or_reserve(
                    &req.tenant_scope,
                    &idempotency_key,
                    idemp_request_hash,
                    expires_at,
                )
                .map_err(|e| e.into_gap_error(&req.request_id))?
            {
                IdempotencyOutcome::Reserved => {
                    owns_reservation = true;
                }
                IdempotencyOutcome::Conflict => {
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "DENY",
                        approval_id: proof_id,
                        outcome: "idempotency_conflict",
                    });
                    return Err(GapError::new(
                        ErrorCode::IdempotencyConflict,
                        "idempotency key reused with different operation or arguments",
                    ));
                }
                IdempotencyOutcome::Replay { result_ref } => {
                    let stored = decode_stored_replay(result_ref.as_deref())?;
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "ALLOW",
                        approval_id: proof_id,
                        outcome: "replayed",
                    });
                    return Ok(MutationExecutionResult::Applied {
                        result: stored.result,
                        idempotency_key,
                        idempotent: true,
                        was_replayed: true,
                        compensation_token: stored.compensation_token,
                    });
                }
                IdempotencyOutcome::InProgress => {
                    let waited = self
                        .wait_for_in_progress(
                            &req.tenant_scope,
                            &idempotency_key,
                            idemp_request_hash,
                            Duration::from_millis(req.deadline_ms.max(50)),
                            &req.cancel,
                        )
                        .await;
                    match waited {
                        Ok(stored) => {
                            self.append_audit(MutationAuditEvent {
                                tenant: &req.tenant_scope,
                                principal_chain: actor_chain,
                                operation: &req.operation,
                                args_hash_hex: &args_hash_hex,
                                decision: "ALLOW",
                                approval_id: proof_id,
                                outcome: "replayed_after_wait",
                            });
                            return Ok(MutationExecutionResult::Applied {
                                result: stored.result,
                                idempotency_key,
                                idempotent: true,
                                was_replayed: true,
                                compensation_token: stored.compensation_token,
                            });
                        }
                        Err(err) => {
                            self.append_audit(MutationAuditEvent {
                                tenant: &req.tenant_scope,
                                principal_chain: actor_chain,
                                operation: &req.operation,
                                args_hash_hex: &args_hash_hex,
                                decision: "ALLOW",
                                approval_id: proof_id,
                                outcome: "in_progress_wait_failed",
                            });
                            return Err(err);
                        }
                    }
                }
            }
        }

        // 6. Execute Mutate against target connector (RUN-075, NGC-030, NGC-033).
        let ngc_req = build_mutate_request(&req, &idempotency_key, false);
        let scope = build_call_scope(&req);
        match target.mutate(ngc_req, scope).await {
            Ok(resp) => {
                if let Some(err) = resp.error {
                    if owns_reservation {
                        let _ = self.idempotency.release_in_progress(
                            &req.tenant_scope,
                            &idempotency_key,
                            idemp_request_hash,
                        );
                    }
                    let mapped = map_mutate_ngc_error(err.code, &err.message, false);
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "ALLOW",
                        approval_id: proof_id,
                        outcome: "connector_rejected",
                    });
                    return Err(mapped);
                }
                if resp.outcome == MutationOutcome::OutcomeRejected as i32 {
                    if owns_reservation {
                        let _ = self.idempotency.release_in_progress(
                            &req.tenant_scope,
                            &idempotency_key,
                            idemp_request_hash,
                        );
                    }
                    self.append_audit(MutationAuditEvent {
                        tenant: &req.tenant_scope,
                        principal_chain: actor_chain,
                        operation: &req.operation,
                        args_hash_hex: &args_hash_hex,
                        decision: "ALLOW",
                        approval_id: proof_id,
                        outcome: "outcome_rejected",
                    });
                    return Err(GapError::new(
                        ErrorCode::BranchError,
                        "connector rejected mutation outcome",
                    ));
                }

                let result_json = resp
                    .result
                    .as_ref()
                    .map_or(serde_json::Value::Null, prost_struct_to_json);
                let comp_token = if resp.compensation_token.is_empty() {
                    None
                } else {
                    Some(resp.compensation_token)
                };
                if owns_reservation {
                    let stored = serde_json::to_string(&StoredReplayPayload {
                        result: result_json.clone(),
                        compensation_token: comp_token.clone(),
                    })
                    .map_err(|err| {
                        GapError::new(
                            ErrorCode::Internal,
                            format!("failed to serialize mutation replay payload: {err}"),
                        )
                    })?;
                    self.idempotency
                        .complete(
                            &req.tenant_scope,
                            &idempotency_key,
                            idemp_request_hash,
                            Some(&stored),
                        )
                        .map_err(|e| e.into_gap_error(&req.request_id))?;
                }
                self.append_audit(MutationAuditEvent {
                    tenant: &req.tenant_scope,
                    principal_chain: actor_chain,
                    operation: &req.operation,
                    args_hash_hex: &args_hash_hex,
                    decision: "ALLOW",
                    approval_id: proof_id,
                    outcome: if resp.was_replayed {
                        "replayed"
                    } else {
                        "applied"
                    },
                });
                Ok(MutationExecutionResult::Applied {
                    result: result_json,
                    idempotency_key,
                    idempotent: req.connector_supports_idempotency,
                    was_replayed: resp.was_replayed,
                    compensation_token: comp_token,
                })
            }
            Err(ngc_err) => {
                if owns_reservation {
                    let _ = self.idempotency.release_in_progress(
                        &req.tenant_scope,
                        &idempotency_key,
                        idemp_request_hash,
                    );
                }
                let mapped = GapError::from(ngc_err);
                self.append_audit(MutationAuditEvent {
                    tenant: &req.tenant_scope,
                    principal_chain: actor_chain,
                    operation: &req.operation,
                    args_hash_hex: &args_hash_hex,
                    decision: "ALLOW",
                    approval_id: proof_id,
                    outcome: "rpc_error",
                });
                Err(mapped)
            }
        }
    }

    async fn wait_for_in_progress(
        &self,
        tenant: &TenantScope,
        key: &str,
        expected_hash: [u8; 32],
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> GapResult<StoredReplayPayload> {
        let deadline = TokioInstant::now() + timeout;
        loop {
            if cancel.is_cancelled() {
                return Err(GapError::new(
                    ErrorCode::Cancelled,
                    "request cancelled while awaiting in-progress mutation",
                ));
            }
            if let Some(rec) = self
                .idempotency
                .get(tenant, key)
                .map_err(|e| e.into_gap_error("idempotency-wait"))?
            {
                if rec.request_hash != expected_hash {
                    return Err(GapError::new(
                        ErrorCode::IdempotencyConflict,
                        "idempotency key hash changed while waiting",
                    ));
                }
                if rec.status == IdempotencyStatus::Completed {
                    return decode_stored_replay(rec.result_ref.as_deref());
                }
            } else {
                return Err(GapError::new(
                    ErrorCode::BranchError,
                    "in-progress mutation aborted before completion",
                ));
            }

            if TokioInstant::now() >= deadline {
                return Err(GapError::new(
                    ErrorCode::DeadlineExceeded,
                    "timed out waiting for in-progress idempotent mutation",
                ));
            }
            sleep(Duration::from_millis(IDEMPOTENCY_POLL_INTERVAL_MS)).await;
        }
    }
}

fn decode_stored_replay(raw: Option<&str>) -> GapResult<StoredReplayPayload> {
    let Some(s) = raw else {
        return Ok(StoredReplayPayload {
            result: serde_json::Value::Null,
            compensation_token: None,
        });
    };
    serde_json::from_str(s)
        .map_err(|_| GapError::new(ErrorCode::Internal, "corrupt idempotency replay payload"))
}

fn compute_idempotency_request_hash(
    tenant: &str,
    operation: &str,
    args_hash: &[u8; 32],
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(tenant.len() + operation.len() + 34);
    buf.extend_from_slice(tenant.as_bytes());
    buf.push(0);
    buf.extend_from_slice(operation.as_bytes());
    buf.push(0);
    buf.extend_from_slice(args_hash);
    blake3_hash(&buf)
}

fn build_call_scope(req: &MutationExecutionRequest) -> CallScope {
    let deadline = TokioInstant::now() + Duration::from_millis(req.deadline_ms.max(1));
    CallScope {
        cancel: req.cancel.clone(),
        deadline,
        request_id: req.request_id.clone(),
        tenant_id: req.tenant_scope.tenant().as_str().to_owned(),
        traceparent: String::new(),
        metadata: BTreeMap::new(),
    }
}

fn build_mutate_request(
    req: &MutationExecutionRequest,
    idempotency_key: &str,
    dry_run: bool,
) -> MutateRequest {
    let ngc_kind = match req.principal.kind {
        gap_core::context::PrincipalKind::Human => NgcPrincipalKind::Human as i32,
        gap_core::context::PrincipalKind::Agent => NgcPrincipalKind::Agent as i32,
        gap_core::context::PrincipalKind::Service => NgcPrincipalKind::Service as i32,
    };
    let mut delegation_chain = Vec::with_capacity(1 + req.principal.delegation.len());
    delegation_chain.push(req.principal.id.clone());
    for actor in &req.principal.delegation {
        delegation_chain.push(actor.id.clone());
    }
    let scopes: Vec<String> = req.principal.scopes.iter().cloned().collect();

    MutateRequest {
        ctx: Some(CallContext {
            tenant_id: req.tenant_scope.tenant().as_str().to_owned(),
            request_id: req.request_id.clone(),
            traceparent: String::new(),
            deadline_unix_ms: i64::try_from(req.deadline_ms).unwrap_or(i64::MAX),
            remaining_cost_units: 0,
            principal: Some(NgcPrincipal {
                id: req.principal.id.clone(),
                kind: ngc_kind,
                delegation_chain,
                scopes,
                attributes: std::collections::HashMap::new(),
            }),
            hop_count: u32::from(req.delegators.len().min(u8::MAX as usize) as u8),
            trace_path: Vec::new(),
            idempotency_key: idempotency_key.to_owned(),
            snapshot_generation: 0,
        }),
        operation: req.operation.clone(),
        arguments: Some(json_to_prost_struct(&req.arguments)),
        selection: None,
        dry_run,
        approval: if dry_run {
            None
        } else {
            req.approval_proof.clone()
        },
    }
}

fn map_mutate_ngc_error(code: i32, message: &str, dry_run: bool) -> GapError {
    let info = NgcErrorInfo::try_from_wire(code, message, false, 0, None)
        .unwrap_or_else(|_| NgcErrorInfo::new(NgcErrorCode::Unspecified, message));
    if dry_run {
        info.into_dry_run_gap_error()
    } else {
        GapError::from(info)
    }
}

/// Converts a [`serde_json::Value`] into a [`prost_types::Struct`].
#[must_use]
pub fn json_to_prost_struct(val: &serde_json::Value) -> prost_types::Struct {
    let mut fields = BTreeMap::new();
    if let serde_json::Value::Object(map) = val {
        for (k, v) in map {
            fields.insert(k.clone(), json_to_prost_value(v));
        }
    }
    prost_types::Struct { fields }
}

fn json_to_prost_value(val: &serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match val {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(json_to_prost_value).collect(),
        }),
        serde_json::Value::Object(_) => Kind::StructValue(json_to_prost_struct(val)),
    };
    prost_types::Value { kind: Some(kind) }
}

/// Converts a [`prost_types::Struct`] into a [`serde_json::Value`].
#[must_use]
pub fn prost_struct_to_json(s: &prost_types::Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in &s.fields {
        map.insert(k.clone(), prost_value_to_json(v));
    }
    serde_json::Value::Object(map)
}

fn prost_value_to_json(val: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match &val.kind {
        Some(Kind::NullValue(_)) | None => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::StructValue(st)) => prost_struct_to_json(st),
        Some(Kind::ListValue(lv)) => {
            serde_json::Value::Array(lv.values.iter().map(prost_value_to_json).collect())
        }
    }
}
