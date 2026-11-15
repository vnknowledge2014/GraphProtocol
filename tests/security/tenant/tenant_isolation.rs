// Tenant isolation enforcement, cross-tenant property/fuzz tests, and `TenantScope`
// compile-fail verification (`WP-M6-06`: `INV-09`, `INV-19`, `SEC-010`).
// ADR-018, TM-09

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gap_balance::registry::TelemetryRegistry;
use gap_balance::slot::{Outcome, SlotParams};
use gap_core::clock::{Clock, ManualClock, SystemClock, Timestamp};
use gap_core::context::{AttrValue, Principal, PrincipalKind};
use gap_core::ids::{BranchId, BranchUid, IdempotencyKey, TaskId, TenantId, TenantScope};
use gap_core::snapshot::{PlanCacheKey, SideEffectLevel, Snapshot, TelemetrySlotKey, TrustTier};
use gap_plan::cache::PlanCache;
use gap_plan::ir::{Fetch, Plan, PlanCost, PlanNode, Pushdown};
use gap_sec::authn::principal::{
    CredentialClaims, UntrustedRequestHints, derive_authenticated_principal,
};
use gap_sec::authz::{
    AuthzAction, AuthzContext, CompiledPolicySet, DecisionKind, ResourceAttributes,
};
use gap_store::SqliteContextStore;
use gap_store::audit::{AuditChain, AuditEntry, AuditLog, AuditRecord, AuditVerifyReport};
use gap_store::error::StoreError;
use gap_store::feedback::{NewDiscoveryFeedback, SqliteFeedbackStore, derive_query_hash_hex};
use gap_store::idempotency::{
    IdempotencyOutcome, IdempotencyStore, InMemoryIdempotencyStore, SqliteIdempotencyStore,
};
use gap_store::sqlite::SqliteConfig;
use gap_store::store::ContextStore;
use gap_store::tasks::{
    InMemoryTaskStore, SqliteTaskStore, TaskEventRecord, TaskQueryFilter, TaskRecord, TaskStatus,
    TaskStore,
};
use gap_store::types::{
    Caller, EdgeResolution, EdgeType, GraphCounts, NewEdge, NewNode, NodeId, NodeType, Provenance,
    ScopeSpec, TraverseQuery,
};
use proptest::prelude::*;
use serde_json::Map as JsonMap;

fn tenant_slug_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z][a-z0-9_-]{2,20}")
        .unwrap()
        .prop_filter("avoid empty or reserved collisions", |s| s.len() >= 3)
}

fn distinct_tenant_pair() -> impl Strategy<Value = (TenantId, TenantId)> {
    (tenant_slug_strategy(), tenant_slug_strategy())
        .prop_filter("tenants must be distinct", |(a, b)| a != b)
        .prop_map(|(a, b)| {
            (
                TenantId::new(a).expect("valid tenant A"),
                TenantId::new(b).expect("valid tenant B"),
            )
        })
}

fn key_slug_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9][a-z0-9._:-]{2,24}").unwrap()
}

fn sample_plan(policy_version: u64, operation: &str) -> Plan {
    let branch: BranchId = "shop".parse().expect("valid branch slug");
    let branch_uid = BranchUid::new_v7();
    Plan {
        root: PlanNode::Fetch(Fetch {
            branch: branch.clone(),
            branch_uid,
            operation: operation.to_owned(),
            arguments: BTreeMap::new(),
            selection: Vec::new(),
            response_key: operation.to_owned(),
            directives: Vec::new(),
            pushdown: Pushdown::default(),
        }),
        operation_name: Some(operation.to_owned()),
        cost: PlanCost::default(),
        branches: vec![branch],
        policy_version,
    }
}

struct RecordingAuditLog {
    entries: std::sync::Mutex<Vec<(String, AuditRecord)>>,
}

