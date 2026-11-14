//! Per-branch concurrency bulkheads and RAII permits (`WP-M3-03`, `RUN-032`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::BranchUid;
use gap_core::snapshot::Snapshot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default bounded wait before failing fast on a saturated branch bulkhead (`50 ms`, `RUN-032`).
pub const DEFAULT_BULKHEAD_QUEUE_WAIT_MS: u64 = 50;

/// RAII concurrency permit acquired from a [`BranchBulkhead`].
///
/// Releasing or dropping the permit (including on future cancellation) immediately decrements
/// `in_flight` and returns capacity to the bulkhead semaphore.
#[derive(Debug)]
pub struct BulkheadPermit {
    _permit: OwnedSemaphorePermit,
    in_flight: Arc<AtomicUsize>,
}

impl Drop for BulkheadPermit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bounded per-branch semaphore enforcing `RUN-032` bulkhead capacity and wait ceiling.
#[derive(Debug, Clone)]
pub struct BranchBulkhead {
    capacity: usize,
    queue_wait: Duration,
    sem: Arc<Semaphore>,
    in_flight: Arc<AtomicUsize>,
}

impl BranchBulkhead {
    /// Creates a new bulkhead with at least 1 concurrent permit and `queue_wait` max wait.
    #[must_use]
    pub fn new(capacity: usize, queue_wait: Duration) -> Self {
        let clamped = capacity.max(1);
        Self {
            capacity: clamped,
            queue_wait,
            sem: Arc::new(Semaphore::new(clamped)),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Creates a new bulkhead using [`DEFAULT_BULKHEAD_QUEUE_WAIT_MS`].
    #[must_use]
    pub fn with_default_wait(capacity: usize) -> Self {
        Self::new(
            capacity,
            Duration::from_millis(DEFAULT_BULKHEAD_QUEUE_WAIT_MS),
        )
    }

    /// Attempts to acquire a permit without waiting.
    pub fn try_acquire(&self, branch_id: &str) -> GapResult<BulkheadPermit> {
        let retry_ms = self.retry_after_ms();
        let permit = self.sem.clone().try_acquire_owned().map_err(|_| {
            GapError::new(
                ErrorCode::BranchUnavailable,
                format!(
                    "branch '{branch_id}' bulkhead saturated (capacity={})",
                    self.capacity
                ),
            )
            .with_retry(true, Some(retry_ms))
        })?;
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        Ok(BulkheadPermit {
            _permit: permit,
            in_flight: Arc::clone(&self.in_flight),
        })
    }

    /// Acquires a permit, waiting at most `self.queue_wait` before failing with
    /// `GAP_BRANCH_UNAVAILABLE(retry.afterMs)` (`RUN-032`).
    pub async fn acquire(&self, branch_id: &str) -> GapResult<BulkheadPermit> {
        if self.queue_wait.is_zero() {
            return self.try_acquire(branch_id);
        }

        let retry_ms = self.retry_after_ms();
        match tokio::time::timeout(self.queue_wait, self.sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => {
                self.in_flight.fetch_add(1, Ordering::AcqRel);
                Ok(BulkheadPermit {
                    _permit: permit,
                    in_flight: Arc::clone(&self.in_flight),
                })
            }
            Ok(Err(_closed)) => Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!("branch '{branch_id}' bulkhead closed"),
            )
            .with_retry(true, Some(retry_ms))),
            Err(_elapsed) => Err(GapError::new(
                ErrorCode::BranchUnavailable,
                format!(
                    "branch '{branch_id}' bulkhead wait exceeded {}ms (capacity={})",
                    retry_ms, self.capacity
                ),
            )
            .with_retry(true, Some(retry_ms))),
        }
    }

    /// Configured concurrency capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Currently available semaphore permits.
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.sem.available_permits()
    }

    /// Number of active permits currently held by callers.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Configured maximum queue wait before failing fast.
    #[must_use]
    pub const fn queue_wait(&self) -> Duration {
        self.queue_wait
    }

    fn retry_after_ms(&self) -> u32 {
        u32::try_from(self.queue_wait.as_millis())
            .unwrap_or(u32::MAX)
            .max(1)
    }
}

/// Immutable per-snapshot map of [`BranchBulkhead`] instances (`INV-02`, `RUN-032`).
#[derive(Debug, Clone, Default)]
pub struct BulkheadRegistry {
    entries: BTreeMap<BranchUid, Arc<BranchBulkhead>>,
}

impl BulkheadRegistry {
    /// Creates an empty [`BulkheadRegistry`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Constructs a [`BulkheadRegistry`] from a pinned [`Snapshot`], sizing each branch's bulkhead
    /// to `branch.limits.max_in_flight`.
    #[must_use]
    pub fn from_snapshot(snapshot: &Snapshot, queue_wait: Duration) -> Self {
        let mut entries = BTreeMap::new();
        for (uid, branch) in &snapshot.branches {
            let cap = (branch.limits.max_in_flight as usize).max(1);
            entries.insert(*uid, Arc::new(BranchBulkhead::new(cap, queue_wait)));
        }
        Self { entries }
    }

