# Runbook: `AutoRollback`

- **Severity:** `page`
- **Spec:** `INV-01`, `ING-052`, `RUN-007`
- **Dashboards:** [gap-overview.json](../../ops/grafana/gap-overview.json)

## Trigger

`increase(gap_snapshot_rollbacks_total[10m]) > 0`.

## Impact

An attempted supergraph snapshot publish failed post-swap validation or error-rate canary checks and automatically reverted to the prior generation. Traffic remains served on the previous immutable snapshot (`INV-01`), so availability is preserved, while pending schema/connector changes remain unapplied.

## Triage

1. Inspect recent structured logs (`OPS-011`) for `gap.snapshot.publish` spans with `ERROR` status in OTel or stdout JSON logs.
2. Diff the rejected candidate snapshot against the currently active snapshot:
   ```bash
   gap snapshot show --config gap.toml
   gap snapshot diff --before <ACTIVE_SNAPSHOT> --after <CANDIDATE_SNAPSHOT>
   ```
3. Check whether an unapproved or breaking proposal triggered a gate failure:
   ```bash
   gap proposals list
   ```

## Mitigation

1. Reject or revise the breaking proposal (`gap proposals deny <ID>`) rather than forcing publication.
2. Verify downstream branch SDL/OpenAPI/Protobuf inputs pass composition cleanly using `gap serve --check-only --config gap.toml`.
3. Only after fixing the root cause in branch descriptors or Cedar policy, re-run ingestion (`gap ingest once`) and verify `gap_snapshot_generation` increments cleanly without incrementing `gap_snapshot_rollbacks_total`.
