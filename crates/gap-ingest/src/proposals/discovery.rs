//! Unmapped domain event intake, deterministic query redaction, immediate abstention error,
//! and allow-listed `DiscoveryWorker` (`WP-M4-07`; `RTE-100`, `RTE-101`, `RTE-102`, `RTE-103`, `GQL-071`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use gap_core::clock::{Clock, Timestamp};
use gap_core::error::{ErrorCode, GapError, NextStep, Suggestion};
use gap_core::ids::{BranchId, TenantId, TenantScope};
use gap_core::snapshot::{Protocol, TrustTier};

use crate::cisg::sha256_hex;
use crate::proposals::service::{ProposalService, canonicalize_location};
use crate::proposals::types::{
    Proposal, ProposalError, ProposeSourceInput, UNMAPPED_QUERY_MAX_TTL_SECS,
    UNMAPPED_RATE_LIMIT_PER_MIN, UNMAPPED_SINGLEFLIGHT_WINDOW_SECS,
};

const MICROS_PER_SEC: i64 = 1_000_000;
const RATE_WINDOW_SECS: u64 = 60;

/// Redacts credentials, tokens, emails, URL userinfo, and secret query parameters from a raw query (`RTE-100`).
///
/// Purely deterministic (zero ML/network calls) and safe for logging or optional opt-in storage.
#[must_use]
pub fn redact_query(raw: &str) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let mut out = Vec::with_capacity(words.len());
    let mut expect_bearer_secret = false;

    for token in words {
        if expect_bearer_secret {
            out.push("[REDACTED]".to_owned());
            expect_bearer_secret = false;
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if lower == "bearer" || lower == "bearer:" {
            out.push("Bearer".to_owned());
            expect_bearer_secret = true;
            continue;
        }
        if let Some((prefix, rest)) = split_kv_secret_prefix(&lower, token) {
            let suffix = trailing_punct(rest);
            out.push(format!("{prefix}[REDACTED]{suffix}"));
            continue;
        }
        if token.contains("://") {
            out.push(redact_url_token(token));
            continue;
        }
        let (core, lead, trail) = trim_token_punct(token);
        if looks_like_email(core) {
            out.push(format!("{lead}[REDACTED_EMAIL]{trail}"));
        } else if looks_like_secret_token(core) {
            out.push(format!("{lead}[REDACTED]{trail}"));
        } else {
            out.push(token.to_owned());
        }
    }

    out.join(" ")
}

fn split_kv_secret_prefix<'a>(lower: &str, raw: &'a str) -> Option<(&'a str, &'a str)> {
    const SECRET_KEYS: &[&str] = &[
        "authorization=",
        "authorization:",
        "api_key=",
        "api_key:",
        "apikey=",
        "apikey:",
        "access_token=",
        "access_token:",
        "secret=",
        "secret:",
        "password=",
        "password:",
        "passwd=",
        "token=",
        "token:",
    ];
    for key in SECRET_KEYS {
        if lower.starts_with(key) && raw.len() >= key.len() {
            return Some((&raw[..key.len()], &raw[key.len()..]));
        }
    }
    None
}

