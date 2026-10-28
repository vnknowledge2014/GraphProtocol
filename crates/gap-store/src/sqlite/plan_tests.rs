//! Deterministic `EXPLAIN QUERY PLAN` and `StatementStatus` work-counter tests verifying
//! `ING-088` and `INV-09` across all `SqliteContextStore` hot paths without wall-clock timing.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_core::ids::{TaskId, TenantId, TenantScope};
use rusqlite::{Connection, StatementStatus, params};
use serde_json::{Map as JsonMap, json};
use tempfile::TempDir;

use super::{
    Durability, SqliteConfig, SqliteContextStore, crud, lock_unpoisoned, maintenance, schema,
    search, traverse,
};
use crate::store::{ContextMaintenance, ContextStore};
use crate::types::{
    BaseScope, Caller, Direction, EdgeId, EdgeResolution, EdgeType, NewEdge, NewNode, NodeId,
    NodeType, Provenance, PurgePolicy, ScopeSpec, TraverseQuery, TraverseStrategy,
};

const GRAPH_NODE_COUNT: usize = 3_000;
const GRAPH_EDGE_COUNT: usize = 15_000;

/// Deterministic PRNG (`xorshift64`) so the 3,000-node / 15,000-edge graph is identical on every run.
struct XorShift64(u64);

impl XorShift64 {
    fn next_usize(&mut self, modulo: usize) -> usize {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x as usize) % modulo
    }
}

struct HotPathFixture {
    _dir: TempDir,
    clock: Arc<ManualClock>,
    store: SqliteContextStore,
    tenant: TenantScope,
    caller: Caller,
    active_task: TaskId,
    sample_edge_id: EdgeId,
}

