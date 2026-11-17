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
| MCP specification | `2026-07-28` (stateless; no `initialize`, no `Mcp-Session-Id`; version/capabilities in `_meta`) + `2025-11-25` (outbound fallback & `CMP-001` compat window); commit `ccaf9102ec5eb656eab50ca8333ef49dcd259c26` | `github.com/modelcontextprotocol/modelcontextprotocol` (`third_party/specs/mcp-2026-07-28`, `third_party/specs/mcp-2025-11-25`) |
| A2A | `v1.0.1` (`3303592588e388e62e0f69f701af531d2f4e3991`, wire-identical `a2a.proto` to `v1.0.0` `173695755607e884aa9acf8ce4feed90e32727a1`) | `github.com/a2aproject/A2A` (`third_party/specs/a2a-v1.0.1`, `third_party/specs/a2a-v1.0.0-proto`) |
| GraphQL | `September 2025 Edition` (tag `September2025`, `89d93ebbe05db06787646d76a696ead8de117b2b`) | `github.com/graphql/graphql-spec` (`third_party/specs/graphql-September2025`) |
| GraphQL-over-HTTP | `Current Working Draft` (`application/graphql-response+json`; commit `d438b209d60fa3d34b0530b605e60d5640c7ef9b`, no tag) | `github.com/graphql/graphql-over-http` (`third_party/specs/graphql-over-http`) |
| `graphql-sse` | `v2.6.3` (`4cced455cb7a23beda9e9bf7a403aab270bd4e66`, distinct-connections mode) | `github.com/enisdenjo/graphql-sse` (`third_party/specs/graphql-sse-v2.6.3`) |
| System-1 weights | `convaiinnovations/laya-typed-decisions` (Apache-2.0, 421M, EN) | huggingface.co |

## WP-M0-02 Offline Verification Note (2026-10-26)

- **Offline execution constraint:** `WP-M0-02` ran inside a network-free sandbox where `cargo add --dry-run` and `cargo update --dry-run` could not query crates.io. All 55 third-party library pins in root `Cargo.toml` `[workspace.dependencies]` (and `libfuzzer-sys = "0.4.13"` in standalone `fuzz/Cargo.toml`) were copied verbatim from the verified table above.
- **Online re-verification (reviewer, 2026-10-26):** a scratch crate depending on all 55 pins plus the 17 crates listed next was resolved against the crates.io index (`cargo generate-lockfile`): 829 packages lock without conflict (the exact 72 third-party entries of the root `Cargo.toml`, features unset), and `axum 0.8.9`, `hyper 1.12.0`, `prost 0.14.4` and `tower 0.5.3` each appear exactly once (ADR-002, review F-08). `tower-http` appears as both `0.6.11` and `0.7.1` through a transitive dependency; it is not covered by the ADR-002 ban and is only reported. The whole graph has 60 crate names with more than one version, which is why `deny.toml` denies duplicates per crate and not globally.
- **Pins added by the review** (no row in the table above; the latest versions that resolve together with the pins): `anyhow 1.0.104`, `blake3 1.8.7`, `bytes 1.12.1`, `clap 4.6.7`, `http 1.5.0`, `hyper 1.12.0`, `im 15.1.0`, `reqwest 0.13.5`, `safetensors 0.8.0`, `schemars 1.2.3`, `serde 1.0.229`, `serde_json 1.0.154`, `thiserror 2.0.21`, `tokio-stream 0.1.19`, `toml 1.1.8`, `unicode-normalization 0.1.26`, `uuid 1.29.0`. Features are chosen by the consuming WPs. `thiserror 1.x`, `reqwest 0.12` and `schemars 0.9` also appear transitively.
- **Pins added for WP-M0-08 (reviewer, 2026-10-26):** `tracing-subscriber 0.3.23`, `opentelemetry_sdk 0.33.1`, `opentelemetry-otlp 0.33.1` (the OTLP exporter uses `tonic 0.14.6`/`prost 0.14.4`). A scratch crate with the exact WP-M0-08 dependency set (`serde`, `serde_json`, `thiserror`, `zeroize`, `toml`, `schemars`, `apollo-compiler`, `tracing`, `tracing-subscriber`, `uuid`, optional OpenTelemetry stack, dev `proptest`) resolves to 199 packages with one version of `hyper`, `prost`, `tower` and `tonic`, was vendored into the git-ignored `.local/vendor` (210 MB, 199 crates) and built offline with `--all-features --all-targets` in 41 s.
- **Pins and features changed for WP-M0-09, WP-M2-01, WP-M3-01 (reviewer, 2026-10-27):** `tempfile 3.27.0` pinned (dev-only; already locked through `proptest`). `reqwest`, `criterion` and `hdrhistogram` now disable default features in the root catalogue: `reqwest` has no TLS backend on purpose (default stacks pull `aws-lc`/`ring`, licences outside the `deny.toml` allow-list), `criterion` is used with `cargo_bench_support` only (no `plotters`/`rayon`), `hdrhistogram` without its default `serialization` and `sync` features (members may re-enable them). A scratch copy of the workspace with the intended dependency sets of the three WPs resolved without changing any locked version (230 -> 359 packages, `axum 0.8.9`, `hyper 1.12.0`, `prost 0.14.4`, `tower 0.5.3` still single-version), was vendored (328 crates, 468 MB), built offline with `--all-features --all-targets`, and passed `cargo deny --offline check`. Verified behaviour of the vendored set: bundled SQLite 3 with FTS5, WAL, recursive CTE, `json_extract` and the backup API; `shuttle`, `loom`, tokio paused clock; `criterion --quick` only works when the crate sets `[lib] bench = false`. Details and the per-WP dependency sets: `work/context/offline-vendoring.md`.
- **Rows intentionally excluded from `[workspace.dependencies]`:**
  - `rhai` (`1.27.0`) — deferred out of 1.0 per `ADR-019`.
  - `a2a-rs` (`0.10.5`) — unused; GAP generates wire types from vendored official `a2a.proto`.
  - `cargo-nextest`, `cargo-fuzz`, `cargo-deny` — Cargo subcommand binaries rather than library dependencies.

