//! `axum` application state, outer telemetry/correlation layer, and HTTP router assembly
//! (`Brief §3.2`, `OPS-002`, `OPS-010`, `OPS-020`, `INV-02`, `INV-15`).

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Method, Request, Response, StatusCode};
use axum::routing::any;
use gap_core::clock::{Clock, SystemClock};
use gap_core::context::TraceCtx;
use gap_core::error::{ErrorCode, GapError};
use gap_exec::ConnectorRegistry;
use gap_plan::PlanCache;
use gap_ql::{EmptyPersistedStore, PersistedOperationStore};
use gap_snapshot::SnapshotHandle;
use tokio::time::Instant;
use tower::{Layer, Service};

use crate::config::{ApiKeyVerifier, Blake3ApiKeyVerifier, ConfigValidationError, TransportConfig};
use crate::gateway::{
    AdmissionPolicy, AllowAllAdmission, resolve_request_id, resolve_trace_context,
};
use crate::health::{healthz_handler, metrics_handler, readyz_handler};
use crate::metrics::{RouteLabel, TransportMetrics};
use crate::pipeline::{
    TransportMuxRequest, build_graphql_pipeline, build_transport_error_response,
};

/// Per-request correlation metadata injected by the outer router telemetry layer into
/// `Request::extensions`.
#[derive(Debug, Clone)]
pub struct RequestCorrelation {
    /// Validated or minted UUIDv7 correlation ID (`X-Request-Id`).
    pub request_id: String,
    /// W3C trace context (`OPS-010`).
    pub trace: TraceCtx,
}

/// Shared immutable/lock-free transport state passed to [`router`] (`Brief §3.2`, `INV-02`).
pub struct AppState {
    /// Wait-free `ArcSwap` supergraph snapshot handle (`INV-01`, `INV-02`).
    pub snapshot: Arc<SnapshotHandle>,
    /// Lock-free branch client and bulkhead registry (`INV-02`).
    pub registry: Arc<ConnectorRegistry>,
    /// Lock-free `moka` execution plan cache (`RUN-014`, `INV-02`).
    pub plan_cache: Arc<PlanCache>,
    /// Transport profile, limits, deadlines, API keys, and drain flag.
    pub config: TransportConfig,
    /// Lock-free Prometheus metrics registry (`OPS-020`).
    pub metrics: Arc<TransportMetrics>,
    /// Monotonic/wall clock source (`RUN-030`).
    pub clock: Arc<dyn Clock>,
    /// Registered persisted-operation store (`GQL-061`).
    pub persisted_store: Arc<dyn PersistedOperationStore>,
    /// Pluggable API-key verifier (`SEC-001`).
    pub api_key_verifier: Arc<dyn ApiKeyVerifier>,
    /// Pluggable transport admission policy (`429`/`503` seam for `WP-M3-07`).
    pub admission: Arc<dyn AdmissionPolicy>,
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("snapshot_generation", &self.snapshot.generation())
            .field("registered_branches", &self.registry.len())
            .field("plan_cache_entries", &self.plan_cache.entry_count())
            .field("config", &self.config)
            .field("metrics", &self.metrics)
            .field("api_key_verifier", &self.api_key_verifier)
            .field("admission", &self.admission)
            .finish()
    }
}

impl AppState {
    /// Creates an [`AppState`] with default [`SystemClock`], [`EmptyPersistedStore`],
    /// [`Blake3ApiKeyVerifier`], and [`AllowAllAdmission`].
    #[must_use]
    pub fn new(
        snapshot: Arc<SnapshotHandle>,
        registry: Arc<ConnectorRegistry>,
        plan_cache: Arc<PlanCache>,
        config: TransportConfig,
        metrics: Arc<TransportMetrics>,
    ) -> Self {
        Self {
            snapshot,
            registry,
            plan_cache,
            config,
            metrics,
            clock: Arc::new(SystemClock),
            persisted_store: Arc::new(EmptyPersistedStore),
            api_key_verifier: Arc::new(Blake3ApiKeyVerifier),
            admission: Arc::new(AllowAllAdmission),
        }
    }

    /// Creates an [`AppState`] after validating `config` (`Brief §3.1`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigValidationError`] if `config.validate()` fails.
    pub fn try_new(
        snapshot: Arc<SnapshotHandle>,
        registry: Arc<ConnectorRegistry>,
        plan_cache: Arc<PlanCache>,
        config: TransportConfig,
        metrics: Arc<TransportMetrics>,
    ) -> Result<Self, ConfigValidationError> {
        let validated_cfg = config.validated()?;
        Ok(Self::new(
            snapshot,
            registry,
            plan_cache,
            validated_cfg,
            metrics,
        ))
    }

