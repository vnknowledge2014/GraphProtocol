//! Integration tests for node/edge CRUD, temporal validity, content-hash idempotence,
//! atomic `put_batch`, 50k/100k batch ingestion, quotas, and hostile SQL-injection inputs.

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_store::{
    BaseScope, Caller, ContextStore, EdgeId, EdgeResolution, EdgeType, GraphCounts, NewEdge,
    NewNode, NodeId, NodeType, Provenance, Scope, ScopeSpec, SqliteConfig, SqliteContextStore,
    StoreError, TaskId, TenantId,
};
use rusqlite::Connection;
use serde_json::{Map as JsonMap, json};
use tempfile::TempDir;

fn make_global_node(id: &str, label: &str) -> NewNode {
    NewNode {
        id: Some(NodeId::new(id).expect("valid id")),
        node_type: NodeType::new("Symbol").expect("valid type"),
        label: label.to_owned(),
        summary: Some(format!("summary for {label}")),
        props: JsonMap::new(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("git:HEAD"),
    }
}

fn make_edge(id: Option<&str>, edge_type: &str, from: &str, to: &str) -> NewEdge {
    NewEdge {
        id: id.map(|s| EdgeId::new(s).expect("valid edge id")),
        edge_type: EdgeType::new(edge_type).expect("valid edge type"),
        from: NodeId::new(from).expect("from id"),
        to: NodeId::new(to).expect("to id"),
        props: JsonMap::new(),
        confidence: Some(0.95),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("git:HEAD"),
    }
}

// spec: ING-062, ING-064, ING-082 (ADR-020)
#[test]
fn temporal_versions_as_of_boundaries_and_content_hash_idempotence() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("temporal.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("tenant").scope();
    let writer = Caller::new("indexer");

    let t_before = manual.now_timestamp();
    manual.advance(Duration::from_secs(10));
    let t_v1 = manual.now_timestamp();

    let mut n_v1 = make_global_node("sym:fn_a", "fn_a_v1");
    n_v1.props.insert("z".to_owned(), json!(-0.0));
    n_v1.props.insert("a".to_owned(), json!(1));
    let id_a = store.put_node(&tenant, &writer, n_v1).expect("put v1");
    let id_b = store
        .put_node(&tenant, &writer, make_global_node("sym:fn_b", "fn_b"))
        .expect("put b");
    let edge_id = store
        .put_edge(
            &tenant,
            &writer,
            make_edge(Some("edge:ab"), "CALLS", "sym:fn_a", "sym:fn_b"),
        )
        .expect("put edge v1");

    // Re-putting identical content (with reversed map insertion order and +0.0) at a later time is a no-op!
    manual.advance(Duration::from_secs(5));
    let mut n_v1_reput = make_global_node("sym:fn_a", "fn_a_v1");
    n_v1_reput.props.insert("a".to_owned(), json!(1));
    n_v1_reput.props.insert("z".to_owned(), json!(0.0));
    store
        .put_node(&tenant, &writer, n_v1_reput)
        .expect("idempotent re-put node");
    store
        .put_edge(
            &tenant,
            &writer,
            make_edge(Some("edge:ab"), "CALLS", "sym:fn_a", "sym:fn_b"),
        )
        .expect("idempotent re-put edge");

    let raw = Connection::open(&db_path).expect("raw inspect");
    let version_rows: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE tenant = 'acme' AND id = 'sym:fn_a'",
            [],
            |r| r.get(0),
        )
        .expect("count node versions");
    assert_eq!(
        version_rows, 1,
        "identical re-put must not create a second version"
    );

    let edge_rows: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE tenant = 'acme' AND id = 'edge:ab'",
            [],
            |r| r.get(0),
        )
        .expect("count edge versions");
    assert_eq!(
        edge_rows, 1,
        "identical edge re-put must not create a second version"
    );

    // Advance and write changed version v2.
    manual.advance(Duration::from_secs(10));
    let t_v2 = manual.now_timestamp();
    let mut n_v2 = make_global_node("sym:fn_a", "fn_a_v2");
    n_v2.props.insert("a".to_owned(), json!(2));
    store.put_node(&tenant, &writer, n_v2).expect("put v2");

    let mut e_v2 = make_edge(Some("edge:ab"), "CALLS", "sym:fn_a", "sym:fn_b");
    e_v2.confidence = Some(0.5);
    store.put_edge(&tenant, &writer, e_v2).expect("put edge v2");

    let version_rows_after: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE tenant = 'acme' AND id = 'sym:fn_a'",
            [],
            |r| r.get(0),
        )
        .expect("count node versions after v2");
    assert_eq!(version_rows_after, 2);

    // Check exact as_of boundaries:
    // 1. Before v1 -> None.
    assert!(
        store
            .get_node(&tenant, &writer, &id_a, Some(t_before))
            .expect("before")
            .is_none()
    );
    assert!(
        store
            .get_edge(&tenant, &writer, &edge_id, Some(t_before))
            .expect("edge before")
            .is_none()
    );

    // 2. Exact valid_from of v1 -> v1 (with valid_to == Some(t_v2)).
    let got_at_v1 = store
        .get_node(&tenant, &writer, &id_a, Some(t_v1))
        .expect("at v1")
        .expect("v1 exists");
    assert_eq!(got_at_v1.label, "fn_a_v1");
    assert_eq!(got_at_v1.valid_from, t_v1);
    assert_eq!(got_at_v1.valid_to, Some(t_v2));

    // 3. 1 microsecond before t_v2 -> still v1!
    let t_just_before_v2 =
        gap_store::types::micros_to_timestamp(t_v2.unix_micros() - 1).expect("ts");
    let got_before_v2 = store
        .get_node(&tenant, &writer, &id_a, Some(t_just_before_v2))
        .expect("just before v2")
        .expect("v1 still active");
    assert_eq!(got_before_v2.label, "fn_a_v1");

    // 4. Exact t_v2 -> v2 (valid_to == None)!
    let got_at_v2 = store
        .get_node(&tenant, &writer, &id_a, Some(t_v2))
        .expect("at v2")
        .expect("v2 active");
    assert_eq!(got_at_v2.label, "fn_a_v2");
    assert_eq!(got_at_v2.valid_from, t_v2);
    assert_eq!(got_at_v2.valid_to, None);

    let got_edge_v2 = store
        .get_edge(&tenant, &writer, &edge_id, Some(t_v2))
        .expect("edge at v2")
        .expect("edge v2 active");
    assert_eq!(got_edge_v2.confidence, Some(0.5));

    // 5. Tombstone closes both node and incident edges at t_tomb.
    manual.advance(Duration::from_secs(10));
    let t_tomb = manual.now_timestamp();
    store
        .tombstone_node(&tenant, &writer, &id_a)
        .expect("tombstone fn_a");

    assert!(
        store
            .get_node(&tenant, &writer, &id_a, Some(t_tomb))
            .expect("at tomb")
            .is_none(),
        "valid_to is exclusive: node must be absent at exact valid_to"
    );
    assert!(
        store
            .get_edge(&tenant, &writer, &edge_id, Some(t_tomb))
            .expect("edge at tomb")
            .is_none(),
        "tombstone must close incident edges"
    );
    assert_eq!(
        store
            .counts(&tenant, &writer, None)
            .expect("counts after tomb"),
        GraphCounts { nodes: 1, edges: 0 }
    );
    // Historical query at t_v2 still sees both nodes and the edge!
    assert_eq!(
        store
            .counts(&tenant, &writer, Some(t_v2))
            .expect("historical counts"),
        GraphCounts { nodes: 2, edges: 1 }
    );

    // 6. Hard delete removes all historical versions of sym:fn_b too.
    store
        .delete_node(&tenant, &writer, &id_b)
        .expect("hard delete fn_b");
    assert!(
        store
            .get_node(&tenant, &writer, &id_b, Some(t_v1))
            .expect("deleted history")
            .is_none()
    );
}

