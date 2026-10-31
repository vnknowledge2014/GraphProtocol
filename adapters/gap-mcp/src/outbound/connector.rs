//! The outbound MCP connector: an MCP server as an NGC branch (`CMP-010..015`, `NGC-010`, `NGC-020`, `NGC-026`, `INV-12`, `INV-15`, `INV-18`).

use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use futures_util::stream::{self, StreamExt};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, HealthRequest,
    HealthResponse, Limits, MetaSummary, OpKind, PaginationKind, Protocol, QueryRequest,
    QueryResponse, RowSetChunk, ServingStatus, query_response,
};
use serde_json::{Map, Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::config::{ConfigError, McpConfig};
use super::error::{McpError, Redactor};
use super::map::{self, Mapped, MappedOp, Resource, Target, Tool};
use super::session::{ClientInfo, Era, Session};
use super::transport::{CallOptions, McpTransport};

/// Retry hint of a saturated connector (`NGC-071`).
const SATURATED_RETRY_AFTER_MS: u32 = 100;

/// Result of one discovery round.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// Tools listed by the server.
    pub tools: Vec<Tool>,
    /// Resources listed by the server.
    pub resources: Vec<Resource>,
    /// Operations and SDL.
    pub mapped: Mapped,
    /// Change detector (`NGC-010`).
    pub spec_hash: String,
    /// `ttlMs` of the `tools/list` result (modern era), if present.
    pub ttl_ms: Option<u64>,
    /// `cacheScope` of the `tools/list` result (modern era), if present.
    pub cache_scope: Option<String>,
}

struct Inner<T> {
    session: Session<T>,
    cfg: McpConfig,
    redactor: Redactor,
    discovery: Mutex<Option<Arc<Discovery>>>,
    discover_lock: tokio::sync::Mutex<()>,
    gate: Arc<Semaphore>,
    last_discovery_failed: std::sync::atomic::AtomicBool,
}

/// An MCP server exposed as an NGC [`Connector`].
pub struct McpConnector<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for McpConnector<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> std::fmt::Debug for McpConnector<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnector")
            .field("connector_id", &self.inner.cfg.connector_id)
            .finish_non_exhaustive()
    }
}

struct GuardedStream {
    inner: BoxStream<QueryResponse>,
    _permit: OwnedSemaphorePermit,
}

impl Stream for GuardedStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

fn invalid(message: &str) -> NgcErrorInfo {
    NgcErrorInfo::new(NgcErrorCode::InvalidArgument, message)
}

fn check_scope(scope: &CallScope) -> NgcResult<Duration> {
    if scope.cancel.is_cancelled() {
        return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
    }
    let now = Instant::now();
    if now >= scope.deadline {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "call deadline exceeded",
        ));
    }
    Ok(scope.deadline.duration_since(now))
}

impl<T: McpTransport> McpConnector<T> {
    /// Builds a connector over `transport`; nothing is sent until the first `describe` or `query`.
    pub fn new(transport: Arc<T>, cfg: McpConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let redactor = Redactor::new(cfg.redact.clone());
        let client = ClientInfo {
            name: cfg.client_name.clone(),
            version: cfg.client_version.clone(),
        };
        let permits = usize::try_from(cfg.max_in_flight).unwrap_or(usize::MAX);
        Ok(Self {
            inner: Arc::new(Inner {
                session: Session::new(transport, client),
                cfg,
                redactor,
                discovery: Mutex::new(None),
                discover_lock: tokio::sync::Mutex::new(()),
                gate: Arc::new(Semaphore::new(permits)),
                last_discovery_failed: std::sync::atomic::AtomicBool::new(false),
            }),
        })
    }

    /// The session (era, capabilities, teardown).
    #[must_use]
    pub fn session(&self) -> &Session<T> {
        &self.inner.session
    }

