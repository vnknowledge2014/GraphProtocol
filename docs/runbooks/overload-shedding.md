# Runbook: `OverloadShedding`

- **Severity:** `page`
- **Spec:** `ARC-012`, `OPS-030`, `INV-02`
- **Dashboards:** [gap-overview.json](../../ops/grafana/gap-overview.json)

## Trigger

Overload shedding ratio (`429` / `503` HTTP or `rate_limited` / `rejected` GAP outcomes) exceeds `2%` over 5m OR `gap_blocking_queue_depth > 256` for 3m.

## Impact

The gateway concurrency limiter / per-tenant rate limiter is rejecting excess requests or the dedicated CPU/blocking thread pool (handling S1/HNSW/SQLite/parse work outside Tokio worker threads, `ARC-012`, `INV-02`) is saturated.

## Triage

1. Check [gap-overview.json](../../ops/grafana/gap-overview.json) panels for:
   - `gap_http_in_flight` and `gap_blocking_queue_depth`
   - `gap:requests_by_tenant_bucket5m` (to detect whether burst load concentrates in specific bounded `tenant_bucket`s `0..15`, `OPS-020`)
   - `gap_plan_cache_total` hit ratio (cache misses dramatically increase parse/plan CPU work)
2. Inspect latency histograms and `429`/`503` breakdown by `route`:
   ```bash
   curl -sf http://127.0.0.1:8080/metrics | grep -E 'gap_http_|gap_plan_cache|gap_blocking_queue'
   ```

## Mitigation

1. If a single traffic burst is overwhelming CPU (`gap_blocking_queue_depth > 256`), verify per-tenant token buckets and global concurrency limits in `gap.toml` protect healthy tenants; wait or scale horizontally behind your L7 load balancer.
2. If `PlanCache` hit ratio collapsed, check whether clients are sending un-persisted dynamic ad-hoc documents with rapidly shifting query shapes instead of persisted queries.
3. If blocking saturation stems from S1 model inference or store I/O latency, verify CPU/disk limits and check [s1-drift.md](s1-drift.md) or store health (`gap doctor`).
4. Confirm `429`/`503` rate drops below `0.5%` and `gap_blocking_queue_depth` returns below `32`.
