//! Per-tenant and per-principal quotas, rate limits, concurrency shares, and cost budgets (`SEC-050`, `RUN-050`, `INV-09`).

pub mod bucket;
pub mod budgets;
pub mod concurrency;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use gap_core::clock::{Clock, SystemClock};
use gap_core::error::GapResult;
use gap_core::ids::{BranchId, TenantId};

pub use bucket::{
    CostReservation, DEFAULT_COST_UNITS_PER_MIN, DEFAULT_REQUEST_BURST, DEFAULT_REQUESTS_PER_SEC,
    SubjectLimitStore, SubjectQuotaKey,
};
pub use budgets::{
    DEFAULT_MAX_BLOB_BYTES_PER_TENANT, DEFAULT_MAX_S1_CALLS_PER_MIN_PER_TENANT,
    DEFAULT_MAX_STORE_ROWS_PER_TENANT, QuotaMetrics, QuotaMetricsSnapshot,
    TenantResourceBudgetStore, TenantStorageUsage,
};
pub use concurrency::{
    BranchConcurrencyPermit, ConcurrencyQuotaStore, DEFAULT_BRANCH_TENANT_SHARE_BPS,
    DEFAULT_MAX_INGEST_CONCURRENCY_PER_TENANT, IngestConcurrencyPermit, TenantBranchKey,
};

/// Gateway quota and rate-limit configuration (`SEC-050`, `RUN-050`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaConfig {
    /// Token bucket refill rate per `(tenant, principal)` in requests/sec (`default 50`).
    pub requests_per_sec: u32,
    /// Token bucket burst capacity per `(tenant, principal)` (`default 100`).
    pub request_burst: u32,
    /// Planned cost-unit quota per `(tenant, principal)` per minute (`default 600_000`).
    pub cost_units_per_min: u64,
    /// Maximum tenant concurrency share on a branch in basis points (`default 5_000` = `50%`).
    pub branch_tenant_share_bps: u16,
    /// Maximum concurrent ingestion jobs per tenant (`default 4`).
    pub max_ingest_concurrency_per_tenant: usize,
    /// Maximum store rows per tenant (`default 5_000_000`).
    pub max_store_rows_per_tenant: u64,
    /// Maximum blob bytes per tenant (`default 1 GiB`).
    pub max_blob_bytes_per_tenant: u64,
    /// Maximum S1 inference calls per minute per tenant (`default 600`).
    pub max_s1_calls_per_min_per_tenant: u32,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            requests_per_sec: DEFAULT_REQUESTS_PER_SEC,
            request_burst: DEFAULT_REQUEST_BURST,
            cost_units_per_min: DEFAULT_COST_UNITS_PER_MIN,
            branch_tenant_share_bps: DEFAULT_BRANCH_TENANT_SHARE_BPS,
            max_ingest_concurrency_per_tenant: DEFAULT_MAX_INGEST_CONCURRENCY_PER_TENANT,
            max_store_rows_per_tenant: DEFAULT_MAX_STORE_ROWS_PER_TENANT,
            max_blob_bytes_per_tenant: DEFAULT_MAX_BLOB_BYTES_PER_TENANT,
            max_s1_calls_per_min_per_tenant: DEFAULT_MAX_S1_CALLS_PER_MIN_PER_TENANT,
        }
    }
}

/// Unified quota enforcer across rate, cost units, branch concurrency share, ingestion concurrency,
/// storage ceilings, and S1 inference budgets (`SEC-050`, `RUN-050`, `INV-09`).
#[derive(Debug)]
pub struct QuotaEnforcer {
    config: QuotaConfig,
    clock: Arc<dyn Clock>,
    subjects: SubjectLimitStore,
    concurrency: ConcurrencyQuotaStore,
    budgets: TenantResourceBudgetStore,
    metrics: QuotaMetrics,
}

impl QuotaEnforcer {
    /// Creates a [`QuotaEnforcer`] using the system wall/monotonic clock.
    #[must_use]
    pub fn new(config: QuotaConfig) -> Self {
        Self::with_clock(config, Arc::new(SystemClock))
    }

    /// Creates a [`QuotaEnforcer`] parameterized by an explicit [`Clock`] for deterministic tests.
    #[must_use]
    pub fn with_clock(config: QuotaConfig, clock: Arc<dyn Clock>) -> Self {
        let subjects = SubjectLimitStore::new(
            config.requests_per_sec,
            config.request_burst,
            config.cost_units_per_min,
        );
        let concurrency = ConcurrencyQuotaStore::new(
            config.branch_tenant_share_bps,
            config.max_ingest_concurrency_per_tenant,
        );
        let budgets = TenantResourceBudgetStore::new(
            config.max_store_rows_per_tenant,
            config.max_blob_bytes_per_tenant,
            config.max_s1_calls_per_min_per_tenant,
        );
        Self {
            config,
            clock,
            subjects,
            concurrency,
            budgets,
            metrics: QuotaMetrics::default(),
        }
    }

    /// Configured limits.
    #[must_use]
    pub const fn config(&self) -> &QuotaConfig {
        &self.config
    }

