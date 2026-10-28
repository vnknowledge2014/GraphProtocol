//! Bounded graph traversal (`BFS`, `DFS`, `SHORTEST`, `WEIGHTED`), stateless resume pagination,
//! recursive CTE queries, and explicit stack/heap drivers (`ING-064`, `ING-065`, `ING-067`, `GQL-040`, `GQL-043`).

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};

use gap_core::ids::TenantScope;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value as JsonValue;

use crate::error::StoreError;
use crate::sqlite::crud::{
    caller_claims_json, is_node_row_visible, materialize_edge, materialize_node, read_edge_row,
    read_node_row,
};
use crate::sqlite::{SqliteContextStore, map_sqlite_err};
use crate::types::{
    Caller, Direction, Edge, EdgeId, Node, NodeId, Scope, TraverseQuery, TraverseResult,
    TraverseResume, TraverseStats, TraverseStrategy, TraverseWarning, normalize_timestamp,
    timestamp_to_micros, validate_caller, validate_traverse_query,
};

pub(crate) const SELECT_START_NODE_SQL: &str = "
    SELECT
        id, node_type, label, summary, props_json,
        scope, base_scope, owner_principal, task_id, group_name, expires_at,
        prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
        valid_from, valid_to
    FROM nodes INDEXED BY idx_nodes_temporal
    WHERE tenant = ?1
      AND id = ?2
      AND valid_from <= ?3
      AND (valid_to IS NULL OR ?3 < valid_to)
";

macro_rules! bfs_cte_anchor_sql {
    () => {
        "WITH RECURSIVE
            caller_groups(val) AS (SELECT value FROM json_each(?6)),
            caller_tasks(val) AS (SELECT value FROM json_each(?7)),
            allowed_scopes(val) AS (SELECT value FROM json_each(?9)),
            allowed_edge_types(val) AS (SELECT value FROM json_each(?11)),
            bfs(node_id, depth) AS (
                SELECT n0.id, 0
                FROM nodes n0 INDEXED BY idx_nodes_temporal
                WHERE n0.tenant = ?1
                  AND n0.id = ?2
                  AND n0.valid_from <= ?3
                  AND (n0.valid_to IS NULL OR ?3 < n0.valid_to)
                  AND (?8 = 0 OR n0.scope IN (SELECT val FROM allowed_scopes))
                  AND (
                      n0.scope != 'EPHEMERAL'
                      OR (n0.expires_at IS NOT NULL AND n0.expires_at > ?4 AND n0.expires_at > ?3)
                  )
                  AND (
                      n0.base_scope = 'GLOBAL'
                      OR (n0.base_scope = 'PRIVATE' AND n0.owner_principal = ?5)
                      OR (
                          n0.base_scope = 'GROUP'
                          AND n0.group_name IS NOT NULL
                          AND n0.group_name IN (SELECT val FROM caller_groups)
                      )
                      OR (
                          n0.base_scope = 'TASK'
                          AND n0.task_id IS NOT NULL
                          AND n0.task_id IN (SELECT val FROM caller_tasks)
                      )
                  )"
    };
}

macro_rules! bfs_cte_arm_sql {
    ($next_expr:expr, $edge_idx:expr, $edge_join_pred:expr) => {
        concat!(
            "
                UNION
                SELECT ",
            $next_expr,
            ", bfs.depth + 1
                FROM bfs
                CROSS JOIN edges e INDEXED BY ",
            $edge_idx,
            "
                  ON e.tenant = ?1
                 AND ",
            $edge_join_pred,
            "
                 AND e.valid_from <= ?3
                 AND (e.valid_to IS NULL OR ?3 < e.valid_to)
                 AND (?10 = 0 OR e.edge_type IN (SELECT val FROM allowed_edge_types))
                CROSS JOIN nodes nv INDEXED BY idx_nodes_temporal
                  ON nv.tenant = ?1
                 AND nv.id = ",
            $next_expr,
            "
                 AND nv.valid_from <= ?3
                 AND (nv.valid_to IS NULL OR ?3 < nv.valid_to)
                 AND (?8 = 0 OR nv.scope IN (SELECT val FROM allowed_scopes))
                 AND (
                     nv.scope != 'EPHEMERAL'
                     OR (nv.expires_at IS NOT NULL AND nv.expires_at > ?4 AND nv.expires_at > ?3)
                 )
                 AND (
                     nv.base_scope = 'GLOBAL'
                     OR (nv.base_scope = 'PRIVATE' AND nv.owner_principal = ?5)
                     OR (
                         nv.base_scope = 'GROUP'
                         AND nv.group_name IS NOT NULL
                         AND nv.group_name IN (SELECT val FROM caller_groups)
                     )
                     OR (
                         nv.base_scope = 'TASK'
                         AND nv.task_id IS NOT NULL
                         AND nv.task_id IN (SELECT val FROM caller_tasks)
                     )
                 )
                WHERE bfs.depth < ?12"
        )
    };
}

macro_rules! bfs_cte_footer_sql {
    () => {
        "
                ORDER BY 2 ASC, 1 ASC
                LIMIT ?13
            )
        SELECT node_id, MIN(depth) AS min_depth
        FROM bfs
        GROUP BY node_id
        ORDER BY min_depth ASC, node_id ASC
        LIMIT ?14"
    };
}

