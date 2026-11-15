//! Routing model contracts, `BaselineRouter`, and System-1 canonical prompt/state formatting,
//! bounded queueing, caching, and circuit-breaker fallback (`spec/04-routing.md` §6; `RTE-041`..`RTE-046`,
//! `RTE-063`, `INV-17`, `WP-M2-05`, `WP-M5-03`).

pub mod semaphore;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::SideEffectLevel;
use gap_core::traits::BoxFuture;
use gap_ql::sha256_hex;

use crate::debias::{average_reversed_logits, reverse_options};
use crate::groups::{
    GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT, cmp_f32_desc, escape_json_str,
    side_effect_as_str,
};
use crate::sanitize::{SanitizedQuery, sanitize_default};

pub use semaphore::{BoundedSemaphore, SemaphorePermit};

/// Default retrieval score temperature `tau` converting baseline priors `score` into logits `score / tau` (`RTE-063`).
pub const DEFAULT_BASELINE_TAU: f32 = 0.25;

/// Default baseline logit assigned to `insufficient_evidence` (`0.15 / 0.25 = 0.60`, matching `GQL-020` `0.15`).
pub const DEFAULT_BASELINE_SINK_LOGIT: f32 = 0.60;

/// S1 option prompt format version pinned in calibration version keys (`RTE-067`).
pub const OPTION_FORMAT_VERSION: u32 = 1;

/// Standard `RTE-042` choice question passed to S1 `DecisionBackend`s.
pub const S1_CHOICE_QUESTION: &str =
    "Select the single capability group whose schema operations fulfill the query.";

/// Contextual inputs supplied to [`RoutingModel::rank`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankCtx {
    /// Maximum side-effect constraint from caller request (`RTE-042`).
    pub max_side_effect: SideEffectLevel,
    /// Request correlation identifier for tracing.
    pub request_id: String,
    /// Model inference timeout budget in milliseconds (`RTE-044`, default `400`).
    pub timeout_ms: u64,
}

impl Default for RankCtx {
    fn default() -> Self {
        Self {
            max_side_effect: SideEffectLevel::None,
            request_id: String::new(),
            timeout_ms: 400,
        }
    }
}

/// Raw uncalibrated logits returned by a [`RoutingModel`] aligned 1:1 with `opts` (`RTE-041`).
#[derive(Debug, Clone, PartialEq)]
pub struct RankOutput {
    /// Raw logits corresponding to each [`GroupOption`] in caller-supplied order.
    pub logits: Vec<f32>,
    /// Measured forward/scoring latency.
    pub latency: Duration,
}

/// High-level discovery routing model contract (`spec/04-routing.md` §§1, 6; `RTE-041`).
pub trait RoutingModel: Send + Sync + fmt::Debug {
    /// Stable router identifier (`"baseline"` or `"s1:<model_id>"`).
    fn id(&self) -> &str;

    /// Computes raw logits aligned with `opts`; calibration is applied centrally by `gap-route` (`RTE-041`).
    fn rank<'a>(
        &'a self,
        sanitized_query: &'a SanitizedQuery,
        opts: &'a [GroupOption],
        ctx: &'a RankCtx,
    ) -> BoxFuture<'a, GapResult<RankOutput>>;
}

impl<M: RoutingModel + ?Sized> RoutingModel for Arc<M> {
    fn id(&self) -> &str {
        (**self).id()
    }

    fn rank<'a>(
        &'a self,
        sanitized_query: &'a SanitizedQuery,
        opts: &'a [GroupOption],
        ctx: &'a RankCtx,
    ) -> BoxFuture<'a, GapResult<RankOutput>> {
        (**self).rank(sanitized_query, opts, ctx)
    }
}

impl<M: RoutingModel + ?Sized> RoutingModel for Box<M> {
    fn id(&self) -> &str {
        (**self).id()
    }

    fn rank<'a>(
        &'a self,
        sanitized_query: &'a SanitizedQuery,
        opts: &'a [GroupOption],
        ctx: &'a RankCtx,
    ) -> BoxFuture<'a, GapResult<RankOutput>> {
        (**self).rank(sanitized_query, opts, ctx)
    }
}

/// Low-level System-1 inference contract declared in `gap-route` and implemented in `gap-s1`
/// (`spec/04-routing.md` §1, `RTE-042`).
pub trait DecisionBackend: Send + Sync + fmt::Debug {
    /// Model identifier including pinned weight digest (`RTE-002`, `RTE-042`).
    fn model_id(&self) -> &str;

