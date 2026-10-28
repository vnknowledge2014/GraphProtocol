//! Integration tests for `ContextMaintenance` (`tombstone_task_nodes` and `purge_expired`)
//! verifying tenant/task isolation, temporal `as_of` preservation before retention expiry,
//! microsecond-exact TTL and retention boundaries under `ManualClock`, FTS5/edge cleanup,
//! and `tenant_stats` quota reclamation (`ING-064`, `ING-066`, `INV-09`).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_store::{
    BaseScope, Caller, ContextMaintenance, ContextStore, EdgeResolution, EdgeType, GraphCounts,
    NewEdge, NewNode, NodeId, NodeType, Provenance, PurgePolicy, PurgeReport, ScopeSpec,
    SqliteConfig, SqliteContextStore, StoreError, TaskId, TenantId,
};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Map as JsonMap;
use tempfile::TempDir;

fn make_node(id: Option<&str>, label: &str, summary: Option<&str>, scope: ScopeSpec) -> NewNode {
    NewNode {
        id: id.map(|s| NodeId::new(s).expect("node id")),
        node_type: NodeType::new("Note").expect("node type"),
        label: label.to_owned(),
        summary: summary.map(ToOwned::to_owned),
        props: JsonMap::new(),
        scope,
        provenance: Provenance::new("maint-test"),
    }
}

fn make_edge(from: &NodeId, to: &NodeId) -> NewEdge {
    NewEdge {
        id: None,
        edge_type: EdgeType::new("LINKS").expect("edge type"),
        from: from.clone(),
        to: to.clone(),
        props: JsonMap::new(),
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("maint-test"),
    }
}

// spec: ING-066, ING-064, INV-09 (ADR-020)
#[test]
fn tombstone_task_nodes_isolates_tenant_and_task_preserves_as_of_and_reclaims_quota() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("tombstone_task.db");
    let mut cfg = SqliteConfig::new(&db_path);
    cfg.max_nodes_per_tenant = 4;
    cfg.max_edges_per_tenant = 2;

    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let store = SqliteContextStore::open(cfg, clock_dyn).expect("open store");

    let tenant_a = TenantId::new("acme").expect("tenant a").scope();
    let tenant_b = TenantId::new("globex").expect("tenant b").scope();

    let task_1 = TaskId::from_unix_ms(1_700_000_000_000, [0x11; 10]);
    let task_2 = TaskId::from_unix_ms(1_700_000_000_001, [0x22; 10]);

    let mut caller = Caller::new("agent-alpha");
    caller.task_chain = vec![task_1, task_2];

    let t0 = clock.now_timestamp();
    let eph_exp =
        gap_core::clock::Timestamp::from_unix_nanos(t0.unix_nanos() + 2 * 3_600 * 1_000_000_000)
            .expect("eph_exp");

    let g_anchor = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(Some("anchor"), "Anchor", None, ScopeSpec::Global),
        )
        .expect("put anchor");
    let t1_node = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                None,
                "Task1 Note",
                None,
                ScopeSpec::Task { task_id: task_1 },
            ),
        )
        .expect("put t1_node");
    let t1_eph = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                None,
                "Task1 Scratch",
                None,
                ScopeSpec::Ephemeral {
                    base: BaseScope::Task { task_id: task_1 },
                    expires_at: Some(eph_exp),
                },
            ),
        )
        .expect("put t1_eph");
    let t2_node = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                None,
                "Task2 Note",
                None,
                ScopeSpec::Task { task_id: task_2 },
            ),
        )
        .expect("put t2_node");

    let e1 = store
        .put_edge(&tenant_a, &caller, make_edge(&g_anchor, &t1_node))
        .expect("put e1");
    let e2 = store
        .put_edge(&tenant_a, &caller, make_edge(&t1_node, &t1_eph))
        .expect("put e2");

    // Both node quota (4) and edge quota (2) are saturated in tenant_a.
    assert_eq!(
        store
            .put_node(
                &tenant_a,
                &caller,
                make_node(Some("overflow"), "Overflow", None, ScopeSpec::Global),
            )
            .expect_err("node quota full"),
        StoreError::QuotaExceeded
    );
    assert_eq!(
        store
            .put_edge(&tenant_a, &caller, make_edge(&g_anchor, &t2_node))
            .expect_err("edge quota full"),
        StoreError::QuotaExceeded
    );

    // Same task_1 in tenant_b must not be touched when tenant_a tombstones task_1.
    let b_t1_node = store
        .put_node(
            &tenant_b,
            &caller,
            make_node(
                None,
                "TenantB Task1",
                None,
                ScopeSpec::Task { task_id: task_1 },
            ),
        )
        .expect("put tenant_b task_1");

    clock.advance(Duration::from_secs(10));
    let as_of_before_tombstone = clock.now_timestamp();

    clock.advance(Duration::from_secs(10));
    let closed_count = store
        .tombstone_task_nodes(&tenant_a, task_1)
        .expect("tombstone task_1 in tenant_a");
    assert_eq!(closed_count, 2);

    // Idempotent second call closes 0 additional nodes.
    assert_eq!(
        store
            .tombstone_task_nodes(&tenant_a, task_1)
            .expect("idempotent tombstone"),
        0
    );

    // Current view in tenant_a: task_1 nodes & edges are closed; anchor and task_2 remain.
    assert!(
        store
            .get_node(&tenant_a, &caller, &t1_node, None)
            .expect("get t1_node")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &caller, &t1_eph, None)
            .expect("get t1_eph")
            .is_none()
    );
    assert!(
        store
            .get_edge(&tenant_a, &caller, &e1, None)
            .expect("get e1")
            .is_none()
    );
    assert!(
        store
            .get_edge(&tenant_a, &caller, &e2, None)
            .expect("get e2")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &caller, &t2_node, None)
            .expect("get t2_node")
            .is_some()
    );
    assert_eq!(
        store.counts(&tenant_a, &caller, None).expect("counts now"),
        GraphCounts { nodes: 2, edges: 0 }
    );

    // Historical `as_of` prior to tombstone still sees t1_node, t1_eph, e1, and e2.
    assert!(
        store
            .get_node(&tenant_a, &caller, &t1_node, Some(as_of_before_tombstone))
            .expect("historical t1_node")
            .is_some()
    );
    assert!(
        store
            .get_edge(&tenant_a, &caller, &e1, Some(as_of_before_tombstone))
            .expect("historical e1")
            .is_some()
    );

    // Tenant B's task_1 node is untouched.
    assert!(
        store
            .get_node(&tenant_b, &caller, &b_t1_node, None)
            .expect("tenant_b untouched")
            .is_some()
    );

    // Quotas in tenant_a were freed by 2 nodes and 2 edges.
    let repl_1 = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(Some("repl-1"), "Repl 1", None, ScopeSpec::Global),
        )
        .expect("reclaimed slot 1");
    let repl_2 = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(Some("repl-2"), "Repl 2", None, ScopeSpec::Global),
        )
        .expect("reclaimed slot 2");
    store
        .put_edge(&tenant_a, &caller, make_edge(&repl_1, &repl_2))
        .expect("reclaimed edge 1");
    store
        .put_edge(&tenant_a, &caller, make_edge(&g_anchor, &t2_node))
        .expect("reclaimed edge 2");
}

