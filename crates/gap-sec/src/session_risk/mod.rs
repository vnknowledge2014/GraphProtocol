//! Per-session Rule-of-Two risk tracking feeding Cedar `context.riskScore` (`SEC-084`, `INV-09`, `TM-05`).

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gap_core::clock::{Clock, SystemClock};
use gap_core::context::{Principal, RequestContext};
use gap_core::ids::TenantId;
use gap_core::snapshot::SideEffectLevel;

use crate::authz::engine::AuthzContext;

/// Default session risk flag retention window (`1 h`, `SEC-084`).
pub const DEFAULT_SESSION_RISK_TTL: Duration = Duration::from_secs(3600);

/// Default bounded capacity of the in-memory session risk table (`INV-10`).
pub const DEFAULT_MAX_RISK_SESSIONS: usize = 65_536;

/// Risk score threshold at which `policies/default.cedar` requires four-eyes approval (`SEC-084`).
pub const RULE_OF_TWO_HIGH_RISK_SCORE: i64 = 100;

/// Tenant-scoped session identifier `(tenant, principal_id, context_id)` (`SEC-084`, `INV-09`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionRiskKey {
    /// Owning tenant (`INV-09`: prevents cross-tenant session collision).
    pub tenant: TenantId,
    /// Authenticated principal subject identifier.
    pub principal_id: String,
    /// Gateway session / conversation `context_id`.
    pub context_id: String,
}

impl SessionRiskKey {
    /// Builds a tenant-scoped session key (`INV-09`).
    #[must_use]
    pub fn new(
        tenant: TenantId,
        principal_id: impl Into<String>,
        context_id: impl Into<String>,
    ) -> Self {
        Self {
            tenant,
            principal_id: principal_id.into(),
            context_id: context_id.into(),
        }
    }
}

/// Rule-of-Two flags tracked per session (`SEC-084`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleOfTwoFlags {
    /// `A`: ingested untrusted content during this session window.
    pub a_ingested_untrusted: bool,
    /// `B`: accessed sensitive data (`classification >= confidential`, incl. `@pii`).
    pub b_accessed_sensitive: bool,
    /// `C`: performed or requested an external side effect (`Reversible` / `Irreversible` / `Unknown`).
    pub c_external_side_effect: bool,
}

impl RuleOfTwoFlags {
    /// Combines two flag sets via logical OR.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            a_ingested_untrusted: self.a_ingested_untrusted || other.a_ingested_untrusted,
            b_accessed_sensitive: self.b_accessed_sensitive || other.b_accessed_sensitive,
            c_external_side_effect: self.c_external_side_effect || other.c_external_side_effect,
        }
    }

    /// Returns `true` when `A ∧ B ∧ C` holds (`SEC-084`).
    #[must_use]
    pub const fn all_three(self) -> bool {
        self.a_ingested_untrusted && self.b_accessed_sensitive && self.c_external_side_effect
    }

    /// Deterministic risk score (`0` / `30` / `60` / `100`) compatible with `policies/default.cedar`.
    ///
    /// - 0 flags -> `0`
    /// - 1 flag -> `30`
    /// - 2 flags -> `60` (`< 80`, so normal read scopes continue uninterrupted)
    /// - 3 flags (`A ∧ B ∧ C`) -> `100` (triggers Cedar `forbid-mutate-rule-of-two-high-risk`)
    #[must_use]
    pub const fn risk_score(self) -> i64 {
        if self.all_three() {
            RULE_OF_TWO_HIGH_RISK_SCORE
        } else {
            let count = (self.a_ingested_untrusted as i64)
                + (self.b_accessed_sensitive as i64)
                + (self.c_external_side_effect as i64);
            count * 30
        }
    }
}

/// Action characteristics contributed by a candidate gateway request (`SEC-084`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRiskContribution {
    /// Whether the action consumes untrusted (`T2` / unverified) content (`Flag A`).
    pub ingests_untrusted: bool,
    /// Whether the action accesses sensitive data (`confidential` / `restricted` / `pii`, `Flag B`).
    pub accesses_sensitive: bool,
    /// Verified side-effect level of the requested action (`Flag C` when not `None`).
    pub side_effect: SideEffectLevel,
}

