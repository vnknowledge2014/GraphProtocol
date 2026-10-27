//! Integration tests for scope visibility (`Private`, `Group`, `Task`, `Global`, `Ephemeral`),
//! byte-identical indistinguishability, edge endpoint visibility, no-ID-oracle UUIDv7 minting,
//! and cross-tenant isolation across identical caller claims.

use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock};
use gap_store::{
    BaseScope, Caller, ContextStore, EdgeId, EdgeResolution, EdgeType, GraphCounts, NewEdge,
    NewNode, NodeId, NodeType, Provenance, Scope, ScopeSpec, SqliteConfig, SqliteContextStore,
    StoreError, TaskId, TenantId,
};
use serde_json::Map as JsonMap;
use tempfile::TempDir;
use uuid::Uuid;

fn new_node(id: Option<NodeId>, label: &str, scope: ScopeSpec) -> NewNode {
    NewNode {
        id,
        node_type: NodeType::new("Note").expect("valid type"),
        label: label.to_owned(),
        summary: Some(format!("summary:{label}")),
        props: JsonMap::new(),
        scope,
        provenance: Provenance::new("agent:test"),
    }
}

fn new_edge(from: &NodeId, to: &NodeId) -> NewEdge {
    NewEdge {
        id: None,
        edge_type: EdgeType::new("LINKS_TO").expect("valid edge type"),
        from: from.clone(),
        to: to.clone(),
        props: JsonMap::new(),
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("agent:test"),
    }
}

