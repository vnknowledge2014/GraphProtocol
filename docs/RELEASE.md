# Release Engineering and Error-Budget Policy

Normative sources: [08-operations.md](../spec/08-operations.md) (`OPS-001`, `OPS-003`, `OPS-030`, `OPS-040`–`OPS-044`), [03-VERIFICATION.md](03-VERIFICATION.md) (`VER-001`–`VER-055`, §8–§10), and [01-DECISIONS.md](01-DECISIONS.md) (`ADR-002`, `ADR-003`, `ADR-007`, `ADR-009`, `ADR-020`–`ADR-022`, `ADR-024`, `ADR-025`).

> [!IMPORTANT]
> Targets become active operational SLOs in the reference environment only after their owning milestone completes ([08-operations.md](../spec/08-operations.md) §5, `ADR-021`). No threshold or gate may be relaxed without an explicit superseding ADR and human approval ([03-VERIFICATION.md](03-VERIFICATION.md) §10, `ADR-022`).

---

## 1. Versioning, Naming, and Change Discipline

- **Artifact naming and SemVer (`OPS-040`, `ADR-024`):**
  - Working project name: `GAP`; server/admin binary: `gap`; Rust crates: `gap-*` (never `gap` or `gap-protocol`).
  - Initial specification version: `0.1.0-draft`; wire protocol identifier: `gap/0.1`.
  - Both the specification and workspace crates follow Semantic Versioning (`ADR-024`).
  - **Pre-1.0:** breaking wire changes are permitted only on minor version bumps and MUST include a migration note (`ADR-024`).
  - **Post-1.0:** `N-1` compatibility window across releases (`ADR-024`, `OPS-041`).
  - **Snapshot format:** `snapshot_format_version: u32` with an explicit migration trait (`ADR-024`, `OPS-043`).
- **Toolchain and dependency reproducibility (`ADR-002`):**
  - Rust edition `2024`, pinned in `rust-toolchain.toml` (`1.98.1`), with `Cargo.lock` committed and all crate versions centralized in `[workspace.dependencies]`.
  - Pre-release dependencies (`ort`) are exact-pinned (`=`) and feature-gated; `cargo tree -d` MUST report a single version each of `axum`, `hyper`, `prost`, and `tower`; `cargo-deny` enforces the license allow-list, `RUSTSEC` advisories, and duplicate bans (`ADR-002`).
- **Changelog and normative changes (`OPS-040`, `ADR-022`):**
  - [CHANGELOG.md](../CHANGELOG.md) follows Keep a Changelog and is generated from Conventional Commits (`OPS-040`).
  - Any protocol or specification change requires an ADR update/addition plus the corresponding conformance/test diff in the same change set (`OPS-040`, `ADR-022`).

---

## 2. Reference SLOs and Error-Budget Policy

### 2.1 Reference SLO Set

Measured in the reference environment once each owning milestone is complete ([08-operations.md](../spec/08-operations.md) §5, [03-VERIFICATION.md](03-VERIFICATION.md) §5 & §9):

| SLI | Objective | Window / Cadence | Governing IDs |
|---|---|---|---|
| Availability of `/graphql` (non-5xx, excluding branch failures attributable to branches) | `99.9%` | `30 d` | [08-operations.md](../spec/08-operations.md) §5 |
| Typed-path overhead p99 | `≤ 5 ms` warm (`≤ 25 ms` cold plan at RC) | per release bench | [08-operations.md](../spec/08-operations.md) §5, `ADR-003`, `G-7` |
| Snapshot swap p99 @256 busy readers | `≤ 250 µs` (`0` failed/torn requests across `100` swaps × `1,000` concurrent requests; reader p99 regression `≤ 5%`) | nightly | [08-operations.md](../spec/08-operations.md) §5, `ADR-009`, `VER-050`, `G-8` |
| Discovery p95 | `≤ 40 ms` baseline / `≤ 250 ms` S1 CPU (`≤ 80 ms` GPU if S1 enabled) | nightly | [08-operations.md](../spec/08-operations.md) §5, `ADR-007`, `G-9` |
| Task recovery after crash (`WORKING` tasks resumed or failed explicitly) | `100%` within `60 s` of start | per release chaos | [08-operations.md](../spec/08-operations.md) §5, `VER-051`, `G-12` |
| Audit hash-chain verification | `0` failures | nightly | [08-operations.md](../spec/08-operations.md) §5 |

