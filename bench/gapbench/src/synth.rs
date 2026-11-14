//! Deterministic synthetic multi-branch supergraphs (`N in {10, 50, 200, 1000}`) and
//! 24-file code-graph repository fixture for GAP-Bench v1 (`ADR-021`, `CMP-008`, `RTE-034`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream;
use gap_core::clock::SystemClock;
use gap_core::config::Profile;
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::GapError;
use gap_core::ids::{BranchId, BranchUid, TenantId, TenantScope};
use gap_core::snapshot::{BranchDescriptor, BranchStatus, Endpoint, Protocol, Snapshot, TrustTier};
use gap_exec::{BranchClient, ConnectorRegistry};
use gap_index::{HybridElementIndex, HybridIndexConfig};
use gap_mcp::inbound::{AllVisible, FacadeConfig, FacadeConfigError, McpFacade};
use gap_ngc_sdk::connector::{BoxStream, CallScope, NgcResult};
use gap_plan::{PlanCache, PlanCacheConfig};
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, Complete, ConnectorDescription, Example as ProtoExample,
    Limits as ProtoLimits, MetaSummary, OpKind as ProtoOpKind,
    OperationDescriptor as ProtoOperationDescriptor, Protocol as ProtoProtocol, QueryRequest,
    QueryResponse, RowSetChunk, SideEffectHint, query_response,
};
use gap_route::{DiscoveryConfig, RouteDiscoveryService};
use gap_snapshot::groups::{GroupAssemblyError, GroupAssignmentConfig, assign_capability_groups};
use gap_snapshot::{BranchInput, CompositionError, SnapshotBuilder, SnapshotHandle, compose};
use gap_store::{
    Caller, ContextStore, Direction, Durability, EdgeResolution, EdgeType, NewEdge, NewNode,
    NodeId, NodeType, Provenance, ScopeSpec, SqliteConfig, SqliteContextStore, StoreError,
    TraverseQuery, TraverseStrategy,
};
use gap_transport::{
    AppState, ConfigValidationError, TransportConfig, TransportEngine, TransportMetrics,
};
use prost_types::{Struct as ProstStruct, Value as ProstValue, value::Kind as ProstKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value, json};
use tempfile::TempDir;
use thiserror::Error;

/// Typed errors produced by the GAP-Bench v1 harness.
#[derive(Debug, Error)]
pub enum BenchError {
    /// Core GAP runtime failure.
    #[error("gap core error: {0}")]
    Gap(#[from] GapError),
    /// Supergraph composition failure.
    #[error("supergraph composition error: {0}")]
    Composition(#[from] CompositionError),
    /// Capability-group / hybrid-index assembly failure.
    #[error("capability group assembly error: {0}")]
    GroupAssembly(#[from] GroupAssemblyError),
    /// Transport configuration validation failure.
    #[error("transport config error: {0}")]
    TransportConfig(#[from] ConfigValidationError),
    /// Inbound MCP facade configuration validation failure.
    #[error("mcp facade config error: {0}")]
    FacadeConfig(#[from] FacadeConfigError),
    /// ContextStore / SQLite error.
    #[error("context store error: {0}")]
    Store(#[from] StoreError),
    /// Filesystem I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization or deserialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Invalid CLI flag or argument.
    #[error("cli error: {0}")]
    Cli(String),
    /// Optional external LLM endpoint failure.
    #[error("llm agent error: {0}")]
    Llm(String),
}

const DOMAINS: &[&str] = &[
    "billing",
    "inventory",
    "telemetry",
    "identity",
    "compliance",
    "analytics",
    "routing",
    "storage",
    "alerting",
    "scheduling",
    "procurement",
    "ledger",
    "catalog",
    "fulfillment",
    "moderation",
    "treasury",
    "observability",
    "governance",
    "workforce",
    "settlement",
];

const SUBSYSTEMS: &[&str] = &[
    "invoice",
    "warehouse",
    "metrics",
    "session",
    "audit",
    "cohort",
    "gateway",
    "archive",
    "incident",
    "calendar",
    "supplier",
    "journal",
    "product",
    "parcel",
    "review",
    "payout",
];

const ASPECTS: &[&str] = &[
    "snapshot",
    "summary",
    "status",
    "record",
    "profile",
    "balance",
    "manifest",
    "checkpoint",
    "digest",
    "allocation",
    "quota",
    "registry",
];

/// Direct-MCP tool metadata and deterministic response template.
#[derive(Debug, Clone)]
pub struct DirectToolSpec {
    /// Registered tool name.
    pub name: String,
    /// Kind of synthetic payload returned by this tool.
    pub responder: SynthOpKind,
}

/// Operation response archetype shared by `SynthBranchClient` and Direct-MCP dispatch.
#[derive(Debug, Clone)]
pub enum SynthOpKind {
    /// `shop_customer(id: Int!)`
    ShopCustomer,
    /// `shop_orders(customer_id: Int!)`
    ShopOrders,
    /// `notes_for_customer(customer_id: Int!)`
    CustomerNotes,
    /// `order_shipment(order_id: Int!)`
    OrderShipment,
    /// Generic domain lookup `svc{b}_tool{k}(id: Int!, filter: String)`
    DomainLookup {
        /// Global 0-based tool index.
        global_idx: usize,
        /// Domain label.
        domain: &'static str,
        /// Subsystem label.
        subsystem: &'static str,
    },
}

/// Ground-truth `__search` evaluation case (`RTE-034`, `RTE-005`).
#[derive(Debug, Clone)]
pub struct LabeledSearchCase {
    /// Natural-language intent query.
    pub query: String,
    /// Expected GraphQL root field / operation name.
    pub expected_op_name: String,
}

/// Suite `(a)` API-lookup benchmark task.
#[derive(Debug, Clone)]
pub struct ApiLookupTask {
    /// Stable task identifier.
    pub task_id: String,
    /// User prompt presented to the agent.
    pub prompt: String,
    /// Natural-language search intent issued to `gap_search`.
    pub search_query: String,
    /// Gold operation name in both Direct-MCP and GAP supergraph.
    pub expected_op_name: String,
    /// Numeric `id` argument passed to the operation.
    pub arg_id: i64,
    /// Expected `code` field returned by the target operation.
    pub expected_code: String,
}

/// Suite `(b)` cross-branch join benchmark task.
#[derive(Debug, Clone)]
pub struct CrossBranchJoinTask {
    /// Stable task identifier.
    pub task_id: String,
    /// User prompt presented to the agent.
    pub prompt: String,
    /// Customer ID to look up across `shop` and `notes`.
    pub customer_id: i64,
    /// First order ID for shipment lookup.
    pub primary_order_id: i64,
    /// Expected customer display name.
    pub expected_customer_name: String,
    /// Expected order count for `customer_id`.
    pub expected_order_count: usize,
    /// Expected parcel carrier for `primary_order_id`.
    pub expected_carrier: String,
    /// Expected note summary substring.
    pub expected_note_summary: String,
}

/// Suite `(c)` code-graph impact / caller-analysis benchmark task (`G-C1`).
#[derive(Debug, Clone)]
pub struct CodeGraphTask {
    /// Stable task identifier.
    pub task_id: String,
    /// User prompt asking for all direct and transitive (depth <= 2) callers.
    pub prompt: String,
    /// Target function symbol name.
    pub target_symbol: String,
    /// Expected transitive caller function names (depth 1 and depth 2).
    pub expected_callers: BTreeSet<String>,
}

/// In-memory `BranchClient` serving deterministic `RowSetChunk` + `Complete` streams.
#[derive(Debug, Clone)]
struct SynthBranchClient {
    ops: Arc<BTreeMap<String, SynthOpKind>>,
}

impl BranchClient for SynthBranchClient {
    fn query(
        &self,
        request: QueryRequest,
        _scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>> {
        let ops = Arc::clone(&self.ops);
        Box::pin(async move {
            let args = request.arguments.unwrap_or_default();
            let rows = match ops.get(request.operation.as_str()) {
                Some(kind) => build_prost_rows(kind, &args),
                None => Vec::new(),
            };
            let rows_total = u64::try_from(rows.len()).unwrap_or(0);
            let items = vec![
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows,
                        last_for_index: true,
                        error: None,
                    })),
                }),
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Complete(Complete {
                        next_cursor: String::new(),
                        truncated: false,
                        rows_total,
                        upstream_calls: 1,
                        latency_us: 80,
                    })),
                }),
            ];
            Ok(Box::pin(stream::iter(items)) as BoxStream<QueryResponse>)
        })
    }
}