impl Default for ActionRiskContribution {
    fn default() -> Self {
        Self {
            ingests_untrusted: false,
            accesses_sensitive: false,
            side_effect: SideEffectLevel::None,
        }
    }
}

impl ActionRiskContribution {
    /// Converts this action descriptor into its contributed Rule-of-Two flags.
    #[must_use]
    pub const fn into_flags(self) -> RuleOfTwoFlags {
        RuleOfTwoFlags {
            a_ingested_untrusted: self.ingests_untrusted,
            b_accessed_sensitive: self.accesses_sensitive,
            c_external_side_effect: !matches!(self.side_effect, SideEffectLevel::None),
        }
    }

    /// Returns `true` if `classification` meets the `confidential` threshold for Flag `B`.
    #[must_use]
    pub fn is_sensitive_classification(classification: &str) -> bool {
        let lower = classification.trim().to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "confidential" | "restricted" | "pii" | "secret"
        )
    }
}

/// Evaluation outcome produced before/during Cedar policy evaluation (`SEC-084`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRiskAssessment {
    /// Flags already recorded on the session before evaluating the candidate action.
    pub prior_flags: RuleOfTwoFlags,
    /// Flags if the candidate action executes (`prior ∪ action`).
    pub effective_flags: RuleOfTwoFlags,
    /// Deterministic `context.riskScore` passed to Cedar.
    pub risk_score: i64,
    /// `true` when `A ∧ B ∧ C` would hold (`requires human approval`).
    pub requires_human_approval: bool,
}

/// Configuration for [`SessionRiskTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRiskConfig {
    /// Decay duration for inactive sessions (`session.ttl`, default `1 h`, `SEC-084`).
    pub ttl: Duration,
    /// Maximum tracked sessions before oldest-idle eviction.
    pub max_sessions: usize,
}

impl Default for SessionRiskConfig {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_SESSION_RISK_TTL,
            max_sessions: DEFAULT_MAX_RISK_SESSIONS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SessionEntry {
    flags: RuleOfTwoFlags,
    last_updated_ms: i64,
}

/// Thread-safe, bounded, TTL-decaying Rule-of-Two tracker (`SEC-084`, `INV-09`).
///
/// Uses an internal non-async `Mutex` held only across O(1) map updates (never across `.await`).
#[derive(Debug, Clone)]
pub struct SessionRiskTracker {
    cfg: SessionRiskConfig,
    clock: Arc<dyn Clock>,
    sessions: Arc<Mutex<BTreeMap<SessionRiskKey, SessionEntry>>>,
}

impl SessionRiskTracker {
    /// Creates a new tracker backed by [`SystemClock`].
    #[must_use]
    pub fn new(cfg: SessionRiskConfig) -> Self {
        Self::with_clock(cfg, Arc::new(SystemClock))
    }

