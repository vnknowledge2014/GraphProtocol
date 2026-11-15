# Decision Memo (`WP-R1-01`): Custom System-1 (`RLCD`-Style) Training Go/No-Go

- **Work Package:** `WP-R1-01`
- **Spec Refs:** `ADR-004`, `ADR-007`, `ADR-021`, `INV-17`, `RTE-069`, `RTE-064`, `RTE-060`, `SEC-102`, `VER-030`
- **Recommendation:** **`NO-GO` for GAP 1.0** (`0` GPU-hours / `\$0` immediate budget authorized; conditional reserve of `64–128` GPU-hours **only** after live domain fine-tuning under [`WP-M5-09`](finetune.md) runs and fails Gate `G-S1`).

---

## 1. Executive Summary & Current Measured Baseline

Under `ADR-007` and `INV-17`, custom Research Track `R1` (`RLCD`-style custom System-1 training on ModernBERT or similar encoder backbones) is strictly **blocked behind a sequential prerequisite**:

1. Pull and SHA-256 verify upstream Laya weights in [`models/manifest.toml`](../../models/manifest.toml) (`WP-M5-01`, `SEC-102`).
2. Run GAP-owned domain fine-tuning strictly on `Fit` + `Fit`-only mined hard negatives via [`tools/finetune/train_laya_routebench.py`](../../tools/finetune/train_laya_routebench.py) (`WP-M5-09`) with an expanded `Calibration` corpus (`>= 500` samples/bucket, `RTE-064`).
3. Only if **post-fine-tune** live weights fail Gate `G-S1` (`RTE-069`) should `R1` training spend be unlocked.

### Measured Preconditions ([`docs/decisions/G-S1.md`](G-S1.md), `--release`, `seed = 20261115`)

| Variant | Split | Top-1 Accuracy | `ECE_15` | `p95` Latency (ms) | Status vs Gate `G-S1` (`RTE-069`) |
|---|---|---|---|---|---|
| `BaselineRouter` | `Test` (`42` in-domain / `6` OOD) | `0.9524 [0.8837, 1.0000]` | `0.7304` | `0.887` (`CLI 0.750`) | Shipping default (`INV-17`) |
| `BaselineRouter` | `HeldOutBranch` (`72` in-domain) | `0.9722 [0.9306, 1.0000]` | `0.8385` | `1.098` (`CLI 0.844`) | Shipping default (`INV-17`) |
| `S1 Debiased` (`builtin:calibrated_default_f32`) | `Test` | `0.1429 [0.0465, 0.2619]` | `0.0303` | `73.851` (`16.2 MiB` RSS) | **`FAIL`** (un-finetuned offline reference weights; `Calibration` has `40 < 500`/bucket; `73.851 ms > 40 ms` `routebench` target while `< 250 ms` CPU spec ceiling) |
| `S1 Debiased` (`builtin:calibrated_default_f32`) | `HeldOutBranch` | `0.0278 [0.0000, 0.0694]` | `0.0700` | `78.439` | **`FAIL`** |

Because live `laya-typed-decisions` / `laya-base` weights are still unpinned offline and RouteBench `Calibration` currently holds `40` samples across `4` buckets (`2, 1, 3, 34 < 500`), **no empirical evidence yet shows that fine-tuned Laya cannot meet GAP's needs**, while `BaselineRouter` already delivers `0.9524` (`Test`) / `0.9722` (`HeldOutBranch`) Top-1 at `< 1.1 ms` `p95`. Spending compute on custom `RLCD` pretraining/RLHF now is unwarranted (`NO-GO`).

---

## 2. Explicit Budget & Recommendation (`Go / No-Go`)

| Phase / Decision | Authorization | Compute / Financial Budget | Calendar | Gate Condition |
|---|---|---|---|---|
| **GAP 1.0 Release (Current Decision)** | **`NO-GO`** | **`0` GPU-hours / `\$0`** | `0` days | Ship `routing.model = "baseline"` (`INV-17`); execute `WP-M5-09` live Laya fine-tune offline/staging first once checkpoints are verified. |
| **Stage R1-A: Contrastive Preference Corpus + Pilot (Conditional)** | Conditional | `16` GPU-hours (`2× A100-80GB` or `4× L40S`, estimated `~\$30–\$60` spot) | `1` week | Triggered **only** if fine-tuned Laya fails `G-S1` Top-1 (`>= Baseline + 0.03`) after `Calibration >= 500`/bucket is satisfied. |
| **Stage R1-B: Full `RLCD`-Style Custom Encoder Run (Conditional)** | Conditional | `64–128` GPU-hours total (`~\$150–\$400` spot) + `~\$250` synthetic preference labeling reserve | `2–3` weeks | Triggered **only** if Stage R1-A pilot improves held-out dev Top-1 by `>= +0.02` over `WP-M5-09` fine-tune. |

