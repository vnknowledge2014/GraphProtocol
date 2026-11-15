# GAP 1.0-RC Soak, Chaos, Upgrade & Backup/Restore Gate (`OPS-030`, `OPS-041`, `OPS-043`, `RUN-091`)

- **Work Package:** `WP-M7-07`
- **Verification Gate (`docs/03-VERIFICATION.md` §9):** `G-11` (`72 h` soak meeting `OPS-030` gates)

---

## 1. Automated Coverage Map

### 1.1 Chaos Matrix (`spec/08-operations.md` §7, `OPS-030`)
Combined coverage spans [`tests/chaos/chaos.rs`](../../tests/chaos/chaos.rs) (via `gap-testkit` deterministic P2C/breaker/replica fault harness) and [`tests/soak/soak.rs`](../../tests/soak/soak.rs) (in-process full gateway runtime):

| # | `spec/08` §7 Chaos Scenario | Automated Verification Location |
|---|---|---|
| 1 | Upstream branch timeout / slow replica isolation | `tests/chaos/chaos.rs` (`p2c` + breaker fast-fail), `tests/soak/soak.rs` |
| 2 | Upstream `5xx` / `429` storm & circuit breaker open | `tests/chaos/chaos.rs`, `tests/soak/soak.rs::ops_030_chaos_upstream_faults_malformed_json_and_drift_isolation` |
| 3 | Malformed upstream payload isolation & canary leak guard (`INV-15`) | `tests/soak/soak.rs::ops_030_chaos_upstream_faults_malformed_json_and_drift_isolation` |
| 4 | Rug-pull schema/description drift freeze (`ING-053`) | `tests/soak/soak.rs::ops_030_chaos_upstream_faults_malformed_json_and_drift_isolation`, `tests/security/rugpull/rug_pull.rs` |
| 5 | System-1 (`S1`) unavailable fallback (`INV-17`, `RTE-044`) | `docs/benchmarks/REPORT.md` & `crates/gap-route` fallback tests |
| 6 | Concurrent snapshot swap with zero torn requests (`INV-01`) | `tests/soak/soak.rs::ops_030_chaos_concurrent_snapshot_swaps_rollback_and_pinning_invariants` |
| 7 | Attribution-aware automatic rollback on planning error spike (`RUN-007`) | `tests/soak/soak.rs::ops_030_chaos_concurrent_snapshot_swaps_rollback_and_pinning_invariants` |
| 8 | Worker crash & lease expiry task recovery (`RUN-091`) | `tests/soak/soak.rs::ops_030_chaos_task_lease_expiry_quota_audit_and_graceful_shutdown` |
| 9 | Concurrent audit hash-chain continuity (`SEC-090`) | `tests/soak/soak.rs::ops_030_chaos_task_lease_expiry_quota_audit_and_graceful_shutdown` |
| 10 | Rate limit & overload shedding (`RUN-050`, `SEC-050`) | `tests/soak/soak.rs::ops_030_chaos_task_lease_expiry_quota_audit_and_graceful_shutdown` |
| 11 | Two-stage graceful drain & forced exit (`RUN-090`) | `tests/soak/soak.rs::ops_030_chaos_task_lease_expiry_quota_audit_and_graceful_shutdown` |
| 12 | Bounded snapshot `Arc` reclamation & zero task leaks under churn | `tests/soak/soak.rs::ops_030_configurable_soak_loop_maintains_bounds_and_zero_task_loss` |
| 13 | N-1 / N+1 SQLite version mismatch & corrupt restore isolation | `tests/upgrade/upgrade.rs::ops_041_sqlite_format_version_migrations_and_future_version_guard` |

### 1.2 Upgrade & Backup/Restore Suite ([`tests/upgrade/upgrade.rs`](../../tests/upgrade/upgrade.rs))
- **SQLite Migration & Fail-Closed Guard (`OPS-041`):** Executes `v0 -> v1 -> v2 -> v3 -> v4` (`FORMAT_VERSION`) migrations on fresh store, writes nodes/edges, exports consistent backup via `SqliteContextStore::backup_to`, and verifies N+1 (`FORMAT_VERSION + 1`) files reject fail-closed with `StoreError::FormatTooNew`.
- **Snapshot Format & Config Compatibility (`OPS-041`):** Loads `examples/walking-skeleton/gap.toml`, verifies `SNAPSHOT_FORMAT_VERSION` enforcement (`PublishError::UnsupportedFormat`) and strict monotonic generations (`PublishError::NonMonotonic`).
- **Full Online Backup & Restore Drill (`OPS-043`, `RUN-091`):** Captures consistent context, audit, task, and idempotency stores; verifies corrupt backups reject fail-closed (`StoreError::Corrupt`) without mutating target paths; verifies overwrite guard (`StoreError::Conflict`); verifies restored tenant/system `AuditVerifyReport::Intact`, `Working` expired-lease task reclaim/completion (`RUN-091`), and idempotency replay.

---

## 2. Reproducing Locally & Running Full `72 h` Release Soak (`G-11`)

```bash
# Fast deterministic PR/local validation (default 3 s soak loop)
cargo nextest run --no-pager -j 1 --test upgrade --offline
cargo nextest run --no-pager -j 1 --test soak --offline
cargo nextest run --no-pager -j 1 --test chaos --offline

# Full 72-hour continuous release gate G-11 (259,200 seconds) on dedicated CI runner
GAP_SOAK_DURATION_SECS=259200 cargo nextest run --release --no-pager -j 1 --test soak --offline
```

### Honest Status (`ADR-021`)
- **Local Verification:** The complete `upgrade`, `soak` (3 s smoke duration), and `chaos` suites compile cleanly and pass deterministically on every local gate check.
- **Full `72 h` Continuous Soak (`G-11`):** Running `259,200 s` of wall-clock execution cannot complete inside interactive air-gapped verification and remains scheduled on the dedicated staging/release CI runner prior to promoting `1.0-RC` to `1.0.0` GA.
