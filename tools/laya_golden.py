#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Generate >=200 golden (state, question, options) -> logits test fixtures for Laya parity (WP-M5-01).

Usage:
  # Deterministic offline reference fixture generation (default):
  python3 tools/laya_golden.py --out tests/fixtures/laya_golden/cases.json --count 210

  # Online PyTorch / OpenJev /v1/systemone parity regeneration when weights are served locally:
  uv run tools/laya_golden.py --reference-endpoint http://127.0.0.1:8080/v1/systemone
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import urllib.request
from pathlib import Path

SINK_KEY = "insufficient_evidence"
SINK_TEXT = "None of the other options can fulfil the request."

DOMAINS = [
    ("billing", "Look up customer invoices, payment status, refunds, and subscription charges", "read invoice status for account"),
    ("crm", "Retrieve contact records, account owners, deals, and support interaction history", "find customer profile details"),
    ("inventory", "Check warehouse stock levels, SKU availability, backorders, and reorder thresholds", "check stock quantity for SKU"),
    ("shipping", "Track parcel shipments, carrier events, delivery estimates, and customs holds", "track parcel delivery status"),
    ("catalog", "Search product titles, categories, pricing tiers, and attribute specifications", "search product catalog by attributes"),
    ("hr", "Inspect employee directory, department rosters, leave balances, and shift schedules", "check team leave calendar"),
    ("analytics", "Query daily active users, retention cohorts, funnel conversion, and revenue rollups", "compute weekly conversion metrics"),
    ("ticketing", "List open support tickets, SLA timers, escalation notes, and assignee queues", "list high priority support incidents"),
    ("identity", "Verify user role bindings, OAuth client metadata, audit sessions, and group membership", "inspect active session scopes"),
    ("documents", "Traverse indexed design documents, API changelogs, runbooks, and ownership edges", "find architecture runbook for service"),
    ("calendar", "Fetch room availability, participant schedules, recurring events, and timezone slots", "find open meeting room tomorrow"),
    ("payments", "Authorize settlement batches, reconcile ledger payouts, and inspect chargebacks", "reconcile settlement batch ledger"),
    ("notifications", "Dispatch transactional templates, inspect bounce logs, and query webhook deliveries", "check webhook delivery failures"),
    ("compliance", "Audit retention policies, PII classification tags, and consent ledger entries", "check data retention rules"),
    ("observability", "Query service latency quantiles, circuit breaker trips, and error budget burn rates", "inspect service p99 error budget"),
]

UNICODE_QUERIES = [
    "tìm hóa đơn chưa thanh toán của khách hàng",
    "顧客の請求書ステータスを確認する",
    "［ｑｕｅｒｙ］ verify café shipment status — priority №1",
    "überprüfen Sie Lagerbestand für Artikel α-42",
    "réconcilier le paiement pour commande numéro 981",
]


def jcs_state(query: str, max_side_effect: str) -> str:
    """Serialize JCS-canonical state JSON (keys sorted alphabetically, no extra whitespace; RTE-042)."""
    obj = {
        "constraints": {"max_side_effect": max_side_effect},
        "query": query,
    }
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def bucket_for_option_count(n_options: int) -> str:
    if n_options == 2:
        return "2"
    if 3 <= n_options <= 5:
        return "3-5"
    if 6 <= n_options <= 10:
        return "6-10"
    return "11-16"


def softmax(logits: list[float]) -> list[float]:
    m = max(logits)
    exps = [math.exp(x - m) for x in logits]
    s = sum(exps)
    return [round(v / s, 8) for v in exps]


def token_set(text: str) -> set[str]:
    cleaned = "".join(ch.lower() if ch.isalnum() else " " for ch in text)
    return {tok for tok in cleaned.split() if len(tok) >= 2}


def stable_hash_unit(key: str) -> float:
    digest = hashlib.sha256(key.encode("utf-8")).digest()
    val = int.from_bytes(digest[:8], "big")
    return (val / float(2**64 - 1)) * 2.0 - 1.0


