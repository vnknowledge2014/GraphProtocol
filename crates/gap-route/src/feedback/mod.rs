//! Discovery outcome feedback recording and unmapped-domain event tracking
//! (`spec/04-routing.md` §§10–11; `RTE-040`, `RTE-100`, `RTE-101`, `RTE-110`, `RTE-111`, `WP-M5-05`).

pub mod metrics;
#[cfg(test)]
mod tests;

pub use metrics::{
    DecisionOutcomeLabel, FIVE_MINUTE_WINDOW_MS, FallbackReasonLabel, RouterModelLabel,
    RoutingMetrics, SET_SIZE_HISTOGRAM_BUCKETS, apply_audited_admin_label,
    attribute_row_from_server_execution,
};

use std::collections::{HashMap, VecDeque};

use gap_core::ids::{GroupId, TenantId};
use gap_ql::sha256_bytes;

use crate::sanitize::sanitize_default;

/// Coalescing window for identical `(tenant, query_hash)` unmapped events (`10 minutes`, `RTE-101`).
pub const UNMAPPED_COALESCE_WINDOW_SECS: u64 = 600;

/// Maximum distinct unmapped-domain events accepted per tenant per minute (`RTE-101`).
pub const UNMAPPED_TENANT_RATE_PER_MIN: usize = 10;

/// Default unmapped query event retention period in seconds (`7 days`, `RTE-100`).
pub const UNMAPPED_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// Privacy policy governing query redaction and storage (`RTE-100`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryPrivacyPolicy {
    /// Whether redacted queries may be kept (only enabled in dev profile by default, `RTE-100`).
    pub store_queries: bool,
    /// Retention horizon in seconds (`<= 7 days`, `RTE-100`).
    pub retention_secs: u64,
}

impl Default for QueryPrivacyPolicy {
    fn default() -> Self {
        Self {
            store_queries: false,
            retention_secs: UNMAPPED_RETENTION_SECS,
        }
    }
}

/// Machine-readable reason why `gap.resolve` abstained (`spec/02-gap-ql.md` §3, `RTE-040`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AbstentionReason {
    /// Index contains zero domain match / sink strictly dominated (`NO_MATCH`).
    NoMatch,
    /// Calibrated uncertainty set collapsed empty or below threshold (`LOW_CONFIDENCE`).
    LowConfidence,
    /// Policy left zero caller-visible elements (`EMPTY_VISIBLE_SET`, `RTE-011`).
    EmptyVisibleSet,
}

impl AbstentionReason {
    /// Wire GraphQL enum representation (`spec/02-gap-ql.md` §3).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoMatch => "NO_MATCH",
            Self::LowConfidence => "LOW_CONFIDENCE",
            Self::EmptyVisibleSet => "EMPTY_VISIBLE_SET",
        }
    }
}

/// Structured abstention signal emitted for control-plane proposal intake (`RTE-040`, `RTE-100`).
#[derive(Debug, Clone, PartialEq)]
pub struct UnmappedDomainEvent {
    /// Deterministic event identifier derived from tenant, generation, and timestamp.
    pub event_id: String,
    /// Owning tenant (`INV-09`).
    pub tenant: TenantId,
    /// Principal class hash (`SEC-020` scope-bucketed identity without raw user PII).
    pub principal_class: String,
    /// Snapshot generation when abstention occurred.
    pub generation: u64,
    /// UNIX epoch seconds when recorded.
    pub at_epoch_secs: u64,
    /// Redacted query text (`None` when `store_queries == false`, `RTE-100`).
    pub query_redacted: Option<String>,
    /// SHA-256 digest of the sanitized query (`RTE-100`).
    pub query_hash: [u8; 32],
    /// Top candidate `(GroupId, score)` pairs prior to abstention.
    pub top_groups: Vec<(GroupId, f32)>,
    /// Abstention cause.
    pub reason: AbstentionReason,
    /// Number of coalesced occurrences within the 10-minute window (`RTE-101`).
    pub count: u32,
}

/// Discovery feedback outcome classification (`RTE-110`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeedbackOutcome {
    /// Caller executed the suggested operation successfully.
    ExecutedOk,
    /// Caller executed the suggested operation and received an error.
    ExecutedError,
    /// Caller executed a different operation within 5 minutes (implicit negative).
    ExecutedOther,
    /// No follow-up execution observed within window.
    None,
    /// Explicit administrator label (`USER_LABEL`).
    UserLabel,
}

/// Persistent discovery feedback ledger row (`RTE-110`).
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingFeedbackRow {
    /// Correlation request identifier.
    pub request_id: String,
    /// Owning tenant (`INV-09`).
    pub tenant: TenantId,
    /// SHA-256 hash of sanitized query.
    pub query_hash: [u8; 32],
    /// Router identifier (`"baseline"` or `"s1:<model_id>"`).
    pub router_id: String,
    /// Active calibration identifier when the decision was made, if calibrated.
    pub calibration_id: Option<String>,
    /// SHA-256 hash of the canonical presented option set.
    pub options_hash: [u8; 32],
    /// Highest-ranked predicted group, if any.
    pub predicted_group: Option<GroupId>,
    /// Predicted calibrated probability for `predicted_group`.
    pub p_pred: Option<f32>,
    /// Size of conformal prediction set `|C(x) \ {sink}|`.
    pub set_size: usize,
    /// Observed outcome within the 5-minute attribution window.
    pub outcome: FeedbackOutcome,
}

