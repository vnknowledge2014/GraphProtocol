//! `moka` `PlanCache` (`RUN-014`), snapshot generation pinning (`INV-01`), lock-free read path +
//! 64-thread coalescing (`INV-02`), tenant isolation (`INV-09`), TTL/byte-weight eviction, and
//! bounded hostile GraphQL documents (`GQL-040`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use common::{crm_uid, shop_uid, two_branch_snapshot, validate_op};
use gap_core::config::LimitsConfig;
use gap_core::error::ErrorCode;
use gap_core::ids::TenantId;
use gap_core::snapshot::{FieldCoordinate, FieldOwnership, PlanCacheKey};
use gap_plan::{
    PlanCache, PlanCacheConfig, PlanCacheStatus, PlanConfig, plan, plan_hash, plan_query,
};
use serde_json::json;

// spec: RUN-014, INV-01, INV-09, RUN-005
#[test]
fn run_014_inv_01_inv_09_plan_cache_hit_miss_and_key_invalidation_matrix() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();
    let cache = PlanCache::default();

    let query = r#"
        query GetOne($cid: ID!) { customer(id: $cid) { id name } }
        query GetTwo($cid: ID!) { customer(id: $cid) { id } order(id: "o1") { sku } }
    "#;
    let doc_hash = *blake3::hash(query.as_bytes()).as_bytes();

    let vars_1 = json!({ "cid": "c-1" });
    let vars_2 = json!({ "cid": "c-999" });
    let op_one_v1 = validate_op(&schema, query, Some("GetOne"), vars_1.as_object().unwrap());
    let op_one_v2 = validate_op(&schema, query, Some("GetOne"), vars_2.as_object().unwrap());
    let op_two = validate_op(&schema, query, Some("GetTwo"), vars_1.as_object().unwrap());

    // 1. Cold lookup -> MISS (`gap.plan_cache = "miss"`)
    assert_eq!(PlanCacheStatus::ATTRIBUTE_KEY, "gap.plan_cache");
    let first = cache
        .get_or_plan(&snap, doc_hash, &op_one_v1, &cfg)
        .expect("cold get_or_plan");
    assert_eq!(first.status, PlanCacheStatus::Miss);
    assert_eq!(first.status.as_str(), "miss");
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 0);

    // 2. Second lookup (even with different variable values!) -> HIT (`gap.plan_cache = "hit"`)
    let second = cache
        .get_or_plan(&snap, doc_hash, &op_one_v2, &cfg)
        .expect("warm get_or_plan");
    assert_eq!(second.status, PlanCacheStatus::Hit);
    assert_eq!(second.status.as_str(), "hit");
    assert!(Arc::ptr_eq(&first.plan, &second.plan));
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 1);

    // 3. Over-budget caller on a WARM cache key MUST still be rejected (`RUN-005`)!
    let tight_budget_cfg = PlanConfig {
        request_budget_cost_units: Some(1),
        ..PlanConfig::default()
    };
    let budget_err = cache
        .get_or_plan(&snap, doc_hash, &op_one_v2, &tight_budget_cfg)
        .expect_err("over-budget warm hit must still fail RUN-005");
    assert_eq!(budget_err.code(), ErrorCode::BudgetExceeded);

    // 4. Different `operation_name` (`GetTwo`) -> MISS
    let diff_op = cache
        .get_or_plan(&snap, doc_hash, &op_two, &cfg)
        .expect("different operation name");
    assert_eq!(diff_op.status, PlanCacheStatus::Miss);
    assert_eq!(cache.misses(), 2);

    // 5. Snapshot `generation` bump -> MISS (`INV-01`)
    let mut snap_gen2 = snap.clone();
    snap_gen2.generation = 2;
    let gen_miss = cache
        .get_or_plan(&snap_gen2, doc_hash, &op_one_v1, &cfg)
        .expect("generation bump replan");
    assert_eq!(gen_miss.status, PlanCacheStatus::Miss);
    assert_eq!(cache.misses(), 3);

    // 6. `policy_version` bump -> MISS and embeds updated `policy_version` (`RUN-014`)
    let mut snap_pol = snap.clone();
    snap_pol.policy_version = 8;
    let pol_miss = cache
        .get_or_plan(&snap_pol, doc_hash, &op_one_v1, &cfg)
        .expect("policy_version bump replan");
    assert_eq!(pol_miss.status, PlanCacheStatus::Miss);
    assert_eq!(pol_miss.plan.policy_version, 8);
    assert_eq!(cache.misses(), 4);

    // 7. Different `tenant_id` -> MISS (`INV-09`)
    let mut snap_tenant = snap.clone();
    snap_tenant.tenant_id = TenantId::new("globex").expect("globex");
    let tenant_miss = cache
        .get_or_plan(&snap_tenant, doc_hash, &op_one_v1, &cfg)
        .expect("different tenant replan");
    assert_eq!(tenant_miss.status, PlanCacheStatus::Miss);
    assert_eq!(cache.misses(), 5);

    // 8. Different `branch_set_hash` -> MISS (`ADR-009` §6)
    let mut snap_bset = snap.clone();
    snap_bset.branch_set_hash = [0xCD; 32];
    let bset_miss = cache
        .get_or_plan(&snap_bset, doc_hash, &op_one_v1, &cfg)
        .expect("different branch_set_hash replan");
    assert_eq!(bset_miss.status, PlanCacheStatus::Miss);
    assert_eq!(cache.misses(), 6);
}

