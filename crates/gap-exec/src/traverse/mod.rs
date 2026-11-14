//! Bounded multi-hop graph traversal and `node(ref: NodeRef!)` executor (`WP-M2-03`).
//!
//! Implements `gap.traverse` and `gap.node` against [`gap_store::ContextStore`]:
//! - Verifies and issues HMAC-SHA256 paginated cursors pinned to `(tenant, op_hash,
//!   generation_family, schema_hash)` (`RUN-025`, `INV-09`) and `asOf` (`ING-085`).
//! - Resolves `NodeRef::Id` and `NodeRef::Key` without leaking invisible nodes (`ING-065`, `ING-083`).
//! - Evaluates `@oneOf Predicate` filters (`where` and `through.where`), projects `returnFields`
//!   (`ING-063`), and computes `tokensEstimate = ceil(utf8_bytes / 3.6)` (`GQL-022`).
//! - Surfaces `TraverseWarning::SkippedInvalidWeights` as non-fatal `GAP_PARTIAL_RESULT` warnings
//!   (`ING-067`).

use std::collections::BTreeSet;

use gap_core::clock::Timestamp;
use gap_core::cursor::{CursorBinding, CursorEnvelope, CursorKey, sign_cursor, verify_cursor};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantScope;
use gap_plan::traverse::{
    PlannedContextScope, PlannedDirection, PlannedNodeLookup, PlannedNodeRef, PlannedPredicate,
    PlannedTraverse, PlannedTraverseStrategy,
};
use gap_store::{
    Caller, ContextStore, Direction, Edge, EdgeId, EdgeResolution, EdgeType, Node, NodeId,
    NodeType, Provenance, Scope, StoreError, TraverseQuery, TraverseResume, TraverseStrategy,
    TraverseWarning,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

/// Hard ceiling on store sub-pages fetched while satisfying a `where`-filtered page.
const MAX_PREDICATE_SCAN_ITERATIONS: usize = 64;

/// GraphQL-compatible provenance view (`spec/02-gap-ql.md` `Provenance`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceView {
    /// Origin identifier.
    pub source: String,
    /// Optional content/descriptor digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc_hash: Option<String>,
    /// Optional extractor confidence `[0.0, 1.0]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Optional extraction timestamp (`DateTime`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_at: Option<Timestamp>,
    /// Optional agent/extractor identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl From<&Provenance> for ProvenanceView {
    fn from(value: &Provenance) -> Self {
        Self {
            source: value.source.clone(),
            desc_hash: value.desc_hash.clone(),
            confidence: value.confidence,
            extracted_at: value.extracted_at,
            agent: value.agent.clone(),
        }
    }
}

/// GraphQL `GapNode` view (`spec/02-gap-ql.md` `GapNode`, `GQL-010`, `GQL-043`, `ING-063`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GapNodeView {
    /// Node identifier (`ID!`).
    pub id: String,
    /// Node category/type (`type: String!`).
    #[serde(rename = "type")]
    pub node_type: String,
    /// Human/agent-readable label (`label: String!`).
    pub label: String,
    /// Deterministic short summary (`ING-063`, optional additive field per `GQL-010`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Full or `returnFields`-projected JSON properties (`props: JSON`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub props: Option<JsonValue>,
    /// Provenance metadata (`provenance: Provenance`).
    pub provenance: ProvenanceView,
    /// Inclusive validity start (`validFrom: DateTime`).
    pub valid_from: Timestamp,
    /// Exclusive validity end (`validTo: DateTime`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<Timestamp>,
    /// Reported visibility scope (`scope: ContextScope`, returned for visible nodes only per `GQL-043`).
    pub scope: PlannedContextScope,
}

/// GraphQL `GapEdge` view (`spec/02-gap-ql.md` `GapEdge`, `GQL-010`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GapEdgeView {
    /// Edge identifier (`ID!`).
    pub id: String,
    /// Edge relationship type (`type: String!`).
    #[serde(rename = "type")]
    pub edge_type: String,
    /// Source node ID (`from: ID!`).
    pub from: String,
    /// Target node ID (`to: ID!`).
    pub to: String,
    /// Edge JSON properties (`props: JSON`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub props: Option<JsonValue>,
    /// Optional confidence (`confidence: Float`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Exact vs heuristic resolution (`resolution: EdgeResolution!`).
    pub resolution: String,
    /// Provenance metadata (`provenance: Provenance`).
    pub provenance: ProvenanceView,
}

/// GraphQL `PageInfo` (`spec/02-gap-ql.md` `PageInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfoView {
    /// Whether additional pages exist beyond `endCursor`.
    pub has_next_page: bool,
    /// Signed opaque continuation cursor (`RUN-025`, `ING-085`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_cursor: Option<String>,
}

/// GraphQL `TraverseStats` (`spec/02-gap-ql.md` `TraverseStats`, `GQL-022`, `ING-083`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraverseStatsView {
    /// Visible visited node count (including skipped cursor prefix, `ING-083`).
    pub visited: u32,
    /// Returned visible node count in this page.
    pub returned: u32,
    /// Maximum hop depth reached among visited visible nodes.
    pub depth_reached: u32,
    /// Tokenizer-agnostic payload estimate `ceil(utf8_bytes / 3.6)` (`GQL-022`).
    pub tokens_estimate: u32,
}

/// GraphQL `Subgraph` payload (`spec/02-gap-ql.md` `Subgraph`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubgraphView {
    /// Ordered visible nodes matching `TraverseStrategy`.
    pub nodes: Vec<GapNodeView>,
    /// Traversed visible edges whose endpoints are both in `nodes`.
    pub edges: Vec<GapEdgeView>,
    /// Pagination cursor state.
    pub page_info: PageInfoView,
    /// `true` iff `visited_cap` stopped exploration while reachable visible nodes remained.
    pub truncated: bool,
    /// Traversal statistics including `tokensEstimate`.
    pub stats: TraverseStatsView,
}

