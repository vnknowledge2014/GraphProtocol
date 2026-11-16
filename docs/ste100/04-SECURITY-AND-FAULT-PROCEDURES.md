# Security Controls and Fault Recovery Procedures `[DESCRIPTIVE]`

## 1. Zero-Trust Defenses in GAP

Every request and upstream response passes deterministic security gates:

1. **Trust Tiers (`SEC-060`–`SEC-061`):** Branches carry tier `T0` (signed manifest), `T1` (approved hashes), or `T2` (untrusted).
2. **Untrusted Envelope (`SEC-080`):** Upstream text from `T1`/`T2` branches enters `<gap:untrusted>` tags with escaped inner closing tags.
3. **Rug-Pull Drift Freeze (`SEC-082`, `INV-08`):** `DriftMonitor` computes fresh description hashes during polls and freezes modified branches into `BranchStatus::Drift`.
4. **SSRF Egress Guard (`SEC-075`):** `EgressGuard` resolves DNS once, blocks private and cloud metadata addresses, and connects to pinned IPs.
5. **Audit Integrity (`SEC-092`, `INV-10`):** Every SQLite audit record chains sequential BLAKE3 hashes per `tenant` chain and per `system` chain.

## 2. Validate and Reload Cedar Policies `[PROCEDURAL]`

> **WARNING:** Never approve mutations classified as `IRREVERSIBLE` or `UNKNOWN` without distinct dual-approver policy checks (`INV-04`).

1. Run `gap policy check` on your schema and policy directory (`SEC-020`).
2. Verify exit code `0` before any live change.
3. Run `gap policy reload` to publish the validated Cedar bundle cleanly without locks (`INV-02`).

```bash
./target/dist/gap policy check --dir policies
./target/dist/gap policy reload --dir policies
```

## 3. Verify the BLAKE3 Audit Chain `[PROCEDURAL]`

1. Run `gap audit verify` on `.gap/audit.db` to check chain hashes (`SEC-092`).
2. If an alert fires, pass `--nightly` to inspect `system` and every `tenant` chain.
3. Export verified records as W3C `PROV-O` JSON-LD with `gap audit export` (`SEC-093`).

```bash
./target/dist/gap audit verify --db .gap/audit.db --nightly
./target/dist/gap audit export --db .gap/audit.db --chain system --format prov-jsonld
```

## 4. Roll Back a Faulty Snapshot Generation `[PROCEDURAL]`

> **CAUTION:** Do not pass `--force` unless an engineer confirms the target `generation` holds safe state.

1. Stop any pending `proposal` approvals in your deployment pipeline.
2. Run `gap snapshot rollback` targeting the last known healthy `generation` (`RUN-007`).
3. Verify error rate metrics drop in Prometheus and Grafana.

```bash
./target/dist/gap snapshot rollback --to 1 --before 2 --reason "error spike after rollout"
```

## 5. Recover from a Branch Drift Freeze `[PROCEDURAL]`

1. Run `gap proposals list` and `gap ingest once` on `gap.toml` (`ING-070`).
2. Compare schemas with `gap snapshot diff` to locate changed descriptions or dropped fields.
3. If the change matches vendor release notes, approve the `proposal` to restore traffic.
4. If the change looks hostile, leave the `branch` in `BranchStatus::Drift` and revoke its credentials.

## 6. Back Up and Restore Durable SQLite Stores `[PROCEDURAL]`

> **NOTE:** GAP uses SQLite online backup APIs (`backup_to` and `restore_from`) without blocking live readers (`OPS-043`).

1. Run `backup_to` to capture a verified point-in-time SQLite context backup (`OPS-043`).
2. Verify offline recovery before deployment by running the upgrade and backup suite.
3. If a host crash or `SIGKILL` occurs, start `gap serve` on the same storage path (`RUN-091`).
4. Verify `/readyz` returns HTTP `200` and stalled tasks reclaim expired leases automatically.

```bash
CI=1 cargo nextest run --no-pager -j 1 --test upgrade --offline
```
