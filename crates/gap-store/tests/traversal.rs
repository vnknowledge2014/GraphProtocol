//! Integration and property tests for `ContextStore::traverse` across `BFS`, `DFS`, `SHORTEST`,
//! and `WEIGHTED`, including literal hand-computed graphs, cycles/self-loops/parallel edges,
//! boundary depths/limits, `visited_cap` truncation, invalid weights, resume paging, `Direction::Both`,
//! visibility/tenant indistinguishability, and a 40-node `proptest` against an independent
//! in-memory reference model.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_store::types::{micros_to_timestamp, timestamp_to_micros};
use gap_store::{
    BaseScope, Caller, ContextStore, Direction, Durability, Edge, EdgeId, EdgeResolution, EdgeType,
    NewEdge, NewNode, Node, NodeId, NodeType, Provenance, Scope, ScopeSpec, SqliteConfig,
    SqliteContextStore, StoreError, TaskId, TenantId, TraverseQuery, TraverseResult,
    TraverseResume, TraverseStats, TraverseStrategy, TraverseWarning,
};
use proptest::prelude::*;
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use tempfile::TempDir;

fn global_node(id: &str) -> NewNode {
    NewNode {
        id: Some(NodeId::new(id).expect("id")),
        node_type: NodeType::new("Symbol").expect("type"),
        label: format!("lbl:{id}"),
        summary: None,
        props: JsonMap::new(),
        scope: ScopeSpec::Global,
        provenance: Provenance::new("test"),
    }
}

fn edge_with_props(
    id: &str,
    edge_type: &str,
    from: &str,
    to: &str,
    props: JsonMap<String, JsonValue>,
) -> NewEdge {
    NewEdge {
        id: Some(EdgeId::new(id).expect("eid")),
        edge_type: EdgeType::new(edge_type).expect("etype"),
        from: NodeId::new(from).expect("from"),
        to: NodeId::new(to).expect("to"),
        props,
        confidence: Some(1.0),
        resolution: EdgeResolution::Exact,
        provenance: Provenance::new("test"),
    }
}

fn weighted_edge(id: &str, from: &str, to: &str, weight: JsonValue) -> NewEdge {
    let mut props = JsonMap::new();
    props.insert("weight".to_owned(), weight);
    edge_with_props(id, "CALLS", from, to, props)
}

fn plain_edge(id: &str, edge_type: &str, from: &str, to: &str) -> NewEdge {
    edge_with_props(id, edge_type, from, to, JsonMap::new())
}

fn node_ids(res: &TraverseResult) -> Vec<&str> {
    res.nodes.iter().map(|n| n.id.as_str()).collect()
}

fn edge_ids(res: &TraverseResult) -> Vec<&str> {
    res.edges.iter().map(|e| e.id.as_str()).collect()
}

// spec: ING-064, ING-067, GQL-040
#[test]
fn literal_hand_computed_orders_for_bfs_dfs_shortest_and_weighted() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("literal_strategies.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // Graph:
    //   s -> a (e_sa, weight 4.0)
    //   s -> b (e_sb, weight 1.0)
    //   a -> c (e_ac, weight 1.0)
    //   a -> d (e_ad, weight 1.0)
    //   b -> c (e_bc, weight 1.0)
    //   c -> e (e_ce, weight 0.5)
    //   b -> e (e_be, weight 10.0)
    let nodes = ["s", "a", "b", "c", "d", "e"]
        .into_iter()
        .map(global_node)
        .collect::<Vec<_>>();
    let edges = vec![
        weighted_edge("e_sa", "s", "a", json!(4.0)),
        weighted_edge("e_sb", "s", "b", json!(1.0)),
        weighted_edge("e_ac", "a", "c", json!(1.0)),
        weighted_edge("e_ad", "a", "d", json!(1.0)),
        weighted_edge("e_bc", "b", "c", json!(1.0)),
        weighted_edge("e_ce", "c", "e", json!(0.5)),
        weighted_edge("e_be", "b", "e", json!(10.0)),
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &edges)
        .expect("seed graph");

    let mut q = TraverseQuery::new(NodeId::new("s").expect("id"));
    q.max_depth = 3;
    q.first = 100;

    // 1. BFS:
    // depth 0: s
    // depth 1: a, b
    // depth 2: c, d, e (e reached via b -> e at depth 2!)
    // Edges ordered by (depth(from), edge_id):
    // depth 0 (s): e_sa, e_sb
    // depth 1 (a, b): e_ac, e_ad, e_bc, e_be
    // depth 2 (c): e_ce
    q.strategy = TraverseStrategy::Bfs;
    let bfs_res = store.traverse(&tenant, &caller, &q).expect("bfs");
    assert_eq!(node_ids(&bfs_res), vec!["s", "a", "b", "c", "d", "e"]);
    assert_eq!(
        edge_ids(&bfs_res),
        vec!["e_sa", "e_sb", "e_ac", "e_ad", "e_bc", "e_be", "e_ce"]
    );
    assert_eq!(
        bfs_res.stats,
        TraverseStats {
            visited: 6,
            returned: 6,
            depth_reached: 2,
        }
    );
    assert!(!bfs_res.truncated);
    assert!(bfs_res.resume.is_none());

    // 2. SHORTEST:
    // Same nodes [s, a, b, c, d, e], but each reached node has ONLY the single edge from its
    // smallest-id predecessor at minimal depth:
    //   a <- s (e_sa)
    //   b <- s (e_sb)
    //   c <- a (e_ac; both a and b are at depth 1, "a" < "b"!)
    //   d <- a (e_ad)
    //   e <- b (e_be; b is at depth 1 so e has min_depth 2 via b, whereas c is at depth 2!)
    q.strategy = TraverseStrategy::Shortest;
    let shortest_res = store.traverse(&tenant, &caller, &q).expect("shortest");
    assert_eq!(node_ids(&shortest_res), vec!["s", "a", "b", "c", "d", "e"]);
    assert_eq!(
        edge_ids(&shortest_res),
        vec!["e_sa", "e_sb", "e_ac", "e_ad", "e_be"]
    );

    // 3. DFS preorder (neighbours in ascending (neighbour_id, edge_id)):
    // s(0) -> a(1) -> c(2) -> e(3), then d(2), then b(1) (which sees c at depth 2 >= 2 and e at depth 2 < 3).
    // Preorder emission: [s, a, c, e, d, b]!
    q.strategy = TraverseStrategy::Dfs;
    let dfs_res = store.traverse(&tenant, &caller, &q).expect("dfs");
    assert_eq!(node_ids(&dfs_res), vec!["s", "a", "c", "e", "d", "b"]);
    assert_eq!(dfs_res.stats.visited, 6);
    assert_eq!(dfs_res.stats.depth_reached, 3);

    // 4. WEIGHTED at max_depth = 3:
    // Minimum costs within <= 3 hops:
    //   s = 0.0 (0 hops)
    //   b = 1.0 (1 hop: s->b)
    //   c = 2.0 (2 hops: s->b->c)
    //   e = 2.5 (3 hops: s->b->c->e)
    //   a = 4.0 (1 hop: s->a)
    //   d = 5.0 (2 hops: s->a->d)
    q.strategy = TraverseStrategy::Weighted;
    let w3_res = store.traverse(&tenant, &caller, &q).expect("weighted d=3");
    assert_eq!(node_ids(&w3_res), vec!["s", "b", "c", "e", "a", "d"]);
    assert!(w3_res.warnings.is_empty());

    // Now restrict max_depth = 2 in WEIGHTED:
    // Path s->b->c->e has 3 edges (> 2!), so `e` must settle via 2-hop path s->b->e at cost 11.0!
    // Order at max_depth = 2: s(0.0), b(1.0), c(2.0), a(4.0), d(5.0), e(11.0)!
    q.max_depth = 2;
    let w2_res = store.traverse(&tenant, &caller, &q).expect("weighted d=2");
    assert_eq!(node_ids(&w2_res), vec!["s", "b", "c", "a", "d", "e"]);
}