    /// Encoder maximum sequence length (`head_max_len`, default `512` tokens).
    fn head_max_len(&self) -> usize {
        512
    }

    /// Evaluates one `choice` question returning raw logits aligned with `options` (`RTE-041`, `RTE-042`).
    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>>;

    /// Evaluates one `choice` question bounded by `timeout` (`RTE-044`).
    ///
    /// Default implementation delegates to [`Self::score_choice`]; async adapters may wrap with
    /// runtime timeout preemption (`TokioTimeoutBackend` in `gap-server`).
    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        _timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        self.score_choice(state_jcs, question, options)
    }
}

impl<B: DecisionBackend + ?Sized> DecisionBackend for Arc<B> {
    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    fn head_max_len(&self) -> usize {
        (**self).head_max_len()
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        (**self).score_choice(state_jcs, question, options)
    }

    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        (**self).score_choice_with_timeout(state_jcs, question, options, timeout)
    }
}

impl<B: DecisionBackend + ?Sized> DecisionBackend for Box<B> {
    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    fn head_max_len(&self) -> usize {
        (**self).head_max_len()
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        (**self).score_choice(state_jcs, question, options)
    }

    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        (**self).score_choice_with_timeout(state_jcs, question, options, timeout)
    }
}

/// Always-available ML-free baseline router mapping hybrid scores via `z = score / tau` (`RTE-063`, `INV-17`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaselineRouter {
    /// Score temperature divisor `tau > 0.0` (default `0.25`, `RTE-063`).
    pub tau: f32,
    /// Constant base logit for `insufficient_evidence` (`0.60` aligns `0.15` prior crossover).
    pub sink_base_logit: f32,
}

impl Default for BaselineRouter {
    fn default() -> Self {
        Self {
            tau: DEFAULT_BASELINE_TAU,
            sink_base_logit: DEFAULT_BASELINE_SINK_LOGIT,
        }
    }
}

impl BaselineRouter {
    /// Constructs a [`BaselineRouter`] with validated positive `tau`.
    #[must_use]
    pub const fn with_tau(tau: f32) -> Self {
        let safe_tau = if tau.is_finite() && tau > 0.0 {
            tau
        } else {
            DEFAULT_BASELINE_TAU
        };
        Self {
            tau: safe_tau,
            sink_base_logit: DEFAULT_BASELINE_SINK_LOGIT,
        }
    }
}

impl RoutingModel for BaselineRouter {
    fn id(&self) -> &'static str {
        "baseline"
    }

    fn rank<'a>(
        &'a self,
        _sanitized_query: &'a SanitizedQuery,
        opts: &'a [GroupOption],
        _ctx: &'a RankCtx,
    ) -> BoxFuture<'a, GapResult<RankOutput>> {
        Box::pin(async move {
            let started = Instant::now();
            let safe_tau = if self.tau.is_finite() && self.tau > 0.0 {
                self.tau
            } else {
                DEFAULT_BASELINE_TAU
            };
            let logits = opts
                .iter()
                .map(|opt| {
                    if opt.key == SINK_OPTION_KEY {
                        if opt.prior > 0.0 && opt.prior.is_finite() {
                            (opt.prior / safe_tau).max(self.sink_base_logit)
                        } else {
                            self.sink_base_logit
                        }
                    } else if opt.prior.is_finite() {
                        opt.prior.max(0.0) / safe_tau
                    } else {
                        0.0
                    }
                })
                .collect();
            Ok(RankOutput {
                logits,
                latency: started.elapsed(),
            })
        })
    }
}

/// Formats canonical RFC 8785 / JCS state JSON per `RTE-042`:
/// `{"constraints":{"max_side_effect":"<LEVEL>"},"query":"<sanitized>"}` (keys sorted lexicographically).
#[must_use]
pub fn format_s1_state_jcs(sanitized: &SanitizedQuery, max_side_effect: SideEffectLevel) -> String {
    let level = side_effect_as_str(max_side_effect);
    let query_escaped = escape_json_str(&sanitize_default(sanitized.as_str()));
    format!("{{\"constraints\":{{\"max_side_effect\":\"{level}\"}},\"query\":\"{query_escaped}\"}}")
}