// spec: INV-01, RUN-014
#[test]
fn inv_01_cached_plan_reflects_pinned_snapshot_generation_ownership() {
    let (snap_g1, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();
    let cache = PlanCache::default();

    // Build generation 2 where `crm` rotated its `BranchUid` while keeping slug `"crm"`.
    let rotated_crm_uid = gap_core::ids::BranchUid::from_unix_ms(1_700_000_009_999, [0x77; 10]);
    let mut snap_g2 = snap_g1.clone();
    snap_g2.generation = 2;
    let mut crm_desc = (**snap_g1.branches.get(&crm_uid()).expect("crm in g1")).clone();
    crm_desc.uid = rotated_crm_uid;
    snap_g2.branches.remove(&crm_uid());
    snap_g2.branches.insert(rotated_crm_uid, Arc::new(crm_desc));
    let mut owners_g2 = snap_g1.ownership.as_map().clone();
    owners_g2.insert(FieldCoordinate::new("Query", "lead"), rotated_crm_uid);
    snap_g2.ownership = Arc::new(FieldOwnership::new(owners_g2));

    let query = r#"{ lead(id: "L-1") { id email } }"#;
    let doc_hash = *blake3::hash(query.as_bytes()).as_bytes();
    let doc = validate_op(&schema, query, None, &serde_json::Map::new());

    let pinned_g1 = cache
        .get_or_plan(&snap_g1, doc_hash, &doc, &cfg)
        .expect("g1 plan");
    let pinned_g2 = cache
        .get_or_plan(&snap_g2, doc_hash, &doc, &cfg)
        .expect("g2 plan");

    assert_ne!(plan_hash(&pinned_g1.plan), plan_hash(&pinned_g2.plan));
    // Re-querying g1 while g2 is active still serves g1's exact cached plan (`INV-01`).
    let pinned_g1_again = cache
        .get_or_plan(&snap_g1, doc_hash, &doc, &cfg)
        .expect("g1 warm hit");
    assert_eq!(pinned_g1_again.status, PlanCacheStatus::Hit);
    assert!(Arc::ptr_eq(&pinned_g1.plan, &pinned_g1_again.plan));
}

// spec: INV-09, RUN-014
#[test]
fn inv_09_plan_cache_isolates_tenants_with_identical_documents_and_generations() {
    let (mut snap_acme, schema) = two_branch_snapshot();
    snap_acme.tenant_id = TenantId::new("acme").expect("acme");

    let mut snap_globex = snap_acme.clone();
    snap_globex.tenant_id = TenantId::new("globex").expect("globex");
    // Give globex a distinct `BranchUid` for `shop` at the exact same generation & branch_set_hash
    // to prove tenant isolation even if every non-tenant key field matches!
    let globex_shop_uid = gap_core::ids::BranchUid::from_unix_ms(1_700_000_055_555, [0x55; 10]);
    let mut globex_shop = (**snap_acme.branches.get(&shop_uid()).expect("shop")).clone();
    globex_shop.uid = globex_shop_uid;
    snap_globex.branches.remove(&shop_uid());
    snap_globex
        .branches
        .insert(globex_shop_uid, Arc::new(globex_shop));
    let mut globex_owners = snap_acme.ownership.as_map().clone();
    globex_owners.insert(FieldCoordinate::new("Query", "customer"), globex_shop_uid);
    snap_globex.ownership = Arc::new(FieldOwnership::new(globex_owners));

    let cache = PlanCache::default();
    let cfg = PlanConfig::default();
    let query = r#"{ customer(id: "c-1") { id name } }"#;
    let doc_hash = *blake3::hash(query.as_bytes()).as_bytes();
    let doc = validate_op(&schema, query, None, &serde_json::Map::new());

    let p_acme = cache
        .get_or_plan(&snap_acme, doc_hash, &doc, &cfg)
        .expect("acme");
    let p_globex = cache
        .get_or_plan(&snap_globex, doc_hash, &doc, &cfg)
        .expect("globex");
    assert_eq!(p_acme.status, PlanCacheStatus::Miss);
    assert_eq!(p_globex.status, PlanCacheStatus::Miss);
    assert!(!Arc::ptr_eq(&p_acme.plan, &p_globex.plan));
    assert_ne!(p_acme.plan, p_globex.plan);
}

// spec: RUN-014
#[test]
fn run_014_plan_cache_ttl_eviction_and_byte_weight_eviction_trigger_replan() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    // 1. Short TTL (`20 ms`) eviction + replan (`sleep <= 35 ms`, obeying AGENT-GUIDE §5.4 <= 50 ms)
    let ttl_cache = PlanCache::new(PlanCacheConfig {
        max_entries: 100,
        ttl: Duration::from_millis(20),
        max_weight_bytes: None,
    });
    let query = r#"{ customer(id: "c-1") { id name } }"#;
    let doc = validate_op(&schema, query, None, &serde_json::Map::new());
    let key = PlanCacheKey::from_snapshot(&snap, *blake3::hash(query.as_bytes()).as_bytes(), None);
    let compile_calls = AtomicUsize::new(0);

    let (_, st1) = ttl_cache
        .get_or_plan_with(key.clone(), || {
            compile_calls.fetch_add(1, Ordering::SeqCst);
            plan(&snap, &doc, &cfg)
        })
        .expect("first plan");
    assert_eq!(st1, PlanCacheStatus::Miss);
    assert_eq!(compile_calls.load(Ordering::SeqCst), 1);

    thread::sleep(Duration::from_millis(35));
    ttl_cache.run_pending_tasks();

    let (_, st2) = ttl_cache
        .get_or_plan_with(key, || {
            compile_calls.fetch_add(1, Ordering::SeqCst);
            plan(&snap, &doc, &cfg)
        })
        .expect("replanned after TTL expiry");
    assert_eq!(st2, PlanCacheStatus::Miss);
    assert_eq!(compile_calls.load(Ordering::SeqCst), 2);

    // 2. Byte-weight eviction (`max_weight_bytes: Some(900)`)
    let tiny_byte_cache = PlanCache::new(PlanCacheConfig {
        max_entries: 100,
        ttl: Duration::from_secs(3_600),
        max_weight_bytes: Some(900),
    });
    for i in 0_u8..12 {
        let k = PlanCacheKey::from_snapshot(&snap, [i; 32], None);
        let _ = tiny_byte_cache
            .get_or_plan_with(k, || plan(&snap, &doc, &cfg))
            .expect("insert weighted plan");
    }
    tiny_byte_cache.run_pending_tasks();
    assert!(
        tiny_byte_cache.weighted_size() <= 900,
        "weighted_size {} exceeded cap 900",
        tiny_byte_cache.weighted_size()
    );
    assert!(
        tiny_byte_cache.entry_count() < 12,
        "expected byte weigher to evict entries, got {}",
        tiny_byte_cache.entry_count()
    );
}

