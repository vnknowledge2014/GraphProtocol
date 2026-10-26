//! Immutable supergraph snapshot builder, `ArcSwap` publisher, off-thread reclaimer, and attribution-aware auto-rollback (`L3`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod diff;
pub mod groups;
pub mod incremental;
pub mod publisher;
pub mod rollback;
