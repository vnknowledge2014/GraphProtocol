//! Inbound `A2A v1.0` HTTP+JSON, JSON-RPC 2.0, and SSE [`HttpModule`] facade (`CMP-020`, `CMP-021`, `CMP-023`, `INV-20`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gap_core::clock::{Clock, SystemClock};
use gap_core::context::RequestContext;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;
use gap_core::traits::{
    BoxFuture, DiscoveryRequest, DiscoveryResolution, DiscoveryService, HttpExchangeRequest,
    HttpExchangeResponse, HttpModule,
};
use gap_sec::crypto::Ed25519SigningKey;
use gap_sec::json::JsonValue;

use crate::card::{AgentCardConfig, build_agent_card};
use crate::wire::{
    A2A_CONTENT_TYPE, A2A_EXTENSIONS_HEADER, A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER, A2aArtifact,
    A2aMessage, A2aPart, A2aRole, A2aStreamEvent, A2aTask, A2aTaskState, A2aTaskStatus,
    DEFAULT_A2A_MAX_HOPS, GAP_TRACE_EXT_URI, TraceExtension, encode_http_sse_stream,
    encode_jsonrpc_sse_stream, opt_string, parse_optional_map,
};

const PREFIXES: &[&str] = &[
    "/.well-known/agent-card.json",
    "/a2a",
    "/message:send",
    "/message:stream",
    "/tasks",
];

/// Inbound `SendMessage` request mode/metadata extracted from HTTP+JSON or JSON-RPC (`CMP-021`).
#[derive(Debug, Clone, PartialEq)]
pub struct InboundSendRequest {
    /// Incoming user message (`ROLE_USER` expected).
    pub message: A2aMessage,
    /// When `true`, return immediately after queueing instead of blocking for completion (`Q-13`).
    pub return_immediately: bool,
    /// Merged request + message trace metadata.
    pub trace: TraceExtension,
    /// Optional opaque routing tenant echoed from `AgentInterface.tenant` (`SEC-010`: routing hint only).
    pub routing_tenant: Option<String>,
}

/// Backend contract bridging `gap-a2a` inbound RPCs onto `TaskEngine` / execution (`CMP-021`).
///
/// Implementations **MUST** derive authorization and partition keys solely from
/// `ctx.tenant_scope()` and never from A2A payload `tenant` routing hints (`SEC-010`, `INV-09`).
pub trait A2aTaskBackend: Send + Sync + std::fmt::Debug {
    /// Submits a GAP-QL `SINGLE` task derived from a structured `{ gapql, variables }` part.
    fn submit_gapql<'a>(
        &'a self,
        ctx: &'a RequestContext,
        gapql: String,
        variables: Option<JsonValue>,
        req: &'a InboundSendRequest,
    ) -> BoxFuture<'a, GapResult<A2aTask>>;

    /// Resumes an interrupted task (`TASK_STATE_INPUT_REQUIRED`) via `message.taskId` (`Q-14`).
    fn resume_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
        req: &'a InboundSendRequest,
    ) -> BoxFuture<'a, GapResult<A2aTask>>;

    /// Reads current task state within `ctx.tenant_scope()`.
    fn get_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
    ) -> BoxFuture<'a, GapResult<Option<A2aTask>>>;

    /// Lists tasks visible to `ctx.tenant_scope()` optionally filtered by `context_id`.
    fn list_tasks<'a>(
        &'a self,
        ctx: &'a RequestContext,
        context_id: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, GapResult<Vec<A2aTask>>>;

    /// Requests idempotent cancellation (`RUN-084`, `CMP-021`).
    fn cancel_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
    ) -> BoxFuture<'a, GapResult<A2aTask>>;

    /// Emits `SubscribeToTask` / `SendStreamingMessage` events starting with the `Task` snapshot (`Q-14`).
    fn subscribe_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
    ) -> BoxFuture<'a, GapResult<Vec<A2aStreamEvent>>>;

    /// Persists an `INPUT_REQUIRED` discovery/resolution task so `message.taskId` continuation can resume it (`CMP-021`).
    fn record_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task: A2aTask,
    ) -> BoxFuture<'a, GapResult<()>>;
}

#[derive(Debug, Clone)]
struct StoredTask {
    tenant: TenantId,
    task: A2aTask,
}

type GapqlExecutor = Arc<
    dyn Fn(&RequestContext, &str, Option<&JsonValue>) -> GapResult<A2aTaskStatus> + Send + Sync,
>;

/// Deterministic, tenant-partitioned in-memory [`A2aTaskBackend`] for adapters and standalone tests.
pub struct InMemoryA2aBackend {
    clock: Arc<dyn Clock>,
    next_id: AtomicU64,
    tasks: Mutex<BTreeMap<String, StoredTask>>,
    executor: GapqlExecutor,
}