/// Non-fatal traversal warning emitted into `extensions.gap.warnings` (`ING-067`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecTraverseWarning {
    /// Stable GAP error/warning code (e.g. `GAP_PARTIAL_RESULT`).
    pub code: ErrorCode,
    /// Human-readable sanitized warning summary (`<= 300` chars).
    pub message: String,
    /// Count of affected items (e.g. skipped invalid-weight edges).
    pub count: u32,
}

/// Full execution result for `gap.traverse` (`WP-M2-03`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraverseExecResult {
    /// Projected GraphQL `Subgraph`.
    pub subgraph: SubgraphView,
    /// Resolved point-in-time snapshot instant pinned into any emitted cursor (`ING-085`).
    pub as_of: Timestamp,
    /// Non-fatal warnings surfaced during execution (`ING-067`).
    pub warnings: Vec<ExecTraverseWarning>,
}

/// Execution context carrying cursor signing key, request binding, and wall-clock time.
pub struct TraverseExecContext<'a> {
    /// HMAC-SHA256 cursor signing/verification key (`RUN-025`).
    pub cursor_key: &'a CursorKey,
    /// Expected binding `(tenant, op_hash, generation_family, generation, schema_hash)` (`RUN-025`, `INV-09`).
    pub binding: &'a CursorBinding,
    /// Issuance timestamp recorded on newly minted pagination cursors.
    pub now: Timestamp,
    /// Sanitized correlation identifier attached to mapped store errors (`INV-15`).
    pub correlation_id: &'a str,
}