fn scan_rust_files_for_locks(dir: &Path, offenders: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            scan_rust_files_for_locks(&path, offenders);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let content = fs::read_to_string(&path).expect("read rs file");
            for (line_no, raw_line) in content.lines().enumerate() {
                let code_part = raw_line.split("//").next().unwrap_or("");
                for forbidden in ["Mutex", "RwLock", "DashMap", "parking_lot"] {
                    if code_part.contains(forbidden) {
                        offenders.push(format!("{}:{}: {forbidden}", path.display(), line_no + 1));
                    }
                }
            }
        }
    }
}

// spec: INV-02, RUN-014
#[test]
fn inv_02_no_lock_on_read_path_static_and_64_thread_coalescing_stress() {
    // 1. Static source check: zero Mutex / RwLock / DashMap / parking_lot in `crates/gap-plan/src/`.
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    scan_rust_files_for_locks(&src_dir, &mut offenders);
    assert!(
        offenders.is_empty(),
        "INV-02 violation: lock primitives found in gap-plan/src: {offenders:?}"
    );

    // 2. 64-thread concurrent cold-key coalescing + warm-read stress (`AGENT-GUIDE` §5.4 >= 64 threads).
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();
    let query = r#"{ customer(id: "c-1") { id name } lead(id: "l-1") { id email } }"#;
    let doc = Arc::new(validate_op(&schema, query, None, &serde_json::Map::new()));
    let snap = Arc::new(snap);
    let cache = Arc::new(PlanCache::default());
    let key = PlanCacheKey::from_snapshot(&snap, *blake3::hash(query.as_bytes()).as_bytes(), None);
    let init_calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(64));

    let mut handles = Vec::with_capacity(64);
    for _ in 0..64 {
        let cache = Arc::clone(&cache);
        let snap = Arc::clone(&snap);
        let doc = Arc::clone(&doc);
        let cfg = cfg.clone();
        let key = key.clone();
        let init_calls = Arc::clone(&init_calls);
        let barrier = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            barrier.wait();
            let (cold_plan, status) = cache
                .get_or_plan_with(key.clone(), || {
                    init_calls.fetch_add(1, Ordering::SeqCst);
                    plan(&snap, &doc, &cfg)
                })
                .expect("coalesced cold get_or_plan_with");

            // Follow immediately with 100 warm reads on the same key.
            for _ in 0..100 {
                let (warm_plan, warm_status) = cache
                    .get_or_plan_with(key.clone(), || {
                        init_calls.fetch_add(1, Ordering::SeqCst);
                        plan(&snap, &doc, &cfg)
                    })
                    .expect("warm get_or_plan_with");
                assert_eq!(warm_status, PlanCacheStatus::Hit);
                assert!(Arc::ptr_eq(&cold_plan, &warm_plan));
            }
            (cold_plan, status)
        }));
    }

    let mut miss_winners = 0_usize;
    let mut first_arc: Option<Arc<gap_plan::Plan>> = None;
    for h in handles {
        let (p, status) = h.join().expect("worker thread panicked");
        if status == PlanCacheStatus::Miss {
            miss_winners += 1;
        }
        if let Some(ref base) = first_arc {
            assert!(Arc::ptr_eq(base, &p));
        } else {
            first_arc = Some(p);
        }
    }

    // `moka::sync::Cache::entry(...).or_try_insert_with(...)` guarantees exact single-evaluation coalescing!
    assert_eq!(
        init_calls.load(Ordering::SeqCst),
        1,
        "moka must coalesce 64 concurrent misses into 1 init"
    );
    assert_eq!(miss_winners, 1);
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 63 + 64 * 100);
}

