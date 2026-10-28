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
pub mod p2c;
pub mod registry;
pub mod retry;
pub mod slot;
mod sync;
pub mod time;

pub use breaker::{Admission, Breaker, BreakerState, ProbePermit};
pub use ewma::DecayedEwma;
pub use p2c::{Candidate, XorShift64, select, select_excluding};
pub use registry::{BranchSlot, RegistryError, TelemetryRegistry};
pub use retry::RetryBudget;
pub use slot::{HealthStatus, InFlightGuard, Outcome, SlotParams, SlotView, TelemetrySlot};
pub use time::Now;
