//! Unit and persistence tests for ingestion proposals and `DiscoveryWorker` (`WP-M4-07`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::ManualClock;
use gap_core::cursor::CursorKey;
use gap_core::error::{ErrorCode, NextStep};
use gap_core::ids::{BranchId, ProposalId, TenantId};
use gap_core::snapshot::{Protocol, TrustTier};
use gap_store::audit::{AuditChain, AuditLog, AuditVerifyReport};
use gap_store::sqlite::{SqliteConfig, SqliteContextStore};

use crate::proposals::audit_adapter::ContextStoreAuditLog;
use crate::proposals::discovery::{
    DiscoveryWorker, IntakeOutcome, RegistrySourceEntry, UnmappedEventIntake, UnmappedIntakeConfig,
    insufficient_evidence_error, redact_query,
};
use crate::proposals::graphql::{
    DecideProposalInput, decide_proposal_graphql, proposals_graphql, propose_source_graphql,
};
use crate::proposals::service::{ProposalService, authorize_shadow_header, canonicalize_location};
use crate::proposals::types::{
    ADMIN_INGEST_SCOPE, ADMIN_SCHEMA_SCOPE, AdminCaller, ApprovalDecision, ApprovalPolicyConfig,
    GateCheckResult, PROPOSAL_APPROVAL_TTL_SECS, ProposalError, ProposalFilter, ProposalReport,
    ProposalStatus, ProposeSourceInput, UNMAPPED_QUERY_MAX_TTL_SECS, UNMAPPED_RATE_LIMIT_PER_MIN,
    UNMAPPED_SINGLEFLIGHT_WINDOW_SECS,
};

struct TempDbGuard {
    dir: PathBuf,
    db_path: PathBuf,
}

impl TempDbGuard {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gap-m4-07-{tag}-{}", ProposalId::new_v7()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("proposals.sqlite");
        Self { dir, db_path }
    }
}

impl Drop for TempDbGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn test_cursor_key() -> CursorKey {
    CursorKey::from_array([0x42; 32])
}

fn make_report(
    breaking: bool,
    manifest_verified: bool,
    enrichment_degraded: bool,
) -> ProposalReport {
    let mut tool_versions = BTreeMap::new();
    tool_versions.insert("openapi-extractor".to_owned(), "0.4.7".to_owned());
    ProposalReport {
        source: "openapi://petstore.example/spec.json".to_owned(),
        spec_hash: "a".repeat(64),
        tool_versions,
        model_ids: vec!["laya-typed-decisions".to_owned()],
        gate_results: vec![GateCheckResult {
            gate: "G1-parse-and-bounds".to_owned(),
            passed: true,
            evidence: "12 operations parsed".to_owned(),
        }],
        diffs_summary: Some("+2 queries".to_owned()),
        enrichment_degraded,
        breaking,
        mutating: false,
        cross_trust_join: false,
        manifest_verified,
    }
}

fn admin_ingest(name: &str) -> AdminCaller {
    AdminCaller::new(name, [ADMIN_INGEST_SCOPE])
}

fn open_service(
    guard: &TempDbGuard,
    clock: Arc<ManualClock>,
    policy: ApprovalPolicyConfig,
) -> (ProposalService, Arc<ContextStoreAuditLog>) {
    let store = Arc::new(
        SqliteContextStore::open(SqliteConfig::new(&guard.db_path), clock.clone()).unwrap(),
    );
    let audit = Arc::new(ContextStoreAuditLog::new(store.clone(), clock.clone()).unwrap());
    let svc = ProposalService::new(store, audit.clone(), clock, policy, test_cursor_key());
    (svc, audit)
}

