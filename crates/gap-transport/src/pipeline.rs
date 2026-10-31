//! Typed 6-layer `tower` request pipeline:
//! `TransportMux -> Gateway -> Operation -> Authz -> Plan -> Exec`
//! (`spec/01-architecture.md` §5.1, `RUN-001`, `RUN-002`, `OPS-010`, `OPS-011`, `INV-01`).

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, Response, StatusCode, header};
use bytes::Bytes;
use gap_core::context::{Principal, TraceCtx};
use gap_core::error::{ErrorCode, GapError};
use gap_core::snapshot::Snapshot;
use gap_core::telemetry::tenant_bucket;
use gap_exec::{ExecLayer, ExecRequest, ExecResponse};
use gap_plan::{CachedPlan, PlanConfig, plan_hash};
use gap_ql::{CostModelConfig, ExecutableDirectives, PersistedOperationId, ValidatedOperation};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};
use tracing::Instrument as _;

use crate::app::{AppState, RequestCorrelation};
use crate::config::Profile;
use crate::gateway::{
    authenticate_headers, check_post_content_type, compute_request_deadline, read_body_bounded,
    resolve_request_id, resolve_trace_context,
};
use crate::metrics::RouteLabel;
use crate::operation::{
    CONTENT_TYPE_APPLICATION_JSON, CONTENT_TYPE_GRAPHQL_RESPONSE_JSON, NegotiatedMedia,
    ParsedHttpGraphqlRequest, graphql_request_error_status, negotiate_accept,
    parse_get_query_params, parse_post_json_body, persisted_id_bytes, retry_after_seconds,
    serialize_errors_envelope, serialize_exec_response_envelope,
};
use crate::sse::{
    SseStreamHandle, build_sse_response, format_sse_next, send_next_and_complete,
    sse_request_error_response,
};

/// Error produced by a pipeline stage before GraphQL execution starts.
#[derive(Debug, Clone)]
pub enum PipelineError {
    /// Transport/HTTP/auth/admission failure (`401`, `403`, `405`, `406`, `413`, `415`, `429`, `503`).
    Transport {
        /// HTTP status code.
        status: StatusCode,
        /// Optional `Allow` header value for `405`.
        allow: Option<&'static str>,
        /// Structured GAP error (`extensions.code`, `retry`).
        error: GapError,
    },
    /// GraphQL request-level parse (`400`) or validation/resolution (`422`) failure (`ADR-023`).
    GraphqlRequest {
        /// `400` for JSON/GraphQL syntax failure; `422` for validation, coercion, limits, or persisted lookup.
        status: StatusCode,
        /// One or more structured GAP errors (`data` omitted).
        errors: Vec<GapError>,
    },
}

/// Output produced by [`TransportExecService`] (`Layer 6`).
#[derive(Debug)]
pub enum PipelineOutput {
    /// Assembled JSON response payload (`application/graphql-response+json` or `application/json`).
    Assembled(AssembledPayload),
    /// Open bounded `graphql-sse` event stream (`text/event-stream`).
    Sse(SseStreamHandle),
}

/// Typed input to `TransportMuxService` (`Layer 1`).
#[derive(Debug)]
pub struct TransportMuxRequest {
    /// Matched closed route label.
    pub route: RouteLabel,
    /// Raw incoming HTTP request.
    pub request: Request<Body>,
}

/// Typed input to `GatewayService` (`Layer 2`).
#[derive(Debug)]
pub struct GatewayRequest {
    /// Matched closed route label.
    pub route: RouteLabel,
    /// Validated or minted correlation request ID (`X-Request-Id`).
    pub request_id: String,
    /// W3C trace context (`OPS-010`).
    pub trace: TraceCtx,
    /// Negotiated response format (`Accept`).
    pub media: NegotiatedMedia,
    /// Root `gap.request` span handle for recording downstream attributes (`OPS-010`).
    pub request_span: tracing::Span,
    /// Request cancellation token cancelled on client disconnect / future drop (`RUN-031`).
    pub cancel: CancellationToken,
    /// Raw incoming HTTP request.
    pub request: Request<Body>,
}

