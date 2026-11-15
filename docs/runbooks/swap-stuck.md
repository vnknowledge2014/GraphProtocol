# Runbook: `SwapStuck`

- **Severity:** `page`
- **Spec:** `INV-01`, `OPS-030`, `RUN-007`
- **Dashboards:** [gap-overview.json](../../ops/grafana/gap-overview.json)

## Trigger

`gap_snapshot_swap_duration_seconds` p99 exceeds `1.0s`, or queued ingestion work sits in `gap_ingest_backlog` for > 15m with `changes(gap_snapshot_generation[15m]) == 0`.

## Impact

New schemas, curated descriptions, and routing calibration tables cannot publish. Existing in-flight requests continue pinning the current snapshot (`INV-01`), but schema updates and new connector registrations stall.

## Triage

1. Verify gateway readiness and current generation:
   ```bash
   curl -sf http://127.0.0.1:8080/readyz
   curl -sf http://127.0.0.1:8080/metrics | grep -E 'gap_snapshot_generation|gap_http_in_flight|gap_snapshot_swap'
   ```
2. Check if long-lived requests or stalled streaming (`SSE` / `/graphql/stream`) connections hold references to an older snapshot generation in the RCU reader drainer (`INV-02`).
3. Run composition and config validation locally:
   ```bash
   gap doctor --json --config gap.toml
   gap snapshot show --config gap.toml
   ```

## Mitigation

1. If a slow connector or streaming subscription is holding the old generation beyond deadline, inspect slow spans in OTel (`latency > 500ms`) and verify `client_deadline_ms` and HTTP read timeouts are enforced.
2. Check whether `gap ingest once` or schema composition is failing validation gates (see [ingest-backlog.md](ingest-backlog.md)).
3. If an immediate rollback to the last known-good generation is needed:
   ```bash
   gap snapshot rollback --generation <PREVIOUS_GEN>
   ```
4. Once `gap_snapshot_generation` advances or `gap_snapshot_swap_duration_seconds` drops below `0.1s`, monitor for 10 minutes before closing.