def reference_forward(case_id: str, state: str, question: str, options: list[dict[str, str]], scenario: str) -> tuple[list[float], list[float], list[float]]:
    """Deterministic seeded typed-decision reference scorer modeling choice/score/noul heads."""
    parsed_state = json.loads(state)
    query_text = parsed_state["query"]
    q_tokens = token_set(query_text)

    choice_logits: list[float] = []
    for idx, opt in enumerate(options):
        if opt["key"] == SINK_KEY:
            base = 2.6 if scenario == "ood_sink" else (-1.8 if scenario != "near_tie" else -0.4)
            noise = 0.08 * stable_hash_unit(f"{case_id}:sink")
            choice_logits.append(round(base + noise, 6))
            continue

        o_tokens = token_set(opt["text"])
        overlap = len(q_tokens & o_tokens) / max(1, min(len(q_tokens), len(o_tokens)))
        if scenario == "ood_sink":
            signal = -1.1 + 0.3 * overlap
        elif scenario == "near_tie" and idx in (0, 1):
            signal = 1.95 + (0.015 if idx == 0 else 0.0) + 0.01 * stable_hash_unit(f"{case_id}:{idx}")
        elif idx == 0:
            signal = 2.4 + 1.8 * overlap
        else:
            signal = -0.7 + 1.2 * overlap - 0.08 * idx
        jitter = 0.06 * stable_hash_unit(f"{case_id}:{question}:{opt['key']}")
        choice_logits.append(round(signal + jitter, 6))

    # 5-class rubric score logits [1..5]
    score_center = 1.2 if scenario == "ood_sink" else (3.1 if scenario == "near_tie" else 4.4)
    score_logits = [
        round(-0.4 * ((lvl - score_center) ** 2) + 0.04 * stable_hash_unit(f"{case_id}:s:{lvl}"), 6)
        for lvl in range(1, 6)
    ]

    # 4-head NoUL logits [supported, needs_clarification, unsafe_side_effect, out_of_scope]
    is_mutating = parsed_state["constraints"]["max_side_effect"] != "NONE"
    if scenario == "ood_sink":
        noul_raw = [-1.4, 0.3, -1.2, 2.5]
    elif scenario == "near_tie":
        noul_raw = [1.1, 1.9, -1.0, -0.8]
    else:
        noul_raw = [2.6, -0.6, 0.9 if is_mutating else -1.8, -2.1]
    noul_logits = [
        round(val + 0.05 * stable_hash_unit(f"{case_id}:n:{i}"), 6)
        for i, val in enumerate(noul_raw)
    ]
    return choice_logits, score_logits, noul_logits


def query_reference_endpoint(endpoint: str, state: str, question: str, options: list[dict[str, str]]) -> tuple[list[float], list[float], list[float]]:
    payload = json.dumps({
        "state": state,
        "question": question,
        "options": options,
    }).encode("utf-8")
    req = urllib.request.Request(
        endpoint,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    return data["choice_logits"], data["score_logits"], data["noul_logits"]


def build_cases(count: int, reference_endpoint: str | None) -> list[dict]:
    cases: list[dict] = []
    side_effects = ["NONE", "REVERSIBLE", "IRREVERSIBLE"]
    for idx in range(count):
        case_id = f"laya-golden-{idx + 1:04d}"
        # Span K = 1..15 capability groups (2..16 options including sink).
        k_groups = (idx % 15) + 1
        scenario = (
            "ood_sink"
            if idx % 7 == 0
            else ("near_tie" if idx % 5 == 0 and k_groups >= 2 else ("unicode" if idx % 11 == 0 else "match"))
        )
        max_side_effect = side_effects[idx % len(side_effects)]
        primary_domain = DOMAINS[idx % len(DOMAINS)]

        if scenario == "ood_sink":
            query = f"book private orbital launch rocket charter #{idx}"
        elif scenario == "unicode":
            query = UNICODE_QUERIES[(idx // 11) % len(UNICODE_QUERIES)]
        else:
            query = f"{primary_domain[2]} (case {idx + 1})"

        state = jcs_state(query, max_side_effect)
        question = "Select the single capability group whose schema operations fulfill the query."

        options: list[dict[str, str]] = []
        for g_idx in range(k_groups):
            dom = DOMAINS[(idx + g_idx) % len(DOMAINS)]
            suffix = " (variant B)" if (scenario == "near_tie" and g_idx == 1) else ""
            opt_text = f"{dom[1]}{suffix}"
            if scenario == "near_tie" and g_idx == 1:
                opt_text = f"{primary_domain[1]} (alternate replica view)"
            options.append({"key": f"g{g_idx}", "text": opt_text})
        options.append({"key": SINK_KEY, "text": SINK_TEXT})

        if reference_endpoint:
            choice_logits, score_logits, noul_logits = query_reference_endpoint(
                reference_endpoint, state, question, options
            )
        else:
            choice_logits, score_logits, noul_logits = reference_forward(
                case_id, state, question, options, scenario
            )

        probs = softmax(choice_logits)
        ranked_pairs = sorted(zip(probs, [o["key"] for o in options]), key=lambda x: (-x[0], x[1]))
        argmax_key = ranked_pairs[0][1]
        top2_margin = round(ranked_pairs[0][0] - ranked_pairs[1][0], 8) if len(ranked_pairs) >= 2 else 1.0

        cases.append({
            "case_id": case_id,
            "scenario": scenario,
            "bucket": bucket_for_option_count(len(options)),
            "state": state,
            "question": question,
            "options": options,
            "choice_logits": choice_logits,
            "score_logits": score_logits,
            "noul_logits": noul_logits,
            "probabilities": probs,
            "argmax_key": argmax_key,
            "top2_margin": top2_margin,
        })
    return cases


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate Laya golden logits fixtures.")
    parser.add_argument("--out", default="tests/fixtures/laya_golden/cases.json")
    parser.add_argument("--count", type=int, default=210)
    parser.add_argument("--reference-endpoint", default=None)
    args = parser.parse_args()

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    cases = build_cases(args.count, args.reference_endpoint)
    out_path.write_text(json.dumps(cases, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    buckets: dict[str, int] = {}
    for c in cases:
        buckets[c["bucket"]] = buckets.get(c["bucket"], 0) + 1
    print(f"Wrote {len(cases)} golden cases to {out_path} (buckets={dict(sorted(buckets.items()))})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
