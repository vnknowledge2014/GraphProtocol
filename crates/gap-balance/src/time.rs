//! Monotonic logical time base (`Now`) derived from a [`crate::TelemetryRegistry`] epoch (`RTE-077`).

/// Logical monotonic timestamp relative to a [`crate::TelemetryRegistry`] epoch (`INV-02`, `RTE-077`).
///
/// Computed once per request via `TelemetryRegistry::now()` and passed by value into hot-path
/// EWMA, circuit-breaker, and P2C operations so hot paths never touch an OS or trait clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Now {
    /// Elapsed milliseconds since the registry epoch.
    pub ms: u64,
    /// Wrapping `4 ms` tick counter: `(ms / 4) as u32` (`⌊elapsed_ms / 4⌋ mod 2³²`, `RTE-077`).
    pub q: u32,
}

impl Now {
    /// Constructs a [`Now`] from elapsed milliseconds since the registry epoch, computing
    /// `q = (ms / 4) as u32` with modular `2^32` wrapping (`RTE-077`).
    #[must_use]
    pub const fn from_ms(ms: u64) -> Self {
        Self {
            ms,
            q: (ms / 4) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec: RTE-077, INV-02
    #[test]
    fn now_from_ms_computes_4ms_ticks_and_wraps_u32() {
        assert_eq!(Now::from_ms(0), Now { ms: 0, q: 0 });
        assert_eq!(Now::from_ms(3), Now { ms: 3, q: 0 });
        assert_eq!(Now::from_ms(4), Now { ms: 4, q: 1 });
        assert_eq!(Now::from_ms(7), Now { ms: 7, q: 1 });
        assert_eq!(Now::from_ms(8), Now { ms: 8, q: 2 });

        let max_tick_ms = u64::from(u32::MAX) * 4 + 3;
        assert_eq!(
            Now::from_ms(max_tick_ms),
            Now {
                ms: max_tick_ms,
                q: u32::MAX,
            }
        );

        let wrapped_ms = (u64::from(u32::MAX) + 1) * 4;
        assert_eq!(
            Now::from_ms(wrapped_ms),
            Now {
                ms: wrapped_ms,
                q: 0,
            }
        );

        let wrapped_plus_five = (u64::from(u32::MAX) + 6) * 4 + 2;
        assert_eq!(
            Now::from_ms(wrapped_plus_five),
            Now {
                ms: wrapped_plus_five,
                q: 5,
            }
        );
    }

    // spec: RTE-070
    #[test]
    fn sync_shim_exposes_const_atomics_and_arc() {
        static GLOBAL_COUNTER: crate::sync::AtomicU64 = crate::sync::AtomicU64::new(41);
        let prev = GLOBAL_COUNTER.fetch_add(1, crate::sync::Ordering::SeqCst);
        assert_eq!(prev, 41);
        assert_eq!(GLOBAL_COUNTER.load(crate::sync::Ordering::SeqCst), 42);

        let shared = crate::sync::Arc::new(crate::sync::AtomicU32::new(7));
        assert_eq!(shared.load(crate::sync::Ordering::Relaxed), 7);
    }
}
