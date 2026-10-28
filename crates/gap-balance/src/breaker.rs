//! Per-branch circuit breaker (`RTE-080`..`RTE-085`; expanded with bucket window & FSM in Group 4).

use std::time::Duration;

use crate::slot::{HealthStatus, Outcome};
use crate::sync::Arc;
use crate::time::Now;

/// Observable circuit-breaker state (`RTE-080`, `RTE-082`, `RTE-085`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakerState {
    /// Normal operation; calls admitted and tracked in the 30 s rolling window.
    Closed,
    /// Recovery trial; up to 3 probes admitted per episode (`RTE-085`).
    HalfOpen,
    /// Tripped or forced `NOT_SERVING`; calls rejected immediately (`RTE-081`).
    Open,
}

/// Per-branch circuit breaker (`RTE-080`..`RTE-085`).
#[derive(Debug)]
pub struct Breaker {
    _default_timeout_ms: u32,
    _label: Arc<str>,
}

impl Breaker {
    /// Constructs a new [`Breaker`] in [`BreakerState::Closed`].
    #[must_use]
    pub fn new(default_timeout_ms: u32, label: Arc<str>) -> Self {
        Self {
            _default_timeout_ms: default_timeout_ms,
            _label: label,
        }
    }

    /// Returns the breaker's state at `now`.
    #[must_use]
    pub fn state(&self, _now: Now) -> BreakerState {
        BreakerState::Closed
    }

    pub(crate) fn record_call(&self, _outcome: Outcome, _latency: Duration, _now: Now) {}

    pub(crate) fn on_health(&self, _status: HealthStatus, _now: Now) {}
}
