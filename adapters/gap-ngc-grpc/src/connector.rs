//! `GrpcConnector<T>` implementing the NGC `Connector` trait (`spec/09-compat.md` §5.3, `NGC-020`, `NGC-025`, `NGC-026`, `SEC-011`, `INV-15`).

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, Stream, StreamExt as _};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, Event,
    HealthRequest, HealthResponse, Limits, MutateRequest, MutateResponse, MutationOutcome, OpKind,
    PaginationKind, Protocol, QueryRequest, QueryResponse, RowSetChunk, ServingStatus,
    SubscribeRequest, query_response,
};
use prost_reflect::MessageDescriptor;
use serde_json::{Map, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tonic::metadata::{Ascii, MetadataKey, MetadataMap, MetadataValue};

use crate::codec::{decode_response_row, encode_request_bytes};
use crate::config::{GrpcConfig, attach_credentials};
use crate::reflect::{GrpcTransport, TonicChannelTransport, load_descriptors};
use crate::schema::{ExtractedGrpcMethod, GrpcSchemaBundle, extract_grpc_schema};

const SATURATED_RETRY_AFTER_MS: u32 = 100;

struct Shared<T> {
    config: GrpcConfig,
    transport: T,
    bundle: GrpcSchemaBundle,
    description: ConnectorDescription,
    in_flight_gate: Arc<Semaphore>,
}

struct GuardedQueryStream {
    inner: BoxStream<QueryResponse>,
    _permit: OwnedSemaphorePermit,
}

impl Stream for GuardedQueryStream {
    type Item = NgcResult<QueryResponse>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

struct GuardedEventStream {
    inner: BoxStream<Event>,
    _permit: OwnedSemaphorePermit,
}

impl Stream for GuardedEventStream {
    type Item = NgcResult<Event>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Dynamic gRPC / Protobuf [`Connector`] adapter (`L4`, `spec/09-compat.md` §5.3).
pub struct GrpcConnector<T = TonicChannelTransport> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for GrpcConnector<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> std::fmt::Debug for GrpcConnector<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcConnector")
            .field("connector_id", &self.shared.config.connector_id)
            .field("endpoint", &self.shared.config.endpoint)
            .field("trust_tier", &self.shared.config.trust_tier)
            .field(
                "operations",
                &self
                    .shared
                    .bundle
                    .descriptors
                    .iter()
                    .map(|d| &d.name)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl GrpcConnector<TonicChannelTransport> {
    /// Connects to the configured gRPC endpoint lazily and loads descriptors from [`GrpcConfig::source`].
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` / `FAILED_PRECONDITION` / `UNAVAILABLE` if endpoint, credentials,
    /// or descriptor loading fails.
    pub async fn connect(config: GrpcConfig) -> NgcResult<Self> {
        let max_dec = usize::try_from(config.max_response_bytes)
            .unwrap_or(usize::MAX)
            .max(64 * 1024);
        let transport = TonicChannelTransport::connect_lazy(&config.endpoint, max_dec)?;
        Self::with_transport(config, transport).await
    }
}

impl<T: GrpcTransport> GrpcConnector<T> {
    /// Creates a [`GrpcConnector`] over an explicit [`GrpcTransport`], loading descriptors from [`GrpcConfig::source`].
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` / `FAILED_PRECONDITION` / `UPSTREAM_ERROR` if credentials or descriptor loading fails.
    pub async fn with_transport(config: GrpcConfig, transport: T) -> NgcResult<Self> {
        validate_config(&config)?;
        let mut init_meta = MetadataMap::new();
        attach_credentials(
            &config.credentials,
            &config.endpoint,
            config.allow_insecure_non_loopback,
            &mut init_meta,
        )?;
        let timeout_dur = Duration::from_millis(u64::from(config.default_timeout_ms.max(1)));
        let loaded =
            load_descriptors(&config.source, Some(&transport), &init_meta, timeout_dur).await?;
        let bundle = extract_grpc_schema(&loaded.pool, loaded.spec_hash, &config)?;
        Ok(Self::from_bundle(config, transport, bundle))
    }

    /// Constructs a [`GrpcConnector`] from an already-extracted [`GrpcSchemaBundle`].
    #[must_use]
    pub fn from_bundle(config: GrpcConfig, transport: T, bundle: GrpcSchemaBundle) -> Self {
        let description = build_description(&config, &bundle);
        let in_flight_cap = usize::try_from(config.max_in_flight.max(1)).unwrap_or(1);
        Self {
            shared: Arc::new(Shared {
                config,
                transport,
                bundle,
                description,
                in_flight_gate: Arc::new(Semaphore::new(in_flight_cap)),
            }),
        }
    }

    /// Returns the extracted [`GrpcSchemaBundle`] (descriptors, operation map, GAP-QL SDL).
    #[must_use]
    pub fn schema_bundle(&self) -> &GrpcSchemaBundle {
        &self.shared.bundle
    }
}

fn validate_config(config: &GrpcConfig) -> NgcResult<()> {
    if config.connector_id.trim().is_empty() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GrpcConfig.connector_id must not be empty",
        ));
    }
    if config.max_in_flight == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GrpcConfig.max_in_flight must be >= 1",
        ));
    }
    if config.default_timeout_ms == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GrpcConfig.default_timeout_ms must be >= 1",
        ));
    }
    crate::config::ensure_credential_transport_safe(
        &config.endpoint,
        config.allow_insecure_non_loopback,
    )?;
    Ok(())
}