/// Typed input to `OperationService` (`Layer 3`), carrying the single pinned [`Snapshot`] (`INV-01`).
#[derive(Debug, Clone)]
pub struct OperationRequest {
    /// Validated/minted request ID.
    pub request_id: String,
    /// W3C trace context.
    pub trace: TraceCtx,
    /// Negotiated response media type.
    pub media: NegotiatedMedia,
    /// Root `gap.request` span handle.
    pub request_span: tracing::Span,
    /// Authenticated caller principal (`SEC-010`).
    pub principal: Principal,
    /// Single pinned [`Snapshot`] for the lifetime of this request (`INV-01`).
    pub snapshot: Arc<Snapshot>,
    /// Is this an HTTP `GET` request?
    pub is_get: bool,
    /// Raw query string from URI (for `GET`).
    pub raw_uri_query: Option<String>,
    /// Bounded raw body bytes (for `POST`).
    pub body_bytes: Bytes,
    /// Cancellation token coupled to connection drop.
    pub cancel: CancellationToken,
}

/// Typed input to `AuthzService` (`Layer 4`).
#[derive(Debug, Clone)]
pub struct AuthzRequest {
    /// Validated/minted request ID.
    pub request_id: String,
    /// W3C trace context.
    pub trace: TraceCtx,
    /// Negotiated response media type.
    pub media: NegotiatedMedia,
    /// Root `gap.request` span handle.
    pub request_span: tracing::Span,
    /// Authenticated caller principal (`SEC-010`).
    pub principal: Principal,
    /// Single pinned [`Snapshot`] (`INV-01`).
    pub snapshot: Arc<Snapshot>,
    /// Parsed HTTP GraphQL request parameters.
    pub http_req: ParsedHttpGraphqlRequest,
    /// Canonical SHA-256 operation identifier.
    pub operation_id: PersistedOperationId,
    /// Whether the operation came from the persisted store.
    pub is_registered: bool,
    /// Validated `apollo-compiler` executable operation and coerced variables.
    pub validated: ValidatedOperation,
    /// Extracted executable directives (`@budget`, `@dryRun`, `@format`).
    pub directives: ExecutableDirectives,
    /// Optional caller cost budget (`costUnits`).
    pub client_cost_budget: Option<u32>,
    /// Whether the caller set any budget or requested `tokensEstimate` (`GQL-050`).
    pub include_tokens_estimate: bool,
    /// Computed monotonic request deadline (`RUN-030`).
    pub deadline: Instant,
    /// Cancellation token coupled to connection drop.
    pub cancel: CancellationToken,
}

/// Typed input to `PlanService` (`Layer 5`).
#[derive(Debug, Clone)]
pub struct PlanStageRequest {
    /// Authorized request payload.
    pub inner: AuthzRequest,
}

/// Typed input to `TransportExecService` (`Layer 6`).
#[derive(Debug, Clone)]
pub struct ExecStageRequest {
    /// Validated/minted request ID.
    pub request_id: String,
    /// Single pinned [`Snapshot`] (`INV-01`).
    pub snapshot: Arc<Snapshot>,
    /// Compiled/cached execution plan (`RUN-014`).
    pub cached_plan: CachedPlan,
    /// Caller-supplied raw JSON variables map.
    pub variables: serde_json::Map<String, serde_json::Value>,
    /// Whether to include `tokensEstimate` in `extensions.gap` (`GQL-050`).
    pub include_tokens_estimate: bool,
    /// Whether the caller negotiated `text/event-stream` (`graphql-sse`).
    pub sse_mode: bool,
    /// Monotonic request deadline (`RUN-030`).
    pub deadline: Instant,
    /// Cancellation token coupled to connection drop (`RUN-031`).
    pub cancel: CancellationToken,
}