fn redact_url_token(token: &str) -> String {
    let Some(scheme_sep) = token.find("://") else {
        return token.to_owned();
    };
    let scheme_and_sep = &token[..scheme_sep + 3];
    let rest = &token[scheme_sep + 3..];
    let (auth_and_path, query_part) = match rest.split_once('?') {
        Some((ap, q)) => (ap, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match auth_and_path.find('/') {
        Some(idx) => (&auth_and_path[..idx], &auth_and_path[idx..]),
        None => (auth_and_path, ""),
    };
    let clean_authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("[REDACTED]@{host}"),
        None => authority.to_owned(),
    };
    let Some(query) = query_part else {
        return format!("{scheme_and_sep}{clean_authority}{path}");
    };
    let clean_pairs: Vec<String> = query
        .split('&')
        .map(|pair| {
            let Some((k, _)) = pair.split_once('=') else {
                return pair.to_owned();
            };
            let kl = k.to_ascii_lowercase();
            if matches!(
                kl.as_str(),
                "token" | "api_key" | "apikey" | "secret" | "password" | "auth" | "key" | "sig"
            ) {
                format!("{k}=[REDACTED]")
            } else {
                pair.to_owned()
            }
        })
        .collect();
    format!(
        "{scheme_and_sep}{clean_authority}{path}?{}",
        clean_pairs.join("&")
    )
}

fn trim_token_punct(token: &str) -> (&str, &str, &str) {
    let bytes = token.as_bytes();
    let mut start = 0;
    while start < bytes.len() && matches!(bytes[start], b'"' | b'\'' | b'(' | b'[' | b'{') {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start
        && matches!(
            bytes[end - 1],
            b'"' | b'\'' | b',' | b';' | b')' | b']' | b'}' | b'.'
        )
    {
        end -= 1;
    }
    (&token[start..end], &token[..start], &token[end..])
}

fn trailing_punct(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut end = bytes.len();
    while end > 0
        && matches!(
            bytes[end - 1],
            b',' | b';' | b')' | b']' | b'}' | b'"' | b'\''
        )
    {
        end -= 1;
    }
    &raw[end..]
}

fn looks_like_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

fn looks_like_secret_token(s: &str) -> bool {
    if s.starts_with("sk-")
        || s.starts_with("sk_")
        || s.starts_with("pk_live_")
        || s.starts_with("ghp_")
        || s.starts_with("gho_")
        || s.starts_with("xoxb-")
        || s.starts_with("AKIA")
    {
        return s.len() >= 10;
    }
    if s.len() >= 24 && !s.contains(' ') {
        let has_alpha = s.bytes().any(|b| b.is_ascii_alphabetic());
        let has_digit = s.bytes().any(|b| b.is_ascii_digit());
        let is_token_chars = s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+' | b'/' | b'=')
        });
        return has_alpha && has_digit && is_token_chars;
    }
    false
}

/// Configuration for [`UnmappedEventIntake`] (`RTE-100`, `RTE-101`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedIntakeConfig {
    /// Opt-in flag mirroring `privacy.store_queries` (default `false`: query text is NOT stored).
    pub privacy_store_queries: bool,
    /// Retention TTL in seconds when `privacy_store_queries` is `true` (`<= 7 days`, `RTE-100`).
    pub retention_secs: u64,
    /// Singleflight deduplication window for identical `(tenant, query_hash)` (`10 min`, `RTE-101`).
    pub singleflight_window_secs: u64,
    /// Maximum distinct unmapped events accepted per tenant per minute (`10 / min`, `RTE-101`).
    pub rate_limit_per_min: usize,
}

impl Default for UnmappedIntakeConfig {
    fn default() -> Self {
        Self {
            privacy_store_queries: false,
            retention_secs: UNMAPPED_QUERY_MAX_TTL_SECS,
            singleflight_window_secs: UNMAPPED_SINGLEFLIGHT_WINDOW_SECS,
            rate_limit_per_min: UNMAPPED_RATE_LIMIT_PER_MIN,
        }
    }
}

