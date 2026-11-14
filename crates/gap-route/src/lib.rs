//! Discovery plane (`__metaGraph`, `__search`, `gap.resolve`), input sanitization, capability
//! grouping, and split-conformal calibration (`L2`, `spec/04-routing.md`, `spec/02-gap-ql.md`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod calibration_job;
pub mod debias;
pub mod feedback;
pub mod groups;
pub mod s1_adapter;
pub mod sanitize;

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use gap_core::context::{Principal, RequestContext};
use gap_core::error::GapResult;
use gap_core::ids::{BranchId, GroupId};
use gap_core::snapshot::{
    BranchDescriptor, BranchStatus, OperationDescriptor, SideEffectLevel, Snapshot, TrustTier,
};
use gap_core::traits::{
    BoxFuture, DiscoveryCandidate, DiscoveryOutcome, DiscoveryRequest, DiscoveryResolution,
    DiscoveryService, ElementKind, Embedder, SearchHit,
};
use gap_ql::{AllowAllVisibility, Schema, Valid, Visibility, project_visible_schema};

pub use crate::calibration_job::{
    ALPHA_MUTATING, ALPHA_READ_ONLY, CalibrationMode, CalibrationVersionKey, ConformalDecision,
    ConformalQuantile, MIN_CONFORMAL_SAMPLES, OptionsBucket, TEMP_MAX, TEMP_MIN,
    TemperatureFitResult, VersionedCalibrationRecord, alpha_for_side_effect_constraint,
    clamp_temperature, compute_ece_15_from_logits, conformal_decide, fit_conformal_lac,
    fit_temperature_golden_section, is_snapshot_calibration_valid, lookup_calibration_params,
    snapshot_calibration_id, softmax_scaled,
};
pub use crate::debias::{average_reversed_logits, reverse_options};
pub use crate::feedback::{
    AbstentionReason, FeedbackOutcome, QueryPrivacyPolicy, RoutingFeedbackRow,
    UNMAPPED_COALESCE_WINDOW_SECS, UNMAPPED_RETENTION_SECS, UNMAPPED_TENANT_RATE_PER_MIN,
    UnmappedDomainEvent, UnmappedDomainTracker, redact_query,
};
pub use crate::groups::{
    BuiltSkeleton, DEFAULT_TOP_K_GROUPS, GroupOption, MAX_TOP_K_GROUPS, MIN_TOP_K_GROUPS,
    ResolvedGroup, SINK_OPTION_KEY, SINK_OPTION_TEXT, build_candidate_skeleton,
    build_group_options, build_sanitized_option_text, cmp_f32_desc, effective_side_effect,
    is_operation_visible, op_kind_as_str, root_type_for_op_kind, side_effect_allows,
    side_effect_as_str, side_effect_rank, trust_as_str,
};
pub use crate::s1_adapter::{
    BaselineRouter, DEFAULT_BASELINE_SINK_LOGIT, DEFAULT_BASELINE_TAU, DecisionBackend,
    OPTION_FORMAT_VERSION, RankCtx, RankOutput, RoutingModel, S1_CHOICE_QUESTION, S1AdapterConfig,
    S1RouterAdapter, format_s1_options, format_s1_state_jcs,
};
pub use crate::sanitize::{
    DEFAULT_MAX_QUERY_CHARS, SPECIAL_TOKEN_MARKERS, SanitizedQuery, lint_branch_text, sanitize,
    sanitize_default,
};

/// Default retrieval score threshold below which `__search` sets `abstained = true` (`GQL-020`).
pub const DEFAULT_RETRIEVAL_MIN_SCORE: f32 = 0.15;

/// Default maximum branch entries surfaced by `__metaGraph` (`GQL-023`).
pub const DEFAULT_METAGRAPH_MAX_BRANCHES: usize = 100;

/// Maximum UTF-8 byte budget per branch synopsis in `__metaGraph` (`<= 24` tokens, `GQL-023`).
pub const MAX_METAGRAPH_SYNOPSIS_BYTES: usize = 90;

/// Tokenizer-agnostic token estimator `ceil(utf8_bytes / 3.6)` specified by `GQL-022`.
#[must_use]
pub fn estimate_tokens(utf8_bytes: usize) -> u32 {
    if utf8_bytes == 0 {
        return 0;
    }
    let est = (utf8_bytes as f64 / 3.6).ceil();
    est.clamp(1.0, u32::MAX as f64) as u32
}

/// Configuration for [`RouteDiscoveryService`].
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryConfig {
    /// Threshold below which `__search` abstains (`GQL-020`, default `0.15`).
    pub min_score: f32,
    /// Top `K` capability groups considered per `gap.resolve` (`RTE-030`, clamped to `4..=15`).
    pub top_k_groups: usize,
    /// Maximum branches emitted by `__metaGraph` (`GQL-023`, default `100`).
    pub metagraph_branches: usize,
    /// Maximum candidate groups returned by `gap.resolve` (`RTE-033`, default `5`, max `10`).
    pub default_max_candidates: usize,
    /// Privacy policy for emitted `UnmappedDomainEvent`s (`RTE-100`).
    pub privacy: QueryPrivacyPolicy,
    /// Require valid calibration key match for non-baseline routers (`RTE-067`).
    pub enforce_calibration_for_s1: bool,
    /// S1 inference timeout budget in milliseconds (`RTE-044`, default `400`).
    pub s1_timeout_ms: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            min_score: DEFAULT_RETRIEVAL_MIN_SCORE,
            top_k_groups: DEFAULT_TOP_K_GROUPS,
            metagraph_branches: DEFAULT_METAGRAPH_MAX_BRANCHES,
            default_max_candidates: 5,
            privacy: QueryPrivacyPolicy::default(),
            enforce_calibration_for_s1: true,
            s1_timeout_ms: 400,
        }
    }
}