/// Builds a fresh, never-analysed database containing `3,000` nodes and `15,000` unique directed
/// edges in the primary tenant (plus a second tenant for `INV-09` isolation) exclusively via
/// `ContextStore::put_batch`.
fn build_never_analysed_3k_15k_fixture() -> HotPathFixture {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("ing_088_plan.db");
    let mut cfg = SqliteConfig::new(&db_path);
    cfg.durability = Durability::Off;
    cfg.reader_pool_size = NonZeroUsize::new(1).expect("nonzero 1");

    let base_ms: i64 = 1_700_000_000_000;
    let clock = Arc::new(ManualClock::new(base_ms as u64));
    let clock_dyn: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let store = SqliteContextStore::open(cfg, clock_dyn).expect("open store");

    let tenant = TenantId::new("alpha-tenant").expect("tenant").scope();
    let other_tenant = TenantId::new("beta-tenant").expect("other tenant").scope();

    let active_task =
        TaskId::parse("018f1000-0000-7000-8000-000000000001").expect("active task uuid");
    let purge_task =
        TaskId::parse("018f1000-0000-7000-8000-000000000002").expect("purge task uuid");

    let mut caller = Caller::new("alice");
    caller.groups.insert("eng".to_owned());
    caller.task_chain = vec![active_task, purge_task];

    let node_type = NodeType::new("Entity").expect("node_type");
    let edge_type = EdgeType::new("LINKS").expect("edge_type");

    // Nodes 0..2,995: GLOBAL graph backbone (`n-000000`..`n-002994`).
    // Node 2,995 (`n-002995`): PRIVATE node owned by `alice`.
    // Node 2,996 (`n-002996`): GROUP node in `"eng"`.
    // Node 2,997 (`n-002997`): active TASK node under `active_task` (for `tombstone_task_nodes`).
    // Node 2,998 (`n-002998`): TASK node under `purge_task` (tombstoned below so `purge_expired`
    //                          exercises expired task-node purging).
    // Node 2,999 (`n-002999`): EPHEMERAL node expiring at `base_ms + 5_000` (so advancing clock
    //                          by `20_000 ms` makes both `n-002998` and `n-002999` purgeable).
    let mut nodes = Vec::with_capacity(GRAPH_NODE_COUNT);
    for i in 0..GRAPH_NODE_COUNT {
        let scope = match i {
            2_995 => ScopeSpec::Private,
            2_996 => ScopeSpec::Group {
                group: "eng".to_owned(),
            },
            2_997 => ScopeSpec::Task {
                task_id: active_task,
            },
            2_998 => ScopeSpec::Task {
                task_id: purge_task,
            },
            2_999 => ScopeSpec::Ephemeral {
                base: BaseScope::Global,
                expires_at: Some(Timestamp::from_unix_millis(base_ms + 5_000).expect("expires_at")),
            },
            _ => ScopeSpec::Global,
        };
        let label = if i == 42 {
            "uniquetokenneedle node42".to_owned()
        } else {
            format!("node label {i}")
        };
        let id = if i < 2_995 {
            Some(NodeId::new(format!("n-{i:06}")).expect("nid"))
        } else {
            None
        };
        nodes.push(NewNode {
            id,
            node_type: node_type.clone(),
            label,
            summary: Some(format!("summary payload for vertex {i}")),
            props: JsonMap::new(),
            scope,
            provenance: Provenance::new("ing-088-fixture"),
        });
    }
    assert_eq!(nodes.len(), GRAPH_NODE_COUNT);
    let node_batch = store
        .put_batch(&tenant, &caller, &nodes, &[])
        .expect("put 3000 nodes");
    let inserted_node_ids = node_batch.node_ids;
    assert_eq!(inserted_node_ids.len(), GRAPH_NODE_COUNT);

    // Generate 15,000 unique directed edges across `0..3,000` (keeping `2,998` and `2,999` with
    // small controlled incident edges included in the 15,000 count).
    let mut seen: HashSet<(usize, usize)> = HashSet::with_capacity(GRAPH_EDGE_COUNT * 2);
    let mut edges: Vec<NewEdge> = Vec::with_capacity(GRAPH_EDGE_COUNT);

    // Ensure `n-000000` has both outbound and inbound edges so all 3 traversal directions reach >= 10 nodes.
    let mut seed_pairs: Vec<(usize, usize)> = Vec::new();
    for k in 1..=6 {
        seed_pairs.push((0, k));
        seed_pairs.push((k + 6, 0));
    }
    seed_pairs.push((0, 2_997));
    seed_pairs.push((2_998, 1));
    seed_pairs.push((2_999, 2));

    for (u, v) in seed_pairs {
        seen.insert((u, v));
        let idx = edges.len();
        let mut props = JsonMap::new();
        props.insert("weight".to_owned(), json!(1.0 + ((idx % 5) as f64) * 0.25));
        edges.push(NewEdge {
            id: Some(EdgeId::new(format!("e-{idx:06}")).expect("eid")),
            edge_type: edge_type.clone(),
            from: inserted_node_ids[u].clone(),
            to: inserted_node_ids[v].clone(),
            props,
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: Provenance::new("ing-088-fixture"),
        });
    }

    let mut rng = XorShift64(0xD1B5_4A32_D192_ED03);
    while edges.len() < GRAPH_EDGE_COUNT {
        let u = rng.next_usize(2_995);
        let v = rng.next_usize(2_995);
        if u == v || !seen.insert((u, v)) {
            continue;
        }
        let idx = edges.len();
        let mut props = JsonMap::new();
        props.insert("weight".to_owned(), json!(1.0 + ((idx % 7) as f64) * 0.5));
        edges.push(NewEdge {
            id: Some(EdgeId::new(format!("e-{idx:06}")).expect("eid")),
            edge_type: edge_type.clone(),
            from: inserted_node_ids[u].clone(),
            to: inserted_node_ids[v].clone(),
            props,
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: Provenance::new("ing-088-fixture"),
        });
    }
    assert_eq!(edges.len(), GRAPH_EDGE_COUNT);
    for chunk in edges.chunks(7_500) {
        store
            .put_batch(&tenant, &caller, &[], chunk)
            .expect("put 7500 edges");
    }

    // Tombstone `purge_task` now (`t = base_ms`) and advance the clock by 20s so both
    // `n-002998` (tombstoned task node past 5s retention) and `n-002999` (ephemeral expired at +5s)
    // are ready for `purge_expired`, while `n-000000..n-002997` remain active.
    assert_eq!(
        store
            .tombstone_task_nodes(&tenant, purge_task)
            .expect("tombstone purge_task"),
        1
    );

    // Populate a second tenant with overlapping node/edge IDs so `tenant_stats` has >1 tenant.
    let other_nodes = vec![
        NewNode {
            id: Some(NodeId::new("n-000000").expect("nid")),
            node_type: node_type.clone(),
            label: "uniquetokenneedle other".to_owned(),
            summary: Some("other tenant".to_owned()),
            props: JsonMap::new(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new("other"),
        },
        NewNode {
            id: Some(NodeId::new("n-000001").expect("nid")),
            node_type,
            label: "other second".to_owned(),
            summary: None,
            props: JsonMap::new(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new("other"),
        },
    ];
    let other_edges = vec![NewEdge {
        id: Some(EdgeId::new("e-000000").expect("eid")),
        edge_type,
        from: NodeId::new("n-000000").expect("from"),
        to: NodeId::new("n-000001").expect("to"),
        props: JsonMap::new(),
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("other"),
    }];
    store
        .put_batch(&other_tenant, &caller, &other_nodes, &other_edges)
        .expect("put other tenant");

    clock.advance(Duration::from_secs(20));

    assert_never_analysed(&store);

    HotPathFixture {
        _dir: dir,
        clock,
        store,
        tenant,
        caller,
        active_task,
        sample_edge_id: EdgeId::new("e-000000").expect("sample edge"),
    }
}

fn assert_conn_never_analysed(conn: &Connection) {
    let stat_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'sqlite_stat%'",
            [],
            |r| r.get(0),
        )
        .expect("query sqlite_stat tables");
    assert_eq!(
        stat_tables, 0,
        "database must never have been ANALYZEd (`sqlite_stat1` must not exist)"
    );
}

fn assert_never_analysed(store: &SqliteContextStore) {
    {
        let writer = lock_unpoisoned(&store.writer);
        assert_conn_never_analysed(&writer);
    }
    let guard = store.readers.checkout();
    let reader = guard.conn().expect("reader conn");
    assert_conn_never_analysed(reader);
}

fn explain_query_plan(conn: &Connection, sql: &str) -> Vec<String> {
    let eqp_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stmt = conn.prepare(&eqp_sql).expect("prepare EXPLAIN QUERY PLAN");
    let param_count = stmt.parameter_count();
    let dummy_params = rusqlite::params_from_iter(std::iter::repeat_n("[]", param_count));
    let rows = stmt
        .query_map(dummy_params, |row| row.get::<_, String>(3))
        .expect("run EXPLAIN QUERY PLAN");
    let mut details = Vec::new();
    for item in rows {
        details.push(item.expect("eqp detail"));
    }
    assert!(
        !details.is_empty(),
        "EXPLAIN QUERY PLAN returned no rows for: {sql}"
    );
    details
}

/// Asserts that `details` never performs a full or tenant-only scan of `nodes` or `edges`.
fn assert_no_table_or_tenant_scan(stmt_label: &str, details: &[String]) {
    let node_aliases = ["nodes", "n", "n0", "nv", "other", "nf", "nt"];
    let edge_aliases = ["edges", "e"];

    for detail in details {
        for alias in node_aliases {
            let scan_prefix = format!("SCAN {alias}");
            assert!(
                detail != &scan_prefix && !detail.starts_with(&format!("{scan_prefix} ")),
                "{stmt_label}: forbidden table/index scan on nodes (`{detail}`) in plan {details:#?}"
            );
            let search_prefix = format!("SEARCH {alias} ");
            if detail.starts_with(&search_prefix) {
                assert!(
                    detail.contains("USING INDEX ")
                        || detail.contains("USING COVERING INDEX ")
                        || detail.contains("USING INTEGER PRIMARY KEY (rowid=?)"),
                    "{stmt_label}: nodes access must use an index or rowid (`{detail}`) in plan {details:#?}"
                );
                assert!(
                    !detail.ends_with("(tenant=?)") && !detail.ends_with("(tenant=? AND scope=?)"),
                    "{stmt_label}: nodes access degraded to unselective tenant/scope scan (`{detail}`) in plan {details:#?}"
                );
            }
        }

        for alias in edge_aliases {
            let scan_prefix = format!("SCAN {alias}");
            assert!(
                detail != &scan_prefix && !detail.starts_with(&format!("{scan_prefix} ")),
                "{stmt_label}: forbidden table/index scan on edges (`{detail}`) in plan {details:#?}"
            );
            let search_prefix = format!("SEARCH {alias} ");
            if detail.starts_with(&search_prefix) {
                assert!(
                    detail.contains("USING INDEX ")
                        || detail.contains("USING COVERING INDEX ")
                        || detail.contains("USING INTEGER PRIMARY KEY (rowid=?)"),
                    "{stmt_label}: edges access must use an index or rowid (`{detail}`) in plan {details:#?}"
                );
                assert!(
                    !detail.ends_with("(tenant=?)"),
                    "{stmt_label}: edges access degraded to tenant-wide scan (`{detail}`) in plan {details:#?}"
                );
            }
        }
    }
}

fn assert_traversal_plan_invariants(conn: &Connection) {
    // 1. Start node lookup.
    let start_plan = explain_query_plan(conn, traverse::SELECT_START_NODE_SQL);
    assert_no_table_or_tenant_scan("SELECT_START_NODE_SQL", &start_plan);
    assert!(
        start_plan.iter().any(|d| {
            d.starts_with("SEARCH nodes USING INDEX idx_nodes_temporal (tenant=? AND id=?")
        }),
        "SELECT_START_NODE_SQL must probe idx_nodes_temporal by (tenant, id): {start_plan:#?}"
    );

    // 2. Recursive CTEs (`BFS` and `SHORTEST`) for OUTBOUND, INBOUND, and BOTH.
    for (dir, expected_edge_indexes) in [
        (Direction::Outbound, vec![("idx_edges_from", "from_id=?")]),
        (Direction::Inbound, vec![("idx_edges_to", "to_id=?")]),
        (
            Direction::Both,
            vec![("idx_edges_from", "from_id=?"), ("idx_edges_to", "to_id=?")],
        ),
    ] {
        let label = format!("CTE_{dir:?}");
        let cte_plan = explain_query_plan(conn, traverse::cte_sql_for_direction(dir));
        assert_no_table_or_tenant_scan(&label, &cte_plan);

        // Anchor must probe `n0` via `idx_nodes_temporal (tenant=? AND id=?...)`.
        assert!(
            cte_plan.iter().any(|d| {
                d.starts_with("SEARCH n0 USING INDEX idx_nodes_temporal (tenant=? AND id=?")
            }),
            "{label}: anchor must probe n0 via idx_nodes_temporal: {cte_plan:#?}"
        );

        // Collect every `edges` access and every recursive step join order.
        let edge_rows: Vec<(usize, &String)> = cte_plan
            .iter()
            .enumerate()
            .filter(|(_, d)| d.starts_with("SEARCH e ") || d.starts_with("SEARCH edges "))
            .collect();
        assert_eq!(
            edge_rows.len(),
            expected_edge_indexes.len(),
            "{label}: unexpected number of edge probes in {cte_plan:#?}"
        );

        let nv_rows: Vec<(usize, &String)> = cte_plan
            .iter()
            .enumerate()
            .filter(|(_, d)| d.starts_with("SEARCH nv "))
            .collect();
        assert_eq!(
            nv_rows.len(),
            expected_edge_indexes.len(),
            "{label}: unexpected number of target-node (nv) probes in {cte_plan:#?}"
        );

        let scan_bfs_positions: Vec<usize> = cte_plan
            .iter()
            .enumerate()
            .filter(|(_, d)| *d == "SCAN bfs")
            .map(|(idx, _)| idx)
            .collect();
        assert!(
            scan_bfs_positions.len() >= expected_edge_indexes.len(),
            "{label}: each recursive term must begin with SCAN bfs: {cte_plan:#?}"
        );

        for (arm_idx, (expected_idx, expected_col)) in expected_edge_indexes.iter().enumerate() {
            let (e_pos, e_detail) = edge_rows[arm_idx];
            let (nv_pos, nv_detail) = nv_rows[arm_idx];
            let bfs_pos = scan_bfs_positions[arm_idx];

            assert!(
                e_detail.contains(&format!(
                    "USING INDEX {expected_idx} (tenant=? AND {expected_col}"
                )),
                "{label} arm {arm_idx}: edge probe must use {expected_idx} with {expected_col}, got `{e_detail}` in {cte_plan:#?}"
            );
            assert!(
                nv_detail
                    .starts_with("SEARCH nv USING INDEX idx_nodes_temporal (tenant=? AND id=?"),
                "{label} arm {arm_idx}: target-node probe must use idx_nodes_temporal (tenant=?, id=?), got `{nv_detail}` in {cte_plan:#?}"
            );
            assert!(
                bfs_pos < e_pos && e_pos < nv_pos,
                "{label} arm {arm_idx}: join order must be `bfs -> e -> nv` ({bfs_pos} < {e_pos} < {nv_pos}) in {cte_plan:#?}"
            );
        }
    }

    // 3. Step neighbour queries (`DFS`, `WEIGHTED`, `BFS` page edges, `SHORTEST` predecessors).
    for (dir, expected_edge_indexes) in [
        (Direction::Outbound, vec![("idx_edges_from", "from_id=?")]),
        (Direction::Inbound, vec![("idx_edges_to", "to_id=?")]),
        (
            Direction::Both,
            vec![("idx_edges_from", "from_id=?"), ("idx_edges_to", "to_id=?")],
        ),
    ] {
        let label = format!("NEIGHBOURS_{dir:?}");
        let step_plan = explain_query_plan(conn, traverse::step_sql_for_direction(dir));
        assert_no_table_or_tenant_scan(&label, &step_plan);

        let edge_rows: Vec<(usize, &String)> = step_plan
            .iter()
            .enumerate()
            .filter(|(_, d)| d.starts_with("SEARCH e ") || d.starts_with("SEARCH edges "))
            .collect();
        let nv_rows: Vec<(usize, &String)> = step_plan
            .iter()
            .enumerate()
            .filter(|(_, d)| d.starts_with("SEARCH nv "))
            .collect();
        assert_eq!(
            edge_rows.len(),
            expected_edge_indexes.len(),
            "{label}: {step_plan:#?}"
        );
        assert_eq!(
            nv_rows.len(),
            expected_edge_indexes.len(),
            "{label}: {step_plan:#?}"
        );

        for (arm_idx, (expected_idx, expected_col)) in expected_edge_indexes.iter().enumerate() {
            let (e_pos, e_detail) = edge_rows[arm_idx];
            let (nv_pos, nv_detail) = nv_rows[arm_idx];
            assert!(
                e_detail.contains(&format!(
                    "USING INDEX {expected_idx} (tenant=? AND {expected_col}"
                )),
                "{label} arm {arm_idx}: edge probe must use {expected_idx} with {expected_col}, got `{e_detail}`"
            );
            assert!(
                nv_detail
                    .starts_with("SEARCH nv USING INDEX idx_nodes_temporal (tenant=? AND id=?"),
                "{label} arm {arm_idx}: nv probe must use idx_nodes_temporal, got `{nv_detail}`"
            );
            assert!(
                e_pos < nv_pos,
                "{label} arm {arm_idx}: join order must be `e -> nv` ({e_pos} < {nv_pos}) in {step_plan:#?}"
            );
        }
    }
}

fn assert_search_nodes_plan_invariants(conn: &Connection, fts_table: &str) {
    let search_sql = search::build_search_nodes_fts_sql(fts_table);
    let search_plan = explain_query_plan(conn, &search_sql);
    assert_no_table_or_tenant_scan("search_nodes", &search_plan);
    let fts_pos = search_plan
        .iter()
        .position(|d| d.starts_with(&format!("SCAN {fts_table} VIRTUAL TABLE INDEX")))
        .unwrap_or_else(|| {
            panic!("search_nodes must drive from FTS5 virtual table: {search_plan:#?}")
        });
    let n_pos = search_plan
        .iter()
        .position(|d| d == "SEARCH n USING INTEGER PRIMARY KEY (rowid=?)")
        .unwrap_or_else(|| {
            panic!("search_nodes must join `nodes` by INTEGER PRIMARY KEY: {search_plan:#?}")
        });
    assert!(
        fts_pos < n_pos,
        "search_nodes join order must be `{fts_table} -> nodes(rowid)` ({fts_pos} < {n_pos}): {search_plan:#?}"
    );
}

fn assert_other_hot_path_plan_invariants(conn: &Connection, fts_table: &str) {
    // `get_node`
    let get_node_plan = explain_query_plan(conn, crud::SELECT_NODE_AS_OF_SQL);
    assert_no_table_or_tenant_scan("SELECT_NODE_AS_OF_SQL", &get_node_plan);
    assert!(
        get_node_plan.iter().any(|d| {
            d.starts_with("SEARCH nodes USING INDEX idx_nodes_temporal (tenant=? AND id=?")
        }),
        "get_node plan: {get_node_plan:#?}"
    );

    // `get_edge`
    let get_edge_plan = explain_query_plan(conn, crud::SELECT_EDGE_AS_OF_SQL);
    assert_no_table_or_tenant_scan("SELECT_EDGE_AS_OF_SQL", &get_edge_plan);
    assert!(
        get_edge_plan.iter().any(|d| {
            d.starts_with("SEARCH edges USING INDEX idx_edges_id_temporal (tenant=? AND id=?")
        }),
        "get_edge plan: {get_edge_plan:#?}"
    );

    // `put_batch` lookups & updates
    let stats_plan = explain_query_plan(conn, crud::SELECT_TENANT_STATS_SQL);
    assert_no_table_or_tenant_scan("SELECT_TENANT_STATS_SQL", &stats_plan);
    assert!(
        stats_plan
            .iter()
            .any(|d| d.contains("SEARCH tenant_stats USING PRIMARY KEY (tenant=?)")),
        "tenant_stats lookup plan: {stats_plan:#?}"
    );

    let cur_node_plan = explain_query_plan(conn, crud::SELECT_CURRENT_NODE_META_SQL);
    assert_no_table_or_tenant_scan("SELECT_CURRENT_NODE_META_SQL", &cur_node_plan);
    assert!(
        cur_node_plan.iter().any(|d| {
            d.starts_with("SEARCH nodes USING INDEX idx_nodes_current (tenant=? AND id=?)")
        }),
        "put_batch current node lookup plan: {cur_node_plan:#?}"
    );

    let close_node_plan = explain_query_plan(conn, crud::CLOSE_NODE_BY_ROWID_SQL);
    assert_no_table_or_tenant_scan("CLOSE_NODE_BY_ROWID_SQL", &close_node_plan);
    assert!(
        close_node_plan
            .iter()
            .any(|d| d == "SEARCH nodes USING INTEGER PRIMARY KEY (rowid=?)"),
        "close node by rowid plan: {close_node_plan:#?}"
    );

    let cur_edge_plan = explain_query_plan(conn, crud::SELECT_CURRENT_EDGE_META_SQL);
    assert_no_table_or_tenant_scan("SELECT_CURRENT_EDGE_META_SQL", &cur_edge_plan);
    assert!(
        cur_edge_plan.iter().any(|d| {
            d.starts_with(
                "SEARCH edges USING INDEX idx_edges_current_quad (tenant=? AND edge_type=? AND from_id=? AND to_id=?)",
            )
        }),
        "put_batch current edge lookup plan: {cur_edge_plan:#?}"
    );

    let close_edge_plan = explain_query_plan(conn, crud::CLOSE_EDGE_BY_ROWID_SQL);
    assert_no_table_or_tenant_scan("CLOSE_EDGE_BY_ROWID_SQL", &close_edge_plan);
    assert!(
        close_edge_plan
            .iter()
            .any(|d| d == "SEARCH edges USING INTEGER PRIMARY KEY (rowid=?)"),
        "close edge by rowid plan: {close_edge_plan:#?}"
    );

    // `search_nodes` (join from per-tenant FTS5 virtual table to `nodes`)
    assert_search_nodes_plan_invariants(conn, fts_table);

    // `tombstone_node` incident edge updates
    let close_out_plan = explain_query_plan(conn, crud::CLOSE_OUTBOUND_EDGES_SQL);
    assert_no_table_or_tenant_scan("CLOSE_OUTBOUND_EDGES_SQL", &close_out_plan);
    assert!(
        close_out_plan.iter().any(|d| {
            d.starts_with("SEARCH edges USING INDEX idx_edges_from (tenant=? AND from_id=?)")
        }),
        "CLOSE_OUTBOUND_EDGES_SQL plan: {close_out_plan:#?}"
    );

    let close_in_plan = explain_query_plan(conn, crud::CLOSE_INBOUND_EDGES_SQL);
    assert_no_table_or_tenant_scan("CLOSE_INBOUND_EDGES_SQL", &close_in_plan);
    assert!(
        close_in_plan.iter().any(|d| {
            d.starts_with("SEARCH edges USING INDEX idx_edges_to (tenant=? AND to_id=?)")
        }),
        "CLOSE_INBOUND_EDGES_SQL plan: {close_in_plan:#?}"
    );

    // `delete_node` lookups, incident edge count/deletes, and node version deletes
    let latest_ver_plan = explain_query_plan(conn, crud::SELECT_LATEST_NODE_VERSION_SQL);
    assert_no_table_or_tenant_scan("SELECT_LATEST_NODE_VERSION_SQL", &latest_ver_plan);
    assert!(
        latest_ver_plan.iter().any(|d| {
            d.starts_with("SEARCH nodes USING INDEX idx_nodes_temporal (tenant=? AND id=?)")
        }),
        "SELECT_LATEST_NODE_VERSION_SQL plan: {latest_ver_plan:#?}"
    );

    let del_vers_sel_plan = explain_query_plan(conn, crud::SELECT_NODE_VERSIONS_FOR_DELETE_SQL);
    assert_no_table_or_tenant_scan("SELECT_NODE_VERSIONS_FOR_DELETE_SQL", &del_vers_sel_plan);
    assert!(
        del_vers_sel_plan.iter().any(|d| {
            d.starts_with("SEARCH nodes USING INDEX idx_nodes_temporal (tenant=? AND id=?)")
                || d.starts_with(
                    "SEARCH nodes USING COVERING INDEX idx_nodes_temporal (tenant=? AND id=?)",
                )
        }),
        "SELECT_NODE_VERSIONS_FOR_DELETE_SQL plan: {del_vers_sel_plan:#?}"
    );

    let cnt_inc_plan = explain_query_plan(conn, crud::COUNT_ACTIVE_INCIDENT_EDGES_SQL);
    assert_no_table_or_tenant_scan("COUNT_ACTIVE_INCIDENT_EDGES_SQL", &cnt_inc_plan);
    assert!(
        cnt_inc_plan
            .iter()
            .any(|d| { d.contains("idx_edges_from (tenant=? AND from_id=?)") }),
        "COUNT_ACTIVE_INCIDENT_EDGES_SQL outbound arm: {cnt_inc_plan:#?}"
    );
    assert!(
        cnt_inc_plan
            .iter()
            .any(|d| d.contains("idx_edges_to (tenant=? AND to_id=?)")),
        "COUNT_ACTIVE_INCIDENT_EDGES_SQL inbound arm: {cnt_inc_plan:#?}"
    );

    let del_out_plan = explain_query_plan(conn, crud::DELETE_OUTBOUND_EDGES_SQL);
    assert_no_table_or_tenant_scan("DELETE_OUTBOUND_EDGES_SQL", &del_out_plan);
    assert!(
        del_out_plan.iter().any(|d| {
            d.starts_with("SEARCH edges USING INDEX idx_edges_from (tenant=? AND from_id=?)")
                || d.starts_with(
                    "SEARCH edges USING COVERING INDEX idx_edges_from (tenant=? AND from_id=?)",
                )
        }),
        "DELETE_OUTBOUND_EDGES_SQL plan: {del_out_plan:#?}"
    );

    let del_in_plan = explain_query_plan(conn, crud::DELETE_INBOUND_EDGES_SQL);
    assert_no_table_or_tenant_scan("DELETE_INBOUND_EDGES_SQL", &del_in_plan);
    assert!(
        del_in_plan.iter().any(|d| {
            d.starts_with("SEARCH edges USING INDEX idx_edges_to (tenant=? AND to_id=?)")
        }),
        "DELETE_INBOUND_EDGES_SQL plan: {del_in_plan:#?}"
    );

    let del_node_plan = explain_query_plan(conn, crud::DELETE_NODE_VERSIONS_SQL);
    assert_no_table_or_tenant_scan("DELETE_NODE_VERSIONS_SQL", &del_node_plan);
    assert!(
        del_node_plan.iter().any(|d| {
            d.starts_with("SEARCH nodes USING INDEX idx_nodes_temporal (tenant=? AND id=?)")
                || d.starts_with(
                    "SEARCH nodes USING COVERING INDEX idx_nodes_temporal (tenant=? AND id=?)",
                )
        }),
        "DELETE_NODE_VERSIONS_SQL plan: {del_node_plan:#?}"
    );

    // `tombstone_task_nodes`
    let act_task_plan = explain_query_plan(conn, maintenance::SELECT_ACTIVE_TASK_NODES_SQL);
    assert_no_table_or_tenant_scan("SELECT_ACTIVE_TASK_NODES_SQL", &act_task_plan);
    assert!(
        act_task_plan.iter().any(|d| {
            d.starts_with(
                "SEARCH nodes USING INDEX idx_nodes_base_task (tenant=? AND base_scope=? AND task_id=? AND valid_to=?)",
            )
        }),
        "SELECT_ACTIVE_TASK_NODES_SQL plan: {act_task_plan:#?}"
    );

    let close_task_plan = explain_query_plan(conn, maintenance::CLOSE_TASK_NODE_ROW_SQL);
    assert_no_table_or_tenant_scan("CLOSE_TASK_NODE_ROW_SQL", &close_task_plan);
    assert!(
        close_task_plan
            .iter()
            .any(|d| d == "SEARCH nodes USING INTEGER PRIMARY KEY (rowid=?)"),
        "CLOSE_TASK_NODE_ROW_SQL plan: {close_task_plan:#?}"
    );

    // `purge_expired`
    let tenants_plan = explain_query_plan(conn, maintenance::LIST_KNOWN_TENANTS_SQL);
    assert_no_table_or_tenant_scan("LIST_KNOWN_TENANTS_SQL", &tenants_plan);
    assert!(
        tenants_plan
            .iter()
            .any(|d| d.starts_with("SCAN tenant_stats")),
        "LIST_KNOWN_TENANTS_SQL must enumerate known tenants from `tenant_stats`, never `nodes`: {tenants_plan:#?}"
    );

    let exp_eph_plan = explain_query_plan(conn, maintenance::SELECT_EXPIRED_EPHEMERAL_ROWS_SQL);
    assert_no_table_or_tenant_scan("SELECT_EXPIRED_EPHEMERAL_ROWS_SQL", &exp_eph_plan);
    assert!(
        exp_eph_plan.iter().any(|d| {
            d.starts_with(
                "SEARCH nodes USING INDEX idx_nodes_ephemeral (tenant=? AND scope=? AND expires_at>?",
            )
        }),
        "SELECT_EXPIRED_EPHEMERAL_ROWS_SQL plan: {exp_eph_plan:#?}"
    );

    let exp_task_plan = explain_query_plan(conn, maintenance::SELECT_EXPIRED_TASK_ROWS_SQL);
    assert_no_table_or_tenant_scan("SELECT_EXPIRED_TASK_ROWS_SQL", &exp_task_plan);
    assert!(
        exp_task_plan.iter().any(|d| {
            d.starts_with(
                "SEARCH n USING INDEX idx_nodes_task_purge (tenant=? AND scope=? AND valid_to>?",
            )
        }),
        "SELECT_EXPIRED_TASK_ROWS_SQL outer range scan must use idx_nodes_task_purge: {exp_task_plan:#?}"
    );
    assert!(
        exp_task_plan.iter().any(|d| {
            d.starts_with("SEARCH other USING INDEX idx_nodes_temporal (tenant=? AND id=?)")
                || d.starts_with(
                    "SEARCH other USING COVERING INDEX idx_nodes_temporal (tenant=? AND id=?)",
                )
        }),
        "SELECT_EXPIRED_TASK_ROWS_SQL anti-join must probe idx_nodes_temporal: {exp_task_plan:#?}"
    );

    let del_purged_plan = explain_query_plan(conn, maintenance::DELETE_PURGED_NODE_ROW_SQL);
    assert_no_table_or_tenant_scan("DELETE_PURGED_NODE_ROW_SQL", &del_purged_plan);
    assert!(
        del_purged_plan
            .iter()
            .any(|d| d == "SEARCH nodes USING INTEGER PRIMARY KEY (rowid=?)"),
        "DELETE_PURGED_NODE_ROW_SQL plan: {del_purged_plan:#?}"
    );

    let rem_ver_plan = explain_query_plan(conn, maintenance::NODE_HAS_ANY_REMAINING_VERSION_SQL);
    assert_no_table_or_tenant_scan("NODE_HAS_ANY_REMAINING_VERSION_SQL", &rem_ver_plan);
    assert!(
        rem_ver_plan
            .iter()
            .any(|d| { d.contains("idx_nodes_temporal (tenant=? AND id=?)") }),
        "NODE_HAS_ANY_REMAINING_VERSION_SQL plan: {rem_ver_plan:#?}"
    );

    // Documented exemption (`ING-088` / `WP-M2-01-review-3.md` item 3):
    // `ContextStore::counts` (`crud::COUNTS_AS_OF_SQL`) computes caller-visible node and edge
    // totals across the tenant at `as_of`, which is proportional to the tenant's rows by
    // definition. Even so, `INV-09` requires tenant-partitioned `SEARCH` probes rather than
    // cross-tenant full-table `SCAN nodes` or `SCAN edges`.
    let counts_plan = explain_query_plan(conn, crud::COUNTS_AS_OF_SQL);
    for detail in &counts_plan {
        assert!(
            !detail.starts_with("SCAN nodes")
                && !detail.starts_with("SCAN n ")
                && !detail.starts_with("SCAN edges")
                && !detail.starts_with("SCAN e "),
            "COUNTS_AS_OF_SQL must still partition by tenant index (`INV-09`), got `{detail}` in {counts_plan:#?}"
        );
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct StmtCounters {
    runs: i64,
    fullscan_steps: i64,
    autoindex_steps: i64,
    vm_steps: i64,
}

fn collect_cached_counters(conn: &Connection, sqls: &[&str]) -> StmtCounters {
    let mut totals = StmtCounters::default();
    for sql in sqls {
        let stmt = conn
            .prepare_cached(sql)
            .expect("prepare_cached counter read");
        let runs = i64::from(stmt.get_status(StatementStatus::Run));
        if runs > 0 {
            totals.runs += runs;
            totals.fullscan_steps += i64::from(stmt.get_status(StatementStatus::FullscanStep));
            totals.autoindex_steps += i64::from(stmt.get_status(StatementStatus::AutoIndex));
            totals.vm_steps += i64::from(stmt.get_status(StatementStatus::VmStep));
        }
    }
    totals
}

// spec: ING-088, INV-09
#[test]
fn ing_088_traversal_and_hot_path_plans_are_index_driven_without_analyze() {
    let fixture = build_never_analysed_3k_15k_fixture();
    assert_never_analysed(&fixture.store);

    let fts_table = schema::tenant_fts_table_name(fixture.tenant.tenant().as_str());
    let guard = fixture.store.readers.checkout();
    let reader = guard.conn().expect("reader conn");

    assert_traversal_plan_invariants(reader);
    assert_other_hot_path_plan_invariants(reader, &fts_table);

    // Re-verify `sqlite_stat1` was never created by any plan check.
    drop(guard);
    assert_never_analysed(&fixture.store);
}

// spec: ING-088, INV-09
#[test]
fn ing_088_traversal_work_counters_have_zero_fullscan_steps_and_bounded_rows_read() {
    let fixture = build_never_analysed_3k_15k_fixture();
    assert_never_analysed(&fixture.store);

    let start = NodeId::new("n-000000").expect("start");
    let traversal_sqls: [&str; 7] = [
        traverse::SELECT_START_NODE_SQL,
        traverse::CTE_OUTBOUND_SQL,
        traverse::CTE_INBOUND_SQL,
        traverse::CTE_BOTH_SQL,
        traverse::NEIGHBOURS_OUTBOUND_SQL,
        traverse::NEIGHBOURS_INBOUND_SQL,
        traverse::NEIGHBOURS_BOTH_SQL,
    ];

    // Literal VM-step ceiling proportional to `visited_cap` (10) times degree:
    // - Graph has 3,000 nodes and 15,000 edges (average out/in degree = 5, undirected degree = 10).
    // - With `visited_cap = 10` and `max_depth = 4`, at most `(visited_cap + 1) * (max_depth + 1) = 55`
    //   CTE queue rows (or `~11` settled/emitted nodes in DFS/WEIGHTED) are expanded, plus 10 page
    //   node lookups and <= 10 two-arm neighbour/predecessor lookups (up to 21 statement runs in
    //   `Shortest Both`, ~34k VDBE instructions total including JSON claim checks).
    // - `50_000` VM steps bounds total SQLite work strictly by `O(visited_cap * degree)`; by contrast,
    //   an unindexed scan of 3,000 nodes or 15,000 edges per recursive depth step executes hundreds
    //   of thousands to millions of VDBE instructions.
    const MAX_BOUNDED_TRAVERSAL_VM_STEPS: i64 = 50_000;

    for strategy in [
        TraverseStrategy::Bfs,
        TraverseStrategy::Shortest,
        TraverseStrategy::Dfs,
        TraverseStrategy::Weighted,
    ] {
        for direction in [Direction::Outbound, Direction::Inbound, Direction::Both] {
            {
                let guard = fixture.store.readers.checkout();
                let reader = guard.conn().expect("reader");
                reader.flush_prepared_statement_cache();
                reader.set_prepared_statement_cache_capacity(64);
            }

            let mut query = TraverseQuery::new(start.clone());
            query.strategy = strategy;
            query.direction = direction;
            query.max_depth = 4;
            query.first = 100;
            query.visited_cap = Some(10);

            let res = fixture
                .store
                .traverse(&fixture.tenant, &fixture.caller, &query)
                .unwrap_or_else(|e| panic!("traverse {strategy:?} {direction:?} failed: {e:?}"));
            assert_eq!(
                res.stats.visited, 10,
                "{strategy:?} {direction:?} must visit exactly visited_cap=10 nodes"
            );
            assert!(
                res.truncated,
                "{strategy:?} {direction:?} on 3k/15k graph must truncate at visited_cap=10"
            );

            let counters = {
                let guard = fixture.store.readers.checkout();
                let reader = guard.conn().expect("reader");
                collect_cached_counters(reader, &traversal_sqls)
            };

            assert!(
                counters.runs > 0,
                "{strategy:?} {direction:?}: expected cached statement runs > 0, got {counters:?}"
            );
            assert_eq!(
                counters.fullscan_steps, 0,
                "{strategy:?} {direction:?}: FullscanStep must be 0, got {counters:?}"
            );
            assert_eq!(
                counters.autoindex_steps, 0,
                "{strategy:?} {direction:?}: AutoIndex must be 0, got {counters:?}"
            );
            assert!(
                counters.vm_steps <= MAX_BOUNDED_TRAVERSAL_VM_STEPS,
                "{strategy:?} {direction:?}: VmStep ({}) exceeded O(visited_cap * degree) bound ({MAX_BOUNDED_TRAVERSAL_VM_STEPS}): {counters:?}",
                counters.vm_steps
            );
        }
    }

    // Exercise the remaining single-entity / bounded-result hot paths on the same 3,000-node,
    // 15,000-edge never-analysed database and verify `FullscanStep == 0` and bounded `VmStep`.
    let fts_table = schema::tenant_fts_table_name(fixture.tenant.tenant().as_str());
    let search_sql = search::build_search_nodes_fts_sql(&fts_table);
    let insert_fts_sql = crud::build_insert_fts_row_sql(&fts_table);
    let delete_fts_sql = crud::build_delete_fts_row_sql(&fts_table);

    {
        let guard = fixture.store.readers.checkout();
        let reader = guard.conn().expect("reader");
        reader.flush_prepared_statement_cache();
        reader.set_prepared_statement_cache_capacity(64);
    }

    let got_node = fixture
        .store
        .get_node(
            &fixture.tenant,
            &fixture.caller,
            &NodeId::new("n-000042").expect("nid"),
            None,
        )
        .expect("get_node")
        .expect("node 42 exists");
    assert_eq!(got_node.id.as_str(), "n-000042");

    let got_edge = fixture
        .store
        .get_edge(
            &fixture.tenant,
            &fixture.caller,
            &fixture.sample_edge_id,
            None,
        )
        .expect("get_edge")
        .expect("sample edge exists");
    assert_eq!(got_edge.id, fixture.sample_edge_id);

    let hits = fixture
        .store
        .search_nodes(
            &fixture.tenant,
            &fixture.caller,
            "uniquetokenneedle",
            10,
            None,
        )
        .expect("search_nodes");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id.as_str(), "n-000042");

    {
        let guard = fixture.store.readers.checkout();
        let reader = guard.conn().expect("reader");
        let read_counters = collect_cached_counters(
            reader,
            &[
                crud::SELECT_NODE_AS_OF_SQL,
                crud::SELECT_EDGE_AS_OF_SQL,
                search_sql.as_str(),
            ],
        );
        assert!(
            read_counters.runs >= 3,
            "read hot paths runs: {read_counters:?}"
        );
        assert_eq!(
            read_counters.fullscan_steps, 0,
            "get_node / get_edge / search_nodes FullscanStep must be 0: {read_counters:?}"
        );
        assert_eq!(read_counters.autoindex_steps, 0);
        assert!(
            read_counters.vm_steps <= 2_000,
            "point read / 1-hit search VmStep exceeded 2,000: {read_counters:?}"
        );
    }

    // Writer hot paths: 1-node/1-edge `put_batch`, `tombstone_node`, `delete_node`,
    // `tombstone_task_nodes`, and `purge_expired`.
    {
        let writer = lock_unpoisoned(&fixture.store.writer);
        writer.flush_prepared_statement_cache();
        writer.set_prepared_statement_cache_capacity(64);
    }

    fixture.clock.advance(Duration::from_secs(1));
    let updated_node = NewNode {
        id: Some(NodeId::new("n-000100").expect("nid")),
        node_type: NodeType::new("Entity").expect("nt"),
        label: "updated node 100".to_owned(),
        summary: Some("updated summary".to_owned()),
        props: JsonMap::new(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("ing-088-update"),
    };
    fixture
        .store
        .put_batch(&fixture.tenant, &fixture.caller, &[updated_node], &[])
        .expect("single-item put_batch update");

    fixture
        .store
        .tombstone_node(
            &fixture.tenant,
            &fixture.caller,
            &NodeId::new("n-000101").expect("nid"),
        )
        .expect("tombstone_node");

    fixture
        .store
        .delete_node(
            &fixture.tenant,
            &fixture.caller,
            &NodeId::new("n-000102").expect("nid"),
        )
        .expect("delete_node");

    let closed_task = fixture
        .store
        .tombstone_task_nodes(&fixture.tenant, fixture.active_task)
        .expect("tombstone_task_nodes");
    assert_eq!(closed_task, 1);

    let purge_report = fixture
        .store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(5),
        })
        .expect("purge_expired");
    assert_eq!(purge_report.purged_ephemeral_nodes, 1);
    assert_eq!(purge_report.purged_task_nodes, 1);

    {
        let writer = lock_unpoisoned(&fixture.store.writer);
        // Check all `nodes`, `edges`, and `fts` statements executed on the writer (`FullscanStep == 0`).
        let writer_table_sqls: [&str; 19] = [
            crud::SELECT_TENANT_STATS_SQL,
            crud::UPSERT_TENANT_STATS_SQL,
            crud::SELECT_CURRENT_NODE_META_SQL,
            crud::CLOSE_NODE_BY_ROWID_SQL,
            crud::INSERT_NODE_SQL,
            crud::CLOSE_OUTBOUND_EDGES_SQL,
            crud::CLOSE_INBOUND_EDGES_SQL,
            crud::SELECT_LATEST_NODE_VERSION_SQL,
            crud::SELECT_NODE_VERSIONS_FOR_DELETE_SQL,
            crud::COUNT_ACTIVE_INCIDENT_EDGES_SQL,
            crud::DELETE_OUTBOUND_EDGES_SQL,
            crud::DELETE_INBOUND_EDGES_SQL,
            crud::DELETE_NODE_VERSIONS_SQL,
            maintenance::SELECT_ACTIVE_TASK_NODES_SQL,
            maintenance::CLOSE_TASK_NODE_ROW_SQL,
            maintenance::SELECT_EXPIRED_EPHEMERAL_ROWS_SQL,
            maintenance::SELECT_EXPIRED_TASK_ROWS_SQL,
            maintenance::DELETE_PURGED_NODE_ROW_SQL,
            maintenance::NODE_HAS_ANY_REMAINING_VERSION_SQL,
        ];
        let mut all_writer_sqls: Vec<&str> = writer_table_sqls.to_vec();
        all_writer_sqls.push(insert_fts_sql.as_str());
        all_writer_sqls.push(delete_fts_sql.as_str());

        let write_counters = collect_cached_counters(&writer, &all_writer_sqls);
        assert!(
            write_counters.runs >= 15,
            "expected writer hot-path statements to run: {write_counters:?}"
        );
        assert_eq!(
            write_counters.fullscan_steps, 0,
            "writer hot-path statements on nodes/edges must have FullscanStep == 0: {write_counters:?}"
        );
        assert_eq!(write_counters.autoindex_steps, 0);
        assert!(
            write_counters.vm_steps <= 10_000,
            "writer hot-path statements VmStep exceeded 10,000 on 3k/15k DB: {write_counters:?}"
        );
    }

    assert_never_analysed(&fixture.store);
}

// spec: ING-088, INV-09
#[test]
fn ing_088_plans_remain_locked_under_adversarial_statistics_and_decoy_indexes() {
    // Pass A (decoy-index statistics): real indexes report 10M rows while decoy indexes report
    // 100 rows (1 row per key). Removing `INDEXED BY` from any hot-path query causes SQLite to
    // choose a decoy index and fail the plan assertions.
    let mut conn = Connection::open_in_memory().expect("open in-memory");
    assert_eq!(
        schema::apply_migrations(&mut conn).expect("migrate"),
        schema::FORMAT_VERSION
    );

    let fts_table = schema::tenant_fts_table_name("alpha-tenant");
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE {fts_table} USING fts5(label, summary);
         CREATE INDEX idx_decoy_nodes_temporal ON nodes (tenant, id, valid_from, valid_to, label);
         CREATE INDEX idx_decoy_nodes_current ON nodes (tenant, id, scope) WHERE valid_to IS NULL;
         CREATE INDEX idx_decoy_nodes_base_task ON nodes (tenant, base_scope, task_id, valid_to, id);
         CREATE INDEX idx_decoy_nodes_ephemeral ON nodes (tenant, scope, expires_at, id);
         CREATE INDEX idx_decoy_nodes_task_purge ON nodes (tenant, scope, valid_to, id);
         CREATE INDEX idx_decoy_edges_from ON edges (tenant, from_id, valid_from, valid_to, id);
         CREATE INDEX idx_decoy_edges_to ON edges (tenant, to_id, valid_from, valid_to, id);
         CREATE INDEX idx_decoy_edges_id_temporal ON edges (tenant, id, valid_from, valid_to, edge_type);
         CREATE INDEX idx_decoy_edges_current_quad ON edges (tenant, edge_type, from_id, to_id, id) WHERE valid_to IS NULL;
         ANALYZE;
         DELETE FROM sqlite_stat1;
         INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
             ('nodes', NULL, '100'),
             ('nodes', 'idx_nodes_temporal', '10000000 10000000 5000000 5000000 5000000'),
             ('nodes', 'idx_nodes_current', '10000000 10000000 10000000'),
             ('nodes', 'idx_nodes_base_task', '10000000 10000000 5000000 5000000 5000000'),
             ('nodes', 'idx_nodes_ephemeral', '10000000 10000000 5000000 5000000'),
             ('nodes', 'idx_nodes_task_purge', '10000000 10000000 5000000 5000000'),
             ('nodes', 'idx_decoy_nodes_temporal', '100 100 1 1 1 1'),
             ('nodes', 'idx_decoy_nodes_current', '100 100 1 1'),
             ('nodes', 'idx_decoy_nodes_base_task', '100 100 1 1 1 1'),
             ('nodes', 'idx_decoy_nodes_ephemeral', '100 100 1 1 1'),
             ('nodes', 'idx_decoy_nodes_task_purge', '100 100 1 1 1'),
             ('edges', NULL, '10000000'),
             ('edges', 'idx_edges_from', '10000000 10000000 5000000 5000000 5000000'),
             ('edges', 'idx_edges_to', '10000000 10000000 5000000 5000000 5000000'),
             ('edges', 'idx_edges_id_temporal', '10000000 10000000 5000000 5000000 5000000'),
             ('edges', 'idx_edges_current_quad', '10000000 10000000 5000000 5000000 5000000'),
             ('edges', 'idx_decoy_edges_from', '100 100 1 1 1 1'),
             ('edges', 'idx_decoy_edges_to', '100 100 1 1 1 1'),
             ('edges', 'idx_decoy_edges_id_temporal', '100 100 1 1 1 1'),
             ('edges', 'idx_decoy_edges_current_quad', '100 100 1 1 1 1');
         ANALYZE sqlite_master;"
    ))
    .expect("install decoy indexes and Pass A adversarial sqlite_stat1");

    assert_traversal_plan_invariants(&conn);
    assert_other_hot_path_plan_invariants(&conn, &fts_table);

    // Pass B (reverse join-order statistics: tiny 5-row `nodes` vs 10M-row `edges` and FTS5's
    // 50,000-cost `MATCH` scan):
    // Even with `INDEXED BY` intact, removing `CROSS JOIN` before `nodes` in traversal CTEs,
    // step neighbour queries, or `build_search_nodes_fts_sql` flips `nodes (tenant=?)` ahead of
    // `edges` / `<fts_table>`.
    let mut conn_b = Connection::open_in_memory().expect("open in-memory B");
    assert_eq!(
        schema::apply_migrations(&mut conn_b).expect("migrate B"),
        schema::FORMAT_VERSION
    );
    conn_b
        .execute_batch(&format!(
            "CREATE VIRTUAL TABLE {fts_table} USING fts5(label, summary);
             ANALYZE;
             DELETE FROM sqlite_stat1;
             INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
                 ('nodes', NULL, '5'),
                 ('nodes', 'idx_nodes_temporal', '5 5 1 1 1'),
                 ('nodes', 'idx_nodes_current', '5 5 1'),
                 ('nodes', 'idx_nodes_task', '5 5 1 1 1'),
                 ('nodes', 'idx_nodes_base_task', '5 5 1 1 1'),
                 ('nodes', 'idx_nodes_ephemeral', '5 5 1 1'),
                 ('nodes', 'idx_nodes_task_purge', '5 5 1 1'),
                 ('edges', NULL, '10000000'),
                 ('edges', 'idx_edges_from', '10000000 10000000 10000000 10000000 10000000'),
                 ('edges', 'idx_edges_to', '10000000 10000000 10000000 10000000 10000000'),
                 ('edges', 'idx_edges_id_temporal', '10000000 10000000 10000000 10000000 10000000'),
                 ('edges', 'idx_edges_current_id', '10000000 10000000 10000000'),
                 ('edges', 'idx_edges_current_quad', '10000000 10000000 10000000 10000000 10000000');
             ANALYZE sqlite_master;"
        ))
        .expect("install Pass B adversarial sqlite_stat1");

    assert_traversal_plan_invariants(&conn_b);
    assert_other_hot_path_plan_invariants(&conn_b, &fts_table);

    // Pass C (huge `nodes` vs tiny `edges` and recursive queue `bfs`):
    // Removing `CROSS JOIN` before `edges` in traversal CTEs flips `edges` ahead of `bfs`, while
    // `search_nodes` must continue driving from FTS into `nodes` via `INTEGER PRIMARY KEY`.
    conn_b
        .execute_batch(
            "DELETE FROM sqlite_stat1;
             INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
                 ('nodes', NULL, '10000000'),
                 ('nodes', 'idx_nodes_temporal', '10000000 10000000 1 1 1'),
                 ('edges', NULL, '2'),
                 ('edges', 'idx_edges_from', '2 2 1 1 1'),
                 ('edges', 'idx_edges_to', '2 2 1 1 1');
             ANALYZE sqlite_master;",
        )
        .expect("install Pass C adversarial sqlite_stat1");

    assert_traversal_plan_invariants(&conn_b);
    assert_other_hot_path_plan_invariants(&conn_b, &fts_table);
}