### 2.2 Error-Budget Policy

1. **30-day availability freeze trigger ([08-operations.md](../spec/08-operations.md) §5):** If the `30 d` availability budget for `/graphql` (`99.9%` non-5xx, excluding upstream failures attributable to branches) is exhausted, **all feature merges pause except reliability fixes**.
2. **Release and milestone gate blocking ([03-VERIFICATION.md](03-VERIFICATION.md) §10):** Any failed milestone, performance, security, or quality gate blocks completion of that milestone or release candidate. The gate record MUST document the failing metric, root-cause hypothesis, remedial work packages, and re-run plan; thresholds are never lowered unilaterally.
3. **Benchmark regression gates (`VER-030`, `VER-031`):**
   - PR microbenchmarks (`criterion --quick`) fail when a tracked benchmark regresses by `> 10%` and reproduces on a re-run on the same runner (`VER-031`).
   - Nightly full benchmarks (`≥ 30` criterion samples or `≥ 5 min` steady-state load with HdrHistogram coordinated-omission correction, `VER-030`) compare against the last release baseline: alert at `+10%`, block RC at `+15%` (`VER-031`).
4. **In-service snapshot promotion guardrails (`ADR-009` §5–§6):**
   - Plan-cache prewarm replays the top-`100` plans; promotion is blocked if `< 95%` re-plan cleanly unless `breaking_change_ack` is explicitly set (`ADR-009` §6).
   - After promotion, a `60 s` watch window compares snapshot-attributable errors against the previous generation's baseline and auto-rolls back once iff the error rate exceeds `max(2× baseline, baseline + 2 pp)` over `≥ 200` samples; rolled-back generations are marked bad and cannot be re-promoted without `--force` (`ADR-009` §5).

---

## 3. Release Train and Verification Cadence

### 3.1 Branch Model and Artifact Provenance (`OPS-041`, `SEC-101`)

- **`main` branch (`OPS-041`):** kept continuously releasable; every PR must pass the PR verification matrix before merge.
- **`rc` branches (`OPS-041`):** cut for release candidates and subjected to the full release verification suite ([03-VERIFICATION.md](03-VERIFICATION.md) §8–§9) before tagging.
- **Supply-chain signing and provenance (`OPS-041`, `SEC-101`, `G-15`):** release artifacts are built reproducibly, cryptographically signed (`SEC-101`), published with an SBOM and SLSA provenance, and paired with verified model manifests (`ADR-005`).

### 3.2 CI and Release Verification Schedule ([03-VERIFICATION.md](03-VERIFICATION.md) §8)