fn prost_str(s: impl Into<String>) -> ProstValue {
    ProstValue {
        kind: Some(ProstKind::StringValue(s.into())),
    }
}

#[allow(clippy::cast_precision_loss)]
fn prost_int(n: i64) -> ProstValue {
    ProstValue {
        kind: Some(ProstKind::NumberValue(n as f64)),
    }
}

fn extract_prost_int(st: &ProstStruct, key: &str, default: i64) -> i64 {
    st.fields
        .get(key)
        .and_then(|v| match &v.kind {
            Some(ProstKind::NumberValue(n)) => {
                #[allow(clippy::cast_possible_truncation)]
                let as_i64 = *n as i64;
                Some(as_i64)
            }
            _ => None,
        })
        .unwrap_or(default)
}

fn customer_fixture(cid: i64) -> (&'static str, &'static str, &'static str) {
    match cid % 4 {
        1 => ("Ada Lovelace", "enterprise", "eu-west"),
        2 => ("Alan Turing", "vip", "eu-north"),
        3 => ("Grace Hopper", "enterprise", "us-east"),
        _ => ("Edsger Dijkstra", "standard", "ap-south"),
    }
}

fn carrier_fixture(order_id: i64) -> (&'static str, &'static str) {
    match order_id % 3 {
        0 => ("dhl-express", "in_transit"),
        1 => ("fedex-priority", "delivered"),
        _ => ("ups-worldwide", "dispatched"),
    }
}

fn build_prost_rows(kind: &SynthOpKind, args: &ProstStruct) -> Vec<ProstStruct> {
    match kind {
        SynthOpKind::ShopCustomer => {
            let cid = extract_prost_int(args, "id", 1);
            let (name, tier, region) = customer_fixture(cid);
            let mut fields = BTreeMap::new();
            fields.insert("id".to_owned(), prost_int(cid));
            fields.insert("name".to_owned(), prost_str(name));
            fields.insert("tier".to_owned(), prost_str(tier));
            fields.insert("region".to_owned(), prost_str(region));
            vec![ProstStruct { fields }]
        }
        SynthOpKind::ShopOrders => {
            let cid = extract_prost_int(args, "customer_id", 1);
            let o1 = cid * 100 + 1;
            let o2 = cid * 100 + 2;
            let mut f1 = BTreeMap::new();
            f1.insert("id".to_owned(), prost_int(o1));
            f1.insert("customer_id".to_owned(), prost_int(cid));
            f1.insert("total_cents".to_owned(), prost_int(4200 + cid * 10));
            f1.insert("status".to_owned(), prost_str("paid"));

            let mut f2 = BTreeMap::new();
            f2.insert("id".to_owned(), prost_int(o2));
            f2.insert("customer_id".to_owned(), prost_int(cid));
            f2.insert("total_cents".to_owned(), prost_int(8500 + cid * 10));
            f2.insert("status".to_owned(), prost_str("processing"));

            vec![ProstStruct { fields: f1 }, ProstStruct { fields: f2 }]
        }
        SynthOpKind::CustomerNotes => {
            let cid = extract_prost_int(args, "customer_id", 1);
            let mut f1 = BTreeMap::new();
            f1.insert("note_id".to_owned(), prost_str(format!("note-{cid}-a")));
            f1.insert("customer_id".to_owned(), prost_int(cid));
            f1.insert(
                "summary".to_owned(),
                prost_str(format!("Priority onboarding verified for account {cid}")),
            );
            f1.insert("author".to_owned(), prost_str("support-bot"));
            vec![ProstStruct { fields: f1 }]
        }
        SynthOpKind::OrderShipment => {
            let oid = extract_prost_int(args, "order_id", 101);
            let (carrier, status) = carrier_fixture(oid);
            let mut fields = BTreeMap::new();
            fields.insert("order_id".to_owned(), prost_int(oid));
            fields.insert("carrier".to_owned(), prost_str(carrier));
            fields.insert("tracking_code".to_owned(), prost_str(format!("TRK-{oid}")));
            fields.insert("status".to_owned(), prost_str(status));
            vec![ProstStruct { fields }]
        }
        SynthOpKind::DomainLookup {
            global_idx,
            domain,
            subsystem,
        } => {
            let id = extract_prost_int(args, "id", 1);
            let code = format!("{domain}-{subsystem}-{global_idx:04}-{id}");
            let summary = format!("Verified {domain} {subsystem} record #{id}");
            let metric = i64::try_from(*global_idx).unwrap_or(0) * 10 + id;
            let mut fields = BTreeMap::new();
            fields.insert("id".to_owned(), prost_int(id));
            fields.insert("code".to_owned(), prost_str(code));
            fields.insert("summary".to_owned(), prost_str(summary));
            fields.insert("metric".to_owned(), prost_int(metric));
            vec![ProstStruct { fields }]
        }
    }
}