// spec: ING-064, INV-09, ING-080
#[test]
fn put_batch_atomicity_cap_and_50k_nodes_100k_edges_ingestion() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("batch.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let mut cfg = SqliteConfig::new(&db_path);
    cfg.durability = gap_store::Durability::Normal;
    let store = SqliteContextStore::open(cfg, clock).expect("open store");

    let tenant = TenantId::new("acme").expect("tenant").scope();
    let writer = Caller::new("indexer");

    // 1. Over-cap batch (10,001 items) rejected with Invalid.
    let over_cap: Vec<NewNode> = (0..10_001)
        .map(|i| make_global_node(&format!("cap:{i}"), "item"))
        .collect();
    assert!(matches!(
        store.put_batch(&tenant, &writer, &over_cap, &[]),
        Err(StoreError::Invalid { field: "batch", .. })
    ));

    // 2. Atomicity: 50 valid nodes + 49 valid edges + 1 final edge pointing to a missing node -> nothing persisted!
    let batch_nodes: Vec<NewNode> = (0..50)
        .map(|i| make_global_node(&format!("atomic:{i}"), "ok"))
        .collect();
    let mut batch_edges: Vec<NewEdge> = (0..49)
        .map(|i| {
            make_edge(
                None,
                "CALLS",
                &format!("atomic:{i}"),
                &format!("atomic:{}", i + 1),
            )
        })
        .collect();
    batch_edges.push(make_edge(
        None,
        "CALLS",
        "atomic:0",
        "atomic:does_not_exist",
    ));

    let err = store
        .put_batch(&tenant, &writer, &batch_nodes, &batch_edges)
        .expect_err("bad last item must fail entire batch");
    assert_eq!(err, StoreError::NotFound);
    assert_eq!(
        store.counts(&tenant, &writer, None).expect("counts"),
        GraphCounts { nodes: 0, edges: 0 }
    );

    // 3. 50,000 nodes (5 x 10,000) and 100,000 edges (10 x 10,000) ingested atomically via batches.
    let sym_type = NodeType::new("Symbol").expect("type");
    let calls_type = EdgeType::new("CALLS").expect("edge type");
    let prov = Provenance::new("bulk-indexer");

    for chunk in 0..5usize {
        let base = chunk * 10_000;
        let nodes: Vec<NewNode> = (0..10_000usize)
            .map(|offset| {
                let idx = base + offset;
                NewNode {
                    id: Some(NodeId::new(format!("n:{idx}")).expect("id")),
                    node_type: sym_type.clone(),
                    label: format!("sym_{idx}"),
                    summary: None,
                    props: JsonMap::new(),
                    scope: ScopeSpec::Global,
                    provenance: prov.clone(),
                }
            })
            .collect();
        let res = store
            .put_batch(&tenant, &writer, &nodes, &[])
            .expect("insert 10k nodes");
        assert_eq!(res.node_ids.len(), 10_000);
    }

    for chunk in 0..10usize {
        let base = chunk * 10_000;
        let edges: Vec<NewEdge> = (0..10_000usize)
            .map(|offset| {
                let idx = base + offset;
                let from_idx = idx % 50_000;
                let to_idx = (idx * 7 + 1 + (idx / 50_000)) % 50_000;
                NewEdge {
                    id: Some(EdgeId::new(format!("e:{idx}")).expect("eid")),
                    edge_type: calls_type.clone(),
                    from: NodeId::new(format!("n:{from_idx}")).expect("from"),
                    to: NodeId::new(format!("n:{to_idx}")).expect("to"),
                    props: JsonMap::new(),
                    confidence: Some(1.0),
                    resolution: EdgeResolution::Exact,
                    provenance: prov.clone(),
                }
            })
            .collect();
        let res = store
            .put_batch(&tenant, &writer, &[], &edges)
            .expect("insert 10k edges");
        assert_eq!(res.edge_ids.len(), 10_000);
    }

    assert_eq!(
        store
            .counts(&tenant, &writer, None)
            .expect("50k/100k counts"),
        GraphCounts {
            nodes: 50_000,
            edges: 100_000,
        }
    );
}

