//! Unit tests for `gap-sec::quota` (`SEC-050`, `RUN-050`, `INV-09`, `TM-11`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_core::error::ErrorCode;
use gap_core::ids::{BranchId, TenantId};

use super::*;

// spec: RUN-050, INV-09, TM-11
#[test]
fn quota_token_bucket_rate_and_cost_unit_budgets_enforce_and_reconcile() {
    let clock = Arc::new(ManualClock::new(1_000_000));
    let enforcer =
        QuotaEnforcer::with_clock(QuotaConfig::default(), Arc::clone(&clock) as Arc<dyn Clock>);
    let acme = TenantId::new("acme").expect("acme");
    let beta = TenantId::new("beta").expect("beta");

    // 1. Default burst = 100, steady rate = 50 rps.
    for _ in 0..100 {
        enforcer
            .check_rate_limit(&acme, "agent:alice")
            .expect("burst request allowed");
    }
    let over = enforcer
        .check_rate_limit(&acme, "agent:alice")
        .expect_err("101st burst rejected");
    assert_eq!(over.code(), ErrorCode::RateLimited);
    assert!(over.retry().retryable);
    assert_eq!(over.retry().after_ms, Some(20));

    // Different principal OR different tenant keeps independent bucket (`INV-09`).
    enforcer
        .check_rate_limit(&acme, "agent:bob")
        .expect("different principal ok");
    enforcer
        .check_rate_limit(&beta, "agent:alice")
        .expect("different tenant ok");

    // Advancing 100 ms refills 5 tokens (50 rps * 0.1 s = 5).
    clock.advance(Duration::from_millis(100));
    for _ in 0..5 {
        enforcer
            .check_rate_limit(&acme, "agent:alice")
            .expect("refilled request ok");
    }
    assert_eq!(
        enforcer
            .check_rate_limit(&acme, "agent:alice")
            .expect_err("6th after 100ms rejected")
            .code(),
        ErrorCode::RateLimited
    );

    // 2. Cost-unit quota = 600_000 units/min with reserve + reconcile refund.
    let res1 = enforcer
        .reserve_cost(&acme, "agent:alice", 400_000)
        .expect("400k reserved");
    assert_eq!(enforcer.remaining_cost_units(&acme, "agent:alice"), 200_000);

    let over_cost = enforcer
        .reserve_cost(&acme, "agent:alice", 250_000)
        .expect_err("400k + 250k exceeds 600k");
    assert_eq!(over_cost.code(), ErrorCode::RateLimited);
    assert!(over_cost.retry().after_ms.is_some());

    // Reconcile actual cost of 100_000 -> refunds 300_000 -> 500_000 remaining!
    enforcer.reconcile_cost(res1, 100_000);
    assert_eq!(enforcer.remaining_cost_units(&acme, "agent:alice"), 500_000);
    let _res2 = enforcer
        .reserve_cost(&acme, "agent:alice", 250_000)
        .expect("250k fits after refund");

    // Next minute window resets cost budget to 600_000.
    clock.advance(Duration::from_secs(60));
    assert_eq!(enforcer.remaining_cost_units(&acme, "agent:alice"), 600_000);

    let metrics = enforcer.metrics();
    assert_eq!(metrics.rate_allowed, 107);
    assert_eq!(metrics.rate_rejected, 2);
    assert_eq!(metrics.cost_allowed, 2);
    assert_eq!(metrics.cost_rejected, 1);
    assert_eq!(metrics.cost_units_reserved, 650_000);
    assert_eq!(metrics.cost_units_refunded, 300_000);
}

