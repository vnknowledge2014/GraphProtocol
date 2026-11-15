//! Unit and tamper-detection tests for `gap-store::audit` (`SEC-090`, `SEC-092`, `INV-09`, `INV-14`, `TM-17`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{ManualClock, Timestamp};
use gap_core::ids::TenantId;
use tempfile::tempdir;

use super::*;

fn sample_entry(ts_ms: i64, action: &str, resource: &str, decision: &str) -> AuditEntry {
    AuditEntry {
        ts: Timestamp::from_unix_millis(ts_ms).expect("valid ts"),
        actor_chain: vec!["agent:planner".to_owned(), "user:alice".to_owned()],
        action: action.to_owned(),
        resource: resource.to_owned(),
        decision: decision.to_owned(),
        details_json: r#"{"args_hash":"abc123","outcome":"ok"}"#.to_owned(),
    }
}

// spec: SEC-090, SEC-091, SEC-092, INV-09
#[test]
fn audit_log_trait_is_object_safe_and_audit_chain_carries_tenant_scope() {
    fn _assert_dyn(_log: &dyn AuditLog) {}

    let scope = TenantId::new("acme").expect("tenant").scope();
    let tenant_chain = AuditChain::Tenant(scope);
    let sys_chain = AuditChain::System;
    assert_ne!(tenant_chain, sys_chain);
}

// spec: INV-14, SEC-090
#[test]
fn inv_14_append_only_hash_chain_and_privileged_sync_vs_standard_batching() {
    let clock = Arc::new(ManualClock::new(1_000_000));
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("audit-batch.sqlite");
    let log = SqliteAuditLog::open_with_clock(
        SqliteAuditConfig::file(&db_path).with_batch_window_ms(100),
        Arc::clone(&clock) as Arc<dyn gap_core::clock::Clock>,
    )
    .expect("open sqlite audit");

    let tenant = TenantId::new("acme").expect("tenant").scope();
    let chain = AuditChain::Tenant(tenant.clone());

    // Standard read event is batched inside the <= 100 ms window.
    let r1 = log
        .append_chain(
            &chain,
            &sample_entry(1_000_000, "discover.search", "crm.Order", "ALLOW"),
        )
        .expect("append standard 1");
    assert_eq!(r1.seq, 1);
    assert_eq!(r1.prev_hash, GENESIS_HASH);
    assert_eq!(log.pending_count(), 1);
    assert_eq!(log.sync_flush_count(), 0);
    // Logical head still reflects sequence 1 before batch flush.
    assert_eq!(log.chain_head(&chain).expect("head"), Some((1, r1.hash)));

    // Privileged mutation event flushes any queued standard event AND commits synchronously.
    clock.advance(Duration::from_millis(25));
    let r2 = log
        .append_chain(
            &chain,
            &sample_entry(1_000_025, "mutate.createOrder", "crm.Order/42", "ALLOW"),
        )
        .expect("append privileged 2");
    assert_eq!(r2.seq, 2);
    assert_eq!(r2.prev_hash, r1.hash);
    assert_eq!(log.pending_count(), 0);
    assert_eq!(log.sync_flush_count(), 1);

    // Another standard event waits while elapsed < 100 ms, then auto-flushes once elapsed >= 100 ms.
    let r3 = log
        .append_chain(
            &chain,
            &sample_entry(1_000_030, "execute.read", "crm.Order/42", "ALLOW"),
        )
        .expect("append standard 3");
    assert_eq!(r3.seq, 3);
    assert_eq!(r3.prev_hash, r2.hash);
    assert_eq!(log.pending_count(), 1);

    clock.advance(Duration::from_millis(100));
    let r4 = log
        .append_chain(
            &chain,
            &sample_entry(1_000_130, "execute.read", "crm.Order/43", "ALLOW"),
        )
        .expect("append standard 4 triggers 100ms batch flush");
    assert_eq!(r4.seq, 4);
    assert_eq!(r4.prev_hash, r3.hash);
    assert!(log.batched_flush_count() >= 1);

    let report = log.verify(&tenant, None).expect("verify full");
    assert_eq!(
        report,
        AuditVerifyReport::Intact {
            head_seq: 4,
            head_hash: r4.hash
        }
    );

    let listed = log.list_records(&chain, 1, 10).expect("list");
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0].entry.ts, r1.entry.ts);
}

