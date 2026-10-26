# Verified dependency versions — 2026-10-26

Source: `https://crates.io/api/v1/crates/<name>` (max_stable_version, updated_at). Toolchain on host: `rustc 1.98.1 (2026-09-01)`, `cargo 1.98.1`.
Agents MUST re-verify with `cargo add --dry-run` / `cargo update --dry-run` at WP-M0-02 and record deviations in an ADR addendum; do **not** copy this table blindly after 2026-12-31.

| Area | Crate | Pin | Updated | Notes |
|---|---|---|---|---|
| Runtime | tokio | 1.54 | 2026-10-21 | |
| Service stack | tower | 0.5.3 | 2026-01-12 | |
| | tower-http | 0.7.1 | 2026-08-31 | |
| HTTP | axum | 0.8.9 | 2026-04-14 | pairs with tonic 0.14 (both on `axum ^0.8`) |
| gRPC | tonic | 0.14.6 | 2026-05-07 | `tonic 0.12.3` depends on `axum ^0.7` ⇒ mismatch with axum 0.8 (review F-08) |
| | tonic-prost / tonic-prost-build | 0.14.6 | 2026-05-07 | codegen split out of `tonic` in 0.14 |
| | tonic-reflection | 0.14.6 | 2026-05-07 | |
| Protobuf | prost | 0.14.4 | 2026-06-07 | |
| | protox | 0.10.0 | 2026-10-07 | pure-Rust protoc replacement for build.rs |
| GraphQL | apollo-compiler | 1.33.1 | 2026-10-21 | 2.0.0-beta.2 exists — do NOT use betas |
| | apollo-parser | 0.8.7 | 2026-10-21 | (0.9.0-beta.1 exists) |
| | async-graphql-parser | 7.2.1 | 2026-04-21 | fallback only |
| Snapshot | arc-swap | 1.9.3 | 2026-10-18 | measured: docs/evidence/arcswap-bench |
| | crossbeam-utils | 0.8.23 | 2026-09-05 | `CachePadded` |
| Caches | moka | 0.12.16 | 2026-08-09 | |
| Graph algos | petgraph | 0.8.3 | 2025-09-30 | |
| Maps | dashmap | 6.2.1 | 2026-05-17 | 7.0.0-rc2 exists; NOT needed on hot path (dense slab instead) |
| ML runtime | candle-core / candle-nn / candle-transformers | 0.11.0 | 2026-06-26 | `laya` 0.1.1 crate pins candle ^0.9 ⇒ do not depend on it directly |
| | ort | `=2.0.0-rc.15` | 2026-10-19 | still release-candidate ⇒ exact pin, optional feature |
| | tokenizers | 0.23.2 | 2026-10-19 | (1.0.0-rc.3 exists) |
| | fastembed | 7.5.0 | 2026-10-23 | optional embedding backend (ort-based) |
| Vector | simsimd | 6.5.16 | 2026-03-07 | flat SIMD search |
| | usearch | 2.26.6 | 2026-10-25 | HNSW when >20k vectors |
| | hnsw_rs | 0.3.5 | 2026-10-09 | pure-Rust alternative |
| Ingest | rmcp | 3.7.0 | 2026-10-22 | official MCP Rust SDK (`modelcontextprotocol/rust-sdk`); verify `2026-07-28` support at WP-M1-09 |
| | openapiv3 | 2.2.0 | 2025-06-02 | stale; WP-M4-02 decides vs `oas3` by corpus test |
| | sqlparser | 0.63.0 | 2026-09-13 | |
| | tree-sitter | 0.27.1 | 2026-10-08 | |
| | llguidance | 1.9.1 | 2026-09-30 | grammar-constrained decoding |
| | jsonschema | 0.60.1 | 2026-10-23 | |
| Storage | rusqlite | 0.41.0 | 2026-10-24 | default `ContextStore` (bundled SQLite) |
| | sqlite-vec | 0.1.9 | 2026-05-18 | optional |
| | redb | 4.4.0 | 2026-10-13 | optional KV |
| | surrealdb | 3.3.4 | 2026-10-19 | license `non-standard` (BSL) ⇒ optional feature only |
| Security | cedar-policy | 4.14.0 | 2026-10-17 | policy engine |
| | rustls | 0.23.46 | 2026-10-14 | |
| | jsonwebtoken | 11.1.0 | 2026-09-16 | |
| | sha2 | 0.11.1 | 2026-10-12 | |
| | ed25519-dalek | 3.0.0 | 2026-07-06 | manifest signatures |
| | zeroize | 1.9.1 | 2026-10-06 | |
| | secrecy | 0.10.3 | 2024-10-09 | stale; acceptable, re-evaluate |
| | blake3 | (verify) | — | not queried; pin at WP-M0-02 |
| Observability | tracing | 0.1.44 | 2025-12-18 | |
| | opentelemetry | 0.33.1 | 2026-10-09 | |
| | tracing-opentelemetry | 0.34.0 | 2026-09-23 | must match opentelemetry minor |
| | metrics | 0.24.6 | 2026-05-13 | |
| | hdrhistogram | 7.6.0 | 2026-07-18 | |
| Rate limit | governor | 0.10.4 | 2025-12-16 | |
| Testing | proptest | 1.12.0 | 2026-10-10 | |
| | criterion | 0.8.2 | 2026-02-04 | |
| | divan | 0.1.21 | 2025-04-10 | stale |
| | shuttle | 0.9.8 | 2026-10-20 | randomized concurrency testing (preferred over loom) |
| | loom | 0.7.2 | 2024-04 | stale; only for tiny hand-written lock-free structures |
| | cargo-nextest | 0.9.149 | 2026-10-20 | |
| | cargo-fuzz / libfuzzer-sys | 0.13.2 / 0.4.13 | 2026-06 | |
| | cargo-deny | 0.20.2 | 2026-07-09 | |
| Scripting | rhai | 1.27.0 | 2026-10-21 | deferred (ADR-019) |
| A2A | a2a-rs | 0.10.5 | 2026-10-20 | third-party; we generate from vendored official `a2a.proto` instead |

Crates that do **not** exist / are unusable (review F-07, F-30): `laya-onnx` (404); `openjev` (0.0.0-rc.0 "Reserved"); `gap-protocol` (taken: Group Audio Protocol); `gap` (taken: P2P pipe). Free at time of check: `gap-core`, `gap-cli`.

External model/spec versions pinned by this documentation set:

| Item | Pin | Source |
|---|---|---|
| MCP specification | `2026-07-28` (stateless; no `initialize`, no `Mcp-Session-Id`; version/capabilities in `_meta`); fallback `2025-11-25` for outbound only | modelcontextprotocol.io/specification/2026-07-28/changelog |
| A2A | v1.0 (stable 2026-03; hosted by AAIF since 2026-08-17) | linuxfoundation.org press release; aaif.io |
| GraphQL | latest stable spec edition + GraphQL-over-HTTP (`application/graphql-response+json`) + `graphql-sse` | graphql.org (verify edition at WP-M0-03) |
| System-1 weights | `convaiinnovations/laya-typed-decisions` (Apache-2.0, 421M, EN) | huggingface.co |
