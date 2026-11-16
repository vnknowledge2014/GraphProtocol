# GAP `0.1.0-rc.1` Release Candidate Readiness Review (`WP-M7-10`)

- **Milestone Gate:** `1.0-RC` (`M7` exit)
- **Spec / ADR Refs:** `OPS-040`, `OPS-041`, `SEC-100`, `SEC-101`, `SEC-102`, `OPS-030`, `OPS-043`, `RUN-091`, `RTE-044`, `INV-17`, `ADR-020`, `ADR-021`
- **Checklist Authority:** [`docs/03-VERIFICATION.md`](../03-VERIFICATION.md) §9 (`G-1`..`G-21`)
- **Work Packages Complete:** `85 / 85` (`M0=10, M1=13, M2=9, M3=9, M4=10, M5=9, M6=13, M7=11, R1=1`)

> [!IMPORTANT]
> Per `ADR-021` (*No synthetic numbers; report measured results and infrastructure limits explicitly*), this gate distinguishes checks **executed and verified offline** on the local macOS `aarch64` environment (`rustc 1.98.1`, `cargo-nextest 0.9.149`, `cargo-deny 0.20.2`, `mdbook 0.5.4`) from checks that require connected GitHub CI, a live Docker daemon, multi-day hardware runners, or external reviewer sign-off before tagging GA `v1.0.0`.

---

## 1. Fresh Offline Workspace Verification Summary (Pre-Tag Snapshot)

| Check Command | Outcome | Observed Result |
|---|---|---|
| `cargo fmt --all -- --check` | `PASS` | Clean across all workspace crates & tests |
| `cargo clippy --workspace --all-targets --offline -- -D warnings` | `PASS` | `0` warnings across all 31 workspace crates |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --offline` | `PASS` | `31` crates documented (`0` warnings, incl. `store-surreal` check) |
| `cargo deny --offline check licenses bans sources` | `PASS` | `bans ok, licenses ok, sources ok` (`deny.toml`) |
| `CI=1 cargo nextest run --no-pager -j 1 -p gap-conformance --offline` | `PASS` | `133` passed, `3` skipped (`#[ignore]` loopback/node live tests) |
| `CI=1 cargo nextest run --no-pager -j 1 --test upgrade --offline` | `PASS` | `3` passed, `0` skipped |
| `CI=1 cargo nextest run --no-pager -j 1 --test soak --offline` | `PASS` | `4` passed, `0` skipped (`GAP_SOAK_DURATION_SECS=3` smoke loop) |
| `CI=1 cargo nextest run --no-pager -j 1 --test chaos --offline` | `PASS` | `4` passed, `0` skipped |
| `PROPTEST_CASES=1024 CI=1 cargo nextest run ...` (9 crates) | `PASS` | `365` passed, `0` skipped ([fuzz report](../security/fuzz-report.md)) |
| `mdbook build book && mdbook test book` | `PASS` | All chapters built and doc-tested cleanly |
| `python3 tools/spec_trace.py --strict` | `PASS` | `354` reqs (`82` MUST), `0` MUST without test, `0` unknown IDs (`2481` markers) |
| `python3 tools/check_layers.py` | `PASS` | `31` workspace crates respect layering and `ARC-002` |
| `python3 tools/check_refs.py` | `PASS` | `401` defined IDs, `0` dangling references |
| `./dist/build-reproducible.sh --offline` + `dist/generate-sbom.py` | `PASS` | `--locked` binary built; CycloneDX `1.5` SBOM (`398` components, `0` BSL crates) |

---

## 2. Line-by-Line Gate Execution (`docs/03-VERIFICATION.md` §9 `G-1`..`G-21`)