// spec: ING-001, INV-14, SEC-090
#[test]
fn proposal_fsm_full_lifecycle_persists_across_reopen_and_verifies_audit_chain() {
    let guard = TempDbGuard::new("fsm");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let tenant = TenantId::new("acme").unwrap().scope();
    let policy = ApprovalPolicyConfig {
        auto_activate_t0: false,
        ..ApprovalPolicyConfig::default()
    };

    let proposal_id = {
        let (svc, audit) = open_service(&guard, clock.clone(), policy.clone());
        let p = svc
            .propose_source(
                &tenant,
                "alice",
                ProposeSourceInput {
                    kind: Protocol::Rest,
                    location: "https://api.example.com/v1/openapi.json".to_owned(),
                    branch: Some(BranchId::new("orders").unwrap()),
                    trust: TrustTier::T1,
                    notes: Some("initial onboarding".to_owned()),
                },
            )
            .unwrap();
        assert_eq!(p.status, ProposalStatus::Proposed);

        let p = svc.begin_extracting(&tenant, None, p.id).unwrap();
        assert_eq!(p.status, ProposalStatus::Extracting);

        let p = svc.begin_enriching(&tenant, "worker", p.id).unwrap();
        assert_eq!(p.status, ProposalStatus::Enriching);

        let p = svc.begin_verifying(&tenant, "worker", p.id).unwrap();
        assert_eq!(p.status, ProposalStatus::Verifying);

        let p = svc
            .stage_verified(
                &tenant,
                "worker",
                p.id,
                "shadow:gen42".to_owned(),
                make_report(false, true, false),
            )
            .unwrap();
        assert_eq!(p.status, ProposalStatus::AwaitingApproval);

        // Illegal skip AWAITING_APPROVAL -> ACTIVE must fail cleanly without mutating state.
        let err = svc.mark_active(&tenant, "publisher", p.id).unwrap_err();
        assert!(matches!(err, ProposalError::Validation(_)));

        let p = svc
            .decide_proposal(
                &tenant,
                &admin_ingest("admin-1"),
                p.id,
                ApprovalDecision::Approve,
                false,
                Some("looks good".to_owned()),
            )
            .unwrap();
        assert_eq!(p.status, ProposalStatus::Promoting);

        let p = svc.mark_active(&tenant, "publisher", p.id).unwrap();
        assert_eq!(p.status, ProposalStatus::Active);

        let p = svc.begin_watching(&tenant, "publisher", p.id).unwrap();
        assert_eq!(p.status, ProposalStatus::Watching);

        let p = svc.mark_stable(&tenant, "publisher", p.id).unwrap();
        assert_eq!(p.status, ProposalStatus::Stable);

        let p = svc
            .reopen_stable(
                &tenant,
                "drift-monitor",
                p.id,
                Some("spec hash changed".to_owned()),
            )
            .unwrap();
        assert_eq!(p.status, ProposalStatus::Proposed);

        assert_eq!(
            svc.transition_count(ProposalStatus::Proposed, ProposalStatus::Extracting),
            1
        );
        assert_eq!(
            svc.transition_count(ProposalStatus::Verifying, ProposalStatus::Staged),
            1
        );
        assert_eq!(
            svc.transition_count(ProposalStatus::Staged, ProposalStatus::AwaitingApproval),
            1
        );
        assert_eq!(
            svc.transition_count(ProposalStatus::Watching, ProposalStatus::Stable),
            1
        );
        assert_eq!(
            svc.transition_count(ProposalStatus::Stable, ProposalStatus::Proposed),
            1
        );

        let report = audit
            .verify_chain(&AuditChain::Tenant(tenant.clone()), None)
            .unwrap();
        assert!(matches!(report, AuditVerifyReport::Intact { head_seq, .. } if head_seq >= 10));
        p.id
    };

    // Reopen fresh service over the same SQLite database file: persisted state & audit remain intact.
    let (svc2, audit2) = open_service(
        &guard,
        clock,
        ApprovalPolicyConfig {
            auto_activate_t0: true,
            ..policy
        },
    );
    let loaded = svc2.get_proposal(&tenant, proposal_id).unwrap().unwrap();
    assert_eq!(loaded.status, ProposalStatus::Proposed);
    assert_eq!(loaded.branch, Some(BranchId::new("orders").unwrap()));
    assert!(loaded.approvals.is_empty());

    let err_on_rollback_branch = {
        let p2 = svc2
            .propose_source(
                &tenant,
                "bob",
                ProposeSourceInput {
                    kind: Protocol::Graphql,
                    location: "https://gql.example.com/graphql".to_owned(),
                    branch: None,
                    trust: TrustTier::T0,
                    notes: None,
                },
            )
            .unwrap();
        let p2 = svc2.begin_extracting(&tenant, None, p2.id).unwrap();
        let p2 = svc2.begin_enriching(&tenant, "w", p2.id).unwrap();
        let p2 = svc2.begin_verifying(&tenant, "w", p2.id).unwrap();
        let p2 = svc2
            .stage_verified(
                &tenant,
                "w",
                p2.id,
                "shadow:gen43".to_owned(),
                make_report(false, true, false),
            )
            .unwrap();
        assert_eq!(p2.status, ProposalStatus::Promoting);
        let p2 = svc2.mark_active(&tenant, "pub", p2.id).unwrap();
        let p2 = svc2.begin_watching(&tenant, "pub", p2.id).unwrap();
        let p2 = svc2
            .rollback_proposal(&tenant, "pub", p2.id, "latency regression")
            .unwrap();
        assert_eq!(p2.status, ProposalStatus::RolledBack);
        p2.id
    };
    let reloaded = svc2
        .get_proposal(&tenant, err_on_rollback_branch)
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, ProposalStatus::RolledBack);

    let failed_id = {
        let pf = svc2
            .propose_source(
                &tenant,
                "bob",
                ProposeSourceInput {
                    kind: Protocol::Sql,
                    location: "sql://warehouse.internal/main".to_owned(),
                    branch: None,
                    trust: TrustTier::T1,
                    notes: None,
                },
            )
            .unwrap();
        let pf = svc2.begin_extracting(&tenant, None, pf.id).unwrap();
        let pf = svc2
            .fail_proposal(&tenant, "extractor", pf.id, "connection refused", None)
            .unwrap();
        assert_eq!(pf.status, ProposalStatus::Failed);
        pf.id
    };
    assert_eq!(
        svc2.get_proposal(&tenant, failed_id)
            .unwrap()
            .unwrap()
            .status,
        ProposalStatus::Failed
    );

    let final_audit = audit2
        .verify_chain(&AuditChain::Tenant(tenant), None)
        .unwrap();
    assert!(matches!(final_audit, AuditVerifyReport::Intact { .. }));
}

