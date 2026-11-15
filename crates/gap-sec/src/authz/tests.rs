#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use gap_core::context::{ActorRef, AttrValue, Principal, PrincipalKind};
use gap_core::error::ErrorCode;
use gap_core::ids::TenantId;
use gap_core::snapshot::{BranchStatus, OpKind, SideEffectLevel, TrustTier};

use super::engine::{
    AuthzContext, CompiledPolicySet, DecisionKind, PolicyStore, ResourceAttributes,
};
use super::layer::{
    FieldClassification, FieldDecisionOutcome, GENERIC_FORBIDDEN_MESSAGE, InMemoryDecisionLog,
    QueryDenyPolicy, TouchedFieldRef, TouchedPlanSurface, authorize_plan_surface,
    should_log_decision, visibility_bitset,
};
use super::schema::{AuthzAction, DEFAULT_CEDAR_POLICIES, DEFAULT_CEDAR_SCHEMA, ResourceKind};

const AUTO_APPROVE_POLICY: &str = include_str!("../../../../policies/auto-approve.cedar");

fn tenant(name: &str) -> TenantId {
    TenantId::new(name).expect("valid tenant id")
}

fn principal_with_groups(
    id: &str,
    kind: PrincipalKind,
    tenant: TenantId,
    groups: &[&str],
) -> Principal {
    let mut p = Principal::new(id, kind, tenant);
    p.attrs.insert(
        "groups".to_owned(),
        AttrValue::List(groups.iter().map(|g| (*g).to_owned()).collect()),
    );
    p
}

