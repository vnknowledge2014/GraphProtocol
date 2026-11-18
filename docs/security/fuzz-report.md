# GAP 1.0-RC Fuzzing, Sanitizer & Miri Verification Report (`ING-012`, `GQL-002`, `RTE-021`, `SEC-100`, `ADR-021`)

- **Work Package:** `WP-M7-06`
- **Toolchains Measured:** Local `rustc 1.98.1` + `cargo-nextest 0.9.149`; GitHub Actions `Nightly` [`44233039093`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233039093) (`nightly` + `cargo-fuzz` + `libFuzzer` + `ASan` on `x86_64-unknown-linux-gnu`)
- **Verification Gate (`docs/03-VERIFICATION.md` §9):** `G-14` (`10/10` Nightly `60 s`/target smoke green; `24 h`/target + full ASan/TSan/Miri tracked for `v1.0.0`)

---

## 1. Seed Corpus Inventory & `10/10` Active Fuzz Harnesses (`fuzz/corpus/`)

All `10` fuzz targets declared in [`fuzz/Cargo.toml`](../../fuzz/Cargo.toml) are wired end-to-end with deterministic valid and adversarial boundary seed files under `fuzz/corpus/<target>/` (`35` seed files total) and executed via `cargo +nightly fuzz run <target> --target x86_64-unknown-linux-gnu -- -max_total_time=60` in `Nightly` [`44233039093`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233039093):

| Fuzz Target | Harness Status (`fuzz/fuzz_targets/*.rs`) | Seed Count | Seed Categories (`fuzz/corpus/<target>/`) | Spec Coverage |
|---|---|---|---|---|
| `fuzz_gapql_parse` | Wired (`parse_executable_ast` + reserved + uniqueness + `validate_query_with_visibility`, `libFuzzer` + ASan green) | 4 | Valid lookup, `__search`, depth bomb (`25` nested selections), `__schema` introspection rejection | `GQL-001`, `GQL-002` |
| `fuzz_extract_openapi` | Wired (`OpenApiExtractor`, `libFuzzer` + ASan green) | 3 | Minimal OpenAPI 3.0.3 JSON, prompt-poisoned operation description, malformed `$ref` JSON | `ING-012` |
| `fuzz_extract_proto` | Wired (`GrpcExtractor`, `libFuzzer` + ASan green) | 3 | Minimal `proto3` service, nested `oneof`/`map`, unclosed broken `.proto` | `ING-012` |
| `fuzz_extract_graphql` | Wired (`GraphqlExtractor`, `libFuzzer` + ASan green; caught & fixed backslash + multibyte UTF-8 slice panic `VER-020`) | 4 | Minimal GraphQL SDL, introspection JSON stub, cyclic type definition, escaped multibyte UTF-8 description | `ING-012`, `VER-020` |
| `fuzz_extract_sql` | Wired (`SqlExtractor`, `libFuzzer` + ASan green) | 3 | Minimal DDL, FK + index DDL, SQL comment & quoted-identifier injection | `ING-012` |
| `fuzz_extract_mcp` | Wired (`McpExtractor` + `A2aExtractor`, `libFuzzer` + ASan green) | 3 | MCP tools JSON, A2A AgentCard JSON, bidi U+202E / delimiter injection | `ING-012`, `TM-01` |
| `fuzz_sanitize` | Wired (`gap_route::sanitize`, checks idempotency + length cap + marker stripping, `libFuzzer` + ASan green) | 4 | Clean query, NFKC fullwidth + zero-width + bidi text, `<|im_start|>`/`[INST]` markers, `2 KiB` padding | `RTE-020`, `RTE-021` |
| `fuzz_ngc_decode` | Wired (`prost::Message::decode` over 7 `gap.ngc.v1` wire types, `libFuzzer` + ASan green) | 4 | Empty frame, short varint header, truncated varint, oversized length prefix | `NGC-010`, `ING-012` |
| `fuzz_cursor_decode` | Wired (`gap_exec::join::decode_and_verify_cursor`, `libFuzzer` + ASan green) | 4 | Empty bytes, canonical cursor shape, tampered signature payload, invalid UTF-8 bytes | `GQL-021`, `INV-09` |
| `fuzz_policy_input` | Wired (`CedarSchema::parse` + `CompiledPolicySet::compile`, `libFuzzer` + ASan green) | 3 | Minimal principal context JSON, Rule-of-Two `(A, B, C)` input, empty `{}` object | `SEC-020`, `SEC-084` |

### Vulnerability Found & Fixed During Live `Nightly` Fuzzing (`VER-020`)
- During GitHub Actions `Nightly` [`44225447869`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44225447869), `fuzz_extract_graphql` discovered a real panic in `crates/gap-ingest/src/extractors/graphql/mod.rs` (`start byte index ... is not a char boundary` inside `'ढ'`) when a single-line GraphQL string literal contained a backslash escape immediately followed by a multi-byte UTF-8 character.
- Fixed in `308b2c4` by decoding a full UTF-8 scalar via `input[idx..].chars().next()` after `\`, advancing `idx += ch.len_utf8()`, adding regression unit test `extractor_graphql_escaped_multibyte_utf8_in_string_never_panics` (`// spec: ING-012`), and archiving seed `fuzz/corpus/fuzz_extract_graphql/escaped_multibyte_utf8.gql`. Subsequent `Nightly` [`44233039093`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233039093) ran all `10/10` targets with zero crashes.

---

## 2. Measured Property & Adversarial Suite (`PROPTEST_CASES=1024`)

High-iteration randomized property testing (`PROPTEST_CASES=1024`) executed across the workspace's pure, parser, router, security, ingestion, store, and wire crates:

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

## 3. Nightly CI Verification & Remaining `24 h` Pre-GA Gate `G-14` (`ADR-021`)

All `10/10` wired `libFuzzer` + `ASan` harnesses are verified green on `x86_64-unknown-linux-gnu` in `Nightly` [`44233039093`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233039093) (`60 s`/target bounded smoke). Per `ADR-021`, only the multi-day wall-clock endurance campaigns and full-workspace nightly sanitizers remain **OPEN (`PENDING-GA-ENDURANCE`)** prior to flipping `v0.1.0-rc.1` to `v1.0.0`:

1. **Continuous `cargo-fuzz` (`24 h` per target, `86400 s`):**
   ```bash
   cd fuzz && cargo +nightly fuzz run <target> --target x86_64-unknown-linux-gnu -- -max_total_time=86400
   ```
   (Executed in parallel for all 10 targets via `./dist/run-linux-ga-gates.sh --full-release` inside local 64 GB Colima Ubuntu 24.04 VM.)
2. **ASan / TSan Sanitizer Sweeps (`-Zsanitizer=address`, `-Zsanitizer=thread`):**
   ```bash
   RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --workspace --target x86_64-unknown-linux-gnu
   RUSTFLAGS="-Zsanitizer=thread"  cargo +nightly test -p gap-exec -p gap-balance -p gap-snapshot -p gap-server --target x86_64-unknown-linux-gnu
   ```
3. **Miri Undefined-Behavior Check on Pure Crates (`gap-core`, `gap-ql`, `gap-balance`):**
   - Every workspace crate except `gap-s1` enforces `#![forbid(unsafe_code)]`, limiting UB surface exclusively to FFI/unsafe-in-dependencies paths (`rusqlite`, Candle FFI in `gap-s1`).
   ```bash
   cargo +nightly miri test -p gap-core -p gap-ql -p gap-balance
   ```