// spec: ING-064, INV-09, INV-19, ING-087
#[test]
fn per_tenant_quotas_enforced_isolated_and_reclaimed_on_tombstone() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("quota.db");
    let mut cfg = SqliteConfig::new(&db_path);
    cfg.max_nodes_per_tenant = 3;
    cfg.max_edges_per_tenant = 2;

    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(cfg, clock).expect("open store");

    let tenant_a = TenantId::new("tenant-a").expect("t").scope();
    let tenant_b = TenantId::new("tenant-b").expect("t").scope();
    let writer = Caller::new("alice");

    for id in ["n1", "n2", "n3"] {
        store
            .put_node(&tenant_a, &writer, make_global_node(id, id))
            .expect("within node quota");
    }
    // 4th node in tenant_a exceeds max_nodes_per_tenant == 3.
    assert_eq!(
        store
            .put_node(&tenant_a, &writer, make_global_node("n4", "n4"))
            .expect_err("4th node must exceed quota"),
        StoreError::QuotaExceeded
    );

    // Updating an existing node at quota still succeeds!
    store
        .put_node(&tenant_a, &writer, make_global_node("n1", "n1_updated"))
        .expect("updating existing node does not consume extra quota");

    // Tenant B has its own independent quota!
    store
        .put_node(&tenant_b, &writer, make_global_node("n1", "tenant_b_n1"))
        .expect("tenant B unaffected by tenant A quota");

    // Edges in tenant_a: 2 allowed, 3rd rejected.
    store
        .put_edge(&tenant_a, &writer, make_edge(None, "CALLS", "n1", "n2"))
        .expect("edge 1");
    store
        .put_edge(&tenant_a, &writer, make_edge(None, "CALLS", "n2", "n3"))
        .expect("edge 2");
    assert_eq!(
        store
            .put_edge(&tenant_a, &writer, make_edge(None, "CALLS", "n1", "n3"))
            .expect_err("3rd edge must exceed quota"),
        StoreError::QuotaExceeded
    );

    // Tombstoning n3 reclaims 1 node slot and 1 incident edge slot!
    store
        .tombstone_node(&tenant_a, &writer, &NodeId::new("n3").expect("id"))
        .expect("tombstone n3");
    store
        .put_node(&tenant_a, &writer, make_global_node("n4", "n4"))
        .expect("reclaimed node slot");
    store
        .put_edge(&tenant_a, &writer, make_edge(None, "CALLS", "n1", "n4"))
        .expect("reclaimed edge slot");
}