// spec: SEC-020, SEC-021, SEC-022, INV-04
#[test]
fn inv_04_authz_default_deny_and_forbid_overrides_permit() {
    let compiled = CompiledPolicySet::compile(
        DEFAULT_CEDAR_SCHEMA,
        &[DEFAULT_CEDAR_POLICIES, AUTO_APPROVE_POLICY],
    )
    .expect("built-in policies should compile");

    let acme = tenant("acme");
    let ctx = AuthzContext::minimal(acme.clone());

    // Unaffiliated principal (no matching permit) => default deny.
    let anon = Principal::new("anon-1", PrincipalKind::Agent, acme.clone());
    let order_read = ResourceAttributes::operation(
        "orders.get",
        acme.clone(),
        "commerce",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    let deny_anon = compiled.evaluate_single(&anon, AuthzAction::Execute, &order_read, &ctx);
    assert_eq!(deny_anon.decision, DecisionKind::Deny);
    assert!(deny_anon.determining_policies.is_empty());

    // Read-only agent allowed on NONE / non-T2 / ACTIVE same-tenant resource.
    let reader = principal_with_groups(
        "agent-reader",
        PrincipalKind::Agent,
        acme.clone(),
        &["agents-readonly"],
    );
    let allow_read = compiled.evaluate_single(&reader, AuthzAction::Execute, &order_read, &ctx);
    assert_eq!(allow_read.decision, DecisionKind::Allow);
    assert!(
        allow_read
            .determining_policies
            .iter()
            .any(|id| id == "permit-agents-readonly")
    );

    // Operator cannot mutate T2 untrusted operation even though operators permits non-T2.
    let operator =
        principal_with_groups("op-1", PrincipalKind::Human, acme.clone(), &["operators"]);
    let t2_mutate = ResourceAttributes::operation(
        "untrusted.mutate",
        acme.clone(),
        "ext",
        TrustTier::T2,
        SideEffectLevel::Reversible,
    );
    let deny_t2 = compiled.evaluate_single(&operator, AuthzAction::Mutate, &t2_mutate, &ctx);
    assert_eq!(deny_t2.decision, DecisionKind::Deny);
    assert!(
        deny_t2
            .determining_policies
            .iter()
            .any(|id| id == "forbid-mutate-untrusted-t2")
    );

    // Irreversible mutation is denied without four-eyes approval even for admin, then allowed once approved by another user.
    let admin = principal_with_groups("admin-1", PrincipalKind::Human, acme.clone(), &["admins"]);
    let irrev = ResourceAttributes::operation(
        "orders.refund",
        acme.clone(),
        "commerce",
        TrustTier::T0,
        SideEffectLevel::Irreversible,
    );
    let deny_unapproved = compiled.evaluate_single(&admin, AuthzAction::Mutate, &irrev, &ctx);
    assert_eq!(deny_unapproved.decision, DecisionKind::Deny);
    assert!(
        deny_unapproved
            .determining_policies
            .iter()
            .any(|id| id == "forbid-mutate-irreversible-or-unknown-unapproved")
    );

    let mut approved_ctx = ctx.clone();
    approved_ctx.approved = true;
    approved_ctx.approver_is_different = true;
    let allow_approved =
        compiled.evaluate_single(&admin, AuthzAction::Mutate, &irrev, &approved_ctx);
    assert_eq!(allow_approved.decision, DecisionKind::Allow);

    // Cross-tenant access forbidden unless admin:impersonate is held.
    let beta = tenant("beta");
    let beta_res = ResourceAttributes::operation(
        "beta.read",
        beta,
        "billing",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    let cross_denied = compiled.evaluate_single(&admin, AuthzAction::Execute, &beta_res, &ctx);
    assert_eq!(cross_denied.decision, DecisionKind::Deny);
    assert!(
        cross_denied
            .determining_policies
            .iter()
            .any(|id| id == "forbid-cross-tenant-without-impersonate")
    );

    let mut super_admin = admin.clone();
    super_admin.scopes.insert("admin:impersonate".to_owned());
    let cross_allowed =
        compiled.evaluate_single(&super_admin, AuthzAction::Execute, &beta_res, &ctx);
    assert_eq!(cross_allowed.decision, DecisionKind::Allow);

    // Rule-of-Two high risk (>= 100) forbids unapproved reversible mutation.
    let rev = ResourceAttributes::operation(
        "orders.updateNote",
        acme,
        "commerce",
        TrustTier::T0,
        SideEffectLevel::Reversible,
    );
    let mut high_risk_ctx = ctx;
    high_risk_ctx.risk_score = 100;
    let deny_high_risk =
        compiled.evaluate_single(&operator, AuthzAction::Mutate, &rev, &high_risk_ctx);
    assert_eq!(deny_high_risk.decision, DecisionKind::Deny);
    assert!(
        deny_high_risk
            .determining_policies
            .iter()
            .any(|id| id == "forbid-mutate-rule-of-two-high-risk")
    );
}

// spec: SEC-020
// TM-22
#[test]
fn policy_store_atomic_reload_failure_retains_previous_and_version_invalidates_plan() {
    let store = PolicyStore::default_builtin().expect("default policy store");
    let initial = store.pin();
    assert_eq!(initial.policy_version_hex().len(), 64);

    // Invalid policy text fails reload and keeps the previously pinned CompiledPolicySet active.
    let err = store
        .reload(DEFAULT_CEDAR_SCHEMA, &["permit ( invalid syntax );"])
        .expect_err("invalid policy syntax must fail reload");
    assert_eq!(err.code(), ErrorCode::ValidationError);
    let still_active = store.pin();
    assert_eq!(
        still_active.policy_version_hex(),
        initial.policy_version_hex()
    );
    assert_eq!(
        still_active.policy_version_u64(),
        initial.policy_version_u64()
    );

    // Valid policy reload atomically swaps and bumps policy_version, flagging stale cached plans.
    let updated = store
        .reload(
            DEFAULT_CEDAR_SCHEMA,
            &[DEFAULT_CEDAR_POLICIES, AUTO_APPROVE_POLICY],
        )
        .expect("adding auto-approve policy succeeds");
    assert_ne!(updated.policy_version_u64(), initial.policy_version_u64());
    assert_ne!(updated.policy_version_hex(), initial.policy_version_hex());

    let acme = tenant("acme");
    let reader = principal_with_groups(
        "agent-reader",
        PrincipalKind::Agent,
        acme.clone(),
        &["agents-readonly"],
    );
    let surface = TouchedPlanSurface {
        request_id: "req-stale-policy".to_owned(),
        operation_kind: OpKind::Query,
        plan_policy_version: initial.policy_version_u64(),
        fields: vec![TouchedFieldRef {
            path: "order.id".to_owned(),
            type_name: "Order".to_owned(),
            field_name: "id".to_owned(),
            nullable: false,
            joined_rhs: false,
            classification: FieldClassification::Normal,
            resource: ResourceAttributes::operation(
                "Order.id",
                acme.clone(),
                "commerce",
                TrustTier::T0,
                SideEffectLevel::None,
            ),
        }],
    };
    let report = authorize_plan_surface(
        &updated,
        &reader,
        &[],
        &surface,
        &AuthzContext::minimal(acme),
        QueryDenyPolicy::NullWithError,
        None,
    );
    assert!(!report.policy_version_matches);
    assert_eq!(report.policy_version_u64, updated.policy_version_u64());
}

// spec: SEC-012
#[test]
fn authz_delegation_intersection_blocks_privilege_elevation() {
    let compiled = CompiledPolicySet::default_builtin().expect("default policies");
    let acme = tenant("acme");
    let ctx = AuthzContext::minimal(acme.clone());

    // Operator agent delegated by a read-only human cannot mutate because intersection fails.
    let mut operator_agent = principal_with_groups(
        "agent-op",
        PrincipalKind::Agent,
        acme.clone(),
        &["operators"],
    );
    operator_agent.delegation.push(ActorRef {
        id: "human-readonly".to_owned(),
        kind: PrincipalKind::Human,
    });
    operator_agent.attrs.insert(
        "delegator:human-readonly:groups".to_owned(),
        AttrValue::List(vec!["agents-readonly".to_owned()]),
    );

    let rev_op = ResourceAttributes::operation(
        "orders.cancel",
        acme.clone(),
        "commerce",
        TrustTier::T0,
        SideEffectLevel::Reversible,
    );
    let denied = compiled.evaluate_principal(&operator_agent, AuthzAction::Mutate, &rev_op, &ctx);
    assert_eq!(denied.decision, DecisionKind::Deny);
    assert_eq!(
        denied.evaluated_actors,
        vec!["agent-op".to_owned(), "human-readonly".to_owned()]
    );

    // Explicit delegators list: operator human + operators agent => allowed on Execute & Mutate.
    let operator_human =
        principal_with_groups("human-op", PrincipalKind::Human, acme, &["operators"]);
    let allowed = compiled.evaluate_with_delegators(
        &operator_agent,
        &[operator_human],
        AuthzAction::Mutate,
        &rev_op,
        &ctx,
    );
    assert_eq!(allowed.decision, DecisionKind::Allow);
    assert_eq!(
        allowed.evaluated_actors,
        vec!["agent-op".to_owned(), "human-op".to_owned()]
    );
}

// spec: SEC-021, SEC-023
// TM-21
#[test]
fn authz_layer_joined_rhs_denial_nulls_without_leaking_policy_ids_and_redacts_pii() {
    let compiled = CompiledPolicySet::default_builtin().expect("default policies");
    let acme = tenant("acme");
    let mut reader = principal_with_groups(
        "agent-reader",
        PrincipalKind::Agent,
        acme.clone(),
        &["agents-readonly"],
    );
    reader.scopes = BTreeSet::from(["read:orders".to_owned()]);

    // LHS order total is T0 ACTIVE; joined RHS customer email is T0 PII; joined RHS secret note is quarantined/denied.
    let lhs_resource = ResourceAttributes::operation(
        "Order.totalCents",
        acme.clone(),
        "commerce",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    let rhs_pii = ResourceAttributes::operation(
        "Customer.email",
        acme.clone(),
        "crm",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    let mut rhs_quarantined = ResourceAttributes::operation(
        "AuditNote.secret",
        acme.clone(),
        "internal-audit",
        TrustTier::T1,
        SideEffectLevel::None,
    );
    rhs_quarantined.status = BranchStatus::Quarantined;

    let surface = TouchedPlanSurface {
        request_id: "req-join-tm21".to_owned(),
        operation_kind: OpKind::Query,
        plan_policy_version: compiled.policy_version_u64(),
        fields: vec![
            TouchedFieldRef {
                path: "orders.totalCents".to_owned(),
                type_name: "Order".to_owned(),
                field_name: "totalCents".to_owned(),
                nullable: false,
                joined_rhs: false,
                classification: FieldClassification::Normal,
                resource: lhs_resource,
            },
            TouchedFieldRef {
                path: "orders.customer.email".to_owned(),
                type_name: "Customer".to_owned(),
                field_name: "email".to_owned(),
                nullable: true,
                joined_rhs: true,
                classification: FieldClassification::Pii,
                resource: rhs_pii,
            },
            TouchedFieldRef {
                path: "orders.auditNote.secret".to_owned(),
                type_name: "AuditNote".to_owned(),
                field_name: "secret".to_owned(),
                nullable: true,
                joined_rhs: true,
                classification: FieldClassification::Normal,
                resource: rhs_quarantined.clone(),
            },
        ],
    };

    let log = InMemoryDecisionLog::new();
    let ctx = AuthzContext::minimal(acme.clone());
    let report = authorize_plan_surface(
        &compiled,
        &reader,
        &[],
        &surface,
        &ctx,
        QueryDenyPolicy::NullWithError,
        Some(&log),
    );
    assert!(report.allowed_execution);
    assert!(report.policy_version_matches);
    assert_eq!(
        report.field_decisions[0].outcome,
        FieldDecisionOutcome::Allowed
    );
    assert_eq!(
        report.field_decisions[1].outcome,
        FieldDecisionOutcome::RedactedPii
    );
    assert_eq!(
        report.field_decisions[2].outcome,
        FieldDecisionOutcome::NullWithError
    );

    let err = report.field_decisions[2]
        .client_error
        .as_ref()
        .expect("nullable RHS deny includes generic error");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(err.message(), GENERIC_FORBIDDEN_MESSAGE);
    assert!(!err.message().contains("permit-"));
    assert!(!err.message().contains("forbid-"));

    // Denied query field is always recorded in DecisionLog (100% deny capture).
    let log_entries = log.entries();
    assert!(
        log_entries
            .iter()
            .any(|e| e.resource_id == "AuditNote.secret" && e.decision == DecisionKind::Deny)
    );

    // With FailDocument on non-nullable or FailDocument policy, whole execution is blocked.
    let fail_doc_report = authorize_plan_surface(
        &compiled,
        &reader,
        &[],
        &surface,
        &ctx,
        QueryDenyPolicy::FailDocument,
        None,
    );
    assert!(!fail_doc_report.allowed_execution);
    assert_eq!(
        fail_doc_report.field_decisions[2].outcome,
        FieldDecisionOutcome::FailWhole
    );
    assert_eq!(
        fail_doc_report
            .document_error
            .as_ref()
            .map(gap_core::error::GapError::code),
        Some(ErrorCode::Forbidden)
    );

    // Granting pii:read unmasks Customer.email.
    reader.scopes.insert("pii:read".to_owned());
    let unmasked = authorize_plan_surface(
        &compiled,
        &reader,
        &[],
        &surface,
        &ctx,
        QueryDenyPolicy::NullWithError,
        None,
    );
    assert_eq!(
        unmasked.field_decisions[1].outcome,
        FieldDecisionOutcome::Allowed
    );

    // Mutation touching a denied operation always rejects the whole plan.
    let mut_surface = TouchedPlanSurface {
        request_id: "req-mut-deny".to_owned(),
        operation_kind: OpKind::Mutation,
        plan_policy_version: compiled.policy_version_u64(),
        fields: vec![TouchedFieldRef {
            path: "quarantineMutation".to_owned(),
            type_name: "Mutation".to_owned(),
            field_name: "quarantineMutation".to_owned(),
            nullable: true,
            joined_rhs: false,
            classification: FieldClassification::Normal,
            resource: rhs_quarantined,
        }],
    };
    let mut_report = authorize_plan_surface(
        &compiled,
        &reader,
        &[],
        &mut_surface,
        &ctx,
        QueryDenyPolicy::NullWithError,
        None,
    );
    assert!(!mut_report.allowed_execution);
    assert_eq!(
        mut_report.field_decisions[0].outcome,
        FieldDecisionOutcome::FailWhole
    );
}

// spec: RTE-010, INV-05
#[test]
fn inv_05_authz_visibility_bitset_hides_forbidden_elements() {
    let compiled = CompiledPolicySet::default_builtin().expect("default policies");
    let acme = tenant("acme");
    let beta = tenant("beta");
    let reader = principal_with_groups(
        "agent-discover",
        PrincipalKind::Agent,
        acme.clone(),
        &["agents-readonly"],
    );

    let mut el_ok = ResourceAttributes::operation(
        "el.orders.list",
        acme.clone(),
        "commerce",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    el_ok.kind = ResourceKind::Element;

    let mut el_t2 = ResourceAttributes::operation(
        "el.untrusted.search",
        acme.clone(),
        "ext",
        TrustTier::T2,
        SideEffectLevel::None,
    );
    el_t2.kind = ResourceKind::Element;

    let mut el_beta = ResourceAttributes::operation(
        "el.beta.list",
        beta,
        "commerce",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    el_beta.kind = ResourceKind::Element;

    let mut el_mut = ResourceAttributes::operation(
        "el.orders.refund",
        acme.clone(),
        "commerce",
        TrustTier::T0,
        SideEffectLevel::Irreversible,
    );
    el_mut.kind = ResourceKind::Element;

    let bits = visibility_bitset(
        &compiled,
        &reader,
        &[],
        &AuthzContext::minimal(acme),
        &[el_ok, el_t2, el_beta, el_mut],
    );
    assert_eq!(bits, vec![true, false, false, false]);
}

// spec: SEC-024
#[test]
fn authz_decision_log_captures_all_denies_all_mutations_and_samples_reads() {
    for req_idx in 0..20 {
        let req_id = format!("req-{req_idx}");
        assert!(should_log_decision(
            DecisionKind::Deny,
            AuthzAction::Execute,
            &req_id,
            "Order.id"
        ));
        assert!(should_log_decision(
            DecisionKind::Allow,
            AuthzAction::Mutate,
            &req_id,
            "orders.create"
        ));
        assert!(should_log_decision(
            DecisionKind::Allow,
            AuthzAction::AdminPolicy,
            &req_id,
            "policy.default"
        ));
        assert!(should_log_decision(
            DecisionKind::Allow,
            AuthzAction::DecideApproval,
            &req_id,
            "approval.1"
        ));
    }

    let mut sampled = 0usize;
    let total = 2_000usize;
    for i in 0..total {
        let req_id = format!("read-sample-{i}");
        if should_log_decision(
            DecisionKind::Allow,
            AuthzAction::Execute,
            &req_id,
            "Order.id",
        ) {
            sampled += 1;
        }
    }
    // 1% deterministic sampling over 2000 distinct requests yields ~20 hits (strictly bounded away from 0% and 100%).
    assert!(
        sampled > 5 && sampled < 60,
        "unexpected sample count: {sampled}"
    );
}
