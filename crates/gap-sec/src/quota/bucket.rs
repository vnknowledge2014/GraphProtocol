//! Per-`(TenantId, principal_id)` token-bucket rate limiting and cost-unit budgeting (`RUN-050`, `INV-09`, `TM-11`).

use std::collections::BTreeMap;
use std::sync::Mutex;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;

/// Default steady-state principal request rate (`50 rps`, `RUN-050`).
pub const DEFAULT_REQUESTS_PER_SEC: u32 = 50;

/// Default principal request burst size (`100` requests, `RUN-050`).
pub const DEFAULT_REQUEST_BURST: u32 = 100;

/// Default per-principal planned cost-unit quota per minute (`600,000 units/min`, `RUN-050`).
pub const DEFAULT_COST_UNITS_PER_MIN: u64 = 600_000;

const MILLITOKENS_PER_REQ: u64 = 1_000;
const MINUTE_MS: i64 = 60_000;
const MAX_TRACKED_SUBJECTS: usize = 65_536;

/// Tenant-scoped principal lookup key (`INV-09`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubjectQuotaKey {
    /// Owning tenant identifier.
    pub tenant: TenantId,
    /// Authenticated principal identifier (`Principal::id`).
    pub principal_id: String,
}

impl SubjectQuotaKey {
    /// Creates a tenant-bound principal quota key (`INV-09`).
    #[must_use]
    pub fn new(tenant: TenantId, principal_id: impl Into<String>) -> Self {
        Self {
            tenant,
            principal_id: principal_id.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct TokenBucketEntry {
    millitokens: u64,
    last_refill_ms: i64,
}

#[derive(Debug, Clone)]
struct CostWindowEntry {
    window_start_ms: i64,
    remaining_units: u64,
}

/// Receipt returned when reserving planned query cost against a principal's minute budget (`RUN-050`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostReservation {
    /// Subject whose budget was debited.
    pub subject: SubjectQuotaKey,
    /// Minute window identifier used for reconciliation.
    pub window_start_ms: i64,
    /// Planned cost units deducted upfront.
    pub reserved_units: u64,
}

#[derive(Debug)]
struct RateAndCostState {
    buckets: BTreeMap<SubjectQuotaKey, TokenBucketEntry>,
    cost_windows: BTreeMap<SubjectQuotaKey, CostWindowEntry>,
}

/// Concurrent store enforcing `(tenant, principal)` token-bucket rate and cost-unit limits (`RUN-050`).
#[derive(Debug)]
pub struct SubjectLimitStore {
    requests_per_sec: u32,
    request_burst: u32,
    cost_units_per_min: u64,
    state: Mutex<RateAndCostState>,
}

impl SubjectLimitStore {
    /// Creates a new subject rate/cost limiter store.
    #[must_use]
    pub fn new(requests_per_sec: u32, request_burst: u32, cost_units_per_min: u64) -> Self {
        Self {
            requests_per_sec,
            request_burst,
            cost_units_per_min,
            state: Mutex::new(RateAndCostState {
                buckets: BTreeMap::new(),
                cost_windows: BTreeMap::new(),
            }),
        }
    }

    /// Consumes 1 request token for `(tenant, principal)` at `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::RateLimited`] with retry guidance (`retry.after_ms`) if depleted (`RUN-050`).
    pub fn check_rate_limit(&self, key: &SubjectQuotaKey, now_ms: i64) -> GapResult<()> {
        if self.requests_per_sec == 0 || self.request_burst == 0 {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "principal request quota is set to zero",
            )
            .with_retry(true, Some(1_000)));
        }

        let cap_milli = u64::from(self.request_burst).saturating_mul(MILLITOKENS_PER_REQ);
        let rate_per_ms = u64::from(self.requests_per_sec);

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        evict_oldest_if_full(&mut guard.buckets, key, |entry| entry.last_refill_ms);

        let entry = guard
            .buckets
            .entry(key.clone())
            .or_insert_with(|| TokenBucketEntry {
                millitokens: cap_milli,
                last_refill_ms: now_ms,
            });

        let elapsed_ms = u64::try_from(now_ms.saturating_sub(entry.last_refill_ms)).unwrap_or(0);
        if elapsed_ms > 0 {
            let added = elapsed_ms.saturating_mul(rate_per_ms);
            entry.millitokens = entry.millitokens.saturating_add(added).min(cap_milli);
            entry.last_refill_ms = now_ms;
        }

        if entry.millitokens >= MILLITOKENS_PER_REQ {
            entry.millitokens -= MILLITOKENS_PER_REQ;
            Ok(())
        } else {
            let deficit = MILLITOKENS_PER_REQ - entry.millitokens;
            let wait_ms = deficit.div_ceil(rate_per_ms).max(1);
            let wait_u32 = u32::try_from(wait_ms).unwrap_or(u32::MAX);
            Err(GapError::new(
                ErrorCode::RateLimited,
                "principal request rate limit exceeded",
            )
            .with_retry(true, Some(wait_u32)))
        }
    }

