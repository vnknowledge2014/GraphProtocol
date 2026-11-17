# Changelog

All notable changes to GAP (Graph Agent Protocol) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
generated and curated from Conventional Commits (`OPS-040`), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (`ADR-024`).
Before `1.0.0`, breaking wire or API changes occur only in minor versions and
must include a migration note here alongside the corresponding conformance diff.

## [Unreleased]

## [0.1.0-rc.1] - 2026-11-16

First Release Candidate (`1.0-RC`) implementing the full GAP protocol, dual-stack HTTP/2 + HTTP/3 QUIC gateway runtime, three-axis open agent facades (MCP `2026-07-28`, A2A `v1.0`, AG-UI `v1.0`), multi-provider System-1 decision models (`Laya`, `Jev`, `gemini-decision-1`), six native connectors, security & isolation pipeline, benchmark harness, chaos/soak/upgrade tests, and reproducible signed release pipeline across all 87 work packages (`M0`–`M7` + `R1`).

### Added

- **Foundation (`M0`):** Normative specification (`spec/00`–`spec/09`), protobuf wire contracts compiled via pure-Rust `protox` (`NGC-001`), 31-crate strictly layered workspace (`ARC-001`–`ARC-003`), telemetry/logging initialisation, and automated CI validation gates (`tools/wp.py`, `tools/check_layers.py`, `tools/spec_trace.py`, `tools/check_refs.py`).
- **Typed GAP-QL Gateway (`M1`):** Complete GAP-QL lexer/parser/validator, query planner with bounded cost estimation and plan cache, execution engine with batched DataLoader federation (`INV-18`), HTTP/SSE and gRPC transport handlers (`gap-transport`), gateway assembly (`gap-server`), and `gap` CLI walking skeleton (`examples/walking-skeleton/`).
- **Durability & Snapshots (`M2`):** Lock-free `ArcSwap` `SnapshotHandle` with per-request generation pinning (`INV-01`, `INV-02`), automatic `RollbackWatcher` error-spike reversion, SQLite `ContextStore` (FTS5 lexical search, online `backup_to`/`restore_from` `OPS-043`, schema migration & `FormatTooNew` guard `OPS-041`), BLAKE3 tamper-evident `AuditLog` (`INV-10`), `TaskStore` lease crash recovery (`RUN-091`), and `IdempotencyStore`.
- **Connectors & Ingestion (`M4`):** `gap-ngc-sdk` and six reference Native Graph Connectors (`OpenAPI`, `GraphQL`, `gRPC`, `SQL`, `CLI`, `Code`), ingestion proposal lifecycle (`propose` / `approve` / `apply`), CISG `UntrustedText` sanitization and injection linter (`SEC-080`, `SEC-081`), plus `DriftMonitor` fail-closed `BranchStatus::Drift` rug-pull protection (`ING-052`, `SEC-082`, `INV-08`).
- **Discovery, Routing & Track `R1` (`M5`, `R1`):** Hybrid BM25 + HNSW discovery index (`gap-index`), calibrated `gap-route` discovery (`GAP_ABSTAINED` / conformal bounds), optional Candle S1 adapter (`gap-s1`, `INV-17`), and formal `G-S1` / `R1` evaluations (`docs/decisions/G-S1.md`, `docs/decisions/r1-custom-s1.md`).
- **Compatibility Facades, Ops & Optional Backend (`M6`, `WP-M7-11`):** Inbound/outbound MCP facade (`<= 1,200` tokens `tools/list`) and A2A facade (`adapters/gap-a2a`), `gap-conformance` offline conformance suite + NGC kit coverage across all 6 shipped connectors, Prometheus/Grafana/OTel artifacts (`ops/`), `gap` operational subcommands (`doctor`, `config`, `models`, `snapshot`, `audit`, `proposals`, `calibrate`), mdBook documentation site (`book/`, `docs/hosts/`), and optional SurrealDB store module (`--features store-surreal`) with zero BSL crate contamination in `Cargo.lock`.
- **Hardening, Benchmarks & Release Packaging (`M7`):**
  - `GAP-Bench` and `RouteBench` reproducible evaluation suite (`docs/benchmarks/REPORT.md`, `bench/results/gapbench/reproduce.sh`).
  - Pure lock-free `ColumnarTable` internal codec and `X-1` closure record (`docs/decisions/x1-columnar.md`).
  - Seed corpora for all 10 fuzz targets (`fuzz/corpus/`), `1024`-case proptest verification, and external pen-test checklist (`docs/security/fuzz-report.md`, `docs/security/pentest-scope.md`).
  - `N-1 -> N` upgrade, backup/restore, chaos, and configurable soak suites (`tests/upgrade/upgrade.rs`, `tests/soak/soak.rs`, `docs/gates/soak.md`).
  - Multi-stage non-root distroless `Dockerfile`, reproducible build script (`dist/build-reproducible.sh`), CycloneDX 1.5 SBOM generator (`dist/generate-sbom.py`), `dist/dist-workspace.toml`, model integrity guide (`dist/MODELS.md`), GitHub Actions signed release workflow (`.github/workflows/release.yml`), and `1.0-RC` readiness gate report (`docs/gates/RC.md`).
  - **Dual-Stack HTTP/3 (QUIC, RFC 9114) & Supply-Chain Hygiene (`WP-M7-12`, `ADR-026`, `OPS-005`):** Added concurrent QUIC/UDP HTTP/3 listener (`quinn` + `h3` + `h3-quinn`) with TLS 1.3 ALPN `h3`, explicit `0-RTT` replay prevention (`max_early_data_size = 0`), `Alt-Svc` advertisement on TCP HTTP/1.1+HTTP/2, fail-closed `prod` TLS secret validation, pure-Rust zero-`unsafe` persistent AVL `OrdMap` removing `im`, and direct `rustls-pki-types` PEM parsing removing `rustls-pemfile` (`0` advisories under `cargo deny check advisories bans licenses sources`).
  - **AG-UI v1.0, Multi-Provider System-1 Decision Models & Bilingual Video Showcase (`WP-M7-13`, `ADR-027`, `ADR-028`):** Added `adapters/gap-agui` (`Layer L4`, `/ag-ui` SSE + `/ag-ui/capabilities`) completing the 3-axis stack (MCP + A2A + AG-UI) with approval interrupts (`CMP-027`), event compaction (`CMP-026`), and escaped `<gap:untrusted>` spotlighting (`CMP-028`); added `@receptron/laya`, TypeSafe `Jev` (`/v1/systemone`), and Google `gemini-decision-1` (`/v1beta/decisions`) adapters in `gap-s1` + `[router]` config (`RTE-047`..`049`, `INV-15`, `INV-17`); produced English & Vietnamese 1080p promo videos (`web/media/gap-promo-{en,vi}.mp4`) embedded in the learning portal (`32` workspace crates).