/// Tokenizer-agnostic token estimator `ceil(utf8_bytes / 3.6)` (`GQL-022`).
#[must_use]
pub fn estimate_tokens(utf8_bytes: usize) -> u32 {
    if utf8_bytes == 0 {
        return 0;
    }
    let numerator = (utf8_bytes as u128).saturating_mul(10);
    let tokens = numerator.div_ceil(36);
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

/// Executes `gap.node(ref: NodeRef!, ...)` against `store` (`GQL-010`, `ING-065`, `ING-083`).
///
/// Missing or caller-invisible nodes deterministically return `Ok(None)` without leaking
/// existence or hidden attributes (`ING-065`, `ING-083`).
pub fn execute_node_lookup(
    store: &dyn ContextStore,
    tenant: &TenantScope,
    caller: &Caller,
    plan: &PlannedNodeLookup,
    correlation_id: &str,
) -> GapResult<Option<GapNodeView>> {
    let maybe_node = match &plan.reference {
        PlannedNodeRef::Id(raw_id) => {
            let node_id = parse_node_id(raw_id)?;
            store
                .get_node(tenant, caller, &node_id, plan.as_of)
                .map_err(|e| map_store_error(e, correlation_id))?
        }
        PlannedNodeRef::Key { node_type, value } => resolve_node_by_key(
            store,
            tenant,
            caller,
            node_type,
            value,
            plan.as_of,
            correlation_id,
        )?,
    };

    Ok(maybe_node.map(|n| node_to_view(&n, plan.return_fields.as_deref())))
}

/// Executes `gap.traverse(input: TraverseInput!)` against `store` (`GQL-040`, `GQL-043`,
/// `ING-063`, `ING-067`, `ING-083`, `ING-084`, `ING-085`, `RUN-025`).
pub fn execute_traverse(
    store: &dyn ContextStore,
    tenant: &TenantScope,
    caller: &Caller,
    plan: &PlannedTraverse,
    ctx: &TraverseExecContext<'_>,
) -> GapResult<TraverseExecResult> {
    if plan.binding_mismatch_check(tenant, ctx.binding) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "traversal cursor binding tenant does not match caller tenant",
        ));
    }

    // 1. Verify signed `after` cursor if provided (`RUN-025`, `ING-085`, `INV-09`).
    let (decoded_resume, effective_as_of) = match &plan.after {
        Some(token) => {
            let env: CursorEnvelope<TraverseResume> =
                verify_cursor(ctx.cursor_key, token, ctx.binding)?;
            let cursor_as_of = env.as_of_timestamp()?;
            if let (Some(explicit), Some(pinned)) = (plan.as_of, cursor_as_of)
                && explicit != pinned
            {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`TraverseInput.asOf` does not match cursor snapshot timestamp",
                ));
            }
            (Some(env.payload), cursor_as_of.or(plan.as_of))
        }
        None => (None, plan.as_of),
    };

    // 2. Resolve start node reference (`ID` or `NodeKey`).
    let start_id = match &plan.from {
        PlannedNodeRef::Id(raw_id) => parse_node_id(raw_id)?,
        PlannedNodeRef::Key { node_type, value } => {
            match resolve_node_by_key(
                store,
                tenant,
                caller,
                node_type,
                value,
                effective_as_of,
                ctx.correlation_id,
            )? {
                Some(node) => node.id,
                None => {
                    // Missing or invisible start node yields empty subgraph (`ING-065`, `ING-083`).
                    let resolved_ts = effective_as_of.unwrap_or(ctx.now);
                    return Ok(empty_traverse_result(resolved_ts));
                }
            }
        }
    };

    let edge_types = plan
        .through
        .as_ref()
        .and_then(|t| t.types.as_ref())
        .map(|list| {
            list.iter()
                .map(EdgeType::new)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(|e| map_store_error(e, ctx.correlation_id))?;

    let scopes = plan
        .scopes
        .as_ref()
        .map(|list| list.iter().map(|s| map_to_store_scope(*s)).collect());

    let direction = match plan.direction {
        PlannedDirection::Outbound => Direction::Outbound,
        PlannedDirection::Inbound => Direction::Inbound,
        PlannedDirection::Both => Direction::Both,
    };

    let strategy = match plan.strategy {
        PlannedTraverseStrategy::Bfs => TraverseStrategy::Bfs,
        PlannedTraverseStrategy::Dfs => TraverseStrategy::Dfs,
        PlannedTraverseStrategy::Shortest => TraverseStrategy::Shortest,
        PlannedTraverseStrategy::Weighted => TraverseStrategy::Weighted,
    };

    // 3. Execute store walk; when `where` is present, iterate bounded sub-pages with `first = remaining`
    //    so filtered pagination fills up to `plan.first` matching nodes deterministically with exact
    //    `TraverseResume` boundaries and pinned `as_of`.
    let has_node_predicate = plan.where_node.is_some();
    let edge_predicate = plan.through.as_ref().and_then(|t| t.where_edge.as_ref());

    let mut collected_nodes: Vec<Node> = Vec::new();
    let mut collected_node_ids: BTreeSet<String> = BTreeSet::new();
    let mut candidate_edges: Vec<Edge> = Vec::new();
    let mut seen_edge_ids: BTreeSet<EdgeId> = BTreeSet::new();

    let mut current_after = decoded_resume;
    let mut pinned_as_of = effective_as_of;
    let mut last_resume: Option<TraverseResume> = None;
    let mut truncated = false;
    let mut max_visited: u32 = 0;
    let mut max_depth_reached: u32 = 0;
    let mut invalid_weights_count: u32 = 0;

    let max_iterations = if has_node_predicate {
        MAX_PREDICATE_SCAN_ITERATIONS
    } else {
        1
    };

    for _ in 0..max_iterations {
        let remaining = (plan.first as usize).saturating_sub(collected_nodes.len());
        if remaining == 0 {
            break;
        }

        let query = TraverseQuery {
            from: start_id.clone(),
            edge_types: edge_types.clone(),
            direction,
            max_depth: plan.max_depth,
            strategy,
            as_of: pinned_as_of,
            scopes: scopes.clone(),
            first: remaining as u32,
            visited_cap: Some(plan.visited_cap),
            after: current_after.clone(),
        };

        let page = store
            .traverse(tenant, caller, &query)
            .map_err(|e| map_store_error(e, ctx.correlation_id))?;

        pinned_as_of.get_or_insert(page.as_of);
        max_visited = max_visited.max(page.stats.visited);
        max_depth_reached = max_depth_reached.max(page.stats.depth_reached);
        truncated |= page.truncated;

        for warning in &page.warnings {
            match warning {
                TraverseWarning::SkippedInvalidWeights { count } => {
                    invalid_weights_count = invalid_weights_count.max(*count);
                }
            }
        }

        for node in page.nodes {
            let keep = match &plan.where_node {
                Some(pred) => evaluate_node_predicate(pred, &node),
                None => true,
            };
            if keep && collected_node_ids.insert(node.id.as_str().to_owned()) {
                collected_nodes.push(node);
            }
        }

        for edge in page.edges {
            if let Some(ep) = edge_predicate
                && !evaluate_edge_predicate(ep, &edge)
            {
                continue;
            }
            if seen_edge_ids.insert(edge.id.clone()) {
                candidate_edges.push(edge);
            }
        }

        last_resume = page.resume.clone();
        if last_resume.is_none() || collected_nodes.len() >= plan.first as usize {
            break;
        }
        current_after = last_resume.clone();
    }

    let resolved_as_of = pinned_as_of.unwrap_or(ctx.now);

    // Preserve visible endpoint invariant (`ING-083`, `ING-084`): every edge must have both endpoints
    // present in `collected_nodes`.
    let node_views: Vec<GapNodeView> = collected_nodes
        .iter()
        .map(|n| node_to_view(n, plan.return_fields.as_deref()))
        .collect();

    let edge_views: Vec<GapEdgeView> = candidate_edges
        .iter()
        .filter(|e| {
            collected_node_ids.contains(e.from.as_str())
                && collected_node_ids.contains(e.to.as_str())
        })
        .map(edge_to_view)
        .collect();

    let has_next_page = last_resume.is_some();
    let end_cursor = match last_resume {
        Some(resume) => {
            let env = CursorEnvelope::new(ctx.binding, Some(resolved_as_of), ctx.now, resume)?;
            Some(sign_cursor(ctx.cursor_key, &env)?)
        }
        None => None,
    };

    let mut warnings = Vec::new();
    if invalid_weights_count > 0 {
        warnings.push(ExecTraverseWarning {
            code: ErrorCode::PartialResult,
            message: format!(
                "skipped {invalid_weights_count} edge(s) with invalid negative/NaN/infinite/non-numeric weight"
            ),
            count: invalid_weights_count,
        });
    }

    let returned = u32::try_from(node_views.len()).unwrap_or(u32::MAX);
    let page_info = PageInfoView {
        has_next_page,
        end_cursor,
    };

    let tokens_estimate = estimate_subgraph_tokens(&node_views, &edge_views);
    let stats = TraverseStatsView {
        visited: max_visited,
        returned,
        depth_reached: max_depth_reached,
        tokens_estimate,
    };

    Ok(TraverseExecResult {
        subgraph: SubgraphView {
            nodes: node_views,
            edges: edge_views,
            page_info,
            truncated,
            stats,
        },
        as_of: resolved_as_of,
        warnings,
    })
}

trait PlanTenantExt {
    fn binding_mismatch_check(&self, tenant: &TenantScope, binding: &CursorBinding) -> bool;
}

impl PlanTenantExt for PlannedTraverse {
    fn binding_mismatch_check(&self, tenant: &TenantScope, binding: &CursorBinding) -> bool {
        tenant.tenant().as_str() != binding.tenant.as_str()
    }
}

fn empty_traverse_result(resolved_ts: Timestamp) -> TraverseExecResult {
    TraverseExecResult {
        subgraph: SubgraphView {
            nodes: Vec::new(),
            edges: Vec::new(),
            page_info: PageInfoView {
                has_next_page: false,
                end_cursor: None,
            },
            truncated: false,
            stats: TraverseStatsView {
                visited: 0,
                returned: 0,
                depth_reached: 0,
                tokens_estimate: 0,
            },
        },
        as_of: resolved_ts,
        warnings: Vec::new(),
    }
}

fn parse_node_id(raw: &str) -> GapResult<NodeId> {
    NodeId::new(raw).map_err(|_| {
        GapError::new(
            ErrorCode::ValidationError,
            "invalid NodeRef identifier format",
        )
    })
}

fn map_store_error(err: StoreError, correlation_id: &str) -> GapError {
    err.into_gap_error(correlation_id)
}

fn map_to_store_scope(scope: PlannedContextScope) -> Scope {
    match scope {
        PlannedContextScope::Private => Scope::Private,
        PlannedContextScope::Task => Scope::Task,
        PlannedContextScope::Group => Scope::Group,
        PlannedContextScope::Global => Scope::Global,
        PlannedContextScope::Ephemeral => Scope::Ephemeral,
    }
}

fn map_from_store_scope(scope: Scope) -> PlannedContextScope {
    match scope {
        Scope::Private => PlannedContextScope::Private,
        Scope::Task => PlannedContextScope::Task,
        Scope::Group => PlannedContextScope::Group,
        Scope::Global => PlannedContextScope::Global,
        Scope::Ephemeral => PlannedContextScope::Ephemeral,
    }
}

/// Resolves a `NodeKey { type, value }` via direct ID candidate or scoped FTS lookup without leaking
/// caller-invisible nodes (`ING-065`, `ING-083`).
fn resolve_node_by_key(
    store: &dyn ContextStore,
    tenant: &TenantScope,
    caller: &Caller,
    raw_type: &str,
    key_value: &JsonValue,
    as_of: Option<Timestamp>,
    correlation_id: &str,
) -> GapResult<Option<Node>> {
    let expected_type = NodeType::new(raw_type).map_err(|e| map_store_error(e, correlation_id))?;

    // Fast path 1: direct ID inside string or `{"id": "..."}` value.
    let direct_id = match key_value {
        JsonValue::String(s) => Some(s.as_str()),
        JsonValue::Object(map) => map.get("id").and_then(JsonValue::as_str),
        _ => None,
    };
    if let Some(candidate) = direct_id
        && let Ok(node_id) = NodeId::new(candidate)
        && let Some(node) = store
            .get_node(tenant, caller, &node_id, as_of)
            .map_err(|e| map_store_error(e, correlation_id))?
        && node.node_type == expected_type
    {
        return Ok(Some(node));
    }

    // Fast path 2: extract search string and verify exact visible match on `node_type` and key/label.
    let mut query_terms: Vec<String> = Vec::new();
    collect_scalar_tokens(key_value, &mut query_terms);
    if query_terms.is_empty() {
        return Ok(None);
    }
    let query_str = query_terms.join(" ");
    let mut hits = store
        .search_nodes(tenant, caller, &query_str, 100, as_of)
        .map_err(|e| map_store_error(e, correlation_id))?;

    hits.retain(|n| {
        if n.node_type != expected_type {
            return false;
        }
        if n.props.get("key") == Some(key_value) {
            return true;
        }
        match key_value {
            JsonValue::String(s) => n.label == *s || n.id.as_str() == s.as_str(),
            JsonValue::Object(map) => {
                let label_match = map
                    .get("label")
                    .and_then(JsonValue::as_str)
                    .is_some_and(|l| l == n.label);
                let name_match = map
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .is_some_and(|name| {
                        n.props.get("name").and_then(JsonValue::as_str) == Some(name)
                            || n.label == name
                    });
                let key_match = map
                    .get("key")
                    .is_some_and(|k| n.props.get("key") == Some(k));
                label_match || name_match || key_match
            }
            _ => false,
        }
    });

    hits.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    Ok(hits.into_iter().next())
}

fn collect_scalar_tokens(val: &JsonValue, out: &mut Vec<String>) {
    match val {
        JsonValue::String(s) if !s.is_empty() => out.push(s.clone()),
        JsonValue::Number(n) => out.push(n.to_string()),
        JsonValue::Object(map) => {
            for v in map.values() {
                collect_scalar_tokens(v, out);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr {
                collect_scalar_tokens(v, out);
            }
        }
        _ => {}
    }
}

fn node_to_view(node: &Node, return_fields: Option<&[String]>) -> GapNodeView {
    GapNodeView {
        id: node.id.as_str().to_owned(),
        node_type: node.node_type.as_str().to_owned(),
        label: node.label.clone(),
        summary: node.summary.clone(),
        props: Some(project_props(&node.props, return_fields)),
        provenance: ProvenanceView::from(&node.provenance),
        valid_from: node.valid_from,
        valid_to: node.valid_to,
        scope: map_from_store_scope(node.scope),
    }
}

fn edge_to_view(edge: &Edge) -> GapEdgeView {
    let resolution = match edge.resolution {
        EdgeResolution::Exact => "EXACT",
        EdgeResolution::Heuristic => "HEURISTIC",
    };
    GapEdgeView {
        id: edge.id.as_str().to_owned(),
        edge_type: edge.edge_type.as_str().to_owned(),
        from: edge.from.as_str().to_owned(),
        to: edge.to.as_str().to_owned(),
        props: Some(JsonValue::Object(edge.props.clone())),
        confidence: edge.confidence,
        resolution: resolution.to_owned(),
        provenance: ProvenanceView::from(&edge.provenance),
    }
}

/// Projects `props` down to only requested `returnFields` dotted paths (`ING-063`).
#[must_use]
pub fn project_props(
    props: &JsonMap<String, JsonValue>,
    return_fields: Option<&[String]>,
) -> JsonValue {
    let Some(fields) = return_fields else {
        return JsonValue::Object(props.clone());
    };
    let mut out = JsonMap::new();
    for path in fields {
        let segments: Vec<&str> = path.split('.').collect();
        if let Some(found) = resolve_json_map_path(props, &segments) {
            insert_projected_path(&mut out, &segments, found.clone());
        }
    }
    JsonValue::Object(out)
}

fn resolve_json_map_path<'a>(
    map: &'a JsonMap<String, JsonValue>,
    segments: &[&str],
) -> Option<&'a JsonValue> {
    let (&first, rest) = segments.split_first()?;
    let mut cur = map.get(first)?;
    for seg in rest {
        cur = cur.as_object()?.get(*seg)?;
    }
    Some(cur)
}