struct DeltaStatsHarness {
    _dir: TempDir,
    clock: Arc<ManualClock>,
    store: SqliteContextStore,
    tenant_a: TenantScope,
    tenant_b: TenantScope,
    caller_a: Caller,
    active_task: TaskId,
    base_ms: i64,
}

fn build_delta_stats_harness() -> DeltaStatsHarness {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("ing_088_delta_stats.db");
    let mut cfg = SqliteConfig::new(&db_path);
    cfg.durability = Durability::Off;
    cfg.reader_pool_size = NonZeroUsize::new(1).expect("nonzero 1");

    let base_ms: i64 = 1_700_000_000_000;
    let clock = Arc::new(ManualClock::new(base_ms as u64));
    let clock_dyn: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let store = SqliteContextStore::open(cfg, clock_dyn).expect("open delta stats store");

    let tenant_a = TenantId::new("alpha-tenant").expect("tenant_a").scope();
    let tenant_b = TenantId::new("beta-tenant").expect("tenant_b").scope();

    let active_task =
        TaskId::parse("018f1000-0000-7000-8000-000000000099").expect("active task uuid");
    let mut caller_a = Caller::new("alice");
    caller_a.task_chain = vec![active_task];
    let caller_b = Caller::new("bob");

    // Populate `tenant_b` with 2 GLOBAL nodes, 1 long-lived EPHEMERAL node, and 2 active edges
    // (`(3, 2)` active counts) so every scenario verifies a second tenant stays untouched and
    // matches its raw active recount after every `purge_expired` call (`INV-09`).
    let b_nodes = vec![
        NewNode {
            id: Some(NodeId::new("b:global-1").expect("id")),
            node_type: NodeType::new("Entity").expect("nt"),
            label: "beta global 1".to_owned(),
            summary: Some("beta summary 1".to_owned()),
            props: JsonMap::new(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new("seed-b"),
        },
        NewNode {
            id: Some(NodeId::new("b:global-2").expect("id")),
            node_type: NodeType::new("Entity").expect("nt"),
            label: "beta global 2".to_owned(),
            summary: Some("beta summary 2".to_owned()),
            props: JsonMap::new(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new("seed-b"),
        },
        NewNode {
            id: None,
            node_type: NodeType::new("Scratch").expect("nt"),
            label: "beta long-lived ephemeral".to_owned(),
            summary: None,
            props: JsonMap::new(),
            scope: ScopeSpec::Ephemeral {
                base: BaseScope::Global,
                expires_at: Some(Timestamp::from_unix_millis(base_ms + 3_600_000).expect("ts")),
            },
            provenance: Provenance::new("seed-b"),
        },
    ];
    let b_batch = store
        .put_batch(&tenant_b, &caller_b, &b_nodes, &[])
        .expect("seed tenant_b nodes");
    let b_ephem_id = b_batch.node_ids[2].clone();
    let b_edges = vec![
        NewEdge {
            id: None,
            edge_type: EdgeType::new("LINKS").expect("et"),
            from: NodeId::new("b:global-1").expect("id"),
            to: NodeId::new("b:global-2").expect("id"),
            props: JsonMap::new(),
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: Provenance::new("seed-b"),
        },
        NewEdge {
            id: None,
            edge_type: EdgeType::new("LINKS").expect("et"),
            from: b_ephem_id,
            to: NodeId::new("b:global-1").expect("id"),
            props: JsonMap::new(),
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: Provenance::new("seed-b"),
        },
    ];
    store
        .put_batch(&tenant_b, &caller_b, &[], &b_edges)
        .expect("seed tenant_b edges");

    DeltaStatsHarness {
        _dir: dir,
        clock,
        store,
        tenant_a,
        tenant_b,
        caller_a,
        active_task,
        base_ms,
    }
}

fn read_tenant_stats_and_raw_recount(
    store: &SqliteContextStore,
    tenant: &TenantScope,
) -> ((u64, u64), (u64, u64)) {
    let t_str = tenant.tenant().as_str();
    let mut writer = lock_unpoisoned(&store.writer);
    let tx = writer.transaction().expect("open stats check tx");
    let stats = crud::read_tenant_stats(&tx, t_str).expect("read_tenant_stats");
    let raw_nodes: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE tenant = ?1 AND valid_to IS NULL",
            params![t_str],
            |r| r.get(0),
        )
        .expect("raw active nodes recount");
    let raw_edges: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE tenant = ?1 AND valid_to IS NULL",
            params![t_str],
            |r| r.get(0),
        )
        .expect("raw active edges recount");
    let recount = (
        u64::try_from(raw_nodes).expect("non-negative raw_nodes"),
        u64::try_from(raw_edges).expect("non-negative raw_edges"),
    );
    (stats, recount)
}

