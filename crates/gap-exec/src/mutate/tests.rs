#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_core::context::{AttrValue, Principal, PrincipalKind};
use gap_core::error::{ErrorCode, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::TenantId;
use gap_core::snapshot::SideEffectLevel;
use gap_core::snapshot::TrustTier;
use gap_ngc_sdk::connector::{CallScope, NgcResult};
use gap_proto::ngc::v1::{MutateRequest, MutateResponse, MutationOutcome, NgcError};
use gap_sec::authz::{AuthzContext, CompiledPolicySet, ResourceAttributes};
use gap_sec::crypto::{Ed25519SigningKey, blake3_hash};
use gap_store::StoreError;
use gap_store::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
use gap_store::idempotency::{IdempotencyStore, InMemoryIdempotencyStore};
use gap_store::tasks::{DEFAULT_APPROVAL_TTL_MS, InMemoryTaskStore, TaskStatus, TaskStore};
use tokio_util::sync::CancellationToken;

use super::{
    ApprovalDecisionOutcome, ApprovalGate, ApprovalReplayGuard, MutationExecutionRequest,
    MutationExecutionResult, MutationPipeline, MutationPipelineConfig, MutationTarget,
    canonical_args_hash, deterministic_approval_summary, json_to_prost_struct,
    prost_struct_to_json, sign_approval_proof, verify_approval_proof,
};

#[derive(Default)]
struct InMemoryAuditRecorder {
    records: crate::lockfree::LockFreeLog<(String, AuditRecord)>,
}

impl InMemoryAuditRecorder {
    fn snapshot(&self) -> Vec<(String, AuditRecord)> {
        self.records.to_vec()
    }
}

impl AuditLog for InMemoryAuditRecorder {
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError> {
        let chain_name = match chain {
            AuditChain::Tenant(t) => t.tenant().as_str().to_owned(),
            AuditChain::System => "__system__".to_owned(),
        };
        let existing = self.records.to_vec();
        let seq = u64::try_from(existing.iter().filter(|(c, _)| c == &chain_name).count() + 1)
            .unwrap_or(u64::MAX);
        let prev_hash = existing
            .iter()
            .rev()
            .find(|(c, _)| c == &chain_name)
            .map_or([0_u8; 32], |(_, r)| r.hash);
        let hash = blake3_hash(format!("{seq}:{}", entry.decision).as_bytes());
        let rec = AuditRecord {
            seq,
            entry: entry.clone(),
            prev_hash,
            hash,
        };
        self.records.push((chain_name, rec.clone()));
        Ok(rec)
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
struct ScriptedMutateTarget {
    calls: AtomicUsize,
    last_requests: crate::lockfree::LockFreeLog<MutateRequest>,
    unimplemented_dry_run: bool,
    reject_outcome: bool,
    delay_ms: u64,
    compensation_token: String,
    required_vk: OnceLock<gap_sec::crypto::Ed25519VerifyingKey>,
}

impl ScriptedMutateTarget {
    fn seen_requests(&self) -> Vec<MutateRequest> {
        self.last_requests.to_vec()
    }
}

impl MutationTarget for ScriptedMutateTarget {
    fn mutate(
        &self,
        request: MutateRequest,
        _scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<MutateResponse>> + Send + '_>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_requests.push(request.clone());
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            if request.dry_run && self.unimplemented_dry_run {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::Unimplemented,
                    "dryRun unsupported by connector",
                ));
            }
            if let Some(vk) = self.required_vk.get() {
                let Some(ref proof) = request.approval else {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::PermissionDenied,
                        "missing ApprovalProof",
                    ));
                };
                let args = request
                    .arguments
                    .as_ref()
                    .map_or(serde_json::Value::Null, prost_struct_to_json);
                let hash = canonical_args_hash(&request.operation, &args);
                let ikey = request
                    .ctx
                    .as_ref()
                    .map(|c| c.idempotency_key.as_str())
                    .unwrap_or("");
                verify_approval_proof(vk, proof, &request.operation, hash, ikey, None).map_err(
                    |_| NgcErrorInfo::new(NgcErrorCode::PermissionDenied, "invalid ApprovalProof"),
                )?;
            }
            if request.dry_run {
                return Ok(MutateResponse {
                    result: None,
                    outcome: MutationOutcome::OutcomeDryRun as i32,
                    was_replayed: false,
                    compensation_token: String::new(),
                    error: None,
                    dry_run_report: Some(json_to_prost_struct(
                        &serde_json::json!({"validated": true, "operation": request.operation}),
                    )),
                });
            }
            if self.reject_outcome {
                return Ok(MutateResponse {
                    result: None,
                    outcome: MutationOutcome::OutcomeRejected as i32,
                    was_replayed: false,
                    compensation_token: String::new(),
                    error: Some(NgcError {
                        code: 6,
                        message: "business rule failure".to_owned(),
                        retryable: false,
                        retry_after_ms: 0,
                        details: None,
                    }),
                    dry_run_report: None,
                });
            }
            let args_json = request
                .arguments
                .as_ref()
                .map_or(serde_json::Value::Null, prost_struct_to_json);
            Ok(MutateResponse {
                result: Some(json_to_prost_struct(&serde_json::json!({
                    "applied": request.operation,
                    "args": args_json,
                }))),
                outcome: MutationOutcome::OutcomeApplied as i32,
                was_replayed: false,
                compensation_token: self.compensation_token.clone(),
                error: None,
                dry_run_report: None,
            })
        })
    }
}