fn insert_projected_path(out: &mut JsonMap<String, JsonValue>, segments: &[&str], val: JsonValue) {
    let Some((&first, rest)) = segments.split_first() else {
        return;
    };
    if rest.is_empty() {
        out.insert(first.to_owned(), val);
        return;
    }
    let entry = out
        .entry(first.to_owned())
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    if let JsonValue::Object(child_map) = entry {
        insert_projected_path(child_map, rest, val);
    }
}

fn estimate_subgraph_tokens(nodes: &[GapNodeView], edges: &[GapEdgeView]) -> u32 {
    let node_bytes = serde_json::to_vec(nodes).map_or(0, |v| v.len());
    let edge_bytes = serde_json::to_vec(edges).map_or(0, |v| v.len());
    estimate_tokens(node_bytes.saturating_add(edge_bytes))
}

/// Evaluates a validated [`PlannedPredicate`] against a visible [`Node`].
#[must_use]
pub fn evaluate_node_predicate(pred: &PlannedPredicate, node: &Node) -> bool {
    eval_pred_with_lookup(pred, &|path| resolve_node_path(node, path))
}

/// Evaluates a validated [`PlannedPredicate`] against a visible [`Edge`].
#[must_use]
pub fn evaluate_edge_predicate(pred: &PlannedPredicate, edge: &Edge) -> bool {
    eval_pred_with_lookup(pred, &|path| resolve_edge_path(edge, path))
}

