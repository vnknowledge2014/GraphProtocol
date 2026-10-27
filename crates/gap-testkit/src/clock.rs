//! Deterministic manual clock re-exports and Tokio paused-time helpers (`INV-02`).
//!
//! Re-exports [`gap_core::clock::ManualClock`] directly rather than introducing a duplicate clock
//! implementation.

use std::sync::Arc;
use std::time::Duration;

pub use gap_core::clock::{Clock, ManualClock, Timestamp, TimestampError};

/// Default deterministic epoch (`2023-11-14T22:13:20Z` in Unix milliseconds) shared by testkit helpers.
pub const DEFAULT_TEST_EPOCH_MILLIS: u64 = 1_700_000_000_000;

/// Returns a shared [`ManualClock`] anchored at [`DEFAULT_TEST_EPOCH_MILLIS`].
#[must_use]
pub fn test_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(DEFAULT_TEST_EPOCH_MILLIS))
}

/// Returns a shared [`ManualClock`] anchored at `start_unix_millis`.
#[must_use]
pub fn test_clock_at_millis(start_unix_millis: u64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(start_unix_millis))
}

/// Advances both `clock` (`gap_core::clock::ManualClock`) and Tokio's paused timer wheel by `delta`
/// in lockstep (`#[tokio::test(start_paused = true)]`).
pub async fn advance_paused(clock: &ManualClock, delta: Duration) {
    clock.advance(delta);
    tokio::time::advance(delta).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: INV-02, ARC-010
    #[tokio::test(start_paused = true)]
    async fn inv_02_manual_clock_reexport_and_advance_paused_stay_in_lockstep() {
        let clock = test_clock();
        assert_eq!(clock.unix_millis(), 1_700_000_000_000);
        let t0_tokio = tokio::time::Instant::now();
        let t0_clock = clock.now_instant();

        let sleep_fut = tokio::time::sleep(Duration::from_millis(250));
        tokio::pin!(sleep_fut);

        advance_paused(&clock, Duration::from_millis(250)).await;
        sleep_fut.await;

        assert_eq!(clock.unix_millis(), 1_700_000_000_250);
        assert_eq!(
            clock.now_instant().duration_since(t0_clock),
            Duration::from_millis(250)
        );
        assert!(t0_tokio.elapsed() >= Duration::from_millis(250));

        // Verify `gap_testkit::clock::ManualClock` is the exact `gap_core::clock::ManualClock` type
        // (zero duplicate implementation).
        let core_ref: &gap_core::clock::ManualClock = &clock;
        assert_eq!(core_ref.elapsed(), Duration::from_millis(250));
    }

    // spec: INV-02, ARC-010
    #[tokio::test(start_paused = true)]
    async fn f3_advance_paused_advances_both_tokio_instant_and_manual_clock_without_sleep() {
        let clock = test_clock();
        let t0_tokio = tokio::time::Instant::now();
        let t0_clock = clock.now_instant();
        let t0_millis = clock.unix_millis();

        advance_paused(&clock, Duration::from_secs(5)).await;

        assert_eq!(
            tokio::time::Instant::now().duration_since(t0_tokio),
            Duration::from_secs(5)
        );
        assert_eq!(
            clock.now_instant().duration_since(t0_clock),
            Duration::from_secs(5)
        );
        assert_eq!(clock.unix_millis() - t0_millis, 5_000);
    }
}
