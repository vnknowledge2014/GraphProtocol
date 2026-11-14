//! `gap-ngc-sdk::Connector` implementation for native code and documentation graph traversal
//! (`ING-060`..`ING-064`, `INV-12`, `INV-18`).

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use futures_util::stream;
use gap_core::clock::Timestamp;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{TenantId, TenantScope};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::{json_map_to_prost_struct, ngc_error_to_proto, prost_struct_to_json_map};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, HealthRequest,
    HealthResponse, Limits, MetaSummary, OpKind, OperationDescriptor, PaginationKind, Protocol,
    QueryRequest, QueryResponse, RowSetChunk, Selection, ServingStatus, SideEffectHint,
    query_response,
};
use gap_store::{
    Caller, ContextStore, Direction, Edge, EdgeType, Node, NodeId, TraverseQuery, TraverseResume,
    TraverseStrategy,
};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use tokio::time::Instant;

use crate::model::{CodeNgcError, EDGE_PREDICATES, NODE_CATEGORIES};

const MAX_TRAVERSE_DEPTH: u32 = gap_store::types::MAX_TRAVERSE_DEPTH;
const MIN_TRAVERSE_DEPTH: u32 = gap_store::types::MIN_TRAVERSE_DEPTH;
const MAX_TRAVERSE_FIRST: u32 = gap_store::types::MAX_TRAVERSE_FIRST;
const MIN_TRAVERSE_FIRST: u32 = gap_store::types::MIN_TRAVERSE_FIRST;
const MAX_VISITED_CAP: u32 = gap_store::types::MAX_VISITED_CAP;
const MIN_SEARCH_LIMIT: u32 = gap_store::types::MIN_SEARCH_LIMIT;
const MAX_SEARCH_LIMIT: u32 = gap_store::types::MAX_SEARCH_LIMIT;
const MAX_FOREACH_BATCH: u32 = 256;
const MAX_IN_FLIGHT: u32 = 64;
const DEFAULT_TIMEOUT_MS: u32 = 5_000;
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

const CODE_GRAPH_SCHEMA_SDL: &str = r#"
scalar JSON

enum TraverseDirection { OUTBOUND INBOUND BOTH }
enum TraverseStrategy { BFS DFS SHORTEST WEIGHTED }

type CodeProvenance {
  source: String!
  agent: String
}

type CodeEdge {
  id: ID!
  type: String!
  from: ID!
  to: ID!
  resolution: String!
  confidence: Float
}

type CodeNode {
  id: ID!
  type: String!
  label: String!
  summary: String
  props: JSON
  provenance: CodeProvenance
  validFrom: String!
  validTo: String
  scope: String!
  edges: [CodeEdge!]
}

type Query {
  code_traverse(
    from: ID!
    direction: TraverseDirection = OUTBOUND
    edgeTypes: [String!]
    maxDepth: Int = 2
    strategy: TraverseStrategy = BFS
    first: Int = 100
    visitedCap: Int = 100000
    after: String
    asOf: String
    returnFields: [String!]
  ): [CodeNode!]!

  code_node(
    id: ID!
    asOf: String
    returnFields: [String!]
  ): [CodeNode!]!

  code_search(
    query: String!
    limit: Int = 20
    asOf: String
    returnFields: [String!]
  ): [CodeNode!]!

  code_callers(
    targetId: ID
    symbol: String
    edgeTypes: [String!]
    maxDepth: Int = 1
    first: Int = 100
    visitedCap: Int = 100000
    asOf: String
    returnFields: [String!]
  ): [CodeNode!]!
}
"#;

/// Async [`Connector`] adapter over a [`ContextStore`] populated by [`crate::CodeIndexer`]
/// (`ING-060`..`ING-064`).
pub struct CodeGraphConnector<S: ContextStore> {
    store: Arc<S>,
    default_tenant: TenantScope,
    default_caller: Caller,
    description: ConnectorDescription,
}

