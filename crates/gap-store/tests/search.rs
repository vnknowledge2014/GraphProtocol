//! Integration tests for `ContextStore::search_nodes` (FTS5 `bm25()` ranking, `NodeId` tie-breaking,
//! limit boundaries, hostile FTS5 operator neutralization, tenant isolation, scope visibility,
//! indistinguishability, and `as_of` temporal versions).

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_store::sqlite::schema::tenant_fts_table_name;
use gap_store::{
    BaseScope, Caller, ContextMaintenance, ContextStore, NewNode, NodeId, NodeType, Provenance,
    PurgePolicy, ScopeSpec, SqliteConfig, SqliteContextStore, StoreError, TaskId, TenantId,
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

// spec: INV-09, ING-065, ING-086
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
        assert!(
            hits.is_empty(),
            "expected empty result for `{empty_like:?}`"
        );
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
        assert!(
            res.is_ok(),
            "hostile query `{hostile}` must not raise FTS5 error"
        );
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

// spec: INV-09, INV-19, ING-065, ING-086
#[test]
fn inv_09_bm25_ranking_is_isolated_per_tenant_and_unaffected_by_other_tenant_corpus() {
    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));

    let shared_store = SqliteContextStore::open(
        SqliteConfig::new(dir.path().join("r4_shared.db")),
        Arc::clone(&clock),
    )
    .expect("open shared");
    let solo_store =
        SqliteContextStore::open(SqliteConfig::new(dir.path().join("r4_solo.db")), clock)
            .expect("open solo");

    let ta = TenantId::new("ta").expect("ta").scope();
    let tb = TenantId::new("tb").expect("tb").scope();
    let caller = Caller::new("alice");

    // Exact R-4 repro: `ta` has `short` ("alpha gamma") and `long` ("alpha alpha delta x6").
    let ta_nodes = vec![
        doc_node(Some("short"), "alpha gamma", None, ScopeSpec::Global),
        doc_node(
            Some("long"),
            "alpha alpha delta delta delta delta delta delta",
            None,
            ScopeSpec::Global,
        ),
    ];
    shared_store
        .put_batch(&ta, &caller, &ta_nodes, &[])
        .expect("seed ta shared");
    solo_store
        .put_batch(&ta, &caller, &ta_nodes, &[])
        .expect("seed ta solo");

    let before_hits = shared_store
        .search_nodes(&ta, &caller, "alpha", 10, None)
        .expect("ta search before tb");
    let before_ids: Vec<&str> = before_hits.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(before_ids, vec!["short", "long"]);

    let solo_hits = solo_store
        .search_nodes(&ta, &caller, "alpha", 10, None)
        .expect("ta solo search");
    let solo_ids: Vec<&str> = solo_hits.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(solo_ids, vec!["short", "long"]);

    // `tb` writes 40 long documents ("zzz", 150-token summary < 1,024 bytes) that previously
    // inflated corpus-wide average document length and flipped `ta`'s BM25 order to `[long, short]`.
    let summary_150 = (0..150)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(summary_150.len() < 1_024);
    let tb_nodes: Vec<NewNode> = (0..40)
        .map(|i| {
            doc_node(
                Some(&format!("tb:{i:02}")),
                "zzz",
                Some(&summary_150),
                ScopeSpec::Global,
            )
        })
        .collect();
    shared_store
        .put_batch(&tb, &caller, &tb_nodes, &[])
        .expect("seed tb 40 long docs");

    // `ta`'s ranking MUST remain `[short, long]`, identical to before `tb`'s writes and to `solo_store`.
    let after_hits = shared_store
        .search_nodes(&ta, &caller, "alpha", 10, None)
        .expect("ta search after tb");
    let after_ids: Vec<&str> = after_hits.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(after_ids, vec!["short", "long"]);
    assert_eq!(after_ids, solo_ids);

    // Terms existing only in `tb` return nothing for `ta`, while `tb` sees its own 40 matches.
    assert!(
        shared_store
            .search_nodes(&ta, &caller, "zzz", 10, None)
            .expect("ta zzz")
            .is_empty()
    );
    assert!(
        shared_store
            .search_nodes(&ta, &caller, "w42", 10, None)
            .expect("ta w42")
            .is_empty()
    );
    assert_eq!(
        shared_store
            .search_nodes(&tb, &caller, "zzz", 100, None)
            .expect("tb zzz")
            .len(),
        40
    );
}