/// Assembled wire payload produced by `TransportExecService` under `gap.assemble`.
#[derive(Debug, Clone)]
pub struct AssembledPayload {
    /// Raw execution response from `gap_exec::ExecLayer`.
    pub exec_response: ExecResponse,
    /// Serialized UTF-8 JSON wire body.
    pub body_bytes: Bytes,
}

/// Builds a transport/auth HTTP error response (`Content-Type: application/json; charset=utf-8`,
/// omitting `"data"` per `ADR-023`).
#[must_use]
pub fn build_transport_error_response(
    status: StatusCode,
    allow: Option<&'static str>,
    error: GapError,
    request_id: &str,
) -> Response<Body> {
    let retry_after = matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    )
    .then(|| retry_after_seconds(error.retry().after_ms));

    let body_bytes = serialize_errors_envelope(vec![error], request_id);
    let mut resp = Response::new(Body::from(body_bytes));
    *resp.status_mut() = status;

    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_APPLICATION_JSON),
    );
    if status == StatusCode::UNAUTHORIZED {
        headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    if let Some(allow_methods) = allow {
        headers.insert(header::ALLOW, HeaderValue::from_static(allow_methods));
    }
    if let Some(secs) = retry_after
        && let Ok(hv) = HeaderValue::from_str(&secs.to_string())
    {
        headers.insert(header::RETRY_AFTER, hv);
    }
    resp
}

/// Builds a GraphQL request-level `400`/`422` error response (`Content-Type: application/graphql-response+json`,
/// omitting `"data"` per `ADR-023` and `GraphQLOverHTTP.md` L455–458).
#[must_use]
pub fn build_graphql_request_error_response(
    status: StatusCode,
    errors: Vec<GapError>,
    request_id: &str,
) -> Response<Body> {
    let body_bytes = serialize_errors_envelope(errors, request_id);
    let mut resp = Response::new(Body::from(body_bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_GRAPHQL_RESPONSE_JSON),
    );
    resp
}

/// `tower::Layer` wrapping an inner service with `TransportMuxService` (`Layer 1`).
#[derive(Clone, Debug)]
pub struct TransportMuxLayer {
    /// Shared application state.
    pub state: Arc<AppState>,
}

/// `tower::Service` implementing `TransportMux` (`Layer 1`).
#[derive(Clone, Debug)]
pub struct TransportMuxService<S> {
    /// Shared application state.
    pub state: Arc<AppState>,
    /// Inner `Gateway` service.
    pub inner: S,
}

impl<S> Layer<S> for TransportMuxLayer {
    type Service = TransportMuxService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TransportMuxService {
            state: Arc::clone(&self.state),
            inner,
        }
    }
}

/// `tower::Layer` wrapping an inner service with `GatewayService` (`Layer 2`).
#[derive(Clone, Debug)]
pub struct GatewayLayer {
    /// Shared application state.
    pub state: Arc<AppState>,
}

/// `tower::Service` implementing `Gateway` (`Layer 2`).
#[derive(Clone, Debug)]
pub struct GatewayService<S> {
    /// Shared application state.
    pub state: Arc<AppState>,
    /// Inner `Operation` service.
    pub inner: S,
}

impl<S> Layer<S> for GatewayLayer {
    type Service = GatewayService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GatewayService {
            state: Arc::clone(&self.state),
            inner,
        }
    }
}

/// `tower::Layer` wrapping an inner service with `OperationService` (`Layer 3`).
#[derive(Clone, Debug)]
pub struct OperationLayer {
    /// Shared application state.
    pub state: Arc<AppState>,
}

/// `tower::Service` implementing `Operation` (`Layer 3`).
#[derive(Clone, Debug)]
pub struct OperationService<S> {
    /// Shared application state.
    pub state: Arc<AppState>,
    /// Inner `Authz` service.
    pub inner: S,
}

impl<S> Layer<S> for OperationLayer {
    type Service = OperationService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        OperationService {
            state: Arc::clone(&self.state),
            inner,
        }
    }
}

