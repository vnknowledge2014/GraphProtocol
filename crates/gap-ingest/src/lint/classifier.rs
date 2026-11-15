//! Deterministic linear prompt-injection feature classifier + heuristic fusion (`SEC-081`).

use crate::cisg::{LintStatus, sha256_hex};

use super::rules::{HeuristicMatchReport, evaluate_heuristics};

/// Default classification margin threshold (in thousandths; `1_000` == 1.000).
pub const DEFAULT_CLASSIFIER_THRESHOLD_MILLI: i32 = 1_000;

/// Combined verdict and telemetry emitted by [`InjectionLinter`] (`SEC-081`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintReport {
    /// Normative `LintStatus::Clean` or `LintStatus::Flagged { reasons }`.
    pub status: LintStatus,
    /// Deterministic classifier logit in milli-units (`>= threshold` triggers classifier flag).
    pub classifier_score_milli: i32,
    /// Decision threshold in milli-units.
    pub threshold_milli: i32,
    /// Machine-readable triggered rule & classifier feature tags.
    pub reasons: Vec<String>,
    /// Whether hidden bidi, zero-width, tag, or control characters were present in raw input.
    pub hidden_unicode_detected: bool,
    /// Sanitized text safe for lexical storage after review (`SEC-080`, `SEC-081`).
    pub sanitized: String,
    /// Lowercase SHA-256 hex digest of the raw untrusted bytes (`SEC-082`).
    pub raw_sha256_hex: String,
}

impl LintReport {
    /// Returns `true` when input was flagged as suspicious or unsafe.
    #[must_use]
    pub const fn is_flagged(&self) -> bool {
        matches!(self.status, LintStatus::Flagged { .. })
    }
}

/// Heuristic + lightweight linear classifier for untrusted upstream text (`SEC-081`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectionLinter {
    /// Score cutoff (in milli-units) above which the linear classifier independently flags text.
    pub threshold_milli: i32,
    /// Maximum byte length preserved in sanitized text.
    pub max_sanitized_bytes: usize,
}

impl Default for InjectionLinter {
    fn default() -> Self {
        Self {
            threshold_milli: DEFAULT_CLASSIFIER_THRESHOLD_MILLI,
            max_sanitized_bytes: 8_192,
        }
    }
}

impl InjectionLinter {
    /// Inspects raw untrusted text and computes a deterministic heuristic + classifier verdict (`SEC-081`).
    #[must_use]
    pub fn analyze(&self, raw: &str) -> LintReport {
        let heuristics = evaluate_heuristics(raw);
        let (classifier_score_milli, mut cls_reasons) = score_linear_features(&heuristics);

        let mut reasons = heuristics.reasons.clone();
        if classifier_score_milli >= self.threshold_milli {
            for tag in cls_reasons.drain(..) {
                if !reasons.iter().any(|existing| existing == &tag) {
                    reasons.push(tag);
                }
            }
            if !reasons.iter().any(|r| r.starts_with("classifier:")) {
                reasons.push("classifier:high_injection_score".to_owned());
            }
        }
        reasons.sort();
        reasons.dedup();

        let status = if reasons.is_empty() {
            LintStatus::Clean
        } else {
            LintStatus::Flagged {
                reasons: reasons.clone(),
            }
        };

        let sanitized = truncate_utf8(&heuristics.cleaned, self.max_sanitized_bytes)
            .replace("<gap:untrusted", "[gap-untrusted")
            .replace("</gap:untrusted>", "[/gap-untrusted]");

        LintReport {
            status,
            classifier_score_milli,
            threshold_milli: self.threshold_milli,
            reasons,
            hidden_unicode_detected: heuristics.unicode.has_hidden_tricks(),
            sanitized,
            raw_sha256_hex: sha256_hex(raw.as_bytes()),
        }
    }
}