    /// Reserves `planned_cost` units against the principal's per-minute cost-unit budget (`RUN-050`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::RateLimited`] with `retry.after_ms` until the next minute window (`RUN-050`).
    pub fn reserve_cost(
        &self,
        key: &SubjectQuotaKey,
        planned_cost: u64,
        now_ms: i64,
    ) -> GapResult<CostReservation> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        evict_oldest_if_full(&mut guard.cost_windows, key, |entry| entry.window_start_ms);

        let cap = self.cost_units_per_min;
        let entry = guard
            .cost_windows
            .entry(key.clone())
            .or_insert_with(|| CostWindowEntry {
                window_start_ms: now_ms,
                remaining_units: cap,
            });

        let elapsed = now_ms.saturating_sub(entry.window_start_ms);
        if elapsed >= MINUTE_MS {
            entry.window_start_ms = now_ms;
            entry.remaining_units = cap;
        }

        if planned_cost > entry.remaining_units {
            let remaining_window = MINUTE_MS.saturating_sub(elapsed).clamp(1, MINUTE_MS);
            let after_ms = u32::try_from(remaining_window).unwrap_or(60_000);
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "principal per-minute cost budget exceeded",
            )
            .with_retry(true, Some(after_ms)));
        }

        entry.remaining_units -= planned_cost;
        Ok(CostReservation {
            subject: key.clone(),
            window_start_ms: entry.window_start_ms,
            reserved_units: planned_cost,
        })
    }

    /// Reconciles `reservation` with `actual_cost`, refunding unused units or debiting overages (`RUN-050`).
    ///
    /// Returns the number of refunded cost units (`> 0` when `actual_cost < reserved_units` in the same window).
    pub fn reconcile_cost(&self, reservation: CostReservation, actual_cost: u64) -> u64 {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = guard.cost_windows.get_mut(&reservation.subject) else {
            return 0;
        };
        if entry.window_start_ms != reservation.window_start_ms {
            return 0;
        }
        if actual_cost < reservation.reserved_units {
            let refund = reservation.reserved_units - actual_cost;
            let next = entry
                .remaining_units
                .saturating_add(refund)
                .min(self.cost_units_per_min);
            let applied = next.saturating_sub(entry.remaining_units);
            entry.remaining_units = next;
            applied
        } else {
            let extra = actual_cost - reservation.reserved_units;
            entry.remaining_units = entry.remaining_units.saturating_sub(extra);
            0
        }
    }

    /// Returns currently remaining cost-unit budget in the active window for `key`.
    #[must_use]
    pub fn remaining_cost_units(&self, key: &SubjectQuotaKey, now_ms: i64) -> u64 {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = guard.cost_windows.get(key) else {
            return self.cost_units_per_min;
        };
        if now_ms.saturating_sub(entry.window_start_ms) >= MINUTE_MS {
            self.cost_units_per_min
        } else {
            entry.remaining_units
        }
    }
}

/// Evicts the oldest entry deterministically when the tracked subject map hits capacity (`TM-11`).
fn evict_oldest_if_full<V>(
    map: &mut BTreeMap<SubjectQuotaKey, V>,
    key: &SubjectQuotaKey,
    timestamp_fn: impl Fn(&V) -> i64,
) {
    if map.len() >= MAX_TRACKED_SUBJECTS
        && !map.contains_key(key)
        && let Some(evict_key) = map
            .iter()
            .min_by_key(|(_, entry)| timestamp_fn(entry))
            .map(|(k, _)| k.clone())
    {
        map.remove(&evict_key);
    }
}