---

## 3. Data Needs (Strict Split Hygiene `RTE-060`)

To train an `RLCD`-style directional decision / group-routing encoder without repeating upstream provenance gaps (`WP-M5-01` `"training_data_note = unstated by authors"`):

1. **In-Domain Supervised + Preference Trajectories (`Fit` split only):**
   - `>= 50,000` labeled routing/decision trajectories (or `>= 500` trajectories/operation across `>= 100` distinct GraphQL/MCP/OpenAPI/A2A/Code-Graph branches), pairing `(intent, compact state JSON, candidate option descriptors)` -> chosen option + hard negatives (`mined_hard_negatives` schema v2).
   - Generated **exclusively** from `Fit` branches/operations (`RTE-060`); zero overlap with `Calibration`, `Test`, or `HeldOutBranch`.
2. **Counterfactual / Contrastive Pairs (`RLCD` Style):**
   - Paired near-miss candidate sets (mutated state preconditions, swapped argument types, ambiguous vs clear intents, plus `insufficient_evidence` OOD abstention pairs matching `RTE-042` sink semantics).
   - At least `15%` explicit OOD / missing-precondition examples so the `[MASK]` head calibrates abstention (`Sel@70%`, `Abstention AUROC`) rather than collapsing onto overconfident forced guesses.
3. **Expanded Conformal Calibration Split (`RTE-064`):**
   - `>= 500` held-out `Calibration` samples per active option-count bucket (`2..3`, `4..6`, `7..10`, `11..16`, `> 16`, totaling `>= 2,500` calibration examples) so conformal quantiles exit `CalibrationMode::Fallback`.
4. **Independent `Test` + `HeldOutBranch` Evaluation Splits:**
   - Stratified across unseen operations (`Test`) and completely unseen connectors/branches (`HeldOutBranch`), including OOD probes, totaling `>= 1,000` test examples (`95%` bootstrap CIs width `<= +/- 0.03`).

---

## 4. Compute Requirements, Expected Gain vs Cost & Latency

### 4.1 Expected Gain vs Engineering & Serving Cost

- **Baseline Target to Beat:** `BaselineRouter` already reaches `0.9524` (`Test`) and `0.9722` (`HeldOutBranch`) Top-1 at `0.887 ms` / `1.098 ms` `p95`, with zero model artifact risk, `~0` startup overhead, and negligible RAM. Its primary weakness is raw probability calibration (`ECE_15 = 0.7304` on `Test`, `0.8385` on `HeldOutBranch`) and semantic paraphrases lacking lexical overlap.
- **Expected Gain Ceiling of Custom `RLCD` S1:**
  - At `0.9524–0.9722` baseline Top-1, residual headroom on clean lexical queries is narrow (`+0.028` to `+0.048` max theoretical gain); realistic `G-S1` Top-1 improvement (`>= Baseline + 0.03` -> `>= 0.9824` on expanded non-trivial paraphrase benchmarks) is only meaningful once `Test` is broadened to adversarial paraphrase / schema-agnostic intents where `BaselineRouter` drops below `0.85`.
  - Meanwhile, running ModernBERT-large (`~421M` parameters, `f16` `~0.85 GB`) costs `~36 ms–78 ms` `p95` on CPU (`40x–80x` slower than `BaselineRouter`) even before accounting for `s1.debias` (`1.744x` latency ratio measured in `WP-M5-08`).
- **Architectural Cost (`INV-17`, `ARC-002`):** Custom weights still require quantized/exported `.safetensors` verification (`SEC-102`), per-target ONNX/Candle parity testing (`WP-M5-06`), and continuous re-calibration across corpus drift.

### 4.2 Compute Budget Breakdown (If Triggered)

| Component | Target Configuration | Estimated Compute |
|---|---|---|
| Base backbone | Pretrained ModernBERT-base (`149M`) or ModernBERT-large (`421M`) | `0` (pretrained weights) |
| Synthetic `RLCD` pair + preference generation | Offline batch on open/local teacher generator + deterministic schema mutator | `16–24` GPU-hours |
| Contrastive SFT + `RLCD` preference optimization (`F16`/`BF16`, seq len `<= 1024`) | `3–5` epochs on `50k` pairs (`batch = 64`, `grad_accum`) | `32–80` GPU-hours |
| Temperature scaling + multi-seed ablations (`>= 3` seeds) + ONNX/Candle export | Single-GPU eval + `s1.debias` sweep | `16–24` GPU-hours |
| **Total Conditional Budget** | Reproducible CLI + pinned `manifest.json` | **`64–128` GPU-hours** (`~16` GPU-hours pilot + `48–112` GPU-hours full) |

---

## 5. Base Model Licensing & Provenance Compliance