impl std::fmt::Debug for InMemoryA2aBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryA2aBackend")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl InMemoryA2aBackend {
    /// Creates an [`InMemoryA2aBackend`] with a custom synchronous GAP-QL status evaluator.
    #[must_use]
    pub fn with_executor<F>(clock: Arc<dyn Clock>, executor: F) -> Self
    where
        F: Fn(&RequestContext, &str, Option<&JsonValue>) -> GapResult<A2aTaskStatus>
            + Send
            + Sync
            + 'static,
    {
        Self {
            clock,
            next_id: AtomicU64::new(1),
            tasks: Mutex::new(BTreeMap::new()),
            executor: Arc::new(executor),
        }
    }

    /// Creates a default [`InMemoryA2aBackend`] that echoes completed `{ gapql, variables }` results.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_executor(clock, |ctx, gapql, variables| {
            let mut reply_data = BTreeMap::new();
            reply_data.insert("gapql".to_owned(), JsonValue::String(gapql.to_owned()));
            reply_data.insert(
                "generation".to_owned(),
                JsonValue::Int(i64::try_from(ctx.snapshot.generation).unwrap_or(i64::MAX)),
            );
            if let Some(vars) = variables {
                reply_data.insert("variables".to_owned(), vars.clone());
            }
            Ok(A2aTaskStatus {
                state: A2aTaskState::Completed,
                message: Some(A2aMessage {
                    message_id: format!("msg-{}", ctx.request_id),
                    context_id: None,
                    task_id: None,
                    role: A2aRole::Agent,
                    parts: vec![A2aPart::Data {
                        data: JsonValue::Object(reply_data),
                        media_type: "application/json".to_owned(),
                        metadata: BTreeMap::new(),
                    }],
                    metadata: BTreeMap::new(),
                    extensions: Vec::new(),
                    reference_task_ids: Vec::new(),
                }),
                timestamp: Some(TimestampClockNow::now_or_epoch()),
            })
        })
    }

    /// Directly stores a pre-built task under `tenant` (helpful for setting up `INPUT_REQUIRED` tests).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Internal`] if the lock is poisoned.
    pub fn insert_task(&self, tenant: &gap_core::ids::TenantScope, task: A2aTask) -> GapResult<()> {
        let mut guard = self
            .tasks
            .lock()
            .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
        guard.insert(
            task.id.clone(),
            StoredTask {
                tenant: tenant.tenant().clone(),
                task,
            },
        );
        Ok(())
    }
}

struct TimestampClockNow;

impl TimestampClockNow {
    fn now_or_epoch() -> gap_core::clock::Timestamp {
        SystemClock.now_timestamp()
    }
}

