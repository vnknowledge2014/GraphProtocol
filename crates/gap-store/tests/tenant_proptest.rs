//! Cross-tenant property tests (`proptest`, 3 tenants) proving that arbitrary interleaved write,
//! tombstone, delete, and task-lifecycle sequences across tenants `alpha`, `bravo`, and `charlie`
//! leave each tenant's observable state (`get_node`, `get_edge`, `counts`, `search_nodes`,
//! `traverse`) equal to an isolated per-tenant in-memory model (`INV-09`, `INV-19`, `ING-065`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use gap_core::clock::{Clock, ManualClock, Timestamp};
use gap_store::{
    Caller, ContextMaintenance, ContextStore, Direction, Durability, EdgeId, EdgeResolution,
    EdgeType, GraphCounts, NewEdge, NewNode, NodeId, NodeType, Provenance, ScopeSpec, SqliteConfig,
    SqliteContextStore, StoreError, TaskId, TenantId, TraverseQuery, TraverseStrategy,
};
use proptest::prelude::*;
use serde_json::Map as JsonMap;
use tempfile::TempDir;

const TENANT_NAMES: [&str; 3] = ["tenant-alpha", "tenant-bravo", "tenant-charlie"];
const SHARED_TOKENS: [&str; 4] = ["sharedzero", "sharedone", "sharedtwo", "sharedthree"];
const EXCLUSIVE_TOKENS: [&str; 3] = ["onlyalpha", "onlybravo", "onlycharlie"];

#[derive(Debug, Clone)]
enum FuzzOp {
    PutGlobal {
        slot: u8,
        shared_tok: u8,
        rev: u8,
        caller_idx: u8,
    },
    PutPrivate {
        shared_tok: u8,
        caller_idx: u8,
    },
    PutGroup {
        group_idx: u8,
        shared_tok: u8,
        caller_idx: u8,
    },
    PutTask {
        task_idx: u8,
        shared_tok: u8,
        caller_idx: u8,
    },
    PutEdge {
        from_slot: u8,
        to_slot: u8,
        caller_idx: u8,
    },
    TombstoneGlobal {
        slot: u8,
        caller_idx: u8,
    },
    DeleteGlobal {
        slot: u8,
        caller_idx: u8,
    },
    TombstoneTask {
        task_idx: u8,
    },
    AdvanceSecs(u8),
}