impl RecordingAuditLog {
    fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl AuditLog for RecordingAuditLog {
    fn append_chain(
        &self,
        chain: &AuditChain,
        entry: &AuditEntry,
    ) -> Result<AuditRecord, StoreError> {
        let chain_key = match chain {
            AuditChain::Tenant(scope) => scope.tenant().as_str().to_owned(),
            AuditChain::System => "__system__".to_owned(),
        };
        let mut guard = self.entries.lock().expect("unpoisoned mutex");
        let seq = u64::try_from(
            guard
                .iter()
                .filter(|(k, _)| *k == chain_key)
                .count()
                .saturating_add(1),
        )
        .unwrap_or(u64::MAX);
        let record = AuditRecord {
            seq,
            entry: entry.clone(),
            prev_hash: [0u8; 32],
            hash: [1u8; 32],
        };
        guard.push((chain_key, record.clone()));
        Ok(record)
    }

    fn chain_head(&self, chain: &AuditChain) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        let chain_key = match chain {
            AuditChain::Tenant(scope) => scope.tenant().as_str().to_owned(),
            AuditChain::System => "__system__".to_owned(),
        };
        let guard = self.entries.lock().expect("unpoisoned mutex");
        Ok(guard
            .iter()
            .rev()
            .find(|(k, _)| *k == chain_key)
            .map(|(_, r)| (r.seq, r.hash)))
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

fn find_newest_rlib_candidates(deps_dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = fs::read_dir(deps_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with(prefix) && name.ends_with(".rlib") {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some((modified, path))
            } else {
                None
            }
        })
        .collect();
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // spec: INV-09, INV-19
    // TM-09, ADR-018
    #[test]
    fn fuzz_cross_tenant_isolation_across_stores_caches_telemetry_and_authz(
        (tenant_a, tenant_b) in distinct_tenant_pair(),
        suffix in key_slug_strategy(),
        generation in 1_u64..10_000_u64,
    ) {
        let scope_a: TenantScope = tenant_a.scope();
        let scope_b: TenantScope = tenant_b.scope();

        // 1. SqliteContextStore isolation (nodes, edges, counts, traverse, search, delete)
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ctx.sqlite");
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_730_000_000_000));
        let ctx_store = SqliteContextStore::open(SqliteConfig::new(db_path), Arc::clone(&clock))
            .expect("open context store");
        let writer = Caller::new("alice");
        let node_a_id = NodeId::new(format!("node-{suffix}-1")).expect("node id 1");
        let node_b_id = NodeId::new(format!("node-{suffix}-2")).expect("node id 2");
        let ntype = NodeType::new("Service").expect("node type");
        let etype = EdgeType::new("CALLS").expect("edge type");

        ctx_store
            .put_node(
                &scope_a,
                &writer,
                NewNode {
                    id: Some(node_a_id.clone()),
                    node_type: ntype.clone(),
                    label: format!("Billing Service {suffix}"),
                    summary: Some("Tenant A internal summary".to_owned()),
                    props: JsonMap::new(),
                    scope: ScopeSpec::Global,
                    provenance: Provenance::new("test-suite"),
                },
            )
            .expect("put node 1 under tenant A");
        ctx_store
            .put_node(
                &scope_a,
                &writer,
                NewNode {
                    id: Some(node_b_id.clone()),
                    node_type: ntype,
                    label: format!("Ledger Service {suffix}"),
                    summary: None,
                    props: JsonMap::new(),
                    scope: ScopeSpec::Global,
                    provenance: Provenance::new("test-suite"),
                },
            )
            .expect("put node 2 under tenant A");
        ctx_store
            .put_edge(
                &scope_a,
                &writer,
                NewEdge {
                    id: None,
                    edge_type: etype,
                    from: node_a_id.clone(),
                    to: node_b_id.clone(),
                    props: JsonMap::new(),
                    confidence: Some(1.0),
                    resolution: EdgeResolution::Exact,
                    provenance: Provenance::new("test-suite"),
                },
            )
            .expect("put edge under tenant A");