// spec: SEC-050, INV-09
#[test]
fn quota_branch_share_ingest_storage_and_s1_budgets_enforce_tenant_isolation() {
    let clock = Arc::new(ManualClock::new(2_000_000));
    let config = QuotaConfig {
        max_ingest_concurrency_per_tenant: 2,
        max_store_rows_per_tenant: 1_000,
        max_blob_bytes_per_tenant: 4_096,
        max_s1_calls_per_min_per_tenant: 3,
        ..QuotaConfig::default()
    };
    let enforcer = QuotaEnforcer::with_clock(config, Arc::clone(&clock) as Arc<dyn Clock>);

    let acme = TenantId::new("acme").expect("acme");
    let beta = TenantId::new("beta").expect("beta");
    let branch = BranchId::new("billing").expect("billing");

    // 1. Branch capacity = 6 -> default 50% share gives acme 3 slots, leaving 3 for beta (`SEC-050`).
    let a1 = enforcer
        .acquire_branch_slot(&acme, &branch, 6)
        .expect("acme slot 1");
    let a2 = enforcer
        .acquire_branch_slot(&acme, &branch, 6)
        .expect("acme slot 2");
    let a3 = enforcer
        .acquire_branch_slot(&acme, &branch, 6)
        .expect("acme slot 3");
    assert_eq!(enforcer.active_branch_tenant_slots(&acme, &branch), 3);

    let a4 = enforcer
        .acquire_branch_slot(&acme, &branch, 6)
        .expect_err("acme 4th exceeds 50% share of 6");
    assert_eq!(a4.code(), ErrorCode::RateLimited);

    // Beta is completely unblocked on its own 50% share (`SEC-050`, `INV-09`).
    let _b1 = enforcer
        .acquire_branch_slot(&beta, &branch, 6)
        .expect("beta slot 1 allowed despite acme hitting 50% cap");

    // RAII drop frees acme's slot cleanly.
    drop(a1);
    assert_eq!(enforcer.active_branch_tenant_slots(&acme, &branch), 2);
    let _a_reacquired = enforcer
        .acquire_branch_slot(&acme, &branch, 6)
        .expect("slot released on drop");
    drop(a2);
    drop(a3);

    // 2. Per-tenant ingestion concurrency (cap = 2) with RAII drop.
    let i1 = enforcer.acquire_ingest_slot(&acme).expect("ingest 1");
    let _i2 = enforcer.acquire_ingest_slot(&acme).expect("ingest 2");
    assert_eq!(
        enforcer
            .acquire_ingest_slot(&acme)
            .expect_err("ingest 3 rejected")
            .code(),
        ErrorCode::RateLimited
    );
    enforcer
        .acquire_ingest_slot(&beta)
        .expect("beta ingest unaffected");
    drop(i1);
    assert_eq!(enforcer.active_ingest_slots(&acme), 1);

    // 3. Per-tenant storage rows + blob bytes ceilings (`SEC-050`).
    let usage = enforcer
        .allocate_storage(&acme, 600, 2_048)
        .expect("alloc 1");
    assert_eq!(
        usage,
        TenantStorageUsage {
            rows: 600,
            blob_bytes: 2_048
        }
    );
    assert_eq!(
        enforcer
            .allocate_storage(&acme, 500, 0)
            .expect_err("row ceiling exceeded")
            .code(),
        ErrorCode::RateLimited
    );
    assert_eq!(
        enforcer
            .allocate_storage(&acme, 0, 2_049)
            .expect_err("blob ceiling exceeded")
            .code(),
        ErrorCode::RateLimited
    );
    enforcer.release_storage(&acme, 200, 1_024);
    assert_eq!(
        enforcer.storage_usage(&acme),
        TenantStorageUsage {
            rows: 400,
            blob_bytes: 1_024
        }
    );

    // 4. Per-tenant S1 calls/min budget (cap = 3) with window reset.
    for _ in 0..3 {
        enforcer.acquire_s1_call(&acme).expect("s1 call ok");
    }
    let s1_err = enforcer
        .acquire_s1_call(&acme)
        .expect_err("4th s1 call rejected");
    assert_eq!(s1_err.code(), ErrorCode::RateLimited);
    assert_eq!(s1_err.retry().after_ms, Some(60_000));

    enforcer
        .acquire_s1_call(&beta)
        .expect("beta s1 budget unaffected");
    clock.advance(Duration::from_secs(60));
    enforcer
        .acquire_s1_call(&acme)
        .expect("acme s1 resets next minute");

    let snap = enforcer.metrics();
    assert_eq!(snap.branch_share_allowed, 5);
    assert_eq!(snap.branch_share_rejected, 1);
    assert_eq!(snap.ingest_allowed, 3);
    assert_eq!(snap.ingest_rejected, 1);
    assert_eq!(snap.storage_allowed, 1);
    assert_eq!(snap.storage_rejected, 2);
    assert_eq!(snap.s1_allowed, 5);
    assert_eq!(snap.s1_rejected, 1);
}
