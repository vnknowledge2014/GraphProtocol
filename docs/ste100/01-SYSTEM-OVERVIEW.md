# System Architecture and Operating Limits `[DESCRIPTIVE]`

## 1. Core Function

The GAP `gateway` unifies REST APIs, SQL databases, gRPC services, GraphQL subgraphs, CLI tools, code graphs, MCP servers, A2A workers, and AG-UI frontends. Clients send `GAP-QL` requests over HTTP/1.1, HTTP/2, or HTTP/3 (QUIC). Clients can also call five MCP `facade` tools or stream `/ag-ui` events.

## 2. Three Architectural Planes

GAP separates runtime duties into three distinct planes (`ARC-002`):

1. **Plane 1 — Deterministic Execution Plane:** Executes authentication, Cedar policies, query planning, batched joins, and upstream calls with zero machine learning models.
2. **Plane 2 — Progressive Discovery Plane:** Ranks candidate operations through `L0`, `L1`, and `L2` introspection behind `visibility bitset` filters (`INV-05`).
3. **Plane 3 — Zero-Trust Control Plane:** Stages connector `proposal` objects, pins description hashes, and swaps `snapshot` generations without locks (`INV-01`, `INV-02`).

> **NOTE:** An optional neural router (`gap-s1`) advises Plane 2 discovery only. It never touches typed execution or authorization (`INV-04`, `INV-17`).

## 3. Key System Mechanisms

### Progressive Disclosure (`GQL-020`–`GQL-022`, `CMP-008`)
The MCP `facade` exports five fixed tools (`gap_search`, `gap_describe`, `gap_query`, `gap_mutate`, `gap_task`). The `tools/list` response uses exactly `651` tokens regardless of catalog size (`N = 10` to `1,000` tools). Agents query `__metaGraph` first, search operations with `__search`, and expand selected types with `__type`.

### Batched Joins and Lock-Free Reads (`INV-01`, `INV-02`, `INV-18`)
Each request pins one immutable `Arc<Snapshot>` upon arrival (`15 ns` load latency) and keeps that `generation` until exit. Zero `Mutex` or `RwLock` locks sit on the read path (`INV-02`). For cross-branch joins, the executor collects unique parent keys and sends bounded `BatchQuery` calls instead of `N+1` loops (`INV-18`).

### Default Router Behavior and External System-1 Models (`RTE-044`, `RTE-047`, `RTE-048`, `G-S1`)
Shipped un-finetuned `laya-candle:f32` weights scored `14.29%` debiased top-1 accuracy at `73.85 ms` p95 on `RouteBench`. The deterministic BM25 plus structural baseline achieved `95.24%` test top-1 accuracy at `0.887 ms` p95. GAP runs on `BASELINE_FALLBACK` by default (`INV-17`). Operators can also attach optional `@receptron/laya`, `Jev`, or `gemini-decision-1` discovery routers under `[router]` without entering Plane 1 execution (`ARC-002`, `INV-17`).

## 4. Measured Operating Limits and Crossover Rules (`ADR-017`, `ADR-021`)

| Regime or Drill | Direct-MCP Value | GAP Measured Value | Engineering Decision |
|---|---|---|---|
| `tools/list` schema at `N = 1,000` | `203,421` tokens | `651` tokens | GAP holds `O(1)` prompt schema cost (`G-F1` pass) |
| Cross-branch join tokens (`N = 1,000`) | `814,959.8` uncached | `8,547.9` uncached (`0.010x`) | GAP saves `99.0%` uncached tokens (`93.5%` cached) |
| Code-graph comprehension (`24` symbols) | `141,681.3` uncached | `1,187.4` uncached (`0.008x`) | GAP cuts round-trips `11.0 -> 1.0` (`208.7x` cached gain) |
| Small flat cached catalog (`N = 10`) | `547.2` cached | `4,691.9` cached (`8.575x`) | Use Direct-MCP if token budget outweighs security governance |
| Cold start and idle memory (`gap serve`) | `—` | `313.98 ms` · `9.24 MiB` RSS | Passes `<= 2,000 ms` and `<= 600 MiB` local gates |
| Mixed TCP load (`800` reqs, `8` threads) | `—` | `2,227.6 req/s` · `5.11 ms` p95 · `0` errs | Meets `VER-032` ratio offline |
| Multiplexed HTTP/3 QUIC load (`8` conns, `800` reqs) | `—` | `7,869.5 req/s` · `1.452 ms` p95 · `0` errs | Zero `0-RTT` replay (`OPS-005`) |
| OS `SIGKILL` (`kill -9`) restart | `—` | `12.96 ms` to HTTP `200 /readyz` | Passes `<= 60,000 ms` recovery gate (`VER-051`) |
