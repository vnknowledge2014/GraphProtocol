# Decision Record: Gate G-S1 — System-1 (Laya) vs Baseline Discovery Routing

- **Gate ID:** `G-S1` (`RTE-069`, `ADR-007`, `ADR-008`, `ADR-021`, `INV-17`, `VER-030`)
- **Work Package:** `WP-M5-08`
- **Decision:** **`FAIL` (S1 ships disabled by default: `routing.model = "baseline"` remains the default; `routing.model = "s1"` is strictly opt-in).**
- **Triggered Follow-ups:** [`WP-M5-09`](../../work/work-packages.toml) (reproducible domain fine-tuning on `Fit` + multilingual assessment) and [`WP-R1-01`](../../work/work-packages.toml) (only if domain fine-tuning fails to clear `G-S1`).

---

## 1. Provenance (`VER-030`)

Every metric reported in this decision record was measured directly after merging `WP-M5-07` onto `main` using `--release` binaries and `--offline` workspace dependencies:

| Field | Measured Value |
|---|---|
| Base `main` Commit SHA | `6fc7c189342d4852e89945926e95f709f9ad7c8a` |
| `Cargo.lock` SHA-256 | `96537407ab8e0eeb958d5659ea72997140da28bd01eb3bd3fb74fc4b1005a531` |
| Toolchain | `rustc 1.98.1 (48a229cea 2026-09-01)` / `cargo 1.98.1 (797e8a9bc 2026-08-05)` |
| Host OS | `macOS 26.7.3` (`BuildVersion 25G342`) |
| CPU / Memory | `Intel(R) Core(TM) i9-9980HK CPU @ 2.40GHz` (`16` logical cores), `64 GiB` (`68719476736` bytes) |
| Weights Tested | `model_id = "laya-candle:f32:657ff01a71e9"`, `source = "builtin:calibrated_default_f32"` |
| External Weights Status | `laya-typed-decisions`, `laya-base`, `laya-multilingual` remain `offline-unpinned-requires-online-verify` in [models/manifest.toml](../../models/manifest.toml) (`WP-M5-01`, `SEC-102`) |
| Seed / Bootstrap | `seed = 20261115`, `smoke = false`, `bootstrap_iters = 1000` (95% percentile CIs) |
| RouteBench v1 Corpus | `5` branches (`4` in-domain, `1` held-out); `fit = 124` (`13` ops), `cal = 40` (`4` ops), `test = 48` (`5` ops: `42` in-domain, `6` OOD), `held_out = 72` (`72` in-domain, `0` OOD) |
| Primary Artifacts | [baseline/report.md](../../bench/results/routebench/baseline/report.md), [baseline/report.json](../../bench/results/routebench/baseline/report.json), [s1_comparison.json](../../bench/results/routebench/s1_comparison/s1_comparison.json), [debias_eval.json](../../bench/results/routebench/s1_comparison/debias_eval.json), [mined_hard_negatives.json](../../bench/results/routebench/s1_comparison/mined_hard_negatives.json) |

### Reproduction Commands

```bash
cargo run --release -p routebench --offline -- \
  --out bench/results/routebench/baseline --bootstrap 1000

GAP_ROUTEBENCH_FULL=1 \
GAP_ROUTEBENCH_S1_OUT="$(pwd)/bench/results/routebench/s1_comparison" \
cargo test --release -p routebench --offline -- --nocapture

cargo test -p gap-s1 --release --features s1-ort ort::tests --offline -- --nocapture
```

---

## 2. Raw (`T = 1.0`) vs Calibrated vs Debiased Latency, Calibration, and RSS

| Variant | Raw (`T=1.0`) Top-1 | Raw `ECE_15` | Raw `p50` (ms) | Raw `p95` (ms) | Raw `p99` (ms) | Calibrated `p50` (ms) | Calibrated `p95` (ms) | Process RSS |
|---|---|---|---|---|---|---|---|---|
| `BaselineRouter` (`s1_comparison`) | `0.9524` | `0.7657` | `0.597` | `0.827` | `0.881` | `0.579` | `0.887` | — |
| `BaselineRouter` (standalone CLI) | — | — | — | — | — | `0.488` | `0.750` | — |
| `S1 LayaCandle` (`debias = false`) | `0.0714` | `0.0853` | `33.708` | `36.979` | `37.273` | `36.096` | `42.339` | `16,616 KiB` (`16.2 MiB`) |
| `S1 LayaCandle` (`debias = true`) | `0.1429` | `0.0481` | `60.245` | `73.503` | `78.732` | `62.085` | `73.851` | `16,616 KiB` (`16.2 MiB`) |

