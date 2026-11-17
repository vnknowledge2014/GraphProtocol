//! Optional System-1 (`Laya`) discovery backend wiring and graceful `BaselineRouter` fallback
//! (`spec/04-routing.md` §6, `RTE-041`, `RTE-042`, `RTE-044`, `RTE-045`, `INV-17`, `WP-M5-03`).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gap_core::config::{Profile, RouterProviderConfig};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::secret::SecretString;
use gap_core::traits::{BoxFuture, DiscoveryService, Embedder};
use gap_route::s1_adapter::{
    BaselineRouter, DecisionBackend, RoutingModel, S1AdapterConfig, S1RouterAdapter,
};
use gap_route::{DiscoveryConfig, GroupOption, RouteDiscoveryService, VisibilityFactory};
use gap_s1::sidecar::{HttpSidecar, SidecarConfig};
use gap_s1::{
    GEMINI_DECISION_1_DEFAULT_MODEL, GeminiDecisionBackend, GeminiDecisionConfig,
    JEV_DEFAULT_MODEL, JevBackend, JevConfig, LayaCandle, LayaTokenizer, Precision,
    RECEPTON_LAYA_DEFAULT_MODEL, ReceptronLayaBackend, ReceptronLayaConfig,
};

use crate::assemble::{BranchBuildError, ServerFile};

/// Configuration selecting which System-1 (`Laya`) backend `gap-server` wires into discovery
/// (`RTE-041`, `RTE-044`, `INV-17`).
#[derive(Debug, Clone, Default)]
pub enum S1BackendMode {
    /// Disable System-1 inference and route directly via [`BaselineRouter`] (`INV-17`).
    #[default]
    Disabled,
    /// Calibrated built-in [`LayaCandle`] checkpoint in `precision` (no disk I/O).
    CalibratedInMemory {
        /// Tensor storage precision (`F32` or `F16`).
        precision: Precision,
    },
    /// ONNX Runtime System-1 backend (`router.provider = "laya_ort"`, degrades fail-open if
    /// external ONNX checkpoint or runtime feature is absent, `INV-17`).
    LayaOrt {
        /// Optional configured model path or identifier.
        model: Option<String>,
    },
    /// Memory-map an external `.safetensors` checkpoint from `weights_path` (`ARC-003`).
    ///
    /// If loading or validating the weights or tokenizer fails at startup, [`wire_s1_discovery`]
    /// logs a structured warning and falls back to [`BaselineRouter`] without aborting startup
    /// (`RTE-044`, `INV-17`).
    Safetensors {
        /// Path to the verified `.safetensors` weights file.
        weights_path: PathBuf,
        /// Optional HuggingFace `vocab.json` tokenizer path (`None` selects `LayaTokenizer::builtin_default`).
        vocab_path: Option<PathBuf>,
        /// Expected tensor precision stored in the checkpoint (`F32` or `F16`).
        precision: Precision,
    },
    /// Generic `/v1/systemone` HTTP sidecar (`omni decide --endpoint`).
    HttpSidecar {
        /// Base URL of the `/v1/systemone` endpoint.
        endpoint: String,
        /// Permit loopback destinations when true (`dev` / local tests).
        allow_loopback: bool,
    },
    /// `@receptron/laya` ONNX HTTP bridge (`github.com/receptron/laya`).
    ReceptronLaya {
        /// Base URL of the `@receptron/laya` endpoint.
        endpoint: String,
        /// Optional model override (`convaiinnovations/laya` default).
        model: Option<String>,
        /// Permit loopback destinations when true.
        allow_loopback: bool,
    },
    /// TypeSafe AI `Jev` (`POST /v1/systemone`, `model: "jev-latest"`).
    Jev {
        /// Base URL of Jev endpoint or local TLS proxy.
        endpoint: String,
        /// Optional model override (`jev-latest` default).
        model: Option<String>,
        /// Optional redacted bearer API credential (`INV-15`).
        api_key: Option<SecretString>,
        /// Permit loopback destinations when true.
        allow_loopback: bool,
    },
    /// Google `Gemini Decision 1` (`POST /v1beta/decisions`, zero output tokens).
    GeminiDecision1 {
        /// Base URL of Gemini Decisions endpoint or local TLS proxy.
        endpoint: String,
        /// Optional model override (`gemini-decision-1` default).
        model: Option<String>,
        /// Optional redacted `x-goog-api-key` credential (`INV-15`).
        api_key: Option<SecretString>,
        /// Permit loopback destinations when true.
        allow_loopback: bool,
    },
}

