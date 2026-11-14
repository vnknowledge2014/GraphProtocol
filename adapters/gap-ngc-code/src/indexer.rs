//! Incremental and reproducible repository graph indexer over `ContextStore` (`ING-060`..`ING-063`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use gap_store::{
    Caller, ContextStore, Direction, Edge, EdgeId, EdgeResolution, EdgeType, NewEdge, NewNode,
    Node, NodeId, NodeType, Provenance, ScopeSpec, StoreError, TenantScope, Timestamp,
    TraverseQuery, TraverseStrategy,
};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use crate::extract::{self, ExtractedFileGraph, ExtractedNode, slugify};
use crate::model::{
    CodeNgcError, EDGE_CALLS, EDGE_CONTAINS, EDGE_DEFINES, EDGE_DEPENDS_ON, EDGE_DOCUMENTS,
    EDGE_EXTENDS, EDGE_IMPLEMENTS, EDGE_IMPORTS, EDGE_REFERENCES, EDGE_SUPERSEDES, EDGE_TESTS,
    EXACT_EDGE_CONFIDENCE, FileParseFailure, GoldenEdge, GoldenGraphSnapshot, GoldenNode,
    HEURISTIC_EDGE_CONFIDENCE, IndexReport, Language, NODE_CONCEPT, NODE_FILE, NODE_MODULE,
    NODE_REPO, NODE_SYMBOL, NODE_TEST, ScipIndex, summarize_text,
};

const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    "dist",
    "build",
    ".local",
];

const PROVENANCE_SOURCE: &str = "gap-ngc-code";
const PROVENANCE_AGENT: &str = "gap-ngc-code/1.0";
const MAX_STORE_BATCH_ITEMS: usize = 10_000;

/// Configuration for [`CodeIndexer`] (`ING-060`..`ING-062`, `spec/09-compat.md`).
#[derive(Debug, Clone)]
pub struct CodeIndexConfig {
    /// Stable repository slug used in node identifiers and labels.
    pub repo_id: String,
    /// Repository root directory on disk.
    pub root_dir: PathBuf,
    /// Optional SCIP/LSIF JSON manifests (`ingest.code.scip_paths[]`, `ING-060`).
    pub scip_paths: Vec<PathBuf>,
    /// File-watcher debounce interval in milliseconds (`spec/09-compat.md`: `500` ms default).
    pub debounce_ms: u64,
}

impl CodeIndexConfig {
    /// Creates a [`CodeIndexConfig`] with default `500` ms debounce and empty `scip_paths`.
    #[must_use]
    pub fn new(repo_id: impl Into<String>, root_dir: impl Into<PathBuf>) -> Self {
        Self {
            repo_id: repo_id.into(),
            root_dir: root_dir.into(),
            scip_paths: Vec::new(),
            debounce_ms: 500,
        }
    }

    /// Configures optional SCIP/LSIF manifest paths (`ING-060`).
    #[must_use]
    pub fn with_scip_paths(mut self, scip_paths: Vec<PathBuf>) -> Self {
        self.scip_paths = scip_paths;
        self
    }

    /// Overrides the watcher debounce interval in milliseconds.
    #[must_use]
    pub const fn with_debounce_ms(mut self, debounce_ms: u64) -> Self {
        self.debounce_ms = debounce_ms;
        self
    }
}

#[derive(Debug, Clone)]
struct FileIndexEntry {
    content_hash: String,
    owned_node_ids: BTreeSet<NodeId>,
    graph: ExtractedFileGraph,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey {
    edge_type: String,
    from: String,
    to: String,
}

struct PendingEdge {
    edge_type: String,
    from: NodeId,
    to: NodeId,
    resolution: EdgeResolution,
    confidence: f32,
}

struct RepoResolutionIndex {
    file_nodes: BTreeMap<String, NodeId>,
    file_by_stem: BTreeMap<String, Vec<NodeId>>,
    module_by_name: BTreeMap<String, Vec<NodeId>>,
    symbols_by_name: BTreeMap<String, Vec<(NodeId, String)>>,
    concepts_by_slug: BTreeMap<String, NodeId>,
}

/// Multi-language incremental code and documentation graph indexer (`ING-060`..`ING-063`).
pub struct CodeIndexer<S: ContextStore> {
    config: CodeIndexConfig,
    store: Arc<S>,
    tenant: TenantScope,
    writer: Caller,
    scip: ScipIndex,
    indexed_files: BTreeMap<String, FileIndexEntry>,
}

impl<S: ContextStore> CodeIndexer<S> {
    /// Creates a new [`CodeIndexer`] writing `GLOBAL` code graph entities into `store` for `tenant`.
    #[must_use]
    pub fn new(
        config: CodeIndexConfig,
        store: Arc<S>,
        tenant: TenantScope,
        writer: Caller,
    ) -> Self {
        Self {
            config,
            store,
            tenant,
            writer,
            scip: ScipIndex::default(),
            indexed_files: BTreeMap::new(),
        }
    }

