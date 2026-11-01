//! Inbound stateless MCP `2026-07-28` facade and `2025-11-25` compatibility session layer (`CMP-001`..`005`, `CMP-007`, `CMP-008`, `SEC-080`..`083`, `ARC-007`).
//!
//! # Architecture & Authentication Boundary (`CMP-006`, `SEC-004`, `ARC-007`)
//!
//! [`McpFacade`] implements [`gap_core::traits::HttpModule`] for Streamable HTTP (`POST /mcp`) and
//! pairs with [`StdioServer`] for newline-delimited JSON-RPC over `stdio`.
//!
//! Transport-level TLS termination, OAuth 2.1 / JWT bearer verification, `401 Unauthorized`
//! (`WWW-Authenticate: Bearer resource_metadata=...`) challenges, `403 insufficient_scope`
//! responses, and `/.well-known/oauth-protected-resource` endpoints (`CMP-006`, `SEC-004`) are the
//! responsibility of `gap-server` (`WP-M1-12` / `WP-M6-01`). `McpFacade` receives an already
//! authenticated, per-request [`RequestContext`] with a pinned [`Snapshot`](gap_core::snapshot::Snapshot)
//! (`INV-01`) and never inspects or forwards raw bearer tokens (`SEC-011`).

#![deny(missing_docs)]

pub(crate) mod discovery;
pub(crate) mod resources;
pub(crate) mod result;
pub(crate) mod session;
pub(crate) mod spotlight;
pub(crate) mod tools;
pub(crate) mod wire;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use gap_core::context::RequestContext;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::{BranchDescriptor, SideEffectLevel, TrustTier};
use gap_core::traits::{
    BoxFuture, GraphqlEngine, HttpExchangeRequest, HttpExchangeResponse, HttpModule,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncWrite};

use self::session::SessionStore;

/// Default maximum HTTP/stdio request payload size in bytes (`1 MiB`, `spec/02-gap-ql.md` §8).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Hard upper bound on `max_request_bytes` (`16 MiB`, `spec/02-gap-ql.md` §8).
pub const HARD_MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Default maximum concurrent legacy `2025-11-25` HTTP sessions (`CMP-001`).
pub const DEFAULT_MAX_SESSIONS: usize = 256;

/// Default idle TTL for legacy `2025-11-25` HTTP sessions (`30 minutes`, `CMP-001`).
pub const DEFAULT_SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// Default maximum branches listed in `gap_describe{overview:true}` (`GQL-023`).
pub const DEFAULT_METAGRAPH_BRANCHES: usize = 100;

/// Configuration validation failure returned by [`FacadeConfig::validate`] and [`McpFacadeBuilder::build`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FacadeConfigError {
    /// `max_request_bytes` was zero or exceeded [`HARD_MAX_REQUEST_BYTES`].
    #[error("max_request_bytes ({0}) must be in 1..={HARD_MAX_REQUEST_BYTES}")]
    InvalidMaxRequestBytes(usize),
    /// `max_sessions` was zero.
    #[error("max_sessions must be at least 1")]
    ZeroMaxSessions,
    /// `session_idle_ttl` was zero.
    #[error("session_idle_ttl must be non-zero")]
    ZeroSessionIdleTtl,
    /// `metagraph_branches` was zero.
    #[error("metagraph_branches must be at least 1")]
    ZeroMetagraphBranches,
    /// `server_name` was empty.
    #[error("server_name must not be empty")]
    EmptyServerName,
    /// `server_version` was empty.
    #[error("server_version must not be empty")]
    EmptyServerVersion,
}

/// Runtime configuration for [`McpFacade`] (`CMP-001`..`008`, `SEC-083`, `GQL-023`).
///
/// Holds no credentials or secrets; [`fmt::Debug`] prints all fields verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacadeConfig {
    /// Wrap root fields owned by `T2` branches in `<gap:untrusted>` tags in text output (`CMP-007`, `SEC-083`).
    pub spotlight: bool,
    /// Also wrap root fields owned by `T1` branches when `spotlight` is `true` (`CMP-007`).
    pub spotlight_t1: bool,
    /// Explicit allow-list of `Origin` header values. When empty, only loopback origins
    /// (`localhost`, `127.0.0.1`, `[::1]`) and requests without `Origin` are permitted (`CMP-001`).
    pub allowed_origins: Vec<String>,
    /// Maximum incoming JSON-RPC request frame size in bytes (default `1 MiB`).
    pub max_request_bytes: usize,
    /// Maximum active legacy `2025-11-25` sessions before oldest-first eviction (default `256`).
    pub max_sessions: usize,
    /// Idle timeout before a legacy `2025-11-25` session expires (default `30 min`).
    pub session_idle_ttl: Duration,
    /// Maximum branches emitted by `gap_describe{overview:true}` (`GQL-023`, default `100`).
    pub metagraph_branches: usize,
    /// Server name reported in `_meta["io.modelcontextprotocol/serverInfo"]` and `InitializeResult`.
    pub server_name: String,
    /// Server version reported in `_meta["io.modelcontextprotocol/serverInfo"]` and `InitializeResult`.
    pub server_version: String,
}

