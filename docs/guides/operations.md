# Operations Guide

Normative specification: [08 — Operations, Observability, and Release Engineering](../../spec/08-operations.md) (`OPS-010`..`OPS-044`), `RUN-007`, `RUN-090`, and [Runbooks Index](../runbooks/README.md).

## 1. CLI Administration Surface (`OPS-044`)

`gap` ships with structured JSON diagnostics (`--json`) and deterministic exit codes (`0` success, non-zero typed error):

| Command | Purpose |
|---|---|
| `gap serve --config gap.toml` | Run HTTP (`/graphql`, `/graphql/stream`, `/mcp`, `/healthz`, `/readyz`, `/metrics`) or `--stdio` gateway |
| `gap doctor --json --config gap.toml` | Preflight report across config, sandbox, egress, allow-list, vault, Cedar policy, and models |
| `gap config check <gap.toml>` | Validate `gap.toml` and emit JSON Schema |
| `gap snapshot show / diff / rollback` | Inspect active generation, diff supergraphs, or roll back cleanly (`INV-01`) |
| `gap policy ...` | Validate and reload Cedar authorization rules (`SEC-020`) |
| `gap audit verify / export` | Verify BLAKE3 hash chains (`SEC-092`) and export W3C PROV-O JSON-LD (`SEC-093`) |
| `gap ingest once / run` | Trigger deterministic schema/context extraction (`ING-010`, `ING-070`) |
| `gap proposals list / show / approve / deny` | Review ingestion and schema-drift proposals |
| `gap tasks list / show / cancel` | Inspect and manage durable A2A tasks (`RUN-080`..`RUN-086`) |
| `gap purge` | Hard-delete expired ephemeral and retention-past task context nodes (`ING-066`) |
| `gap calibrate` | Re-fit S1 temperature scaling and conformal thresholds (`RTE-068`) |
| `gap models` | Verify pinned model manifests (`SEC-102`) |
| `gap completions <shell>` | Emit completions for `bash`, `zsh`, `fish`, or `powershell` |

## 2. Observability Stack (`ops/`)

Pre-built operational assets live under `ops/`:

- **Prometheus Rules:** [ops/prometheus/alerts.yml](../../ops/prometheus/alerts.yml) configures recording rules and all 9 alerts (`SwapStuck`, `AutoRollback`, `BreakerFlapping`, `BranchDrift`, `S1Drift`, `IngestBacklog`, `AuditVerifyFail`, `TaskLeaseExpiry`, `OverloadShedding`).
- **Grafana Dashboards:** [gap-overview.json](../../ops/grafana/gap-overview.json), [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json), and [gap-security-ingest.json](../../ops/grafana/gap-security-ingest.json).
- **OTel Collector:** [ops/otel/collector.yaml](../../ops/otel/collector.yaml) scrubs authorization headers, raw `tenant`/`tenant_id`, and `graphql.document` attributes (`OPS-020`, `INV-15`) and applies tail-based sampling (`ERROR` spans, latency `> 500ms`, policy denies, discovery abstentions, snapshot swaps, plus 10% baseline).

## 3. Health Probes, Graceful Shutdown, and Rollbacks (`RUN-090`)

- `/healthz`: HTTP `200` once listener is bound until `SIGTERM`/`SIGINT` initiates drain.
- `/readyz`: HTTP `200` only when compiled snapshot generation `>= 1`, policy engine compiled, and stores ready.
- Zero-downtime reload & rollback (`INV-01`): if a canary snapshot publish raises error rates or fails health gates, `PublisherActor` triggers automatic rollback (`AutoRollback`) and operators can manually restore any retained generation with `gap snapshot rollback --generation <N>`.
- Follow per-alert escalation and mitigation steps in [docs/runbooks/README.md](../runbooks/README.md).

## 4. Cardinality & Conformal Coverage Health Check (`OPS-020`, `OPS-030`)

Operators monitor bounded metric labels (`0..15` `tenant_bucket`s) and conformal coverage (`1 - alpha >= 0.88` at `alpha = 0.10`):

```rust
fn within_coverage_floor(hits: usize, total: usize, alpha: f64, tolerance: f64) -> bool {
    if total == 0 {
        return false;
    }
    let empirical = hits as f64 / total as f64;
    empirical >= (1.0 - alpha - tolerance)
}

fn main() {
    assert!(within_coverage_floor(91, 100, 0.10, 0.02));
    assert!(!within_coverage_floor(80, 100, 0.10, 0.02));
}
```