// spec: ING-064, ING-067, GQL-040
#[test]
fn cycles_self_loops_parallel_edges_directions_and_depth_cap_boundaries() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("cycles_bounds.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // Cycle + self-loop + parallel edges graph:
    //   n1 -> n1 (self-loop)
    //   n1 ==e_calls==> n2, n1 ==e_refs==> n2 (parallel edges of different types)
    //   n2 -> n3 -> n1 (3-cycle)
    //   n0 -> n1 (inbound to n1!)
    let nodes: Vec<NewNode> = ["n0", "n1", "n2", "n3"]
        .into_iter()
        .map(global_node)
        .collect();
    let edges = vec![
        plain_edge("e_self", "CALLS", "n1", "n1"),
        plain_edge("e_12_b", "REFERENCES", "n1", "n2"),
        plain_edge("e_12_a", "CALLS", "n1", "n2"),
        plain_edge("e_23", "CALLS", "n2", "n3"),
        plain_edge("e_31", "CALLS", "n3", "n1"),
        plain_edge("e_01", "IMPORTS", "n0", "n1"),
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &edges)
        .expect("seed cycle graph");

    for strat in [
        TraverseStrategy::Bfs,
        TraverseStrategy::Dfs,
        TraverseStrategy::Shortest,
        TraverseStrategy::Weighted,
    ] {
        let mut q = TraverseQuery::new(NodeId::new("n1").expect("id"));
        q.max_depth = 16;
        q.strategy = strat;
        q.direction = Direction::Outbound;
        let out_res = store
            .traverse(&tenant, &caller, &q)
            .expect("outbound cycle");
        assert_eq!(node_ids(&out_res), vec!["n1", "n2", "n3"]);

        if strat == TraverseStrategy::Shortest {
            // Parallel edges n1->n2 tie-break to smallest edge_id `e_12_a`; self-loop and back-edge excluded!
            assert_eq!(edge_ids(&out_res), vec!["e_12_a", "e_23"]);
        } else {
            assert!(edge_ids(&out_res).contains(&"e_self"));
            assert!(edge_ids(&out_res).contains(&"e_12_a"));
            assert!(edge_ids(&out_res).contains(&"e_12_b"));
        }

        // Direction::Both reaches n0 as well!
        q.direction = Direction::Both;
        let both_res = store.traverse(&tenant, &caller, &q).expect("both cycle");
        let mut sorted_ids = node_ids(&both_res);
        sorted_ids.sort_unstable();
        assert_eq!(sorted_ids, vec!["n0", "n1", "n2", "n3"]);
    }

    // Direction::Inbound from n1 with max_depth = 1 reaches n0 (via e_01), n1 (self-loop), and n3 (via e_31), NOT n2!
    let mut in_q = TraverseQuery::new(NodeId::new("n1").expect("id"));
    in_q.direction = Direction::Inbound;
    in_q.max_depth = 1;
    let in_res = store
        .traverse(&tenant, &caller, &in_q)
        .expect("inbound d=1");
    assert_eq!(node_ids(&in_res), vec!["n1", "n0", "n3"]);

    // 18-node chain c00 -> c01 -> ... -> c17 to test depth 1, 16, and 17, and first 1, 1000, 1001.
    let chain_nodes: Vec<NewNode> = (0..=17).map(|i| global_node(&format!("c{i:02}"))).collect();
    let chain_edges: Vec<NewEdge> = (0..17)
        .map(|i| {
            plain_edge(
                &format!("ce{i:02}"),
                "CALLS",
                &format!("c{i:02}"),
                &format!("c{:02}", i + 1),
            )
        })
        .collect();
    store
        .put_batch(&tenant, &caller, &chain_nodes, &chain_edges)
        .expect("seed chain");

    let mut d1_q = TraverseQuery::new(NodeId::new("c00").expect("id"));
    d1_q.max_depth = 1;
    let d1_res = store.traverse(&tenant, &caller, &d1_q).expect("d=1");
    assert_eq!(node_ids(&d1_res), vec!["c00", "c01"]);
    assert_eq!(d1_res.stats.depth_reached, 1);

    let mut d16_q = TraverseQuery::new(NodeId::new("c00").expect("id"));
    d16_q.max_depth = 16;
    d16_q.first = 1_000;
    for strat in [
        TraverseStrategy::Bfs,
        TraverseStrategy::Dfs,
        TraverseStrategy::Shortest,
        TraverseStrategy::Weighted,
    ] {
        d16_q.strategy = strat;
        let res = store.traverse(&tenant, &caller, &d16_q).expect("d=16");
        assert_eq!(res.nodes.len(), 17, "c00..=c16 == 17 nodes for {strat:?}");
        assert_eq!(res.nodes.last().expect("last").id.as_str(), "c16");
        assert!(!node_ids(&res).contains(&"c17"));
        assert_eq!(res.stats.depth_reached, 16);
    }

    let mut d17_q = d16_q.clone();
    d17_q.max_depth = 17;
    assert!(matches!(
        store.traverse(&tenant, &caller, &d17_q),
        Err(StoreError::Invalid {
            field: "max_depth",
            ..
        })
    ));

    let mut f1_q = d16_q.clone();
    f1_q.max_depth = 16;
    f1_q.first = 1;
    let f1_res = store.traverse(&tenant, &caller, &f1_q).expect("first=1");
    assert_eq!(node_ids(&f1_res), vec!["c00"]);
    assert!(f1_res.resume.is_some());

    let mut f1001_q = d16_q;
    f1001_q.first = 1_001;
    assert!(matches!(
        store.traverse(&tenant, &caller, &f1001_q),
        Err(StoreError::Invalid { field: "first", .. })
    ));
}