/// Complete configuration bundle for wiring GAP's discovery service with optional System-1
/// reranking (`WP-M5-03`).
#[derive(Clone)]
pub struct S1WiringConfig {
    /// Selected System-1 backend mode (`Disabled`, `CalibratedInMemory`, or `Safetensors`).
    pub mode: S1BackendMode,
    /// Concurrency semaphore, TTL cache, debias, and circuit-breaker configuration (`RTE-043`..`046`).
    pub adapter: S1AdapterConfig,
    /// High-level discovery configuration (`top_k_groups`, `s1_timeout_ms`, calibration enforcement).
    pub discovery: DiscoveryConfig,
    /// Active gateway deployment profile (`Dev` or `Prod`).
    pub profile: Profile,
    /// Optional query embedder used during hybrid retrieval (`ADR-010`).
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Optional principal-aware schema visibility policy factory (`INV-05`).
    pub visibility_factory: Option<VisibilityFactory>,
}

impl fmt::Debug for S1WiringConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S1WiringConfig")
            .field("mode", &self.mode)
            .field("adapter", &self.adapter)
            .field("discovery", &self.discovery)
            .field("profile", &self.profile)
            .field("has_embedder", &self.embedder.is_some())
            .field("has_visibility_factory", &self.visibility_factory.is_some())
            .finish()
    }
}

impl Default for S1WiringConfig {
    fn default() -> Self {
        Self::from_profile(Profile::Dev)
    }
}

impl S1WiringConfig {
    /// Constructs a baseline-first configuration appropriate for `profile` (`INV-17`).
    ///
    /// Query redaction/storage in `Dev` defaults `store_queries = true` whereas `Prod` sets
    /// `store_queries = false` (`RTE-100`).
    #[must_use]
    pub fn from_profile(profile: Profile) -> Self {
        let mut discovery = DiscoveryConfig::default();
        discovery.privacy.store_queries = matches!(profile, Profile::Dev);
        Self {
            mode: S1BackendMode::Disabled,
            adapter: S1AdapterConfig::default(),
            discovery,
            profile,
            embedder: None,
            visibility_factory: None,
        }
    }

    /// Derives [`S1WiringConfig`] from a validated [`ServerFile`] (`[router]`, `RTE-049`, `INV-15`).
    ///
    /// Resolves any configured `router.api_key_secret` without echoing secret material in errors.
    ///
    /// # Errors
    ///
    /// Returns [`BranchBuildError::SecretResolution`] when `router.api_key_secret` fails to resolve.
    pub fn from_server_file(server_file: &ServerFile) -> Result<Self, BranchBuildError> {
        let cfg = server_file.config();
        let mut out = Self::from_profile(cfg.profile);
        out.discovery.s1_timeout_ms = cfg.router.timeout_ms;
        let allow_loopback = matches!(cfg.profile, Profile::Dev);
        let api_key = match &cfg.router.api_key_secret {
            Some(secret_ref) => {
                Some(server_file.resolve_secret(secret_ref, "router.api_key_secret")?)
            }
            None => None,
        };
        let trimmed_model = cfg
            .router
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        out.mode = match cfg.router.provider {
            RouterProviderConfig::Disabled => S1BackendMode::Disabled,
            RouterProviderConfig::LayaCandle => match trimmed_model {
                Some(raw) => {
                    let p = Path::new(raw);
                    let weights_path = if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        server_file.base_dir().join(p)
                    };
                    S1BackendMode::Safetensors {
                        weights_path,
                        vocab_path: None,
                        precision: Precision::F16,
                    }
                }
                None => S1BackendMode::CalibratedInMemory {
                    precision: Precision::F16,
                },
            },
            RouterProviderConfig::LayaOrt => S1BackendMode::LayaOrt {
                model: trimmed_model.map(ToOwned::to_owned),
            },
            RouterProviderConfig::HttpSidecar => S1BackendMode::HttpSidecar {
                endpoint: cfg.router.endpoint.clone().unwrap_or_default(),
                allow_loopback,
            },
            RouterProviderConfig::ReceptronLaya => S1BackendMode::ReceptronLaya {
                endpoint: cfg.router.endpoint.clone().unwrap_or_default(),
                model: trimmed_model.map(ToOwned::to_owned),
                allow_loopback,
            },
            RouterProviderConfig::Jev => S1BackendMode::Jev {
                endpoint: cfg.router.endpoint.clone().unwrap_or_default(),
                model: trimmed_model.map(ToOwned::to_owned),
                api_key,
                allow_loopback,
            },
            RouterProviderConfig::GeminiDecision1 => S1BackendMode::GeminiDecision1 {
                endpoint: cfg.router.endpoint.clone().unwrap_or_default(),
                model: trimmed_model.map(ToOwned::to_owned),
                api_key,
                allow_loopback,
            },
        };
        Ok(out)
    }
}

/// Async timeout wrapper around any [`DecisionBackend`] using [`tokio::time::timeout`] (`RTE-044`).
#[derive(Debug, Clone)]
pub struct TokioTimeoutBackend<B> {
    inner: B,
}

impl<B> TokioTimeoutBackend<B> {
    /// Wraps `inner` so [`DecisionBackend::score_choice_with_timeout`] actively preempts stalls.
    #[must_use]
    pub const fn new(inner: B) -> Self {
        Self { inner }
    }