// spec: ING-065, ING-066, INV-09 (ADR-016)
#[test]
fn scope_visibility_rules_for_literal_callers_and_ephemeral_expiry() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("scopes.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant_a = TenantId::new("tenant-a").expect("t").scope();
    let tenant_b = TenantId::new("tenant-b").expect("t").scope();

    let t_root = TaskId::from_unix_ms(1_700_000_000_000, [1u8; 10]);
    let t_child = TaskId::from_unix_ms(1_700_000_000_001, [2u8; 10]);
    let t_grandchild = TaskId::from_unix_ms(1_700_000_000_002, [3u8; 10]);
    let t_unrelated = TaskId::from_unix_ms(1_700_000_000_003, [9u8; 10]);

    let alice_full = Caller::new("alice")
        .with_groups(["sec-eng", "infra"])
        .with_task_chain(vec![t_child, t_root]);
    let bob_plain = Caller::new("bob");
    let carol_group = Caller::new("carol").with_groups(["sec-eng"]);
    let dave_wrong_group = Caller::new("dave").with_groups(["marketing"]);
    let eve_descendant_task =
        Caller::new("eve").with_task_chain(vec![t_grandchild, t_child, t_root]);
    let frank_root_only = Caller::new("frank").with_task_chain(vec![t_root]);
    let grace_unrelated_task = Caller::new("grace").with_task_chain(vec![t_unrelated]);

    // 1. Private node written by Alice.
    let priv_id = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(None, "alice-secret", ScopeSpec::Private),
        )
        .expect("put private");
    let priv_node = store
        .get_node(&tenant_a, &alice_full, &priv_id, None)
        .expect("alice get")
        .expect("visible to alice");
    assert_eq!(priv_node.scope, Scope::Private);
    assert_eq!(priv_node.owner_principal, "alice");
    assert!(
        store
            .get_node(&tenant_a, &bob_plain, &priv_id, None)
            .expect("bob get")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &carol_group, &priv_id, None)
            .expect("carol get")
            .is_none()
    );

    // 2. Group node ("sec-eng").
    let grp_id = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(
                None,
                "sec-runbook",
                ScopeSpec::Group {
                    group: "sec-eng".to_owned(),
                },
            ),
        )
        .expect("put group");
    let grp_node = store
        .get_node(&tenant_a, &carol_group, &grp_id, None)
        .expect("carol get")
        .expect("visible to group member");
    assert_eq!(grp_node.scope, Scope::Group);
    assert_eq!(grp_node.group.as_deref(), Some("sec-eng"));
    assert!(
        store
            .get_node(&tenant_a, &dave_wrong_group, &grp_id, None)
            .expect("dave get")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &bob_plain, &grp_id, None)
            .expect("bob get")
            .is_none()
    );

    // 3. Task nodes: one on t_root, one on t_child.
    let root_task_id = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(None, "root-plan", ScopeSpec::Task { task_id: t_root }),
        )
        .expect("put root task node");
    let child_task_id = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(None, "child-step", ScopeSpec::Task { task_id: t_child }),
        )
        .expect("put child task node");

    // Descendant task chain [t_grandchild, t_child, t_root] sees BOTH root and child task nodes!
    assert!(
        store
            .get_node(&tenant_a, &eve_descendant_task, &root_task_id, None)
            .expect("eve root")
            .is_some()
    );
    assert!(
        store
            .get_node(&tenant_a, &eve_descendant_task, &child_task_id, None)
            .expect("eve child")
            .is_some()
    );
    // Root-only chain [t_root] sees t_root node, NOT t_child node!
    assert!(
        store
            .get_node(&tenant_a, &frank_root_only, &root_task_id, None)
            .expect("frank root")
            .is_some()
    );
    assert!(
        store
            .get_node(&tenant_a, &frank_root_only, &child_task_id, None)
            .expect("frank child")
            .is_none()
    );
    // Unrelated task sees neither.
    assert!(
        store
            .get_node(&tenant_a, &grace_unrelated_task, &root_task_id, None)
            .expect("grace root")
            .is_none()
    );

    // 4. Global node.
    let global_id = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(
                Some(NodeId::new("doc:architecture").expect("id")),
                "Arch",
                ScopeSpec::Global,
            ),
        )
        .expect("put global");
    let g_node = store
        .get_node(&tenant_a, &bob_plain, &global_id, None)
        .expect("bob global")
        .expect("visible to bob");
    assert_eq!(g_node.scope, Scope::Global);
    assert_eq!(g_node.owner_principal, "alice");

    // Cross-tenant check with identical Alice caller claims: sees nothing in tenant_b!
    for id in [&priv_id, &grp_id, &root_task_id, &child_task_id, &global_id] {
        assert!(
            store
                .get_node(&tenant_b, &alice_full, id, None)
                .expect("tenant_b get")
                .is_none()
        );
    }
    assert_eq!(
        store.counts(&tenant_b, &alice_full, None).expect("tenant_b counts"),
        GraphCounts { nodes: 0, edges: 0 }
    );

    // 5. Ephemeral scopes (all 4 BaseScopes + default 1h TTL expiry under ManualClock).
    let t_eph_created = manual.now_timestamp();
    let eph_priv = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(
                None,
                "eph-priv",
                ScopeSpec::ephemeral_default(BaseScope::Private),
            ),
        )
        .expect("eph priv");
    let eph_grp = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(
                None,
                "eph-grp",
                ScopeSpec::ephemeral_default(BaseScope::Group {
                    group: "sec-eng".to_owned(),
                }),
            ),
        )
        .expect("eph grp");
    let eph_task = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(
                None,
                "eph-task",
                ScopeSpec::ephemeral_default(BaseScope::Task { task_id: t_root }),
            ),
        )
        .expect("eph task");
    let eph_glob = store
        .put_node(
            &tenant_a,
            &alice_full,
            new_node(
                None,
                "eph-glob",
                ScopeSpec::ephemeral_default(BaseScope::Global),
            ),
        )
        .expect("eph glob");

    // Before expiry: base scope rules govern visibility!
    assert_eq!(
        store
            .get_node(&tenant_a, &alice_full, &eph_priv, None)
            .expect("ok")
            .expect("vis")
            .scope,
        Scope::Ephemeral
    );
    assert!(
        store
            .get_node(&tenant_a, &bob_plain, &eph_priv, None)
            .expect("ok")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &carol_group, &eph_grp, None)
            .expect("ok")
            .is_some()
    );
    assert!(
        store
            .get_node(&tenant_a, &dave_wrong_group, &eph_grp, None)
            .expect("ok")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &eve_descendant_task, &eph_task, None)
            .expect("ok")
            .is_some()
    );
    assert!(
        store
            .get_node(&tenant_a, &grace_unrelated_task, &eph_task, None)
            .expect("ok")
            .is_none()
    );
    assert!(
        store
            .get_node(&tenant_a, &bob_plain, &eph_glob, None)
            .expect("ok")
            .is_some()
    );

    // Advance to 1 microsecond BEFORE 1h default TTL: still visible!
    manual.advance(Duration::from_micros(3_600_000_000 - 1));
    assert!(
        store
            .get_node(&tenant_a, &bob_plain, &eph_glob, None)
            .expect("1us before expiry")
            .is_some()
    );

    // Advance 1 more microsecond (exact `expires_at`): expired, NEVER returned even before purge
    // and even when querying with historical `as_of = Some(t_eph_created)`!
    manual.advance(Duration::from_micros(1));
    for id in [&eph_priv, &eph_grp, &eph_task, &eph_glob] {
        assert!(
            store
                .get_node(&tenant_a, &alice_full, id, None)
                .expect("expired now")
                .is_none()
        );
        assert!(
            store
                .get_node(&tenant_a, &alice_full, id, Some(t_eph_created))
                .expect("expired even at historical as_of")
                .is_none()
        );
    }
    // Bob now sees only the 1 Global non-ephemeral node.
    assert_eq!(
        store.counts(&tenant_a, &bob_plain, None).expect("bob counts"),
        GraphCounts { nodes: 1, edges: 0 }
    );
}