/// `tower::Layer` wrapping an inner service with `AuthzService` (`Layer 4`).
#[derive(Clone, Debug)]
pub struct AuthzLayer {
    /// Shared application state.
    pub state: Arc<AppState>,
}

/// `tower::Service` implementing `Authz` (`Layer 4` stub: allow in `Dev`, deny in `Prod`).
#[derive(Clone, Debug)]
pub struct AuthzService<S> {
    /// Shared application state.
    pub state: Arc<AppState>,
    /// Inner `Plan` service.
    pub inner: S,
}

impl<S> Layer<S> for AuthzLayer {
    type Service = AuthzService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthzService {
            state: Arc::clone(&self.state),
            inner,
        }
    }
}

/// `tower::Layer` wrapping an inner service with `PlanService` (`Layer 5`).
#[derive(Clone, Debug)]
pub struct PlanLayer {
    /// Shared application state.
    pub state: Arc<AppState>,
}

/// `tower::Service` implementing `Plan` (`Layer 5`).
#[derive(Clone, Debug)]
pub struct PlanService<S> {
    /// Shared application state.
    pub state: Arc<AppState>,
    /// Inner `Exec` service.
    pub inner: S,
}

impl<S> Layer<S> for PlanLayer {
    type Service = PlanService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PlanService {
            state: Arc::clone(&self.state),
            inner,
        }
    }
}

/// `tower::Service` implementing `Exec` + `Assemble` (`Layer 6`).
#[derive(Clone, Debug)]
pub struct TransportExecService {
    /// Shared application state.
    pub state: Arc<AppState>,
}

async fn execute_and_assemble(state: Arc<AppState>, req: ExecStageRequest) -> AssembledPayload {
    let exec_span = tracing::info_span!("gap.exec", gap.generation = req.snapshot.generation);
    let exec_response = async {
        let mut exec_layer =
            ExecLayer::new(Arc::clone(&state.registry)).with_clock(Arc::clone(&state.clock));
        let exec_req = ExecRequest {
            snapshot: Arc::clone(&req.snapshot),
            plan: Arc::clone(&req.cached_plan.plan),
            variables: req.variables,
            deadline: req.deadline,
            cancel: req.cancel,
            limits: state.config.exec,
        };
        match exec_layer.call(exec_req).await {
            Ok(resp) => resp,
            Err(never) => match never {},
        }
    }
    .instrument(exec_span)
    .await;

    let assemble_span = tracing::info_span!(
        "gap.assemble",
        gap.bytes = tracing::field::Empty,
        gap.partial = tracing::field::Empty,
    );
    let body_bytes = assemble_span.in_scope(|| {
        let bytes = serialize_exec_response_envelope(
            &exec_response,
            &req.request_id,
            req.include_tokens_estimate,
        );
        assemble_span.record("gap.bytes", bytes.len() as u64);
        assemble_span.record("gap.partial", exec_response.extensions.partial);
        bytes
    });

    AssembledPayload {
        exec_response,
        body_bytes,
    }
}

impl Service<ExecStageRequest> for TransportExecService {
    type Response = PipelineOutput;
    type Error = PipelineError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ExecStageRequest) -> Self::Future {
        let state = Arc::clone(&self.state);
        if !req.sse_mode {
            return Box::pin(async move {
                let assembled = execute_and_assemble(state, req).await;
                Ok(PipelineOutput::Assembled(assembled))
            });
        }

        Box::pin(async move {
            let cap = state.config.sse_channel_capacity.max(1);
            let (tx, rx) = mpsc::channel::<Bytes>(cap);
            let producer_guard = state.metrics.track_sse_producer();
            let stream_cancel = req.cancel.clone();
            let producer_cancel = req.cancel.clone();
            let parent_span = tracing::Span::current();

            tokio::spawn(
                async move {
                    let _guard = producer_guard;
                    let assembled = execute_and_assemble(state, req).await;
                    if producer_cancel.is_cancelled() {
                        return;
                    }
                    let next_frame = format_sse_next(&assembled.body_bytes);
                    send_next_and_complete(tx, next_frame, producer_cancel).await;
                }
                .instrument(parent_span),
            );

            Ok(PipelineOutput::Sse(SseStreamHandle {
                rx,
                cancel: stream_cancel,
            }))
        })
    }
}