    /// Creates a new tracker using a caller-supplied [`Clock`] (deterministic tests).
    #[must_use]
    pub fn with_clock(cfg: SessionRiskConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            cfg,
            clock,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Evaluates whether executing `action` under `key` triggers `A ∧ B ∧ C` (`SEC-084`).
    ///
    /// When `commit == true` and `A ∧ B ∧ C` does not force an unapproved block, or when explicitly
    /// recording completed non-blocking steps (`A`, `B`), updates the session's active flags and
    /// refreshes its `session.ttl` timestamp.
    #[must_use]
    pub fn evaluate_and_update(
        &self,
        key: &SessionRiskKey,
        action: ActionRiskContribution,
        commit: bool,
    ) -> SessionRiskAssessment {
        let now_ms = self.clock.unix_millis();
        let ttl_ms = i64::try_from(self.cfg.ttl.as_millis()).unwrap_or(i64::MAX);
        let action_flags = action.into_flags();

        let Ok(mut map) = self.sessions.lock() else {
            // Mutex poisoned fallback: fail conservative on the candidate flags alone.
            let score = action_flags.risk_score();
            return SessionRiskAssessment {
                prior_flags: RuleOfTwoFlags::default(),
                effective_flags: action_flags,
                risk_score: score,
                requires_human_approval: action_flags.all_three(),
            };
        };

        // Prune expired entry for `key` if its TTL elapsed (`SEC-084`).
        let prior_flags = match map.get(key).copied() {
            Some(entry) if now_ms.saturating_sub(entry.last_updated_ms) < ttl_ms => entry.flags,
            Some(_) => {
                map.remove(key);
                RuleOfTwoFlags::default()
            }
            None => RuleOfTwoFlags::default(),
        };

        let effective_flags = prior_flags.union(action_flags);
        let risk_score = effective_flags.risk_score();
        let requires_human_approval = effective_flags.all_three();

        if commit {
            enforce_bounded_capacity(&mut map, now_ms, ttl_ms, self.cfg.max_sessions);
            map.insert(
                key.clone(),
                SessionEntry {
                    flags: effective_flags,
                    last_updated_ms: now_ms,
                },
            );
        }

        SessionRiskAssessment {
            prior_flags,
            effective_flags,
            risk_score,
            requires_human_approval,
        }
    }

    /// Returns active `(flags, risk_score)` for `key`, or `None` if absent/expired (`SEC-084`).
    #[must_use]
    pub fn peek(&self, key: &SessionRiskKey) -> Option<(RuleOfTwoFlags, i64)> {
        let now_ms = self.clock.unix_millis();
        let ttl_ms = i64::try_from(self.cfg.ttl.as_millis()).unwrap_or(i64::MAX);
        let Ok(mut map) = self.sessions.lock() else {
            return None;
        };
        match map.get(key).copied() {
            Some(entry) if now_ms.saturating_sub(entry.last_updated_ms) < ttl_ms => {
                Some((entry.flags, entry.flags.risk_score()))
            }
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    /// Builds a Cedar [`AuthzContext`] pinned to `req_ctx` with live Rule-of-Two `riskScore` (`SEC-084`).
    #[must_use]
    pub fn authz_context(
        &self,
        req_ctx: &RequestContext,
        context_id: impl Into<String>,
        action: ActionRiskContribution,
        commit: bool,
    ) -> (AuthzContext, SessionRiskAssessment) {
        let key = SessionRiskKey::new(
            req_ctx.tenant.clone(),
            req_ctx.principal.id.clone(),
            context_id,
        );
        let assessment = self.evaluate_and_update(&key, action, commit);
        let ctx =
            AuthzContext::from_request(req_ctx, self.clock.unix_millis(), assessment.risk_score);
        (ctx, assessment)
    }

    /// Builds a Cedar [`AuthzContext`] for `principal` with live Rule-of-Two `riskScore` (`SEC-084`).
    #[must_use]
    pub fn authz_context_for_principal(
        &self,
        principal: &Principal,
        context_id: impl Into<String>,
        action: ActionRiskContribution,
        commit: bool,
    ) -> (AuthzContext, SessionRiskAssessment) {
        let key = SessionRiskKey::new(principal.tenant.clone(), principal.id.clone(), context_id);
        let assessment = self.evaluate_and_update(&key, action, commit);
        let ctx = AuthzContext {
            tenant: principal.tenant.clone(),
            time: self.clock.unix_millis(),
            hop: 0,
            risk_score: assessment.risk_score,
            approved: false,
            approver_is_different: false,
            scopes: principal.scopes.clone(),
        };
        (ctx, assessment)
    }
}

fn enforce_bounded_capacity(
    map: &mut BTreeMap<SessionRiskKey, SessionEntry>,
    now_ms: i64,
    ttl_ms: i64,
    max_sessions: usize,
) {
    let cap = max_sessions.max(1);
    if map.len() < cap {
        return;
    }

    let expired_keys: Vec<SessionRiskKey> = map
        .iter()
        .filter(|(_, entry)| now_ms.saturating_sub(entry.last_updated_ms) >= ttl_ms)
        .map(|(k, _)| k.clone())
        .collect();
    for key in expired_keys {
        map.remove(&key);
    }

    while map.len() >= cap {
        if let Some(oldest_key) = map
            .iter()
            .min_by_key(|(_, entry)| entry.last_updated_ms)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest_key);
        } else {
            break;
        }
    }
}