fn arb_step() -> impl Strategy<Value = (usize, FuzzOp)> {
    let op = prop_oneof![
        4 => (0u8..6, 0u8..4, 0u8..3, 0u8..2).prop_map(|(slot, shared_tok, rev, caller_idx)| {
            FuzzOp::PutGlobal { slot, shared_tok, rev, caller_idx }
        }),
        2 => (0u8..4, 0u8..2).prop_map(|(shared_tok, caller_idx)| {
            FuzzOp::PutPrivate { shared_tok, caller_idx }
        }),
        2 => (0u8..2, 0u8..4, 0u8..2).prop_map(|(group_idx, shared_tok, caller_idx)| {
            FuzzOp::PutGroup { group_idx, shared_tok, caller_idx }
        }),
        2 => (0u8..2, 0u8..4, 0u8..2).prop_map(|(task_idx, shared_tok, caller_idx)| {
            FuzzOp::PutTask { task_idx, shared_tok, caller_idx }
        }),
        4 => (0u8..6, 0u8..6, 0u8..2).prop_map(|(from_slot, to_slot, caller_idx)| {
            FuzzOp::PutEdge { from_slot, to_slot, caller_idx }
        }),
        2 => (0u8..6, 0u8..2).prop_map(|(slot, caller_idx)| {
            FuzzOp::TombstoneGlobal { slot, caller_idx }
        }),
        1 => (0u8..6, 0u8..2).prop_map(|(slot, caller_idx)| {
            FuzzOp::DeleteGlobal { slot, caller_idx }
        }),
        1 => (0u8..2).prop_map(|task_idx| FuzzOp::TombstoneTask { task_idx }),
        1 => (1u8..30).prop_map(FuzzOp::AdvanceSecs),
    ];
    (0usize..3, op)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelScope {
    Global,
    Private,
    Group(u8),
    Task(u8),
}

#[derive(Debug, Clone)]
struct ModelNode {
    id: String,
    label: String,
    summary: String,
    owner: String,
    scope: ModelScope,
    tokens: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct ModelEdge {
    id: String,
    from: String,
    to: String,
}

#[derive(Debug, Default)]
struct IsolatedTenantModel {
    nodes: BTreeMap<String, ModelNode>,
    tombstoned: BTreeMap<String, ModelNode>,
    edges: BTreeMap<(String, String), ModelEdge>,
}

impl IsolatedTenantModel {
    fn is_visible(&self, node: &ModelNode, caller_idx: u8) -> bool {
        let principal = if caller_idx == 0 { "alice" } else { "bob" };
        match node.scope {
            ModelScope::Global => true,
            ModelScope::Private => node.owner == principal,
            ModelScope::Group(g) => g == caller_idx,
            ModelScope::Task(t) => t == caller_idx,
        }
    }

    fn visible_node(&self, id: &str, caller_idx: u8) -> Option<&ModelNode> {
        let n = self.nodes.get(id)?;
        if self.is_visible(n, caller_idx) {
            Some(n)
        } else {
            None
        }
    }

    fn can_delete_node(&self, id: &str, caller_idx: u8) -> bool {
        self.nodes
            .get(id)
            .or_else(|| self.tombstoned.get(id))
            .is_some_and(|n| self.is_visible(n, caller_idx))
    }

    fn upsert_node(&mut self, node: ModelNode) {
        if node.scope == ModelScope::Global
            && let Some(existing) = self.nodes.get(&node.id)
            && existing.scope == ModelScope::Global
            && existing.label == node.label
            && existing.summary == node.summary
        {
            // Content-hash no-op (`ING-062`, `m2-01-design.md` §4): identical content creates no
            // new version and preserves the existing row verbatim.
            return;
        }
        self.tombstoned.remove(&node.id);
        self.nodes.insert(node.id.clone(), node);
    }

    fn upsert_edge(&mut self, edge: ModelEdge) {
        self.edges
            .insert((edge.from.clone(), edge.to.clone()), edge);
    }

    fn tombstone_node(&mut self, id: &str) {
        if let Some(removed) = self.nodes.remove(id) {
            self.edges.retain(|(f, t), _| f != id && t != id);
            self.tombstoned.insert(id.to_owned(), removed);
        }
    }

    fn hard_delete_node(&mut self, id: &str) {
        self.nodes.remove(id);
        self.tombstoned.remove(id);
        self.edges.retain(|(f, t), _| f != id && t != id);
    }

    fn tombstone_task(&mut self, task_idx: u8) {
        let doomed: Vec<String> = self
            .nodes
            .values()
            .filter(|n| n.scope == ModelScope::Task(task_idx))
            .map(|n| n.id.clone())
            .collect();
        for id in doomed {
            self.tombstone_node(&id);
        }
    }

    fn counts(&self, caller_idx: u8) -> GraphCounts {
        let nodes = self
            .nodes
            .values()
            .filter(|n| self.is_visible(n, caller_idx))
            .count() as u64;
        let edges = self
            .edges
            .values()
            .filter(|e| {
                self.visible_node(&e.from, caller_idx).is_some()
                    && self.visible_node(&e.to, caller_idx).is_some()
            })
            .count() as u64;
        GraphCounts { nodes, edges }
    }

    fn search_ids(&self, token: &str, caller_idx: u8) -> BTreeSet<String> {
        self.nodes
            .values()
            .filter(|n| self.is_visible(n, caller_idx) && n.tokens.contains(token))
            .map(|n| n.id.clone())
            .collect()
    }

    fn bfs_outbound(
        &self,
        start_id: &str,
        max_depth: u32,
        caller_idx: u8,
    ) -> (Vec<String>, Vec<String>) {
        if self.visible_node(start_id, caller_idx).is_none() {
            return (Vec::new(), Vec::new());
        }
        let mut depth_map: BTreeMap<String, u32> = BTreeMap::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        depth_map.insert(start_id.to_owned(), 0);
        queue.push_back((start_id.to_owned(), 0));

        while let Some((u, d)) = queue.pop_front() {
            if d >= max_depth {
                continue;
            }
            let mut next_ids: Vec<String> = self
                .edges
                .values()
                .filter(|e| e.from == u && self.visible_node(&e.to, caller_idx).is_some())
                .map(|e| e.to.clone())
                .collect();
            next_ids.sort();
            next_ids.dedup();
            for v in next_ids {
                if !depth_map.contains_key(&v) {
                    depth_map.insert(v.clone(), d + 1);
                    queue.push_back((v, d + 1));
                }
            }
        }

        let mut ordered_nodes: Vec<(u32, String)> =
            depth_map.iter().map(|(id, d)| (*d, id.clone())).collect();
        ordered_nodes.sort();
        let node_ids: Vec<String> = ordered_nodes.into_iter().map(|(_, id)| id).collect();
        let returned_set: BTreeSet<&str> = node_ids.iter().map(String::as_str).collect();

        // Outbound traversed edges: from_depth < max_depth, both endpoints in returned_set,
        // ordered by (depth_of_from, edge_id).
        let mut edge_items: Vec<(u32, String)> = Vec::new();
        for e in self.edges.values() {
            if returned_set.contains(e.from.as_str())
                && returned_set.contains(e.to.as_str())
                && let Some(&src_depth) = depth_map.get(&e.from)
                && src_depth < max_depth
            {
                edge_items.push((src_depth, e.id.clone()));
            }
        }
        edge_items.sort();
        let edge_ids = edge_items.into_iter().map(|(_, id)| id).collect();
        (node_ids, edge_ids)
    }
}

fn slot_id(slot: u8) -> NodeId {
    NodeId::new(format!("g{slot}")).expect("slot id")
}

fn build_caller(caller_idx: u8, tasks: &[TaskId; 2]) -> Caller {
    let mut c = Caller::new(if caller_idx == 0 { "alice" } else { "bob" });
    c.groups.insert(format!("g{caller_idx}"));
    c.task_chain.push(tasks[usize::from(caller_idx)]);
    c
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        max_shrink_iters: 128,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    // spec: INV-09, INV-19, ING-065
    #[test]
    fn inv_09_three_tenant_interleaved_sequences_match_isolated_models(
        steps in proptest::collection::vec(arb_step(), 1..=35)
    ) {
        let dir = TempDir::new().expect("tempdir");
        let mut cfg = SqliteConfig::new(dir.path().join("three_tenants.db"));
        cfg.durability = Durability::Off;
        cfg.reader_pool_size = NonZeroUsize::new(1).expect("nonzero");

        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let store = SqliteContextStore::open(cfg, clock_dyn).expect("open store");

        let scopes = [
            TenantId::new(TENANT_NAMES[0]).expect("t0").scope(),
            TenantId::new(TENANT_NAMES[1]).expect("t1").scope(),
            TenantId::new(TENANT_NAMES[2]).expect("t2").scope(),
        ];
        let tasks = [
            TaskId::from_unix_ms(1_700_000_000_000, [0x10; 10]),
            TaskId::from_unix_ms(1_700_000_000_001, [0x20; 10]),
        ];

        let mut models: [IsolatedTenantModel; 3] = Default::default();
        let mut all_known_node_ids: BTreeSet<NodeId> = (0..6).map(slot_id).collect();
        let mut all_known_edge_ids: BTreeSet<EdgeId> = BTreeSet::new();

        for (t_idx, op) in steps {
            let scope = &scopes[t_idx];
            let excl_tok = EXCLUSIVE_TOKENS[t_idx];

            match op {
                FuzzOp::PutGlobal { slot, shared_tok, rev, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let shared = SHARED_TOKENS[usize::from(shared_tok)];
                    let id = slot_id(slot);
                    let label = format!("Global s{slot} r{rev} {shared}");
                    let summary = format!("{excl_tok} {shared}");
                    let out_id = store
                        .put_node(
                            scope,
                            &caller,
                            NewNode {
                                id: Some(id),
                                node_type: NodeType::new("Item").expect("type"),
                                label: label.clone(),
                                summary: Some(summary.clone()),
                                props: JsonMap::new(),
                                scope: ScopeSpec::Global,
                                provenance: Provenance::new("proptest"),
                            },
                        )
                        .expect("put global");
                    let mut tokens = BTreeSet::new();
                    tokens.insert(shared.to_owned());
                    tokens.insert(excl_tok.to_owned());
                    models[t_idx].upsert_node(ModelNode {
                        id: out_id.as_str().to_owned(),
                        label,
                        summary,
                        owner: caller.principal,
                        scope: ModelScope::Global,
                        tokens,
                    });
                }
                FuzzOp::PutPrivate { shared_tok, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let shared = SHARED_TOKENS[usize::from(shared_tok)];
                    let label = format!("Priv {shared}");
                    let summary = format!("{excl_tok} {shared}");
                    let minted = store
                        .put_node(
                            scope,
                            &caller,
                            NewNode {
                                id: None,
                                node_type: NodeType::new("Secret").expect("type"),
                                label: label.clone(),
                                summary: Some(summary.clone()),
                                props: JsonMap::new(),
                                scope: ScopeSpec::Private,
                                provenance: Provenance::new("proptest"),
                            },
                        )
                        .expect("put private");
                    all_known_node_ids.insert(minted.clone());
                    let mut tokens = BTreeSet::new();
                    tokens.insert(shared.to_owned());
                    tokens.insert(excl_tok.to_owned());
                    models[t_idx].upsert_node(ModelNode {
                        id: minted.as_str().to_owned(),
                        label,
                        summary,
                        owner: caller.principal,
                        scope: ModelScope::Private,
                        tokens,
                    });
                }
                FuzzOp::PutGroup { group_idx, shared_tok, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let shared = SHARED_TOKENS[usize::from(shared_tok)];
                    let label = format!("Grp {group_idx} {shared}");
                    let summary = format!("{excl_tok} {shared}");
                    let minted = store
                        .put_node(
                            scope,
                            &caller,
                            NewNode {
                                id: None,
                                node_type: NodeType::new("Doc").expect("type"),
                                label: label.clone(),
                                summary: Some(summary.clone()),
                                props: JsonMap::new(),
                                scope: ScopeSpec::Group {
                                    group: format!("g{group_idx}"),
                                },
                                provenance: Provenance::new("proptest"),
                            },
                        )
                        .expect("put group");
                    all_known_node_ids.insert(minted.clone());
                    let mut tokens = BTreeSet::new();
                    tokens.insert(shared.to_owned());
                    tokens.insert(excl_tok.to_owned());
                    models[t_idx].upsert_node(ModelNode {
                        id: minted.as_str().to_owned(),
                        label,
                        summary,
                        owner: caller.principal,
                        scope: ModelScope::Group(group_idx),
                        tokens,
                    });
                }
                FuzzOp::PutTask { task_idx, shared_tok, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let shared = SHARED_TOKENS[usize::from(shared_tok)];
                    let label = format!("Tsk {task_idx} {shared}");
                    let summary = format!("{excl_tok} {shared}");
                    let minted = store
                        .put_node(
                            scope,
                            &caller,
                            NewNode {
                                id: None,
                                node_type: NodeType::new("Step").expect("type"),
                                label: label.clone(),
                                summary: Some(summary.clone()),
                                props: JsonMap::new(),
                                scope: ScopeSpec::Task {
                                    task_id: tasks[usize::from(task_idx)],
                                },
                                provenance: Provenance::new("proptest"),
                            },
                        )
                        .expect("put task");
                    all_known_node_ids.insert(minted.clone());
                    let mut tokens = BTreeSet::new();
                    tokens.insert(shared.to_owned());
                    tokens.insert(excl_tok.to_owned());
                    models[t_idx].upsert_node(ModelNode {
                        id: minted.as_str().to_owned(),
                        label,
                        summary,
                        owner: caller.principal,
                        scope: ModelScope::Task(task_idx),
                        tokens,
                    });
                }
                FuzzOp::PutEdge { from_slot, to_slot, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let from_id = slot_id(from_slot);
                    let to_id = slot_id(to_slot);
                    let model_has_both = models[t_idx].visible_node(from_id.as_str(), caller_idx).is_some()
                        && models[t_idx].visible_node(to_id.as_str(), caller_idx).is_some();
                    let res = store.put_edge(
                        scope,
                        &caller,
                        NewEdge {
                            id: None,
                            edge_type: EdgeType::new("LINKS").expect("etype"),
                            from: from_id.clone(),
                            to: to_id.clone(),
                            props: JsonMap::new(),
                            confidence: Some(1.0),
                            resolution: EdgeResolution::Exact,
                            provenance: Provenance::new("proptest"),
                        },
                    );
                    if model_has_both {
                        let eid = res.expect("put_edge when both global endpoints exist");
                        all_known_edge_ids.insert(eid.clone());
                        models[t_idx].upsert_edge(ModelEdge {
                            id: eid.as_str().to_owned(),
                            from: from_id.as_str().to_owned(),
                            to: to_id.as_str().to_owned(),
                        });
                    } else {
                        prop_assert!(res.is_err());
                    }
                }
                FuzzOp::TombstoneGlobal { slot, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let id = slot_id(slot);
                    let existed = models[t_idx].visible_node(id.as_str(), caller_idx).is_some();
                    let res = store.tombstone_node(scope, &caller, &id);
                    if existed {
                        prop_assert_eq!(res, Ok(()));
                        models[t_idx].tombstone_node(id.as_str());
                    } else {
                        prop_assert_eq!(res, Err(StoreError::NotFound));
                    }
                }
                FuzzOp::DeleteGlobal { slot, caller_idx } => {
                    let caller = build_caller(caller_idx, &tasks);
                    let id = slot_id(slot);
                    let deletable = models[t_idx].can_delete_node(id.as_str(), caller_idx);
                    let res = store.delete_node(scope, &caller, &id);
                    if deletable {
                        prop_assert_eq!(res, Ok(()));
                        models[t_idx].hard_delete_node(id.as_str());
                    } else {
                        prop_assert_eq!(res, Err(StoreError::NotFound));
                    }
                }
                FuzzOp::TombstoneTask { task_idx } => {
                    store
                        .tombstone_task_nodes(scope, tasks[usize::from(task_idx)])
                        .expect("tombstone_task_nodes");
                    models[t_idx].tombstone_task(task_idx);
                }
                FuzzOp::AdvanceSecs(secs) => {
                    clock.advance(Duration::from_secs(u64::from(secs)));
                }
            }
        }

        // Verify EVERY tenant against its isolated single-tenant model across all APIs.
        for t_idx in 0..3 {
            let scope = &scopes[t_idx];
            let model = &models[t_idx];

            for caller_idx in 0u8..2 {
                let caller = build_caller(caller_idx, &tasks);

                // 1. `counts`
                let actual_counts = store.counts(scope, &caller, None).expect("counts");
                prop_assert_eq!(actual_counts, model.counts(caller_idx));

                // 2. `get_node` across ALL known NodeIds (including UUIDv7s minted in other tenants)
                for nid in &all_known_node_ids {
                    let got = store.get_node(scope, &caller, nid, None).expect("get_node");
                    let expected = model.visible_node(nid.as_str(), caller_idx);
                    match (got, expected) {
                        (Some(actual), Some(exp)) => {
                            prop_assert_eq!(actual.id.as_str(), exp.id.as_str());
                            prop_assert_eq!(actual.label, exp.label.clone());
                            prop_assert_eq!(actual.summary, Some(exp.summary.clone()));
                            prop_assert_eq!(actual.owner_principal, exp.owner.clone());
                        }
                        (None, None) => {}
                        (actual, exp) => {
                            prop_assert!(
                                false,
                                "tenant {t_idx} caller {caller_idx} node {nid:?} mismatch: actual={actual:?}, exp={exp:?}"
                            );
                        }
                    }
                }

                // 3. `get_edge` across ALL known EdgeIds (including edges minted in other tenants)
                for eid in &all_known_edge_ids {
                    let got = store.get_edge(scope, &caller, eid, None).expect("get_edge");
                    let exp = model.edges.values().find(|e| {
                        e.id == eid.as_str()
                            && model.visible_node(&e.from, caller_idx).is_some()
                            && model.visible_node(&e.to, caller_idx).is_some()
                    });
                    prop_assert_eq!(got.is_some(), exp.is_some());
                }

                // 4. `search_nodes` on shared tokens matches `model`, and on OTHER tenants'
                // exclusive tokens MUST return empty!
                for shared in SHARED_TOKENS {
                    let hits = store
                        .search_nodes(scope, &caller, shared, 100, None)
                        .expect("search shared");
                    let hit_ids: BTreeSet<String> =
                        hits.into_iter().map(|n| n.id.as_str().to_owned()).collect();
                    prop_assert_eq!(hit_ids, model.search_ids(shared, caller_idx));
                }
                for (other_t, other_excl) in EXCLUSIVE_TOKENS.iter().enumerate() {
                    if other_t != t_idx {
                        let cross_hits = store
                            .search_nodes(scope, &caller, other_excl, 100, None)
                            .expect("search cross-tenant exclusive token");
                        prop_assert!(
                            cross_hits.is_empty(),
                            "tenant {t_idx} leaked exclusive token `{other_excl}` of tenant {other_t}"
                        );
                    }
                }

                // 5. `traverse` (BFS outbound from `g0`, max_depth = 5)
                let start = slot_id(0);
                let mut tq = TraverseQuery::new(start.clone());
                tq.max_depth = 5;
                tq.strategy = TraverseStrategy::Bfs;
                tq.direction = Direction::Outbound;
                let trav = store.traverse(scope, &caller, &tq).expect("traverse");
                let actual_trav_nodes: Vec<String> =
                    trav.nodes.iter().map(|n| n.id.as_str().to_owned()).collect();
                let actual_trav_edges: Vec<String> =
                    trav.edges.iter().map(|e| e.id.as_str().to_owned()).collect();
                let (exp_nodes, exp_edges) = model.bfs_outbound(start.as_str(), 5, caller_idx);
                prop_assert_eq!(actual_trav_nodes, exp_nodes);
                prop_assert_eq!(actual_trav_edges, exp_edges);
            }
        }
    }
}

// spec: ING-062, ING-065, ING-066, INV-09
#[test]
fn ing_066_delete_after_tombstone_purges_as_of_history_and_second_delete_is_not_found() {
    let dir = TempDir::new().expect("tempdir");
    let mut cfg = SqliteConfig::new(dir.path().join("r1_delete_after_tombstone.db"));
    cfg.durability = Durability::Off;
    cfg.reader_pool_size = NonZeroUsize::new(1).expect("nonzero");

    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(cfg, clock.clone()).expect("open");

    let alpha = TenantId::new("tenant-alpha").expect("t0").scope();
    let bravo = TenantId::new("tenant-bravo").expect("t1").scope();
    let alice = Caller::new("alice");

    // Deleting a never-written node id must return `NotFound`.
    let never_written = slot_id(5);
    assert_eq!(
        store.delete_node(&alpha, &alice, &never_written),
        Err(StoreError::NotFound)
    );

    // Exact minimal sequence from R-1: PutGlobal{slot 4}, TombstoneGlobal{slot 4}, DeleteGlobal{slot 4}.
    let g4 = slot_id(4);
    store
        .put_node(
            &alpha,
            &alice,
            NewNode {
                id: Some(g4.clone()),
                node_type: NodeType::new("Item").expect("type"),
                label: "Global s4 r0".to_owned(),
                summary: Some("history payload".to_owned()),
                props: JsonMap::new(),
                scope: ScopeSpec::Global,
                provenance: Provenance::new("r1"),
            },
        )
        .expect("put g4");

    let before_tombstone = Timestamp::from_unix_millis(1_700_000_005_000).expect("ts");
    clock.advance(Duration::from_secs(10));
    assert_eq!(store.tombstone_node(&alpha, &alice, &g4), Ok(()));

    // Before hard delete: current read is `None`, historical `as_of` read still sees `g4`.
    assert!(
        store
            .get_node(&alpha, &alice, &g4, None)
            .expect("get current")
            .is_none()
    );
    let historical = store
        .get_node(&alpha, &alice, &g4, Some(before_tombstone))
        .expect("get as_of before delete")
        .expect("historical version exists before delete_node");
    assert_eq!(historical.label, "Global s4 r0");

    // Another tenant deleting `g4` must fail with `NotFound` and leave `alpha`'s history intact.
    assert_eq!(
        store.delete_node(&bravo, &alice, &g4),
        Err(StoreError::NotFound)
    );
    assert!(
        store
            .get_node(&alpha, &alice, &g4, Some(before_tombstone))
            .expect("get as_of after cross-tenant delete")
            .is_some()
    );

    // Hard delete after tombstone purges historical versions and returns `Ok(())`.
    clock.advance(Duration::from_secs(10));
    assert_eq!(store.delete_node(&alpha, &alice, &g4), Ok(()));
    assert!(
        store
            .get_node(&alpha, &alice, &g4, Some(before_tombstone))
            .expect("get as_of after delete")
            .is_none()
    );

    // A second delete on the already-purged id must return `NotFound`.
    assert_eq!(
        store.delete_node(&alpha, &alice, &g4),
        Err(StoreError::NotFound)
    );
}

// spec: ING-062, ING-065, ING-066
#[test]
fn ing_065_delete_after_tombstone_enforces_caller_visibility_before_purging_history() {
    let dir = TempDir::new().expect("tempdir");
    let mut cfg = SqliteConfig::new(dir.path().join("r1_private_delete_after_tombstone.db"));
    cfg.durability = Durability::Off;
    cfg.reader_pool_size = NonZeroUsize::new(1).expect("nonzero");

    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let store = SqliteContextStore::open(cfg, clock.clone()).expect("open");

    let alpha = TenantId::new("tenant-alpha").expect("t0").scope();
    let alice = Caller::new("alice");
    let bob = Caller::new("bob");

    let secret_id = store
        .put_node(
            &alpha,
            &alice,
            NewNode {
                id: None,
                node_type: NodeType::new("Secret").expect("type"),
                label: "Alice private note".to_owned(),
                summary: None,
                props: JsonMap::new(),
                scope: ScopeSpec::Private,
                provenance: Provenance::new("r1"),
            },
        )
        .expect("put private");

    let before_tombstone = Timestamp::from_unix_millis(1_700_000_005_000).expect("ts");
    clock.advance(Duration::from_secs(10));
    assert_eq!(store.tombstone_node(&alpha, &alice, &secret_id), Ok(()));

    // Bob cannot see Alice's tombstoned node and cannot hard-delete its history.
    assert_eq!(
        store.delete_node(&alpha, &bob, &secret_id),
        Err(StoreError::NotFound)
    );
    assert!(
        store
            .get_node(&alpha, &alice, &secret_id, Some(before_tombstone))
            .expect("alice still sees historical private node")
            .is_some()
    );

    // Alice (owner) can hard-delete the tombstoned history; a second delete returns `NotFound`.
    assert_eq!(store.delete_node(&alpha, &alice, &secret_id), Ok(()));
    assert!(
        store
            .get_node(&alpha, &alice, &secret_id, Some(before_tombstone))
            .expect("alice history purged")
            .is_none()
    );
    assert_eq!(
        store.delete_node(&alpha, &alice, &secret_id),
        Err(StoreError::NotFound)
    );
}