| Cadence | Required Verification Jobs |
|---|---|
| **PR** | `fmt`, `clippy` (`-D warnings`), `build` (macOS + Linux), `nextest` (`proptest` `≥ 256` cases; `shuttle` `≥ 1,000` iterations), `doc`, `cargo deny` / `cargo audit`, `check_layers.py`, `wp.py validate`, `spec_trace.py` (`--warn` until M1 exit; `--strict --milestone Mx` at milestone gates; `VER-001`), offline conformance (`VER-010`, `VER-011`), `60 s` fuzz smoke on changed targets (`VER-020`), `criterion --quick` (`VER-031`), RouteBench smoke (`ADR-007`), `mdbook` build/test |
| **Nightly** | Extended `proptest` (`10,000` cases), extended `shuttle` (`100,000` iterations), `1 h` fuzz, full benchmarks (`VER-030`, `VER-031`), GAP-Bench smoke (`VER-042`), full RouteBench (`VER-041`), chaos subset (`VER-052`), `swap_stress` (`VER-050`), `cargo-mutants`, coverage, live conformance |
| **Weekly** | Sanitizers (`ASan`/`TSan`), `miri`, dependency freshness report, full `RUSTSEC` scan, `8 h` soak |
| **Release (`rc`)** | Everything above plus `spec_trace.py --strict` + generated `docs/traceability.md` (`VER-001`–`VER-003`), `72 h` soak (`OPS-030`, `VER-053`), `30 min` `10k rps` mixed load (`VER-032`), `24 h` fuzz per target (`VER-020`), full chaos + crash-recovery + graceful-shutdown suite (`VER-051`, `VER-052`, `VER-055`), `N-1 → N` upgrade and backup/restore drills (`OPS-041`, `OPS-043`, `VER-054`), SBOM/signing/reproducible-build verification (`OPS-041`, `SEC-101`) |

### 3.3 Summary of `1.0-RC` Readiness Gates (`G-1`–`G-21`, [03-VERIFICATION.md](03-VERIFICATION.md) §9)

- **Functional (`G-1`–`G-3`):** `100%` pass across all conformance classes (Core, Discovery, Federation, Tasks, Secure, Facade-MCP, Facade-A2A, NGC kit; `VER-010`), `100%` `MUST`/`MUST NOT` test traceability + `20/20` `inv_<nn>_*` invariant tests (`VER-001`, `VER-002`), clean-machine walking-skeleton and quickstart execution.
- **Quality (`G-4`–`G-6`):** `just verify` green on macOS and Linux; line coverage `≥ 80%` on L0–L3 crates and `≥ 70%` overall; mutation score `≥ 60%` on designated crates; clean `clippy -D warnings`, `cargo deny`, and `cargo audit` (no unexcused high/critical); zero non-test `unwrap`/`expect`/`panic`/`todo`; `unsafe` isolated to `gap-s1` and documented in [UNSAFE.md](UNSAFE.md) (`ADR-002`); `0` open S0/S1 defects and triaged S2 list.
- **Performance & Capacity (`G-7`–`G-10`, `VER-032`, `VER-033`):** warm/cold plan latency (`≤ 5 ms` / `≤ 25 ms`), swap latency (`≤ 250 µs`), discovery latency (`≤ 40 ms` baseline; `≤ 250 ms` CPU / `≤ 80 ms` GPU with S1), and `30 min` at `10k rps` mixed load (`60%` single-branch read, `25%` joined read, `10%` discovery, `5%` mutation/task) with zero unexplained errors, RSS `≤ 600 MB` without S1 (`≤ 2.0 GB` with S1), and cold start `≤ 2 s` without S1.
- **Reliability (`G-11`–`G-12`, `OPS-030`, `VER-051`–`VER-055`):** `72 h` soak at `30%` max throughput with periodic swaps, drift events, and chaos injections showing RSS growth `≤ 5%` after warm-up, no fd/thread leaks, p99 drift `≤ 10%`, and `0` audit-verify failures; full chaos suite, crash recovery (`≤ 60 s`), and `N-1 → N` upgrade/backup-restore drills passing.
- **Security (`G-13`–`G-15`):** `17/17` security matrix scenarios, reviewed threat model, `≥ 24 h` fuzzing per target across all `10` named targets (`VER-020`: per-input `≤ 1 s`, memory `≤ 512 MiB`), clean sanitizers, external pen-test (or formal risk acceptance), signed artifacts (`SEC-101`), SBOM, SLSA provenance, reproducible build, verified model manifests.
- **Intelligence & Efficiency (`G-16`–`G-18`, `ADR-007`, `ADR-021`, `VER-040`–`VER-044`):** N+1 join bound enforced (`INV-18`, `VER-040`); Gate `G-S1` recorded (`ADR-007`: if S1 enabled, `ECE ≤ 0.05`, conformal coverage `≥ 1 − α − 0.02`, `+3` points top-1 or selective accuracy over `BaselineRouter`; retrieval `recall@10 ≥ 0.95` either way); MCP facade `tools/list` `≤ 1,200` tokens across `5`..`1,000` branches (`G-F1`); GAP-Bench (`{10, 50, 200, 1000}` tools, `≥ 3` runs/cell, raw transcripts retained, negative cells reported) and code-graph efficiency gate `G-C1` published.
- **Documentation, Operations & Legal (`G-19`–`G-21`, `ADR-002`, `ADR-020`):** complete tested docs and host guides, game-day drilled runbooks, chaos-validated dashboards/alerts, clean `cargo deny` licenses, complete `NOTICE`, accepted model provenance memo, and zero BSL dependencies in default builds.