fn build_description(config: &GrpcConfig, bundle: &GrpcSchemaBundle) -> ConnectorDescription {
    let has_subscribe = bundle
        .descriptors
        .iter()
        .any(|d| d.kind == OpKind::Subscription as i32);
    ConnectorDescription {
        connector_id: config.connector_id.clone(),
        connector_version: config.connector_version.clone(),
        ngc_version: NGC_WIRE_VERSION.to_owned(),
        protocol: Protocol::Grpc as i32,
        spec_hash: bundle.spec_hash.clone(),
        capabilities: Some(Capabilities {
            foreach_batch: config.max_batch_size > 0,
            predicate_ops: Vec::new(),
            order_by: false,
            pagination: PaginationKind::PaginationNone as i32,
            native_traverse: false,
            subscribe: has_subscribe,
            tasks: false,
            dry_run: config.supports_dry_run,
            idempotency: false,
            sketch: false,
            explain: false,
            as_of: false,
        }),
        limits: Some(Limits {
            max_batch_size: config.max_batch_size,
            max_in_flight: config.max_in_flight,
            max_page_size: config.max_page_size,
            default_timeout_ms: config.default_timeout_ms,
            max_response_bytes: config.max_response_bytes,
        }),
        schema_sdl: bundle.schema_sdl.clone(),
        operations: bundle.descriptors.clone(),
        meta: None,
        auth: None,
    }
}

fn check_scope(scope: &CallScope) -> NgcResult<()> {
    if scope.cancel.is_cancelled() {
        return Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"));
    }
    if Instant::now() >= scope.deadline {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "call deadline exceeded",
        ));
    }
    Ok(())
}

fn effective_timeout(
    config: &GrpcConfig,
    method: &ExtractedGrpcMethod,
    scope: &CallScope,
) -> NgcResult<Duration> {
    check_scope(scope)?;
    let remaining = scope.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "call deadline exceeded",
        ));
    }
    let method_limit = config
        .method_deadlines
        .get(&method.operation_name)
        .or_else(|| config.method_deadlines.get(&method.full_grpc_name))
        .copied()
        .unwrap_or_else(|| Duration::from_millis(u64::from(config.default_timeout_ms.max(1))));
    Ok(remaining.min(method_limit))
}