// spec: ING-065, ING-066
#[test]
fn no_id_oracle_uuidv7_minting_and_scoped_updates() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("oracle.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("t").scope();
    let t_id = TaskId::from_unix_ms(1_700_000_000_000, [5u8; 10]);
    let alice = Caller::new("alice")
        .with_groups(["eng"])
        .with_task_chain(vec![t_id]);
    let bob = Caller::new("bob");

    // Non-Global scopes reject caller-chosen IDs on creation with NotFound (no existence oracle).
    let chosen = NodeId::new("caller-chosen-secret-id").expect("valid id");
    for non_global_scope in [
        ScopeSpec::Private,
        ScopeSpec::Group {
            group: "eng".to_owned(),
        },
        ScopeSpec::Task { task_id: t_id },
        ScopeSpec::ephemeral_default(BaseScope::Private),
    ] {
        let err = store
            .put_node(
                &tenant,
                &alice,
                new_node(Some(chosen.clone()), "probe", non_global_scope),
            )
            .expect_err("non-GLOBAL creation with caller id must fail");
        assert_eq!(err, StoreError::NotFound);
    }

    // Minting with id: None produces RFC 9562 UUIDv7 IDs.
    let minted_priv = store
        .put_node(&tenant, &alice, new_node(None, "v1", ScopeSpec::Private))
        .expect("mint private");
    let parsed_uuid = Uuid::parse_str(minted_priv.as_str()).expect("must be valid UUID");
    assert_eq!(parsed_uuid.get_version_num(), 7);

    // Owner can update the scoped node using its minted ID.
    manual.advance(Duration::from_secs(5));
    let updated_id = store
        .put_node(
            &tenant,
            &alice,
            new_node(Some(minted_priv.clone()), "v2", ScopeSpec::Private),
        )
        .expect("owner update scoped node");
    assert_eq!(updated_id, minted_priv);
    assert_eq!(
        store
            .get_node(&tenant, &alice, &minted_priv, None)
            .expect("get")
            .expect("exists")
            .label,
        "v2"
    );

    // Non-owner attempting to update that exact minted ID gets NotFound.
    assert_eq!(
        store
            .put_node(
                &tenant,
                &bob,
                new_node(Some(minted_priv), "hijack", ScopeSpec::Private),
            )
            .expect_err("bob cannot update alice private node"),
        StoreError::NotFound
    );

    // Global node created with deterministic ID cannot be downgraded to Private!
    let glob_id = store
        .put_node(
            &tenant,
            &alice,
            new_node(
                Some(NodeId::new("sym:public").expect("id")),
                "pub",
                ScopeSpec::Global,
            ),
        )
        .expect("create global");
    assert!(matches!(
        store.put_node(
            &tenant,
            &alice,
            new_node(Some(glob_id), "downgrade", ScopeSpec::Private),
        ),
        Err(StoreError::Invalid { field: "scope", .. })
    ));
}