fn tenant(slug: &str) -> TenantId {
    TenantId::new(slug).expect("tenant")
}

fn operator_principal(id: &str, tid: &TenantId) -> Principal {
    let mut p = Principal::new(id, PrincipalKind::Human, tid.clone());
    p.attrs.insert(
        "groups".to_owned(),
        AttrValue::List(vec!["operators".to_owned()]),
    );
    p
}

fn admin_principal(id: &str, tid: &TenantId) -> Principal {
    let mut p = Principal::new(id, PrincipalKind::Human, tid.clone());
    p.attrs.insert(
        "groups".to_owned(),
        AttrValue::List(vec!["admins".to_owned()]),
    );
    p
}

fn test_signing_key() -> Ed25519SigningKey {
    Ed25519SigningKey::from_seed([0x42_u8; 32]).expect("signing key")
}

fn make_pipeline(
    clock: Arc<dyn Clock>,
    idempotency: Arc<dyn IdempotencyStore>,
    audit: Arc<dyn AuditLog>,
    task_store: Arc<dyn TaskStore>,
    ttl_ms: Option<i64>,
) -> (MutationPipeline, Arc<ApprovalGate>) {
    let sk = test_signing_key();
    let vk = sk.verifying_key().expect("vk");
    let mut gate = ApprovalGate::new(task_store, Arc::clone(&clock), sk);
    if let Some(custom_ttl) = ttl_ms {
        gate = gate.with_ttl_ms(custom_ttl);
    }
    let gate = Arc::new(gate);
    let policy = Arc::new(CompiledPolicySet::default_builtin().expect("builtin policy"));
    let pipe = MutationPipeline::new(MutationPipelineConfig {
        clock,
        policy,
        idempotency,
        audit,
        approval_gate: Some(Arc::clone(&gate)),
        approval_verifying_key: Some(vk),
        approval_replay_guard: Arc::new(ApprovalReplayGuard::new()),
    });
    (pipe, gate)
}

fn base_req(
    tid: &TenantId,
    principal: Principal,
    operation: &str,
    side_effect: SideEffectLevel,
) -> MutationExecutionRequest {
    MutationExecutionRequest {
        tenant_scope: tid.scope(),
        request_id: "req-mut-1".to_owned(),
        principal,
        delegators: Vec::new(),
        resource: ResourceAttributes::operation(
            operation,
            tid.clone(),
            "commerce",
            TrustTier::T0,
            side_effect,
        ),
        authz_context: AuthzContext::minimal(tid.clone()),
        target_branch: "commerce".to_owned(),
        operation: operation.to_owned(),
        arguments: serde_json::json!({"orderId": "ord-1", "apiKey": "SECRET-TOKEN-999"}),
        side_effect,
        compensate_operation: None,
        connector_supports_idempotency: true,
        connector_supports_dry_run: true,
        dry_run: false,
        four_eyes: true,
        idempotency_key: Some("idem-1".to_owned()),
        approval_proof: None,
        auto_approve_policies: None,
        deadline_ms: 1_000,
        cancel: CancellationToken::new(),
    }
}