impl UnmappedIntakeConfig {
    /// Validates privacy retention and rate-limit invariants (`RTE-100`, `RTE-101`).
    pub fn validate(&self) -> Result<(), ProposalError> {
        if self.retention_secs > UNMAPPED_QUERY_MAX_TTL_SECS {
            return Err(ProposalError::Validation(format!(
                "unmapped query retention_secs ({}) exceeds 7 day maximum ({UNMAPPED_QUERY_MAX_TTL_SECS})",
                self.retention_secs
            )));
        }
        if self.rate_limit_per_min == 0 {
            return Err(ProposalError::Validation(
                "unmapped rate_limit_per_min must be >= 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Queued domain abstention telemetry emitted for offline background discovery (`RTE-100`).
#[derive(Debug, Clone, PartialEq)]
pub struct UnmappedDomainEvent {
    /// Owning tenant (`INV-09`).
    pub tenant: TenantId,
    /// SHA-256 hex digest of the redacted query intent.
    pub query_hash: String,
    /// Redacted query text, populated ONLY when `privacy.store_queries` is enabled (`RTE-100`).
    pub query_redacted: Option<String>,
    /// Optional capability candidate scores `(group_or_tag, confidence)` from S1 abstention.
    pub candidate_scores: Vec<(String, f64)>,
    /// Timestamp when event entered the queue.
    pub emitted_at: Timestamp,
    /// Expiry deadline (`<= 7 days` when `query_redacted` is stored, `RTE-100`).
    pub expires_at: Option<Timestamp>,
}

/// Outcome returned when recording an unmapped query into [`UnmappedEventIntake`] (`RTE-101`).
#[derive(Debug, Clone, PartialEq)]
pub enum IntakeOutcome {
    /// Event passed singleflight and rate-limit checks and was enqueued for `DiscoveryWorker`.
    Accepted(UnmappedDomainEvent),
    /// Identical `(tenant, query_hash)` already arrived within the singleflight window.
    Coalesced {
        /// Deduplicated SHA-256 query hash.
        query_hash: String,
    },
    /// Per-tenant rate limit exceeded; event was dropped and `gap_unmapped_events_dropped_total` incremented.
    RateLimited {
        /// Cumulative dropped events across this intake instance.
        dropped_total: u64,
    },
}

#[derive(Debug, Default)]
struct IntakeState {
    last_seen: BTreeMap<(TenantId, String), Timestamp>,
    rate_buckets: BTreeMap<TenantId, VecDeque<Timestamp>>,
    pending: VecDeque<UnmappedDomainEvent>,
    dropped_total: u64,
}

/// Singleflight-coalescing, rate-limited intake queue for `UnmappedDomainEvent` (`RTE-100`, `RTE-101`).
#[derive(Clone)]
pub struct UnmappedEventIntake {
    config: UnmappedIntakeConfig,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<IntakeState>>,
}

impl std::fmt::Debug for UnmappedEventIntake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnmappedEventIntake")
            .field("config", &self.config)
            .field("dropped_total", &self.dropped_total())
            .finish_non_exhaustive()
    }
}

impl UnmappedEventIntake {
    /// Creates an [`UnmappedEventIntake`] after validating privacy TTL constraints (`RTE-100`).
    pub fn new(config: UnmappedIntakeConfig, clock: Arc<dyn Clock>) -> Result<Self, ProposalError> {
        config.validate()?;
        Ok(Self {
            config,
            clock,
            state: Arc::new(Mutex::new(IntakeState::default())),
        })
    }

    /// Returns cumulative dropped events (`gap_unmapped_events_dropped_total`, `RTE-101`).
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .dropped_total
    }

    /// Records an abstained query event with redaction, 10-minute `(tenant, query_hash)` singleflight,
    /// and `10 / min / tenant` rate limiting (`RTE-100`, `RTE-101`).
    pub fn record(
        &self,
        tenant: &TenantScope,
        raw_query: &str,
        candidate_scores: Vec<(String, f64)>,
    ) -> Result<IntakeOutcome, ProposalError> {
        let trimmed = raw_query.trim();
        if trimmed.is_empty() {
            return Err(ProposalError::Validation(
                "unmapped query text must not be empty".to_owned(),
            ));
        }

        let redacted = redact_query(trimmed);
        let query_hash = sha256_hex(redacted.as_bytes());
        let now = self.clock.now_timestamp();

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        purge_expired_locked(&mut guard.pending, now);

        let sf_window_us =
            i128::from(self.config.singleflight_window_secs) * i128::from(MICROS_PER_SEC);
        let sf_key = (tenant.tenant().clone(), query_hash.clone());
        if let Some(prev_ts) = guard.last_seen.get(&sf_key) {
            let delta = i128::from(now.unix_micros()) - i128::from(prev_ts.unix_micros());
            if delta >= 0 && delta < sf_window_us {
                return Ok(IntakeOutcome::Coalesced { query_hash });
            }
        }

        let rate_window_us = i128::from(RATE_WINDOW_SECS) * i128::from(MICROS_PER_SEC);
        let bucket = guard
            .rate_buckets
            .entry(tenant.tenant().clone())
            .or_default();
        while let Some(front) = bucket.front().copied() {
            let age = i128::from(now.unix_micros()) - i128::from(front.unix_micros());
            if age >= rate_window_us {
                let _ = bucket.pop_front();
            } else {
                break;
            }
        }
        if bucket.len() >= self.config.rate_limit_per_min {
            guard.dropped_total = guard.dropped_total.saturating_add(1);
            return Ok(IntakeOutcome::RateLimited {
                dropped_total: guard.dropped_total,
            });
        }

        bucket.push_back(now);
        guard.last_seen.insert(sf_key, now);

        let (query_redacted, expires_at) = if self.config.privacy_store_queries {
            let exp = add_secs(now, self.config.retention_secs)?;
            (Some(redacted), Some(exp))
        } else {
            (None, None)
        };

        let event = UnmappedDomainEvent {
            tenant: tenant.tenant().clone(),
            query_hash,
            query_redacted,
            candidate_scores,
            emitted_at: now,
            expires_at,
        };
        guard.pending.push_back(event.clone());
        Ok(IntakeOutcome::Accepted(event))
    }

    /// Drains non-expired queued [`UnmappedDomainEvent`]s for asynchronous background processing.
    #[must_use]
    pub fn drain_pending(&self) -> Vec<UnmappedDomainEvent> {
        let now = self.clock.now_timestamp();
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        purge_expired_locked(&mut guard.pending, now);
        guard.pending.drain(..).collect()
    }
}

fn purge_expired_locked(pending: &mut VecDeque<UnmappedDomainEvent>, now: Timestamp) {
    pending.retain(|ev| ev.expires_at.is_none_or(|exp| now < exp));
}

fn add_secs(ts: Timestamp, secs: u64) -> Result<Timestamp, ProposalError> {
    let add_nanos = i128::from(secs) * 1_000_000_000;
    let total = ts
        .unix_nanos()
        .checked_add(add_nanos)
        .ok_or_else(|| ProposalError::Validation("retention timestamp overflow".to_owned()))?;
    Timestamp::from_unix_nanos(total)
        .map_err(|err| ProposalError::Validation(format!("invalid retention timestamp: {err}")))
}

/// Constructs an immediate `GAP_INSUFFICIENT_EVIDENCE` error when no active capability matches (`RTE-103`, `GQL-071`).
///
/// Always carries `extensions.suggestion.nextStep = "propose_source"` so callers know they may
/// submit a connector proposal explicitly.
#[must_use]
pub fn insufficient_evidence_error(
    correlation_id: impl Into<String>,
    examples: Vec<String>,
) -> GapError {
    GapError::new(
        ErrorCode::InsufficientEvidence,
        "no active capability group matches query domain",
    )
    .with_correlation_id(correlation_id)
    .with_suggestion(Suggestion {
        next_step: Some(NextStep::ProposeSource),
        examples: (!examples.is_empty()).then_some(examples),
        ..Suggestion::default()
    })
}

/// Static allow-listed registry catalog entry inspected by [`DiscoveryWorker`] (`RTE-102`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySourceEntry {
    /// Registry hostname (must match an entry in `DiscoveryWorker::allowed_registries`).
    pub registry_host: String,
    /// Protocol kind of the discoverable source.
    pub kind: Protocol,
    /// Source descriptor URL hosted on `registry_host`.
    pub location: String,
    /// Optional suggested branch slug.
    pub branch: Option<BranchId>,
    /// Default trust tier (typically `T2` for external discovered sources).
    pub trust: TrustTier,
    /// Domain keywords matched deterministically against unmapped intents/candidates.
    pub keywords: Vec<String>,
    /// Human-readable description attached to `notes` (treated as untrusted data).
    pub summary: String,
}

/// Offline registry scanner that surfaces candidate sources strictly as `PROPOSED` proposals (`RTE-102`).
///
/// Only queries explicitly allow-listed registries and NEVER triggers extraction, verification,
/// tool execution, or snapshot activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryWorker {
    allowed_registries: BTreeSet<String>,
    catalog: Vec<RegistrySourceEntry>,
}