// spec: ING-002, INV-09
#[test]
fn proposal_idempotency_canonicalizes_location_and_isolates_tenants() {
    let guard = TempDbGuard::new("idemp");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (svc, _) = open_service(&guard, clock, ApprovalPolicyConfig::default());

    assert_eq!(
        canonicalize_location("  HTTPS://Api.Example.COM:443/v1//users/#frag  ").unwrap(),
        "https://api.example.com/v1/users"
    );
    assert_eq!(
        canonicalize_location("http://svc.internal:80/").unwrap(),
        "http://svc.internal"
    );
    assert!(canonicalize_location("/safe/../etc/passwd").is_err());

    let t1 = TenantId::new("tenant-alpha").unwrap().scope();
    let t2 = TenantId::new("tenant-beta").unwrap().scope();

    let first = svc
        .propose_source(
            &t1,
            "dev-a",
            ProposeSourceInput {
                kind: Protocol::Rest,
                location: "HTTPS://Catalog.Example.COM:443/openapi/./v2/".to_owned(),
                branch: None,
                trust: TrustTier::T1,
                notes: None,
            },
        )
        .unwrap();

    let dup = svc
        .propose_source(
            &t1,
            "dev-b",
            ProposeSourceInput {
                kind: Protocol::Rest,
                location: "https://catalog.example.com/openapi/v2#top".to_owned(),
                branch: None,
                trust: TrustTier::T2,
                notes: Some("should deduplicate".to_owned()),
            },
        )
        .unwrap();
    assert_eq!(dup.id, first.id);

    // Different kind creates a distinct proposal even for the same location.
    let diff_kind = svc
        .propose_source(
            &t1,
            "dev-a",
            ProposeSourceInput {
                kind: Protocol::Graphql,
                location: "https://catalog.example.com/openapi/v2".to_owned(),
                branch: None,
                trust: TrustTier::T1,
                notes: None,
            },
        )
        .unwrap();
    assert_ne!(diff_kind.id, first.id);

    // Different tenant creates its own isolated proposal (INV-09).
    let other_tenant = svc
        .propose_source(
            &t2,
            "dev-c",
            ProposeSourceInput {
                kind: Protocol::Rest,
                location: "https://catalog.example.com/openapi/v2".to_owned(),
                branch: None,
                trust: TrustTier::T1,
                notes: None,
            },
        )
        .unwrap();
    assert_ne!(other_tenant.id, first.id);
    assert!(svc.get_proposal(&t2, first.id).unwrap().is_none());

    // Once terminal (REJECTED), the same (tenant, location, kind) can open a new proposal.
    svc.decide_proposal(
        &t1,
        &admin_ingest("admin-a"),
        first.id,
        ApprovalDecision::Deny,
        false,
        Some("denied for retry test".to_owned()),
    )
    .unwrap();
    let reopened = svc
        .propose_source(
            &t1,
            "dev-a",
            ProposeSourceInput {
                kind: Protocol::Rest,
                location: "https://catalog.example.com/openapi/v2".to_owned(),
                branch: None,
                trust: TrustTier::T1,
                notes: None,
            },
        )
        .unwrap();
    assert_ne!(reopened.id, first.id);
}