1. **Upstream Laya (`laya-base`, `laya-typed-decisions`, `laya-multilingual`):**
   - `models/manifest.toml` records `training_data_note = "unstated by authors"`. Before any derivative custom training built on Laya checkpoints is distributed or relied upon operationally, its upstream repository licence (`NOTICE` / `LICENSE` header) and training data provenance must be verified online (`gap models pull --verify`).
2. **ModernBERT (`answerdotai/ModernBERT-base` / `ModernBERT-large`):**
   - Released under permissive **Apache-2.0**, suitable for commercial and open-source reference shipping, provided `LICENSE` and attribution notices remain intact and any synthetic training pairs respect upstream terms.
3. **Prohibited Dependencies (`ARC-002`, `ADR-005`):**
   - Never pull GPL/AGPL training runtimes or server-side translation models into Rust runtime crates (`L0`–`L6`). Training scripts remain strictly isolated under `tools/finetune/` (`uv` offline/batch), exporting audited `.safetensors` + SHA-256 `manifest.json` + `MODEL_CARD.md` (`SEC-102`).
   - Do **not** replace `translate-at-edge` with `laya-multilingual` (`ADR-005`, measured `0.342` Top-1 / `0.387` ECE upstream; `docs/decisions/finetune.md` confirmed `translate-at-edge` restores `1.0000` Top-1 on curated multilingual probes at zero server latency).

---

## 6. Evaluation Plan (`VER-030`, `RTE-061`, Gate `G-S1`)

Any future `R1` candidate model must pass the **exact** automated Rust gate without relaxing thresholds:

1. **Split & Leakage Audit (`RTE-060`):** Verify `source_op` and `held_out_branch` sets are strictly disjoint across `Fit` vs `Calibration` / `Test` / `HeldOutBranch`.
2. **Backend Parity (`WP-M5-06`):** Confirm `max |Δlogit| <= 1e-4` (`F32`) / `<= 1e-2` (`F16`) between PyTorch reference export, `LayaCandle`, and `LayaOrt` (`Cpu` / `CoreMl`).
3. **Gate `G-S1` (`RTE-069`) on Expanded `--release` RouteBench (`bootstrap_iters >= 1000`, `calibration >= 500`/bucket):**
   - **(i) Accuracy:** `Test` Top-1 `>= Baseline + 0.03` **or** `Selective Acc @ 70% >= Baseline + 0.03`, with non-regression (`>= Baseline - 0.01`) on `HeldOutBranch`.
   - **(ii) Calibration:** `ECE_15 <= 0.05` on both `Test` and `HeldOutBranch`.
   - **(iii) Conformal Coverage (`RTE-064`):** Empirical coverage `>= 1 - alpha - 0.02` across all calibrated option-count buckets (zero fallback buckets).
   - **(iv) Latency & RSS (`spec/04-routing.md` §9, `RTE-069`):** Server/CLI `p95 <= 40.0 ms` under RouteBench target (with `debias = true` evaluated under `<= 2.0x` forward ratio per `RTE-043`) and process RSS `<= 2.0 GB`.

---

## 7. Explicit Re-Entry & Stop Criteria

1. **Mandatory Prerequisite (Re-Entry Trigger):**
   - `WP-R1-01` stays closed (`NO-GO`) until operators pull verified Laya weights (`models/manifest.toml`), expand RouteBench (`Calibration >= 500`/bucket + paraphrase-rich `Test` >= 500 queries), run [`tools/finetune/train_laya_routebench.py`](../../tools/finetune/train_laya_routebench.py) (`WP-M5-09`), and produce a post-fine-tune Gate `G-S1` report showing Criterion (i) fails while Latency Criterion (iv) passes.
2. **Early-Stop Criteria (Abort `R1` Spend Immediately If Any Hold):**
   - **Stop-1 (Pilot Gain Too Small):** After Stage `R1-A` (`<= 16` GPU-hours), dev `Top-1` fails to exceed the `WP-M5-09` fine-tuned baseline by `>= +0.02` or fails to reduce `Error Rate` on paraphrase/OOD queries by `>= 20%` relative.
   - **Stop-2 (Latency Envelope Broken):** Quantized `f16` `LayaCandle`/`LayaOrt` forward latency exceeds `40 ms` `p95` (`routebench` target) or `250 ms` (`spec/04-routing.md` §9 CPU ceiling) without an architectural path (`<= 149M` backbone distillation) restoring latency budget.
   - **Stop-3 (Insufficient Calibration / Baseline Dominance):** If `BaselineRouter` (`BM25 + RRF + state rules`) already matches `>= 0.95` Top-1 on production paraphrase traffic once combined with MCP host intent normalization (`ADR-005`), permanently retire custom `R1` training and keep `routing.model = "baseline"` (`INV-17`).
