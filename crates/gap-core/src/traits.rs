//! Cross-layer traits (`ElementSearch`, `Embedder`, `DiscoveryService`, `GraphqlEngine`,
//! `HttpModule`) and RAII concurrency guards (`L0`, `ARC-007`, `ARC-010`, `ARC-011`, `ARC-012`).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::context::RequestContext;
use crate::error::{ErrorCode, GapError, GapResult};
use crate::ids::{BranchUid, OperationId, TenantScope};

/// Object-safe boxed future alias used by cross-layer `L0` traits (`ARC-007`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Kind of indexed schema element (`spec/00-conventions.md` §6, `spec/02-gap-ql.md`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ElementKind {
    /// Callable branch operation (`QUERY` / `MUTATION` / `SUBSCRIPTION` / `TASK`).
    Operation,
    /// GraphQL object, interface, union, enum, input, or scalar type.
    Type,
    /// Field on a GraphQL type.
    Field,
    /// Code/doc/context graph node category.
    NodeCategory,
    /// Positive or negative intent example (`NGC-016`).
    Example,
}

/// Candidate returned by [`ElementSearch::search`] (`ADR-010`, `INV-05`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    /// Stable element identifier within the snapshot index.
    pub element_id: String,
    /// Kind of element matched.
    pub kind: ElementKind,
    /// Owning branch UID, if branch-scoped.
    pub branch_uid: Option<BranchUid>,
    /// Owning operation ID, if operation-scoped.
    pub operation_id: Option<OperationId>,
    /// BM25 lexical score.
    pub lexical_score: f32,
    /// Dense cosine/dot similarity score when embeddings are available.
    pub dense_score: Option<f32>,
    /// Reciprocal-rank / hybrid fused score.
    pub fused_score: f32,
}

/// Hybrid BM25 + dense element index lookup pinned inside `Snapshot` (`spec/01-architecture.md`
/// §§3–4, `ADR-010`, `INV-05`, `INV-17`).
pub trait ElementSearch: Send + Sync + fmt::Debug {
    /// Identifier of the embedding model pinned into this index, if dense vectors are present.
    fn embedding_model_id(&self) -> Option<&str>;

    /// Total number of indexed elements in this snapshot.
    fn element_count(&self) -> usize;

    /// Searches elements within `tenant` (`INV-09`, `INV-19`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if query execution or vector dimension validation fails.
    fn search(
        &self,
        tenant: &TenantScope,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> GapResult<Vec<SearchHit>>;
}

/// Zero-element [`ElementSearch`] implementation used by empty/bootstrap snapshots.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyElementSearch;

impl ElementSearch for EmptyElementSearch {
    fn embedding_model_id(&self) -> Option<&str> {
        None
    }

    fn element_count(&self) -> usize {
        0
    }

    fn search(
        &self,
        _tenant: &TenantScope,
        _query: &str,
        _query_embedding: Option<&[f32]>,
        _limit: usize,
    ) -> GapResult<Vec<SearchHit>> {
        Ok(Vec::new())
    }
}

/// Async text embedding provider injected into snapshot builds and discovery (`ARC-007`).
pub trait Embedder: Send + Sync + fmt::Debug {
    /// Stable model identifier pinned into `ElementIndex` (`ADR-010`).
    fn model_id(&self) -> &str;

    /// Output vector dimensionality.
    fn dimension(&self) -> usize;

    /// Computes embeddings for a batch of input strings off the request I/O path (`ARC-010`).
    fn embed<'a>(&'a self, texts: &'a [&'a str]) -> BoxFuture<'a, GapResult<Vec<Vec<f32>>>>;
}

/// High-level outcome of calibrated discovery (`spec/01-architecture.md` §5.2, `GQL-071`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiscoveryOutcome {
    /// Confident candidate set resolved.
    Resolved,
    /// Multiple distinct capability groups remain ambiguous (`GAP_AMBIGUOUS`).
    Ambiguous,
    /// Calibrated router abstained (`GAP_INSUFFICIENT_EVIDENCE`).
    Abstained,
    /// S1 unavailable; baseline retrieval ranking returned (`GAP_DISCOVERY_DEGRADED`).
    Degraded,
}

/// Input to [`DiscoveryService::resolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRequest {
    /// Natural-language or structured caller intent.
    pub intent: String,
    /// Maximum candidates requested (`top_k`).
    pub top_k: usize,
    /// Optional scope filter narrowing visible elements.
    pub scopes: Vec<String>,
}

/// Visible element surfaced by [`DiscoveryService::resolve`] (`INV-05`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryCandidate {
    /// Element identifier.
    pub element_id: String,
    /// Element category.
    pub kind: ElementKind,
    /// Calibrated relevance score.
    pub score: f32,
    /// Sanitized progressive-disclosure synopsis.
    pub synopsis: String,
    /// Optional SDL fragment when progressive disclosure expands detail.
    pub sdl: Option<String>,
}