// spec: ING-003, ING-035, ING-050
#[test]
fn proposal_t2_extract_gate_t0_auto_activate_degraded_block_and_shadow_header() {
    let guard = TempDbGuard::new("t0-shadow");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (svc, _) = open_service(
        &guard,
        clock,
        ApprovalPolicyConfig {
            auto_activate_t0: true,
            ..ApprovalPolicyConfig::default()
        },
    );
    let tenant = TenantId::new("acme").unwrap().scope();

    // T2 requires admin:ingest before PROPOSED -> EXTRACTING (ING-003).
    let t2 = svc
        .propose_source(
            &tenant,
            "discovery",
            ProposeSourceInput {
                kind: Protocol::Mcp,
                location: "https://mcp.vendor.example/manifest.json".to_owned(),
                branch: None,
                trust: TrustTier::T2,
                notes: None,
            },
        )
        .unwrap();
    assert!(matches!(
        svc.begin_extracting(&tenant, None, t2.id),
        Err(ProposalError::Forbidden(_))
    ));
    assert!(matches!(
        svc.begin_extracting(
            &tenant,
            Some(&AdminCaller::new("viewer", ["read:all"])),
            t2.id
        ),
        Err(ProposalError::Forbidden(_))
    ));
    let t2_ext = svc
        .begin_extracting(&tenant, Some(&admin_ingest("alice")), t2.id)
        .unwrap();
    assert_eq!(t2_ext.status, ProposalStatus::Extracting);

    // T0 with signed manifest + intact enrichment + non-breaking auto-activates STAGED -> PROMOTING.
    let t0_ok = svc
        .propose_source(
            &tenant,
            "cicd",
            ProposeSourceInput {
                kind: Protocol::Grpc,
                location: "https://internal.acme.example/orders.proto".to_owned(),
                branch: None,
                trust: TrustTier::T0,
                notes: None,
            },
        )
        .unwrap();
    svc.begin_extracting(&tenant, None, t0_ok.id).unwrap();
    svc.begin_enriching(&tenant, "w", t0_ok.id).unwrap();
    svc.begin_verifying(&tenant, "w", t0_ok.id).unwrap();
    let t0_staged = svc
        .stage_verified(
            &tenant,
            "w",
            t0_ok.id,
            "snap-shadow-t0-ok".to_owned(),
            make_report(false, true, false),
        )
        .unwrap();
    assert_eq!(t0_staged.status, ProposalStatus::Promoting);

    // Shadow header check (ING-050): requires admin:schema and matching proposal ID.
    let reader = AdminCaller::new("viewer", [ADMIN_INGEST_SCOPE]);
    assert!(matches!(
        authorize_shadow_header(&format!("shadow:{}", t0_staged.id), &reader, &t0_staged),
        Err(ProposalError::Forbidden(_))
    ));
    let schema_admin = AdminCaller::new("schema-op", [ADMIN_SCHEMA_SCOPE]);
    let snap = authorize_shadow_header(
        &format!("shadow:{}", t0_staged.id),
        &schema_admin,
        &t0_staged,
    )
    .unwrap();
    assert_eq!(snap, "snap-shadow-t0-ok");

    // enrichment=degraded blocks T0 auto-activation even when signed & non-breaking (ING-035).
    let t0_deg = svc
        .propose_source(
            &tenant,
            "cicd",
            ProposeSourceInput {
                kind: Protocol::Grpc,
                location: "https://internal.acme.example/inventory.proto".to_owned(),
                branch: None,
                trust: TrustTier::T0,
                notes: None,
            },
        )
        .unwrap();
    svc.begin_extracting(&tenant, None, t0_deg.id).unwrap();
    svc.begin_enriching(&tenant, "w", t0_deg.id).unwrap();
    svc.begin_verifying(&tenant, "w", t0_deg.id).unwrap();
    let t0_deg_staged = svc
        .stage_verified(
            &tenant,
            "w",
            t0_deg.id,
            "snap-shadow-t0-deg".to_owned(),
            make_report(false, true, true),
        )
        .unwrap();
    assert_eq!(t0_deg_staged.status, ProposalStatus::AwaitingApproval);
}