// spec: RUN-070, RUN-075, INV-11
#[tokio::test]
async fn mutate_side_effect_policy_and_authz_record_audit_on_denial() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let idemp = Arc::new(InMemoryIdempotencyStore::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    let audit = Arc::new(InMemoryAuditRecorder::default());
    let tasks = Arc::new(InMemoryTaskStore::new());
    let (pipe, _) = make_pipeline(
        Arc::clone(&clock) as Arc<dyn Clock>,
        idemp,
        Arc::clone(&audit) as Arc<dyn AuditLog>,
        tasks,
        None,
    );
    let target = ScriptedMutateTarget::default();
    let acme = tenant("acme");

    // 1. Read-only agent denied by Cedar => GAP_FORBIDDEN + audit recorded.
    let readonly = Principal::new("ro-1", PrincipalKind::Agent, acme.clone());
    let req_ro = base_req(&acme, readonly, "commerce.ping", SideEffectLevel::None);
    let err = pipe.execute(req_ro, &target).await.expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(target.calls.load(Ordering::SeqCst), 0);

    // 2. REVERSIBLE without declared @compensate fails validation + audit recorded.
    let op = operator_principal("op-1", &acme);
    let req_rev_missing = base_req(
        &acme,
        op.clone(),
        "commerce.updateOrder",
        SideEffectLevel::Reversible,
    );
    let err = pipe
        .execute(req_rev_missing, &target)
        .await
        .expect_err("missing compensate");
    assert_eq!(err.code(), ErrorCode::ValidationError);

    // 3. REVERSIBLE with @compensate succeeds.
    let mut req_rev_ok = base_req(
        &acme,
        op,
        "commerce.updateOrder",
        SideEffectLevel::Reversible,
    );
    req_rev_ok.compensate_operation = Some("commerce.undoOrder".to_owned());
    let applied = pipe.execute(req_rev_ok, &target).await.expect("ok");
    assert!(matches!(applied, MutationExecutionResult::Applied { .. }));
    assert_eq!(audit.snapshot().len(), 3);
}