// spec: INV-09, INV-19, ING-065
#[test]
fn hostile_sql_injection_strings_stored_and_returned_verbatim() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("hostile_sql.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("t").scope();
    let hostile_group = "grp'); DROP TABLE nodes;--";
    let caller = Caller::new("user'; DELETE FROM edges;--").with_groups([hostile_group]);

    let hostile_id = "id'); DROP TABLE tenant_stats; --";
    let hostile_label = "label\" OR 1=1; ATTACH DATABASE ':memory:' AS pwn;--";
    let mut props = JsonMap::new();
    props.insert(
        "payload';--".to_owned(),
        json!("SELECT * FROM sqlite_master WHERE '1'='1'"),
    );

    let mut node = make_global_node(hostile_id, hostile_label);
    node.props = props.clone();
    store
        .put_node(&tenant, &caller, node)
        .expect("store hostile global node");

    let group_node_id = store
        .put_node(
            &tenant,
            &caller,
            NewNode {
                id: None,
                node_type: NodeType::new("Doc").expect("type"),
                label: hostile_label.to_owned(),
                summary: Some("'; DROP VIRTUAL TABLE nodes_fts;--".to_owned()),
                props: props.clone(),
                scope: ScopeSpec::Group {
                    group: hostile_group.to_owned(),
                },
                provenance: Provenance::new("src';--"),
            },
        )
        .expect("store hostile group node");

    let got = store
        .get_node(
            &tenant,
            &caller,
            &NodeId::new(hostile_id).expect("id"),
            None,
        )
        .expect("get hostile")
        .expect("exists");
    assert_eq!(got.id.as_str(), hostile_id);
    assert_eq!(got.label, hostile_label);
    assert_eq!(got.props, props);

    let got_grp = store
        .get_node(&tenant, &caller, &group_node_id, None)
        .expect("get hostile group")
        .expect("exists");
    assert_eq!(got_grp.group.as_deref(), Some(hostile_group));
    assert_eq!(
        store.counts(&tenant, &caller, None).expect("counts"),
        GraphCounts { nodes: 2, edges: 0 }
    );
}