## WP-M0-03 Verification Note (2026-10-28)

Verified offline against the 218 vendored upstream files in `third_party/specs/` (`third_party/MANIFEST.toml`; full citations and correction proposals in `third_party/specs/SPEC-NOTES.md`). Contradictions and material omissions found (`our text` vs `upstream`):

- **`Q-1` (`spec/09-compat.md:7`, `CMP-001`):** our text omits `server/discover`, omits top-level `resultType` on results, and says `/mcp` serves `2025-11-25` `"when the request says so"` vs upstream `2026-07-28` requires every server to implement `server/discover`, requires `resultType: "complete" | "input_required"` on every `Result`, and selects legacy `2025-11-25` on dual-era servers only when the client opens with an `initialize` request (`MCP-Session-Id` on HTTP), not via `_meta`.
- **`Q-2` (`spec/09-compat.md:8-18`, `CMP-002`, `CMP-004`):** our text omits caching/result envelope fields on `tools/list` and `tools/call` and leaves `readOnlyHint`/`openWorldHint` unset on `gap_task` vs upstream `2026-07-28` requires `resultType: "complete"`, `ttlMs`, and `cacheScope: "public" | "private"` on `ListToolsResult`, requires `resultType` on `CallToolResult`, and defaults omitted `readOnlyHint` to `false` and `openWorldHint` to `true`.
- **`Q-3` (`spec/09-compat.md:5,7,26`, `CMP-001`, `CMP-010`):** our text mentions only `_meta` on Streamable HTTP vs upstream `2026-07-28` Streamable HTTP is `POST`-only (`GET`/`DELETE` -> `405`) and requires `Accept: application/json, text/event-stream`, `MCP-Protocol-Version`, `Mcp-Method` on all requests, and `Mcp-Name` on `tools/call` and `resources/read` (returning HTTP `400` + JSON-RPC `-32020` `HeaderMismatchError` on missing/mismatched headers) plus `Origin` validation (`403`).
- **`Q-5` (`spec/09-compat.md:29`, `CMP-013`):** our text treats `roots/sampling/logging` as uniform deprecated RPCs vs upstream `2026-07-28` removed `logging/setLevel`, `notifications/roots/list_changed`, and server-initiated `sampling/createMessage`/`roots/list` RPCs, delivering deprecated `Roots` and `Sampling` only inside MRTR `inputRequests` (when advertised in request `_meta["io.modelcontextprotocol/clientCapabilities"]`) and deprecated `Logging` via per-request `_meta["io.modelcontextprotocol/logLevel"]`.
- **`Q-6` (`spec/07-security.md:37`, `spec/08-operations.md:7`, `spec/09-compat.md:20`, `SEC-001`, `OPS-002`, `CMP-006`):** our text publishes RFC 9728 metadata only at root `/.well-known/oauth-protected-resource` vs upstream MCP authorization (RFC 9728 path insertion) probes `/.well-known/oauth-protected-resource/mcp` for `/mcp` before root when `resource_metadata` is absent from `WWW-Authenticate`.
- **`Q-7` (`spec/09-compat.md:19`, `CMP-005`):** our text omits resource result metadata vs upstream `2026-07-28` requires `resultType: "complete"`, `ttlMs`, and `cacheScope` on `resources/list`, `resources/templates/list`, and `resources/read` (and removed `resources/subscribe` in favor of `subscriptions/listen`).
- **`Q-8` (`spec/09-compat.md:28`, `CMP-012`):** our text says outbound `"cancellation sends the MCP cancel notification"` unconditionally vs upstream `2026-07-28` sends `notifications/cancelled` **only on `stdio`** (and legacy `2025-11-25` HTTP), while on `2026-07-28` Streamable HTTP closing the HTTP/SSE stream is the sole cancellation signal.
- **`Q-10` (`spec/09-compat.md:7,26`, `CMP-001`, `CMP-010`):** our text does not separate stateless `2026-07-28` from stateful `2025-11-25` session management vs upstream `2025-11-25` requires `initialize` -> `notifications/initialized`, `MCP-Session-Id` echoing, full re-initialization on HTTP `404`, and `DELETE` teardown, dispatched via upstream's dual-era probe rules.
- **`Q-11` (`spec/09-compat.md:35`, `docs/01-DECISIONS.md:122`, `CMP-021`, `ADR-016`):** our text says A2A `"State names map identically (ADR-016)"` across 8 unprefixed names vs upstream `a2a.proto` and ProtoJSON (§5.5) serialize all states with the `TASK_STATE_` prefix (`"TASK_STATE_SUBMITTED"`, etc.) and define a 9th zero-value `TASK_STATE_UNSPECIFIED = 0` absent from GAP.
- **`Q-12` (`spec/09-compat.md:34`, `CMP-020`):** our text lists `supportedInterfaces` bindings as `JSON-RPC/HTTP+SSE` and `gRPC`, omits required `defaultInputModes`, `defaultOutputModes`, `protocolVersion`, and `skills[].tags`, and says `"also signed variant if configured"` vs upstream `a2a.proto` requires official `protocolBinding` strings `"JSONRPC"`, `"HTTP+JSON"`, `"GRPC"`, requires `defaultInputModes`, `defaultOutputModes`, `supportedInterfaces[].protocolVersion`, and `skills[].tags`, and embeds detached JWS signatures inline in `AgentCard.signatures[]` over RFC 8785 JCS JSON rather than a separate endpoint.
- **`Q-13` (`spec/09-compat.md:35-37`, `CMP-021..023`):** our text uses bare metadata keys `gap.hop` and `gap.path` vs upstream A2A v1.0 requires extensions to be identified by versioned URIs declared in `AgentCard.capabilities.extensions[]`, activated via `A2A-Extensions`, and keyed by that URI inside `metadata` (plus `A2A-Version: 1.0` header, `returnImmediately` defaulting to `false`, and upstream's internal `a2a.proto` `GET /tasks/{id}:subscribe` vs prose `POST` discrepancy).
- **`Q-14` (`spec/09-compat.md:35,63`, `CMP-021`):** our text requires an A2A conformance fixture for `"streaming resume by seq"` vs upstream A2A v1.0 `SubscribeToTaskRequest` (`{ tenant?, id }`) and `StreamResponse` have no sequence number `seq` (resubscription emits the current `Task` snapshot followed by live updates).
- **`Q-16` (`spec/02-gap-ql.md:6,22`, `GQL-001`, `GQL-002`):** our SDL at `spec/02-gap-ql.md:22` explicitly declares `directive @oneOf on INPUT_OBJECT` vs upstream `GraphQL September 2025 Edition` defines `@oneOf` as a built-in directive (`Appendix D`); which GraphQL edition `apollo-compiler 1.33.1` implements is `UNVERIFIABLE-OFFLINE` from `.local/specs-staging/`.
- **`Q-17` (`spec/02-gap-ql.md:249,253,275-277`, `docs/01-DECISIONS.md:155`, `GQL-003`, `ADR-023`):** our text maps `GAP_PARSE_ERROR` / `GAP_VALIDATION_ERROR` (`GAP_UNKNOWN_FIELD`) to `"HTTP 200 if a response document is produced"` with fixture `"status": 200, "dataNull": true` vs upstream GraphQL `September2025` forbids `"data"` on pre-execution request/validation errors and `GraphQLOverHTTP.md` MUST NOT return `2xx` when `"data"` is absent (`400` for JSON/GraphQL parse errors, `422` for validation/coercion/operation-resolution errors).

## WP-M7-12 Addendum: Dual-Stack HTTP/3 (QUIC) & Clean Supply Chain (2026-11-16)

- **Added HTTP/3 + QUIC transport pins (`ADR-026`, `OPS-005`):** `h3 0.0.8`, `h3-quinn 0.0.10`, `quinn 0.11.12` (`runtime-tokio`, `rustls-ring`, `bloom`, `default-features = false`), `rcgen 0.14.10` (`crypto`, `pem`, `ring`, `default-features = false`), `rustls 0.23.46` (`ring`, `std`, `tls12`), and `rustls-pki-types 1.16.0` (`std`).
- **Removed unmaintained/advisory crates:** Removed `im 15.1.0` (replaced by an owned `#![forbid(unsafe_code)]` persistent AVL `OrdMap<K, V>` in `crates/gap-core/src/ord_map.rs`) and `rustls-pemfile` (replaced by `rustls_pki_types::pem::PemObject`), and upgraded `pulldown-cmark` to `0.13.6` (`RUSTSEC-2026-0433`).
- **Verification:** `cargo deny --offline check advisories bans licenses sources` (`ignore = []`) passes with `0` advisories, `0` bans, `0` license violations, and single-version `axum 0.8.9`, `hyper 1.12.0`, `prost 0.14.4`, and `tower 0.5.3`.
