//! Integration tests for `SqliteContextStore::backup_to` and `SqliteContextStore::restore_from`
//! covering full row-by-row round-trips, tenant isolation across backup/restore, existing-target
//! refusal, corrupted and future-version rejection, and barrier-synchronized concurrent online
//! backup while writers and readers run (`ING-064`, `INV-02`, `INV-09`, `INV-15`).

use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_store::sqlite::FORMAT_VERSION;
use gap_store::{
    Caller, ContextStore, Durability, EdgeResolution, EdgeType, GraphCounts, NewEdge, NewNode,
    NodeId, NodeType, Provenance, ScopeSpec, SqliteConfig, SqliteContextStore, StoreError, TaskId,
    TenantId,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map as JsonMap, json};
use tempfile::TempDir;

#[derive(Debug, PartialEq, Eq)]
struct NodeDumpRow {
    tenant: String,
    id: String,
    node_type: String,
    label: String,
    summary: Option<String>,
    props_json: String,
    scope: String,
    base_scope: String,
    owner_principal: String,
    task_id: Option<String>,
    group_name: Option<String>,
    expires_at: Option<i64>,
    valid_from: i64,
    valid_to: Option<i64>,
    content_hash: Vec<u8>,
}

type EdgeDumpRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<f64>,
    String,
    i64,
    Option<i64>,
    Vec<u8>,
);

#[derive(Debug, PartialEq)]
struct FullDatabaseSnapshot {
    tenant_stats: Vec<(String, i64, i64)>,
    nodes: Vec<NodeDumpRow>,
    edges: Vec<EdgeDumpRow>,
    fts: Vec<(i64, String, String, String)>,
}

fn dump_all_rows(path: &Path) -> FullDatabaseSnapshot {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open readonly dump");

    let tenant_stats = conn
        .prepare("SELECT tenant, node_count, edge_count FROM tenant_stats ORDER BY tenant ASC")
        .expect("prep stats")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query stats")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect stats");

    let nodes = conn
        .prepare(
            "SELECT tenant, id, node_type, label, summary, props_json, scope, base_scope,
                    owner_principal, task_id, group_name, expires_at, valid_from, valid_to, content_hash
             FROM nodes
             ORDER BY tenant ASC, id ASC, valid_from ASC",
        )
        .expect("prep nodes")
        .query_map([], |r| {
            Ok(NodeDumpRow {
                tenant: r.get(0)?,
                id: r.get(1)?,
                node_type: r.get(2)?,
                label: r.get(3)?,
                summary: r.get(4)?,
                props_json: r.get(5)?,
                scope: r.get(6)?,
                base_scope: r.get(7)?,
                owner_principal: r.get(8)?,
                task_id: r.get(9)?,
                group_name: r.get(10)?,
                expires_at: r.get(11)?,
                valid_from: r.get(12)?,
                valid_to: r.get(13)?,
                content_hash: r.get(14)?,
            })
        })
        .expect("query nodes")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect nodes");

    let edges = conn
        .prepare(
            "SELECT tenant, id, edge_type, from_id, to_id, props_json, confidence, resolution,
                    valid_from, valid_to, content_hash
             FROM edges
             ORDER BY tenant ASC, id ASC, valid_from ASC",
        )
        .expect("prep edges")
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })
        .expect("query edges")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect edges");

    let fts = conn
        .prepare("SELECT rowid, tenant, label, summary FROM nodes_fts ORDER BY rowid ASC")
        .expect("prep fts")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query fts")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect fts");

    FullDatabaseSnapshot {
        tenant_stats,
        nodes,
        edges,
        fts,
    }
}