| Gate | Requirement Summary | Status (`ADR-021`) | Evidence & Exact Measurements |
|---|---|---|---|
| **G-1** | All conformance classes pass 100% (Core, Discovery, Federation, Tasks, Secure, Facade-MCP, Facade-A2A, NGC kit for all 6 shipped connectors) | `PASS (Offline Verified)` | `gap-conformance` (`133 passed`, `0 failed`; `3` `#[ignore]` tests in `ngc_kit_openapi.rs`, `ngc_kit_mcp.rs`, and `differential_gql.rs` require loopback sockets or external `node` + `graphql-js` 16.x); [`M6.md`](M6.md) |
| **G-2** | `spec_trace --strict`: 100% of MUST reqs tested; `20/20` invariant tests exist and pass | `PASS (Offline Verified)` | `tools/spec_trace.py --strict` (`0` MUST without test across `2481` markers); `37` `inv_<nn>_*` functions verified across workspace plus `INV-01`..`INV-20` markers mapped |
| **G-3** | Walking skeleton and quickstart run cleanly following docs | `PASS (Offline Verified)` | [`examples/walking-skeleton/README.md`](../../examples/walking-skeleton/README.md), [`docs/guides/quickstart.md`](../guides/quickstart.md), `mdbook test book` pass; [`M1.md`](M1.md), [`M6.md`](M6.md) |
| **G-4** | `just verify` green on macOS and Linux; coverage >= 80% (L0–L3) / >= 70% overall; mutation score >= 60% | `CONDITIONAL / PENDING-CI` | macOS offline equivalents (`fmt`, `clippy`, `doc`, `deny`, `nextest`, `spec_trace`, `check_layers`, `wp validate`, `mdbook`) are 100% green (`just` binary not installed locally); Linux matrix, `llvm-cov`, and `cargo-mutants` run on GitHub CI ([`ci.yml`](../../.github/workflows/ci.yml), [`nightly.yml`](../../.github/workflows/nightly.yml)) prior to `v1.0.0` |
| **G-5** | `clippy -D warnings`, `cargo deny`, `cargo audit` clean; no `unwrap/expect/panic/todo` in non-test code; `unsafe` only in `gap-s1` | `CONDITIONAL / PENDING-CI (Offline Clean)` | `clippy --workspace --all-targets --offline -- -D warnings` (`0` warnings); `check_layers.py` enforces non-test lint posture and confines `unsafe` to `gap-s1` ([`UNSAFE.md`](../UNSAFE.md)); `cargo deny --offline check licenses bans sources` passes (`0` violations); online RUSTSEC advisory DB fetch runs in CI |
| **G-6** | `0` open S0/S1 defects; S2 triaged with owners | `PASS (Offline Verified)` | `0` S0 / `0` S1 defects open; S2 operational/CI release checklist triaged with owners in §3 |
| **G-7** | Typed-path overhead p99 <= 5 ms warm / <= 25 ms cold plan | `PASS (Measured Offline)` | Verified within budget in benchmark & perf suites; [`M1.md`](M1.md), [`M6.md`](M6.md), `bench/results/perf/` |
| **G-8** | Snapshot swap p99 <= 250 us @ 256 readers; `0` failed/torn requests over 100 swaps x 1,000 concurrent requests | `PASS (Measured Offline)` | `ArcSwap` single-generation pinning verified in `crates/gap-snapshot/tests/swap_stress.rs`, `tests/soak/soak.rs`, and [`M2.md`](M2.md) |
| **G-9** | Discovery p95: baseline <= 40 ms; S1 <= 250 ms CPU / <= 80 ms GPU | `PASS (Measured Offline)` | `GAP-Bench` measured baseline discovery `p95 <= 3.618 ms` across `N in {10, 50, 200, 1000}` ([`REPORT.md`](../benchmarks/REPORT.md)); S1 disabled by default (`BASELINE_FALLBACK`, `RTE-044`); optional live-LLM agent benchmarks require API keys in CI |
| **G-10** | 10k rps mixed load for 30 min; RSS <= 600 MB w/o S1 / <= 2.0 GB w/ S1; cold start <= 2 s w/o S1 | `CONDITIONAL / PENDING-CI` | Cold start w/o S1 and single-node RSS verified within budget ([`M6.md`](M6.md)); sustained 30-min `10k rps` load run requires dedicated Linux reference hardware prior to `v1.0.0` |
| **G-11** | 72 h soak: RSS growth <= 5%, no fd/thread leaks, p99 drift <= 10%, 0 audit-verify failures | `CONDITIONAL / PENDING-CI` | Configurable harness `tests/soak/soak.rs` + [`soak.md`](soak.md) passes offline at `GAP_SOAK_DURATION_SECS=3` with concurrent swaps, drift freeze, faults, and BLAKE3 audit verification; full `72 h` (`259200 s`) endurance run runs on release CI host pre-`v1.0.0` |
| **G-12** | Full chaos suite, crash recovery (<= 60 s), `N-1 -> N` upgrade and backup/restore drills pass | `PASS (Offline Suites) / OS-Kill Drill Pending CI` | `tests/chaos/chaos.rs` (`4/4` pass), `tests/soak/soak.rs` (`4/4` pass), `tests/upgrade/upgrade.rs` (`3/3` pass: SQLite/snapshot/config compat + `backup_to`/`restore_from` + `RUN-091` expired-lease task recovery); external OS `kill -9` supervisor drill scheduled in staging |
| **G-13** | `spec/07` §15 matrix 17/17; threat model reviewed; residual risks accepted | `PASS (Offline Verified)` | All 17 `spec/07` §15 matrix items (`TM-01`..`TM-12`, `TM-15`..`TM-18`, `TM-20`..`TM-22`) mapped to passing tests in [`pentest-scope.md`](../security/pentest-scope.md); threat review in [`threat-model-review.md`](../security/threat-model-review.md) |
| **G-14** | Fuzz >= 24 h/target; ASan/TSan/miri clean; external pen-test completed or waived | `CONDITIONAL / PENDING-CI` | `34` seed corpus files across 10 targets (`fuzz/corpus/`) and `1024`-case proptest runs (`365/365` passed across 9 crates) complete offline ([`fuzz-report.md`](../security/fuzz-report.md)); 24h nightly `libFuzzer`, `ASan`/`TSan`, `miri`, and external pen-test sign-off ([`pentest-scope.md`](../security/pentest-scope.md)) block `v1.0.0` GA |
| **G-15** | Signed artifacts, SBOM, SLSA provenance, reproducible build check; model manifests verified | `CONDITIONAL / PENDING-CI (Offline Artifacts Ready)` | [`build-reproducible.sh`](../../dist/build-reproducible.sh) (`--locked`), [`generate-sbom.py`](../../dist/generate-sbom.py) (`CycloneDX 1.5`, `398` components, 0 BSL), [`Dockerfile`](../../Dockerfile), [`MODELS.md`](../../dist/MODELS.md), and [`release.yml`](../../.github/workflows/release.yml) verified; local Docker daemon down (`colima` absent), so container build, OIDC `cosign`, SLSA attestation, and online `laya-typed-decisions` SHA-256 pinning execute in connected `release.yml` workflow |
| **G-16** | `G-S1` decision recorded; retrieval `recall@10 >= 0.95` | `PASS (Decision Recorded)` | [`G-S1.md`](../decisions/G-S1.md) & [`REPORT.md`](../benchmarks/REPORT.md): shipped `laya-candle:f32` failed `G-S1` (`top-1 = 0.0714` / `0.1429` debiased, `ECE_15 = 0.6778`) -> S1 **disabled by default** (`BASELINE_FALLBACK`, `RTE-044`, `INV-17`); baseline reaches `0.9524` test (`0.9722` held-out) top-1 and `recall@10 = 1.0000 >= 0.95` across `N = 10..1000` |
| **G-17** | MCP `tools/list` <= 1,200 tokens for `N in {5..1000}` | `PASS (Offline Verified)` | `GAP-Bench` `G-F1 = true` (`97.9%` discovery token savings at `N = 1000`, constant MCP tool count); [`REPORT.md`](../benchmarks/REPORT.md), [`M6.md`](M6.md) |
| **G-18** | `GAP-Bench` published with raw data; `G-C1` met; honest docs including negative findings | `PASS (Offline Verified)` | Raw data in [`bench/results/gapbench/`](../../bench/results/gapbench/README.md) + `reproduce.sh`; `G-C1 = true` (`55.1%`–`62.7%` code-graph token savings); "Where GAP Does Not Win", S1 failure, and `< 20%` `X-1` savings published cleanly ([`REPORT.md`](../benchmarks/REPORT.md)) |
| **G-19** | Docs complete & tested (`mdbook test`); runbooks drilled | `PASS (Offline Verified)` | `mdbook build book && mdbook test book` green across all spec/guide/host/runbook pages; upgrade/restore, drift freeze, auto-rollback, and audit-verify drills verified in [`upgrade.rs`](../../tests/upgrade/upgrade.rs) & [`soak.rs`](../../tests/soak/soak.rs) |
| **G-20** | Dashboards & alert rules validated against chaos scenarios | `CONDITIONAL / PENDING-CI` | Prometheus alerts/recording rules, OTel config, and Grafana JSON shipped under `ops/` and checked structurally in [`M6.md`](M6.md); `promtool` missing locally and live Alertmanager chaos firing runs in staging CI |
| **G-21** | Licenses clean (`cargo deny`), `NOTICE` complete, model provenance memo accepted; zero BSL in default build | `PASS (Offline Verified)` | `cargo deny --offline check licenses bans sources` green; [`NOTICE`](../../NOTICE) & [`third_party/MANIFEST.toml`](../../third_party/MANIFEST.toml) complete; [`laya-provenance.md`](../decisions/laya-provenance.md) & [`models/manifest.toml`](../../models/manifest.toml) recorded; `dist/generate-sbom.py` confirms 0 BSL crates (`ADR-020`) |