    /// The last discovery result, if any.
    #[must_use]
    pub fn discovery(&self) -> Option<Arc<Discovery>> {
        self.inner
            .discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn opts(&self, timeout: Duration, cancel: &CancellationToken) -> CallOptions {
        CallOptions {
            timeout,
            cancel: cancel.clone(),
        }
    }

    async fn call_with_one_retry(
        &self,
        method: &str,
        params: Value,
        opts: CallOptions,
    ) -> Result<Value, McpError> {
        // Discovery calls are side-effect free: one retry on a connection failure (`CMP-010`).
        match self
            .inner
            .session
            .call(method, params.clone(), None, opts.clone())
            .await
        {
            Err(McpError::Connect) => self.inner.session.call(method, params, None, opts).await,
            other => other,
        }
    }

    /// Drains a paginated list (`tools/list`, `resources/list`) under the page and item caps.
    async fn drain(
        &self,
        method: &str,
        key: &str,
        opts: &CallOptions,
    ) -> Result<(Vec<Value>, Option<u64>, Option<String>), McpError> {
        let cfg = &self.inner.cfg;
        let modern = self.inner.session.era() == Some(Era::Modern);
        let mut items: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        let (mut ttl, mut scope) = (None, None);
        for page_no in 0..cfg.max_pages {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({"cursor": c}));
            let result = self
                .call_with_one_retry(method, params, opts.clone())
                .await?;
            let modern_now = modern || self.inner.session.era() == Some(Era::Modern);
            if modern_now && page_no == 0 {
                // Caching hints: an absent or negative `ttlMs` means "immediately stale", an unknown `cacheScope` means `private`.
                ttl = Some(
                    result
                        .get("ttlMs")
                        .and_then(Value::as_i64)
                        .map_or(0, |t| u64::try_from(t).unwrap_or(0)),
                );
                scope = Some(
                    if result.get("cacheScope").and_then(Value::as_str) == Some("public") {
                        "public"
                    } else {
                        "private"
                    }
                    .to_owned(),
                );
            }
            let page = result
                .get(key)
                .and_then(Value::as_array)
                .ok_or(McpError::Protocol("list result lacks its items array"))?;
            if items.len().saturating_add(page.len()) > cfg.max_items {
                return Err(McpError::Protocol("list exceeds the item cap"));
            }
            items.extend(page.iter().cloned());
            match result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                None => return Ok((items, ttl, scope)),
                Some(next) => {
                    if seen.iter().any(|s| s == next) {
                        return Err(McpError::Protocol("server repeats a pagination cursor"));
                    }
                    seen.push(next.to_owned());
                    cursor = Some(next.to_owned());
                }
            }
        }
        Err(McpError::Protocol("list exceeds the page cap"))
    }

    async fn run_discovery(
        &self,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<Arc<Discovery>, McpError> {
        let opts = self.opts(timeout.min(self.inner.cfg.discovery_timeout), cancel);
        let (tool_items, ttl_ms, cache_scope) = self.drain("tools/list", "tools", &opts).await?;
        let wants_resources = self
            .inner
            .session
            .server_capabilities()
            .get("resources")
            .is_some_and(Value::is_object);
        let resource_items = if wants_resources {
            self.drain("resources/list", "resources", &opts).await?.0
        } else {
            Vec::new()
        };
        let tools = map::parse_tools(&tool_items);
        let resources = map::parse_resources(&resource_items);
        let mapped = map::map_discovery(&tools, &resources, &self.inner.cfg.classification);
        let spec_hash = map::spec_hash(&tools, &resources);
        Ok(Arc::new(Discovery {
            tools,
            resources,
            mapped,
            spec_hash,
            ttl_ms,
            cache_scope,
        }))
    }

    /// Returns the cached discovery or performs it (once, even under concurrent callers).
    async fn ensure_discovery(&self, scope: &CallScope) -> Result<Arc<Discovery>, McpError> {
        if let Some(found) = self.discovery() {
            return Ok(found);
        }
        let _guard = self.inner.discover_lock.lock().await;
        if let Some(found) = self.discovery() {
            return Ok(found);
        }
        let remaining = scope.deadline.saturating_duration_since(Instant::now());
        match self.run_discovery(&scope.cancel, remaining).await {
            Ok(found) => {
                *self
                    .inner
                    .discovery
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&found));
                self.inner
                    .last_discovery_failed
                    .store(false, Ordering::Release);
                Ok(found)
            }
            Err(err) => {
                self.inner
                    .last_discovery_failed
                    .store(true, Ordering::Release);
                Err(err)
            }
        }
    }

    /// Re-reads the tool and resource lists (drift polling is `WP-M6-10`'s scheduler); `true` when the change detector moved.
    pub async fn refresh(&self, cancel: &CancellationToken) -> Result<bool, NgcErrorInfo> {
        let _guard = self.inner.discover_lock.lock().await;
        let fresh = self
            .run_discovery(cancel, self.inner.cfg.discovery_timeout)
            .await
            .map_err(|e| {
                self.inner
                    .last_discovery_failed
                    .store(true, Ordering::Release);
                e.into_ngc(&self.inner.redactor)
            })?;
        self.inner
            .last_discovery_failed
            .store(false, Ordering::Release);
        let changed = self
            .discovery()
            .is_none_or(|old| old.spec_hash != fresh.spec_hash);
        *self
            .inner
            .discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(fresh);
        Ok(changed)
    }

    fn description(&self, disc: &Discovery) -> ConnectorDescription {
        let cfg = &self.inner.cfg;
        ConnectorDescription {
            connector_id: cfg.connector_id.clone(),
            connector_version: cfg.connector_version.clone(),
            ngc_version: NGC_WIRE_VERSION.to_owned(),
            protocol: Protocol::Mcp as i32,
            spec_hash: disc.spec_hash.clone(),
            capabilities: Some(Capabilities {
                foreach_batch: false,
                predicate_ops: Vec::new(),
                order_by: false,
                pagination: PaginationKind::PaginationNone as i32,
                native_traverse: false,
                subscribe: false,
                tasks: false,
                dry_run: false,
                idempotency: false,
                sketch: false,
                explain: false,
                as_of: false,
            }),
            limits: Some(Limits {
                max_batch_size: cfg.max_batch_size,
                max_in_flight: cfg.max_in_flight,
                max_page_size: 0,
                default_timeout_ms: u32::try_from(cfg.call_timeout.as_millis()).unwrap_or(u32::MAX),
                max_response_bytes: cfg.max_response_bytes,
            }),
            schema_sdl: disc.mapped.sdl.clone(),
            operations: disc
                .mapped
                .ops
                .values()
                .map(|op| op.descriptor.clone())
                .collect(),
            meta: Some(MetaSummary {
                node_categories: disc.mapped.node_categories.clone(),
                edge_predicates: Vec::new(),
                synopsis_hint: String::new(),
            }),
            auth: None,
        }
    }

    fn acquire(&self) -> NgcResult<OwnedSemaphorePermit> {
        Arc::clone(&self.inner.gate)
            .try_acquire_owned()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::ResourceExhausted,
                    "connector max_in_flight capacity exhausted",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })
    }

    /// One upstream call; counts the request in `attempts` before it is sent.
    async fn call_op(
        &self,
        op: &MappedOp,
        args: &Map<String, Value>,
        scope: &CallScope,
        attempts: &AtomicU32,
    ) -> NgcResult<Vec<Value>> {
        let remaining = check_scope(scope)?;
        let timeout = self.inner.cfg.call_timeout.min(remaining);
        let arguments = map::call_arguments(op, args)?;
        if matches!(op.target, Target::Tool(_)) {
            map::validate_arguments(&op.input_schema, &arguments)?;
        }
        let (method, params, name) = match &op.target {
            Target::Tool(tool) => (
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                tool.clone(),
            ),
            Target::ReadResource => {
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| invalid("argument `uri` must be a non-empty string"))?;
                ("resources/read", json!({"uri": uri}), uri.to_owned())
            }
        };
        attempts.fetch_add(1, Ordering::AcqRel);
        let redactor = &self.inner.redactor;
        let result = self
            .inner
            .session
            .call(
                method,
                params,
                Some(&name),
                self.opts(timeout, &scope.cancel),
            )
            .await
            .map_err(|e| e.into_ngc(redactor))?;
        map::map_call_result(&result, redactor)
    }

    fn prepare(&self, disc: &Discovery, request: &QueryRequest) -> NgcResult<MappedOp> {
        let Some(op) = disc.mapped.ops.get(&request.operation) else {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::NotFound,
                "unknown query operation",
            ));
        };
        if request.predicate.is_some() {
            return Err(invalid(
                "predicate pushdown is not supported by this connector",
            ));
        }
        if !request.as_of.is_empty() {
            return Err(invalid("as_of is not supported by this connector"));
        }
        if !request.order_by.is_empty() {
            return Err(invalid("order_by is not supported by this connector"));
        }
        if request
            .page
            .as_ref()
            .is_some_and(|p| p.limit > 0 || !p.cursor.is_empty() || p.offset > 0)
        {
            return Err(invalid("pagination is not supported by this connector"));
        }
        if op.kind != OpKind::Query {
            return Err(invalid(
                "mutations are not supported by this connector in v0",
            ));
        }
        Ok(op.clone())
    }
}