// spec: TM-17, SEC-092, INV-14
#[test]
fn audit_tamper_modify_delete_reorder_and_corrupt_prev_pinpoint_exact_seq() {
    let acme = TenantId::new("acme").expect("tenant").scope();
    let chain = AuditChain::Tenant(acme);

    // 1. In-memory tamper scenarios (modify, delete, swap/reorder)
    let mem = InMemoryAuditLog::new();
    for seq in 1..=5_i64 {
        let entry = sample_entry(
            seq * 1000,
            "mutate.updateInvoice",
            &format!("inv/{seq}"),
            "ALLOW",
        );
        mem.append_chain(&chain, &entry).expect("mem append");
    }
    assert!(matches!(
        mem.verify_chain(&chain, None).expect("verify intact"),
        AuditVerifyReport::Intact { head_seq: 5, .. }
    ));

    mem.tamper_modify_entry(&chain, 3, "DENY");
    assert!(matches!(
        mem.verify_chain(&chain, None).expect("verify modified"),
        AuditVerifyReport::Inconsistent { seq: 3, .. }
    ));

    // Fresh in-memory chain for delete and swap checks
    let mem_del = InMemoryAuditLog::new();
    for seq in 1..=5_i64 {
        mem_del
            .append_chain(
                &chain,
                &sample_entry(seq * 1000, "mutate.op", "res", "ALLOW"),
            )
            .expect("append");
    }
    mem_del.tamper_delete_seq(&chain, 2);
    assert!(matches!(
        mem_del.verify_chain(&chain, None).expect("verify delete"),
        AuditVerifyReport::Inconsistent { seq: 2, .. }
    ));

    let mem_swap = InMemoryAuditLog::new();
    for seq in 1..=5_i64 {
        mem_swap
            .append_chain(
                &chain,
                &sample_entry(seq * 1000, "mutate.op", "res", "ALLOW"),
            )
            .expect("append");
    }
    mem_swap.tamper_swap_seqs(&chain, 2, 4);
    assert!(matches!(
        mem_swap.verify_chain(&chain, None).expect("verify swap"),
        AuditVerifyReport::Inconsistent { seq: 2, .. }
    ));

    // 2. Durable SQLite WAL tamper scenarios (modify, corrupt prev_hash, delete, reorder)
    let dir = tempdir().expect("tempdir");
    for scenario in ["modify", "corrupt_prev", "delete", "reorder"] {
        let db = SqliteAuditLog::open(SqliteAuditConfig::file(
            dir.path().join(format!("{scenario}.sqlite")),
        ))
        .expect("open sqlite");

        for seq in 1..=5_i64 {
            db.append_chain(
                &chain,
                &sample_entry(
                    seq * 1000,
                    "approval.decide",
                    &format!("req/{seq}"),
                    "ALLOW",
                ),
            )
            .expect("sqlite append");
        }
        assert!(matches!(
            db.verify_chain(&chain, None).expect("verify sqlite intact"),
            AuditVerifyReport::Intact { head_seq: 5, .. }
        ));

        match scenario {
            "modify" => {
                db.tamper_modify_decision(&chain, 4, "DENY")
                    .expect("tamper modify");
                assert!(matches!(
                    db.verify_chain(&chain, None).expect("verify"),
                    AuditVerifyReport::Inconsistent { seq: 4, .. }
                ));
            }
            "corrupt_prev" => {
                db.tamper_corrupt_prev_hash(&chain, 3, &[0xAA; 32])
                    .expect("tamper prev");
                assert!(matches!(
                    db.verify_chain(&chain, None).expect("verify"),
                    AuditVerifyReport::Inconsistent { seq: 3, .. }
                ));
            }
            "delete" => {
                db.tamper_delete_seq(&chain, 2).expect("tamper delete");
                assert!(matches!(
                    db.verify_chain(&chain, None).expect("verify"),
                    AuditVerifyReport::Inconsistent { seq: 2, .. }
                ));
            }
            "reorder" => {
                db.tamper_swap_seqs(&chain, 2, 3).expect("tamper reorder");
                assert!(matches!(
                    db.verify_chain(&chain, None).expect("verify"),
                    AuditVerifyReport::Inconsistent { seq: 2, .. }
                ));
            }
            _ => unreachable!(),
        }
    }
}