fn eval_pred_with_lookup<F>(pred: &PlannedPredicate, lookup: &F) -> bool
where
    F: Fn(&str) -> Option<JsonValue>,
{
    match pred {
        PlannedPredicate::And(clauses) => clauses.iter().all(|c| eval_pred_with_lookup(c, lookup)),
        PlannedPredicate::Or(clauses) => clauses.iter().any(|c| eval_pred_with_lookup(c, lookup)),
        PlannedPredicate::Not(inner) => !eval_pred_with_lookup(inner, lookup),
        PlannedPredicate::Eq(cmp) => {
            lookup(&cmp.path).is_some_and(|v| json_values_eq(&v, &cmp.value))
        }
        PlannedPredicate::Ne(cmp) => {
            lookup(&cmp.path).is_some_and(|v| !json_values_eq(&v, &cmp.value))
        }
        PlannedPredicate::Gt(cmp) => lookup(&cmp.path)
            .and_then(|v| compare_json_ord(&v, &cmp.value))
            .is_some_and(|o| o.is_gt()),
        PlannedPredicate::Gte(cmp) => lookup(&cmp.path)
            .and_then(|v| compare_json_ord(&v, &cmp.value))
            .is_some_and(|o| o.is_ge()),
        PlannedPredicate::Lt(cmp) => lookup(&cmp.path)
            .and_then(|v| compare_json_ord(&v, &cmp.value))
            .is_some_and(|o| o.is_lt()),
        PlannedPredicate::Lte(cmp) => lookup(&cmp.path)
            .and_then(|v| compare_json_ord(&v, &cmp.value))
            .is_some_and(|o| o.is_le()),
        PlannedPredicate::In(list) => lookup(&list.path)
            .is_some_and(|v| list.values.iter().any(|item| json_values_eq(&v, item))),
        PlannedPredicate::Contains(cmp) => {
            lookup(&cmp.path).is_some_and(|v| match (&v, &cmp.value) {
                (JsonValue::String(hay), JsonValue::String(needle)) => {
                    hay.contains(needle.as_str())
                }
                (JsonValue::Array(items), needle) => {
                    items.iter().any(|x| json_values_eq(x, needle))
                }
                _ => false,
            })
        }
        PlannedPredicate::StartsWith(cmp) => {
            lookup(&cmp.path).is_some_and(|v| match (v.as_str(), cmp.value.as_str()) {
                (Some(hay), Some(prefix)) => hay.starts_with(prefix),
                _ => false,
            })
        }
        PlannedPredicate::Exists(path) => lookup(path).is_some_and(|v| !v.is_null()),
    }
}

fn resolve_node_path(node: &Node, path: &str) -> Option<JsonValue> {
    let trimmed = path.strip_prefix("props.").unwrap_or(path);
    let segments: Vec<&str> = trimmed.split('.').collect();
    if let Some(found) = resolve_json_map_path(&node.props, &segments) {
        return Some(found.clone());
    }
    if path.starts_with("props.") {
        return None;
    }
    match path {
        "id" => Some(JsonValue::String(node.id.as_str().to_owned())),
        "type" => Some(JsonValue::String(node.node_type.as_str().to_owned())),
        "label" => Some(JsonValue::String(node.label.clone())),
        "summary" => node.summary.as_ref().map(|s| JsonValue::String(s.clone())),
        "scope" => Some(JsonValue::String(node.scope.as_str().to_owned())),
        _ => None,
    }
}

fn resolve_edge_path(edge: &Edge, path: &str) -> Option<JsonValue> {
    let trimmed = path.strip_prefix("props.").unwrap_or(path);
    let segments: Vec<&str> = trimmed.split('.').collect();
    if let Some(found) = resolve_json_map_path(&edge.props, &segments) {
        return Some(found.clone());
    }
    if path.starts_with("props.") {
        return None;
    }
    match path {
        "id" => Some(JsonValue::String(edge.id.as_str().to_owned())),
        "type" => Some(JsonValue::String(edge.edge_type.as_str().to_owned())),
        "from" => Some(JsonValue::String(edge.from.as_str().to_owned())),
        "to" => Some(JsonValue::String(edge.to.as_str().to_owned())),
        "resolution" => Some(JsonValue::String(edge.resolution.as_str().to_owned())),
        "confidence" => edge
            .confidence
            .and_then(|c| serde_json::Number::from_f64(f64::from(c)))
            .map(JsonValue::Number),
        _ => None,
    }
}

fn json_values_eq(lhs: &JsonValue, rhs: &JsonValue) -> bool {
    if lhs == rhs {
        return true;
    }
    if let (Some(a), Some(b)) = (lhs.as_f64(), rhs.as_f64())
        && a.is_finite()
        && b.is_finite()
    {
        return a == b;
    }
    false
}

