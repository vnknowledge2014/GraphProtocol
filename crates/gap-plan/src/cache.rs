//! Tenant-isolated, lock-free `moka` execution plan cache (`RUN-014`, `INV-01`, `INV-02`,
//! `INV-09`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_core::error::GapResult;
use gap_core::snapshot::{PlanCacheKey, Snapshot};
use gap_ql::{CostEstimate, ValidatedOperation, enforce_operation_cost};
use moka::sync::Cache;
use serde::{Deserialize, Serialize};

use crate::ir::{Plan, plan_compact_json};
use crate::planner::{PlanConfig, check_query_operation_kind, plan_with_verified_cost};

/// Default logical plan-cache entry capacity (`plan_cache.max_entries = 10_000`, `RUN-014`).
pub const DEFAULT_PLAN_CACHE_MAX_ENTRIES: u64 = 10_000;

/// Default time-to-live per cached plan in seconds (`1 h`, `RUN-014`).
pub const DEFAULT_PLAN_CACHE_TTL_SECS: u64 = 3_600;

/// Default estimated serialized bytes per plan used to convert [`PlanCacheConfig::max_entries`]
/// into `moka`'s byte-weighted `max_capacity` when [`PlanCacheConfig::max_weight_bytes`] is `None`.
pub const DEFAULT_ESTIMATED_PLAN_BYTES: u64 = 4_096;

/// Configuration for [`PlanCache`] (`RUN-014`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCacheConfig {
    /// Logical entry capacity (`plan_cache.max_entries`, default `10_000`, `RUN-014`).
    pub max_entries: u64,
    /// Time-to-live per cached plan (`1 h` default, `RUN-014`).
    pub ttl: Duration,
    /// Optional explicit total serialized-byte cap for `moka`'s weigher. When `None`, defaults to
    /// `max_entries.saturating_mul(DEFAULT_ESTIMATED_PLAN_BYTES)`.
    pub max_weight_bytes: Option<u64>,
}

impl Default for PlanCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_PLAN_CACHE_MAX_ENTRIES,
            ttl: Duration::from_secs(DEFAULT_PLAN_CACHE_TTL_SECS),
            max_weight_bytes: None,
        }
    }
}

/// Outcome reported on the `gap.plan_cache` span/telemetry attribute (`Brief §3.6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanCacheStatus {
    /// Plan was served from the `moka` cache (`"hit"`).
    Hit,
    /// Plan was compiled on a cache miss (`"miss"`).
    Miss,
}

impl PlanCacheStatus {
    /// Canonical telemetry attribute key (`"gap.plan_cache"`).
    pub const ATTRIBUTE_KEY: &'static str = "gap.plan_cache";

    /// Returns `"hit"` or `"miss"` (`Brief §3.6`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
        }
    }
}

/// Result of [`PlanCache::get_or_plan`], pairing the shared [`Plan`] with its cache status and
/// verified per-request cost estimate.
#[derive(Debug, Clone)]
pub struct CachedPlan {
    /// Shared immutable [`Plan`] (`RUN-014`).
    pub plan: Arc<Plan>,
    /// Cache lookup outcome (`"hit"` or `"miss"` for `gap.plan_cache`).
    pub status: PlanCacheStatus,
    /// Request-specific `GQL-041` cost estimate verified before returning (`RUN-005`).
    pub request_cost: CostEstimate,
}

fn plan_weight_bytes(plan: &Plan) -> u32 {
    let serialized_len = plan_compact_json(plan).len().max(1);
    u32::try_from(serialized_len).unwrap_or(u32::MAX)
}