// spec: ING-051
#[test]
fn proposal_two_person_rule_breaking_ack_and_14d_ttl_expiry() {
    let guard = TempDbGuard::new("policy");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (svc, _) = open_service(&guard, clock.clone(), ApprovalPolicyConfig::default());
    let tenant = TenantId::new("acme").unwrap().scope();

    let p = svc
        .propose_source(
            &tenant,
            "bot",
            ProposeSourceInput {
                kind: Protocol::Rest,
                location: "https://partner.example/payments.yaml".to_owned(),
                branch: None,
                trust: TrustTier::T2,
                notes: None,
            },
        )
        .unwrap();
    svc.begin_extracting(&tenant, Some(&admin_ingest("admin-1")), p.id)
        .unwrap();
    svc.begin_enriching(&tenant, "w", p.id).unwrap();
    svc.begin_verifying(&tenant, "w", p.id).unwrap();
    let p = svc
        .stage_verified(
            &tenant,
            "w",
            p.id,
            "shadow-breaking".to_owned(),
            make_report(true, true, false),
        )
        .unwrap();
    assert_eq!(p.status, ProposalStatus::AwaitingApproval);

    // Missing breakingChangeAck rejected cleanly.
    assert!(matches!(
        svc.decide_proposal(
            &tenant,
            &admin_ingest("admin-1"),
            p.id,
            ApprovalDecision::Approve,
            false,
            None,
        ),
        Err(ProposalError::BreakingChangeAckRequired(_))
    ));

    // First admin approves: still AWAITING_APPROVAL because T2 requires 2 distinct admins.
    let p1 = svc
        .decide_proposal(
            &tenant,
            &admin_ingest("admin-1"),
            p.id,
            ApprovalDecision::Approve,
            true,
            Some("ack 1".to_owned()),
        )
        .unwrap();
    assert_eq!(p1.status, ProposalStatus::AwaitingApproval);
    assert_eq!(p1.approvals, vec!["admin-1".to_owned()]);

    // Same admin repeating does NOT satisfy two-person rule.
    let p_dup = svc
        .decide_proposal(
            &tenant,
            &admin_ingest("admin-1"),
            p.id,
            ApprovalDecision::Approve,
            true,
            None,
        )
        .unwrap();
    assert_eq!(p_dup.status, ProposalStatus::AwaitingApproval);
    assert_eq!(p_dup.approvals, vec!["admin-1".to_owned()]);

    // Distinct second admin promotes to PROMOTING.
    let p2 = svc
        .decide_proposal(
            &tenant,
            &admin_ingest("admin-2"),
            p.id,
            ApprovalDecision::Approve,
            true,
            Some("ack 2".to_owned()),
        )
        .unwrap();
    assert_eq!(p2.status, ProposalStatus::Promoting);
    assert_eq!(
        p2.approvals,
        vec!["admin-1".to_owned(), "admin-2".to_owned()]
    );

    // 14-day TTL expiry transitions AWAITING_APPROVAL -> REJECTED.
    let expiring = svc
        .propose_source(
            &tenant,
            "bot",
            ProposeSourceInput {
                kind: Protocol::Cli,
                location: "/opt/tools/slow-cli.json".to_owned(),
                branch: None,
                trust: TrustTier::T1,
                notes: None,
            },
        )
        .unwrap();
    svc.begin_extracting(&tenant, None, expiring.id).unwrap();
    svc.begin_enriching(&tenant, "w", expiring.id).unwrap();
    svc.begin_verifying(&tenant, "w", expiring.id).unwrap();
    let expiring = svc
        .stage_verified(
            &tenant,
            "w",
            expiring.id,
            "shadow-expire".to_owned(),
            make_report(false, false, false),
        )
        .unwrap();
    assert_eq!(expiring.status, ProposalStatus::AwaitingApproval);

    clock.advance(Duration::from_secs(PROPOSAL_APPROVAL_TTL_SECS));
    let expired_err = svc
        .decide_proposal(
            &tenant,
            &admin_ingest("admin-1"),
            expiring.id,
            ApprovalDecision::Approve,
            false,
            None,
        )
        .unwrap_err();
    assert!(matches!(expired_err, ProposalError::ApprovalExpired(_)));
    assert_eq!(
        svc.get_proposal(&tenant, expiring.id)
            .unwrap()
            .unwrap()
            .status,
        ProposalStatus::Rejected
    );
}

