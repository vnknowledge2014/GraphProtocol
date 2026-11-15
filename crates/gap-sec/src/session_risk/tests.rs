//! Unit tests for Rule-of-Two session risk tracking (`SEC-084`, `INV-09`, `TM-05`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::ManualClock;
use gap_core::context::{AttrValue, Principal, PrincipalKind};
use gap_core::ids::TenantId;
use gap_core::snapshot::SideEffectLevel;

use super::*;
use crate::authz::engine::{CompiledPolicySet, DecisionKind, ResourceAttributes};
use crate::authz::schema::{AuthzAction, DEFAULT_CEDAR_POLICIES, DEFAULT_CEDAR_SCHEMA};

fn tenant(name: &str) -> TenantId {
    TenantId::new(name).expect("valid tenant id")
}

// spec: SEC-084, TM-05
#[test]
fn rule_of_two_triggers_risk_100_and_forces_cedar_four_eyes_approval() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let tracker = SessionRiskTracker::with_clock(SessionRiskConfig::default(), clock.clone());
    let acme = tenant("acme");
    let key = SessionRiskKey::new(acme.clone(), "agent-1", "ctx-run-42");

    // Step 1 (A): Session reads untrusted T2 data -> riskScore = 30.
    let step_a = tracker.evaluate_and_update(
        &key,
        ActionRiskContribution {
            ingests_untrusted: true,
            accesses_sensitive: false,
            side_effect: SideEffectLevel::None,
        },
        true,
    );
    assert_eq!(step_a.risk_score, 30);
    assert!(!step_a.requires_human_approval);

    // Step 2 (B): Session reads confidential customer record -> riskScore = 60 (< 80 reader cap).
    let step_b = tracker.evaluate_and_update(
        &key,
        ActionRiskContribution {
            ingests_untrusted: false,
            accesses_sensitive: ActionRiskContribution::is_sensitive_classification("confidential"),
            side_effect: SideEffectLevel::None,
        },
        true,
    );
    assert_eq!(step_b.risk_score, 60);
    assert!(!step_b.requires_human_approval);

    // Step 3 (C): Agent requests external reversible mutation -> A ∧ B ∧ C holds, riskScore = 100!
    let step_c = tracker.evaluate_and_update(
        &key,
        ActionRiskContribution {
            ingests_untrusted: false,
            accesses_sensitive: false,
            side_effect: SideEffectLevel::Reversible,
        },
        false,
    );
    assert_eq!(step_c.risk_score, 100);
    assert!(step_c.requires_human_approval);

    // Verify Cedar `forbid-mutate-rule-of-two-high-risk` enforces human approval from another person.
    let policy_set = CompiledPolicySet::compile(DEFAULT_CEDAR_SCHEMA, &[DEFAULT_CEDAR_POLICIES])
        .expect("compile default cedar policies");
    let mut operator = Principal::new("agent-1", PrincipalKind::Agent, acme.clone());
    operator.attrs.insert(
        "groups".to_owned(),
        AttrValue::List(vec!["operators".to_owned()]),
    );
    let target = ResourceAttributes::operation(
        "payments.refund",
        acme.clone(),
        "billing",
        gap_core::snapshot::TrustTier::T0,
        SideEffectLevel::Reversible,
    );

    let mut authz_ctx = AuthzContext::minimal(acme.clone());
    authz_ctx.risk_score = step_c.risk_score;
    let denied = policy_set.evaluate_single(&operator, AuthzAction::Mutate, &target, &authz_ctx);
    assert_eq!(denied.decision, DecisionKind::Deny);

    // Self-approval (`approverIsDifferent = false`) MUST still deny.
    authz_ctx.approved = true;
    let self_approved =
        policy_set.evaluate_single(&operator, AuthzAction::Mutate, &target, &authz_ctx);
    assert_eq!(self_approved.decision, DecisionKind::Deny);

    // Approval from a distinct reviewer permits the guarded mutation.
    authz_ctx.approver_is_different = true;
    let approved = policy_set.evaluate_single(&operator, AuthzAction::Mutate, &target, &authz_ctx);
    assert_eq!(approved.decision, DecisionKind::Allow);
}

// spec: SEC-084, INV-09
#[test]
fn rule_of_two_flags_decay_after_ttl_and_remain_tenant_isolated() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let tracker = SessionRiskTracker::with_clock(
        SessionRiskConfig {
            ttl: Duration::from_secs(3600),
            max_sessions: 4,
        },
        clock.clone(),
    );

    let acme = tenant("acme");
    let beta = tenant("beta");
    let k_acme = SessionRiskKey::new(acme, "shared-principal", "ctx-1");
    let k_beta = SessionRiskKey::new(beta, "shared-principal", "ctx-1");

    let _ = tracker.evaluate_and_update(
        &k_acme,
        ActionRiskContribution {
            ingests_untrusted: true,
            accesses_sensitive: true,
            side_effect: SideEffectLevel::None,
        },
        true,
    );

    // Same `(principal_id, context_id)` in another tenant MUST see 0 risk (`INV-09`).
    assert!(tracker.peek(&k_beta).is_none());
    assert_eq!(tracker.peek(&k_acme).map(|(_, s)| s), Some(60));

    // Advance clock by 59 minutes: flags still active.
    clock.advance(Duration::from_secs(59 * 60));
    assert!(tracker.peek(&k_acme).is_some());

    // Advance past 1 hour TTL: prior A & B flags decay cleanly (`SEC-084`).
    clock.advance(Duration::from_secs(120));
    assert!(tracker.peek(&k_acme).is_none());
    let after_decay = tracker.evaluate_and_update(
        &k_acme,
        ActionRiskContribution {
            ingests_untrusted: false,
            accesses_sensitive: false,
            side_effect: SideEffectLevel::Reversible,
        },
        false,
    );
    assert_eq!(after_decay.risk_score, 30);
    assert!(!after_decay.requires_human_approval);
}