pub(crate) const CTE_OUTBOUND_SQL: &str = concat!(
    bfs_cte_anchor_sql!(),
    bfs_cte_arm_sql!("e.to_id", "idx_edges_from", "e.from_id = bfs.node_id"),
    bfs_cte_footer_sql!(),
);

pub(crate) const CTE_INBOUND_SQL: &str = concat!(
    bfs_cte_anchor_sql!(),
    bfs_cte_arm_sql!("e.from_id", "idx_edges_to", "e.to_id = bfs.node_id"),
    bfs_cte_footer_sql!(),
);

pub(crate) const CTE_BOTH_SQL: &str = concat!(
    bfs_cte_anchor_sql!(),
    bfs_cte_arm_sql!("e.to_id", "idx_edges_from", "e.from_id = bfs.node_id"),
    bfs_cte_arm_sql!(
        "e.from_id",
        "idx_edges_to",
        "e.to_id = bfs.node_id AND e.from_id != bfs.node_id"
    ),
    bfs_cte_footer_sql!(),
);

macro_rules! step_neighbours_with_ctes_sql {
    () => {
        "WITH
            caller_groups(val) AS (SELECT value FROM json_each(?6)),
            caller_tasks(val) AS (SELECT value FROM json_each(?7)),
            allowed_scopes(val) AS (SELECT value FROM json_each(?9)),
            allowed_edge_types(val) AS (SELECT value FROM json_each(?11))
        "
    };
}

macro_rules! step_neighbours_arm_sql {
    ($next_expr:expr, $edge_idx:expr, $edge_pred:expr) => {
        concat!(
            "SELECT
                e.id, e.edge_type, e.from_id, e.to_id, e.props_json,
                e.confidence, e.resolution,
                e.prov_source, e.prov_desc_hash, e.prov_confidence, e.prov_extracted_at, e.prov_agent,
                e.valid_from, e.valid_to,
                ",
            $next_expr,
            " AS neighbour_id
            FROM edges e INDEXED BY ",
            $edge_idx,
            "
            CROSS JOIN nodes nv INDEXED BY idx_nodes_temporal
              ON nv.tenant = ?1
             AND nv.id = ",
            $next_expr,
            "
             AND nv.valid_from <= ?3
             AND (nv.valid_to IS NULL OR ?3 < nv.valid_to)
             AND (?8 = 0 OR nv.scope IN (SELECT val FROM allowed_scopes))
             AND (
                 nv.scope != 'EPHEMERAL'
                 OR (nv.expires_at IS NOT NULL AND nv.expires_at > ?4 AND nv.expires_at > ?3)
             )
             AND (
                 nv.base_scope = 'GLOBAL'
                 OR (nv.base_scope = 'PRIVATE' AND nv.owner_principal = ?5)
                 OR (
                     nv.base_scope = 'GROUP'
                     AND nv.group_name IS NOT NULL
                     AND nv.group_name IN (SELECT val FROM caller_groups)
                 )
                 OR (
                     nv.base_scope = 'TASK'
                     AND nv.task_id IS NOT NULL
                     AND nv.task_id IN (SELECT val FROM caller_tasks)
                 )
             )
            WHERE e.tenant = ?1
              AND ",
            $edge_pred,
            "
              AND e.valid_from <= ?3
              AND (e.valid_to IS NULL OR ?3 < e.valid_to)
              AND (?10 = 0 OR e.edge_type IN (SELECT val FROM allowed_edge_types))"
        )
    };
}