impl DiscoveryWorker {
    /// Constructs a [`DiscoveryWorker`] pinned to an allow-list of registry hostnames (`RTE-102`).
    #[must_use]
    pub fn new<I, S>(allowed_registries: I, catalog: Vec<RegistrySourceEntry>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let allowed = allowed_registries
            .into_iter()
            .map(|s| s.into().trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            allowed_registries: allowed,
            catalog,
        }
    }

    /// Evaluates an [`UnmappedDomainEvent`] against allow-listed registry entries and persists
    /// matching candidates solely in [`ProposalStatus::Proposed`](crate::proposals::ProposalStatus::Proposed) (`RTE-102`).
    pub fn process_event(
        &self,
        service: &ProposalService,
        tenant: &TenantScope,
        event: &UnmappedDomainEvent,
    ) -> Result<Vec<Proposal>, ProposalError> {
        if event.tenant != *tenant.tenant() {
            return Err(ProposalError::Validation(format!(
                "unmapped event tenant `{}` does not match caller tenant `{}`",
                event.tenant,
                tenant.tenant()
            )));
        }

        let search_tokens = extract_search_tokens(event);
        if search_tokens.is_empty() {
            return Ok(Vec::new());
        }

        let mut created = Vec::new();
        for entry in &self.catalog {
            if !self.is_entry_allowlisted(entry) {
                continue;
            }
            if !entry_matches_tokens(entry, &search_tokens) {
                continue;
            }
            let input = ProposeSourceInput {
                kind: entry.kind,
                location: entry.location.clone(),
                branch: entry.branch.clone(),
                trust: entry.trust,
                notes: Some(format!(
                    "discovered from allow-listed registry `{}` for query_hash={}: {}",
                    entry.registry_host, event.query_hash, entry.summary
                )),
            };
            let proposal = service.propose_source(tenant, "discovery-worker", input)?;
            created.push(proposal);
        }
        Ok(created)
    }

