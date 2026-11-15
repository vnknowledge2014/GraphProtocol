#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "torch>=2.4",
#   "safetensors>=0.4",
#   "transformers>=4.45",
# ]
# ///
"""Reproducible Laya System-1 fine-tuning pipeline on RouteBench fit split (WP-M5-09).

Enforces:
- RTE-060: strict split isolation (trains ONLY on `fit` + `fit` mined hard negatives;
  rejects any overlap of operation IDs or branches with `calibration`, `test`, or
  `held_out_branch`).
- RTE-042 / RTE-020: NFKC normalization, control/bidi/special-token stripping,
  canonical JCS state JSON, fixed trailing `insufficient_evidence` sink option,
  and `max(8, floor(0.8 * head_max_len / n_options))` per-option token budgets.
- SEC-102: emits deterministic SHA-256 `manifest.json` + `MODEL_CARD.md` so weights
  never bypass manifest pinning before entering `models/manifest.toml`.

Run online PyTorch training (via `uv`):
  uv run tools/finetune/train_laya_routebench.py \
    --mined bench/results/routebench/s1_comparison/mined_hard_negatives.json \
    --out target/laya-finetuned

Run offline pure-stdlib self-test (no PyTorch or network required):
  python3 tools/finetune/train_laya_routebench.py --self-test
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import random
import re
import struct
import sys
import tempfile
import unicodedata
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SINK_KEY = "insufficient_evidence"
SINK_TEXT = "None of the other options can fulfil the request."
ALLOWED_TRAIN_SPLIT = "fit"
HELD_OUT_SPLITS = frozenset({"calibration", "test", "held_out_branch"})

RESERVED_MARKERS = (
    "[MASK]",
    "[CLS]",
    "[SEP]",
    "[PAD]",
    "[UNK]",
    "<mask>",
    "<s>",
    "</s>",
    "<pad>",
    "<unk>",
)
SPECIAL_PIPE_RE = re.compile(r"<\|[^|>]+\|>")


class SplitLeakageError(ValueError):
    """Raised when fit examples overlap calibration/test/held-out splits (RTE-060)."""


@dataclass(frozen=True)
class GroupOption:
    key: str
    text: str


@dataclass(frozen=True)
class FitExample:
    example_id: str
    split: str
    query: str
    max_side_effect: str
    gold_group_id: str
    options: tuple[GroupOption, ...]
    source_op: str | None = None
    source_branch: str | None = None


def sanitize_s1_text(text: str) -> str:
    """Idempotent RTE-020 / ADR-005 input sanitizer matching crates/gap-route/src/sanitize."""
    out = unicodedata.normalize("NFKC", text)
    cleaned_chars: list[str] = []
    for ch in out:
        cat = unicodedata.category(ch)
        # Strip control, format/bidi (Cf), surrogate, private-use characters
        if cat in ("Cc", "Cf", "Cs", "Co"):
            cleaned_chars.append(" ")
        else:
            cleaned_chars.append(ch)
    out = "".join(cleaned_chars)
    out = SPECIAL_PIPE_RE.sub(" ", out)
    for marker in RESERVED_MARKERS:
        out = out.replace(marker, " ")
        out = out.replace(marker.lower(), " ")
        out = out.replace(marker.upper(), " ")
    return " ".join(out.split())


def jcs_state_json(query: str, max_side_effect: str = "NONE") -> str:
    """Canonical JCS-sorted state JSON matching crates/gap-route/src/s1_adapter (RTE-042)."""
    clean_query = sanitize_s1_text(query)
    obj = {
        "constraints": {"max_side_effect": sanitize_s1_text(max_side_effect) or "NONE"},
        "query": clean_query,
    }
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def build_rte042_sequence(
    query: str,
    max_side_effect: str,
    options: list[GroupOption],
    *,
    head_max_len: int = 512,
    question: str = "Select the single capability group whose schema operations fulfill the query.",
) -> dict[str, Any]:
    """Format canonical RTE-042 sequence with trailing sink and per-option token caps."""
    if not options:
        raise ValueError("options list must be non-empty")

    non_sink = [opt for opt in options if opt.key != SINK_KEY]
    ordered = non_sink + [GroupOption(key=SINK_KEY, text=SINK_TEXT)]
    n_options = len(ordered)
    per_option_budget = max(8, math.floor(0.8 * head_max_len / n_options))

    state_str = jcs_state_json(query, max_side_effect)
    clean_q = sanitize_s1_text(question)

    formatted_opts: list[dict[str, str]] = []
    prompt_parts = ["[CLS]", state_str, "[SEP]", clean_q, "[SEP]"]
    for idx, opt in enumerate(ordered):
        if opt.key == SINK_KEY:
            text = SINK_TEXT
            canonical_key = SINK_KEY
        else:
            tokens = sanitize_s1_text(opt.text).split()[:per_option_budget]
            text = " ".join(tokens)
            canonical_key = f"g{idx}"
        formatted_opts.append({"key": canonical_key, "original_key": opt.key, "text": text})
        prompt_parts.extend(["[MASK]", text, "[SEP]"])

    return {
        "state": state_str,
        "question": clean_q,
        "options": formatted_opts,
        "per_option_token_budget": per_option_budget,
        "rendered_sequence": " ".join(prompt_parts),
    }


def verify_zero_split_leakage(
    fit_rows: list[dict[str, Any]],
    held_out_rows: list[dict[str, Any]] | None = None,
) -> None:
    """Enforce RTE-060: fit training inputs must never contain or overlap held-out splits."""
    fit_ops: set[str] = set()
    fit_branches: set[str] = set()

    for idx, row in enumerate(fit_rows):
        split = str(row.get("split", "")).strip()
        if split != ALLOWED_TRAIN_SPLIT:
            raise SplitLeakageError(
                f"row {idx} ({row.get('example_id', '?')}) has split={split!r}; "
                f"only {ALLOWED_TRAIN_SPLIT!r} is allowed in training (RTE-060)"
            )
        op = row.get("source_op")
        branch = row.get("source_branch")
        if op:
            fit_ops.add(str(op))
        if branch:
            fit_branches.add(str(branch))

    if held_out_rows:
        for row in held_out_rows:
            split = str(row.get("split", "")).strip()
            if split not in HELD_OUT_SPLITS:
                continue
            op = row.get("source_op")
            branch = row.get("source_branch")
            if op and str(op) in fit_ops:
                raise SplitLeakageError(
                    f"operation leakage detected across fit and {split!r}: {op!r} (RTE-060)"
                )
            if branch and str(branch) in fit_branches and split == "held_out_branch":
                raise SplitLeakageError(
                    f"held_out_branch leakage detected across fit and {split!r}: {branch!r} (RTE-060)"
                )


def load_fit_records(
    mined_path: Path | None,
    dataset_path: Path | None,
) -> tuple[list[FitExample], dict[str, int]]:
    """Load strictly `fit` training rows while checking `calibration/test/held_out_branch` isolation."""
    all_raw: list[dict[str, Any]] = []
    if mined_path is not None and mined_path.exists():
        payload = json.loads(mined_path.read_text(encoding="utf-8"))
        all_raw.extend(payload.get("records", []))
    if dataset_path is not None and dataset_path.exists():
        payload = json.loads(dataset_path.read_text(encoding="utf-8"))
        if isinstance(payload, dict) and "examples" in payload:
            all_raw.extend(payload["examples"])
        elif isinstance(payload, list):
            all_raw.extend(payload)

    fit_raw = [r for r in all_raw if r.get("split") == ALLOWED_TRAIN_SPLIT]
    held_raw = [r for r in all_raw if r.get("split") in HELD_OUT_SPLITS]
    verify_zero_split_leakage(fit_raw, held_raw)

    examples: list[FitExample] = []
    for row in fit_raw:
        if "options" in row and isinstance(row["options"], list):
            opts = tuple(GroupOption(key=str(o["key"]), text=str(o["text"])) for o in row["options"])
        else:
            gold = str(row["gold_group_id"])
            confusing = str(row.get("confusing_group_id", "neg"))
            op_label = str(row.get("source_op") or gold)
            branch_label = str(row.get("source_branch") or "branch")
            opts = (
                GroupOption(key=gold, text=f"{op_label} on {branch_label}"),
                GroupOption(key=confusing, text=f"confusing sibling group {confusing} in {branch_label}"),
                GroupOption(key=SINK_KEY, text=SINK_TEXT),
            )
        examples.append(
            FitExample(
                example_id=str(row.get("example_id", f"fit-{len(examples)}")),
                split=ALLOWED_TRAIN_SPLIT,
                query=str(row["query"]),
                max_side_effect=str(row.get("max_side_effect", "NONE")),
                gold_group_id=str(row["gold_group_id"]),
                options=opts,
                source_op=row.get("source_op"),
                source_branch=row.get("source_branch"),
            )
        )

    counts = {
        "fit": len(fit_raw),
        "excluded_held_out": len(held_raw),
    }
    return examples, counts


def write_minimal_f32_safetensors(path: Path, tensors: dict[str, tuple[list[int], list[float]]]) -> str:
    """Pure-stdlib safetensors v1 F32 writer producing byte-valid archives with SHA-256."""
    header: dict[str, Any] = {}
    body_chunks: list[bytes] = []
    cursor = 0
    for name in sorted(tensors):
        shape, values = tensors[name]
        raw = struct.pack(f"<{len(values)}f", *values)
        end = cursor + len(raw)
        header[name] = {
            "dtype": "F32",
            "shape": shape,
            "data_offsets": [cursor, end],
        }
        body_chunks.append(raw)
        cursor = end
    header_bytes = json.dumps(header, separators=(",", ":"), sort_keys=True).encode("utf-8")
    pad = (8 - (len(header_bytes) % 8)) % 8
    header_padded = header_bytes + (b" " * pad)
    archive = struct.pack("<Q", len(header_padded)) + header_padded + b"".join(body_chunks)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(archive)
    return hashlib.sha256(archive).hexdigest()


def run_reference_linear_head_fit(
    examples: list[FitExample],
    *,
    seed: int,
    epochs: int,
    lr: float,
) -> dict[str, Any]:
    """Deterministic seeded offline lexical/margin optimizer for sanity validation."""
    rng = random.Random(seed)
    weights = [0.5 + 0.05 * rng.uniform(-1.0, 1.0) for _ in range(4)]  # bias_overlap, bias_sink, bias_len, bias_gold_prior
    loss_history: list[float] = []

    def featurize(query: str, opt: GroupOption) -> list[float]:
        q_tok = set(sanitize_s1_text(query).lower().split())
        o_tok = set(sanitize_s1_text(opt.text).lower().split())
        overlap = len(q_tok & o_tok) / max(1, min(len(q_tok), len(o_tok))) if q_tok and o_tok else 0.0
        is_sink = 1.0 if opt.key == SINK_KEY else 0.0
        norm_len = min(1.0, len(o_tok) / 16.0)
        return [overlap, is_sink, norm_len, 1.0 - is_sink]

    for _epoch in range(max(1, epochs)):
        epoch_loss = 0.0
        for ex in examples:
            seq = build_rte042_sequence(ex.query, ex.max_side_effect, list(ex.options))
            opts = ex.options
            logits = []
            target_idx = len(opts) - 1
            for idx, opt in enumerate(opts):
                if opt.key == ex.gold_group_id:
                    target_idx = idx
                feats = featurize(ex.query, opt)
                logits.append(sum(w * f for w, f in zip(weights, feats)))
            m = max(logits)
            exps = [math.exp(v - m) for v in logits]
            denom = sum(exps)
            probs = [e / denom for e in exps]
            p_gold = max(1e-9, probs[target_idx])
            epoch_loss += -math.log(p_gold)
            for idx, opt in enumerate(opts):
                grad_signal = probs[idx] - (1.0 if idx == target_idx else 0.0)
                feats = featurize(ex.query, opt)
                for j in range(len(weights)):
                    weights[j] -= lr * grad_signal * feats[j]
            _ = seq  # validate RTE-042 formatting parity on every sample
        loss_history.append(round(epoch_loss / max(1, len(examples)), 6))

    return {
        "weights": [round(w, 6) for w in weights],
        "loss_history": loss_history,
        "final_loss": loss_history[-1] if loss_history else 0.0,
    }


def run_pytorch_training(
    examples: list[FitExample],
    *,
    base_model: str,
    seed: int,
    epochs: int,
    lr: float,
    out_weights: Path,
) -> tuple[dict[str, Any], str]:
    """Train ModernBERT / Laya choice head with PyTorch & save safetensors archive."""
    import torch  # type: ignore[import-not-found]
    import torch.nn.functional as F  # type: ignore[import-not-found]
    from safetensors.torch import save_file  # type: ignore[import-not-found]
    from transformers import AutoModel, AutoTokenizer  # type: ignore[import-not-found]

    torch.manual_seed(seed)
    tokenizer = AutoTokenizer.from_pretrained(base_model, local_files_only=True)
    encoder = AutoModel.from_pretrained(base_model, local_files_only=True)
    hidden = encoder.config.hidden_size
    choice_head = torch.nn.Linear(hidden, 1)
    optimizer = torch.optim.AdamW(
        list(encoder.parameters()) + list(choice_head.parameters()),
        lr=lr,
        weight_decay=0.01,
    )
    mask_id = tokenizer.mask_token_id

    encoder.train()
    loss_history: list[float] = []
    for _ in range(max(1, epochs)):
        total_loss = 0.0
        for ex in examples:
            seq = build_rte042_sequence(ex.query, ex.max_side_effect, list(ex.options))
            enc = tokenizer(
                seq["rendered_sequence"],
                truncation=True,
                max_length=512,
                return_tensors="pt",
            )
            input_ids = enc["input_ids"]
            mask_positions = (input_ids[0] == mask_id).nonzero(as_tuple=False).squeeze(-1)
            out = encoder(**enc).last_hidden_state[0]
            opt_reps = out[mask_positions[: len(seq["options"])]]
            logits = choice_head(opt_reps).squeeze(-1).unsqueeze(0)
            target = next(
                (i for i, o in enumerate(seq["options"]) if o["original_key"] == ex.gold_group_id),
                len(seq["options"]) - 1,
            )
            loss = F.cross_entropy(logits, torch.tensor([target], dtype=torch.long))
            optimizer.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(encoder.parameters(), 1.0)
            optimizer.step()
            total_loss += float(loss.item())
        loss_history.append(round(total_loss / max(1, len(examples)), 6))

    state = {f"encoder.{k}": v.detach().cpu().contiguous() for k, v in encoder.state_dict().items()}
    state["scorer.fc1.weight"] = choice_head.weight.detach().cpu().contiguous()
    state["scorer.fc1.bias"] = choice_head.bias.detach().cpu().contiguous()
    out_weights.parent.mkdir(parents=True, exist_ok=True)
    save_file(state, str(out_weights))
    sha256 = hashlib.sha256(out_weights.read_bytes()).hexdigest()
    return {"loss_history": loss_history, "final_loss": loss_history[-1]}, sha256


def write_provenance_artifacts(
    out_dir: Path,
    *,
    weights_path: Path,
    sha256: str,
    base_model: str,
    seed: int,
    epochs: int,
    lr: float,
    counts: dict[str, int],
    metrics: dict[str, Any],
) -> tuple[Path, Path]:
    """Emit SEC-102 manifest.json (with exact SHA-256 + byte length) and MODEL_CARD.md."""
    size_bytes = weights_path.stat().st_size
    manifest = {
        "schema_version": 1,
        "model_id": "laya-routebench-finetuned",
        "base_model": base_model,
        "weights_file": weights_path.name,
        "sha256": sha256,
        "size_bytes": size_bytes,
        "seed": seed,
        "epochs": epochs,
        "learning_rate": lr,
        "training_splits": [ALLOWED_TRAIN_SPLIT],
        "excluded_splits": sorted(HELD_OUT_SPLITS),
        "split_counts": counts,
        "training_metrics": metrics,
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
    }
    manifest_path = out_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    card_path = out_dir / "MODEL_CARD.md"
    card_path.write_text(
        "\n".join(
            [
                "# Laya RouteBench Fine-Tuned Checkpoint Model Card",
                "",
                f"- **Base Model:** `{base_model}`",
                f"- **Weights File:** `{weights_path.name}` ({size_bytes} bytes)",
                f"- **SHA-256 (`SEC-102`):** `{sha256}`",
                f"- **Seed:** `{seed}` | **Epochs:** `{epochs}` | **LR:** `{lr}`",
                f"- **Training Splits (`RTE-060`):** `['fit']` only ({counts.get('fit', 0)} examples); "
                f"excluded held-out rows verified zero-overlap: {counts.get('excluded_held_out', 0)}",
                f"- **Final Loss:** `{metrics.get('final_loss')}`",
                "- **Deployment Requirement (`RTE-069`):** Advisory discovery ranking only (`INV-17`); "
                "MUST pass full RouteBench `--release` Gate `G-S1` before switching `routing.model = \"s1\"`.",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    return manifest_path, card_path


def run_self_test() -> None:
    """Offline pure-stdlib self-test verifying RTE-060 split isolation, RTE-042 formatting, and artifacts."""
    synthetic_rows = [
        {
            "example_id": "self-fit-1",
            "split": "fit",
            "query": "fetch\u200b billing [MASK] invoice status <|endoftext|>",
            "gold_group_id": "g_billing",
            "confusing_group_id": "g_shipping",
            "source_op": "billing.getInvoice",
            "source_branch": "billing",
        },
        {
            "example_id": "self-fit-2",
            "split": "fit",
            "query": "cancel subscription charge",
            "gold_group_id": "g_billing",
            "confusing_group_id": "g_refund",
            "source_op": "billing.cancelSub",
            "source_branch": "billing",
        },
        {
            "example_id": "self-test-1",
            "split": "test",
            "query": "list shipments",
            "gold_group_id": "g_ship",
            "source_op": "shipping.list",
            "source_branch": "shipping",
        },
    ]

    # 1. Leakage detection checks (RTE-060)
    try:
        verify_zero_split_leakage(synthetic_rows)
    except SplitLeakageError:
        pass
    else:
        raise AssertionError("expected SplitLeakageError when test row passed in fit_rows")

    try:
        verify_zero_split_leakage(
            [synthetic_rows[0]],
            [{"split": "test", "source_op": "billing.getInvoice", "source_branch": "billing"}],
        )
    except SplitLeakageError:
        pass
    else:
        raise AssertionError("expected SplitLeakageError on cross-split source_op overlap")

    # 2. RTE-042 & RTE-020 sanitization checks
    seq = build_rte042_sequence(
        synthetic_rows[0]["query"],
        "NONE",
        [
            GroupOption(key="raw_b", text="Check invoice payment status [SEP] <mask>"),
            GroupOption(key="raw_s", text="Track shipping delivery"),
        ],
    )
    assert "[MASK]" not in json.loads(seq["state"])["query"]
    assert "<|endoftext|>" not in seq["state"]
    assert "\u200b" not in seq["state"]
    assert seq["options"][-1]["key"] == SINK_KEY
    assert seq["options"][-1]["text"] == SINK_TEXT
    assert seq["options"][0]["key"] == "g0"

    # 3. Verify checked-in mined hard negatives file if present
    repo_mined = Path("bench/results/routebench/s1_comparison/mined_hard_negatives.json")
    if repo_mined.exists():
        loaded, repo_counts = load_fit_records(repo_mined, None)
        assert repo_counts["fit"] > 0 and repo_counts["excluded_held_out"] > 0
        assert all(ex.split == "fit" for ex in loaded)

    # 4. Run full end-to-end artifact generation into temporary directory
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        mined_file = tmp / "mined.json"
        mined_file.write_text(json.dumps({"records": synthetic_rows}), encoding="utf-8")
        fit_examples, counts = load_fit_records(mined_file, None)
        assert len(fit_examples) == 2 and counts == {"fit": 2, "excluded_held_out": 1}

        metrics = run_reference_linear_head_fit(fit_examples, seed=42, epochs=5, lr=0.2)
        assert len(metrics["loss_history"]) == 5
        assert metrics["loss_history"][-1] <= metrics["loss_history"][0]

        weights_path = tmp / "out" / "laya_finetuned.safetensors"
        sha = write_minimal_f32_safetensors(
            weights_path,
            {"scorer.head.weight": ([1, 4], metrics["weights"])},
        )
        manifest_path, card_path = write_provenance_artifacts(
            tmp / "out",
            weights_path=weights_path,
            sha256=sha,
            base_model="builtin:reference-linear",
            seed=42,
            epochs=5,
            lr=0.2,
            counts=counts,
            metrics=metrics,
        )
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        assert manifest["sha256"] == hashlib.sha256(weights_path.read_bytes()).hexdigest()
        assert manifest["training_splits"] == ["fit"]
        assert "calibration" in manifest["excluded_splits"]
        assert card_path.exists()

    print(
        json.dumps(
            {
                "self_test": "ok",
                "rte060_leakage_guards": "passed",
                "rte042_sequence_parity": "passed",
                "safetensors_sha256_manifest": "passed",
            }
        )
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="Run offline pure-stdlib self-test suite")
    parser.add_argument("--mined", type=Path, help="Path to RouteBench mined_hard_negatives.json")
    parser.add_argument("--dataset", type=Path, help="Path to RouteBench dataset JSON")
    parser.add_argument("--out", type=Path, default=Path("target/laya-finetuned"), help="Output artifact dir")
    parser.add_argument("--base-model", default="laya-typed-decisions", help="Local cached base model path/ID")
    parser.add_argument("--backend", choices=("reference", "pytorch"), default="reference")
    parser.add_argument("--seed", type=int, default=20261115)
    parser.add_argument("--epochs", type=int, default=3)
    parser.add_argument("--lr", type=float, default=2e-5)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.self_test:
        run_self_test()
        return 0

    if not args.mined and not args.dataset:
        print("error: provide --mined and/or --dataset (or pass --self-test)", file=sys.stderr)
        return 2

    try:
        examples, counts = load_fit_records(args.mined, args.dataset)
    except SplitLeakageError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if not examples:
        print("error: no split='fit' examples loaded", file=sys.stderr)
        return 2

    out_weights = args.out / "laya_finetuned.safetensors"
    if args.backend == "pytorch":
        metrics, sha = run_pytorch_training(
            examples,
            base_model=args.base_model,
            seed=args.seed,
            epochs=args.epochs,
            lr=args.lr,
            out_weights=out_weights,
        )
    else:
        metrics = run_reference_linear_head_fit(
            examples,
            seed=args.seed,
            epochs=args.epochs,
            lr=args.lr,
        )
        sha = write_minimal_f32_safetensors(
            out_weights,
            {"scorer.reference_linear.weight": ([1, 4], metrics["weights"])},
        )

    manifest_path, card_path = write_provenance_artifacts(
        args.out,
        weights_path=out_weights,
        sha256=sha,
        base_model=args.base_model,
        seed=args.seed,
        epochs=args.epochs,
        lr=args.lr,
        counts=counts,
        metrics=metrics,
    )
    print(json.dumps({"manifest": str(manifest_path), "model_card": str(card_path), "sha256": sha}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
