//! Multi-branch explicit compensation saga runner (`RUN-074`, `NGC-033`, `WP-M6-04`).
//!
//! Steps execute sequentially in declared order, capturing any returned `compensation_token`. On
//! failure at step *k*, the runner invokes declared `@compensate` operations of completed steps
//! `k - 1 ..= 0` in reverse order and records a durable per-step ledger. If any compensation fails,
//! the final outcome sets `needs_manual_intervention = true` and notifies [`SagaAlertSink`].

use std::sync::Arc;

use gap_core::clock::Clock;
use gap_core::context::Principal;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{TenantId, TenantScope};
use gap_core::snapshot::{SideEffectLevel, TrustTier};
use gap_sec::authz::{AuthzContext, CompiledPolicySet, ResourceAttributes};
use gap_store::audit::{AuditChain, AuditEntry, AuditLog};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::mutate::{
    MutationExecutionRequest, MutationExecutionResult, MutationPipeline, MutationTarget,
};

/// Receiver notified whenever reverse compensation fails and manual operator review is needed (`RUN-074`).
pub trait SagaAlertSink: Send + Sync {
    /// Emits a structured alert describing a compensation failure.
    fn alert_manual_intervention(&self, tenant: &str, saga_id: &str, step_id: &str, reason: &str);
}

/// No-op alert sink for scenarios without external paging hooks.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSagaAlertSink;

impl SagaAlertSink for NoopSagaAlertSink {
    fn alert_manual_intervention(
        &self,
        _tenant: &str,
        _saga_id: &str,
        _step_id: &str,
        _reason: &str,
    ) {
    }
}

/// Single step in a sequential opt-in compensation saga (`RUN-074`).
#[derive(Debug, Clone)]
pub struct SagaStep {
    /// Stable caller-assigned step identifier.
    pub step_id: String,
    /// Target branch slug.
    pub target_branch: String,
    /// Forward mutation operation (`<branch>.<name>`).
    pub operation: String,
    /// JSON arguments for forward mutation.
    pub arguments: serde_json::Value,
    /// Declared `@compensate` operation invoked in reverse order upon downstream failure (`RUN-074`).
    pub compensate_operation: String,
    /// Verified side-effect classification for forward step (`NONE` or `REVERSIBLE`).
    pub side_effect: SideEffectLevel,
    /// Optional explicit idempotency key for forward step.
    pub idempotency_key: Option<String>,
}

/// Forward execution status of a [`SagaStep`] in the ledger (`RUN-074`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SagaStepStatus {
    /// Forward mutation completed cleanly.
    Completed,
    /// Forward mutation failed at this step.
    Failed,
}

/// Reverse compensation verdict recorded for a previously completed step (`RUN-074`, `NGC-033`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompensationOutcome {
    /// `@compensate` succeeded with `compensation_token`.
    Compensated,
    /// `@compensate` failed with sanitized diagnostic `reason` -> triggers `needs_manual_intervention`.
    Failed {
        /// Diagnostic failure description.
        reason: String,
    },
}

/// Per-step audit entry inside [`SagaOutcome`] (`RUN-074`).
#[derive(Debug, Clone, PartialEq)]
pub struct SagaLedgerEntry {
    /// Zero-based index of the step in submission order.
    pub step_index: usize,
    /// Caller step identifier.
    pub step_id: String,
    /// Forward operation identifier.
    pub operation: String,
    /// Forward outcome.
    pub status: SagaStepStatus,
    /// Token returned by forward step (`NGC-033`) and passed to `@compensate`.
    pub compensation_token: Option<String>,
    /// Forward JSON output (when [`SagaStepStatus::Completed`]).
    pub result: Option<serde_json::Value>,
    /// Populated when rollback ran for this step.
    pub compensation: Option<CompensationOutcome>,
}

