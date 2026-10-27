//! Node and edge CRUD, canonical `blake3` content hashing, temporal versions, atomic `put_batch`,
//! and per-tenant quota accounting (`ADR-020`, `ADR-025`, `INV-09`, `INV-19`, `ING-062`, `ING-064`..`ING-066`).

use std::collections::HashMap;

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::error::StoreError;
use crate::sqlite::{SqliteContextStore, lock_unpoisoned, map_sqlite_err};
use crate::types::{
    BatchResult, Caller, Edge, EdgeId, EdgeResolution, EdgeType, GraphCounts, MAX_BATCH_ITEMS,
    NewEdge, NewNode, Node, NodeId, NodeType, Provenance, ResolvedScope, Scope,
    micros_to_timestamp, normalize_timestamp, timestamp_to_micros, validate_caller,
    validate_new_edge, validate_new_node,
};

#[derive(Debug, Clone)]
pub(crate) struct RawNodeRow {
    pub id: String,
    pub node_type: String,
    pub label: String,
    pub summary: Option<String>,
    pub props_json: String,
    pub scope: Scope,
    pub base_scope: Scope,
    pub owner_principal: String,
    pub task_id: Option<TaskId>,
    pub group_name: Option<String>,
    pub expires_at_us: Option<i64>,
    pub prov_source: String,
    pub prov_desc_hash: Option<String>,
    pub prov_confidence: Option<f32>,
    pub prov_extracted_at_us: Option<i64>,
    pub prov_agent: Option<String>,
    pub valid_from_us: i64,
    pub valid_to_us: Option<i64>,
}

#[derive(Debug)]
struct CurrentNodeMeta {
    fts_rowid: i64,
    scope: Scope,
    base_scope: Scope,
    owner_principal: String,
    task_id: Option<TaskId>,
    group_name: Option<String>,
    expires_at_us: Option<i64>,
    valid_from_us: i64,
    content_hash: Vec<u8>,
}

