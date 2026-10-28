# GAP Specification — 08 Operations, Observability and Release Engineering

Normative IDs `OPS-*`. Targets are SLOs only after the owning milestone completes (spec/00 §3).

## 1. Process model and deployment
- **OPS-001** `gap-server` is a single static-ish binary (plus optional model files). Profiles: `dev` (anonymous allowed, `operation_mode=open`, verbose logs), `prod` (auth required, `operation_mode=hybrid`, T2 hidden, sandbox mandatory).
- **OPS-002** Endpoints: `/graphql` (POST; GET persisted; also accepts `Accept: text/event-stream` for `graphql-sse` distinct-connections clients that use one URL), `/graphql/stream` (SSE alias), `/mcp` (`POST`-only Streamable HTTP, stateless, plus `2025-11-25` `initialize` compat), `/.well-known/agent-card.json`, `/.well-known/oauth-protected-resource` (and `/.well-known/oauth-protected-resource/mcp`), `/healthz` (liveness: process responsive), `/readyz` (readiness: snapshot loaded ∧ store reachable ∧ ≥ 1 required branch not `Open` ∧ not draining), `/metrics` (Prometheus), `/admin/*` (bound to a separate listener, authenticated, `admin:*` scopes). gRPC on its own port.
- **OPS-003** Topologies: (a) *single node* (SQLite store, local blobs); (b) *HA pair/cluster* — stateless gateway replicas behind an L7 load balancer sharing an external store (Postgres adapter is a post-1.0 `ContextStore`/`TaskStore` backend; until then HA means active/standby with snapshot shipping) — document honestly as such in 1.0; (c) *embedded* library mode for tests. Zero-downtime binary upgrade requires topology (b); handing the listening socket to a new process (`LISTEN_FDS`/`SCM_RIGHTS`) on a single node is a 1.0 non-goal (ADR-025).
- **OPS-004** Resource guidance (documented, verified by soak): 2 vCPU/2 GB for ≤ 20 branches without S1; 8 vCPU/8 GB with S1 f16 on CPU.

## 2. Tracing (OpenTelemetry)
Span hierarchy (names are normative):
```
gap.request                      attrs: gap.generation, gap.operation_name, gap.tenant, gap.principal_kind, gap.cost.planned
├─ gap.gateway.authn
├─ gap.operation.validate        attrs: gap.persisted, gap.depth, gap.cost.estimate
├─ gap.authz                     attrs: gap.policy_version, gap.decision_count, gap.denied_count
├─ gap.plan                      attrs: gap.plan_cache (hit|miss|prewarm), gap.plan_hash
├─ gap.exec
│  ├─ gap.exec.fetch             attrs: gap.branch, gap.op, gap.rows
│  ├─ gap.exec.join              attrs: gap.join.keys, gap.join.unique, gap.join.chunks, gap.join.upstream_calls
│  └─ gap.connector.call         attrs: gap.branch, gap.replica, gap.attempt, gap.breaker_state, rpc.system
└─ gap.assemble                  attrs: gap.bytes, gap.partial
gap.discovery.resolve            attrs: gap.router, gap.outcome, gap.set_size, gap.candidates, gap.embed_ms, gap.rank_ms
gap.ingest.pipeline / gap.ingest.<stage>      attrs: gap.proposal, gap.source_kind, gap.gate, gap.result
gap.snapshot.publish             attrs: gap.generation, gap.swap_us, gap.prewarm_ok, gap.prewarm_failed
gap.task.<state-transition>      attrs: gap.task, gap.seq
```
- **OPS-010** W3C Trace Context in/out on every protocol (HTTP, gRPC metadata, NGC `traceparent`, MCP `_meta`, A2A headers). Sampling: parent-based, default 10% head sampling plus 100% of errors and of requests slower than `trace.slow_ms` (tail sampling at the collector).
- **OPS-011** Attributes MUST NOT contain secrets, raw queries (use hashes), or PII (INV-15).

## 3. Metrics (Prometheus; prefix `gap_`)
| Metric | Type | Labels (bounded) |
|---|---|---|
| `gap_requests_total` | counter | `protocol, outcome, op_kind` |
| `gap_request_duration_seconds` | histogram | `protocol, op_kind` |
| `gap_branch_requests_total` / `gap_branch_duration_seconds` | counter/hist | `branch, outcome` |
| `gap_inflight` / `gap_breaker_state` / `gap_branch_ewma_latency_us` / `gap_branch_error_ewma` | gauge | `branch` |
| `gap_retry_total`, `gap_retry_budget_exhausted_total` | counter | `branch` |
| `gap_plan_cache_total` | counter | `result` |
| `gap_join_unique_keys` / `gap_join_upstream_calls` | histogram | `branch` |
| `gap_snapshot_generation`, `gap_snapshot_swap_duration_seconds`, `gap_snapshot_rollbacks_total` | gauge/hist/counter | – |
| `gap_discovery_decisions_total` | counter | `outcome, router` |
| `gap_s1_inference_duration_seconds` | histogram | `backend` |
| `gap_s1_online_ece`, `gap_discovery_set_size` | gauge/hist | `router` |
| `gap_router_fallback_total` | counter | `reason` |
| `gap_policy_decisions_total` | counter | `action, effect` |
| `gap_ingest_transitions_total`, `gap_ingest_backlog`, `gap_ingest_gate_failures_total` | counter/gauge | `from,to` / `gate` |
| `gap_task_state_total`, `gap_tasks_active` | counter/gauge | `state` |
| `gap_tokens_returned_total` | counter | `tool` (facade tools / gql ops class) |
| `gap_blocking_queue_depth` | gauge | `pool` |
| `gap_audit_chain_head_seq`, `gap_audit_verify_failures_total` | gauge/counter | `chain` |
- **OPS-020** Label cardinality is bounded: `branch` ≤ `limits.max_branches` (default 1,000); never label by query text, principal id, or tenant (use `tenant_bucket` hash into 16 buckets when needed).