/// Caller-supplied visibility factory parameterized on authenticated [`Principal`] (`INV-05`,
/// `GQL-062`).
pub type VisibilityFactory = Arc<dyn Fn(&Principal) -> Box<dyn Visibility> + Send + Sync>;

/// Input constraints for `gap.resolve` (`spec/02-gap-ql.md` §3, `RTE-032`, `RTE-033`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveConstraints {
    /// Optional subset of branch slugs to restrict candidates to.
    pub branches: Vec<BranchId>,
    /// Maximum permitted side-effect severity (`NONE` by default, `RTE-032`).
    pub max_side_effect: SideEffectLevel,
    /// Maximum candidates to return (`1..=10`, default `5`, `RTE-033`).
    pub max_candidates: usize,
    /// Optional token budget constraining rendered candidate skeletons/SDLs.
    pub budget_tokens: Option<u32>,
}

impl Default for ResolveConstraints {
    fn default() -> Self {
        Self {
            branches: Vec::new(),
            max_side_effect: SideEffectLevel::None,
            max_candidates: 5,
            budget_tokens: None,
        }
    }
}

/// Single ranked capability group candidate emitted by `gap.resolve` (`spec/02-gap-ql.md` §3).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveCandidate {
    /// Capability group identifier.
    pub group_id: GroupId,
    /// Calibrated relevance score / probability.
    pub score: f32,
    /// Calibrated softmax probability when calibrated (`None` on uncalibrated baseline).
    pub probability: Option<f32>,
    /// Operation identifier (`<branch>.<name>`).
    pub operation_id: String,
    /// Owning branch slug.
    pub branch: BranchId,
    /// Operation kind (`QUERY`, `MUTATION`, `SUBSCRIPTION`, `TASK`).
    pub kind: String,
    /// Validated GAP-QL query skeleton (`RTE-031`, `GQL-012`).
    pub skeleton: String,
    /// JSON Schema string describing required skeleton variables (`variablesSchema`).
    pub variables_schema_json: String,
    /// Progressive-disclosure SDL fragment (`RTE-031`).
    pub sdl: String,
    /// Effective side-effect classification (`GQL-032`).
    pub side_effect: SideEffectLevel,
    /// Owning branch trust tier.
    pub trust: TrustTier,
    /// Advisory explanation string.
    pub why: Option<String>,
}

/// Full `gap.resolve` output matching `spec/02-gap-ql.md` §3 and `spec/04-routing.md` §§5–10.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailedResolution {
    /// High-level discovery outcome (`Resolved`, `Ambiguous`, `Abstained`, `Degraded`).
    pub outcome: DiscoveryOutcome,
    /// Ranked candidate list (always empty iff `Abstained`).
    pub candidates: Vec<ResolveCandidate>,
    /// Abstention reason when `outcome == Abstained`.
    pub abstention_reason: Option<AbstentionReason>,
    /// Calibration table SHA-256 identifier when active and valid (`None` when uncalibrated/invalidated).
    pub calibration_id: Option<String>,
    /// Calibration mode actually used (`Conformal`, `Fallback`, or `InvalidatedBaseline`).
    pub calibration_mode: CalibrationMode,
    /// Active router identifier (`"baseline"` or `"s1:<model_id>"`).
    pub router: String,
    /// Estimated token cost (`GQL-022`).
    pub tokens_estimate: u32,
    /// Structured warnings (e.g. `"GAP_DISCOVERY_DEGRADED"`, `"GAP_CALIBRATION_INVALID"`).
    pub warnings: Vec<String>,
    /// Recorded unmapped-domain event on `NO_MATCH` / `LOW_CONFIDENCE` abstention (`RTE-040`).
    pub unmapped_event: Option<UnmappedDomainEvent>,
}

impl DetailedResolution {
    /// Converts this rich discovery result into the cross-layer [`DiscoveryResolution`] summary.
    #[must_use]
    pub fn into_core_resolution(self) -> DiscoveryResolution {
        let degraded = self
            .warnings
            .iter()
            .any(|w| w == "GAP_DISCOVERY_DEGRADED" || w == "GAP_CALIBRATION_INVALID");
        let outcome = if degraded && !self.candidates.is_empty() {
            DiscoveryOutcome::Degraded
        } else {
            self.outcome
        };
        let candidates = self
            .candidates
            .into_iter()
            .map(|c| DiscoveryCandidate {
                element_id: c.operation_id,
                kind: ElementKind::Operation,
                score: c.score,
                synopsis: c.skeleton,
                sdl: if c.sdl.is_empty() { None } else { Some(c.sdl) },
            })
            .collect();
        DiscoveryResolution {
            outcome,
            candidates,
            tokens_estimate: self.tokens_estimate,
            router: self.router,
        }
    }
}

/// Request arguments for implicit root field `__search` (`spec/02-gap-ql.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchRequest {
    /// Natural-language or identifier search query (`RTE-020`).
    pub query: String,
    /// Maximum hits to return (`default 5`, `GQL-020`).
    pub first: usize,
    /// Optional filter by [`ElementKind`].
    pub kinds: Vec<ElementKind>,
    /// Optional filter by branch slugs.
    pub branches: Vec<BranchId>,
    /// Optional progressive-disclosure SDL token budget (`GQL-021`).
    pub budget_tokens: Option<u32>,
}

