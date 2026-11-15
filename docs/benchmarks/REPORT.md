# GAP 1.0-RC Performance & Benchmark Report (`ADR-021`, `ADR-017`, `CMP-008`, `RTE-069`)

This report publishes all measured performance, token-efficiency, and routing-quality evidence for Graph Agent Protocol (`GAP`) at commit `e31d7e0`. Per `ADR-021`, only directly measured results from `bench/results/gapbench/`, `bench/results/routebench/final/`, `bench/results/routebench/s1_comparison/`, and `bench/results/perf/` are reported; unexecuted external gates (live multi-provider LLM `G-9` runs) are explicitly marked pending.

---

## 1. Environment & Provenance

| Property | Measured Value |
|---|---|
| Git commit | `e31d7e0` |
| Host OS / Arch | `macos/x86_64` |
| Cargo profile | `release` (`--offline`) |
| GAP-Bench driver | `scripted` deterministic harness (`Direct-MCP` vs `GAP MCP 2026-07-28 Facade`) |
| N-sweep | `[10, 50, 200, 1000]` tools (`2`, `5`, `20`, `100` branches) |
| Token estimators | Primary `GQL-022` `ceil(utf8_bytes / 3.6)`; secondary `ceil(utf8_bytes / 4)` |
| Cached model assumptions | `1.25x` cold prefix write amortized over suite tasks + `0.10x` cached prefix read per round-trip + `1.0x` dynamic turn tokens |
| RouteBench split | `5` branches (`4` in-domain, `1` held-out); `fit=124`, `cal=40`, `test=48` (`42` ID, `6` OOD), `held-out=72`; seed `20261115`, `1000` bootstrap resamples |
| Reproduction script | [`bench/results/gapbench/reproduce.sh`](../../bench/results/gapbench/reproduce.sh) |
| Raw artifacts | [`bench/results/gapbench/baseline.json`](../../bench/results/gapbench/baseline.json), [`bench/results/routebench/final/report.json`](../../bench/results/routebench/final/report.json), [`bench/results/routebench/s1_comparison/s1_comparison.json`](../../bench/results/routebench/s1_comparison/s1_comparison.json) |

### Scope Caveat (`G-9` Live LLM Suite)
The local reproduction ran in deterministic `--scripted` mode (air-gapped environment with no external model API keys). Live LLM driver evaluation (`gapbench --llm-endpoint ...`) covering `G-9` token/round-trip variance under real frontier tool-use models remains scheduled for pre-GA online CI.

---

## 2. GAP-Bench Results (`CMP-008`, `RTE-034`)

### 2.1 Gate Verdicts

| Gate | Criterion | Measured | Verdict |
|---|---|---|---|
| `G-F1` (`CMP-008`) | `gap_tools_list <= 1,200` tokens across all `N` | max `651` (`GQL-022`), `586` (`ceil/4`) | **PASS** |
| `G-C1` (M2) | `code_graph` tokens `<= 15%` of file-scan at `>=` accuracy | `0.84%` uncached / `0.48%` cached (`1.000` vs `1.000` accuracy) | **PASS** |
| `__search` Recall@10 (M2) | `recall@10 >= 0.95` across `N` | min `1.0000` | **PASS** |
| `__search` Latency (M2) | `p95 <= 40.0 ms` across `N` | max `3.618 ms` (at `N=1000`) | **PASS** |

### 2.2 Discovery & Schema Footprint Scaling (`tools/list`, `__metaGraph`, `__search`)

