//! Outbound `A2A v1.0` peer-agent `NGC` connector (`CMP-022`, `INV-15`, `INV-20`, `Q-15`).
//!
//! Exposes a peer A2A agent as a GAP branch (`Protocol::A2a`) with `OpKind::Task` operations (one
//! per `AgentCard.skills[]` entry), translates `StartTask`/`GetTask`/`CancelTask`/`WatchTask`,
//! echoes `AgentInterface.tenant` routing metadata (`Q-15`), propagates `https://gap-protocol.org/ext/trace/v1`
//! for loop detection (`INV-20`), strips client bearer credentials (`INV-15`), and supports
//! `1 s -> 10 s` exponential backoff `GetTask` polling.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_core::error::{ErrorCode, GapError, NgcErrorCode, NgcErrorInfo};
use gap_core::traits::BoxFuture;
use gap_ngc_sdk::connector::{
    BoxStream, CallScope, Connector, NgcResult, is_sensitive_metadata_key,
};
use gap_ngc_sdk::json::{from_json_slice, to_json_string};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Artifact as ProtoArtifact, CancelTaskRequest, Capabilities as ProtoCapabilities,
    ConnectorDescription, DescribeRequest, DescribeResponse, GetTaskRequest, Limits as ProtoLimits,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOperation, Protocol as ProtoProtocol,
    QueryRequest, QueryResponse, SideEffectHint, StartTaskRequest, TaskStatus as ProtoTaskStatus,
    WatchTaskRequest,
};
use gap_sec::crypto::{blake3_hash, hex_encode};
use gap_sec::json::JsonValue;

use crate::card::{AgentCard, AgentProtocolBinding};
use crate::wire::{
    A2A_CONTENT_TYPE, A2A_EXTENSIONS_HEADER, A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER, A2aArtifact,
    A2aMessage, A2aPart, A2aRole, A2aStreamEvent, A2aTask, A2aTaskState, DEFAULT_A2A_MAX_HOPS,
    GAP_TRACE_EXT_URI, TraceExtension,
};

/// Metadata key carrying outbound hop count from `CallScope::metadata` (non-secret internal signal).
pub const SCOPE_HOP_HEADER: &str = "x-gap-hop";

/// Metadata key carrying comma-separated visited peer nodes from `CallScope::metadata`.
pub const SCOPE_TRACE_PATH_HEADER: &str = "x-gap-trace-path";

/// Default initial `GetTask` polling backoff (`1 s`, `CMP-022`).
pub const DEFAULT_POLL_INITIAL_MS: u64 = 1_000;

/// Default maximum `GetTask` polling backoff (`10 s`, `CMP-022`).
pub const DEFAULT_POLL_MAX_MS: u64 = 10_000;

/// Sanitized outbound wire request sent to [`A2aPeerTransport`] (never carries caller bearer tokens, `INV-15`).
#[derive(Debug, Clone, PartialEq)]
pub struct A2aOutboundEnvelope {
    /// Target URL from the peer `AgentCard.supportedInterfaces[]`.
    pub target_url: String,
    /// Binding selected (`HTTP+JSON`, `JSONRPC`, or `GRPC`).
    pub binding: AgentProtocolBinding,
    /// Optional `AgentInterface.tenant` routing hint echoed verbatim per `v1.0.1` (`Q-15`).
    pub interface_tenant: Option<String>,
    /// Safe outgoing headers (`A2A-Version: 1.0`, `A2A-Extensions`, `Content-Type`).
    pub headers: BTreeMap<String, String>,
    /// A2A JSON payload (`SendMessage`, `CancelTask`, etc.).
    pub body: JsonValue,
}

/// Async transport abstraction backing [`A2aPeerConnector`] (`CMP-022`).
pub trait A2aPeerTransport: Send + Sync + std::fmt::Debug {
    /// Fetches the peer's live `AgentCard` (`/.well-known/agent-card.json`).
    fn fetch_agent_card<'a>(&'a self, url: &'a str) -> BoxFuture<'a, NgcResult<AgentCard>>;

