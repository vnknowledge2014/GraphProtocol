# tests/e2e

This directory is owned by WP-M1-13 (`tests/e2e/**`).

The M1 walking-skeleton E2E tests live in:

- `crates/gap-server/tests/e2e.rs` — in-process tests (gap_describe → gap_search → gap_query,
  typed-path overhead, determinism, auth, MCP protocol)
- `crates/gap-conformance/tests/spawn.rs` — spawn-mode structural tests
- `examples/walking-skeleton/conformance/` — Core conformance fixtures for
  `cargo run -p gap-conformance -- --class Core --spawn examples/walking-skeleton`

A workspace-level integration crate will be added here in a later milestone when
the first cross-crate end-to-end test requires it.
