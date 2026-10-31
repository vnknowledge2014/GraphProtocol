//! `axum` application state and HTTP router assembly (`Brief §3.2`, `OPS-002`).

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use gap_core::clock::{Clock, SystemClock};
use gap_exec::ConnectorRegistry;
use gap_plan::PlanCache;
use gap_ql::{EmptyPersistedStore, PersistedOperationStore};
use gap_snapshot::SnapshotHandle;

use crate::config::{ApiKeyVerifier, Blake3ApiKeyVerifier, TransportConfig};
use crate::gateway::{AdmissionPolicy, AllowAllAdmission};
use crate::health::{healthz_handler, metrics_handler, readyz_handler};
use crate::metrics::TransportMetrics;

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

/// Builds the `axum::Router` serving `/graphql`, `/graphql/stream`, `/healthz`, `/readyz`, and
/// `/metrics` (`Brief §3.2`, `OPS-002`).
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}
