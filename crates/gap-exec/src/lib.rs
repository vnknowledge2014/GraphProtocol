//! Deterministic execution plane engine: batched `foreach` joins, traversals, mutations, sagas, and tasks (`L3`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod bulkhead;
pub mod columnar;
pub mod connector_layer;
pub mod join;
pub mod mutate;
pub mod saga;
pub mod shadow;
pub mod tasks;
pub mod traverse;
