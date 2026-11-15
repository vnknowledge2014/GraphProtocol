//! Per-`(TenantId, BranchId)` concurrency shares and per-tenant ingestion concurrency guards (`SEC-050`, `INV-09`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{BranchId, TenantId};

/// Default per-tenant share ceiling on any shared branch (`5_000 bps` = `50%`, `SEC-050`).
pub const DEFAULT_BRANCH_TENANT_SHARE_BPS: u16 = 5_000;

/// Default maximum concurrent ingestion jobs allowed per tenant (`SEC-050`).
pub const DEFAULT_MAX_INGEST_CONCURRENCY_PER_TENANT: usize = 4;

/// Key isolating branch concurrency limits per `(TenantId, BranchId)` (`INV-09`, `SEC-050`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantBranchKey {
    /// Owning tenant.
    pub tenant: TenantId,
    /// Target downstream branch.
    pub branch: BranchId,
}

#[derive(Debug, Default)]
pub(crate) struct ConcurrencyState {
    branch_total_active: BTreeMap<BranchId, usize>,
    branch_tenant_active: BTreeMap<TenantBranchKey, usize>,
    tenant_ingest_active: BTreeMap<TenantId, usize>,
}

/// Manages RAII concurrency permits for branch shares and per-tenant ingestion (`SEC-050`).
#[derive(Debug)]
pub struct ConcurrencyQuotaStore {
    branch_tenant_share_bps: u16,
    max_ingest_per_tenant: usize,
    state: Arc<Mutex<ConcurrencyState>>,
}

impl ConcurrencyQuotaStore {
    /// Creates a new concurrency quota tracker.
    #[must_use]
    pub fn new(branch_tenant_share_bps: u16, max_ingest_per_tenant: usize) -> Self {
        Self {
            branch_tenant_share_bps: branch_tenant_share_bps.min(10_000),
            max_ingest_per_tenant,
            state: Arc::new(Mutex::new(ConcurrencyState::default())),
        }
    }

    /// Computes maximum concurrent slots permitted for a single tenant on a branch of `branch_capacity` (`SEC-050`).
    #[must_use]
    pub fn max_tenant_slots_for_branch(&self, branch_capacity: usize) -> usize {
        if branch_capacity == 0 || self.branch_tenant_share_bps == 0 {
            return 0;
        }
        let share = (u128::from(branch_capacity as u64) * u128::from(self.branch_tenant_share_bps))
            / 10_000;
        usize::try_from(share).unwrap_or(branch_capacity).max(1)
    }

    /// Acquires an RAII [`BranchConcurrencyPermit`] respecting both branch capacity and the tenant's
    /// `<= 50%` share (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::RateLimited`] with retry guidance if either limit is saturated.
    pub fn acquire_branch_slot(
        &self,
        tenant: &TenantId,
        branch: &BranchId,
        branch_capacity: usize,
    ) -> GapResult<BranchConcurrencyPermit> {
        let tenant_cap = self.max_tenant_slots_for_branch(branch_capacity);
        if tenant_cap == 0 || branch_capacity == 0 {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "branch concurrency capacity is zero",
            )
            .with_retry(true, Some(100)));
        }

        let key = TenantBranchKey {
            tenant: tenant.clone(),
            branch: branch.clone(),
        };

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tenant_used = guard.branch_tenant_active.get(&key).copied().unwrap_or(0);
        if tenant_used >= tenant_cap {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "tenant branch concurrency share exceeded",
            )
            .with_retry(true, Some(50)));
        }

        let total_used = guard.branch_total_active.get(branch).copied().unwrap_or(0);
        if total_used >= branch_capacity {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "branch total concurrency capacity exhausted",
            )
            .with_retry(true, Some(50)));
        }

        guard
            .branch_tenant_active
            .insert(key.clone(), tenant_used + 1);
        guard
            .branch_total_active
            .insert(branch.clone(), total_used + 1);

        Ok(BranchConcurrencyPermit {
            state: Arc::clone(&self.state),
            key,
            armed: true,
        })
    }

    /// Acquires an RAII [`IngestConcurrencyPermit`] against `tenant`'s ingestion concurrency limit (`SEC-050`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::RateLimited`] if `max_ingest_per_tenant` is already active.
    pub fn acquire_ingest_slot(&self, tenant: &TenantId) -> GapResult<IngestConcurrencyPermit> {
        if self.max_ingest_per_tenant == 0 {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "tenant ingestion concurrency limit is zero",
            )
            .with_retry(true, Some(500)));
        }

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let used = guard.tenant_ingest_active.get(tenant).copied().unwrap_or(0);
        if used >= self.max_ingest_per_tenant {
            return Err(GapError::new(
                ErrorCode::RateLimited,
                "tenant ingestion concurrency limit exceeded",
            )
            .with_retry(true, Some(250)));
        }
        guard.tenant_ingest_active.insert(tenant.clone(), used + 1);
        Ok(IngestConcurrencyPermit {
            state: Arc::clone(&self.state),
            tenant: tenant.clone(),
            armed: true,
        })
    }

    /// Current active requests for `(tenant, branch)`.
    #[must_use]
    pub fn active_branch_tenant_slots(&self, tenant: &TenantId, branch: &BranchId) -> usize {
        let key = TenantBranchKey {
            tenant: tenant.clone(),
            branch: branch.clone(),
        };
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .branch_tenant_active
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    /// Current active ingestion jobs for `tenant`.
    #[must_use]
    pub fn active_ingest_slots(&self, tenant: &TenantId) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tenant_ingest_active
            .get(tenant)
            .copied()
            .unwrap_or(0)
    }
}

/// RAII concurrency guard automatically releasing a tenant's branch slot on drop (`SEC-050`, `RUN-031`).
#[derive(Debug)]
pub struct BranchConcurrencyPermit {
    state: Arc<Mutex<ConcurrencyState>>,
    key: TenantBranchKey,
    armed: bool,
}

impl Drop for BranchConcurrencyPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = guard.branch_tenant_active.get_mut(&self.key) {
            if *count <= 1 {
                guard.branch_tenant_active.remove(&self.key);
            } else {
                *count -= 1;
            }
        }
        if let Some(total) = guard.branch_total_active.get_mut(&self.key.branch) {
            if *total <= 1 {
                guard.branch_total_active.remove(&self.key.branch);
            } else {
                *total -= 1;
            }
        }
    }
}

/// RAII concurrency guard automatically releasing a tenant's ingestion slot on drop (`SEC-050`).
#[derive(Debug)]
pub struct IngestConcurrencyPermit {
    state: Arc<Mutex<ConcurrencyState>>,
    tenant: TenantId,
    armed: bool,
}

impl Drop for IngestConcurrencyPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = guard.tenant_ingest_active.get_mut(&self.tenant) {
            if *count <= 1 {
                guard.tenant_ingest_active.remove(&self.tenant);
            } else {
                *count -= 1;
            }
        }
    }
}