### Backend Microbenchmark Parity (`WP-M5-06`, Single Reference Forward Fixture)

Measured on the same `i9-9980HK` macOS host with `cargo test -p gap-s1 --release --features s1-ort ort::tests --offline -- --nocapture` (`10` iterations):

| Backend | Mean Latency (`us` / `ms`) | Max `|Δlogit|` vs `LayaCandle` | Dispatch / Notes |
|---|---|---|---|
| `LayaCandle` (`F32`) | `2664.64 us` (`2.665 ms`) | reference | Pure-Rust ModernBERT + 2-layer head |
| `LayaOrt` (`Cpu`) | `2879.37 us` (`2.879 ms`) | `0.000000` | Pinned `ort = "=2.0.0-rc.15"` CPU execution provider |
| `LayaOrt` (`CoreMl`) | `2695.57 us` (`2.696 ms`) | `0.000000` | Apple CoreML dispatch (host CPU fallback on `x86_64`) |

- Full-context multi-group RouteBench forward passes (`p50 = 36.10 ms`, `p95 = 42.34 ms`, `debias p95 = 73.85 ms`) reflect realistic multi-option `[MASK]` sequence lengths across all candidate groups per request, whereas the isolated unit parity fixture measures a single short canonical sequence (`~2.67 ms`).
- Measured process RSS during S1 evaluation was `16,616 KiB` (`~16.2 MiB`) for the built-in reference weights, well under the `2.0 GB` `M5` memory budget (`spec/04-routing.md` §9); production `f16` 421M weights (`~0.85 GB`) + `bge-small-en-v1.5` (`~31 MB` index) are projected to remain within `<= 2.0 GB` but require re-measurement once live weights are verified.

---

## 3. All 14 `RTE-061` Metrics with 95% Bootstrap Confidence Intervals

### 3.1 `Test` Split (`48` total: `42` in-domain, `6` OOD)