/// Single visible element returned by `__search` (`spec/02-gap-ql.md` §4).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHitView {
    /// Deterministic RRF score in `[0, 1]`.
    pub score: f32,
    /// Element classification.
    pub kind: ElementKind,
    /// Short element name.
    pub name: String,
    /// Graph path / coordinate string.
    pub path: String,
    /// Owning branch slug.
    pub branch: BranchId,
    /// Capability group identifier when operation-scoped.
    pub group: Option<GroupId>,
    /// Sanitized element description.
    pub description: String,
    /// Self-contained minimal transitive SDL fragment (`GQL-021`).
    pub sdl: String,
    /// Effective side-effect classification when applicable (`GQL-032`).
    pub side_effect: Option<SideEffectLevel>,
    /// Owning branch trust tier.
    pub trust: TrustTier,
}

/// Result returned by `__search` (`spec/02-gap-ql.md` §4; `GQL-020`, `GQL-021`, `RTE-034`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResponse {
    /// Ranked visible hits (`<= first`).
    pub hits: Vec<SearchHitView>,
    /// Estimated token cost via `ceil(utf8_bytes / 3.6)` (`GQL-022`).
    pub tokens_estimate: u32,
    /// `true` iff no visible hit meets `retrieval.min_score` (`0.15`, `GQL-020`).
    pub abstained: bool,
}

/// Branch summary entry in `__metaGraph` (`spec/02-gap-ql.md` §3–4, `GQL-023`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaBranchView {
    /// Branch slug identifier.
    pub id: BranchId,
    /// Upstream protocol (`SQL`, `REST`, `GRAPHQL`, ...).
    pub protocol: gap_core::snapshot::Protocol,
    /// Branch trust tier (`T0`, `T1`, `T2`).
    pub trust: TrustTier,
    /// Administrative lifecycle status.
    pub status: BranchStatus,
    /// Sanitized progressive-disclosure synopsis truncated to `<= 90` bytes (`GQL-023`).
    pub synopsis: String,
    /// Node categories exposed by this branch (if code/doc graph).
    pub node_categories: Vec<String>,
    /// Edge predicates exposed by this branch (if code/doc graph).
    pub edge_predicates: Vec<String>,
    /// Number of caller-visible operations in this branch (`INV-05`).
    pub operation_count: u32,
    /// Number of caller-visible read-only (`NONE`) operations.
    pub read_only_count: u32,
    /// Number of caller-visible mutating operations.
    pub mutating_count: u32,
}

/// Result returned by `__metaGraph` / `Query.metaGraph` (`GQL-023`, `INV-05`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaGraphResponse {
    /// Snapshot generation (`INV-01`).
    pub generation: u64,
    /// Up to `limits.metagraph_branches` (default `100`) visible branches sorted by `operation_count` desc, `id` asc.
    pub branches: Vec<MetaBranchView>,
    /// Count of distinct caller-visible capability groups (`INV-05`).
    pub group_count: u32,
    /// Estimated token cost (`GQL-022`).
    pub tokens_estimate: u32,
}

/// Discovery plane coordinator unifying `__metaGraph`, `__search`, and `gap.resolve`
/// (`spec/04-routing.md`, `INV-05`, `INV-17`, `WP-M2-05`).
pub struct RouteDiscoveryService<R: RoutingModel = BaselineRouter> {
    router: R,
    baseline: BaselineRouter,
    embedder: Option<Arc<dyn Embedder>>,
    config: DiscoveryConfig,
    visibility_factory: VisibilityFactory,
    unmapped_tracker: Mutex<UnmappedDomainTracker>,
}

impl<R: RoutingModel> std::fmt::Debug for RouteDiscoveryService<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteDiscoveryService")
            .field("router", &self.router.id())
            .field("has_embedder", &self.embedder.is_some())
            .field("config", &self.config)
            .finish()
    }
}

impl RouteDiscoveryService<BaselineRouter> {
    /// Constructs a baseline-only ML-free [`RouteDiscoveryService`] (`INV-17`).
    #[must_use]
    pub fn baseline(config: DiscoveryConfig) -> Self {
        Self::new(BaselineRouter::default(), None, config)
    }
}

impl<R: RoutingModel> RouteDiscoveryService<R> {
    /// Constructs a [`RouteDiscoveryService`] with permissive default visibility.
    #[must_use]
    pub fn new(router: R, embedder: Option<Arc<dyn Embedder>>, config: DiscoveryConfig) -> Self {
        Self {
            router,
            baseline: BaselineRouter::default(),
            embedder,
            config,
            visibility_factory: Arc::new(|_| Box::new(AllowAllVisibility::default())),
            unmapped_tracker: Mutex::new(UnmappedDomainTracker::new()),
        }
    }

    /// Attaches a principal-aware visibility policy factory (`INV-05`, `GQL-062`).
    #[must_use]
    pub fn with_visibility_factory(mut self, factory: VisibilityFactory) -> Self {
        self.visibility_factory = factory;
        self
    }

    /// Snapshots all currently coalesced unmapped-domain events and dropped counter (`RTE-101`).
    #[must_use]
    pub fn unmapped_dropped_total(&self) -> u64 {
        self.lock_tracker().dropped_total()
    }

    fn lock_tracker(&self) -> std::sync::MutexGuard<'_, UnmappedDomainTracker> {
        match self.unmapped_tracker.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Evaluates L0 `__metaGraph` over visible snapshot state (`GQL-023`, `INV-05`).
    #[must_use]
    pub fn meta_graph(&self, ctx: &RequestContext) -> MetaGraphResponse {
        let vis = (self.visibility_factory)(&ctx.principal);
        meta_graph_snapshot(
            ctx.snapshot.as_ref(),
            vis.as_ref(),
            self.config.metagraph_branches,
        )
    }