// spec: ING-062, ING-065, ING-066, ING-082 (ADR-020, ADR-025)
#[test]
fn ing_082_content_hash_single_field_reput_versions_every_hashed_node_and_edge_field() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("t2_hash_fields.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("tenant").scope();
    let task_1 = TaskId::from_unix_ms(1_700_000_000_000, [0x11; 10]);
    let task_2 = TaskId::from_unix_ms(1_700_000_000_001, [0x22; 10]);

    let mut alice = Caller::new("alice").with_groups(["g_old", "g_new"]);
    alice.task_chain = vec![task_1, task_2];
    let old_group_member = Caller::new("old_member").with_groups(["g_old"]);
    let new_group_member = Caller::new("new_member").with_groups(["g_new"]);
    let outsider = Caller::new("outsider");

    let t0_ns = manual.now_timestamp().unix_nanos();
    let exp_fixed = Timestamp::from_unix_nanos(t0_ns + 3_600 * 1_000_000_000).expect("exp_fixed");
    let exp_short = Timestamp::from_unix_nanos(t0_ns + 120 * 1_000_000_000).expect("exp_short");
    let exp_long = Timestamp::from_unix_nanos(t0_ns + 600 * 1_000_000_000).expect("exp_long");

    let base_node = |id: Option<NodeId>, scope: ScopeSpec| NewNode {
        id,
        node_type: NodeType::new("Symbol").expect("type"),
        label: "FixedLabel".to_owned(),
        summary: Some("FixedSummary".to_owned()),
        props: JsonMap::new(),
        scope,
        provenance: Provenance::new("hash-test"),
    };

    let t_v1 = manual.now_timestamp();

    // Create v1 of every single-field test entity at `t_v1`.
    let id_label = store
        .put_node(
            &tenant,
            &alice,
            base_node(Some(NodeId::new("n:label").expect("id")), ScopeSpec::Global),
        )
        .expect("put label v1");
    let id_summary = store
        .put_node(
            &tenant,
            &alice,
            base_node(
                Some(NodeId::new("n:summary").expect("id")),
                ScopeSpec::Global,
            ),
        )
        .expect("put summary v1");

    let mut n_props_v1 = base_node(Some(NodeId::new("n:props").expect("id")), ScopeSpec::Global);
    n_props_v1.props.insert("k".to_owned(), json!(1));
    let id_props = store
        .put_node(&tenant, &alice, n_props_v1)
        .expect("put props v1");

    let id_scope_kind = store
        .put_node(&tenant, &alice, base_node(None, ScopeSpec::Private))
        .expect("put scope_kind v1");

    let id_group = store
        .put_node(
            &tenant,
            &alice,
            base_node(
                None,
                ScopeSpec::Group {
                    group: "g_old".to_owned(),
                },
            ),
        )
        .expect("put group v1");

    let id_eph_base = store
        .put_node(
            &tenant,
            &alice,
            base_node(None, ScopeSpec::ephemeral(BaseScope::Private, exp_fixed)),
        )
        .expect("put eph_base v1");

    let id_eph_exp = store
        .put_node(
            &tenant,
            &alice,
            base_node(None, ScopeSpec::ephemeral(BaseScope::Global, exp_short)),
        )
        .expect("put eph_exp v1");

    let id_task = store
        .put_node(
            &tenant,
            &alice,
            base_node(None, ScopeSpec::Task { task_id: task_1 }),
        )
        .expect("put task v1");

    // Edges v1 (`e_props`, `e_res`, `e_conf`) between `n:label` and `n:summary`.
    let mut e_props_v1 = make_edge(Some("e:props"), "CALLS", "n:label", "n:summary");
    e_props_v1.props.insert("weight".to_owned(), json!(1.0));
    let eid_props = store
        .put_edge(&tenant, &alice, e_props_v1)
        .expect("put edge props v1");

    let mut e_res_v1 = make_edge(Some("e:res"), "IMPORTS", "n:label", "n:summary");
    e_res_v1.confidence = Some(1.0);
    e_res_v1.resolution = EdgeResolution::Exact;
    let eid_res = store
        .put_edge(&tenant, &alice, e_res_v1)
        .expect("put edge res v1");

    let mut e_conf_v1 = make_edge(Some("e:conf"), "REFERENCES", "n:label", "n:summary");
    e_conf_v1.confidence = Some(0.9);
    e_conf_v1.resolution = EdgeResolution::Heuristic;
    let eid_conf = store
        .put_edge(&tenant, &alice, e_conf_v1)
        .expect("put edge conf v1");

    // Verify pre-change visibility of group and ephemeral-base nodes at `t_v1`.
    assert!(
        store
            .get_node(&tenant, &old_group_member, &id_group, Some(t_v1))
            .expect("old group v1")
            .is_some()
    );
    assert!(
        store
            .get_node(&tenant, &new_group_member, &id_group, Some(t_v1))
            .expect("new group v1")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant, &outsider, &id_eph_base, Some(t_v1))
            .expect("outsider eph_base v1")
            .is_none()
    );

    // Advance clock to `t_v2` and re-put each entity changing ONLY ONE hashed field!
    manual.advance(Duration::from_secs(10));
    let t_v2 = manual.now_timestamp();

    // 1. Node `label` only
    let mut n_label_v2 = base_node(Some(id_label.clone()), ScopeSpec::Global);
    n_label_v2.label = "ChangedLabel".to_owned();
    store
        .put_node(&tenant, &alice, n_label_v2)
        .expect("reput label");

    // 2. Node `summary` only
    let mut n_sum_v2 = base_node(Some(id_summary.clone()), ScopeSpec::Global);
    n_sum_v2.summary = Some("ChangedSummary".to_owned());
    store
        .put_node(&tenant, &alice, n_sum_v2)
        .expect("reput summary");

    // 3. Node `props` only (Mutant [28])
    let mut n_props_v2 = base_node(Some(id_props.clone()), ScopeSpec::Global);
    n_props_v2.props.insert("k".to_owned(), json!(2));
    store
        .put_node(&tenant, &alice, n_props_v2)
        .expect("reput props");

    // 4. Node `scope` kind (`PRIVATE` -> `GROUP`)
    store
        .put_node(
            &tenant,
            &alice,
            base_node(
                Some(id_scope_kind.clone()),
                ScopeSpec::Group {
                    group: "g_new".to_owned(),
                },
            ),
        )
        .expect("reput scope_kind");

    // 5. Node `group` only (`g_old` -> `g_new`, Mutant [25])
    store
        .put_node(
            &tenant,
            &alice,
            base_node(
                Some(id_group.clone()),
                ScopeSpec::Group {
                    group: "g_new".to_owned(),
                },
            ),
        )
        .expect("reput group");

    // 6. Ephemeral `base` scope only (`Private` -> `Global`, identical `exp_fixed`, Mutant [24])
    store
        .put_node(
            &tenant,
            &alice,
            base_node(
                Some(id_eph_base.clone()),
                ScopeSpec::ephemeral(BaseScope::Global, exp_fixed),
            ),
        )
        .expect("reput eph_base");

    // 7. Ephemeral `expires_at` only (`exp_short` -> `exp_long`, identical `BaseScope::Global`, Mutant [26])
    store
        .put_node(
            &tenant,
            &alice,
            base_node(
                Some(id_eph_exp.clone()),
                ScopeSpec::ephemeral(BaseScope::Global, exp_long),
            ),
        )
        .expect("reput eph_exp");

    // 8. Node `task_id` only (`task_1` -> `task_2`)
    store
        .put_node(
            &tenant,
            &alice,
            base_node(Some(id_task.clone()), ScopeSpec::Task { task_id: task_2 }),
        )
        .expect("reput task_id");

    // 9. Edge `props` only (Mutant [29])
    let mut e_props_v2 = make_edge(Some("e:props"), "CALLS", "n:label", "n:summary");
    e_props_v2.props.insert("weight".to_owned(), json!(2.5));
    store
        .put_edge(&tenant, &alice, e_props_v2)
        .expect("reput edge props");

    // 10. Edge `resolution` only (`Exact` -> `Heuristic` with identical `confidence: Some(1.0)`, Mutant [31])
    let mut e_res_v2 = make_edge(Some("e:res"), "IMPORTS", "n:label", "n:summary");
    e_res_v2.confidence = Some(1.0);
    e_res_v2.resolution = EdgeResolution::Heuristic;
    store
        .put_edge(&tenant, &alice, e_res_v2)
        .expect("reput edge res");

    // 11. Edge `confidence` only (`0.9` -> `0.4` with identical `Heuristic`)
    let mut e_conf_v2 = make_edge(Some("e:conf"), "REFERENCES", "n:label", "n:summary");
    e_conf_v2.confidence = Some(0.4);
    e_conf_v2.resolution = EdgeResolution::Heuristic;
    store
        .put_edge(&tenant, &alice, e_conf_v2)
        .expect("reput edge conf");

    // Assert two temporal versions (`as_of = t_v1` vs `as_of = t_v2`) and updated semantics for EVERY field!
    let label_v1 = store
        .get_node(&tenant, &alice, &id_label, Some(t_v1))
        .expect("get")
        .expect("v1");
    let label_v2 = store
        .get_node(&tenant, &alice, &id_label, Some(t_v2))
        .expect("get")
        .expect("v2");
    assert_eq!(
        (label_v1.label.as_str(), label_v1.valid_to),
        ("FixedLabel", Some(t_v2))
    );
    assert_eq!(
        (
            label_v2.label.as_str(),
            label_v2.valid_from,
            label_v2.valid_to
        ),
        ("ChangedLabel", t_v2, None)
    );

    let sum_v1 = store
        .get_node(&tenant, &alice, &id_summary, Some(t_v1))
        .expect("get")
        .expect("v1");
    let sum_v2 = store
        .get_node(&tenant, &alice, &id_summary, Some(t_v2))
        .expect("get")
        .expect("v2");
    assert_eq!(
        (sum_v1.summary.as_deref(), sum_v1.valid_to),
        (Some("FixedSummary"), Some(t_v2))
    );
    assert_eq!(
        (
            sum_v2.summary.as_deref(),
            sum_v2.valid_from,
            sum_v2.valid_to
        ),
        (Some("ChangedSummary"), t_v2, None)
    );

    let props_v1 = store
        .get_node(&tenant, &alice, &id_props, Some(t_v1))
        .expect("get")
        .expect("v1");
    let props_v2 = store
        .get_node(&tenant, &alice, &id_props, Some(t_v2))
        .expect("get")
        .expect("v2");
    assert_eq!(
        (props_v1.props.get("k"), props_v1.valid_to),
        (Some(&json!(1)), Some(t_v2))
    );
    assert_eq!(
        (
            props_v2.props.get("k"),
            props_v2.valid_from,
            props_v2.valid_to
        ),
        (Some(&json!(2)), t_v2, None)
    );

    let sk_v1 = store
        .get_node(&tenant, &alice, &id_scope_kind, Some(t_v1))
        .expect("get")
        .expect("v1");
    let sk_v2 = store
        .get_node(&tenant, &new_group_member, &id_scope_kind, Some(t_v2))
        .expect("get")
        .expect("v2");
    assert_eq!((sk_v1.scope, sk_v1.valid_to), (Scope::Private, Some(t_v2)));
    assert_eq!((sk_v2.scope, sk_v2.valid_from), (Scope::Group, t_v2));

    // Group change: old member loses current access; new member gains current access; v1 closed at t_v2.
    assert!(
        store
            .get_node(&tenant, &old_group_member, &id_group, Some(t_v2))
            .expect("old member lost access")
            .is_none()
    );
    let grp_v1 = store
        .get_node(&tenant, &old_group_member, &id_group, Some(t_v1))
        .expect("old member v1")
        .expect("v1");
    let grp_v2 = store
        .get_node(&tenant, &new_group_member, &id_group, Some(t_v2))
        .expect("new member gained access")
        .expect("v2");
    assert_eq!(
        (grp_v1.group.as_deref(), grp_v1.valid_to),
        (Some("g_old"), Some(t_v2))
    );
    assert_eq!(
        (grp_v2.group.as_deref(), grp_v2.valid_from, grp_v2.valid_to),
        (Some("g_new"), t_v2, None)
    );

    // Ephemeral base scope change (`Private` -> `Global`): outsider gains access at `t_v2`, v1 closed at `t_v2`.
    let eb_v1 = store
        .get_node(&tenant, &alice, &id_eph_base, Some(t_v1))
        .expect("alice eb v1")
        .expect("v1");
    assert_eq!(eb_v1.valid_to, Some(t_v2));
    assert!(
        store
            .get_node(&tenant, &outsider, &id_eph_base, Some(t_v1))
            .expect("outsider eb v1")
            .is_none()
    );
    let eb_v2 = store
        .get_node(&tenant, &outsider, &id_eph_base, Some(t_v2))
        .expect("outsider eb v2")
        .expect("outsider must see Global ephemeral v2");
    assert_eq!((eb_v2.valid_from, eb_v2.valid_to), (t_v2, None));

    // Ephemeral expiry change (`exp_short` -> `exp_long`): v1 closed at `t_v2`, v2 carries `exp_long`.
    let ex_v1 = store
        .get_node(&tenant, &alice, &id_eph_exp, Some(t_v1))
        .expect("ex v1")
        .expect("v1");
    let ex_v2 = store
        .get_node(&tenant, &alice, &id_eph_exp, Some(t_v2))
        .expect("ex v2")
        .expect("v2");
    assert_eq!(
        (ex_v1.expires_at, ex_v1.valid_to),
        (Some(exp_short), Some(t_v2))
    );
    assert_eq!(
        (ex_v2.expires_at, ex_v2.valid_from, ex_v2.valid_to),
        (Some(exp_long), t_v2, None)
    );

    // Task id change (`task_1` -> `task_2`)
    let tk_v1 = store
        .get_node(&tenant, &alice, &id_task, Some(t_v1))
        .expect("tk v1")
        .expect("v1");
    let tk_v2 = store
        .get_node(&tenant, &alice, &id_task, Some(t_v2))
        .expect("tk v2")
        .expect("v2");
    assert_eq!((tk_v1.task_id, tk_v1.valid_to), (Some(task_1), Some(t_v2)));
    assert_eq!(
        (tk_v2.task_id, tk_v2.valid_from, tk_v2.valid_to),
        (Some(task_2), t_v2, None)
    );

    // Edge props, resolution, and confidence versions
    let ep_v1 = store
        .get_edge(&tenant, &alice, &eid_props, Some(t_v1))
        .expect("ep v1")
        .expect("v1");
    let ep_v2 = store
        .get_edge(&tenant, &alice, &eid_props, Some(t_v2))
        .expect("ep v2")
        .expect("v2");
    assert_eq!(
        (ep_v1.props.get("weight"), ep_v1.valid_to),
        (Some(&json!(1.0)), Some(t_v2))
    );
    assert_eq!(
        (ep_v2.props.get("weight"), ep_v2.valid_from, ep_v2.valid_to),
        (Some(&json!(2.5)), t_v2, None)
    );

    let er_v1 = store
        .get_edge(&tenant, &alice, &eid_res, Some(t_v1))
        .expect("er v1")
        .expect("v1");
    let er_v2 = store
        .get_edge(&tenant, &alice, &eid_res, Some(t_v2))
        .expect("er v2")
        .expect("v2");
    assert_eq!(
        (er_v1.resolution, er_v1.valid_to),
        (EdgeResolution::Exact, Some(t_v2))
    );
    assert_eq!(
        (er_v2.resolution, er_v2.valid_from, er_v2.valid_to),
        (EdgeResolution::Heuristic, t_v2, None)
    );

    let ec_v1 = store
        .get_edge(&tenant, &alice, &eid_conf, Some(t_v1))
        .expect("ec v1")
        .expect("v1");
    let ec_v2 = store
        .get_edge(&tenant, &alice, &eid_conf, Some(t_v2))
        .expect("ec v2")
        .expect("v2");
    assert_eq!((ec_v1.confidence, ec_v1.valid_to), (Some(0.9), Some(t_v2)));
    assert_eq!(
        (ec_v2.confidence, ec_v2.valid_from, ec_v2.valid_to),
        (Some(0.4), t_v2, None)
    );

    // Advance past `exp_short` (to t0 + 200s < exp_long) to prove the updated expiry took effect!
    manual.advance(Duration::from_secs(190));
    assert!(
        store
            .get_node(&tenant, &alice, &id_eph_exp, None)
            .expect("after exp_short")
            .is_some(),
        "ephemeral node whose expires_at was extended to exp_long must still be visible"
    );
}