fn build_direct_json(kind: &SynthOpKind, args: &Value) -> Value {
    match kind {
        SynthOpKind::ShopCustomer => {
            let cid = args.get("id").and_then(Value::as_i64).unwrap_or(1);
            let (name, tier, region) = customer_fixture(cid);
            json!({
                "id": cid,
                "name": name,
                "tier": tier,
                "region": region
            })
        }
        SynthOpKind::ShopOrders => {
            let cid = args.get("customer_id").and_then(Value::as_i64).unwrap_or(1);
            let o1 = cid * 100 + 1;
            let o2 = cid * 100 + 2;
            json!({
                "orders": [
                    {"id": o1, "customer_id": cid, "total_cents": 4200 + cid * 10, "status": "paid"},
                    {"id": o2, "customer_id": cid, "total_cents": 8500 + cid * 10, "status": "processing"}
                ]
            })
        }
        SynthOpKind::CustomerNotes => {
            let cid = args.get("customer_id").and_then(Value::as_i64).unwrap_or(1);
            json!({
                "notes": [
                    {
                        "note_id": format!("note-{cid}-a"),
                        "customer_id": cid,
                        "summary": format!("Priority onboarding verified for account {cid}"),
                        "author": "support-bot"
                    }
                ]
            })
        }
        SynthOpKind::OrderShipment => {
            let oid = args.get("order_id").and_then(Value::as_i64).unwrap_or(101);
            let (carrier, status) = carrier_fixture(oid);
            json!({
                "order_id": oid,
                "carrier": carrier,
                "tracking_code": format!("TRK-{oid}"),
                "status": status
            })
        }
        SynthOpKind::DomainLookup {
            global_idx,
            domain,
            subsystem,
        } => {
            let id = args.get("id").and_then(Value::as_i64).unwrap_or(1);
            json!({
                "id": id,
                "code": format!("{domain}-{subsystem}-{global_idx:04}-{id}"),
                "summary": format!("Verified {domain} {subsystem} record #{id}"),
                "metric": i64::try_from(*global_idx).unwrap_or(0) * 10 + id
            })
        }
    }
}

/// Assembled in-process supergraph + Direct-MCP catalog for a given tool count `N`.
pub struct SyntheticSupergraph {
    /// Total number of callable operations (`N`).
    pub n_tools: usize,
    /// Number of composed branches (`B`).
    pub branch_count: usize,
    /// Pinned supergraph snapshot handle (`INV-01`).
    pub snapshot_handle: Arc<SnapshotHandle>,
    /// Stateless MCP `2026-07-28` facade (`L4`).
    pub facade: McpFacade,
    /// Baseline `RouteDiscoveryService` for `__metaGraph` and `__search` SLO measurements.
    pub discovery: RouteDiscoveryService,
    /// Compact UTF-8 JSON-RPC response bytes for Direct-MCP `tools/list` (`N` tools).
    pub direct_tools_list_bytes: Vec<u8>,
    /// Direct-MCP tool registry keyed by tool name.
    pub direct_tools: BTreeMap<String, DirectToolSpec>,
    /// 40 labeled `__search` intent queries for retrieval recall@10 & p95 benchmarking.
    pub search_cases: Vec<LabeledSearchCase>,
    /// Suite `(a)` API-lookup tasks.
    pub api_lookup_tasks: Vec<ApiLookupTask>,
    /// Suite `(b)` cross-branch join tasks.
    pub cross_branch_tasks: Vec<CrossBranchJoinTask>,
}

impl SyntheticSupergraph {
    /// Constructs a fresh per-request [`RequestContext`] pinning the active [`Snapshot`].
    pub fn make_request_context(&self) -> Result<RequestContext, BenchError> {
        let tenant = TenantId::default_tenant();
        let ctx = RequestContext::try_new(
            BranchUid::new_v7().as_uuid(),
            tenant.clone(),
            Principal::new("gapbench-agent", PrincipalKind::Agent, tenant),
            Instant::now() + Duration::from_secs(30),
            Budget::new(500_000),
            TraceCtx::generate_root(false),
            0,
            None,
            self.snapshot_handle.pin(),
        )?;
        Ok(ctx)
    }

    /// Executes a single Direct-MCP `tools/call` against the in-memory baseline catalog.
    pub fn call_direct_tool(&self, tool_name: &str, args: &Value) -> Result<Vec<u8>, BenchError> {
        let spec = self
            .direct_tools
            .get(tool_name)
            .ok_or_else(|| BenchError::Cli(format!("unknown direct MCP tool `{tool_name}`")))?;
        let data = build_direct_json(&spec.responder, args);
        let text = serde_json::to_string(&data)?;
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "resultType": "complete",
                "content": [{ "type": "text", "text": text }],
                "structuredContent": data,
                "isError": false
            }
        });
        Ok(serde_json::to_vec(&resp)?)
    }
}