impl Default for FacadeConfig {
    fn default() -> Self {
        Self {
            spotlight: true,
            spotlight_t1: false,
            allowed_origins: Vec::new(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_sessions: DEFAULT_MAX_SESSIONS,
            session_idle_ttl: DEFAULT_SESSION_IDLE_TTL,
            metagraph_branches: DEFAULT_METAGRAPH_BRANCHES,
            server_name: "gap-mcp".to_owned(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

impl FacadeConfig {
    /// Validates configuration invariants before constructing a [`McpFacade`].
    ///
    /// # Errors
    ///
    /// Returns [`FacadeConfigError`] if any size, count, TTL, or server identity field is invalid.
    pub fn validate(&self) -> Result<(), FacadeConfigError> {
        if self.max_request_bytes == 0 || self.max_request_bytes > HARD_MAX_REQUEST_BYTES {
            return Err(FacadeConfigError::InvalidMaxRequestBytes(
                self.max_request_bytes,
            ));
        }
        if self.max_sessions == 0 {
            return Err(FacadeConfigError::ZeroMaxSessions);
        }
        if self.session_idle_ttl.is_zero() {
            return Err(FacadeConfigError::ZeroSessionIdleTtl);
        }
        if self.metagraph_branches == 0 {
            return Err(FacadeConfigError::ZeroMetagraphBranches);
        }
        if self.server_name.trim().is_empty() {
            return Err(FacadeConfigError::EmptyServerName);
        }
        if self.server_version.trim().is_empty() {
            return Err(FacadeConfigError::EmptyServerVersion);
        }
        Ok(())
    }
}

/// Summary of a registered persisted operation exposed by `gap_describe{operations:true}` and
/// `gap://operations` (`GQL-061`, `CMP-002`, `CMP-005`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationInfo {
    /// Stable persisted operation identifier (e.g. `sha256:...`).
    pub id: String,
    /// Human/agent-readable operation name.
    pub name: String,
    /// Sanitized operation summary.
    pub description: String,
    /// Declared side-effect classification.
    pub side_effect: SideEffectLevel,
}

/// Read-only view of registered persisted GAP-QL operations (`GQL-061`).
pub trait OperationCatalog: Send + Sync + fmt::Debug {
    /// Returns registered operations for `ctx`'s tenant before [`Visibility`] filtering.
    fn list(&self, ctx: &RequestContext) -> Vec<OperationInfo>;
}

/// Default empty [`OperationCatalog`] used when no catalogue is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyOperationCatalog;

impl OperationCatalog for EmptyOperationCatalog {
    fn list(&self, _ctx: &RequestContext) -> Vec<OperationInfo> {
        Vec::new()
    }
}

/// Approval decision supplied to `gap_task` when `action == "decide"` (`CMP-002`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskDecision {
    /// Approve the pending operation (`"approve"`).
    Approve,
    /// Deny the pending operation (`"deny"`).
    Deny,
}

/// Pluggable backend for `gap_task` and `gap://task/{id}` (`CMP-002`, `CMP-005`).
pub trait TaskBackend: Send + Sync + fmt::Debug {
    /// Fetches a task snapshot by `id`.
    fn get<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _id: &'a str,
    ) -> BoxFuture<'a, GapResult<serde_json::Value>> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "task API is not enabled",
            ))
        })
    }

    /// Lists tasks matching optional filter `input`.
    fn list<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _input: Option<&'a serde_json::Map<String, serde_json::Value>>,
    ) -> BoxFuture<'a, GapResult<serde_json::Value>> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "task API is not enabled",
            ))
        })
    }

    /// Requests cancellation of task `id`.
    fn cancel<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _id: &'a str,
    ) -> BoxFuture<'a, GapResult<serde_json::Value>> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "task API is not enabled",
            ))
        })
    }

    /// Records a human approval `decision` (with optional `note`) on task `id`.
    fn decide<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _id: &'a str,
        _decision: TaskDecision,
        _note: Option<&'a str>,
    ) -> BoxFuture<'a, GapResult<serde_json::Value>> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "task API is not enabled",
            ))
        })
    }

    /// Submits a new asynchronous task from `input`.
    fn submit<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _input: &'a serde_json::Map<String, serde_json::Value>,
    ) -> BoxFuture<'a, GapResult<serde_json::Value>> {
        Box::pin(async {
            Err(GapError::new(
                ErrorCode::UnsupportedFeature,
                "task API is not enabled",
            ))
        })
    }
}