fn rows_response(rows: Vec<Value>, max_bytes: u64) -> (Vec<prost_types::Struct>, bool) {
    let mut out = Vec::with_capacity(rows.len());
    let mut acc: u64 = 0;
    for row in rows {
        let Value::Object(map) = row else { continue };
        let size =
            u64::try_from(serde_json::to_vec(&map).map_or(0, |v| v.len())).unwrap_or(u64::MAX);
        if acc.saturating_add(size) > max_bytes {
            return (out, true);
        }
        acc = acc.saturating_add(size);
        out.push(json_map_to_prost_struct(&map));
    }
    (out, false)
}

impl<T: McpTransport> Connector for McpConnector<T> {
    async fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        check_scope(&scope)?;
        let disc = self
            .ensure_discovery(&scope)
            .await
            .map_err(|e| e.into_ngc(&self.inner.redactor))?;
        if !request.known_spec_hash.is_empty() && request.known_spec_hash == disc.spec_hash {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }
        Ok(DescribeResponse {
            not_modified: false,
            description: Some(self.description(&disc)),
        })
    }

    async fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        check_scope(&scope)?;
        let disc = self
            .ensure_discovery(&scope)
            .await
            .map_err(|e| e.into_ngc(&self.inner.redactor))?;
        let op = self.prepare(&disc, &request)?;
        let permit = self.acquire()?;
        let base_args = request
            .arguments
            .as_ref()
            .map_or_else(Map::new, prost_struct_to_json_map);
        let attempts = Arc::new(AtomicU32::new(0));
        let max_bytes = self.inner.cfg.max_response_bytes;

        if request.foreach.is_empty() {
            let this = self.clone();
            let attempts_c = Arc::clone(&attempts);
            let started = Instant::now();
            let inner: BoxStream<QueryResponse> = Box::pin(
                stream::once(async move {
                    let rows = this.call_op(&op, &base_args, &scope, &attempts_c).await?;
                    let (rows, truncated) = rows_response(rows, max_bytes);
                    let total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
                    let latency_us =
                        u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
                    Ok::<_, NgcErrorInfo>(vec![
                        QueryResponse {
                            chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                                foreach_index: 0,
                                rows,
                                last_for_index: true,
                                error: None,
                            })),
                        },
                        QueryResponse {
                            chunk: Some(query_response::Chunk::Complete(Complete {
                                next_cursor: String::new(),
                                truncated,
                                rows_total: total,
                                upstream_calls: attempts_c.load(Ordering::Acquire),
                                latency_us,
                            })),
                        },
                    ])
                })
                .flat_map(|res| match res {
                    Ok(items) => stream::iter(items.into_iter().map(Ok)).boxed(),
                    Err(err) => stream::iter(vec![Err(err)]).boxed(),
                }),
            );
            return Ok(Box::pin(GuardedStream {
                inner,
                _permit: permit,
            }));
        }

        // `foreach` (CMP-015): bounded fan-out, one upstream request per distinct key, honest `upstream_calls`.
        let cfg = ForeachConfig::new(1, self.inner.cfg.fanout_concurrency.max(1));
        let this = self.clone();
        let attempts_c = Arc::clone(&attempts);
        let scope_c = scope.clone();
        let run = foreach(
            request.foreach,
            cfg,
            &scope,
            move |batch: Vec<prost_types::Struct>| {
                let this = this.clone();
                let op = op.clone();
                let base_args = base_args.clone();
                let scope = scope_c.clone();
                let attempts = Arc::clone(&attempts_c);
                async move {
                    let mut results: Vec<NgcResult<Vec<prost_types::Struct>>> =
                        Vec::with_capacity(batch.len());
                    for set in &batch {
                        let mut merged = base_args.clone();
                        merged.extend(prost_struct_to_json_map(set));
                        match this.call_op(&op, &merged, &scope, &attempts).await {
                            Ok(rows) => {
                                let (rows, _truncated) = rows_response(rows, max_bytes);
                                results.push(Ok(rows));
                            }
                            Err(err)
                                if matches!(
                                    err.code,
                                    NgcErrorCode::Cancelled | NgcErrorCode::DeadlineExceeded
                                ) =>
                            {
                                return Err(err);
                            }
                            Err(err) => results.push(Err(err)),
                        }
                    }
                    Ok(results)
                }
            },
        )?;
        let (chunks, _sdk_calls) = run.into_parts();
        struct State {
            chunks: BoxStream<RowSetChunk>,
            attempts: Arc<AtomicU32>,
            rows_total: u64,
            done: bool,
        }
        let inner: BoxStream<QueryResponse> = Box::pin(stream::unfold(
            State {
                chunks,
                attempts,
                rows_total: 0,
                done: false,
            },
            |mut st| async move {
                if st.done {
                    return None;
                }
                match st.chunks.next().await {
                    Some(Ok(chunk)) => {
                        st.rows_total = st
                            .rows_total
                            .saturating_add(u64::try_from(chunk.rows.len()).unwrap_or(u64::MAX));
                        Some((
                            Ok(QueryResponse {
                                chunk: Some(query_response::Chunk::Rows(chunk)),
                            }),
                            st,
                        ))
                    }
                    Some(Err(err)) => {
                        st.done = true;
                        Some((Err(err), st))
                    }
                    None => {
                        st.done = true;
                        let complete = Complete {
                            next_cursor: String::new(),
                            truncated: false,
                            rows_total: st.rows_total,
                            upstream_calls: st.attempts.load(Ordering::Acquire),
                            latency_us: 0,
                        };
                        Some((
                            Ok(QueryResponse {
                                chunk: Some(query_response::Chunk::Complete(complete)),
                            }),
                            st,
                        ))
                    }
                }
            },
        ));
        Ok(Box::pin(GuardedStream {
            inner,
            _permit: permit,
        }))
    }

    async fn health(&self, _request: HealthRequest, scope: CallScope) -> NgcResult<HealthResponse> {
        check_scope(&scope)?;
        let failed = self.inner.last_discovery_failed.load(Ordering::Acquire);
        let era = match self.inner.session.era() {
            Some(Era::Modern) => "2026-07-28",
            Some(Era::Legacy) => "2025-11-25",
            None => "unknown",
        };
        let mut details = Map::new();
        details.insert("protocol_version".to_owned(), json!(era));
        details.insert(
            "tools".to_owned(),
            json!(self.discovery().map_or(0, |d| d.tools.len())),
        );
        Ok(HealthResponse {
            status: if failed {
                ServingStatus::NotServing as i32
            } else {
                ServingStatus::Serving as i32
            },
            version: NGC_WIRE_VERSION.to_owned(),
            details: Some(json_map_to_prost_struct(&details)),
        })
    }
}
