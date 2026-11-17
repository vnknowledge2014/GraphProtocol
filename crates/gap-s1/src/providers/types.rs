//! Shared typed decision-model primitives (`Choice`, `Score`, `Noul`) and log-odds helpers
//! (`ADR-028`, `RTE-047`, `RTE-048`).

use std::collections::BTreeMap;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_route::GroupOption;

/// Supported single-pass non-generative question types across Laya, Jev, and Gemini Decision 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    /// Select exactly one mutually exclusive option from criteria.
    Choice,
    /// Rate input against an ordered rubric scale.
    Score,
    /// Binary calibrated yes/no predicate (`P(true)`).
    Noul,
}

impl QuestionKind {
    /// Wire `"type"` string used by `@receptron/laya`, `Jev`, and `gemini-decision-1`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Score => "score",
            Self::Noul => "noul",
        }
    }
}

/// Typed question submitted to a multi-head System-1 decision model (`RTE-047`, `RTE-048`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionQuestion {
    /// Select one option from `(key, description)` pairs.
    Choice {
        /// Question or selection prompt.
        instructions: String,
        /// Ordered option keys paired with criteria descriptions.
        criteria: Vec<(String, String)>,
    },
    /// Score against an ordered sequence of rubric labels (lowest to highest).
    Score {
        /// Scoring prompt.
        instructions: String,
        /// Ordered level descriptions.
        criteria: Vec<String>,
    },
    /// Binary yes/no evaluation returning calibrated probability `P(true)`.
    Noul {
        /// Predicate statement/question.
        instructions: String,
    },
}

impl DecisionQuestion {
    /// Returns the wire question kind.
    #[must_use]
    pub const fn kind(&self) -> QuestionKind {
        match self {
            Self::Choice { .. } => QuestionKind::Choice,
            Self::Score { .. } => QuestionKind::Score,
            Self::Noul { .. } => QuestionKind::Noul,
        }
    }
}

/// Parsed typed answer for one named decision question (`RTE-047`, `RTE-048`).
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionAnswer {
    /// Single-select choice answer with per-option probabilities.
    Choice {
        /// Selected option key.
        choice: String,
        /// Probability mass assigned to each option key.
        probabilities: Vec<(String, f32)>,
        /// Calibrated answer confidence (`max` probability or vendor `confidence`).
        confidence: f32,
    },
    /// Ordered rubric rating answer.
    Score {
        /// Probability-weighted expected score.
        score: f32,
        /// Optional level index legend (`"0"`, `"1"`, ...).
        legend: Vec<(String, String)>,
        /// Per-level probability distribution.
        probabilities: Vec<(String, f32)>,
        /// Model-reported confidence.
        confidence: f32,
    },
    /// Binary `noul` answer.
    Noul {
        /// Calibrated probability `P(true)` in `[0.0, 1.0]`.
        noul: f32,
        /// Confidence score (defaults to `max(p, 1 - p)` if omitted).
        confidence: f32,
    },
}

/// Aggregated response from a multi-question single-pass System-1 evaluation (`RTE-047`, `RTE-048`).
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionBatchResponse {
    /// Model identifier echoed by the backend.
    pub model: String,
    /// Map from question key to typed parsed answer.
    pub answers: BTreeMap<String, DecisionAnswer>,
    /// Input token count reported by provider (`0` if omitted).
    pub input_tokens: u64,
    /// Generated prose tokens (always `0` for non-generative Decision APIs).
    pub output_tokens: u64,
}

/// Converts a calibrated probability `p in [0.0, 1.0]` into raw log-odds `ln(clamp(p, 1e-6, 1.0))`
/// so `gap-route` can apply its own temperature scaling (`RTE-041`, `RTE-047`).
#[must_use]
pub fn prob_to_logit(p: f32) -> f32 {
    let clean = if p.is_finite() { p } else { 1e-6 };
    clean.clamp(1e-6_f32, 1.0_f32).ln()
}

/// Maps `(option_key, probability)` pairs aligned 1:1 with `options` into raw logits (`RTE-041`).
///
/// # Errors
///
/// Returns [`ErrorCode::DiscoveryDegraded`] if any option key is missing from `probabilities`
/// or carries a non-finite value.
pub fn align_choice_probabilities_to_logits(
    options: &[GroupOption],
    probabilities: &[(String, f32)],
) -> GapResult<Vec<f32>> {
    if options.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "cannot align probabilities for empty options slice",
        ));
    }
    let mut logits = Vec::with_capacity(options.len());
    for opt in options {
        let Some((_, prob)) = probabilities.iter().find(|(k, _)| k == &opt.key) else {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "decision provider response missing probability for option key `{}`",
                    opt.key
                ),
            ));
        };
        if !prob.is_finite() {
            return Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!(
                    "decision provider returned non-finite probability for `{}`",
                    opt.key
                ),
            ));
        }
        logits.push(prob_to_logit(*prob));
    }
    Ok(logits)
}