    fn is_entry_allowlisted(&self, entry: &RegistrySourceEntry) -> bool {
        let host = entry.registry_host.trim().to_ascii_lowercase();
        if host.is_empty() || !self.allowed_registries.contains(&host) {
            return false;
        }
        let Ok(canon) = canonicalize_location(&entry.location) else {
            return false;
        };
        let Some(loc_host) = extract_uri_host(&canon) else {
            return false;
        };
        loc_host == host
    }
}

fn extract_uri_host(uri: &str) -> Option<String> {
    let (_, after) = uri.split_once("://")?;
    let authority = after.split('/').next()?;
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let host = if hostport.starts_with('[') {
        hostport.find(']').map(|end| &hostport[..=end])?
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn extract_search_tokens(event: &UnmappedDomainEvent) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    if let Some(q) = &event.query_redacted {
        for word in q.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
            let lower = word.to_ascii_lowercase();
            if lower.len() >= 2 && lower != "redacted" && lower != "redacted_email" {
                tokens.insert(lower);
            }
        }
    }
    for (tag, score) in &event.candidate_scores {
        if *score >= 0.0 {
            for word in tag.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
                let lower = word.to_ascii_lowercase();
                if lower.len() >= 2 {
                    tokens.insert(lower);
                }
            }
        }
    }
    tokens
}

fn entry_matches_tokens(entry: &RegistrySourceEntry, tokens: &BTreeSet<String>) -> bool {
    entry
        .keywords
        .iter()
        .any(|kw| tokens.contains(&kw.trim().to_ascii_lowercase()))
}