pub(crate) const NEIGHBOURS_OUTBOUND_SQL: &str = concat!(
    step_neighbours_with_ctes_sql!(),
    step_neighbours_arm_sql!("e.to_id", "idx_edges_from", "e.from_id = ?2"),
    "
            ORDER BY 15 ASC, 1 ASC"
);

pub(crate) const NEIGHBOURS_INBOUND_SQL: &str = concat!(
    step_neighbours_with_ctes_sql!(),
    step_neighbours_arm_sql!("e.from_id", "idx_edges_to", "e.to_id = ?2"),
    "
            ORDER BY 15 ASC, 1 ASC"
);

pub(crate) const NEIGHBOURS_BOTH_SQL: &str = concat!(
    step_neighbours_with_ctes_sql!(),
    step_neighbours_arm_sql!("e.to_id", "idx_edges_from", "e.from_id = ?2"),
    "
            UNION ALL
            ",
    step_neighbours_arm_sql!(
        "e.from_id",
        "idx_edges_to",
        "e.to_id = ?2 AND e.from_id != ?2"
    ),
    "
            ORDER BY 15 ASC, 1 ASC"
);

struct TraverseEnv<'a> {
    conn: &'a Connection,
    tenant_str: &'a str,
    caller: &'a Caller,
    query: &'a TraverseQuery,
    visited_cap: u32,
    now_us: i64,
    as_of_us: i64,
    groups_json: String,
    tasks_json: String,
    has_scopes: i64,
    scopes_json: String,
    has_edge_types: i64,
    edge_types_json: String,
}

pub(crate) fn cte_sql_for_direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Outbound => CTE_OUTBOUND_SQL,
        Direction::Inbound => CTE_INBOUND_SQL,
        Direction::Both => CTE_BOTH_SQL,
    }
}

pub(crate) fn step_sql_for_direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Outbound => NEIGHBOURS_OUTBOUND_SQL,
        Direction::Inbound => NEIGHBOURS_INBOUND_SQL,
        Direction::Both => NEIGHBOURS_BOTH_SQL,
    }
}

fn inverse_direction(direction: Direction) -> Direction {
    match direction {
        Direction::Outbound => Direction::Inbound,
        Direction::Inbound => Direction::Outbound,
        Direction::Both => Direction::Both,
    }
}

fn query_eligible_neighbours(
    stmt: &mut rusqlite::Statement<'_>,
    env: &TraverseEnv<'_>,
    origin_id: &str,
) -> Result<Vec<(String, Edge)>, StoreError> {
    let rows = stmt
        .query_map(
            params![
                env.tenant_str,
                origin_id,
                env.as_of_us,
                env.now_us,
                env.caller.principal.as_str(),
                env.groups_json.as_str(),
                env.tasks_json.as_str(),
                env.has_scopes,
                env.scopes_json.as_str(),
                env.has_edge_types,
                env.edge_types_json.as_str(),
            ],
            |row| {
                let raw_edge = read_edge_row(row)?;
                let neighbour_id: String = row.get(14)?;
                Ok((neighbour_id, raw_edge))
            },
        )
        .map_err(map_sqlite_err)?;

    let mut out = Vec::new();
    for item in rows {
        let (neighbour_id, raw_edge) = item.map_err(map_sqlite_err)?;
        out.push((neighbour_id, materialize_edge(raw_edge)?));
    }
    out.sort_by(|(na, ea), (nb, eb)| na.cmp(nb).then_with(|| ea.id.cmp(&eb.id)));
    Ok(out)
}

fn materialize_page_nodes(
    conn: &Connection,
    tenant_str: &str,
    as_of_us: i64,
    page_ids: &[&str],
) -> Result<Vec<Node>, StoreError> {
    let mut stmt = conn
        .prepare_cached(SELECT_START_NODE_SQL)
        .map_err(map_sqlite_err)?;
    let mut nodes = Vec::with_capacity(page_ids.len());
    for &id in page_ids {
        let raw = stmt
            .query_row(params![tenant_str, id, as_of_us], read_node_row)
            .optional()
            .map_err(map_sqlite_err)?
            .ok_or(StoreError::Backend)?;
        nodes.push(materialize_node(raw)?);
    }
    Ok(nodes)
}