fn deterministic_branch_uid(slug: &str) -> Result<BranchUid, BenchError> {
    let mut bytes = *blake3::hash(format!("gapbench-branch:{slug}").as_bytes()).as_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex_encode(&bytes[..16]);
    let formatted = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    BranchUid::parse(&formatted).map_err(|e| BenchError::Cli(e.to_string()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
    out
}

fn make_direct_tool_json(name: &str, title: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

/// Builds and indexes a deterministic `N`-tool supergraph (`N >= 4`) and Direct-MCP baseline.
pub fn build_synthetic_supergraph(n_tools: usize) -> Result<SyntheticSupergraph, BenchError> {
    if n_tools < 4 {
        return Err(BenchError::Cli(format!(
            "`--n` values must be at least 4 (got {n_tools})"
        )));
    }

    let branch_count = (n_tools / 10).max(2);
    let base_per_branch = n_tools / branch_count;
    let remainder = n_tools % branch_count;

    let mut branch_inputs = Vec::with_capacity(branch_count);
    let mut registry_builder = ConnectorRegistry::builder();
    let mut direct_tools_json = Vec::with_capacity(n_tools);
    let mut direct_tools = BTreeMap::new();
    let mut all_domain_ops: Vec<(usize, String, &'static str, &'static str, &'static str)> =
        Vec::new();

    let mut global_idx = 0usize;

    for b_idx in 0..branch_count {
        let quota = base_per_branch + usize::from(b_idx < remainder);
        let slug = match b_idx {
            0 => "shop".to_owned(),
            1 => "notes".to_owned(),
            _ => format!("svc_{b_idx:03}"),
        };
        let branch_id = BranchId::new(&slug).map_err(|e| BenchError::Cli(e.to_string()))?;
        let uid = deterministic_branch_uid(&slug)?;

        let mut type_defs = String::new();
        let mut query_fields = String::new();
        let mut proto_ops = Vec::with_capacity(quota);
        let mut client_ops = BTreeMap::new();

        for local_idx in 0..quota {
            if b_idx == 0 && local_idx == 0 {
                let op_name = "shop_customer";
                let desc = "Lookup shop customer profile tier and region by numeric customer id";
                let _ = writeln!(
                    type_defs,
                    "type ShopCustomer {{\n  id: Int!\n  name: String!\n  tier: String!\n  region: String!\n}}"
                );
                let _ = writeln!(
                    query_fields,
                    "  \"\"\"{desc}\"\"\"\n  {op_name}(id: Int!): ShopCustomer!"
                );
                proto_ops.push(make_proto_op(
                    op_name,
                    desc,
                    "find shop customer profile and vip tier by customer id",
                    "id",
                ));
                client_ops.insert(op_name.to_owned(), SynthOpKind::ShopCustomer);
                direct_tools.insert(
                    op_name.to_owned(),
                    DirectToolSpec {
                        name: op_name.to_owned(),
                        responder: SynthOpKind::ShopCustomer,
                    },
                );
                direct_tools_json.push(make_direct_tool_json(
                    op_name,
                    "Shop Customer Lookup",
                    "Retrieve a verified shop customer profile record including VIP tier, billing region, and display name by numeric customer identifier.",
                    json!({
                        "type": "object",
                        "required": ["id"],
                        "additionalProperties": false,
                        "properties": {
                            "id": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Unique numeric shop customer account identifier."
                            },
                            "include_metadata": {
                                "type": "boolean",
                                "default": false,
                                "description": "Whether to include extended account tier metadata."
                            }
                        }
                    }),
                ));
                global_idx += 1;
                continue;
            }

            if b_idx == 0 && local_idx == 1 {
                let op_name = "shop_orders";
                let desc =
                    "List shop purchase orders total cents and payment status for a customer id";
                let _ = writeln!(
                    type_defs,
                    "type ShopOrder {{\n  id: Int!\n  customer_id: Int!\n  total_cents: Int!\n  status: String!\n}}"
                );
                let _ = writeln!(
                    query_fields,
                    "  \"\"\"{desc}\"\"\"\n  {op_name}(customer_id: Int!): [ShopOrder!]!"
                );
                proto_ops.push(make_proto_op(
                    op_name,
                    desc,
                    "list purchase orders and order totals for shop customer",
                    "customer_id",
                ));
                client_ops.insert(op_name.to_owned(), SynthOpKind::ShopOrders);
                direct_tools.insert(
                    op_name.to_owned(),
                    DirectToolSpec {
                        name: op_name.to_owned(),
                        responder: SynthOpKind::ShopOrders,
                    },
                );
                direct_tools_json.push(make_direct_tool_json(
                    op_name,
                    "Shop Customer Orders",
                    "List purchase orders, line totals in cents, and settlement status associated with a shop customer identifier.",
                    json!({
                        "type": "object",
                        "required": ["customer_id"],
                        "additionalProperties": false,
                        "properties": {
                            "customer_id": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Customer account identifier owning the orders."
                            },
                            "status_filter": {
                                "type": "string",
                                "enum": ["paid", "processing", "refunded"],
                                "description": "Optional payment lifecycle filter."
                            }
                        }
                    }),
                ));
                global_idx += 1;
                continue;
            }

            if b_idx == 1 && local_idx == 0 {
                let op_name = "notes_for_customer";
                let desc = "Fetch account support notes and onboarding summaries for a customer id";
                let _ = writeln!(
                    type_defs,
                    "type CustomerNote {{\n  note_id: String!\n  customer_id: Int!\n  summary: String!\n  author: String!\n}}"
                );
                let _ = writeln!(
                    query_fields,
                    "  \"\"\"{desc}\"\"\"\n  {op_name}(customer_id: Int!): [CustomerNote!]!"
                );
                proto_ops.push(make_proto_op(
                    op_name,
                    desc,
                    "retrieve customer support notes and account summary",
                    "customer_id",
                ));
                client_ops.insert(op_name.to_owned(), SynthOpKind::CustomerNotes);
                direct_tools.insert(
                    op_name.to_owned(),
                    DirectToolSpec {
                        name: op_name.to_owned(),
                        responder: SynthOpKind::CustomerNotes,
                    },
                );
                direct_tools_json.push(make_direct_tool_json(
                    op_name,
                    "Customer Support Notes",
                    "Fetch CRM support notes, onboarding verification summaries, and author attribution for a customer account id.",
                    json!({
                        "type": "object",
                        "required": ["customer_id"],
                        "additionalProperties": false,
                        "properties": {
                            "customer_id": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Numeric customer account identifier."
                            },
                            "limit": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 50,
                                "default": 10,
                                "description": "Maximum number of recent support notes to return."
                            }
                        }
                    }),
                ));
                global_idx += 1;
                continue;
            }

            if b_idx == 1 && local_idx == 1 {
                let op_name = "order_shipment";
                let desc =
                    "Retrieve parcel carrier tracking code and delivery status for an order id";
                let _ = writeln!(
                    type_defs,
                    "type OrderShipment {{\n  order_id: Int!\n  carrier: String!\n  tracking_code: String!\n  status: String!\n}}"
                );
                let _ = writeln!(
                    query_fields,
                    "  \"\"\"{desc}\"\"\"\n  {op_name}(order_id: Int!): OrderShipment!"
                );
                proto_ops.push(make_proto_op(
                    op_name,
                    desc,
                    "check parcel shipment carrier and delivery tracking for order",
                    "order_id",
                ));
                client_ops.insert(op_name.to_owned(), SynthOpKind::OrderShipment);
                direct_tools.insert(
                    op_name.to_owned(),
                    DirectToolSpec {
                        name: op_name.to_owned(),
                        responder: SynthOpKind::OrderShipment,
                    },
                );
                direct_tools_json.push(make_direct_tool_json(
                    op_name,
                    "Order Parcel Shipment",
                    "Retrieve logistics parcel carrier, tracking identifier, and fulfillment delivery state for a purchase order id.",
                    json!({
                        "type": "object",
                        "required": ["order_id"],
                        "additionalProperties": false,
                        "properties": {
                            "order_id": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Numeric shop order identifier."
                            }
                        }
                    }),
                ));
                global_idx += 1;
                continue;
            }

            let domain = DOMAINS[global_idx % DOMAINS.len()];
            let subsystem = SUBSYSTEMS[(global_idx / DOMAINS.len()) % SUBSYSTEMS.len()];
            let aspect = ASPECTS[(global_idx / 7) % ASPECTS.len()];
            let op_name = format!("svc{b_idx:03}_tool{global_idx:04}");
            let type_name = format!("Svc{b_idx:03}Tool{global_idx:04}Record");
            let desc =
                format!("Retrieve {domain} {subsystem} {aspect} metrics for unit{global_idx:04}");
            let example_intent =
                format!("fetch {domain} {subsystem} {aspect} record unit{global_idx:04} by id");

            let _ = writeln!(
                type_defs,
                "type {type_name} {{\n  id: Int!\n  code: String!\n  summary: String!\n  metric: Int!\n}}"
            );
            let _ = writeln!(
                query_fields,
                "  \"\"\"{desc}\"\"\"\n  {op_name}(id: Int!, filter: String): {type_name}!"
            );

            proto_ops.push(make_proto_op(&op_name, &desc, &example_intent, "id"));
            let responder = SynthOpKind::DomainLookup {
                global_idx,
                domain,
                subsystem,
            };
            client_ops.insert(op_name.clone(), responder.clone());
            direct_tools.insert(
                op_name.clone(),
                DirectToolSpec {
                    name: op_name.clone(),
                    responder,
                },
            );
            direct_tools_json.push(make_direct_tool_json(
                &op_name,
                &format!("{domain} {subsystem} {aspect} (unit{global_idx:04})"),
                &format!(
                    "Query {domain} {subsystem} {aspect} telemetry and audit record unit{global_idx:04} by numeric entity id with optional filter expression."
                ),
                json!({
                    "type": "object",
                    "required": ["id"],
                    "additionalProperties": false,
                    "properties": {
                        "id": {
                            "type": "integer",
                            "minimum": 1,
                            "description": format!("Numeric identifier for {domain} {subsystem} unit{global_idx:04}.")
                        },
                        "filter": {
                            "type": "string",
                            "maxLength": 128,
                            "description": format!("Optional {aspect} status filter expression.")
                        },
                        "region": {
                            "type": "string",
                            "enum": ["us-east", "eu-west", "ap-south"],
                            "description": "Optional regional partition hint."
                        }
                    }
                }),
            ));
            all_domain_ops.push((global_idx, op_name, domain, subsystem, aspect));
            global_idx += 1;
        }

        let schema_sdl = format!("{type_defs}\ntype Query {{\n{query_fields}}}\n");
        let synopsis = match b_idx {
            0 => "Shop customer profiles, VIP tiers, and purchase orders".to_owned(),
            1 => "Customer CRM notes and parcel shipment tracking".to_owned(),
            _ => {
                let d = DOMAINS[b_idx % DOMAINS.len()];
                let s = SUBSYSTEMS[b_idx % SUBSYSTEMS.len()];
                format!("Branch {slug} {d} {s} read service")
            }
        };
        let spec_hash = hex_encode(blake3::hash(schema_sdl.as_bytes()).as_bytes());

        branch_inputs.push(BranchInput {
            id: branch_id,
            uid,
            protocol: Protocol::Rest,
            trust: TrustTier::T1,
            endpoint: Endpoint {
                uri: format!("https://{slug}.bench.local"),
                transport: "http".to_owned(),
            },
            description: ConnectorDescription {
                connector_id: slug,
                connector_version: "0.1.0".to_owned(),
                ngc_version: "gap/0.1".to_owned(),
                protocol: ProtoProtocol::Rest as i32,
                spec_hash,
                capabilities: Some(ProtoCapabilities {
                    foreach_batch: true,
                    ..ProtoCapabilities::default()
                }),
                limits: Some(ProtoLimits {
                    max_batch_size: 64,
                    max_in_flight: 64,
                    max_page_size: 100,
                    default_timeout_ms: 5_000,
                    max_response_bytes: 4 * 1024 * 1024,
                }),
                schema_sdl,
                operations: proto_ops,
                meta: Some(MetaSummary {
                    node_categories: Vec::new(),
                    edge_predicates: Vec::new(),
                    synopsis_hint: synopsis,
                }),
                auth: None,
            },
            status: BranchStatus::Active,
        });

        let client = Arc::new(SynthBranchClient {
            ops: Arc::new(client_ops),
        });
        registry_builder = registry_builder.register(uid, client, 64);
    }

    let composition = compose(&branch_inputs)?;
    let base = Snapshot::empty_for_tenant(0, TenantId::default_tenant());
    let composed = SnapshotBuilder::new(&base, composition)
        .builder("gapbench")
        .build();

    let branch_arcs: Vec<Arc<BranchDescriptor>> = composed.branches.values().cloned().collect();
    let assembled_groups = assign_capability_groups(&branch_arcs, &GroupAssignmentConfig::new())?;
    let mut indexed_snapshot = assembled_groups.apply_to_snapshot(
        &composed,
        TenantId::default_tenant(),
        HybridIndexConfig::default(),
    )?;
    // `McpFacade::execute_gap_search` (`gap-mcp`) keys `visible_catalog` by `Query.<op_name>`
    // (`doc.path`), whereas `build_group_element_docs` uses `group.id` as `element_id`. Keying the
    // benchmark `HybridElementIndex` by `doc.path` preserves identical group-deduplicated BM25 text
    // while working across both `McpFacade` (`gap_search`) and `RouteDiscoveryService::search`.
    let mcp_aligned_docs: Vec<_> = assembled_groups
        .build_group_element_docs(&branch_arcs, TenantId::default_tenant())
        .into_iter()
        .map(|mut doc| {
            if !doc.path.is_empty() {
                doc.element_id = doc.path.clone();
            }
            doc
        })
        .collect();
    indexed_snapshot.index = Arc::new(HybridElementIndex::build(
        mcp_aligned_docs,
        HybridIndexConfig::default(),
    )?);
    let snapshot_handle = Arc::new(SnapshotHandle::new(indexed_snapshot));

    let registry = Arc::new(registry_builder.build());
    let plan_cache = Arc::new(PlanCache::new(PlanCacheConfig::default()));
    let metrics = Arc::new(TransportMetrics::new());

    let mut transport_cfg = TransportConfig::for_profile(Profile::Dev);
    transport_cfg.allow_anonymous = true;

    let app_state = Arc::new(AppState::try_new(
        Arc::clone(&snapshot_handle),
        registry,
        plan_cache,
        transport_cfg,
        metrics,
    )?);
    let engine = TransportEngine::new(app_state);
    let facade = McpFacade::builder(Arc::new(engine))
        .visibility(Arc::new(AllVisible))
        .config(FacadeConfig::default())
        .build()?;
    let discovery = RouteDiscoveryService::baseline(DiscoveryConfig::default());

    let direct_list_envelope = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "resultType": "complete",
            "ttlMs": 300_000,
            "cacheScope": "public",
            "tools": direct_tools_json
        }
    });
    let direct_tools_list_bytes = serde_json::to_vec(&direct_list_envelope)?;

    let search_cases = build_search_cases(&all_domain_ops);
    let api_lookup_tasks = build_api_lookup_tasks(&all_domain_ops);
    let cross_branch_tasks = build_cross_branch_tasks();

    Ok(SyntheticSupergraph {
        n_tools,
        branch_count,
        snapshot_handle,
        facade,
        discovery,
        direct_tools_list_bytes,
        direct_tools,
        search_cases,
        api_lookup_tasks,
        cross_branch_tasks,
    })
}