fn compare_json_ord(lhs: &JsonValue, rhs: &JsonValue) -> Option<std::cmp::Ordering> {
    match (lhs, rhs) {
        (JsonValue::Number(a), JsonValue::Number(b)) => {
            let fa = a.as_f64()?;
            let fb = b.as_f64()?;
            if !fa.is_finite() || !fb.is_finite() {
                return None;
            }
            fa.partial_cmp(&fb)
        }
        (JsonValue::String(a), JsonValue::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use gap_core::clock::{Clock, ManualClock};
    use gap_core::ids::{TaskId, TenantId};
    use gap_plan::ir::ArgValue;
    use gap_plan::traverse::{
        TraversePlanLimits, validate_and_plan_node_lookup, validate_and_plan_traverse,
    };
    use gap_store::{NewEdge, NewNode, ScopeSpec, SqliteConfig, SqliteContextStore};

    use super::*;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

    struct TempDb {
        dir: PathBuf,
        store: SqliteContextStore,
        clock: Arc<ManualClock>,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn open_test_store(start_millis: u64) -> TempDb {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gap-exec-traverse-test-{}-{}",
            std::process::id(),
            seq
        ));
        let _ = std::fs::create_dir_all(&dir);
        let db_path = dir.join("context.sqlite");
        let clock = Arc::new(ManualClock::new(start_millis));
        let store =
            SqliteContextStore::open(SqliteConfig::new(&db_path), clock.clone()).expect("open db");
        TempDb { dir, store, clock }
    }

    fn mk_node(id: &str, label: &str, props: JsonValue, scope: ScopeSpec) -> NewNode {
        let mut map = JsonMap::new();
        if let JsonValue::Object(obj) = props {
            map = obj;
        }
        NewNode {
            id: Some(NodeId::new(id).expect("node id")),
            node_type: NodeType::new("Symbol").expect("node type"),
            label: label.to_owned(),
            summary: Some(format!("summary of {label}")),
            props: map,
            scope,
            provenance: Provenance::new("rust-analyzer"),
        }
    }

    fn mk_edge(id: &str, from: &str, to: &str, props: JsonValue) -> NewEdge {
        let mut map = JsonMap::new();
        if let JsonValue::Object(obj) = props {
            map = obj;
        }
        NewEdge {
            id: Some(EdgeId::new(id).expect("edge id")),
            edge_type: EdgeType::new("CALLS").expect("edge type"),
            from: NodeId::new(from).expect("from id"),
            to: NodeId::new(to).expect("to id"),
            props: map,
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: Provenance::new("rust-analyzer"),
        }
    }

    fn cursor_setup(
        tenant_str: &str,
        schema_hash: &str,
    ) -> (TenantScope, CursorKey, CursorBinding) {
        let tenant = TenantId::new(tenant_str).expect("tenant");
        let scope = TenantScope::new(tenant.clone());
        let key = CursorKey::from_array([0xAA_u8; 32]);
        let binding = CursorBinding {
            tenant,
            op_hash: "op-traverse-test".to_owned(),
            generation_family: "gen-main".to_owned(),
            generation: 1,
            schema_hash: schema_hash.to_owned(),
        };
        (scope, key, binding)
    }

    fn plan_traverse_json(input: JsonValue) -> PlannedTraverse {
        let mut args = BTreeMap::new();
        args.insert("input".to_owned(), ArgValue::Literal(input));
        validate_and_plan_traverse(
            "traverse",
            &args,
            &[],
            &BTreeMap::new(),
            TraversePlanLimits::default(),
        )
        .expect("plan traverse")
    }

    // spec: GQL-022, ING-063
    #[test]
    fn traverse_estimates_tokens_and_projects_return_fields() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(36), 10);
        assert_eq!(estimate_tokens(37), 11);

        let db = open_test_store(1_700_000_000_000);
        let (tenant, key, mut binding) = cursor_setup("acme", "");
        let writer = Caller::new("alice");

        let large_body = "x".repeat(4_000);
        for (id, label) in [("n0", "root"), ("n1", "callee_a"), ("n2", "callee_b")] {
            db.store
                .put_node(
                    &tenant,
                    &writer,
                    mk_node(
                        id,
                        label,
                        serde_json::json!({
                            "signature": format!("fn {label}() -> ()"),
                            "meta": { "module": "core" },
                            "sourceBody": large_body,
                        }),
                        ScopeSpec::Global,
                    ),
                )
                .expect("put node");
        }
        db.store
            .put_edge(
                &tenant,
                &writer,
                mk_edge("e1", "n0", "n1", serde_json::json!({})),
            )
            .expect("e1");
        db.store
            .put_edge(
                &tenant,
                &writer,
                mk_edge("e2", "n0", "n2", serde_json::json!({})),
            )
            .expect("e2");

        let full_plan = plan_traverse_json(serde_json::json!({
            "from": { "id": "n0" },
            "maxDepth": 2,
        }));
        binding.schema_hash.clone_from(&full_plan.schema_hash);
        let ctx = TraverseExecContext {
            cursor_key: &key,
            binding: &binding,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-full",
        };
        let full_res =
            execute_traverse(&db.store, &tenant, &writer, &full_plan, &ctx).expect("full traverse");

        let slice_plan = plan_traverse_json(serde_json::json!({
            "from": { "id": "n0" },
            "maxDepth": 2,
            "returnFields": ["signature", "meta.module"],
        }));
        binding.schema_hash.clone_from(&slice_plan.schema_hash);
        let slice_ctx = TraverseExecContext {
            cursor_key: &key,
            binding: &binding,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-slice",
        };
        let slice_res = execute_traverse(&db.store, &tenant, &writer, &slice_plan, &slice_ctx)
            .expect("slice traverse");

        assert_eq!(slice_res.subgraph.nodes.len(), 3);
        let first_props = slice_res.subgraph.nodes[0].props.as_ref().expect("props");
        assert_eq!(
            first_props.get("signature").and_then(JsonValue::as_str),
            Some("fn root() -> ()")
        );
        assert!(first_props.get("sourceBody").is_none());
        assert_eq!(
            first_props
                .get("meta")
                .and_then(|m| m.get("module"))
                .and_then(JsonValue::as_str),
            Some("core")
        );
        // Token reduction gate (`G-C1` <= 15%).
        assert!(
            slice_res.subgraph.stats.tokens_estimate * 100
                <= full_res.subgraph.stats.tokens_estimate * 15,
            "slice={} full={}",
            slice_res.subgraph.stats.tokens_estimate,
            full_res.subgraph.stats.tokens_estimate
        );
    }

    // spec: ING-085, RUN-025
    #[test]
    fn traverse_cursor_pins_as_of_across_temporal_mutations_without_duplicates() {
        let db = open_test_store(1_700_000_000_000);
        let (tenant, key, mut binding) = cursor_setup("acme", "");
        let writer = Caller::new("alice");

        for id in ["n0", "n1", "n2", "n3"] {
            db.store
                .put_node(
                    &tenant,
                    &writer,
                    mk_node(id, id, serde_json::json!({"ver": 1}), ScopeSpec::Global),
                )
                .expect("put");
        }
        for (eid, to) in [("e1", "n1"), ("e2", "n2"), ("e3", "n3")] {
            db.store
                .put_edge(
                    &tenant,
                    &writer,
                    mk_edge(eid, "n0", to, serde_json::json!({})),
                )
                .expect("edge");
        }

        // Request page 1 with `first = 2` (omits explicit `asOf`; resolver captures store `as_of`).
        let page1_plan = plan_traverse_json(serde_json::json!({
            "from": { "id": "n0" },
            "maxDepth": 1,
            "first": 2,
        }));
        binding.schema_hash.clone_from(&page1_plan.schema_hash);
        let ctx1 = TraverseExecContext {
            cursor_key: &key,
            binding: &binding,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-p1",
        };
        let p1 = execute_traverse(&db.store, &tenant, &writer, &page1_plan, &ctx1).expect("page1");
        assert_eq!(p1.subgraph.nodes.len(), 2);
        assert!(p1.subgraph.page_info.has_next_page);
        let cursor = p1.subgraph.page_info.end_cursor.clone().expect("endCursor");

        // Advance clock and mutate graph after T1 (`n4` added, `n3` updated).
        db.clock.advance(Duration::from_secs(60));
        db.store
            .put_node(
                &tenant,
                &writer,
                mk_node(
                    "n4",
                    "n4_new",
                    serde_json::json!({"ver": 2}),
                    ScopeSpec::Global,
                ),
            )
            .expect("put n4");
        db.store
            .put_edge(
                &tenant,
                &writer,
                mk_edge("e4", "n0", "n4", serde_json::json!({})),
            )
            .expect("edge e4");
        db.store
            .put_node(
                &tenant,
                &writer,
                mk_node(
                    "n3",
                    "n3_mutated",
                    serde_json::json!({"ver": 2}),
                    ScopeSpec::Global,
                ),
            )
            .expect("update n3");

        // Fetch page 2 across a bumped snapshot generation (`generation: 2`, unchanged schema).
        let mut binding_gen2 = binding.clone();
        binding_gen2.generation = 2;
        let page2_plan = plan_traverse_json(serde_json::json!({
            "from": { "id": "n0" },
            "maxDepth": 1,
            "first": 2,
            "after": cursor,
        }));
        let ctx2 = TraverseExecContext {
            cursor_key: &key,
            binding: &binding_gen2,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-p2",
        };
        let p2 = execute_traverse(&db.store, &tenant, &writer, &page2_plan, &ctx2).expect("page2");

        // Page 2 MUST see `n3` at T1 (ver: 1), MUST NOT see `n4`, and MUST NOT duplicate page 1!
        let p1_ids: Vec<&str> = p1.subgraph.nodes.iter().map(|n| n.id.as_str()).collect();
        let p2_ids: Vec<&str> = p2.subgraph.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(p1_ids, vec!["n0", "n1"]);
        assert_eq!(p2_ids, vec!["n2", "n3"]);
        assert!(!p2.subgraph.page_info.has_next_page);
        assert_eq!(
            p2.subgraph.nodes[1]
                .props
                .as_ref()
                .and_then(|v| v.get("ver"))
                .and_then(JsonValue::as_i64),
            Some(1)
        );
    }

    // spec: ING-065, ING-083, GQL-043
    #[test]
    fn inv_09_traverse_and_node_lookup_enforce_scopes_and_hide_invisible_entities() {
        let db = open_test_store(1_700_000_000_000);
        let (tenant, key, mut binding) = cursor_setup("acme", "");
        let alice = Caller::new("alice").with_groups(["eng"]);
        let bob = Caller::new("bob").with_task_chain(vec![TaskId::new_v7()]);

        db.store
            .put_node(
                &tenant,
                &alice,
                mk_node(
                    "pub",
                    "sym:pub",
                    serde_json::json!({"key": "sym:pub"}),
                    ScopeSpec::Global,
                ),
            )
            .unwrap();
        let mut priv_node = mk_node("priv", "priv", serde_json::json!({}), ScopeSpec::Private);
        priv_node.id = None;
        let priv_id = db.store.put_node(&tenant, &alice, priv_node).unwrap();

        let mut grp_node = mk_node(
            "grp",
            "grp",
            serde_json::json!({}),
            ScopeSpec::Group {
                group: "eng".to_owned(),
            },
        );
        grp_node.id = None;
        let grp_id = db.store.put_node(&tenant, &alice, grp_node).unwrap();

        db.store
            .put_edge(
                &tenant,
                &alice,
                mk_edge("e_priv", "pub", priv_id.as_str(), serde_json::json!({})),
            )
            .unwrap();
        db.store
            .put_edge(
                &tenant,
                &alice,
                mk_edge("e_grp", "pub", grp_id.as_str(), serde_json::json!({})),
            )
            .unwrap();

        // Bob cannot see Alice's private node via `node(ref: ...)` or `traverse`.
        let mut lookup_args = BTreeMap::new();
        lookup_args.insert(
            "ref".to_owned(),
            ArgValue::Literal(serde_json::json!({"id": priv_id.as_str()})),
        );
        let lookup_plan = validate_and_plan_node_lookup(&lookup_args, &BTreeMap::new()).unwrap();
        assert!(
            execute_node_lookup(&db.store, &tenant, &bob, &lookup_plan, "cid-bob")
                .unwrap()
                .is_none()
        );

        // Bob cannot see Alice's private node via `node(ref: id)` OR `node(ref: key)` (`ING-065`, `ING-083`).
        let mut priv_key_args = BTreeMap::new();
        priv_key_args.insert(
            "ref".to_owned(),
            ArgValue::Literal(serde_json::json!({"key": {"type": "Symbol", "value": "priv"}})),
        );
        let priv_key_plan =
            validate_and_plan_node_lookup(&priv_key_args, &BTreeMap::new()).unwrap();
        assert!(
            execute_node_lookup(&db.store, &tenant, &bob, &priv_key_plan, "cid-bob-key")
                .unwrap()
                .is_none()
        );
        // Alice herself CAN resolve her private node via `NodeKey`.
        assert_eq!(
            execute_node_lookup(&db.store, &tenant, &alice, &priv_key_plan, "cid-alice-key")
                .unwrap()
                .expect("owner sees private key")
                .id,
            priv_id.as_str()
        );

        // `node(ref: { key: ... })` finds visible Global node.
        let mut key_args = BTreeMap::new();
        key_args.insert(
            "ref".to_owned(),
            ArgValue::Literal(serde_json::json!({"key": {"type": "Symbol", "value": "sym:pub"}})),
        );
        let key_plan = validate_and_plan_node_lookup(&key_args, &BTreeMap::new()).unwrap();
        let found = execute_node_lookup(&db.store, &tenant, &bob, &key_plan, "cid-key")
            .unwrap()
            .expect("visible global node");
        assert_eq!(found.id, "pub");
        assert_eq!(found.scope, PlannedContextScope::Global);

        // Alice sees `pub`, `priv`, and `grp` without scope filter, and only `grp` + `pub` when
        // narrowing `scopes: [GLOBAL, GROUP]` (`GQL-043`).
        let unfiltered = plan_traverse_json(serde_json::json!({
            "from": { "id": "pub" },
            "maxDepth": 1,
        }));
        binding.schema_hash.clone_from(&unfiltered.schema_hash);
        let ctx_all = TraverseExecContext {
            cursor_key: &key,
            binding: &binding,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-all",
        };
        let res_all = execute_traverse(&db.store, &tenant, &alice, &unfiltered, &ctx_all).unwrap();
        assert_eq!(res_all.subgraph.nodes.len(), 3);

        let narrowed = plan_traverse_json(serde_json::json!({
            "from": { "id": "pub" },
            "maxDepth": 1,
            "scopes": ["GLOBAL", "GROUP"],
        }));
        binding.schema_hash.clone_from(&narrowed.schema_hash);
        let ctx = TraverseExecContext {
            cursor_key: &key,
            binding: &binding,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-scope",
        };
        let res = execute_traverse(&db.store, &tenant, &alice, &narrowed, &ctx).unwrap();
        let ids: BTreeSet<&str> = res.subgraph.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, BTreeSet::from(["pub", grp_id.as_str()]));

        // Bob traversing sees only `pub` (even `grp` is hidden because Bob lacks `"eng"`).
        let bob_res = execute_traverse(&db.store, &tenant, &bob, &narrowed, &ctx).unwrap();
        let bob_ids: Vec<&str> = bob_res
            .subgraph
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(bob_ids, vec!["pub"]);
    }

    // spec: ING-067, GQL-010
    #[test]
    fn traverse_weighted_reports_invalid_weights_warning_and_evaluates_predicates() {
        let db = open_test_store(1_700_000_000_000);
        let (tenant, key, mut binding) = cursor_setup("acme", "");
        let writer = Caller::new("alice");

        for (id, lang, tier) in [("n0", "rust", 1), ("n1", "rust", 2), ("n2", "python", 3)] {
            db.store
                .put_node(
                    &tenant,
                    &writer,
                    mk_node(
                        id,
                        id,
                        serde_json::json!({ "lang": lang, "tier": tier }),
                        ScopeSpec::Global,
                    ),
                )
                .unwrap();
        }
        // One valid weighted edge and one invalid negative-weight edge (`ING-067`).
        db.store
            .put_edge(
                &tenant,
                &writer,
                mk_edge("e_ok", "n0", "n1", serde_json::json!({"weight": 2.0})),
            )
            .unwrap();
        db.store
            .put_edge(
                &tenant,
                &writer,
                mk_edge("e_bad", "n0", "n2", serde_json::json!({"weight": -5.0})),
            )
            .unwrap();

        let plan = plan_traverse_json(serde_json::json!({
            "from": { "id": "n0" },
            "maxDepth": 2,
            "strategy": "WEIGHTED",
            "where": {
                "and": [
                    { "eq": { "path": "lang", "value": "rust" } },
                    { "gte": { "path": "tier", "value": 2 } }
                ]
            }
        }));
        binding.schema_hash.clone_from(&plan.schema_hash);
        let ctx = TraverseExecContext {
            cursor_key: &key,
            binding: &binding,
            now: db.clock.now_timestamp(),
            correlation_id: "cid-weighted",
        };
        let res = execute_traverse(&db.store, &tenant, &writer, &plan, &ctx).unwrap();

        assert_eq!(res.subgraph.nodes.len(), 1);
        assert_eq!(res.subgraph.nodes[0].id, "n1");
        assert_eq!(res.warnings.len(), 1);
        assert_eq!(res.warnings[0].code, ErrorCode::PartialResult);
        assert_eq!(res.warnings[0].count, 1);
    }
}