// spec: ING-066, ING-064, ING-065, INV-09
#[test]
fn purge_expired_enforces_exact_microsecond_boundaries_cleans_fts_and_edges_and_reclaims_quota() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("purge_expired.db");
    let mut cfg = SqliteConfig::new(&db_path);
    cfg.max_nodes_per_tenant = 4;
    cfg.max_edges_per_tenant = 3;

    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let store = SqliteContextStore::open(cfg, clock_dyn).expect("open store");

    let tenant_a = TenantId::new("acme").expect("tenant a").scope();
    let tenant_b = TenantId::new("globex").expect("tenant b").scope();

    let task_done = TaskId::from_unix_ms(1_700_000_000_010, [0xaa; 10]);
    let task_live = TaskId::from_unix_ms(1_700_000_000_020, [0xbb; 10]);

    let mut caller = Caller::new("owner-1");
    caller.task_chain = vec![task_done, task_live];

    let t0 = clock.now_timestamp();
    let exp_1h =
        gap_core::clock::Timestamp::from_unix_nanos(t0.unix_nanos() + 3_600 * 1_000_000_000)
            .expect("exp_1h");
    let exp_2h =
        gap_core::clock::Timestamp::from_unix_nanos(t0.unix_nanos() + 2 * 3_600 * 1_000_000_000)
            .expect("exp_2h");

    // Tenant A: 1 permanent global node, 1 ephemeral (1h), 1 finished task node (with 2 versions),
    // and 1 long-running active task node (also with 2 versions).
    let g_keep = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(Some("keep-a"), "Permanent A", None, ScopeSpec::Global),
        )
        .expect("put g_keep");
    let eph_1h = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                None,
                "Eph One Hour",
                Some("ephemeralzebratoken"),
                ScopeSpec::Ephemeral {
                    base: BaseScope::Global,
                    expires_at: Some(exp_1h),
                },
            ),
        )
        .expect("put eph_1h");
    let task_finished_id = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                None,
                "taskunicorntoken v1",
                None,
                ScopeSpec::Task { task_id: task_done },
            ),
        )
        .expect("put task_finished v1");
    let task_live_id = store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                None,
                "live task v1",
                None,
                ScopeSpec::Task { task_id: task_live },
            ),
        )
        .expect("put task_live v1");

    store
        .put_edge(&tenant_a, &caller, make_edge(&g_keep, &eph_1h))
        .expect("edge g_keep -> eph_1h");
    store
        .put_edge(&tenant_a, &caller, make_edge(&g_keep, &task_finished_id))
        .expect("edge g_keep -> task_finished");
    store
        .put_edge(&tenant_a, &caller, make_edge(&g_keep, &task_live_id))
        .expect("edge g_keep -> task_live");

    // Tenant B: 1 permanent node + 1 ephemeral (2h) + 1 edge.
    let b_keep = store
        .put_node(
            &tenant_b,
            &caller,
            make_node(Some("keep-b"), "Permanent B", None, ScopeSpec::Global),
        )
        .expect("put b_keep");
    let eph_2h = store
        .put_node(
            &tenant_b,
            &caller,
            make_node(
                None,
                "Eph Two Hour",
                Some("ephemeralgiraffetoken"),
                ScopeSpec::Ephemeral {
                    base: BaseScope::Private,
                    expires_at: Some(exp_2h),
                },
            ),
        )
        .expect("put eph_2h");
    store
        .put_edge(&tenant_b, &caller, make_edge(&b_keep, &eph_2h))
        .expect("edge b_keep -> eph_2h");

    // t0 + 1m: capture `as_of_v1`, then at t0 + 5m update both task nodes so each has 2 versions!
    clock.advance(Duration::from_secs(60));
    let as_of_v1 = clock.now_timestamp();

    clock.advance(Duration::from_secs(4 * 60));
    let mut update_live = make_node(
        None,
        "live task v2",
        None,
        ScopeSpec::Task { task_id: task_live },
    );
    update_live.id = Some(task_live_id.clone());
    store
        .put_node(&tenant_a, &caller, update_live)
        .expect("update task_live v2");

    // t0 + 10m: update task_finished to v2.
    clock.advance(Duration::from_secs(5 * 60));
    let mut update_done = make_node(
        None,
        "taskunicorntoken v2",
        None,
        ScopeSpec::Task { task_id: task_done },
    );
    update_done.id = Some(task_finished_id.clone());
    store
        .put_node(&tenant_a, &caller, update_done)
        .expect("update task_finished v2");

    clock.advance(Duration::from_secs(5 * 60));
    let as_of_done_v2 = clock.now_timestamp(); // t0 + 15m

    // t0 + 30m: tombstone `task_done` (now at 1,800s from t0).
    clock.advance(Duration::from_secs(15 * 60));
    assert_eq!(
        store
            .tombstone_task_nodes(&tenant_a, task_done)
            .expect("tombstone task_done"),
        1
    );

    // Note: tenant_a now has 3 active nodes (`g_keep`, `eph_1h`, `task_live_id`). Fill the 4th
    // slot with another permanent node so active node quota is saturated (4/4) until `eph_1h` is
    // purged!
    store
        .put_node(
            &tenant_a,
            &caller,
            make_node(Some("filler-a"), "Filler A", None, ScopeSpec::Global),
        )
        .expect("fill 4th slot");
    assert_eq!(
        store
            .put_node(
                &tenant_a,
                &caller,
                make_node(
                    Some("blocked-before-purge"),
                    "Blocked",
                    None,
                    ScopeSpec::Global
                ),
            )
            .expect_err("saturated before eph_1h purge"),
        StoreError::QuotaExceeded
    );

    // Boundary 1: 1 microsecond BEFORE `exp_1h` (`t0 + 3600s - 1us`; currently at `t0 + 1800s`).
    clock.advance(Duration::from_micros(1_800 * 1_000_000 - 1));
    assert_eq!(
        store
            .purge_expired(&PurgePolicy::default())
            .expect("1us before exp_1h"),
        PurgeReport {
            purged_ephemeral_nodes: 0,
            purged_task_nodes: 0,
            purged_edges: 0,
        }
    );
    assert!(
        store
            .get_node(&tenant_a, &caller, &eph_1h, None)
            .expect("still visible 1us before expiry")
            .is_some()
    );

    // Boundary 2: advance 1 microsecond to exact `exp_1h` (`t0 + 3600s`).
    clock.advance(Duration::from_micros(1));
    assert!(
        store
            .get_node(&tenant_a, &caller, &eph_1h, None)
            .expect("invisible at exact expiry before purge")
            .is_none()
    );
    assert!(
        store
            .search_nodes(&tenant_a, &caller, "ephemeralzebratoken", 10, None)
            .expect("search invisible at expiry")
            .is_empty()
    );

    assert_eq!(
        store
            .purge_expired(&PurgePolicy::default())
            .expect("purge at exact exp_1h"),
        PurgeReport {
            purged_ephemeral_nodes: 1,
            purged_task_nodes: 0,
            purged_edges: 1,
        }
    );

    // Quota in tenant_a was reclaimed by purging active expired ephemeral `eph_1h`!
    store
        .put_node(
            &tenant_a,
            &caller,
            make_node(
                Some("reclaimed-after-eph"),
                "Reclaimed",
                None,
                ScopeSpec::Global,
            ),
        )
        .expect("node slot reclaimed after purging expired ephemeral");

    // Boundary 3: advance to `(t0 + 30m + 7d) - 1us`.
    // Currently at `t0 + 60m`; target is `t0 + 30m + 7d - 1us`, delta = `7d - 30m - 1us`.
    let seven_days_minus_30m_us: u64 = (7 * 86_400 - 1_800) * 1_000_000 - 1;
    clock.advance(Duration::from_micros(seven_days_minus_30m_us));
    assert_eq!(
        store
            .purge_expired(&PurgePolicy::default())
            .expect("1us before 7d task retention"),
        PurgeReport {
            // `eph_2h` in tenant_b expired days ago and is purged now; `task_finished_id` is 1us shy!
            purged_ephemeral_nodes: 1,
            purged_task_nodes: 0,
            purged_edges: 1,
        }
    );
    // Historical read of `task_finished_id` MUST still succeed 1us before retention expires.
    assert_eq!(
        store
            .get_node(&tenant_a, &caller, &task_finished_id, Some(as_of_done_v2))
            .expect("still retained 1us before 7d")
            .expect("present")
            .label,
        "taskunicorntoken v2"
    );

    // Boundary 4: advance 1 microsecond to exact `t0 + 30m + 7d`.
    clock.advance(Duration::from_micros(1));
    assert_eq!(
        store
            .purge_expired(&PurgePolicy::default())
            .expect("exact 7d task retention"),
        PurgeReport {
            purged_ephemeral_nodes: 0,
            // Both v1 and v2 rows of `task_finished_id` are purged; `task_live_id` (whose v2 is
            // still active!) is NOT purged even though its v1 closed >7d ago!
            purged_task_nodes: 2,
            purged_edges: 1,
        }
    );

    // Historical read of `task_finished_id` now returns `None`.
    assert!(
        store
            .get_node(&tenant_a, &caller, &task_finished_id, Some(as_of_done_v2))
            .expect("purged after 7d")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &caller, &task_finished_id, Some(as_of_v1))
            .expect("v1 also purged")
            .is_none()
    );

    // Meanwhile `task_live_id` (still active!) retains BOTH v2 (current) and v1 (`as_of_v1`)!
    assert_eq!(
        store
            .get_node(&tenant_a, &caller, &task_live_id, None)
            .expect("live task v2")
            .expect("present")
            .label,
        "live task v2"
    );
    assert_eq!(
        store
            .get_node(&tenant_a, &caller, &task_live_id, Some(as_of_v1))
            .expect("live task v1")
            .expect("present")
            .label,
        "live task v1"
    );

    // Raw SQLite verification: zero FTS5 or table remnants for purged nodes in either tenant shard.
    let raw = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open raw readonly");
    for t_name in ["acme", "globex"] {
        let shard_tbl = gap_store::sqlite::schema::tenant_fts_table_name(t_name);
        let remaining_fts_hits: i64 = raw
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {shard_tbl} WHERE {shard_tbl} MATCH '\"ephemeralzebratoken\" OR \"ephemeralgiraffetoken\" OR \"taskunicorntoken\"'"
                ),
                [],
                |r| r.get(0),
            )
            .expect("fts remnant query");
        assert_eq!(remaining_fts_hits, 0, "remnant in {shard_tbl}");
    }

    let remaining_purged_edges: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE to_id IN (?1, ?2, ?3)",
            params![eph_1h.as_str(), eph_2h.as_str(), task_finished_id.as_str()],
            |r| r.get(0),
        )
        .expect("edge remnant query");
    assert_eq!(remaining_purged_edges, 0);
}