### Security

- Cedar policy + capability deterministic authorization engine (`INV-04`), JWT/API-key authentication, DNS-pinned `EgressGuard` SSRF protection (`SEC-075`), Seatbelt/Bubblewrap process sandbox (`SEC-070`, `SEC-071`), typed & pattern `Secret<T>` log/error redaction (`SEC-041`, `INV-15`), and per-`(tenant, principal)` / per-tenant `QuotaEnforcer` rate & resource limits (`SEC-050`, `RUN-050`).
- Fail-closed model manifest verification (`SEC-102`) and CycloneDX 1.5 SBOM enforcement ensuring zero BSL-1.1 packages in the default dependency graph (`SEC-101`, `ADR-020`).

### Changed / Decisions

- **Default Router Mode (`RTE-044`, `INV-17`):** Shipped `laya-candle:f32` failed gate `G-S1` (`0.0714` forward / `0.1429` debiased top-1, `0.6778` ECE) against the deterministic lexical/structural baseline (`0.9524` test top-1 / `0.9722` held-out at `0.887 ms` p95); `0.1.0-rc.1` defaults to `BASELINE_FALLBACK` (`docs/decisions/G-S1.md`, `docs/benchmarks/REPORT.md`).
- **Columnar Wire Extension (`GQL-003`):** Deferred `@format(as: COLUMNAR)` wire activation out of `1.0-RC` because `GAP-Bench` measured `< 20%` token reduction on the large-list suite (`docs/decisions/x1-columnar.md`).

### Earlier Bootstrap History

- Normative GAP `0.1.0-draft` specification (`spec/00`–`spec/09`), wire contracts (`proto/`), architecture decision records (`docs/01-DECISIONS.md`), verification gates (`docs/03-VERIFICATION.md`), 85-package execution plan (`work/work-packages.toml`), pinned Rust `1.98.1` toolchain (`rust-toolchain.toml`), Apache-2.0 `LICENSE`, and `NOTICE` (`WP-M0-01`).
