# Laya Fine-Tuning & Multilingual Evaluation Tooling (`WP-M5-09`)

This directory contains reproducible fine-tuning and multilingual evaluation scripts for GAP System-1 (`Laya`) discovery reranking (`ADR-005`, `ADR-004`, `ADR-007`).

## Files

- [`train_laya_routebench.py`](train_laya_routebench.py) — PEP 723 `uv` script enforcing `RTE-060` split isolation (`split == "fit"` only, zero operation/branch overlap with `calibration`, `test`, or `held_out_branch`) and `RTE-042` / `RTE-020` JCS canonical sequence construction. Emits `.safetensors`, SHA-256 `manifest.json` (`SEC-102`), and `MODEL_CARD.md`.
- [`eval_multilingual.py`](eval_multilingual.py) — Evaluates Vietnamese (`vi_accented`, `vi_unaccented`, `vi_code_switched`) and multilingual (`fr`, `de`, `ja`, `en_control`, `ood_abstention`) intents against English capability-group schemas under `raw_lexical`, `diacritic_fold`, and `translate_at_edge` modes.

## Usage

### Offline Self-Tests (Pure Python stdlib, no GPU/network required)

```bash
python3 tools/finetune/train_laya_routebench.py --self-test
python3 tools/finetune/eval_multilingual.py --self-test
```

### Multilingual Comparison Summary

```bash
python3 tools/finetune/eval_multilingual.py
```

### Online Fine-Tuning via `uv` (when cached base weights are verified in `models/manifest.toml`)

```bash
uv run tools/finetune/train_laya_routebench.py \
  --backend pytorch \
  --base-model laya-typed-decisions \
  --mined bench/results/routebench/s1_comparison/mined_hard_negatives.json \
  --out target/laya-finetuned \
  --seed 20261115 \
  --epochs 3
```

## Promotion Rule (`RTE-069`)

Exported weights remain unpromoted until:
1. Their exact SHA-256 digest and byte length from `manifest.json` are pinned in `models/manifest.toml` (`SEC-102`).
2. `routebench --release` is re-executed against the held-out `test` split and passes all 4 criteria of Gate `G-S1` (`docs/decisions/G-S1.md`).