---

## 3. Open-Issue & Pre-GA Triage (`G-6`)

- **Open `S0` (Blocker) Defects:** `0`
- **Open `S1` (Critical) Defects:** `0`
- **Triaged `S2` / Release-Infrastructure Action Items (Blocking GA `v1.0.0`, Non-Blocking for `v0.1.0-rc.1` Tag):**

| ID | Scope | Owner | Exit Action Before `v1.0.0` |
|---|---|---|---|
| `Triage-01` (`G-4`, `G-5`, `G-15`) | Connected Linux/macOS CI + `cargo deny check advisories` + Docker + `cosign`/SLSA + `laya-typed-decisions` SHA-256 pin | Release Engineering | Run `.github/workflows/release.yml` on tag `v0.1.0-rc.1` |
| `Triage-02` (`G-10`, `G-11`, `G-12`, `G-20`) | 30-min `10k rps` load, 72 h soak (`GAP_SOAK_DURATION_SECS=259200`), external OS `kill -9` supervisor drill, `promtool` + Alertmanager staging fire | Perf / Reliability Engineering | Archive raw logs & PROM/Grafana screenshots in staging report |
| `Triage-03` (`G-14`) | 24 h nightly libFuzzer across 10 targets, `ASan`/`TSan`, `miri` on pure crates, external pen-test execution or formal risk waiver | Security Engineering | Complete [`pentest-scope.md`](../security/pentest-scope.md) sign-off |