impl<S: ContextStore> CodeGraphConnector<S> {
    /// Constructs a new [`CodeGraphConnector`] bound to a backing [`ContextStore`].
    #[must_use]
    pub fn new(
        connector_id: impl Into<String>,
        store: Arc<S>,
        default_tenant: TenantScope,
        default_caller: Caller,
    ) -> Self {
        let connector_id = connector_id.into();
        let spec_hash = blake3::hash(CODE_GRAPH_SCHEMA_SDL.as_bytes())
            .to_hex()
            .to_string();
        let description = build_description(connector_id, spec_hash);
        Self {
            store,
            default_tenant,
            default_caller,
            description,
        }
    }

    /// Returns the static connector descriptor.
    #[must_use]
    pub const fn description(&self) -> &ConnectorDescription {
        &self.description
    }
}

impl<S: ContextStore + 'static> Connector for CodeGraphConnector<S> {
    async fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        let description = &self.description;
        check_scope(&scope)?;
        if !request.known_spec_hash.is_empty() && request.known_spec_hash == description.spec_hash {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }
        Ok(DescribeResponse {
            not_modified: false,
            description: Some(description.clone()),
        })
    }

    async fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        let store = Arc::clone(&self.store);
        let default_tenant = self.default_tenant.clone();
        let default_caller = self.default_caller.clone();

        check_scope(&scope)?;
        validate_operation_name(&request.operation)?;
        if request.foreach.len() > MAX_FOREACH_BATCH as usize {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "foreach batch size {} exceeds limit {MAX_FOREACH_BATCH}",
                    request.foreach.len()
                ),
            ));
        }

        let (tenant, caller) =
            resolve_tenant_and_caller(&request, &default_tenant, &default_caller)?;
        let req_as_of = if request.as_of.is_empty() {
            None
        } else {
            Some(parse_timestamp(&request.as_of)?)
        };
        let base_args = request
            .arguments
            .as_ref()
            .map(prost_struct_to_json_map)
            .unwrap_or_default();
        let selected_fields = selection_field_names(request.selection.as_ref());
        let page_limit = request.page.as_ref().map(|p| p.limit).filter(|&l| l > 0);
        let page_cursor = request
            .page
            .as_ref()
            .map(|p| p.cursor.clone())
            .filter(|c| !c.is_empty());
        let op_name = request.operation.clone();

        let started = std::time::Instant::now();
        if request.foreach.is_empty() {
            let outcome = tokio::task::spawn_blocking(move || {
                execute_single(
                    store.as_ref(),
                    &tenant,
                    &caller,
                    &op_name,
                    &base_args,
                    req_as_of,
                    page_limit,
                    page_cursor.as_deref(),
                    &selected_fields,
                )
            })
            .await
            .map_err(|err| {
                NgcErrorInfo::new(
                    NgcErrorCode::Internal,
                    format!("code graph query worker failed: {err}"),
                )
            })??;

            check_scope(&scope)?;
            let rows_total = u64::try_from(outcome.rows.len()).unwrap_or(u64::MAX);
            let latency_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
            let chunks = vec![
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: outcome.rows,
                        last_for_index: true,
                        error: None,
                    })),
                }),
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Complete(Complete {
                        next_cursor: outcome.next_cursor,
                        truncated: outcome.truncated,
                        rows_total,
                        upstream_calls: 1,
                        latency_us,
                    })),
                }),
            ];
            let boxed: BoxStream<QueryResponse> = Box::pin(stream::iter(chunks));
            return Ok(boxed);
        }

        let foreach_items: Vec<(u32, JsonMap<String, JsonValue>)> = request
            .foreach
            .iter()
            .map(|vs| {
                let item_map = vs
                    .values
                    .as_ref()
                    .map(prost_struct_to_json_map)
                    .unwrap_or_default();
                (vs.index, item_map)
            })
            .collect();

        let batch_chunks = tokio::task::spawn_blocking(move || {
            let mut rows_total = 0u64;
            let mut any_truncated = false;
            let mut results = Vec::with_capacity(foreach_items.len());
            for (index, item_vars) in foreach_items {
                let mut merged = base_args.clone();
                for (k, v) in item_vars {
                    merged.insert(k, v);
                }
                match execute_single(
                    store.as_ref(),
                    &tenant,
                    &caller,
                    &op_name,
                    &merged,
                    req_as_of,
                    page_limit,
                    page_cursor.as_deref(),
                    &selected_fields,
                ) {
                    Ok(out) => {
                        rows_total =
                            rows_total.saturating_add(u64::try_from(out.rows.len()).unwrap_or(0));
                        any_truncated |= out.truncated;
                        results.push(Ok(QueryResponse {
                            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                foreach_index: index,
                                rows: out.rows,
                                last_for_index: true,
                                error: None,
                            })),
                        }));
                    }
                    Err(err) => {
                        results.push(Ok(QueryResponse {
                            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                foreach_index: index,
                                rows: Vec::new(),
                                last_for_index: true,
                                error: Some(ngc_error_to_proto(&err)),
                            })),
                        }));
                    }
                }
            }
            (results, rows_total, any_truncated)
        })
        .await
        .map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!("code graph foreach worker failed: {err}"),
            )
        })?;

        check_scope(&scope)?;
        let (mut responses, rows_total, truncated) = batch_chunks;
        let latency_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        responses.push(Ok(QueryResponse {
            chunk: Some(query_response::Chunk::Complete(Complete {
                next_cursor: String::new(),
                truncated,
                rows_total,
                upstream_calls: 1,
                latency_us,
            })),
        }));
        let boxed: BoxStream<QueryResponse> = Box::pin(stream::iter(responses));
        Ok(boxed)
    }

    async fn health(&self, _request: HealthRequest, scope: CallScope) -> NgcResult<HealthResponse> {
        check_scope(&scope)?;
        Ok(HealthResponse {
            status: ServingStatus::Serving as i32,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            details: Default::default(),
        })
    }
}