fn sort_returned_edges(
    edge_map: HashMap<EdgeId, Edge>,
    returned_depth: &HashMap<&str, u32>,
) -> Vec<Edge> {
    let mut edges: Vec<Edge> = edge_map
        .into_values()
        .filter(|e| {
            returned_depth.contains_key(e.from.as_str())
                && returned_depth.contains_key(e.to.as_str())
        })
        .collect();
    edges.sort_by(|a, b| {
        let da = returned_depth.get(a.from.as_str()).copied().unwrap_or(0);
        let db = returned_depth.get(b.from.as_str()).copied().unwrap_or(0);
        da.cmp(&db).then_with(|| a.id.cmp(&b.id))
    });
    edges
}

fn parse_edge_weight(edge: &Edge) -> Option<f64> {
    match edge.props.get("weight") {
        None => Some(1.0),
        Some(JsonValue::Number(num)) => {
            let w = num.as_f64()?;
            if w.is_finite() && w >= 0.0 {
                Some(if w == 0.0 { 0.0 } else { w })
            } else {
                None
            }
        }
        Some(_) => None,
    }
}

struct WalkOutput {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    resume: Option<TraverseResume>,
    truncated: bool,
    stats: TraverseStats,
    warnings: Vec<TraverseWarning>,
}

fn run_bfs_or_shortest(env: &TraverseEnv<'_>) -> Result<WalkOutput, StoreError> {
    let cte_sql = cte_sql_for_direction(env.query.direction);
    let cte_row_limit =
        (i64::from(env.visited_cap) + 1).saturating_mul(i64::from(env.query.max_depth) + 1);
    let visited_plus_one = i64::from(env.visited_cap) + 1;

    let mut cte_rows: Vec<(String, u32)> = {
        let mut stmt = env.conn.prepare_cached(cte_sql).map_err(map_sqlite_err)?;
        let iter = stmt
            .query_map(
                params![
                    env.tenant_str,
                    env.query.from.as_str(),
                    env.as_of_us,
                    env.now_us,
                    env.caller.principal.as_str(),
                    env.groups_json.as_str(),
                    env.tasks_json.as_str(),
                    env.has_scopes,
                    env.scopes_json.as_str(),
                    env.has_edge_types,
                    env.edge_types_json.as_str(),
                    i64::from(env.query.max_depth),
                    cte_row_limit,
                    visited_plus_one,
                ],
                |row| {
                    let id: String = row.get(0)?;
                    let depth_raw: i64 = row.get(1)?;
                    let depth = u32::try_from(depth_raw.max(0)).unwrap_or(0);
                    Ok((id, depth))
                },
            )
            .map_err(map_sqlite_err)?;
        let mut collected = Vec::new();
        for item in iter {
            collected.push(item.map_err(map_sqlite_err)?);
        }
        collected
    };

    let cap_usize = env.visited_cap as usize;
    let truncated = cte_rows.len() > cap_usize;
    let cutoff_depth = if truncated {
        cte_rows[cap_usize].1
    } else {
        env.query.max_depth
    };
    if truncated {
        cte_rows.truncate(cap_usize);
    }

    let visited_count = u32::try_from(cte_rows.len()).unwrap_or(u32::MAX);
    let depth_reached = cte_rows.iter().map(|(_, d)| *d).max().unwrap_or(0);

    let start_idx = match &env.query.after {
        None => 0,
        Some(
            TraverseResume::Bfs { depth, node_id } | TraverseResume::Shortest { depth, node_id },
        ) => cte_rows.partition_point(|(id, d)| (*d, id.as_str()) <= (*depth, node_id.as_str())),
        Some(_) => return Err(StoreError::Backend),
    };

    let remaining = &cte_rows[start_idx..];
    let take_n = remaining.len().min(env.query.first as usize);
    let page_slice = &remaining[..take_n];

    let resume = if remaining.len() > take_n {
        if let Some((last_id, last_depth)) = page_slice.last() {
            let nid = NodeId::new(last_id.clone())?;
            Some(match env.query.strategy {
                TraverseStrategy::Bfs => TraverseResume::Bfs {
                    depth: *last_depth,
                    node_id: nid,
                },
                TraverseStrategy::Shortest => TraverseResume::Shortest {
                    depth: *last_depth,
                    node_id: nid,
                },
                _ => return Err(StoreError::Backend),
            })
        } else {
            None
        }
    } else {
        None
    };

    let page_ids: Vec<&str> = page_slice.iter().map(|(id, _)| id.as_str()).collect();
    let nodes = materialize_page_nodes(env.conn, env.tenant_str, env.as_of_us, &page_ids)?;

    let returned_depth: HashMap<&str, u32> =
        page_slice.iter().map(|(id, d)| (id.as_str(), *d)).collect();

    let mut edge_map: HashMap<EdgeId, Edge> = HashMap::new();
    if !page_slice.is_empty() {
        match env.query.strategy {
            TraverseStrategy::Bfs => {
                let mut step_stmt = env
                    .conn
                    .prepare_cached(step_sql_for_direction(env.query.direction))
                    .map_err(map_sqlite_err)?;
                for (u_id, u_depth) in page_slice {
                    let was_expanded =
                        *u_depth < env.query.max_depth && (!truncated || *u_depth < cutoff_depth);
                    if !was_expanded {
                        continue;
                    }
                    let steps = query_eligible_neighbours(&mut step_stmt, env, u_id)?;
                    for (v_id, edge) in steps {
                        if returned_depth.contains_key(v_id.as_str()) {
                            edge_map.entry(edge.id.clone()).or_insert(edge);
                        }
                    }
                }
            }
            TraverseStrategy::Shortest => {
                let visited_depth_map: HashMap<&str, u32> =
                    cte_rows.iter().map(|(id, d)| (id.as_str(), *d)).collect();
                let mut pred_stmt = env
                    .conn
                    .prepare_cached(step_sql_for_direction(inverse_direction(
                        env.query.direction,
                    )))
                    .map_err(map_sqlite_err)?;
                for (v_id, v_depth) in page_slice {
                    if *v_depth == 0 {
                        continue;
                    }
                    let candidates = query_eligible_neighbours(&mut pred_stmt, env, v_id)?;
                    let chosen = candidates.into_iter().find(|(u_id, _)| {
                        visited_depth_map.get(u_id.as_str()) == Some(&(v_depth - 1))
                    });
                    if let Some((u_id, edge)) = chosen
                        && returned_depth.contains_key(u_id.as_str())
                    {
                        edge_map.entry(edge.id.clone()).or_insert(edge);
                    }
                }
            }
            _ => return Err(StoreError::Backend),
        }
    }

    let edges = sort_returned_edges(edge_map, &returned_depth);
    let stats = TraverseStats {
        visited: visited_count,
        returned: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        depth_reached,
    };
    Ok(WalkOutput {
        nodes,
        edges,
        resume,
        truncated,
        stats,
        warnings: Vec::new(),
    })
}