| `N` Tools | Branches | GAP `tools/list` (`GQL-022`) | Direct-MCP `tools/list` (`GQL-022`) | `__metaGraph` Tokens | `__search` Recall@10 | `__search` p50 (ms) | `__search` p95 (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 2 | 651 | 1,904 | 58 (2 br) | 1.0000 | 0.307 | 0.413 |
| 50 | 5 | 651 | 10,066 | 140 (5 br) | 1.0000 | 0.487 | 0.553 |
| 200 | 20 | 651 | 40,586 | 558 (20 br) | 1.0000 | 0.835 | 0.928 |
| 1000 | 100 | 651 | 203,421 | 2,777 (100 br) | 1.0000 | 2.384 | 3.618 |

GAP's MCP facade exports the exact 5 static tools (`gap_search`, `gap_describe`, `gap_query`, `gap_mutate`, `gap_task`), holding schema prompt overhead constant at **651 tokens** regardless of catalog growth from `10` to `1,000` tools (where Direct-MCP expands to **203,421 tokens**).

### 2.3 Full Task Suite Comparison (`Direct-MCP` vs `GAP-Facade`, Uncached & Cached)

| Suite | `N` | Direct 1st-Call Prompt | GAP 1st-Call Prompt | Direct Uncached / Succ | GAP Uncached / Succ | Uncached Ratio (G/D) | Direct Cached / Succ | GAP Cached / Succ | Cached Ratio (G/D) | Round-Trips (D / G) | Success (D / G) | Latency p50/p95 ms (Direct) | Latency p50/p95 ms (GAP) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `api_lookup` | 10 | 1,942.3 | 2,829.1 | 2,041.6 | 5,823.0 | **2.852x** (`285.2%`) | 547.2 | 4,691.9 | **8.575x** (`857.5%`) | 1.0 / 2.0 | 100% / 100% | 0.025 / 0.029 | 0.711 / 1.190 |
| `cross_branch_join` | 10 | 1,956.0 | 4,099.0 | 8,891.8 | 8,517.9 | **0.958x** (`95.8%`) | 2,200.5 | 7,383.7 | **3.355x** (`335.5%`) | 4.0 / 2.0 | 100% / 100% | 0.072 / 0.089 | 1.034 / 1.209 |
| `api_lookup` | 50 | 10,104.7 | 2,846.6 | 10,205.5 | 5,859.5 | **0.574x** (`57.4%`) | 2,385.2 | 4,728.0 | **1.982x** (`198.2%`) | 1.0 / 2.0 | 100% / 100% | 0.017 / 0.022 | 1.843 / 1.987 |
| `cross_branch_join` | 50 | 10,118.0 | 4,114.0 | 41,539.8 | 8,547.9 | **0.206x** (`20.6%`) | 6,740.7 | 7,413.7 | **1.100x** (`110.0%`) | 4.0 / 2.0 | 100% / 100% | 0.056 / 0.061 | 2.060 / 2.331 |
| `api_lookup` | 200 | 40,624.4 | 2,850.2 | 40,724.3 | 5,865.5 | **0.144x** (`14.4%`) | 9,251.2 | 4,734.9 | **0.512x** (`51.2%`) | 1.0 / 2.0 | 100% / 100% | 0.018 / 0.024 | 6.426 / 6.683 |
| `cross_branch_join` | 200 | 40,638.0 | 4,114.0 | 163,619.8 | 8,547.9 | **0.052x** (`5.2%`) | 23,717.4 | 7,413.7 | **0.313x** (`31.3%`) | 4.0 / 2.0 | 100% / 100% | 0.060 / 0.095 | 6.558 / 7.073 |
| `api_lookup` | 1000 | 203,459.7 | 2,856.6 | 203,559.5 | 5,878.0 | **0.029x** (`2.9%`) | 45,889.1 | 4,747.0 | **0.103x** (`10.3%`) | 1.0 / 2.0 | 100% / 100% | 0.018 / 0.019 | 33.115 / 34.120 |
| `cross_branch_join` | 1000 | 203,473.0 | 4,114.0 | 814,959.8 | 8,547.9 | **0.010x** (`1.0%`) | 114,294.4 | 7,413.7 | **0.065x** (`6.5%`) | 4.0 / 2.0 | 100% / 100% | 0.063 / 0.065 | 33.618 / 38.316 |
| `code_graph` | 24 | 700.0 | 759.0 | 141,681.3 | 1,187.4 | **0.008x** (`0.8%`) | 137,810.4 | 660.4 | **0.005x** (`0.5%`) | 11.0 / 1.0 | 100% / 100% | 0.752 / 0.980 | 1.041 / 2.266 |

---

## 3. Where GAP Does Not Win (Honest Regression & Trade-Off Analysis)

Per `ADR-021`, we highlight every region in the parameter sweep where GAP underperforms Direct-MCP:

1. **Small tool catalogs (`N = 10`, uncached & cached):**
   - For single-step `api_lookup` at `N = 10`, Direct-MCP serializes all 10 tool schemas in `1,904` tokens and finishes in `1.0` turn (`2,041.6` uncached / `547.2` cached tokens). GAP sends `651` static meta-tool tokens plus discovery (`gap_search`/`__metaGraph` + `gap_describe`) across `2.0` turns, consuming **`5,823.0` uncached (`2.852x` worse)** and **`4,691.9` cached (`8.575x` worse)** tokens.
   - For `cross_branch_join` at `N = 10`, uncached tokens are nearly tied (`8,517.9` GAP vs `8,891.8` Direct, `0.958x`), but when provider prefix caching is enabled Direct wins comfortably (**`2,200.5` Direct vs `7,383.7` GAP, `3.355x` worse**).
2. **Medium catalogs with hot prompt caching (`N = 50`, cached):**
   - Because Direct-MCP's `10,066`-token static tool prefix is discounted to `0.10x` reads under warm prefix caching while GAP's dynamic discovery turn cannot be cached across heterogeneous user intents, Direct beats GAP in **cached** tokens at `N = 50`: `2,385.2` vs `4,728.0` (`1.982x` worse) on `api_lookup`, and `6,740.7` vs `7,413.7` (`1.100x` worse) on `cross_branch_join`.
3. **Additional round-trip on cold discovery (`api_lookup`):**
   - Direct-MCP executes simple known single-tool calls in `1.0` LLM turn whenever all tool definitions sit in context. GAP's discover-describe-query cycle requires `2.0` turns on cold `api_lookup` tasks (unless the client reuses APQ / direct native GAP queries).
4. **In-process execution & indexing latency at `N = 1000`:**
   - Direct mock dispatch completes in `0.019`–`0.065 ms` p95 because schemas are already in context. GAP executes full lexical/graph discovery, query validation, Cedar authorization, and composition (`34.120 ms` p95 for `api_lookup`, `38.316 ms` p95 for `cross_branch_join` at `N=1000`). While negligible relative to network LLM generation (`0.5`–`3 s`), GAP adds tens of milliseconds at 1,000-tool scale.

**Operational takeaway:** Do **not** deploy GAP solely for token savings when integrating `<= 50` mostly independent flat RPC tools on cache-warm providers — GAP's justification at `N <= 50` is governance (Cedar RBAC, SSRF/sandbox isolation, rug-pull pinning, hash-chained audit), server-side joins, or graph queries (`code_graph` wins by **`119.32x` uncached / `208.68x` cached** even at 24 symbols).

---

## 4. `ADR-017` "Replacement" Adoption Criterion Evaluation

`ADR-017` specifies that GAP ships an **MCP `2026-07-28` + A2A v1.0 facade** on day 1 so existing hosts adopt GAP with zero protocol changes, while GAP-Bench measures when native GAP transport ("replacement") is justified:

| Regime | MCP Facade vs Direct-MCP Crossover | `ADR-017` Verdict |
|---|---|---|
| Single-branch RPC, **uncached** | GAP wins cleanly by **`N = 50`** (`0.574x`), scaling to **`34.63x`** savings at `N = 1000`. | **Facade Wins (`N >= 50`)** |
| Single-branch RPC, **cached** (`0.10x` prefix) | Crossover falls between `N = 50` (`1.982x`) and **`N = 200`** (`0.512x`), reaching **`9.67x`** savings at `N = 1000`. | **Facade Wins (`N >= ~100–200`)** |
| Multi-branch join (`cross_branch_join`) | Uncached wins from **`N = 10`** (`0.958x` -> `95.34x` at `1000`); cached crosses right above **`N = 50`** (`1.100x` -> `15.42x` at `1000`). Round-trips drop **`4.0 -> 2.0`**. | **Facade + Server Joins Win (`N >= ~60` cached)** |
| Typed context / code graph (`code_graph`) | Immediate win at `24` symbols: **`119.32x` uncached**, **`208.68x` cached**, round-trips **`11.0 -> 1.0`**. | **Dominant Immediate Win** |

**Recorded Criterion (`ADR-017`):**
1. The **MCP facade satisfies 1.0-RC adoption goals** for catalogs with **`N >= 200` tools** (both cached and uncached) and for any workload involving cross-branch joins or `ContextStore` graph traversal, without modifying host runtimes.
2. **Replacing MCP with native GAP-QL (`HTTP`/`SSE`/APQ)** eliminates the remaining **`651`-token 5-tool wrapper** and removes the extra discovery round-trip on repeated/persisted workflows, yielding full value once a host supports typed GraphQL operations natively or experiences `N <= 50` cached-prompt sensitivity.

---

## 5. Final RouteBench & System-1 (`S1`) Evaluation (`RTE-069`, `G-S1`)

### 5.1 Published `routebench/final` Baseline Run

| Split | Examples | Top-1 [95% CI] | Top-3 | MRR | ECE_15 | Conformal Coverage (target `0.927`) | p50 / p95 Latency (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|
| Test (`42` ID, `6` OOD) | 48 | **0.9524** `[0.8837, 1.0000]` | 0.9762 | 0.9702 | 0.7304 | 0.0000 (fallback) | 0.625 / **0.882** |
| Held-out cold start | 72 | **0.9722** `[0.9306, 1.0000]` | 1.0000 | 0.9838 | 0.8385 | 0.0000 (fallback) | 0.654 / **1.143** |

Selective accuracies on Test at `50%` / `70%` / `90%` coverage: `0.8333` / `0.8824` / `0.8636`; OOD abstention AUROC: `0.6508`.

### 5.2 S1 Neural Router vs Deterministic Baseline (`bench/results/routebench/s1_comparison/s1_comparison.json`)

| Router | Test Top-1 | Held-Out Top-1 | Test `ECE_15` | Test Conformal Coverage | Test p95 (ms) | `G-S1` Verdict (`accuracy`, `ece`, `cov`, `lat`) |
|---|---:|---:|---:|---:|---:|---|
| `baseline` (lexical + structural) | **0.9524** | **0.9722** | 0.7304 | 0.0000 | **0.887** | **BASELINE_FALLBACK** (`false, false, false, true`) |
| `s1_forward` (`laya-candle:f32`, `16,616 KiB` RSS) | 0.0714 | 0.0278 | 0.0445 | 0.0000 | 42.339 | FAIL (`false, true, false, false`) |
| `s1_debiased` (Laya + PMI debiasing) | 0.1429 | 0.0278 | **0.0303** | 0.0000 | 73.851 | FAIL (`false, true, false, false`) |

**Analysis (`INV-17`, `RTE-044`, `RTE-069`):**
- Builtin default un-finetuned `laya-candle:f32` weights calibrate probabilities cleanly (`ECE_15 = 0.0303 <= 0.05` under debiasing, which outperforms forward pass by `+0.0714` top-1 in `debias_eval.json`), **but** lag dramatically in accuracy (`14.29%` vs `95.24%` baseline) and exceed the `40.0 ms` p95 CPU SLO (`42.3 ms`–`73.9 ms`).
- Meanwhile, the deterministic lexical/structural **baseline** achieves **`95.24%` test top-1 / `97.22%` held-out top-1 in `< 1 ms` p95**, but emits uncalibrated raw scores (`ECE_15 = 0.7304`) with `n < 20` per-bucket calibration counts (`bucket 2..10` fall back to safe temperature defaults without conformal sets).
- **Resulting 1.0-RC Policy:** System-1 fails `G-S1` on shipped weights (`overall_pass = false`). Per `INV-17` and `RTE-044`, **1.0-RC ships safely on the deterministic baseline (`BASELINE_FALLBACK`)** by default; neural S1 routing stays opt-in until fine-tuned task-specific weights achieve `>= +0.03` over baseline within `p95 <= 40.0 ms`.

---

## 6. Cross-Link to M3 Microbenchmarks & X-1 Columnar Assessment (`GQL-003`)

- **Engine microbenchmarks ([`bench/results/perf/README.md`](../../bench/results/perf/README.md)):** M3 criterion baselines (`m3-baseline.json`) establish deterministic join/execution latencies without regressions through M7.
- **Extension X-1 (`@format(as: COLUMNAR)`, `spec/02-gap-ql.md` §12, `GQL-003`):**
  - End-to-end GAP-Bench token consumption across `api_lookup`, `cross_branch_join`, and `code_graph` is governed by schema discovery (`203,421 -> 651` tokens) and server-side multi-hop reductions (`4 -> 2` and `11 -> 1` round-trips), **not** repeated JSON property keys in large array payloads.
  - Because GAP-Bench measures `< 20%` suite-level end-to-end token savings attributable to wire columnarization across the 3 benchmark suites, wire activation of `@format(as: COLUMNAR)` is evaluated and formally closed for `1.0-RC` in `WP-M7-09` (`docs/decisions/x1-columnar.md`).
