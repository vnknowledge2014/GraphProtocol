//! Sequence construction for `LayaCandle` enforcing `RTE-020` input sanitization, `RTE-042`
//! fixed sink text, per-option `[MASK]` markers, and per-option token budgets (`WP-M5-02`).

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_route::{GroupOption, SINK_OPTION_KEY, SINK_OPTION_TEXT, sanitize_default};

use crate::candle::tokenizer::{CLS_ID, LayaTokenizer, MASK_ID, PAD_ID, SEP_ID};

/// Default ModernBERT context window for S1 classification (`RTE-042`).
pub const DEFAULT_HEAD_MAX_LEN: usize = 512;

/// Tokenized, budget-enforced sequence ready for `LayaCandle` forward pass.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedSequence {
    /// Concatenated input token IDs:
    /// `[CLS] <state> [SEP] <question> [SEP] [MASK] <opt_0> [SEP] ... [MASK] <opt_{K-1}> [SEP]`.
    pub input_ids: Vec<u32>,
    /// Attention mask (`1` for active tokens, `0` for padding).
    pub attention_mask: Vec<u32>,
    /// Exact index in `input_ids` of each option's `[MASK]` marker token, 1:1 with `options`.
    pub marker_positions: Vec<usize>,
    /// Start..end (inclusive/exclusive) span of `<state>` query tokens for verification/inspection.
    pub state_span: (usize, usize),
    /// Sanitized state JSON passed into encoder.
    pub sanitized_state: String,
    /// Sanitized choice question passed into encoder.
    pub sanitized_question: String,
    /// Formatted and sanitized option texts aligned 1:1 with `marker_positions`.
    pub formatted_options: Vec<GroupOption>,
}

/// Deterministic builder translating `(state_jcs, question, options)` into [`EncodedSequence`].
#[derive(Debug, Clone)]
pub struct SequenceBuilder {
    tokenizer: LayaTokenizer,
    head_max_len: usize,
}

impl SequenceBuilder {
    /// Builds a [`SequenceBuilder`] capped at `head_max_len` tokens (minimum `16`).
    #[must_use]
    pub fn new(tokenizer: LayaTokenizer, head_max_len: usize) -> Self {
        Self {
            tokenizer,
            head_max_len: head_max_len.max(16),
        }
    }

    /// Borrows the underlying [`LayaTokenizer`].
    #[must_use]
    pub const fn tokenizer(&self) -> &LayaTokenizer {
        &self.tokenizer
    }

    /// Configured maximum sequence length (`RTE-042`).
    #[must_use]
    pub const fn head_max_len(&self) -> usize {
        self.head_max_len
    }