fn run_dfs(env: &TraverseEnv<'_>) -> Result<WalkOutput, StoreError> {
    let mut step_stmt = env
        .conn
        .prepare_cached(step_sql_for_direction(env.query.direction))
        .map_err(map_sqlite_err)?;

    let mut neighbour_cache: HashMap<String, Vec<(String, Edge)>> = HashMap::new();
    let mut emitted_nodes: Vec<(String, u32)> = Vec::new();
    let mut emitted_depth: HashMap<String, u32> = HashMap::new();
    let mut best_depth: HashMap<String, u32> = HashMap::new();
    let mut traversed_edges: HashMap<EdgeId, Edge> = HashMap::new();
    let mut truncated = false;

    let cap_usize = env.visited_cap as usize;
    let mut stack: Vec<(String, u32, Option<Edge>)> =
        vec![(env.query.from.as_str().to_owned(), 0, None)];

    while let Some((u, d, via_edge)) = stack.pop() {
        if !emitted_depth.contains_key(&u) {
            if emitted_nodes.len() == cap_usize {
                truncated = true;
                break;
            }
            emitted_depth.insert(u.clone(), d);
            best_depth.insert(u.clone(), d);
            emitted_nodes.push((u.clone(), d));
            if let Some(edge) = via_edge {
                traversed_edges.entry(edge.id.clone()).or_insert(edge);
            }
        } else {
            if let Some(edge) = via_edge {
                traversed_edges.entry(edge.id.clone()).or_insert(edge);
            }
            let prev_best = best_depth.get(&u).copied().unwrap_or(0);
            if d >= prev_best {
                continue;
            }
            best_depth.insert(u.clone(), d);
        }

        if d < env.query.max_depth {
            if !neighbour_cache.contains_key(&u) {
                let loaded = query_eligible_neighbours(&mut step_stmt, env, &u)?;
                neighbour_cache.insert(u.clone(), loaded);
            }
            let steps = neighbour_cache.get(&u).ok_or(StoreError::Backend)?;
            for (v, edge) in steps.iter().rev() {
                stack.push((v.clone(), d + 1, Some(edge.clone())));
            }
        }
    }

    let visited_count = u32::try_from(emitted_nodes.len()).unwrap_or(u32::MAX);
    let depth_reached = emitted_nodes.iter().map(|(_, d)| *d).max().unwrap_or(0);

    let start_idx = match &env.query.after {
        None => 0,
        Some(TraverseResume::Dfs { ordinal, node_id }) => {
            if let Some(pos) = emitted_nodes
                .iter()
                .position(|(id, _)| id == node_id.as_str())
            {
                pos + 1
            } else {
                (*ordinal as usize)
                    .saturating_add(1)
                    .min(emitted_nodes.len())
            }
        }
        Some(_) => return Err(StoreError::Backend),
    };

    let remaining = &emitted_nodes[start_idx..];
    let take_n = remaining.len().min(env.query.first as usize);
    let page_slice = &remaining[..take_n];

    let resume = if remaining.len() > take_n && take_n > 0 {
        let last_idx = start_idx + take_n - 1;
        let last_ordinal = u32::try_from(last_idx).unwrap_or(u32::MAX);
        Some(TraverseResume::Dfs {
            ordinal: last_ordinal,
            node_id: NodeId::new(emitted_nodes[last_idx].0.clone())?,
        })
    } else {
        None
    };

    let page_ids: Vec<&str> = page_slice.iter().map(|(id, _)| id.as_str()).collect();
    let nodes = materialize_page_nodes(env.conn, env.tenant_str, env.as_of_us, &page_ids)?;
    let returned_depth: HashMap<&str, u32> =
        page_slice.iter().map(|(id, d)| (id.as_str(), *d)).collect();
    let edges = sort_returned_edges(traversed_edges, &returned_depth);
    let stats = TraverseStats {
        visited: visited_count,
        returned: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        depth_reached,
    };
    Ok(WalkOutput {
        nodes,
        edges,
        resume,
        truncated,
        stats,
        warnings: Vec::new(),
    })
}

