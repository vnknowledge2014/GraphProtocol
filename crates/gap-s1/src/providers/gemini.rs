//! Google `Gemini Decision 1` (`POST /v1beta/decisions`) zero-output-token adapter
//! (`ADR-028`, `RTE-048`, `RTE-049`, `INV-15`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::secret::SecretString;
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

/// Official Gemini Decisions API model identifier (`aistudio.google.com/docs/models/gemini-decision-1`).
pub const GEMINI_DECISION_1_DEFAULT_MODEL: &str = "gemini-decision-1";

/// Maximum independent questions accepted per shared input batch by Google's Decisions API (`RTE-048`).
pub const GEMINI_MAX_QUESTIONS_PER_CALL: usize = 1_000;

/// Configuration for [`GeminiDecisionBackend`] (`RTE-048`, `RTE-049`, `INV-15`).
#[derive(Clone)]
pub struct GeminiDecisionConfig {
    /// Base proxy/endpoint URL (e.g. `http://127.0.0.1:4200` for Envoy/proxy or custom TLS transport).
    pub endpoint: String,
    /// Model identifier (`"gemini-decision-1"` by default).
    pub model: String,
    /// Gemini API credential passed via `x-goog-api-key` (`SecretString`, redacted per `INV-15`).
    pub api_key: Option<SecretString>,
    /// Prompt truncation length budget reported to `gap-route`.
    pub head_max_len: usize,
    /// Request timeout (`RTE-044`).
    pub io_timeout: Duration,
    /// Pluggable HTTP/HTTPS transport.
    pub transport: Arc<dyn DecisionHttpTransport>,
}

impl fmt::Debug for GeminiDecisionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiDecisionConfig")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("head_max_len", &self.head_max_len)
            .field("io_timeout", &self.io_timeout)
            .finish()
    }
}

impl GeminiDecisionConfig {
    /// Builds a strict egress-guarded `GeminiDecisionConfig` (`RTE-049`).
    #[must_use]
    pub fn strict(endpoint: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: GEMINI_DECISION_1_DEFAULT_MODEL.to_owned(),
            api_key,
            head_max_len: DEFAULT_HEAD_MAX_LEN,
            io_timeout: crate::sidecar::DEFAULT_SIDECAR_IO_TIMEOUT,
            transport: Arc::new(TcpHttpDecisionTransport::strict()),
        }
    }

    /// Builds a loopback-permitted config for local proxy relays and integration tests.
    #[must_use]
    pub fn loopback(endpoint: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            transport: Arc::new(TcpHttpDecisionTransport::loopback()),
            ..Self::strict(endpoint, api_key)
        }
    }
}

/// Google `gemini-decision-1` (`POST /v1beta/decisions`) adapter implementing [`DecisionBackend`]
/// and multi-question batch evaluation with zero output tokens (`RTE-048`, `INV-15`, `INV-17`).
#[derive(Clone)]
pub struct GeminiDecisionBackend {
    config: GeminiDecisionConfig,
    post_url: String,
    model_id: String,
}

impl fmt::Debug for GeminiDecisionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiDecisionBackend")
            .field("config", &self.config)
            .field("post_url", &self.post_url)
            .field("model_id", &self.model_id)
            .finish()
    }
}

impl GeminiDecisionBackend {
    /// Constructs a validated [`GeminiDecisionBackend`].
    #[must_use]
    pub fn new(config: GeminiDecisionConfig) -> Self {
        let post_url = resolve_provider_url(&config.endpoint, "/v1beta/decisions");
        let model_id = format!("gemini:{}", config.model);
        Self {
            config,
            post_url,
            model_id,
        }
    }

    /// Evaluates up to `1,000` independent `choice`/`score`/`noul` questions over shared `input`
    /// in a single prefill pass with zero generated output tokens (`RTE-048`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if `questions` is empty or exceeds `1,000` entries, egress blocks the
    /// endpoint, or the response is invalid.
    pub fn create_decisions(
        &self,
        input_text: &str,
        questions: &BTreeMap<String, DecisionQuestion>,
        timeout: Duration,
    ) -> GapResult<DecisionBatchResponse> {
        if questions.len() > GEMINI_MAX_QUESTIONS_PER_CALL {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "gemini-decision-1 accepts at most {GEMINI_MAX_QUESTIONS_PER_CALL} questions per request (got {})",
                    questions.len()
                ),
            ));
        }
        let headers: Vec<(&str, &str)> = match self.config.api_key.as_ref() {
            Some(key) => vec![("x-goog-api-key", key.expose_secret())],
            None => Vec::new(),
        };
        let safe_input = sanitize_default(input_text);
        dispatch_decision_batch(
            self.config.transport.as_ref(),
            &DecisionBatchParams {
                post_url: &self.post_url,
                headers: &headers,
                model: &self.config.model,
                input_field: "input",
                state_json_or_text: &safe_input,
                questions,
                timeout,
                provider_label: "Gemini /v1beta/decisions",
            },
        )
    }
}

impl DecisionBackend for GeminiDecisionBackend {
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
            let batch = self.create_decisions(state_jcs, &questions, timeout)?;
            let Some(DecisionAnswer::Choice { probabilities, .. }) = batch.answers.get("route")
            else {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "Gemini /v1beta/decisions response missing `choice` answer for `route`",
                ));
            };
            align_choice_probabilities_to_logits(options, probabilities)
        })
    }
}
