# GAP-Bench v1 Baseline Results (`ADR-021`, `CMP-008`, `RTE-034`)

Measured in-process end-to-end comparison between **Direct-MCP** (loading all `N` tool schemas into prompt context, plus filesystem grep/read loops for code navigation) and the **GAP MCP `2026-07-28` Facade** (`McpFacade` over `TransportEngine`, `ExecLayer`, `RouteDiscoveryService`, and `SqliteContextStore`).

## Environment & Provenance

- **Git commit:** `e31d7e0`
- **Host platform:** `macos/x86_64` (`release` profile)
- **Agent driver:** `scripted`
- **N-sweep:** `[10, 50, 200, 1000]`
- **Token estimators:** Primary `GQL-022` `ceil(utf8_bytes / 3.6)`; secondary `ceil(utf8_bytes / 4)`. Cached token accounting bills `1.25x` cold prefix write amortized over suite tasks + `0.10x` cached prefix read per round-trip + `1.0x` dynamic turn tokens.

## Gate Verdicts

| Gate | Criterion | Measured | Verdict |
|---|---|---|---|
| `G-F1` (`CMP-008`) | `gap_tools_list <= 1,200` tokens across `N` | max `651` (`GQL-022`), `586` (`ceil/4`) | **PASS** |
| `G-C1` (M2) | `code_graph` tokens `<= 15%` of file-scan at `>=` accuracy | `0.84%` uncached (`1.000` vs `1.000` accuracy) | **PASS** |
| `__search` Recall@10 (M2) | `recall@10 >= 0.95` across `N` | min `1.0000` | **PASS** |
| `__search` Latency (M2) | `p95 <= 40.0 ms` across `N` | max `3.618 ms` | **PASS** |

## Discovery & Schema Footprint Scaling (`tools/list`, `__metaGraph`, `__search`)

| `N` Tools | Branches | GAP `tools/list` (`GQL-022`) | Direct-MCP `tools/list` (`GQL-022`) | `__metaGraph` Tokens | `__search` Recall@10 | `__search` p50 (ms) | `__search` p95 (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 2 | 651 | 1904 | 58 (2 br) | 1.0000 | 0.307 | 0.413 |
| 50 | 5 | 651 | 10066 | 140 (5 br) | 1.0000 | 0.487 | 0.553 |
| 200 | 20 | 651 | 40586 | 558 (20 br) | 1.0000 | 0.835 | 0.928 |
| 1000 | 100 | 651 | 203421 | 2777 (100 br) | 1.0000 | 2.384 | 3.618 |

## End-to-End Task Suites (`Direct-MCP` vs `GAP-Facade`)

| Suite | `N` | Direct 1st-Call Prompt | GAP 1st-Call Prompt | Direct Uncached / Succ | GAP Uncached / Succ | Reduction | Direct Cached / Succ | GAP Cached / Succ | Round-Trips (D / G) | Success (D / G) | Latency p50/p95 ms (G) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `api_lookup` | 10 | 1942.3 | 2829.1 | 2041.6 | 5823.0 | 0.35x (285.2%) | 547.2 | 4691.9 | 1.0 / 2.0 | 100% / 100% | 0.711 / 1.190 |
| `cross_branch_join` | 10 | 1956.0 | 4099.0 | 8891.8 | 8517.9 | 1.04x (95.8%) | 2200.5 | 7383.7 | 4.0 / 2.0 | 100% / 100% | 1.034 / 1.209 |
| `api_lookup` | 50 | 10104.7 | 2846.6 | 10205.5 | 5859.5 | 1.74x (57.4%) | 2385.2 | 4728.0 | 1.0 / 2.0 | 100% / 100% | 1.843 / 1.987 |
| `cross_branch_join` | 50 | 10118.0 | 4114.0 | 41539.8 | 8547.9 | 4.86x (20.6%) | 6740.7 | 7413.7 | 4.0 / 2.0 | 100% / 100% | 2.060 / 2.331 |
| `api_lookup` | 200 | 40624.4 | 2850.2 | 40724.3 | 5865.5 | 6.94x (14.4%) | 9251.2 | 4734.9 | 1.0 / 2.0 | 100% / 100% | 6.426 / 6.683 |
| `cross_branch_join` | 200 | 40638.0 | 4114.0 | 163619.8 | 8547.9 | 19.14x (5.2%) | 23717.4 | 7413.7 | 4.0 / 2.0 | 100% / 100% | 6.558 / 7.073 |
| `api_lookup` | 1000 | 203459.7 | 2856.6 | 203559.5 | 5878.0 | 34.63x (2.9%) | 45889.1 | 4747.0 | 1.0 / 2.0 | 100% / 100% | 33.115 / 34.120 |
| `cross_branch_join` | 1000 | 203473.0 | 4114.0 | 814959.8 | 8547.9 | 95.34x (1.0%) | 114294.4 | 7413.7 | 4.0 / 2.0 | 100% / 100% | 33.618 / 38.316 |
| `code_graph` | 24 | 700.0 | 759.0 | 141681.3 | 1187.4 | 119.32x (0.8%) | 137810.4 | 660.4 | 11.0 / 1.0 | 100% / 100% | 1.041 / 2.266 |