struct SingleQueryOutput {
    rows: Vec<prost_types::Struct>,
    next_cursor: String,
    truncated: bool,
}

#[allow(clippy::too_many_arguments)]
fn execute_single<S: ContextStore>(
    store: &S,
    tenant: &TenantScope,
    caller: &Caller,
    operation: &str,
    args: &JsonMap<String, JsonValue>,
    req_as_of: Option<Timestamp>,
    page_limit: Option<u32>,
    page_cursor: Option<&str>,
    selected_fields: &[String],
) -> NgcResult<SingleQueryOutput> {
    let as_of = match args.get("asOf").and_then(|v| v.as_str()) {
        Some(raw) if !raw.is_empty() => Some(parse_timestamp(raw)?),
        Some(_) => None,
        None => req_as_of,
    };
    let return_fields = resolve_return_fields(args, selected_fields)?;

    match operation {
        "code_traverse" => execute_traverse(
            store,
            tenant,
            caller,
            args,
            as_of,
            page_limit,
            page_cursor,
            return_fields.as_deref(),
        ),
        "code_node" => {
            let id_str = required_str(args, "id")?;
            let id = NodeId::new(id_str).map_err(CodeNgcError::from)?;
            let node = store
                .get_node(tenant, caller, &id, as_of)
                .map_err(CodeNgcError::from)?;
            let rows = match node {
                Some(n) => vec![json_map_to_prost_struct(&project_node(
                    &n,
                    &[],
                    return_fields.as_deref(),
                ))],
                None => Vec::new(),
            };
            Ok(SingleQueryOutput {
                rows,
                next_cursor: String::new(),
                truncated: false,
            })
        }
        "code_search" => {
            let query = required_str(args, "query")?;
            let limit = parse_bounded_u32(
                args.get("limit"),
                page_limit,
                20,
                MIN_SEARCH_LIMIT,
                MAX_SEARCH_LIMIT,
                "limit",
            )?;
            let nodes = store
                .search_nodes(tenant, caller, query, limit, as_of)
                .map_err(CodeNgcError::from)?;
            let rows = nodes
                .iter()
                .map(|n| json_map_to_prost_struct(&project_node(n, &[], return_fields.as_deref())))
                .collect();
            Ok(SingleQueryOutput {
                rows,
                next_cursor: String::new(),
                truncated: false,
            })
        }
        "code_callers" => execute_callers(
            store,
            tenant,
            caller,
            args,
            as_of,
            page_limit,
            return_fields.as_deref(),
        ),
        other => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("unknown code-graph operation `{other}`"),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_traverse<S: ContextStore>(
    store: &S,
    tenant: &TenantScope,
    caller: &Caller,
    args: &JsonMap<String, JsonValue>,
    as_of: Option<Timestamp>,
    page_limit: Option<u32>,
    page_cursor: Option<&str>,
    return_fields: Option<&[String]>,
) -> NgcResult<SingleQueryOutput> {
    let from_str = required_str(args, "from")?;
    let from = NodeId::new(from_str).map_err(CodeNgcError::from)?;

    let mut query = TraverseQuery::new(from);
    query.direction = parse_direction(args.get("direction"))?;
    query.strategy = parse_strategy(args.get("strategy"))?;
    query.edge_types = parse_edge_types(args.get("edgeTypes"))?;
    query.max_depth = parse_bounded_u32(
        args.get("maxDepth"),
        None,
        2,
        MIN_TRAVERSE_DEPTH,
        MAX_TRAVERSE_DEPTH,
        "maxDepth",
    )?;
    query.first = parse_bounded_u32(
        args.get("first"),
        page_limit,
        100,
        MIN_TRAVERSE_FIRST,
        MAX_TRAVERSE_FIRST,
        "first",
    )?;
    query.visited_cap = parse_optional_visited_cap(args.get("visitedCap"))?;
    query.as_of = as_of;
    query.after = match args.get("after").and_then(|v| v.as_str()).or(page_cursor) {
        Some(raw) if !raw.is_empty() => {
            let resume: TraverseResume = serde_json::from_str(raw).map_err(|err| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("invalid `after` cursor: {err}"),
                )
            })?;
            Some(resume)
        }
        _ => None,
    };

    let result = store
        .traverse(tenant, caller, &query)
        .map_err(CodeNgcError::from)?;
    let rows = result
        .nodes
        .iter()
        .map(|node| {
            let incident: Vec<&Edge> = result
                .edges
                .iter()
                .filter(|e| e.from == node.id || e.to == node.id)
                .collect();
            json_map_to_prost_struct(&project_node(node, &incident, return_fields))
        })
        .collect();
    let next_cursor = match result.resume {
        Some(resume) => serde_json::to_string(&resume).map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!("failed to serialize resume cursor: {err}"),
            )
        })?,
        None => String::new(),
    };

    Ok(SingleQueryOutput {
        rows,
        next_cursor,
        truncated: result.truncated,
    })
}