    /// Borrows the underlying [`DecisionBackend`].
    #[must_use]
    pub const fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: DecisionBackend> DecisionBackend for TokioTimeoutBackend<B> {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn head_max_len(&self) -> usize {
        self.inner.head_max_len()
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        self.inner.score_choice(state_jcs, question, options)
    }

    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        Box::pin(async move {
            tokio::time::timeout(
                timeout,
                self.inner.score_choice(state_jcs, question, options),
            )
            .await
            .map_err(|_| {
                GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("S1 inference timed out after {}ms", timeout.as_millis()),
                )
            })?
        })
    }
}

/// Wired discovery plane instance produced by [`wire_s1_discovery`] or [`wire_s1_with_backend`].
#[derive(Debug, Clone)]
pub struct WiredS1Discovery {
    /// Concrete [`RouteDiscoveryService`] erased over [`RoutingModel`].
    pub discovery: Arc<RouteDiscoveryService<Arc<dyn RoutingModel>>>,
    /// Active primary router identifier (`"baseline"` or `"s1:<model_id>"`).
    router_id: String,
    /// `true` iff a System-1 backend was loaded and attached (`false` when baseline or degraded).
    pub using_s1: bool,
    /// Startup degradation warning emitted when System-1 loading fell back to baseline (`INV-17`).
    pub degraded_startup_warning: Option<String>,
}

impl WiredS1Discovery {
    /// Returns the object-safe [`DiscoveryService`] trait object handle.
    #[must_use]
    pub fn as_discovery_service(&self) -> Arc<dyn DiscoveryService> {
        Arc::clone(&self.discovery) as Arc<dyn DiscoveryService>
    }

    /// Returns the primary configured router identifier (`"baseline"` or `"s1:<model_id>"`).
    #[must_use]
    pub fn router_id(&self) -> &str {
        &self.router_id
    }
}

fn build_service(
    router: Arc<dyn RoutingModel>,
    cfg: &S1WiringConfig,
) -> Arc<RouteDiscoveryService<Arc<dyn RoutingModel>>> {
    let mut service =
        RouteDiscoveryService::new(router, cfg.embedder.clone(), cfg.discovery.clone());
    if let Some(factory) = &cfg.visibility_factory {
        service = service.with_visibility_factory(Arc::clone(factory));
    }
    Arc::new(service)
}

fn baseline_fallback(cfg: &S1WiringConfig, warning: Option<String>) -> WiredS1Discovery {
    let router: Arc<dyn RoutingModel> = Arc::new(BaselineRouter::default());
    let router_id = router.id().to_owned();
    let discovery = build_service(router, cfg);
    WiredS1Discovery {
        discovery,
        router_id,
        using_s1: false,
        degraded_startup_warning: warning,
    }
}