// spec: RUN-071, RUN-085, NGC-032
#[tokio::test]
async fn mutate_approval_gate_four_eyes_expiry_and_signed_proof_verification() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let idemp = Arc::new(InMemoryIdempotencyStore::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    let audit = Arc::new(InMemoryAuditRecorder::default());
    let tasks = Arc::new(InMemoryTaskStore::new());
    let (pipe, gate) = make_pipeline(
        Arc::clone(&clock) as Arc<dyn Clock>,
        idemp,
        Arc::clone(&audit) as Arc<dyn AuditLog>,
        Arc::clone(&tasks) as Arc<dyn TaskStore>,
        None,
    );

    let acme = tenant("acme");
    let admin = admin_principal("requester-1", &acme);
    let target = ScriptedMutateTarget {
        compensation_token: "comp-tok-77".to_owned(),
        ..Default::default()
    };
    target
        .required_vk
        .set(gate.verifying_key().expect("vk"))
        .expect("set vk");

    // 1. IRREVERSIBLE mutation without proof stops at INPUT_REQUIRED with deterministic redacted summary.
    let mut req = base_req(
        &acme,
        admin.clone(),
        "commerce.refundOrder",
        SideEffectLevel::Irreversible,
    );
    req.idempotency_key = Some("idem-refund-1".to_owned());
    let res = pipe.execute(req.clone(), &target).await.expect("pending");
    let MutationExecutionResult::ApprovalRequired { pending } = res else {
        panic!("expected ApprovalRequired");
    };
    assert!(pending.summary.contains("commerce.refundOrder"));
    assert!(pending.summary.contains("[REDACTED]"));
    assert!(!pending.summary.contains("SECRET-TOKEN-999"));
    assert_eq!(target.calls.load(Ordering::SeqCst), 0);

    // Verify exact deterministic summary helper output.
    let summary_again = deterministic_approval_summary(
        "commerce.refundOrder",
        "commerce",
        SideEffectLevel::Irreversible,
        &req.arguments,
    );
    assert_eq!(pending.summary, summary_again);

    // 2. Four-eyes blocks requester from self-approving.
    let self_err = gate
        .decide_approval(&acme.scope(), pending.task_id, "requester-1", true, None)
        .expect_err("self-approval blocked");
    assert_eq!(self_err.code(), ErrorCode::Forbidden);

    // 3. Different reviewer approves => returns Ed25519 signed ApprovalProof bound to (operation, args_hash, idempotency_key).
    let outcome = gate
        .decide_approval(&acme.scope(), pending.task_id, "approver-2", true, None)
        .expect("approved");
    let ApprovalDecisionOutcome::Approved { proof, .. } = outcome else {
        panic!("expected Approved");
    };

    // 4. Forged / tampered args, operation, or idempotency_key rejects ApprovalProof (NGC-032).
    let vk = gate.verifying_key().expect("vk");
    let real_hash = canonical_args_hash(&req.operation, &req.arguments);
    let tampered_args_hash =
        canonical_args_hash(&req.operation, &serde_json::json!({"orderId": "ord-999"}));
    assert_eq!(
        verify_approval_proof(
            &vk,
            &proof,
            &req.operation,
            tampered_args_hash,
            "idem-refund-1",
            None
        )
        .expect_err("tampered args must fail")
        .code(),
        ErrorCode::Forbidden
    );
    assert_eq!(
        verify_approval_proof(
            &vk,
            &proof,
            "commerce.otherOp",
            real_hash,
            "idem-refund-1",
            None
        )
        .expect_err("tampered op must fail")
        .code(),
        ErrorCode::Forbidden
    );
    assert_eq!(
        verify_approval_proof(
            &vk,
            &proof,
            &req.operation,
            real_hash,
            "idem-different",
            None
        )
        .expect_err("tampered key must fail")
        .code(),
        ErrorCode::Forbidden
    );

    // 5. Resuming with genuine ApprovalProof + approved context succeeds against connector verifying proof.
    let mut resume_req = req.clone();
    resume_req.approval_proof = Some(proof.clone());
    resume_req.authz_context.approved = true;
    resume_req.authz_context.approver_is_different = true;
    let applied = pipe
        .execute(resume_req.clone(), &target)
        .await
        .expect("approved mutation executes");
    assert!(matches!(
        applied,
        MutationExecutionResult::Applied {
            was_replayed: false,
            compensation_token: Some(_),
            ..
        }
    ));

    // 6. Replaying same ApprovalProof on a *new* idempotency key is rejected as replay (NGC-032).
    let mut replay_attack = req.clone();
    replay_attack.idempotency_key = Some("idem-refund-2".to_owned());
    replay_attack.approval_proof = Some(proof);
    replay_attack.authz_context.approved = true;
    replay_attack.authz_context.approver_is_different = true;
    assert_eq!(
        pipe.execute(replay_attack, &target)
            .await
            .expect_err("proof bound to original idempotency key")
            .code(),
        ErrorCode::Forbidden
    );

    // 7. Approval expiry after 24 h transitions task to REJECTED(reason=approval_expired) (RUN-085).
    let pending_exp = gate
        .request_approval(
            &acme.scope(),
            "requester-1",
            "commerce",
            "commerce.refundOrder",
            SideEffectLevel::Irreversible,
            &req.arguments,
            "idem-expire-1",
            true,
        )
        .expect("request approval");
    clock.advance(Duration::from_millis(
        u64::try_from(DEFAULT_APPROVAL_TTL_MS).expect("ttl") + 1,
    ));
    let expired_outcome = gate
        .decide_approval(&acme.scope(), pending_exp.task_id, "approver-2", true, None)
        .expect("expiry transitions gracefully to Rejected");
    let ApprovalDecisionOutcome::Rejected { task, reason } = expired_outcome else {
        panic!("expected Rejected on expiry");
    };
    assert_eq!(task.status, TaskStatus::Rejected);
    assert_eq!(reason, "approval_expired");

    // Wrong signing key is also rejected as forgery (NGC-032).
    let rogue_sk = Ed25519SigningKey::from_seed([0x99_u8; 32]).expect("rogue");
    let forged = sign_approval_proof(
        &rogue_sk,
        &pending.approval_id.to_string(),
        "approver-2",
        clock.now_timestamp(),
        &req.operation,
        real_hash,
        "idem-forged",
    )
    .expect("sign forged");
    assert_eq!(
        verify_approval_proof(&vk, &forged, &req.operation, real_hash, "idem-forged", None)
            .expect_err("forged key fails")
            .code(),
        ErrorCode::Forbidden
    );
}

