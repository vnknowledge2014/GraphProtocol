# Runbook: `AuditVerifyFail`

- **Severity:** `page`
- **Spec:** `SEC-092`, `SEC-093`, `INV-09`, `TM-04`
- **Dashboards:** [gap-security-ingest.json](../../ops/grafana/gap-security-ingest.json)

## Trigger

`increase(gap_audit_verify_failures_total[5m]) > 0` (`for: 0m` — immediate page).

## Impact

Cryptographic integrity verification of a per-tenant or system BLAKE3 audit hash chain detected a broken `prev_hash` link, sequence gap, or tampered payload. Treat as a high-priority security or storage corruption incident.

## Triage

1. **Preserve evidence immediately** — take a read-only filesystem/volume snapshot of the SQLite/store database before running any maintenance or purge command.
2. Run offline verification specifying the chain reported in `$labels.chain`:
   ```bash
   gap audit verify --chain system --json
   gap audit verify --chain tenant:<TENANT_ID> --json
   ```
3. Export intact segments in W3C PROV-O JSON-LD format (`SEC-093`) for forensics:
   ```bash
   gap audit export --chain system --format json-ld
   ```

## Mitigation

1. Check host storage health (disk full, unclean crash, SQLite WAL truncation) and review recent deployment/restore events.
2. Isolate any compromised credentials or admin sessions if sequence tampering without crash evidence is observed.
3. Restore verified store state from the latest integrity-checked backup and re-run `gap audit verify` across all chains (`SEC-092`) until `verify_failures == 0`.