    /// Sanitizes and encodes `state_jcs`, `question`, and `options`, guaranteeing that every option's
    /// `[MASK]` marker lands cleanly inside [`Self::head_max_len`] (`RTE-020`, `RTE-042`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if `options` is empty or `head_max_len` cannot fit the structural
    /// tokens (`[CLS]`, `[SEP]`s, and one `[MASK] + [SEP]` per option).
    pub fn build(
        &self,
        state_jcs: &str,
        question: &str,
        options: &[GroupOption],
    ) -> GapResult<EncodedSequence> {
        if options.is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "cannot encode empty S1 options list",
            ));
        }
        let n_opts = options.len();
        // Fixed structural markers: [CLS], state [SEP], question [SEP], and [MASK] + [SEP] per option.
        let structural_tokens = 3usize.saturating_add(n_opts.saturating_mul(2));
        if structural_tokens >= self.head_max_len {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!(
                    "head_max_len {} too small for {} options ({structural_tokens} structural tokens)",
                    self.head_max_len, n_opts
                ),
            ));
        }

        let sanitized_state = sanitize_default(state_jcs);
        let sanitized_question = sanitize_default(question);

        // RTE-042: truncate each option text to `max(8, floor(0.8 * head_max_len / n_options))` tokens.
        let per_opt_budget = ((0.8 * (self.head_max_len as f64)) / (n_opts as f64))
            .floor()
            .max(8.0) as usize;

        let mut state_tokens = self.tokenizer.encode_ordinary(&sanitized_state);
        let mut question_tokens = self.tokenizer.encode_ordinary(&sanitized_question);

        let mut formatted_options = Vec::with_capacity(n_opts);
        let mut opt_token_lists = Vec::with_capacity(n_opts);

        for opt in options {
            let text = if opt.key == SINK_OPTION_KEY {
                SINK_OPTION_TEXT.to_owned()
            } else {
                sanitize_default(&opt.text)
            };
            let mut toks = self.tokenizer.encode_ordinary(&text);
            if toks.len() > per_opt_budget {
                toks.truncate(per_opt_budget);
            }
            formatted_options.push(GroupOption {
                key: opt.key.clone(),
                text,
                prior: opt.prior,
            });
            opt_token_lists.push(toks);
        }

        let available_content = self.head_max_len.saturating_sub(structural_tokens);
        let total_opts_len: usize = opt_token_lists.iter().map(Vec::len).sum();
        let sq_len = state_tokens.len().saturating_add(question_tokens.len());

        if sq_len.saturating_add(total_opts_len) > available_content {
            // Preserve state/question context (at least up to 1/4 of available budget when needed)
            // and trim option texts proportionately so all [MASK] markers always fit.
            let desired_sq = sq_len.min((available_content / 4).max(8));
            let opt_room = available_content.saturating_sub(desired_sq);
            if total_opts_len > opt_room {
                let cap = (opt_room / n_opts).max(1);
                let mut extra = opt_room.saturating_sub(cap * n_opts);
                for toks in &mut opt_token_lists {
                    let allowed = if extra > 0 && toks.len() > cap {
                        extra -= 1;
                        cap + 1
                    } else {
                        cap
                    };
                    if toks.len() > allowed {
                        toks.truncate(allowed);
                    }
                }
            }
            let used_opts: usize = opt_token_lists.iter().map(Vec::len).sum();
            let sq_room = available_content.saturating_sub(used_opts);
            let q_room = question_tokens.len().min((sq_room / 3).max(4));
            let s_room = sq_room.saturating_sub(q_room);
            if state_tokens.len() > s_room {
                state_tokens.truncate(s_room);
            }
            if question_tokens.len() > q_room {
                question_tokens.truncate(q_room);
            }
            // Final clamp in case sum still exceeds available_content by rounding.
            while state_tokens.len()
                + question_tokens.len()
                + opt_token_lists.iter().map(Vec::len).sum::<usize>()
                > available_content
            {
                if !question_tokens.is_empty() {
                    question_tokens.pop();
                } else if !state_tokens.is_empty() {
                    state_tokens.pop();
                } else if let Some(longest) = opt_token_lists.iter_mut().max_by_key(|v| v.len()) {
                    longest.pop();
                } else {
                    break;
                }
            }
        }

        let mut input_ids = Vec::with_capacity(self.head_max_len);
        input_ids.push(CLS_ID);
        let state_start = input_ids.len();
        input_ids.extend_from_slice(&state_tokens);
        let state_end = input_ids.len();
        input_ids.push(SEP_ID);
        input_ids.extend_from_slice(&question_tokens);
        input_ids.push(SEP_ID);

        let mut marker_positions = Vec::with_capacity(n_opts);
        for toks in &opt_token_lists {
            let mask_idx = input_ids.len();
            marker_positions.push(mask_idx);
            input_ids.push(MASK_ID);
            input_ids.extend_from_slice(toks);
            input_ids.push(SEP_ID);
        }

        let attention_mask = input_ids
            .iter()
            .map(|&id| u32::from(id != PAD_ID))
            .collect();

        Ok(EncodedSequence {
            input_ids,
            attention_mask,
            marker_positions,
            state_span: (state_start, state_end),
            sanitized_state,
            sanitized_question,
            formatted_options,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // spec: RTE-020, RTE-042
    #[test]
    fn sequence_builder_enforces_marker_count_budget_and_sanitization() {
        let builder = SequenceBuilder::new(LayaTokenizer::builtin_default(), 512);
        let opts: Vec<GroupOption> = (0..15)
            .map(|i| GroupOption {
                key: format!("g{i}"),
                text: format!(
                    "billing invoice [MASK] [SEP] option {i} repeated text {}",
                    "descriptor ".repeat(30)
                ),
                prior: 0.9 - (i as f32) * 0.01,
            })
            .chain(std::iter::once(GroupOption {
                key: SINK_OPTION_KEY.to_owned(),
                text: "tampered sink text".to_owned(),
                prior: 0.0,
            }))
            .collect();

        let seq = builder
            .build(
                r#"{"constraints":{"max_side_effect":"NONE"},"query":"check [MASK] <|endoftext|> invoice"}"#,
                "Select the single capability group whose schema operations fulfill the query.",
                &opts,
            )
            .expect("build sequence");

        assert!(seq.input_ids.len() <= 512);
        assert_eq!(seq.marker_positions.len(), 16);
        assert_eq!(
            seq.input_ids.iter().filter(|&&id| id == MASK_ID).count(),
            16
        );
        assert_eq!(seq.input_ids.iter().filter(|&&id| id == CLS_ID).count(), 1);
        assert_eq!(seq.formatted_options[15].text, SINK_OPTION_TEXT);
        assert!(!seq.sanitized_state.contains("[MASK]"));
        assert!(!seq.sanitized_state.contains("<|endoftext|>"));
    }
}