fn execute_callers<S: ContextStore>(
    store: &S,
    tenant: &TenantScope,
    caller: &Caller,
    args: &JsonMap<String, JsonValue>,
    as_of: Option<Timestamp>,
    page_limit: Option<u32>,
    return_fields: Option<&[String]>,
) -> NgcResult<SingleQueryOutput> {
    let target_ids: Vec<NodeId> = match args.get("targetId").and_then(|v| v.as_str()) {
        Some(raw) if !raw.is_empty() => vec![NodeId::new(raw).map_err(CodeNgcError::from)?],
        _ => {
            let symbol = required_str(args, "symbol")?;
            let found = store
                .search_nodes(tenant, caller, symbol, 5, as_of)
                .map_err(CodeNgcError::from)?;
            found
                .into_iter()
                .filter(|n| n.label == symbol || n.node_type.as_str() == "Symbol")
                .map(|n| n.id)
                .collect()
        }
    };

    if target_ids.is_empty() {
        return Ok(SingleQueryOutput {
            rows: Vec::new(),
            next_cursor: String::new(),
            truncated: false,
        });
    }

    let edge_types = match parse_edge_types(args.get("edgeTypes"))? {
        Some(ets) => Some(ets),
        None => Some(vec![
            EdgeType::new("CALLS").map_err(CodeNgcError::from)?,
            EdgeType::new("TESTS").map_err(CodeNgcError::from)?,
            EdgeType::new("DOCUMENTS").map_err(CodeNgcError::from)?,
            EdgeType::new("REFERENCES").map_err(CodeNgcError::from)?,
        ]),
    };
    let max_depth = parse_bounded_u32(
        args.get("maxDepth"),
        None,
        1,
        MIN_TRAVERSE_DEPTH,
        MAX_TRAVERSE_DEPTH,
        "maxDepth",
    )?;
    let first = parse_bounded_u32(
        args.get("first"),
        page_limit,
        100,
        MIN_TRAVERSE_FIRST,
        MAX_TRAVERSE_FIRST,
        "first",
    )?;
    let visited_cap = parse_optional_visited_cap(args.get("visitedCap"))?;

    let mut seen_nodes = BTreeSet::new();
    let mut rows = Vec::new();
    let mut truncated = false;

    for tid in &target_ids {
        let mut q = TraverseQuery::new(tid.clone());
        q.direction = Direction::Inbound;
        q.edge_types = edge_types.clone();
        q.max_depth = max_depth;
        q.strategy = TraverseStrategy::Bfs;
        q.as_of = as_of;
        q.first = first;
        q.visited_cap = visited_cap;

        let res = store
            .traverse(tenant, caller, &q)
            .map_err(CodeNgcError::from)?;
        truncated |= res.truncated;
        for node in &res.nodes {
            if &node.id != tid && seen_nodes.insert(node.id.as_str().to_owned()) {
                let incident: Vec<&Edge> = res
                    .edges
                    .iter()
                    .filter(|e| e.from == node.id || e.to == node.id)
                    .collect();
                rows.push(json_map_to_prost_struct(&project_node(
                    node,
                    &incident,
                    return_fields,
                )));
                if rows.len() >= first as usize {
                    break;
                }
            }
        }
        if rows.len() >= first as usize {
            break;
        }
    }

    Ok(SingleQueryOutput {
        rows,
        next_cursor: String::new(),
        truncated,
    })
}