/// Final result of executing a [`SagaRunner`] plan (`RUN-074`).
#[derive(Debug, Clone, PartialEq)]
pub enum SagaOutcome {
    /// All steps succeeded without needing compensation.
    Completed {
        /// Ordered step results.
        ledger: Vec<SagaLedgerEntry>,
    },
    /// A step failed and reverse compensation ran across completed predecessors (`RUN-074`).
    Failed {
        /// Index of the step whose failure aborted the saga.
        failed_step_index: usize,
        /// Sanitized error code from the failing step.
        error_code: ErrorCode,
        /// Error message from failing forward step.
        error_message: String,
        /// Full per-step forward & reverse-compensation ledger.
        ledger: Vec<SagaLedgerEntry>,
        /// `true` iff one or more `@compensate` invocations failed (`RUN-074`).
        needs_manual_intervention: bool,
        /// `true` when an alert was dispatched via [`SagaAlertSink`].
        alert_emitted: bool,
    },
}

/// Context applied across all steps of a single saga execution (`RUN-074`).
#[derive(Clone)]
pub struct SagaRunContext {
    /// Saga execution identifier for audit & alert correlation.
    pub saga_id: String,
    /// Non-forgeable tenant scope (`INV-09`).
    pub tenant_scope: TenantScope,
    /// Tenant identifier matching `tenant_scope`.
    pub tenant: TenantId,
    /// Authenticated caller principal.
    pub principal: Principal,
    /// Delegation chain subjects (`SEC-012`).
    pub delegators: Vec<Principal>,
    /// Cedar request evaluation context.
    pub authz_context: AuthzContext,
    /// Optional auto-approve policy set.
    pub auto_approve_policies: Option<Arc<CompiledPolicySet>>,
    /// Whether forward & compensation connectors support idempotency.
    pub connector_supports_idempotency: bool,
    /// Per-step deadline in milliseconds.
    pub step_deadline_ms: u64,
    /// Cancellation token.
    pub cancel: CancellationToken,
}

/// Orchestrates opt-in sequential sagas with reverse `@compensate` rollbacks (`RUN-074`, `NGC-033`).
pub struct SagaRunner {
    pipeline: Arc<MutationPipeline>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditLog>,
    alert_sink: Arc<dyn SagaAlertSink>,
}

impl std::fmt::Debug for SagaRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SagaRunner").finish()
    }
}

impl SagaRunner {
    /// Creates a [`SagaRunner`] backed by `pipeline`, durable `audit`, and `alert_sink`.
    #[must_use]
    pub fn new(
        pipeline: Arc<MutationPipeline>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditLog>,
        alert_sink: Arc<dyn SagaAlertSink>,
    ) -> Self {
        Self {
            pipeline,
            clock,
            audit,
            alert_sink,
        }
    }

