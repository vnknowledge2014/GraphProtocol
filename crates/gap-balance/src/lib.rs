//! ML-free branch telemetry slots, decayed EWMA, circuit breakers, retry budgets, and power-of-two-choices balancing (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod breaker;
pub mod ewma;
pub mod registry;
pub mod retry;
pub mod slot;
mod sync;
pub mod time;

pub use breaker::{Admission, Breaker, BreakerState, ProbePermit};
pub use ewma::DecayedEwma;
pub use registry::{BranchSlot, RegistryError, TelemetryRegistry};
pub use retry::RetryBudget;
pub use slot::{HealthStatus, InFlightGuard, Outcome, SlotParams, SlotView, TelemetrySlot};
pub use time::Now;
