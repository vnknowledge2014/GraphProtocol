//! Fuzz target for Deterministic MCP tools/resources/prompts and A2A AgentCard extractors (`WP-M4-03`, `ING-012`).
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
use gap_ingest::extractors::a2a::A2aExtractor;
use gap_ingest::extractors::mcp::McpExtractor;
use gap_ingest::{ExtractLimits, Extractor, SourceHandle};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = ExtractLimits {
        max_bytes: 64 * 1024,
        max_depth: 16,
        max_items: 512,
        timeout: Duration::from_millis(50),
    };

    if let Ok(mcp_src) =
        SourceHandle::new(Protocol::Mcp, "fuzz://mcp/manifest", data.to_vec(), &limits)
    {
        if let Ok(cisg) = McpExtractor::new().extract(&mcp_src, &limits) {
            let _ = cisg.to_canonical_json_bytes();
        }
    }

    if let Ok(a2a_src) = SourceHandle::new(
        Protocol::A2a,
        "fuzz://a2a/agent-card.json",
        data.to_vec(),
        &limits,
    ) {
        if let Ok(cisg) = A2aExtractor::new().extract(&a2a_src, &limits) {
            let _ = cisg.to_canonical_json_bytes();
        }
    }
});