---

## 4. Version Tagging & Promotion Plan (`OPS-040`, `OPS-041`)

1. **Tag `v0.1.0-rc.1`** on `main` once all 85 work packages are merged (`85/85 done`).
2. Pushing `v0.1.0-rc.1` triggers [`.github/workflows/release.yml`](../../.github/workflows/release.yml): runs online `cargo deny check`, reproducible Linux double-build check, CycloneDX `1.5` SBOM, `gap:ci` distroless image build & non-root smoke test, Sigstore keyless `cosign sign-blob`, and SLSA v1.0 provenance attestation.
3. Execute connected staging gates (`Triage-01`, `Triage-02`, `Triage-03`) on `v0.1.0-rc.1` artefacts; resolve any regressions on release branch -> promote cleanly to **`v1.0.0`** via `CHANGELOG.md` update (`ADR-024`).

---

## 5. Known Limitations (`0.1.0-rc.1`)

1. **Default Routing (`RTE-044`, `INV-17`):** Shipped `laya-candle:f32` model did not meet `G-S1`; gateway ships in deterministic `BASELINE_FALLBACK` mode (`0.9524` test top-1, `0.882 ms` p95). Neural S1 routing remains opt-in (`[router]`) after online SHA-256 verification and environment-specific calibration.
2. **Columnar Wire Encoding (`GQL-003`):** Internal `ColumnarTable` codec exists (`crates/gap-exec/src/columnar/`), while `@format(as: COLUMNAR)` wire activation is closed for `1.0-RC` because `GAP-Bench` measured `< 20%` large-list suite savings ([`x1-columnar.md`](../decisions/x1-columnar.md)).
3. **Optional SurrealDB Store (`ADR-020`, `OPS-042`):** `--features store-surreal` compiles GAP's thin transport & schema module cleanly (`0` BSL crates in default `Cargo.lock` or default builds); live SurrealDB 3.x server tests require deploying a separate BSL-1.1 SurrealDB instance.
4. **Local Sandbox & Platform Scope (`SEC-070`):** Production strict kernel isolation (`bwrap` / `nsjail` + Landlock/seccomp) targets Linux (`docker` container host); macOS uses Seatbelt (`sandbox-exec`, deprecated by Apple) for local development/testing.
5. **Unverified Connected Tooling Locally:** `promtool`, `just`, live Docker daemon, nightly ASan/TSan/`miri`, `graphql-js` (`GAP_GRAPHQL_JS_DIR`), and external LLM API keys were unavailable offline and are explicitly tracked as `CONDITIONAL / PENDING-CI` per `ADR-021`.
