//! ML-free hybrid element index (BM25, flat SIMD, HNSW, and reciprocal rank fusion) (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]
