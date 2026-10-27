//! Integration tests for `ContextStore::search_nodes` (FTS5 `bm25()` ranking, `NodeId` tie-breaking,
//! limit boundaries, hostile FTS5 operator neutralization, tenant isolation, scope visibility,
//! indistinguishability, and `as_of` temporal versions).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_store::{
    BaseScope, Caller, ContextStore, NewNode, NodeId, NodeType, Provenance, ScopeSpec,
    SqliteConfig, SqliteContextStore, StoreError, TaskId, TenantId,
};
use serde_json::Map as JsonMap;
use tempfile::TempDir;

fn doc_node(id: Option<&str>, label: &str, summary: Option<&str>, scope: ScopeSpec) -> NewNode {
    NewNode {
        id: id.map(|s| NodeId::new(s).expect("id")),
        node_type: NodeType::new("Doc").expect("type"),
        label: label.to_owned(),
        summary: summary.map(ToOwned::to_owned),
        props: JsonMap::new(),
        scope,
        provenance: Provenance::new("test"),
    }
}

// spec: ING-063, ING-064, ING-065
#[test]
fn search_bm25_ranking_id_tie_breaking_and_limit_boundaries() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("search_rank.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // Insert tie_b before tie_a so insertion order is opposite of `id ASC` tie-break order!
    store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                Some("doc:tie_b"),
                "authentication guide",
                Some("overview notes"),
                ScopeSpec::Global,
            ),
        )
        .expect("put tie_b");
    store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                Some("doc:tie_a"),
                "authentication guide",
                Some("overview notes"),
                ScopeSpec::Global,
            ),
        )
        .expect("put tie_a");
    store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                Some("doc:strong"),
                "authentication authentication",
                Some("authentication authentication authentication"),
                ScopeSpec::Global,
            ),
        )
        .expect("put strong");

    // Limit validation boundaries: 0 invalid, 1 valid, 100 valid, 101 invalid.
    assert!(matches!(
        store.search_nodes(&tenant, &caller, "authentication", 0, None),
        Err(StoreError::Invalid { field: "limit", .. })
    ));
    assert!(matches!(
        store.search_nodes(&tenant, &caller, "authentication", 101, None),
        Err(StoreError::Invalid { field: "limit", .. })
    ));

    let top_1 = store
        .search_nodes(&tenant, &caller, "authentication", 1, None)
        .expect("limit 1");
    assert_eq!(top_1.len(), 1);
    assert_eq!(top_1[0].id.as_str(), "doc:strong");

    let all = store
        .search_nodes(&tenant, &caller, "authentication", 100, None)
        .expect("limit 100");
    let ids: Vec<&str> = all.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["doc:strong", "doc:tie_a", "doc:tie_b"]);
}

// spec: INV-09, ING-065
#[test]
fn hostile_fts5_syntax_never_errors_and_never_acts_as_operators() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("search_hostile.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                Some("doc:alpha"),
                "alpha module",
                Some("handles requests"),
                ScopeSpec::Global,
            ),
        )
        .expect("alpha");
    store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                Some("doc:beta"),
                "beta module",
                Some("handles responses"),
                ScopeSpec::Global,
            ),
        )
        .expect("beta");
    store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                Some("doc:literal_or"),
                "alpha OR beta",
                Some("literal boolean text"),
                ScopeSpec::Global,
            ),
        )
        .expect("literal_or");

    // Empty / punctuation / syntax-only strings return empty without error.
    for empty_like in [
        "",
        "   ",
        "\"",
        "\"\"\"",
        "*",
        "***",
        "-",
        "---",
        ":",
        ":::",
        "()",
        "^",
        "\0\x01\t\n",
    ] {
        let hits = store
            .search_nodes(&tenant, &caller, empty_like, 10, None)
            .expect("must not error on punctuation/control query");
        assert!(hits.is_empty(), "expected empty result for `{empty_like:?}`");
    }

    // `"alpha OR beta"` MUST quote `OR` as a literal token — returning ONLY `doc:literal_or`,
    // never `doc:alpha` or `doc:beta`!
    let or_hits = store
        .search_nodes(&tenant, &caller, "alpha OR beta", 10, None)
        .expect("literal OR");
    let or_ids: Vec<&str> = or_hits.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(or_ids, vec!["doc:literal_or"]);

    // Hostile FTS5 operator syntax (`NEAR(`, `unknown_column:`, `*`, `-`, unmatched `"`, `NOT`, `AND`)
    for hostile in [
        "NEAR(alpha beta, 2)",
        "NEAR(",
        "nonexistent_column:alpha",
        "\"unterminated quote alpha",
        "alpha* -beta",
        "NOT alpha",
        "AND alpha",
        "^alpha ) (",
        "alpha\0module",
    ] {
        let res = store.search_nodes(&tenant, &caller, hostile, 10, None);
        assert!(res.is_ok(), "hostile query `{hostile}` must not raise FTS5 error");
    }

    // Prefix wildcard `*` must NOT act as FTS5 prefix wildcard: `"alph*"` tokenizes to `"alph"`,
    // which does NOT match `"alpha"`!
    let prefix_attempt = store
        .search_nodes(&tenant, &caller, "alph*", 10, None)
        .expect("wildcard literal");
    assert!(
        prefix_attempt.is_empty(),
        "`*` must not act as FTS5 prefix wildcard"
    );
}

