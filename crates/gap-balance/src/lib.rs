//! ML-free branch telemetry slots, decayed EWMA, circuit breakers, retry budgets, and power-of-two-choices balancing (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod ewma;
mod sync;
pub mod time;

pub use ewma::DecayedEwma;
pub use time::Now;