/// Formats and truncates candidate options deterministically for S1 input per `RTE-042`:
/// - Non-sink options ordered by descending retrieval prior (`key` tie-break) and canonically re-keyed `"g0"`, `"g1"`, ...
/// - Each option text capped to `max(8, floor(0.8 * head_max_len / n_options))` tokens.
/// - Fixed sink option text enforced.
#[must_use]
pub fn format_s1_options(opts: &[GroupOption], head_max_len: usize) -> Vec<GroupOption> {
    format_s1_options_indexed(opts, head_max_len).0
}

fn format_s1_options_indexed(
    opts: &[GroupOption],
    head_max_len: usize,
) -> (Vec<GroupOption>, Vec<usize>) {
    let n_opts = opts.len().max(1);
    let max_tokens = ((0.8 * head_max_len as f64) / (n_opts as f64))
        .floor()
        .max(8.0) as usize;
    let max_bytes = max_tokens.saturating_mul(4).max(24);

    let mut non_sink: Vec<(usize, &GroupOption)> = opts
        .iter()
        .enumerate()
        .filter(|(_, o)| o.key != SINK_OPTION_KEY)
        .collect();
    non_sink.sort_by(|(idx_a, a), (idx_b, b)| {
        cmp_f32_desc(a.prior, b.prior)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| idx_a.cmp(idx_b))
    });

    let mut formatted = Vec::with_capacity(opts.len());
    let mut formatted_to_caller = Vec::with_capacity(opts.len());
    for (slot_idx, (caller_idx, opt)) in non_sink.into_iter().enumerate() {
        let clean = sanitize_default(&opt.text);
        formatted.push(GroupOption {
            key: format!("g{slot_idx}"),
            text: truncate_to_token_budget(&clean, max_tokens, max_bytes),
            prior: opt.prior,
        });
        formatted_to_caller.push(caller_idx);
    }

    if let Some((caller_idx, sink_opt)) = opts
        .iter()
        .enumerate()
        .find(|(_, o)| o.key == SINK_OPTION_KEY)
    {
        formatted.push(GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: sink_opt.prior,
        });
        formatted_to_caller.push(caller_idx);
    }

    (formatted, formatted_to_caller)
}

fn truncate_to_token_budget(s: &str, max_tokens: usize, max_bytes: usize) -> String {
    let mut words_seen = 0usize;
    let mut in_word = false;
    let mut cut_at = s.len();

    for (idx, ch) in s.char_indices() {
        if idx >= max_bytes {
            cut_at = idx;
            break;
        }
        if ch.is_whitespace() {
            if in_word {
                in_word = false;
            }
        } else if !in_word {
            if words_seen >= max_tokens {
                cut_at = idx;
                break;
            }
            words_seen += 1;
            in_word = true;
        }
    }

    s[..cut_at].trim_end().to_owned()
}

/// Configuration knobs for [`S1RouterAdapter`].
#[derive(Debug, Clone, PartialEq)]
pub struct S1AdapterConfig {
    /// Maximum concurrent forward passes (`RTE-045`, defaults to `(cores / 2).max(1)`).
    pub max_concurrency: usize,
    /// Maximum waiting callers in the bounded semaphore FIFO queue (`RTE-045`, defaults to `2 * max_concurrency` when `None`).
    pub max_queue_depth: Option<usize>,
    /// Enable second reversed-order pass averaging (`RTE-043`).
    pub debias: bool,
    /// Decision cache TTL in seconds (default `600`, `RTE-046`).
    pub cache_ttl_secs: u64,
    /// Maximum cached decision entries (`RTE-046`).
    pub cache_max_entries: usize,
    /// Rolling failure-rate threshold opening the fallback circuit (default `0.20`, `RTE-044`).
    pub failure_rate_threshold: f32,
    /// Fallback circuit open cooldown duration (default `60s`, `RTE-044`).
    pub circuit_cooldown_secs: u64,
}

impl Default for S1AdapterConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(8, std::num::NonZeroUsize::get);
        let max_concurrency = (cores / 2).max(1);
        Self {
            max_concurrency,
            max_queue_depth: None,
            debias: false,
            cache_ttl_secs: 600,
            cache_max_entries: 4_096,
            failure_rate_threshold: 0.20,
            circuit_cooldown_secs: 60,
        }
    }
}

#[derive(Debug, Default)]
struct AdapterState {
    cache: HashMap<String, (Instant, Vec<f32>)>,
    cache_order: VecDeque<String>,
    recent_outcomes: VecDeque<(Instant, bool)>,
    degraded_until: Option<Instant>,
}