    /// Executes `steps` sequentially; on failure at step *k*, runs `@compensate` of completed
    /// steps `k - 1 ..= 0` in reverse order (`RUN-074`, `NGC-033`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ValidationError`] if `steps` is empty or any step lacks a
    /// `compensate_operation`.
    #[allow(clippy::too_many_lines)]
    pub async fn run(
        &self,
        ctx: SagaRunContext,
        steps: &[SagaStep],
        target: &dyn MutationTarget,
    ) -> GapResult<SagaOutcome> {
        if steps.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "saga requires at least one step",
            ));
        }
        for step in steps {
            if step.compensate_operation.is_empty() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "saga step `{}` requires @compensate operation",
                        step.step_id
                    ),
                ));
            }
        }

        let mut ledger: Vec<SagaLedgerEntry> = Vec::with_capacity(steps.len());

        for (idx, step) in steps.iter().enumerate() {
            let resource = ResourceAttributes::operation(
                &step.operation,
                ctx.tenant.clone(),
                &step.target_branch,
                TrustTier::T0,
                step.side_effect,
            );
            let req = MutationExecutionRequest {
                tenant_scope: ctx.tenant_scope.clone(),
                request_id: format!("{}:step:{}", ctx.saga_id, step.step_id),
                principal: ctx.principal.clone(),
                delegators: ctx.delegators.clone(),
                resource,
                authz_context: ctx.authz_context.clone(),
                target_branch: step.target_branch.clone(),
                operation: step.operation.clone(),
                arguments: step.arguments.clone(),
                side_effect: step.side_effect,
                compensate_operation: Some(step.compensate_operation.clone()),
                connector_supports_idempotency: ctx.connector_supports_idempotency,
                connector_supports_dry_run: false,
                dry_run: false,
                four_eyes: false,
                idempotency_key: Some(
                    step.idempotency_key
                        .clone()
                        .unwrap_or_else(|| format!("{}:{}", ctx.saga_id, step.step_id)),
                ),
                approval_proof: None,
                auto_approve_policies: ctx.auto_approve_policies.clone(),
                deadline_ms: ctx.step_deadline_ms,
                cancel: ctx.cancel.clone(),
            };

            match self.pipeline.execute(req, target).await {
                Ok(MutationExecutionResult::Applied {
                    result,
                    compensation_token,
                    ..
                }) => {
                    ledger.push(SagaLedgerEntry {
                        step_index: idx,
                        step_id: step.step_id.clone(),
                        operation: step.operation.clone(),
                        status: SagaStepStatus::Completed,
                        compensation_token,
                        result: Some(result),
                        compensation: None,
                    });
                }
                Ok(MutationExecutionResult::ApprovalRequired { .. }) => {
                    ledger.push(SagaLedgerEntry {
                        step_index: idx,
                        step_id: step.step_id.clone(),
                        operation: step.operation.clone(),
                        status: SagaStepStatus::Failed,
                        compensation_token: None,
                        result: None,
                        compensation: None,
                    });
                    return Ok(self
                        .compensate_completed(
                            &ctx,
                            steps,
                            ledger,
                            idx,
                            ErrorCode::ApprovalRequired,
                            "saga step cannot pause unapproved".to_owned(),
                            target,
                        )
                        .await);
                }
                Ok(MutationExecutionResult::DryRun { .. }) => {
                    ledger.push(SagaLedgerEntry {
                        step_index: idx,
                        step_id: step.step_id.clone(),
                        operation: step.operation.clone(),
                        status: SagaStepStatus::Failed,
                        compensation_token: None,
                        result: None,
                        compensation: None,
                    });
                    return Ok(self
                        .compensate_completed(
                            &ctx,
                            steps,
                            ledger,
                            idx,
                            ErrorCode::ValidationError,
                            "unexpected dry-run result during saga".to_owned(),
                            target,
                        )
                        .await);
                }
                Err(err) => {
                    ledger.push(SagaLedgerEntry {
                        step_index: idx,
                        step_id: step.step_id.clone(),
                        operation: step.operation.clone(),
                        status: SagaStepStatus::Failed,
                        compensation_token: None,
                        result: None,
                        compensation: None,
                    });
                    return Ok(self
                        .compensate_completed(
                            &ctx,
                            steps,
                            ledger,
                            idx,
                            err.code(),
                            err.message().to_owned(),
                            target,
                        )
                        .await);
                }
            }
        }

        Ok(SagaOutcome::Completed { ledger })
    }

    #[allow(clippy::too_many_arguments)]
    async fn compensate_completed(
        &self,
        ctx: &SagaRunContext,
        steps: &[SagaStep],
        mut ledger: Vec<SagaLedgerEntry>,
        failed_step_index: usize,
        error_code: ErrorCode,
        error_message: String,
        target: &dyn MutationTarget,
    ) -> SagaOutcome {
        let mut needs_manual_intervention = false;
        let mut alert_emitted = false;

        // Roll back completed steps strictly in reverse order (`k - 1 ..= 0`).
        for rev_idx in (0..failed_step_index).rev() {
            let step = &steps[rev_idx];
            let token = ledger[rev_idx].compensation_token.clone();
            let comp_args = serde_json::json!({
                "compensationToken": token,
                "originalArguments": step.arguments,
                "sagaId": ctx.saga_id,
                "stepId": step.step_id,
            });
            let comp_res = ResourceAttributes::operation(
                &step.compensate_operation,
                ctx.tenant.clone(),
                &step.target_branch,
                TrustTier::T0,
                SideEffectLevel::None,
            );
            let comp_req = MutationExecutionRequest {
                tenant_scope: ctx.tenant_scope.clone(),
                request_id: format!("{}:comp:{}", ctx.saga_id, step.step_id),
                principal: ctx.principal.clone(),
                delegators: ctx.delegators.clone(),
                resource: comp_res,
                authz_context: ctx.authz_context.clone(),
                target_branch: step.target_branch.clone(),
                operation: step.compensate_operation.clone(),
                arguments: comp_args,
                side_effect: SideEffectLevel::None,
                compensate_operation: None,
                connector_supports_idempotency: ctx.connector_supports_idempotency,
                connector_supports_dry_run: false,
                dry_run: false,
                four_eyes: false,
                idempotency_key: Some(format!("{}:comp:{}", ctx.saga_id, step.step_id)),
                approval_proof: None,
                auto_approve_policies: None,
                deadline_ms: ctx.step_deadline_ms,
                // Run compensating calls with an active token even if forward workflow cancelled.
                cancel: CancellationToken::new(),
            };

            match self.pipeline.execute(comp_req, target).await {
                Ok(_) => {
                    ledger[rev_idx].compensation = Some(CompensationOutcome::Compensated);
                }
                Err(comp_err) => {
                    let reason = format!("{}: {}", comp_err.code().as_str(), comp_err.message());
                    needs_manual_intervention = true;
                    alert_emitted = true;
                    self.alert_sink.alert_manual_intervention(
                        ctx.tenant.as_str(),
                        &ctx.saga_id,
                        &step.step_id,
                        &reason,
                    );
                    let _ = self.audit.append_chain(
                        &AuditChain::Tenant(ctx.tenant_scope.clone()),
                        &AuditEntry {
                            ts: self.clock.now_timestamp(),
                            actor_chain: vec![ctx.principal.id.clone()],
                            action: step.compensate_operation.clone(),
                            resource: step.operation.clone(),
                            decision: "COMPENSATION_FAILED".to_owned(),
                            details_json: serde_json::json!({
                                "saga_id": ctx.saga_id,
                                "step_id": step.step_id,
                                "needs_manual_intervention": true,
                                "reason": reason,
                            })
                            .to_string(),
                        },
                    );
                    ledger[rev_idx].compensation = Some(CompensationOutcome::Failed { reason });
                }
            }
        }

        SagaOutcome::Failed {
            failed_step_index,
            error_code,
            error_message,
            ledger,
            needs_manual_intervention,
            alert_emitted,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use gap_core::clock::{Clock, ManualClock};
    use gap_core::context::{AttrValue, Principal, PrincipalKind};
    use gap_core::error::NgcErrorInfo;
    use gap_core::ids::TenantId;
    use gap_core::snapshot::SideEffectLevel;
    use gap_ngc_sdk::connector::{CallScope, NgcResult};
    use gap_proto::ngc::v1::{MutateRequest, MutateResponse, MutationOutcome};
    use gap_sec::authz::{AuthzContext, CompiledPolicySet};
    use gap_store::StoreError;
    use gap_store::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
    use gap_store::idempotency::InMemoryIdempotencyStore;
    use tokio_util::sync::CancellationToken;

    use crate::mutate::{
        ApprovalReplayGuard, MutationPipeline, MutationPipelineConfig, MutationTarget,
        json_to_prost_struct, prost_struct_to_json,
    };

    use super::{
        CompensationOutcome, SagaAlertSink, SagaOutcome, SagaRunContext, SagaRunner, SagaStep,
        SagaStepStatus,
    };

    #[derive(Default)]
    struct DummyAudit;
    impl AuditLog for DummyAudit {
        fn append_chain(
            &self,
            _chain: &AuditChain,
            entry: &AuditEntry,
        ) -> Result<AuditRecord, StoreError> {
            Ok(AuditRecord {
                seq: 1,
                entry: entry.clone(),
                prev_hash: [0_u8; 32],
                hash: [1_u8; 32],
            })
        }
        fn chain_head(&self, _chain: &AuditChain) -> Result<Option<(u64, [u8; 32])>, StoreError> {
            Ok(None)
        }
        fn verify_chain(
            &self,
            _chain: &AuditChain,
            _last_n: Option<u64>,
        ) -> Result<AuditVerifyReport, StoreError> {
            Ok(AuditVerifyReport::Intact {
                head_seq: 0,
                head_hash: [0_u8; 32],
            })
        }
    }

    #[derive(Default)]
    struct RecordingAlerts(crate::lockfree::LockFreeLog<(String, String, String)>);
    impl SagaAlertSink for RecordingAlerts {
        fn alert_manual_intervention(&self, tenant: &str, saga_id: &str, step_id: &str, _r: &str) {
            self.0
                .push((tenant.to_owned(), saga_id.to_owned(), step_id.to_owned()));
        }
    }

    #[derive(Default)]
    struct SagaTestTarget {
        fail_op: Option<String>,
        call_log: crate::lockfree::LockFreeLog<(String, Option<String>)>,
    }

    impl MutationTarget for SagaTestTarget {
        fn mutate(
            &self,
            request: MutateRequest,
            _scope: CallScope,
        ) -> Pin<Box<dyn Future<Output = NgcResult<MutateResponse>> + Send + '_>> {
            Box::pin(async move {
                let args = request
                    .arguments
                    .as_ref()
                    .map_or(serde_json::Value::Null, prost_struct_to_json);
                let comp_tok_arg = args
                    .get("compensationToken")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned);
                self.call_log
                    .push((request.operation.clone(), comp_tok_arg));

                if self.fail_op.as_deref() == Some(request.operation.as_str()) {
                    return Err(NgcErrorInfo::new(
                        gap_core::error::NgcErrorCode::Internal,
                        format!("simulated failure in {}", request.operation),
                    ));
                }
                let tok = format!("tok-{}", request.operation);
                Ok(MutateResponse {
                    result: Some(json_to_prost_struct(&serde_json::json!({"ok": true}))),
                    outcome: MutationOutcome::OutcomeApplied as i32,
                    was_replayed: false,
                    compensation_token: tok,
                    error: None,
                    dry_run_report: None,
                })
            })
        }
    }

    fn make_runner(alerts: Arc<RecordingAlerts>) -> (SagaRunner, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let idemp = Arc::new(InMemoryIdempotencyStore::with_clock(
            Arc::clone(&clock) as Arc<dyn Clock>
        ));
        let audit = Arc::new(DummyAudit);
        let policy = Arc::new(CompiledPolicySet::default_builtin().expect("policy"));
        let pipe = Arc::new(MutationPipeline::new(MutationPipelineConfig {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            policy,
            idempotency: idemp,
            audit: Arc::clone(&audit) as Arc<dyn AuditLog>,
            approval_gate: None,
            approval_verifying_key: None,
            approval_replay_guard: Arc::new(ApprovalReplayGuard::new()),
        }));
        let runner = SagaRunner::new(pipe, Arc::clone(&clock) as Arc<dyn Clock>, audit, alerts);
        (runner, clock)
    }

    fn sample_steps() -> Vec<SagaStep> {
        vec![
            SagaStep {
                step_id: "s1".to_owned(),
                target_branch: "inventory".to_owned(),
                operation: "inventory.reserve".to_owned(),
                arguments: serde_json::json!({"sku": "A1"}),
                compensate_operation: "inventory.release".to_owned(),
                side_effect: SideEffectLevel::Reversible,
                idempotency_key: None,
            },
            SagaStep {
                step_id: "s2".to_owned(),
                target_branch: "billing".to_owned(),
                operation: "billing.hold".to_owned(),
                arguments: serde_json::json!({"cents": 500}),
                compensate_operation: "billing.voidHold".to_owned(),
                side_effect: SideEffectLevel::Reversible,
                idempotency_key: None,
            },
            SagaStep {
                step_id: "s3".to_owned(),
                target_branch: "shipping".to_owned(),
                operation: "shipping.dispatch".to_owned(),
                arguments: serde_json::json!({"addr": "1 Main"}),
                compensate_operation: "shipping.cancel".to_owned(),
                side_effect: SideEffectLevel::Reversible,
                idempotency_key: None,
            },
        ]
    }

    fn sample_ctx(saga_id: &str) -> SagaRunContext {
        let tid = TenantId::new("acme").expect("tenant");
        let mut p = Principal::new("op-1", PrincipalKind::Human, tid.clone());
        p.attrs.insert(
            "groups".to_owned(),
            AttrValue::List(vec!["operators".to_owned()]),
        );
        SagaRunContext {
            saga_id: saga_id.to_owned(),
            tenant_scope: tid.scope(),
            tenant: tid.clone(),
            principal: p,
            delegators: Vec::new(),
            authz_context: AuthzContext::minimal(tid),
            auto_approve_policies: None,
            connector_supports_idempotency: true,
            step_deadline_ms: 1_000,
            cancel: CancellationToken::new(),
        }
    }

    // spec: RUN-074, NGC-033
    #[tokio::test]
    async fn saga_reverse_compensation_runs_with_tokens_and_flags_manual_intervention_on_failure() {
        // Case 1: Step 3 fails -> Step 2 then Step 1 compensated in strict reverse order with tokens.
        let alerts = Arc::new(RecordingAlerts::default());
        let (runner, _) = make_runner(Arc::clone(&alerts));
        let target_ok_rollback = SagaTestTarget {
            fail_op: Some("shipping.dispatch".to_owned()),
            ..Default::default()
        };
        let out = runner
            .run(sample_ctx("saga-1"), &sample_steps(), &target_ok_rollback)
            .await
            .expect("saga executes cleanly");
        let SagaOutcome::Failed {
            failed_step_index,
            ledger,
            needs_manual_intervention,
            alert_emitted,
            ..
        } = out
        else {
            panic!("expected Failed outcome with compensation ledger");
        };
        assert_eq!(failed_step_index, 2);
        assert!(!needs_manual_intervention);
        assert!(!alert_emitted);
        assert_eq!(ledger[0].status, SagaStepStatus::Completed);
        assert_eq!(ledger[1].status, SagaStepStatus::Completed);
        assert_eq!(ledger[2].status, SagaStepStatus::Failed);
        assert_eq!(
            ledger[0].compensation,
            Some(CompensationOutcome::Compensated)
        );
        assert_eq!(
            ledger[1].compensation,
            Some(CompensationOutcome::Compensated)
        );

        let calls = target_ok_rollback.call_log.to_vec();
        assert_eq!(
            calls,
            vec![
                ("inventory.reserve".to_owned(), None),
                ("billing.hold".to_owned(), None),
                ("shipping.dispatch".to_owned(), None),
                (
                    "billing.voidHold".to_owned(),
                    Some("tok-billing.hold".to_owned())
                ),
                (
                    "inventory.release".to_owned(),
                    Some("tok-inventory.reserve".to_owned())
                ),
            ]
        );

        // Case 2: Compensation step itself fails -> needs_manual_intervention = true + alert dispatched.
        let target_comp_fail = SagaTestTarget {
            fail_op: Some("billing.voidHold".to_owned()),
            ..Default::default()
        };
        let mut steps_2 = sample_steps();
        steps_2[2].operation = "billing.voidHold".to_owned();
        let out2 = runner
            .run(sample_ctx("saga-2"), &steps_2, &target_comp_fail)
            .await
            .expect("saga runs");
        let SagaOutcome::Failed {
            needs_manual_intervention: manual,
            alert_emitted: alerted,
            ledger: ledger2,
            ..
        } = out2
        else {
            panic!("expected Failed outcome");
        };
        assert!(manual);
        assert!(alerted);
        assert!(matches!(
            ledger2[1].compensation,
            Some(CompensationOutcome::Failed { .. })
        ));
        assert_eq!(
            ledger2[0].compensation,
            Some(CompensationOutcome::Compensated)
        );
        assert_eq!(alerts.0.len(), 1);
    }
}