## 4. Logging
Structured JSON (`tracing-subscriber`), fields: `ts, level, target, msg, request_id, trace_id, span_id, tenant_hash, generation`. Levels: `error` (action needed), `warn` (degraded), `info` (state transitions: publish, breaker, approval, ingest), `debug/trace` (off in prod). Log sampling for repetitive warnings (≤ 1/s/key). Audit events are **not** logs (SEC-090).

## 5. SLO set (reference environment, after owning milestone)
| SLI | Objective | Window |
|---|---|---|
| Availability of `/graphql` (non-5xx, excluding branch failures attributable to branches) | 99.9% | 30 d |
| Typed-path overhead p99 (warm) | ≤ 5 ms | per release bench |
| Snapshot swap p99 @256 readers | ≤ 250 µs | nightly |
| Discovery p95 (baseline / S1 CPU) | ≤ 40 ms / ≤ 250 ms | nightly |
| Task recovery after crash (WORKING tasks resumed or failed explicitly) | 100% within 60 s of start | per release chaos |
| Audit verify | 0 failures | nightly |
Error-budget policy: if the 30-day availability budget is exhausted, feature merges pause except reliability fixes (documented in `docs/RELEASE.md`).

## 6. Alerts and runbooks (`docs/runbooks/*.md`, WP-M7-04)
| Alert | Condition | Runbook outline |
|---|---|---|
| SwapStuck | publish > 5 s or prewarm fails > 5% | inspect proposal report; `gap snapshot diff`; use `breaking_change_ack` only with approval; rollback command |
| AutoRollback | rollback executed | compare attributable errors; mark generation bad; review proposal |
| BreakerFlapping | > 6 transitions/10 min | check branch health, widen window/threshold, drain replica |
| BranchDrift | branch enters DRIFT | review diff of `desc_hash`; approve/deny; contact owner |
| S1Drift | `online_ece > 0.08` ×2 windows | run recalibration; if persists disable S1 (`routing.model=baseline`) |
| IngestBacklog | backlog > 20 or oldest > 1 h | check workers, limits; pause discovery proposals |
| AuditVerifyFail | verify failure | freeze admin actions; restore from anchor; incident |
| TaskLeaseExpiry | spikes of expired leases | check node health, GC pauses, store latency |
| OverloadShedding | shed > 1% for 5 min | scale out; inspect heavy principals; adjust quotas |

## 7. Chaos and resilience scenarios (CI nightly subset; full set per release)
Latency injection +300 ms on one replica (traffic shifts, no oscillation) · branch 100% errors (breaker opens ≤ 30 s; other group members serve) · connector crash mid-stream · network partition to store · disk full on SQLite · slow SQLite (fsync stall) · clock skew ±5 min (JWT, leases) · OOM-kill during task execution (recovery) · poisoned spec during ingestion · mass cancellation storm · DNS failure to JWKS (cached keys serve) · S1 weights deleted at runtime (fallback) · snapshot swap during 10k rps.
- **OPS-030** Soak: 72 h at 30% of max throughput with periodic swaps, drift events and chaos injections; gates: RSS growth ≤ 5% after warm-up, no fd/thread leaks, p99 drift ≤ 10%, zero audit/verify failures.

## 8. Release engineering
- **OPS-040** Versioning per ADR-024; `CHANGELOG.md` (Keep a Changelog) generated from Conventional Commits; protocol/spec changes require ADR + conformance diff.
- **OPS-041** Release train: `main` always releasable; `rc` branches get full verification (docs/03-VERIFICATION.md §9); signed artifacts (SEC-101); upgrade test matrix N-1 → N for snapshots, stores (migrations are forward-only with a verified backup/restore), config.
- **OPS-042** Feature flags for risky capabilities: `routing.model`, `incremental`, `facade.spotlight`, `ingest.auto_activate_t0`, `saga`, `store-surreal`; flag defaults documented; removal tracked.
- **OPS-043** Store/snapshot migrations: `MigrationPlan` per `format_version`, dry-run mode, checksum, backup before apply, tested with fixtures from every released version.
- **OPS-044** Admin CLI (`gap`): `serve, init, doctor, config {schema,check}, models {pull,verify}, proposals {list,show,approve,deny}, snapshot {show,diff,rollback}, policy {check,reload}, audit {verify,export}, calibrate, bench, ingest {run,once}, tasks {list,show,cancel}, purge`.

## 9. Documentation deliverables (part of "done")
mdBook site: concepts, quickstart (MCP host in 5 min), spec (this directory rendered), connector authoring guide, security hardening guide, operations guide + runbooks, API reference (rustdoc + GraphQL schema docs), benchmark methodology and published results, migration guides. Docs are tested: code blocks compile/run in CI (`mdbook test`, doc-tests, GraphQL snippets validated against the reference schema).