/// Wires GAP discovery from `cfg`, loading optional System-1 weights fail-safe (`INV-17`).
///
/// Missing, corrupt, or precision-mismatched weight/tokenizer files never fail gateway startup:
/// a `GAP_DISCOVERY_DEGRADED` warning is recorded and [`BaselineRouter`] serves discovery cleanly.
#[must_use]
pub fn wire_s1_discovery(cfg: &S1WiringConfig) -> WiredS1Discovery {
    match &cfg.mode {
        S1BackendMode::Disabled => baseline_fallback(cfg, None),
        S1BackendMode::CalibratedInMemory { precision } => {
            wire_s1_with_backend(LayaCandle::calibrated_default(*precision), cfg)
        }
        S1BackendMode::LayaOrt { model } => {
            let msg = format!(
                "GAP_DISCOVERY_DEGRADED: router.provider = `laya_ort` (model={model:?}) requires external ONNX runtime checkpoint; falling back to baseline router"
            );
            tracing::warn!(model = ?model, "laya_ort runtime unavailable; falling back to BaselineRouter");
            baseline_fallback(cfg, Some(msg))
        }
        S1BackendMode::Safetensors {
            weights_path,
            vocab_path,
            precision,
        } => {
            let tokenizer_res = match vocab_path.as_deref() {
                Some(path) => LayaTokenizer::from_vocab_json(path),
                None => Ok(LayaTokenizer::builtin_default()),
            };
            let backend_res = tokenizer_res.and_then(|tokenizer| {
                LayaCandle::from_safetensors_mmap(weights_path, tokenizer, *precision)
            });
            match backend_res {
                Ok(model) => wire_s1_with_backend(model, cfg),
                Err(err) => {
                    let msg = format!(
                        "GAP_DISCOVERY_DEGRADED: failed to load S1 weights `{}`: {err}; falling back to baseline router",
                        weights_path.display()
                    );
                    tracing::warn!(
                        weights_path = %weights_path.display(),
                        error = %err,
                        "S1 weights unavailable; falling back to BaselineRouter"
                    );
                    baseline_fallback(cfg, Some(msg))
                }
            }
        }
        S1BackendMode::HttpSidecar {
            endpoint,
            allow_loopback,
        } => {
            let sc_cfg = if *allow_loopback {
                SidecarConfig::loopback(endpoint)
            } else {
                SidecarConfig::strict(endpoint)
            };
            match HttpSidecar::new(sc_cfg) {
                Ok(sidecar) => wire_s1_with_backend(sidecar, cfg),
                Err(err) => baseline_fallback(
                    cfg,
                    Some(format!(
                        "GAP_DISCOVERY_DEGRADED: failed to initialize HttpSidecar `{endpoint}`: {err}; falling back to baseline router"
                    )),
                ),
            }
        }
        S1BackendMode::ReceptronLaya {
            endpoint,
            model,
            allow_loopback,
        } => {
            let mut rc = if *allow_loopback {
                ReceptronLayaConfig::loopback(endpoint)
            } else {
                ReceptronLayaConfig::strict(endpoint)
            };
            rc.model = model
                .clone()
                .unwrap_or_else(|| RECEPTON_LAYA_DEFAULT_MODEL.to_owned());
            wire_s1_with_backend(ReceptronLayaBackend::new(rc), cfg)
        }
        S1BackendMode::Jev {
            endpoint,
            model,
            api_key,
            allow_loopback,
        } => {
            let mut jc = if *allow_loopback {
                JevConfig::loopback(endpoint, api_key.clone())
            } else {
                JevConfig::strict(endpoint, api_key.clone())
            };
            jc.model = model
                .clone()
                .unwrap_or_else(|| JEV_DEFAULT_MODEL.to_owned());
            wire_s1_with_backend(JevBackend::new(jc), cfg)
        }
        S1BackendMode::GeminiDecision1 {
            endpoint,
            model,
            api_key,
            allow_loopback,
        } => {
            let mut gc = if *allow_loopback {
                GeminiDecisionConfig::loopback(endpoint, api_key.clone())
            } else {
                GeminiDecisionConfig::strict(endpoint, api_key.clone())
            };
            gc.model = model
                .clone()
                .unwrap_or_else(|| GEMINI_DECISION_1_DEFAULT_MODEL.to_owned());
            wire_s1_with_backend(GeminiDecisionBackend::new(gc), cfg)
        }
    }
}