impl<S> Service<PlanStageRequest> for PlanService<S>
where
    S: Service<ExecStageRequest, Error = PipelineError> + Clone + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = PipelineError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: PlanStageRequest) -> Self::Future {
        let state = Arc::clone(&self.state);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let authz = req.inner;
            let plan_span = tracing::info_span!(
                "gap.plan",
                gap.plan_cache = tracing::field::Empty,
                gap.plan_hash = tracing::field::Empty,
            );

            let cached_plan = plan_span.in_scope(|| {
                let doc_hash = persisted_id_bytes(&authz.operation_id);
                let plan_cfg = PlanConfig {
                    limits: state.config.limits.clone(),
                    cost_model: CostModelConfig::default(),
                    request_budget_cost_units: authz.client_cost_budget,
                };
                let res = state.plan_cache.get_or_plan(
                    &authz.snapshot,
                    doc_hash,
                    &authz.validated,
                    &plan_cfg,
                );
                if let Ok(ref cp) = res {
                    state.metrics.record_plan_cache(cp.status);
                    let ph = plan_hash(&cp.plan);
                    plan_span.record("gap.plan_cache", cp.status.as_str());
                    plan_span.record("gap.plan_hash", ph.as_str());
                    authz
                        .request_span
                        .record("gap.cost.planned", cp.request_cost.planned_cost);
                }
                res.map_err(|err| PipelineError::GraphqlRequest {
                    status: graphql_request_error_status(&err),
                    errors: vec![err],
                })
            })?;

            let variables = authz.http_req.variables.unwrap_or_default();
            let exec_req = ExecStageRequest {
                request_id: authz.request_id,
                snapshot: authz.snapshot,
                cached_plan,
                variables,
                include_tokens_estimate: authz.include_tokens_estimate,
                sse_mode: authz.media == NegotiatedMedia::EventStream,
                deadline: authz.deadline,
                cancel: authz.cancel,
            };

            inner.call(exec_req).await
        })
    }
}

impl<S> Service<AuthzRequest> for AuthzService<S>
where
    S: Service<PlanStageRequest, Error = PipelineError> + Clone + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = PipelineError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: AuthzRequest) -> Self::Future {
        let profile = self.state.config.profile;
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let authz_span = tracing::info_span!(
                "gap.authz",
                gap.policy_version = req.snapshot.policy_version,
                gap.decision_count = 1_u64,
                gap.denied_count = tracing::field::Empty,
            );

            authz_span.in_scope(|| match profile {
                Profile::Dev => {
                    authz_span.record("gap.denied_count", 0_u64);
                    Ok(())
                }
                Profile::Prod => {
                    authz_span.record("gap.denied_count", 1_u64);
                    Err(PipelineError::Transport {
                        status: StatusCode::FORBIDDEN,
                        allow: None,
                        error: GapError::new(
                            ErrorCode::Forbidden,
                            "authorization denied in prod profile (WP-M6 Cedar policy stub)",
                        ),
                    })
                }
            })?;

            inner.call(PlanStageRequest { inner: req }).await
        })
    }
}

