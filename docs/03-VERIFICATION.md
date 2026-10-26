# 03 — Verification Strategy and Release Gates

Normative IDs `VER-*`. Goal: every claim in `spec/` is backed by a test, a benchmark, or a recorded gate decision. "Production-grade stable" is defined operationally in §9.

## 1. Test pyramid and tooling
| Layer | Scope | Tooling | Runs |
|---|---|---|---|
| Unit | pure functions, state machines, math (cost model, calibration, conformal, EWMA, breaker) | `cargo nextest`, table-driven tests | every PR |
| Property | parsers, sanitizer, cursor encode/decode, join stitching, plan determinism, tenant scopes | `proptest` (≥ 256 cases PR, 10 000 nightly) | every PR / nightly |
| Concurrency | publisher/reclaimer/rollback FSM, telemetry counters, breaker windows | `shuttle` (randomized schedules, ≥ 1,000 iterations PR / 100,000 nightly); `loom` only for tiny lock-free structures; ≥ 64-thread stress | PR / nightly |
| Component | one crate with real dependencies (SQLite, in-process gRPC/HTTP) | `nextest` | PR |
| Contract / conformance | black-box GAP-QL, NGC, MCP facade, A2A, tasks, security | `gap-conformance` + fixtures | PR (offline subset) / nightly (live) |
| Differential | GAP-QL validator vs graphql-js | node harness | PR |
| E2E | walking skeleton, real MCP host smoke, multi-node peers | `tests/e2e`, MCP Inspector | PR (scripted) / nightly (real host) |
| Fuzz | parser, NGC decoders, extractors, sanitizer, cursor, policy inputs | `cargo-fuzz` | 60 s PR smoke / 1 h nightly / 24 h per target before RC |
| Performance | microbench + load | `criterion`, `hdrhistogram`, `oha`/`k6` | `--quick` PR / full nightly |
| Quality of intelligence | RouteBench, GAP-Bench, enrichment golden set | dedicated bins | smoke PR / full nightly |
| Chaos / soak | fault injection, 72 h soak | testkit chaos + scripts | subset nightly / full before RC |
| Security | SEC §15 matrix, SSRF table, canary secrets, sandbox escape smoke | dedicated tests | PR / nightly |
| Mutation | selected crates | `cargo-mutants` | nightly |
| Sanitizers | ASan/TSan builds of the test-suite; miri on pure crates | nightly toolchain | weekly / before RC |

## 2. Spec traceability
- **VER-001** `tools/spec_trace.py` is a CI gate: *every* `MUST`/`MUST NOT` requirement ID has ≥ 1 test marker (`spec: <ID>`), and every marker references an existing ID. `--warn` until M1 exit; `--strict --milestone Mx` at each milestone gate; `--strict` for RC.
- **VER-002** Invariants INV-01…20 each have a test named `inv_<nn>_*`; the gate WPs list them.
- **VER-003** The requirement→test matrix is generated (`docs/traceability.md`) and published with each release.

## 3. Conformance suite
- **VER-010** Layout `conformance/<class>/<name>.json` (spec/02 §11 format) with fields `name, class[], spec[], schema?, principal?, request|steps, expect`. Classes: Core, Discovery, Federation, Tasks, Secure, Facade-MCP, Facade-A2A, plus `ngc/` connector kit.
- **VER-011** `gap-conformance` modes: `--offline <dir>` (in-process server from fixture schema), `--target <url>` (live), `--spawn <example>` (starts example project), `--connector-kit` (NGC), `--report <path>` (JSON + spec-id coverage).
- **VER-012** Fixtures are the *protocol*: they MUST NOT depend on Rust internals. A second implementation (even a minimal one in another language) should be able to run them; a TypeScript mock server in `tests/ts-ref/` demonstrates this for Core (post-M1 stretch).