    /// Dispatches `SendMessage` (`StartTask`) over the wire.
    fn send_message<'a>(
        &'a self,
        envelope: &'a A2aOutboundEnvelope,
    ) -> BoxFuture<'a, NgcResult<A2aTask>>;

    /// Polls `GetTask` by task identifier.
    fn get_task<'a>(
        &'a self,
        target_url: &'a str,
        interface_tenant: Option<&'a str>,
        headers: &'a BTreeMap<String, String>,
        task_id: &'a str,
    ) -> BoxFuture<'a, NgcResult<A2aTask>>;

    /// Dispatches `CancelTask` by task identifier.
    fn cancel_task<'a>(
        &'a self,
        target_url: &'a str,
        interface_tenant: Option<&'a str>,
        headers: &'a BTreeMap<String, String>,
        task_id: &'a str,
    ) -> BoxFuture<'a, NgcResult<A2aTask>>;

    /// Optional live `SubscribeToTask` / `WatchTask` stream (`WatchTask` defaults to `Unimplemented`).
    fn watch_task<'a>(
        &'a self,
        _target_url: &'a str,
        _interface_tenant: Option<&'a str>,
        _headers: &'a BTreeMap<String, String>,
        _task_id: &'a str,
    ) -> BoxFuture<'a, NgcResult<BoxStream<ProtoTaskStatus>>> {
        Box::pin(std::future::ready(Err(NgcErrorInfo::new(
            NgcErrorCode::Unimplemented,
            "streaming WatchTask is not provided by this A2A transport; use GetTask polling",
        ))))
    }
}

/// Configuration for [`A2aPeerConnector`] (`CMP-022`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aPeerConfig {
    /// Logical GAP branch/connector identifier.
    pub connector_id: String,
    /// This GAP node's ID stamped into `trace.path` for cycle detection (`INV-20`).
    pub node_id: String,
    /// Pinned peer `AgentCard` describing skills and interfaces.
    pub card: AgentCard,
    /// Maximum allowed hop count (`DEFAULT_A2A_MAX_HOPS = 4`).
    pub max_hops: u8,
    /// Initial `GetTask` poll delay in milliseconds (`1_000 ms`, `CMP-022`).
    pub poll_initial_ms: u64,
    /// Maximum `GetTask` poll delay in milliseconds (`10_000 ms`, `CMP-022`).
    pub poll_max_ms: u64,
    /// Per-call timeout advertised in `Limits`.
    pub default_timeout_ms: u32,
}

impl A2aPeerConfig {
    /// Constructs a peer config anchored on `card` using `1 s -> 10 s` backoff defaults (`CMP-022`).
    #[must_use]
    pub fn new(
        connector_id: impl Into<String>,
        node_id: impl Into<String>,
        card: AgentCard,
    ) -> Self {
        Self {
            connector_id: connector_id.into(),
            node_id: node_id.into(),
            card,
            max_hops: DEFAULT_A2A_MAX_HOPS,
            poll_initial_ms: DEFAULT_POLL_INITIAL_MS,
            poll_max_ms: DEFAULT_POLL_MAX_MS,
            default_timeout_ms: 30_000,
        }
    }
}

/// Outbound `A2A v1.0` peer-agent connector implementing [`Connector`] (`CMP-022`).
#[derive(Debug)]
pub struct A2aPeerConnector<T> {
    config: A2aPeerConfig,
    transport: Arc<T>,
    msg_seq: AtomicU64,
}

