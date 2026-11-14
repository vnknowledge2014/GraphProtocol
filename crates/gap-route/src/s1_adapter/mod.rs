//! Routing model contracts, `BaselineRouter`, and System-1 canonical prompt/state formatting,
//! bounded caching, and circuit-breaker fallback (`spec/04-routing.md` §6; `RTE-041`..`RTE-046`,
//! `RTE-063`, `INV-17`, `WP-M2-05`, `WP-M5-03`).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
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
/// - Non-sink options ordered by descending retrieval prior (`group_id` / slot key tie-break).
/// - Each option text capped to `max(8, floor(0.8 * head_max_len / n_options))` estimated tokens.
/// - Fixed sink option text enforced.
#[must_use]
pub fn format_s1_options(opts: &[GroupOption], head_max_len: usize) -> Vec<GroupOption> {
    let n_opts = opts.len().max(1);
    let max_tokens = ((0.8 * head_max_len as f64) / (n_opts as f64))
        .floor()
        .max(8.0) as usize;
    let max_bytes = max_tokens.saturating_mul(3).max(24);

    let mut non_sink: Vec<GroupOption> = opts
        .iter()
        .filter(|o| o.key != SINK_OPTION_KEY)
        .cloned()
        .collect();
    non_sink.sort_by(|a, b| cmp_f32_desc(a.prior, b.prior).then_with(|| a.key.cmp(&b.key)));

    let mut formatted: Vec<GroupOption> = non_sink
        .into_iter()
        .map(|mut opt| {
            let clean = sanitize_default(&opt.text);
            opt.text = truncate_utf8_bytes_clean(&clean, max_bytes);
            opt
        })
        .collect();

    if opts.iter().any(|o| o.key == SINK_OPTION_KEY) {
        formatted.push(GroupOption {
            key: SINK_OPTION_KEY.to_owned(),
            text: SINK_OPTION_TEXT.to_owned(),
            prior: opts
                .iter()
                .find(|o| o.key == SINK_OPTION_KEY)
                .map_or(0.0, |o| o.prior),
        });
    }
    formatted
}

fn truncate_utf8_bytes_clean(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_owned()
}

/// Configuration knobs for [`S1RouterAdapter`].
#[derive(Debug, Clone, PartialEq)]
pub struct S1AdapterConfig {
    /// Maximum concurrent forward passes (`RTE-045`).
    pub max_concurrency: usize,
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
        Self {
            max_concurrency: 4,
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

struct ConcurrencyPermit<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// [`RoutingModel`] adapter wrapping any [`DecisionBackend`] with `RTE-042` JCS formatting,
/// `RTE-043` optional debiasing, `RTE-044` rolling failure degradation, `RTE-045` concurrency cap,
/// and `RTE-046` TTL decision caching.
#[derive(Debug)]
pub struct S1RouterAdapter<B: DecisionBackend> {
    backend: B,
    router_id: String,
    config: S1AdapterConfig,
    in_flight: AtomicUsize,
    state: Mutex<AdapterState>,
}

impl<B: DecisionBackend> S1RouterAdapter<B> {
    /// Wraps `backend` using `config`.
    #[must_use]
    pub fn new(backend: B, config: S1AdapterConfig) -> Self {
        let router_id = format!("s1:{}", backend.model_id());
        Self {
            backend,
            router_id,
            config,
            in_flight: AtomicUsize::new(0),
            state: Mutex::new(AdapterState::default()),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AdapterState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn try_acquire_permit(&self) -> GapResult<ConcurrencyPermit<'_>> {
        let max = self.config.max_concurrency.max(1);
        let mut cur = self.in_flight.load(Ordering::Acquire);
        loop {
            if cur >= max {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "S1 concurrency limit reached",
                ));
            }
            match self.in_flight.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(ConcurrencyPermit {
                        counter: &self.in_flight,
                    });
                }
                Err(actual) => cur = actual,
            }
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
            let formatted_opts = format_s1_options(opts, self.backend.head_max_len());
            let state_jcs = format_s1_state_jcs(sanitized_query, ctx.max_side_effect);
            let cache_key = build_cache_key(self.backend.model_id(), &state_jcs, &formatted_opts);

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
                let ttl = Duration::from_secs(self.config.cache_ttl_secs);
                if let Some((cached_at, cached_logits)) = st.cache.get(&cache_key)
                    && started.saturating_duration_since(*cached_at) < ttl
                {
                    let aligned = align_logits_to_caller(opts, &formatted_opts, cached_logits);
                    return Ok(RankOutput {
                        logits: aligned,
                        latency: started.elapsed(),
                    });
                }
            }

            let _permit = self.try_acquire_permit()?;

            let forward_res = self
                .backend
                .score_choice(&state_jcs, S1_CHOICE_QUESTION, &formatted_opts)
                .await;

            let raw_formatted_logits = match forward_res {
                Ok(logits) if logits.len() == formatted_opts.len() => {
                    if self.config.debias && formatted_opts.len() > 1 {
                        let rev_opts = reverse_options(&formatted_opts);
                        match self
                            .backend
                            .score_choice(&state_jcs, S1_CHOICE_QUESTION, &rev_opts)
                            .await
                        {
                            Ok(rev_logits) if rev_logits.len() == rev_opts.len() => {
                                average_reversed_logits(&logits, &rev_logits)
                            }
                            _ => logits,
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
                if self.config.cache_max_entries > 0 {
                    while st.cache.len() >= self.config.cache_max_entries {
                        if let Some(evict) = st.cache_order.pop_front() {
                            st.cache.remove(&evict);
                        } else {
                            break;
                        }
                    }
                    st.cache
                        .insert(cache_key.clone(), (now, raw_formatted_logits.clone()));
                    st.cache_order.push_back(cache_key);
                }
            }

            let aligned = align_logits_to_caller(opts, &formatted_opts, &raw_formatted_logits);
            Ok(RankOutput {
                logits: aligned,
                latency: started.elapsed(),
            })
        })
    }
}

fn align_logits_to_caller(
    caller_opts: &[GroupOption],
    formatted_opts: &[GroupOption],
    formatted_logits: &[f32],
) -> Vec<f32> {
    let mut by_key: HashMap<&str, f32> = HashMap::with_capacity(formatted_opts.len());
    for (opt, &logit) in formatted_opts.iter().zip(formatted_logits.iter()) {
        by_key.insert(opt.key.as_str(), logit);
    }
    caller_opts
        .iter()
        .map(|opt| by_key.get(opt.key.as_str()).copied().unwrap_or(0.0))
        .collect()
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