    /// Injects a custom [`Clock`] (e.g. `ManualClock` in deterministic paused-time tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Injects a [`PersistedOperationStore`] for `GET /graphql` and `hybrid`/`safelist` modes.
    #[must_use]
    pub fn with_persisted_store(mut self, store: Arc<dyn PersistedOperationStore>) -> Self {
        self.persisted_store = store;
        self
    }

    /// Injects a custom [`ApiKeyVerifier`].
    #[must_use]
    pub fn with_api_key_verifier(mut self, verifier: Arc<dyn ApiKeyVerifier>) -> Self {
        self.api_key_verifier = verifier;
        self
    }

    /// Injects a custom [`AdmissionPolicy`] (`429` / `503` transport refusal seam).
    #[must_use]
    pub fn with_admission_policy(mut self, admission: Arc<dyn AdmissionPolicy>) -> Self {
        self.admission = admission;
        self
    }
}

#[derive(Clone, Debug)]
struct RouterTelemetryLayer {
    state: Arc<AppState>,
}

#[derive(Clone, Debug)]
struct RouterTelemetryService<S> {
    state: Arc<AppState>,
    inner: S,
}

impl<S> Layer<S> for RouterTelemetryLayer {
    type Service = RouterTelemetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RouterTelemetryService {
            state: Arc::clone(&self.state),
            inner,
        }
    }
}

impl<S> Service<Request<Body>> for RouterTelemetryService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let state = Arc::clone(&self.state);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let route = RouteLabel::from_path(req.uri().path());
            let in_flight_guard = state.metrics.track_in_flight();
            let request_id = resolve_request_id(req.headers());
            let trace = resolve_trace_context(req.headers());
            let traceparent_hdr = trace.traceparent();

            req.extensions_mut().insert(RequestCorrelation {
                request_id: request_id.clone(),
                trace,
            });

            let started = Instant::now();
            let mut resp = match inner.call(req).await {
                Ok(r) => r,
                Err(never) => match never {},
            };

            let headers = resp.headers_mut();
            if let Ok(hv) = HeaderValue::from_str(&request_id) {
                headers.insert("x-request-id", hv);
            }
            if let Ok(tp) = HeaderValue::from_str(&traceparent_hdr) {
                headers.insert("traceparent", tp);
            }

            state
                .metrics
                .observe_request(route, resp.status().as_u16(), started.elapsed());
            drop(in_flight_guard);

            Ok(resp)
        })
    }
}

fn correlation_id_from_req(req: &Request<Body>) -> String {
    req.extensions().get::<RequestCorrelation>().map_or_else(
        || resolve_request_id(req.headers()),
        |c| c.request_id.clone(),
    )
}

fn method_not_allowed_get_only(req: &Request<Body>) -> Response<Body> {
    let request_id = correlation_id_from_req(req);
    build_transport_error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        Some("GET"),
        GapError::new(
            ErrorCode::OperationNotAllowed,
            format!("HTTP method `{}` is not allowed; use GET", req.method()),
        ),
        &request_id,
    )
}

async fn graphql_endpoint(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    let mut svc = build_graphql_pipeline(state);
    match svc
        .call(TransportMuxRequest {
            route: RouteLabel::Graphql,
            request: req,
        })
        .await
    {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

async fn graphql_stream_endpoint(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    let mut svc = build_graphql_pipeline(state);
    match svc
        .call(TransportMuxRequest {
            route: RouteLabel::GraphqlStream,
            request: req,
        })
        .await
    {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

async fn guarded_healthz(state: State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    if req.method() != Method::GET {
        return method_not_allowed_get_only(&req);
    }
    healthz_handler(state).await
}

async fn guarded_readyz(state: State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    if req.method() != Method::GET {
        return method_not_allowed_get_only(&req);
    }
    readyz_handler(state).await
}

async fn guarded_metrics(state: State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    if req.method() != Method::GET {
        return method_not_allowed_get_only(&req);
    }
    metrics_handler(state).await
}

async fn not_found_handler(req: Request<Body>) -> Response<Body> {
    let request_id = correlation_id_from_req(&req);
    build_transport_error_response(
        StatusCode::NOT_FOUND,
        None,
        GapError::new(ErrorCode::ValidationError, "route not found"),
        &request_id,
    )
}

/// Builds the `axum::Router` serving `/graphql`, `/graphql/stream`, `/healthz`, `/readyz`, and
/// `/metrics` (`Brief §3.2`, `OPS-002`).
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/graphql", any(graphql_endpoint))
        .route("/graphql/stream", any(graphql_stream_endpoint))
        .route("/healthz", any(guarded_healthz))
        .route("/readyz", any(guarded_readyz))
        .route("/metrics", any(guarded_metrics))
        .fallback(not_found_handler)
        .with_state(Arc::clone(&state))
        .layer(RouterTelemetryLayer { state })
}
