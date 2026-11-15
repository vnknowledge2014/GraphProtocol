# Runbook: `BranchDrift`

- **Severity:** `ticket`
- **Spec:** `ING-070`, `ARC-011`
- **Dashboards:** [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json)

## Trigger

`gap_branch_error_ewma > 0.15`, branch 5xx/error ratio > `10%` over 10m, or `gap_retry_budget_exhausted_total` rising for 5m.

## Impact

Downstream branch responses deviate from expected schema or reliability SLOs (upstream HTTP 4xx/5xx, timeouts, or contract mismatches caught by response assembly), causing GraphQL partial errors or failed MCP tool calls.

## Triage

1. Inspect `gap.exec.fetch` and `gap.connector.call` spans in OTel filtered on `status=ERROR` (collected at 100% by `tail_sampling` in [collector.yaml](../../ops/otel/collector.yaml)).
2. Check if an uncoordinated downstream OpenAPI/GraphQL/Protobuf/MCP schema change introduced contract drift (`ING-070`).
3. Inspect recent proposals and ingestion logs:
   ```bash
   gap proposals list
   gap ingest once --config gap.toml
   ```

## Mitigation

1. If contract drift is detected (missing required fields, renamed enums, unexpected error payloads), trigger ingestion (`gap ingest once`), inspect the generated diff (`gap snapshot diff`), and approve safe non-breaking proposals (`gap proposals approve <ID>`).
2. If the downstream endpoint itself is degraded, verify the breaker isolates it (`gap_breaker_state`) or drain traffic from that branch in configuration after running `gap doctor --config gap.toml`.
3. Confirm `gap_branch_error_ewma` falls below `0.05` and retry exhaustion ceases.