impl A2aTaskBackend for InMemoryA2aBackend {
    fn submit_gapql<'a>(
        &'a self,
        ctx: &'a RequestContext,
        gapql: String,
        variables: Option<JsonValue>,
        req: &'a InboundSendRequest,
    ) -> BoxFuture<'a, GapResult<A2aTask>> {
        Box::pin(async move {
            let seq = self.next_id.fetch_add(1, Ordering::AcqRel);
            let task_id = format!("a2a-task-{seq}");
            let context_id = req
                .message
                .context_id
                .clone()
                .unwrap_or_else(|| format!("a2a-ctx-{seq}"));

            let mut status = if req.return_immediately {
                A2aTaskStatus {
                    state: A2aTaskState::Submitted,
                    message: None,
                    timestamp: Some(self.clock.now_timestamp()),
                }
            } else {
                (self.executor)(ctx, &gapql, variables.as_ref())?
            };
            if status.timestamp.is_none() {
                status.timestamp = Some(self.clock.now_timestamp());
            }

            let mut metadata = BTreeMap::new();
            req.trace.insert_into(&mut metadata);
            let mut artifacts = Vec::new();
            if status.state == A2aTaskState::Completed {
                let mut out_parts = Vec::new();
                if let Some(ref msg) = status.message {
                    out_parts.clone_from(&msg.parts);
                }
                if out_parts.is_empty() {
                    let mut obj = BTreeMap::new();
                    obj.insert("status".to_owned(), JsonValue::String("ok".to_owned()));
                    out_parts.push(A2aPart::Data {
                        data: JsonValue::Object(obj),
                        media_type: "application/json".to_owned(),
                        metadata: BTreeMap::new(),
                    });
                }
                artifacts.push(A2aArtifact {
                    artifact_id: format!("art-{task_id}-1"),
                    name: Some("gapql-result".to_owned()),
                    description: None,
                    parts: out_parts,
                    metadata: BTreeMap::new(),
                    extensions: Vec::new(),
                });
            }

            let task = A2aTask {
                id: task_id.clone(),
                context_id,
                status,
                artifacts,
                history: vec![req.message.clone()],
                metadata,
            };

            let mut guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            guard.insert(
                task_id,
                StoredTask {
                    tenant: ctx.tenant_scope().tenant().clone(),
                    task: task.clone(),
                },
            );
            Ok(task)
        })
    }

    fn resume_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
        req: &'a InboundSendRequest,
    ) -> BoxFuture<'a, GapResult<A2aTask>> {
        Box::pin(async move {
            let mut guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            let stored = guard.get_mut(task_id).ok_or_else(|| {
                GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                )
            })?;
            if &stored.tenant != ctx.tenant_scope().tenant() {
                return Err(GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                ));
            }
            if stored.task.status.state != A2aTaskState::InputRequired {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "task `{task_id}` is in state {} and cannot accept continuation input",
                        stored.task.status.state.as_wire_str()
                    ),
                ));
            }
            if let Some(ref cid) = req.message.context_id
                && *cid != stored.task.context_id
            {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "continuation message.contextId does not match task contextId",
                ));
            }
            stored.task.history.push(req.message.clone());
            req.trace.insert_into(&mut stored.task.metadata);
            stored.task.status = A2aTaskStatus {
                state: A2aTaskState::Completed,
                message: Some(A2aMessage {
                    message_id: format!("msg-resume-{}", req.message.message_id),
                    context_id: Some(stored.task.context_id.clone()),
                    task_id: Some(stored.task.id.clone()),
                    role: A2aRole::Agent,
                    parts: vec![A2aPart::Text {
                        text: "input accepted; task completed".to_owned(),
                        metadata: BTreeMap::new(),
                    }],
                    metadata: BTreeMap::new(),
                    extensions: Vec::new(),
                    reference_task_ids: Vec::new(),
                }),
                timestamp: Some(self.clock.now_timestamp()),
            };
            Ok(stored.task.clone())
        })
    }

    fn get_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
    ) -> BoxFuture<'a, GapResult<Option<A2aTask>>> {
        Box::pin(async move {
            let guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            Ok(guard
                .get(task_id)
                .filter(|stored| &stored.tenant == ctx.tenant_scope().tenant())
                .map(|stored| stored.task.clone()))
        })
    }

    fn list_tasks<'a>(
        &'a self,
        ctx: &'a RequestContext,
        context_id: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, GapResult<Vec<A2aTask>>> {
        Box::pin(async move {
            let guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            let out = guard
                .values()
                .filter(|stored| &stored.tenant == ctx.tenant_scope().tenant())
                .filter(|stored| context_id.is_none_or(|cid| stored.task.context_id == cid))
                .take(limit)
                .map(|stored| stored.task.clone())
                .collect();
            Ok(out)
        })
    }

    fn cancel_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
    ) -> BoxFuture<'a, GapResult<A2aTask>> {
        Box::pin(async move {
            let mut guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            let stored = guard.get_mut(task_id).ok_or_else(|| {
                GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                )
            })?;
            if &stored.tenant != ctx.tenant_scope().tenant() {
                return Err(GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                ));
            }
            if !stored.task.status.state.is_terminal() {
                stored.task.status = A2aTaskStatus {
                    state: A2aTaskState::Canceled,
                    message: None,
                    timestamp: Some(self.clock.now_timestamp()),
                };
            }
            Ok(stored.task.clone())
        })
    }

    fn subscribe_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task_id: &'a str,
    ) -> BoxFuture<'a, GapResult<Vec<A2aStreamEvent>>> {
        Box::pin(async move {
            let guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            let stored = guard.get(task_id).ok_or_else(|| {
                GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                )
            })?;
            if &stored.tenant != ctx.tenant_scope().tenant() {
                return Err(GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                ));
            }
            let mut events = vec![A2aStreamEvent::Task(stored.task.clone())];
            events.push(A2aStreamEvent::StatusUpdate {
                task_id: stored.task.id.clone(),
                context_id: stored.task.context_id.clone(),
                status: stored.task.status.clone(),
                metadata: BTreeMap::new(),
            });
            for (idx, artifact) in stored.task.artifacts.iter().enumerate() {
                events.push(A2aStreamEvent::ArtifactUpdate {
                    task_id: stored.task.id.clone(),
                    context_id: stored.task.context_id.clone(),
                    artifact: artifact.clone(),
                    append: false,
                    last_chunk: idx + 1 == stored.task.artifacts.len(),
                    metadata: BTreeMap::new(),
                });
            }
            Ok(events)
        })
    }

    fn record_task<'a>(
        &'a self,
        ctx: &'a RequestContext,
        task: A2aTask,
    ) -> BoxFuture<'a, GapResult<()>> {
        Box::pin(async move {
            let mut guard = self
                .tasks
                .lock()
                .map_err(|_| GapError::new(ErrorCode::Internal, "a2a task store poisoned"))?;
            guard.insert(
                task.id.clone(),
                StoredTask {
                    tenant: ctx.tenant_scope().tenant().clone(),
                    task,
                },
            );
            Ok(())
        })
    }
}

/// Configuration for [`A2aFacade`] (`CMP-020`, `CMP-021`, `INV-20`).
#[derive(Debug, Clone)]
pub struct A2aFacadeConfig {
    /// Logical identifier of this GAP gateway node appended to trace paths (`INV-20`).
    pub node_id: String,
    /// Maximum allowed peer hop count (`DEFAULT_A2A_MAX_HOPS = 4`).
    pub max_hops: u8,
    /// Candidate count passed to [`DiscoveryService::resolve`] on text-only messages (`CMP-021`).
    pub resolve_top_k: usize,
    /// Metadata used to build `/.well-known/agent-card.json` (`CMP-020`).
    pub card: AgentCardConfig,
    /// Optional JWS signing `(kid, Ed25519SigningKey)` embedded inline into `AgentCard.signatures[]`.
    pub card_signing: Option<(String, Ed25519SigningKey)>,
}