// spec: ING-065, INV-15
#[test]
fn invisible_nodes_and_edges_are_byte_identical_to_missing_entities() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("indistinguishable.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("t").scope();
    let alice = Caller::new("alice");
    let bob = Caller::new("bob");

    let alice_priv_1 = store
        .put_node(&tenant, &alice, new_node(None, "secret-1", ScopeSpec::Private))
        .expect("p1");
    let alice_priv_2 = store
        .put_node(&tenant, &alice, new_node(None, "secret-2", ScopeSpec::Private))
        .expect("p2");
    let alice_edge = store
        .put_edge(&tenant, &alice, new_edge(&alice_priv_1, &alice_priv_2))
        .expect("edge");
    let bob_vis_node = store
        .put_node(
            &tenant,
            &bob,
            new_node(
                Some(NodeId::new("bob:public").expect("id")),
                "bob-pub",
                ScopeSpec::Global,
            ),
        )
        .expect("bob global");

    let missing_node = NodeId::new("018ba1f0-0000-7000-8000-000000000099").expect("missing node");
    let missing_edge = EdgeId::new("018ba1f0-0000-7000-8000-000000000098").expect("missing edge");

    // 1. get_node: serialized response and Debug output are byte-identical.
    let get_invis = store.get_node(&tenant, &bob, &alice_priv_1, None);
    let get_miss = store.get_node(&tenant, &bob, &missing_node, None);
    assert_eq!(format!("{get_invis:?}"), format!("{get_miss:?}"));
    assert_eq!(
        serde_json::to_vec(&get_invis.expect("ok")).expect("json"),
        serde_json::to_vec(&get_miss.expect("ok")).expect("json")
    );

    // 2. get_edge: invisible edge vs missing edge.
    let edge_invis = store.get_edge(&tenant, &bob, &alice_edge, None);
    let edge_miss = store.get_edge(&tenant, &bob, &missing_edge, None);
    assert_eq!(format!("{edge_invis:?}"), format!("{edge_miss:?}"));
    assert_eq!(
        serde_json::to_vec(&edge_invis.expect("ok")).expect("json"),
        serde_json::to_vec(&edge_miss.expect("ok")).expect("json")
    );

    // 3. put_node targeting invisible vs missing.
    let put_invis_err = store
        .put_node(
            &tenant,
            &bob,
            new_node(Some(alice_priv_1.clone()), "probe", ScopeSpec::Private),
        )
        .expect_err("invis put");
    let put_miss_err = store
        .put_node(
            &tenant,
            &bob,
            new_node(Some(missing_node.clone()), "probe", ScopeSpec::Private),
        )
        .expect_err("miss put");
    assert_eq!(put_invis_err, put_miss_err);
    assert_eq!(put_invis_err.to_string(), put_miss_err.to_string());
    assert_eq!(
        serde_json::to_vec(&put_invis_err.into_gap_error("corr-1")).expect("json"),
        serde_json::to_vec(&put_miss_err.into_gap_error("corr-1")).expect("json")
    );

    // 4. put_edge with invisible endpoint vs missing endpoint (both `to` and `from`).
    let pe_invis = store
        .put_edge(&tenant, &bob, new_edge(&bob_vis_node, &alice_priv_1))
        .expect_err("invis to");
    let pe_miss = store
        .put_edge(&tenant, &bob, new_edge(&bob_vis_node, &missing_node))
        .expect_err("miss to");
    assert_eq!(pe_invis.to_string(), pe_miss.to_string());

    let pe_from_invis = store
        .put_edge(&tenant, &bob, new_edge(&alice_priv_1, &bob_vis_node))
        .expect_err("invis from");
    let pe_from_miss = store
        .put_edge(&tenant, &bob, new_edge(&missing_node, &bob_vis_node))
        .expect_err("miss from");
    assert_eq!(pe_from_invis.to_string(), pe_from_miss.to_string());

    // 5. tombstone_node and delete_node targeting invisible vs missing.
    let tomb_invis = store
        .tombstone_node(&tenant, &bob, &alice_priv_1)
        .expect_err("invis tomb");
    let tomb_miss = store
        .tombstone_node(&tenant, &bob, &missing_node)
        .expect_err("miss tomb");
    assert_eq!(tomb_invis.to_string(), tomb_miss.to_string());

    let del_invis = store
        .delete_node(&tenant, &bob, &alice_priv_1)
        .expect_err("invis del");
    let del_miss = store
        .delete_node(&tenant, &bob, &missing_node)
        .expect_err("miss del");
    assert_eq!(del_invis.to_string(), del_miss.to_string());
}

