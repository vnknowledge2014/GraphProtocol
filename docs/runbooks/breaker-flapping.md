# Runbook: `BreakerFlapping`

- **Severity:** `ticket`
- **Spec:** `ARC-011`
- **Dashboards:** [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json)

## Trigger

`changes(gap_breaker_state[15m]) >= 4` on any downstream `branch`.

## Impact

The adaptive circuit breaker (`0=Closed`, `1=HalfOpen`, `2=Open`) is cycling rapidly between `Open` and `Closed` for a downstream connector branch, causing intermittent partial failures, retries, and latency spikes.

## Triage

1. Identify the affected branch from alert labels (`$labels.branch`) and inspect breaker + error EWMA panels in [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json).
2. Check if the downstream service is experiencing grey failures (intermittent 5xx/timeouts just above the half-open probe threshold) or connection pool starvation.
3. Inspect retry budget exhaustion and branch latency:
   ```bash
   curl -sf http://127.0.0.1:8080/metrics | grep -E 'gap_breaker_state|gap_branch_|gap_retry_'
   ```

## Mitigation

1. Contact the downstream connector owner or inspect upstream dependency health; verify whether a downstream rate limit or deploy is underway.
2. If retry amplification is destabilizing a recovering downstream, tighten retry budgets or increase open-state cooldown / half-open probe window in `gap.toml`, then validate via `gap config validate --config gap.toml` and `gap doctor`.
3. Confirm `gap_breaker_state{branch="<branch>"}` stabilizes at `0` (healthy) or stays `2` (isolated) without flapping.