impl Default for A2aFacadeConfig {
    fn default() -> Self {
        Self {
            node_id: "gap-node-local".to_owned(),
            max_hops: DEFAULT_A2A_MAX_HOPS,
            resolve_top_k: 8,
            card: AgentCardConfig::default(),
            card_signing: None,
        }
    }
}

/// Inbound `A2A v1.0` [`HttpModule`] facade (`CMP-020`, `CMP-021`, `INV-20`).
#[derive(Debug)]
pub struct A2aFacade<B, D> {
    config: A2aFacadeConfig,
    backend: Arc<B>,
    discovery: Arc<D>,
    clock: Arc<dyn Clock>,
    resolve_seq: AtomicU64,
}

impl<B, D> A2aFacade<B, D>
where
    B: A2aTaskBackend + 'static,
    D: DiscoveryService + 'static,
{
    /// Constructs an [`A2aFacade`] over task backend `backend` and discovery service `discovery`.
    #[must_use]
    pub fn new(
        config: A2aFacadeConfig,
        backend: Arc<B>,
        discovery: Arc<D>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            backend,
            discovery,
            clock,
            resolve_seq: AtomicU64::new(1),
        }
    }

    async fn serve_agent_card(&self, ctx: &RequestContext) -> GapResult<HttpExchangeResponse> {
        let signing = self
            .config
            .card_signing
            .as_ref()
            .map(|(kid, sk)| (kid.as_str(), sk));
        let card = build_agent_card(&ctx.snapshot, &self.config.card, signing)?;
        Ok(json_response(
            200,
            A2A_CONTENT_TYPE,
            card.to_json_string().into_bytes(),
        ))
    }

    fn validate_a2a_headers(req: &HttpExchangeRequest) -> GapResult<()> {
        if let Some((_, val)) = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(A2A_VERSION_HEADER))
            && val.trim() != A2A_PROTOCOL_VERSION
        {
            return Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                format!("unsupported A2A-Version `{val}`; expected `{A2A_PROTOCOL_VERSION}`"),
            ));
        }
        Ok(())
    }

    fn advance_trace(
        &self,
        ctx: &RequestContext,
        inbound_trace: Option<TraceExtension>,
    ) -> GapResult<TraceExtension> {
        let ctx_next = ctx.advance_peer_hop(&self.config.node_id, self.config.max_hops)?;
        let merged = match inbound_trace {
            Some(ext) => {
                let mut combined_path = ctx.trace.path.clone();
                for node in ext.path {
                    if !combined_path.iter().any(|existing| existing == &node) {
                        combined_path.push(node);
                    }
                }
                TraceExtension {
                    hop: ext.hop.max(ctx.hop),
                    path: combined_path,
                }
            }
            None => TraceExtension {
                hop: ctx.hop,
                path: ctx.trace.path.clone(),
            },
        };
        let advanced = merged.advance(&self.config.node_id, self.config.max_hops)?;
        Ok(TraceExtension {
            hop: advanced.hop.max(ctx_next.hop),
            path: advanced.path,
        })
    }

    async fn handle_send_message_payload(
        &self,
        ctx: &RequestContext,
        params: &BTreeMap<String, JsonValue>,
    ) -> GapResult<(A2aTask, Vec<A2aStreamEvent>)> {
        let msg_val = params
            .get("message")
            .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "missing `message` field"))?;
        let message = A2aMessage::from_json_value(msg_val)?;
        let req_meta = parse_optional_map(params.get("metadata"))?;
        let return_immediately = params
            .get("configuration")
            .and_then(|c| c.get_bool("returnImmediately"))
            .unwrap_or(false);
        let routing_tenant = opt_string(params.get("tenant"))?;

        let inbound_trace = match TraceExtension::from_metadata(&message.metadata)? {
            Some(t) => Some(t),
            None => TraceExtension::from_metadata(&req_meta)?,
        };
        let trace = self.advance_trace(ctx, inbound_trace)?;
        let send_req = InboundSendRequest {
            message: message.clone(),
            return_immediately,
            trace: trace.clone(),
            routing_tenant,
        };

        if let Some(ref existing_task_id) = message.task_id {
            let resumed = self
                .backend
                .resume_task(ctx, existing_task_id, &send_req)
                .await?;
            let events = build_events_for_task(&resumed);
            return Ok((resumed, events));
        }

        if let Some((gapql, variables)) = extract_gapql_payload(&message)? {
            let task = self
                .backend
                .submit_gapql(ctx, gapql, variables, &send_req)
                .await?;
            let events = build_events_for_task(&task);
            return Ok((task, events));
        }

        // Text-only message => deterministic `gap.resolve` returned as artifact (`CMP-021`).
        let intent = collect_text_intent(&message)?;
        let resolution = self
            .discovery
            .resolve(
                ctx,
                DiscoveryRequest {
                    intent,
                    top_k: self.config.resolve_top_k,
                    scopes: ctx.principal.scopes.iter().cloned().collect(),
                },
            )
            .await?;

        let seq = self.resolve_seq.fetch_add(1, Ordering::AcqRel);
        let task_id = format!("a2a-resolve-{seq}");
        let context_id = message
            .context_id
            .clone()
            .unwrap_or_else(|| format!("a2a-ctx-resolve-{seq}"));
        let artifact = A2aArtifact {
            artifact_id: format!("art-resolve-{seq}"),
            name: Some("resolution".to_owned()),
            description: Some("Deterministic GAP discovery resolution subgraph".to_owned()),
            parts: vec![A2aPart::Data {
                data: resolution_to_json(&resolution),
                media_type: "application/json".to_owned(),
                metadata: BTreeMap::new(),
            }],
            metadata: BTreeMap::new(),
            extensions: Vec::new(),
        };
        let mut metadata = BTreeMap::new();
        trace.insert_into(&mut metadata);
        let is_ambiguous = matches!(
            resolution.outcome,
            gap_core::traits::DiscoveryOutcome::Ambiguous
        );
        let status = if is_ambiguous {
            A2aTaskStatus {
                state: A2aTaskState::InputRequired,
                message: Some(A2aMessage {
                    message_id: format!("msg-clarify-{seq}"),
                    context_id: Some(context_id.clone()),
                    task_id: Some(task_id.clone()),
                    role: A2aRole::Agent,
                    parts: vec![A2aPart::Text {
                        text: format!(
                            "Ambiguous intent resolved {} candidates; clarify selection with message.taskId={task_id}",
                            resolution.candidates.len()
                        ),
                        metadata: BTreeMap::new(),
                    }],
                    metadata: BTreeMap::new(),
                    extensions: Vec::new(),
                    reference_task_ids: Vec::new(),
                }),
                timestamp: Some(self.clock.now_timestamp()),
            }
        } else {
            A2aTaskStatus {
                state: A2aTaskState::Completed,
                message: None,
                timestamp: Some(self.clock.now_timestamp()),
            }
        };
        let task = A2aTask {
            id: task_id,
            context_id,
            status,
            artifacts: vec![artifact],
            history: vec![message],
            metadata,
        };
        if is_ambiguous {
            self.backend.record_task(ctx, task.clone()).await?;
        }
        let events = build_events_for_task(&task);
        Ok((task, events))
    }

    async fn handle_http_json(
        &self,
        ctx: &RequestContext,
        req: HttpExchangeRequest,
    ) -> GapResult<HttpExchangeResponse> {
        let method = req.method.to_ascii_uppercase();
        let path = req.path.as_str();

        if method == "GET" && path == "/.well-known/agent-card.json" {
            return self.serve_agent_card(ctx).await;
        }
        Self::validate_a2a_headers(&req)?;

        match (method.as_str(), path) {
            ("POST", "/message:send") => {
                let obj = parse_json_object(&req.body)?;
                match self.handle_send_message_payload(ctx, &obj).await {
                    Ok((task, _)) => {
                        let mut wrap = BTreeMap::new();
                        wrap.insert("task".to_owned(), task.to_json_value());
                        Ok(json_response(
                            200,
                            A2A_CONTENT_TYPE,
                            JsonValue::Object(wrap).to_jcs_string().into_bytes(),
                        ))
                    }
                    Err(err) => Ok(a2a_http_error_response(&err)),
                }
            }
            ("POST", "/message:stream") => {
                let obj = parse_json_object(&req.body)?;
                match self.handle_send_message_payload(ctx, &obj).await {
                    Ok((_, events)) => {
                        let body = encode_http_sse_stream(&events);
                        Ok(sse_response(body))
                    }
                    Err(err) => Ok(a2a_http_error_response(&err)),
                }
            }
            ("GET", "/tasks") => {
                let context_id = req
                    .query
                    .as_deref()
                    .and_then(|q| extract_query_param(q, "contextId"));
                let limit = req
                    .query
                    .as_deref()
                    .and_then(|q| extract_query_param(q, "pageSize"))
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(50)
                    .min(200);
                let tasks = self
                    .backend
                    .list_tasks(ctx, context_id.as_deref(), limit)
                    .await?;
                let mut wrap = BTreeMap::new();
                wrap.insert(
                    "tasks".to_owned(),
                    JsonValue::Array(tasks.iter().map(A2aTask::to_json_value).collect()),
                );
                Ok(json_response(
                    200,
                    A2A_CONTENT_TYPE,
                    JsonValue::Object(wrap).to_jcs_string().into_bytes(),
                ))
            }
            ("POST", "/a2a") => self.handle_jsonrpc(ctx, &req.body).await,
            _ => self.handle_task_subpath(ctx, &method, path).await,
        }
    }

    async fn handle_task_subpath(
        &self,
        ctx: &RequestContext,
        method: &str,
        path: &str,
    ) -> GapResult<HttpExchangeResponse> {
        let Some(rest) = path.strip_prefix("/tasks/") else {
            return Ok(a2a_http_error_response(&GapError::new(
                ErrorCode::TaskNotFound,
                format!("unknown A2A route `{path}`"),
            )));
        };
        if let Some(task_id) = rest.strip_suffix(":cancel") {
            if method != "POST" {
                return Ok(method_not_allowed(&["POST"]));
            }
            return match self.backend.cancel_task(ctx, task_id).await {
                Ok(task) => Ok(json_response(
                    200,
                    A2A_CONTENT_TYPE,
                    task.to_json_value().to_jcs_string().into_bytes(),
                )),
                Err(err) => Ok(a2a_http_error_response(&err)),
            };
        }
        if let Some(task_id) = rest.strip_suffix(":subscribe") {
            // `a2a.proto` specifies GET whereas prose docs show POST; accept both (`Q-13`).
            if method != "GET" && method != "POST" {
                return Ok(method_not_allowed(&["GET", "POST"]));
            }
            return match self.backend.subscribe_task(ctx, task_id).await {
                Ok(events) => Ok(sse_response(encode_http_sse_stream(&events))),
                Err(err) => Ok(a2a_http_error_response(&err)),
            };
        }
        if method != "GET" || rest.contains('/') {
            return Ok(a2a_http_error_response(&GapError::new(
                ErrorCode::TaskNotFound,
                format!("unknown A2A route `{path}`"),
            )));
        }
        match self.backend.get_task(ctx, rest).await? {
            Some(task) => Ok(json_response(
                200,
                A2A_CONTENT_TYPE,
                task.to_json_value().to_jcs_string().into_bytes(),
            )),
            None => Ok(a2a_http_error_response(&GapError::new(
                ErrorCode::TaskNotFound,
                format!("task `{rest}` not found"),
            ))),
        }
    }

    async fn handle_jsonrpc(
        &self,
        ctx: &RequestContext,
        body: &[u8],
    ) -> GapResult<HttpExchangeResponse> {
        let obj = parse_json_object(body)?;
        let id = obj.get("id").cloned().unwrap_or(JsonValue::Null);
        let method = obj
            .get("method")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                GapError::new(ErrorCode::ValidationError, "JSON-RPC method is required")
            })?;
        let params = obj
            .get("params")
            .and_then(JsonValue::as_object)
            .cloned()
            .unwrap_or_default();

        match method {
            "SendMessage" => match self.handle_send_message_payload(ctx, &params).await {
                Ok((task, _)) => {
                    let mut res = BTreeMap::new();
                    res.insert("task".to_owned(), task.to_json_value());
                    Ok(jsonrpc_ok(&id, JsonValue::Object(res)))
                }
                Err(err) => Ok(jsonrpc_err(&id, &err)),
            },
            "SendStreamingMessage" => match self.handle_send_message_payload(ctx, &params).await {
                Ok((_, events)) => Ok(sse_response(encode_jsonrpc_sse_stream(&id, &events))),
                Err(err) => Ok(jsonrpc_err(&id, &err)),
            },
            "GetTask" => {
                let Some(task_id) = params.get("id").and_then(JsonValue::as_str) else {
                    return Ok(jsonrpc_err(
                        &id,
                        &GapError::new(ErrorCode::ValidationError, "GetTask requires params.id"),
                    ));
                };
                match self.backend.get_task(ctx, task_id).await {
                    Ok(Some(task)) => Ok(jsonrpc_ok(&id, task.to_json_value())),
                    Ok(None) => Ok(jsonrpc_err(
                        &id,
                        &GapError::new(
                            ErrorCode::TaskNotFound,
                            format!("task `{task_id}` not found"),
                        ),
                    )),
                    Err(err) => Ok(jsonrpc_err(&id, &err)),
                }
            }
            "ListTasks" => {
                let context_id = params.get("contextId").and_then(JsonValue::as_str);
                let limit = params
                    .get("pageSize")
                    .and_then(JsonValue::as_i64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(50)
                    .min(200);
                match self.backend.list_tasks(ctx, context_id, limit).await {
                    Ok(tasks) => {
                        let mut res = BTreeMap::new();
                        res.insert(
                            "tasks".to_owned(),
                            JsonValue::Array(tasks.iter().map(A2aTask::to_json_value).collect()),
                        );
                        Ok(jsonrpc_ok(&id, JsonValue::Object(res)))
                    }
                    Err(err) => Ok(jsonrpc_err(&id, &err)),
                }
            }
            "CancelTask" => {
                let Some(task_id) = params.get("id").and_then(JsonValue::as_str) else {
                    return Ok(jsonrpc_err(
                        &id,
                        &GapError::new(ErrorCode::ValidationError, "CancelTask requires params.id"),
                    ));
                };
                match self.backend.cancel_task(ctx, task_id).await {
                    Ok(task) => Ok(jsonrpc_ok(&id, task.to_json_value())),
                    Err(err) => Ok(jsonrpc_err(&id, &err)),
                }
            }
            "SubscribeToTask" => {
                let Some(task_id) = params.get("id").and_then(JsonValue::as_str) else {
                    return Ok(jsonrpc_err(
                        &id,
                        &GapError::new(
                            ErrorCode::ValidationError,
                            "SubscribeToTask requires params.id",
                        ),
                    ));
                };
                match self.backend.subscribe_task(ctx, task_id).await {
                    Ok(events) => Ok(sse_response(encode_jsonrpc_sse_stream(&id, &events))),
                    Err(err) => Ok(jsonrpc_err(&id, &err)),
                }
            }
            other => Ok(jsonrpc_err(
                &id,
                &GapError::new(
                    ErrorCode::UnsupportedFeature,
                    format!("unknown A2A method `{other}`"),
                ),
            )),
        }
    }
}

