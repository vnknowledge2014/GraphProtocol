//! GAP gateway server library: configuration-driven assembly of execution, transport and protocol facades (`L5`).
//!
//! `WP-M1-12` fills this crate: `assemble` (branches -> snapshot, registry, `AppState`, MCP facade), `run`
//! (listener, stdio, graceful shutdown). The `gap-server` binary and the `gap` CLI (`gap serve`) both call it.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod wiring_s1;