fn run_weighted(env: &TraverseEnv<'_>) -> Result<WalkOutput, StoreError> {
    let mut step_stmt = env
        .conn
        .prepare_cached(step_sql_for_direction(env.query.direction))
        .map_err(map_sqlite_err)?;

    let mut valid_step_cache: HashMap<String, Vec<(String, Edge, f64)>> = HashMap::new();
    let mut skipped_invalid_edges: BTreeSet<EdgeId> = BTreeSet::new();
    let mut settled_nodes: Vec<(String, u64, u32)> = Vec::new();
    let mut settled_depth: HashMap<String, u32> = HashMap::new();
    let mut best_cost_by_hops: HashMap<String, [f64; 17]> = HashMap::new();
    let mut traversed_edges: HashMap<EdgeId, Edge> = HashMap::new();
    let mut cap_cost_bits: Option<u64> = None;
    let mut more_beyond_cap = false;

    let cap_usize = env.visited_cap as usize;
    let start_id = env.query.from.as_str().to_owned();
    let mut init_hops = [f64::INFINITY; 17];
    init_hops[0] = 0.0;
    best_cost_by_hops.insert(start_id.clone(), init_hops);

    // Order heap by `(cost_bits, hops, node_id)` (`ING-067`, `R-2`):
    // 1. For finite non-negative `f64` (`-0.0` normalized to `+0.0`), `u64` bit order equals `f64` order.
    // 2. Ordering `hops` before `node_id` guarantees the first pop of `u` achieves the smallest
    //    hop count among all minimum-cost paths of length `<= max_depth`.
    // 3. Do not cut immediately at `cap_usize`: zero-weight edges (`weight == 0.0`) can settle
    //    additional nodes at the exact same `cap_cost_bits` with smaller `node_id`s. Finish the
    //    full equal-cost group, sort settled nodes by `(cost_bits, node_id)`, then take `cap_usize`.
    let mut heap: BinaryHeap<Reverse<(u64, u8, String)>> = BinaryHeap::new();
    heap.push(Reverse((0.0_f64.to_bits(), 0, start_id)));

    while let Some(Reverse((cost_bits, hops, u))) = heap.pop() {
        let cost = f64::from_bits(cost_bits);
        let h = usize::from(hops);
        if let Some(u_table) = best_cost_by_hops.get(&u)
            && (cost > u_table[h] || (0..h).any(|hp| u_table[hp] <= cost))
        {
            continue;
        }

        let in_post_cap_probe = cap_cost_bits.is_some_and(|limit_bits| cost_bits > limit_bits);
        if in_post_cap_probe {
            if settled_nodes.len() > cap_usize || !settled_depth.contains_key(&u) {
                more_beyond_cap = true;
                break;
            }
            if u32::from(hops) < env.query.max_depth {
                let raw_steps = query_eligible_neighbours(&mut step_stmt, env, &u)?;
                for (v, edge) in raw_steps {
                    let Some(weight) = parse_edge_weight(&edge) else {
                        continue;
                    };
                    let raw_next = cost + weight;
                    if !raw_next.is_finite() {
                        continue;
                    }
                    let next_cost = if raw_next == 0.0 { 0.0 } else { raw_next };
                    let nh = h + 1;
                    let v_table = best_cost_by_hops
                        .entry(v.clone())
                        .or_insert([f64::INFINITY; 17]);
                    if (0..=nh).all(|hp| next_cost < v_table[hp]) {
                        v_table[nh] = next_cost;
                        heap.push(Reverse((next_cost.to_bits(), hops + 1, v)));
                    }
                }
            }
            continue;
        }

        if !settled_depth.contains_key(&u) {
            let d = u32::from(hops);
            settled_depth.insert(u.clone(), d);
            settled_nodes.push((u.clone(), cost_bits, d));
            if settled_nodes.len() == cap_usize {
                cap_cost_bits = Some(cost_bits);
            }
        }

        if u32::from(hops) < env.query.max_depth {
            if !valid_step_cache.contains_key(&u) {
                let raw_steps = query_eligible_neighbours(&mut step_stmt, env, &u)?;
                let mut valid = Vec::with_capacity(raw_steps.len());
                for (v, edge) in raw_steps {
                    if let Some(w) = parse_edge_weight(&edge) {
                        valid.push((v, edge, w));
                    } else {
                        skipped_invalid_edges.insert(edge.id);
                    }
                }
                valid_step_cache.insert(u.clone(), valid);
            }
            let steps = valid_step_cache.get(&u).ok_or(StoreError::Backend)?;
            for (v, edge, weight) in steps {
                traversed_edges
                    .entry(edge.id.clone())
                    .or_insert_with(|| edge.clone());
                let raw_next = cost + *weight;
                if !raw_next.is_finite() {
                    continue;
                }
                let next_cost = if raw_next == 0.0 { 0.0 } else { raw_next };
                let nh = h + 1;
                let v_table = best_cost_by_hops
                    .entry(v.clone())
                    .or_insert([f64::INFINITY; 17]);
                if (0..=nh).all(|hp| next_cost < v_table[hp]) {
                    v_table[nh] = next_cost;
                    heap.push(Reverse((next_cost.to_bits(), hops + 1, v.clone())));
                }
            }
        }
    }

    settled_nodes.sort_by(|a, b| (a.1, a.0.as_str()).cmp(&(b.1, b.0.as_str())));
    let truncated = settled_nodes.len() > cap_usize || more_beyond_cap;
    if settled_nodes.len() > cap_usize {
        settled_nodes.truncate(cap_usize);
    }

    let warnings = if skipped_invalid_edges.is_empty() {
        Vec::new()
    } else {
        vec![TraverseWarning::SkippedInvalidWeights {
            count: u32::try_from(skipped_invalid_edges.len()).unwrap_or(u32::MAX),
        }]
    };

    let visited_count = u32::try_from(settled_nodes.len()).unwrap_or(u32::MAX);
    let depth_reached = settled_nodes.iter().map(|(_, _, d)| *d).max().unwrap_or(0);

    let start_idx = match &env.query.after {
        None => 0,
        Some(TraverseResume::Weighted { cost_bits, node_id }) => {
            settled_nodes.partition_point(|(id, bits, _)| {
                (*bits, id.as_str()) <= (*cost_bits, node_id.as_str())
            })
        }
        Some(_) => return Err(StoreError::Backend),
    };

    let remaining = &settled_nodes[start_idx..];
    let take_n = remaining.len().min(env.query.first as usize);
    let page_slice = &remaining[..take_n];

    let resume = if remaining.len() > take_n {
        if let Some((last_id, last_bits, _)) = page_slice.last() {
            Some(TraverseResume::Weighted {
                cost_bits: *last_bits,
                node_id: NodeId::new(last_id.clone())?,
            })
        } else {
            None
        }
    } else {
        None
    };

    let page_ids: Vec<&str> = page_slice.iter().map(|(id, _, _)| id.as_str()).collect();
    let nodes = materialize_page_nodes(env.conn, env.tenant_str, env.as_of_us, &page_ids)?;
    let returned_depth: HashMap<&str, u32> = page_slice
        .iter()
        .map(|(id, _, d)| (id.as_str(), *d))
        .collect();
    let edges = sort_returned_edges(traversed_edges, &returned_depth);
    let stats = TraverseStats {
        visited: visited_count,
        returned: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        depth_reached,
    };
    Ok(WalkOutput {
        nodes,
        edges,
        resume,
        truncated,
        stats,
        warnings,
    })
}