| Metric (`RTE-061`) | `BaselineRouter` | `S1 Forward` (`debias=false`) | `S1 Debiased` (`debias=true`) |
|---|---|---|---|
| `p50` Latency (ms) | `0.579` (CLI `0.488`) | `36.096` | `62.085` |
| `p95` Latency (ms) | `0.887` (CLI `0.750`) | `42.339` | `73.851` |
| Top-1 Accuracy | `0.9524 [0.8837, 1.0000]` | `0.0714 [0.0000, 0.1500]` | `0.1429 [0.0465, 0.2619]` |
| Top-3 Accuracy | `0.9762 [0.9211, 1.0000]` | `0.2381 [0.1136, 0.3721]` | `0.3333 [0.2000, 0.4762]` |
| MRR | `0.9702 [0.9226, 1.0000]` | `0.2448 [0.1776, 0.3178]` | `0.3000 [0.2119, 0.3996]` |
| `ECE_15` | `0.7304 [0.6328, 0.8125]` | `0.0445 [0.0177, 0.1104]` | `0.0303 [0.0268, 0.1436]` |
| Brier Score | `0.7964 [0.7428, 0.8353]` | `0.8705 [0.7964, 0.9234]` | `0.8578 [0.7830, 0.9121]` |
| NLL | `1.9467 [1.8048, 2.0583]` | `2.4783 [2.2496, 2.6665]` | `2.4224 [2.1760, 2.6224]` |
| Selective Acc @ 50% | `0.8333 [0.7500, 1.0000]` | `0.0417 [0.0000, 0.1250]` | `0.2500 [0.0833, 0.4583]` |
| Selective Acc @ 70% | `0.8824 [0.7941, 1.0000]` | `0.0882 [0.0000, 0.1765]` | `0.1765 [0.0588, 0.3235]` |
| Selective Acc @ 90% | `0.8636 [0.7727, 0.9773]` | `0.0682 [0.0000, 0.1364]` | `0.1364 [0.0455, 0.2500]` |
| Abstention AUROC | `0.6508 [0.1907, 1.0000]` | `0.4048 [0.0398, 0.8636]` | `0.4127 [0.0568, 0.8580]` |
| Conformal Coverage | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` |
| Mean Set Size | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` |
| Ambiguity Rate | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` |
| OOD Abstention Prec | `0.1250 [0.0417, 0.2292]` | `0.1250 [0.0417, 0.2292]` | `0.1250 [0.0417, 0.2292]` |

### 3.2 `HeldOutBranch` Split (`72` total: `72` in-domain, `0` OOD)

| Metric (`RTE-061`) | `BaselineRouter` | `S1 Forward` (`debias=false`) | `S1 Debiased` (`debias=true`) |
|---|---|---|---|
| `p50` Latency (ms) | `0.692` (CLI `0.572`) | `35.818` | `68.147` |
| `p95` Latency (ms) | `1.098` (CLI `0.844`) | `41.744` | `78.439` |
| Top-1 Accuracy | `0.9722 [0.9306, 1.0000]` | `0.0278 [0.0000, 0.0694]` | `0.0278 [0.0000, 0.0694]` |
| Top-3 Accuracy | `1.0000 [1.0000, 1.0000]` | `0.2361 [0.1389, 0.3333]` | `0.1944 [0.1111, 0.2917]` |
| MRR | `0.9838 [0.9583, 1.0000]` | `0.2320 [0.1960, 0.2757]` | `0.2070 [0.1687, 0.2489]` |
| `ECE_15` | `0.8385 [0.7970, 0.8666]` | `0.0673 [0.0345, 0.0990]` | `0.0700 [0.0289, 0.0994]` |
| Brier Score | `0.8160 [0.8143, 0.8184]` | `0.9342 [0.9307, 0.9380]` | `0.9343 [0.9281, 0.9401]` |
| NLL | `2.0177 [2.0082, 2.0288]` | `2.7093 [2.6871, 2.7314]` | `2.7195 [2.6695, 2.7623]` |
| Selective Acc @ 50% | — | `0.0000 [0.0000, 0.0000]` | `0.0556 [0.0000, 0.1389]` |
| Selective Acc @ 70% | (CLI `0.9804`) | `0.0000 [0.0000, 0.0000]` | `0.0392 [0.0000, 0.0980]` |
| Selective Acc @ 90% | — | `0.0154 [0.0000, 0.0462]` | `0.0308 [0.0000, 0.0769]` |
| Abstention AUROC | — | `0.0929 [0.0141, 1.0000]` | `0.7571 [0.6479, 1.0000]` |
| Conformal Coverage | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` | `0.0000 [0.0000, 0.0000]` |

---

## 4. `RTE-043` (`s1.debias`) and Hard-Negative Mining Results

- **`s1.debias` Evaluation (`debias_eval.json`):**
  - Forward Top-1: `0.071429` -> Debiased Top-1: `0.142857` (**`+0.071429`** gain, satisfying `>= +0.01`).
  - Forward `p95`: `42.339 ms` -> Debiased `p95`: `73.851 ms` (**`1.744x`** ratio, satisfying `<= 2.0x`).
  - `enable_by_default = true`: whenever an operator explicitly enables S1 (`routing.model = "s1"`), `s1.debias` defaults to `true` per `RTE-043`. However, because `G-S1` fails overall, S1 itself remains disabled by default (`routing.model = "baseline"`).
- **Hard-Negative Mining (`mined_hard_negatives.json`):**
  - Mined `270` confusing-group records (`schema_version = 2`, `router_id = "baseline"`) across `fit = 113`, `calibration = 39`, `test = 46`, `held_out_branch = 72` with zero operation/branch split leakage verified (`RTE-060`). These mined examples feed `WP-M5-09` fine-tuning on the `Fit` split only.

---

## 5. Criterion-by-Criterion Gate `G-S1` Verdict (`RTE-069`)

