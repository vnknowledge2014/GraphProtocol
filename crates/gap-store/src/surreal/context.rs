//! `ContextStore` and `ContextMaintenance` implementation backed by SurrealDB 3.x (`ADR-020`,
//! `INV-09`, `ING-065`, `ING-066`, `ING-067`).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gap_core::clock::{Clock, Timestamp};
use gap_core::ids::{TaskId, TenantScope};
use serde_json::{Map as JsonMap, Value as JsonValue};
use uuid::{Builder as UuidBuilder, Uuid};

use crate::error::StoreError;
use crate::store::{ContextMaintenance, ContextStore};
use crate::surreal::schema::{SURREAL_SCHEMA_SURREALQL, tokenize_alphanumeric};
use crate::surreal::transport::{
    EngineState, HttpSurrealTransport, InMemorySurrealEngine, SharedEngine, StoredEdgeVersion,
    StoredNodeVersion, SurrealConfig, SurrealTransport, TenantStats,
};
use crate::types::{
    BatchResult, Caller, Direction, Edge, EdgeId, EdgeType, GraphCounts, MAX_BATCH_ITEMS, NewEdge,
    NewNode, Node, NodeId, NodeType, Provenance, PurgePolicy, PurgeReport, ResolvedScope, Scope,
    TraverseQuery, TraverseResult, TraverseResume, TraverseStats, TraverseStrategy,
    TraverseWarning, append_canonical_json, micros_to_timestamp, normalize_timestamp,
    timestamp_to_micros, validate_caller, validate_new_edge, validate_new_node,
    validate_search_limit, validate_traverse_query,
};

/// SurrealDB 3.x implementation of [`ContextStore`], [`ContextMaintenance`], and
/// [`crate::TaskStore`] (`ADR-020`).
///
/// Shares one transactional Surreal engine and optional remote HTTP `/sql` transport so callers
/// can run purely offline deterministic suites or mirror DDL and queries against an external
/// SurrealDB 3.x server without bringing BSL-1.1 Rust crates into default workspace builds.
pub struct SurrealContextStore {
    pub(crate) config: SurrealConfig,
    pub(crate) engine: SharedEngine,
    pub(crate) remote: Option<Arc<dyn SurrealTransport>>,
    pub(crate) clock: Arc<dyn Clock>,
    uuid_seq: AtomicU64,
}

impl std::fmt::Debug for SurrealContextStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurrealContextStore")
            .field("namespace", &self.config.namespace)
            .field("database", &self.config.database)
            .field("remote_configured", &self.remote.is_some())
            .finish_non_exhaustive()
    }
}

impl SurrealContextStore {
    /// Opens a [`SurrealContextStore`] configured with `config` and deterministic `clock`.
    ///
    /// If `config.endpoint` is `Some(...)`, validates the HTTP endpoint and prepares a remote
    /// [`HttpSurrealTransport`]; otherwise runs purely on the embedded transactional engine.
    pub fn open(config: SurrealConfig, clock: Arc<dyn Clock>) -> Result<Self, StoreError> {
        let remote: Option<Arc<dyn SurrealTransport>> = match &config.endpoint {
            Some(_) => Some(Arc::new(HttpSurrealTransport::new(&config)?)),
            None => None,
        };
        let engine = Arc::new(InMemorySurrealEngine::new());
        let _ = engine.execute_surrealql(SURREAL_SCHEMA_SURREALQL);
        Ok(Self {
            config,
            engine,
            remote,
            clock,
            uuid_seq: AtomicU64::new(0),
        })
    }

    /// Creates a [`SurrealContextStore`] using a custom [`SurrealTransport`] alongside the
    /// transactional state engine (useful for testing remote DDL/query emission offline).
    pub fn open_with_transport(
        config: SurrealConfig,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn SurrealTransport>,
    ) -> Result<Self, StoreError> {
        let engine = Arc::new(InMemorySurrealEngine::new());
        let _ = engine.execute_surrealql(SURREAL_SCHEMA_SURREALQL);
        Ok(Self {
            config,
            engine,
            remote: Some(transport),
            clock,
            uuid_seq: AtomicU64::new(0),
        })
    }

    /// Sends the idempotent `SURREAL_SCHEMA_SURREALQL` DDL to the configured remote transport (or
    /// records it in the embedded engine when offline).
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        if let Some(ref transport) = self.remote {
            let _ = transport.execute_surrealql(SURREAL_SCHEMA_SURREALQL)?;
        } else {
            let _ = self.engine.execute_surrealql(SURREAL_SCHEMA_SURREALQL)?;
        }
        Ok(())
    }

    /// Returns a cloned handle to the underlying shared engine so callers can construct a
    /// companion [`crate::surreal::SurrealTaskStore`] over the same tenant state.
    pub(crate) fn shared_engine(&self) -> SharedEngine {
        Arc::clone(&self.engine)
    }

    /// Returns the injected [`Clock`] handle.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub(crate) fn mint_uuid_v7(&self) -> Uuid {
        let unix_ms = u64::try_from(self.clock.unix_millis().max(0)).unwrap_or(0);
        let seq = self.uuid_seq.fetch_add(1, Ordering::Relaxed);
        let entropy = *Uuid::new_v4().as_bytes();
        let mut rand_bytes = [0u8; 10];
        let seq_hi = ((seq >> 8) as u8) & 0x0f;
        let seq_lo = (seq & 0xff) as u8;
        rand_bytes[0] = (entropy[0] & 0xf0) | seq_hi;
        rand_bytes[1] = seq_lo;
        rand_bytes[2..10].copy_from_slice(&entropy[2..10]);
        UuidBuilder::from_unix_timestamp_millis(unix_ms, &rand_bytes).into_uuid()
    }
}

fn scope_wire_str(scope: Scope) -> &'static str {
    match scope {
        Scope::Private => "PRIVATE",
        Scope::Task => "TASK",
        Scope::Group => "GROUP",
        Scope::Global => "GLOBAL",
        Scope::Ephemeral => "EPHEMERAL",
    }
}

