#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use gap_core::clock::Timestamp;
use gap_core::ids::TenantId;
use serde_json::json;

use crate::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
use crate::error::StoreError;

use super::{
    ATTRIBUTION_WINDOW_MS, DEFAULT_FEEDBACK_RETENTION_MS, FeedbackOutcomeKind,
    NewDiscoveryFeedback, SqliteFeedbackStore, derive_query_hash_hex,
    validate_no_client_outcome_override,
};

#[derive(Default)]
struct InMemoryAuditLog {
    entries: Mutex<Vec<(AuditChain, AuditRecord)>>,
    fail_next: Mutex<bool>,
}

impl AuditLog for InMemoryAuditLog {
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError> {
        if *self.fail_next.lock().unwrap() {
            return Err(StoreError::Backend);
        }
        let mut guard = self.entries.lock().unwrap();
        let seq = guard.iter().filter(|(c, _)| c == chain).count() as u64 + 1;
        let rec = AuditRecord {
            seq,
            entry: entry.clone(),
            prev_hash: [0u8; 32],
            hash: [seq as u8; 32],
        };
        guard.push((chain.clone(), rec.clone()));
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
            head_hash: [0u8; 32],
        })
    }
}

fn sample_feedback(
    id: &str,
    trace: &str,
    session: Option<&str>,
    ts_ms: i64,
) -> NewDiscoveryFeedback {
    NewDiscoveryFeedback {
        feedback_id: id.to_owned(),
        ts: Timestamp::from_unix_millis(ts_ms).unwrap(),
        trace_id: trace.to_owned(),
        session_id: session.map(str::to_owned),
        query_hash_hex: derive_query_hash_hex("list invoices for acme"),
        redacted_query: None,
        candidate_group_ids: vec!["billing:read".to_owned(), "orders:write".to_owned()],
        option_logits: vec![2.4, 0.2, -1.0],
        selected_group_id: Some("billing:read".to_owned()),
        prediction_set_ids: vec!["billing:read".to_owned()],
        abstained_reason: None,
        calibration_id: Some("cal_v1".to_owned()),
    }
}

// spec: RTE-110, RTE-112, TM-14, INV-09
#[test]
fn feedback_server_observed_5min_attribution_and_readonly_export() {
    let store = SqliteFeedbackStore::open_in_memory().expect("open memory store");
    let acme = TenantId::new("acme").unwrap().scope();
    let beta = TenantId::new("beta").unwrap().scope();

    let t0 = 1_700_000_000_000_i64;
    // Row 1: trace match -> ExecutedOk
    store
        .record_discovery(&acme, sample_feedback("fb-1", "tr-1", Some("sess-1"), t0))
        .unwrap();
    // Row 2: trace match + failed execution -> ExecutedError
    store
        .record_discovery(&acme, sample_feedback("fb-2", "tr-2", None, t0 + 10_000))
        .unwrap();
    // Row 3: session match + different group executed -> ExecutedOther
    store
        .record_discovery(
            &acme,
            sample_feedback("fb-3", "tr-3", Some("sess-other"), t0 + 20_000),
        )
        .unwrap();
    // Row 4: outside 5-min window -> stays None
    store
        .record_discovery(
            &acme,
            sample_feedback("fb-4", "tr-late", None, t0 - 400_000),
        )
        .unwrap();
    // Row 5: different tenant sharing same trace -> must remain untouched by acme attribution
    store
        .record_discovery(&beta, sample_feedback("fb-5", "tr-1", Some("sess-1"), t0))
        .unwrap();

    // Verify ExecutedOk within 5 min window.
    let c1 = store
        .attribute_server_execution(
            &acme,
            Some("tr-1"),
            None,
            "billing:read",
            "acme.billing.listInvoices",
            true,
            Timestamp::from_unix_millis(t0 + 60_000).unwrap(),
        )
        .unwrap();
    assert_eq!(c1, 1);

    // Verify ExecutedError when execution failed on predicted group.
    let c2 = store
        .attribute_server_execution(
            &acme,
            Some("tr-2"),
            None,
            "billing:read",
            "acme.billing.listInvoices",
            false,
            Timestamp::from_unix_millis(t0 + 30_000).unwrap(),
        )
        .unwrap();
    assert_eq!(c2, 1);

    // Verify ExecutedOther when server observed another group within session window.
    let c3 = store
        .attribute_server_execution(
            &acme,
            None,
            Some("sess-other"),
            "orders:write",
            "acme.orders.createOrder",
            true,
            Timestamp::from_unix_millis(t0 + 120_000).unwrap(),
        )
        .unwrap();
    assert_eq!(c3, 1);

    // Verify rows past ATTRIBUTION_WINDOW_MS (300_000 ms) are ignored.
    let c4 = store
        .attribute_server_execution(
            &acme,
            Some("tr-late"),
            None,
            "billing:read",
            "acme.billing.listInvoices",
            true,
            Timestamp::from_unix_millis(t0 + ATTRIBUTION_WINDOW_MS).unwrap(),
        )
        .unwrap();
    assert_eq!(c4, 0);

    assert_eq!(
        store.get(&acme, "fb-1").unwrap().unwrap().outcome,
        FeedbackOutcomeKind::ExecutedOk
    );
    assert_eq!(
        store.get(&acme, "fb-2").unwrap().unwrap().outcome,
        FeedbackOutcomeKind::ExecutedError
    );
    assert_eq!(
        store.get(&acme, "fb-3").unwrap().unwrap().outcome,
        FeedbackOutcomeKind::ExecutedOther
    );
    assert_eq!(
        store.get(&acme, "fb-4").unwrap().unwrap().outcome,
        FeedbackOutcomeKind::None
    );
    // Tenant isolation (`INV-09`): beta's fb-5 stayed NONE and invisible to acme.
    assert!(store.get(&acme, "fb-5").unwrap().is_none());
    assert_eq!(
        store.get(&beta, "fb-5").unwrap().unwrap().outcome,
        FeedbackOutcomeKind::None
    );

    // Export read-only calibration samples (`RTE-112`): only ExecutedOk & ExecutedOther exported
    // (ExecutedError & None excluded).
    let exported = store.export_calibration_samples(&acme, 50).unwrap();
    let exported_ids: Vec<&str> = exported.iter().map(|r| r.feedback_id.as_str()).collect();
    assert_eq!(exported_ids, vec!["fb-1", "fb-3"]);
}

