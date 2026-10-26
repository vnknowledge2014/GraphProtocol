//! Core identifiers, error types, configuration, immutable snapshot model, and shared cross-layer
//! traits for the Graph Agent Protocol (`L0`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cursor;
pub mod ids;
pub mod joingraph;
pub mod snapshot;