    /// Constructs a [`BulkheadRegistry`] from explicit `(BranchUid, capacity)` pairs.
    #[must_use]
    pub fn from_capacities(
        entries_iter: impl IntoIterator<Item = (BranchUid, usize)>,
        queue_wait: Duration,
    ) -> Self {
        let mut entries = BTreeMap::new();
        for (uid, cap) in entries_iter {
            entries.insert(uid, Arc::new(BranchBulkhead::new(cap, queue_wait)));
        }
        Self { entries }
    }

    /// Looks up the bulkhead assigned to `uid`.
    #[must_use]
    pub fn get(&self, uid: &BranchUid) -> Option<&Arc<BranchBulkhead>> {
        self.entries.get(uid)
    }

    /// Acquires a permit for `uid`, or returns `GAP_BRANCH_UNAVAILABLE` if unregistered.
    pub async fn acquire_for(&self, uid: BranchUid, branch_id: &str) -> GapResult<BulkheadPermit> {
        let bulkhead = self.entries.get(&uid).ok_or_else(|| {
            GapError::new(
                ErrorCode::BranchUnavailable,
                format!("no bulkhead registered for branch '{branch_id}' ({uid})"),
            )
            .with_retry(false, None)
        })?;
        bulkhead.acquire(branch_id).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use gap_core::ids::{BranchId, GroupId};
    use gap_core::snapshot::{
        BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CostHint, Endpoint, OpKind,
        OperationDescriptor, Protocol, SideEffect, SideEffectLevel, TrustTier,
    };

    // spec: RUN-032
    #[tokio::test]
    async fn bulkhead_raii_permit_releases_on_drop_and_fails_fast_with_after_ms() {
        let bh = Arc::new(BranchBulkhead::new(2, Duration::from_millis(25)));
        assert_eq!(bh.capacity(), 2);
        assert_eq!(bh.available_permits(), 2);
        assert_eq!(bh.in_flight(), 0);

        let p1 = bh.acquire("orders").await.expect("first permit");
        let p2 = bh.acquire("orders").await.expect("second permit");
        assert_eq!(bh.in_flight(), 2);
        assert_eq!(bh.available_permits(), 0);

        // Third concurrent caller waits at most queue_wait and fails with GAP_BRANCH_UNAVAILABLE + afterMs.
        let err = bh
            .acquire("orders")
            .await
            .expect_err("saturated bulkhead must reject after bounded wait");
        assert_eq!(err.code(), ErrorCode::BranchUnavailable);
        assert!(err.extensions.retry.retryable);
        assert_eq!(err.extensions.retry.after_ms, Some(25));

        drop(p1);
        assert_eq!(bh.in_flight(), 1);
        assert_eq!(bh.available_permits(), 1);

        let p3 = bh
            .acquire("orders")
            .await
            .expect("released slot becomes available");
        assert_eq!(bh.in_flight(), 2);
        drop(p2);
        drop(p3);
        assert_eq!(bh.in_flight(), 0);
        assert_eq!(bh.available_permits(), 2);
    }

    // spec: RUN-032
    #[tokio::test]
    async fn bulkhead_registry_builds_per_branch_limits_from_snapshot() {
        let mut snap = Snapshot::empty(1);
        let id = BranchId::new("crm").unwrap();
        let uid = BranchUid::new_v7();
        let group: GroupId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        let op = Arc::new(OperationDescriptor {
            id: "crm.user".parse().unwrap(),
            group,
            kind: OpKind::Query,
            side_effect: SideEffect {
                level: SideEffectLevel::None,
                verified: true,
                requires_approval: false,
            },
            idempotent: true,
            cost: CostHint::default(),
            desc_hash: [1_u8; 32],
            sdl: Arc::from("user: User"),
            examples: Vec::new(),
        });
        snap.branches.insert(
            uid,
            Arc::new(BranchDescriptor {
                uid,
                id,
                protocol: Protocol::Grpc,
                trust: TrustTier::T0,
                endpoint: Endpoint {
                    uri: "https://crm.internal".to_owned(),
                    transport: "grpc".to_owned(),
                },
                spec_hash: [2_u8; 32],
                pinned_desc_hash: [3_u8; 32],
                capabilities: Capabilities::default(),
                limits: BranchLimits {
                    max_batch_size: 100,
                    max_in_flight: 4,
                    timeout_ms: 1_000,
                    max_response_bytes: 1_048_576,
                },
                synopsis: "crm".to_owned(),
                operations: vec![op],
                status: BranchStatus::Active,
            }),
        );

        let registry = BulkheadRegistry::from_snapshot(&snap, Duration::from_millis(50));
        let bh = registry.get(&uid).expect("branch registered");
        assert_eq!(bh.capacity(), 4);

        let permit = registry
            .acquire_for(uid, "crm")
            .await
            .expect("permit acquired");
        assert_eq!(bh.in_flight(), 1);
        drop(permit);
        assert_eq!(bh.in_flight(), 0);
    }
}
