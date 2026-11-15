//! Pure-`std`, cancel-safe bounded FIFO concurrency semaphore (`RTE-045`, `WP-M5-03`).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use gap_core::error::{ErrorCode, GapError, GapResult};

#[derive(Debug)]
struct Waiter {
    id: u64,
    waker: Option<Waker>,
    granted: bool,
}

#[derive(Debug, Default)]
struct SemaphoreState {
    in_flight: usize,
    next_id: u64,
    waiters: VecDeque<Waiter>,
}

/// Bounded asynchronous FIFO semaphore capping active permits and queued waiters (`RTE-045`).
///
/// Built strictly with `std::sync::Mutex` and `std::task::Waker` so `gap-route` stays free of
/// runtime dependencies (`ARC-002`).
#[derive(Debug)]
pub struct BoundedSemaphore {
    max_permits: usize,
    max_waiters: usize,
    state: Mutex<SemaphoreState>,
}

impl BoundedSemaphore {
    /// Creates a bounded semaphore permitting up to `max_permits` concurrent holders (`>= 1`)
    /// and up to `max_waiters` queued waiters before shedding load.
    #[must_use]
    pub fn new(max_permits: usize, max_waiters: usize) -> Self {
        Self {
            max_permits: max_permits.max(1),
            max_waiters,
            state: Mutex::new(SemaphoreState::default()),
        }
    }

    /// Configured maximum active permits (`max_concurrency`).
    #[must_use]
    pub const fn max_permits(&self) -> usize {
        self.max_permits
    }

    /// Configured maximum queued waiter depth (`<= 2 * max_concurrency` default).
    #[must_use]
    pub const fn max_waiters(&self) -> usize {
        self.max_waiters
    }

    /// Current number of active permits (including woken waiters holding a transferred slot).
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.lock_state().in_flight
    }

    /// Current number of ungranted callers waiting in the FIFO queue.
    #[must_use]
    pub fn queued_waiters(&self) -> usize {
        self.lock_state()
            .waiters
            .iter()
            .filter(|w| !w.granted)
            .count()
    }

    /// Attempts to acquire a permit, queueing FIFO up to [`Self::max_waiters`] and returning
    /// [`ErrorCode::DiscoveryDegraded`] immediately if the queue is full (`RTE-045`).
    #[must_use]
    pub fn acquire(&self) -> AcquireFuture<'_> {
        AcquireFuture {
            sem: self,
            waiter_id: None,
            completed: false,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, SemaphoreState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn release_slot(&self) {
        let mut st = self.lock_state();
        let wake = Self::release_slot_locked(&mut st);
        drop(st);
        if let Some(waker) = wake {
            waker.wake();
        }
    }

    fn release_slot_locked(st: &mut SemaphoreState) -> Option<Waker> {
        if let Some(waiter) = st.waiters.iter_mut().find(|w| !w.granted) {
            waiter.granted = true;
            return waiter.waker.take();
        }
        st.in_flight = st.in_flight.saturating_sub(1);
        None
    }
}

/// RAII permit released back to [`BoundedSemaphore`] on drop.
#[derive(Debug)]
pub struct SemaphorePermit<'a> {
    sem: &'a BoundedSemaphore,
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.release_slot();
    }
}

/// Cancel-safe future returned by [`BoundedSemaphore::acquire`].
#[derive(Debug)]
pub struct AcquireFuture<'a> {
    sem: &'a BoundedSemaphore,
    waiter_id: Option<u64>,
    completed: bool,
}

impl<'a> Future for AcquireFuture<'a> {
    type Output = GapResult<SemaphorePermit<'a>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.completed {
            return Poll::Ready(Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "S1 semaphore acquire future polled after completion",
            )));
        }

        let mut st = self.sem.lock_state();
        if let Some(wid) = self.waiter_id {
            if let Some(pos) = st.waiters.iter().position(|w| w.id == wid) {
                if st.waiters[pos].granted {
                    st.waiters.remove(pos);
                    drop(st);
                    self.waiter_id = None;
                    self.completed = true;
                    return Poll::Ready(Ok(SemaphorePermit { sem: self.sem }));
                }
                st.waiters[pos].waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            drop(st);
            self.waiter_id = None;
            self.completed = true;
            return Poll::Ready(Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "S1 semaphore waiter entry unexpectedly missing",
            )));
        }

        let has_ungranted = st.waiters.iter().any(|w| !w.granted);
        if st.in_flight < self.sem.max_permits && !has_ungranted {
            st.in_flight += 1;
            drop(st);
            self.completed = true;
            return Poll::Ready(Ok(SemaphorePermit { sem: self.sem }));
        }

        let ungranted = st.waiters.iter().filter(|w| !w.granted).count();
        if ungranted >= self.sem.max_waiters {
            drop(st);
            self.completed = true;
            return Poll::Ready(Err(GapError::new(
                ErrorCode::DiscoveryDegraded,
                "S1 concurrency queue depth exceeded",
            )));
        }

        let wid = st.next_id;
        st.next_id = st.next_id.wrapping_add(1);
        st.waiters.push_back(Waiter {
            id: wid,
            waker: Some(cx.waker().clone()),
            granted: false,
        });
        drop(st);
        self.waiter_id = Some(wid);
        Poll::Pending
    }
}

impl Drop for AcquireFuture<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let Some(wid) = self.waiter_id.take() else {
            return;
        };
        let mut st = self.sem.lock_state();
        let wake = st
            .waiters
            .iter()
            .position(|w| w.id == wid)
            .and_then(|pos| st.waiters.remove(pos))
            .filter(|removed| removed.granted)
            .and_then(|_| BoundedSemaphore::release_slot_locked(&mut st));
        drop(st);
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}
