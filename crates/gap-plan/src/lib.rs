//! Deterministic, ML-free supergraph query planner, predicate pushdown, and `foreach` join planner (`L2`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod join;
pub mod pushdown;
pub mod traverse;