// spec: INV-09, INV-19, ING-062, ING-065, ING-066 (ADR-020)
#[test]
fn search_tenant_isolation_scope_indistinguishability_and_temporal_as_of() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("search_iso.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");

    let tenant_a = TenantId::new("tenant-a").expect("t").scope();
    let tenant_b = TenantId::new("tenant-b").expect("t").scope();
    let task_1 = TaskId::from_unix_ms(1_700_000_000_000, [7u8; 10]);

    let alice = Caller::new("alice")
        .with_groups(["sec"])
        .with_task_chain(vec![task_1]);
    let bob = Caller::new("bob");

    // Tenant A writes Private, Group, Task, and Ephemeral(Global) nodes containing `zephyrterm`.
    store
        .put_node(
            &tenant_a,
            &alice,
            doc_node(None, "zephyrterm private", None, ScopeSpec::Private),
        )
        .expect("priv");
    store
        .put_node(
            &tenant_a,
            &alice,
            doc_node(
                None,
                "zephyrterm group",
                None,
                ScopeSpec::Group {
                    group: "sec".to_owned(),
                },
            ),
        )
        .expect("grp");
    store
        .put_node(
            &tenant_a,
            &alice,
            doc_node(
                None,
                "zephyrterm task",
                None,
                ScopeSpec::Task { task_id: task_1 },
            ),
        )
        .expect("task");
    let eph_id = store
        .put_node(
            &tenant_a,
            &alice,
            doc_node(
                None,
                "zephyrterm ephemeral",
                None,
                ScopeSpec::ephemeral_default(BaseScope::Global),
            ),
        )
        .expect("eph");

    // Alice in Tenant A sees all 4; Alice in Tenant B sees 0!
    assert_eq!(
        store
            .search_nodes(&tenant_a, &alice, "zephyrterm", 10, None)
            .expect("a")
            .len(),
        4
    );
    assert!(
        store
            .search_nodes(&tenant_b, &alice, "zephyrterm", 10, None)
            .expect("b")
            .is_empty()
    );

    // Bob in Tenant A sees only the 1 unexpired Ephemeral(Global) node.
    let bob_before = store
        .search_nodes(&tenant_a, &bob, "zephyrterm", 10, None)
        .expect("bob before");
    assert_eq!(bob_before.len(), 1);
    assert_eq!(bob_before[0].id, eph_id);

    // Advance past 1h ephemeral TTL (without purging): Bob's search for `zephyrterm` is now
    // byte-identical to searching for a completely non-existent word!
    manual.advance(Duration::from_secs(3_600));
    let bob_invis = store
        .search_nodes(&tenant_a, &bob, "zephyrterm", 10, None)
        .expect("bob invis");
    let bob_missing = store
        .search_nodes(&tenant_a, &bob, "nonexistentwordxyz", 10, None)
        .expect("bob missing");
    assert!(bob_invis.is_empty());
    assert_eq!(
        serde_json::to_vec(&bob_invis).expect("json"),
        serde_json::to_vec(&bob_missing).expect("json")
    );

    // Temporal `as_of` across versions and tombstone.
    let t1 = manual.now_timestamp();
    store
        .put_node(
            &tenant_a,
            &alice,
            doc_node(
                Some("doc:versioned"),
                "legacykeyword handler",
                None,
                ScopeSpec::Global,
            ),
        )
        .expect("v1");

    manual.advance(Duration::from_secs(10));
    let t2 = manual.now_timestamp();
    store
        .put_node(
            &tenant_a,
            &alice,
            doc_node(
                Some("doc:versioned"),
                "modernkeyword handler",
                None,
                ScopeSpec::Global,
            ),
        )
        .expect("v2");

    manual.advance(Duration::from_secs(10));
    store
        .tombstone_node(
            &tenant_a,
            &alice,
            &NodeId::new("doc:versioned").expect("id"),
        )
        .expect("tombstone");

    assert!(
        store
            .search_nodes(&tenant_a, &bob, "modernkeyword", 10, None)
            .expect("after tomb")
            .is_empty()
    );
    assert_eq!(
        store
            .search_nodes(&tenant_a, &bob, "legacykeyword", 10, Some(t1))
            .expect("at t1")
            .len(),
        1
    );
    assert!(
        store
            .search_nodes(&tenant_a, &bob, "modernkeyword", 10, Some(t1))
            .expect("modern at t1")
            .is_empty()
    );
    assert_eq!(
        store
            .search_nodes(&tenant_a, &bob, "modernkeyword", 10, Some(t2))
            .expect("at t2")
            .len(),
        1
    );
    assert!(
        store
            .search_nodes(&tenant_a, &bob, "legacykeyword", 10, Some(t2))
            .expect("legacy at t2")
            .is_empty()
    );
}
