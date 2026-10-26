//! Discovery plane (`__metaGraph`, `__search`, `gap.resolve`), input sanitization, capability grouping, and split-conformal calibration (`L2`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod calibration_job;
pub mod debias;
pub mod feedback;
pub mod groups;
pub mod s1_adapter;