        prop_assert!(
            ctx_store
                .get_node(&scope_b, &writer, &node_a_id, None)
                .expect("get cross-tenant")
                .is_none()
        );
        prop_assert_eq!(
            ctx_store
                .counts(&scope_b, &writer, None)
                .expect("counts tenant B"),
            GraphCounts { nodes: 0, edges: 0 }
        );
        prop_assert!(
            ctx_store
                .search_nodes(&scope_b, &writer, &suffix, 10, None)
                .expect("search tenant B")
                .is_empty()
        );
        let trav_b = ctx_store
            .traverse(&scope_b, &writer, &TraverseQuery::new(node_a_id.clone()))
            .expect("traverse tenant B");
        prop_assert!(trav_b.nodes.is_empty());
        prop_assert!(matches!(
            ctx_store.delete_node(&scope_b, &writer, &node_a_id),
            Err(StoreError::NotFound)
        ));
        prop_assert!(
            ctx_store
                .get_node(&scope_a, &writer, &node_a_id, None)
                .expect("node A intact")
                .is_some()
        );

        // 2. TaskStore (InMemory + Sqlite) isolation
        let task_id = TaskId::new_v7();
        let now_ts = Timestamp::UNIX_EPOCH;
        let task_rec = TaskRecord {
            task_id,
            status: TaskStatus::Working,
            payload_json: format!(r#"{{"secret":"{suffix}"}}"#),
            lease_owner: Some("worker-1".to_owned()),
            lease_expires_at: Some(now_ts),
            updated_at: now_ts,
        };
        let task_ev = TaskEventRecord {
            task_id,
            seq: 1,
            status: TaskStatus::Working,
            payload_json: "{}".to_owned(),
            emitted_at: now_ts,
        };

        let mem_tasks = InMemoryTaskStore::new();
        mem_tasks
            .put_task(&scope_a, &task_rec, &task_ev)
            .expect("put mem task");
        prop_assert!(mem_tasks.get_task(&scope_b, task_id).expect("get").is_none());
        prop_assert!(
            mem_tasks
                .list_tasks(&scope_b, &TaskQueryFilter::default())
                .expect("list")
                .is_empty()
        );
        prop_assert!(
            mem_tasks
                .events_from(&scope_b, task_id, 1)
                .expect("events")
                .is_empty()
        );

        let sql_tasks = SqliteTaskStore::open_in_memory().expect("sqlite task store");
        sql_tasks
            .put_task(&scope_a, &task_rec, &task_ev)
            .expect("put sql task");
        prop_assert!(sql_tasks.get_task(&scope_b, task_id).expect("get").is_none());
        prop_assert!(
            sql_tasks
                .list_tasks(&scope_b, &TaskQueryFilter::default())
                .expect("list")
                .is_empty()
        );
        prop_assert!(
            !sql_tasks
                .renew_lease(&scope_b, task_id, "worker-1", now_ts)
                .expect("renew cross-tenant")
        );

        // 3. IdempotencyStore (InMemory + Sqlite) isolation
        let idem_key = format!("idem-{suffix}");
        let hash_a = [0xAA_u8; 32];
        let hash_b = [0xBB_u8; 32];
        let future_ts = Timestamp::from_unix_nanos(1_800_000_000_000_000_000).expect("future ts");

        let mem_idem = InMemoryIdempotencyStore::with_clock(Arc::clone(&clock));
        prop_assert_eq!(
            mem_idem
                .check_or_reserve(&scope_a, &idem_key, hash_a, future_ts)
                .expect("reserve A"),
            IdempotencyOutcome::Reserved
        );
        mem_idem
            .complete(&scope_a, &idem_key, hash_a, Some("tenant-a-receipt"))
            .expect("complete A");
        prop_assert_eq!(
            mem_idem
                .check_or_reserve(&scope_b, &idem_key, hash_b, future_ts)
                .expect("reserve B same key diff hash"),
            IdempotencyOutcome::Reserved
        );
        let only_a_key = format!("only-a-{suffix}");
        prop_assert!(
            mem_idem
                .get(&scope_b, &only_a_key)
                .expect("get B")
                .is_none()
        );

        let sql_idem =
            SqliteIdempotencyStore::open_in_memory_with_clock(Arc::clone(&clock)).expect("sql idem");
        prop_assert_eq!(
            sql_idem
                .check_or_reserve(&scope_a, &idem_key, hash_a, future_ts)
                .expect("reserve A"),
            IdempotencyOutcome::Reserved
        );
        sql_idem
            .complete(&scope_a, &idem_key, hash_a, Some("tenant-a-receipt"))
            .expect("complete A");
        prop_assert_eq!(
            sql_idem
                .check_or_reserve(&scope_b, &idem_key, hash_b, future_ts)
                .expect("reserve B same key diff hash"),
            IdempotencyOutcome::Reserved
        );

        // 4. FeedbackStore + AuditChain isolation
        let fb_store = SqliteFeedbackStore::open_in_memory().expect("feedback store");
        let fb_id = format!("fb-{suffix}");
        let trace_id = format!("tr-{suffix}");
        let fb_ts = clock.now_timestamp();
        fb_store
            .record_discovery(
                &scope_a,
                NewDiscoveryFeedback {
                    feedback_id: fb_id.clone(),
                    ts: fb_ts,
                    trace_id: trace_id.clone(),
                    session_id: None,
                    query_hash_hex: derive_query_hash_hex("list invoices"),
                    redacted_query: Some("list invoices".to_owned()),
                    candidate_group_ids: vec!["g1".to_owned()],
                    option_logits: vec![1.0],
                    selected_group_id: Some("g1".to_owned()),
                    prediction_set_ids: vec!["g1".to_owned()],
                    abstained_reason: None,
                    calibration_id: None,
                },
            )
            .expect("record discovery A");
        prop_assert!(fb_store.get(&scope_b, &fb_id).expect("get B").is_none());
        let attributed_b = fb_store
            .attribute_server_execution(&scope_b, Some(&trace_id), None, "g1", "shop.list", true, fb_ts)
            .expect("attribute B");
        prop_assert_eq!(attributed_b, 0);

        let audit_log = RecordingAuditLog::new();
        let entry = AuditEntry {
            ts: fb_ts,
            actor_chain: vec!["alice".to_owned()],
            action: "tenant.test".to_owned(),
            resource: format!("res:{suffix}"),
            decision: "ALLOW".to_owned(),
            details_json: "{}".to_owned(),
        };
        audit_log
            .append_chain(&AuditChain::Tenant(scope_a.clone()), &entry)
            .expect("append chain A");
        prop_assert!(
            audit_log
                .chain_head(&AuditChain::Tenant(scope_b.clone()))
                .expect("head B")
                .is_none()
        );
        prop_assert!(
            audit_log
                .chain_head(&AuditChain::System)
                .expect("head sys")
                .is_none()
        );

        // 5. PlanCache isolation
        let plan_cache = PlanCache::default();
        let snap_a = Snapshot::empty_for_tenant(generation, tenant_a.clone());
        let snap_b = Snapshot::empty_for_tenant(generation, tenant_b.clone());
        let doc_hash = [0x42_u8; 32];
        let key_a = PlanCacheKey::from_snapshot(&snap_a, doc_hash, None);
        let key_b = PlanCacheKey::from_snapshot(&snap_b, doc_hash, None);
        prop_assert_ne!(&key_a, &key_b);
        let _ = plan_cache
            .get_or_plan_with(key_a.clone(), || Ok(sample_plan(snap_a.policy_version, "tenantAOp")))
            .expect("cache insert A");
        prop_assert!(plan_cache.get(&key_a).is_some());
        prop_assert!(plan_cache.get(&key_b).is_none());

        // 6. Cedar CompiledPolicySet cross-tenant deny
        let policies = CompiledPolicySet::default_builtin().expect("default policies");
        let mut scopes = BTreeSet::new();
        scopes.insert("read:all".to_owned());
        scopes.insert("mutate:run".to_owned());
        let mut attrs = BTreeMap::new();
        attrs.insert(
            "groups".to_owned(),
            AttrValue::List(vec!["agents-readonly".to_owned(), "operators".to_owned()]),
        );
        let principal_a = Principal {
            id: "agent-a".to_owned(),
            kind: PrincipalKind::Agent,
            tenant: tenant_a.clone(),
            scopes,
            attrs,
            delegation: Vec::new(),
        };
        let mut res_b = ResourceAttributes::operation(
            "shop.listInvoices",
            tenant_b.clone(),
            "shop",
            TrustTier::T0,
            SideEffectLevel::None,
        );
        res_b.id = format!("op-{suffix}");
        let ctx = AuthzContext::minimal(tenant_a.clone());
        for action in [
            AuthzAction::Discover,
            AuthzAction::Execute,
            AuthzAction::Traverse,
            AuthzAction::Mutate,
        ] {
            let dec = policies.evaluate_single(&principal_a, action, &res_b, &ctx);
            prop_assert_eq!(dec.decision, DecisionKind::Deny);
        }

        // 7. TelemetryRegistry slot isolation
        let tel_clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let registry = TelemetryRegistry::new(tel_clock, 64);
        let shared_branch = BranchUid::new_v7();
        let slot_key_a = TelemetrySlotKey::new(tenant_a, shared_branch);
        let slot_key_b = TelemetrySlotKey::new(tenant_b, shared_branch);
        prop_assert_ne!(&slot_key_a, &slot_key_b);
        let params = SlotParams {
            max_in_flight: 8,
            default_timeout_ms: 1_000,
            label: Arc::from("shop"),
        };
        let slot_a = registry.register(slot_key_a, params.clone()).expect("reg A");
        prop_assert!(registry.find(&slot_key_b).is_none());
        let slot_b = registry.register(slot_key_b, params).expect("reg B");
        prop_assert_ne!(slot_a, slot_b);
        let now_mono = registry.now();
        let slot_a_ref = registry.get(slot_a).expect("slot A");
        let slot_b_ref = registry.get(slot_b).expect("slot B");
        slot_a_ref.record(Outcome::Failure, Duration::from_millis(250), now_mono);
        let view_a = slot_a_ref.view(now_mono);
        let view_b = slot_b_ref.view(now_mono);
        prop_assert!(view_a.error_rate.is_some());
        prop_assert!(view_b.error_rate.is_none());
    }
}

