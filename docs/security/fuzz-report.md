# GAP 1.0-RC Fuzzing, Sanitizer & Miri Verification Report (`ING-012`, `GQL-002`, `RTE-021`, `SEC-100`, `ADR-021`)

- **Work Package:** `WP-M7-06`
- **Toolchain Measured Locally:** `rustc 1.98.1` (stable, offline), `cargo 1.98.1`, `cargo-nextest 0.9.149`
- **Verification Gate (`docs/03-VERIFICATION.md` §9):** `G-14` (`24 h` per fuzz target + ASan/TSan + Miri)

---

## 1. Seed Corpus Inventory (`fuzz/corpus/`)

All `10` fuzz targets declared in [`fuzz/Cargo.toml`](../../fuzz/Cargo.toml) now ship deterministic valid and adversarial boundary seed files under `fuzz/corpus/<target>/` (`34` seed files total):

| Fuzz Target | Harness Status (`fuzz/fuzz_targets/*.rs`) | Seed Count | Seed Categories (`fuzz/corpus/<target>/`) | Spec Coverage |
|---|---|---:|---|---|
| `fuzz_gapql_parse` | Wired (`parse_executable_ast` + reserved + uniqueness + `validate_query_with_visibility`) | 4 | Valid lookup, `__search`, depth bomb (`25` nested selections), `__schema` introspection rejection | `GQL-001`, `GQL-002` |
| `fuzz_extract_openapi` | Wired (`OpenApiExtractor`) | 3 | Minimal OpenAPI 3.0.3 JSON, prompt-poisoned operation description, malformed `$ref` JSON | `ING-012` |
| `fuzz_extract_proto` | Wired (`GrpcExtractor`) | 3 | Minimal `proto3` service, nested `oneof`/`map`, unclosed broken `.proto` | `ING-012` |
| `fuzz_extract_graphql` | Wired (`GraphqlExtractor`) | 3 | Minimal GraphQL SDL, introspection JSON stub, cyclic type definition | `ING-012` |
| `fuzz_extract_sql` | Wired (`SqlExtractor`) | 3 | Minimal DDL, FK + index DDL, SQL comment & quoted-identifier injection | `ING-012` |
| `fuzz_extract_mcp` | Wired (`McpExtractor` + `A2aExtractor`) | 3 | MCP tools JSON, A2A AgentCard JSON, bidi U+202E / delimiter injection | `ING-012`, `TM-01` |
| `fuzz_sanitize` | Wired (`gap_route::sanitize`, checks idempotency + length cap + marker stripping) | 4 | Clean query, NFKC fullwidth + zero-width + bidi text, `<|im_start|>`/`[INST]` markers, `2 KiB` padding | `RTE-020`, `RTE-021` |
| `fuzz_ngc_decode` | `libfuzzer-sys` stub awaiting nightly CI harness completion (covered by `gap-proto`/`gap-ngc-sdk` unit & proptests) | 4 | Empty frame, short varint header, truncated varint, oversized length prefix | `NGC-010`, `ING-012` |
| `fuzz_cursor_decode` | `libfuzzer-sys` stub awaiting nightly CI harness completion (covered by `gap-exec`/`gap-store` cursor tests) | 4 | Empty bytes, canonical cursor shape, tampered signature payload, invalid UTF-8 bytes | `GQL-021`, `INV-09` |
| `fuzz_policy_input` | `libfuzzer-sys` stub awaiting nightly CI harness completion (covered by `gap-sec` authz & `session_risk` tests) | 3 | Minimal principal context JSON, Rule-of-Two `(A, B, C)` input, empty `{}` object | `SEC-020`, `SEC-084` |

---

## 2. Measured Air-Gapped Property & Adversarial Suite (`PROPTEST_CASES=1024`)

In this air-gapped environment, high-iteration randomized property testing (`PROPTEST_CASES=1024`) was executed across the workspace's pure, parser, router, security, ingestion, store, and wire crates:

```bash
PROPTEST_CASES=1024 CI=1 cargo nextest run --no-pager -j 4 \
  -p gap-core -p gap-ql -p gap-balance -p gap-sec \
  -p gap-ingest -p gap-ngc-sdk -p gap-route -p gap-store --offline
```

- **Result:** **`365 tests run: 365 passed, 0 skipped`** (`1586.861 s`, `25` test binaries, exit `0`).
- **Triage Summary:**
  - `0` panics, `0` integer overflows, and `0` invariant violations across all `365` test suites.
  - Notably covered at `>= 1024` generated cases per property:
    - `gap-balance`: `breaker_differential_proptest_2000_cases_against_reference_model` and concurrent 64-thread CAS transitions (`INV-02`).
    - `gap-ql` & `gap-route`: hostile GraphQL validation bounds (`GQL-002`, `GQL-040`) and idempotent Unicode NFKC sanitization (`RTE-021`).
    - `gap-sec`: per-tenant rate buckets, policy surface evaluation (`TM-21`, `TM-22`), Rule-of-Two session risk (`TM-05`), redaction (`TM-18`), and Ed25519 audit anchors (`TM-17`).
    - `gap-store`: 3-tenant interleaved property testing against reference models (`inv_09_three_tenant_interleaved_sequences_match_isolated_models`), FTS5 hostile operator escaping (`hostile_fts5_syntax_never_errors_and_never_acts_as_operators`), and traversal determinism up to 40 nodes (`ING-067`).

---

## 3. Honest Environment Disclosure & Nightly CI Gate `G-14` (`ADR-021`)

Per `ADR-021`, the following components of `WP-M7-06` / release gate `G-14` **could not be executed locally** on stable air-gapped `rustc 1.98.1` without nightly toolchains or unvendored `cargo-fuzz` network crates, and remain **OPEN (`PENDING-CI`)** prior to final `1.0.0` GA sign-off:

1. **Continuous `cargo-fuzz` (`24 h` per target):**
   - Requires nightly `rustc`, `cargo-fuzz` (`0.13.2`), and building the standalone [`fuzz/Cargo.toml`](../../fuzz/Cargo.toml) workspace (`libfuzzer-sys = "0.4.13"` is outside the root offline Cargo vendor set); additionally, the 3 remaining stubs (`fuzz_ngc_decode`, `fuzz_cursor_decode`, `fuzz_policy_input`) need their inner parse calls connected prior to their `24 h` campaigns.
   - CI command per target:
     ```bash
     cd fuzz && cargo +nightly fuzz run <target> corpus/<target> -- -max_total_time=86400
     ```
2. **ASan / TSan Sanitizer Sweeps (`-Zsanitizer=address`, `-Zsanitizer=thread`):**
   - Requires `-Zsanitizer` flags on nightly `rustc`.
   - CI commands:
     ```bash
     RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --workspace --target x86_64-unknown-linux-gnu
     RUSTFLAGS="-Zsanitizer=thread"  cargo +nightly test -p gap-exec -p gap-balance -p gap-snapshot -p gap-server --target x86_64-unknown-linux-gnu
     ```
3. **Miri Undefined-Behavior Check on Pure Crates (`gap-core`, `gap-ql`, `gap-balance`):**
   - Note that every workspace crate except `gap-s1` enforces `#![forbid(unsafe_code)]`, limiting UB surface exclusively to safe-Rust pointer/FFI/unsafe-in-dependencies paths (`rusqlite`, Candle FFI in `gap-s1`).
   - CI command:
     ```bash
     cargo +nightly miri test -p gap-core -p gap-ql -p gap-balance
     ```
