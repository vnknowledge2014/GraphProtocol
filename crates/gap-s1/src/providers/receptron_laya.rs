//! `@receptron/laya` (`github.com/receptron/laya`) ONNX HTTP wire adapter and cache resolver
//! (`ADR-028`, `RTE-047`, `RTE-049`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::traits::BoxFuture;
use gap_route::s1_adapter::DecisionBackend;
use gap_route::{GroupOption, sanitize_default};

use crate::candle::sequence::DEFAULT_HEAD_MAX_LEN;
use crate::providers::http_client::{
    DecisionBatchParams, DecisionHttpTransport, TcpHttpDecisionTransport, build_routing_questions,
    dispatch_decision_batch, resolve_provider_url,
};
use crate::providers::types::{
    DecisionAnswer, DecisionBatchResponse, DecisionQuestion, align_choice_probabilities_to_logits,
};

/// Default model name reported by `@receptron/laya` ONNX bundles.
pub const RECEPTON_LAYA_DEFAULT_MODEL: &str = "convaiinnovations/laya";

/// Resolves `@receptron/laya`'s ONNX weight cache directory (`$LAYA_CACHE` or `~/.cache/receptron-laya`).
#[must_use]
pub fn resolve_receptron_laya_cache_dir(
    env_laya_cache: Option<&Path>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = env_laya_cache
        && !p.as_os_str().is_empty()
    {
        return Some(p.to_path_buf());
    }
    home_dir.map(|h| h.join(".cache").join("receptron-laya"))
}

/// Configuration for [`ReceptronLayaBackend`] (`RTE-047`, `RTE-049`).
#[derive(Debug, Clone)]
pub struct ReceptronLayaConfig {
    /// Local `@receptron/laya` bridge/sidecar URL (e.g. `http://127.0.0.1:4100`).
    pub endpoint: String,
    /// Model identifier (`convaiinnovations/laya` by default).
    pub model: String,
    /// Request path suffix on `endpoint` (`/systemOne` or `/v1/systemone`).
    pub path_suffix: String,
    /// Encoder max sequence length (`head_max_len`).
    pub head_max_len: usize,
    /// Inference timeout (`RTE-044`).
    pub io_timeout: Duration,
    /// Pluggable HTTP transport.
    pub transport: Arc<dyn DecisionHttpTransport>,
}

impl ReceptronLayaConfig {
    /// Builds a loopback `@receptron/laya` sidecar configuration (`http://127.0.0.1:...`).
    #[must_use]
    pub fn loopback(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: RECEPTON_LAYA_DEFAULT_MODEL.to_owned(),
            path_suffix: "/systemOne".to_owned(),
            head_max_len: DEFAULT_HEAD_MAX_LEN,
            io_timeout: crate::sidecar::DEFAULT_SIDECAR_IO_TIMEOUT,
            transport: Arc::new(TcpHttpDecisionTransport::loopback()),
        }
    }

    /// Builds a strict egress-guarded remote configuration (`RTE-049`).
    #[must_use]
    pub fn strict(endpoint: impl Into<String>) -> Self {
        Self {
            transport: Arc::new(TcpHttpDecisionTransport::strict()),
            ..Self::loopback(endpoint)
        }
    }
}

/// Client for `@receptron/laya` (`github.com/receptron/laya`) Node/ONNX or Python `system_one` services
/// implementing [`DecisionBackend`] (`ADR-028`, `RTE-047`).
#[derive(Debug, Clone)]
pub struct ReceptronLayaBackend {
    config: ReceptronLayaConfig,
    post_url: String,
    model_id: String,
}

impl ReceptronLayaBackend {
    /// Constructs a new [`ReceptronLayaBackend`].
    #[must_use]
    pub fn new(config: ReceptronLayaConfig) -> Self {
        let suffix = if config.path_suffix.starts_with('/') {
            config.path_suffix.as_str()
        } else {
            "/systemOne"
        };
        let post_url = resolve_provider_url(&config.endpoint, suffix);
        let model_id = format!("receptron-laya:{}", config.model);
        Self {
            config,
            post_url,
            model_id,
        }
    }

    /// Executes a `systemOne(state, questions)` multi-question pass (`RTE-047`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on empty questions, egress rejection, timeout, HTTP error, or JSON mismatch.
    pub fn system_one(
        &self,
        state_json_or_text: &str,
        questions: &BTreeMap<String, DecisionQuestion>,
        timeout: Duration,
    ) -> GapResult<DecisionBatchResponse> {
        let safe_state = sanitize_default(state_json_or_text);
        dispatch_decision_batch(
            self.config.transport.as_ref(),
            &DecisionBatchParams {
                post_url: &self.post_url,
                headers: &[],
                model: &self.config.model,
                input_field: "state",
                state_json_or_text: &safe_state,
                questions,
                timeout,
                provider_label: "receptron/laya systemOne",
            },
        )
    }
}

impl DecisionBackend for ReceptronLayaBackend {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn head_max_len(&self) -> usize {
        self.config.head_max_len
    }

    fn score_choice<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        let timeout = self.config.io_timeout;
        self.score_choice_with_timeout(state_jcs, question, options, timeout)
    }

    fn score_choice_with_timeout<'a>(
        &'a self,
        state_jcs: &'a str,
        question: &'a str,
        options: &'a [GroupOption],
        timeout: Duration,
    ) -> BoxFuture<'a, GapResult<Vec<f32>>> {
        Box::pin(async move {
            let questions = build_routing_questions(question, options);
            let batch = self.system_one(state_jcs, &questions, timeout)?;
            let Some(DecisionAnswer::Choice { probabilities, .. }) = batch.answers.get("route")
            else {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "receptron/laya response missing `choice` answer for `route`",
                ));
            };
            align_choice_probabilities_to_logits(options, probabilities)
        })
    }
}