impl<B, D> HttpModule for A2aFacade<B, D>
where
    B: A2aTaskBackend + 'static,
    D: DiscoveryService + 'static,
{
    fn module_name(&self) -> &'static str {
        "gap-a2a"
    }

    fn mount_prefixes(&self) -> &'static [&'static str] {
        PREFIXES
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: HttpExchangeRequest,
    ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>> {
        Box::pin(async move {
            match self.handle_http_json(ctx, req).await {
                Ok(resp) => Ok(resp),
                Err(err) => Ok(a2a_http_error_response(&err)),
            }
        })
    }
}

fn build_events_for_task(task: &A2aTask) -> Vec<A2aStreamEvent> {
    let mut events = vec![
        A2aStreamEvent::Task(task.clone()),
        A2aStreamEvent::StatusUpdate {
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
            status: task.status.clone(),
            metadata: BTreeMap::new(),
        },
    ];
    for (idx, artifact) in task.artifacts.iter().enumerate() {
        events.push(A2aStreamEvent::ArtifactUpdate {
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
            artifact: artifact.clone(),
            append: false,
            last_chunk: idx + 1 == task.artifacts.len(),
            metadata: BTreeMap::new(),
        });
    }
    events
}

fn extract_gapql_payload(msg: &A2aMessage) -> GapResult<Option<(String, Option<JsonValue>)>> {
    for part in &msg.parts {
        if let A2aPart::Data { data, .. } = part
            && let Some(gapql_val) = data.get("gapql")
        {
            let gapql = gapql_val
                .as_str()
                .ok_or_else(|| {
                    GapError::new(
                        ErrorCode::ValidationError,
                        "data.gapql must be a GraphQL string",
                    )
                })?
                .to_owned();
            let variables = data.get("variables").cloned();
            return Ok(Some((gapql, variables)));
        }
    }
    Ok(None)
}