fn score_linear_features(h: &HeuristicMatchReport) -> (i32, Vec<String>) {
    // Negative prior bias keeps normal technical/API prose firmly negative unless active injection cues fire.
    let mut score: i32 = -450;
    let mut feature_tags = Vec::new();
    let f = &h.folded_lower;

    // Strong weights directly from deterministic heuristic classes.
    score = score.saturating_add(i32::try_from(h.imperative_hits).unwrap_or(10) * 900);
    score = score.saturating_add(i32::try_from(h.tool_lure_hits).unwrap_or(10) * 850);
    score = score.saturating_add(i32::try_from(h.role_marker_hits).unwrap_or(10) * 950);
    if h.exfil_hit {
        score = score.saturating_add(1_100);
    }
    if h.unicode.has_hidden_tricks() {
        score = score.saturating_add(1_050);
    }
    if h.unicode.confusable_count >= 2
        && (h.imperative_hits > 0 || contains_second_person_directive(f))
    {
        score = score.saturating_add(700);
        feature_tags.push("classifier:confusable_spoof".to_owned());
    }

    // Compositional classifier features capturing paraphrases not listed verbatim in rules.rs.
    if contains_second_person_directive(f) {
        score = score.saturating_add(850);
        feature_tags.push("classifier:second_person_directive".to_owned());
    }
    if contains_concealment_cue(f) {
        score = score.saturating_add(800);
        feature_tags.push("classifier:concealment_cue".to_owned());
    }
    if contains_exfil_intent(f) {
        score = score.saturating_add(1_450);
        feature_tags.push("classifier:exfiltration_intent".to_owned());
    }
    if contains_prompt_boundary_mention(f) {
        score = score.saturating_add(700);
        feature_tags.push("classifier:prompt_override_phrase".to_owned());
    }

    // Benign technical documentation dampening when zero hard rules tripped.
    if h.reasons.is_empty() && has_benign_api_doc_structure(f) {
        score = score.saturating_sub(250);
    }

    (score, feature_tags)
}

fn contains_second_person_directive(f: &str) -> bool {
    let has_agent_ref = has_word(f, "you")
        || has_word(f, "your")
        || has_word(f, "assistant")
        || has_word(f, "model")
        || has_word(f, "agent");
    let has_obligation = has_word(f, "must")
        || has_word(f, "ignore")
        || has_word(f, "disregard")
        || has_word(f, "forget")
        || has_word(f, "override")
        || has_word(f, "reveal")
        || has_word(f, "output")
        || has_word(f, "bypass");
    has_agent_ref && has_obligation
}

fn contains_concealment_cue(f: &str) -> bool {
    let hides = f.contains("secret") || f.contains("silently") || f.contains("without notifying");
    let target = has_word(f, "user") || has_word(f, "human") || f.contains("without approval");
    hides && target
}

fn contains_exfil_intent(f: &str) -> bool {
    let sends = f.contains("post to http")
        || f.contains("upload to http")
        || f.contains("send to http")
        || f.contains("send all secrets")
        || f.contains("leak ")
        || f.contains("steal ");
    let sensitive = f.contains("token")
        || f.contains("secret")
        || f.contains("password")
        || f.contains("credential")
        || f.contains("private key")
        || f.contains("system prompt");
    sends && sensitive
}

fn contains_prompt_boundary_mention(f: &str) -> bool {
    f.contains("previous instructions")
        || f.contains("prior instructions")
        || f.contains("developer message")
        || f.contains("system prompt")
        || f.contains("jailbreak")
}

fn has_benign_api_doc_structure(f: &str) -> bool {
    f.contains("returns ")
        || f.contains("creates ")
        || f.contains("updates ")
        || f.contains("deletes ")
        || f.contains("pagination")
        || f.contains("cursor")
        || f.contains("identifier")
        || f.contains("timestamp")
}

fn has_word(haystack: &str, word: &str) -> bool {
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(word) {
        let abs = start + rel;
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        let end = abs + word.len();
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        if before_ok && after_ok {
            return true;
        }
        start = abs + word.len();
    }
    false
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    s[..end].to_owned()
}