/// [`RoutingModel`] adapter wrapping any [`DecisionBackend`] with `RTE-042` JCS formatting,
/// `RTE-043` optional debiasing, `RTE-044` timeout & rolling failure degradation, `RTE-045` bounded
/// concurrency semaphore (`<= 2 * max_concurrency` queue), and `RTE-046` TTL decision caching.
#[derive(Debug)]
pub struct S1RouterAdapter<B: DecisionBackend> {
    backend: B,
    router_id: String,
    config: S1AdapterConfig,
    semaphore: BoundedSemaphore,
    state: Mutex<AdapterState>,
}

impl<B: DecisionBackend> S1RouterAdapter<B> {
    /// Wraps `backend` using `config`.
    #[must_use]
    pub fn new(backend: B, config: S1AdapterConfig) -> Self {
        let router_id = format!("s1:{}", backend.model_id());
        let max_permits = config.max_concurrency.max(1);
        let max_waiters = config
            .max_queue_depth
            .unwrap_or_else(|| max_permits.saturating_mul(2));
        Self {
            backend,
            router_id,
            semaphore: BoundedSemaphore::new(max_permits, max_waiters),
            config,
            state: Mutex::new(AdapterState::default()),
        }
    }

    /// Borrows the underlying [`DecisionBackend`].
    #[must_use]
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Borrows the bounded FIFO semaphore governing concurrency and waiting queue depth (`RTE-045`).
    #[must_use]
    pub const fn semaphore(&self) -> &BoundedSemaphore {
        &self.semaphore
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AdapterState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn record_outcome(&self, now: Instant, ok: bool) {
        let mut st = self.lock_state();
        st.recent_outcomes.push_back((now, ok));
        while let Some(&(ts, _)) = st.recent_outcomes.front() {
            if now.saturating_duration_since(ts) > Duration::from_secs(60) {
                st.recent_outcomes.pop_front();
            } else {
                break;
            }
        }
        let total = st.recent_outcomes.len();
        if total >= 5 {
            let failures = st.recent_outcomes.iter().filter(|&&(_, s)| !s).count();
            let rate = (failures as f32) / (total as f32);
            if rate > self.config.failure_rate_threshold {
                st.degraded_until =
                    Some(now + Duration::from_secs(self.config.circuit_cooldown_secs));
            }
        }
    }
}

impl<B: DecisionBackend> RoutingModel for S1RouterAdapter<B> {
    fn id(&self) -> &str {
        &self.router_id
    }

    fn rank<'a>(
        &'a self,
        sanitized_query: &'a SanitizedQuery,
        opts: &'a [GroupOption],
        ctx: &'a RankCtx,
    ) -> BoxFuture<'a, GapResult<RankOutput>> {
        Box::pin(async move {
            let started = Instant::now();
            let (formatted_opts, formatted_to_caller) =
                format_s1_options_indexed(opts, self.backend.head_max_len());
            let state_jcs = format_s1_state_jcs(sanitized_query, ctx.max_side_effect);
            let cache_key = build_cache_key(self.backend.model_id(), &state_jcs, &formatted_opts);
            let ttl = Duration::from_secs(self.config.cache_ttl_secs);

            // Check cooldown circuit + TTL cache synchronously without holding mutex across `.await`.
            {
                let mut st = self.lock_state();
                if let Some(until) = st.degraded_until {
                    if started < until {
                        return Err(GapError::new(
                            ErrorCode::DiscoveryDegraded,
                            "S1 circuit cooldown active",
                        ));
                    }
                    st.degraded_until = None;
                }
                prune_expired_cache(&mut st, started, ttl);
                if let Some((cached_at, cached_logits)) = st.cache.get(&cache_key)
                    && started.saturating_duration_since(*cached_at) < ttl
                {
                    let aligned =
                        align_logits_to_caller(opts.len(), &formatted_to_caller, cached_logits);
                    return Ok(RankOutput {
                        logits: aligned,
                        latency: started.elapsed(),
                    });
                }
            }

            let timeout_budget = Duration::from_millis(ctx.timeout_ms.max(1));
            let _permit = self.semaphore.acquire().await?;

            let after_queue = started.elapsed();
            if after_queue >= timeout_budget {
                self.record_outcome(Instant::now(), false);
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    format!("S1 queue wait exceeded {}ms timeout budget", ctx.timeout_ms),
                ));
            }
            let remaining = timeout_budget.saturating_sub(after_queue);

