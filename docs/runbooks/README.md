# Operational Runbooks Index

Normative specification: [08-operations.md](../../spec/08-operations.md) (`OPS-001`–`OPS-004`, `OPS-010`, `OPS-011`, `OPS-020`, `OPS-030`, `OPS-044`), [RELEASE.md](../RELEASE.md), and [03-VERIFICATION.md](../03-VERIFICATION.md) (`G-19`, `G-20`).

> [!NOTE]
> **Status:** `WP-M0-10` index skeleton. Individual per-alert runbook documents (`docs/runbooks/*.md`), Prometheus alert rules (`ops/prometheus/alerts.yml`), Grafana dashboards, and OpenTelemetry Collector configs are authored in `WP-M7-04` and game-day drilled prior to `1.0-RC` (`G-19`, `G-20`).

## Alert-to-Runbook Catalogue ([08-operations.md](../../spec/08-operations.md) §6)

| Alert | Trigger Condition | Triage Outline | Planned Runbook (`WP-M7-04`) |
|---|---|---|---|
| `SwapStuck` | Snapshot publish `> 5 s` or plan-cache prewarm fails `> 5%` | Inspect proposal report; run `gap snapshot diff`; apply `breaking_change_ack` only with explicit approval; run `gap snapshot rollback` if needed (`ADR-009`) | `swap-stuck.md` |
| `AutoRollback` | Automatic snapshot rollback executed | Compare snapshot-attributable error rates (`ADR-009` §5); confirm bad generation remains quarantined; review offending proposal | `auto-rollback.md` |
| `BreakerFlapping` | `> 6` circuit-breaker transitions in `10 min` | Check upstream branch health (`gap_breaker_state`, `gap_branch_error_ewma`); widen window/threshold or drain unhealthy replica (`ADR-011`) | `breaker-flapping.md` |
| `BranchDrift` | Branch enters `DRIFT` state | Review `desc_hash` / schema diff via `gap proposals show`; approve or deny re-pin; contact branch owner (`ADR-014`) | `branch-drift.md` |
| `S1Drift` | `gap_s1_online_ece > 0.08` across `2` consecutive windows | Run `gap calibrate`; if drift persists, disable S1 reranking (`routing.model = "baseline"`, `ADR-007`, `OPS-042`) | `s1-drift.md` |
| `IngestBacklog` | `gap_ingest_backlog > 20` or oldest proposal `> 1 h` | Inspect ingestion worker pools and limits (`gap_blocking_queue_depth`); pause discovery proposals (`OPS-020`) | `ingest-backlog.md` |
| `AuditVerifyFail` | `gap_audit_verify_failures_total > 0` | Freeze privileged/admin actions immediately; run `gap audit verify`; restore from signed anchor; declare security incident | `audit-verify-fail.md` |
| `TaskLeaseExpiry` | Spike in expired task leases | Check node health (`/healthz`, `/readyz`), host pauses, and `ContextStore`/`TaskStore` write latency (`ADR-016`, `ADR-020`) | `task-lease-expiry.md` |
| `OverloadShedding` | Load shedding (`GAP_OVERLOADED`) `> 1%` for `5 min` | Scale out gateway replicas (`OPS-003`), inspect heavy `tenant_bucket` metrics (`OPS-020`), and adjust per-principal quotas | `overload-shedding.md` |

## Operator Entry Points (`OPS-002`, `OPS-044`)

- **Health & telemetry endpoints (`OPS-002`):** `/healthz` (liveness), `/readyz` (snapshot loaded ∧ store reachable ∧ ≥ 1 required branch not `Open` ∧ not draining), `/metrics` (Prometheus, bounded cardinality per `OPS-020`), `/admin/*` (separate authenticated listener).
- **Admin CLI (`OPS-044`):**
  ```bash
  gap doctor
  gap snapshot show
  gap snapshot diff
  gap snapshot rollback
  gap proposals list
  gap policy check
  gap audit verify
  gap calibrate
  gap tasks list
  ```
- **Trace & log hygiene (`OPS-010`, `OPS-011`, `INV-15`):** structured JSON logs and W3C Trace Context spans NEVER include raw queries, bearer tokens, secrets, or PII.