// spec: INV-09, INV-19, ING-065, ING-066, ING-086
#[test]
fn inv_09_delete_purge_hex_shards_and_punctuation_tenant_ids_isolate_fts_tables() {
    // Two distinct tenants produce distinct 16-hex shards, and punctuation like `a-b_c` never leaks
    // into the SQL table identifier.
    let shard_ta = tenant_fts_table_name("ta");
    let shard_tb = tenant_fts_table_name("tb");
    let shard_punct = tenant_fts_table_name("a-b_c");
    assert_ne!(shard_ta, shard_tb);
    assert_ne!(shard_ta, shard_punct);
    assert_ne!(shard_tb, shard_punct);
    for tbl in [&shard_ta, &shard_tb, &shard_punct] {
        let hex = tbl.strip_prefix("nodes_fts_").expect("prefix");
        assert_eq!(hex.len(), 16);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    let dir = TempDir::new().expect("tempdir");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(dir.path().join("r4_ops.db")), clock)
        .expect("open");

    let ta = TenantId::new("ta").expect("ta").scope();
    let tb = TenantId::new("tb").expect("tb").scope();
    let punct = TenantId::new("a-b_c").expect("punct").scope();
    let unwritten = TenantId::new("unwritten-tenant")
        .expect("unwritten")
        .scope();
    let caller = Caller::new("alice");

    // Searching a tenant that has never created an FTS table returns `Ok(vec![])`, not an error.
    assert!(
        store
            .search_nodes(&unwritten, &caller, "anything", 10, None)
            .expect("empty shard search")
            .is_empty()
    );

    let t0_ns = manual.now_timestamp().unix_nanos();
    let exp_60s = Timestamp::from_unix_nanos(t0_ns + 60 * 1_000_000_000).expect("60s");
    let exp_1h = Timestamp::from_unix_nanos(t0_ns + 3_600 * 1_000_000_000).expect("1h");

    for (scope, exp) in [(&ta, exp_60s), (&tb, exp_1h), (&punct, exp_1h)] {
        store
            .put_node(
                scope,
                &caller,
                doc_node(
                    Some("doc:shared"),
                    "sharedkeyword",
                    Some("per tenant copy"),
                    ScopeSpec::Global,
                ),
            )
            .expect("put shared");
        store
            .put_node(
                scope,
                &caller,
                doc_node(
                    None,
                    "ephemeralkeyword",
                    None,
                    ScopeSpec::ephemeral(BaseScope::Global, exp),
                ),
            )
            .expect("put eph");
    }

    // 1. `delete_node` in `ta` removes ONLY `ta`'s FTS row; `tb` and `a-b_c` keep theirs.
    let shared_id = NodeId::new("doc:shared").expect("id");
    store
        .delete_node(&ta, &caller, &shared_id)
        .expect("delete ta shared");
    assert!(
        store
            .search_nodes(&ta, &caller, "sharedkeyword", 10, None)
            .expect("ta after delete")
            .is_empty()
    );
    assert_eq!(
        store
            .search_nodes(&tb, &caller, "sharedkeyword", 10, None)
            .expect("tb intact")
            .len(),
        1
    );
    assert_eq!(
        store
            .search_nodes(&punct, &caller, "sharedkeyword", 10, None)
            .expect("a-b_c intact")
            .len(),
        1
    );

    // 2. `purge_expired` at +60s purges `ta`'s expired ephemeral FTS row while `tb` and `a-b_c` keep theirs.
    manual.advance(Duration::from_secs(60));
    let report = store
        .purge_expired(&PurgePolicy::default())
        .expect("purge +60s");
    assert_eq!(report.purged_ephemeral_nodes, 1);
    assert!(
        store
            .search_nodes(&ta, &caller, "ephemeralkeyword", 10, None)
            .expect("ta eph purged")
            .is_empty()
    );
    assert_eq!(
        store
            .search_nodes(&tb, &caller, "ephemeralkeyword", 10, None)
            .expect("tb eph intact")
            .len(),
        1
    );
    assert_eq!(
        store
            .search_nodes(&punct, &caller, "ephemeralkeyword", 10, None)
            .expect("a-b_c eph intact")
            .len(),
        1
    );
}

