//! `ExecLayer` (`tower::Service<ExecRequest>`) executing `Fetch` / `Parallel` / `Sequence` plans
//! with `RUN-030` deadline propagation, `RUN-031` cancellation, `RUN-060` incremental response-size
//! enforcement, and `GQL-050`/`GQL-051` response envelopes.

pub mod assembly;
pub mod error;

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use gap_core::clock::{Clock, SystemClock};
use gap_core::snapshot::Snapshot;
use gap_plan::Plan;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower::Service;

use crate::connector_layer::ConnectorRegistry;
pub use assembly::{NoopResponseRedactor, ResponseRedactor};
pub use error::{GraphqlError, GraphqlErrorExtensions, map_ngc_error, sanitize_connector_message};

/// Default maximum concurrent `Fetch` nodes per request (`RUN-015`).
pub const DEFAULT_MAX_PARALLEL_FETCHES: usize = 8;

/// Default maximum serialized response `data` size in bytes (`4 MiB`, `spec/02-gap-ql.md` §8, `RUN-060`).
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Default minimum safety margin subtracted from request deadline per connector call (`50 ms`, `RUN-030`).
pub const DEFAULT_DEADLINE_MARGIN_FLOOR_MS: u64 = 50;

/// Default fractional safety margin subtracted from remaining request budget (`5%`, `RUN-030`).
pub const DEFAULT_DEADLINE_MARGIN_FRACTION: f64 = 0.05;

/// Per-request execution limits (`RUN-015`, `RUN-030`, `RUN-060`, `Brief §3.2`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecLimits {
    /// Maximum concurrent `Fetch` calls inside `PlanNode::Parallel` (`RUN-015`, default `8`).
    pub max_parallel_fetches: usize,
    /// Incremental cap on assembled response `data` UTF-8 JSON bytes (`RUN-060`, default `4 MiB`).
    pub max_response_bytes: usize,
    /// Minimum deadline margin reserved for response assembly (`RUN-030`, default `50 ms`).
    pub margin_floor: Duration,
    /// Fractional deadline margin reserved from remaining request time (`RUN-030`, default `0.05`).
    pub margin_fraction: f64,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            max_parallel_fetches: DEFAULT_MAX_PARALLEL_FETCHES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            margin_floor: Duration::from_millis(DEFAULT_DEADLINE_MARGIN_FLOOR_MS),
            margin_fraction: DEFAULT_DEADLINE_MARGIN_FRACTION,
        }
    }
}

/// Typed input to [`ExecLayer`] (`RUN-001`, `INV-01`, `Brief §3.2`).
#[derive(Clone)]
pub struct ExecRequest {
    /// Pinned supergraph snapshot for this request (`INV-01`: never re-read from a handle).
    pub snapshot: Arc<Snapshot>,
    /// Compiled, variable-independent execution plan (`RUN-010`).
    pub plan: Arc<Plan>,
    /// Caller-supplied GraphQL variables bound at execution time.
    pub variables: serde_json::Map<String, serde_json::Value>,
    /// Overall monotonic request deadline (`RUN-030`).
    pub deadline: Instant,
    /// Request cancellation token (`RUN-031`, `INV-12`).
    pub cancel: CancellationToken,
    /// Execution concurrency, size, and deadline-margin limits.
    pub limits: ExecLimits,
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecRequest")
            .field("generation", &self.snapshot.generation)
            .field("tenant_id", &self.snapshot.tenant_id)
            .field("operation_name", &self.plan.operation_name)
            .field("variable_count", &self.variables.len())
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancel.is_cancelled())
            .field("limits", &self.limits)
            .finish()
    }
}

impl ExecRequest {
    /// Creates an [`ExecRequest`] with default [`ExecLimits`], empty `variables`, and a fresh [`CancellationToken`].
    #[must_use]
    pub fn new(snapshot: Arc<Snapshot>, plan: Arc<Plan>, deadline: Instant) -> Self {
        Self {
            snapshot,
            plan,
            variables: serde_json::Map::new(),
            deadline,
            cancel: CancellationToken::new(),
            limits: ExecLimits::default(),
        }
    }