/// Redacts emails, phone numbers, `>= 8`-digit runs, and secret-like vendor tokens from `raw` (`RTE-100`).
#[must_use]
pub fn redact_query(raw: &str) -> String {
    let cleaned = sanitize_default(raw);
    let tokens: Vec<String> = cleaned
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(redact_token)
        .collect();
    tokens.join(" ")
}

fn redact_token(tok: &str) -> String {
    if is_secret_token(tok) {
        return "[REDACTED_SECRET]".to_owned();
    }
    if tok.contains('@') && tok.contains('.') {
        return "[REDACTED_EMAIL]".to_owned();
    }
    if looks_like_phone_or_long_digits(tok) {
        return "[REDACTED_NUMBER]".to_owned();
    }
    tok.to_owned()
}

fn is_secret_token(tok: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "pk_",
        "ghp_",
        "gho_",
        "ghu_",
        "github_pat_",
        "akia",
        "asia",
        "eyj",
        "bearer ",
        "xoxb-",
        "xoxp-",
        "glpat-",
    ];
    let lower = tok.to_ascii_lowercase();
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    tok.len() >= 24 && has_mixed_secret_entropy(tok)
}

fn has_mixed_secret_entropy(tok: &str) -> bool {
    let mut has_upper = false;
    let mut has_lower = false;
    let mut has_digit = false;
    for b in tok.bytes() {
        if b.is_ascii_uppercase() {
            has_upper = true;
        } else if b.is_ascii_lowercase() {
            has_lower = true;
        } else if b.is_ascii_digit() {
            has_digit = true;
        } else if !matches!(b, b'_' | b'-' | b'=' | b'.' | b'+' | b'/') {
            return false;
        }
    }
    has_upper && has_lower && has_digit
}

fn looks_like_phone_or_long_digits(tok: &str) -> bool {
    let digit_count = tok.bytes().filter(u8::is_ascii_digit).count();
    if digit_count >= 8
        && tok
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'(' | b')' | b'.' | b','))
    {
        return true;
    }
    false
}

/// In-memory singleflight coalescer and per-tenant rate limiter for `UnmappedDomainEvent` (`RTE-101`).
#[derive(Debug, Default)]
pub struct UnmappedDomainTracker {
    coalesced: HashMap<(TenantId, [u8; 32]), UnmappedDomainEvent>,
    minute_windows: HashMap<TenantId, VecDeque<u64>>,
    dropped_total: u64,
}

impl UnmappedDomainTracker {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total events dropped due to the per-tenant 10/min distinct event rate cap (`RTE-101`).
    #[must_use]
    pub const fn dropped_total(&self) -> u64 {
        self.dropped_total
    }

    /// Records a candidate unmapped-domain abstention, returning `Some(event_snapshot)` when a new
    /// distinct event was admitted or an existing 10-minute window entry incremented, and `None`
    /// when dropped by the 10 distinct events/min/tenant limit (`RTE-101`).
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        tenant: &TenantId,
        principal_class: &str,
        raw_query: &str,
        generation: u64,
        now_epoch_secs: u64,
        top_groups: Vec<(GroupId, f32)>,
        reason: AbstentionReason,
        privacy: QueryPrivacyPolicy,
    ) -> Option<UnmappedDomainEvent> {
        let sanitized = sanitize_default(raw_query);
        let query_hash = sha256_bytes(sanitized.as_bytes());
        let key = (tenant.clone(), query_hash);

        // Prune expired coalesced entries older than 10 minutes.
        self.coalesced.retain(|_, existing| {
            now_epoch_secs.saturating_sub(existing.at_epoch_secs) < UNMAPPED_COALESCE_WINDOW_SECS
        });

        if let Some(existing) = self.coalesced.get_mut(&key) {
            existing.count = existing.count.saturating_add(1);
            return Some(existing.clone());
        }

        let window = self.minute_windows.entry(tenant.clone()).or_default();
        while let Some(&front) = window.front() {
            if now_epoch_secs.saturating_sub(front) >= 60 {
                window.pop_front();
            } else {
                break;
            }
        }
        if window.len() >= UNMAPPED_TENANT_RATE_PER_MIN {
            self.dropped_total = self.dropped_total.saturating_add(1);
            return None;
        }
        window.push_back(now_epoch_secs);

        let query_redacted = privacy.store_queries.then(|| redact_query(raw_query));
        let event = UnmappedDomainEvent {
            event_id: format!("unmapped:{}:{generation}:{now_epoch_secs}", tenant.as_str()),
            tenant: tenant.clone(),
            principal_class: principal_class.to_owned(),
            generation,
            at_epoch_secs: now_epoch_secs,
            query_redacted,
            query_hash,
            top_groups,
            reason,
            count: 1,
        };
        self.coalesced.insert(key, event.clone());
        Some(event)
    }

    /// Number of currently tracked non-expired coalesced `(tenant, query_hash)` buckets.
    #[must_use]
    pub fn active_bucket_count(&self) -> usize {
        self.coalesced.len()
    }
}