fn collect_text_intent(msg: &A2aMessage) -> GapResult<String> {
    let mut segments = Vec::new();
    for part in &msg.parts {
        if let A2aPart::Text { text, .. } = part {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                segments.push(trimmed.to_owned());
            }
        }
    }
    if segments.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "A2A Message must contain non-empty text or data.gapql",
        ));
    }
    Ok(segments.join(" "))
}

fn resolution_to_json(res: &DiscoveryResolution) -> JsonValue {
    let mut obj = BTreeMap::new();
    let outcome_str = match res.outcome {
        gap_core::traits::DiscoveryOutcome::Resolved => "RESOLVED",
        gap_core::traits::DiscoveryOutcome::Ambiguous => "AMBIGUOUS",
        gap_core::traits::DiscoveryOutcome::Abstained => "ABSTAINED",
        gap_core::traits::DiscoveryOutcome::Degraded => "DEGRADED",
    };
    obj.insert(
        "outcome".to_owned(),
        JsonValue::String(outcome_str.to_owned()),
    );
    obj.insert(
        "tokensEstimate".to_owned(),
        JsonValue::Int(i64::from(res.tokens_estimate)),
    );
    obj.insert("router".to_owned(), JsonValue::String(res.router.clone()));
    let candidates = res
        .candidates
        .iter()
        .map(|c| {
            let mut cm = BTreeMap::new();
            cm.insert(
                "elementId".to_owned(),
                JsonValue::String(c.element_id.clone()),
            );
            cm.insert(
                "kind".to_owned(),
                JsonValue::String(format!("{:?}", c.kind).to_ascii_uppercase()),
            );
            cm.insert("score".to_owned(), JsonValue::Float(f64::from(c.score)));
            cm.insert("synopsis".to_owned(), JsonValue::String(c.synopsis.clone()));
            if let Some(ref sdl) = c.sdl {
                cm.insert("sdl".to_owned(), JsonValue::String(sdl.clone()));
            }
            JsonValue::Object(cm)
        })
        .collect();
    obj.insert("candidates".to_owned(), JsonValue::Array(candidates));
    JsonValue::Object(obj)
}