// spec: ING-064, ING-065, ING-067
#[test]
fn weighted_invalid_weights_warnings_visited_cap_truncation_resume_and_indistinguishability() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("weighted_resume.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(&db_path), clock).expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let alice = Caller::new("alice");
    let bob = Caller::new("bob");

    // Nodes: s, alpha, beta, bad_neg, bad_nan_null, bad_inf_str, via_default
    let nodes: Vec<NewNode> = [
        "s",
        "alpha",
        "beta",
        "bad_neg",
        "bad_nan_null",
        "bad_inf_str",
        "via_default",
    ]
    .into_iter()
    .map(global_node)
    .collect();

    // Note: json!(f64::NAN) and json!(f64::INFINITY) serialize to JsonValue::Null in serde_json.
    // We test negative (-2.5), f64::NAN (-> Null), f64::INFINITY (-> Null), and string "Infinity".
    let edges = vec![
        weighted_edge("e_tie_beta", "s", "beta", json!(1.5)),
        weighted_edge("e_tie_alpha", "s", "alpha", json!(1.5)),
        plain_edge("e_absent_default", "CALLS", "s", "via_default"), // absent weight => 1.0!
        weighted_edge("e_neg", "s", "bad_neg", json!(-2.5)),
        weighted_edge("e_nan", "s", "bad_nan_null", json!(f64::NAN)),
        weighted_edge("e_inf_null", "alpha", "bad_inf_str", json!(f64::INFINITY)),
        weighted_edge("e_inf_str", "beta", "bad_inf_str", json!("Infinity")),
    ];
    store
        .put_batch(&tenant, &alice, &nodes, &edges)
        .expect("seed weighted");

    let mut wq = TraverseQuery::new(NodeId::new("s").expect("id"));
    wq.strategy = TraverseStrategy::Weighted;
    wq.max_depth = 3;
    let wres = store.traverse(&tenant, &alice, &wq).expect("weighted");

    // Expected order: s (0.0), via_default (1.0), alpha (1.5), beta (1.5 — tie broken after alpha!).
    // None of bad_neg, bad_nan_null, bad_inf_str are visited!
    assert_eq!(node_ids(&wres), vec!["s", "via_default", "alpha", "beta"]);
    assert_eq!(
        wres.warnings,
        vec![TraverseWarning::SkippedInvalidWeights { count: 4 }]
    );

    // Add an invisible Private node owned by Alice hanging off `beta`, and verify:
    // 1) Bob gets `truncated = false` at visited_cap = 4 (even though Alice has a 5th node!).
    // 2) Alice gets `truncated = true` at visited_cap = 4 and `truncated = false` at visited_cap = 5!
    let alice_priv = store
        .put_node(
            &tenant,
            &alice,
            NewNode {
                id: None,
                node_type: NodeType::new("Symbol").expect("t"),
                label: "secret".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Private,
                provenance: Provenance::new("test"),
            },
        )
        .expect("alice priv");
    store
        .put_edge(
            &tenant,
            &alice,
            NewEdge {
                id: None,
                edge_type: EdgeType::new("CALLS").expect("t"),
                from: NodeId::new("beta").expect("id"),
                to: alice_priv.clone(),
                props: JsonMap::new(),
                confidence: Some(1.0),
                resolution: EdgeResolution::Exact,
                provenance: Provenance::new("test"),
            },
        )
        .expect("priv edge");

    let mut cap_q = TraverseQuery::new(NodeId::new("s").expect("id"));
    cap_q.max_depth = 3;
    for (strat, bob_visible_total) in [
        (TraverseStrategy::Bfs, 7_u32),
        (TraverseStrategy::Dfs, 7_u32),
        (TraverseStrategy::Shortest, 7_u32),
        (TraverseStrategy::Weighted, 4_u32),
    ] {
        cap_q.strategy = strat;
        cap_q.visited_cap = Some(bob_visible_total);
        let bob_res = store.traverse(&tenant, &bob, &cap_q).expect("bob at cap");
        assert_eq!(bob_res.stats.visited, bob_visible_total);
        assert!(
            !bob_res.truncated,
            "invisible node must never trigger truncated for Bob in {strat:?}"
        );

        let alice_res = store
            .traverse(&tenant, &alice, &cap_q)
            .expect("alice at cap");
        assert_eq!(alice_res.stats.visited, bob_visible_total);
        assert!(
            alice_res.truncated,
            "extra visible node must set truncated=true for Alice in {strat:?}"
        );
    }

    // Indistinguishability when starting from Alice's Private node vs a non-existent node for Bob.
    let missing_start = NodeId::new("018ba1f0-0000-7000-8000-ffffffffffff").expect("id");
    let invis_start_res = store
        .traverse(&tenant, &bob, &TraverseQuery::new(alice_priv))
        .expect("invis start");
    let miss_start_res = store
        .traverse(&tenant, &bob, &TraverseQuery::new(missing_start))
        .expect("miss start");
    assert_eq!(
        serde_json::to_vec(&invis_start_res).expect("json"),
        serde_json::to_vec(&miss_start_res).expect("json")
    );

    // Stateless resume pagination across all 4 strategies with first = 2 concatenates to full result!
    for (strat, expected_alice_total) in [
        (TraverseStrategy::Bfs, 8_usize),
        (TraverseStrategy::Dfs, 8_usize),
        (TraverseStrategy::Shortest, 8_usize),
        (TraverseStrategy::Weighted, 5_usize),
    ] {
        let mut full_q = TraverseQuery::new(NodeId::new("s").expect("id"));
        full_q.strategy = strat;
        full_q.max_depth = 3;
        full_q.first = 100;
        let full = store.traverse(&tenant, &alice, &full_q).expect("full");
        assert_eq!(full.nodes.len(), expected_alice_total);

        let mut paged_nodes = Vec::new();
        let mut cursor: Option<TraverseResume> = None;
        loop {
            let mut pq = full_q.clone();
            pq.first = 2;
            pq.as_of = Some(full.as_of);
            pq.after = cursor.clone();
            let page = store.traverse(&tenant, &alice, &pq).expect("page");
            assert!(page.nodes.len() <= 2);
            assert_eq!(
                page.stats.visited, expected_alice_total as u32,
                "visited counts skipped nodes too"
            );
            paged_nodes.extend(page.nodes);
            match page.resume {
                Some(next_token) => {
                    // Round-trip through JSON to verify Serialize + Deserialize stability.
                    let encoded = serde_json::to_string(&next_token).expect("ser");
                    cursor = Some(serde_json::from_str(&encoded).expect("de"));
                }
                None => break,
            }
        }
        assert_eq!(
            paged_nodes, full.nodes,
            "paged nodes must concatenate to full for {strat:?}"
        );
    }
}

// ============================================================================
// Independent test-only in-memory reference model (zero SQL) for proptest
// ============================================================================

#[derive(Clone)]
struct RefNodeVersion {
    node: Node,
    base_scope: Scope,
}

#[derive(Default)]
struct ReferenceGraph {
    nodes: Vec<RefNodeVersion>,
    edges: Vec<Edge>,
}

impl ReferenceGraph {
    fn is_node_eligible(
        entry: &RefNodeVersion,
        caller: &Caller,
        now_us: i64,
        as_of_us: i64,
        scopes: Option<&[Scope]>,
    ) -> bool {
        let vf = timestamp_to_micros(entry.node.valid_from);
        let vt = entry.node.valid_to.map(timestamp_to_micros);
        if vf > as_of_us || vt.is_some_and(|end| as_of_us >= end) {
            return false;
        }
        if let Some(allowed) = scopes
            && !allowed.contains(&entry.node.scope)
        {
            return false;
        }
        if entry.node.scope == Scope::Ephemeral {
            let Some(exp) = entry.node.expires_at.map(timestamp_to_micros) else {
                return false;
            };
            if exp <= now_us || exp <= as_of_us {
                return false;
            }
        }
        match entry.base_scope {
            Scope::Global => true,
            Scope::Private => entry.node.owner_principal == caller.principal,
            Scope::Group => entry
                .node
                .group
                .as_deref()
                .is_some_and(|g| caller.groups.contains(g)),
            Scope::Task => entry
                .node
                .task_id
                .is_some_and(|t| caller.task_chain.contains(&t)),
            Scope::Ephemeral => false,
        }
    }