fn project_node(
    node: &Node,
    incident_edges: &[&Edge],
    return_fields: Option<&[String]>,
) -> JsonMap<String, JsonValue> {
    let mut full = JsonMap::new();
    full.insert(
        String::from("id"),
        JsonValue::String(node.id.as_str().to_owned()),
    );
    full.insert(
        String::from("type"),
        JsonValue::String(node.node_type.as_str().to_owned()),
    );
    full.insert(String::from("label"), JsonValue::String(node.label.clone()));
    full.insert(
        String::from("summary"),
        node.summary
            .as_ref()
            .map_or(JsonValue::Null, |s| JsonValue::String(s.clone())),
    );
    full.insert(String::from("props"), JsonValue::Object(node.props.clone()));
    full.insert(
        String::from("provenance"),
        json!({
            "source": node.provenance.source,
            "agent": node.provenance.agent,
        }),
    );
    full.insert(
        String::from("validFrom"),
        JsonValue::String(node.valid_from.to_string()),
    );
    full.insert(
        String::from("validTo"),
        node.valid_to
            .map_or(JsonValue::Null, |t| JsonValue::String(t.to_string())),
    );
    full.insert(
        String::from("scope"),
        JsonValue::String(node.scope.as_str().to_owned()),
    );

    let wants_edges = return_fields.is_none_or(|fields| fields.iter().any(|f| f == "edges"));
    if wants_edges && !incident_edges.is_empty() {
        let edges_json: Vec<JsonValue> = incident_edges
            .iter()
            .map(|e| {
                json!({
                    "id": e.id.as_str(),
                    "type": e.edge_type.as_str(),
                    "from": e.from.as_str(),
                    "to": e.to.as_str(),
                    "resolution": e.resolution.as_str(),
                    "confidence": e.confidence,
                })
            })
            .collect();
        full.insert(String::from("edges"), JsonValue::Array(edges_json));
    }

    match return_fields {
        Some(fields) if !fields.is_empty() => {
            let mut sliced = JsonMap::with_capacity(fields.len());
            for key in fields {
                if let Some(val) = full.remove(key) {
                    sliced.insert(key.clone(), val);
                }
            }
            sliced
        }
        _ => full,
    }
}

fn check_scope(scope: &CallScope) -> NgcResult<()> {
    if scope.cancel.is_cancelled() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::Cancelled,
            "request cancelled",
        ));
    }
    if Instant::now() >= scope.deadline {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "request deadline exceeded",
        ));
    }
    Ok(())
}

fn validate_operation_name(op: &str) -> NgcResult<()> {
    match op {
        "code_traverse" | "code_node" | "code_search" | "code_callers" => Ok(()),
        _ => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("unknown code-graph operation `{op}`"),
        )),
    }
}