fn parse_json_object(bytes: &[u8]) -> GapResult<BTreeMap<String, JsonValue>> {
    let raw = std::str::from_utf8(bytes)
        .map_err(|_| GapError::new(ErrorCode::ParseError, "request body is not valid UTF-8"))?;
    let val = JsonValue::parse(raw)?;
    val.as_object()
        .cloned()
        .ok_or_else(|| GapError::new(ErrorCode::ValidationError, "JSON body must be an object"))
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(v.to_owned());
        }
    }
    None
}

fn json_response(status: u16, content_type: &str, body: Vec<u8>) -> HttpExchangeResponse {
    HttpExchangeResponse {
        status,
        headers: vec![
            ("content-type".to_owned(), content_type.to_owned()),
            (
                A2A_VERSION_HEADER.to_owned(),
                A2A_PROTOCOL_VERSION.to_owned(),
            ),
            (
                A2A_EXTENSIONS_HEADER.to_owned(),
                GAP_TRACE_EXT_URI.to_owned(),
            ),
        ],
        body,
    }
}

fn sse_response(body: String) -> HttpExchangeResponse {
    HttpExchangeResponse {
        status: 200,
        headers: vec![
            (
                "content-type".to_owned(),
                "text/event-stream; charset=utf-8".to_owned(),
            ),
            ("cache-control".to_owned(), "no-cache".to_owned()),
            (
                A2A_VERSION_HEADER.to_owned(),
                A2A_PROTOCOL_VERSION.to_owned(),
            ),
            (
                A2A_EXTENSIONS_HEADER.to_owned(),
                GAP_TRACE_EXT_URI.to_owned(),
            ),
        ],
        body: body.into_bytes(),
    }
}

