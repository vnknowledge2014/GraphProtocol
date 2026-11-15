# GAP Alert Runbooks Index

Operational runbooks for the 9 Prometheus alerts defined in [alerts.yml](../../ops/prometheus/alerts.yml) (`spec/08-operations.md` §6, `OPS-010`, `OPS-020`, `OPS-044`).

## Alert Mapping

| Alert | Severity | Component | Primary Metrics | Runbook |
|---|---|---|---|---|
| `SwapStuck` | `page` | `snapshot` | `gap_snapshot_swap_duration_seconds`, `gap_snapshot_generation`, `gap_ingest_backlog` | [swap-stuck.md](swap-stuck.md) |
| `AutoRollback` | `page` | `snapshot` | `gap_snapshot_rollbacks_total`, `gap_snapshot_generation` | [auto-rollback.md](auto-rollback.md) |
| `BreakerFlapping` | `ticket` | `balance` | `gap_breaker_state`, `gap_branch_error_ewma` | [breaker-flapping.md](breaker-flapping.md) |
| `BranchDrift` | `ticket` | `exec` | `gap_branch_error_ewma`, `gap_branch_requests_total`, `gap_retry_budget_exhausted_total` | [branch-drift.md](branch-drift.md) |
| `S1Drift` | `ticket` | `route` | `gap_s1_online_ece`, `gap_router_fallback_total`, `gap_discovery_decisions_total` | [s1-drift.md](s1-drift.md) |
| `IngestBacklog` | `ticket` | `ingest` | `gap_ingest_backlog`, `gap_ingest_gate_failures_total`, `gap_unmapped_events_dropped_total` | [ingest-backlog.md](ingest-backlog.md) |
| `AuditVerifyFail` | `page` | `security` | `gap_audit_verify_failures_total`, `gap_audit_chain_head_seq` | [audit-verify-fail.md](audit-verify-fail.md) |
| `TaskLeaseExpiry` | `ticket` | `tasks` | `gap_task_state_total`, `gap_tasks_active` | [task-lease-expiry.md](task-lease-expiry.md) |
| `OverloadShedding` | `page` | `transport` | `gap_http_requests_total`, `gap_requests_total`, `gap_blocking_queue_depth` | [overload-shedding.md](overload-shedding.md) |

## Standard Endpoints and Diagnostics

- `GET /healthz` — process liveness check (`200 OK` unless shutting down).
- `GET /readyz` — readiness gate (loaded supergraph snapshot generation > 0, policy compiled, store reachable).
- `GET /metrics` — Prometheus text exposition (`0.0.4`) with bounded cardinality labels (`tenant_bucket` in `0..15`, `OPS-020`).
- `gap doctor --json --config <gap.toml>` — full preflight health check across config, sandbox, egress, allow-list, vault, policy, and pinned models.
- `gap snapshot show --config <gap.toml>` — inspect active supergraph generation and branch composition.
- `gap audit verify --chain <system|tenant:ID>` — offline BLAKE3 hash-chain integrity check (`SEC-092`).