// spec: RUN-025, INV-09
#[test]
fn proposal_graphql_ops_and_signed_cursor_pagination() {
    let guard = TempDbGuard::new("graphql");
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let (svc, _) = open_service(&guard, clock.clone(), ApprovalPolicyConfig::default());
    let t1 = TenantId::new("alpha").unwrap().scope();
    let t2 = TenantId::new("beta").unwrap().scope();

    let caller_ingest = admin_ingest("op-a");
    let caller_unauth = AdminCaller::new("guest", ["read:public"]);

    for idx in 0..3 {
        clock.advance(Duration::from_secs(1));
        let val = propose_source_graphql(
            &svc,
            &t1,
            "op-a",
            ProposeSourceInput {
                kind: Protocol::Rest,
                location: format!("https://api{idx}.example/spec.json"),
                branch: None,
                trust: TrustTier::T1,
                notes: None,
            },
            "corr-prop",
        )
        .unwrap();
        assert!(val.to_canonical_json().is_ok());
    }

    // Missing scope rejected on gap.proposals.
    let forbidden = proposals_graphql(
        &svc,
        &t1,
        &caller_unauth,
        ProposalFilter::default(),
        "corr-q",
    );
    assert_eq!(forbidden.unwrap_err().code(), ErrorCode::Forbidden);

    let page1 = svc
        .list_proposals(
            &t1,
            ProposalFilter {
                statuses: None,
                first: 2,
                after: None,
            },
        )
        .unwrap();
    assert_eq!(page1.nodes.len(), 2);
    assert!(page1.page_info.has_next_page);
    let cursor = page1.page_info.end_cursor.clone().unwrap();

    // Cursor cannot be replayed across tenants (INV-09, RUN-025).
    assert!(
        svc.list_proposals(
            &t2,
            ProposalFilter {
                statuses: None,
                first: 2,
                after: Some(cursor.clone()),
            },
        )
        .is_err()
    );

    let page2 = svc
        .list_proposals(
            &t1,
            ProposalFilter {
                statuses: None,
                first: 2,
                after: Some(cursor),
            },
        )
        .unwrap();
    assert_eq!(page2.nodes.len(), 1);
    assert!(!page2.page_info.has_next_page);

    // decide_proposal_graphql returns canonical AST on DENY.
    let denied = decide_proposal_graphql(
        &svc,
        &t1,
        &caller_ingest,
        DecideProposalInput {
            id: page2.nodes[0].id,
            decision: ApprovalDecision::Deny,
            breaking_change_ack: false,
            reason: Some("obsolete".to_owned()),
        },
        "corr-deny",
    )
    .unwrap();
    assert!(denied.to_canonical_json().unwrap().contains("\"REJECTED\""));
}