impl<S> Service<OperationRequest> for OperationService<S>
where
    S: Service<AuthzRequest, Error = PipelineError> + Clone + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = PipelineError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: OperationRequest) -> Self::Future {
        let state = Arc::clone(&self.state);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let validate_span = tracing::info_span!(
                "gap.operation.validate",
                gap.persisted = tracing::field::Empty,
                gap.depth = tracing::field::Empty,
                gap.query_hash = tracing::field::Empty,
            );

            let (http_req, prepared) = validate_span.in_scope(|| {
                let parsed_http = if req.is_get {
                    parse_get_query_params(
                        req.raw_uri_query.as_deref(),
                        state.config.limits.max_request_body_bytes,
                        state.config.max_json_depth,
                    )?
                } else {
                    parse_post_json_body(&req.body_bytes, state.config.max_json_depth)?
                };

                let prep = crate::operation::prepare_http_operation(
                    &req.snapshot,
                    &parsed_http,
                    &state.config,
                    state.persisted_store.as_ref(),
                )?;

                validate_span.record("gap.persisted", prep.is_registered);
                validate_span.record("gap.depth", prep.metrics.selection_depth);
                validate_span.record("gap.query_hash", prep.query_blake3_hex.as_str());

                let op_label = prep
                    .validated
                    .operation_name
                    .as_deref()
                    .unwrap_or("<anonymous>");
                req.request_span.record("gap.operation_name", op_label);

                Ok::<_, PipelineError>((parsed_http, prep))
            })?;

            let deadline = compute_request_deadline(
                Instant::now(),
                prepared.gap_ext.client_deadline_ms,
                state.config.default_deadline,
                state.config.max_deadline,
            );

            let authz_req = AuthzRequest {
                request_id: req.request_id,
                trace: req.trace,
                media: req.media,
                request_span: req.request_span,
                principal: req.principal,
                snapshot: req.snapshot,
                http_req,
                operation_id: prepared.operation_id,
                is_registered: prepared.is_registered,
                validated: prepared.validated,
                directives: prepared.directives,
                client_cost_budget: prepared.gap_ext.client_cost_budget,
                include_tokens_estimate: prepared.gap_ext.include_tokens_estimate,
                deadline,
                cancel: req.cancel,
            };

            inner.call(authz_req).await
        })
    }
}

impl<S> Service<GatewayRequest> for GatewayService<S>
where
    S: Service<OperationRequest, Error = PipelineError> + Clone + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = PipelineError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: GatewayRequest) -> Self::Future {
        let state = Arc::clone(&self.state);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let authn_span = tracing::info_span!(
                "gap.gateway.authn",
                gap.principal_kind = tracing::field::Empty,
            );
            let principal = authn_span.in_scope(|| {
                let p = authenticate_headers(
                    req.request.headers(),
                    &state.config,
                    state.api_key_verifier.as_ref(),
                )
                .map_err(|error| PipelineError::Transport {
                    status: StatusCode::UNAUTHORIZED,
                    allow: None,
                    error,
                })?;
                authn_span.record("gap.principal_kind", p.kind.as_str());
                Ok::<_, PipelineError>(p)
            })?;

            state
                .admission
                .check(&principal, req.request.headers())
                .map_err(|error| {
                    let status = match error.code() {
                        ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
                        ErrorCode::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
                        _ => StatusCode::FORBIDDEN,
                    };
                    PipelineError::Transport {
                        status,
                        allow: None,
                        error,
                    }
                })?;

            // INV-01: Pin the supergraph snapshot ONCE per request right after authentication.
            let snapshot = state.snapshot.pin();
            req.request_span
                .record("gap.generation", snapshot.generation);
            req.request_span
                .record("gap.tenant", u64::from(tenant_bucket(&principal.tenant)));
            req.request_span
                .record("gap.principal_kind", principal.kind.as_str());

            let method = req.request.method().clone();
            let (is_get, raw_uri_query, body_bytes) = if method == Method::GET {
                let qs = req.request.uri().query().map(str::to_owned);
                (true, qs, Bytes::new())
            } else {
                check_post_content_type(req.request.headers()).map_err(|error| {
                    PipelineError::Transport {
                        status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        allow: None,
                        error,
                    }
                })?;
                // A declared `Content-Length` above the cap is refused before a single body byte is read, so an
                // `Expect: 100-continue` client is never invited to upload a body that would be rejected anyway.
                let max_body = state.config.limits.max_request_body_bytes;
                let declared_len = req
                    .request
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                if declared_len.is_some_and(|len| len > max_body) {
                    return Err(PipelineError::Transport {
                        status: StatusCode::PAYLOAD_TOO_LARGE,
                        allow: None,
                        error: GapError::new(
                            ErrorCode::ComplexityExceeded,
                            format!(
                                "request body exceeds maximum allowed size of {max_body} bytes"
                            ),
                        ),
                    });
                }
                let (_parts, body) = req.request.into_parts();
                let body_timeout = state.config.default_deadline.min(state.config.max_deadline);
                let bytes = tokio::time::timeout(
                    body_timeout,
                    read_body_bounded(body, state.config.limits.max_request_body_bytes),
                )
                .await
                .map_err(|_| PipelineError::Transport {
                    status: StatusCode::REQUEST_TIMEOUT,
                    allow: None,
                    error: GapError::new(
                        ErrorCode::DeadlineExceeded,
                        "timed out reading HTTP request body",
                    ),
                })?
                .map_err(|(code, error)| {
                    if code == 413 {
                        PipelineError::Transport {
                            status: StatusCode::PAYLOAD_TOO_LARGE,
                            allow: None,
                            error,
                        }
                    } else {
                        PipelineError::GraphqlRequest {
                            status: StatusCode::BAD_REQUEST,
                            errors: vec![error],
                        }
                    }
                })?;
                (false, None, bytes)
            };

            let op_req = OperationRequest {
                request_id: req.request_id,
                trace: req.trace,
                media: req.media,
                request_span: req.request_span,
                principal,
                snapshot,
                is_get,
                raw_uri_query,
                body_bytes,
                cancel: req.cancel,
            };

            inner.call(op_req).await
        })
    }
}

