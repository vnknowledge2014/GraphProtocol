//! Control-plane ingestion pipeline: deterministic extractors (`CISG`), optional enrichment, verification gates, drift detection, and proposals (`L2`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cisg;
pub mod drift;
pub mod enrich;
pub mod extractors;
pub mod fetch;
pub mod gates;
pub mod lint;
pub mod proposals;
pub mod worker;
