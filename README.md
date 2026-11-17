# GAP — Graph Agent Protocol

> **Languages:** **English** · [Tiếng Việt (README.vi.md)](README.vi.md)

**Graph Agent Protocol (GAP)** is an Apache-2.0 (`Copyright 2026 Mike (vnknowledge2014)`), strongly typed GraphQL-profile gateway and governance layer unifying heterogeneous AI agent backends (OpenAPI REST, GraphQL subgraphs, gRPC, SQL, CLI tools, AST code/doc graphs, external MCP servers, A2A peer tasks, and AG-UI v1.0 frontends) behind **progressive disclosure**, **batched federation**, **dual-stack HTTP/2 + HTTP/3 (QUIC) transport**, **multi-provider System-1 decision models (`Laya`, `Jev`, `gemini-decision-1`)**, **deterministic Cedar authorization**, and **wait-free immutable snapshots**.

**Status (`0.1.0-rc.1` · `2026-11-17`):** **87 / 87 work packages complete (`main`)**, verified end-to-end offline (`362` spec requirements / `89` MUST / `2,515` test markers, `135/136` conformance tests, `32` Rust workspace crates, `0` `RUSTSEC` advisories via owned AVL `OrdMap`). Canonical repo: [`github.com/vnknowledge2014/GraphProtocol`](https://github.com/vnknowledge2014/GraphProtocol) · Live Portal, Promo Videos & Paper: [`vnknowledge2014.github.io/GraphProtocol`](https://vnknowledge2014.github.io/GraphProtocol/) ([`/paper/`](https://vnknowledge2014.github.io/GraphProtocol/paper/) · [English Promo Video](web/media/gap-promo-en.mp4) · [Vietnamese Promo Video](web/media/gap-promo-vi.mp4)).

---

## Measured Highlights (`0.1.0-rc.1`, `ADR-021`)

| Dimension | Measured Value | Evidence Artifact |
|---|---|---|
| Static MCP `2026-07-28` schema cost | **`651` tokens** constant for `N in {10..1,000}` (vs `203,421` Direct-MCP at `N=1,000`) | [`docs/benchmarks/REPORT.md`](docs/benchmarks/REPORT.md) |
| Cross-branch join token reduction (`N=1,000`) | **`99.0%`** uncached (`95.34x`) · **`93.5%`** cached (`15.42x`) · turns `4 -> 2` | [`bench/results/gapbench/baseline.json`](bench/results/gapbench/baseline.json) |
| Code-graph traversal (`24` symbols) | **`119.32x`** uncached · **`208.68x`** cached · turns `11 -> 1` | [`bench/results/gapbench/baseline.json`](bench/results/gapbench/baseline.json) |
| Honest negative crossover (`ADR-017`) | Direct-MCP wins flat `N=10` cached (`547` vs `4,692` tokens) and `N=50` cached (`2,385` vs `4,728`) | [`docs/benchmarks/REPORT.md`](docs/benchmarks/REPORT.md) |
| Deterministic router vs System-1 (`G-S1`) | Baseline **`95.24%`** test / **`97.22%`** held-out top-1 at **`0.887 ms`** p95 (`INV-17` fallback) | [`docs/decisions/G-S1.md`](docs/decisions/G-S1.md) |
| Bit-reproducible `--locked` release binary | **`28.1 MB`** (`28,084,064` B), identical double-build SHA-256 (`d3220637...ee965d4c2`), **`0` RUSTSEC advisories** | [`bench/results/ga_local/report.json`](bench/results/ga_local/report.json) |
| Live cold start / RSS / TCP + HTTP/3 QUIC / `SIGKILL` | **`1,055 ms`** boot · **`9.32 MiB`** idle · **`1,064 req/s`** TCP (`11.54 ms` p95) · **`5,013 req/s`** HTTP/3 QUIC (`2.36 ms` p95, `0-RTT` off, `0` errs) · **`28.86 ms`** `kill -9` recover | [`bench/results/ga_local/report.json`](bench/results/ga_local/report.json) |

---

## Quickstart & Local Verification

```bash
# 1. Run 5-minute walking skeleton (REST shop + MCP notes + GraphQL/MCP gateway)
./target/dist/gap config check examples/walking-skeleton/gap.toml
python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &
python3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &
./target/dist/gap serve --config examples/walking-skeleton/gap.toml

# 2. Reproduce GAP-Bench & live GA local runtime/SIGKILL/reproducibility drills
./bench/results/gapbench/reproduce.sh
python3 dist/verify-ga-local.py

# 3. Validate specs, workspace layering, traceability matrix, and paper bundle
python3 tools/wp.py validate && python3 tools/check_layers.py && python3 tools/spec_trace.py --strict
./paper/build-arxiv-bundle.sh
```

---

## Documentation, Specification & Academic Paper

| Resource | Location |
|---|---|
| **Interactive Learning Portal & Promo Videos (EN / VI)** | [`web/README.md`](web/README.md) · [`web/index.html`](web/index.html) · [`gap-promo-en.mp4`](web/media/gap-promo-en.mp4) · [`gap-promo-vi.mp4`](web/media/gap-promo-vi.mp4) · `python3 web/serve.py --port 8787` |
| **Academic Paper (LaTeX / HTML / PDF / Markdown)** | [`paper/main.tex`](paper/main.tex) · [`paper/gap-paper.pdf`](paper/gap-paper.pdf) · [`docs/paper/GAP-PAPER.md`](docs/paper/GAP-PAPER.md) |
| **ASD-STE100 Aircraft Manual (Karpathy Style + Linter)** | [`docs/ste100/README.md`](docs/ste100/README.md) · `python3 dist/check-ste100.py` |
| **arXiv / Zenodo DOI Publication & Linux GA Playbook** | [`docs/paper/PUBLICATION-AND-GA-PLAYBOOK.md`](docs/paper/PUBLICATION-AND-GA-PLAYBOOK.md) · [`dist/set-author.py`](dist/set-author.py) |
| **1.0-RC Release Gates & Traceability Matrix** | [`docs/gates/RC.md`](docs/gates/RC.md) · [`docs/traceability.md`](docs/traceability.md) |
| **Quickstart, Concepts, Connector & Security Guides** | [`docs/guides/quickstart.md`](docs/guides/quickstart.md) · [`docs/guides/concepts.md`](docs/guides/concepts.md) · [`docs/guides/connector-authoring.md`](docs/guides/connector-authoring.md) · [`docs/guides/security-hardening.md`](docs/guides/security-hardening.md) |
| **Normative Specification (`00`–`09`), ADRs & Design Review** | [`spec/`](spec/) · [`docs/01-DECISIONS.md`](docs/01-DECISIONS.md) · [`docs/00-REVIEW.md`](docs/00-REVIEW.md) |
| **Wire Contracts & Work Packages (`87/87` complete)** | [`proto/`](proto/) · [`work/work-packages.toml`](work/work-packages.toml) · [`work/status.toml`](work/status.toml) |

---

## How to Cite GAP

Standard software and paper citation metadata lives in [`CITATION.cff`](CITATION.cff) and [`.zenodo.json`](.zenodo.json). Author identity & repository URL are managed reproducibly across all citation, LaTeX, and notice files via:

```bash
python3 dist/set-author.py --check
```

`Copyright 2026 Mike (vnknowledge2014)` · License: [Apache-2.0](LICENSE) · Provenance & Third-Party Notices: [`NOTICE`](NOTICE)