impl SqliteContextStore {
    pub(crate) fn exec_traverse(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &TraverseQuery,
    ) -> Result<TraverseResult, StoreError> {
        validate_caller(caller)?;
        let visited_cap = validate_traverse_query(query)?;
        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);
        let as_of_ts = match query.as_of {
            Some(ts) => normalize_timestamp(ts)?,
            None => now_ts,
        };
        let as_of_us = timestamp_to_micros(as_of_ts);

        let guard = self.readers.checkout();
        let conn = guard.conn()?;
        let tenant_str = tenant.tenant().as_str();

        // Check start node existence, visibility, and scope narrowing before running the walk.
        let start_row_opt = {
            let mut stmt = conn
                .prepare_cached(SELECT_START_NODE_SQL)
                .map_err(map_sqlite_err)?;
            stmt.query_row(
                params![tenant_str, query.from.as_str(), as_of_us],
                read_node_row,
            )
            .optional()
            .map_err(map_sqlite_err)?
        };
        let Some(start_row) = start_row_opt else {
            return Ok(TraverseResult {
                nodes: Vec::new(),
                edges: Vec::new(),
                resume: None,
                truncated: false,
                stats: TraverseStats::default(),
                warnings: Vec::new(),
                as_of: as_of_ts,
            });
        };
        let scope_narrow_ok = query
            .scopes
            .as_ref()
            .is_none_or(|allowed: &Vec<Scope>| allowed.contains(&start_row.scope));
        if !scope_narrow_ok || !is_node_row_visible(&start_row, caller, now_us, as_of_us) {
            return Ok(TraverseResult {
                nodes: Vec::new(),
                edges: Vec::new(),
                resume: None,
                truncated: false,
                stats: TraverseStats::default(),
                warnings: Vec::new(),
                as_of: as_of_ts,
            });
        }