    /// Evaluates deterministic L1 `__search` over visible elements (`GQL-020`, `GQL-021`, `RTE-034`, `INV-05`).
    pub fn search<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: SearchRequest,
    ) -> BoxFuture<'a, GapResult<SearchResponse>> {
        let vis = (self.visibility_factory)(&ctx.principal);
        self.search_with_visibility(ctx, req, vis)
    }

    /// Evaluates deterministic L1 `__search` using an explicit [`Visibility`] predicate (`INV-05`).
    pub fn search_with_visibility<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: SearchRequest,
        visibility: Box<dyn Visibility + 'a>,
    ) -> BoxFuture<'a, GapResult<SearchResponse>> {
        Box::pin(async move {
            let clean = sanitize_default(&req.query);
            let query_embedding = self.maybe_embed_query(&clean).await;
            search_snapshot(
                ctx,
                &req,
                query_embedding.as_deref(),
                visibility.as_ref(),
                self.config.min_score,
            )
        })
    }

    /// Executes full `gap.resolve` discovery pipeline (`spec/04-routing.md` §5, `RTE-020`..`RTE-067`).
    pub fn resolve_detailed<'a>(
        &'a self,
        ctx: &'a RequestContext,
        constraints: ResolveConstraints,
        intent: &'a str,
    ) -> BoxFuture<'a, GapResult<DetailedResolution>> {
        let vis = (self.visibility_factory)(&ctx.principal);
        self.resolve_detailed_with_visibility(ctx, constraints, intent, vis)
    }

    /// Executes `gap.resolve` against an explicit caller [`Visibility`] predicate (`INV-05`).
    pub fn resolve_detailed_with_visibility<'a>(
        &'a self,
        ctx: &'a RequestContext,
        constraints: ResolveConstraints,
        intent: &'a str,
        visibility: Box<dyn Visibility + 'a>,
    ) -> BoxFuture<'a, GapResult<DetailedResolution>> {
        Box::pin(async move {
            let snapshot = ctx.snapshot.as_ref();
            let sanitized = SanitizedQuery::new(intent);
            let max_candidates = constraints.max_candidates.clamp(1, 10);

            let branch_filter: Option<BTreeSet<BranchId>> = if constraints.branches.is_empty() {
                None
            } else {
                Some(constraints.branches.iter().cloned().collect())
            };

            // Enforce RTE-011 before retrieval: check if any visible operation survives
            // active branch status, visibility, and side-effect constraints.
            if !has_any_visible_eligible_operation(
                snapshot,
                branch_filter.as_ref(),
                constraints.max_side_effect,
                visibility.as_ref(),
            ) {
                return Ok(DetailedResolution {
                    outcome: DiscoveryOutcome::Abstained,
                    candidates: Vec::new(),
                    abstention_reason: Some(AbstentionReason::EmptyVisibleSet),
                    calibration_id: None,
                    calibration_mode: CalibrationMode::Fallback,
                    router: self.router.id().to_owned(),
                    tokens_estimate: 0,
                    warnings: Vec::new(),
                    unmapped_event: None,
                });
            }

            let query_emb = self.maybe_embed_query(sanitized.as_str()).await;
            let raw_hits = snapshot.index.search(
                &ctx.tenant_scope(),
                sanitized.as_str(),
                query_emb.as_deref(),
                64,
            )?;

            let primary_is_s1 = self.router.id() != "baseline";
            let (mut resolved_groups, options) = build_group_options(
                snapshot,
                &raw_hits,
                branch_filter.as_ref(),
                constraints.max_side_effect,
                self.config.top_k_groups,
                primary_is_s1,
                visibility.as_ref(),
                0.0,
            );

            if resolved_groups.is_empty() {
                let unmapped =
                    self.record_unmapped(ctx, intent, Vec::new(), AbstentionReason::NoMatch);
                return Ok(DetailedResolution {
                    outcome: DiscoveryOutcome::Abstained,
                    candidates: Vec::new(),
                    abstention_reason: Some(AbstentionReason::NoMatch),
                    calibration_id: None,
                    calibration_mode: CalibrationMode::Fallback,
                    router: self.router.id().to_owned(),
                    tokens_estimate: 0,
                    warnings: Vec::new(),
                    unmapped_event: unmapped,
                });
            }

            let mut warnings = Vec::new();
            let mut active_router_id = self.router.id().to_owned();

            // Verify calibration table compatibility before running S1 (`RTE-067`).
            let expected_s1_key = CalibrationVersionKey {
                router_model_id: self.router.id().to_owned(),
                embedding_model_id: snapshot.index.embedding_model_id().map(str::to_owned),
                option_format_version: OPTION_FORMAT_VERSION,
            };
            let expected_baseline_key = CalibrationVersionKey {
                router_model_id: "baseline".to_owned(),
                embedding_model_id: snapshot.index.embedding_model_id().map(str::to_owned),
                option_format_version: OPTION_FORMAT_VERSION,
            };

            let mut use_baseline_override = false;
            if primary_is_s1 && self.config.enforce_calibration_for_s1 {
                match snapshot.calibration.as_deref() {
                    Some(cal) if is_snapshot_calibration_valid(cal, &expected_s1_key) => {}
                    _ => {
                        use_baseline_override = true;
                        active_router_id = "baseline".to_owned();
                        warnings.push("GAP_CALIBRATION_INVALID".to_owned());
                    }
                }
            }

            let rank_ctx = RankCtx {
                max_side_effect: constraints.max_side_effect,
                request_id: ctx.request_id.to_string(),
                timeout_ms: self.config.s1_timeout_ms,
            };

            let rank_output = if use_baseline_override {
                self.baseline.rank(&sanitized, &options, &rank_ctx).await?
            } else {
                match self.router.rank(&sanitized, &options, &rank_ctx).await {
                    Ok(out) if out.logits.len() == options.len() => out,
                    Ok(_) | Err(_) => {
                        active_router_id = "baseline".to_owned();
                        warnings.push("GAP_DISCOVERY_DEGRADED".to_owned());
                        self.baseline.rank(&sanitized, &options, &rank_ctx).await?
                    }
                }
            };

            let bucket = OptionsBucket::from_option_count(options.len());
            let has_mutating = resolved_groups
                .iter()
                .any(|g| side_effect_rank(g.effective_side_effect) > 0);
            let alpha = alpha_for_side_effect_constraint(has_mutating);

            let active_key = if active_router_id == "baseline" {
                &expected_baseline_key
            } else {
                &expected_s1_key
            };

            let (temp, q_hat, calibration_id, cal_mode_override) =
                match snapshot.calibration.as_deref() {
                    Some(cal) if is_snapshot_calibration_valid(cal, active_key) => {
                        let (t, q) = lookup_calibration_params(cal, bucket, alpha);
                        (t, q, Some(snapshot_calibration_id(cal)), None)
                    }
                    Some(_) => (1.0, None, None, Some(CalibrationMode::InvalidatedBaseline)),
                    None => (1.0, None, None, None),
                };

            let probs = softmax_scaled(&rank_output.logits, temp);
            let mut decision = conformal_decide(&probs, &options, bucket, q_hat, max_candidates);
            if let Some(over_mode) = cal_mode_override {
                decision.mode = over_mode;
            }
            if decision.truncated {
                warnings.push("GAP_CANDIDATES_TRUNCATED".to_owned());
            }
            if matches!(decision.mode, CalibrationMode::Fallback) {
                warnings.push("GAP_CALIBRATION_FALLBACK".to_owned());
            }

            let projected = project_visible_schema(
                Valid::assume_valid_ref(snapshot.schema.as_ref()),
                visibility.as_ref(),
            );

            let mut candidates = Vec::with_capacity(decision.selected_indices.len());
            let mut total_bytes = 0usize;
            let byte_budget = constraints
                .budget_tokens
                .map(|b| (f64::from(b) * 3.6).floor() as usize);

            for &opt_idx in &decision.selected_indices {
                let Some(group) = resolved_groups.get_mut(opt_idx) else {
                    continue;
                };
                let prob = probs.get(opt_idx).copied().unwrap_or(0.0);
                let built = build_candidate_skeleton(
                    Valid::assume_valid_ref(snapshot.schema.as_ref()),
                    &group.primary_op,
                    visibility.as_ref(),
                );
                let (skeleton, variables_schema_json, mut field_sdl) = match built {
                    Ok(sk) => {
                        let ret_inner = find_operation_return_type(&projected, &group.primary_op)
                            .unwrap_or_default();
                        let sdl = if ret_inner.is_empty() {
                            sk.field_sdl.clone()
                        } else {
                            let rem = byte_budget
                                .map(|b| b.saturating_sub(total_bytes))
                                .unwrap_or(usize::MAX);
                            build_transitive_sdl_fragment(
                                &projected,
                                &sk.field_sdl,
                                &ret_inner,
                                rem,
                            )
                        };
                        (sk.skeleton, sk.variables_schema_json, sdl)
                    }
                    Err(_) => {
                        // Do not emit invalid GraphQL skeletons (`GQL-012`); skip unvalidatable ops.
                        continue;
                    }
                };

                if let Some(limit) = byte_budget
                    && !candidates.is_empty()
                    && total_bytes.saturating_add(skeleton.len() + field_sdl.len()) > limit
                {
                    let rem = limit.saturating_sub(total_bytes);
                    if rem < 64 {
                        break;
                    }
                    field_sdl = truncate_sdl_with_marker(&field_sdl, "Query", rem);
                }

                total_bytes = total_bytes
                    .saturating_add(skeleton.len())
                    .saturating_add(variables_schema_json.len())
                    .saturating_add(field_sdl.len());

                candidates.push(ResolveCandidate {
                    group_id: group.group_id.clone(),
                    score: prob,
                    probability: (decision.mode == CalibrationMode::Conformal).then_some(prob),
                    operation_id: group.primary_op.id.as_str().to_owned(),
                    branch: group.primary_branch.clone(),
                    kind: op_kind_as_str(group.primary_op.kind).to_owned(),
                    skeleton,
                    variables_schema_json,
                    sdl: field_sdl,
                    side_effect: group.effective_side_effect,
                    trust: group.primary_trust,
                    why: Some(format!(
                        "router={active_router_id},prior={:.3},replicas={}",
                        group.prior, group.member_count
                    )),
                });
            }

            let (final_outcome, abstention_reason, unmapped_event) = if candidates.is_empty() {
                let reason = decision
                    .abstention_reason
                    .unwrap_or(AbstentionReason::LowConfidence);
                let top_pairs: Vec<(GroupId, f32)> = resolved_groups
                    .iter()
                    .take(5)
                    .enumerate()
                    .map(|(i, g)| (g.group_id.clone(), probs.get(i).copied().unwrap_or(g.prior)))
                    .collect();
                let ev = self.record_unmapped(ctx, intent, top_pairs, reason);
                (DiscoveryOutcome::Abstained, Some(reason), ev)
            } else {
                (decision.outcome, None, None)
            };

            Ok(DetailedResolution {
                outcome: final_outcome,
                candidates,
                abstention_reason,
                calibration_id,
                calibration_mode: decision.mode,
                router: active_router_id,
                tokens_estimate: estimate_tokens(total_bytes),
                warnings,
                unmapped_event,
            })
        })
    }

    async fn maybe_embed_query(&self, sanitized_query: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?;
        let texts = [sanitized_query];
        match embedder.embed(&texts).await {
            Ok(mut vecs) if vecs.len() == 1 => vecs.pop(),
            _ => None,
        }
    }

    fn record_unmapped(
        &self,
        ctx: &RequestContext,
        raw_query: &str,
        top_groups: Vec<(GroupId, f32)>,
        reason: AbstentionReason,
    ) -> Option<UnmappedDomainEvent> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let p_class = principal_class_digest(&ctx.principal);
        let mut tracker = self.lock_tracker();
        tracker.record(
            &ctx.tenant,
            &p_class,
            raw_query,
            ctx.snapshot.generation,
            now_secs,
            top_groups,
            reason,
            self.config.privacy,
        )
    }
}