// spec: RTE-110, SEC-110, TM-14, INV-09
#[test]
fn feedback_rejects_client_outcomes_audits_user_label_and_enforces_pii_retention() {
    assert!(validate_no_client_outcome_override(&json!({"query": "hi"})).is_ok());
    assert!(
        validate_no_client_outcome_override(&json!({"query": "hi", "outcome": "EXECUTED_OK"}))
            .is_err()
    );
    assert!(
        validate_no_client_outcome_override(&json!({"gold_group_id": "billing:read"})).is_err()
    );

    let store = SqliteFeedbackStore::open_in_memory().unwrap();
    let acme = TenantId::new("acme").unwrap().scope();
    let other = TenantId::new("other").unwrap().scope();
    let audit = InMemoryAuditLog::default();

    // SEC-110: reject raw unredacted email or secret in redacted_query.
    let mut bad_pii = sample_feedback("fb-pii", "tr-pii", None, 1_700_000_000_000);
    bad_pii.redacted_query = Some("contact alice@example.com for invoice".to_owned());
    assert!(store.record_discovery(&acme, bad_pii).is_err());

    let t0 = 1_700_000_000_000_i64;
    let mut clean = sample_feedback("fb-admin", "tr-admin", None, t0);
    clean.redacted_query = Some("contact [REDACTED_EMAIL] for invoice".to_owned());
    store.record_discovery(&acme, clean).unwrap();

    // Cross-tenant admin label must fail NotFound (`INV-09`).
    let cross = store.apply_admin_user_label(
        &other,
        "fb-admin",
        Some("billing:read"),
        "admin@acme",
        "manual review",
        Timestamp::from_unix_millis(t0 + 1_000).unwrap(),
        &audit,
    );
    assert_eq!(cross, Err(StoreError::NotFound));

    // Audited UserLabel fails cleanly without mutating row if audit log rejects append (`TM-14`).
    *audit.fail_next.lock().unwrap() = true;
    assert!(
        store
            .apply_admin_user_label(
                &acme,
                "fb-admin",
                Some("billing:read"),
                "admin@acme",
                "audit failure test",
                Timestamp::from_unix_millis(t0 + 2_000).unwrap(),
                &audit,
            )
            .is_err()
    );
    assert_eq!(
        store.get(&acme, "fb-admin").unwrap().unwrap().outcome,
        FeedbackOutcomeKind::None
    );

    // Succeeding audit append records Tenant audit entry and commits USER_LABEL.
    *audit.fail_next.lock().unwrap() = false;
    store
        .apply_admin_user_label(
            &acme,
            "fb-admin",
            Some("orders:write"),
            "sec-reviewer-1",
            "confirmed actual target group",
            Timestamp::from_unix_millis(t0 + 3_000).unwrap(),
            &audit,
        )
        .unwrap();
    let labeled = store.get(&acme, "fb-admin").unwrap().unwrap();
    assert_eq!(labeled.outcome, FeedbackOutcomeKind::UserLabel);
    assert_eq!(
        labeled.attributed_operation_id.as_deref(),
        Some("orders:write")
    );
    assert_eq!(audit.entries.lock().unwrap().len(), 1);
    assert_eq!(
        audit.entries.lock().unwrap()[0].1.entry.action,
        "routing.feedback.user_label"
    );

    // Retention cap rejects >7d and purges expired rows on schedule (`SEC-110`).
    assert!(
        store
            .purge_expired(
                &acme,
                Timestamp::from_unix_millis(t0).unwrap(),
                DEFAULT_FEEDBACK_RETENTION_MS + 1,
            )
            .is_err()
    );
    let purged = store
        .purge_expired(
            &acme,
            Timestamp::from_unix_millis(t0 + DEFAULT_FEEDBACK_RETENTION_MS + 10_000).unwrap(),
            DEFAULT_FEEDBACK_RETENTION_MS,
        )
        .unwrap();
    assert_eq!(purged, 1);
    assert!(store.get(&acme, "fb-admin").unwrap().is_none());
}
