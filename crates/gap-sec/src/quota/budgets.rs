//! Per-tenant storage (store rows, blob bytes) and S1 model-inference budgets + atomic metrics (`SEC-050`, `INV-09`).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;

/// Default maximum context-store rows permitted per tenant (`5,000,000`, `SEC-050`).
pub const DEFAULT_MAX_STORE_ROWS_PER_TENANT: u64 = 5_000_000;

/// Default maximum blob storage permitted per tenant (`1 GiB`, `SEC-050`).
pub const DEFAULT_MAX_BLOB_BYTES_PER_TENANT: u64 = 1_073_741_824;

/// Default maximum S1 inference calls per minute per tenant (`600 calls/min`, `SEC-050`).
pub const DEFAULT_MAX_S1_CALLS_PER_MIN_PER_TENANT: u32 = 600;

const MINUTE_MS: i64 = 60_000;

/// Per-tenant storage accounting snapshot (`SEC-050`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TenantStorageUsage {
    /// Currently allocated graph/context rows for this tenant.
    pub rows: u64,
    /// Currently allocated blob bytes for this tenant.
    pub blob_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct S1Window {
    window_start_ms: i64,
    used_calls: u32,
}

#[derive(Debug, Default)]
struct TenantBudgetState {
    storage: BTreeMap<TenantId, TenantStorageUsage>,
    s1_windows: BTreeMap<TenantId, S1Window>,
}

/// Enforces per-tenant storage (`rows`, `blob_bytes`) and S1 calls/min budgets (`SEC-050`, `INV-09`).
#[derive(Debug)]
pub struct TenantResourceBudgetStore {
    max_store_rows: u64,
    max_blob_bytes: u64,
    max_s1_calls_per_min: u32,
    state: Mutex<TenantBudgetState>,
}

impl TenantResourceBudgetStore {
    /// Creates a new per-tenant storage & S1 budget tracker.
    #[must_use]
    pub fn new(max_store_rows: u64, max_blob_bytes: u64, max_s1_calls_per_min: u32) -> Self {
        Self {
            max_store_rows,
            max_blob_bytes,
            max_s1_calls_per_min,
            state: Mutex::new(TenantBudgetState::default()),
        }
    }

    /// Reserves `add_rows` and `add_blob_bytes` against `tenant`'s storage ceiling (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::RateLimited`] if either `rows` or `blob_bytes` exceeds the tenant ceiling.
    pub fn allocate_storage(
        &self,
        tenant: &TenantId,
        add_rows: u64,
        add_blob_bytes: u64,
    ) -> GapResult<TenantStorageUsage> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = guard.storage.get(tenant).copied().unwrap_or_default();
        let next_rows = current
            .rows
            .checked_add(add_rows)
            .ok_or_else(|| GapError::new(ErrorCode::RateLimited, "tenant row storage overflow"))?;
        let next_bytes = current
            .blob_bytes
            .checked_add(add_blob_bytes)
            .ok_or_else(|| GapError::new(ErrorCode::RateLimited, "tenant blob storage overflow"))?;

        if next_rows > self.max_store_rows {
            return Err(
                GapError::new(ErrorCode::RateLimited, "tenant store row quota exceeded")
                    .with_retry(false, None),
            );
        }
        if next_bytes > self.max_blob_bytes {
            return Err(
                GapError::new(ErrorCode::RateLimited, "tenant blob byte quota exceeded")
                    .with_retry(false, None),
            );
        }

        let updated = TenantStorageUsage {
            rows: next_rows,
            blob_bytes: next_bytes,
        };
        guard.storage.insert(tenant.clone(), updated);
        Ok(updated)
    }

    /// Releases previously allocated storage capacity for `tenant` (e.g. after purge/retention).
    pub fn release_storage(&self, tenant: &TenantId, sub_rows: u64, sub_blob_bytes: u64) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(usage) = guard.storage.get_mut(tenant) {
            usage.rows = usage.rows.saturating_sub(sub_rows);
            usage.blob_bytes = usage.blob_bytes.saturating_sub(sub_blob_bytes);
            if usage.rows == 0 && usage.blob_bytes == 0 {
                guard.storage.remove(tenant);
            }
        }
    }

    /// Returns current storage usage for `tenant`.
    #[must_use]
    pub fn storage_usage(&self, tenant: &TenantId) -> TenantStorageUsage {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .storage
            .get(tenant)
            .copied()
            .unwrap_or_default()
    }

    /// Consumes 1 S1 model-inference call from `tenant`'s calls/min budget (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::RateLimited`] with `retry.after_ms` when the minute budget is spent.
    pub fn acquire_s1_call(&self, tenant: &TenantId, now_ms: i64) -> GapResult<()> {
        if self.max_s1_calls_per_min == 0 {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "tenant S1 inference budget is disabled",
            )
            .with_retry(false, None));
        }

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = guard.s1_windows.entry(tenant.clone()).or_insert(S1Window {
            window_start_ms: now_ms,
            used_calls: 0,
        });

        let elapsed = now_ms.saturating_sub(entry.window_start_ms);
        if elapsed >= MINUTE_MS {
            entry.window_start_ms = now_ms;
            entry.used_calls = 0;
        }

        if entry.used_calls >= self.max_s1_calls_per_min {
            let remaining = MINUTE_MS.saturating_sub(elapsed).clamp(1, MINUTE_MS);
            let after_ms = u32::try_from(remaining).unwrap_or(60_000);
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "tenant S1 inference calls/min budget exceeded",
            )
            .with_retry(true, Some(after_ms)));
        }

        entry.used_calls += 1;
        Ok(())
    }
}

