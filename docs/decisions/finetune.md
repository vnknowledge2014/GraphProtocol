# Decision Record: Laya Fine-Tuning Pipeline & Multilingual Discovery Strategy

- **Work Package:** `WP-M5-09`
- **Spec Refs:** `ADR-005`, `ADR-004`, `ADR-007`, `RTE-042`, `RTE-060`, `RTE-069`, `SEC-102`, `ARC-002`, `INV-17`
- **Decision:** **Keep `translate-at-edge` as the GAP 1.0 multilingual default; execute domain fine-tuning strictly via [`tools/finetune/train_laya_routebench.py`](../../tools/finetune/train_laya_routebench.py) once live verified Laya base weights are pulled into `models/manifest.toml`.**

---

## 1. Context & Provenance Prerequisites

1. **Unstated Author Provenance (`WP-M5-01`):** Upstream `laya-typed-decisions`, `laya-base`, and `laya-multilingual` entries in [models/manifest.toml](../../models/manifest.toml) record `training_data_note = "unstated by authors"` and remain `offline-unpinned-requires-online-verify` until fetched via `gap models pull --verify` (`SEC-102`).
2. **Gate `G-S1` Outcome (`WP-M5-08`):** On offline `--release` RouteBench (`seed = 20261115`, [docs/decisions/G-S1.md](G-S1.md)), built-in deterministic reference weights achieved `0.1429` (`s1_debiased`) vs `0.9524` (`BaselineRouter`), leaving `routing.model = "baseline"` (`INV-17`) and triggering `WP-M5-09` to provide a GAP-owned, auditable fine-tuning pipeline before considering Research Track `WP-R1-01`.
3. **Prior Multilingual Baseline (`ADR-005`):** Upstream `laya-multilingual` measured near chance (`0.342`, macro-ECE `0.387`) and `ADR-005` explicitly bans `laya-multilingual` from default serving in GAP 1.0.

---

## 2. Reproducible Fine-Tuning Pipeline (`tools/finetune/train_laya_routebench.py`)

To ensure GAP can adapt S1 strictly on GAP-owned corpora without inheriting unverified upstream bias or data leakage:

1. **Strict Split Isolation (`RTE-060`):** `load_fit_records` and `verify_zero_split_leakage` filter inputs to `split == "fit"` only (`124` synthetic `fit` examples + `113` `fit` hard negatives from [mined_hard_negatives.json](../../bench/results/routebench/s1_comparison/mined_hard_negatives.json)), aborting (`SplitLeakageError`, exit `2`) if any `calibration`, `test`, or `held_out_branch` row, operation ID, or held-out branch appears in training.
2. **Canonical Sequence Parity (`RTE-042`, `RTE-020`):** Applies identical NFKC + control/bidi/reserved-marker stripping, sorted JCS state JSON (`sort_keys=True, separators=(",",":")`), trailing fixed `insufficient_evidence` sink option, and `max(8, floor(0.8 * head_max_len / n_options))` token budgets matching `crates/gap-s1/src/candle/sequence.rs`.
3. **Supply-Chain Provenance (`SEC-102`):** Every training run emits:
   - `.safetensors` weights (`F32`, pure PyTorch via `uv` or deterministic offline reference writer),
   - `manifest.json` pinning exact SHA-256, byte size, `seed`, hyperparameters, and split counts,
   - `MODEL_CARD.md` recording split provenance and the mandatory `RTE-069` Gate `G-S1` re-verification rule before updating `models/manifest.toml`.

---

## 3. Multilingual & Vietnamese Evaluation (`tools/finetune/eval_multilingual.py`)

Measured across `22` deterministic evaluation queries spanning `en_control` (`3`), `vi_accented` (`5`), `vi_unaccented` (`4`), `vi_code_switched` (`4`), `multilingual_other` (`fr`/`de`/`ja`, `4`), and cross-language `ood_abstention` (`2`) matched against English GAP capability-group schemas:

| Strategy | Overall Top-1 | Overall MRR | `vi_accented` | `vi_unaccented` | `vi_code_switched` | `multilingual_other` | `ood_abstention` |
|---|---|---|---|---|---|---|---|
| `raw_lexical` (raw UTF-8 query) | `0.5000` | `0.6674` | `0.2000` | `0.2500` | `0.7500` | `0.2500` | `1.0000` |
| `diacritic_fold` (NFKD + `đ->d`) | `0.5000` | `0.6674` | `0.2000` | `0.2500` | `0.7500` | `0.2500` | `1.0000` |
| `translate_at_edge` (MCP host EN intent) | **`1.0000`** | **`1.0000`** | **`1.0000`** | **`1.0000`** | **`1.0000`** | **`1.0000`** | **`1.0000`** |

### Diagnosis

- Stripping Latin diacritics (`diacritic_fold`) does **not** bridge non-cognate vocabulary across languages (`hóa đơn` vs `invoice`, `請求書` vs `invoice`): `vi_accented` (`0.2000`) and `vi_unaccented` (`0.2500`) remain low unless structured identifiers or English loanwords appear (`vi_code_switched`: `0.7500`, preserving `SKU-42`, `OAuth`, `GitHub`).
- Running `translate-at-edge` — where the calling LLM host translates the user's intent into English via the MCP tool description (`ADR-005`) while retaining literal SKUs/IDs/error codes — restores `1.0000` Top-1 across all slices while adding **zero** translation models or latency to GAP's discovery or execution paths (`ARC-002`, `ADR-004`).

---

## 4. Operational Recommendation & Trigger Matrix

1. **Default Policy (GAP 1.0):**
   - Retain `translate-at-edge` in MCP tool instructions (`ADR-005`).
   - Do **not** deploy `laya-multilingual` or server-side translation in GAP 1.0 (`ARC-002`).
2. **Trigger for Domain Fine-Tuning (`train_laya_routebench.py --backend pytorch`):**
   - Execute when online verified `laya-typed-decisions` / `laya-base` checkpoints are pulled (`gap models pull --verify`) and RouteBench calibration/test splits are expanded (`N_cal >= 500` per bucket, `RTE-064`).
   - Re-evaluate exported `.safetensors` against Gate `G-S1` (`docs/decisions/G-S1.md`); enable `routing.model = "s1"` only if all 4 `RTE-069` criteria pass.
3. **Trigger for Track `WP-R1-01` (Custom S1 Model):**
   - Open `WP-R1-01` go/no-go review **only if** live domain fine-tuned Laya weights still fail `G-S1` on `Test` Top-1/selective accuracy (`ADR-004`, `ADR-007`).
4. **Trigger for Dedicated Multilingual S1 Fine-Tuning:**
   - Revisit only if production telemetry shows `> 10%` of discovery calls arrive with untranslated non-English `intent` strings or domain slang that hosts fail to normalize at the edge, backed by a domain-annotated non-English `Fit`/`Calibration`/`Test` corpus (`>= 500` calibration examples per language bucket).
