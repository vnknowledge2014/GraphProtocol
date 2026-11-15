//! Optional System-1 (`Laya`) `DecisionBackend` implementations (`LayaCandle`, `LayaOrt`, `HttpSidecar`) (`L3`).
// ADR-002 / ARC-003: gap-s1 is the sole crate exempted from #![forbid(unsafe_code)].
// Audited POSIX mmap blocks live exclusively in `src/candle/mmap.rs` (`UNS-001`..`UNS-005`, tracked in `docs/UNSAFE.md`).
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

pub use candle::{LayaCandle, LayaConfig, LayaForwardOutput, LayaTokenizer, Precision};