// spec: INV-09, SEC-090, SEC-092
#[test]
fn audit_tenant_and_system_chains_are_strictly_isolated_with_startup_and_nightly_verification() {
    let dir = tempdir().expect("tempdir");
    let db = SqliteAuditLog::open(SqliteAuditConfig::file(dir.path().join("multi.sqlite")))
        .expect("open sqlite");

    let acme = TenantId::new("acme").expect("tenant").scope();
    let beta = TenantId::new("beta").expect("tenant").scope();

    let s1 = db
        .append_chain(
            &AuditChain::System,
            &sample_entry(100, "system.startup", "gateway", "ALLOW"),
        )
        .expect("sys 1");
    let a1 = db
        .append(&acme, &sample_entry(200, "mutate.a", "acme/1", "ALLOW"))
        .expect("acme 1");
    let b1 = db
        .append(&beta, &sample_entry(300, "mutate.b", "beta/1", "DENY"))
        .expect("beta 1");

    // All three start at seq=1 with GENESIS prev_hash, and modifying beta never affects acme or system.
    assert_eq!(s1.seq, 1);
    assert_eq!(a1.seq, 1);
    assert_eq!(b1.seq, 1);
    assert_ne!(a1.hash, b1.hash);

    // Startup verification (`System` chain, last 10_000 records) passes.
    assert_eq!(
        db.verify_startup().expect("startup"),
        AuditVerifyReport::Intact {
            head_seq: 1,
            head_hash: s1.hash
        }
    );

    // Nightly verification verifies system + acme + beta independently.
    let nightly = db.verify_nightly_all().expect("nightly");
    assert_eq!(nightly.len(), 3);
    assert!(
        nightly
            .iter()
            .all(|(_, rep)| matches!(rep, AuditVerifyReport::Intact { head_seq: 1, .. }))
    );

    // Tamper only `beta` and assert `acme` + `System` remain intact while nightly catches `tenant:beta`.
    db.tamper_modify_decision(&AuditChain::Tenant(beta.clone()), 1, "ALLOW")
        .expect("tamper beta");
    assert!(matches!(
        db.verify_startup().expect("startup still ok"),
        AuditVerifyReport::Intact { .. }
    ));
    assert!(matches!(
        db.verify(&acme, None).expect("acme still ok"),
        AuditVerifyReport::Intact { .. }
    ));
    let nightly_after = db.verify_nightly_all().expect("nightly after");
    assert!(nightly_after.iter().any(|(key, rep)| {
        key == "tenant:beta" && matches!(rep, AuditVerifyReport::Inconsistent { seq: 1, .. })
    }));

    // Startup last-N window check (`Some(2)` on a 5-record chain anchors against seq 3).
    for seq in 2..=5_i64 {
        db.append_chain(
            &AuditChain::System,
            &sample_entry(100 + seq * 10, "system.reload", "gateway", "ALLOW"),
        )
        .expect("sys more");
    }
    assert!(matches!(
        db.verify_chain(&AuditChain::System, Some(2))
            .expect("last 2 ok"),
        AuditVerifyReport::Intact { head_seq: 5, .. }
    ));
}