impl<S> Service<TransportMuxRequest> for TransportMuxService<S>
where
    S: Service<GatewayRequest, Response = PipelineOutput, Error = PipelineError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: TransportMuxRequest) -> Self::Future {
        let state = Arc::clone(&self.state);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let (request_id, trace) = req
                .request
                .extensions()
                .get::<RequestCorrelation>()
                .cloned()
                .map_or_else(
                    || {
                        (
                            resolve_request_id(req.request.headers()),
                            resolve_trace_context(req.request.headers()),
                        )
                    },
                    |c| (c.request_id, c.trace),
                );

            let request_span = tracing::info_span!(
                "gap.request",
                gap.generation = tracing::field::Empty,
                gap.operation_name = tracing::field::Empty,
                gap.tenant = tracing::field::Empty,
                gap.principal_kind = tracing::field::Empty,
                gap.cost.planned = tracing::field::Empty,
                request_id = %request_id,
                trace_id = %trace.trace_id_hex(),
            );

            let inner_span = request_span.clone();
            let resp = async move {
                let method = req.request.method();
                match req.route {
                    RouteLabel::GraphqlStream if method != Method::POST => {
                        return build_transport_error_response(
                            StatusCode::METHOD_NOT_ALLOWED,
                            Some("POST"),
                            GapError::new(
                                ErrorCode::OperationNotAllowed,
                                format!("HTTP method `{method}` is not allowed; use POST"),
                            ),
                            &request_id,
                        );
                    }
                    _ if method != Method::GET && method != Method::POST => {
                        return build_transport_error_response(
                            StatusCode::METHOD_NOT_ALLOWED,
                            Some("GET, POST"),
                            GapError::new(
                                ErrorCode::OperationNotAllowed,
                                format!("HTTP method `{method}` is not allowed; use GET or POST"),
                            ),
                            &request_id,
                        );
                    }
                    _ => {}
                }

                let media = match negotiate_accept(req.request.headers(), req.route) {
                    Ok(m) => m,
                    Err(error) => {
                        return build_transport_error_response(
                            StatusCode::NOT_ACCEPTABLE,
                            None,
                            error,
                            &request_id,
                        );
                    }
                };

                let cancel = CancellationToken::new();
                // Cancel downstream connector calls automatically if the caller drops the HTTP
                // response future before headers are returned (`RUN-031`).
                let outer_drop_guard = cancel.clone().drop_guard();

                let gw_req = GatewayRequest {
                    route: req.route,
                    request_id: request_id.clone(),
                    trace,
                    media,
                    request_span: inner_span.clone(),
                    cancel: cancel.clone(),
                    request: req.request,
                };

                match inner.call(gw_req).await {
                    Ok(PipelineOutput::Assembled(assembled)) => {
                        let _ = outer_drop_guard.disarm();
                        let mut http_resp = Response::new(Body::from(assembled.body_bytes));
                        *http_resp.status_mut() = StatusCode::OK;
                        http_resp.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static(media.success_content_type()),
                        );
                        http_resp
                    }
                    Ok(PipelineOutput::Sse(sse_handle)) => {
                        let http_resp = build_sse_response(sse_handle);
                        let _ = outer_drop_guard.disarm();
                        http_resp
                    }
                    Err(PipelineError::Transport {
                        status,
                        allow,
                        error,
                    }) => {
                        let _ = outer_drop_guard.disarm();
                        build_transport_error_response(status, allow, error, &request_id)
                    }
                    Err(PipelineError::GraphqlRequest { status, errors }) => {
                        if media == NegotiatedMedia::EventStream {
                            let http_resp = sse_request_error_response(
                                errors,
                                &request_id,
                                state.config.sse_channel_capacity,
                                &state.metrics,
                                cancel,
                            );
                            let _ = outer_drop_guard.disarm();
                            http_resp
                        } else {
                            let _ = outer_drop_guard.disarm();
                            build_graphql_request_error_response(status, errors, &request_id)
                        }
                    }
                }
            }
            .instrument(request_span)
            .await;

            Ok(resp)
        })
    }
}