fn make_proto_op(
    name: &str,
    description: &str,
    intent: &str,
    key_field: &str,
) -> ProtoOperationDescriptor {
    ProtoOperationDescriptor {
        name: name.to_owned(),
        kind: ProtoOpKind::Query as i32,
        description: description.to_owned(),
        input_schema: None,
        output_schema: None,
        side_effect_hint: SideEffectHint::SeNone as i32,
        idempotent_hint: true,
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: vec![ProtoExample {
            intent: intent.to_owned(),
            gapql: String::new(),
            negative: false,
        }],
        cost_weight: 1,
        pagination: 0,
        key_fields: vec![key_field.to_owned()],
        annotations: std::collections::HashMap::default(),
    }
}

fn build_search_cases(
    domain_ops: &[(usize, String, &'static str, &'static str, &'static str)],
) -> Vec<LabeledSearchCase> {
    let mut out = Vec::with_capacity(40);
    out.push(LabeledSearchCase {
        query: "lookup shop customer profile vip tier by customer id".to_owned(),
        expected_op_name: "shop_customer".to_owned(),
    });
    out.push(LabeledSearchCase {
        query: "list shop purchase orders total cents and payment status".to_owned(),
        expected_op_name: "shop_orders".to_owned(),
    });
    out.push(LabeledSearchCase {
        query: "fetch account support notes and onboarding summaries for customer".to_owned(),
        expected_op_name: "notes_for_customer".to_owned(),
    });
    out.push(LabeledSearchCase {
        query: "check parcel carrier tracking code and delivery status for order".to_owned(),
        expected_op_name: "order_shipment".to_owned(),
    });

    if !domain_ops.is_empty() {
        for slot in 0..36 {
            let pick = (slot * domain_ops.len()) / 36;
            let (g_idx, ref op_name, domain, subsystem, aspect) =
                domain_ops[pick % domain_ops.len()];
            let query = match slot % 3 {
                0 => format!("retrieve {domain} {subsystem} {aspect} metrics unit{g_idx:04}"),
                1 => format!("fetch {domain} {subsystem} {aspect} record unit{g_idx:04}"),
                _ => format!("{domain} {subsystem} unit{g_idx:04} {aspect}"),
            };
            out.push(LabeledSearchCase {
                query,
                expected_op_name: op_name.clone(),
            });
        }
    }
    out
}

fn build_api_lookup_tasks(
    domain_ops: &[(usize, String, &'static str, &'static str, &'static str)],
) -> Vec<ApiLookupTask> {
    let mut tasks = Vec::with_capacity(10);
    if domain_ops.is_empty() {
        return tasks;
    }
    for idx in 0..10 {
        let pick = (idx * domain_ops.len()) / 10;
        let (g_idx, ref op_name, domain, subsystem, aspect) = domain_ops[pick % domain_ops.len()];
        let arg_id = i64::try_from(idx + 1).unwrap_or(1) * 7;
        tasks.push(ApiLookupTask {
            task_id: format!("api-lookup-{idx:02}"),
            prompt: format!(
                "Look up the {domain} {subsystem} {aspect} record for unit{g_idx:04} with id={arg_id} and report its code."
            ),
            search_query: format!("retrieve {domain} {subsystem} {aspect} metrics unit{g_idx:04}"),
            expected_op_name: op_name.clone(),
            arg_id,
            expected_code: format!("{domain}-{subsystem}-{g_idx:04}-{arg_id}"),
        });
    }
    tasks
}

fn build_cross_branch_tasks() -> Vec<CrossBranchJoinTask> {
    let mut tasks = Vec::with_capacity(8);
    for idx in 0..8 {
        let cid = i64::from(idx + 1);
        let primary_order_id = cid * 100 + 1;
        let (name, _, _) = customer_fixture(cid);
        let (carrier, _) = carrier_fixture(primary_order_id);
        tasks.push(CrossBranchJoinTask {
            task_id: format!("cross-join-{idx:02}"),
            prompt: format!(
                "Assemble a unified dossier for customer id={cid}: customer profile, all shop orders, carrier shipment status for order {primary_order_id}, and CRM support notes."
            ),
            customer_id: cid,
            primary_order_id,
            expected_customer_name: name.to_owned(),
            expected_order_count: 2,
            expected_carrier: carrier.to_owned(),
            expected_note_summary: format!("Priority onboarding verified for account {cid}"),
        });
    }
    tasks
}

/// Synthetic 24-file repository + populated [`SqliteContextStore`] for Suite `(c)` (`G-C1`).
pub struct CodeGraphFixture {
    _temp_dir: TempDir,
    /// Embedded SQLite context store (`WAL`, `Durability::Off`).
    pub store: SqliteContextStore,
    /// Tenant scope (`INV-09`).
    pub tenant_scope: TenantScope,
    /// Authenticated benchmark caller.
    pub caller: Caller,
    /// 24 virtual source files (`path -> source text`, ~1,350 tokens/file).
    pub files: BTreeMap<String, String>,
    /// Compact JSON-RPC `tools/list` response for the 3 Direct-MCP filesystem tools.
    pub direct_fs_tools_list_bytes: Vec<u8>,
    /// 10 caller-impact tasks with exact ground-truth depth-2 callers.
    pub tasks: Vec<CodeGraphTask>,
}

impl CodeGraphFixture {
    /// Simulates Direct-MCP `list_files` returning all 24 repository paths.
    pub fn list_files_json(&self) -> Result<Vec<u8>, BenchError> {
        let paths: Vec<&str> = self.files.keys().map(String::as_str).collect();
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "resultType": "complete",
                "structuredContent": { "files": paths },
                "isError": false
            }
        });
        Ok(serde_json::to_vec(&resp)?)
    }

    /// Simulates Direct-MCP `grep_files` returning matching file paths + line snippets.
    pub fn grep_files_json(&self, needle: &str) -> Result<(Vec<String>, Vec<u8>), BenchError> {
        let mut matched_files = Vec::new();
        let mut matches_json = Vec::new();
        for (path, content) in &self.files {
            let mut file_hits = Vec::new();
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(needle) {
                    file_hits.push(json!({
                        "line": line_idx + 1,
                        "text": line.trim()
                    }));
                }
            }
            if !file_hits.is_empty() {
                matched_files.push(path.clone());
                matches_json.push(json!({
                    "path": path,
                    "hits": file_hits
                }));
            }
        }
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "resultType": "complete",
                "structuredContent": { "matches": matches_json },
                "isError": false
            }
        });
        Ok((matched_files, serde_json::to_vec(&resp)?))
    }

    /// Simulates Direct-MCP `read_file` returning the full UTF-8 file contents.
    pub fn read_file_json(&self, path: &str) -> Result<Vec<u8>, BenchError> {
        let content = self
            .files
            .get(path)
            .ok_or_else(|| BenchError::Cli(format!("missing virtual file `{path}`")))?;
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "resultType": "complete",
                "structuredContent": {
                    "path": path,
                    "content": content
                },
                "isError": false
            }
        });
        Ok(serde_json::to_vec(&resp)?)
    }

    /// Executes GAP context-graph FTS5 `search_nodes` + bounded `traverse(Inbound, max_depth=2)`
    /// and returns `(caller_symbols, compact_response_bytes)`.
    pub fn query_context_graph_impact(
        &self,
        symbol: &str,
    ) -> Result<(BTreeSet<String>, Vec<u8>), BenchError> {
        let hits = self
            .store
            .search_nodes(&self.tenant_scope, &self.caller, symbol, 5, None)?;
        let target = hits
            .iter()
            .find(|n| n.label == symbol)
            .or_else(|| hits.first())
            .ok_or_else(|| {
                BenchError::Cli(format!("symbol `{symbol}` not found in ContextStore"))
            })?;

        let calls_edge = EdgeType::new("CALLS")?;
        let mut tq = TraverseQuery::new(target.id.clone());
        tq.edge_types = Some(vec![calls_edge]);
        tq.direction = Direction::Inbound;
        tq.max_depth = 2;
        tq.strategy = TraverseStrategy::Bfs;
        tq.first = 50;

        let walk = self.store.traverse(&self.tenant_scope, &self.caller, &tq)?;

        let mut callers = BTreeSet::new();
        let mut compact_nodes = Vec::with_capacity(walk.nodes.len());
        for node in &walk.nodes {
            if node.id != target.id {
                callers.insert(node.label.clone());
            }
            compact_nodes.push(json!({
                "id": node.id.as_str(),
                "label": node.label,
                "summary": node.summary,
                "file": node.props.get("file")
            }));
        }
        let compact_edges: Vec<Value> = walk
            .edges
            .iter()
            .map(|e| {
                json!({
                    "type": e.edge_type.as_str(),
                    "from": e.from.as_str(),
                    "to": e.to.as_str()
                })
            })
            .collect();

        let payload = json!({
            "data": {
                "gap": {
                    "traverse": {
                        "target": target.label,
                        "nodes": compact_nodes,
                        "edges": compact_edges,
                        "stats": {
                            "visited": walk.stats.visited,
                            "returned": walk.stats.returned,
                            "depthReached": walk.stats.depth_reached
                        }
                    }
                }
            }
        });
        Ok((callers, serde_json::to_vec(&payload)?))
    }
}

