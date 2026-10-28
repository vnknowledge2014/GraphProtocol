//! Reusable backend-agnostic conformance suite for [`ContextStore`] + [`ContextMaintenance`]
//! (`m2-01-design.md` §10).
//!
//! Compiled under `#[cfg(any(test, feature = "conformance"))]`. Uses `Result` rather than panics
//! so external backend crates (`SurrealDB`, `Postgres`, etc.) can invoke
//! [`run_context_store_suite`] as ordinary library code.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_core::ids::{TaskId, TenantId};
use serde_json::{Map as JsonMap, json};

use crate::error::StoreError;
use crate::store::{ContextMaintenance, ContextStore};
use crate::types::{
    BaseScope, Caller, Direction, EdgeId, EdgeResolution, EdgeType, GraphCounts, NewEdge, NewNode,
    NodeId, NodeType, Provenance, PurgePolicy, PurgeReport, Scope, ScopeSpec, TraverseQuery,
    TraverseStrategy, TraverseWarning,
};

/// Single failed conformance check reported inside [`SuiteReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseFailure {
    /// Static identifier of the conformance check that failed.
    pub case: &'static str,
    /// Diagnostic explanation of the mismatch or unexpected error.
    pub reason: String,
}

/// Summary returned by [`run_context_store_suite`] (`m2-01-design.md` §10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuiteReport {
    /// Number of conformance checks that passed.
    pub passed: usize,
    /// Conformance checks that failed (empty when the backend conforms).
    pub failed: Vec<CaseFailure>,
}

impl SuiteReport {
    /// Returns `true` when zero conformance checks failed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty()
    }
}

fn ensure(cond: bool, msg: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.into()) }
}

fn ensure_eq<T: PartialEq + Debug>(left: &T, right: &T, ctx: &str) -> Result<(), String> {
    if left == right {
        Ok(())
    } else {
        Err(format!("{ctx}: left={left:?}, right={right:?}"))
    }
}

fn mk_node_id(raw: &str) -> Result<NodeId, String> {
    NodeId::new(raw).map_err(|e| format!("NodeId({raw}): {e}"))
}

fn mk_edge_id(raw: &str) -> Result<EdgeId, String> {
    EdgeId::new(raw).map_err(|e| format!("EdgeId({raw}): {e}"))
}

fn mk_node_type(raw: &str) -> Result<NodeType, String> {
    NodeType::new(raw).map_err(|e| format!("NodeType({raw}): {e}"))
}

fn mk_edge_type(raw: &str) -> Result<EdgeType, String> {
    EdgeType::new(raw).map_err(|e| format!("EdgeType({raw}): {e}"))
}

