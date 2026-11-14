//! `GraphqlConnector<T>` implementing the NGC `Connector` trait (`spec/09-compat.md` §5.4, `NGC-020`, `NGC-025`, `NGC-026`, `ADR-023`, `SEC-011`, `INV-18`).

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::stream::{self, Stream};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    Capabilities, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, HealthRequest,
    HealthResponse, Limits, MutateRequest, MutateResponse, MutationOutcome, OpKind, PaginationKind,
    Protocol, QueryRequest, QueryResponse, RowSetChunk, ServingStatus, query_response,
};
use http::{HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::config::{GraphqlConfig, UpstreamSchemaSource, resolve_auth_headers, validate_endpoint};
use crate::rewrite::{
    build_aliased_batch_call, build_entities_batch_call, build_single_call,
    extract_aliased_batch_results, extract_entities_batch_results, extract_single_result,
    merge_operation_args,
};
use crate::schema::{
    ExtractedGraphqlOp, GraphqlSchemaBundle, INTROSPECTION_QUERY, build_bundle_from_sdl,
    extract_graphql_schema,
};
use crate::transport::{
    GraphqlHttpRequest, GraphqlTransport, ReqwestGraphqlTransport, build_default_transport,
    build_outbound_headers,
};

const SATURATED_RETRY_AFTER_MS: u32 = 100;

struct Shared<T> {
    config: GraphqlConfig,
    transport: T,
    auth_headers: Vec<(HeaderName, HeaderValue)>,
    bundle: GraphqlSchemaBundle,
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

/// Outbound upstream GraphQL [`Connector`] adapter (`L4`, `spec/09-compat.md` §5.4).
pub struct GraphqlConnector<T = ReqwestGraphqlTransport> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for GraphqlConnector<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> std::fmt::Debug for GraphqlConnector<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphqlConnector")
            .field("connector_id", &self.shared.config.connector_id)
            .field("endpoint", &self.shared.config.endpoint)
            .field("branch_namespace", &self.shared.config.branch_namespace)
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

impl GraphqlConnector<ReqwestGraphqlTransport> {
    /// Initializes a `reqwest`-backed [`GraphqlConnector`], fetching live `__schema` introspection
    /// when `UpstreamSchemaSource::LiveIntrospection` is configured.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` / `FAILED_PRECONDITION` / `UPSTREAM_ERROR` on invalid configuration,
    /// unsafe credentials, or introspection/SDL failure.
    pub async fn connect(config: GraphqlConfig) -> NgcResult<Self> {
        let transport = build_default_transport(&config)?;
        Self::with_transport(config, transport).await
    }
}

impl<T: GraphqlTransport> GraphqlConnector<T> {
    /// Creates a [`GraphqlConnector`] over a custom [`GraphqlTransport`], resolving credentials and schema.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` / `FAILED_PRECONDITION` / `UPSTREAM_ERROR` on config, credential,
    /// or schema normalization failures.
    pub async fn with_transport(config: GraphqlConfig, transport: T) -> NgcResult<Self> {
        validate_config(&config)?;
        let auth_headers = resolve_auth_headers(
            &config.credentials,
            &config.endpoint,
            config.allow_insecure_non_loopback,
        )?;

        let bundle = match &config.schema_source {
            UpstreamSchemaSource::LiveIntrospection => {
                let timeout = Duration::from_millis(u64::from(config.default_timeout_ms.max(1)));
                let req = GraphqlHttpRequest {
                    query: INTROSPECTION_QUERY.to_owned(),
                    variables: Map::new(),
                    operation_name: Some("GapIntrospectionQuery".to_owned()),
                    headers: auth_headers.clone(),
                    timeout,
                };
                let wire = transport.post(req).await?;
                let data = wire.data.ok_or_else(|| {
                    NgcErrorInfo::new(
                        NgcErrorCode::UpstreamError,
                        "upstream GraphQL introspection returned no `data`",
                    )
                })?;
                extract_graphql_schema(&UpstreamSchemaSource::IntrospectionJson(data), &config)?
            }
            src => extract_graphql_schema(src, &config)?,
        };

        Ok(Self::from_bundle(config, transport, auth_headers, bundle))
    }

    /// Synchronous constructor when caller supplies upstream SDL directly (`UpstreamSchemaSource::Sdl`).
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` / `FAILED_PRECONDITION` on invalid SDL or config.
    pub fn from_sdl(config: GraphqlConfig, transport: T, sdl: &str) -> NgcResult<Self> {
        validate_config(&config)?;
        let auth_headers = resolve_auth_headers(
            &config.credentials,
            &config.endpoint,
            config.allow_insecure_non_loopback,
        )?;
        let bundle = build_bundle_from_sdl(sdl, &config)?;
        Ok(Self::from_bundle(config, transport, auth_headers, bundle))
    }

    fn from_bundle(
        config: GraphqlConfig,
        transport: T,
        auth_headers: Vec<(HeaderName, HeaderValue)>,
        bundle: GraphqlSchemaBundle,
    ) -> Self {
        let description = build_description(&config, &bundle);
        let cap = usize::try_from(config.max_in_flight.max(1)).unwrap_or(1);
        Self {
            shared: Arc::new(Shared {
                config,
                transport,
                auth_headers,
                bundle,
                description,
                in_flight_gate: Arc::new(Semaphore::new(cap)),
            }),
        }
    }

    /// Borrows the normalized [`GraphqlSchemaBundle`] (namespaced SDL, Relay flags, Federation entities).
    #[must_use]
    pub fn schema_bundle(&self) -> &GraphqlSchemaBundle {
        &self.shared.bundle
    }
}

fn validate_config(config: &GraphqlConfig) -> NgcResult<()> {
    if config.connector_id.trim().is_empty() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GraphqlConfig.connector_id must not be empty",
        ));
    }
    if config.max_in_flight == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GraphqlConfig.max_in_flight must be >= 1",
        ));
    }
    if config.default_timeout_ms == 0 {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GraphqlConfig.default_timeout_ms must be >= 1",
        ));
    }
    validate_endpoint(
        &config.endpoint,
        !config.credentials.is_empty(),
        config.allow_insecure_non_loopback,
    )?;
    Ok(())
}

fn build_description(config: &GraphqlConfig, bundle: &GraphqlSchemaBundle) -> ConnectorDescription {
    let any_relay = bundle.operations.values().any(|op| op.relay.is_some());
    ConnectorDescription {
        connector_id: config.connector_id.clone(),
        connector_version: config.connector_version.clone(),
        ngc_version: NGC_WIRE_VERSION.to_owned(),
        protocol: Protocol::Graphql as i32,
        spec_hash: bundle.spec_hash.clone(),
        capabilities: Some(Capabilities {
            foreach_batch: config.max_batch_size > 0,
            predicate_ops: Vec::new(),
            order_by: false,
            pagination: if any_relay {
                PaginationKind::PaginationCursor as i32
            } else {
                PaginationKind::PaginationNone as i32
            },
            native_traverse: false,
            subscribe: false,
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

fn remaining_timeout(config: &GraphqlConfig, scope: &CallScope) -> NgcResult<Duration> {
    check_scope(scope)?;
    let rem = scope.deadline.saturating_duration_since(Instant::now());
    if rem.is_zero() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "call deadline exceeded",
        ));
    }
    let default_d = Duration::from_millis(u64::from(config.default_timeout_ms.max(1)));
    Ok(rem.min(default_d))
}

fn lookup_op<'a>(bundle: &'a GraphqlSchemaBundle, name: &str) -> NgcResult<&'a ExtractedGraphqlOp> {
    bundle.operations.get(name).ok_or_else(|| {
        NgcErrorInfo::new(
            NgcErrorCode::NotFound,
            format!("unknown GraphQL operation `{name}`"),
        )
    })
}

fn prost_struct_to_value_map(st: &prost_types::Struct) -> Map<String, Value> {
    gap_ngc_sdk::error::prost_struct_to_json_map(st)
}

fn merge_args_with_value_map(
    op: &ExtractedGraphqlOp,
    base: Option<&prost_types::Struct>,
    overlay_map: &Map<String, Value>,
    page: Option<&gap_proto::ngc::v1::Page>,
) -> Map<String, Value> {
    let mut merged = merge_operation_args(op, base, None, page);
    for (k, v) in overlay_map {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

impl<T: GraphqlTransport> Connector for GraphqlConnector<T> {
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
                    "GraphQL connector max_in_flight saturated",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })?;

        let op = lookup_op(&self.shared.bundle, &request.operation)?.clone();
        if op.kind != OpKind::Query {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "GraphQL operation `{}` is {:?} and cannot execute via Query",
                    request.operation, op.kind
                ),
            ));
        }

        let headers = build_outbound_headers(&self.shared.auth_headers, &scope);

        if request.foreach.is_empty() {
            let started = Instant::now();
            let timeout = remaining_timeout(&self.shared.config, &scope)?;
            let args_map =
                merge_operation_args(&op, request.arguments.as_ref(), None, request.page.as_ref());
            let prepared = build_single_call(
                &op,
                &args_map,
                request.selection.as_ref(),
                &self.shared.bundle.upstream_schema,
                &self.shared.bundle.branch_to_upstream_type,
            )?;
            let wire = self
                .shared
                .transport
                .post(GraphqlHttpRequest {
                    query: prepared.query,
                    variables: prepared.variables,
                    operation_name: Some(prepared.operation_name),
                    headers,
                    timeout,
                })
                .await?;
            let decoded = extract_single_result(
                &op,
                wire.data.as_ref(),
                &wire.errors,
                prepared.relay_unwrap,
            )?;
            let rows_total = u64::try_from(decoded.rows.len()).unwrap_or(u64::MAX);
            let latency_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);

            let mut chunks = vec![Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Rows(RowSetChunk {
                    foreach_index: 0,
                    rows: decoded.rows,
                    last_for_index: true,
                    error: None,
                })),
            })];
            chunks.push(Ok(QueryResponse {
                chunk: Some(query_response::Chunk::Complete(Complete {
                    next_cursor: decoded.next_cursor,
                    truncated: false,
                    rows_total,
                    upstream_calls: 1,
                    latency_us,
                })),
            }));
            return Ok(Box::pin(GuardedQueryStream {
                inner: Box::pin(stream::iter(chunks)),
                _permit: permit,
            }));
        }

        let max_batch = usize::try_from(self.shared.config.max_batch_size).unwrap_or(0);
        if max_batch == 0 {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "foreach is unsupported when GraphqlConfig.max_batch_size == 0",
            ));
        }

        // Batch up to `max_batch_size` distinct variable sets into ONE upstream HTTP call
        // (either Apollo Federation `_entities(representations: $reps)` or aliased root fields `_b0.._bN`)
        // so `N` distinct keys execute in `ceil(N / max_batch_size)` calls (`NGC-020`, `NGC-026`, `INV-18`).
        let max_in_flight = usize::try_from(self.shared.config.max_in_flight.max(1)).unwrap_or(1);
        let foreach_cfg = ForeachConfig::new(max_batch, max_in_flight);

        let shared = Arc::clone(&self.shared);
        let base_args = request.arguments.clone();
        let page = request.page.clone();
        let selection = request.selection.clone();
        let scope_for_batch = scope.clone();

        let run = foreach(request.foreach, foreach_cfg, &scope, move |inputs| {
            let shared = Arc::clone(&shared);
            let op = op.clone();
            let base_args = base_args.clone();
            let page = page.clone();
            let selection = selection.clone();
            let headers = headers.clone();
            let scope_call = scope_for_batch.clone();

            async move {
                let timeout = remaining_timeout(&shared.config, &scope_call)?;
                let mut merged_batch = Vec::with_capacity(inputs.len());
                for item in &inputs {
                    let item_map = prost_struct_to_value_map(item);
                    merged_batch.push(merge_args_with_value_map(
                        &op,
                        base_args.as_ref(),
                        &item_map,
                        page.as_ref(),
                    ));
                }

                // Prefer `_entities(representations: $reps)` when enabled & all required keys exist.
                let use_entities = op.entity_batch.as_ref().filter(|eb| {
                    merged_batch
                        .iter()
                        .all(|m| eb.key_fields.iter().all(|k| m.contains_key(k)))
                });

                if let Some(entity_map) = use_entities {
                    let prepared = build_entities_batch_call(
                        &op,
                        entity_map,
                        &merged_batch,
                        selection.as_ref(),
                        &shared.bundle.upstream_schema,
                        &shared.bundle.branch_to_upstream_type,
                    )?;
                    let wire = shared
                        .transport
                        .post(GraphqlHttpRequest {
                            query: prepared.query,
                            variables: prepared.variables,
                            operation_name: Some(prepared.operation_name),
                            headers,
                            timeout,
                        })
                        .await?;
                    extract_entities_batch_results(
                        merged_batch.len(),
                        wire.data.as_ref(),
                        &wire.errors,
                    )
                } else {
                    let (prepared, aliases) = build_aliased_batch_call(
                        &op,
                        &merged_batch,
                        selection.as_ref(),
                        &shared.bundle.upstream_schema,
                        &shared.bundle.branch_to_upstream_type,
                    )?;
                    let wire = shared
                        .transport
                        .post(GraphqlHttpRequest {
                            query: prepared.query,
                            variables: prepared.variables,
                            operation_name: Some(prepared.operation_name),
                            headers,
                            timeout,
                        })
                        .await?;
                    extract_aliased_batch_results(
                        &op,
                        &aliases,
                        wire.data.as_ref(),
                        &wire.errors,
                        prepared.relay_unwrap,
                    )
                }
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
                    "GraphQL connector max_in_flight saturated",
                )
                .with_retry(true, SATURATED_RETRY_AFTER_MS)
            })?;

        if self.shared.config.read_only {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::PermissionDenied,
                "GraphQL connector is configured read_only",
            ));
        }

        let op = lookup_op(&self.shared.bundle, &request.operation)?.clone();
        if op.kind != OpKind::Mutation {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "GraphQL operation `{}` is {:?}, not a Mutation",
                    request.operation, op.kind
                ),
            ));
        }

        let args_map = merge_operation_args(&op, request.arguments.as_ref(), None, None);
        let prepared = build_single_call(
            &op,
            &args_map,
            request.selection.as_ref(),
            &self.shared.bundle.upstream_schema,
            &self.shared.bundle.branch_to_upstream_type,
        )?;

        if request.dry_run {
            if !self.shared.config.supports_dry_run {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::Unimplemented,
                    "dry_run is disabled on this GraphQL connector",
                ));
            }
            let mut report = Map::new();
            report.insert("dryRun".to_owned(), json!(true));
            report.insert("operation".to_owned(), json!(op.name));
            report.insert("query".to_owned(), Value::String(prepared.query));
            return Ok(MutateResponse {
                result: None,
                outcome: MutationOutcome::OutcomeDryRun as i32,
                was_replayed: false,
                compensation_token: String::new(),
                error: None,
                dry_run_report: Some(json_map_to_prost_struct(&report)),
            });
        }

        let timeout = remaining_timeout(&self.shared.config, &scope)?;
        let headers = build_outbound_headers(&self.shared.auth_headers, &scope);
        let wire = self
            .shared
            .transport
            .post(GraphqlHttpRequest {
                query: prepared.query,
                variables: prepared.variables,
                operation_name: Some(prepared.operation_name),
                headers,
                timeout,
            })
            .await?;

        let decoded = extract_single_result(&op, wire.data.as_ref(), &wire.errors, false)?;
        let result = decoded.rows.into_iter().next().or_else(|| {
            let mut wrap = Map::new();
            wrap.insert("result".to_owned(), Value::Null);
            Some(json_map_to_prost_struct(&wrap))
        });

        Ok(MutateResponse {
            result,
            outcome: MutationOutcome::OutcomeApplied as i32,
            was_replayed: false,
            compensation_token: String::new(),
            error: None,
            dry_run_report: None,
        })
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

#[allow(dead_code)]
fn _unused(_: BTreeMap<String, String>) {}