// spec: RUN-072, RUN-073, RUN-075, NGC-030, NGC-031, NGC-033
#[tokio::test]
async fn mutate_idempotency_concurrent_wait_conflict_non_idempotent_and_strict_dry_run() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let idemp = Arc::new(InMemoryIdempotencyStore::with_clock(
        Arc::clone(&clock) as Arc<dyn Clock>
    ));
    let audit = Arc::new(InMemoryAuditRecorder::default());
    let tasks = Arc::new(InMemoryTaskStore::new());
    let (pipe, _) = make_pipeline(
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::clone(&idemp) as Arc<dyn IdempotencyStore>,
        Arc::clone(&audit) as Arc<dyn AuditLog>,
        tasks,
        None,
    );
    let pipe = Arc::new(pipe);
    let acme = tenant("acme");
    let op = operator_principal("op-1", &acme);

    // 1. Concurrent duplicate idempotency key: second caller waits while first executes, both receive Applied without double-executing connector.
    let target = Arc::new(ScriptedMutateTarget {
        delay_ms: 40,
        compensation_token: "comp-1".to_owned(),
        ..Default::default()
    });
    let mut r1 = base_req(&acme, op.clone(), "commerce.charge", SideEffectLevel::None);
    r1.idempotency_key = Some("idem-dup-concurrent".to_owned());
    let r2 = r1.clone();

    let p1 = Arc::clone(&pipe);
    let p2 = Arc::clone(&pipe);
    let t1 = Arc::clone(&target);
    let t2 = Arc::clone(&target);
    let (res1, res2) = tokio::join!(
        async move { p1.execute(r1, t1.as_ref()).await },
        async move { p2.execute(r2, t2.as_ref()).await },
    );
    let out1 = res1.expect("first caller ok");
    let out2 = res2.expect("second caller ok");
    assert_eq!(target.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        (&out1, &out2),
        (
            MutationExecutionResult::Applied {
                was_replayed: false,
                ..
            },
            MutationExecutionResult::Applied {
                was_replayed: true,
                ..
            }
        ) | (
            MutationExecutionResult::Applied {
                was_replayed: true,
                ..
            },
            MutationExecutionResult::Applied {
                was_replayed: false,
                ..
            }
        )
    ));

    // 2. Same idempotency key with different arguments => GAP_IDEMPOTENCY_CONFLICT.
    let mut r_conflict = base_req(&acme, op.clone(), "commerce.charge", SideEffectLevel::None);
    r_conflict.idempotency_key = Some("idem-dup-concurrent".to_owned());
    r_conflict.arguments = serde_json::json!({"orderId": "ord-DIFFERENT"});
    assert_eq!(
        pipe.execute(r_conflict, target.as_ref())
            .await
            .expect_err("conflict")
            .code(),
        ErrorCode::IdempotencyConflict
    );

    // 3. Connector without capabilities.idempotency marks result `idempotent = false` and never replays via IdempotencyStore (NGC-030).
    let mut r_non_idemp = base_req(
        &acme,
        op.clone(),
        "commerce.legacyOp",
        SideEffectLevel::None,
    );
    r_non_idemp.idempotency_key = Some("idem-non-idemp".to_owned());
    r_non_idemp.connector_supports_idempotency = false;
    let out_non = pipe
        .execute(r_non_idemp, target.as_ref())
        .await
        .expect("non-idempotent executes once");
    assert!(matches!(
        out_non,
        MutationExecutionResult::Applied {
            idempotent: false,
            was_replayed: false,
            ..
        }
    ));

    // 4. Dry-run executes dryRun=true when supported, and fails GAP_DRY_RUN_UNSUPPORTED when unsupported (never simulated) (RUN-073, NGC-031).
    let mut r_dry = base_req(&acme, op.clone(), "commerce.charge", SideEffectLevel::None);
    r_dry.dry_run = true;
    let dry_res = pipe
        .execute(r_dry.clone(), target.as_ref())
        .await
        .expect("dry run");
    assert!(matches!(dry_res, MutationExecutionResult::DryRun { .. }));
    assert!(target.seen_requests().last().expect("last").dry_run);

    let unsupported_target = ScriptedMutateTarget {
        unimplemented_dry_run: true,
        ..Default::default()
    };
    assert_eq!(
        pipe.execute(r_dry.clone(), &unsupported_target)
            .await
            .expect_err("unimplemented must map to GAP_DRY_RUN_UNSUPPORTED")
            .code(),
        ErrorCode::DryRunUnsupported
    );

    r_dry.connector_supports_dry_run = false;
    assert_eq!(
        pipe.execute(r_dry, target.as_ref())
            .await
            .expect_err("capability false must fail GAP_DRY_RUN_UNSUPPORTED")
            .code(),
        ErrorCode::DryRunUnsupported
    );
}
