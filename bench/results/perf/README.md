# Performance Baselines (`bench/results/perf`)

Recorded performance baselines for GAP milestones (`ADR-021`, `VER-030`..`VER-033`). Every metric stored here must correspond to an executed benchmark or release test run captured in this repository; unimplemented end-to-end harnesses are recorded explicitly as deferred until their blocking work packages land.

## Artifacts

- `m3-baseline.json` — Milestone 3 (`WP-M3-09`) baseline: `gap-balance` P2C selection/EWMA/retry-budget microbenchmarks (`percentiles.rs`, `selection.rs`, `update.rs`), `gap-index` (`index_bench.rs`), strict M3 spec-traceability check, and M3 runtime test/gate status.

## Reproduction Commands

Run all commands from the repository root with `--offline`:

```bash
# Quick workspace microbenchmark suite (passes --quick only to [[bench]] targets)
cargo bench --workspace --bench '*' --offline -- --quick

# Snapshot hot-swap p99 stress gate (must run in release profile; debug is unoptimized)
cargo test --release -p gap-snapshot --test swap_stress --offline

# M3 crate suites (balancer, planner, executor, transport, NGC adapters, testkit)
cargo test -p gap-balance -p gap-plan -p gap-exec -p gap-transport -p gap-ngc-sql -p gap-ngc-grpc -p gap-ngc-graphql -p gap-testkit --offline

# Strict M3 MUST -> test traceability
python3 tools/spec_trace.py --strict --milestone M3
```

## Notes

- End-to-end 10k RPS 5-minute HTTP mixed load (`VER-032`) and full-server cold-start (`VER-033`) depend on `bench/gapbench` HTTP server wiring (`WP-M2-07`) and final milestone soak verification (`WP-M7-05`).
