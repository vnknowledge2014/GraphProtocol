# Runbook: `TaskLeaseExpiry`

- **Severity:** `ticket`
- **Spec:** `RUN-080`, `RUN-082`, `RUN-086`, `INV-09`
- **Dashboards:** [gap-security-ingest.json](../../ops/grafana/gap-security-ingest.json)

## Trigger

`gap_task_state_total{state=~"failed|timed_out|expired"}` increases by > 3 in 10m OR `gap_tasks_active > 500`.

## Impact

Durable A2A tasks (`gap_task`) are exhausting worker leases before renewal/checkpointing or accumulating in active/input-required states, stalling long-running agent workflows.

## Triage

1. Inspect active and failed durable tasks via CLI (`OPS-044`):
   ```bash
   gap tasks list
   gap tasks show <TASK_ID>
   ```
2. Correlate with [gap-overview.json](../../ops/grafana/gap-overview.json) and [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json) to check whether a specific downstream branch (`BreakerFlapping` / `BranchDrift`) or slow SQLite write is preventing lease renewal (`RUN-082`).
3. Check if human/approval gates (`input-required`) are accumulating without operator/client resolution.

## Mitigation

1. If a worker crashed mid-lease, confirm orphan recovery reclaims recoverable tasks cleanly without duplicating non-idempotent side-effect mutations (`SEC-020`, idempotency ledger).
2. Cancel genuinely orphaned or stuck tasks safely:
   ```bash
   gap tasks cancel <TASK_ID>
   ```
3. Tune lease TTL vs checkpoint heartbeat interval or downstream connector timeout if tasks legitimately exceed the default lease window, then confirm `gap_tasks_active` returns within budget.
