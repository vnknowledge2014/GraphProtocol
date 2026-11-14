//! Fuzz target for Deterministic GraphQL SDL & Introspection extractor (`WP-M4-02`, `ING-012`).
#![no_main]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::time::Duration;

use gap_core::snapshot::Protocol;
use gap_ingest::extractors::graphql::GraphqlExtractor;
use gap_ingest::{ExtractLimits, Extractor, SourceHandle};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = ExtractLimits {
        max_bytes: 64 * 1024,
        max_depth: 16,
        max_items: 512,
        timeout: Duration::from_millis(50),
    };

    if let Ok(src) = SourceHandle::new(
        Protocol::Graphql,
        "fuzz://graphql/schema.graphql",
        data.to_vec(),
        &limits,
    ) && let Ok(cisg) = GraphqlExtractor::default().extract(&src, &limits)
    {
        let _ = cisg.to_canonical_json_bytes();
    }
});