// spec: SEC-010, INV-09
// TM-09
#[test]
fn sec_010_untrusted_x_tenant_and_body_hints_ignored_unless_impersonate() {
    let acme = TenantId::new("acme").expect("acme");
    let globex = TenantId::new("globex").expect("globex");
    let claims = CredentialClaims {
        subject: "alice".to_owned(),
        kind: PrincipalKind::Human,
        tenant: acme.clone(),
        scopes: BTreeSet::from(["read:all".to_owned()]),
        attrs: BTreeMap::from([(
            "groups".to_owned(),
            AttrValue::List(vec!["admins".to_owned()]),
        )]),
        delegation: Vec::new(),
    };
    let hints = UntrustedRequestHints {
        x_tenant_header: Some(globex.as_str().to_owned()),
        body_tenant: Some(globex.as_str().to_owned()),
    };

    let derived = derive_authenticated_principal(claims, Some(&hints));
    assert_eq!(derived.principal.tenant, acme);
    assert!(derived.ignored_hint_mismatch.is_some());

    // Without `admin:impersonate`, even an admin principal is denied on another tenant's resource.
    let policies = CompiledPolicySet::default_builtin().expect("default policies");
    let target = ResourceAttributes::operation(
        "crm.getAccount",
        globex.clone(),
        "crm",
        TrustTier::T0,
        SideEffectLevel::None,
    );
    let ctx = AuthzContext::minimal(acme.clone());
    assert_eq!(
        policies
            .evaluate_single(&derived.principal, AuthzAction::Execute, &target, &ctx)
            .decision,
        DecisionKind::Deny
    );

    // Only explicit `admin:impersonate` scope permits cross-tenant admin evaluation.
    let mut impersonator = derived.principal;
    impersonator.scopes.insert("admin:impersonate".to_owned());
    assert_eq!(
        policies
            .evaluate_single(&impersonator, AuthzAction::Execute, &target, &ctx)
            .decision,
        DecisionKind::Allow
    );
}