| Criterion (`RTE-069`) | Required Threshold | Measured (`S1 Debiased` / `S1 Forward`) | Pass? | Diagnosis |
|---|---|---|---|---|
| (i) Accuracy gain on `Test` | Top-1 `>= Baseline + 0.03` (`0.9824`) or selective acc gain `>= +0.03` | Top-1 `0.1429` / `0.0714` vs `0.9524` (`-0.8095` / `-0.8810`); Sel@70 `0.1765` / `0.0882` vs `0.8824` | **FAIL** | Offline built-in `calibrated_default_f32` weights are un-finetuned deterministic reference weights; live pretrained Laya weights require online verification (`WP-M5-01`) and domain fine-tuning (`WP-M5-09`). |
| (ii) Calibration error (`Test`) | `ECE_15 <= 0.05` | `0.0303` (`s1_debiased`) / `0.0445` (`s1_forward`) | **PASS** | Post-temperature-scaling `ECE_15` satisfies `<= 0.05` (`T = 1.248` in `11..16` bucket), whereas `BaselineRouter` shows severe underconfidence (`ECE_15 = 0.7304`). |
| (iii) Conformal empirical coverage | `>= 1 - alpha - 0.02` (`0.9269` weighted target in `s1_comparison.json`) | `0.0000` (all buckets in `CalibrationMode::Fallback`) | **FAIL** | RouteBench v1 `Calibration` split contains `40` examples across 4 buckets (`2, 1, 3, 34`), below `MIN_CONFORMAL_CAL_SAMPLES = 500` (`RTE-064`). Synthetic `>= 500`/bucket coverage holds (`WP-M5-04`), but real RouteBench calibration data must reach `>= 500` per bucket. |
| (iv) Latency SLO | `routebench` strict target `<= 40.0 ms`; `spec/04-routing.md` §9 ceiling `<= 250 ms` CPU / `<= 80 ms` GPU-Metal | `42.339 ms` (`s1_forward`) / `73.851 ms` (`s1_debiased`) vs `0.887 ms` baseline | **FAIL** (vs `40 ms` `routebench` SLO; **PASS** vs `250 ms` CPU / `80 ms` Metal) | Recorded accurately under both thresholds: `s1_comparison.json` records `latency_ok = false` against `slo_p95_ms = 40.0`, while `p95` stays comfortably below the `spec/04-routing.md` §9 `250 ms` CPU budget (`~3.4x–5.9x` headroom). |

**Overall Verdict (`RTE-069`):** **`FAIL`** (`accuracy_gain_ok = false`, `ece_ok = true`, `coverage_ok = false`, `latency_ok = false` at `40 ms`). Per `INV-17` and `RTE-069`, a failing S1 **never blocks release**; S1 ships **disabled by default**.

---

## 6. Operational Default, Opt-In Flag, and Rollback Runbook

- **Shipping Default (`INV-17`):**
  ```toml
  [routing]
  model = "baseline"
  ```
- **Opt-In Flag (Staging / Benchmark / Evaluation Only):**
  Operators who pull and SHA-256 verify compatible weights into `models/manifest.toml` (`WP-M5-01`) or configure an external sidecar (`WP-M5-06`) may enable S1 explicitly while keeping automatic fallback (`RTE-044`, `INV-17`) armed:
  ```toml
  [routing]
  model = "s1"

  [routing.s1]
  debias = true              # RTE-043 (+0.0714 Top-1 at 1.74x latency)
  timeout_ms = 400           # RTE-044 hard timeout fallback to BaselineRouter
  failure_rate_threshold = 0.20
  circuit_cooldown_secs = 60
  ```
- **Rollback Procedure:**
  1. Set `routing.model = "baseline"` in server config and hot-reload/restart (or remove weights to trigger deterministic `S1BackendMode::Disabled` fallback per `WP-M5-03`).
  2. Automatic circuit rollback (`RTE-044`) also flips active traffic to `BaselineRouter` for `60 s` whenever rolling timeout/backend failure rate exceeds `20%` over `1 min`, emitting `GAP_DISCOVERY_DEGRADED` and incrementing `gap_router_fallback_total`.
- **Re-Evaluation Criteria (Next Actions):**
  1. Complete `WP-M5-09` to train domain-adapted weights strictly on `Fit` + mined `Fit` hard negatives and evaluate multilingual vs translate-at-edge behavior (`ADR-004`).
  2. Expand RouteBench calibration split to `>= 500` examples per active option-count bucket (`RTE-064`) so conformal quantiles exit `Fallback`.
  3. Re-run `routebench` `--release` against `G-S1`; promote `routing.model = "s1"` only if all 4 `RTE-069` checks pass, or advance to `WP-R1-01` if fine-tuning cannot clear `G-S1`.