fn is_base_scope_visible(
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

fn is_node_visible_at(
    node: &StoredNodeVersion,
    caller: &Caller,
    now_us: i64,
    as_of_us: i64,
) -> bool {
    let temporal_ok =
        node.valid_from_us <= as_of_us && node.valid_to_us.is_none_or(|vt| as_of_us < vt);
    if !temporal_ok {
        return false;
    }
    if node.scope == Scope::Ephemeral {
        let Some(exp) = node.expires_at_us else {
            return false;
        };
        if exp <= now_us || exp <= as_of_us {
            return false;
        }
    }
    is_base_scope_visible(
        node.base_scope,
        &node.owner_principal,
        node.task_id,
        node.group_name.as_deref(),
        caller,
    )
}

fn is_node_current_visible(node: &StoredNodeVersion, caller: &Caller, now_us: i64) -> bool {
    if node.valid_to_us.is_some() {
        return false;
    }
    if node.scope == Scope::Ephemeral && node.expires_at_us.is_none_or(|exp| exp <= now_us) {
        return false;
    }
    is_base_scope_visible(
        node.base_scope,
        &node.owner_principal,
        node.task_id,
        node.group_name.as_deref(),
        caller,
    )
}

fn node_content_hash(
    node: &NewNode,
    props_json: &str,
    resolved: &ResolvedScope,
) -> Result<[u8; 32], StoreError> {
    let extracted_us = node
        .provenance
        .extracted_at
        .map(normalize_timestamp)
        .transpose()?
        .map(timestamp_to_micros);
    let conf_bits = node.provenance.confidence.map(|c| {
        let norm = if c == 0.0 { 0.0 } else { c };
        norm.to_bits()
    });
    let payload = serde_json::json!([
        node.node_type.as_str(),
        &node.label,
        &node.summary,
        props_json,
        scope_wire_str(resolved.scope),
        scope_wire_str(resolved.base_scope),
        resolved.task_id.map(|t| t.to_string()),
        &resolved.group,
        resolved.expires_at_us,
        &node.provenance.source,
        &node.provenance.desc_hash,
        conf_bits,
        extracted_us,
        &node.provenance.agent,
    ]);
    let mut bytes = Vec::with_capacity(128);
    append_canonical_json(&payload, &mut bytes)?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

fn edge_content_hash(
    edge: &NewEdge,
    props_json: &str,
    norm_conf: Option<f32>,
) -> Result<[u8; 32], StoreError> {
    let extracted_us = edge
        .provenance
        .extracted_at
        .map(normalize_timestamp)
        .transpose()?
        .map(timestamp_to_micros);
    let prov_conf_bits = edge.provenance.confidence.map(|c| {
        let norm = if c == 0.0 { 0.0 } else { c };
        norm.to_bits()
    });
    let res_tag = match edge.resolution {
        crate::types::EdgeResolution::Exact => "EXACT",
        crate::types::EdgeResolution::Heuristic => "HEURISTIC",
    };
    let payload = serde_json::json!([
        edge.edge_type.as_str(),
        edge.from.as_str(),
        edge.to.as_str(),
        props_json,
        norm_conf.map(f32::to_bits),
        res_tag,
        &edge.provenance.source,
        &edge.provenance.desc_hash,
        prov_conf_bits,
        extracted_us,
        &edge.provenance.agent,
    ]);
    let mut bytes = Vec::with_capacity(128);
    append_canonical_json(&payload, &mut bytes)?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

fn materialize_node(v: &StoredNodeVersion) -> Result<Node, StoreError> {
    let props: JsonMap<String, JsonValue> =
        serde_json::from_str(&v.props_json).map_err(|_| StoreError::Corrupt)?;
    let expires_at = match v.expires_at_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    let extracted_at = match v.prov_extracted_at_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    let valid_from = micros_to_timestamp(v.valid_from_us).map_err(|_| StoreError::Corrupt)?;
    let valid_to = match v.valid_to_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    Ok(Node {
        id: NodeId::new(v.id.clone()).map_err(|_| StoreError::Corrupt)?,
        node_type: NodeType::new(v.node_type.clone()).map_err(|_| StoreError::Corrupt)?,
        label: v.label.clone(),
        summary: v.summary.clone(),
        props,
        scope: v.scope,
        owner_principal: v.owner_principal.clone(),
        task_id: v.task_id,
        group: v.group_name.clone(),
        expires_at,
        provenance: Provenance {
            source: v.prov_source.clone(),
            desc_hash: v.prov_desc_hash.clone(),
            confidence: v.prov_confidence,
            extracted_at,
            agent: v.prov_agent.clone(),
        },
        valid_from,
        valid_to,
    })
}

fn materialize_edge(v: &StoredEdgeVersion) -> Result<Edge, StoreError> {
    let props: JsonMap<String, JsonValue> =
        serde_json::from_str(&v.props_json).map_err(|_| StoreError::Corrupt)?;
    let extracted_at = match v.prov_extracted_at_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    let valid_from = micros_to_timestamp(v.valid_from_us).map_err(|_| StoreError::Corrupt)?;
    let valid_to = match v.valid_to_us {
        Some(us) => Some(micros_to_timestamp(us).map_err(|_| StoreError::Corrupt)?),
        None => None,
    };
    Ok(Edge {
        id: EdgeId::new(v.id.clone()).map_err(|_| StoreError::Corrupt)?,
        edge_type: EdgeType::new(v.edge_type.clone()).map_err(|_| StoreError::Corrupt)?,
        from: NodeId::new(v.from_id.clone()).map_err(|_| StoreError::Corrupt)?,
        to: NodeId::new(v.to_id.clone()).map_err(|_| StoreError::Corrupt)?,
        props,
        confidence: v.confidence,
        resolution: v.resolution,
        provenance: Provenance {
            source: v.prov_source.clone(),
            desc_hash: v.prov_desc_hash.clone(),
            confidence: v.prov_confidence,
            extracted_at,
            agent: v.prov_agent.clone(),
        },
        valid_from,
        valid_to,
    })
}

fn build_node_version(
    id: String,
    owner_principal: &str,
    raw: &NewNode,
    props_json: String,
    resolved: ResolvedScope,
    valid_from_us: i64,
    content_hash: [u8; 32],
) -> Result<StoredNodeVersion, StoreError> {
    let extracted_us = raw
        .provenance
        .extracted_at
        .map(normalize_timestamp)
        .transpose()?
        .map(timestamp_to_micros);
    Ok(StoredNodeVersion {
        id,
        node_type: raw.node_type.as_str().to_owned(),
        label: raw.label.clone(),
        summary: raw.summary.clone(),
        props_json,
        scope: resolved.scope,
        base_scope: resolved.base_scope,
        owner_principal: owner_principal.to_owned(),
        task_id: resolved.task_id,
        group_name: resolved.group,
        expires_at_us: resolved.expires_at_us,
        prov_source: raw.provenance.source.clone(),
        prov_desc_hash: raw.provenance.desc_hash.clone(),
        prov_confidence: raw.provenance.confidence,
        prov_extracted_at_us: extracted_us,
        prov_agent: raw.provenance.agent.clone(),
        valid_from_us,
        valid_to_us: None,
        content_hash,
    })
}

fn build_edge_version(
    id: String,
    raw: &NewEdge,
    props_json: String,
    norm_conf: Option<f32>,
    valid_from_us: i64,
    content_hash: [u8; 32],
) -> Result<StoredEdgeVersion, StoreError> {
    let extracted_us = raw
        .provenance
        .extracted_at
        .map(normalize_timestamp)
        .transpose()?
        .map(timestamp_to_micros);
    Ok(StoredEdgeVersion {
        id,
        edge_type: raw.edge_type.as_str().to_owned(),
        from_id: raw.from.as_str().to_owned(),
        to_id: raw.to.as_str().to_owned(),
        props_json,
        confidence: norm_conf,
        resolution: raw.resolution,
        prov_source: raw.provenance.source.clone(),
        prov_desc_hash: raw.provenance.desc_hash.clone(),
        prov_confidence: raw.provenance.confidence,
        prov_extracted_at_us: extracted_us,
        prov_agent: raw.provenance.agent.clone(),
        valid_from_us,
        valid_to_us: None,
        content_hash,
    })
}

fn find_current_node_idx(versions: &[StoredNodeVersion], id: &str) -> Option<usize> {
    versions
        .iter()
        .rposition(|v| v.id == id && v.valid_to_us.is_none())
}

fn find_node_as_of<'a>(
    versions: &'a [StoredNodeVersion],
    id: &str,
    as_of_us: i64,
) -> Option<&'a StoredNodeVersion> {
    versions.iter().rev().find(|v| {
        v.id == id && v.valid_from_us <= as_of_us && v.valid_to_us.is_none_or(|vt| as_of_us < vt)
    })
}

fn find_edge_as_of<'a>(
    versions: &'a [StoredEdgeVersion],
    id: &str,
    as_of_us: i64,
) -> Option<&'a StoredEdgeVersion> {
    versions.iter().rev().find(|v| {
        v.id == id && v.valid_from_us <= as_of_us && v.valid_to_us.is_none_or(|vt| as_of_us < vt)
    })
}