const CODE_MODULES: &[(&str, &str)] = &[
    ("payments", "token_validator"),
    ("payments", "checkout_gate"),
    ("payments", "settlement_engine"),
    ("orders", "cart_pipeline"),
    ("orders", "invoice_dispatch"),
    ("orders", "refund_coordinator"),
    ("ledger", "journal_writer"),
    ("ledger", "reconcile_worker"),
    ("ledger", "balance_projector"),
    ("identity", "session_verifier"),
    ("identity", "policy_guard"),
    ("identity", "audit_emitter"),
    ("shipping", "carrier_router"),
    ("shipping", "manifest_builder"),
    ("shipping", "customs_clearance"),
    ("inventory", "stock_allocator"),
    ("inventory", "warehouse_sync"),
    ("inventory", "reorder_planner"),
    ("analytics", "funnel_aggregator"),
    ("analytics", "cohort_exporter"),
    ("alerts", "pager_notifier"),
    ("alerts", "slo_watcher"),
    ("gateway", "http_entrypoint"),
    ("gateway", "batch_orchestrator"),
];

/// Builds the 24-file synthetic codebase (~32k source tokens) and indexes its 72 symbols + `CALLS`
/// edges into [`SqliteContextStore`].
pub fn build_code_graph_fixture() -> Result<CodeGraphFixture, BenchError> {
    let temp_dir = TempDir::new()?;
    let mut cfg = SqliteConfig::new(temp_dir.path().join("code_graph.sqlite"));
    cfg.durability = Durability::Off;
    let store = SqliteContextStore::open(cfg, Arc::new(SystemClock))?;

    let tenant_scope = TenantScope::new(TenantId::default_tenant());
    let caller = Caller::new("gapbench-agent");
    let fn_node_type = NodeType::new("Function")?;
    let calls_edge_type = EdgeType::new("CALLS")?;
    let prov = Provenance::new("gapbench-repo-indexer");

    let mut file_functions: Vec<Vec<(String, Vec<String>, String)>> =
        (0..24).map(|_| Vec::new()).collect();

    let mut tasks = Vec::with_capacity(10);

    for t_idx in 0..10 {
        let leaf_fn = format!("core_verify_invariant_{t_idx:02}");
        let direct_a = format!("service_apply_workflow_{t_idx:02}_a");
        let direct_b = format!("service_apply_workflow_{t_idx:02}_b");
        let entry_a = format!("api_handle_endpoint_{t_idx:02}_alpha");
        let entry_b = format!("job_run_scheduled_{t_idx:02}_beta");

        let leaf_file = t_idx;
        let direct_a_file = 10 + (t_idx % 10);
        let direct_b_file = 10 + ((t_idx + 3) % 10);
        let entry_a_file = 20 + (t_idx % 4);
        let entry_b_file = 20 + ((t_idx + 2) % 4);

        file_functions[leaf_file].push((
            leaf_fn.clone(),
            Vec::new(),
            format!(
                "Validates domain invariant #{t_idx:02} checksums, quota bounds, and tenant isolation tags."
            ),
        ));
        file_functions[direct_a_file].push((
            direct_a.clone(),
            vec![leaf_fn.clone()],
            format!(
                "Executes primary transactional workflow #{t_idx:02}A after verifying core invariant #{t_idx:02}."
            ),
        ));
        file_functions[direct_b_file].push((
            direct_b.clone(),
            vec![leaf_fn.clone()],
            format!(
                "Executes secondary reconciliation workflow #{t_idx:02}B gated on core invariant #{t_idx:02}."
            ),
        ));
        file_functions[entry_a_file].push((
            entry_a.clone(),
            vec![direct_a.clone()],
            format!("HTTP gateway handler #{t_idx:02}-alpha dispatching workflow #{t_idx:02}A."),
        ));
        file_functions[entry_b_file].push((
            entry_b.clone(),
            vec![direct_b.clone()],
            format!(
                "Scheduled batch worker #{t_idx:02}-beta invoking reconciliation workflow #{t_idx:02}B."
            ),
        ));

        let mut expected_callers = BTreeSet::new();
        expected_callers.insert(direct_a);
        expected_callers.insert(direct_b);
        expected_callers.insert(entry_a);
        expected_callers.insert(entry_b);

        tasks.push(CodeGraphTask {
            task_id: format!("code-impact-{t_idx:02}"),
            prompt: format!(
                "Identify every direct and transitive caller (up to 2 hops) impacted by changing `{leaf_fn}`."
            ),
            target_symbol: leaf_fn,
            expected_callers,
        });
    }

    let mut helper_seq = 0usize;
    for (f_idx, fns) in file_functions.iter_mut().enumerate() {
        while fns.len() < 3 {
            let helper_name = format!("module_{f_idx:02}_helper_{helper_seq:03}");
            fns.push((
                helper_name,
                Vec::new(),
                format!(
                    "Internal formatting, telemetry histogram recording, and structured audit helper #{helper_seq:03}."
                ),
            ));
            helper_seq += 1;
        }
    }

    let mut files = BTreeMap::new();
    let mut new_nodes = Vec::new();
    let mut pending_edges: Vec<(String, String)> = Vec::new();

    for (f_idx, fns) in file_functions.iter().enumerate() {
        let (pkg, raw_mod) = CODE_MODULES[f_idx % CODE_MODULES.len()];
        let mod_slug = raw_mod.trim().to_ascii_lowercase();
        let file_path = format!("src/{pkg}/{mod_slug}_{f_idx:02}.rs");
        let mut src = String::with_capacity(5600);
        let _ = writeln!(
            src,
            "//! Module `{pkg}::{mod_slug}_{f_idx:02}` — deterministic production service implementation."
        );
        let _ = writeln!(
            src,
            "//! Enforces tenant scope isolation, structured error propagation, and bounded retry budgets.\n"
        );

        for (fn_idx, (fn_name, callees, summary)) in fns.iter().enumerate() {
            let _ = writeln!(src, "/// {summary}");
            let _ = writeln!(
                src,
                "pub fn {fn_name}(tenant: &str, entity_id: i64, payload: &[u8]) -> Result<i64, String> {{"
            );
            let _ = writeln!(src, "    if tenant.trim().is_empty() || entity_id <= 0 {{");
            let _ = writeln!(
                src,
                "        return Err(String::from(\"invalid tenant scope or non-positive entity identifier\"));"
            );
            let _ = writeln!(src, "    }}");
            let _ = writeln!(src, "    let mut acc: i64 = entity_id.saturating_mul(31);");
            for step in 0..14 {
                let _ = writeln!(
                    src,
                    "    // Step {step:02} ({fn_idx}): deterministic state validation, checksum mixing, and budget accounting"
                );
                let _ = writeln!(
                    src,
                    "    let byte_sample_{step:02} = i64::from(payload.get({step}).copied().unwrap_or({}));",
                    (f_idx + step + 7) % 251
                );
                let _ = writeln!(
                    src,
                    "    acc = acc.wrapping_add(byte_sample_{step:02}.saturating_mul({}));",
                    (step + 3) * 17
                );
            }
            for callee in callees {
                let _ = writeln!(
                    src,
                    "    let sub_val = {callee}(tenant, entity_id, payload)?;"
                );
                let _ = writeln!(src, "    acc = acc.saturating_add(sub_val);");
                pending_edges.push((fn_name.clone(), callee.clone()));
            }
            let _ = writeln!(src, "    Ok(acc)\n}}\n");

            let node_id = NodeId::new(format!("fn:{fn_name}"))?;
            let mut props = JsonMap::new();
            props.insert("file".to_owned(), Value::String(file_path.clone()));
            props.insert(
                "signature".to_owned(),
                Value::String(format!(
                    "pub fn {fn_name}(tenant: &str, entity_id: i64, payload: &[u8]) -> Result<i64, String>"
                )),
            );
            new_nodes.push(NewNode {
                id: Some(node_id),
                node_type: fn_node_type.clone(),
                label: fn_name.clone(),
                summary: Some(summary.clone()),
                props,
                scope: ScopeSpec::Global,
                provenance: prov.clone(),
            });
        }

        files.insert(file_path, src);
    }

    let mut new_edges = Vec::with_capacity(pending_edges.len());
    for (caller_fn, callee_fn) in pending_edges {
        new_edges.push(NewEdge {
            id: None,
            edge_type: calls_edge_type.clone(),
            from: NodeId::new(format!("fn:{caller_fn}"))?,
            to: NodeId::new(format!("fn:{callee_fn}"))?,
            props: JsonMap::new(),
            confidence: Some(1.0),
            resolution: EdgeResolution::Exact,
            provenance: prov.clone(),
        });
    }

    store.put_batch(&tenant_scope, &caller, &new_nodes, &new_edges)?;

    let fs_tools = vec![
        make_direct_tool_json(
            "list_files",
            "List Workspace Files",
            "List all source file paths in the repository workspace tree.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prefix": { "type": "string", "description": "Optional directory prefix." }
                }
            }),
        ),
        make_direct_tool_json(
            "grep_files",
            "Search File Contents",
            "Search repository source files for an exact identifier or substring match.",
            json!({
                "type": "object",
                "required": ["pattern"],
                "additionalProperties": false,
                "properties": {
                    "pattern": { "type": "string", "description": "Identifier pattern to match." }
                }
            }),
        ),
        make_direct_tool_json(
            "read_file",
            "Read Source File",
            "Read the complete UTF-8 source text of a workspace file by relative path.",
            json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "description": "Relative repository file path." }
                }
            }),
        ),
    ];
    let fs_envelope = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "resultType": "complete",
            "ttlMs": 300_000,
            "cacheScope": "public",
            "tools": fs_tools
        }
    });
    let direct_fs_tools_list_bytes = serde_json::to_vec(&fs_envelope)?;

    Ok(CodeGraphFixture {
        _temp_dir: temp_dir,
        store,
        tenant_scope,
        caller,
        files,
        direct_fs_tools_list_bytes,
        tasks,
    })
}

/// Suite selector parsed from `--suite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteFilter {
    /// Run all three suites (`api_lookup`, `cross_branch_join`, `code_graph`).
    All,
    /// Suite `(a)`: single-operation API lookup across `N` tools.
    ApiLookup,
    /// Suite `(b)`: cross-branch multi-service join.
    CrossBranchJoin,
    /// Suite `(c)`: repository caller / impact analysis (`G-C1`).
    CodeGraph,
}

impl SuiteFilter {
    /// Parses a CLI `--suite` argument.
    pub fn parse(raw: &str) -> Result<Self, BenchError> {
        match raw.trim() {
            "all" => Ok(Self::All),
            "api_lookup" => Ok(Self::ApiLookup),
            "cross_branch_join" => Ok(Self::CrossBranchJoin),
            "code_graph" => Ok(Self::CodeGraph),
            other => Err(BenchError::Cli(format!(
                "invalid `--suite` value `{other}` (expected all|api_lookup|cross_branch_join|code_graph)"
            ))),
        }
    }

    /// Returns `true` when `target` is enabled under this filter.
    #[must_use]
    pub const fn includes(self, target: Self) -> bool {
        matches!(self, Self::All) || matches!((self, target), (a, b) if a as u8 == b as u8)
    }
}