#[derive(Debug)]
struct CurrentEdgeMeta {
    edge_rowid: i64,
    id: String,
    valid_from_us: i64,
    content_hash: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct RawEdgeRow {
    pub id: String,
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub props_json: String,
    pub confidence: Option<f32>,
    pub resolution: EdgeResolution,
    pub prov_source: String,
    pub prov_desc_hash: Option<String>,
    pub prov_confidence: Option<f32>,
    pub prov_extracted_at_us: Option<i64>,
    pub prov_agent: Option<String>,
    pub valid_from_us: i64,
    pub valid_to_us: Option<i64>,
}

pub(crate) fn caller_claims_json(caller: &Caller) -> Result<(String, String), StoreError> {
    let groups: Vec<&str> = caller.groups.iter().map(String::as_str).collect();
    let tasks: Vec<String> = caller.task_chain.iter().map(ToString::to_string).collect();
    let groups_json = serde_json::to_string(&groups).map_err(|_| StoreError::Backend)?;
    let tasks_json = serde_json::to_string(&tasks).map_err(|_| StoreError::Backend)?;
    Ok((groups_json, tasks_json))
}

/// Evaluates §5 visibility in Rust identically to the SQL visibility predicate (`ING-065`, `ING-066`).
#[must_use]
pub(crate) fn is_node_row_visible(
    row: &RawNodeRow,
    caller: &Caller,
    now_us: i64,
    as_of_us: i64,
) -> bool {
    is_scope_fields_visible(
        row.scope,
        row.base_scope,
        &row.owner_principal,
        row.task_id,
        row.group_name.as_deref(),
        row.expires_at_us,
        caller,
        now_us,
        as_of_us,
    )
}

#[allow(clippy::too_many_arguments)]
#[must_use]
fn is_scope_fields_visible(
    scope: Scope,
    base_scope: Scope,
    owner_principal: &str,
    task_id: Option<TaskId>,
    group_name: Option<&str>,
    expires_at_us: Option<i64>,
    caller: &Caller,
    now_us: i64,
    as_of_us: i64,
) -> bool {
    if scope == Scope::Ephemeral {
        let Some(exp_us) = expires_at_us else {
            return false;
        };
        if exp_us <= now_us || exp_us <= as_of_us {
            return false;
        }
    }
    is_base_scope_visible(base_scope, owner_principal, task_id, group_name, caller)
}

#[must_use]
pub(crate) fn is_base_scope_visible(
    base_scope: Scope,
    owner_principal: &str,
    task_id: Option<TaskId>,
    group_name: Option<&str>,
    caller: &Caller,
) -> bool {
    match base_scope {
        Scope::Global => true,
        Scope::Private => owner_principal == caller.principal,
        Scope::Group => group_name.is_some_and(|g| caller.groups.contains(g)),
        Scope::Task => task_id.is_some_and(|tid| caller.task_chain.contains(&tid)),
        Scope::Ephemeral => false,
    }
}

pub(crate) fn read_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawNodeRow> {
    let scope_raw: String = row.get(5)?;
    let base_scope_raw: String = row.get(6)?;
    let task_raw: Option<String> = row.get(8)?;
    let scope = Scope::parse_db(&scope_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let base_scope = Scope::parse_db(&base_scope_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let task_id = match task_raw {
        Some(s) => Some(TaskId::parse(&s).map_err(|_| rusqlite::Error::InvalidQuery)?),
        None => None,
    };
    let prov_conf: Option<f64> = row.get(13)?;
    Ok(RawNodeRow {
        id: row.get(0)?,
        node_type: row.get(1)?,
        label: row.get(2)?,
        summary: row.get(3)?,
        props_json: row.get(4)?,
        scope,
        base_scope,
        owner_principal: row.get(7)?,
        task_id,
        group_name: row.get(9)?,
        expires_at_us: row.get(10)?,
        prov_source: row.get(11)?,
        prov_desc_hash: row.get(12)?,
        prov_confidence: prov_conf.map(|v| v as f32),
        prov_extracted_at_us: row.get(14)?,
        prov_agent: row.get(15)?,
        valid_from_us: row.get(16)?,
        valid_to_us: row.get(17)?,
    })
}

fn read_current_node_meta(row: &rusqlite::Row<'_>) -> rusqlite::Result<CurrentNodeMeta> {
    let scope_raw: String = row.get(1)?;
    let base_scope_raw: String = row.get(2)?;
    let task_raw: Option<String> = row.get(4)?;
    let scope = Scope::parse_db(&scope_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let base_scope = Scope::parse_db(&base_scope_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let task_id = match task_raw {
        Some(s) => Some(TaskId::parse(&s).map_err(|_| rusqlite::Error::InvalidQuery)?),
        None => None,
    };
    Ok(CurrentNodeMeta {
        fts_rowid: row.get(0)?,
        scope,
        base_scope,
        owner_principal: row.get(3)?,
        task_id,
        group_name: row.get(5)?,
        expires_at_us: row.get(6)?,
        valid_from_us: row.get(7)?,
        content_hash: row.get(8)?,
    })
}

pub(crate) fn materialize_node(raw: RawNodeRow) -> Result<Node, StoreError> {
    let props: JsonMap<String, JsonValue> =
        serde_json::from_str(&raw.props_json).map_err(|_| StoreError::Corrupt)?;
    let expires_at = match raw.expires_at_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    let extracted_at = match raw.prov_extracted_at_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    let valid_from = micros_to_timestamp(raw.valid_from_us).map_err(|_| StoreError::Corrupt)?;
    let valid_to = match raw.valid_to_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    Ok(Node {
        id: NodeId::new(raw.id).map_err(|_| StoreError::Corrupt)?,
        node_type: NodeType::new(raw.node_type).map_err(|_| StoreError::Corrupt)?,
        label: raw.label,
        summary: raw.summary,
        props,
        scope: raw.scope,
        owner_principal: raw.owner_principal,
        task_id: raw.task_id,
        group: raw.group_name,
        expires_at,
        provenance: Provenance {
            source: raw.prov_source,
            desc_hash: raw.prov_desc_hash,
            confidence: raw.prov_confidence,
            extracted_at,
            agent: raw.prov_agent,
        },
        valid_from,
        valid_to,
    })
}

pub(crate) fn read_edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEdgeRow> {
    let conf: Option<f64> = row.get(5)?;
    let resolution_raw: String = row.get(6)?;
    let resolution =
        EdgeResolution::parse_db(&resolution_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let prov_conf: Option<f64> = row.get(9)?;
    Ok(RawEdgeRow {
        id: row.get(0)?,
        edge_type: row.get(1)?,
        from_id: row.get(2)?,
        to_id: row.get(3)?,
        props_json: row.get(4)?,
        confidence: conf.map(|v| v as f32),
        resolution,
        prov_source: row.get(7)?,
        prov_desc_hash: row.get(8)?,
        prov_confidence: prov_conf.map(|v| v as f32),
        prov_extracted_at_us: row.get(10)?,
        prov_agent: row.get(11)?,
        valid_from_us: row.get(12)?,
        valid_to_us: row.get(13)?,
    })
}

pub(crate) fn materialize_edge(raw: RawEdgeRow) -> Result<Edge, StoreError> {
    let props: JsonMap<String, JsonValue> =
        serde_json::from_str(&raw.props_json).map_err(|_| StoreError::Corrupt)?;
    let extracted_at = match raw.prov_extracted_at_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    let valid_from = micros_to_timestamp(raw.valid_from_us).map_err(|_| StoreError::Corrupt)?;
    let valid_to = match raw.valid_to_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    Ok(Edge {
        id: EdgeId::new(raw.id).map_err(|_| StoreError::Corrupt)?,
        edge_type: EdgeType::new(raw.edge_type).map_err(|_| StoreError::Corrupt)?,
        from: NodeId::new(raw.from_id).map_err(|_| StoreError::Corrupt)?,
        to: NodeId::new(raw.to_id).map_err(|_| StoreError::Corrupt)?,
        props,
        confidence: raw.confidence,
        resolution: raw.resolution,
        provenance: Provenance {
            source: raw.prov_source,
            desc_hash: raw.prov_desc_hash,
            confidence: raw.prov_confidence,
            extracted_at,
            agent: raw.prov_agent,
        },
        valid_from,
        valid_to,
    })
}

#[inline]
fn hash_str(hasher: &mut blake3::Hasher, s: &str) {
    hasher.update(&(s.len() as u64).to_le_bytes());
    hasher.update(s.as_bytes());
}

#[inline]
fn hash_opt_str(hasher: &mut blake3::Hasher, opt: Option<&str>) {
    match opt {
        Some(s) => {
            hasher.update(&[1u8]);
            hash_str(hasher, s);
        }
        None => {
            hasher.update(&[0u8]);
        }
    }
}

#[inline]
fn hash_opt_f32(hasher: &mut blake3::Hasher, opt: Option<f32>) {
    match opt {
        Some(c) => {
            let norm: f32 = if c == 0.0 { 0.0 } else { c };
            hasher.update(&[1u8]);
            hasher.update(&norm.to_bits().to_le_bytes());
        }
        None => {
            hasher.update(&[0u8]);
        }
    }
}

#[inline]
fn hash_opt_i64(hasher: &mut blake3::Hasher, opt: Option<i64>) {
    match opt {
        Some(v) => {
            hasher.update(&[1u8]);
            hasher.update(&v.to_le_bytes());
        }
        None => {
            hasher.update(&[0u8]);
        }
    }
}

#[inline]
fn hash_provenance(hasher: &mut blake3::Hasher, prov: &Provenance) {
    hash_str(hasher, &prov.source);
    hash_opt_str(hasher, prov.desc_hash.as_deref());
    hash_opt_f32(hasher, prov.confidence);
    hash_opt_i64(hasher, prov.extracted_at.map(timestamp_to_micros));
    hash_opt_str(hasher, prov.agent.as_deref());
}

fn compute_node_content_hash(
    node: &NewNode,
    props_canonical_json: &str,
    resolved: &ResolvedScope,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gap-store:node:v1:");
    hash_str(&mut hasher, node.node_type.as_str());
    hash_str(&mut hasher, &node.label);
    hash_opt_str(&mut hasher, node.summary.as_deref());
    hash_str(&mut hasher, props_canonical_json);
    hash_str(&mut hasher, resolved.scope.as_str());
    hash_str(&mut hasher, resolved.base_scope.as_str());
    match resolved.task_id {
        Some(tid) => {
            hasher.update(&[1u8]);
            hasher.update(tid.as_uuid().as_bytes());
        }
        None => {
            hasher.update(&[0u8]);
        }
    }
    hash_opt_str(&mut hasher, resolved.group.as_deref());
    hash_opt_i64(&mut hasher, resolved.expires_at_us);
    hash_provenance(&mut hasher, &node.provenance);
    *hasher.finalize().as_bytes()
}

fn compute_edge_content_hash(
    edge: &NewEdge,
    props_canonical_json: &str,
    normalized_conf: Option<f32>,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gap-store:edge:v1:");
    hash_str(&mut hasher, edge.edge_type.as_str());
    hash_str(&mut hasher, edge.from.as_str());
    hash_str(&mut hasher, edge.to.as_str());
    hash_str(&mut hasher, props_canonical_json);
    hash_opt_f32(&mut hasher, normalized_conf);
    hash_str(&mut hasher, edge.resolution.as_str());
    hash_provenance(&mut hasher, &edge.provenance);
    *hasher.finalize().as_bytes()
}

const SELECT_TENANT_STATS_SQL: &str = "
    SELECT node_count, edge_count
    FROM tenant_stats
    WHERE tenant = ?1
";

const UPSERT_TENANT_STATS_SQL: &str = "
    INSERT INTO tenant_stats (tenant, node_count, edge_count)
    VALUES (?1, ?2, ?3)
    ON CONFLICT(tenant) DO UPDATE SET
        node_count = excluded.node_count,
        edge_count = excluded.edge_count
";

const SELECT_CURRENT_NODE_META_SQL: &str = "
    SELECT
        fts_rowid, scope, base_scope, owner_principal, task_id, group_name,
        expires_at, valid_from, content_hash
    FROM nodes
    WHERE tenant = ?1 AND id = ?2 AND valid_to IS NULL
";

const SELECT_NODE_AS_OF_SQL: &str = "
    SELECT
        id, node_type, label, summary, props_json,
        scope, base_scope, owner_principal, task_id, group_name, expires_at,
        prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
        valid_from, valid_to
    FROM nodes
    WHERE tenant = ?1
      AND id = ?2
      AND valid_from <= ?3
      AND (valid_to IS NULL OR ?3 < valid_to)
";

const SELECT_LATEST_NODE_VERSION_SQL: &str = "
    SELECT
        id, node_type, label, summary, props_json,
        scope, base_scope, owner_principal, task_id, group_name, expires_at,
        prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
        valid_from, valid_to
    FROM nodes
    WHERE tenant = ?1 AND id = ?2
    ORDER BY fts_rowid DESC
    LIMIT 1
";

const CLOSE_NODE_BY_ROWID_SQL: &str = "
    UPDATE nodes
    SET valid_to = ?1
    WHERE tenant = ?2 AND fts_rowid = ?3 AND valid_to IS NULL
";

const INSERT_NODE_SQL: &str = "
    INSERT INTO nodes (
        tenant, id, node_type, label, summary, props_json,
        scope, base_scope, owner_principal, task_id, group_name, expires_at,
        prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
        valid_from, valid_to, content_hash
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6,
        ?7, ?8, ?9, ?10, ?11, ?12,
        ?13, ?14, ?15, ?16, ?17,
        ?18, NULL, ?19
    )
";

const INSERT_NODE_FTS_SQL: &str = "
    INSERT INTO nodes_fts (rowid, tenant, label, summary)
    VALUES (?1, ?2, ?3, COALESCE(?4, ''))
";

const SELECT_CURRENT_EDGE_META_SQL: &str = "
    SELECT edge_rowid, id, valid_from, content_hash
    FROM edges
    WHERE tenant = ?1
      AND edge_type = ?2
      AND from_id = ?3
      AND to_id = ?4
      AND valid_to IS NULL
";

const SELECT_EDGE_AS_OF_SQL: &str = "
    SELECT
        id, edge_type, from_id, to_id, props_json,
        confidence, resolution,
        prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
        valid_from, valid_to
    FROM edges
    WHERE tenant = ?1
      AND id = ?2
      AND valid_from <= ?3
      AND (valid_to IS NULL OR ?3 < valid_to)
";

const CLOSE_EDGE_BY_ROWID_SQL: &str = "
    UPDATE edges
    SET valid_to = ?1
    WHERE tenant = ?2 AND edge_rowid = ?3 AND valid_to IS NULL
";

const INSERT_EDGE_SQL: &str = "
    INSERT INTO edges (
        tenant, id, edge_type, from_id, to_id, props_json,
        confidence, resolution,
        prov_source, prov_desc_hash, prov_confidence, prov_extracted_at, prov_agent,
        valid_from, valid_to, content_hash
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6,
        ?7, ?8,
        ?9, ?10, ?11, ?12, ?13,
        ?14, NULL, ?15
    )
";

const CLOSE_INCIDENT_EDGES_SQL: &str = "
    UPDATE edges
    SET valid_to = MAX(?1, valid_from)
    WHERE tenant = ?2
      AND (from_id = ?3 OR to_id = ?3)
      AND valid_to IS NULL
";

const SELECT_NODE_VERSIONS_FOR_DELETE_SQL: &str = "
    SELECT fts_rowid, valid_to IS NULL
    FROM nodes
    WHERE tenant = ?1 AND id = ?2
";

const COUNT_ACTIVE_INCIDENT_EDGES_SQL: &str = "
    SELECT COUNT(*)
    FROM edges
    WHERE tenant = ?1
      AND (from_id = ?2 OR to_id = ?2)
      AND valid_to IS NULL
";

const DELETE_NODE_FTS_ROW_SQL: &str = "
    DELETE FROM nodes_fts
    WHERE rowid = ?1 AND tenant = ?2
";

const DELETE_INCIDENT_EDGES_SQL: &str = "
    DELETE FROM edges
    WHERE tenant = ?1 AND (from_id = ?2 OR to_id = ?2)
";

const DELETE_NODE_VERSIONS_SQL: &str = "
    DELETE FROM nodes
    WHERE tenant = ?1 AND id = ?2
";

const COUNTS_AS_OF_SQL: &str = "
    SELECT
        (
            SELECT COUNT(*)
            FROM nodes n
            WHERE n.tenant = ?1
              AND n.valid_from <= ?2
              AND (n.valid_to IS NULL OR ?2 < n.valid_to)
              AND (n.scope != 'EPHEMERAL' OR (n.expires_at IS NOT NULL AND n.expires_at > ?3 AND n.expires_at > ?2))
              AND (
                  n.base_scope = 'GLOBAL'
                  OR (n.base_scope = 'PRIVATE' AND n.owner_principal = ?4)
                  OR (n.base_scope = 'GROUP' AND n.group_name IS NOT NULL
                      AND EXISTS (SELECT 1 FROM json_each(?5) AS g WHERE g.value = n.group_name))
                  OR (n.base_scope = 'TASK' AND n.task_id IS NOT NULL
                      AND EXISTS (SELECT 1 FROM json_each(?6) AS t WHERE t.value = n.task_id))
              )
        ) AS visible_nodes,
        (
            SELECT COUNT(*)
            FROM edges e
            JOIN nodes nf
              ON nf.tenant = e.tenant
             AND nf.id = e.from_id
             AND nf.valid_from <= ?2
             AND (nf.valid_to IS NULL OR ?2 < nf.valid_to)
            JOIN nodes nt
              ON nt.tenant = e.tenant
             AND nt.id = e.to_id
             AND nt.valid_from <= ?2
             AND (nt.valid_to IS NULL OR ?2 < nt.valid_to)
            WHERE e.tenant = ?1
              AND e.valid_from <= ?2
              AND (e.valid_to IS NULL OR ?2 < e.valid_to)
              AND (nf.scope != 'EPHEMERAL' OR (nf.expires_at IS NOT NULL AND nf.expires_at > ?3 AND nf.expires_at > ?2))
              AND (
                  nf.base_scope = 'GLOBAL'
                  OR (nf.base_scope = 'PRIVATE' AND nf.owner_principal = ?4)
                  OR (nf.base_scope = 'GROUP' AND nf.group_name IS NOT NULL
                      AND EXISTS (SELECT 1 FROM json_each(?5) AS g WHERE g.value = nf.group_name))
                  OR (nf.base_scope = 'TASK' AND nf.task_id IS NOT NULL
                      AND EXISTS (SELECT 1 FROM json_each(?6) AS t WHERE t.value = nf.task_id))
              )
              AND (nt.scope != 'EPHEMERAL' OR (nt.expires_at IS NOT NULL AND nt.expires_at > ?3 AND nt.expires_at > ?2))
              AND (
                  nt.base_scope = 'GLOBAL'
                  OR (nt.base_scope = 'PRIVATE' AND nt.owner_principal = ?4)
                  OR (nt.base_scope = 'GROUP' AND nt.group_name IS NOT NULL
                      AND EXISTS (SELECT 1 FROM json_each(?5) AS g WHERE g.value = nt.group_name))
                  OR (nt.base_scope = 'TASK' AND nt.task_id IS NOT NULL
                      AND EXISTS (SELECT 1 FROM json_each(?6) AS t WHERE t.value = nt.task_id))
              )
        ) AS visible_edges
";

pub(crate) fn read_tenant_stats(
    tx: &Transaction<'_>,
    tenant_str: &str,
) -> Result<(u64, u64), StoreError> {
    let mut stmt = tx
        .prepare_cached(SELECT_TENANT_STATS_SQL)
        .map_err(map_sqlite_err)?;
    let pair: Option<(i64, i64)> = stmt
        .query_row(params![tenant_str], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(map_sqlite_err)?;
    match pair {
        Some((n, e)) => Ok((
            u64::try_from(n.max(0)).unwrap_or(0),
            u64::try_from(e.max(0)).unwrap_or(0),
        )),
        None => Ok((0, 0)),
    }
}

pub(crate) fn write_tenant_stats(
    tx: &Transaction<'_>,
    tenant_str: &str,
    node_count: u64,
    edge_count: u64,
) -> Result<(), StoreError> {
    let n = i64::try_from(node_count).map_err(|_| StoreError::QuotaExceeded)?;
    let e = i64::try_from(edge_count).map_err(|_| StoreError::QuotaExceeded)?;
    let mut stmt = tx
        .prepare_cached(UPSERT_TENANT_STATS_SQL)
        .map_err(map_sqlite_err)?;
    stmt.execute(params![tenant_str, n, e])
        .map_err(map_sqlite_err)?;
    Ok(())
}

fn load_current_node_meta_stmt(
    stmt: &mut rusqlite::Statement<'_>,
    tenant_str: &str,
    node_id: &str,
) -> Result<Option<CurrentNodeMeta>, StoreError> {
    stmt.query_row(params![tenant_str, node_id], read_current_node_meta)
        .optional()
        .map_err(map_sqlite_err)
}

fn load_current_node_meta(
    conn: &Connection,
    tenant_str: &str,
    node_id: &str,
) -> Result<Option<CurrentNodeMeta>, StoreError> {
    let mut stmt = conn
        .prepare_cached(SELECT_CURRENT_NODE_META_SQL)
        .map_err(map_sqlite_err)?;
    load_current_node_meta_stmt(&mut stmt, tenant_str, node_id)
}

fn is_meta_visible(meta: &CurrentNodeMeta, caller: &Caller, now_us: i64) -> bool {
    is_scope_fields_visible(
        meta.scope,
        meta.base_scope,
        &meta.owner_principal,
        meta.task_id,
        meta.group_name.as_deref(),
        meta.expires_at_us,
        caller,
        now_us,
        now_us,
    )
}

struct NodeVersionWrite<'a> {
    tenant_str: &'a str,
    id: &'a str,
    owner_principal: &'a str,
    node: &'a NewNode,
    props_json: &'a str,
    resolved: &'a ResolvedScope,
    valid_from_us: i64,
    content_hash: &'a [u8; 32],
}

fn insert_node_version(
    tx: &Transaction<'_>,
    ins: &mut rusqlite::Statement<'_>,
    fts_ins: &mut rusqlite::Statement<'_>,
    w: &NodeVersionWrite<'_>,
) -> Result<(), StoreError> {
    let task_str = w.resolved.task_id.map(|t| t.to_string());
    let prov_conf = w.node.provenance.confidence.map(f64::from);
    let prov_extracted_us = w.node.provenance.extracted_at.map(timestamp_to_micros);

    ins.execute(params![
        w.tenant_str,
        w.id,
        w.node.node_type.as_str(),
        w.node.label.as_str(),
        w.node.summary.as_deref(),
        w.props_json,
        w.resolved.scope.as_str(),
        w.resolved.base_scope.as_str(),
        w.owner_principal,
        task_str.as_deref(),
        w.resolved.group.as_deref(),
        w.resolved.expires_at_us,
        w.node.provenance.source.as_str(),
        w.node.provenance.desc_hash.as_deref(),
        prov_conf,
        prov_extracted_us,
        w.node.provenance.agent.as_deref(),
        w.valid_from_us,
        w.content_hash.as_slice(),
    ])
    .map_err(map_sqlite_err)?;

    let fts_rowid = tx.last_insert_rowid();
    fts_ins
        .execute(params![
            fts_rowid,
            w.tenant_str,
            w.node.label.as_str(),
            w.node.summary.as_deref()
        ])
        .map_err(map_sqlite_err)?;
    Ok(())
}

struct EdgeVersionWrite<'a> {
    tenant_str: &'a str,
    edge_id: &'a str,
    edge: &'a NewEdge,
    props_json: &'a str,
    norm_conf: Option<f32>,
    valid_from_us: i64,
    content_hash: &'a [u8; 32],
}

fn insert_edge_version(
    ins: &mut rusqlite::Statement<'_>,
    w: &EdgeVersionWrite<'_>,
) -> Result<(), StoreError> {
    let conf_f64 = w.norm_conf.map(f64::from);
    let prov_conf = w.edge.provenance.confidence.map(f64::from);
    let prov_extracted_us = w.edge.provenance.extracted_at.map(timestamp_to_micros);
    ins.execute(params![
        w.tenant_str,
        w.edge_id,
        w.edge.edge_type.as_str(),
        w.edge.from.as_str(),
        w.edge.to.as_str(),
        w.props_json,
        conf_f64,
        w.edge.resolution.as_str(),
        w.edge.provenance.source.as_str(),
        w.edge.provenance.desc_hash.as_deref(),
        prov_conf,
        prov_extracted_us,
        w.edge.provenance.agent.as_deref(),
        w.valid_from_us,
        w.content_hash.as_slice(),
    ])
    .map_err(map_sqlite_err)?;
    Ok(())
}

impl SqliteContextStore {
    pub(crate) fn exec_put_batch(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        nodes: &[NewNode],
        edges: &[NewEdge],
    ) -> Result<BatchResult, StoreError> {
        validate_caller(writer)?;
        let total = nodes.len().saturating_add(edges.len());
        if total > MAX_BATCH_ITEMS {
            return Err(StoreError::Invalid {
                field: "batch",
                reason: "batch exceeds 10000 items",
            });
        }
        if total == 0 {
            return Ok(BatchResult::default());
        }

        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);

        let mut prepared_nodes = Vec::with_capacity(nodes.len());
        for n in nodes {
            let (props_json, resolved) = validate_new_node(n, now_us)?;
            let hash = compute_node_content_hash(n, &props_json, &resolved);
            prepared_nodes.push((n, props_json, resolved, hash));
        }

        let mut prepared_edges = Vec::with_capacity(edges.len());
        for e in edges {
            let (props_json, norm_conf) = validate_new_edge(e)?;
            let hash = compute_edge_content_hash(e, &props_json, norm_conf);
            prepared_edges.push((e, props_json, norm_conf, hash));
        }

        let tenant_str = tenant.tenant().as_str();
        let mut writer_conn = lock_unpoisoned(&self.writer);
        let tx = writer_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let (mut active_nodes, mut active_edges) = read_tenant_stats(&tx, tenant_str)?;
        let mut visible_node_cache: HashMap<String, bool> =
            HashMap::with_capacity(nodes.len().max(edges.len().min(4_096)));

        let mut node_ids = Vec::with_capacity(prepared_nodes.len());
        let mut edge_ids = Vec::with_capacity(prepared_edges.len());

        {
            let mut select_node_stmt = tx
                .prepare_cached(SELECT_CURRENT_NODE_META_SQL)
                .map_err(map_sqlite_err)?;
            let mut close_node_stmt = tx
                .prepare_cached(CLOSE_NODE_BY_ROWID_SQL)
                .map_err(map_sqlite_err)?;
            let mut insert_node_stmt =
                tx.prepare_cached(INSERT_NODE_SQL).map_err(map_sqlite_err)?;
            let mut insert_fts_stmt = tx
                .prepare_cached(INSERT_NODE_FTS_SQL)
                .map_err(map_sqlite_err)?;

            for (raw_node, props_json, resolved, hash) in &prepared_nodes {
                match (&raw_node.id, resolved.scope) {
                    (None, _) => {
                        if active_nodes >= self.config.max_nodes_per_tenant {
                            return Err(StoreError::QuotaExceeded);
                        }
                        let minted = NodeId::new(self.mint_uuid_v7().to_string())?;
                        insert_node_version(
                            &tx,
                            &mut insert_node_stmt,
                            &mut insert_fts_stmt,
                            &NodeVersionWrite {
                                tenant_str,
                                id: minted.as_str(),
                                owner_principal: &writer.principal,
                                node: raw_node,
                                props_json,
                                resolved,
                                valid_from_us: now_us,
                                content_hash: hash,
                            },
                        )?;
                        active_nodes = active_nodes.saturating_add(1);
                        let post_vis = is_base_scope_visible(
                            resolved.base_scope,
                            &writer.principal,
                            resolved.task_id,
                            resolved.group.as_deref(),
                            writer,
                        );
                        visible_node_cache.insert(minted.as_str().to_owned(), post_vis);
                        node_ids.push(minted);
                    }
                    (Some(id), Scope::Global) => {
                        if let Some(existing) = load_current_node_meta_stmt(
                            &mut select_node_stmt,
                            tenant_str,
                            id.as_str(),
                        )? {
                            if !is_meta_visible(&existing, writer, now_us) {
                                return Err(StoreError::Conflict);
                            }
                            if existing.content_hash.as_slice() != hash.as_slice() {
                                let eff_now = now_us.max(existing.valid_from_us);
                                close_node_stmt
                                    .execute(params![eff_now, tenant_str, existing.fts_rowid])
                                    .map_err(map_sqlite_err)?;
                                insert_node_version(
                                    &tx,
                                    &mut insert_node_stmt,
                                    &mut insert_fts_stmt,
                                    &NodeVersionWrite {
                                        tenant_str,
                                        id: id.as_str(),
                                        owner_principal: &writer.principal,
                                        node: raw_node,
                                        props_json,
                                        resolved,
                                        valid_from_us: eff_now,
                                        content_hash: hash,
                                    },
                                )?;
                            }
                            visible_node_cache.insert(id.as_str().to_owned(), true);
                            node_ids.push(id.clone());
                        } else {
                            if active_nodes >= self.config.max_nodes_per_tenant {
                                return Err(StoreError::QuotaExceeded);
                            }
                            insert_node_version(
                                &tx,
                                &mut insert_node_stmt,
                                &mut insert_fts_stmt,
                                &NodeVersionWrite {
                                    tenant_str,
                                    id: id.as_str(),
                                    owner_principal: &writer.principal,
                                    node: raw_node,
                                    props_json,
                                    resolved,
                                    valid_from_us: now_us,
                                    content_hash: hash,
                                },
                            )?;
                            active_nodes = active_nodes.saturating_add(1);
                            visible_node_cache.insert(id.as_str().to_owned(), true);
                            node_ids.push(id.clone());
                        }
                    }
                    (Some(id), _) => {
                        let Some(existing) = load_current_node_meta_stmt(
                            &mut select_node_stmt,
                            tenant_str,
                            id.as_str(),
                        )?
                        else {
                            return Err(StoreError::NotFound);
                        };
                        if !is_meta_visible(&existing, writer, now_us) {
                            return Err(StoreError::NotFound);
                        }
                        if existing.scope == Scope::Global {
                            return Err(StoreError::Invalid {
                                field: "scope",
                                reason: "cannot change a GLOBAL node to a non-GLOBAL scope",
                            });
                        }
                        let owner_changed = existing.owner_principal != writer.principal;
                        if existing.content_hash.as_slice() != hash.as_slice() || owner_changed {
                            let eff_now = now_us.max(existing.valid_from_us);
                            close_node_stmt
                                .execute(params![eff_now, tenant_str, existing.fts_rowid])
                                .map_err(map_sqlite_err)?;
                            insert_node_version(
                                &tx,
                                &mut insert_node_stmt,
                                &mut insert_fts_stmt,
                                &NodeVersionWrite {
                                    tenant_str,
                                    id: id.as_str(),
                                    owner_principal: &writer.principal,
                                    node: raw_node,
                                    props_json,
                                    resolved,
                                    valid_from_us: eff_now,
                                    content_hash: hash,
                                },
                            )?;
                        }
                        let post_visible = is_base_scope_visible(
                            resolved.base_scope,
                            &writer.principal,
                            resolved.task_id,
                            resolved.group.as_deref(),
                            writer,
                        );
                        visible_node_cache.insert(id.as_str().to_owned(), post_visible);
                        node_ids.push(id.clone());
                    }
                }
            }

            if !prepared_edges.is_empty() {
                let mut select_edge_stmt = tx
                    .prepare_cached(SELECT_CURRENT_EDGE_META_SQL)
                    .map_err(map_sqlite_err)?;
                let mut close_edge_stmt = tx
                    .prepare_cached(CLOSE_EDGE_BY_ROWID_SQL)
                    .map_err(map_sqlite_err)?;
                let mut insert_edge_stmt =
                    tx.prepare_cached(INSERT_EDGE_SQL).map_err(map_sqlite_err)?;

                for (raw_edge, props_json, norm_conf, hash) in &prepared_edges {
                    for endpoint_id in [raw_edge.from.as_str(), raw_edge.to.as_str()] {
                        let is_vis = if let Some(&cached) = visible_node_cache.get(endpoint_id) {
                            cached
                        } else {
                            let vis = match load_current_node_meta_stmt(
                                &mut select_node_stmt,
                                tenant_str,
                                endpoint_id,
                            )? {
                                Some(meta) => is_meta_visible(&meta, writer, now_us),
                                None => false,
                            };
                            visible_node_cache.insert(endpoint_id.to_owned(), vis);
                            vis
                        };
                        if !is_vis {
                            return Err(StoreError::NotFound);
                        }
                    }

                    let existing_edge: Option<CurrentEdgeMeta> = select_edge_stmt
                        .query_row(
                            params![
                                tenant_str,
                                raw_edge.edge_type.as_str(),
                                raw_edge.from.as_str(),
                                raw_edge.to.as_str()
                            ],
                            |r| {
                                Ok(CurrentEdgeMeta {
                                    edge_rowid: r.get(0)?,
                                    id: r.get(1)?,
                                    valid_from_us: r.get(2)?,
                                    content_hash: r.get(3)?,
                                })
                            },
                        )
                        .optional()
                        .map_err(map_sqlite_err)?;

                    if let Some(existing) = existing_edge {
                        let existing_id = EdgeId::new(existing.id)?;
                        if let Some(given_id) = &raw_edge.id
                            && given_id != &existing_id
                        {
                            return Err(StoreError::Conflict);
                        }
                        if existing.content_hash.as_slice() != hash.as_slice() {
                            let eff_now = now_us.max(existing.valid_from_us);
                            close_edge_stmt
                                .execute(params![eff_now, tenant_str, existing.edge_rowid])
                                .map_err(map_sqlite_err)?;
                            insert_edge_version(
                                &mut insert_edge_stmt,
                                &EdgeVersionWrite {
                                    tenant_str,
                                    edge_id: existing_id.as_str(),
                                    edge: raw_edge,
                                    props_json,
                                    norm_conf: *norm_conf,
                                    valid_from_us: eff_now,
                                    content_hash: hash,
                                },
                            )?;
                        }
                        edge_ids.push(existing_id);
                    } else {
                        if active_edges >= self.config.max_edges_per_tenant {
                            return Err(StoreError::QuotaExceeded);
                        }
                        let edge_id = match &raw_edge.id {
                            Some(id) => id.clone(),
                            None => EdgeId::new(self.mint_uuid_v7().to_string())?,
                        };
                        insert_edge_version(
                            &mut insert_edge_stmt,
                            &EdgeVersionWrite {
                                tenant_str,
                                edge_id: edge_id.as_str(),
                                edge: raw_edge,
                                props_json,
                                norm_conf: *norm_conf,
                                valid_from_us: now_us,
                                content_hash: hash,
                            },
                        )?;
                        active_edges = active_edges.saturating_add(1);
                        edge_ids.push(edge_id);
                    }
                }
            }
        }

        write_tenant_stats(&tx, tenant_str, active_nodes, active_edges)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(BatchResult { node_ids, edge_ids })
    }

    pub(crate) fn exec_get_node(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &NodeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Node>, StoreError> {
        validate_caller(caller)?;
        let now_us = timestamp_to_micros(self.clock.now_timestamp());
        let as_of_us = as_of.map_or(now_us, timestamp_to_micros);

        let guard = self.readers.checkout();
        let conn = guard.conn()?;
        let mut stmt = conn
            .prepare_cached(SELECT_NODE_AS_OF_SQL)
            .map_err(map_sqlite_err)?;
        let raw_opt = stmt
            .query_row(
                params![tenant.tenant().as_str(), id.as_str(), as_of_us],
                read_node_row,
            )
            .optional()
            .map_err(map_sqlite_err)?;
        let Some(raw) = raw_opt else {
            return Ok(None);
        };
        if !is_node_row_visible(&raw, caller, now_us, as_of_us) {
            return Ok(None);
        }
        materialize_node(raw).map(Some)
    }

    pub(crate) fn exec_get_edge(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &EdgeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Edge>, StoreError> {
        validate_caller(caller)?;
        let now_us = timestamp_to_micros(self.clock.now_timestamp());
        let as_of_us = as_of.map_or(now_us, timestamp_to_micros);
        let tenant_str = tenant.tenant().as_str();

        let guard = self.readers.checkout();
        let conn = guard.conn()?;
        let mut edge_stmt = conn
            .prepare_cached(SELECT_EDGE_AS_OF_SQL)
            .map_err(map_sqlite_err)?;
        let Some(raw_edge) = edge_stmt
            .query_row(params![tenant_str, id.as_str(), as_of_us], read_edge_row)
            .optional()
            .map_err(map_sqlite_err)?
        else {
            return Ok(None);
        };

        let mut node_stmt = conn
            .prepare_cached(SELECT_NODE_AS_OF_SQL)
            .map_err(map_sqlite_err)?;
        for endpoint_id in [&raw_edge.from_id, &raw_edge.to_id] {
            let Some(ep_row) = node_stmt
                .query_row(params![tenant_str, endpoint_id, as_of_us], read_node_row)
                .optional()
                .map_err(map_sqlite_err)?
            else {
                return Ok(None);
            };
            if !is_node_row_visible(&ep_row, caller, now_us, as_of_us) {
                return Ok(None);
            }
        }
        materialize_edge(raw_edge).map(Some)
    }

    pub(crate) fn exec_tombstone_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError> {
        validate_caller(writer)?;
        let now_us = timestamp_to_micros(self.clock.now_timestamp());
        let tenant_str = tenant.tenant().as_str();

        let mut writer_conn = lock_unpoisoned(&self.writer);
        let tx = writer_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let Some(current) = load_current_node_meta(&tx, tenant_str, id.as_str())? else {
            return Err(StoreError::NotFound);
        };
        if !is_meta_visible(&current, writer, now_us) {
            return Err(StoreError::NotFound);
        }

        let eff_now = now_us.max(current.valid_from_us);
        {
            let mut close_node = tx
                .prepare_cached(CLOSE_NODE_BY_ROWID_SQL)
                .map_err(map_sqlite_err)?;
            close_node
                .execute(params![eff_now, tenant_str, current.fts_rowid])
                .map_err(map_sqlite_err)?;
        }
        let closed_edges = {
            let mut close_edges = tx
                .prepare_cached(CLOSE_INCIDENT_EDGES_SQL)
                .map_err(map_sqlite_err)?;
            close_edges
                .execute(params![eff_now, tenant_str, id.as_str()])
                .map_err(map_sqlite_err)?
        };

        let (active_nodes, active_edges) = read_tenant_stats(&tx, tenant_str)?;
        write_tenant_stats(
            &tx,
            tenant_str,
            active_nodes.saturating_sub(1),
            active_edges.saturating_sub(u64::try_from(closed_edges).unwrap_or(0)),
        )?;
        tx.commit().map_err(map_sqlite_err)
    }

    pub(crate) fn exec_delete_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError> {
        validate_caller(writer)?;
        let now_us = timestamp_to_micros(self.clock.now_timestamp());
        let tenant_str = tenant.tenant().as_str();

        let mut writer_conn = lock_unpoisoned(&self.writer);
        let tx = writer_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let latest: Option<RawNodeRow> = {
            let mut stmt = tx
                .prepare_cached(SELECT_LATEST_NODE_VERSION_SQL)
                .map_err(map_sqlite_err)?;
            stmt.query_row(params![tenant_str, id.as_str()], read_node_row)
                .optional()
                .map_err(map_sqlite_err)?
        };
        let Some(latest_row) = latest else {
            return Err(StoreError::NotFound);
        };
        if !is_node_row_visible(&latest_row, writer, now_us, now_us) {
            return Err(StoreError::NotFound);
        }

        let mut fts_rowids = Vec::new();
        let mut active_node_delta = 0u64;
        {
            let mut v_stmt = tx
                .prepare_cached(SELECT_NODE_VERSIONS_FOR_DELETE_SQL)
                .map_err(map_sqlite_err)?;
            let rows = v_stmt
                .query_map(params![tenant_str, id.as_str()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?))
                })
                .map_err(map_sqlite_err)?;
            for item in rows {
                let (rowid, is_active) = item.map_err(map_sqlite_err)?;
                fts_rowids.push(rowid);
                if is_active {
                    active_node_delta = active_node_delta.saturating_add(1);
                }
            }
        }

        let active_edge_delta: u64 = {
            let mut cnt_stmt = tx
                .prepare_cached(COUNT_ACTIVE_INCIDENT_EDGES_SQL)
                .map_err(map_sqlite_err)?;
            let raw: i64 = cnt_stmt
                .query_row(params![tenant_str, id.as_str()], |r| r.get(0))
                .map_err(map_sqlite_err)?;
            u64::try_from(raw.max(0)).unwrap_or(0)
        };

        {
            let mut del_fts = tx
                .prepare_cached(DELETE_NODE_FTS_ROW_SQL)
                .map_err(map_sqlite_err)?;
            for rowid in fts_rowids {
                del_fts
                    .execute(params![rowid, tenant_str])
                    .map_err(map_sqlite_err)?;
            }
        }
        {
            let mut del_edges = tx
                .prepare_cached(DELETE_INCIDENT_EDGES_SQL)
                .map_err(map_sqlite_err)?;
            del_edges
                .execute(params![tenant_str, id.as_str()])
                .map_err(map_sqlite_err)?;
        }
        {
            let mut del_nodes = tx
                .prepare_cached(DELETE_NODE_VERSIONS_SQL)
                .map_err(map_sqlite_err)?;
            del_nodes
                .execute(params![tenant_str, id.as_str()])
                .map_err(map_sqlite_err)?;
        }

        let (active_nodes, active_edges) = read_tenant_stats(&tx, tenant_str)?;
        write_tenant_stats(
            &tx,
            tenant_str,
            active_nodes.saturating_sub(active_node_delta),
            active_edges.saturating_sub(active_edge_delta),
        )?;
        tx.commit().map_err(map_sqlite_err)
    }

    pub(crate) fn exec_counts(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        as_of: Option<Timestamp>,
    ) -> Result<GraphCounts, StoreError> {
        validate_caller(caller)?;
        let now_us = timestamp_to_micros(self.clock.now_timestamp());
        let as_of_us = as_of.map_or(now_us, timestamp_to_micros);
        let (groups_json, tasks_json) = caller_claims_json(caller)?;

        let guard = self.readers.checkout();
        let conn = guard.conn()?;
        let mut stmt = conn
            .prepare_cached(COUNTS_AS_OF_SQL)
            .map_err(map_sqlite_err)?;
        let (n, e): (i64, i64) = stmt
            .query_row(
                params![
                    tenant.tenant().as_str(),
                    as_of_us,
                    now_us,
                    caller.principal.as_str(),
                    groups_json,
                    tasks_json
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(map_sqlite_err)?;
        Ok(GraphCounts {
            nodes: u64::try_from(n.max(0)).unwrap_or(0),
            edges: u64::try_from(e.max(0)).unwrap_or(0),
        })
    }
}