fn build_outbound_metadata(config: &GrpcConfig, scope: &CallScope) -> NgcResult<MetadataMap> {
    let mut meta = MetadataMap::new();
    // Attach ONLY connector-configured credentials; never pass through caller bearer tokens (SEC-011).
    attach_credentials(
        &config.credentials,
        &config.endpoint,
        config.allow_insecure_non_loopback,
        &mut meta,
    )?;

    if !scope.traceparent.trim().is_empty()
        && let Ok(val) = MetadataValue::<Ascii>::try_from(scope.traceparent.as_str())
    {
        meta.insert("traceparent", val);
    }
    if !scope.request_id.trim().is_empty()
        && let Ok(val) = MetadataValue::<Ascii>::try_from(scope.request_id.as_str())
    {
        let key = MetadataKey::<Ascii>::from_static("x-request-id");
        meta.insert(key, val);
    }
    Ok(meta)
}

fn merge_struct_args(
    base: Option<&prost_types::Struct>,
    overlay: Option<&prost_types::Struct>,
) -> prost_types::Struct {
    let mut fields = BTreeMap::new();
    if let Some(b) = base {
        for (k, v) in &b.fields {
            fields.insert(k.clone(), v.clone());
        }
    }
    if let Some(o) = overlay {
        for (k, v) in &o.fields {
            fields.insert(k.clone(), v.clone());
        }
    }
    prost_types::Struct { fields }
}

fn lookup_method<'a>(
    bundle: &'a GrpcSchemaBundle,
    op_name: &str,
) -> NgcResult<&'a ExtractedGrpcMethod> {
    bundle.methods.get(op_name).ok_or_else(|| {
        NgcErrorInfo::new(
            NgcErrorCode::NotFound,
            format!("unknown gRPC operation `{op_name}`"),
        )
    })
}

fn now_timestamp() -> prost_types::Timestamp {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => prost_types::Timestamp {
            seconds: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            nanos: i32::try_from(d.subsec_nanos()).unwrap_or(0),
        },
        Err(_) => prost_types::Timestamp::default(),
    }
}

fn decode_to_value(
    desc: &MessageDescriptor,
    raw: &[u8],
    selection: Option<&gap_proto::ngc::v1::Selection>,
) -> NgcResult<prost_types::Struct> {
    decode_response_row(desc, raw, selection)
}

impl<T: GrpcTransport> Connector for GrpcConnector<T> {
    async fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> NgcResult<DescribeResponse> {
        check_scope(&scope)?;
        if !request.known_spec_hash.is_empty()
            && request.known_spec_hash == self.shared.bundle.spec_hash
        {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }
        Ok(DescribeResponse {
            not_modified: false,
            description: Some(self.shared.description.clone()),
        })
    }

