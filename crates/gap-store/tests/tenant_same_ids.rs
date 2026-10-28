//! Cross-tenant collision tests where two tenants (`ta` and `tb`) reuse the exact same node IDs,
//! edge IDs, and `TaskId`s (`INV-09`, `INV-19`, `ING-065`, `ING-066`, `ING-080`..`ING-084`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_store::sqlite::schema::tenant_fts_table_name;
use gap_store::{
    BaseScope, Caller, ContextMaintenance, ContextStore, Direction, Edge, EdgeId, EdgeResolution,
    EdgeType, GraphCounts, NewEdge, NewNode, Node, NodeId, NodeType, Provenance, PurgePolicy,
    ScopeSpec, SqliteConfig, SqliteContextStore, TaskId, TenantId, TenantScope, TraverseQuery,
    TraverseStats, TraverseStrategy,
};
use rusqlite::{Connection, params};
use serde_json::Map as JsonMap;
use tempfile::TempDir;

fn global_node(id: &str, label: &str, summary: &str) -> NewNode {
    NewNode {
        id: Some(NodeId::new(id).expect("node id")),
        node_type: NodeType::new("Symbol").expect("node type"),
        label: label.to_owned(),
        summary: Some(summary.to_owned()),
        props: JsonMap::new(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("same-id-test"),
    }
}

fn scoped_node(id: Option<NodeId>, label: &str, summary: &str, scope: ScopeSpec) -> NewNode {
    NewNode {
        id,
        node_type: NodeType::new("Note").expect("node type"),
        label: label.to_owned(),
        summary: Some(summary.to_owned()),
        props: JsonMap::new(),
        scope,
        provenance: Provenance::new("same-id-test"),
    }
}

fn edge(id: &str, from: &str, to: &str) -> NewEdge {
    NewEdge {
        id: Some(EdgeId::new(id).expect("edge id")),
        edge_type: EdgeType::new("CALLS").expect("edge type"),
        from: NodeId::new(from).expect("from id"),
        to: NodeId::new(to).expect("to id"),
        props: JsonMap::new(),
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("same-id-test"),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct TenantSnapshot {
    api_nodes: Vec<Option<Node>>,
    api_edges: Vec<Option<Edge>>,
    api_counts: GraphCounts,
    api_search: Vec<Node>,
    raw_node_count: i64,
    raw_edge_count: i64,
    raw_fts_count: i64,
    raw_node_dump: Vec<(String, String, Option<i64>)>,
    raw_edge_dump: Vec<(String, String, String, Option<i64>)>,
    raw_fts_dump: Vec<(i64, String, String)>,
}

fn capture_tenant_snapshot(
    store: &SqliteContextStore,
    db_path: &std::path::Path,
    tenant: &TenantScope,
    caller: &Caller,
    watch_nodes: &[NodeId],
    watch_edges: &[EdgeId],
    search_token: &str,
) -> TenantSnapshot {
    let api_nodes = watch_nodes
        .iter()
        .map(|nid| store.get_node(tenant, caller, nid, None).expect("get_node"))
        .collect();
    let api_edges = watch_edges
        .iter()
        .map(|eid| store.get_edge(tenant, caller, eid, None).expect("get_edge"))
        .collect();
    let api_counts = store.counts(tenant, caller, None).expect("counts");
    let api_search = store
        .search_nodes(tenant, caller, search_token, 50, None)
        .expect("search_nodes");

    let raw = Connection::open(db_path).expect("open raw");
    let t_str = tenant.tenant().as_str();
    let raw_node_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE tenant = ?1",
            params![t_str],
            |r| r.get(0),
        )
        .expect("raw_node_count");
    let raw_edge_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE tenant = ?1",
            params![t_str],
            |r| r.get(0),
        )
        .expect("raw_edge_count");

    let mut n_stmt = raw
        .prepare("SELECT id, label, valid_to FROM nodes WHERE tenant = ?1 ORDER BY fts_rowid ASC")
        .expect("prep nodes");
    let raw_node_dump = n_stmt
        .query_map(params![t_str], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("map nodes")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect nodes");

    let mut e_stmt = raw
        .prepare(
            "SELECT id, from_id, to_id, valid_to FROM edges WHERE tenant = ?1 ORDER BY edge_rowid ASC",
        )
        .expect("prep edges");
    let raw_edge_dump = e_stmt
        .query_map(params![t_str], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .expect("map edges")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect edges");

    let fts_table = tenant_fts_table_name(t_str);
    let fts_exists: bool = raw
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            params![fts_table],
            |r| r.get(0),
        )
        .expect("fts exists");
    let (raw_fts_count, raw_fts_dump) = if fts_exists {
        let cnt: i64 = raw
            .query_row(&format!("SELECT COUNT(*) FROM {fts_table}"), [], |r| {
                r.get(0)
            })
            .expect("fts count");
        let mut f_stmt = raw
            .prepare(&format!(
                "SELECT rowid, label, summary FROM {fts_table} ORDER BY rowid ASC"
            ))
            .expect("prep fts");
        let dump = f_stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("map fts")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect fts");
        (cnt, dump)
    } else {
        (0, Vec::new())
    };

    TenantSnapshot {
        api_nodes,
        api_edges,
        api_counts,
        api_search,
        raw_node_count,
        raw_edge_count,
        raw_fts_count,
        raw_node_dump,
        raw_edge_dump,
        raw_fts_dump,
    }
}

// spec: INV-09, INV-19, ING-080, ING-082, ING-083
#[test]
fn inv_09_same_ids_get_edge_put_tombstone_and_delete_node_preserve_other_tenant() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("t1_crud.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");

    let ta = TenantId::new("ta").expect("ta").scope();
    let tb = TenantId::new("tb").expect("tb").scope();
    let caller = Caller::new("alice");

    // Seed `tb` with GLOBAL nodes `n1`, `n2`, `n3` and edge `e1` (`n1 -> n2`).
    store
        .put_batch(
            &tb,
            &caller,
            &[
                global_node("n1", "tb_n1", "tbsearchtoken one"),
                global_node("n2", "tb_n2", "tbsearchtoken two"),
                global_node("n3", "tb_n3", "tbsearchtoken three"),
            ],
            &[edge("e1", "n1", "n2")],
        )
        .expect("seed tb");

    // Seed `ta` with the exact same GLOBAL node IDs `n1`, `n2`, `n3`, WITHOUT edge `e1` first.
    store
        .put_batch(
            &ta,
            &caller,
            &[
                global_node("n1", "ta_n1", "tasearchtoken one"),
                global_node("n2", "ta_n2", "tasearchtoken two"),
                global_node("n3", "ta_n3", "tasearchtoken three"),
            ],
            &[],
        )
        .expect("seed ta nodes");

    let n1 = NodeId::new("n1").expect("n1");
    let n2 = NodeId::new("n2").expect("n2");
    let n3 = NodeId::new("n3").expect("n3");
    let e1 = EdgeId::new("e1").expect("e1");
    let watch_nodes = [n1.clone(), n2.clone(), n3.clone()];
    let watch_edges = [e1.clone()];

    let tb_before = capture_tenant_snapshot(
        &store,
        &db_path,
        &tb,
        &caller,
        &watch_nodes,
        &watch_edges,
        "tbsearchtoken",
    );
    assert_eq!(tb_before.raw_node_count, 3);
    assert_eq!(tb_before.raw_edge_count, 1);
    assert_eq!(tb_before.raw_fts_count, 3);
    assert_eq!(tb_before.api_search.len(), 3);

    // 1. Mutant [10]: `get_edge` in `ta` for `e1` (which exists in `tb` between `n1` and `n2`,
    //    while `ta` ALSO has visible `n1` and `n2`) MUST return `None`.
    assert_eq!(
        store
            .get_edge(&ta, &caller, &e1, None)
            .expect("get_edge ta"),
        None
    );

    // 2. Now add `e1` (`n1 -> n2`) in `ta`, update `n2` in `ta`, tombstone `n3` in `ta`, and
    //    hard-delete `n1` in `ta` (Mutant [12]: `DELETE FROM edges` must be scoped to `ta`).
    store
        .put_edge(&ta, &caller, edge("e1", "n1", "n2"))
        .expect("put_edge ta e1");
    manual.advance(Duration::from_secs(5));
    store
        .put_node(&ta, &caller, global_node("n2", "ta_n2_v2", "tasearchtoken"))
        .expect("update ta n2");
    store
        .tombstone_node(&ta, &caller, &n3)
        .expect("tombstone ta n3");
    store.delete_node(&ta, &caller, &n1).expect("delete ta n1");

    assert_eq!(
        store
            .get_node(&ta, &caller, &n1, None)
            .expect("ta n1 deleted"),
        None
    );
    assert_eq!(
        store
            .get_edge(&ta, &caller, &e1, None)
            .expect("ta e1 deleted"),
        None
    );

    // `tb` must be byte-for-byte unchanged across API reads and raw SQL tables (`nodes`, `edges`, FTS).
    let tb_after = capture_tenant_snapshot(
        &store,
        &db_path,
        &tb,
        &caller,
        &watch_nodes,
        &watch_edges,
        "tbsearchtoken",
    );
    assert_eq!(tb_after, tb_before);
}

// spec: INV-09, INV-19, ING-066, ING-082
#[test]
fn inv_09_same_task_and_node_ids_tombstone_task_nodes_leaves_other_tenant_and_own_global_edges_intact()
 {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("t1_task.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");

    let ta = TenantId::new("ta").expect("ta").scope();
    let tb = TenantId::new("tb").expect("tb").scope();
    let shared_task = TaskId::from_unix_ms(1_700_000_000_000, [0x77; 10]);

    let mut caller = Caller::new("alice");
    caller.task_chain = vec![shared_task];

    // `tb` creates a TASK node under `shared_task` (minting `tb_task_nid`) + GLOBAL `n2` + edge `e1`.
    let tb_task_nid = store
        .put_node(
            &tb,
            &caller,
            scoped_node(
                None,
                "tb_task_note",
                "tbtasktoken",
                ScopeSpec::Task {
                    task_id: shared_task,
                },
            ),
        )
        .expect("put tb task node");
    let n2 = store
        .put_node(&tb, &caller, global_node("n2", "tb_n2", "tbtasktoken"))
        .expect("put tb n2");
    let e1 = store
        .put_edge(&tb, &caller, edge("e1", tb_task_nid.as_str(), n2.as_str()))
        .expect("put tb e1");

    // Part A (kills Mutant [90] directly): `ta` has NO `TASK` nodes for `shared_task`,
    // but `ta` has a `GLOBAL` node with the EXACT same ID `tb_task_nid` and an active edge `e1`
    // (`tb_task_nid -> n2`). If `SELECT_ACTIVE_TASK_NODES_SQL` ignores `tenant`, `tombstone_task_nodes`
    // in `ta` picks up `tb_task_nid` from `tb` and closes `ta`'s `GLOBAL` edge `e1`!
    store
        .put_batch(
            &ta,
            &caller,
            &[
                global_node(tb_task_nid.as_str(), "ta_global_same_id", "tatoken"),
                global_node("n2", "ta_n2", "tatoken"),
            ],
            &[edge("e1", tb_task_nid.as_str(), "n2")],
        )
        .expect("seed ta global collision");

    let watch_tb_nodes = [tb_task_nid.clone(), n2.clone()];
    let watch_tb_edges = [e1.clone()];
    let tb_before = capture_tenant_snapshot(
        &store,
        &db_path,
        &tb,
        &caller,
        &watch_tb_nodes,
        &watch_tb_edges,
        "tbtasktoken",
    );

    manual.advance(Duration::from_secs(10));
    let closed_when_ta_has_no_task = store
        .tombstone_task_nodes(&ta, shared_task)
        .expect("tombstone empty task in ta");
    assert_eq!(closed_when_ta_has_no_task, 0);
    assert!(
        store
            .get_edge(&ta, &caller, &e1, None)
            .expect("ta global edge e1 must stay active")
            .is_some(),
        "tombstone_task_nodes(ta, T) must not close ta's GLOBAL edge via tb's TASK node id"
    );

    // Part B: Now `ta` ALSO writes its own TASK node with `shared_task` and tombstones it;
    // `tb`'s TASK node, edge, search hits, and raw SQL rows stay byte-for-byte unchanged.
    let ta_task_nid = store
        .put_node(
            &ta,
            &caller,
            scoped_node(
                None,
                "ta_task_note",
                "tatoken",
                ScopeSpec::Task {
                    task_id: shared_task,
                },
            ),
        )
        .expect("put ta task node");
    manual.advance(Duration::from_secs(10));
    let closed_ta = store
        .tombstone_task_nodes(&ta, shared_task)
        .expect("tombstone ta task");
    assert_eq!(closed_ta, 1);
    assert_eq!(
        store
            .get_node(&ta, &caller, &ta_task_nid, None)
            .expect("ta task closed"),
        None
    );

    let tb_after = capture_tenant_snapshot(
        &store,
        &db_path,
        &tb,
        &caller,
        &watch_tb_nodes,
        &watch_tb_edges,
        "tbtasktoken",
    );
    assert_eq!(tb_after, tb_before);
}

// spec: INV-09, INV-19, ING-066, ING-082, ING-083
#[test]
fn inv_09_same_ids_purge_expired_isolates_tenants_and_preserves_edges_with_surviving_node_versions()
{
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("t1_purge.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");

    let ta = TenantId::new("ta").expect("ta").scope();
    let tb = TenantId::new("tb").expect("tb").scope();
    let shared_task = TaskId::from_unix_ms(1_700_000_000_000, [0x55; 10]);

    let mut alice = Caller::new("alice");
    alice.task_chain = vec![shared_task];

    let t0_ns = manual.now_timestamp().unix_nanos();
    let exp_30s = Timestamp::from_unix_nanos(t0_ns + 30 * 1_000_000_000).expect("exp_30s");

    // 1. In `ta`: create an EPHEMERAL node (`eph_id`), a TASK node (`task_nid`), and a second
    //    EPHEMERAL node (`multi_ver_id`) that we upgrade to persistent PRIVATE before it expires,
    //    plus GLOBAL `n3` and incident edges in `ta`.
    let ta_n3 = store
        .put_node(&ta, &alice, global_node("n3", "ta_n3", "tapurge"))
        .expect("put ta n3");
    let eph_id = store
        .put_node(
            &ta,
            &alice,
            scoped_node(
                None,
                "ta_eph",
                "tapurge eph",
                ScopeSpec::ephemeral(BaseScope::Global, exp_30s),
            ),
        )
        .expect("put ta eph");
    let task_nid = store
        .put_node(
            &ta,
            &alice,
            scoped_node(
                None,
                "ta_task",
                "tapurge task",
                ScopeSpec::Task {
                    task_id: shared_task,
                },
            ),
        )
        .expect("put ta task");
    let multi_ver_id = store
        .put_node(
            &ta,
            &alice,
            scoped_node(
                None,
                "ta_multiver_v1_eph",
                "tapurge mv1",
                ScopeSpec::ephemeral(BaseScope::Global, exp_30s),
            ),
        )
        .expect("put ta multiver v1");

    store
        .put_edge(&ta, &alice, edge("e_eph", eph_id.as_str(), ta_n3.as_str()))
        .expect("put e_eph");
    store
        .put_edge(
            &ta,
            &alice,
            edge("e_task", task_nid.as_str(), ta_n3.as_str()),
        )
        .expect("put e_task");
    let e_keep = store
        .put_edge(
            &ta,
            &alice,
            edge("e_keep", multi_ver_id.as_str(), ta_n3.as_str()),
        )
        .expect("put e_keep");

    // Before `multi_ver_id` expires, update it to persistent `ScopeSpec::Private` so version 1
    // (`scope = 'EPHEMERAL'`) will be purged at +8d while version 2 (`scope = 'PRIVATE'`) survives
    // (kills Mutant [97]).
    manual.advance(Duration::from_secs(5));
    store
        .put_node(
            &ta,
            &alice,
            scoped_node(
                Some(multi_ver_id.clone()),
                "ta_multiver_v2_priv",
                "tapurge mv2",
                ScopeSpec::Private,
            ),
        )
        .expect("upgrade multi_ver_id to Private");

    // Tombstone `ta`'s `shared_task` at t0 + 5s.
    assert_eq!(
        store
            .tombstone_task_nodes(&ta, shared_task)
            .expect("tombstone ta task"),
        1
    );

    // 2. In `tb`: create live GLOBAL nodes with the EXACT SAME IDs (`eph_id`, `task_nid`, `n3`)
    //    and live edges `e1` (`eph_id -> n3`) and `e2` (`task_nid -> n3`).
    //    - Kills Mutant [94]: `tb` has an active (`valid_to IS NULL`) node with `id == task_nid`,
    //      which MUST NOT block `ta`'s tombstoned `task_nid` from being purged!
    //    - Kills Mutant [96]: purging `eph_id` and `task_nid` in `ta` MUST NOT delete `tb`'s edges!
    store
        .put_batch(
            &tb,
            &alice,
            &[
                global_node(eph_id.as_str(), "tb_live_same_eph_id", "tbpurgetoken"),
                global_node(task_nid.as_str(), "tb_live_same_task_id", "tbpurgetoken"),
                global_node("n3", "tb_n3", "tbpurgetoken"),
            ],
            &[
                edge("e1", eph_id.as_str(), "n3"),
                edge("e2", task_nid.as_str(), "n3"),
            ],
        )
        .expect("seed tb same ids");

    let watch_tb_nodes = [
        eph_id.clone(),
        task_nid.clone(),
        NodeId::new("n3").expect("n3"),
    ];
    let watch_tb_edges = [
        EdgeId::new("e1").expect("e1"),
        EdgeId::new("e2").expect("e2"),
    ];
    let tb_before = capture_tenant_snapshot(
        &store,
        &db_path,
        &tb,
        &alice,
        &watch_tb_nodes,
        &watch_tb_edges,
        "tbpurgetoken",
    );

    // Advance 8 days (> 7-day default task retention and > 30s ephemeral TTL).
    manual.advance(Duration::from_secs(8 * 86_400));
    let report = store
        .purge_expired(&PurgePolicy::default())
        .expect("purge_expired");

    // 2 ephemeral rows purged in `ta` (`eph_id` + v1 of `multi_ver_id`), 1 task row (`task_nid`),
    // and ONLY 2 edges (`e_eph` and `e_task`) because `multi_ver_id` still has v2 (`e_keep` stays!).
    assert_eq!(report.purged_ephemeral_nodes, 2);
    assert_eq!(report.purged_task_nodes, 1);
    assert_eq!(report.purged_edges, 2);

    // `e_keep` and `multi_ver_id` v2 remain live in `ta`!
    assert_eq!(
        store
            .get_node(&ta, &alice, &multi_ver_id, None)
            .expect("get multi_ver_id")
            .expect("multi_ver_id v2 survives")
            .label,
        "ta_multiver_v2_priv"
    );
    assert!(
        store
            .get_edge(&ta, &alice, &e_keep, None)
            .expect("get e_keep")
            .is_some(),
        "purge_expired must keep an edge whose endpoint still has a surviving version"
    );

    // `tb` is byte-for-byte unchanged!
    let tb_after = capture_tenant_snapshot(
        &store,
        &db_path,
        &tb,
        &alice,
        &watch_tb_nodes,
        &watch_tb_edges,
        "tbpurgetoken",
    );
    assert_eq!(tb_after, tb_before);
}

// spec: INV-09, INV-19, ING-065, ING-083, ING-084
#[test]
fn inv_09_same_ids_traverse_isolates_cross_tenant_edges_and_neighbour_visibility_across_all_strategies()
 {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("t1_traverse.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");

    let ta = TenantId::new("ta").expect("ta").scope();
    let tb = TenantId::new("tb").expect("tb").scope();
    let alice = Caller::new("alice");
    let bob = Caller::new("bob");

    // Sub-case A (kills Mutants [59] and [60]):
    // `tb` has GLOBAL `n1`, `n2`, `n3` with edges `n1 -> n2` (`e1`) and `n3 -> n1` (`e2`).
    // `ta` has GLOBAL `n1`, `n2`, `n3` with NO edges.
    store
        .put_batch(
            &tb,
            &alice,
            &[
                global_node("n1", "tb_n1", "tb"),
                global_node("n2", "tb_n2", "tb"),
                global_node("n3", "tb_n3", "tb"),
            ],
            &[edge("e1", "n1", "n2"), edge("e2", "n3", "n1")],
        )
        .expect("seed tb edges");
    store
        .put_batch(
            &ta,
            &alice,
            &[
                global_node("n1", "ta_n1", "ta"),
                global_node("n2", "ta_n2", "ta"),
                global_node("n3", "ta_n3", "ta"),
            ],
            &[],
        )
        .expect("seed ta isolated nodes");

    let n1 = NodeId::new("n1").expect("n1");
    for strategy in [
        TraverseStrategy::Bfs,
        TraverseStrategy::Dfs,
        TraverseStrategy::Shortest,
        TraverseStrategy::Weighted,
    ] {
        for direction in [Direction::Outbound, Direction::Inbound, Direction::Both] {
            let mut q = TraverseQuery::new(n1.clone());
            q.strategy = strategy;
            q.direction = direction;
            q.max_depth = 3;
            let res = store
                .traverse(&ta, &alice, &q)
                .expect("traverse ta isolated");
            let ids: Vec<&str> = res.nodes.iter().map(|n| n.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["n1"],
                "strategy={strategy:?} direction={direction:?} leaked cross-tenant edge"
            );
            assert!(
                res.edges.is_empty(),
                "strategy={strategy:?} direction={direction:?} must return zero edges"
            );
            assert_eq!(
                res.stats,
                TraverseStats {
                    visited: 1,
                    returned: 1,
                    depth_reached: 0,
                }
            );
        }
    }

    // Sub-case B (kills Mutants [57] and [58]):
    // In `ta`, `alice` creates a PRIVATE node (`priv_n2`) and connects `n1 -> priv_n2` (`e_priv`).
    // In `tb`, a GLOBAL node is created with the EXACT SAME ID `priv_n2`.
    // When `bob` traverses from `n1` in `ta`, `bob` MUST NOT see `priv_n2`, `e_priv`, or visited=2!
    let priv_n2 = store
        .put_node(
            &ta,
            &alice,
            scoped_node(None, "ta_alice_secret", "secret", ScopeSpec::Private),
        )
        .expect("put ta private node");
    store
        .put_edge(&ta, &alice, edge("e_priv", "n1", priv_n2.as_str()))
        .expect("put ta edge to private node");
    store
        .put_node(
            &tb,
            &bob,
            global_node(priv_n2.as_str(), "tb_global_same_id", "public in tb"),
        )
        .expect("put tb global node with same id");

    for strategy in [
        TraverseStrategy::Bfs,
        TraverseStrategy::Dfs,
        TraverseStrategy::Shortest,
        TraverseStrategy::Weighted,
    ] {
        for direction in [Direction::Outbound, Direction::Both] {
            let mut q = TraverseQuery::new(n1.clone());
            q.strategy = strategy;
            q.direction = direction;
            q.max_depth = 3;
            let res = store
                .traverse(&ta, &bob, &q)
                .expect("bob traverse ta with hidden neighbour");
            let ids: Vec<&str> = res.nodes.iter().map(|n| n.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["n1"],
                "strategy={strategy:?} direction={direction:?} leaked private node via tb's GLOBAL node"
            );
            assert!(res.edges.is_empty());
            assert_eq!(
                res.stats,
                TraverseStats {
                    visited: 1,
                    returned: 1,
                    depth_reached: 0,
                }
            );
            assert!(!res.truncated);
        }
    }
}