// spec: RTE-100, RTE-101, RTE-102, RTE-103, GQL-071
#[test]
fn discovery_worker_intake_redaction_rate_limit_singleflight_and_allowlist() {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let tenant = TenantId::new("acme").unwrap().scope();

    // RTE-100: redacts bearer tokens, secrets, emails, URL userinfo & query secrets.
    let redacted = redact_query(
        "call https://user:pw123@billing.example/pay?api_key=xyz&ok=1 with Bearer eyJhbGciOi.payload.sig for alice@acme.org token=sk-secret999999",
    );
    assert!(!redacted.contains("pw123"));
    assert!(!redacted.contains("xyz"));
    assert!(!redacted.contains("eyJhbGciOi"));
    assert!(!redacted.contains("alice@acme.org"));
    assert!(!redacted.contains("sk-secret999999"));
    assert!(redacted.contains("[REDACTED]"));
    assert!(redacted.contains("[REDACTED_EMAIL]"));

    // RTE-100: retention > 7 days rejected.
    let bad_cfg = UnmappedIntakeConfig {
        privacy_store_queries: true,
        retention_secs: UNMAPPED_QUERY_MAX_TTL_SECS + 1,
        ..UnmappedIntakeConfig::default()
    };
    assert!(UnmappedEventIntake::new(bad_cfg, clock.clone()).is_err());

    // Default config: privacy.store_queries = false -> query_redacted & expires_at omitted.
    let default_intake =
        UnmappedEventIntake::new(UnmappedIntakeConfig::default(), clock.clone()).unwrap();
    let accepted_default = default_intake
        .record(
            &tenant,
            "check stripe invoices",
            vec![("billing".to_owned(), 0.2)],
        )
        .unwrap();
    let IntakeOutcome::Accepted(ev_no_text) = accepted_default else {
        panic!("expected accepted unmapped event");
    };
    assert!(ev_no_text.query_redacted.is_none());
    assert!(ev_no_text.expires_at.is_none());

    // Coalescing (10 min) & rate limiting (10 / min / tenant).
    let intake = UnmappedEventIntake::new(
        UnmappedIntakeConfig {
            privacy_store_queries: true,
            retention_secs: UNMAPPED_QUERY_MAX_TTL_SECS,
            singleflight_window_secs: UNMAPPED_SINGLEFLIGHT_WINDOW_SECS,
            rate_limit_per_min: UNMAPPED_RATE_LIMIT_PER_MIN,
        },
        clock.clone(),
    )
    .unwrap();

    for i in 0..UNMAPPED_RATE_LIMIT_PER_MIN {
        let out = intake
            .record(&tenant, &format!("find billing invoice {i}"), vec![])
            .unwrap();
        assert!(matches!(out, IntakeOutcome::Accepted(_)));
    }
    // Identical query within 10 min coalesces without hitting rate limit counter.
    let coalesced = intake
        .record(&tenant, "find billing invoice 0", vec![])
        .unwrap();
    assert!(matches!(coalesced, IntakeOutcome::Coalesced { .. }));
    assert_eq!(intake.dropped_total(), 0);

    // 11th distinct query within the same minute is rate-limited.
    let limited = intake
        .record(&tenant, "find billing invoice 99", vec![])
        .unwrap();
    assert!(matches!(
        limited,
        IntakeOutcome::RateLimited { dropped_total: 1 }
    ));
    assert_eq!(intake.dropped_total(), 1);

    // After singleflight window elapses (>10 min), same query accepted again and expired queue entries drop after 7d.
    clock.advance(Duration::from_secs(UNMAPPED_SINGLEFLIGHT_WINDOW_SECS + 1));
    let accepted_later = intake
        .record(&tenant, "find billing invoice 0", vec![])
        .unwrap();
    let IntakeOutcome::Accepted(ev) = accepted_later else {
        panic!("expected accepted event after singleflight expiry");
    };

    // RTE-103 / GQL-071: immediate GAP_INSUFFICIENT_EVIDENCE error with NextStep::ProposeSource.
    let err = insufficient_evidence_error("corr-103", vec!["list invoices".to_owned()]);
    assert_eq!(err.code(), ErrorCode::InsufficientEvidence);
    assert_eq!(
        err.suggestion().and_then(|s| s.next_step),
        Some(NextStep::ProposeSource)
    );

    // RTE-102: DiscoveryWorker queries only allow-listed registries and emits PROPOSED only.
    let guard = TempDbGuard::new("worker");
    let (svc, _) = open_service(&guard, clock, ApprovalPolicyConfig::default());
    let worker = DiscoveryWorker::new(
        ["registry.trusted.example"],
        vec![
            RegistrySourceEntry {
                registry_host: "registry.trusted.example".to_owned(),
                kind: Protocol::Rest,
                location: "https://registry.trusted.example/billing/openapi.json".to_owned(),
                branch: Some(BranchId::new("billing").unwrap()),
                trust: TrustTier::T2,
                keywords: vec!["billing".to_owned(), "invoice".to_owned()],
                summary: "Trusted billing spec".to_owned(),
            },
            RegistrySourceEntry {
                registry_host: "evil.unlisted.example".to_owned(),
                kind: Protocol::Rest,
                location: "https://evil.unlisted.example/billing.json".to_owned(),
                branch: None,
                trust: TrustTier::T2,
                keywords: vec!["billing".to_owned(), "invoice".to_owned()],
                summary: "Should be ignored because host not in allow-list".to_owned(),
            },
        ],
    );

    let proposed = worker.process_event(&svc, &tenant, &ev).unwrap();
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].status, ProposalStatus::Proposed);
    assert_eq!(
        proposed[0].location,
        "https://registry.trusted.example/billing/openapi.json"
    );

    // Repeating process_event hits ING-002 idempotency without creating duplicates or activating.
    let proposed_again = worker.process_event(&svc, &tenant, &ev).unwrap();
    assert_eq!(proposed_again.len(), 1);
    assert_eq!(proposed_again[0].id, proposed[0].id);
    assert_eq!(proposed_again[0].status, ProposalStatus::Proposed);
}