fn assert_all_tenants_stats_equal_recount(
    context: &str,
    store: &SqliteContextStore,
    expected_per_tenant: &[(&TenantScope, (u64, u64))],
) {
    for (tenant, expected) in expected_per_tenant {
        let (stats, recount) = read_tenant_stats_and_raw_recount(store, tenant);
        assert_eq!(
            stats,
            recount,
            "{context}: tenant `{}` delta-maintained tenant_stats {stats:?} != raw active recount {recount:?}",
            tenant.tenant().as_str()
        );
        assert_eq!(
            stats,
            *expected,
            "{context}: tenant `{}` tenant_stats {stats:?} != expected {expected:?}",
            tenant.tenant().as_str()
        );
    }
}

fn new_global_node(id: &str, label: &str) -> NewNode {
    NewNode {
        id: Some(NodeId::new(id).expect("global id")),
        node_type: NodeType::new("Entity").expect("nt"),
        label: label.to_owned(),
        summary: Some(format!("summary for {label}")),
        props: JsonMap::new(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("test"),
    }
}

fn new_ephemeral_node(label: &str, expires_at_ms: i64) -> NewNode {
    NewNode {
        id: None,
        node_type: NodeType::new("Scratch").expect("nt"),
        label: label.to_owned(),
        summary: Some(format!("ephemeral {label}")),
        props: JsonMap::new(),
        scope: ScopeSpec::Ephemeral {
            base: BaseScope::Global,
            expires_at: Some(Timestamp::from_unix_millis(expires_at_ms).expect("ts")),
        },
        provenance: Provenance::new("test"),
    }
}

fn new_links_edge(from: NodeId, to: NodeId) -> NewEdge {
    NewEdge {
        id: None,
        edge_type: EdgeType::new("LINKS").expect("et"),
        from,
        to,
        props: JsonMap::new(),
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("test"),
    }
}

// spec: ING-088
// spec: INV-09
#[test]
fn ing_088_purge_expired_decrements_tenant_stats_for_active_ephemeral_and_incident_edges() {
    let h = build_delta_stats_harness();
    let y_id = NodeId::new("a:global-y").expect("y");

    let batch = h
        .store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[
                new_global_node("a:global-y", "surviving Y"),
                new_ephemeral_node("expiring X", h.base_ms + 5_000),
            ],
            &[],
        )
        .expect("put X and Y");
    let x_id = batch.node_ids[1].clone();

    h.store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[],
            &[
                new_links_edge(x_id.clone(), y_id.clone()),
                new_links_edge(y_id.clone(), x_id),
            ],
        )
        .expect("put outbound and inbound edges on X");

    assert_all_tenants_stats_equal_recount(
        "before scenario 1 purge",
        &h.store,
        &[(&h.tenant_a, (2, 2)), (&h.tenant_b, (3, 2))],
    );

    h.clock.advance(Duration::from_secs(10));
    let report = h
        .store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(60),
        })
        .expect("purge_expired scenario 1");
    assert_eq!(report.purged_ephemeral_nodes, 1);
    assert_eq!(report.purged_edges, 2);

    // Node count drops by 1 (2 -> 1) and edge count drops by 2 (2 -> 0); `tenant_b` is untouched.
    assert_all_tenants_stats_equal_recount(
        "after scenario 1 purge",
        &h.store,
        &[(&h.tenant_a, (1, 0)), (&h.tenant_b, (3, 2))],
    );
}