        let (groups_json, tasks_json) = caller_claims_json(caller)?;
        let (has_scopes, scopes_json) = match &query.scopes {
            Some(list) => {
                let names: Vec<&str> = list.iter().map(|s| s.as_str()).collect();
                (
                    1_i64,
                    serde_json::to_string(&names).map_err(|_| StoreError::Backend)?,
                )
            }
            None => (0_i64, "[]".to_owned()),
        };
        let (has_edge_types, edge_types_json) = match &query.edge_types {
            Some(list) => {
                let names: Vec<&str> = list.iter().map(|et| et.as_str()).collect();
                (
                    1_i64,
                    serde_json::to_string(&names).map_err(|_| StoreError::Backend)?,
                )
            }
            None => (0_i64, "[]".to_owned()),
        };

        let env = TraverseEnv {
            conn,
            tenant_str,
            caller,
            query,
            visited_cap,
            now_us,
            as_of_us,
            groups_json,
            tasks_json,
            has_scopes,
            scopes_json,
            has_edge_types,
            edge_types_json,
        };

        let out = match query.strategy {
            TraverseStrategy::Bfs | TraverseStrategy::Shortest => run_bfs_or_shortest(&env)?,
            TraverseStrategy::Dfs => run_dfs(&env)?,
            TraverseStrategy::Weighted => run_weighted(&env)?,
        };
        Ok(TraverseResult {
            nodes: out.nodes,
            edges: out.edges,
            resume: out.resume,
            truncated: out.truncated,
            stats: out.stats,
            warnings: out.warnings,
            as_of: as_of_ts,
        })
    }
}