// spec: ING-065, ING-066
#[test]
fn edge_visible_and_counted_only_when_both_endpoints_are_visible() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("edge_vis.db");
    let manual = Arc::new(ManualClock::new(1_700_000_000_000));
    let clock: Arc<dyn Clock> = manual.clone();
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open store");

    let tenant = TenantId::new("acme").expect("t").scope();
    let alice = Caller::new("alice");
    let bob = Caller::new("bob");

    let g1 = store
        .put_node(
            &tenant,
            &alice,
            new_node(Some(NodeId::new("g1").expect("id")), "g1", ScopeSpec::Global),
        )
        .expect("g1");
    let g2 = store
        .put_node(
            &tenant,
            &alice,
            new_node(Some(NodeId::new("g2").expect("id")), "g2", ScopeSpec::Global),
        )
        .expect("g2");
    let p1 = store
        .put_node(&tenant, &alice, new_node(None, "p1", ScopeSpec::Private))
        .expect("p1");
    let eph = store
        .put_node(
            &tenant,
            &alice,
            new_node(None, "eph", ScopeSpec::ephemeral_default(BaseScope::Global)),
        )
        .expect("eph");

    let e_gg = store
        .put_edge(&tenant, &alice, new_edge(&g1, &g2))
        .expect("g1->g2");
    let e_gp = store
        .put_edge(&tenant, &alice, new_edge(&g1, &p1))
        .expect("g1->p1");
    let e_pg = store
        .put_edge(&tenant, &alice, new_edge(&p1, &g2))
        .expect("p1->g2");
    let e_ge = store
        .put_edge(&tenant, &alice, new_edge(&g1, &eph))
        .expect("g1->eph");

    // Alice sees all 4 nodes and all 4 edges.
    assert_eq!(
        store.counts(&tenant, &alice, None).expect("alice counts"),
        GraphCounts { nodes: 4, edges: 4 }
    );

    // Bob sees g1, g2, eph (3 nodes) and e_gg, e_ge (2 edges) — never e_gp or e_pg!
    assert_eq!(
        store.counts(&tenant, &bob, None).expect("bob counts"),
        GraphCounts { nodes: 3, edges: 2 }
    );
    assert!(store.get_edge(&tenant, &bob, &e_gg, None).expect("ok").is_some());
    assert!(store.get_edge(&tenant, &bob, &e_ge, None).expect("ok").is_some());
    assert!(store.get_edge(&tenant, &bob, &e_gp, None).expect("ok").is_none());
    assert!(store.get_edge(&tenant, &bob, &e_pg, None).expect("ok").is_none());

    // Once `eph` expires, e_ge immediately becomes invisible and uncounted too (before purge)!
    manual.advance(Duration::from_secs(3_600));
    assert_eq!(
        store.counts(&tenant, &bob, None).expect("bob counts post-expiry"),
        GraphCounts { nodes: 2, edges: 1 }
    );
    assert!(
        store
            .get_edge(&tenant, &bob, &e_ge, None)
            .expect("ok")
            .is_none()
    );
}