/// Advisory resolution returned by [`DiscoveryService::resolve`] (`spec/01-architecture.md` §5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryResolution {
    /// Calibrated decision outcome.
    pub outcome: DiscoveryOutcome,
    /// Visible ranked candidates (`INV-05`).
    pub candidates: Vec<DiscoveryCandidate>,
    /// Estimated token cost of rendered candidates (`INV-05`).
    pub tokens_estimate: u32,
    /// Router identifier (`"baseline"` or S1 backend name).
    pub router: String,
}

/// Advisory discovery service (`__metaGraph`, `__search`, `gap.resolve`; `ARC-007`, `INV-17`).
pub trait DiscoveryService: Send + Sync + fmt::Debug {
    /// Resolves caller intent against the request's pinned snapshot without executing operations.
    fn resolve<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: DiscoveryRequest,
    ) -> BoxFuture<'a, GapResult<DiscoveryResolution>>;
}

/// Transport-neutral GraphQL execution request (`spec/01-architecture.md` §3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphqlRequest {
    /// GraphQL query document (optional when persisted-operation hash is supplied in `extensions`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Selected operation name in multi-operation documents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    /// Coerced GraphQL variable map.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub variables: serde_json::Map<String, serde_json::Value>,
    /// Protocol extensions (`extensions.gap`, persistedQuery, etc.).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// Transport-neutral GraphQL execution response (`spec/01-architecture.md` §5.1, `ADR-023`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GraphqlResponse {
    /// GraphQL `data` payload (`None` when omitted or null before execution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Operation/validation errors (`ADR-023`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<GapError>,
    /// Response extensions (`extensions.gap.{generation,cost,partial}`).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// Deterministic GAP-QL execution engine trait (`spec/01-architecture.md` §3, `INV-03`).
pub trait GraphqlEngine: Send + Sync + fmt::Debug {
    /// Executes a GAP-QL document under the pinned `ctx.snapshot` (`INV-01`, `INV-03`).
    fn execute<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: GraphqlRequest,
    ) -> BoxFuture<'a, GraphqlResponse>;
}

/// Lightweight HTTP request envelope passed to mounted [`HttpModule`] facades (`ARC-007`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpExchangeRequest {
    /// HTTP method (`GET`, `POST`, `DELETE`, ...).
    pub method: String,
    /// Request path (`/mcp`, `/.well-known/agent-card.json`, ...).
    pub path: String,
    /// Raw query string without leading `?`, if present.
    pub query: Option<String>,
    /// Lowercase header name/value pairs (client bearer tokens stripped per `SEC-011` where applicable).
    pub headers: Vec<(String, String)>,
    /// Raw request body bytes.
    pub body: Vec<u8>,
}

/// Lightweight HTTP response envelope returned by [`HttpModule`] facades (`ARC-007`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpExchangeResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response header name/value pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Inbound protocol facade mounted by `gap-server` (`gap-mcp`, `gap-a2a`; `ARC-007`).
pub trait HttpModule: Send + Sync + fmt::Debug {
    /// Stable module name for diagnostics and metrics.
    fn module_name(&self) -> &'static str;

    /// URL path prefixes served by this module (e.g. `&["/mcp"]`).
    fn mount_prefixes(&self) -> &'static [&'static str];

    /// Handles an inbound HTTP exchange under `ctx`.
    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: HttpExchangeRequest,
    ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>>;
}

/// Lock-free bounded concurrency & blocking-pool tracker (`ARC-010`, `ARC-011`, `ARC-012`, `INV-02`).
///
/// Tracks queue/in-flight depth (`gap_blocking_queue_depth`), sheds excess load with
/// [`ErrorCode::Overloaded`], and releases slots via the RAII [`InFlightGuard`] even when a future
/// is cancelled (`ARC-012`).
#[derive(Debug)]
pub struct BoundedPoolTracker {
    pool_name: &'static str,
    max_in_flight: usize,
    in_flight: AtomicUsize,
    rejected_total: AtomicU64,
}

/// Cancellation-safe RAII permit guard returned by [`BoundedPoolTracker::try_acquire`] (`ARC-012`).
#[derive(Debug)]
pub struct InFlightGuard {
    tracker: Arc<BoundedPoolTracker>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.tracker.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl BoundedPoolTracker {
    /// Creates a new lock-free [`BoundedPoolTracker`] for `pool_name` with capacity `max_in_flight`.
    #[must_use]
    pub fn new(pool_name: &'static str, max_in_flight: usize) -> Arc<Self> {
        Arc::new(Self {
            pool_name,
            max_in_flight,
            in_flight: AtomicUsize::new(0),
            rejected_total: AtomicU64::new(0),
        })
    }

    /// Pool label exported on `gap_blocking_queue_depth{pool=...}` (`OPS-020`).
    #[must_use]
    pub const fn pool_name(&self) -> &'static str {
        self.pool_name
    }

