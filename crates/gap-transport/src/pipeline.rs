//! Typed 6-layer `tower` request pipeline:
//! `TransportMux -> Gateway -> Operation -> Authz -> Plan -> Exec`
//! (`spec/01-architecture.md` §5.1, `RUN-001`, `RUN-002`, `OPS-010`, `OPS-011`, `INV-01`).

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use bytes::Bytes;
use gap_core::context::{Principal, TraceCtx};
use gap_core::error::GapError;
use gap_core::snapshot::Snapshot;
use gap_exec::ExecResponse;
use gap_plan::CachedPlan;
use gap_ql::{ExecutableDirectives, PersistedOperationId, ValidatedOperation};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};

use crate::app::AppState;
use crate::metrics::RouteLabel;
use crate::operation::{NegotiatedMedia, ParsedHttpGraphqlRequest};

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

impl Service<ExecStageRequest> for TransportExecService {
    type Response = AssembledPayload;
    type Error = PipelineError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: ExecStageRequest) -> Self::Future {
        Box::pin(async {
            Err(PipelineError::Transport {
                status: StatusCode::NOT_IMPLEMENTED,
                allow: None,
                error: GapError::new(gap_core::error::ErrorCode::Internal, "stub"),
            })
        })
    }
}

impl<S> Service<TransportMuxRequest> for TransportMuxService<S>
where
    S: Clone + Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: TransportMuxRequest) -> Self::Future {
        Box::pin(async {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NOT_IMPLEMENTED;
            Ok(resp)
        })
    }
}
