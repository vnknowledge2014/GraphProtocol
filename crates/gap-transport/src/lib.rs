//! Deterministic transport stack (`GraphQL-over-HTTP`, `graphql-sse`, gRPC, stdio), rate limiting, and load shedding (`L4`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod ratelimit;
pub mod shed;
