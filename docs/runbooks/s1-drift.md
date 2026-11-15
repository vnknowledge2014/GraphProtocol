# Runbook: `S1Drift`

- **Severity:** `ticket`
- **Spec:** `RTE-110`, `RTE-111`, `INV-17`
- **Dashboards:** [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json)

## Trigger

`gap_s1_online_ece > 0.05` for 10m OR `gap:discovery_fallback_ratio10m > 0.10`.

## Impact

System-1 (Laya/ONNX) routing probabilities have drifted out of calibration relative to observed execution outcomes (`RTE-111`), or S1 is frequently timing out / abstaining and falling back to the deterministic hybrid search baseline. Note `INV-17`: deterministic routing keeps GAP functional without ML, though discovery set sizes (`|C(x)|`) may widen.

## Triage

1. Check [gap-routing-s1.json](../../ops/grafana/gap-routing-s1.json) to distinguish **calibration drift** (`gap_s1_online_ece > 0.05`) from **fallback spike** (`gap_router_fallback_total{reason=...}`).
2. Verify pinned S1 artifacts and runtime configuration:
   ```bash
   gap models list --config gap.toml
   gap doctor --json --config gap.toml
   ```
3. Check if `gap_s1_inference_duration_seconds` p99 breached its latency budget or if a new schema snapshot altered operation descriptors significantly without recalibrating.

## Mitigation

1. Run offline temperature/conformal recalibration against recent attributed feedback rows (`RTE-110`):
   ```bash
   gap calibrate --help
   ```
   and publish the updated calibration table into a new snapshot.
2. If model weights fail checksum verification or inference latency spikes due to host contention, disable S1 temporarily (per `INV-17`, deterministic BM25+HNSW discovery serves traffic safely with zero ML dependency) until the artifact or CPU budget is restored.
3. Verify `gap_s1_online_ece <= 0.05` and conformal prediction set sizes normalize.