    fn traverse(
        &self,
        caller: &Caller,
        now_ts: Timestamp,
        query: &TraverseQuery,
    ) -> TraverseResult {
        let visited_cap = query.visited_cap.unwrap_or(100_000) as usize;
        let now_us = timestamp_to_micros(now_ts);
        let as_of_ts = query.as_of.unwrap_or(now_ts);
        let as_of_us = timestamp_to_micros(as_of_ts);

        let eligible_nodes: BTreeMap<NodeId, Node> = self
            .nodes
            .iter()
            .filter(|n| {
                Self::is_node_eligible(n, caller, now_us, as_of_us, query.scopes.as_deref())
            })
            .map(|n| (n.node.id.clone(), n.node.clone()))
            .collect();

        if !eligible_nodes.contains_key(&query.from) {
            return TraverseResult {
                nodes: Vec::new(),
                edges: Vec::new(),
                resume: None,
                truncated: false,
                stats: TraverseStats::default(),
                warnings: Vec::new(),
                as_of: as_of_ts,
            };
        }

        let eligible_edges: Vec<Edge> = self
            .edges
            .iter()
            .filter(|e| {
                let vf = timestamp_to_micros(e.valid_from);
                let vt = e.valid_to.map(timestamp_to_micros);
                let time_ok = vf <= as_of_us && vt.is_none_or(|end| as_of_us < end);
                let type_ok = query
                    .edge_types
                    .as_ref()
                    .is_none_or(|ets| ets.contains(&e.edge_type));
                time_ok
                    && type_ok
                    && eligible_nodes.contains_key(&e.from)
                    && eligible_nodes.contains_key(&e.to)
            })
            .cloned()
            .collect();

        let steps_from = |u: &NodeId, dir: Direction| -> Vec<(NodeId, Edge)> {
            let mut list = Vec::new();
            for e in &eligible_edges {
                let next_opt = match dir {
                    Direction::Outbound if &e.from == u => Some(e.to.clone()),
                    Direction::Inbound if &e.to == u => Some(e.from.clone()),
                    Direction::Both if &e.from == u || &e.to == u => Some(if &e.from == u {
                        e.to.clone()
                    } else {
                        e.from.clone()
                    }),
                    _ => None,
                };
                if let Some(v) = next_opt {
                    list.push((v, e.clone()));
                }
            }
            list.sort_by(|(na, ea), (nb, eb)| na.cmp(nb).then_with(|| ea.id.cmp(&eb.id)));
            list
        };

        match query.strategy {
            TraverseStrategy::Bfs | TraverseStrategy::Shortest => {
                let mut visited_order: Vec<(NodeId, u32)> = vec![(query.from.clone(), 0)];
                let mut min_depth: BTreeMap<NodeId, u32> = BTreeMap::new();
                min_depth.insert(query.from.clone(), 0);
                let mut level: Vec<NodeId> = vec![query.from.clone()];
                let mut truncated = false;
                let mut bfs_edges: BTreeMap<EdgeId, Edge> = BTreeMap::new();

                for d in 0..query.max_depth {
                    let mut next_candidates: BTreeSet<NodeId> = BTreeSet::new();
                    let mut level_edges: Vec<(NodeId, Edge)> = Vec::new();
                    for u in &level {
                        for (v, edge) in steps_from(u, query.direction) {
                            if !min_depth.contains_key(&v) {
                                next_candidates.insert(v.clone());
                            }
                            level_edges.push((v, edge));
                        }
                    }
                    if visited_order.len() + next_candidates.len() <= visited_cap {
                        for v in &next_candidates {
                            min_depth.insert(v.clone(), d + 1);
                            visited_order.push((v.clone(), d + 1));
                        }
                        for (v, edge) in level_edges {
                            if min_depth.contains_key(&v) {
                                bfs_edges.entry(edge.id.clone()).or_insert(edge);
                            }
                        }
                        if next_candidates.is_empty() {
                            break;
                        }
                        level = next_candidates.into_iter().collect();
                    } else {
                        let room = visited_cap - visited_order.len();
                        for v in next_candidates.into_iter().take(room) {
                            min_depth.insert(v.clone(), d + 1);
                            visited_order.push((v, d + 1));
                        }
                        for (v, edge) in level_edges {
                            if min_depth.contains_key(&v) {
                                bfs_edges.entry(edge.id.clone()).or_insert(edge);
                            }
                        }
                        truncated = true;
                        break;
                    }
                }

                let depth_reached = visited_order.iter().map(|(_, d)| *d).max().unwrap_or(0);
                let start_idx = match &query.after {
                    None => 0,
                    Some(
                        TraverseResume::Bfs { depth, node_id }
                        | TraverseResume::Shortest { depth, node_id },
                    ) => visited_order.partition_point(|(id, d)| (*d, id) <= (*depth, node_id)),
                    Some(_) => 0,
                };
                let remaining = &visited_order[start_idx..];
                let take_n = remaining.len().min(query.first as usize);
                let page_slice = &remaining[..take_n];
                let resume = if remaining.len() > take_n {
                    page_slice.last().map(|(id, d)| match query.strategy {
                        TraverseStrategy::Bfs => TraverseResume::Bfs {
                            depth: *d,
                            node_id: id.clone(),
                        },
                        _ => TraverseResume::Shortest {
                            depth: *d,
                            node_id: id.clone(),
                        },
                    })
                } else {
                    None
                };

                let nodes: Vec<Node> = page_slice
                    .iter()
                    .map(|(id, _)| eligible_nodes[id].clone())
                    .collect();
                let returned_depth: BTreeMap<NodeId, u32> = page_slice.iter().cloned().collect();

                let candidate_map = if query.strategy == TraverseStrategy::Bfs {
                    bfs_edges
                } else {
                    let inv_dir = match query.direction {
                        Direction::Outbound => Direction::Inbound,
                        Direction::Inbound => Direction::Outbound,
                        Direction::Both => Direction::Both,
                    };
                    let mut tree_edges = BTreeMap::new();
                    for (v, d) in &visited_order {
                        if *d == 0 {
                            continue;
                        }
                        if let Some((_, edge)) = steps_from(v, inv_dir)
                            .into_iter()
                            .find(|(u, _)| min_depth.get(u) == Some(&(d - 1)))
                        {
                            tree_edges.entry(edge.id.clone()).or_insert(edge);
                        }
                    }
                    tree_edges
                };

                let mut edges: Vec<Edge> = candidate_map
                    .into_values()
                    .filter(|e| {
                        returned_depth.contains_key(&e.from) && returned_depth.contains_key(&e.to)
                    })
                    .collect();
                edges.sort_by(|a, b| {
                    returned_depth[&a.from]
                        .cmp(&returned_depth[&b.from])
                        .then_with(|| a.id.cmp(&b.id))
                });

                TraverseResult {
                    stats: TraverseStats {
                        visited: visited_order.len() as u32,
                        returned: nodes.len() as u32,
                        depth_reached,
                    },
                    nodes,
                    edges,
                    resume,
                    truncated,
                    warnings: Vec::new(),
                    as_of: as_of_ts,
                }
            }
            TraverseStrategy::Dfs => {
                let mut emitted_order: Vec<(NodeId, u32)> = Vec::new();
                let mut emitted_depth: BTreeMap<NodeId, u32> = BTreeMap::new();
                let mut best_depth: BTreeMap<NodeId, u32> = BTreeMap::new();
                let mut traversed_edges: BTreeMap<EdgeId, Edge> = BTreeMap::new();
                let mut truncated = false;
                let mut stack: Vec<(NodeId, u32, Option<Edge>)> =
                    vec![(query.from.clone(), 0, None)];

                while let Some((u, d, via)) = stack.pop() {
                    if !emitted_depth.contains_key(&u) {
                        if emitted_order.len() == visited_cap {
                            truncated = true;
                            break;
                        }
                        emitted_depth.insert(u.clone(), d);
                        best_depth.insert(u.clone(), d);
                        emitted_order.push((u.clone(), d));
                        if let Some(e) = via {
                            traversed_edges.entry(e.id.clone()).or_insert(e);
                        }
                    } else {
                        if let Some(e) = via {
                            traversed_edges.entry(e.id.clone()).or_insert(e);
                        }
                        if d >= best_depth[&u] {
                            continue;
                        }
                        best_depth.insert(u.clone(), d);
                    }
                    if d < query.max_depth {
                        let steps = steps_from(&u, query.direction);
                        for (v, e) in steps.into_iter().rev() {
                            stack.push((v, d + 1, Some(e)));
                        }
                    }
                }

                let depth_reached = emitted_order.iter().map(|(_, d)| *d).max().unwrap_or(0);
                let start_idx = match &query.after {
                    None => 0,
                    Some(TraverseResume::Dfs { ordinal, node_id }) => {
                        if let Some(pos) = emitted_order.iter().position(|(id, _)| id == node_id) {
                            pos + 1
                        } else {
                            (*ordinal as usize + 1).min(emitted_order.len())
                        }
                    }
                    Some(_) => 0,
                };
                let remaining = &emitted_order[start_idx..];
                let take_n = remaining.len().min(query.first as usize);
                let page_slice = &remaining[..take_n];
                let resume = if remaining.len() > take_n && take_n > 0 {
                    let last_idx = start_idx + take_n - 1;
                    Some(TraverseResume::Dfs {
                        ordinal: last_idx as u32,
                        node_id: emitted_order[last_idx].0.clone(),
                    })
                } else {
                    None
                };

                let nodes: Vec<Node> = page_slice
                    .iter()
                    .map(|(id, _)| eligible_nodes[id].clone())
                    .collect();
                let returned_depth: BTreeMap<NodeId, u32> = page_slice.iter().cloned().collect();
                let mut edges: Vec<Edge> = traversed_edges
                    .into_values()
                    .filter(|e| {
                        returned_depth.contains_key(&e.from) && returned_depth.contains_key(&e.to)
                    })
                    .collect();
                edges.sort_by(|a, b| {
                    returned_depth[&a.from]
                        .cmp(&returned_depth[&b.from])
                        .then_with(|| a.id.cmp(&b.id))
                });

                TraverseResult {
                    stats: TraverseStats {
                        visited: emitted_order.len() as u32,
                        returned: nodes.len() as u32,
                        depth_reached,
                    },
                    nodes,
                    edges,
                    resume,
                    truncated,
                    warnings: Vec::new(),
                    as_of: as_of_ts,
                }
            }
            TraverseStrategy::Weighted => {
                let mut settled_order: Vec<(NodeId, u64, u32)> = Vec::new();
                let mut settled_depth: BTreeMap<NodeId, u32> = BTreeMap::new();
                let mut best_hops: HashMap<NodeId, [f64; 17]> = HashMap::new();
                let mut traversed_edges: BTreeMap<EdgeId, Edge> = BTreeMap::new();
                let mut skipped_invalid: BTreeSet<EdgeId> = BTreeSet::new();
                let mut cap_cost_bits: Option<u64> = None;
                let mut more_beyond_cap = false;

                let mut init = [f64::INFINITY; 17];
                init[0] = 0.0;
                best_hops.insert(query.from.clone(), init);

                let mut heap: BinaryHeap<Reverse<(u64, u8, NodeId)>> = BinaryHeap::new();
                heap.push(Reverse((0.0_f64.to_bits(), 0, query.from.clone())));

                while let Some(Reverse((bits, hops, u))) = heap.pop() {
                    let cost = f64::from_bits(bits);
                    let h = usize::from(hops);
                    if let Some(tbl) = best_hops.get(&u)
                        && (cost > tbl[h] || (0..h).any(|hp| tbl[hp] <= cost))
                    {
                        continue;
                    }

                    let in_post_cap_probe =
                        cap_cost_bits.is_some_and(|limit_bits| bits > limit_bits);
                    if in_post_cap_probe {
                        if settled_order.len() > visited_cap || !settled_depth.contains_key(&u) {
                            more_beyond_cap = true;
                            break;
                        }
                        if u32::from(hops) < query.max_depth {
                            for (v, e) in steps_from(&u, query.direction) {
                                let w_opt = match e.props.get("weight") {
                                    None => Some(1.0),
                                    Some(JsonValue::Number(num)) => num
                                        .as_f64()
                                        .filter(|w| w.is_finite() && *w >= 0.0)
                                        .map(|w| if w == 0.0 { 0.0 } else { w }),
                                    Some(_) => None,
                                };
                                let Some(w) = w_opt else {
                                    continue;
                                };
                                let raw_next = cost + w;
                                if !raw_next.is_finite() {
                                    continue;
                                }
                                let next = if raw_next == 0.0 { 0.0 } else { raw_next };
                                let nh = h + 1;
                                let v_tbl =
                                    best_hops.entry(v.clone()).or_insert([f64::INFINITY; 17]);
                                if (0..=nh).all(|hp| next < v_tbl[hp]) {
                                    v_tbl[nh] = next;
                                    heap.push(Reverse((next.to_bits(), hops + 1, v)));
                                }
                            }
                        }
                        continue;
                    }

                    if !settled_depth.contains_key(&u) {
                        settled_depth.insert(u.clone(), u32::from(hops));
                        settled_order.push((u.clone(), bits, u32::from(hops)));
                        if settled_order.len() == visited_cap {
                            cap_cost_bits = Some(bits);
                        }
                    }
                    if u32::from(hops) < query.max_depth {
                        for (v, e) in steps_from(&u, query.direction) {
                            let w_opt = match e.props.get("weight") {
                                None => Some(1.0),
                                Some(JsonValue::Number(num)) => num
                                    .as_f64()
                                    .filter(|w| w.is_finite() && *w >= 0.0)
                                    .map(|w| if w == 0.0 { 0.0 } else { w }),
                                Some(_) => None,
                            };
                            let Some(w) = w_opt else {
                                skipped_invalid.insert(e.id);
                                continue;
                            };
                            traversed_edges
                                .entry(e.id.clone())
                                .or_insert_with(|| e.clone());
                            let raw_next = cost + w;
                            if !raw_next.is_finite() {
                                continue;
                            }
                            let next = if raw_next == 0.0 { 0.0 } else { raw_next };
                            let nh = h + 1;
                            let v_tbl = best_hops.entry(v.clone()).or_insert([f64::INFINITY; 17]);
                            if (0..=nh).all(|hp| next < v_tbl[hp]) {
                                v_tbl[nh] = next;
                                heap.push(Reverse((next.to_bits(), hops + 1, v)));
                            }
                        }
                    }
                }

                settled_order.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
                let truncated = settled_order.len() > visited_cap || more_beyond_cap;
                if settled_order.len() > visited_cap {
                    settled_order.truncate(visited_cap);
                }

                let warnings = if skipped_invalid.is_empty() {
                    Vec::new()
                } else {
                    vec![TraverseWarning::SkippedInvalidWeights {
                        count: skipped_invalid.len() as u32,
                    }]
                };
                let depth_reached = settled_order.iter().map(|(_, _, d)| *d).max().unwrap_or(0);
                let start_idx = match &query.after {
                    None => 0,
                    Some(TraverseResume::Weighted { cost_bits, node_id }) => settled_order
                        .partition_point(|(id, b, _)| (*b, id) <= (*cost_bits, node_id)),
                    Some(_) => 0,
                };
                let remaining = &settled_order[start_idx..];
                let take_n = remaining.len().min(query.first as usize);
                let page_slice = &remaining[..take_n];
                let resume = if remaining.len() > take_n {
                    page_slice
                        .last()
                        .map(|(id, b, _)| TraverseResume::Weighted {
                            cost_bits: *b,
                            node_id: id.clone(),
                        })
                } else {
                    None
                };

                let nodes: Vec<Node> = page_slice
                    .iter()
                    .map(|(id, _, _)| eligible_nodes[id].clone())
                    .collect();
                let returned_depth: BTreeMap<NodeId, u32> = page_slice
                    .iter()
                    .map(|(id, _, d)| (id.clone(), *d))
                    .collect();
                let mut edges: Vec<Edge> = traversed_edges
                    .into_values()
                    .filter(|e| {
                        returned_depth.contains_key(&e.from) && returned_depth.contains_key(&e.to)
                    })
                    .collect();
                edges.sort_by(|a, b| {
                    returned_depth[&a.from]
                        .cmp(&returned_depth[&b.from])
                        .then_with(|| a.id.cmp(&b.id))
                });

                TraverseResult {
                    stats: TraverseStats {
                        visited: settled_order.len() as u32,
                        returned: nodes.len() as u32,
                        depth_reached,
                    },
                    nodes,
                    edges,
                    resume,
                    truncated,
                    warnings,
                    as_of: as_of_ts,
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct GeneratedNodeSpec {
    writer_idx: usize,
    scope_kind: u8,
    epoch_slot: usize,
}

#[derive(Debug, Clone)]
struct GeneratedEdgeSpec {
    from_idx: usize,
    to_idx: usize,
    edge_type_idx: usize,
    weight_kind: u8,
    epoch_slot: usize,
}

#[derive(Debug, Clone)]
struct GeneratedQuerySpec {
    start_idx: usize,
    caller_idx: usize,
    strategy: TraverseStrategy,
    direction: Direction,
    max_depth: u32,
    visited_cap: u32,
    first: u32,
    as_of_slot: usize,
    filter_calls_only: bool,
    narrow_global_and_group: bool,
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    // spec: ING-064, ING-065, ING-067, GQL-040, GQL-043
    #[test]
    fn sqlite_traverse_matches_reference_model_up_to_40_nodes(
        node_specs in prop::collection::vec(
            (0usize..3, 0u8..6, 0usize..2),
            1..=40
        ),
        edge_specs in prop::collection::vec(
            (0usize..40, 0usize..40, 0usize..2, 0u8..6, 0usize..2),
            0..=90
        ),
        queries in prop::collection::vec(
            (
                0usize..40,
                0usize..3,
                prop_oneof![
                    Just(TraverseStrategy::Bfs),
                    Just(TraverseStrategy::Dfs),
                    Just(TraverseStrategy::Shortest),
                    Just(TraverseStrategy::Weighted),
                ],
                prop_oneof![
                    Just(Direction::Outbound),
                    Just(Direction::Inbound),
                    Just(Direction::Both),
                ],
                1u32..=16,
                1u32..=45,
                1u32..=15,
                0usize..3,
                any::<bool>(),
                any::<bool>(),
            ),
            4..=8
        )
    ) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("prop_traverse.db");
        let manual = Arc::new(ManualClock::new(1_700_000_000_000));
        let clock: Arc<dyn Clock> = manual.clone();
        let mut cfg = SqliteConfig::new(&db_path);
        cfg.durability = Durability::Off;
        let store = SqliteContextStore::open(cfg, clock).expect("open");
        let tenant = TenantId::new("prop-tenant").expect("t").scope();

        let t1 = TaskId::from_unix_ms(1_700_000_000_000, [1u8; 10]);
        let t2 = TaskId::from_unix_ms(1_700_000_000_001, [2u8; 10]);

        let callers = [
            Caller::new("alice").with_groups(["g1"]).with_task_chain(vec![t2, t1]),
            Caller::new("bob").with_groups(["g2"]).with_task_chain(vec![t1]),
            Caller::new("carol"),
        ];
        // A super-caller used only to read back stored Node/Edge metadata when building the reference graph!
        let super_alice = Caller::new("alice").with_groups(["g1", "g2"]).with_task_chain(vec![t2, t1]);
        let super_bob = Caller::new("bob").with_groups(["g1", "g2"]).with_task_chain(vec![t2, t1]);
        let super_carol = Caller::new("carol").with_groups(["g1", "g2"]).with_task_chain(vec![t2, t1]);
        let supers = [super_alice, super_bob, super_carol];

        let generated_nodes: Vec<GeneratedNodeSpec> = node_specs
            .into_iter()
            .map(|(writer_idx, scope_kind, epoch_slot)| GeneratedNodeSpec {
                writer_idx,
                scope_kind,
                epoch_slot,
            })
            .collect();
        let n_count = generated_nodes.len();

        let generated_edges: Vec<GeneratedEdgeSpec> = edge_specs
            .into_iter()
            .map(|(f, t, et, wk, ep)| GeneratedEdgeSpec {
                from_idx: f % n_count,
                to_idx: t % n_count,
                edge_type_idx: et,
                weight_kind: wk,
                epoch_slot: ep,
            })
            .collect();

        let mut ref_graph = ReferenceGraph::default();
        let mut actual_ids: Vec<NodeId> = vec![NodeId::new("placeholder").expect("id"); n_count];
        let mut node_base_scopes: Vec<Scope> = vec![Scope::Global; n_count];
        let mut node_writers: Vec<usize> = vec![0; n_count];

        let mut epoch_timestamps = Vec::new();
        for slot in 0..2usize {
            let now_ts = manual.now_timestamp();
            epoch_timestamps.push(now_ts);
            let now_us = timestamp_to_micros(now_ts);

            for (idx, spec) in generated_nodes.iter().enumerate() {
                if spec.epoch_slot != slot {
                    continue;
                }
                let writer = &callers[spec.writer_idx];
                let (opt_id, scope_spec, base_scope) = match spec.scope_kind {
                    0 => (
                        Some(NodeId::new(format!("g:{idx:02}")).expect("id")),
                        ScopeSpec::Global,
                        Scope::Global,
                    ),
                    1 => (None, ScopeSpec::Private, Scope::Private),
                    2 => (
                        None,
                        ScopeSpec::Group { group: "g1".to_owned() },
                        Scope::Group,
                    ),
                    3 => (
                        None,
                        ScopeSpec::Task { task_id: t1 },
                        Scope::Task,
                    ),
                    4 => (
                        None,
                        ScopeSpec::ephemeral(
                            BaseScope::Global,
                            micros_to_timestamp(now_us + 20_000_000).expect("ts"),
                        ),
                        Scope::Global,
                    ),
                    _ => (
                        None,
                        ScopeSpec::ephemeral(
                            BaseScope::Group { group: "g2".to_owned() },
                            micros_to_timestamp(now_us + 5_000_000).expect("ts"),
                        ),
                        Scope::Group,
                    ),
                };

                let nid = store
                    .put_node(
                        &tenant,
                        writer,
                        NewNode {
                            id: opt_id,
                            node_type: NodeType::new("Symbol").expect("t"),
                            label: format!("node_{idx}"),
                            summary: None,
                            props: JsonMap::new(),
                            scope: scope_spec,
                            provenance: Provenance::new("prop"),
                        },
                    )
                    .expect("put prop node");
                let stored = store
                    .get_node(&tenant, &supers[spec.writer_idx], &nid, None)
                    .expect("get back")
                    .expect("visible to writer super");
                ref_graph.nodes.push(RefNodeVersion {
                    node: stored,
                    base_scope,
                });
                actual_ids[idx] = nid;
                node_base_scopes[idx] = base_scope;
                node_writers[idx] = spec.writer_idx;
            }

            for (e_idx, espec) in generated_edges.iter().enumerate() {
                if espec.epoch_slot != slot {
                    continue;
                }
                // Only insert edges whose endpoints have already been created by `slot`.
                if generated_nodes[espec.from_idx].epoch_slot > slot
                    || generated_nodes[espec.to_idx].epoch_slot > slot
                {
                    continue;
                }
                // Pick a writer who can see both endpoints (if private to different principals, skip).
                let from_id = &actual_ids[espec.from_idx];
                let to_id = &actual_ids[espec.to_idx];
                let Some(writer_i) = (0..3).find(|&wi| {
                    store.get_node(&tenant, &supers[wi], from_id, None).expect("ok").is_some()
                        && store.get_node(&tenant, &supers[wi], to_id, None).expect("ok").is_some()
                }) else {
                    continue;
                };

                let etype = if espec.edge_type_idx == 0 { "CALLS" } else { "IMPORTS" };
                let mut props = JsonMap::new();
                match espec.weight_kind {
                    0 => {} // absent => 1.0
                    1 => {
                        props.insert("weight".to_owned(), json!(0.0));
                    }
                    2 => {
                        props.insert("weight".to_owned(), json!(0.5));
                    }
                    3 => {
                        props.insert("weight".to_owned(), json!(2.25));
                    }
                    4 => {
                        props.insert("weight".to_owned(), json!(-1.0));
                    }
                    _ => {
                        props.insert("weight".to_owned(), json!(f64::NAN));
                    }
                }

                let res = store.put_edge(
                    &tenant,
                    &supers[writer_i],
                    NewEdge {
                        id: Some(EdgeId::new(format!("pe:{e_idx:03}")).expect("eid")),
                        edge_type: EdgeType::new(etype).expect("et"),
                        from: from_id.clone(),
                        to: to_id.clone(),
                        props,
                        confidence: Some(1.0),
                        resolution: EdgeResolution::Exact,
                        provenance: Provenance::new("prop"),
                    },
                );
                if let Ok(eid) = res
                    && let Some(stored_edge) = store
                        .get_edge(&tenant, &supers[writer_i], &eid, None)
                        .expect("get edge")
                {
                    // Update any prior version of this same quad in ref_graph if superseded!
                    for prev in &mut ref_graph.edges {
                        if prev.id == stored_edge.id && prev.valid_to.is_none() && prev.valid_from < stored_edge.valid_from {
                            prev.valid_to = Some(stored_edge.valid_from);
                        }
                    }
                    if !ref_graph.edges.iter().any(|x| x.id == stored_edge.id && x.valid_from == stored_edge.valid_from) {
                        ref_graph.edges.push(stored_edge);
                    }
                }
            }

            manual.advance(Duration::from_secs(10));
        }
        // Final timestamp `slot == 2` is after the 5s ephemeral has expired (`now = t0 + 20s` means 20s ephemerals also just expired at t0 + 20s!).
        // Advance 0 so t_final == t0 + 20s (expires_at <= now_us triggers!).
        let final_now = manual.now_timestamp();
        epoch_timestamps.push(final_now);
        let _ = (&node_base_scopes, &node_writers);

        for (
            start_raw,
            caller_idx,
            strategy,
            direction,
            max_depth,
            visited_cap,
            first,
            as_of_slot,
            filter_calls_only,
            narrow_global_and_group,
        ) in queries
        {
            let qspec = GeneratedQuerySpec {
                start_idx: start_raw % n_count,
                caller_idx,
                strategy,
                direction,
                max_depth,
                visited_cap,
                first,
                as_of_slot,
                filter_calls_only,
                narrow_global_and_group,
            };
            let caller = &callers[qspec.caller_idx];
            let q = TraverseQuery {
                from: actual_ids[qspec.start_idx].clone(),
                edge_types: if qspec.filter_calls_only {
                    Some(vec![EdgeType::new("CALLS").expect("et")])
                } else {
                    None
                },
                direction: qspec.direction,
                max_depth: qspec.max_depth,
                strategy: qspec.strategy,
                as_of: Some(epoch_timestamps[qspec.as_of_slot]),
                scopes: if qspec.narrow_global_and_group {
                    Some(vec![Scope::Global, Scope::Group])
                } else {
                    None
                },
                first: 1_000,
                visited_cap: Some(qspec.visited_cap),
                after: None,
            };

            let actual_full = store.traverse(&tenant, caller, &q).expect("sqlite traverse");
            let expected_full = ref_graph.traverse(caller, final_now, &q);
            prop_assert_eq!(&actual_full, &expected_full);

            // Also verify stateless pagination with `qspec.first` against both reference model and full result!
            let mut paged_nodes = Vec::new();
            let mut cursor = None;
            loop {
                let mut pq = q.clone();
                pq.first = qspec.first;
                pq.after = cursor;
                let page_actual = store.traverse(&tenant, caller, &pq).expect("sqlite page");
                let page_expected = ref_graph.traverse(caller, final_now, &pq);
                prop_assert_eq!(&page_actual, &page_expected);
                paged_nodes.extend(page_actual.nodes);
                match page_actual.resume {
                    Some(tok) => cursor = Some(tok),
                    None => break,
                }
            }
            prop_assert_eq!(paged_nodes, actual_full.nodes);
        }
    }
}

fn collect_pages_strict(
    store: &SqliteContextStore,
    tenant: &gap_core::ids::TenantScope,
    caller: &Caller,
    base_query: &TraverseQuery,
    first: u32,
    max_pages: usize,
) -> (Vec<String>, usize) {
    let mut concatenated = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut seen_pages: Vec<Vec<String>> = Vec::new();
    let mut cursor = None;
    let mut pages = 0usize;

    loop {
        pages += 1;
        assert!(
            pages <= max_pages,
            "paging exceeded {max_pages} pages (infinite loop on resume cursor {cursor:?})"
        );
        let mut pq = base_query.clone();
        pq.first = first;
        pq.after = cursor.clone();
        let page = store.traverse(tenant, caller, &pq).expect("page traverse");
        let ids: Vec<String> = page
            .nodes
            .iter()
            .map(|n| n.id.as_str().to_owned())
            .collect();
        assert!(
            !seen_pages.contains(&ids),
            "repeated page {ids:?} on page {pages}"
        );
        for id in &ids {
            assert!(
                seen_ids.insert(id.clone()),
                "duplicate node `{id}` across pages"
            );
        }
        seen_pages.push(ids.clone());
        concatenated.extend(ids);
        match page.resume {
            Some(tok) => cursor = Some(tok),
            None => break,
        }
    }
    (concatenated, pages)
}

// spec: ING-064, ING-067, GQL-040
#[test]
fn ing_067_weighted_repro_1_zero_weight_edge_sorts_by_cost_then_node_id_and_pages_cleanly() {
    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(dir.path().join("r2_repro1.db")), clock)
        .expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // Repro 1 (R-2): `a -> z` weight 1.0, `z -> m` weight 0.0, start `a`, Outbound, max_depth 2.
    let nodes: Vec<NewNode> = ["a", "m", "z"].into_iter().map(global_node).collect();
    let edges = vec![
        weighted_edge("e_az", "a", "z", json!(1.0)),
        weighted_edge("e_zm", "z", "m", json!(0.0)),
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &edges)
        .expect("seed repro 1");

    let mut q = TraverseQuery::new(NodeId::new("a").expect("id"));
    q.direction = Direction::Outbound;
    q.max_depth = 2;
    q.strategy = TraverseStrategy::Weighted;
    q.first = 100;

    let full = store.traverse(&tenant, &caller, &q).expect("unpaged");
    assert_eq!(node_ids(&full), vec!["a", "m", "z"]);

    let (paged_f1, count_f1) = collect_pages_strict(&store, &tenant, &caller, &q, 1, 10);
    assert_eq!(count_f1, 3);
    assert_eq!(paged_f1, vec!["a", "m", "z"]);

    let (paged_f2, count_f2) = collect_pages_strict(&store, &tenant, &caller, &q, 2, 10);
    assert_eq!(count_f2, 2);
    assert_eq!(paged_f2, vec!["a", "m", "z"]);
}

// spec: ING-064, ING-067, GQL-040
#[test]
fn ing_067_weighted_repro_2_direction_both_zero_weight_does_not_loop_on_resume() {
    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(SqliteConfig::new(dir.path().join("r2_repro2.db")), clock)
        .expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // Repro 2 (R-2): ids `n4` (start), `n11`, `n1`, `n0`, `s1`, Direction::Both.
    let nodes: Vec<NewNode> = ["n4", "n11", "n1", "n0", "s1"]
        .into_iter()
        .map(global_node)
        .collect();
    let edges = vec![
        weighted_edge("e1", "n11", "n4", json!(0.5)),
        plain_edge("e2", "CALLS", "n4", "n11"),
        weighted_edge("e3", "n1", "n4", json!(1.0)),
        weighted_edge("e4", "n11", "n1", json!(0.5)),
        weighted_edge("e5", "n0", "n1", json!(0.0)),
        plain_edge("e6", "CALLS", "s1", "n1"),
        weighted_edge("e7", "n11", "n0", json!(3.0)),
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &edges)
        .expect("seed repro 2");

    let mut q = TraverseQuery::new(NodeId::new("n4").expect("id"));
    q.direction = Direction::Both;
    q.max_depth = 16;
    q.strategy = TraverseStrategy::Weighted;
    q.first = 100;

    let full = store.traverse(&tenant, &caller, &q).expect("unpaged");
    assert_eq!(node_ids(&full), vec!["n4", "n11", "n0", "n1", "s1"]);

    let (paged_f1, count_f1) = collect_pages_strict(&store, &tenant, &caller, &q, 1, 10);
    assert_eq!(count_f1, 5);
    assert_eq!(paged_f1, vec!["n4", "n11", "n0", "n1", "s1"]);

    let (paged_f2, count_f2) = collect_pages_strict(&store, &tenant, &caller, &q, 2, 10);
    assert_eq!(count_f2, 3);
    assert_eq!(paged_f2, vec!["n4", "n11", "n0", "n1", "s1"]);
}

// spec: ING-064, ING-067, GQL-040
#[test]
fn ing_067_weighted_zero_weight_cycle_terminates_without_duplicates() {
    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(
        SqliteConfig::new(dir.path().join("r2_zero_cycle.db")),
        clock,
    )
    .expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // 4-node zero-weight cycle + zero-weight self-loop, starting from `d` (largest id) so every
    // reachable node has cost 0.0 and `(cost, id)` tie-breaking orders `[a, b, c, d]`.
    let nodes: Vec<NewNode> = ["a", "b", "c", "d"].into_iter().map(global_node).collect();
    let edges = vec![
        weighted_edge("e_dc", "d", "c", json!(0.0)),
        weighted_edge("e_cb", "c", "b", json!(0.0)),
        weighted_edge("e_ba", "b", "a", json!(0.0)),
        weighted_edge("e_ad", "a", "d", json!(0.0)),
        weighted_edge("e_dd", "d", "d", json!(0.0)),
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &edges)
        .expect("seed zero cycle");

    let mut q = TraverseQuery::new(NodeId::new("d").expect("id"));
    q.direction = Direction::Outbound;
    q.max_depth = 16;
    q.strategy = TraverseStrategy::Weighted;
    q.first = 100;

    let full = store.traverse(&tenant, &caller, &q).expect("unpaged");
    assert_eq!(node_ids(&full), vec!["a", "b", "c", "d"]);
    assert_eq!(full.stats.visited, 4);
    assert_eq!(full.stats.depth_reached, 3);
    assert!(!full.truncated);

    let (paged_f1, count_f1) = collect_pages_strict(&store, &tenant, &caller, &q, 1, 10);
    assert_eq!(count_f1, 4);
    assert_eq!(paged_f1, vec!["a", "b", "c", "d"]);

    let (paged_f2, count_f2) = collect_pages_strict(&store, &tenant, &caller, &q, 2, 10);
    assert_eq!(count_f2, 2);
    assert_eq!(paged_f2, vec!["a", "b", "c", "d"]);
}

// spec: ING-064, ING-067, GQL-040
#[test]
fn ing_067_weighted_equal_cost_group_straddling_visited_cap_keeps_smallest_ids_and_min_hop_depth() {
    let dir = TempDir::new().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
    let store =
        SqliteContextStore::open(SqliteConfig::new(dir.path().join("r2_cap_tie.db")), clock)
            .expect("open");
    let tenant = TenantId::new("acme").expect("t").scope();
    let caller = Caller::new("alice");

    // `n20` (cost 0.0, hops 0) -> `n0` (cost 0.5, hops 1)
    // `n20` -> `s3` (cost 1.0, hops 1) -> `s2` (cost 1.0, hops 2 via weight 0.0)
    // Also a longer equal-cost path `n20 -> s4 (1.0) -> s3_mid (0.0) -> s2 (0.0)` (3 hops) to verify
    // `s2`'s depth is the smallest hop count (2, not 3) among its minimum-cost paths.
    let nodes: Vec<NewNode> = ["n20", "n0", "s2", "s3", "s4", "z9"]
        .into_iter()
        .map(global_node)
        .collect();
    let edges = vec![
        weighted_edge("e_20_0", "n20", "n0", json!(0.5)),
        weighted_edge("e_20_s3", "n20", "s3", json!(1.0)),
        weighted_edge("e_s3_s2", "s3", "s2", json!(0.0)),
        weighted_edge("e_20_s4", "n20", "s4", json!(1.0)),
        weighted_edge("e_s4_z9", "s4", "z9", json!(0.0)),
        weighted_edge("e_z9_s2", "z9", "s2", json!(0.0)),
    ];
    store
        .put_batch(&tenant, &caller, &nodes, &edges)
        .expect("seed cap tie");

    let mut q = TraverseQuery::new(NodeId::new("n20").expect("id"));
    q.direction = Direction::Outbound;
    q.max_depth = 6;
    q.strategy = TraverseStrategy::Weighted;
    q.first = 100;
    q.visited_cap = Some(3);

    // Equal-cost group at cost 1.0 has `{s2, s3, s4, z9}`; first 3 overall by `(cost, id)` is
    // `[n20 (0.0), n0 (0.5), s2 (1.0)]` (NOT `[n20, n0, s3]`), with `depth_reached == 2` from `s2`.
    let capped = store.traverse(&tenant, &caller, &q).expect("capped=3");
    assert_eq!(node_ids(&capped), vec!["n20", "n0", "s2"]);
    assert!(capped.truncated);
    assert_eq!(
        capped.stats,
        TraverseStats {
            visited: 3,
            returned: 3,
            depth_reached: 2,
        }
    );

    // Paging the capped query with `first = 1` and `first = 2` returns the exact same 3-node prefix.
    let (paged_f1, count_f1) = collect_pages_strict(&store, &tenant, &caller, &q, 1, 10);
    assert_eq!(count_f1, 3);
    assert_eq!(paged_f1, vec!["n20", "n0", "s2"]);

    let (paged_f2, count_f2) = collect_pages_strict(&store, &tenant, &caller, &q, 2, 10);
    assert_eq!(count_f2, 2);
    assert_eq!(paged_f2, vec!["n20", "n0", "s2"]);

    // When `visited_cap` equals total reachable nodes (6), `truncated` must be `false`.
    q.visited_cap = Some(6);
    let exact_cap = store.traverse(&tenant, &caller, &q).expect("capped=6");
    assert_eq!(
        node_ids(&exact_cap),
        vec!["n20", "n0", "s2", "s3", "s4", "z9"]
    );
    assert!(!exact_cap.truncated);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 128,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    // spec: ING-064, ING-067, GQL-040
    #[test]
    fn ing_067_weighted_zero_and_positive_weights_sorted_by_cost_id_and_paging_matches(
        n_nodes in 2usize..=16,
        raw_edges in prop::collection::vec((0usize..16, 0usize..16, 0usize..4), 0..=45),
        start_raw in 0usize..16,
        direction in prop_oneof![
            Just(Direction::Outbound),
            Just(Direction::Inbound),
            Just(Direction::Both),
        ],
        max_depth in 1u32..=8,
        visited_cap in 1u32..=20,
        rand_first in 3u32..=7,
    ) {
        const WEIGHTS: [f64; 4] = [0.0, 0.5, 1.0, 2.0];

        let dir = TempDir::new().expect("tempdir");
        let mut cfg = SqliteConfig::new(dir.path().join("r2_weighted_prop.db"));
        cfg.durability = Durability::Off;
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(1_700_000_000_000));
        let store = SqliteContextStore::open(cfg, clock).expect("open");
        let tenant = TenantId::new("prop-weighted").expect("t").scope();
        let caller = Caller::new("alice");

        // Deliberately non-padded + varied prefixes so lexicographic order differs from index order.
        let ids: Vec<String> = (0..n_nodes)
            .map(|i| if i % 2 == 0 { format!("n{i}") } else { format!("s{i}") })
            .collect();
        let new_nodes: Vec<NewNode> = ids.iter().map(|s| global_node(s)).collect();

        let mut new_edges = Vec::new();
        let mut bellman_steps: Vec<(usize, usize, f64)> = Vec::new();
        // Deduplicate `(from, to)` so `put_batch` writes one canonical weight per directed pair.
        let mut seen_pairs: BTreeMap<(usize, usize), f64> = BTreeMap::new();
        for (f_raw, t_raw, w_idx) in raw_edges {
            let f = f_raw % n_nodes;
            let t = t_raw % n_nodes;
            seen_pairs.entry((f, t)).or_insert(WEIGHTS[w_idx]);
        }
        for (idx, ((f, t), w)) in seen_pairs.into_iter().enumerate() {
            new_edges.push(weighted_edge(&format!("we{idx}"), &ids[f], &ids[t], json!(w)));
            match direction {
                Direction::Outbound => bellman_steps.push((f, t, w)),
                Direction::Inbound => bellman_steps.push((t, f, w)),
                Direction::Both => {
                    bellman_steps.push((f, t, w));
                    bellman_steps.push((t, f, w));
                }
            }
        }
        store
            .put_batch(&tenant, &caller, &new_nodes, &new_edges)
            .expect("seed weighted prop");

        let start_idx = start_raw % n_nodes;
        // Independent Bellman-Ford hop-bounded DP (`0..=max_depth` relaxations).
        let depth_usize = max_depth as usize;
        let mut dp = vec![vec![f64::INFINITY; n_nodes]; depth_usize + 1];
        dp[0][start_idx] = 0.0;
        for h in 0..depth_usize {
            for &(u, v, w) in &bellman_steps {
                let c = dp[h][u];
                if c.is_finite() {
                    let cand = if c + w == 0.0 { 0.0 } else { c + w };
                    if cand < dp[h + 1][v] {
                        dp[h + 1][v] = cand;
                    }
                }
            }
        }
        let mut expected_all: Vec<(u64, String)> = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let best = (0..=depth_usize)
                .map(|h| dp[h][i])
                .fold(f64::INFINITY, f64::min);
            if best.is_finite() {
                let norm = if best == 0.0 { 0.0 } else { best };
                expected_all.push((norm.to_bits(), id.clone()));
            }
        }
        expected_all.sort();
        let expected_truncated = expected_all.len() > visited_cap as usize;
        expected_all.truncate(visited_cap as usize);
        let expected_ids: Vec<String> = expected_all.iter().map(|(_, id)| id.clone()).collect();

        let mut q = TraverseQuery::new(NodeId::new(&ids[start_idx]).expect("id"));
        q.direction = direction;
        q.max_depth = max_depth;
        q.strategy = TraverseStrategy::Weighted;
        q.first = 1_000;
        q.visited_cap = Some(visited_cap);

        let full = store.traverse(&tenant, &caller, &q).expect("unpaged");
        let actual_ids: Vec<String> = full.nodes.iter().map(|n| n.id.as_str().to_owned()).collect();

        // (a) Returned order equals the `(cost, id)`-sorted reachable prefix.
        prop_assert_eq!(&actual_ids, &expected_ids);
        prop_assert_eq!(full.truncated, expected_truncated);

        // (b) Paging concatenation with `first = 1`, `first = 2`, and `rand_first` equals unpaged.
        for page_size in [1u32, 2u32, rand_first] {
            let (paged_ids, page_count) =
                collect_pages_strict(&store, &tenant, &caller, &q, page_size, n_nodes + 2);
            let expected_pages = if actual_ids.is_empty() {
                1
            } else {
                actual_ids.len().div_ceil(page_size as usize)
            };
            prop_assert_eq!(page_count, expected_pages);
            prop_assert_eq!(&paged_ids, &actual_ids);
        }
    }
}