// spec: ING-088
// spec: INV-09
#[test]
fn ing_088_purge_expired_counts_ephemeral_self_loop_once_in_tenant_stats() {
    let h = build_delta_stats_harness();
    let y_id = NodeId::new("a:global-y").expect("y");
    let z_id = NodeId::new("a:global-z").expect("z");

    let batch = h
        .store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[
                new_global_node("a:global-y", "surviving Y"),
                new_global_node("a:global-z", "surviving Z"),
                new_ephemeral_node("expiring X with self-loop", h.base_ms + 5_000),
            ],
            &[],
        )
        .expect("put Y, Z, and X");
    let x_id = batch.node_ids[2].clone();

    // Include surviving edge `Y -> Z` alongside `X -> Y`, `Y -> X`, and self-loop `X -> X`
    // (`edge_count == 4` before purge) so `saturating_sub` cannot mask subtracting 4 instead of 3.
    h.store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[],
            &[
                new_links_edge(x_id.clone(), y_id.clone()),
                new_links_edge(y_id.clone(), x_id.clone()),
                new_links_edge(x_id.clone(), x_id),
                new_links_edge(y_id, z_id),
            ],
        )
        .expect("put outbound, inbound, self-loop, and surviving edges");

    assert_all_tenants_stats_equal_recount(
        "before scenario 2 self-loop purge",
        &h.store,
        &[(&h.tenant_a, (3, 4)), (&h.tenant_b, (3, 2))],
    );

    h.clock.advance(Duration::from_secs(10));
    let report = h
        .store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(60),
        })
        .expect("purge_expired scenario 2");
    assert_eq!(report.purged_ephemeral_nodes, 1);
    assert_eq!(report.purged_edges, 3);

    // Edge count drops by 3 (4 -> 1), NOT by 4 (4 -> 0); `tenant_b` stays at (3, 2).
    assert_all_tenants_stats_equal_recount(
        "after scenario 2 self-loop purge",
        &h.store,
        &[(&h.tenant_a, (2, 1)), (&h.tenant_b, (3, 2))],
    );
}