/// Default [`TaskBackend`] returning `GAP_UNSUPPORTED_FEATURE` for every action.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedTaskBackend;

impl TaskBackend for UnsupportedTaskBackend {}

/// Borrowed schema or catalogue element inspected by [`Visibility`] (`GQL-062`, `INV-05`).
#[derive(Debug, Clone, Copy)]
pub enum ElementRef<'a> {
    /// Supergraph branch descriptor.
    Branch(&'a BranchDescriptor),
    /// Named GraphQL schema type and its owning branch, if branch-scoped.
    Type {
        /// Composed GraphQL type name.
        name: &'a str,
        /// Owning branch descriptor when the type belongs to a branch.
        branch: Option<&'a BranchDescriptor>,
    },
    /// Field on a GraphQL type and its owning branch, if branch-scoped.
    Field {
        /// Enclosing GraphQL type name.
        type_name: &'a str,
        /// Field name on `type_name`.
        field_name: &'a str,
        /// Owning branch descriptor when the field belongs to a branch.
        branch: Option<&'a BranchDescriptor>,
    },
    /// Registered persisted operation from [`OperationCatalog`].
    Operation(&'a OperationInfo),
}

/// Discovery and introspection visibility policy (`GQL-062`, `INV-05`).
///
/// Hidden elements behave as if they do not exist in `gap_describe`, `gap_search`, `resources/read`,
/// counts, and `didYouMean` suggestions.
pub trait Visibility: Send + Sync + fmt::Debug {
    /// Returns `true` iff `ctx.principal` may discover `element`.
    fn visible(&self, ctx: &RequestContext, element: ElementRef<'_>) -> bool;

    /// Returns `true` when visibility depends on the caller's principal (forcing `cacheScope: "private"`
    /// and `ttlMs: 0` on filtered resource responses per `CMP-005`).
    fn is_principal_scoped(&self) -> bool {
        true
    }
}

/// Permissive [`Visibility`] policy that exposes all elements (`dev` default).
#[derive(Debug, Default, Clone, Copy)]
pub struct AllVisible;

impl Visibility for AllVisible {
    fn visible(&self, _ctx: &RequestContext, _element: ElementRef<'_>) -> bool {
        true
    }

    fn is_principal_scoped(&self) -> bool {
        false
    }
}

/// Production [`Visibility`] policy hiding every element owned by a [`TrustTier::T2`] branch
/// (`OPS-001`, `GQL-062`, `INV-05`).
#[derive(Debug, Default, Clone, Copy)]
pub struct HideT2;

impl Visibility for HideT2 {
    fn visible(&self, _ctx: &RequestContext, element: ElementRef<'_>) -> bool {
        match element {
            ElementRef::Branch(branch) => branch.trust != TrustTier::T2,
            ElementRef::Type { branch, .. } | ElementRef::Field { branch, .. } => {
                branch.is_none_or(|b| b.trust != TrustTier::T2)
            }
            ElementRef::Operation(_) => true,
        }
    }

    fn is_principal_scoped(&self) -> bool {
        false
    }
}

pub(crate) struct FacadeInner {
    pub(crate) engine: Arc<dyn GraphqlEngine>,
    pub(crate) operations: Arc<dyn OperationCatalog>,
    pub(crate) tasks: Arc<dyn TaskBackend>,
    pub(crate) visibility: Arc<dyn Visibility>,
    pub(crate) config: FacadeConfig,
    pub(crate) sessions: SessionStore,
}

impl fmt::Debug for FacadeInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FacadeInner")
            .field("engine", &self.engine)
            .field("operations", &self.operations)
            .field("tasks", &self.tasks)
            .field("visibility", &self.visibility)
            .field("config", &self.config)
            .field("sessions", &self.sessions)
            .finish()
    }
}