/// Wires an arbitrary [`DecisionBackend`] wrapped in [`TokioTimeoutBackend`] and
/// [`S1RouterAdapter`] into [`RouteDiscoveryService`].
#[must_use]
pub fn wire_s1_with_backend<B: DecisionBackend + 'static>(
    backend: B,
    cfg: &S1WiringConfig,
) -> WiredS1Discovery {
    let wrapped = TokioTimeoutBackend::new(backend);
    let adapter: Arc<dyn RoutingModel> =
        Arc::new(S1RouterAdapter::new(wrapped, cfg.adapter.clone()));
    let router_id = adapter.id().to_owned();
    let discovery = build_service(adapter, cfg);
    WiredS1Discovery {
        discovery,
        router_id,
        using_s1: true,
        degraded_startup_warning: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Instant;

    use axum::body::Body;
    use gap_core::config::{BranchProtocol, BranchSpec};
    use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
    use gap_core::ids::{BranchId, BranchUid, GroupId, OperationId, TenantId};
    use gap_core::snapshot::{
        BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CapabilityGroup, CostHint,
        Endpoint, Example, OpKind, OperationDescriptor, Protocol, SideEffect, SideEffectLevel,
        Snapshot, TrustTier,
    };
    use gap_core::traits::{DiscoveryOutcome, DiscoveryRequest, ElementKind};
    use gap_index::{ElementDoc, HybridElementIndex, HybridIndexConfig};
    use gap_mcp::outbound::fake::{FakeCall, FakeMcpServer, FakeTool, ServerEra};
    use gap_mcp::outbound::transport::http::HttpTransport;
    use gap_mcp::outbound::{McpConfig, McpConnector};
    use gap_ngc_openapi::{
        AuthConfig, OpenApiConfig, OpenApiConnector, StaticCredentialSource, Upstream,
        UpstreamError, UpstreamRequest, UpstreamResponse,
    };
    use gap_ql::with_gap_builtins;
    use gap_s1::LayaConfig;
    use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
    use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tower::ServiceExt as _;

    use super::*;
    use crate::assemble::{
        BranchBuildError, BranchFactory, BranchRuntime, ServerFile, assemble, wrap_connector,
    };

    const DEV_KEY: &str = "gap_dev_skeleton_key_01";

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("canonical repo root")
    }

    #[derive(Debug, Clone, Default)]
    struct FakeShopUpstream;

    impl Upstream for FakeShopUpstream {
        async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            let pq = request.path_and_query.as_str();
            let (path, _query) = pq.split_once('?').unwrap_or((pq, ""));
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

            let body_val = match path {
                "/customers/1" => json!({"id": 1, "name": "Ada Lovelace", "tier": "vip"}),
                "/orders" => json!([
                    {"id": 101, "customer_id": 1, "total_cents": 4200, "status": "paid"}
                ]),
                _ => {
                    return Ok(UpstreamResponse {
                        status: StatusCode::NOT_FOUND,
                        headers,
                        body: br#"{"error":"not_found"}"#.to_vec(),
                    });
                }
            };
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers,
                body: serde_json::to_vec(&body_val).unwrap(),
            })
        }
    }

    fn build_fake_mcp_server() -> FakeMcpServer {
        let server = FakeMcpServer::new(ServerEra::Modern);
        let mut tool = FakeTool::new(
            "customer_notes",
            json!({
                "type": "object",
                "required": ["customer_id"],
                "additionalProperties": false,
                "properties": {
                    "customer_id": { "type": "integer", "minimum": 1 }
                }
            }),
        );
        tool.description = "Fetch account notes for a shop customer id.".to_owned();
        server.set_tools(vec![tool]);
        server.set_call_handler(|name, args| {
            if name == "customer_notes" {
                let cid = args
                    .get("customer_id")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                FakeCall::Reply(json!({
                    "content": [{"type": "text", "text": format!("notes for {cid}")}],
                    "structuredContent": {
                        "customer_id": cid,
                        "notes": [{"note_id": "n-1", "customer_id": cid, "summary": "VIP note"}]
                    },
                    "isError": false
                }))
            } else {
                FakeCall::Error(-32602, "unknown tool".to_owned())
            }
        });
        server
    }

    #[derive(Debug, Default)]
    struct FakeBranchFactory;

    impl BranchFactory for FakeBranchFactory {
        fn build_branch<'a>(
            &'a self,
            server_file: &'a ServerFile,
            _branch_index: usize,
            branch: &'a BranchSpec,
            _uid: BranchUid,
        ) -> BoxFuture<'a, Result<BranchRuntime, BranchBuildError>> {
            Box::pin(async move {
                match branch.protocol {
                    BranchProtocol::Openapi => {
                        let doc_bytes =
                            server_file.openapi_document(&branch.id).ok_or_else(|| {
                                BranchBuildError::ConnectorInit {
                                    branch_id: branch.id.clone(),
                                    reason: "missing preloaded OpenAPI bytes".to_owned(),
                                }
                            })?;
                        let spec = branch.openapi.as_ref().unwrap();
                        let cfg = OpenApiConfig {
                            connector_id: branch.id.to_string(),
                            base_url: spec.base_url.clone(),
                            allow_insecure_upstream: spec.allow_insecure_upstream,
                            auth: AuthConfig::None,
                            ..OpenApiConfig::default()
                        };
                        let creds = StaticCredentialSource::new();
                        let connector = OpenApiConnector::from_document_bytes(
                            doc_bytes,
                            cfg,
                            &creds,
                            FakeShopUpstream,
                        )
                        .map_err(|err| {
                            BranchBuildError::ConnectorInit {
                                branch_id: branch.id.clone(),
                                reason: err.to_string(),
                            }
                        })?;
                        wrap_connector(&branch.id, connector).await
                    }
                    BranchProtocol::Mcp => {
                        let fake_mcp = build_fake_mcp_server();
                        let transport =
                            Arc::new(HttpTransport::new(fake_mcp.http_client(), 1 << 20));
                        let mcp_cfg = McpConfig {
                            connector_id: branch.id.to_string(),
                            ..McpConfig::default()
                        };
                        let connector = McpConnector::new(transport, mcp_cfg).map_err(|err| {
                            BranchBuildError::ConnectorInit {
                                branch_id: branch.id.clone(),
                                reason: err.to_string(),
                            }
                        })?;
                        let mut runtime = wrap_connector(&branch.id, connector).await?;
                        runtime.handle = Box::new((runtime.handle, fake_mcp));
                        Ok(runtime)
                    }
                }
            })
        }
    }

    fn make_group_id(tag: &str) -> GroupId {
        let mut bytes = [0u8; 16];
        for (idx, b) in tag.bytes().take(16).enumerate() {
            bytes[idx] = b;
        }
        GroupId::from_bytes(bytes)
    }

    fn make_op(
        branch: &BranchId,
        name: &str,
        group_tag: &str,
        sdl: &str,
        intent: &str,
    ) -> Arc<OperationDescriptor> {
        let id = OperationId::from_parts(branch, name).expect("valid operation id");
        Arc::new(OperationDescriptor {
            id,
            group: make_group_id(group_tag),
            kind: OpKind::Query,
            side_effect: SideEffect {
                level: SideEffectLevel::None,
                verified: true,
                requires_approval: false,
            },
            idempotent: true,
            cost: CostHint::default(),
            desc_hash: [1u8; 32],
            sdl: Arc::from(sdl),
            examples: vec![Example {
                intent: intent.to_owned(),
                positive: true,
                query_snippet: None,
            }],
        })
    }

    fn make_branch(
        slug: &str,
        synopsis: &str,
        ops: Vec<Arc<OperationDescriptor>>,
    ) -> BranchDescriptor {
        BranchDescriptor {
            uid: BranchUid::new_v7(),
            id: BranchId::new(slug).expect("valid branch id"),
            protocol: Protocol::Rest,
            trust: TrustTier::T0,
            endpoint: Endpoint {
                uri: format!("https://{slug}.internal"),
                transport: "http".to_owned(),
            },
            spec_hash: [3u8; 32],
            pinned_desc_hash: [3u8; 32],
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: synopsis.to_owned(),
            operations: ops,
            status: BranchStatus::Active,
        }
    }

    fn make_multi_branch_snapshot() -> Arc<Snapshot> {
        let sdl = r#"
            type Invoice @key(fields: "id") { id: ID! status: String! amountCents: Int! }
            type Shipment @key(fields: "id") { id: ID! carrier: String! etaDays: Int! }
            type Query {
                billingInvoiceStatus(invoiceId: ID!): Invoice
                shippingParcelTrack(parcelId: ID!): Shipment
            }
        "#;
        let schema = with_gap_builtins(sdl).expect("valid schema").into_inner();
        let tenant = TenantId::default_tenant();

        let b_billing = BranchId::new("billing").unwrap();
        let b_shipping = BranchId::new("shipping").unwrap();
        let op_billing = make_op(
            &b_billing,
            "billingInvoiceStatus",
            "grp-billing",
            "billingInvoiceStatus(invoiceId: ID!): Invoice",
            "billing invoice details and payment status",
        );
        let op_shipping = make_op(
            &b_shipping,
            "shippingParcelTrack",
            "grp-shipping",
            "shippingParcelTrack(parcelId: ID!): Shipment",
            "shipping parcel tracking and carrier delivery estimates",
        );

        let br_billing = make_branch(
            "billing",
            "Billing invoices and ledger payments",
            vec![Arc::clone(&op_billing)],
        );
        let br_shipping = make_branch(
            "shipping",
            "Shipping parcel carrier tracking",
            vec![Arc::clone(&op_shipping)],
        );

        let docs = vec![
            ElementDoc::new(
                op_billing.id.as_str(),
                ElementKind::Operation,
                "billingInvoiceStatus",
            )
            .with_tenant(tenant.clone())
            .with_branch_uid(br_billing.uid)
            .with_operation_id(op_billing.id.clone())
            .with_path("Query.billingInvoiceStatus")
            .with_text("billing invoice details payment ledger refund status")
            .with_examples(vec!["find billing invoice status".to_owned()]),
            ElementDoc::new(
                op_shipping.id.as_str(),
                ElementKind::Operation,
                "shippingParcelTrack",
            )
            .with_tenant(tenant)
            .with_branch_uid(br_shipping.uid)
            .with_operation_id(op_shipping.id.clone())
            .with_path("Query.shippingParcelTrack")
            .with_text("shipping parcel delivery tracking carrier customs events")
            .with_examples(vec!["track shipping parcel delivery".to_owned()]),
        ];
        let index = HybridElementIndex::build(docs, HybridIndexConfig::default())
            .expect("build hybrid index");

        let mut snap = Snapshot::empty(7);
        snap.schema = Arc::new(schema);
        snap.index = Arc::new(index);
        snap.groups.insert(
            op_billing.group.clone(),
            Arc::new(CapabilityGroup {
                id: op_billing.group.clone(),
                signature_sdl: Arc::from("Query.billingInvoiceStatus(invoiceId: ID!): Invoice"),
                members: vec![(br_billing.uid, op_billing.id.clone())],
            }),
        );
        snap.groups.insert(
            op_shipping.group.clone(),
            Arc::new(CapabilityGroup {
                id: op_shipping.group.clone(),
                signature_sdl: Arc::from("Query.shippingParcelTrack(parcelId: ID!): Shipment"),
                members: vec![(br_shipping.uid, op_shipping.id.clone())],
            }),
        );
        snap.branches.insert(br_billing.uid, Arc::new(br_billing));
        snap.branches.insert(br_shipping.uid, Arc::new(br_shipping));
        Arc::new(snap)
    }

    fn make_ctx(snapshot: Arc<Snapshot>) -> RequestContext {
        let tenant = TenantId::default_tenant();
        RequestContext::try_new(
            BranchUid::new_v7().as_uuid(),
            tenant.clone(),
            Principal::new("agent-wiring-test", PrincipalKind::Agent, tenant),
            Instant::now() + Duration::from_secs(30),
            Budget::new(10_000),
            TraceCtx::generate_root(false),
            0,
            None,
            snapshot,
        )
        .expect("valid RequestContext")
    }

    #[derive(Debug)]
    enum StubMode {
        Hang,
        FailImmediate,
    }

    #[derive(Debug)]
    struct FaultyDecisionBackend {
        mode: StubMode,
        observed: Mutex<Vec<String>>,
    }

    impl FaultyDecisionBackend {
        fn hanging() -> Self {
            Self {
                mode: StubMode::Hang,
                observed: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                mode: StubMode::FailImmediate,
                observed: Mutex::new(Vec::new()),
            }
        }
    }

    impl DecisionBackend for FaultyDecisionBackend {
        fn model_id(&self) -> &str {
            "faulty-stub"
        }

        fn score_choice<'a>(
            &'a self,
            state_jcs: &'a str,
            _question: &'a str,
            _options: &'a [GroupOption],
        ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
            Box::pin(async move {
                self.observed.lock().unwrap().push(state_jcs.to_owned());
                match self.mode {
                    StubMode::Hang => {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        Ok(Vec::new())
                    }
                    StubMode::FailImmediate => Err(GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        "synthetic S1 backend down",
                    )),
                }
            })
        }
    }

    // spec: INV-17, RTE-044
    #[tokio::test]
    async fn inv_17_disabled_and_missing_weights_fall_back_cleanly_and_gateway_executes_queries() {
        let disabled_cfg = S1WiringConfig::from_profile(Profile::Prod);
        assert!(!disabled_cfg.discovery.privacy.store_queries);
        let disabled = wire_s1_discovery(&disabled_cfg);
        assert!(!disabled.using_s1);
        assert_eq!(disabled.router_id(), "baseline");
        assert!(disabled.degraded_startup_warning.is_none());

        let dir = TempDir::new().unwrap();
        let missing_path = dir.path().join("nonexistent-laya.safetensors");
        let mut missing_cfg = S1WiringConfig::from_profile(Profile::Dev);
        missing_cfg.mode = S1BackendMode::Safetensors {
            weights_path: missing_path,
            vocab_path: None,
            precision: Precision::F16,
        };
        let fallback = wire_s1_discovery(&missing_cfg);
        assert!(!fallback.using_s1);
        assert_eq!(fallback.router_id(), "baseline");
        assert!(
            fallback
                .degraded_startup_warning
                .as_deref()
                .is_some_and(|w| w.contains("GAP_DISCOVERY_DEGRADED"))
        );

        let snap = make_multi_branch_snapshot();
        let ctx = make_ctx(snap);
        let res = fallback
            .as_discovery_service()
            .resolve(
                &ctx,
                DiscoveryRequest {
                    intent: "find billing invoice status".to_owned(),
                    top_k: 3,
                    scopes: Vec::new(),
                },
            )
            .await
            .expect("baseline discovery succeeds even when weights are missing");
        assert_eq!(res.router, "baseline");
        assert!(!res.candidates.is_empty());

        // Assemble full gateway without S1 weights and confirm /healthz, /graphql, and /mcp remain green (`INV-17`).
        let sf = ServerFile::load(repo_root().join("examples/walking-skeleton/gap.toml")).unwrap();
        let assembled = assemble(&sf, &FakeBranchFactory).await.unwrap();
        let app = gap_transport::router(Arc::clone(&assembled.app_state));

        let health_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health_resp.status(), StatusCode::OK);

        let gql_req = Request::builder()
            .method(Method::POST)
            .uri("/graphql")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {DEV_KEY}"))
            .header(ACCEPT, "application/graphql-response+json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"
                }))
                .unwrap(),
            ))
            .unwrap();
        let gql_resp = app.clone().oneshot(gql_req).await.unwrap();
        assert_eq!(gql_resp.status(), StatusCode::OK);
        let gql_bytes = axum::body::to_bytes(gql_resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let gql_json: Value = serde_json::from_slice(&gql_bytes).unwrap();
        assert_eq!(gql_json["data"]["customer"]["name"], json!("Ada Lovelace"));
        assert_eq!(gql_json["data"]["customer_notes"]["customer_id"], json!(1));

        let mcp_req = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "server/discover")
            .header(AUTHORIZATION, format!("Bearer {DEV_KEY}"))
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "server/discover",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                }))
                .unwrap(),
            ))
            .unwrap();
        let mcp_resp = app.oneshot(mcp_req).await.unwrap();
        assert_eq!(mcp_resp.status(), StatusCode::OK);
    }

    // spec: INV-17, RTE-044, RTE-045
    #[tokio::test]
    async fn inv_17_s1_timeout_and_down_backend_degrade_to_baseline_with_warning() {
        let snap = make_multi_branch_snapshot();
        let ctx = make_ctx(snap);

        // 1. Hung backend preempted by `TokioTimeoutBackend` within s1_timeout_ms budget (`RTE-044`).
        let mut hang_cfg = S1WiringConfig::from_profile(Profile::Dev);
        hang_cfg.discovery.enforce_calibration_for_s1 = false;
        hang_cfg.discovery.s1_timeout_ms = 25;
        let wired_hang = wire_s1_with_backend(FaultyDecisionBackend::hanging(), &hang_cfg);
        assert!(wired_hang.using_s1);

        let t0 = Instant::now();
        let res_hang = wired_hang
            .discovery
            .resolve_detailed(
                &ctx,
                gap_route::ResolveConstraints::default(),
                "find billing invoice status",
            )
            .await
            .expect("timeout must gracefully fall back to baseline");
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "TokioTimeoutBackend should preempt hung S1 well under 500ms, took {elapsed:?}"
        );
        assert_eq!(res_hang.router, "baseline");
        assert!(
            res_hang
                .warnings
                .iter()
                .any(|w| w == "GAP_DISCOVERY_DEGRADED"),
            "warnings={:?}",
            res_hang.warnings
        );
        assert!(!res_hang.candidates.is_empty());

        // 2. Unreachable Jev / Gemini providers also fall back cleanly via `BaselineRouter` (`RTE-049`, `INV-17`).
        let mut remote_cfg = S1WiringConfig::from_profile(Profile::Dev);
        remote_cfg.discovery.enforce_calibration_for_s1 = false;
        remote_cfg.mode = S1BackendMode::GeminiDecision1 {
            endpoint: "http://127.0.0.1:9".to_owned(),
            model: None,
            api_key: Some(SecretString::new("canary_secret_wiring".to_owned())),
            allow_loopback: true,
        };
        let wired_gemini = wire_s1_discovery(&remote_cfg);
        assert!(wired_gemini.using_s1);
        let res_remote_err = wired_gemini
            .discovery
            .resolve_detailed(
                &ctx,
                gap_route::ResolveConstraints::default(),
                "find billing invoice status",
            )
            .await
            .expect("unreachable Gemini Decision 1 falls back cleanly");
        assert_eq!(res_remote_err.router, "baseline");
        assert!(!res_remote_err.candidates.is_empty());

        // 3. Immediate backend error also falls back to BaselineRouter (`INV-17`).
        let mut down_cfg = S1WiringConfig::from_profile(Profile::Dev);
        down_cfg.discovery.enforce_calibration_for_s1 = false;
        let wired_down = wire_s1_with_backend(FaultyDecisionBackend::failing(), &down_cfg);
        let res_down = wired_down
            .discovery
            .resolve_detailed(
                &ctx,
                gap_route::ResolveConstraints::default(),
                "track shipping parcel delivery",
            )
            .await
            .expect("down backend must gracefully degrade to baseline");
        assert_eq!(res_down.router, "baseline");
        assert!(
            res_down
                .warnings
                .iter()
                .any(|w| w == "GAP_DISCOVERY_DEGRADED")
        );
        assert_ne!(res_down.outcome, DiscoveryOutcome::Abstained);
    }

    // spec: RTE-030, RTE-041, RTE-042, INV-17
    #[tokio::test]
    async fn s1_adapter_wired_laya_candle_safetensors_ranks_groups_end_to_end() {
        let dir = TempDir::new().unwrap();
        let weights_path = dir.path().join("laya-f16.safetensors");
        LayaCandle::write_deterministic_checkpoint(
            &weights_path,
            &LayaConfig::reference_architecture(),
            Precision::F16,
        )
        .expect("write deterministic F16 checkpoint");

        let mut cfg = S1WiringConfig::from_profile(Profile::Dev);
        cfg.discovery.enforce_calibration_for_s1 = false;
        cfg.discovery.s1_timeout_ms = 2_000;
        cfg.mode = S1BackendMode::Safetensors {
            weights_path,
            vocab_path: None,
            precision: Precision::F16,
        };

        let wired = wire_s1_discovery(&cfg);
        assert!(wired.using_s1);
        assert!(wired.degraded_startup_warning.is_none());
        assert!(wired.router_id().starts_with("s1:laya-candle:f16:"));

        let snap = make_multi_branch_snapshot();
        let ctx = make_ctx(snap);
        let detailed = wired
            .discovery
            .resolve_detailed(
                &ctx,
                gap_route::ResolveConstraints::default(),
                "find billing invoice status details",
            )
            .await
            .expect("S1 end-to-end resolution succeeds");

        assert!(
            detailed.router.starts_with("s1:laya-candle:f16:"),
            "router={}",
            detailed.router
        );
        assert!(
            !detailed
                .warnings
                .iter()
                .any(|w| w == "GAP_DISCOVERY_DEGRADED"),
            "unexpected degradation warnings: {:?}",
            detailed.warnings
        );
        assert!(!detailed.candidates.is_empty());
        assert_eq!(detailed.candidates[0].branch.as_str(), "billing");
    }
}