    /// Point-in-time atomic metrics snapshot (`SEC-050`).
    #[must_use]
    pub fn metrics(&self) -> QuotaMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Enforces `(tenant, principal)` token-bucket rate limit (`RUN-050`).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` with `retry.after_ms` on exhaustion.
    pub fn check_rate_limit(&self, tenant: &TenantId, principal_id: &str) -> GapResult<()> {
        let key = SubjectQuotaKey::new(tenant.clone(), principal_id);
        let now_ms = self.clock.now_timestamp().unix_millis();
        match self.subjects.check_rate_limit(&key, now_ms) {
            Ok(()) => {
                self.metrics.rate_allowed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(err) => {
                self.metrics.rate_rejected.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Reserves `planned_cost` units against `(tenant, principal)`'s per-minute cost budget (`RUN-050`).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` with `retry.after_ms` when the minute budget would be exceeded.
    pub fn reserve_cost(
        &self,
        tenant: &TenantId,
        principal_id: &str,
        planned_cost: u64,
    ) -> GapResult<CostReservation> {
        let key = SubjectQuotaKey::new(tenant.clone(), principal_id);
        let now_ms = self.clock.now_timestamp().unix_millis();
        match self.subjects.reserve_cost(&key, planned_cost, now_ms) {
            Ok(reservation) => {
                self.metrics.cost_allowed.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .cost_units_reserved
                    .fetch_add(planned_cost, Ordering::Relaxed);
                Ok(reservation)
            }
            Err(err) => {
                self.metrics.cost_rejected.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Reconciles `reservation` against `actual_cost`, refunding unused budget (`RUN-050`).
    pub fn reconcile_cost(&self, reservation: CostReservation, actual_cost: u64) {
        let refunded = self.subjects.reconcile_cost(reservation, actual_cost);
        if refunded > 0 {
            self.metrics
                .cost_units_refunded
                .fetch_add(refunded, Ordering::Relaxed);
        }
    }

    /// Returns currently remaining cost-unit budget for `(tenant, principal)` in the active minute.
    #[must_use]
    pub fn remaining_cost_units(&self, tenant: &TenantId, principal_id: &str) -> u64 {
        let key = SubjectQuotaKey::new(tenant.clone(), principal_id);
        let now_ms = self.clock.now_timestamp().unix_millis();
        self.subjects.remaining_cost_units(&key, now_ms)
    }

    /// Acquires an RAII [`BranchConcurrencyPermit`] enforcing `<= 50%` tenant share on `branch` (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` if the tenant's branch share or branch total capacity is saturated.
    pub fn acquire_branch_slot(
        &self,
        tenant: &TenantId,
        branch: &BranchId,
        branch_capacity: usize,
    ) -> GapResult<BranchConcurrencyPermit> {
        match self
            .concurrency
            .acquire_branch_slot(tenant, branch, branch_capacity)
        {
            Ok(permit) => {
                self.metrics
                    .branch_share_allowed
                    .fetch_add(1, Ordering::Relaxed);
                Ok(permit)
            }
            Err(err) => {
                self.metrics
                    .branch_share_rejected
                    .fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Acquires an RAII [`IngestConcurrencyPermit`] against `tenant`'s ingestion budget (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` when active tenant ingestion jobs equal the configured ceiling.
    pub fn acquire_ingest_slot(&self, tenant: &TenantId) -> GapResult<IngestConcurrencyPermit> {
        match self.concurrency.acquire_ingest_slot(tenant) {
            Ok(permit) => {
                self.metrics.ingest_allowed.fetch_add(1, Ordering::Relaxed);
                Ok(permit)
            }
            Err(err) => {
                self.metrics.ingest_rejected.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Reserves `add_rows` and `add_blob_bytes` against `tenant`'s storage quota (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` if rows or blob bytes exceed tenant storage limits.
    pub fn allocate_storage(
        &self,
        tenant: &TenantId,
        add_rows: u64,
        add_blob_bytes: u64,
    ) -> GapResult<TenantStorageUsage> {
        match self
            .budgets
            .allocate_storage(tenant, add_rows, add_blob_bytes)
        {
            Ok(usage) => {
                self.metrics.storage_allowed.fetch_add(1, Ordering::Relaxed);
                Ok(usage)
            }
            Err(err) => {
                self.metrics
                    .storage_rejected
                    .fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Releases `sub_rows` and `sub_blob_bytes` from `tenant`'s storage accounting (`SEC-050`).
    pub fn release_storage(&self, tenant: &TenantId, sub_rows: u64, sub_blob_bytes: u64) {
        self.budgets
            .release_storage(tenant, sub_rows, sub_blob_bytes);
    }

    /// Current storage accounting for `tenant`.
    #[must_use]
    pub fn storage_usage(&self, tenant: &TenantId) -> TenantStorageUsage {
        self.budgets.storage_usage(tenant)
    }

    /// Consumes 1 S1 model-inference call from `tenant`'s calls/min budget (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns `GAP_RATE_LIMITED` with `retry.after_ms` when `max_s1_calls_per_min_per_tenant` is spent.
    pub fn acquire_s1_call(&self, tenant: &TenantId) -> GapResult<()> {
        let now_ms = self.clock.now_timestamp().unix_millis();
        match self.budgets.acquire_s1_call(tenant, now_ms) {
            Ok(()) => {
                self.metrics.s1_allowed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(err) => {
                self.metrics.s1_rejected.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// Active concurrency count for `(tenant, branch)`.
    #[must_use]
    pub fn active_branch_tenant_slots(&self, tenant: &TenantId, branch: &BranchId) -> usize {
        self.concurrency.active_branch_tenant_slots(tenant, branch)
    }

    /// Active ingestion concurrency count for `tenant`.
    #[must_use]
    pub fn active_ingest_slots(&self, tenant: &TenantId) -> usize {
        self.concurrency.active_ingest_slots(tenant)
    }
}
