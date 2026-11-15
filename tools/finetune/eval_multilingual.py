#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Multilingual discovery routing & translate-at-edge evaluator (WP-M5-09).

Evaluates Vietnamese (accented, unaccented/ASCII-folded, code-switched EN+VI)
and multilingual (fr, de, ja, en, ood) intent queries against English GAP capability
group descriptions under three normalization strategies:
  1. `raw_lexical`      - raw UTF-8 lexical matching without normalization or translation
  2. `diacritic_fold`   - NFKD Latin diacritic folding + Vietnamese d-stroke normalization
  3. `translate_at_edge`- simulated MCP host-edge English normalization (ADR-005 / ADR-004)

Run standalone report:
  python3 tools/finetune/eval_multilingual.py --json

Run deterministic offline self-test:
  python3 tools/finetune/eval_multilingual.py --self-test
"""
from __future__ import annotations

import argparse
import json
import sys
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Any

SINK_KEY = "insufficient_evidence"

CAPABILITY_GROUPS: tuple[tuple[str, str], ...] = (
    ("g_billing", "Look up customer invoices, unpaid balances, payment status, refunds, and charges"),
    ("g_shipping", "Track parcel shipments, carrier delivery estimates, tracking numbers, and customs"),
    ("g_inventory", "Check warehouse stock levels, SKU availability, backorders, and reorder thresholds"),
    ("g_issues", "List GitHub repository issues, pull requests, commit history, and review status"),
    ("g_users", "Inspect organization member roles, active sessions, audit permissions, and OAuth tokens"),
)


@dataclass(frozen=True)
class MultilingualCase:
    case_id: str
    slice_name: str
    raw_query: str
    edge_translated_en: str
    gold_group: str


EVAL_CORPUS: tuple[MultilingualCase, ...] = (
    # English anchor control cases
    MultilingualCase("en-1", "en_control", "check unpaid customer invoices", "check unpaid customer invoices", "g_billing"),
    MultilingualCase("en-2", "en_control", "track shipping parcel status", "track shipping parcel status", "g_shipping"),
    MultilingualCase("en-3", "en_control", "check warehouse stock for SKU-42", "check warehouse stock for SKU-42", "g_inventory"),
    # Vietnamese accented queries
    MultilingualCase("vi-acc-1", "vi_accented", "kiểm tra hóa đơn chưa thanh toán của khách hàng", "check unpaid customer invoices", "g_billing"),
    MultilingualCase("vi-acc-2", "vi_accented", "theo dõi trạng thái giao hàng bưu kiện", "track shipping parcel delivery status", "g_shipping"),
    MultilingualCase("vi-acc-3", "vi_accented", "xem tồn kho trong kho cho mã sản phẩm SKU-42", "check warehouse stock levels for SKU-42", "g_inventory"),
    MultilingualCase("vi-acc-4", "vi_accented", "liệt kê các vấn đề và yêu cầu kéo trên kho mã", "list repository issues and pull requests", "g_issues"),
    MultilingualCase("vi-acc-5", "vi_accented", "kiểm tra vai trò thành viên và phiên đăng nhập", "inspect member roles and active sessions", "g_users"),
    # Vietnamese unaccented queries
    MultilingualCase("vi-un-1", "vi_unaccented", "kiem tra hoa don chua thanh toan khach hang", "check unpaid customer invoices", "g_billing"),
    MultilingualCase("vi-un-2", "vi_unaccented", "theo doi trang thai giao hang buu kien", "track shipping parcel delivery status", "g_shipping"),
    MultilingualCase("vi-un-3", "vi_unaccented", "kiem tra ton kho san pham SKU-42", "check warehouse stock for SKU-42", "g_inventory"),
    MultilingualCase("vi-un-4", "vi_unaccented", "xem quyen va phien hoat dong thanh vien", "inspect member roles and active sessions", "g_users"),
    # Vietnamese-English code-switched queries retaining identifiers
    MultilingualCase("vi-cs-1", "vi_code_switched", "check invoice trạng thái refund của customer", "check customer invoice refund payment status", "g_billing"),
    MultilingualCase("vi-cs-2", "vi_code_switched", "xem GitHub pull requests và issues đang mở", "list GitHub pull requests and repository issues", "g_issues"),
    MultilingualCase("vi-cs-3", "vi_code_switched", "kiểm tra OAuth tokens và member roles", "inspect member roles and OAuth tokens", "g_users"),
    MultilingualCase("vi-cs-4", "vi_code_switched", "track parcel carrier customs holding SKU-99", "track parcel shipment carrier customs status", "g_shipping"),
    # Multilingual European & CJK queries
    MultilingualCase("fr-1", "multilingual_other", "vérifier le remboursement de la facture client", "check customer invoice refund status", "g_billing"),
    MultilingualCase("de-1", "multilingual_other", "Lagerbestand im Lagerhaus für SKU-42 prüfen", "check warehouse stock levels for SKU-42", "g_inventory"),
    MultilingualCase("ja-1", "multilingual_other", "顧客の請求書と支払い状況を確認する", "check customer invoice and payment status", "g_billing"),
    MultilingualCase("ja-2", "multilingual_other", "荷物の配送状況と追跡番号を調べる", "track parcel delivery and tracking numbers", "g_shipping"),
    # Cross-language OOD abstention checks
    MultilingualCase("ood-vi", "ood_abstention", "đặt vé máy bay đi Đà Nẵng vào cuối tuần", "book weekend domestic airline flight tickets", SINK_KEY),
    MultilingualCase("ood-ja", "ood_abstention", "週末のフライトチケットを予約する", "book weekend airline flight tickets", SINK_KEY),
)


def strip_diacritics(text: str) -> str:
    nfkd = unicodedata.normalize("NFKD", text)
    chars = []
    for ch in nfkd:
        if unicodedata.combining(ch):
            continue
        if ch == "đ":
            chars.append("d")
        elif ch == "Đ":
            chars.append("D")
        else:
            chars.append(ch)
    return "".join(chars)


def tokenize(text: str, *, fold: bool) -> set[str]:
    norm = strip_diacritics(text) if fold else unicodedata.normalize("NFKC", text)
    cleaned = "".join(ch.lower() if ch.isalnum() else " " for ch in norm)
    return {tok for tok in cleaned.split() if len(tok) >= 2}


def rank_groups(query_text: str, *, fold: bool) -> list[tuple[str, float]]:
    q_tok = tokenize(query_text, fold=fold)
    scored: list[tuple[str, float]] = []
    for gid, desc in CAPABILITY_GROUPS:
        g_tok = tokenize(desc, fold=fold)
        overlap = len(q_tok & g_tok) / max(1.0, len(q_tok) ** 0.5 * len(g_tok) ** 0.5)
        scored.append((gid, overlap))
    # Abstain when no group shows meaningful lexical overlap (threshold tuned cleanly for OOD vs match)
    max_overlap = max(score for _, score in scored)
    sink_score = 0.08 if max_overlap < 0.08 else -1.0
    scored.append((SINK_KEY, sink_score))
    scored.sort(key=lambda item: (-item[1], item[0]))
    return scored


def evaluate_corpus(corpus: tuple[MultilingualCase, ...] = EVAL_CORPUS) -> dict[str, Any]:
    modes = ("raw_lexical", "diacritic_fold", "translate_at_edge")
    summary: dict[str, Any] = {"total_cases": len(corpus), "modes": {}}

    for mode in modes:
        per_slice: dict[str, dict[str, float | int]] = {}
        correct_total = 0
        mrr_total = 0.0
        for case in corpus:
            if mode == "raw_lexical":
                q = case.raw_query
                fold = False
            elif mode == "diacritic_fold":
                q = case.raw_query
                fold = True
            else:
                q = case.edge_translated_en
                fold = True

            ranked = rank_groups(q, fold=fold)
            pred_ids = [gid for gid, _ in ranked]
            top1_ok = 1 if pred_ids[0] == case.gold_group else 0
            rr = 1.0 / (pred_ids.index(case.gold_group) + 1)
            correct_total += top1_ok
            mrr_total += rr

            bucket = per_slice.setdefault(case.slice_name, {"n": 0, "correct": 0, "rr_sum": 0.0})
            bucket["n"] = int(bucket["n"]) + 1
            bucket["correct"] = int(bucket["correct"]) + top1_ok
            bucket["rr_sum"] = float(bucket["rr_sum"]) + rr

        slice_metrics = {
            name: {
                "n": int(vals["n"]),
                "top1": round(int(vals["correct"]) / int(vals["n"]), 4),
                "mrr": round(float(vals["rr_sum"]) / int(vals["n"]), 4),
            }
            for name, vals in sorted(per_slice.items())
        }
        summary["modes"][mode] = {
            "overall_top1": round(correct_total / len(corpus), 4),
            "overall_mrr": round(mrr_total / len(corpus), 4),
            "slices": slice_metrics,
        }

    return summary


def run_self_test() -> None:
    report = evaluate_corpus()
    raw = report["modes"]["raw_lexical"]
    fold = report["modes"]["diacritic_fold"]
    edge = report["modes"]["translate_at_edge"]

    assert edge["overall_top1"] > raw["overall_top1"], "expected translate_at_edge to beat raw_lexical overall"
    assert edge["slices"]["vi_accented"]["top1"] > raw["slices"]["vi_accented"]["top1"]
    assert edge["slices"]["vi_unaccented"]["top1"] > fold["slices"]["vi_unaccented"]["top1"]
    assert edge["slices"]["ood_abstention"]["top1"] == 1.0
    assert raw["slices"]["vi_code_switched"]["top1"] >= raw["slices"]["vi_accented"]["top1"]
    print(json.dumps({"self_test": "ok", "report": report}, indent=2))


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="Run deterministic offline self-test")
    parser.add_argument("--json", action="store_true", help="Print full JSON evaluation summary")
    parser.add_argument("--out", type=Path, help="Optional path to write JSON summary")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.self_test:
        run_self_test()
        return 0

    report = evaluate_corpus()
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"Multilingual Evaluation ({report['total_cases']} cases):")
        for mode, stats in report["modes"].items():
            print(
                f"  - {mode:18s} Top-1={stats['overall_top1']:.4f} MRR={stats['overall_mrr']:.4f} "
                f"(vi_acc={stats['slices']['vi_accented']['top1']:.2f}, "
                f"vi_cs={stats['slices']['vi_code_switched']['top1']:.2f}, "
                f"ood={stats['slices']['ood_abstention']['top1']:.2f})"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