impl<T> A2aPeerConnector<T>
where
    T: A2aPeerTransport + 'static,
{
    /// Creates an [`A2aPeerConnector`] using `config` and `transport`.
    #[must_use]
    pub fn new(config: A2aPeerConfig, transport: Arc<T>) -> Self {
        Self {
            config,
            transport,
            msg_seq: AtomicU64::new(1),
        }
    }

    /// Exposes the configured peer `AgentCard`.
    #[must_use]
    pub fn agent_card(&self) -> &AgentCard {
        &self.config.card
    }

    /// Selects the preferred HTTP+JSON / JSONRPC interface and optional `tenant` hint (`Q-15`).
    ///
    /// # Errors
    ///
    /// Returns `FailedPrecondition` if the `AgentCard` defines no supported interface.
    pub fn select_interface(&self) -> NgcResult<(String, AgentProtocolBinding, Option<String>)> {
        let iface = self
            .config
            .card
            .supported_interfaces
            .iter()
            .find(|i| i.protocol_binding == AgentProtocolBinding::HttpJson)
            .or_else(|| {
                self.config
                    .card
                    .supported_interfaces
                    .iter()
                    .find(|i| i.protocol_binding == AgentProtocolBinding::JsonRpc)
            })
            .or_else(|| self.config.card.supported_interfaces.first())
            .ok_or_else(|| {
                NgcErrorInfo::new(
                    NgcErrorCode::FailedPrecondition,
                    "peer AgentCard declares no supportedInterfaces",
                )
            })?;
        Ok((
            iface.url.clone(),
            iface.protocol_binding,
            iface.tenant.clone(),
        ))
    }

    /// Builds sanitized outbound HTTP headers, strictly excluding caller bearer tokens (`INV-15`).
    #[must_use]
    pub fn sanitized_headers(scope: &CallScope) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert(
            A2A_VERSION_HEADER.to_owned(),
            A2A_PROTOCOL_VERSION.to_owned(),
        );
        headers.insert(
            A2A_EXTENSIONS_HEADER.to_owned(),
            GAP_TRACE_EXT_URI.to_owned(),
        );
        headers.insert("content-type".to_owned(), A2A_CONTENT_TYPE.to_owned());
        if !scope.request_id.is_empty() {
            headers.insert("x-request-id".to_owned(), scope.request_id.clone());
        }
        if !scope.traceparent.is_empty() {
            headers.insert("traceparent".to_owned(), scope.traceparent.clone());
        }
        // Only non-sensitive explicit non-auth propagation headers are ever inspected elsewhere;
        // sensitive headers (`authorization`, `cookie`, `*-bin`, `*token*`, `*secret*`, `*key*`)
        // are NEVER forwarded (`SEC-011`, `INV-15`).
        for (k, v) in &scope.metadata {
            if !is_sensitive_metadata_key(k) && k.eq_ignore_ascii_case("x-correlation-id") {
                headers.insert(k.to_ascii_lowercase(), v.clone());
            }
        }
        headers
    }

    /// Computes exponential polling backoff schedule starting at `poll_initial_ms` (`1 s`) capped
    /// at `poll_max_ms` (`10 s`, `CMP-022`).
    #[must_use]
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        let shift = attempt.min(10);
        let mult = 1_u64 << shift;
        let ms = self
            .config
            .poll_initial_ms
            .saturating_mul(mult)
            .min(self.config.poll_max_ms.max(self.config.poll_initial_ms));
        Duration::from_millis(ms)
    }

    /// Polls `GetTask` until reaching a terminal or interrupted A2A state (`CMP-022`).
    ///
    /// Invokes `sleep_fn(delay)` between non-terminal attempts (`1 s -> 10 s` backoff).
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if polling fails or `max_attempts` is exceeded.
    pub async fn poll_task_until_settled<F, Fut>(
        &self,
        scope: &CallScope,
        task_id: &str,
        max_attempts: usize,
        mut sleep_fn: F,
    ) -> NgcResult<Vec<ProtoTaskStatus>>
    where
        F: FnMut(Duration) -> Fut,
        Fut: Future<Output = ()>,
    {
        let (target_url, _, iface_tenant) = self.select_interface()?;
        let headers = Self::sanitized_headers(scope);
        let mut updates = Vec::new();
        for attempt in 0..max_attempts {
            let task = self
                .transport
                .get_task(&target_url, iface_tenant.as_deref(), &headers, task_id)
                .await?;
            let state = task.status.state;
            let seq = u64::try_from(attempt + 1).unwrap_or(u64::MAX);
            updates.push(a2a_task_to_proto_status(&task, seq)?);
            if state.is_terminal() || state.is_interrupted() {
                return Ok(updates);
            }
            let delay = self.backoff_delay(u32::try_from(attempt).unwrap_or(u32::MAX));
            sleep_fn(delay).await;
        }
        Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            format!("A2A task `{task_id}` did not settle within {max_attempts} poll attempts"),
        ))
    }

    fn advance_scope_trace(
        &self,
        scope: &CallScope,
        call_ctx: Option<&gap_proto::ngc::v1::CallContext>,
    ) -> NgcResult<TraceExtension> {
        let meta_hop = scope
            .metadata
            .get(SCOPE_HOP_HEADER)
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(0);
        let mut path: Vec<String> = scope
            .metadata
            .get(SCOPE_TRACE_PATH_HEADER)
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut hop = meta_hop;
        if let Some(ctx) = call_ctx {
            let ctx_hop = u8::try_from(ctx.hop_count).unwrap_or(u8::MAX);
            hop = hop.max(ctx_hop);
            for node in &ctx.trace_path {
                if !path.iter().any(|existing| existing == node) {
                    path.push(node.clone());
                }
            }
        }
        let current = TraceExtension { hop, path };
        current
            .advance(&self.config.node_id, self.config.max_hops)
            .map_err(gap_error_to_ngc)
    }

    /// Converts a list of [`A2aStreamEvent`]s into NGC [`ProtoTaskStatus`] updates (`CMP-022`).
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on invalid wire state (`TASK_STATE_UNSPECIFIED`).
    pub fn decode_stream_events(events: &[A2aStreamEvent]) -> NgcResult<Vec<ProtoTaskStatus>> {
        let mut out = Vec::with_capacity(events.len());
        for (idx, ev) in events.iter().enumerate() {
            let seq = u64::try_from(idx + 1).unwrap_or(u64::MAX);
            match ev {
                A2aStreamEvent::Task(task) => {
                    out.push(a2a_task_to_proto_status(task, seq)?);
                }
                A2aStreamEvent::StatusUpdate {
                    task_id, status, ..
                } => {
                    let message = status
                        .message
                        .as_ref()
                        .and_then(|m| extract_text_or_json(&m.parts))
                        .unwrap_or_default();
                    out.push(ProtoTaskStatus {
                        task_id: task_id.clone(),
                        state: status.state.to_proto() as i32,
                        message,
                        artifacts: Vec::new(),
                        input_request: if status.state.is_interrupted() {
                            "A2A peer requested continuation input".to_owned()
                        } else {
                            String::new()
                        },
                        updated_at: None,
                        seq,
                        error: None,
                    });
                }
                A2aStreamEvent::ArtifactUpdate {
                    task_id, artifact, ..
                } => {
                    out.push(ProtoTaskStatus {
                        task_id: task_id.clone(),
                        state: A2aTaskState::Working.to_proto() as i32,
                        message: String::new(),
                        artifacts: vec![a2a_artifact_to_proto(artifact)?],
                        input_request: String::new(),
                        updated_at: None,
                        seq,
                        error: None,
                    });
                }
                A2aStreamEvent::Message(_) => {}
            }
        }
        Ok(out)
    }

    fn build_description(&self) -> ConnectorDescription {
        let spec_hash = hex_encode(&blake3_hash(
            self.config.card.canonical_unsigned_json().as_bytes(),
        ));
        let mut seen_names = BTreeSet::new();
        let mut operations = Vec::with_capacity(self.config.card.skills.len());
        let mut sdl_fields = Vec::with_capacity(self.config.card.skills.len());
        for skill in &self.config.card.skills {
            let op_name = unique_graphql_ident(&skill.id, &mut seen_names);
            sdl_fields.push(format!("  {op_name}(input: String): A2aTaskOutput"));
            operations.push(ProtoOperation {
                name: op_name,
                kind: ProtoOpKind::Task as i32,
                description: skill.description.clone(),
                input_schema: None,
                output_schema: None,
                side_effect_hint: SideEffectHint::SideEffectUnspecified as i32,
                idempotent_hint: false,
                supports_dry_run: false,
                compensation_operation: String::new(),
                examples: Vec::new(),
                cost_weight: 2,
                pagination: 0,
                key_fields: Vec::new(),
                annotations: HashMap::from([("a2a.skill_id".to_owned(), skill.id.clone())]),
            });
        }
        if sdl_fields.is_empty() {
            sdl_fields.push("  a2a_default(input: String): A2aTaskOutput".to_owned());
        }
        let schema_sdl = format!(
            "type A2aTaskOutput {{\n  taskId: String!\n  state: String!\n}}\n\ntype Query {{\n{}\n}}\n",
            sdl_fields.join("\n")
        );

        ConnectorDescription {
            connector_id: self.config.connector_id.clone(),
            connector_version: self.config.card.version.clone(),
            ngc_version: NGC_WIRE_VERSION.to_owned(),
            protocol: ProtoProtocol::A2a as i32,
            spec_hash,
            capabilities: Some(ProtoCapabilities {
                tasks: true,
                ..ProtoCapabilities::default()
            }),
            limits: Some(ProtoLimits {
                max_batch_size: 0,
                max_in_flight: 32,
                max_page_size: 50,
                default_timeout_ms: self.config.default_timeout_ms,
                max_response_bytes: 4 * 1024 * 1024,
            }),
            schema_sdl,
            operations,
            meta: None,
            auth: None,
        }
    }
}