/// Lock-free atomic quota telemetry counters (`SEC-050`, `INV-02`).
#[derive(Debug, Default)]
pub struct QuotaMetrics {
    pub(crate) rate_allowed: AtomicU64,
    pub(crate) rate_rejected: AtomicU64,
    pub(crate) cost_allowed: AtomicU64,
    pub(crate) cost_rejected: AtomicU64,
    pub(crate) cost_units_reserved: AtomicU64,
    pub(crate) cost_units_refunded: AtomicU64,
    pub(crate) branch_share_allowed: AtomicU64,
    pub(crate) branch_share_rejected: AtomicU64,
    pub(crate) ingest_allowed: AtomicU64,
    pub(crate) ingest_rejected: AtomicU64,
    pub(crate) storage_allowed: AtomicU64,
    pub(crate) storage_rejected: AtomicU64,
    pub(crate) s1_allowed: AtomicU64,
    pub(crate) s1_rejected: AtomicU64,
}

impl QuotaMetrics {
    /// Captures a point-in-time metrics snapshot suitable for Prometheus export.
    #[must_use]
    pub fn snapshot(&self) -> QuotaMetricsSnapshot {
        QuotaMetricsSnapshot {
            rate_allowed: self.rate_allowed.load(Ordering::Relaxed),
            rate_rejected: self.rate_rejected.load(Ordering::Relaxed),
            cost_allowed: self.cost_allowed.load(Ordering::Relaxed),
            cost_rejected: self.cost_rejected.load(Ordering::Relaxed),
            cost_units_reserved: self.cost_units_reserved.load(Ordering::Relaxed),
            cost_units_refunded: self.cost_units_refunded.load(Ordering::Relaxed),
            branch_share_allowed: self.branch_share_allowed.load(Ordering::Relaxed),
            branch_share_rejected: self.branch_share_rejected.load(Ordering::Relaxed),
            ingest_allowed: self.ingest_allowed.load(Ordering::Relaxed),
            ingest_rejected: self.ingest_rejected.load(Ordering::Relaxed),
            storage_allowed: self.storage_allowed.load(Ordering::Relaxed),
            storage_rejected: self.storage_rejected.load(Ordering::Relaxed),
            s1_allowed: self.s1_allowed.load(Ordering::Relaxed),
            s1_rejected: self.s1_rejected.load(Ordering::Relaxed),
        }
    }
}

/// Immutable point-in-time snapshot of [`QuotaMetrics`] (`SEC-050`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaMetricsSnapshot {
    /// Total principal requests permitted.
    pub rate_allowed: u64,
    /// Total principal requests rejected (`GAP_RATE_LIMITED`).
    pub rate_rejected: u64,
    /// Total planned cost reservations granted.
    pub cost_allowed: u64,
    /// Total planned cost reservations rejected (`GAP_RATE_LIMITED`).
    pub cost_rejected: u64,
    /// Cumulative planned cost units reserved.
    pub cost_units_reserved: u64,
    /// Cumulative unused cost units refunded at reconciliation.
    pub cost_units_refunded: u64,
    /// Total branch concurrency slots acquired.
    pub branch_share_allowed: u64,
    /// Total branch concurrency acquisitions rejected (`GAP_RATE_LIMITED`).
    pub branch_share_rejected: u64,
    /// Total ingestion concurrency slots acquired.
    pub ingest_allowed: u64,
    /// Total ingestion concurrency acquisitions rejected (`GAP_RATE_LIMITED`).
    pub ingest_rejected: u64,
    /// Total storage allocations approved.
    pub storage_allowed: u64,
    /// Total storage allocations rejected (`GAP_RATE_LIMITED`).
    pub storage_rejected: u64,
    /// Total S1 inference calls permitted.
    pub s1_allowed: u64,
    /// Total S1 inference calls rejected (`GAP_RATE_LIMITED`).
    pub s1_rejected: u64,
}