// spec: ING-065, ING-066, ING-082, ING-083, ING-086
#[test]
fn ing_066_ephemeral_expiry_enforces_both_now_and_as_of_in_get_node_and_search_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store =
        SqliteContextStore::open(SqliteConfig::new(dir.path().join("t4_eph_asof.db")), clock)
            .expect("open");

    let tenant = TenantId::new("acme").expect("tenant").scope();
    let caller = Caller::new("alice");

    let t0 = manual.now_timestamp();
    let t0_ns = t0.unix_nanos();
    let exp_60s = Timestamp::from_unix_nanos(t0_ns + 60 * 1_000_000_000).expect("exp_60s");
    let exp_60s_minus_1us =
        gap_store::types::micros_to_timestamp(exp_60s.unix_micros() - 1).expect("exp - 1us");
    let future_120s = Timestamp::from_unix_nanos(t0_ns + 120 * 1_000_000_000).expect("future_120s");

    let eph_id = store
        .put_node(
            &tenant,
            &caller,
            doc_node(
                None,
                "ephemeraltemporalalpha",
                Some("ttl check"),
                ScopeSpec::ephemeral(BaseScope::Global, exp_60s),
            ),
        )
        .expect("put eph");

    // 1. Not expired at `now = t0`, queried at `as_of` before expiry (`exp_60s - 1us`) -> returned;
    //    queried at `as_of = exp_60s` or `future_120s` (>= `expires_at`) -> NOT returned (Mutant [5])!
    assert!(
        store
            .get_node(&tenant, &caller, &eph_id, Some(exp_60s_minus_1us))
            .expect("get 1us before exp")
            .is_some()
    );
    assert_eq!(
        store
            .search_nodes(
                &tenant,
                &caller,
                "ephemeraltemporalalpha",
                10,
                Some(exp_60s_minus_1us),
            )
            .expect("search 1us before exp")
            .len(),
        1
    );
    assert_eq!(
        store
            .get_node(&tenant, &caller, &eph_id, Some(exp_60s))
            .expect("get at exact exp_60s"),
        None
    );
    assert_eq!(
        store
            .get_node(&tenant, &caller, &eph_id, Some(future_120s))
            .expect("get at future as_of past expires_at"),
        None
    );
    assert!(
        store
            .search_nodes(
                &tenant,
                &caller,
                "ephemeraltemporalalpha",
                10,
                Some(exp_60s),
            )
            .expect("search at exact exp_60s")
            .is_empty()
    );
    assert!(
        store
            .search_nodes(
                &tenant,
                &caller,
                "ephemeraltemporalalpha",
                10,
                Some(future_120s),
            )
            .expect("search at future as_of past expires_at")
            .is_empty()
    );

    // 2. Advance to `as_of_earlier = t0 + 30s` (still valid then), then advance `now` to `t0 + 90s`
    //    (expired after `as_of_earlier` and before `now`). Per ING-066 ("never returned once expired"),
    //    BOTH `get_node` and `search_nodes` at `Some(as_of_earlier)` MUST return nothing (Mutant [87])!
    manual.advance(Duration::from_secs(30));
    let as_of_earlier = manual.now_timestamp();
    manual.advance(Duration::from_secs(60));

    assert_eq!(
        store
            .get_node(&tenant, &caller, &eph_id, Some(as_of_earlier))
            .expect("get expired node at earlier as_of"),
        None
    );
    assert!(
        store
            .search_nodes(
                &tenant,
                &caller,
                "ephemeraltemporalalpha",
                10,
                Some(as_of_earlier),
            )
            .expect("search expired node at earlier as_of")
            .is_empty()
    );
}
