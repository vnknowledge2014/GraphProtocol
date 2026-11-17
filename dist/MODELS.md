# GAP Release Model Artifacts & Verification Guide

**Spec / ADR Refs:** `SEC-101`, `SEC-102`, `OPS-041`, `RTE-044`, `INV-17`, `ADR-021`

GAP ships **zero model weights inside container images or release archives**. All model
provenance, licensing, SHA-256 digests, and download URLs are governed centrally by
[`models/manifest.toml`](../models/manifest.toml).

---

## 1. Fail-Closed Integrity Model (`SEC-102`)

1. Model weights are downloaded out-of-band (or pre-staged into an air-gapped `.gap/models`
   directory) and verified against `models/manifest.toml` before use:
   ```bash
   gap models verify --manifest models/manifest.toml --cache .gap/models
   ```
2. If any cached file fails SHA-256 or byte-size verification, has status
   `offline-unpinned-requires-online-verify`, or is missing in offline mode:
   ```bash
   gap models pull --manifest models/manifest.toml --cache .gap/models --offline
   ```
   GAP fails closed (`GAP_MODEL_INTEGRITY` / `SEC-102`) and **does not** execute unverified weights.
3. Per `INV-17` and `RTE-044`, **GAP remains fully operational in `BASELINE_FALLBACK`**
   (deterministic lexical BM25 + structural routing) whenever S1 weights are absent,
   unpinned offline, or disabled by gate `G-S1`.

---

## 2. Manifest Inventory (`models/manifest.toml`)

| ID | Role | Licence | Default `1.0-RC` Status |
|---|---|---|---|
| `bge-small-en-v1.5` | `embedder` | `MIT` | Pinned (`sha256: 3c9f31665447c8911517620762200d2245a2518d6e7208acc78cd9db317e21ad`, `133466304` bytes) |
| `laya-typed-decisions` | `s1-router` | `Apache-2.0` | `conditional-advisory-only` · Pinned Hugging Face LFS (`sha256: 4fa56de7...8107a24e`, `842609220` bytes, commit `989e28fe`) · **disabled by default (`BASELINE_FALLBACK`, `RTE-044`, Gate `G-S1`)** |
| `laya-base` | `s1-experiment` | `Apache-2.0` | `experimental-offline-eval-only` · Pinned Hugging Face LFS (`sha256: 891102d3...cbc8d86c`, `842609210` bytes, commit `e333cb7c`) |
| `laya-multilingual` | `s1-experiment` | `Apache-2.0` | `experimental-non-default-regression-0.342` · Pinned Hugging Face LFS (`sha256: 9d628fd9...95aa8f204`, `643835514` bytes, commit `92d22143`) |

---

## 3. Enabling S1 Router in Connected Environments

Only enable S1 after completing online release verification (`SEC-102`, `ADR-021`):
1. Fetch the upstream Hugging Face commit digest for `laya-typed-decisions` and update
   `models/manifest.toml` with the verified SHA-256 and byte count via a reviewed PR.
2. Pull into the local model cache with SHA-256 verification enabled and confirm `gap doctor`
   plus `routebench` `G-S1` calibration pass in your target environment before setting
   `[router] mode = "s1_candle"` (or `shadow`).