// spec: ING-065, ING-082 (ADR-020, ADR-025)
#[test]
fn ing_082_group_member_reput_with_unchanged_payload_transfers_owner_and_creates_version() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("t2_owner_change.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("tenant").scope();
    let alice = Caller::new("alice").with_groups(["eng"]);
    let bob = Caller::new("bob").with_groups(["eng"]);

    let t_v1 = manual.now_timestamp();
    let gid = store
        .put_node(
            &tenant,
            &alice,
            NewNode {
                id: None,
                node_type: NodeType::new("Doc").expect("type"),
                label: "SharedRunbook".to_owned(),
                summary: Some("identical summary".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Group {
                    group: "eng".to_owned(),
                },
                provenance: Provenance::new("team-sync"),
            },
        )
        .expect("alice put group node");

    manual.advance(Duration::from_secs(10));
    let t_v2 = manual.now_timestamp();

    // `bob` (in group `eng`) updates `gid` with 100% identical hashed payload; ONLY `writer` differs!
    store
        .put_node(
            &tenant,
            &bob,
            NewNode {
                id: Some(gid.clone()),
                node_type: NodeType::new("Doc").expect("type"),
                label: "SharedRunbook".to_owned(),
                summary: Some("identical summary".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Group {
                    group: "eng".to_owned(),
                },
                provenance: Provenance::new("team-sync"),
            },
        )
        .expect("bob update group node");

    let v1 = store
        .get_node(&tenant, &alice, &gid, Some(t_v1))
        .expect("get v1")
        .expect("v1 exists");
    let v2 = store
        .get_node(&tenant, &bob, &gid, Some(t_v2))
        .expect("get v2")
        .expect("v2 exists");
    assert_eq!(v1.owner_principal, "alice");
    assert_eq!(v1.valid_to, Some(t_v2));
    assert_eq!(v2.owner_principal, "bob");
    assert_eq!(v2.valid_from, t_v2);
    assert_eq!(v2.valid_to, None);
}
