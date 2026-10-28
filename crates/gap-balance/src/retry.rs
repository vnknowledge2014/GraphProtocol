//! Fixed-point milli-token retry budget (`RTE-091`, `RTE-093`).

use crate::sync::{AtomicU32, Ordering};

/// Initial retry budget: `10` tokens = `10,000` milli-tokens (`RTE-091`, `RTE-093`).
const INITIAL_MILLIS: u32 = 10_000;

/// Deposit per first attempt: `0.1` token = `100` milli-tokens (`RTE-091`, `RTE-093`).
const DEPOSIT_MILLIS: u32 = 100;

/// Cost per retry or hedged attempt: `1.0` token = `1,000` milli-tokens (`RTE-091`, `RTE-093`).
const WITHDRAW_MILLIS: u32 = 1_000;

/// Maximum retry budget balance: `100` tokens = `100,000` milli-tokens (`RTE-091`, `RTE-093`).
const CAP_MILLIS: u32 = 100_000;

/// Lock-free fixed-point token bucket bounding retry and hedge amplification (`RTE-091`, `RTE-093`).
///
/// Balances are represented strictly in integer milli-tokens (`1` token = `1,000` milli-tokens)
/// with no floating-point arithmetic.
#[derive(Debug)]
pub struct RetryBudget {
    millis: AtomicU32,
}

impl RetryBudget {
    /// Creates a [`RetryBudget`] with the initial balance of `10,000` milli-tokens (`10` tokens).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            millis: AtomicU32::new(INITIAL_MILLIS),
        }
    }

    /// Deposits `100` milli-tokens (`0.1` token) for a first attempt, saturating at `100,000`
    /// milli-tokens (`RTE-091`, `RTE-093`).
    pub fn deposit_first_attempt(&self) {
        let mut cur = self.millis.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_add(DEPOSIT_MILLIS).min(CAP_MILLIS);
            if next == cur {
                return;
            }
            match self.millis.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Atomically withdraws `1,000` milli-tokens (`1` token) if `balance_millis >= 1,000`,
    /// returning `true` iff the withdrawal succeeded (`RTE-091`, `RTE-093`).
    pub fn try_withdraw(&self) -> bool {
        let mut cur = self.millis.load(Ordering::Acquire);
        loop {
            if cur < WITHDRAW_MILLIS {
                return false;
            }
            match self.millis.compare_exchange_weak(
                cur,
                cur - WITHDRAW_MILLIS,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Returns the current balance in milli-tokens (`0..=100_000`).
    #[must_use]
    pub fn balance_millis(&self) -> u32 {
        self.millis.load(Ordering::Acquire)
    }
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    // spec: RTE-091, RTE-093
    #[test]
    fn retry_budget_initial_withdraw_deposit_cap_and_reenable() {
        let budget = RetryBudget::new();
        assert_eq!(budget.balance_millis(), 10_000);

        // 10 deposits add +1,000 milli-tokens.
        let plus_ten = RetryBudget::new();
        for _ in 0..10 {
            plus_ten.deposit_first_attempt();
        }
        assert_eq!(plus_ten.balance_millis(), 11_000);

        // 10 withdrawals succeed from 10,000 and the 11th fails.
        for remaining in (0..10).rev() {
            assert!(budget.try_withdraw());
            assert_eq!(budget.balance_millis(), remaining * 1_000);
        }
        assert!(!budget.try_withdraw());
        assert_eq!(budget.balance_millis(), 0);

        // Empty budget stays disabled at 9 deposits (900 milli-tokens) and re-enables at 10 (1,000).
        for i in 1..=9 {
            budget.deposit_first_attempt();
            assert_eq!(budget.balance_millis(), i * 100);
            assert!(!budget.try_withdraw());
            assert_eq!(budget.balance_millis(), i * 100);
        }
        budget.deposit_first_attempt();
        assert_eq!(budget.balance_millis(), 1_000);
        assert!(budget.try_withdraw());
        assert_eq!(budget.balance_millis(), 0);
        assert!(!budget.try_withdraw());

        // Cap at 100,000 after 2,000 deposits; verify exact saturation boundary at 99,900 -> 100,000.
        for _ in 0..999 {
            budget.deposit_first_attempt();
        }
        assert_eq!(budget.balance_millis(), 99_900);
        budget.deposit_first_attempt();
        assert_eq!(budget.balance_millis(), 100_000);
        for _ in 0..1_000 {
            budget.deposit_first_attempt();
        }
        assert_eq!(budget.balance_millis(), 100_000);
        assert!(budget.try_withdraw());
        assert_eq!(budget.balance_millis(), 99_000);
    }

    // spec: RTE-091, RTE-093
    #[test]
    fn retry_budget_64_threads_withdraw_and_deposit_conservation() {
        const THREADS: usize = 64;

        // 64 threads withdrawing concurrently from 10 tokens (10,000 milli-tokens) succeed 10 times.
        let budget = Arc::new(RetryBudget::new());
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let budget = Arc::clone(&budget);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                u32::from(budget.try_withdraw())
            }));
        }
        let successes: u32 = handles
            .into_iter()
            .map(|h| h.join().expect("withdraw worker"))
            .sum();
        assert_eq!(successes, 10);
        assert_eq!(budget.balance_millis(), 0);

        // Non-saturating deposit conservation: from 0, 64 * 15 deposits = 96,000 milli-tokens exact.
        let exact_barrier = Arc::new(Barrier::new(THREADS));
        let mut exact_handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let budget = Arc::clone(&budget);
            let barrier = Arc::clone(&exact_barrier);
            exact_handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..15 {
                    budget.deposit_first_attempt();
                }
            }));
        }
        for h in exact_handles {
            h.join().expect("exact deposit worker");
        }
        assert_eq!(budget.balance_millis(), 96_000);

        // Drain back to 0, then 64 * 1,000 deposits saturate cleanly at 100,000 without overflow.
        for _ in 0..96 {
            assert!(budget.try_withdraw());
        }
        assert_eq!(budget.balance_millis(), 0);

        let sat_barrier = Arc::new(Barrier::new(THREADS));
        let mut sat_handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let budget = Arc::clone(&budget);
            let barrier = Arc::clone(&sat_barrier);
            sat_handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..1_000 {
                    budget.deposit_first_attempt();
                }
            }));
        }
        for h in sat_handles {
            h.join().expect("saturating deposit worker");
        }
        assert_eq!(budget.balance_millis(), 100_000);
    }
}