/// Concurrent `moka`-backed execution plan cache keyed by [`PlanCacheKey`] (`RUN-014`, `INV-02`,
/// `INV-09`).
///
/// Concurrent misses on the same [`PlanCacheKey`] are coalesced into a single evaluation of the
/// planner closure via `moka::sync::Cache::entry(...).or_try_insert_with(...)`. Read and write
/// paths take zero locks in `gap-plan` (`INV-02`).
#[derive(Debug)]
pub struct PlanCache {
    cache: Cache<PlanCacheKey, Arc<Plan>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for PlanCache {
    fn default() -> Self {
        Self::new(PlanCacheConfig::default())
    }
}

impl PlanCache {
    /// Creates a new [`PlanCache`] with `config`.
    #[must_use]
    pub fn new(config: PlanCacheConfig) -> Self {
        let effective_max_weight = config.max_weight_bytes.unwrap_or_else(|| {
            config
                .max_entries
                .saturating_mul(DEFAULT_ESTIMATED_PLAN_BYTES)
        });

        let cache = Cache::builder()
            .max_capacity(effective_max_weight)
            .time_to_live(config.ttl)
            .weigher(|_key: &PlanCacheKey, value: &Arc<Plan>| plan_weight_bytes(value))
            .build();

        Self {
            cache,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Performs a pure lock-free cache lookup without planning on miss, updating `hits`/`misses`.
    #[must_use]
    pub fn get(&self, key: &PlanCacheKey) -> Option<Arc<Plan>> {
        if let Some(found) = self.cache.get(key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(found)
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Looks up or compiles a [`Plan`] for `(snapshot, document_hash, document)`, coalescing
    /// concurrent misses on the same [`PlanCacheKey`] (`RUN-014`, `INV-01`, `INV-02`, `INV-09`).
    ///
    /// Per-request `GQL-041` cost caps (`max_cost_units` and caller `@budget` / request budget) are
    /// always enforced via [`enforce_operation_cost`] even when the plan skeleton is warm in cache
    /// (`RUN-005`).
    ///
    /// # Errors
    ///
    /// Returns [`gap_core::error::GapError`] if cost enforcement or query planning fails.
    pub fn get_or_plan(
        &self,
        snapshot: &Snapshot,
        document_hash: [u8; 32],
        document: &ValidatedOperation,
        config: &PlanConfig,
    ) -> GapResult<CachedPlan> {
        check_query_operation_kind(document)?;
        let request_cost = enforce_operation_cost(
            document,
            &config.limits,
            config.request_budget_cost_units,
            config.cost_model,
        )?;
        let key =
            PlanCacheKey::from_snapshot(snapshot, document_hash, document.operation_name.clone());
        let cost_for_miss = request_cost.clone();
        let (plan, status) = self.get_or_plan_with(key, || {
            plan_with_verified_cost(snapshot, document, cost_for_miss)
        })?;

        tracing::Span::current().record(PlanCacheStatus::ATTRIBUTE_KEY, status.as_str());
        tracing::debug!(
            gap.plan_cache = status.as_str(),
            generation = snapshot.generation,
            policy_version = snapshot.policy_version,
            "resolved query plan"
        );

        Ok(CachedPlan {
            plan,
            status,
            request_cost,
        })
    }

    /// Coalescing cache lookup/insertion over an explicit [`PlanCacheKey`] and planner closure.
    ///
    /// Guarantees via `moka`'s entry selector that concurrent calls for the same missing key
    /// evaluate `init` once (`entry.is_fresh() == true` -> [`PlanCacheStatus::Miss`]; waiting
    /// callers observe `entry.is_fresh() == false` -> [`PlanCacheStatus::Hit`]).
    ///
    /// # Errors
    ///
    /// Returns [`gap_core::error::GapError`] if `init` fails on a cache miss (errors are never
    /// cached).
    pub fn get_or_plan_with<F>(
        &self,
        key: PlanCacheKey,
        init: F,
    ) -> GapResult<(Arc<Plan>, PlanCacheStatus)>
    where
        F: FnOnce() -> GapResult<Plan>,
    {
        match self
            .cache
            .entry(key)
            .or_try_insert_with(|| init().map(Arc::new))
        {
            Ok(entry) => {
                let status = if entry.is_fresh() {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    PlanCacheStatus::Miss
                } else {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    PlanCacheStatus::Hit
                };
                Ok((entry.into_value(), status))
            }
            Err(arc_err) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                Err((*arc_err).clone())
            }
        }
    }

    /// Total cache hits observed across all threads.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Total cache misses observed across all threads.
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Approximate number of live entries currently held by `moka`.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Approximate total byte weight of live entries held by `moka`.
    #[must_use]
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Synchronously drives pending `moka` eviction and maintenance tasks.
    pub fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks();
    }

    /// Marks all currently cached plans invalid.
    pub fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }
}