impl<R: RoutingModel> DiscoveryService for RouteDiscoveryService<R> {
    fn resolve<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: DiscoveryRequest,
    ) -> BoxFuture<'a, GapResult<DiscoveryResolution>> {
        Box::pin(async move {
            let constraints = ResolveConstraints {
                max_candidates: if req.top_k == 0 {
                    self.config.default_max_candidates
                } else {
                    req.top_k.clamp(1, 10)
                },
                ..ResolveConstraints::default()
            };
            let detailed = self.resolve_detailed(ctx, constraints, &req.intent).await?;
            Ok(detailed.into_core_resolution())
        })
    }
}

/// Evaluates deterministic L0 `__metaGraph` directly against `snapshot` and caller `visibility`
/// (`GQL-023`, `INV-05`, `GQL-011`).
#[must_use]
pub fn meta_graph_snapshot(
    snapshot: &Snapshot,
    visibility: &dyn Visibility,
    max_branches: usize,
) -> MetaGraphResponse {
    let cap = if max_branches == 0 {
        DEFAULT_METAGRAPH_MAX_BRANCHES
    } else {
        max_branches
    };

    let mut entries: Vec<MetaBranchView> = Vec::new();
    let mut visible_groups: BTreeSet<GroupId> = BTreeSet::new();

    for branch in snapshot.branches.values() {
        let mut op_count = 0u32;
        let mut read_only = 0u32;

        if branch.operations.is_empty() {
            if !visibility.can_see_type(branch.id.as_str()) {
                continue;
            }
        } else {
            for op in &branch.operations {
                if !is_operation_visible(op, visibility) {
                    continue;
                }
                op_count = op_count.saturating_add(1);
                let eff = effective_side_effect(branch.trust, op.side_effect.level);
                if matches!(eff, SideEffectLevel::None) {
                    read_only = read_only.saturating_add(1);
                }
                visible_groups.insert(op.group.clone());
            }
            if op_count == 0 {
                continue;
            }
        }

        let synopsis = truncate_utf8_bytes(
            &sanitize_default(&branch.synopsis),
            MAX_METAGRAPH_SYNOPSIS_BYTES,
        );
        let mutating = op_count.saturating_sub(read_only);
        entries.push(MetaBranchView {
            id: branch.id.clone(),
            protocol: branch.protocol,
            trust: branch.trust,
            status: branch.status,
            synopsis,
            node_categories: Vec::new(),
            edge_predicates: Vec::new(),
            operation_count: op_count,
            read_only_count: read_only,
            mutating_count: mutating,
        });
    }

    // `GQL-023`: ordered by operationCount desc, then id asc.
    entries.sort_by(|a, b| {
        b.operation_count
            .cmp(&a.operation_count)
            .then_with(|| a.id.cmp(&b.id))
    });
    entries.truncate(cap);

    let total_bytes: usize = entries
        .iter()
        .map(|b| b.id.as_str().len() + b.synopsis.len() + 48)
        .sum();

    MetaGraphResponse {
        generation: snapshot.generation,
        branches: entries,
        group_count: visible_groups.len() as u32,
        tokens_estimate: estimate_tokens(total_bytes),
    }
}

