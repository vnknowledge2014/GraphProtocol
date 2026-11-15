# Decision Record: Defer / Close Wire Activation of Extension X-1 (`@format(as: COLUMNAR)`) for 1.0-RC (`GQL-003`, `ADR-021`)

- **Work Package:** `WP-M7-09`
- **Status:** Closed for `1.0-RC` (wire directive `@format(as: COLUMNAR)` remains deferred; lossless internal `gap_exec::columnar::ColumnarTable` codec retained)
- **Date:** `2026-11-16`
- **References:** `GQL-003`, `ADR-021`, `CMP-008`, `docs/benchmarks/REPORT.md`

## 1. Normative Criterion (`spec/02-gap-ql.md` §12, `GQL-003`)

`spec/02-gap-ql.md` §12 specifies:
> `@format(as: COLUMNAR)` on a list field returns `{"columns":["id","total"],"rows":[["1",10.5],...]}` instead of repeated object keys, targeted at token reduction for large homogeneous lists; clients expand losslessly. Normative details are deferred until GAP-Bench shows >= 20% token reduction on the large-list suite.

## 2. Empirical Evidence from `WP-M7-05` (`docs/benchmarks/REPORT.md`)

1. **Suite-level GAP-Bench (`N ∈ {10, 50, 200, 1000}` across `api_lookup`, `cross_branch_join`, `code_graph`):**
   - End-to-end token savings are overwhelmingly driven by progressive schema discovery (`tools/list` held at `651` tokens vs `203,421` at `N=1000`), server-side multi-branch joins (`8,547.9` vs `814,959.8` uncached tokens at `N=1000`), and server-side code-graph traversal (`1,187.4` vs `141,681.3` uncached tokens).
   - Across the three `GAP-Bench` suites published in `WP-M7-05`, compact scalar/small-record task responses carry almost no large (`>= 100`-row) homogeneous object arrays, so wire columnarization achieves **`< 20%` suite-level end-to-end token reduction** and adds unnecessary client/SDK decoding complexity across GraphQL/MCP/A2A boundaries for `1.0-RC`.
2. **Micro-payload property check (`gap_exec::columnar` unit benchmark):**
   - On isolated 100-row 5-column homogeneous JSON arrays (`x1_columnar_reduces_large_homogeneous_list_wire_bytes`), `ColumnarTable` reduces raw payload JSON bytes by `> 40%`. However, in real agent loops gated by progressive discovery and server-side aggregation, adding a non-standard GraphQL directive to every schema/client/MCP adapter before `1.0-RC` is not justified without an end-to-end benchmark suite exceeding the `20%` threshold.

## 3. Decision

1. **Close Extension X-1 wire activation for `1.0-RC`:** Do **not** enable `@format(as: COLUMNAR)` in the GraphQL executor, supergraph SDL directives, conformance suite, or TypeScript/Python SDKs for `1.0-RC`.
2. **Retain the lock-free pure Rust codec (`crates/gap-exec/src/columnar/mod.rs`):** Keep `ColumnarTable` (`from_homogeneous_objects`, `to_objects`, `measure_compact_json_savings`) tested and documented inside `gap-exec` (`// spec: GQL-003`) so post-`1.0` work can re-open X-1 cleanly if future analytics/telemetry-table workloads add a dedicated large-list GAP-Bench suite meeting the `>= 20%` end-to-end gate (`ADR-021`).