/// Assembles the full 6-layer `tower` GraphQL service stack:
/// `TransportMux -> Gateway -> Operation -> Authz -> Plan -> Exec` (`RUN-001`, `Brief §3.3`).
pub type GraphqlPipelineService = TransportMuxService<
    GatewayService<OperationService<AuthzService<PlanService<TransportExecService>>>>,
>;

/// Builds [`GraphqlPipelineService`] using [`tower::Layer`] composition.
#[must_use]
pub fn build_graphql_pipeline(state: Arc<AppState>) -> GraphqlPipelineService {
    let exec_svc = TransportExecService {
        state: Arc::clone(&state),
    };
    let plan_svc = PlanLayer {
        state: Arc::clone(&state),
    }
    .layer(exec_svc);
    let authz_svc = AuthzLayer {
        state: Arc::clone(&state),
    }
    .layer(plan_svc);
    let op_svc = OperationLayer {
        state: Arc::clone(&state),
    }
    .layer(authz_svc);
    let gw_svc = GatewayLayer {
        state: Arc::clone(&state),
    }
    .layer(op_svc);
    TransportMuxLayer { state }.layer(gw_svc)
}

#[cfg(test)]
mod tests {
    use gap_core::telemetry::{
        SPAN_ASSEMBLE, SPAN_AUTHZ, SPAN_EXEC, SPAN_GATEWAY_AUTHN, SPAN_OPERATION_VALIDATE,
        SPAN_PLAN, SPAN_REQUEST,
    };

    // spec: OPS-010
    #[test]
    fn pipeline_span_literals_match_normative_ops_010_constants() {
        assert_eq!("gap.request", SPAN_REQUEST);
        assert_eq!("gap.gateway.authn", SPAN_GATEWAY_AUTHN);
        assert_eq!("gap.operation.validate", SPAN_OPERATION_VALIDATE);
        assert_eq!("gap.authz", SPAN_AUTHZ);
        assert_eq!("gap.plan", SPAN_PLAN);
        assert_eq!("gap.exec", SPAN_EXEC);
        assert_eq!("gap.assemble", SPAN_ASSEMBLE);
    }
}