/// Evaluates deterministic L1 `__search` directly against `ctx.snapshot.index`, visibility
/// filtering BEFORE truncation/counts and abstaining cleanly below `min_score` (`GQL-020`,
/// `GQL-021`, `GQL-022`, `RTE-034`, `INV-05`).
///
/// # Errors
///
/// Propagates index lookup errors from [`gap_core::traits::ElementSearch::search`].
pub fn search_snapshot(
    ctx: &RequestContext,
    req: &SearchRequest,
    query_embedding: Option<&[f32]>,
    visibility: &dyn Visibility,
    min_score: f32,
) -> GapResult<SearchResponse> {
    let snapshot = ctx.snapshot.as_ref();
    let sanitized = sanitize_default(&req.query);
    let first = if req.first == 0 { 5 } else { req.first };
    let branch_filter: Option<BTreeSet<BranchId>> = if req.branches.is_empty() {
        None
    } else {
        Some(req.branches.iter().cloned().collect())
    };
    let kind_filter: Option<BTreeSet<ElementKind>> = if req.kinds.is_empty() {
        None
    } else {
        Some(req.kinds.iter().copied().collect())
    };

    let fetch_limit = snapshot
        .index
        .element_count()
        .clamp(first.saturating_mul(8), 512);
    let raw_hits = snapshot.index.search(
        &ctx.tenant_scope(),
        &sanitized,
        query_embedding,
        fetch_limit,
    )?;

    let projected = project_visible_schema(
        Valid::assume_valid_ref(snapshot.schema.as_ref()),
        visibility,
    );
    let max_total_bytes = req
        .budget_tokens
        .map(|b| (f64::from(b) * 3.6).floor() as usize);

    let mut views: Vec<SearchHitView> = Vec::with_capacity(first);
    let mut rendered_bytes = 0usize;

    for hit in raw_hits {
        if views.len() >= first {
            break;
        }
        if let Some(ref kf) = kind_filter
            && !kf.contains(&hit.kind)
        {
            continue;
        }

        let Some(entry) = resolve_visible_search_hit(
            snapshot,
            &projected,
            &hit,
            branch_filter.as_ref(),
            visibility,
        ) else {
            continue;
        };

        let remaining = max_total_bytes.map(|cap| cap.saturating_sub(rendered_bytes));
        let sdl = match remaining {
            Some(0) => break,
            Some(rem) => build_transitive_sdl_fragment(
                &projected,
                &entry.base_sdl,
                &entry.root_type_for_sdl,
                rem,
            ),
            None => build_transitive_sdl_fragment(
                &projected,
                &entry.base_sdl,
                &entry.root_type_for_sdl,
                usize::MAX,
            ),
        };

        let hit_bytes = entry.name.len() + entry.path.len() + entry.description.len() + sdl.len();
        rendered_bytes = rendered_bytes.saturating_add(hit_bytes);

        views.push(SearchHitView {
            score: hit.fused_score,
            kind: hit.kind,
            name: entry.name,
            path: entry.path,
            branch: entry.branch,
            group: entry.group,
            description: entry.description,
            sdl,
            side_effect: entry.side_effect,
            trust: entry.trust,
        });
    }

    let threshold = if min_score.is_finite() {
        min_score
    } else {
        DEFAULT_RETRIEVAL_MIN_SCORE
    };
    let abstained = views.is_empty() || views.iter().all(|h| h.score < threshold);

    Ok(SearchResponse {
        hits: views,
        tokens_estimate: estimate_tokens(rendered_bytes),
        abstained,
    })
}