## 4. Property and fuzz targets (named; each owns a stub in `fuzz/fuzz_targets/`)
`fuzz_gapql_parse`, `fuzz_ngc_decode`, `fuzz_extract_openapi`, `fuzz_extract_proto`, `fuzz_extract_graphql`, `fuzz_extract_sql`, `fuzz_extract_mcp`, `fuzz_sanitize`, `fuzz_cursor_decode`, `fuzz_policy_input`.
- **VER-020** Each target: no panics/aborts/OOM, per-input time ≤ 1 s, memory ≤ 512 MiB; corpus checked into `fuzz/corpus/` (minimized); crashes become regression tests under the owning crate.

## 5. Performance methodology
- **VER-030** Every SLO has a benchmark with: environment capture (CPU model, cores, RAM, OS, rustc, commit, `Cargo.lock` hash), warm-up, ≥ 30 samples (criterion) or ≥ 5 min steady state (load), reported percentiles via HdrHistogram with coordinated-omission correction for load tests.
- **VER-031** CI regression rule: PR benches run `--quick`; a regression > 10% on a tracked microbench fails only if reproduced on a re-run on the same runner; nightly full benches compare to the last release baseline (alert at +10%, fail RC at +15%).
- **VER-032** Load profile for M3/RC: 10k rps mixed (60% single-branch read, 25% joined read, 10% discovery, 5% mutation/task) for 5 min (M3) / 30 min (RC), zero 5xx not attributable to injected faults.
- **VER-033** Cold start, memory (RSS at idle, after warm-up, after 1 h load), fd/thread counts are recorded in each run.

## 6. Quality-of-intelligence protocols
- **VER-040 (N+1 audit)** For every join test: assert `Σ upstream_calls` per RHS branch ≤ `⌈unique_keys/chunk⌉` using `Complete.upstream_calls` (native-batch connectors) or the declared emulation bound (emulated connectors, reported separately). A regression to N calls fails CI (INV-18).
- **VER-041 (RouteBench)** Dataset generation seed-controlled and versioned; splits by operation id; reported with 95% bootstrap CIs; the *same* code computes metrics in CI smoke and nightly; thresholds in ADR-007/RTE-069. Reliability diagrams stored as SVG artifacts.
- **VER-042 (GAP-Bench)** Protocol per ADR-021: tool counts {10, 50, 200, 1000}; suites (API lookup, cross-branch join, code-graph questions); metrics (prompt tokens before first useful call, tokens per solved task, success rate, round-trips, latency); with and without provider prompt caching; baselines: direct MCP with all tools loaded, direct MCP with tool-search, GAP via MCP facade; ≥ 3 runs per cell (LLM agents) or deterministic scripted agent; raw transcripts retained. **Report must include cells where GAP does not win.**
- **VER-043 (Enrichment golden set)** per ING-034; adversarial additions each release.
- **VER-044 (Code-graph efficiency)** per ING-063 on three public repositories (Rust, TS, Python) with a fixed question set and gold answers; answer accuracy judged by exact-match on symbol sets (no LLM judge for the gate).

## 7. Reliability tests
- **VER-050** `swap_stress` (RUN-008) nightly; **VER-051** crash recovery (kill -9 during task execution, during snapshot publish, during audit write); **VER-052** chaos suite (spec/08 §7) — subset nightly, full before RC; **VER-053** soak 72 h (OPS-030); **VER-054** upgrade N-1→N (snapshot, stores, config) and backup/restore drill; **VER-055** graceful shutdown with in-flight requests/tasks.

## 8. CI job map
| Trigger | Jobs |
|---|---|
| PR | fmt, clippy, build (macOS+Linux), nextest, doc, deny/audit, layers, wp validate, spec_trace (mode per milestone), conformance offline, fuzz smoke (60 s × changed targets), benches `--quick`, routebench smoke (if relevant paths), mdbook build/test |
| Nightly | proptest long, shuttle long, fuzz 1 h, benches full, GAP-Bench smoke, RouteBench full, chaos subset, swap_stress, mutants, coverage, live conformance |
| Weekly | sanitizers (ASan/TSan), miri, dependency freshness report, RUSTSEC full scan, 8 h soak |
| Release | everything above + 72 h soak, 24 h fuzz/target, upgrade tests, SBOM/signing, reproducible-build check |