fn resolve_tenant_and_caller(
    request: &QueryRequest,
    default_tenant: &TenantScope,
    default_caller: &Caller,
) -> NgcResult<(TenantScope, Caller)> {
    let Some(ctx) = request.ctx.as_ref() else {
        return Ok((default_tenant.clone(), default_caller.clone()));
    };
    let tenant = if ctx.tenant_id.is_empty() {
        default_tenant.clone()
    } else {
        let tid = TenantId::new(&ctx.tenant_id).map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("invalid tenant_id `{}`: {err}", ctx.tenant_id),
            )
        })?;
        TenantScope::new(tid)
    };
    let caller = match ctx.principal.as_ref() {
        Some(p) if !p.id.is_empty() => Caller::new(&p.id).with_groups(p.scopes.iter().cloned()),
        _ => default_caller.clone(),
    };
    Ok((tenant, caller))
}

fn parse_timestamp(raw: &str) -> NgcResult<Timestamp> {
    Timestamp::from_str(raw).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("invalid RFC 3339 UTC timestamp `{raw}`: {err}"),
        )
    })
}

fn required_str<'a>(args: &'a JsonMap<String, JsonValue>, key: &str) -> NgcResult<&'a str> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("missing required string argument `{key}`"),
        )),
    }
}

fn parse_bounded_u32(
    arg_val: Option<&JsonValue>,
    fallback: Option<u32>,
    default_val: u32,
    min: u32,
    max: u32,
    field: &str,
) -> NgcResult<u32> {
    let raw = match arg_val {
        None | Some(JsonValue::Null) => fallback.unwrap_or(default_val),
        Some(JsonValue::Number(num)) => {
            let signed = num
                .as_i64()
                .or_else(|| {
                    num.as_f64()
                        .filter(|f| f.is_finite() && f.fract() == 0.0)
                        .map(|f| f as i64)
                })
                .ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("argument `{field}` must be an integer"),
                    )
                })?;
            if signed < 0 {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("argument `{field}` must be within {min}..={max}, got {signed}"),
                ));
            }
            u32::try_from(signed).map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("argument `{field}` exceeds maximum {max}"),
                )
            })?
        }
        Some(_) => {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("argument `{field}` must be an integer"),
            ));
        }
    };
    if raw < min || raw > max {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            format!("argument `{field}` must be within {min}..={max}, got {raw}"),
        ));
    }
    Ok(raw)
}

fn parse_optional_visited_cap(arg_val: Option<&JsonValue>) -> NgcResult<Option<u32>> {
    match arg_val {
        None | Some(JsonValue::Null) => Ok(None),
        Some(val) => {
            let cap =
                parse_bounded_u32(Some(val), None, 100_000, 1, MAX_VISITED_CAP, "visitedCap")?;
            Ok(Some(cap))
        }
    }
}

fn parse_direction(arg_val: Option<&JsonValue>) -> NgcResult<Direction> {
    match arg_val {
        None | Some(JsonValue::Null) => Ok(Direction::Outbound),
        Some(JsonValue::String(s)) => match s.to_ascii_uppercase().as_str() {
            "OUTBOUND" => Ok(Direction::Outbound),
            "INBOUND" => Ok(Direction::Inbound),
            "BOTH" => Ok(Direction::Both),
            _ => Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("unsupported traverse direction `{s}`"),
            )),
        },
        Some(_) => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "argument `direction` must be a string",
        )),
    }
}

fn parse_strategy(arg_val: Option<&JsonValue>) -> NgcResult<TraverseStrategy> {
    match arg_val {
        None | Some(JsonValue::Null) => Ok(TraverseStrategy::Bfs),
        Some(JsonValue::String(s)) => match s.to_ascii_uppercase().as_str() {
            "BFS" => Ok(TraverseStrategy::Bfs),
            "DFS" => Ok(TraverseStrategy::Dfs),
            "SHORTEST" => Ok(TraverseStrategy::Shortest),
            "WEIGHTED" => Ok(TraverseStrategy::Weighted),
            _ => Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("unsupported traverse strategy `{s}`"),
            )),
        },
        Some(_) => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "argument `strategy` must be a string",
        )),
    }
}