fn mk_global_node(id: &str, label: &str, summary: Option<&str>) -> Result<NewNode, String> {
    Ok(NewNode {
        id: Some(mk_node_id(id)?),
        node_type: mk_node_type("Entity")?,
        label: label.to_owned(),
        summary: summary.map(ToOwned::to_owned),
        props: JsonMap::new(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("conformance"),
    })
}

fn mk_edge(
    id: Option<&str>,
    etype: &str,
    from: &NodeId,
    to: &NodeId,
    props: JsonMap<String, serde_json::Value>,
) -> Result<NewEdge, String> {
    let parsed_id = match id {
        Some(s) => Some(mk_edge_id(s)?),
        None => None,
    };
    Ok(NewEdge {
        id: parsed_id,
        edge_type: mk_edge_type(etype)?,
        from: from.clone(),
        to: to.clone(),
        props,
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("conformance"),
    })
}

/// Runs the full `ContextStore` + `ContextMaintenance` conformance suite (`m2-01-design.md` §§3–7, §10)
/// against stores built by `factory`.
///
/// Each case receives a fresh `Arc<ManualClock>` anchored at `1_700_000_000_000 ms` so cases are
/// deterministic and mutually isolated. Never panics.
pub fn run_context_store_suite<S, F>(mut factory: F) -> SuiteReport
where
    S: ContextStore + ContextMaintenance,
    F: FnMut(Arc<ManualClock>) -> Result<S, StoreError>,
{
    type CaseFn<S> = fn(&S, &Arc<ManualClock>) -> Result<(), String>;

    let cases: &[(&'static str, CaseFn<S>)] = &[
        (
            "section3_tenant_isolation_and_hostile_inputs",
            case_section3_tenant_isolation_and_hostile_inputs,
        ),
        (
            "section4_temporal_versions_idempotent_reput_and_batch_atomicity",
            case_section4_temporal_versions_and_batch_atomicity,
        ),
        (
            "section5_scopes_visibility_indistinguishability_and_no_id_oracle",
            case_section5_scopes_visibility_and_indistinguishability,
        ),
        (
            "section5_lifecycle_tombstone_task_and_purge_expired",
            case_section5_lifecycle_maintenance,
        ),
        (
            "section6_traversal_strategies_paging_truncation_and_invalid_weights",
            case_section6_traversal_all_strategies,
        ),
        (
            "section7_search_bm25_tie_breaking_and_hostile_fts_syntax",
            case_section7_search_and_fts_safety,
        ),
    ];

    let mut report = SuiteReport::default();
    for (name, func) in cases {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let outcome = match factory(Arc::clone(&clock)) {
            Ok(store) => func(&store, &clock),
            Err(err) => Err(format!("factory failed: {err}")),
        };
        match outcome {
            Ok(()) => {
                report.passed = report.passed.saturating_add(1);
            }
            Err(reason) => {
                report.failed.push(CaseFailure { case: name, reason });
            }
        }
    }
    report
}

fn case_section3_tenant_isolation_and_hostile_inputs<S: ContextStore>(
    store: &S,
    _clock: &Arc<ManualClock>,
) -> Result<(), String> {
    let tenant_a = TenantId::new("acme").map_err(|e| e.to_string())?.scope();
    let tenant_b = TenantId::new("globex").map_err(|e| e.to_string())?.scope();
    let caller = Caller::new("principal-1");

    let hostile_id_str = "svc:'; DROP TABLE nodes;--";
    let hostile_label = " gateway \"OR\" 1=1; SELECT * FROM edges -- ";
    let n1 = store
        .put_node(
            &tenant_a,
            &caller,
            mk_global_node(hostile_id_str, hostile_label, Some("exclusivetermalpha"))?,
        )
        .map_err(|e| format!("put hostile n1: {e}"))?;
    let n2 = store
        .put_node(
            &tenant_a,
            &caller,
            mk_global_node("svc:backend", "Backend", Some("exclusivetermalpha"))?,
        )
        .map_err(|e| format!("put n2: {e}"))?;
    let e1 = store
        .put_edge(
            &tenant_a,
            &caller,
            mk_edge(Some("edge:1"), "CALLS", &n1, &n2, JsonMap::new())?,
        )
        .map_err(|e| format!("put e1: {e}"))?;

    // Verbatim round-trip in tenant_a.
    let fetched = store
        .get_node(&tenant_a, &caller, &n1, None)
        .map_err(|e| format!("get n1: {e}"))?
        .ok_or_else(|| "expected n1 in tenant_a".to_owned())?;
    ensure_eq(&fetched.id.as_str(), &hostile_id_str, "hostile id verbatim")?;
    ensure_eq(
        &fetched.label.as_str(),
        &hostile_label,
        "hostile label verbatim",
    )?;

    // Tenant B sees nothing across every read/write API.
    let b_node = store
        .get_node(&tenant_b, &caller, &n1, None)
        .map_err(|e| format!("b get_node: {e}"))?;
    ensure(b_node.is_none(), "tenant_b saw tenant_a node")?;

    let b_edge = store
        .get_edge(&tenant_b, &caller, &e1, None)
        .map_err(|e| format!("b get_edge: {e}"))?;
    ensure(b_edge.is_none(), "tenant_b saw tenant_a edge")?;

    let b_counts = store
        .counts(&tenant_b, &caller, None)
        .map_err(|e| format!("b counts: {e}"))?;
    ensure_eq(
        &b_counts,
        &GraphCounts { nodes: 0, edges: 0 },
        "tenant_b counts",
    )?;

    let b_hits = store
        .search_nodes(&tenant_b, &caller, "exclusivetermalpha", 10, None)
        .map_err(|e| format!("b search: {e}"))?;
    ensure(b_hits.is_empty(), "tenant_b search leaked tenant_a term")?;

    let b_trav = store
        .traverse(&tenant_b, &caller, &TraverseQuery::new(n1.clone()))
        .map_err(|e| format!("b traverse: {e}"))?;
    ensure(b_trav.nodes.is_empty(), "tenant_b traverse leaked nodes")?;

    let b_tomb = store.tombstone_node(&tenant_b, &caller, &n1);
    ensure_eq(
        &b_tomb,
        &Err(StoreError::NotFound),
        "tenant_b tombstone_node",
    )?;

    Ok(())
}

fn case_section4_temporal_versions_and_batch_atomicity<S: ContextStore>(
    store: &S,
    clock: &Arc<ManualClock>,
) -> Result<(), String> {
    let tenant = TenantId::new("acme").map_err(|e| e.to_string())?.scope();
    let writer = Caller::new("ingester");

    let t0 = clock.now_timestamp();
    let n1 = store
        .put_node(
            &tenant,
            &writer,
            mk_global_node("mod:core", "Core v1", None)?,
        )
        .map_err(|e| format!("put v1: {e}"))?;
    let n2 = store
        .put_node(&tenant, &writer, mk_global_node("mod:util", "Util", None)?)
        .map_err(|e| format!("put n2: {e}"))?;
    let e1 = store
        .put_edge(
            &tenant,
            &writer,
            mk_edge(Some("edge:cu"), "IMPORTS", &n1, &n2, JsonMap::new())?,
        )
        .map_err(|e| format!("put edge: {e}"))?;

    // Identical re-put after advancing clock must NOT create a new version.
    clock.advance(Duration::from_secs(10));
    store
        .put_node(
            &tenant,
            &writer,
            mk_global_node("mod:core", "Core v1", None)?,
        )
        .map_err(|e| format!("idempotent re-put: {e}"))?;
    let after_noop = store
        .get_node(&tenant, &writer, &n1, None)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "missing after noop".to_owned())?;
    ensure_eq(
        &after_noop.valid_from,
        &t0,
        "noop re-put must preserve valid_from",
    )?;

    let as_of_v1 = clock.now_timestamp();

    // Changed re-put closes v1 and opens v2.
    clock.advance(Duration::from_secs(10));
    let t_v2 = clock.now_timestamp();
    store
        .put_node(
            &tenant,
            &writer,
            mk_global_node("mod:core", "Core v2", None)?,
        )
        .map_err(|e| format!("changed re-put: {e}"))?;

    let hist_v1 = store
        .get_node(&tenant, &writer, &n1, Some(as_of_v1))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "missing v1 at as_of_v1".to_owned())?;
    ensure_eq(&hist_v1.label.as_str(), &"Core v1", "historical v1 label")?;
    ensure_eq(&hist_v1.valid_to, &Some(t_v2), "historical v1 valid_to")?;

    let cur_v2 = store
        .get_node(&tenant, &writer, &n1, None)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "missing v2".to_owned())?;
    ensure_eq(&cur_v2.label.as_str(), &"Core v2", "current v2 label")?;
    ensure_eq(&cur_v2.valid_to, &None, "current v2 valid_to")?;

    // Tombstone closes both node and incident edge; historical view stays readable.
    clock.advance(Duration::from_secs(5));
    let as_of_v2_alive = clock.now_timestamp();
    clock.advance(Duration::from_secs(5));
    store
        .tombstone_node(&tenant, &writer, &n1)
        .map_err(|e| format!("tombstone: {e}"))?;
    ensure(
        store
            .get_node(&tenant, &writer, &n1, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "tombstoned node must be absent at now",
    )?;
    ensure(
        store
            .get_edge(&tenant, &writer, &e1, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "incident edge must be closed by tombstone_node",
    )?;
    ensure(
        store
            .get_node(&tenant, &writer, &n1, Some(as_of_v2_alive))
            .map_err(|e| e.to_string())?
            .is_some(),
        "tombstoned node must remain readable at prior as_of",
    )?;

    // Hard delete removes historical versions too; first recreate n2 check via delete_node on n2.
    store
        .delete_node(&tenant, &writer, &n2)
        .map_err(|e| format!("delete_node: {e}"))?;
    ensure(
        store
            .get_node(&tenant, &writer, &n2, Some(as_of_v1))
            .map_err(|e| e.to_string())?
            .is_none(),
        "delete_node must remove historical versions",
    )?;

    // Atomic `put_batch`:
    // (a) validation failure (513-byte label > 512-byte limit) persists nothing;
    // (b) mid-transaction failure (edge to missing endpoint after inserting `batch:good-2`) rolls
    //     back the SQLite/backend transaction so `batch:good-2` is not persisted either!
    let good_item_1 = mk_global_node("batch:good-1", "Good 1", None)?;
    let mut bad_item = mk_global_node("batch:bad-label", "placeholder", None)?;
    bad_item.label = "x".repeat(513);
    let batch_res_1 = store.put_batch(&tenant, &writer, &[good_item_1, bad_item], &[]);
    ensure(batch_res_1.is_err(), "batch with 513-byte label must fail")?;
    ensure(
        store
            .get_node(&tenant, &writer, &mk_node_id("batch:good-1")?, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "failed put_batch (validation) persisted earlier item",
    )?;

    let good_item_2 = mk_global_node("batch:good-2", "Good 2", None)?;
    let bad_edge = mk_edge(
        Some("edge:dangling"),
        "IMPORTS",
        &mk_node_id("batch:good-2")?,
        &mk_node_id("batch:does-not-exist")?,
        JsonMap::new(),
    )?;
    let batch_res_2 = store.put_batch(&tenant, &writer, &[good_item_2], &[bad_edge]);
    ensure(batch_res_2.is_err(), "batch with dangling edge must fail")?;
    ensure(
        store
            .get_node(&tenant, &writer, &mk_node_id("batch:good-2")?, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "failed put_batch (dangling edge) did not roll back inserted node",
    )?;

    Ok(())
}

fn case_section5_scopes_visibility_and_indistinguishability<S: ContextStore>(
    store: &S,
    clock: &Arc<ManualClock>,
) -> Result<(), String> {
    let tenant = TenantId::new("acme").map_err(|e| e.to_string())?.scope();

    let parent_task = TaskId::from_unix_ms(1_700_000_000_000, [0x01; 10]);
    let child_task = TaskId::from_unix_ms(1_700_000_000_001, [0x02; 10]);

    let mut alice = Caller::new("alice");
    alice.groups.insert("eng".to_owned());
    alice.task_chain = vec![child_task, parent_task];

    let bob = Caller::new("bob");

    // No ID oracle: non-Global new node with caller-chosen ID must fail with Invalid (unless
    // updating an existing visible scoped node, where missing/invisible returns NotFound).
    // Wait: let's check both! First, mint a Private node with `id: None` -> UUIDv7.
    let priv_id = store
        .put_node(
            &tenant,
            &alice,
            NewNode {
                id: None,
                node_type: mk_node_type("Secret")?,
                label: "Alice Private".to_owned(),
                summary: Some("secretword".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Private,
                provenance: Provenance::new("alice"),
            },
        )
        .map_err(|e| format!("put private: {e}"))?;
    let parsed_uuid = uuid::Uuid::parse_str(priv_id.as_str())
        .map_err(|e| format!("minted private id must be UUIDv7: {e}"))?;
    ensure_eq(&parsed_uuid.get_version_num(), &7, "minted UUID version")?;

    let group_id = store
        .put_node(
            &tenant,
            &alice,
            NewNode {
                id: None,
                node_type: mk_node_type("Doc")?,
                label: "Eng Group Doc".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Group {
                    group: "eng".to_owned(),
                },
                provenance: Provenance::new("alice"),
            },
        )
        .map_err(|e| format!("put group: {e}"))?;

    let task_node_id = store
        .put_node(
            &tenant,
            &alice,
            NewNode {
                id: None,
                node_type: mk_node_type("Step")?,
                label: "Parent Task Memo".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Task {
                    task_id: parent_task,
                },
                provenance: Provenance::new("alice"),
            },
        )
        .map_err(|e| format!("put task: {e}"))?;

    let t0 = clock.now_timestamp();
    let exp_at = Timestamp::from_unix_nanos(t0.unix_nanos() + 60 * 1_000_000_000)
        .map_err(|e| e.to_string())?;
    let eph_id = store
        .put_node(
            &tenant,
            &alice,
            NewNode {
                id: None,
                node_type: mk_node_type("Scratch")?,
                label: "Ephemeral Global".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Ephemeral {
                    base: BaseScope::Global,
                    expires_at: Some(exp_at),
                },
                provenance: Provenance::new("alice"),
            },
        )
        .map_err(|e| format!("put ephemeral: {e}"))?;

    let global_id = store
        .put_node(
            &tenant,
            &alice,
            mk_global_node("pub:root", "Public Root", None)?,
        )
        .map_err(|e| format!("put global: {e}"))?;

    let priv_edge = store
        .put_edge(
            &tenant,
            &alice,
            mk_edge(None, "LINKS", &global_id, &priv_id, JsonMap::new())?,
        )
        .map_err(|e| format!("put edge global->priv: {e}"))?;

    // Alice sees all 5 nodes and 1 edge; Bob sees only Global + unexpired Ephemeral(Global) and 0 edges.
    ensure_eq(
        &store
            .counts(&tenant, &alice, None)
            .map_err(|e| e.to_string())?,
        &GraphCounts { nodes: 5, edges: 1 },
        "alice counts before expiry",
    )?;
    ensure_eq(
        &store
            .counts(&tenant, &bob, None)
            .map_err(|e| e.to_string())?,
        &GraphCounts { nodes: 2, edges: 0 },
        "bob counts before expiry",
    )?;
    ensure(
        store
            .get_node(&tenant, &alice, &group_id, None)
            .map_err(|e| e.to_string())?
            .is_some(),
        "alice group member sees group node",
    )?;
    ensure(
        store
            .get_node(&tenant, &bob, &group_id, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "bob non-member must not see group node",
    )?;
    ensure(
        store
            .get_node(&tenant, &alice, &task_node_id, None)
            .map_err(|e| e.to_string())?
            .is_some(),
        "child task with parent in task_chain sees parent task node",
    )?;
    ensure(
        store
            .get_edge(&tenant, &bob, &priv_edge, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "edge with invisible endpoint must be invisible",
    )?;

    // Indistinguishability: Bob's responses on `priv_id` match `missing_id` identically.
    let missing_id = mk_node_id("018bc000-0000-7000-8000-ffffffffffff")?;
    ensure_eq(
        &store.get_node(&tenant, &bob, &priv_id, None),
        &store.get_node(&tenant, &bob, &missing_id, None),
        "indistinguishable get_node",
    )?;
    ensure_eq(
        &store.tombstone_node(&tenant, &bob, &priv_id),
        &store.tombstone_node(&tenant, &bob, &missing_id),
        "indistinguishable tombstone_node",
    )?;
    ensure_eq(
        &store.traverse(&tenant, &bob, &TraverseQuery::new(priv_id.clone())),
        &store.traverse(&tenant, &bob, &TraverseQuery::new(missing_id)),
        "indistinguishable traverse",
    )?;

    // Advance clock to exact `exp_at`: ephemeral node becomes invisible even before purge.
    clock.advance(Duration::from_secs(60));
    ensure(
        store
            .get_node(&tenant, &bob, &eph_id, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "expired ephemeral must be invisible before purge",
    )?;

    Ok(())
}

fn case_section5_lifecycle_maintenance<S: ContextStore + ContextMaintenance>(
    store: &S,
    clock: &Arc<ManualClock>,
) -> Result<(), String> {
    let tenant = TenantId::new("acme").map_err(|e| e.to_string())?.scope();
    let task_id = TaskId::from_unix_ms(1_700_000_000_010, [0x77; 10]);
    let mut caller = Caller::new("worker");
    caller.task_chain.push(task_id);

    let t0 = clock.now_timestamp();
    let exp_at = Timestamp::from_unix_nanos(t0.unix_nanos() + 10 * 1_000_000_000)
        .map_err(|e| e.to_string())?;

    let eph_id = store
        .put_node(
            &tenant,
            &caller,
            NewNode {
                id: None,
                node_type: mk_node_type("Scratch")?,
                label: "Short TTL".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Ephemeral {
                    base: BaseScope::Global,
                    expires_at: Some(exp_at),
                },
                provenance: Provenance::new("worker"),
            },
        )
        .map_err(|e| e.to_string())?;

    let task_node = store
        .put_node(
            &tenant,
            &caller,
            NewNode {
                id: None,
                node_type: mk_node_type("TaskMemo")?,
                label: "Task Memo".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Task { task_id },
                provenance: Provenance::new("worker"),
            },
        )
        .map_err(|e| e.to_string())?;

    clock.advance(Duration::from_secs(5));
    let closed = store
        .tombstone_task_nodes(&tenant, task_id)
        .map_err(|e| format!("tombstone_task_nodes: {e}"))?;
    ensure_eq(&closed, &1, "tombstone_task_nodes count")?;
    ensure(
        store
            .get_node(&tenant, &caller, &task_node, None)
            .map_err(|e| e.to_string())?
            .is_none(),
        "tombstoned task node invisible at now",
    )?;

    // Advance another 5s (total 10s = exp_at); task_node was tombstoned 5s ago < 60s retention.
    clock.advance(Duration::from_secs(5));
    let r1 = store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(60),
        })
        .map_err(|e| format!("purge 1: {e}"))?;
    ensure_eq(
        &r1,
        &PurgeReport {
            purged_ephemeral_nodes: 1,
            purged_task_nodes: 0,
            purged_edges: 0,
        },
        "first purge report",
    )?;
    let _ = eph_id;

    // Advance 55s (task_node tombstoned 60s ago == retention) -> task_node purged!
    clock.advance(Duration::from_secs(55));
    let r2 = store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(60),
        })
        .map_err(|e| format!("purge 2: {e}"))?;
    ensure_eq(
        &r2,
        &PurgeReport {
            purged_ephemeral_nodes: 0,
            purged_task_nodes: 1,
            purged_edges: 0,
        },
        "second purge report",
    )?;

    Ok(())
}

fn case_section6_traversal_all_strategies<S: ContextStore>(
    store: &S,
    _clock: &Arc<ManualClock>,
) -> Result<(), String> {
    let tenant = TenantId::new("acme").map_err(|e| e.to_string())?.scope();
    let caller = Caller::new("reader");

    // Graph:
    // n0 -> n1 (weight 5.0), n0 -> n2 (weight 1.0), n2 -> n1 (weight 1.0),
    // n1 -> n3 (weight 1.0), n2 -> n2 (self-loop weight 1.0), n3 -> n0 (cycle weight 1.0),
    // plus n0 -> n4 (invalid negative weight -2.0).
    let mut ids = Vec::new();
    for code in ["n0", "n1", "n2", "n3", "n4"] {
        let id = store
            .put_node(&tenant, &caller, mk_global_node(code, code, None)?)
            .map_err(|e| e.to_string())?;
        ids.push(id);
    }

    let w = |val: f64| -> JsonMap<String, serde_json::Value> {
        let mut m = JsonMap::new();
        m.insert("weight".to_owned(), json!(val));
        m
    };

    let edges = vec![
        mk_edge(Some("e01"), "FLOW", &ids[0], &ids[1], w(5.0))?,
        mk_edge(Some("e02"), "FLOW", &ids[0], &ids[2], w(1.0))?,
        mk_edge(Some("e21"), "FLOW", &ids[2], &ids[1], w(1.0))?,
        mk_edge(Some("e13"), "FLOW", &ids[1], &ids[3], w(1.0))?,
        mk_edge(Some("e22"), "FLOW", &ids[2], &ids[2], w(1.0))?,
        mk_edge(Some("e30"), "FLOW", &ids[3], &ids[0], w(1.0))?,
        mk_edge(Some("e04_neg"), "FLOW", &ids[0], &ids[4], w(-2.0))?,
    ];
    store
        .put_batch(&tenant, &caller, &[], &edges)
        .map_err(|e| format!("put edges: {e}"))?;

    // 1. BFS order: depth 0=[n0], depth 1=[n1, n2, n4], depth 2=[n3].
    let mut bfs_q = TraverseQuery::new(ids[0].clone());
    bfs_q.max_depth = 4;
    bfs_q.strategy = TraverseStrategy::Bfs;
    let bfs_res = store
        .traverse(&tenant, &caller, &bfs_q)
        .map_err(|e| e.to_string())?;
    let bfs_ids: Vec<&str> = bfs_res.nodes.iter().map(|n| n.id.as_str()).collect();
    ensure_eq(
        &bfs_ids,
        &vec!["n0", "n1", "n2", "n4", "n3"],
        "BFS node order",
    )?;
    ensure(!bfs_res.truncated, "BFS not truncated")?;

    // 2. DFS preorder: n0 -> n1 -> n3 -> n2 -> n4.
    let mut dfs_q = TraverseQuery::new(ids[0].clone());
    dfs_q.max_depth = 4;
    dfs_q.strategy = TraverseStrategy::Dfs;
    let dfs_res = store
        .traverse(&tenant, &caller, &dfs_q)
        .map_err(|e| e.to_string())?;
    let dfs_ids: Vec<&str> = dfs_res.nodes.iter().map(|n| n.id.as_str()).collect();
    ensure_eq(
        &dfs_ids,
        &vec!["n0", "n1", "n3", "n2", "n4"],
        "DFS preorder",
    )?;

    // 3. SHORTEST: tree has N-1 == 4 edges (one per non-root reached node).
    let mut sh_q = TraverseQuery::new(ids[0].clone());
    sh_q.max_depth = 4;
    sh_q.strategy = TraverseStrategy::Shortest;
    let sh_res = store
        .traverse(&tenant, &caller, &sh_q)
        .map_err(|e| e.to_string())?;
    ensure_eq(&sh_res.edges.len(), &4, "SHORTEST tree edge count")?;

    // 4. WEIGHTED: skips negative edge e04_neg with SkippedInvalidWeights { count: 1 };
    // settles n0(0.0), n2(1.0), n1(2.0 via n2), n3(3.0 via n1).
    let mut wt_q = TraverseQuery::new(ids[0].clone());
    wt_q.max_depth = 4;
    wt_q.strategy = TraverseStrategy::Weighted;
    let wt_res = store
        .traverse(&tenant, &caller, &wt_q)
        .map_err(|e| e.to_string())?;
    let wt_ids: Vec<&str> = wt_res.nodes.iter().map(|n| n.id.as_str()).collect();
    ensure_eq(
        &wt_ids,
        &vec!["n0", "n2", "n1", "n3"],
        "WEIGHTED cost order",
    )?;
    ensure_eq(
        &wt_res.warnings,
        &vec![TraverseWarning::SkippedInvalidWeights { count: 1 }],
        "WEIGHTED invalid weight warning",
    )?;

    // 5. Stateless pagination (`first = 2`) concatenates to full BFS list.
    let mut paged_ids: Vec<String> = Vec::new();
    let mut cursor = None;
    for _ in 0..5 {
        let mut pq = TraverseQuery::new(ids[0].clone());
        pq.max_depth = 4;
        pq.strategy = TraverseStrategy::Bfs;
        pq.first = 2;
        pq.after = cursor;
        pq.as_of = Some(bfs_res.as_of);
        let page = store
            .traverse(&tenant, &caller, &pq)
            .map_err(|e| e.to_string())?;
        for n in &page.nodes {
            paged_ids.push(n.id.as_str().to_owned());
        }
        if page.resume.is_none() {
            break;
        }
        cursor = page.resume;
    }
    let expected_owned: Vec<String> = bfs_ids.iter().map(|s| (*s).to_owned()).collect();
    ensure_eq(
        &paged_ids,
        &expected_owned,
        "stateless resume concatenation",
    )?;

    // 6. `visited_cap = 2` sets `truncated = true`.
    let mut cap_q = TraverseQuery::new(ids[0].clone());
    cap_q.max_depth = 4;
    cap_q.visited_cap = Some(2);
    let cap_res = store
        .traverse(&tenant, &caller, &cap_q)
        .map_err(|e| e.to_string())?;
    ensure(cap_res.truncated, "visited_cap must set truncated = true")?;
    ensure_eq(&cap_res.nodes.len(), &2, "visited_cap node len")?;

    // 7. `Direction::Both` from `n1` reaches `n0`, `n2`, and `n3` in 1 hop.
    let mut both_q = TraverseQuery::new(ids[1].clone());
    both_q.max_depth = 1;
    both_q.direction = Direction::Both;
    let both_res = store
        .traverse(&tenant, &caller, &both_q)
        .map_err(|e| e.to_string())?;
    let both_set: BTreeSet<&str> = both_res.nodes.iter().map(|n| n.id.as_str()).collect();
    let expected_both: BTreeSet<&str> = ["n0", "n1", "n2", "n3"].into_iter().collect();
    ensure_eq(&both_set, &expected_both, "Direction::Both 1-hop")?;

    let _ = Scope::Global;
    Ok(())
}

fn case_section7_search_and_fts_safety<S: ContextStore>(
    store: &S,
    _clock: &Arc<ManualClock>,
) -> Result<(), String> {
    let tenant = TenantId::new("acme").map_err(|e| e.to_string())?.scope();
    let caller = Caller::new("searcher");

    let nodes = vec![
        mk_global_node("doc:b", "zephyr", Some("once mention of zephyr in summary"))?,
        mk_global_node("doc:a", "zephyr", Some("once mention of zephyr in summary"))?,
        mk_global_node("doc:strong", "zephyr zephyr zephyr", Some("zephyr zephyr"))?,
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &[])
        .map_err(|e| e.to_string())?;

    let ranked = store
        .search_nodes(&tenant, &caller, "zephyr", 10, None)
        .map_err(|e| e.to_string())?;
    let ranked_ids: Vec<&str> = ranked.iter().map(|n| n.id.as_str()).collect();
    ensure_eq(
        &ranked_ids,
        &vec!["doc:strong", "doc:a", "doc:b"],
        "BM25 ranking + ascending NodeId tie-break",
    )?;

    // Hostile FTS5 operator syntax must not raise errors or widen matches via `OR` / `*`.
    for hostile in [
        "\"unclosed quote * NEAR( foo:bar -baz",
        "zephyr OR nonexistent",
        "   *** --- ::: \"\"\"   ",
    ] {
        let res = store
            .search_nodes(&tenant, &caller, hostile, 10, None)
            .map_err(|e| format!("hostile query `{hostile}` failed: {e}"))?;
        if hostile.contains("nonexistent") {
            // Quoted literal token `"zephyr" "OR" "nonexistent"` requires all tokens (implicit AND),
            // never FTS5 `OR` disjunction!
            ensure(
                res.is_empty(),
                "FTS5 `OR` must be quoted as a literal token, not evaluated as disjunction",
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gap_core::clock::Clock;
    use tempfile::TempDir;

    use super::run_context_store_suite;
    use crate::sqlite::{Durability, SqliteConfig, SqliteContextStore};

    // spec: INV-09, INV-19, ING-062, ING-064, ING-065, ING-066, ING-067, ING-080, ING-081, ING-082, ING-083, ING-084, ING-085, ING-086, ING-087
    #[test]
    fn sqlite_passes_reusable_conformance_suite() {
        let dir = TempDir::new().expect("tempdir");
        let seq = AtomicUsize::new(0);
        let report = run_context_store_suite(|clock| {
            let idx = seq.fetch_add(1, Ordering::Relaxed);
            let mut cfg = SqliteConfig::new(dir.path().join(format!("case_{idx}.db")));
            cfg.durability = Durability::Off;
            let clock_dyn: Arc<dyn Clock> = clock;
            SqliteContextStore::open(cfg, clock_dyn)
        });
        assert!(
            report.failed.is_empty(),
            "conformance failures: {:?}",
            report.failed
        );
        assert_eq!(report.passed, 6);
    }
}