fn method_not_allowed(allowed: &[&str]) -> HttpExchangeResponse {
    HttpExchangeResponse {
        status: 405,
        headers: vec![
            ("allow".to_owned(), allowed.join(", ")),
            ("content-type".to_owned(), A2A_CONTENT_TYPE.to_owned()),
        ],
        body: b"{}".to_vec(),
    }
}

/// Maps [`GapError`] into `google.rpc.Status` + `google.rpc.ErrorInfo` HTTP+JSON error (`Q-15`).
fn a2a_http_error_response(err: &GapError) -> HttpExchangeResponse {
    let (http_status, rpc_code, rpc_status) = match err.code() {
        ErrorCode::ValidationError | ErrorCode::ParseError | ErrorCode::TypeMismatch => {
            (400, 3_i64, "INVALID_ARGUMENT")
        }
        ErrorCode::UnsupportedFeature => (400, 12_i64, "UNIMPLEMENTED"),
        ErrorCode::LoopDetected | ErrorCode::HopLimit => (508, 9_i64, "FAILED_PRECONDITION"),
        ErrorCode::TaskNotFound => (404, 5_i64, "NOT_FOUND"),
        ErrorCode::Unauthenticated => (401, 16_i64, "UNAUTHENTICATED"),
        ErrorCode::Forbidden | ErrorCode::TenantMismatch => (403, 7_i64, "PERMISSION_DENIED"),
        ErrorCode::RateLimited => (429, 8_i64, "RESOURCE_EXHAUSTED"),
        ErrorCode::Overloaded => (503, 14_i64, "UNAVAILABLE"),
        ErrorCode::DeadlineExceeded => (504, 4_i64, "DEADLINE_EXCEEDED"),
        _ => (500, 13_i64, "INTERNAL"),
    };
    let mut info = BTreeMap::new();
    info.insert(
        "@type".to_owned(),
        JsonValue::String("type.googleapis.com/google.rpc.ErrorInfo".to_owned()),
    );
    info.insert(
        "reason".to_owned(),
        JsonValue::String(err.code().to_string()),
    );
    info.insert(
        "domain".to_owned(),
        JsonValue::String("gap-protocol.org".to_owned()),
    );

    let mut body = BTreeMap::new();
    body.insert("code".to_owned(), JsonValue::Int(rpc_code));
    body.insert(
        "status".to_owned(),
        JsonValue::String(rpc_status.to_owned()),
    );
    body.insert(
        "message".to_owned(),
        JsonValue::String(err.message().to_owned()),
    );
    body.insert(
        "details".to_owned(),
        JsonValue::Array(vec![JsonValue::Object(info)]),
    );
    json_response(
        http_status,
        A2A_CONTENT_TYPE,
        JsonValue::Object(body).to_jcs_string().into_bytes(),
    )
}

fn jsonrpc_ok(id: &JsonValue, result: JsonValue) -> HttpExchangeResponse {
    let mut obj = BTreeMap::new();
    obj.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
    obj.insert("id".to_owned(), id.clone());
    obj.insert("result".to_owned(), result);
    json_response(
        200,
        "application/json",
        JsonValue::Object(obj).to_jcs_string().into_bytes(),
    )
}

fn jsonrpc_err(id: &JsonValue, err: &GapError) -> HttpExchangeResponse {
    let code = match err.code() {
        ErrorCode::ParseError => -32700_i64,
        ErrorCode::ValidationError | ErrorCode::TypeMismatch => -32602_i64,
        ErrorCode::UnsupportedFeature => -32601_i64,
        ErrorCode::TaskNotFound => -32001_i64,
        ErrorCode::LoopDetected | ErrorCode::HopLimit => -32020_i64,
        _ => -32000_i64,
    };
    let mut data = BTreeMap::new();
    data.insert(
        "gapCode".to_owned(),
        JsonValue::String(err.code().to_string()),
    );
    let mut error_obj = BTreeMap::new();
    error_obj.insert("code".to_owned(), JsonValue::Int(code));
    error_obj.insert(
        "message".to_owned(),
        JsonValue::String(err.message().to_owned()),
    );
    error_obj.insert("data".to_owned(), JsonValue::Object(data));

    let mut obj = BTreeMap::new();
    obj.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
    obj.insert("id".to_owned(), id.clone());
    obj.insert("error".to_owned(), JsonValue::Object(error_obj));
    json_response(
        200,
        "application/json",
        JsonValue::Object(obj).to_jcs_string().into_bytes(),
    )
}