// spec: ING-064, INV-09, INV-19 (ADR-020)
#[test]
fn backup_and_restore_round_trip_preserves_all_rows_versions_fts_and_tenant_isolation() {
    let dir = TempDir::new().expect("tempdir");
    let live_path = dir.path().join("live.db");
    let backup_path = dir.path().join("backup.db");
    let restored_path = dir.path().join("restored.db");

    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&live_path), Arc::clone(&clock_dyn))
        .expect("open live");

    let tenant_a = TenantId::new("acme").expect("acme").scope();
    let tenant_b = TenantId::new("globex").expect("globex").scope();

    let task_id = TaskId::from_unix_ms(1_700_000_000_000, [0x42; 10]);
    let mut alice = Caller::new("alice");
    alice.groups.insert("sec-ops".to_owned());
    alice.task_chain.push(task_id);

    let bob = Caller::new("bob");

    let mut props_v1 = JsonMap::new();
    props_v1.insert("rev".to_owned(), json!(1));
    let n_global = store
        .put_node(
            &tenant_a,
            &alice,
            NewNode {
                id: Some(NodeId::new("svc:gateway").expect("id")),
                node_type: NodeType::new("Service").expect("type"),
                label: "Gateway Service v1".to_owned(),
                summary: Some("prismquartzalpha".to_owned()),
                props: props_v1,
                scope: ScopeSpec::Global,
                provenance: Provenance::new("ingester"),
            },
        )
        .expect("put n_global v1");

    let n_priv = store
        .put_node(
            &tenant_a,
            &alice,
            NewNode {
                id: None,
                node_type: NodeType::new("SecretNote").expect("type"),
                label: "Alice Private Backup Note".to_owned(),
                summary: Some("aliceonlysecret".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Private,
                provenance: Provenance::new("alice"),
            },
        )
        .expect("put n_priv");

    let n_task = store
        .put_node(
            &tenant_a,
            &alice,
            NewNode {
                id: None,
                node_type: NodeType::new("Plan").expect("type"),
                label: "Task Scoped Plan".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Task { task_id },
                provenance: Provenance::new("alice"),
            },
        )
        .expect("put n_task");

    store
        .put_edge(
            &tenant_a,
            &alice,
            NewEdge {
                id: Some(edge_id("edge:gw-plan")),
                edge_type: EdgeType::new("USES").expect("etype"),
                from: n_global.clone(),
                to: n_task.clone(),
                props: JsonMap::new(),
                confidence: Some(0.95),
                resolution: EdgeResolution::Heuristic,
                provenance: Provenance::new("alice"),
            },
        )
        .expect("put edge");

    // Advance clock and create v2 of `svc:gateway` so historical temporal rows exist in WAL!
    clock.advance(Duration::from_secs(30));
    let as_of_v1 = clock.now_timestamp();
    clock.advance(Duration::from_secs(30));

    let mut props_v2 = JsonMap::new();
    props_v2.insert("rev".to_owned(), json!(2));
    store
        .put_node(
            &tenant_a,
            &alice,
            NewNode {
                id: Some(n_global.clone()),
                node_type: NodeType::new("Service").expect("type"),
                label: "Gateway Service v2".to_owned(),
                summary: Some("prismquartzbeta".to_owned()),
                props: props_v2,
                scope: ScopeSpec::Global,
                provenance: Provenance::new("ingester"),
            },
        )
        .expect("put n_global v2");

    // Populate tenant_b with distinct data to prove tenant isolation survives backup & restore.
    store
        .put_node(
            &tenant_b,
            &bob,
            NewNode {
                id: Some(NodeId::new("svc:gateway").expect("id")),
                node_type: NodeType::new("Service").expect("type"),
                label: "Globex Gateway".to_owned(),
                summary: Some("globexexclusivetoken".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Global,
                provenance: Provenance::new("bob"),
            },
        )
        .expect("put tenant_b");

    // Online backup WITHOUT manual checkpointing first (proving WAL frames are captured!).
    store.backup_to(&backup_path).expect("backup_to");
    SqliteContextStore::restore_from(&backup_path, &restored_path).expect("restore_from");

    let live_snapshot = dump_all_rows(&live_path);
    let restored_snapshot = dump_all_rows(&restored_path);
    assert_eq!(live_snapshot, restored_snapshot);

    let restored = SqliteContextStore::open(SqliteConfig::new(&restored_path), clock_dyn)
        .expect("open restored");

    assert_eq!(
        restored
            .get_node(&tenant_a, &alice, &n_global, Some(as_of_v1))
            .expect("v1")
            .expect("present")
            .label,
        "Gateway Service v1"
    );
    assert_eq!(
        restored
            .get_node(&tenant_a, &alice, &n_global, None)
            .expect("v2")
            .expect("present")
            .label,
        "Gateway Service v2"
    );
    assert!(
        restored
            .get_node(&tenant_a, &bob, &n_priv, None)
            .expect("bob cannot see alice private after restore")
            .is_none()
    );
    assert!(
        restored
            .search_nodes(&tenant_a, &alice, "globexexclusivetoken", 10, None)
            .expect("tenant_a never sees tenant_b fts after restore")
            .is_empty()
    );
    assert_eq!(
        restored
            .search_nodes(&tenant_a, &alice, "prismquartzbeta", 10, None)
            .expect("fts v2 hit")
            .len(),
        1
    );
    assert_eq!(
        restored
            .counts(&tenant_b, &bob, None)
            .expect("tenant_b counts"),
        GraphCounts { nodes: 1, edges: 0 }
    );
}

fn edge_id(s: &str) -> gap_store::EdgeId {
    gap_store::EdgeId::new(s).expect("edge id")
}

// spec: ING-064, INV-15
#[test]
fn restore_and_backup_refuse_existing_targets_and_reject_corrupted_or_future_backups() {
    let dir = TempDir::new().expect("tempdir");
    let live_path = dir.path().join("src_live.db");
    let backup_path = dir.path().join("valid_backup.db");

    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store =
        SqliteContextStore::open(SqliteConfig::new(&live_path), Arc::clone(&clock)).expect("open");
    let tenant = TenantId::new("acme").expect("tenant").scope();
    let writer = Caller::new("writer");

    // Insert enough payload so the SQLite database spans multiple pages for corruption testing.
    let big_summary = "x".repeat(900);
    for idx in 0..80 {
        store
            .put_node(
                &tenant,
                &writer,
                NewNode {
                    id: Some(NodeId::new(format!("node:{idx:04}")).expect("id")),
                    node_type: NodeType::new("Item").expect("type"),
                    label: format!("Item label {idx:04}"),
                    summary: Some(big_summary.clone()),
                    props: JsonMap::new(),
                    scope: ScopeSpec::Global,
                    provenance: Provenance::new("seed"),
                },
            )
            .expect("seed node");
    }

    store.backup_to(&backup_path).expect("valid backup");

    // 1. `backup_to` refuses an existing destination file without modifying it.
    let pre_existing_backup = dir.path().join("existing_dest.db");
    fs::write(&pre_existing_backup, b"keep-me-intact").expect("write sentinel");
    assert_eq!(
        store
            .backup_to(&pre_existing_backup)
            .expect_err("must refuse existing backup dest"),
        StoreError::Conflict
    );
    assert_eq!(
        fs::read(&pre_existing_backup).expect("read sentinel"),
        b"keep-me-intact"
    );

    // 2. `restore_from` refuses an existing target file and leaves it byte-identical.
    let pre_existing_target = dir.path().join("existing_target.db");
    fs::write(&pre_existing_target, b"do-not-overwrite").expect("write target sentinel");
    assert_eq!(
        SqliteContextStore::restore_from(&backup_path, &pre_existing_target)
            .expect_err("must refuse existing restore target"),
        StoreError::Conflict
    );
    assert_eq!(
        fs::read(&pre_existing_target).expect("read target sentinel"),
        b"do-not-overwrite"
    );

    // 3. Missing source file -> `NotFound`.
    let missing_target = dir.path().join("never_created_0.db");
    assert_eq!(
        SqliteContextStore::restore_from(dir.path().join("no_such.db"), &missing_target)
            .expect_err("missing src"),
        StoreError::NotFound
    );
    assert!(!missing_target.exists());

    // 4. Non-SQLite garbage file -> `Corrupt`.
    let garbage_src = dir.path().join("garbage.db");
    fs::write(&garbage_src, vec![0xAAu8; 4096]).expect("write garbage");
    let bad_target_1 = dir.path().join("never_created_1.db");
    assert_eq!(
        SqliteContextStore::restore_from(&garbage_src, &bad_target_1).expect_err("garbage"),
        StoreError::Corrupt
    );
    assert!(!bad_target_1.exists());

    // 5. Truncated header (< 100 bytes) -> `Corrupt`.
    let truncated_src = dir.path().join("truncated.db");
    fs::write(&truncated_src, b"SQLite format 3\0short").expect("write truncated");
    let bad_target_2 = dir.path().join("never_created_2.db");
    assert_eq!(
        SqliteContextStore::restore_from(&truncated_src, &bad_target_2).expect_err("truncated"),
        StoreError::Corrupt
    );
    assert!(!bad_target_2.exists());

    // 6. Corrupted B-tree pages inside a copy of a valid backup -> `Corrupt`.
    let damaged_src = dir.path().join("damaged_btree.db");
    let mut raw_bytes = fs::read(&backup_path).expect("read valid backup");
    assert!(raw_bytes.len() > 8192, "backup must span multiple pages");
    for byte in &mut raw_bytes[4096..8192] {
        *byte ^= 0xFF;
    }
    fs::write(&damaged_src, &raw_bytes).expect("write damaged backup");
    let bad_target_3 = dir.path().join("never_created_3.db");
    assert_eq!(
        SqliteContextStore::restore_from(&damaged_src, &bad_target_3).expect_err("damaged btree"),
        StoreError::Corrupt
    );
    assert!(!bad_target_3.exists());

    // 7. SQLite database missing required GAP table -> `Corrupt`.
    let incomplete_src = dir.path().join("incomplete_schema.db");
    fs::copy(&backup_path, &incomplete_src).expect("copy for drop");
    {
        let raw = Connection::open(&incomplete_src).expect("open incomplete");
        raw.execute_batch("DROP TABLE edges;").expect("drop edges");
    }
    let bad_target_4 = dir.path().join("never_created_4.db");
    assert_eq!(
        SqliteContextStore::restore_from(&incomplete_src, &bad_target_4)
            .expect_err("missing table"),
        StoreError::Corrupt
    );
    assert!(!bad_target_4.exists());

    // 8. Future `user_version > FORMAT_VERSION` -> `FormatTooNew`.
    let future_src = dir.path().join("future_backup.db");
    fs::copy(&backup_path, &future_src).expect("copy for future ver");
    {
        let raw = Connection::open(&future_src).expect("open future");
        raw.execute_batch("PRAGMA user_version = 77;")
            .expect("set user_version 77");
    }
    let bad_target_5 = dir.path().join("never_created_5.db");
    assert_eq!(
        SqliteContextStore::restore_from(&future_src, &bad_target_5).expect_err("future version"),
        StoreError::FormatTooNew {
            found: 77,
            supported: FORMAT_VERSION,
        }
    );
    assert!(!bad_target_5.exists());

    // 9. Directory source (non-NotFound I/O error on bounded header read) -> `Backend`.
    let src_dir = dir.path().join("src_is_directory");
    fs::create_dir(&src_dir).expect("create src_dir");
    let bad_target_6 = dir.path().join("never_created_6.db");
    assert_eq!(
        SqliteContextStore::restore_from(&src_dir, &bad_target_6).expect_err("directory src"),
        StoreError::Backend
    );
    assert!(!bad_target_6.exists());
}

// spec: INV-02, ING-064
#[test]
fn online_backup_is_safe_under_concurrent_writer_and_readers_with_barriers() {
    let dir = TempDir::new().expect("tempdir");
    let live_path = dir.path().join("concurrent_live.db");
    let backup_path = dir.path().join("concurrent_backup.db");
    let restored_path = dir.path().join("concurrent_restored.db");

    let mut cfg = SqliteConfig::new(&live_path);
    cfg.durability = Durability::Normal;
    cfg.reader_pool_size = NonZeroUsize::new(4).expect("nonzero");

    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = Arc::new(SqliteContextStore::open(cfg, Arc::clone(&clock)).expect("open"));
    let tenant = TenantId::new("acme").expect("tenant").scope();
    let caller = Caller::new("worker");

    // Pre-seed 20 baseline nodes so readers and backup always see at least 20 nodes.
    let baseline: Vec<NewNode> = (0..20)
        .map(|i| NewNode {
            id: Some(NodeId::new(format!("base:{i:03}")).expect("id")),
            node_type: NodeType::new("Item").expect("type"),
            label: format!("Baseline {i:03}"),
            summary: Some("concurrentbackuptoken".to_owned()),
            props: JsonMap::new(),
            scope: ScopeSpec::Global,
            provenance: Provenance::new("seed"),
        })
        .collect();
    store
        .put_batch(&tenant, &caller, &baseline, &[])
        .expect("seed baseline");

    // 1 writer + 2 readers + 1 backup thread = 4 threads synchronized by a Barrier (no sleeps).
    let barrier = Arc::new(Barrier::new(4));

    let writer_handle = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let tenant = tenant.clone();
        let caller = caller.clone();
        thread::spawn(move || {
            barrier.wait();
            for i in 0..80 {
                store
                    .put_node(
                        &tenant,
                        &caller,
                        NewNode {
                            id: Some(NodeId::new(format!("dyn:{i:03}")).expect("id")),
                            node_type: NodeType::new("Item").expect("type"),
                            label: format!("Dynamic {i:03}"),
                            summary: Some("concurrentbackuptoken".to_owned()),
                            props: JsonMap::new(),
                            scope: ScopeSpec::Global,
                            provenance: Provenance::new("writer"),
                        },
                    )
                    .expect("concurrent put_node");
            }
        })
    };

    let mut reader_handles = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let tenant = tenant.clone();
        let caller = caller.clone();
        reader_handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..60 {
                let c = store
                    .counts(&tenant, &caller, None)
                    .expect("concurrent counts");
                assert!(c.nodes >= 20 && c.nodes <= 100);
                let hits = store
                    .search_nodes(&tenant, &caller, "concurrentbackuptoken", 50, None)
                    .expect("concurrent search");
                assert!(hits.len() >= 20);
            }
        }));
    }

    let backup_handle = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let backup_path = backup_path.clone();
        thread::spawn(move || {
            barrier.wait();
            store.backup_to(&backup_path).expect("concurrent backup_to");
        })
    };

    writer_handle.join().expect("join writer");
    for rh in reader_handles {
        rh.join().expect("join reader");
    }
    backup_handle.join().expect("join backup");

    SqliteContextStore::restore_from(&backup_path, &restored_path)
        .expect("restore concurrent backup");
    let mut restored_cfg = SqliteConfig::new(&restored_path);
    restored_cfg.durability = Durability::Normal;
    let restored = SqliteContextStore::open(restored_cfg, clock).expect("open restored");
    let restored_counts = restored
        .counts(&tenant, &caller, None)
        .expect("restored counts");
    assert!(restored_counts.nodes >= 20 && restored_counts.nodes <= 100);
}