impl<T> Connector for A2aPeerConnector<T>
where
    T: A2aPeerTransport + 'static,
{
    async fn describe(
        &self,
        request: DescribeRequest,
        _scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        let desc = self.build_description();
        if !request.known_spec_hash.is_empty() && request.known_spec_hash == desc.spec_hash {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }
        Ok(DescribeResponse {
            not_modified: false,
            description: Some(desc),
        })
    }

    async fn query(
        &self,
        _request: QueryRequest,
        _scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        Err(NgcErrorInfo::new(
            NgcErrorCode::Unimplemented,
            "A2A outbound connector exposes TASK operations via StartTask",
        ))
    }

    async fn start_task(
        &self,
        request: StartTaskRequest,
        scope: CallScope,
    ) -> NgcResult<ProtoTaskStatus> {
        let trace = self.advance_scope_trace(&scope, request.ctx.as_ref())?;
        let (target_url, binding, iface_tenant) = self.select_interface()?;
        let headers = Self::sanitized_headers(&scope);
        let msg_json = to_json_string(&request).map_err(|e| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("failed to encode StartTaskRequest payload: {}", e.message()),
            )
        })?;
        let parsed_req = JsonValue::parse(&msg_json).map_err(gap_error_to_ngc)?;
        let data_payload = parsed_req.get("message").cloned().unwrap_or_else(|| {
            let mut m = BTreeMap::new();
            m.insert(
                "operation".to_owned(),
                JsonValue::String(request.operation.clone()),
            );
            JsonValue::Object(m)
        });

        let msg_seq = self.msg_seq.fetch_add(1, Ordering::AcqRel);
        let mut msg_meta = BTreeMap::new();
        trace.insert_into(&mut msg_meta);
        let message = A2aMessage {
            message_id: format!("msg-out-{msg_seq}"),
            context_id: if request.context_id.is_empty() {
                None
            } else {
                Some(request.context_id.clone())
            },
            task_id: None,
            role: A2aRole::User,
            parts: vec![A2aPart::Data {
                data: data_payload,
                media_type: "application/json".to_owned(),
                metadata: BTreeMap::new(),
            }],
            metadata: msg_meta,
            extensions: vec![GAP_TRACE_EXT_URI.to_owned()],
            reference_task_ids: Vec::new(),
        };

        let mut req_body = BTreeMap::new();
        req_body.insert("message".to_owned(), message.to_json_value());
        if let Some(ref t) = iface_tenant {
            req_body.insert("tenant".to_owned(), JsonValue::String(t.clone()));
        }
        let body = match binding {
            AgentProtocolBinding::JsonRpc => {
                let mut rpc = BTreeMap::new();
                rpc.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
                rpc.insert(
                    "id".to_owned(),
                    JsonValue::Int(i64::try_from(msg_seq).unwrap_or(i64::MAX)),
                );
                rpc.insert(
                    "method".to_owned(),
                    JsonValue::String("SendMessage".to_owned()),
                );
                rpc.insert("params".to_owned(), JsonValue::Object(req_body));
                JsonValue::Object(rpc)
            }
            AgentProtocolBinding::HttpJson | AgentProtocolBinding::Grpc => {
                JsonValue::Object(req_body)
            }
        };
        let envelope = A2aOutboundEnvelope {
            target_url,
            binding,
            interface_tenant: iface_tenant,
            headers,
            body,
        };
        let task = self.transport.send_message(&envelope).await?;
        a2a_task_to_proto_status(&task, 1)
    }

    async fn get_task(
        &self,
        request: GetTaskRequest,
        scope: CallScope,
    ) -> NgcResult<ProtoTaskStatus> {
        let (target_url, _, iface_tenant) = self.select_interface()?;
        let headers = Self::sanitized_headers(&scope);
        let task = self
            .transport
            .get_task(
                &target_url,
                iface_tenant.as_deref(),
                &headers,
                &request.task_id,
            )
            .await?;
        a2a_task_to_proto_status(&task, 1)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskRequest,
        scope: CallScope,
    ) -> NgcResult<ProtoTaskStatus> {
        let (target_url, _, iface_tenant) = self.select_interface()?;
        let headers = Self::sanitized_headers(&scope);
        let task = self
            .transport
            .cancel_task(
                &target_url,
                iface_tenant.as_deref(),
                &headers,
                &request.task_id,
            )
            .await?;
        a2a_task_to_proto_status(&task, 1)
    }

    async fn watch_task(
        &self,
        request: WatchTaskRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<ProtoTaskStatus>> {
        let (target_url, _, iface_tenant) = self.select_interface()?;
        let headers = Self::sanitized_headers(&scope);
        self.transport
            .watch_task(
                &target_url,
                iface_tenant.as_deref(),
                &headers,
                &request.task_id,
            )
            .await
    }
}