struct ResolvedHitEntry {
    name: String,
    path: String,
    branch: BranchId,
    group: Option<GroupId>,
    description: String,
    base_sdl: String,
    root_type_for_sdl: String,
    side_effect: Option<SideEffectLevel>,
    trust: TrustTier,
}

fn resolve_visible_search_hit(
    snapshot: &Snapshot,
    projected: &Valid<Schema>,
    hit: &SearchHit,
    branch_filter: Option<&BTreeSet<BranchId>>,
    visibility: &dyn Visibility,
) -> Option<ResolvedHitEntry> {
    let branch_opt = hit
        .branch_uid
        .and_then(|uid| snapshot.branches.get(&uid))
        .or_else(|| {
            hit.operation_id.as_ref().and_then(|op_id| {
                snapshot
                    .branches
                    .values()
                    .find(|b| b.id.as_str() == op_id.branch_str())
            })
        });

    let branch = branch_opt?;
    if branch.status != BranchStatus::Active {
        return None;
    }
    if let Some(bf) = branch_filter
        && !bf.contains(&branch.id)
    {
        return None;
    }

    match hit.kind {
        ElementKind::Operation | ElementKind::Example => {
            let op = find_op_on_branch(branch, hit)?;
            if !is_operation_visible(op, visibility) {
                return None;
            }
            let eff = effective_side_effect(branch.trust, op.side_effect.level);
            let ret_type = find_operation_return_type(projected, op).unwrap_or_default();
            let desc = sanitize_default(&build_sanitized_option_text(branch, op));
            let root = groups::root_type_for_op_kind(op.kind);
            Some(ResolvedHitEntry {
                name: op.id.name().to_owned(),
                path: format!("{root}.{}", op.id.name()),
                branch: branch.id.clone(),
                group: Some(op.group.clone()),
                description: desc,
                base_sdl: op.sdl.as_ref().to_owned(),
                root_type_for_sdl: ret_type,
                side_effect: Some(eff),
                trust: branch.trust,
            })
        }
        ElementKind::Type => {
            let type_name = hit
                .element_id
                .strip_prefix("type:")
                .unwrap_or(hit.element_id.as_str());
            if !visibility.can_see_type(type_name) {
                return None;
            }
            let ext = projected.types.get(type_name)?;
            Some(ResolvedHitEntry {
                name: type_name.to_owned(),
                path: type_name.to_owned(),
                branch: branch.id.clone(),
                group: None,
                description: sanitize_default(&branch.synopsis),
                base_sdl: ext.serialize().to_string(),
                root_type_for_sdl: type_name.to_owned(),
                side_effect: None,
                trust: branch.trust,
            })
        }
        ElementKind::Field => {
            let coord = hit
                .element_id
                .strip_prefix("field:")
                .unwrap_or(hit.element_id.as_str());
            let (parent_ty, field_name) = coord.split_once('.')?;
            if !visibility.can_see_type(parent_ty)
                || !visibility.can_see_field(parent_ty, field_name)
            {
                return None;
            }
            let fdef = projected.get_object(parent_ty)?.fields.get(field_name)?;
            Some(ResolvedHitEntry {
                name: field_name.to_owned(),
                path: coord.to_owned(),
                branch: branch.id.clone(),
                group: None,
                description: sanitize_default(&branch.synopsis),
                base_sdl: fdef.serialize().to_string(),
                root_type_for_sdl: fdef.ty.inner_named_type().as_str().to_owned(),
                side_effect: None,
                trust: branch.trust,
            })
        }
        ElementKind::NodeCategory => {
            if !visibility.can_see_type(&hit.element_id) {
                return None;
            }
            Some(ResolvedHitEntry {
                name: hit.element_id.clone(),
                path: hit.element_id.clone(),
                branch: branch.id.clone(),
                group: None,
                description: sanitize_default(&branch.synopsis),
                base_sdl: String::new(),
                root_type_for_sdl: String::new(),
                side_effect: None,
                trust: branch.trust,
            })
        }
    }
}