            let forward_res = self
                .backend
                .score_choice_with_timeout(
                    &state_jcs,
                    S1_CHOICE_QUESTION,
                    &formatted_opts,
                    remaining,
                )
                .await;

            let raw_formatted_logits = match forward_res {
                Ok(logits) if logits.len() == formatted_opts.len() => {
                    if started.elapsed() > timeout_budget {
                        self.record_outcome(Instant::now(), false);
                        return Err(GapError::new(
                            ErrorCode::DiscoveryDegraded,
                            format!("S1 inference exceeded {}ms timeout budget", ctx.timeout_ms),
                        ));
                    }
                    if self.config.debias && formatted_opts.len() > 1 {
                        let debias_rem = timeout_budget.saturating_sub(started.elapsed());
                        if debias_rem.is_zero() {
                            logits
                        } else {
                            let rev_opts = reverse_options(&formatted_opts);
                            match self
                                .backend
                                .score_choice_with_timeout(
                                    &state_jcs,
                                    S1_CHOICE_QUESTION,
                                    &rev_opts,
                                    debias_rem,
                                )
                                .await
                            {
                                Ok(rev_logits)
                                    if rev_logits.len() == rev_opts.len()
                                        && started.elapsed() <= timeout_budget =>
                                {
                                    average_reversed_logits(&logits, &rev_logits)
                                }
                                _ => logits,
                            }
                        }
                    } else {
                        logits
                    }
                }
                Ok(_) => {
                    self.record_outcome(Instant::now(), false);
                    return Err(GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        "S1 backend returned mismatched logit vector length",
                    ));
                }
                Err(err) => {
                    self.record_outcome(Instant::now(), false);
                    return Err(err);
                }
            };

            let now = Instant::now();
            self.record_outcome(now, true);

            {
                let mut st = self.lock_state();
                if self.config.cache_max_entries > 0 && !ttl.is_zero() {
                    prune_expired_cache(&mut st, now, ttl);
                    insert_cache_entry(
                        &mut st,
                        cache_key,
                        now,
                        raw_formatted_logits.clone(),
                        self.config.cache_max_entries,
                    );
                }
            }

            let aligned =
                align_logits_to_caller(opts.len(), &formatted_to_caller, &raw_formatted_logits);
            Ok(RankOutput {
                logits: aligned,
                latency: started.elapsed(),
            })
        })
    }
}

fn prune_expired_cache(st: &mut AdapterState, now: Instant, ttl: Duration) {
    if ttl.is_zero() {
        st.cache.clear();
        st.cache_order.clear();
        return;
    }
    while let Some(front_key) = st.cache_order.front() {
        let is_expired = st
            .cache
            .get(front_key)
            .is_none_or(|(ts, _)| now.saturating_duration_since(*ts) >= ttl);
        if is_expired {
            if let Some(evicted) = st.cache_order.pop_front() {
                st.cache.remove(&evicted);
            }
        } else {
            break;
        }
    }
}

fn insert_cache_entry(
    st: &mut AdapterState,
    cache_key: String,
    now: Instant,
    logits: Vec<f32>,
    max_entries: usize,
) {
    if st.cache.contains_key(&cache_key) {
        st.cache_order.retain(|k| k != &cache_key);
    } else {
        while st.cache.len() >= max_entries {
            if let Some(evict) = st.cache_order.pop_front() {
                st.cache.remove(&evict);
            } else {
                break;
            }
        }
    }
    st.cache.insert(cache_key.clone(), (now, logits));
    st.cache_order.push_back(cache_key);
}

fn align_logits_to_caller(
    caller_len: usize,
    formatted_to_caller: &[usize],
    formatted_logits: &[f32],
) -> Vec<f32> {
    let mut aligned = vec![0.0f32; caller_len];
    for (&caller_idx, &logit) in formatted_to_caller.iter().zip(formatted_logits.iter()) {
        if let Some(slot) = aligned.get_mut(caller_idx) {
            *slot = logit;
        }
    }
    aligned
}

fn build_cache_key(model_id: &str, state_jcs: &str, opts: &[GroupOption]) -> String {
    let mut payload = String::with_capacity(model_id.len() + state_jcs.len() + opts.len() * 64);
    payload.push_str(model_id);
    payload.push('\n');
    payload.push_str(state_jcs);
    for opt in opts {
        payload.push('\n');
        payload.push_str(&opt.key);
        payload.push(':');
        payload.push_str(&opt.text);
    }
    sha256_hex(payload.as_bytes())
}

#[cfg(test)]
mod tests;
