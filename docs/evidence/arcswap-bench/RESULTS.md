# ArcSwap micro-benchmark (evidence for review finding F-01)

- Date: 2026-10-26 · Host: macOS Darwin 25.6 (x86_64-apple-darwin) · rustc 1.98.1 · arc-swap 1.9.3 · `--release`
- Re-run: `cd docs/evidence/arcswap-bench && cargo run --release`
- Raw output (single run; treat as order-of-magnitude, not as a benchmark of record):

```
load() uncontended: 15.0 ns/op
store() no readers: p50=164ns p99=177ns max=454531ns
store() with 8 busy readers: p50=1716ns p99=17910ns max=24570ns
store() with 64 busy readers: p50=6471ns p99=25203ns max=26003ns
store() with 256 busy readers: p50=22838ns p99=58929ns max=68755ns
store() that drops a 400MB snapshot: 28910 us
```

Conclusions used by the spec: (1) the old claims "load ~1 ns, store < 5 ns" are false by 1-4 orders of magnitude;
(2) the property that matters is *wait-free reads / no torn state*, not swap latency; (3) retiring a large old snapshot
MUST happen off the swapping thread (29 ms measured for 400 MB).