fn a2a_task_to_proto_status(task: &A2aTask, seq: u64) -> NgcResult<ProtoTaskStatus> {
    let mut artifacts = Vec::with_capacity(task.artifacts.len());
    for artifact in &task.artifacts {
        artifacts.push(a2a_artifact_to_proto(artifact)?);
    }
    let message = task
        .status
        .message
        .as_ref()
        .and_then(|m| extract_text_or_json(&m.parts))
        .unwrap_or_default();
    let input_request = if task.status.state.is_interrupted() {
        if message.is_empty() {
            "A2A peer requested input".to_owned()
        } else {
            message.clone()
        }
    } else {
        String::new()
    };
    Ok(ProtoTaskStatus {
        task_id: task.id.clone(),
        state: task.status.state.to_proto() as i32,
        message,
        artifacts,
        input_request,
        updated_at: None,
        seq,
        error: None,
    })
}

fn a2a_artifact_to_proto(artifact: &A2aArtifact) -> NgcResult<ProtoArtifact> {
    let mut obj = BTreeMap::new();
    obj.insert(
        "id".to_owned(),
        JsonValue::String(artifact.artifact_id.clone()),
    );
    if let Some(ref name) = artifact.name {
        obj.insert("name".to_owned(), JsonValue::String(name.clone()));
    }
    let media_type = artifact
        .parts
        .iter()
        .find_map(|p| match p {
            A2aPart::Data { media_type, .. } => Some(media_type.clone()),
            A2aPart::Text { .. } => None,
        })
        .unwrap_or_else(|| "application/json".to_owned());
    obj.insert("mediaType".to_owned(), JsonValue::String(media_type));
    if let Some(data_val) = artifact.parts.first().map(|p| match p {
        A2aPart::Data { data, .. } => data.clone(),
        A2aPart::Text { text, .. } => {
            let mut m = BTreeMap::new();
            m.insert("text".to_owned(), JsonValue::String(text.clone()));
            JsonValue::Object(m)
        }
    }) {
        let data_struct = match data_val {
            JsonValue::Object(_) => data_val,
            other => {
                let mut wrap = BTreeMap::new();
                wrap.insert("value".to_owned(), other);
                JsonValue::Object(wrap)
            }
        };
        obj.insert("data".to_owned(), data_struct);
    }
    let json = JsonValue::Object(obj).to_jcs_string();
    from_json_slice::<ProtoArtifact>(json.as_bytes()).map_err(|e| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!(
                "failed to convert A2A artifact to Protobuf Artifact: {}",
                e.message()
            ),
        )
    })
}