fn parse_edge_types(arg_val: Option<&JsonValue>) -> NgcResult<Option<Vec<EdgeType>>> {
    match arg_val {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Array(items)) => {
            if items.is_empty() {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "argument `edgeTypes` must not be empty when supplied",
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let s = item.as_str().ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "entries in `edgeTypes` must be strings",
                    )
                })?;
                out.push(EdgeType::new(s).map_err(CodeNgcError::from)?);
            }
            Ok(Some(out))
        }
        Some(_) => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "argument `edgeTypes` must be a string array",
        )),
    }
}

fn selection_field_names(sel: Option<&Selection>) -> Vec<String> {
    match sel {
        Some(s) => s.fields.iter().map(|f| f.name.clone()).collect(),
        None => Vec::new(),
    }
}

fn resolve_return_fields(
    args: &JsonMap<String, JsonValue>,
    selected: &[String],
) -> NgcResult<Option<Vec<String>>> {
    match args.get("returnFields") {
        Some(JsonValue::Array(arr)) => {
            let mut fields = Vec::with_capacity(arr.len());
            for item in arr {
                let name = item.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "entries in `returnFields` must be non-empty strings",
                    )
                })?;
                fields.push(name.to_owned());
            }
            Ok(if fields.is_empty() {
                None
            } else {
                Some(fields)
            })
        }
        Some(JsonValue::Null) | None => {
            if selected.is_empty() {
                Ok(None)
            } else {
                Ok(Some(selected.to_vec()))
            }
        }
        Some(_) => Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "argument `returnFields` must be a string array",
        )),
    }
}

fn build_description(connector_id: String, spec_hash: String) -> ConnectorDescription {
    ConnectorDescription {
        connector_id,
        connector_version: env!("CARGO_PKG_VERSION").to_owned(),
        ngc_version: NGC_WIRE_VERSION.to_owned(),
        protocol: Protocol::CodeGraph as i32,
        spec_hash,
        capabilities: Some(Capabilities {
            foreach_batch: true,
            predicate_ops: Vec::new(),
            order_by: false,
            pagination: PaginationKind::PaginationCursor as i32,
            native_traverse: true,
            subscribe: false,
            tasks: false,
            dry_run: false,
            idempotency: false,
            sketch: false,
            explain: false,
            as_of: true,
        }),
        limits: Some(Limits {
            max_batch_size: MAX_FOREACH_BATCH,
            max_in_flight: MAX_IN_FLIGHT,
            max_page_size: MAX_TRAVERSE_FIRST,
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_response_bytes: MAX_RESPONSE_BYTES,
        }),
        schema_sdl: CODE_GRAPH_SCHEMA_SDL.to_owned(),
        operations: vec![
            op_desc(
                "code_traverse",
                "Bounded native graph traversal from a code/doc node with depth, strategy, asOf, and returnFields slicing.",
            ),
            op_desc(
                "code_node",
                "Look up a single repository, file, module, symbol, doc, test, or concept node by id with optional asOf and returnFields.",
            ),
            op_desc(
                "code_search",
                "Full-text BM25 search over indexed code and documentation labels and summaries.",
            ),
            op_desc(
                "code_callers",
                "Find inbound callers, tests, and documentation referencing a target symbol or node.",
            ),
        ],
        meta: Some(MetaSummary {
            node_categories: NODE_CATEGORIES.iter().map(|s| (*s).to_owned()).collect(),
            edge_predicates: EDGE_PREDICATES.iter().map(|s| (*s).to_owned()).collect(),
            synopsis_hint: String::from(
                "Repository code, symbol call, inheritance, test, and documentation graph.",
            ),
        }),
        auth: None,
    }
}

fn op_desc(name: &str, description: &str) -> OperationDescriptor {
    OperationDescriptor {
        name: name.to_owned(),
        kind: OpKind::Query as i32,
        description: description.to_owned(),
        input_schema: None,
        output_schema: None,
        side_effect_hint: SideEffectHint::SeNone as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 1,
        pagination: PaginationKind::PaginationCursor as i32,
        key_fields: vec![String::from("id")],
        annotations: Default::default(),
    }
}