    /// Configured maximum concurrent slots.
    #[must_use]
    pub const fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// Current occupied slot count (`gap_blocking_queue_depth`).
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Total load-shed rejections since startup.
    #[must_use]
    pub fn rejected_total(&self) -> u64 {
        self.rejected_total.load(Ordering::Acquire)
    }

    /// Attempts to acquire a slot without blocking; sheds overflow with `GAP_OVERLOADED`
    /// (`ARC-010`, `ARC-011`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::Overloaded`] (`retryable: true`) if `in_flight >= max_in_flight`.
    pub fn try_acquire(self: &Arc<Self>) -> GapResult<InFlightGuard> {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current >= self.max_in_flight {
                self.rejected_total.fetch_add(1, Ordering::Relaxed);
                return Err(GapError::new(
                    ErrorCode::Overloaded,
                    format!(
                        "pool `{}` saturated ({current}/{})",
                        self.pool_name, self.max_in_flight
                    ),
                ));
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(InFlightGuard {
                        tracker: Arc::clone(self),
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct MockEmbedder;
    impl Embedder for MockEmbedder {
        fn model_id(&self) -> &str {
            "mock-e5-small"
        }
        fn dimension(&self) -> usize {
            4
        }
        fn embed<'a>(&'a self, texts: &'a [&'a str]) -> BoxFuture<'a, GapResult<Vec<Vec<f32>>>> {
            Box::pin(async move { Ok(texts.iter().map(|_| vec![0.25_f32; 4]).collect()) })
        }
    }

    #[derive(Debug)]
    struct MockDiscovery;
    impl DiscoveryService for MockDiscovery {
        fn resolve<'a>(
            &'a self,
            _ctx: &'a RequestContext,
            _req: DiscoveryRequest,
        ) -> BoxFuture<'a, GapResult<DiscoveryResolution>> {
            Box::pin(async move {
                Ok(DiscoveryResolution {
                    outcome: DiscoveryOutcome::Resolved,
                    candidates: Vec::new(),
                    tokens_estimate: 12,
                    router: "baseline".to_owned(),
                })
            })
        }
    }

    #[derive(Debug)]
    struct MockEngine;
    impl GraphqlEngine for MockEngine {
        fn execute<'a>(
            &'a self,
            _ctx: &'a RequestContext,
            _req: GraphqlRequest,
        ) -> BoxFuture<'a, GraphqlResponse> {
            Box::pin(async move { GraphqlResponse::default() })
        }
    }

    #[derive(Debug)]
    struct MockHttpModule;
    impl HttpModule for MockHttpModule {
        fn module_name(&self) -> &'static str {
            "mcp"
        }
        fn mount_prefixes(&self) -> &'static [&'static str] {
            &["/mcp"]
        }
        fn handle<'a>(
            &'a self,
            _ctx: &'a RequestContext,
            _req: HttpExchangeRequest,
        ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>> {
            Box::pin(async move {
                Ok(HttpExchangeResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"{}".to_vec(),
                })
            })
        }
    }

    // spec: ARC-007, INV-17
    #[test]
    fn shared_l0_traits_are_dyn_compatible_for_upward_injection() {
        let search: Arc<dyn ElementSearch> = Arc::new(EmptyElementSearch);
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
        let discovery: Arc<dyn DiscoveryService> = Arc::new(MockDiscovery);
        let engine: Arc<dyn GraphqlEngine> = Arc::new(MockEngine);
        let module: Arc<dyn HttpModule> = Arc::new(MockHttpModule);

        let scope = crate::ids::TenantId::default_tenant().scope();
        assert_eq!(search.element_count(), 0);
        assert!(
            search
                .search(&scope, "invoice", None, 5)
                .expect("empty search")
                .is_empty()
        );
        assert_eq!(embedder.dimension(), 4);
        assert_eq!(module.module_name(), "mcp");
        assert_eq!(module.mount_prefixes(), &["/mcp"]);
        let _ = (&discovery, &engine);
    }

    // spec: ARC-010, ARC-011, ARC-012, INV-02
    #[test]
    fn inv_02_bounded_pool_tracker_sheds_overload_and_releases_on_drop() {
        let pool = BoundedPoolTracker::new("ingest-parse", 2);
        assert_eq!(pool.pool_name(), "ingest-parse");
        assert_eq!(pool.max_in_flight(), 2);

        let g1 = pool.try_acquire().expect("slot 1");
        let g2 = pool.try_acquire().expect("slot 2");
        assert_eq!(pool.in_flight(), 2);

        let shed = pool.try_acquire().expect_err("slot 3 must shed load");
        assert_eq!(shed.code(), ErrorCode::Overloaded);
        assert!(shed.retry().retryable);
        assert_eq!(pool.rejected_total(), 1);

        drop(g1);
        assert_eq!(pool.in_flight(), 1);
        let g3 = pool
            .try_acquire()
            .expect("slot re-acquired after RAII drop");
        assert_eq!(pool.in_flight(), 2);

        drop(g2);
        drop(g3);
        assert_eq!(pool.in_flight(), 0);
    }
}
