//! Durable storage traits (`ContextStore`, `TaskStore`, `AuditLog`, `IdempotencyStore`) and backends (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod audit;
pub mod feedback;
pub mod idempotency;
pub mod surreal;
pub mod tasks;
