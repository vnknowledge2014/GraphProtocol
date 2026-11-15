# Runbook: `IngestBacklog`

- **Severity:** `ticket`
- **Spec:** `ING-060`, `ING-070`, `RTE-101`
- **Dashboards:** [gap-security-ingest.json](../../ops/grafana/gap-security-ingest.json)

## Trigger

`gap_ingest_backlog > 100`, `increase(gap_ingest_gate_failures_total[15m]) > 0`, OR `rate(gap_unmapped_events_dropped_total[10m]) > 0.5`.

## Impact

Schema drift extraction, context graph updates, or pending curation proposals are delayed; malformed/untrusted proposal inputs or noisy telemetry floods are hitting deterministic gates or bounded per-tenant rate limiters (`RTE-101`).

## Triage

1. Identify which gate is failing (`gap_ingest_gate_failures_total{gate=...}`) in [gap-security-ingest.json](../../ops/grafana/gap-security-ingest.json).
2. Inspect pending proposals and verify config:
   ```bash
   gap proposals list
   gap doctor --json --config gap.toml
   ```
3. If `gap_unmapped_events_dropped_total` is rising, inspect telemetry for a noisy tenant bucket (`tenant_bucket` in `0..15`, `OPS-020`) emitting malformed or unmapped feedback events.

## Mitigation

1. Run a bounded manual ingestion pass to surface explicit diagnostic errors:
   ```bash
   gap ingest once --config gap.toml
   ```
2. Deny or fix proposals failing structural/security gates (`SEC-020`, `ING-070`).
3. Ensure expired ephemeral context nodes are cleaned periodically (`gap purge`) so store scan latency does not inflate ingestion queue times.
4. Confirm `gap_ingest_backlog` drains toward `0` and gate failures stop incrementing.
