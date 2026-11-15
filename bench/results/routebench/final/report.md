# RouteBench v1 Report

- **Router:** `baseline` | **Seed:** `20261115` | **Smoke:** `false` | **Bootstrap resamples:** `1000`
- **Branches:** 5 total (4 in-domain, 1 held-out) | **Examples:** fit=124, cal=40, test=48, held-out=72

## Test Split Metrics (95% Bootstrap CI)

Evaluated `48` examples (`42` in-domain, `6` OOD); latency p50 `0.625 ms`, p95 `0.882 ms`.

| Metric | Estimate [95% CI] |
|---|---|
| Top-1 accuracy | 0.9524 [0.8837, 1.0000] |
| Top-3 recall | 0.9762 [0.9211, 1.0000] |
| MRR | 0.9702 [0.9226, 1.0000] |
| ECE_15 | 0.7304 [0.6328, 0.8125] |
| Brier score | 0.7964 [0.7428, 0.8353] |
| Mean NLL | 1.9467 [1.8048, 2.0583] |
| Selective acc @ 50% | 0.8333 [0.7500, 1.0000] |
| Selective acc @ 70% | 0.8824 [0.7941, 1.0000] |
| Selective acc @ 90% | 0.8636 [0.7727, 0.9773] |
| Abstention AUROC | 0.6508 [0.1907, 1.0000] |
| Conformal coverage | 0.0000 [0.0000, 0.0000] |
| Mean set size | 0.0000 [0.0000, 0.0000] |
| Ambiguity rate | 0.0000 [0.0000, 0.0000] |
| OOD abstention precision | 0.1250 [0.0417, 0.2292] |

## Held-Out Branch Cold-Start Metrics (95% Bootstrap CI)

Evaluated `72` examples (`72` in-domain, `0` OOD); latency p50 `0.654 ms`, p95 `1.143 ms`.

| Metric | Estimate [95% CI] |
|---|---|
| Top-1 accuracy | 0.9722 [0.9306, 1.0000] |
| Top-3 recall | 1.0000 [1.0000, 1.0000] |
| MRR | 0.9838 [0.9583, 1.0000] |
| ECE_15 | 0.8385 [0.7970, 0.8666] |
| Brier score | 0.8160 [0.8143, 0.8184] |
| Mean NLL | 2.0177 [2.0082, 2.0288] |
| Selective acc @ 50% | 0.9722 [0.9167, 1.0000] |
| Selective acc @ 70% | 0.9804 [0.9216, 1.0000] |
| Selective acc @ 90% | 0.9692 [0.9231, 1.0000] |
| Abstention AUROC | 0.5571 [0.5224, 1.0000] |
| Conformal coverage | 0.0000 [0.0000, 0.0000] |
| Mean set size | 0.0000 [0.0000, 0.0000] |
| Ambiguity rate | 0.0000 [0.0000, 0.0000] |
| OOD abstention precision | 0.0000 [0.0000, 0.0000] |

## Per-Bucket Calibration

| Bucket | Mode | N_fit | N_cal | T_hat | alpha | q_hat | ECE before | ECE after |
|---|---|---|---|---|---|---|---|---|
| 2 | `fallback` | 13 | 2 | 5.000 | 0.10 | fallback | 0.1489 | 0.1021 |
| 3..5 | `fallback` | 1 | 1 | 5.000 | 0.10 | fallback | 0.2717 | 0.2256 |
| 6..10 | `fallback` | 6 | 3 | 5.000 | 0.10 | fallback | 0.5859 | 0.6039 |
| 11..16 | `fallback` | 104 | 34 | 0.500 | 0.10 | fallback | 0.8536 | 0.8174 |

## Test Split Reliability Diagram (15 Bins)

| Bin | Range | Count | Avg Conf | Avg Acc |
|---|---|---|---|---|
| 0 | [0.00, 0.07) | 0 | 0.0000 | 0.0000 |
| 1 | [0.07, 0.13) | 32 | 0.1306 | 0.8750 |
| 2 | [0.13, 0.20) | 12 | 0.1426 | 1.0000 |
| 3 | [0.20, 0.27) | 0 | 0.0000 | 0.0000 |
| 4 | [0.27, 0.33) | 1 | 0.2883 | 0.0000 |
| 5 | [0.33, 0.40) | 0 | 0.0000 | 0.0000 |
| 6 | [0.40, 0.47) | 0 | 0.0000 | 0.0000 |
| 7 | [0.47, 0.53) | 0 | 0.0000 | 0.0000 |
| 8 | [0.53, 0.60) | 0 | 0.0000 | 0.0000 |
| 9 | [0.60, 0.67) | 1 | 0.6637 | 0.0000 |
| 10 | [0.67, 0.73) | 0 | 0.0000 | 0.0000 |
| 11 | [0.73, 0.80) | 0 | 0.0000 | 0.0000 |
| 12 | [0.80, 0.87) | 0 | 0.0000 | 0.0000 |
| 13 | [0.87, 0.93) | 0 | 0.0000 | 0.0000 |
| 14 | [0.93, 1.00) | 2 | 1.0000 | 1.0000 |

## Gate G-S1 Summary (`RTE-069`)

- **Meets baseline delta (>= +0.03 top-1 or sel@70):** `false`
- **ECE_15 <= 0.05:** `false` (actual: `0.7304`)
- **Conformal coverage >= 1 - alpha - 0.02:** `false` (actual: `0.0000`, target: `0.9269`)
- **Latency p95 <= SLO (40.0 ms):** `true` (actual: `0.882 ms`)
- **Overall G-S1 verdict:** **BASELINE_FALLBACK**
