//! Black-box GAP conformance suite runner and `NGC` connector conformance kit (`L5`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod matcher;
pub mod ngc_kit;