fn commit_rollback_guard<F, T>(engine: &SharedEngine, op: F) -> Result<T, StoreError>
where
    F: FnOnce(&mut EngineState) -> Result<T, StoreError>,
{
    let mut guard = engine.write()?;
    let backup = guard.clone();
    match op(&mut guard) {
        Ok(res) => Ok(res),
        Err(err) => {
            *guard = backup;
            Err(err)
        }
    }
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

fn filter_visible_nodes_at<'a>(
    nodes: &'a [StoredNodeVersion],
    caller: &Caller,
    now_us: i64,
    as_of_us: i64,
    scopes_filter: Option<&[Scope]>,
) -> HashMap<&'a str, &'a StoredNodeVersion> {
    let mut visible = HashMap::new();
    for v in nodes {
        if is_node_visible_at(v, caller, now_us, as_of_us)
            && scopes_filter.is_none_or(|s| s.contains(&v.scope))
        {
            visible.insert(v.id.as_str(), v);
        }
    }
    visible
}

fn eligible_neighbours<'a>(
    edges: &'a [StoredEdgeVersion],
    visible_nodes: &HashMap<&'a str, &'a StoredNodeVersion>,
    origin_id: &str,
    direction: Direction,
    edge_types_filter: Option<&[EdgeType]>,
    as_of_us: i64,
) -> Result<Vec<(String, Edge)>, StoreError> {
    let mut out = Vec::new();
    for ev in edges {
        let temporal_ok =
            ev.valid_from_us <= as_of_us && ev.valid_to_us.is_none_or(|vt| as_of_us < vt);
        if !temporal_ok {
            continue;
        }
        if let Some(allowed) = edge_types_filter
            && !allowed.iter().any(|et| et.as_str() == ev.edge_type)
        {
            continue;
        }
        match direction {
            Direction::Outbound => {
                if ev.from_id == origin_id && visible_nodes.contains_key(ev.to_id.as_str()) {
                    out.push((ev.to_id.clone(), materialize_edge(ev)?));
                }
            }
            Direction::Inbound => {
                if ev.to_id == origin_id && visible_nodes.contains_key(ev.from_id.as_str()) {
                    out.push((ev.from_id.clone(), materialize_edge(ev)?));
                }
            }
            Direction::Both => {
                if ev.from_id == origin_id && visible_nodes.contains_key(ev.to_id.as_str()) {
                    out.push((ev.to_id.clone(), materialize_edge(ev)?));
                } else if ev.to_id == origin_id
                    && ev.from_id != origin_id
                    && visible_nodes.contains_key(ev.from_id.as_str())
                {
                    out.push((ev.from_id.clone(), materialize_edge(ev)?));
                }
            }
        }
    }
    out.sort_by(|(na, ea), (nb, eb)| na.cmp(nb).then_with(|| ea.id.cmp(&eb.id)));
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdCost(f64);

impl Eq for OrdCost {}

impl PartialOrd for OrdCost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdCost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl ContextStore for SurrealContextStore {
    fn put_batch(
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
                reason: "batch exceeds 1000 items",
            });
        }

        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);

        let mut prepared_nodes = Vec::with_capacity(nodes.len());
        for raw_node in nodes {
            let (props_json, resolved) = validate_new_node(raw_node, now_us)?;
            let hash = node_content_hash(raw_node, &props_json, &resolved)?;
            prepared_nodes.push((raw_node, props_json, resolved, hash));
        }

        let mut prepared_edges = Vec::with_capacity(edges.len());
        for raw_edge in edges {
            let (props_json, norm_conf) = validate_new_edge(raw_edge)?;
            let hash = edge_content_hash(raw_edge, &props_json, norm_conf)?;
            prepared_edges.push((raw_edge, props_json, norm_conf, hash));
        }

        if total == 0 {
            return Ok(BatchResult::default());
        }

        let tenant_str = tenant.tenant().as_str().to_owned();
        commit_rollback_guard(&self.engine, |state| {
            let mut stats = state
                .stats_by_tenant
                .get(&tenant_str)
                .copied()
                .unwrap_or_default();
            let mut node_versions = state
                .nodes_by_tenant
                .remove(&tenant_str)
                .unwrap_or_default();
            let mut edge_versions = state
                .edges_by_tenant
                .remove(&tenant_str)
                .unwrap_or_default();

            let mut node_ids = Vec::with_capacity(prepared_nodes.len());
            let mut edge_ids = Vec::with_capacity(prepared_edges.len());
            let mut visible_cache: HashMap<String, bool> = HashMap::new();

            for (raw_node, props_json, resolved, hash) in prepared_nodes {
                match (&raw_node.id, resolved.scope) {
                    (None, _) => {
                        if stats.active_nodes >= self.config.max_nodes_per_tenant {
                            return Err(StoreError::QuotaExceeded);
                        }
                        let minted = NodeId::new(self.mint_uuid_v7().to_string())?;
                        let post_vis = is_base_scope_visible(
                            resolved.base_scope,
                            &writer.principal,
                            resolved.task_id,
                            resolved.group.as_deref(),
                            writer,
                        );
                        let ver = build_node_version(
                            minted.as_str().to_owned(),
                            &writer.principal,
                            raw_node,
                            props_json,
                            resolved,
                            now_us,
                            hash,
                        )?;
                        node_versions.push(ver);
                        stats.active_nodes = stats.active_nodes.saturating_add(1);
                        visible_cache.insert(minted.as_str().to_owned(), post_vis);
                        node_ids.push(minted);
                    }
                    (Some(id), Scope::Global) => {
                        if let Some(idx) = find_current_node_idx(&node_versions, id.as_str()) {
                            let existing_vis =
                                is_node_current_visible(&node_versions[idx], writer, now_us);
                            if !existing_vis {
                                return Err(StoreError::Conflict);
                            }
                            if node_versions[idx].content_hash != hash {
                                let eff_now = now_us.max(node_versions[idx].valid_from_us);
                                node_versions[idx].valid_to_us = Some(eff_now);
                                let ver = build_node_version(
                                    id.as_str().to_owned(),
                                    &writer.principal,
                                    raw_node,
                                    props_json,
                                    resolved,
                                    eff_now,
                                    hash,
                                )?;
                                node_versions.push(ver);
                            }
                            visible_cache.insert(id.as_str().to_owned(), true);
                            node_ids.push(id.clone());
                        } else {
                            if stats.active_nodes >= self.config.max_nodes_per_tenant {
                                return Err(StoreError::QuotaExceeded);
                            }
                            let ver = build_node_version(
                                id.as_str().to_owned(),
                                &writer.principal,
                                raw_node,
                                props_json,
                                resolved,
                                now_us,
                                hash,
                            )?;
                            node_versions.push(ver);
                            stats.active_nodes = stats.active_nodes.saturating_add(1);
                            visible_cache.insert(id.as_str().to_owned(), true);
                            node_ids.push(id.clone());
                        }
                    }
                    (Some(id), _) => {
                        let Some(idx) = find_current_node_idx(&node_versions, id.as_str()) else {
                            return Err(StoreError::NotFound);
                        };
                        if !is_node_current_visible(&node_versions[idx], writer, now_us) {
                            return Err(StoreError::NotFound);
                        }
                        if node_versions[idx].scope == Scope::Global {
                            return Err(StoreError::Invalid {
                                field: "scope",
                                reason: "cannot change a GLOBAL node to a non-GLOBAL scope",
                            });
                        }
                        let owner_changed = node_versions[idx].owner_principal != writer.principal;
                        let post_vis = is_base_scope_visible(
                            resolved.base_scope,
                            &writer.principal,
                            resolved.task_id,
                            resolved.group.as_deref(),
                            writer,
                        );
                        if node_versions[idx].content_hash != hash || owner_changed {
                            let eff_now = now_us.max(node_versions[idx].valid_from_us);
                            node_versions[idx].valid_to_us = Some(eff_now);
                            let ver = build_node_version(
                                id.as_str().to_owned(),
                                &writer.principal,
                                raw_node,
                                props_json,
                                resolved,
                                eff_now,
                                hash,
                            )?;
                            node_versions.push(ver);
                        }
                        visible_cache.insert(id.as_str().to_owned(), post_vis);
                        node_ids.push(id.clone());
                    }
                }
            }

            for (raw_edge, props_json, norm_conf, hash) in prepared_edges {
                for endpoint_id in [raw_edge.from.as_str(), raw_edge.to.as_str()] {
                    let vis = if let Some(&cached) = visible_cache.get(endpoint_id) {
                        cached
                    } else {
                        let computed = find_current_node_idx(&node_versions, endpoint_id)
                            .is_some_and(|i| {
                                is_node_current_visible(&node_versions[i], writer, now_us)
                            });
                        visible_cache.insert(endpoint_id.to_owned(), computed);
                        computed
                    };
                    if !vis {
                        return Err(StoreError::NotFound);
                    }
                }

                let existing_edge_idx = edge_versions.iter().rposition(|ev| {
                    ev.valid_to_us.is_none()
                        && ev.edge_type == raw_edge.edge_type.as_str()
                        && ev.from_id == raw_edge.from.as_str()
                        && ev.to_id == raw_edge.to.as_str()
                });

                if let Some(e_idx) = existing_edge_idx {
                    let existing_id = EdgeId::new(edge_versions[e_idx].id.clone())?;
                    if let Some(given_id) = &raw_edge.id
                        && given_id != &existing_id
                    {
                        return Err(StoreError::Conflict);
                    }
                    if edge_versions[e_idx].content_hash != hash {
                        let eff_now = now_us.max(edge_versions[e_idx].valid_from_us);
                        edge_versions[e_idx].valid_to_us = Some(eff_now);
                        let ver = build_edge_version(
                            existing_id.as_str().to_owned(),
                            raw_edge,
                            props_json,
                            norm_conf,
                            eff_now,
                            hash,
                        )?;
                        edge_versions.push(ver);
                    }
                    edge_ids.push(existing_id);
                } else {
                    if stats.active_edges >= self.config.max_edges_per_tenant {
                        return Err(StoreError::QuotaExceeded);
                    }
                    let edge_id = match &raw_edge.id {
                        Some(id) => id.clone(),
                        None => EdgeId::new(self.mint_uuid_v7().to_string())?,
                    };
                    let ver = build_edge_version(
                        edge_id.as_str().to_owned(),
                        raw_edge,
                        props_json,
                        norm_conf,
                        now_us,
                        hash,
                    )?;
                    edge_versions.push(ver);
                    stats.active_edges = stats.active_edges.saturating_add(1);
                    edge_ids.push(edge_id);
                }
            }

            state.stats_by_tenant.insert(tenant_str.clone(), stats);
            state
                .nodes_by_tenant
                .insert(tenant_str.clone(), node_versions);
            state.edges_by_tenant.insert(tenant_str, edge_versions);
            Ok(BatchResult { node_ids, edge_ids })
        })
    }

    fn get_node(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &NodeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Node>, StoreError> {
        validate_caller(caller)?;
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let as_of_us = match as_of {
            Some(ts) => timestamp_to_micros(normalize_timestamp(ts)?),
            None => now_us,
        };
        let state = self.engine.read()?;
        let Some(versions) = state.nodes_by_tenant.get(tenant.tenant().as_str()) else {
            return Ok(None);
        };
        let Some(ver) = find_node_as_of(versions, id.as_str(), as_of_us) else {
            return Ok(None);
        };
        if !is_node_visible_at(ver, caller, now_us, as_of_us) {
            return Ok(None);
        }
        materialize_node(ver).map(Some)
    }

    fn get_edge(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &EdgeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Edge>, StoreError> {
        validate_caller(caller)?;
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let as_of_us = match as_of {
            Some(ts) => timestamp_to_micros(normalize_timestamp(ts)?),
            None => now_us,
        };
        let state = self.engine.read()?;
        let tenant_str = tenant.tenant().as_str();
        let (Some(edge_versions), Some(node_versions)) = (
            state.edges_by_tenant.get(tenant_str),
            state.nodes_by_tenant.get(tenant_str),
        ) else {
            return Ok(None);
        };
        let Some(edge) = find_edge_as_of(edge_versions, id.as_str(), as_of_us) else {
            return Ok(None);
        };
        for ep in [&edge.from_id, &edge.to_id] {
            let Some(ep_node) = find_node_as_of(node_versions, ep, as_of_us) else {
                return Ok(None);
            };
            if !is_node_visible_at(ep_node, caller, now_us, as_of_us) {
                return Ok(None);
            }
        }
        materialize_edge(edge).map(Some)
    }

    fn tombstone_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError> {
        validate_caller(writer)?;
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let tenant_str = tenant.tenant().as_str();

        commit_rollback_guard(&self.engine, |state| {
            let Some(node_versions) = state.nodes_by_tenant.get_mut(tenant_str) else {
                return Err(StoreError::NotFound);
            };
            let Some(idx) = find_current_node_idx(node_versions, id.as_str()) else {
                return Err(StoreError::NotFound);
            };
            if !is_node_current_visible(&node_versions[idx], writer, now_us) {
                return Err(StoreError::NotFound);
            }
            let eff_now = now_us.max(node_versions[idx].valid_from_us);
            node_versions[idx].valid_to_us = Some(eff_now);

            let mut closed_edges: u64 = 0;
            if let Some(edge_versions) = state.edges_by_tenant.get_mut(tenant_str) {
                for ev in edge_versions.iter_mut() {
                    if ev.valid_to_us.is_none()
                        && (ev.from_id == id.as_str() || ev.to_id == id.as_str())
                    {
                        ev.valid_to_us = Some(eff_now.max(ev.valid_from_us));
                        closed_edges = closed_edges.saturating_add(1);
                    }
                }
            }
            let stats = state
                .stats_by_tenant
                .entry(tenant_str.to_owned())
                .or_default();
            stats.active_nodes = stats.active_nodes.saturating_sub(1);
            stats.active_edges = stats.active_edges.saturating_sub(closed_edges);
            Ok(())
        })
    }

    fn delete_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError> {
        validate_caller(writer)?;
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let tenant_str = tenant.tenant().as_str();

        commit_rollback_guard(&self.engine, |state| {
            let Some(node_versions) = state.nodes_by_tenant.get_mut(tenant_str) else {
                return Err(StoreError::NotFound);
            };
            let has_visible = node_versions
                .iter()
                .any(|v| v.id == id.as_str() && is_node_visible_at(v, writer, now_us, now_us));
            let has_any = node_versions.iter().any(|v| v.id == id.as_str());
            if !has_any
                || (!has_visible
                    && find_current_node_idx(node_versions, id.as_str()).is_none_or(|i| {
                        !is_node_current_visible(&node_versions[i], writer, now_us)
                    }))
            {
                // Allow deleting historical versions owned/visible to writer OR closed prior versions
                // only when at least one visible or owner-visible version exists; match SQLite
                // visibility check on current/historical nodes.
                let owner_or_visible = node_versions.iter().any(|v| {
                    v.id == id.as_str()
                        && is_base_scope_visible(
                            v.base_scope,
                            &v.owner_principal,
                            v.task_id,
                            v.group_name.as_deref(),
                            writer,
                        )
                });
                if !owner_or_visible {
                    return Err(StoreError::NotFound);
                }
            }

            let mut active_node_delta: u64 = 0;
            node_versions.retain(|v| {
                if v.id == id.as_str() {
                    if v.valid_to_us.is_none() {
                        active_node_delta = active_node_delta.saturating_add(1);
                    }
                    false
                } else {
                    true
                }
            });

            let mut active_edge_delta: u64 = 0;
            if let Some(edge_versions) = state.edges_by_tenant.get_mut(tenant_str) {
                edge_versions.retain(|ev| {
                    if ev.from_id == id.as_str() || ev.to_id == id.as_str() {
                        if ev.valid_to_us.is_none() {
                            active_edge_delta = active_edge_delta.saturating_add(1);
                        }
                        false
                    } else {
                        true
                    }
                });
            }

            let stats = state
                .stats_by_tenant
                .entry(tenant_str.to_owned())
                .or_default();
            stats.active_nodes = stats.active_nodes.saturating_sub(active_node_delta);
            stats.active_edges = stats.active_edges.saturating_sub(active_edge_delta);
            Ok(())
        })
    }

    fn traverse(
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

        let state = self.engine.read()?;
        let tenant_str = tenant.tenant().as_str();
        let node_versions = state
            .nodes_by_tenant
            .get(tenant_str)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let edge_versions = state
            .edges_by_tenant
            .get(tenant_str)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let visible_nodes = filter_visible_nodes_at(
            node_versions,
            caller,
            now_us,
            as_of_us,
            query.scopes.as_deref(),
        );
        if !visible_nodes.contains_key(query.from.as_str()) {
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

        let cap_usize = visited_cap as usize;
        let scopes_slice = query.edge_types.as_deref();

        match query.strategy {
            TraverseStrategy::Bfs | TraverseStrategy::Shortest => {
                let mut dist: HashMap<String, u32> = HashMap::new();
                let mut parent_edge: HashMap<String, (String, Edge)> = HashMap::new();
                let mut ordered_nodes: Vec<(String, u32)> = Vec::new();
                let mut current_layer: BTreeSet<String> = BTreeSet::new();
                let mut truncated = false;
                let mut cutoff_depth = query.max_depth;

                dist.insert(query.from.as_str().to_owned(), 0);
                current_layer.insert(query.from.as_str().to_owned());
                ordered_nodes.push((query.from.as_str().to_owned(), 0));

                if ordered_nodes.len() > cap_usize {
                    truncated = true;
                    cutoff_depth = 0;
                    ordered_nodes.truncate(cap_usize);
                }

                for depth in 0..query.max_depth {
                    if truncated || current_layer.is_empty() {
                        break;
                    }
                    let mut next_layer: BTreeSet<String> = BTreeSet::new();
                    for u in &current_layer {
                        let steps = eligible_neighbours(
                            edge_versions,
                            &visible_nodes,
                            u,
                            query.direction,
                            scopes_slice,
                            as_of_us,
                        )?;
                        for (v, edge) in steps {
                            let next_d = depth + 1;
                            if !dist.contains_key(&v) {
                                dist.insert(v.clone(), next_d);
                                parent_edge.insert(v.clone(), (u.clone(), edge));
                                next_layer.insert(v);
                            } else if dist.get(&v).copied() == Some(next_d) {
                                let replace = match parent_edge.get(&v) {
                                    Some((cur_u, cur_e)) => (u, &edge.id) < (cur_u, &cur_e.id),
                                    None => false,
                                };
                                if replace {
                                    parent_edge.insert(v, (u.clone(), edge));
                                }
                            }
                        }
                    }
                    for v in &next_layer {
                        if ordered_nodes.len() >= cap_usize {
                            truncated = true;
                            cutoff_depth = depth + 1;
                            break;
                        }
                        ordered_nodes.push((v.clone(), depth + 1));
                    }
                    current_layer = next_layer;
                }

                let visited_count = u32::try_from(ordered_nodes.len()).unwrap_or(u32::MAX);
                let depth_reached = ordered_nodes.iter().map(|(_, d)| *d).max().unwrap_or(0);
                let start_idx = match &query.after {
                    None => 0,
                    Some(
                        TraverseResume::Bfs { depth, node_id }
                        | TraverseResume::Shortest { depth, node_id },
                    ) => ordered_nodes
                        .partition_point(|(id, d)| (*d, id.as_str()) <= (*depth, node_id.as_str())),
                    Some(_) => return Err(StoreError::Backend),
                };

                let remaining = &ordered_nodes[start_idx..];
                let take_n = remaining.len().min(query.first as usize);
                let page_slice = &remaining[..take_n];

                let resume = if remaining.len() > take_n {
                    page_slice
                        .last()
                        .map(|(last_id, last_depth)| {
                            let nid = NodeId::new(last_id.clone())?;
                            Ok(match query.strategy {
                                TraverseStrategy::Bfs => TraverseResume::Bfs {
                                    depth: *last_depth,
                                    node_id: nid,
                                },
                                _ => TraverseResume::Shortest {
                                    depth: *last_depth,
                                    node_id: nid,
                                },
                            })
                        })
                        .transpose()?
                } else {
                    None
                };

                let mut nodes = Vec::with_capacity(page_slice.len());
                let mut returned_depth: HashMap<&str, u32> = HashMap::new();
                for (id, d) in page_slice {
                    let v = visible_nodes.get(id.as_str()).ok_or(StoreError::Backend)?;
                    nodes.push(materialize_node(v)?);
                    returned_depth.insert(id.as_str(), *d);
                }

                let mut edge_map: HashMap<EdgeId, Edge> = HashMap::new();
                match query.strategy {
                    TraverseStrategy::Bfs => {
                        for (u_id, u_depth) in page_slice {
                            let expanded = *u_depth < query.max_depth
                                && (!truncated || *u_depth < cutoff_depth);
                            if !expanded {
                                continue;
                            }
                            for (v_id, edge) in eligible_neighbours(
                                edge_versions,
                                &visible_nodes,
                                u_id,
                                query.direction,
                                scopes_slice,
                                as_of_us,
                            )? {
                                if returned_depth.contains_key(v_id.as_str()) {
                                    edge_map.insert(edge.id.clone(), edge);
                                }
                            }
                        }
                    }
                    TraverseStrategy::Shortest => {
                        for (v_id, _) in page_slice {
                            if let Some((p_id, edge)) = parent_edge.get(v_id)
                                && returned_depth.contains_key(p_id.as_str())
                            {
                                edge_map.insert(edge.id.clone(), edge.clone());
                            }
                        }
                    }
                    _ => {}
                }

                let mut edges: Vec<Edge> = edge_map.into_values().collect();
                edges.sort_by(|a, b| {
                    let da = returned_depth.get(a.from.as_str()).copied().unwrap_or(0);
                    let db = returned_depth.get(b.from.as_str()).copied().unwrap_or(0);
                    da.cmp(&db).then_with(|| a.id.cmp(&b.id))
                });

                Ok(TraverseResult {
                    stats: TraverseStats {
                        visited: visited_count,
                        returned: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
                        depth_reached,
                    },
                    nodes,
                    edges,
                    resume,
                    truncated,
                    warnings: Vec::new(),
                    as_of: as_of_ts,
                })
            }
            TraverseStrategy::Dfs => {
                let mut stack: Vec<(String, u32)> = vec![(query.from.as_str().to_owned(), 0)];
                let mut seen_order: Vec<(String, u32, u32)> = Vec::new();
                let mut visited_set: HashSet<String> = HashSet::new();
                let mut truncated = false;

                while let Some((u_id, depth)) = stack.pop() {
                    if !visited_set.insert(u_id.clone()) {
                        continue;
                    }
                    if seen_order.len() >= cap_usize {
                        visited_set.remove(&u_id);
                        truncated = true;
                        break;
                    }
                    let ordinal = u32::try_from(seen_order.len()).unwrap_or(u32::MAX);
                    seen_order.push((u_id.clone(), depth, ordinal));

                    if depth < query.max_depth {
                        let neighbours = eligible_neighbours(
                            edge_versions,
                            &visible_nodes,
                            &u_id,
                            query.direction,
                            scopes_slice,
                            as_of_us,
                        )?;
                        for (v_id, _) in neighbours.into_iter().rev() {
                            if !visited_set.contains(&v_id) {
                                stack.push((v_id, depth + 1));
                            }
                        }
                    }
                }

                let visited_count = u32::try_from(seen_order.len()).unwrap_or(u32::MAX);
                let depth_reached = seen_order.iter().map(|(_, d, _)| *d).max().unwrap_or(0);
                let start_idx = match &query.after {
                    None => 0,
                    Some(TraverseResume::Dfs { ordinal, node_id }) => {
                        seen_order.partition_point(|(id, _, ord)| {
                            (*ord, id.as_str()) <= (*ordinal, node_id.as_str())
                        })
                    }
                    Some(_) => return Err(StoreError::Backend),
                };

                let remaining = &seen_order[start_idx..];
                let take_n = remaining.len().min(query.first as usize);
                let page_slice = &remaining[..take_n];

                let resume = if remaining.len() > take_n {
                    page_slice
                        .last()
                        .map(|(last_id, _, ord)| {
                            Ok(TraverseResume::Dfs {
                                ordinal: *ord,
                                node_id: NodeId::new(last_id.clone())?,
                            })
                        })
                        .transpose()?
                } else {
                    None
                };

                let mut nodes = Vec::with_capacity(page_slice.len());
                let mut returned_depth: HashMap<&str, u32> = HashMap::new();
                for (id, d, _) in page_slice {
                    let v = visible_nodes.get(id.as_str()).ok_or(StoreError::Backend)?;
                    nodes.push(materialize_node(v)?);
                    returned_depth.insert(id.as_str(), *d);
                }

                let mut edge_map: HashMap<EdgeId, Edge> = HashMap::new();
                for (u_id, u_depth, _) in page_slice {
                    if *u_depth >= query.max_depth {
                        continue;
                    }
                    for (v_id, edge) in eligible_neighbours(
                        edge_versions,
                        &visible_nodes,
                        u_id,
                        query.direction,
                        scopes_slice,
                        as_of_us,
                    )? {
                        if returned_depth.contains_key(v_id.as_str()) {
                            edge_map.insert(edge.id.clone(), edge);
                        }
                    }
                }
                let mut edges: Vec<Edge> = edge_map.into_values().collect();
                edges.sort_by(|a, b| {
                    let da = returned_depth.get(a.from.as_str()).copied().unwrap_or(0);
                    let db = returned_depth.get(b.from.as_str()).copied().unwrap_or(0);
                    da.cmp(&db).then_with(|| a.id.cmp(&b.id))
                });

                Ok(TraverseResult {
                    stats: TraverseStats {
                        visited: visited_count,
                        returned: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
                        depth_reached,
                    },
                    nodes,
                    edges,
                    resume,
                    truncated,
                    warnings: Vec::new(),
                    as_of: as_of_ts,
                })
            }
            TraverseStrategy::Weighted => {
                let mut heap: BinaryHeap<Reverse<(OrdCost, String, u32)>> = BinaryHeap::new();
                let mut best_cost: HashMap<String, f64> = HashMap::new();
                let mut settled: Vec<(String, f64, u32)> = Vec::new();
                let mut settled_set: HashSet<String> = HashSet::new();
                let mut skipped_invalid_edges: HashSet<EdgeId> = HashSet::new();
                let mut truncated = false;

                best_cost.insert(query.from.as_str().to_owned(), 0.0);
                heap.push(Reverse((OrdCost(0.0), query.from.as_str().to_owned(), 0)));

                while let Some(Reverse((OrdCost(cost), u_id, depth))) = heap.pop() {
                    if settled_set.contains(&u_id) {
                        continue;
                    }
                    if settled.len() >= cap_usize {
                        truncated = true;
                        break;
                    }
                    settled_set.insert(u_id.clone());
                    settled.push((u_id.clone(), cost, depth));

                    if depth < query.max_depth {
                        for (v_id, edge) in eligible_neighbours(
                            edge_versions,
                            &visible_nodes,
                            &u_id,
                            query.direction,
                            scopes_slice,
                            as_of_us,
                        )? {
                            let Some(w) = parse_edge_weight(&edge) else {
                                skipped_invalid_edges.insert(edge.id);
                                continue;
                            };
                            let next_cost = cost + w;
                            if !next_cost.is_finite() {
                                continue;
                            }
                            let improve = best_cost.get(&v_id).is_none_or(|&prev| next_cost < prev);
                            if improve {
                                best_cost.insert(v_id.clone(), next_cost);
                                heap.push(Reverse((OrdCost(next_cost), v_id, depth + 1)));
                            }
                        }
                    }
                }

                let visited_count = u32::try_from(settled.len()).unwrap_or(u32::MAX);
                let depth_reached = settled.iter().map(|(_, _, d)| *d).max().unwrap_or(0);
                let start_idx = match &query.after {
                    None => 0,
                    Some(TraverseResume::Weighted { cost_bits, node_id }) => {
                        let cursor_cost = f64::from_bits(*cost_bits);
                        settled.partition_point(|(id, c, _)| {
                            c.total_cmp(&cursor_cost)
                                .then_with(|| id.as_str().cmp(node_id.as_str()))
                                != std::cmp::Ordering::Greater
                        })
                    }
                    Some(_) => return Err(StoreError::Backend),
                };

                let remaining = &settled[start_idx..];
                let take_n = remaining.len().min(query.first as usize);
                let page_slice = &remaining[..take_n];
                let resume = if remaining.len() > take_n {
                    page_slice
                        .last()
                        .map(|(last_id, last_cost, _)| {
                            let norm = if *last_cost == 0.0 { 0.0 } else { *last_cost };
                            Ok(TraverseResume::Weighted {
                                cost_bits: norm.to_bits(),
                                node_id: NodeId::new(last_id.clone())?,
                            })
                        })
                        .transpose()?
                } else {
                    None
                };

                let mut nodes = Vec::with_capacity(page_slice.len());
                let mut returned_depth: HashMap<&str, u32> = HashMap::new();
                for (id, _, d) in page_slice {
                    let v = visible_nodes.get(id.as_str()).ok_or(StoreError::Backend)?;
                    nodes.push(materialize_node(v)?);
                    returned_depth.insert(id.as_str(), *d);
                }

                let mut edge_map: HashMap<EdgeId, Edge> = HashMap::new();
                for (u_id, _, u_depth) in page_slice {
                    if *u_depth >= query.max_depth {
                        continue;
                    }
                    for (v_id, edge) in eligible_neighbours(
                        edge_versions,
                        &visible_nodes,
                        u_id,
                        query.direction,
                        scopes_slice,
                        as_of_us,
                    )? {
                        if parse_edge_weight(&edge).is_some()
                            && returned_depth.contains_key(v_id.as_str())
                        {
                            edge_map.insert(edge.id.clone(), edge);
                        }
                    }
                }
                let mut edges: Vec<Edge> = edge_map.into_values().collect();
                edges.sort_by(|a, b| {
                    let da = returned_depth.get(a.from.as_str()).copied().unwrap_or(0);
                    let db = returned_depth.get(b.from.as_str()).copied().unwrap_or(0);
                    da.cmp(&db).then_with(|| a.id.cmp(&b.id))
                });

                let mut warnings = Vec::new();
                if !skipped_invalid_edges.is_empty() {
                    warnings.push(TraverseWarning::SkippedInvalidWeights {
                        count: u32::try_from(skipped_invalid_edges.len()).unwrap_or(u32::MAX),
                    });
                }

                Ok(TraverseResult {
                    stats: TraverseStats {
                        visited: visited_count,
                        returned: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
                        depth_reached,
                    },
                    nodes,
                    edges,
                    resume,
                    truncated,
                    warnings,
                    as_of: as_of_ts,
                })
            }
        }
    }

    fn search_nodes(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &str,
        limit: u32,
        as_of: Option<Timestamp>,
    ) -> Result<Vec<Node>, StoreError> {
        validate_caller(caller)?;
        validate_search_limit(limit)?;
        let query_tokens = tokenize_alphanumeric(query);
        if query_tokens.is_empty() {
            return Ok(Vec::new());
        }

        let now_ts = normalize_timestamp(self.clock.now_timestamp())?;
        let now_us = timestamp_to_micros(now_ts);
        let as_of_us = match as_of {
            Some(ts) => timestamp_to_micros(normalize_timestamp(ts)?),
            None => now_us,
        };

        let state = self.engine.read()?;
        let Some(node_versions) = state.nodes_by_tenant.get(tenant.tenant().as_str()) else {
            return Ok(Vec::new());
        };
        let visible = filter_visible_nodes_at(node_versions, caller, now_us, as_of_us, None);

        let mut scored: Vec<(f64, &str, &StoredNodeVersion)> = Vec::new();
        for (id, ver) in visible {
            let mut label_counts: BTreeMap<String, usize> = BTreeMap::new();
            for tok in tokenize_alphanumeric(&ver.label) {
                *label_counts.entry(tok).or_insert(0) += 1;
            }
            let mut summary_counts: BTreeMap<String, usize> = BTreeMap::new();
            if let Some(ref sum) = ver.summary {
                for tok in tokenize_alphanumeric(sum) {
                    *summary_counts.entry(tok).or_insert(0) += 1;
                }
            }

            let mut all_present = true;
            let mut score = 0.0f64;
            for qtok in &query_tokens {
                let lc = label_counts.get(qtok).copied().unwrap_or(0);
                let sc = summary_counts.get(qtok).copied().unwrap_or(0);
                if lc == 0 && sc == 0 {
                    all_present = false;
                    break;
                }
                let tf = (lc as f64) * 2.0 + (sc as f64);
                score += tf / (tf + 1.2);
            }
            if all_present {
                scored.push((score, id, ver));
            }
        }

        scored.sort_by(|(sa, ida, _), (sb, idb, _)| sb.total_cmp(sa).then_with(|| ida.cmp(idb)));
        scored.truncate(limit as usize);

        let mut results = Vec::with_capacity(scored.len());
        for (_, _, ver) in scored {
            results.push(materialize_node(ver)?);
        }
        Ok(results)
    }

    fn counts(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        as_of: Option<Timestamp>,
    ) -> Result<GraphCounts, StoreError> {
        validate_caller(caller)?;
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let as_of_us = match as_of {
            Some(ts) => timestamp_to_micros(normalize_timestamp(ts)?),
            None => now_us,
        };
        let state = self.engine.read()?;
        let tenant_str = tenant.tenant().as_str();
        let node_versions = state
            .nodes_by_tenant
            .get(tenant_str)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let edge_versions = state
            .edges_by_tenant
            .get(tenant_str)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let visible = filter_visible_nodes_at(node_versions, caller, now_us, as_of_us, None);
        let mut edge_count: u64 = 0;
        for ev in edge_versions {
            let temporal_ok =
                ev.valid_from_us <= as_of_us && ev.valid_to_us.is_none_or(|vt| as_of_us < vt);
            if temporal_ok
                && visible.contains_key(ev.from_id.as_str())
                && visible.contains_key(ev.to_id.as_str())
            {
                edge_count = edge_count.saturating_add(1);
            }
        }
        Ok(GraphCounts {
            nodes: u64::try_from(visible.len()).unwrap_or(u64::MAX),
            edges: edge_count,
        })
    }
}

impl ContextMaintenance for SurrealContextStore {
    fn tombstone_task_nodes(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<u64, StoreError> {
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let tenant_str = tenant.tenant().as_str();
        commit_rollback_guard(&self.engine, |state| {
            let Some(node_versions) = state.nodes_by_tenant.get_mut(tenant_str) else {
                return Ok(0);
            };
            let mut closed_ids: HashSet<String> = HashSet::new();
            let mut closed_count: u64 = 0;
            for v in node_versions.iter_mut() {
                if v.base_scope == Scope::Task
                    && v.task_id == Some(task_id)
                    && v.valid_to_us.is_none()
                {
                    v.valid_to_us = Some(now_us.max(v.valid_from_us));
                    closed_ids.insert(v.id.clone());
                    closed_count = closed_count.saturating_add(1);
                }
            }
            let mut closed_edges: u64 = 0;
            if !closed_ids.is_empty()
                && let Some(edge_versions) = state.edges_by_tenant.get_mut(tenant_str)
            {
                for ev in edge_versions.iter_mut() {
                    if ev.valid_to_us.is_none()
                        && (closed_ids.contains(&ev.from_id) || closed_ids.contains(&ev.to_id))
                    {
                        ev.valid_to_us = Some(now_us.max(ev.valid_from_us));
                        closed_edges = closed_edges.saturating_add(1);
                    }
                }
            }
            let stats = state
                .stats_by_tenant
                .entry(tenant_str.to_owned())
                .or_default();
            stats.active_nodes = stats.active_nodes.saturating_sub(closed_count);
            stats.active_edges = stats.active_edges.saturating_sub(closed_edges);
            Ok(closed_count)
        })
    }

    fn purge_expired(&self, policy: &PurgePolicy) -> Result<PurgeReport, StoreError> {
        let now_us = timestamp_to_micros(normalize_timestamp(self.clock.now_timestamp())?);
        let retention_us = i64::try_from(policy.task_retention.as_micros()).unwrap_or(i64::MAX);
        let cutoff_us = now_us.saturating_sub(retention_us);

        commit_rollback_guard(&self.engine, |state| {
            let mut report = PurgeReport::default();
            let tenants: Vec<String> = state.nodes_by_tenant.keys().cloned().collect();

            for tenant_str in tenants {
                let Some(mut node_versions) = state.nodes_by_tenant.remove(&tenant_str) else {
                    continue;
                };
                let mut edge_versions = state
                    .edges_by_tenant
                    .remove(&tenant_str)
                    .unwrap_or_default();

                // 1. Expired EPHEMERAL versions
                let mut removed_ephemeral_ids: HashSet<String> = HashSet::new();
                let mut active_ephem_delta: u64 = 0;
                node_versions.retain(|v| {
                    let expired = v.scope == Scope::Ephemeral
                        && v.expires_at_us.is_some_and(|exp| exp <= now_us);
                    if expired {
                        report.purged_ephemeral_nodes =
                            report.purged_ephemeral_nodes.saturating_add(1);
                        removed_ephemeral_ids.insert(v.id.clone());
                        if v.valid_to_us.is_none() {
                            active_ephem_delta = active_ephem_delta.saturating_add(1);
                        }
                        false
                    } else {
                        true
                    }
                });

                // 2. Closed TASK versions whose valid_to <= cutoff_us with no surviving version
                let surviving_ids: HashSet<String> = node_versions
                    .iter()
                    .filter(|v| {
                        v.valid_to_us.is_none() || v.valid_to_us.is_some_and(|vt| vt > cutoff_us)
                    })
                    .map(|v| v.id.clone())
                    .collect();

                let mut removed_task_ids: HashSet<String> = HashSet::new();
                node_versions.retain(|v| {
                    let purge_task = v.scope == Scope::Task
                        && v.valid_to_us.is_some_and(|vt| vt <= cutoff_us)
                        && !surviving_ids.contains(&v.id);
                    if purge_task {
                        report.purged_task_nodes = report.purged_task_nodes.saturating_add(1);
                        removed_task_ids.insert(v.id.clone());
                        false
                    } else {
                        true
                    }
                });

                // 3. Remove edges referencing removed nodes with no surviving version at all
                let remaining_ids: HashSet<String> =
                    node_versions.iter().map(|v| v.id.clone()).collect();
                let mut active_edge_delta: u64 = 0;
                edge_versions.retain(|ev| {
                    let orphaned =
                        !remaining_ids.contains(&ev.from_id) || !remaining_ids.contains(&ev.to_id);
                    if orphaned
                        && (removed_ephemeral_ids.contains(&ev.from_id)
                            || removed_ephemeral_ids.contains(&ev.to_id)
                            || removed_task_ids.contains(&ev.from_id)
                            || removed_task_ids.contains(&ev.to_id))
                    {
                        report.purged_edges = report.purged_edges.saturating_add(1);
                        if ev.valid_to_us.is_none() {
                            active_edge_delta = active_edge_delta.saturating_add(1);
                        }
                        false
                    } else {
                        true
                    }
                });

                let stats = state
                    .stats_by_tenant
                    .entry(tenant_str.clone())
                    .or_insert_with(TenantStats::default);
                stats.active_nodes = stats.active_nodes.saturating_sub(active_ephem_delta);
                stats.active_edges = stats.active_edges.saturating_sub(active_edge_delta);

                state
                    .nodes_by_tenant
                    .insert(tenant_str.clone(), node_versions);
                state.edges_by_tenant.insert(tenant_str, edge_versions);
            }
            Ok(report)
        })
    }
}