## 9. Release gates for 1.0-RC (checklist executed by WP-M7-10; each line needs an evidence link)
**Functional**
- [ ] G-1 All conformance classes pass 100% (Core, Discovery, Federation, Tasks, Secure, Facade-MCP, Facade-A2A, NGC kit for every shipped connector).
- [ ] G-2 `spec_trace --strict`: 100% of MUST requirements have tests; 20/20 invariant tests exist and pass.
- [ ] G-3 Walking-skeleton and quickstart run from a clean machine following only the docs (recorded).
**Quality**
- [ ] G-4 `just verify` green on macOS and Linux; coverage ≥ 80% (L0–L3) and ≥ 70% overall; mutation score ≥ 60% on the designated crates.
- [ ] G-5 clippy `-D warnings`, `cargo deny`, `cargo audit` clean (no unexcused high/critical); no `unwrap/expect/panic/todo` in non-test code (clippy lints); `unsafe` only in `gap-s1`, all documented.
- [ ] G-6 0 open S0/S1 defects; S2 list triaged with owners.
**Performance & capacity (reference environment)**
- [ ] G-7 Typed-path overhead p99 ≤ 5 ms warm / ≤ 25 ms cold plan (RUN/ARC §8).
- [ ] G-8 Snapshot swap p99 ≤ 250 µs @256 readers; 0 failed/torn requests over 100 swaps × 1,000 concurrent requests.
- [ ] G-9 Discovery p95: baseline ≤ 40 ms; S1 (if enabled) ≤ 250 ms CPU / ≤ 80 ms GPU.
- [ ] G-10 10k rps mixed load for 30 min without unexplained errors; RSS ≤ budget (600 MB w/o S1; ≤ 2.0 GB with S1); cold start ≤ 2 s w/o S1.
**Reliability**
- [ ] G-11 72 h soak: RSS growth ≤ 5% after warm-up, no fd/thread leaks, p99 drift ≤ 10%, 0 audit-verify failures.
- [ ] G-12 Full chaos suite passes; crash-recovery tests pass (tasks resume/fail explicitly within 60 s); N-1→N upgrade and backup/restore drills pass.
**Security**
- [ ] G-13 SEC §15 matrix 17/17; threat model reviewed; residual risks accepted in writing.
- [ ] G-14 Fuzz: ≥ 24 h per target without crashes; sanitizer runs clean; external pen-test completed or formally waived with risk acceptance.
- [ ] G-15 Signed artifacts, SBOM, SLSA provenance, reproducible build check; model manifests verified.
**Intelligence & efficiency**
- [ ] G-16 Gate G-S1 decision recorded (S1 enabled or disabled by data); if enabled: ECE₁₅ ≤ 0.05, coverage ≥ 1−α−0.02, top-1/selective-accuracy +3 pts over baseline; retrieval recall@10 ≥ 0.95 either way.
- [ ] G-17 `tools/list` of the MCP facade ≤ 1,200 tokens for N ∈ {5…1,000} branches.
- [ ] G-18 GAP-Bench published with raw data; code-graph efficiency gate G-C1 met; claims in README/docs match measured results (including negative ones).
**Documentation & operations**
- [ ] G-19 Docs complete and tested (quickstart, concepts, connector authoring, security hardening, ops, migration, host guides); runbooks drilled at least once (game day).
- [ ] G-20 Dashboards and alert rules validated against chaos scenarios (each alert fired at least once in test).
**Legal**
- [ ] G-21 Licenses clean (`cargo deny`), `NOTICE` complete, model provenance/licence memo accepted; BSL components absent from default build.

## 10. Failure handling for gates
A failed gate blocks its milestone. The gate WP records: failing metric, root-cause hypothesis, remedial WPs (added to `work-packages.toml` and re-validated), and a re-run date. Thresholds are changed only through an ADR with explicit rationale and human approval (AGENT-GUIDE §11).