// spec: GQL-040, RUN-013
#[test]
fn gql_040_hostile_documents_deep_nesting_10000_root_fields_and_1mib_alias_end_bounded() {
    let (snap, schema) = two_branch_snapshot();

    // 1. Deep nesting right at `max_selection_depth = 12` succeeds; depth `13` is rejected.
    // Depth 1: `customer` -> Depth 2..=11: 10 nested `friends(first: 1)` -> Depth 12: `id`
    let mut deep_12 = String::from(r#"{ customer(id: "c-1") { "#);
    for _ in 0..10 {
        deep_12.push_str("friends(first: 1) { ");
    }
    deep_12.push_str("id ");
    for _ in 0..10 {
        deep_12.push_str("} ");
    }
    deep_12.push('}');

    let plan_12 = plan_query(
        &snap,
        &deep_12,
        None,
        &serde_json::Map::new(),
        &PlanConfig::default(),
    )
    .expect("depth 12 at limit must succeed");
    assert_eq!(plan_hash(&plan_12).len(), 64);

    let mut deep_13 = String::from(r#"{ customer(id: "c-1") { "#);
    for _ in 0..11 {
        deep_13.push_str("friends(first: 1) { ");
    }
    deep_13.push_str("id ");
    for _ in 0..11 {
        deep_13.push_str("} ");
    }
    deep_13.push('}');
    let depth_err = plan_query(
        &snap,
        &deep_13,
        None,
        &serde_json::Map::new(),
        &PlanConfig::default(),
    )
    .expect_err("depth 13 > 12 must fail");
    assert_eq!(depth_err.code(), ErrorCode::DepthExceeded);

    // 2. 10,000 aliased root fields (`f0000`..`f9999`) complete bounded and sorted deterministically.
    let mut ten_k = String::with_capacity(140_000);
    ten_k.push('{');
    for i in (0..10_000).rev() {
        write!(&mut ten_k, " f{i:04}: ping").unwrap();
    }
    ten_k.push_str(" }");

    let wide_cfg = PlanConfig {
        limits: LimitsConfig {
            max_root_fields: 10_000,
            max_aliases: 10_000,
            max_cost_units: 100_000,
            max_request_body_bytes: 4 * 1024 * 1024,
            ..LimitsConfig::default()
        },
        ..PlanConfig::default()
    };
    let wide_doc = validate_op(&schema, &ten_k, None, &serde_json::Map::new());
    let wide_plan =
        plan(&snap, &wide_doc, &wide_cfg).expect("10,000 root fields must plan bounded");
    let gap_plan::PlanNode::Parallel { nodes } = &wide_plan.root else {
        panic!("expected Parallel for 10,000 root fields");
    };
    assert_eq!(nodes.len(), 10_000);
    if let (Some(gap_plan::PlanNode::Fetch(first)), Some(gap_plan::PlanNode::Fetch(last))) =
        (nodes.first(), nodes.last())
    {
        assert_eq!(first.response_key, "f0000");
        assert_eq!(last.response_key, "f9999");
    } else {
        panic!("expected Fetch nodes");
    }

    // 3. 1 MiB (`1_048_576`-byte) alias plans and hashes bounded without overflow.
    let big_alias = "a".repeat(1_048_576);
    let big_alias_query = format!(r#"{{ {big_alias}: order(id: "o1") {{ id }} }}"#);
    let big_cfg = PlanConfig {
        limits: LimitsConfig {
            max_request_body_bytes: 4 * 1024 * 1024,
            ..LimitsConfig::default()
        },
        ..PlanConfig::default()
    };
    // Note: `apollo-compiler` bounded_parser caps tokens, not single identifier length; validate directly.
    let big_doc = validate_op(&schema, &big_alias_query, None, &serde_json::Map::new());
    let big_plan = plan(&snap, &big_doc, &big_cfg).expect("1 MiB alias must plan bounded");
    let gap_plan::PlanNode::Fetch(big_fetch) = &big_plan.root else {
        panic!("expected single Fetch for 1 MiB alias");
    };
    assert_eq!(big_fetch.response_key.len(), 1_048_576);
    assert_eq!(plan_hash(&big_plan).len(), 64);
}
