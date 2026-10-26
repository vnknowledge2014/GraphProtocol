//! Inbound stateless MCP `2026-07-28` facade and outbound MCP branch connector (`L4`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod inbound;
pub mod outbound;