    /// Returns the active repository index configuration.
    #[must_use]
    pub const fn config(&self) -> &CodeIndexConfig {
        &self.config
    }

    /// Returns the tenant scope owning the indexed graph.
    #[must_use]
    pub const fn tenant(&self) -> &TenantScope {
        &self.tenant
    }

    /// Returns the writer identity recorded on persisted nodes and edges.
    #[must_use]
    pub const fn writer(&self) -> &Caller {
        &self.writer
    }

    /// Returns a shared handle to the backing [`ContextStore`].
    #[must_use]
    pub const fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// Returns the deterministic root `Repo` [`NodeId`].
    ///
    /// # Errors
    ///
    /// Returns [`CodeNgcError::Store`] if `repo_id` violates [`NodeId`] rules.
    pub fn repo_node_id(&self) -> Result<NodeId, CodeNgcError> {
        make_node_id(&format!("repo::{}", self.config.repo_id))
    }

    /// Runs a full reproducible index pass from scratch (`ING-062`).
    ///
    /// # Errors
    ///
    /// Returns an error if repository walking, SCIP manifest loading, or store persistence fails.
    pub fn reindex_full(&mut self) -> Result<IndexReport, CodeNgcError> {
        let disk_files = collect_repo_files(&self.config.root_dir)?;
        let prior_entries = std::mem::take(&mut self.indexed_files);
        let mut stale_ids = BTreeSet::new();
        for (rel_path, entry) in prior_entries {
            if !disk_files.contains_key(&rel_path) {
                stale_ids.extend(entry.owned_node_ids);
            }
        }
        tombstone_node_set(self.store.as_ref(), &self.tenant, &self.writer, &stale_ids)?;
        self.index_incremental()
    }