fn extract_text_or_json(parts: &[A2aPart]) -> Option<String> {
    parts.first().map(|part| match part {
        A2aPart::Text { text, .. } => text.clone(),
        A2aPart::Data { data, .. } => data.to_jcs_string(),
    })
}

fn gap_error_to_ngc(err: GapError) -> NgcErrorInfo {
    let code = match err.code() {
        ErrorCode::LoopDetected | ErrorCode::HopLimit => NgcErrorCode::FailedPrecondition,
        ErrorCode::ValidationError | ErrorCode::ParseError => NgcErrorCode::InvalidArgument,
        ErrorCode::Unauthenticated => NgcErrorCode::Unauthenticated,
        ErrorCode::Forbidden | ErrorCode::TenantMismatch => NgcErrorCode::PermissionDenied,
        ErrorCode::TaskNotFound => NgcErrorCode::NotFound,
        ErrorCode::RateLimited => NgcErrorCode::ResourceExhausted,
        ErrorCode::Overloaded => NgcErrorCode::Unavailable,
        ErrorCode::DeadlineExceeded => NgcErrorCode::DeadlineExceeded,
        _ => NgcErrorCode::Internal,
    };
    NgcErrorInfo::new(code, format!("{}: {}", err.code(), err.message()))
}

fn unique_graphql_ident(raw: &str, seen: &mut BTreeSet<String>) -> String {
    let mut base = String::with_capacity(raw.len() + 2);
    for (idx, ch) in raw.chars().enumerate() {
        let valid = ch.is_ascii_alphanumeric() || ch == '_';
        if idx == 0 && ch.is_ascii_digit() {
            base.push('s');
        }
        base.push(if valid { ch } else { '_' });
    }
    if base.is_empty() {
        base.push_str("a2a_skill");
    }
    if seen.insert(base.clone()) {
        return base;
    }
    let mut n = 2_u32;
    loop {
        let candidate = format!("{base}_{n}");
        if seen.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}
