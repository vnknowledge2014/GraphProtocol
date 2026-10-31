//! Tenant-isolated, lock-free `moka` execution plan cache (`RUN-014`, `INV-01`, `INV-02`,
//! `INV-09`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::{PlanCacheKey, Snapshot};
use gap_ql::{CostEstimate, ValidatedOperation};
use serde::{Deserialize, Serialize};

use crate::ir::Plan;
use crate::planner::PlanConfig;

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

/// Concurrent `moka`-backed execution plan cache keyed by [`PlanCacheKey`] (`RUN-014`, `INV-02`,
/// `INV-09`).
#[derive(Debug)]
pub struct PlanCache {
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
    pub fn new(_config: PlanCacheConfig) -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Looks up or compiles a [`Plan`] for `(snapshot, document_hash, document)`, coalescing
    /// concurrent misses on the same [`PlanCacheKey`] (`RUN-014`, `INV-01`, `INV-02`, `INV-09`).
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if cost enforcement or query planning fails.
    pub fn get_or_plan(
        &self,
        _snapshot: &Snapshot,
        _document_hash: [u8; 32],
        _document: &ValidatedOperation,
        _config: &PlanConfig,
    ) -> GapResult<CachedPlan> {
        Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "PlanCache::get_or_plan stub not yet implemented",
        ))
    }

    /// Coalescing cache lookup/insertion over an explicit [`PlanCacheKey`] and planner closure.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if `init` fails on a cache miss.
    pub fn get_or_plan_with<F>(
        &self,
        _key: PlanCacheKey,
        _init: F,
    ) -> GapResult<(Arc<Plan>, PlanCacheStatus)>
    where
        F: FnOnce() -> GapResult<Plan>,
    {
        Err(GapError::new(
            ErrorCode::UnsupportedFeature,
            "PlanCache::get_or_plan_with stub not yet implemented",
        ))
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
        0
    }

    /// Approximate total byte weight of live entries held by `moka`.
    #[must_use]
    pub fn weighted_size(&self) -> u64 {
        0
    }

    /// Synchronously drives pending `moka` eviction and maintenance tasks (useful in deterministic tests).
    pub fn run_pending_tasks(&self) {}

    /// Marks all currently cached plans invalid.
    pub fn invalidate_all(&self) {}
}