// spec: ING-088
// spec: INV-09
#[test]
fn ing_088_purge_expired_does_not_double_decrement_already_tombstoned_ephemeral_node() {
    let h = build_delta_stats_harness();
    let y_id = NodeId::new("a:global-live").expect("y");

    let batch = h
        .store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[
                new_global_node("a:global-live", "live node that must remain counted"),
                new_ephemeral_node("tombstoned before expiry", h.base_ms + 10_000),
            ],
            &[],
        )
        .expect("put live Y and ephemeral E");
    let e_id = batch.node_ids[1].clone();

    h.store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[],
            &[new_links_edge(e_id.clone(), y_id)],
        )
        .expect("put E -> Y");

    assert_all_tenants_stats_equal_recount(
        "before tombstone_node on ephemeral E",
        &h.store,
        &[(&h.tenant_a, (2, 1)), (&h.tenant_b, (3, 2))],
    );

    // Tombstone `E` before it expires: `tombstone_node` already decrements `tenant_stats` to (1, 0).
    h.clock.advance(Duration::from_secs(2));
    h.store
        .tombstone_node(&h.tenant_a, &h.caller_a, &e_id)
        .expect("tombstone_node E before expiry");

    assert_all_tenants_stats_equal_recount(
        "after tombstone_node before expiry",
        &h.store,
        &[(&h.tenant_a, (1, 0)), (&h.tenant_b, (3, 2))],
    );

    // Advance past `E`'s expiry and purge: `node_count` must remain 1 (not subtracted again to 0).
    h.clock.advance(Duration::from_secs(15));
    let report = h
        .store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(60),
        })
        .expect("purge_expired scenario 3");
    assert_eq!(report.purged_ephemeral_nodes, 1);

    assert_all_tenants_stats_equal_recount(
        "after purging already-tombstoned ephemeral node",
        &h.store,
        &[(&h.tenant_a, (1, 0)), (&h.tenant_b, (3, 2))],
    );
}