    /// Indexes added/modified files (detected via `blake3`) and tombstones deleted files (`ING-062`).
    ///
    /// Isolated file parse failures are recorded in [`IndexReport::parse_failures`] without
    /// aborting healthy sibling files (`ING-061`).
    ///
    /// # Errors
    ///
    /// Returns an error if repository walking, configured SCIP loading, or store persistence fails.
    pub fn index_incremental(&mut self) -> Result<IndexReport, CodeNgcError> {
        if !self.config.scip_paths.is_empty() {
            self.scip = ScipIndex::load_paths(&self.config.scip_paths)?;
        }

        let disk_files = collect_repo_files(&self.config.root_dir)?;
        let mut report = IndexReport {
            files_scanned: disk_files.len(),
            ..IndexReport::default()
        };

        // 1. Tombstone deleted files (`ING-062`).
        let deleted_paths: Vec<String> = self
            .indexed_files
            .keys()
            .filter(|path| !disk_files.contains_key(*path))
            .cloned()
            .collect();
        for rel_path in deleted_paths {
            if let Some(entry) = self.indexed_files.remove(&rel_path) {
                tombstone_node_set(
                    self.store.as_ref(),
                    &self.tenant,
                    &self.writer,
                    &entry.owned_node_ids,
                )?;
                report.files_tombstoned += 1;
            }
        }

        // 2. Hash files on disk and re-extract only changed or new files (`ING-062`).
        let mut changed_files: BTreeMap<String, (String, ExtractedFileGraph)> = BTreeMap::new();
        for (rel_path, (lang, abs_path)) in &disk_files {
            let bytes = fs::read(abs_path).map_err(|source| CodeNgcError::Io {
                path: abs_path.display().to_string(),
                source,
            })?;
            let hash = blake3::hash(&bytes).to_hex().to_string();

            if let Some(prev) = self.indexed_files.get(rel_path)
                && prev.content_hash == hash
            {
                report.files_unchanged += 1;
                continue;
            }

            match extract::extract_file(rel_path, *lang, &bytes) {
                Ok(graph) => {
                    changed_files.insert(rel_path.clone(), (hash, graph));
                    report.files_reindexed += 1;
                }
                Err(reason) => {
                    report.parse_failures.push(FileParseFailure {
                        path: rel_path.clone(),
                        language: Some(lang.as_str().to_owned()),
                        reason,
                    });
                }
            }
        }

        // 3. Ensure `Repo` root node is written/up-to-date.
        let repo_id_node = self.repo_node_id()?;
        let mut nodes_to_write = Vec::new();
        nodes_to_write.push(build_repo_node(&self.config.repo_id, &repo_id_node)?);

        if changed_files.is_empty() {
            flush_nodes_and_edges(
                self.store.as_ref(),
                &self.tenant,
                &self.writer,
                &nodes_to_write,
                &[],
            )?;
            return Ok(report);
        }

        // 4. Tombstone symbols/sections removed from modified files, then update `indexed_files`.
        for (rel_path, (hash, graph)) in &changed_files {
            let new_owned = collect_owned_node_ids(&self.config.repo_id, rel_path, graph)?;
            if let Some(prev) = self.indexed_files.get(rel_path) {
                let stale: BTreeSet<NodeId> = prev
                    .owned_node_ids
                    .difference(&new_owned)
                    .cloned()
                    .collect();
                tombstone_node_set(self.store.as_ref(), &self.tenant, &self.writer, &stale)?;
            }
            let file_node_id = file_node_id(&self.config.repo_id, rel_path)?;
            nodes_to_write.push(build_file_node(
                &self.config.repo_id,
                file_node_id,
                rel_path,
                hash,
                graph,
            )?);
            for node in &graph.nodes {
                let node_id = entity_node_id(&self.config.repo_id, rel_path, &node.local_key)?;
                nodes_to_write.push(build_entity_node(
                    &self.config.repo_id,
                    node_id,
                    rel_path,
                    graph.language,
                    graph.grammar_version,
                    node,
                )?);
            }
        }

        // Apply changed entries before building the full repo symbol lookup table.
        for (rel_path, (hash, graph)) in changed_files.iter() {
            let owned_node_ids = collect_owned_node_ids(&self.config.repo_id, rel_path, graph)?;
            self.indexed_files.insert(
                rel_path.clone(),
                FileIndexEntry {
                    content_hash: hash.clone(),
                    owned_node_ids,
                    graph: graph.clone(),
                },
            );
        }

        // 5. Resolve intra-file & cross-file edges over all healthy indexed files.
        let resolution_index = self.build_resolution_index()?;
        let mut pending_edges: BTreeMap<EdgeKey, PendingEdge> = BTreeMap::new();
        let mut external_nodes: BTreeMap<String, NewNode> = BTreeMap::new();

        for (rel_path, entry) in &self.indexed_files {
            self.collect_edges_for_file(
                &repo_id_node,
                rel_path,
                &entry.graph,
                &resolution_index,
                &mut pending_edges,
                &mut external_nodes,
            )?;
        }

        nodes_to_write.extend(external_nodes.into_values());

        let mut edges_to_write = Vec::with_capacity(pending_edges.len());
        for (_, pe) in pending_edges {
            let edge_id = make_edge_id(&format!(
                "edge::{}::{}->{}",
                pe.edge_type,
                pe.from.as_str(),
                pe.to.as_str()
            ))?;
            let mut props = JsonMap::new();
            props.insert(
                String::from("repo"),
                JsonValue::String(self.config.repo_id.clone()),
            );
            edges_to_write.push(NewEdge {
                id: Some(edge_id),
                edge_type: EdgeType::new(pe.edge_type)?,
                from: pe.from,
                to: pe.to,
                props,
                confidence: Some(pe.confidence),
                resolution: pe.resolution,
                provenance: default_provenance(),
            });
        }

        flush_nodes_and_edges(
            self.store.as_ref(),
            &self.tenant,
            &self.writer,
            &nodes_to_write,
            &edges_to_write,
        )?;

        report.nodes_written = nodes_to_write.len();
        report.edges_written = edges_to_write.len();
        Ok(report)
    }