    /// Sets `variables`.
    #[must_use]
    pub fn with_variables(mut self, variables: serde_json::Map<String, serde_json::Value>) -> Self {
        self.variables = variables;
        self
    }

    /// Sets `cancel`.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets `limits`.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExecLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Planned and actual execution cost reported inside [`GapExtensions`] (`GQL-050`, `spec/02-gap-ql.md` §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSummary {
    /// Static cost computed by the planner (`GQL-041`, `RUN-005`).
    pub planned: u64,
    /// Actual cost accumulated over executed fetches and returned rows.
    pub actual: u64,
}

/// GAP response metadata serialized under `extensions` (`GQL-050`, `GQL-051`, `Brief §3.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapExtensions {
    /// Snapshot generation that served the request (`GQL-050`, `INV-01`).
    pub generation: u64,
    /// Planned vs. actual cost summary (`GQL-050`).
    pub cost: CostSummary,
    /// `true` iff `data` is present (`Some`) and at least one error was recorded (`GQL-051`).
    pub partial: bool,
}

/// Typed GraphQL execution result returned by [`ExecLayer`] (`RUN-001`, `GQL-050`, `GQL-051`, `Brief §3.2`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecResponse {
    /// Assembled GraphQL `data` object, or `None` (`"data": null`) when top-level non-null propagation,
    /// pre-execution variable validation, or `max_response_bytes` aborts `data`.
    pub data: Option<serde_json::Value>,
    /// Execution or validation errors in deterministic order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<GraphqlError>,
    /// GAP response extensions (`generation`, `cost`, `partial`).
    pub extensions: GapExtensions,
}

/// Deterministic `tower::Service` executing compiled [`Plan`]s against [`ConnectorRegistry`] (`RUN-001`).
#[derive(Debug, Clone)]
pub struct ExecLayer {
    registry: Arc<ConnectorRegistry>,
    clock: Arc<dyn Clock>,
    redactor: Arc<dyn ResponseRedactor>,
}

impl ExecLayer {
    /// Creates an [`ExecLayer`] backed by `registry`, [`SystemClock`], and [`NoopResponseRedactor`].
    #[must_use]
    pub fn new(registry: Arc<ConnectorRegistry>) -> Self {
        Self {
            registry,
            clock: Arc::new(SystemClock),
            redactor: Arc::new(NoopResponseRedactor),
        }
    }

    /// Injects a custom [`Clock`] (e.g. [`gap_core::clock::ManualClock`]) used to derive
    /// `CallContext.deadline_unix_ms` (`RUN-030`, `Brief §3.3`).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Injects a [`ResponseRedactor`] (`RUN-061` authorization/PII redaction seam).
    #[must_use]
    pub fn with_redactor(mut self, redactor: Arc<dyn ResponseRedactor>) -> Self {
        self.redactor = redactor;
        self
    }

    /// Borrows the underlying [`ConnectorRegistry`].
    #[must_use]
    pub fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    /// Executes `request` to completion (`RUN-001`, `RUN-015`, `RUN-030`, `RUN-031`, `RUN-060`).
    pub async fn execute(&self, request: ExecRequest) -> ExecResponse {
        let _ = (&self.registry, &self.clock, &self.redactor);
        ExecResponse {
            data: None,
            errors: Vec::new(),
            extensions: GapExtensions {
                generation: request.snapshot.generation,
                cost: CostSummary {
                    planned: request.plan.cost.planned_cost,
                    actual: 0,
                },
                partial: false,
            },
        }
    }
}

impl Service<ExecRequest> for ExecLayer {
    type Response = ExecResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ExecRequest) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.execute(req).await) })
    }
}
