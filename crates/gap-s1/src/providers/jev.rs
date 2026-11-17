//! TypeSafe AI `Jev` (`POST /v1/systemone`) adapter (`ADR-028`, `RTE-047`, `RTE-049`, `INV-15`).

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

/// Default TypeSafe Jev model name (`typesafe.ai/blog/introducing-system-one-models-and-jev`).
pub const JEV_DEFAULT_MODEL: &str = "jev-latest";

/// Configuration for [`JevBackend`] (`RTE-047`, `RTE-049`).
#[derive(Clone)]
pub struct JevConfig {
    /// Base URL or full `/v1/systemone` URL (e.g. `http://127.0.0.1:4000` or proxy URL).
    pub endpoint: String,
    /// Model identifier (`jev-latest` by default).
    pub model: String,
    /// Optional bearer credential (`SecretString` redacted in `Debug` / errors per `INV-15`).
    pub api_key: Option<SecretString>,
    /// Encoder/context length budget reported to `gap-route`.
    pub head_max_len: usize,
    /// Default request timeout (`RTE-044`).
    pub io_timeout: Duration,
    /// Pluggable HTTP/HTTPS transport.
    pub transport: Arc<dyn DecisionHttpTransport>,
}

impl fmt::Debug for JevConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JevConfig")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("head_max_len", &self.head_max_len)
            .field("io_timeout", &self.io_timeout)
            .finish()
    }
}

impl JevConfig {
    /// Builds a strict `JevConfig` blocking private/loopback/metadata IPs (`RTE-049`).
    #[must_use]
    pub fn strict(endpoint: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: JEV_DEFAULT_MODEL.to_owned(),
            api_key,
            head_max_len: DEFAULT_HEAD_MAX_LEN,
            io_timeout: crate::sidecar::DEFAULT_SIDECAR_IO_TIMEOUT,
            transport: Arc::new(TcpHttpDecisionTransport::strict()),
        }
    }

    /// Builds a loopback-permitted config for local gateways/tests (`INV-17`).
    #[must_use]
    pub fn loopback(endpoint: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            transport: Arc::new(TcpHttpDecisionTransport::loopback()),
            ..Self::strict(endpoint, api_key)
        }
    }
}

/// TypeSafe AI `Jev` `/v1/systemone` client implementing [`DecisionBackend`] and multi-question
/// (`choice`/`score`/`noul`) batch evaluation (`ADR-028`, `RTE-047`, `RTE-049`).
#[derive(Clone)]
pub struct JevBackend {
    config: JevConfig,
    post_url: String,
    model_id: String,
}

impl fmt::Debug for JevBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JevBackend")
            .field("config", &self.config)
            .field("post_url", &self.post_url)
            .field("model_id", &self.model_id)
            .finish()
    }
}

impl JevBackend {
    /// Constructs a validated [`JevBackend`].
    #[must_use]
    pub fn new(config: JevConfig) -> Self {
        let post_url = resolve_provider_url(&config.endpoint, "/v1/systemone");
        let model_id = format!("jev:{}", config.model);
        Self {
            config,
            post_url,
            model_id,
        }
    }

    /// Evaluates multiple typed `choice`, `score`, and `noul` questions against `state` (`RTE-047`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] on empty questions, egress rejection, HTTP error, timeout, or schema mismatch.
    pub fn evaluate_questions(
        &self,
        state_json_or_text: &str,
        questions: &BTreeMap<String, DecisionQuestion>,
        timeout: Duration,
    ) -> GapResult<DecisionBatchResponse> {
        let auth_header = self
            .config
            .api_key
            .as_ref()
            .map(|s| format!("Bearer {}", s.expose_secret()));
        let headers: Vec<(&str, &str)> = match auth_header.as_deref() {
            Some(v) => vec![("Authorization", v)],
            None => Vec::new(),
        };
        let safe_state = sanitize_default(state_json_or_text);
        dispatch_decision_batch(
            self.config.transport.as_ref(),
            &DecisionBatchParams {
                post_url: &self.post_url,
                headers: &headers,
                model: &self.config.model,
                input_field: "state",
                state_json_or_text: &safe_state,
                questions,
                timeout,
                provider_label: "Jev /v1/systemone",
            },
        )
    }
}

impl DecisionBackend for JevBackend {
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
            let batch = self.evaluate_questions(state_jcs, &questions, timeout)?;
            let Some(DecisionAnswer::Choice { probabilities, .. }) = batch.answers.get("route")
            else {
                return Err(GapError::new(
                    ErrorCode::DiscoveryDegraded,
                    "Jev /v1/systemone response missing `choice` answer for `route`",
                ));
            };
            align_choice_probabilities_to_logits(options, probabilities)
        })
    }
}