    /// Exports the visible indexed graph for `repo_id` at `as_of` into a deterministic
    /// [`GoldenGraphSnapshot`] (`ING-062`).
    ///
    /// # Errors
    ///
    /// Returns [`CodeNgcError::Store`] if traversal or node retrieval fails.
    pub fn export_golden_snapshot(
        &self,
        as_of: Option<Timestamp>,
    ) -> Result<GoldenGraphSnapshot, CodeNgcError> {
        let root_id = self.repo_node_id()?;
        if self
            .store
            .get_node(&self.tenant, &self.writer, &root_id, as_of)?
            .is_none()
        {
            return Ok(GoldenGraphSnapshot {
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        }

        let mut nodes_by_id: BTreeMap<String, Node> = BTreeMap::new();
        let mut edges_by_key: BTreeMap<EdgeKey, Edge> = BTreeMap::new();
        let mut cursor = None;

        loop {
            let mut query = TraverseQuery::new(root_id.clone());
            query.direction = Direction::Both;
            query.max_depth = 16;
            query.strategy = TraverseStrategy::Bfs;
            query.as_of = as_of;
            query.first = 1_000;
            query.after = cursor.take();

            let page = self.store.traverse(&self.tenant, &self.writer, &query)?;
            for node in page.nodes {
                nodes_by_id.insert(node.id.as_str().to_owned(), node);
            }
            for edge in page.edges {
                let key = EdgeKey {
                    edge_type: edge.edge_type.as_str().to_owned(),
                    from: edge.from.as_str().to_owned(),
                    to: edge.to.as_str().to_owned(),
                };
                edges_by_key.insert(key, edge);
            }
            match page.resume {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        let nodes = nodes_by_id
            .into_values()
            .map(|n| {
                let mut canonical_props = JsonMap::new();
                for (k, v) in n.props {
                    canonical_props.insert(k, v);
                }
                GoldenNode {
                    id: n.id.as_str().to_owned(),
                    node_type: n.node_type.as_str().to_owned(),
                    label: n.label,
                    summary: n.summary,
                    props: canonical_props,
                }
            })
            .collect();

        let edges = edges_by_key
            .into_values()
            .map(|e| GoldenEdge {
                edge_type: e.edge_type.as_str().to_owned(),
                from: e.from.as_str().to_owned(),
                to: e.to.as_str().to_owned(),
                resolution: e.resolution.as_str().to_owned(),
                confidence: e.confidence.unwrap_or(EXACT_EDGE_CONFIDENCE),
            })
            .collect();

        let mut snapshot = GoldenGraphSnapshot { nodes, edges };
        snapshot.canonicalize();
        Ok(snapshot)
    }

    fn build_resolution_index(&self) -> Result<RepoResolutionIndex, CodeNgcError> {
        let mut file_nodes = BTreeMap::new();
        let mut file_by_stem: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        let mut module_by_name: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        let mut symbols_by_name: BTreeMap<String, Vec<(NodeId, String)>> = BTreeMap::new();
        let mut concepts_by_slug = BTreeMap::new();

        for (rel_path, entry) in &self.indexed_files {
            let fid = file_node_id(&self.config.repo_id, rel_path)?;
            file_nodes.insert(rel_path.clone(), fid.clone());

            if let Some(stem) = Path::new(rel_path).file_stem().and_then(|s| s.to_str()) {
                file_by_stem
                    .entry(stem.to_owned())
                    .or_default()
                    .push(fid.clone());
            }

            for node in &entry.graph.nodes {
                let nid = entity_node_id(&self.config.repo_id, rel_path, &node.local_key)?;
                match node.node_type {
                    NODE_MODULE => {
                        module_by_name
                            .entry(node.label.clone())
                            .or_default()
                            .push(nid);
                    }
                    NODE_SYMBOL | NODE_TEST => {
                        symbols_by_name
                            .entry(node.label.clone())
                            .or_default()
                            .push((nid, rel_path.clone()));
                    }
                    NODE_CONCEPT => {
                        concepts_by_slug.insert(node.local_key.clone(), nid.clone());
                        concepts_by_slug.insert(slugify(&node.label), nid);
                    }
                    _ => {}
                }
            }
        }

        Ok(RepoResolutionIndex {
            file_nodes,
            file_by_stem,
            module_by_name,
            symbols_by_name,
            concepts_by_slug,
        })
    }

    fn collect_edges_for_file(
        &self,
        repo_node_id: &NodeId,
        rel_path: &str,
        graph: &ExtractedFileGraph,
        idx: &RepoResolutionIndex,
        pending: &mut BTreeMap<EdgeKey, PendingEdge>,
        external_nodes: &mut BTreeMap<String, NewNode>,
    ) -> Result<(), CodeNgcError> {
        let fid = file_node_id(&self.config.repo_id, rel_path)?;
        insert_edge(
            pending,
            EDGE_CONTAINS,
            repo_node_id.clone(),
            fid.clone(),
            EdgeResolution::Exact,
            EXACT_EDGE_CONFIDENCE,
        );

        let local_key_set: BTreeSet<&str> =
            graph.nodes.iter().map(|n| n.local_key.as_str()).collect();
        let primary_module_id = graph
            .nodes
            .iter()
            .find(|n| n.node_type == NODE_MODULE)
            .map(|n| entity_node_id(&self.config.repo_id, rel_path, &n.local_key))
            .transpose()?;

        for node in &graph.nodes {
            let nid = entity_node_id(&self.config.repo_id, rel_path, &node.local_key)?;

            // Structural hierarchy (`CONTAINS` / `DEFINES`).
            if let Some(parent_key) = &node.parent_local_key
                && local_key_set.contains(parent_key.as_str())
            {
                let pid = entity_node_id(&self.config.repo_id, rel_path, parent_key)?;
                insert_edge(
                    pending,
                    EDGE_CONTAINS,
                    pid,
                    nid.clone(),
                    EdgeResolution::Exact,
                    EXACT_EDGE_CONFIDENCE,
                );
            } else if node.node_type == NODE_MODULE {
                insert_edge(
                    pending,
                    EDGE_DEFINES,
                    fid.clone(),
                    nid.clone(),
                    EdgeResolution::Exact,
                    EXACT_EDGE_CONFIDENCE,
                );
            } else if node.node_type == NODE_SYMBOL || node.node_type == NODE_TEST {
                let container = primary_module_id.as_ref().unwrap_or(&fid);
                insert_edge(
                    pending,
                    EDGE_DEFINES,
                    container.clone(),
                    nid.clone(),
                    EdgeResolution::Exact,
                    EXACT_EDGE_CONFIDENCE,
                );
            } else {
                insert_edge(
                    pending,
                    EDGE_CONTAINS,
                    fid.clone(),
                    nid.clone(),
                    EdgeResolution::Exact,
                    EXACT_EDGE_CONFIDENCE,
                );
            }

            // `CALLS` and `TESTS` (`ING-060`).
            for callee_raw in &node.calls {
                if let Some(target_id) = resolve_symbol_target(idx, rel_path, callee_raw) {
                    if target_id == nid {
                        continue;
                    }
                    let tail = callee_raw
                        .rsplit(['.', ':'])
                        .next()
                        .unwrap_or(callee_raw.as_str());
                    let is_scip_exact = self.scip.confirms_call(&node.label, callee_raw)
                        || self.scip.confirms_call(&node.label, tail);
                    let (resolution, confidence) = if is_scip_exact {
                        (EdgeResolution::Exact, EXACT_EDGE_CONFIDENCE)
                    } else {
                        (EdgeResolution::Heuristic, HEURISTIC_EDGE_CONFIDENCE)
                    };
                    insert_edge(
                        pending,
                        EDGE_CALLS,
                        nid.clone(),
                        target_id.clone(),
                        resolution,
                        confidence,
                    );
                    if node.node_type == NODE_TEST {
                        insert_edge(
                            pending,
                            EDGE_TESTS,
                            nid.clone(),
                            target_id,
                            resolution,
                            confidence,
                        );
                    }
                }
            }

            // `IMPLEMENTS` and `EXTENDS` (`ING-060`).
            for iface in &node.implements {
                if let Some(target_id) = resolve_symbol_target(idx, rel_path, iface) {
                    insert_edge(
                        pending,
                        EDGE_IMPLEMENTS,
                        nid.clone(),
                        target_id,
                        EdgeResolution::Exact,
                        EXACT_EDGE_CONFIDENCE,
                    );
                }
            }
            for base in &node.extends {
                if let Some(target_id) = resolve_symbol_target(idx, rel_path, base) {
                    insert_edge(
                        pending,
                        EDGE_EXTENDS,
                        nid.clone(),
                        target_id,
                        EdgeResolution::Exact,
                        EXACT_EDGE_CONFIDENCE,
                    );
                }
            }
        }

        // File & doc references (`IMPORTS`, `DEPENDS_ON`, `DOCUMENTS`, `REFERENCES`, `SUPERSEDES`).
        for r in &graph.refs {
            let from_id = match &r.from_local_key {
                Some(lk) => entity_node_id(&self.config.repo_id, rel_path, lk)?,
                None => primary_module_id.clone().unwrap_or_else(|| fid.clone()),
            };
            match r.edge_type {
                EDGE_IMPORTS | EDGE_DEPENDS_ON => {
                    if let Some(internal) = resolve_import_target(idx, rel_path, &r.target) {
                        insert_edge(
                            pending,
                            EDGE_IMPORTS,
                            from_id,
                            internal,
                            EdgeResolution::Exact,
                            EXACT_EDGE_CONFIDENCE,
                        );
                    } else if let Some(pkg) = normalize_external_pkg(&r.target) {
                        let ext_id = make_node_id(&format!(
                            "ext::{}::{}",
                            self.config.repo_id,
                            slugify(&pkg)
                        ))?;
                        if !external_nodes.contains_key(ext_id.as_str()) {
                            external_nodes.insert(
                                ext_id.as_str().to_owned(),
                                build_external_module_node(
                                    &self.config.repo_id,
                                    ext_id.clone(),
                                    &pkg,
                                )?,
                            );
                        }
                        insert_edge(
                            pending,
                            EDGE_DEPENDS_ON,
                            from_id,
                            ext_id,
                            EdgeResolution::Exact,
                            EXACT_EDGE_CONFIDENCE,
                        );
                    }
                }
                EDGE_DOCUMENTS => {
                    let target_id = idx
                        .concepts_by_slug
                        .get(&r.target)
                        .or_else(|| idx.concepts_by_slug.get(&slugify(&r.target)))
                        .cloned()
                        .or_else(|| resolve_symbol_target(idx, rel_path, &r.target))
                        .or_else(|| resolve_path_target(idx, rel_path, &r.target));
                    if let Some(to_id) = target_id
                        && to_id != from_id
                    {
                        let (resolution, confidence) = if r.exact_syntax {
                            (EdgeResolution::Exact, EXACT_EDGE_CONFIDENCE)
                        } else {
                            (EdgeResolution::Heuristic, HEURISTIC_EDGE_CONFIDENCE)
                        };
                        insert_edge(
                            pending,
                            EDGE_DOCUMENTS,
                            from_id,
                            to_id,
                            resolution,
                            confidence,
                        );
                    }
                }
                EDGE_REFERENCES | EDGE_SUPERSEDES => {
                    let target_id = resolve_path_target(idx, rel_path, &r.target)
                        .or_else(|| resolve_symbol_target(idx, rel_path, &r.target));
                    if let Some(to_id) = target_id
                        && to_id != from_id
                    {
                        insert_edge(
                            pending,
                            r.edge_type,
                            from_id,
                            to_id,
                            EdgeResolution::Exact,
                            EXACT_EDGE_CONFIDENCE,
                        );
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
}

fn insert_edge(
    pending: &mut BTreeMap<EdgeKey, PendingEdge>,
    edge_type: &str,
    from: NodeId,
    to: NodeId,
    resolution: EdgeResolution,
    confidence: f32,
) {
    let key = EdgeKey {
        edge_type: edge_type.to_owned(),
        from: from.as_str().to_owned(),
        to: to.as_str().to_owned(),
    };
    match pending.get_mut(&key) {
        Some(existing) => {
            if matches!(resolution, EdgeResolution::Exact)
                && !matches!(existing.resolution, EdgeResolution::Exact)
            {
                existing.resolution = EdgeResolution::Exact;
                existing.confidence = confidence.max(existing.confidence);
            }
        }
        None => {
            pending.insert(
                key,
                PendingEdge {
                    edge_type: edge_type.to_owned(),
                    from,
                    to,
                    resolution,
                    confidence,
                },
            );
        }
    }
}

fn default_provenance() -> Provenance {
    Provenance {
        source: String::from(PROVENANCE_SOURCE),
        desc_hash: None,
        confidence: None,
        extracted_at: None,
        agent: Some(String::from(PROVENANCE_AGENT)),
    }
}

fn build_repo_node(repo_id: &str, node_id: &NodeId) -> Result<NewNode, CodeNgcError> {
    let mut props = JsonMap::new();
    props.insert(String::from("repo"), JsonValue::String(repo_id.to_owned()));
    Ok(NewNode {
        id: Some(node_id.clone()),
        node_type: NodeType::new(NODE_REPO)?,
        label: repo_id.to_owned(),
        summary: Some(summarize_text(None, &format!("Repository {repo_id}"))),
        props,
        scope: ScopeSpec::Global,
        provenance: default_provenance(),
    })
}

fn build_file_node(
    repo_id: &str,
    node_id: NodeId,
    rel_path: &str,
    hash: &str,
    graph: &ExtractedFileGraph,
) -> Result<NewNode, CodeNgcError> {
    let mut props = JsonMap::new();
    props.insert(String::from("repo"), JsonValue::String(repo_id.to_owned()));
    props.insert(String::from("file"), JsonValue::String(rel_path.to_owned()));
    props.insert(
        String::from("language"),
        JsonValue::String(graph.language.as_str().to_owned()),
    );
    props.insert(
        String::from("grammar_version"),
        JsonValue::String(graph.grammar_version.to_owned()),
    );
    props.insert(String::from("blake3"), JsonValue::String(hash.to_owned()));

    let summary = summarize_text(None, &format!("{} ({})", rel_path, graph.language.as_str()));
    Ok(NewNode {
        id: Some(node_id),
        node_type: NodeType::new(NODE_FILE)?,
        label: rel_path.to_owned(),
        summary: Some(summary),
        props,
        scope: ScopeSpec::Global,
        provenance: default_provenance(),
    })
}

fn build_entity_node(
    repo_id: &str,
    node_id: NodeId,
    rel_path: &str,
    lang: Language,
    grammar_version: &str,
    node: &ExtractedNode,
) -> Result<NewNode, CodeNgcError> {
    let mut props = JsonMap::new();
    props.insert(String::from("repo"), JsonValue::String(repo_id.to_owned()));
    props.insert(String::from("file"), JsonValue::String(rel_path.to_owned()));
    props.insert(
        String::from("language"),
        JsonValue::String(lang.as_str().to_owned()),
    );
    props.insert(
        String::from("grammar_version"),
        JsonValue::String(grammar_version.to_owned()),
    );
    props.insert(
        String::from("start_line"),
        JsonValue::from(node.start_line as u64),
    );
    props.insert(
        String::from("signature"),
        JsonValue::String(node.signature.clone()),
    );
    if let Some(sk) = node.symbol_kind {
        props.insert(
            String::from("kind"),
            JsonValue::String(sk.as_str().to_owned()),
        );
    }
    if let Some(dk) = node.doc_kind {
        props.insert(
            String::from("doc_kind"),
            JsonValue::String(dk.as_str().to_owned()),
        );
    }

    let summary = summarize_text(node.doc_comment.as_deref(), &node.signature);
    Ok(NewNode {
        id: Some(node_id),
        node_type: NodeType::new(node.node_type)?,
        label: node.label.clone(),
        summary: Some(summary),
        props,
        scope: ScopeSpec::Global,
        provenance: default_provenance(),
    })
}

fn build_external_module_node(
    repo_id: &str,
    node_id: NodeId,
    pkg: &str,
) -> Result<NewNode, CodeNgcError> {
    let mut props = JsonMap::new();
    props.insert(String::from("repo"), JsonValue::String(repo_id.to_owned()));
    props.insert(String::from("external"), json!(true));
    props.insert(String::from("package"), JsonValue::String(pkg.to_owned()));
    Ok(NewNode {
        id: Some(node_id),
        node_type: NodeType::new(NODE_MODULE)?,
        label: pkg.to_owned(),
        summary: Some(summarize_text(None, &format!("External module {pkg}"))),
        props,
        scope: ScopeSpec::Global,
        provenance: default_provenance(),
    })
}

fn collect_owned_node_ids(
    repo_id: &str,
    rel_path: &str,
    graph: &ExtractedFileGraph,
) -> Result<BTreeSet<NodeId>, CodeNgcError> {
    let mut ids = BTreeSet::new();
    ids.insert(file_node_id(repo_id, rel_path)?);
    for node in &graph.nodes {
        ids.insert(entity_node_id(repo_id, rel_path, &node.local_key)?);
    }
    Ok(ids)
}

fn tombstone_node_set<S: ContextStore>(
    store: &S,
    tenant: &TenantScope,
    writer: &Caller,
    ids: &BTreeSet<NodeId>,
) -> Result<(), CodeNgcError> {
    for id in ids {
        match store.tombstone_node(tenant, writer, id) {
            Ok(()) | Err(StoreError::NotFound) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn flush_nodes_and_edges<S: ContextStore>(
    store: &S,
    tenant: &TenantScope,
    writer: &Caller,
    nodes: &[NewNode],
    edges: &[NewEdge],
) -> Result<(), CodeNgcError> {
    for chunk in nodes.chunks(MAX_STORE_BATCH_ITEMS) {
        store.put_batch(tenant, writer, chunk, &[])?;
    }
    for chunk in edges.chunks(MAX_STORE_BATCH_ITEMS) {
        store.put_batch(tenant, writer, &[], chunk)?;
    }
    Ok(())
}

fn file_node_id(repo_id: &str, rel_path: &str) -> Result<NodeId, CodeNgcError> {
    make_node_id(&format!("file::{repo_id}::{rel_path}"))
}

fn entity_node_id(repo_id: &str, rel_path: &str, local_key: &str) -> Result<NodeId, CodeNgcError> {
    make_node_id(&format!("node::{repo_id}::{rel_path}::{local_key}"))
}

fn make_node_id(raw: &str) -> Result<NodeId, CodeNgcError> {
    if raw.len() <= gap_store::types::MAX_NODE_ID_BYTES {
        return Ok(NodeId::new(raw)?);
    }
    let digest = blake3::hash(raw.as_bytes()).to_hex().to_string();
    let keep = gap_store::types::MAX_NODE_ID_BYTES.saturating_sub(digest.len() + 2);
    let mut prefix_end = keep.min(raw.len());
    while prefix_end > 0 && !raw.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let clamped = format!("{}..{}", &raw[..prefix_end], digest);
    Ok(NodeId::new(clamped)?)
}

fn make_edge_id(raw: &str) -> Result<EdgeId, CodeNgcError> {
    if raw.len() <= gap_store::types::MAX_EDGE_ID_BYTES {
        return Ok(EdgeId::new(raw)?);
    }
    let digest = blake3::hash(raw.as_bytes()).to_hex().to_string();
    let keep = gap_store::types::MAX_EDGE_ID_BYTES.saturating_sub(digest.len() + 2);
    let mut prefix_end = keep.min(raw.len());
    while prefix_end > 0 && !raw.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let clamped = format!("{}..{}", &raw[..prefix_end], digest);
    Ok(EdgeId::new(clamped)?)
}

fn collect_repo_files(
    root_dir: &Path,
) -> Result<BTreeMap<String, (Language, PathBuf)>, CodeNgcError> {
    let mut out = BTreeMap::new();
    walk_dir(root_dir, root_dir, &mut out)?;
    Ok(out)
}

fn walk_dir(
    root: &Path,
    current: &Path,
    out: &mut BTreeMap<String, (Language, PathBuf)>,
) -> Result<(), CodeNgcError> {
    let entries = fs::read_dir(current).map_err(|source| CodeNgcError::Io {
        path: current.display().to_string(),
        source,
    })?;
    let mut sorted_paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CodeNgcError::Io {
            path: current.display().to_string(),
            source,
        })?;
        sorted_paths.push(entry.path());
    }
    sorted_paths.sort();

    for path in sorted_paths {
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if path.is_dir() {
            if SKIPPED_DIRS.contains(&file_name) || file_name.starts_with('.') {
                continue;
            }
            walk_dir(root, &path, out)?;
        } else if path.is_file()
            && let Some(lang) = Language::from_path(&path)
        {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_posix = to_posix_rel(rel);
            if !rel_posix.is_empty() {
                out.insert(rel_posix, (lang, path));
            }
        }
    }
    Ok(())
}

fn to_posix_rel(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(os) => os.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn resolve_symbol_target(
    idx: &RepoResolutionIndex,
    caller_rel_path: &str,
    raw_target: &str,
) -> Option<NodeId> {
    let cleaned = raw_target.trim().trim_end_matches("()");
    let tail = cleaned.rsplit(['.', ':']).next().unwrap_or(cleaned);
    for candidate in [cleaned, tail] {
        if let Some(matches) = idx.symbols_by_name.get(candidate) {
            if let Some((same_file_id, _)) = matches.iter().find(|(_, p)| p == caller_rel_path) {
                return Some(same_file_id.clone());
            }
            if let Some((first_id, _)) = matches.first() {
                return Some(first_id.clone());
            }
        }
    }
    None
}

fn resolve_import_target(
    idx: &RepoResolutionIndex,
    caller_rel_path: &str,
    raw_import: &str,
) -> Option<NodeId> {
    if let Some(path_match) = resolve_path_target(idx, caller_rel_path, raw_import) {
        return Some(path_match);
    }
    let norm = raw_import.trim().replace("::", ".");
    let segments: Vec<&str> = norm
        .split(['.', '/'])
        .filter(|s| !s.is_empty() && *s != "crate" && *s != "self" && *s != "super")
        .collect();
    let tail = *segments.last()?;
    if let Some(mod_ids) = idx.module_by_name.get(tail)
        && let Some(first) = mod_ids.first()
    {
        return Some(first.clone());
    }
    if let Some(file_ids) = idx.file_by_stem.get(tail)
        && let Some(first) = file_ids.first()
    {
        return Some(first.clone());
    }
    None
}

fn resolve_path_target(
    idx: &RepoResolutionIndex,
    caller_rel_path: &str,
    raw_ref: &str,
) -> Option<NodeId> {
    let cleaned = raw_ref.trim().trim_start_matches("./");
    if let Some(id) = idx.file_nodes.get(cleaned) {
        return Some(id.clone());
    }
    let parent_dir = Path::new(caller_rel_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let mut stack: Vec<String> = parent_dir
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .collect();
    for part in raw_ref.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other.to_owned()),
        }
    }
    let joined = stack.join("/");
    if let Some(id) = idx.file_nodes.get(&joined) {
        return Some(id.clone());
    }
    // Try appending known source extensions for relative JS/TS/Python imports like `./client`.
    for ext in ["rs", "ts", "tsx", "js", "py", "go", "java", "md"] {
        let with_ext = format!("{joined}.{ext}");
        if let Some(id) = idx.file_nodes.get(&with_ext) {
            return Some(id.clone());
        }
    }
    None
}

fn normalize_external_pkg(raw_import: &str) -> Option<String> {
    let trimmed = raw_import.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.starts_with("crate::")
        || trimmed.starts_with("self::")
        || trimmed.starts_with("super::")
    {
        return None;
    }
    let pkg = trimmed
        .split([':', '/', '.'])
        .find(|s| !s.is_empty())
        .unwrap_or(trimmed);
    Some(pkg.to_owned())
}