// spec: ING-088
// spec: INV-09
#[test]
fn ing_088_purge_expired_subtracts_shared_edge_once_and_keeps_closed_task_stats_unchanged() {
    let h = build_delta_stats_harness();
    let y_id = NodeId::new("a:global-y").expect("y");
    let z_id = NodeId::new("a:global-z").expect("z");

    // Scenario 4: `A -> B` where both `A` and `B` are EPHEMERAL and expire in the same pass,
    // plus surviving `Y -> Z` so `saturating_sub` cannot hide subtracting `A -> B` twice.
    let batch = h
        .store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[
                new_global_node("a:global-y", "surviving Y"),
                new_global_node("a:global-z", "surviving Z"),
                new_ephemeral_node("ephemeral A", h.base_ms + 5_000),
                new_ephemeral_node("ephemeral B", h.base_ms + 5_000),
            ],
            &[],
        )
        .expect("put Y, Z, A, B");
    let a_id = batch.node_ids[2].clone();
    let b_id = batch.node_ids[3].clone();

    h.store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[],
            &[
                new_links_edge(a_id, b_id),
                new_links_edge(y_id.clone(), z_id),
            ],
        )
        .expect("put A -> B and Y -> Z");

    assert_all_tenants_stats_equal_recount(
        "before scenario 4 shared-edge purge",
        &h.store,
        &[(&h.tenant_a, (4, 2)), (&h.tenant_b, (3, 2))],
    );

    h.clock.advance(Duration::from_secs(10));
    let report_eph = h
        .store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(60),
        })
        .expect("purge_expired scenario 4");
    assert_eq!(report_eph.purged_ephemeral_nodes, 2);
    assert_eq!(report_eph.purged_edges, 1);

    // `A -> B` is subtracted once (2 -> 1), leaving surviving `Y -> Z` counted.
    assert_all_tenants_stats_equal_recount(
        "after scenario 4 shared-edge purge",
        &h.store,
        &[(&h.tenant_a, (2, 1)), (&h.tenant_b, (3, 2))],
    );

    // Scenario 5: TASK node purged after `tombstone_task_nodes` (closed versions only) leaves
    // `tenant_stats` unchanged by the purge and equal to the raw recount.
    let task_batch = h
        .store
        .put_batch(
            &h.tenant_a,
            &h.caller_a,
            &[NewNode {
                id: None,
                node_type: NodeType::new("Note").expect("nt"),
                label: "task artifact".to_owned(),
                summary: Some("closed then purged".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Task {
                    task_id: h.active_task,
                },
                provenance: Provenance::new("test"),
            }],
            &[],
        )
        .expect("put TASK node");
    let k_id = task_batch.node_ids[0].clone();

    h.store
        .put_batch(&h.tenant_a, &h.caller_a, &[], &[new_links_edge(k_id, y_id)])
        .expect("put K -> Y");

    assert_all_tenants_stats_equal_recount(
        "before tombstone_task_nodes",
        &h.store,
        &[(&h.tenant_a, (3, 2)), (&h.tenant_b, (3, 2))],
    );

    h.clock.advance(Duration::from_secs(1));
    let closed = h
        .store
        .tombstone_task_nodes(&h.tenant_a, h.active_task)
        .expect("tombstone_task_nodes");
    assert_eq!(closed, 1);

    assert_all_tenants_stats_equal_recount(
        "after tombstone_task_nodes before purge",
        &h.store,
        &[(&h.tenant_a, (2, 1)), (&h.tenant_b, (3, 2))],
    );

    h.clock.advance(Duration::from_secs(20));
    let report_task = h
        .store
        .purge_expired(&PurgePolicy {
            task_retention: Duration::from_secs(5),
        })
        .expect("purge_expired scenario 5");
    assert_eq!(report_task.purged_task_nodes, 1);
    assert_eq!(report_task.purged_edges, 1);

    assert_all_tenants_stats_equal_recount(
        "after scenario 5 closed-task purge",
        &h.store,
        &[(&h.tenant_a, (2, 1)), (&h.tenant_b, (3, 2))],
    );
}