    async fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        check_scope(&scope)?;
        let permit = Arc::clone(&self.shared.in_flight_gate)
            .try_acquire_owned()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::ResourceExhausted,
                    "gRPC connector max_in_flight saturated",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })?;

        let method = lookup_method(&self.shared.bundle, &request.operation)?.clone();
        if method.kind != OpKind::Query {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "gRPC operation `{}` has kind {:?} and cannot execute via Query",
                    request.operation, method.kind
                ),
            ));
        }

        let outbound_meta = build_outbound_metadata(&self.shared.config, &scope)?;
        let call_timeout = effective_timeout(&self.shared.config, &method, &scope)?;

        if request.foreach.is_empty() {
            let started = Instant::now();
            let req_bytes = encode_request_bytes(&method.input_desc, request.arguments.as_ref())?;
            let resp_bytes = self
                .shared
                .transport
                .unary(
                    method.grpc_path.clone(),
                    outbound_meta,
                    req_bytes,
                    call_timeout,
                )
                .await?;
            let row =
                decode_to_value(&method.output_desc, &resp_bytes, request.selection.as_ref())?;
            let latency_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);

            let items = vec![
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                        foreach_index: 0,
                        rows: vec![row],
                        last_for_index: true,
                        error: None,
                    })),
                }),
                Ok(QueryResponse {
                    chunk: Some(query_response::Chunk::Complete(Complete {
                        next_cursor: String::new(),
                        truncated: false,
                        rows_total: 1,
                        upstream_calls: 1,
                        latency_us,
                    })),
                }),
            ];
            return Ok(Box::pin(GuardedQueryStream {
                inner: Box::pin(stream::iter(items)),
                _permit: permit,
            }));
        }

        let max_batch = usize::try_from(self.shared.config.max_batch_size).unwrap_or(usize::MAX);
        if max_batch == 0 {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "foreach is unsupported when GrpcConfig.max_batch_size == 0",
            ));
        }

        // Emulated foreach (`batch=emulated`, CMP-015): pass `max_batch_size = 1` to `foreach`
        // so deduplication + bounded concurrency run across distinct keys and `Complete.upstream_calls`
        // accurately reflects 1 upstream unary gRPC RPC per distinct variable set (`NGC-026`).
        let max_in_flight = usize::try_from(self.shared.config.max_in_flight.max(1)).unwrap_or(1);
        let foreach_cfg = ForeachConfig::new(1, max_in_flight);

        let shared = Arc::clone(&self.shared);
        let base_args = request.arguments.clone();
        let selection = request.selection.clone();
        let scope_for_closure = scope.clone();

        let run = foreach(request.foreach, foreach_cfg, &scope, move |inputs| {
            let shared = Arc::clone(&shared);
            let method = method.clone();
            let base_args = base_args.clone();
            let selection = selection.clone();
            let outbound_meta = outbound_meta.clone();
            let scope_for_call = scope_for_closure.clone();

            async move {
                let mut results = Vec::with_capacity(inputs.len());
                for var_struct in inputs {
                    let merged = merge_struct_args(base_args.as_ref(), Some(&var_struct));
                    let item_res = match effective_timeout(&shared.config, &method, &scope_for_call)
                    {
                        Err(e) => Err(e),
                        Ok(t) => match encode_request_bytes(&method.input_desc, Some(&merged)) {
                            Err(e) => Err(e),
                            Ok(payload) => match shared
                                .transport
                                .unary(method.grpc_path.clone(), outbound_meta.clone(), payload, t)
                                .await
                            {
                                Err(e) => Err(e),
                                Ok(raw) => {
                                    decode_to_value(&method.output_desc, &raw, selection.as_ref())
                                        .map(|row| vec![row])
                                }
                            },
                        },
                    };
                    results.push(item_res);
                }
                Ok(results)
            }
        })?;

        Ok(Box::pin(GuardedQueryStream {
            inner: run.into_query_stream(),
            _permit: permit,
        }))
    }

    async fn mutate(&self, request: MutateRequest, scope: CallScope) -> NgcResult<MutateResponse> {
        check_scope(&scope)?;
        let _permit = Arc::clone(&self.shared.in_flight_gate)
            .try_acquire_owned()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::ResourceExhausted,
                    "gRPC connector max_in_flight saturated",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })?;

        let method = lookup_method(&self.shared.bundle, &request.operation)?.clone();
        if method.kind == OpKind::Subscription {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "server-streaming gRPC operation `{}` must use Subscribe",
                    request.operation
                ),
            ));
        }

        // Validate arguments before dry-run or upstream transmission.
        let req_bytes = encode_request_bytes(&method.input_desc, request.arguments.as_ref())?;

        if request.dry_run {
            if !self.shared.config.supports_dry_run {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::Unimplemented,
                    "dry_run is disabled on this gRPC connector",
                ));
            }
            let mut report = Map::new();
            report.insert("dryRun".to_owned(), json!(true));
            report.insert("grpcMethod".to_owned(), json!(method.full_grpc_name));
            report.insert("encodedBytes".to_owned(), json!(req_bytes.len()));
            return Ok(MutateResponse {
                result: None,
                outcome: MutationOutcome::OutcomeDryRun as i32,
                was_replayed: false,
                compensation_token: String::new(),
                error: None,
                dry_run_report: Some(json_map_to_prost_struct(&report)),
            });
        }

        let outbound_meta = build_outbound_metadata(&self.shared.config, &scope)?;
        let call_timeout = effective_timeout(&self.shared.config, &method, &scope)?;
        let resp_bytes = self
            .shared
            .transport
            .unary(
                method.grpc_path.clone(),
                outbound_meta,
                req_bytes,
                call_timeout,
            )
            .await?;
        let row = decode_to_value(&method.output_desc, &resp_bytes, request.selection.as_ref())?;

        Ok(MutateResponse {
            result: Some(row),
            outcome: MutationOutcome::OutcomeApplied as i32,
            was_replayed: false,
            compensation_token: String::new(),
            error: None,
            dry_run_report: None,
        })
    }

    async fn subscribe(
        &self,
        request: SubscribeRequest,
        scope: CallScope,
    ) -> NgcResult<BoxStream<Event>> {
        check_scope(&scope)?;
        let permit = Arc::clone(&self.shared.in_flight_gate)
            .try_acquire_owned()
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::ResourceExhausted,
                    "gRPC connector max_in_flight saturated",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })?;

        let method = lookup_method(&self.shared.bundle, &request.operation)?.clone();
        if method.kind != OpKind::Subscription {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "gRPC operation `{}` is {:?}, not a server-streaming SUBSCRIPTION",
                    request.operation, method.kind
                ),
            ));
        }

        let outbound_meta = build_outbound_metadata(&self.shared.config, &scope)?;
        let call_timeout = effective_timeout(&self.shared.config, &method, &scope)?;
        let req_bytes = encode_request_bytes(&method.input_desc, request.arguments.as_ref())?;

        let upstream = self
            .shared
            .transport
            .server_streaming(
                method.grpc_path.clone(),
                outbound_meta,
                req_bytes,
                call_timeout,
            )
            .await?;

        let output_desc = method.output_desc.clone();
        let selection = request.selection.clone();
        let cancel = scope.cancel;
        let deadline = scope.deadline;

        struct SubState {
            upstream: BoxStream<bytes::Bytes>,
            output_desc: MessageDescriptor,
            selection: Option<gap_proto::ngc::v1::Selection>,
            cancel: tokio_util::sync::CancellationToken,
            deadline: Instant,
            seq: u64,
            done: bool,
        }

        let init = SubState {
            upstream,
            output_desc,
            selection,
            cancel,
            deadline,
            seq: 0,
            done: false,
        };

        let event_stream: BoxStream<Event> = Box::pin(stream::unfold(init, |mut st| async move {
            if st.done {
                return None;
            }
            if st.cancel.is_cancelled() {
                st.done = true;
                return Some((
                    Err(NgcErrorInfo::new(
                        NgcErrorCode::Cancelled,
                        "subscription cancelled",
                    )),
                    st,
                ));
            }
            let remaining = st.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                st.done = true;
                return Some((
                    Err(NgcErrorInfo::new(
                        NgcErrorCode::DeadlineExceeded,
                        "subscription deadline exceeded",
                    )),
                    st,
                ));
            }

            let next_item = tokio::select! {
                biased;
                () = st.cancel.cancelled() => {
                    st.done = true;
                    return Some((
                        Err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "subscription cancelled")),
                        st,
                    ));
                }
                () = tokio::time::sleep(remaining) => {
                    st.done = true;
                    return Some((
                        Err(NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "subscription deadline exceeded")),
                        st,
                    ));
                }
                item = st.upstream.next() => item,
            };

            match next_item {
                None => None,
                Some(Err(err)) => {
                    st.done = true;
                    Some((Err(err), st))
                }
                Some(Ok(raw_bytes)) => {
                    match decode_to_value(&st.output_desc, &raw_bytes, st.selection.as_ref()) {
                        Err(err) => {
                            st.done = true;
                            Some((Err(err), st))
                        }
                        Ok(payload) => {
                            st.seq = st.seq.saturating_add(1);
                            let ev = Event {
                                id: st.seq.to_string(),
                                at: Some(now_timestamp()),
                                payload: Some(payload),
                                resume_token: st.seq.to_string(),
                            };
                            Some((Ok(ev), st))
                        }
                    }
                }
            }
        }));

        Ok(Box::pin(GuardedEventStream {
            inner: event_stream,
            _permit: permit,
        }))
    }

    async fn health(&self, _request: HealthRequest, scope: CallScope) -> NgcResult<HealthResponse> {
        check_scope(&scope)?;
        Ok(HealthResponse {
            status: ServingStatus::Serving as i32,
            version: self.shared.config.connector_version.clone(),
            details: None,
        })
    }
}
