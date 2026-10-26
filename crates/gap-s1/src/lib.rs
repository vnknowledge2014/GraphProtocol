//! Optional System-1 (`Laya`) `DecisionBackend` implementations (`LayaCandle`, `LayaOrt`, `HttpSidecar`) (`L3`).
// ADR-002 / ARC-003: gap-s1 is the sole crate exempted from #![forbid(unsafe_code)]
// (audited mmap blocks are tracked in docs/UNSAFE.md; zero unsafe blocks exist yet).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod candle;
pub mod ort;
pub mod sidecar;