/// Builder for [`McpFacade`].
#[derive(Debug)]
pub struct McpFacadeBuilder {
    engine: Arc<dyn GraphqlEngine>,
    operations: Option<Arc<dyn OperationCatalog>>,
    tasks: Option<Arc<dyn TaskBackend>>,
    visibility: Option<Arc<dyn Visibility>>,
    config: FacadeConfig,
}

impl McpFacadeBuilder {
    /// Attaches a custom [`OperationCatalog`].
    #[must_use]
    pub fn operations(mut self, operations: Arc<dyn OperationCatalog>) -> Self {
        self.operations = Some(operations);
        self
    }

    /// Attaches a custom [`TaskBackend`].
    #[must_use]
    pub fn tasks(mut self, tasks: Arc<dyn TaskBackend>) -> Self {
        self.tasks = Some(tasks);
        self
    }

    /// Attaches a custom [`Visibility`] policy.
    #[must_use]
    pub fn visibility(mut self, visibility: Arc<dyn Visibility>) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Overrides the [`FacadeConfig`].
    #[must_use]
    pub fn config(mut self, config: FacadeConfig) -> Self {
        self.config = config;
        self
    }

    /// Validates [`FacadeConfig`] and constructs the [`McpFacade`].
    ///
    /// # Errors
    ///
    /// Returns [`FacadeConfigError`] if `config` violates any bound.
    pub fn build(self) -> Result<McpFacade, FacadeConfigError> {
        self.config.validate()?;
        let sessions = SessionStore::new(self.config.max_sessions, self.config.session_idle_ttl);
        Ok(McpFacade {
            inner: Arc::new(FacadeInner {
                engine: self.engine,
                operations: self
                    .operations
                    .unwrap_or_else(|| Arc::new(EmptyOperationCatalog)),
                tasks: self
                    .tasks
                    .unwrap_or_else(|| Arc::new(UnsupportedTaskBackend)),
                visibility: self.visibility.unwrap_or_else(|| Arc::new(AllVisible)),
                config: self.config,
                sessions,
            }),
        })
    }
}

/// Inbound MCP `2026-07-28` (+ `2025-11-25` session compat) server facade (`CMP-001`..`008`, `ARC-007`).
#[derive(Debug, Clone)]
pub struct McpFacade {
    pub(crate) inner: Arc<FacadeInner>,
}

impl McpFacade {
    /// Starts building a [`McpFacade`] backed by `engine`.
    #[must_use]
    pub fn builder(engine: Arc<dyn GraphqlEngine>) -> McpFacadeBuilder {
        McpFacadeBuilder {
            engine,
            operations: None,
            tasks: None,
            visibility: None,
            config: FacadeConfig::default(),
        }
    }

    /// Returns a reference to the validated [`FacadeConfig`].
    #[must_use]
    pub fn config(&self) -> &FacadeConfig {
        &self.inner.config
    }
}

impl HttpModule for McpFacade {
    fn module_name(&self) -> &'static str {
        "mcp"
    }

    fn mount_prefixes(&self) -> &'static [&'static str] {
        &["/mcp"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: HttpExchangeRequest,
    ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>> {
        Box::pin(async move { wire::handle_http(&self.inner, ctx, req).await })
    }
}

/// Factory closure producing a fresh [`RequestContext`] (with pinned [`Snapshot`](gap_core::snapshot::Snapshot))
/// per inbound `stdio` request (`INV-01`).
pub type ContextFactory = Arc<dyn Fn() -> GapResult<RequestContext> + Send + Sync>;

/// Newline-delimited JSON-RPC MCP server over `stdio` (`CMP-001`, `INV-01`).
pub struct StdioServer {
    facade: McpFacade,
    ctx_factory: ContextFactory,
}

impl fmt::Debug for StdioServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StdioServer")
            .field("facade", &self.facade)
            .finish_non_exhaustive()
    }
}

impl StdioServer {
    /// Creates a [`StdioServer`] dispatching each newline-delimited request through `facade` with a
    /// fresh [`RequestContext`] minted by `ctx_factory` (`INV-01`).
    #[must_use]
    pub fn new(facade: McpFacade, ctx_factory: ContextFactory) -> Self {
        Self {
            facade,
            ctx_factory,
        }
    }

    /// Serves newline-delimited JSON-RPC messages from `reader` to `writer` until EOF (`CMP-001`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] only if writing to `writer` or reading from `reader` suffers an
    /// unrecoverable I/O failure.
    pub async fn serve<R, W>(&self, reader: R, writer: W) -> GapResult<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        wire::serve_stdio(&self.facade.inner, &self.ctx_factory, reader, writer).await
    }
}