---

## 4. Feature Flags (`OPS-042`)

Risk-bearing or optional capabilities are gated behind explicit configuration or compile-time flags with documented defaults and tracked removal criteria (`OPS-042`):

| Flag | Scope | Governing Rules |
|---|---|---|
| `routing.model` | Runtime (`"baseline"` vs S1) | Defaults to `"baseline"` unless Gate `G-S1` passes on held-out RouteBench data (`ADR-007`, `OPS-042`) |
| `incremental` | Runtime / feature | Gates optional `@defer` / `@stream` incremental delivery (`OPS-042`, `ADR-025`) |
| `facade.spotlight` | Runtime | Controls `<gap:untrusted>` spotlighting posture on facade responses (`OPS-042`) |
| `ingest.auto_activate_t0` | Runtime policy | Permits automatic activation only for Ed25519-signed `T0` sources when explicitly enabled (`OPS-042`) |
| `saga` | Runtime | Gates multi-branch compensating saga mutations (`OPS-042`) |
| `store-surreal` | Cargo compile-time feature | Isolates optional SurrealDB `3.x` `ContextStore` backend so BSL-licensed code never enters default builds (`OPS-042`, `ADR-002`, `ADR-020`) |

---

## 5. Migrations, Upgrade Matrix, and Deployment Topologies

- **Forward-only migrations (`OPS-041`, `OPS-043`, `ADR-024`):**
  - Store and snapshot schema changes execute through a versioned `MigrationPlan` keyed by `format_version` (`OPS-043`, `ADR-024`).
  - Migrations are strictly **forward-only** and MUST provide a dry-run mode, checksum validation, automatic backup before apply, and verified backup/restore rollback procedures (`OPS-041`, `OPS-043`).
  - Every release runs the `N-1 → N` upgrade test matrix over snapshots, stores, and `gap.toml` configurations using fixtures retained from every prior released version (`OPS-041`, `OPS-043`, `VER-054`).
- **Deployment profiles and upgrade topology (`OPS-001`, `OPS-003`, `ADR-025`):**
  - Profiles (`OPS-001`): `dev` (anonymous allowed, `operation_mode=open`, verbose logs) and `prod` (authentication required, `operation_mode=hybrid`, `T2` hidden, sandbox mandatory).
  - Zero-downtime **binary** upgrades require the `OPS-003` active/standby (or external-store HA) topology behind an L7 load balancer; single-node listener socket transfer (`LISTEN_FDS` / `SCM_RIGHTS`) is an explicit 1.0 non-goal (`OPS-003`, `ADR-025`), while in-process snapshot swaps remain zero-drop (`ADR-009`).
- **Release & operations admin CLI (`OPS-044`):**
  - Operators inspect, verify, and roll back state via `gap`: `doctor`, `config {schema,check}`, `models {pull,verify}`, `snapshot {show,diff,rollback}`, `policy {check,reload}`, `audit {verify,export}`, `calibrate`, `bench`, and `purge`.