fn find_op_on_branch<'a>(
    branch: &'a BranchDescriptor,
    hit: &SearchHit,
) -> Option<&'a Arc<OperationDescriptor>> {
    if let Some(ref target_op) = hit.operation_id
        && let Some(op) = branch.operations.iter().find(|op| &op.id == target_op)
    {
        return Some(op);
    }
    branch
        .operations
        .iter()
        .find(|op| op.id.as_str() == hit.element_id || op.id.name() == hit.element_id)
}

fn find_operation_return_type(projected: &Schema, op: &OperationDescriptor) -> Option<String> {
    let root = groups::root_type_for_op_kind(op.kind);
    let field_def = projected.get_object(root)?.fields.get(op.id.name())?;
    Some(field_def.ty.inner_named_type().as_str().to_owned())
}

fn has_any_visible_eligible_operation(
    snapshot: &Snapshot,
    branch_filter: Option<&BTreeSet<BranchId>>,
    max_side_effect: SideEffectLevel,
    visibility: &dyn Visibility,
) -> bool {
    for branch in snapshot.branches.values() {
        if branch.status != BranchStatus::Active {
            continue;
        }
        if let Some(bf) = branch_filter
            && !bf.contains(&branch.id)
        {
            continue;
        }
        for op in &branch.operations {
            if !is_operation_visible(op, visibility) {
                continue;
            }
            let eff = effective_side_effect(branch.trust, op.side_effect.level);
            if side_effect_allows(max_side_effect, eff) {
                return true;
            }
        }
    }
    false
}

/// Builds a minimal self-contained transitive SDL snippet for an element (`GQL-021`), truncating
/// at `max_bytes` with `# truncated: use __type(name: "X")`.
#[must_use]
pub fn build_transitive_sdl_fragment(
    projected: &Schema,
    base_sdl: &str,
    entry_type_name: &str,
    max_bytes: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let trimmed_base = base_sdl.trim();
    if !trimmed_base.is_empty() {
        parts.push(trimmed_base.to_owned());
    }

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    if !entry_type_name.is_empty() && !is_builtin_scalar(entry_type_name) {
        queue.push_back(entry_type_name.to_owned());
    }

    let mut marker_type = if entry_type_name.is_empty() {
        "Query".to_owned()
    } else {
        entry_type_name.to_owned()
    };

    while let Some(ty_name) = queue.pop_front() {
        if !visited.insert(ty_name.clone())
            || is_builtin_scalar(&ty_name)
            || ty_name.starts_with("__")
        {
            continue;
        }
        let Some(ext) = projected.types.get(ty_name.as_str()) else {
            continue;
        };
        let rendered = ext.serialize().to_string();
        if !parts.iter().any(|p| p == &rendered) {
            parts.push(rendered);
        }
        if marker_type == "Query" {
            marker_type = ty_name.clone();
        }
        enqueue_referenced_types(projected, &ty_name, &mut queue);
    }

    let full = parts.join("\n");
    if full.len() <= max_bytes {
        full
    } else {
        truncate_sdl_with_marker(&full, &marker_type, max_bytes)
    }
}

fn enqueue_referenced_types(schema: &Schema, type_name: &str, queue: &mut VecDeque<String>) {
    if let Some(obj) = schema.get_object(type_name) {
        for (fname, fdef) in &obj.fields {
            if fname.as_str().starts_with("__") {
                continue;
            }
            let inner = fdef.ty.inner_named_type().as_str();
            if !is_builtin_scalar(inner) {
                queue.push_back(inner.to_owned());
            }
            for arg in &fdef.arguments {
                let arg_inner = arg.ty.inner_named_type().as_str();
                if !is_builtin_scalar(arg_inner) {
                    queue.push_back(arg_inner.to_owned());
                }
            }
        }
    } else if let Some(iface) = schema.get_interface(type_name) {
        for (fname, fdef) in &iface.fields {
            if fname.as_str().starts_with("__") {
                continue;
            }
            let inner = fdef.ty.inner_named_type().as_str();
            if !is_builtin_scalar(inner) {
                queue.push_back(inner.to_owned());
            }
        }
    } else if let Some(union_def) = schema.get_union(type_name) {
        for member in &union_def.members {
            let m = member.name.as_str();
            if !is_builtin_scalar(m) {
                queue.push_back(m.to_owned());
            }
        }
    } else if let Some(input_def) = schema.get_input_object(type_name) {
        for fdef in input_def.fields.values() {
            let inner = fdef.ty.inner_named_type().as_str();
            if !is_builtin_scalar(inner) {
                queue.push_back(inner.to_owned());
            }
        }
    }
}

fn truncate_sdl_with_marker(sdl: &str, type_name: &str, max_bytes: usize) -> String {
    let trailer = format!("\n# truncated: use __type(name: \"{type_name}\")");
    if max_bytes <= trailer.len() {
        return trailer.trim_start_matches('\n').to_owned();
    }
    let body_cap = max_bytes.saturating_sub(trailer.len());
    let prefix = truncate_utf8_bytes(sdl, body_cap);
    format!("{prefix}{trailer}")
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_owned()
}

fn is_builtin_scalar(name: &str) -> bool {
    matches!(name, "Int" | "Float" | "String" | "Boolean" | "ID")
}

fn principal_class_digest(principal: &Principal) -> String {
    let mut raw = format!("{:?}", principal.kind);
    for scope in &principal.scopes {
        raw.push('|');
        raw.push_str(scope);
    }
    gap_ql::sha256_hex(raw.as_bytes())
}