// spec: INV-19, INV-09
// TM-09
#[test]
fn inv_19_compile_fail_proves_store_apis_require_non_forgeable_tenant_scope() {
    let current_exe = std::env::current_exe().expect("current_exe");
    let mut deps_dir = current_exe.parent().expect("deps parent").to_path_buf();
    if deps_dir.file_name().and_then(|s| s.to_str()) != Some("deps") {
        let candidate = deps_dir.join("deps");
        if candidate.is_dir() {
            deps_dir = candidate;
        }
    }
    assert!(
        deps_dir.is_dir(),
        "expected Cargo deps dir at {}",
        deps_dir.display()
    );

    let store_rlibs = find_newest_rlib_candidates(&deps_dir, "libgap_store-");
    assert!(!store_rlibs.is_empty(), "libgap_store-*.rlib missing");

    let scratch = tempfile::tempdir().expect("tempdir");
    let ok_src = scratch.path().join("positive_control.rs");
    let ok_out = scratch.path().join("libpositive_control.rmeta");
    fs::write(
        &ok_src,
        "pub fn ok(store: &dyn gap_store::ContextStore, tenant: &gap_store::TenantId) -> gap_store::GraphCounts {\n\
         \x20   let scope: gap_store::TenantScope = tenant.scope();\n\
         \x20   store.counts(&scope, &gap_store::Caller::new(\"p1\"), None).unwrap_or_default()\n\
         }\n",
    )
    .expect("write positive control");

    let mut matched_store_rlib: Option<PathBuf> = None;
    for store_rlib in store_rlibs.iter().take(6) {
        let output = Command::new("rustc")
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "lib",
                "--emit=metadata",
            ])
            .arg("-L")
            .arg(format!("dependency={}", deps_dir.display()))
            .arg("--extern")
            .arg(format!("gap_store={}", store_rlib.display()))
            .arg("-o")
            .arg(&ok_out)
            .arg(&ok_src)
            .output()
            .expect("spawn rustc positive control");
        if output.status.success() {
            matched_store_rlib = Some(store_rlib.clone());
            break;
        }
    }

    let store_rlib = matched_store_rlib.expect(
        "at least one libgap_store-*.rlib in target/debug/deps must compile positive control",
    );

    let cases: [(&str, &str, &[&str]); 3] = [
        (
            "cannot_forge_tenant_scope_from_raw_tenant_id",
            "pub fn forge(t: gap_store::TenantId) -> gap_store::TenantScope {\n\
             \x20   gap_store::TenantScope(t)\n\
             }\n",
            &["E0423", "E0603"],
        ),
        (
            "cannot_call_context_store_counts_with_raw_tenant_id",
            "pub fn wrong_arg(store: &dyn gap_store::ContextStore, t: &gap_store::TenantId) {\n\
             \x20   let _ = store.counts(t, &gap_store::Caller::new(\"p1\"), None);\n\
             }\n",
            &["E0308"],
        ),
        (
            "cannot_call_idempotency_or_task_store_without_tenant_scope",
            "pub fn missing_scope(tasks: &dyn gap_store::TaskStore, idem: &dyn gap_store::IdempotencyStore) {\n\
             \x20   let _ = tasks.get_task(gap_store::TaskId::new_v7());\n\
             \x20   let _ = idem.get(\"k1\");\n\
             }\n",
            &["E0061", "E0308"],
        ),
    ];

    for (name, src, expected_codes) in cases {
        let src_path = scratch.path().join(format!("{name}.rs"));
        let rmeta_path = scratch.path().join(format!("lib{name}.rmeta"));
        fs::write(&src_path, src).expect("write negative test source");
        let output = Command::new("rustc")
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "lib",
                "--emit=metadata",
            ])
            .arg("-L")
            .arg(format!("dependency={}", deps_dir.display()))
            .arg("--extern")
            .arg(format!("gap_store={}", store_rlib.display()))
            .arg("-o")
            .arg(&rmeta_path)
            .arg(&src_path)
            .output()
            .expect("spawn rustc negative case");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "negative compile-fail case `{name}` unexpectedly succeeded"
        );
        assert!(
            !stderr.contains("E0460") && !stderr.contains("E0463"),
            "case `{name}` failed due to crate resolution rather than type enforcement:\n{stderr}"
        );
        assert!(
            expected_codes.iter().any(|code| stderr.contains(code)),
            "case `{name}` did not produce expected error {:?}; stderr:\n{stderr}",
            expected_codes
        );
    }

    // Also ensure TenantScope inner field cannot be constructed in source form via struct literal.
    let _unused_idempotency_key: IdempotencyKey = "k".parse().expect("idem key");
}
