#!/usr/bin/env python3
"""ASD-STE100 ("aircraft maintenance manual") compliance linter for docs/ste100/*.md."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STE_DIR = ROOT / "docs" / "ste100"

APPROVED_IMPERATIVES = {
    "If",
    "Build",
    "Run",
    "Start",
    "Stop",
    "Check",
    "Verify",
    "Open",
    "Send",
    "Read",
    "Write",
    "Create",
    "Copy",
    "Set",
    "Approve",
    "Reject",
    "Roll",
    "Export",
    "Restore",
    "Inspect",
    "Connect",
    "Configure",
    "Wait",
    "Kill",
    "Reload",
    "Publish",
    "Delete",
    "Compare",
    "Pass",
    "Point",
    "Select",
}

BANNED_PHRASES = [
    "utilize",
    "utilizes",
    "utilized",
    "leverage",
    "leverages",
    "leveraged",
    "facilitate",
    "facilitates",
    "commence",
    "prior to",
    "subsequent to",
    "in order to",
    "ensure",
    "ensures",
    "seamless",
    "seamlessly",
    "robust",
    "holistic",
    "delve",
    "tapestry",
    "paradigm",
    "synergy",
    "cutting-edge",
    "moreover",
    "furthermore",
    "nevertheless",
    "it is important to note",
    "as previously mentioned",
    "effortlessly",
]

PASSIVE_RE = re.compile(r"\b(?:is|are|was|were|be|been|being)\s+([a-z]+ed)\b", re.IGNORECASE)
BANNED_RE = re.compile(
    r"\b(?:" + "|".join(re.escape(p) for p in sorted(BANNED_PHRASES, key=len, reverse=True)) + r")\b",
    re.IGNORECASE,
)


def mask_inline_code_and_links(text: str) -> tuple[str, list[str]]:
    """Replace inline code spans with CODETOKEN (1 atomic word) and strip link URLs."""
    text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
    tokens: list[str] = []

    def _repl(m: re.Match[str]) -> str:
        tokens.append(m.group(0))
        return "CODETOKEN"

    masked = re.sub(r"`[^`]+`", _repl, text)
    return masked, tokens


def split_sentences(masked_text: str) -> list[str]:
    """Split English prose into sentences without breaking decimals like 95.24 or 1.20."""
    decimal_masked = re.sub(r"(\d)\.(\d)", r"\1DOTDEC\2", masked_text)
    parts = re.split(r"(?<=[.!?])\s+(?=[A-Z0-9*\"'(]|CODETOKEN)", decimal_masked.strip())
    return [p.replace("DOTDEC", ".").strip() for p in parts if p.strip()]


def count_words(sentence: str) -> int:
    clean = re.sub(r"\*\*([^*]+)\*\*", r"\1", sentence)
    clean = re.sub(r"\*([^*]+)\*", r"\1", clean)
    words = re.findall(r"[A-Za-z0-9_'-]+", clean)
    return len(words)


def audit_file(path: Path) -> tuple[list[str], int, int, int]:
    errors: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    mode = "descriptive"
    in_fence = False
    para_lines: list[tuple[int, str, bool, bool]] = []  # (line_no, text, is_step, is_callout)

    total_sentences = 0
    max_desc = 0
    max_proc = 0

    def flush_paragraph() -> None:
        nonlocal total_sentences, max_desc, max_proc
        if not para_lines:
            return
        joined = " ".join(x[1] for x in para_lines)
        start_line = para_lines[0][0]
        is_step = para_lines[0][2]
        is_callout = para_lines[0][3]
        masked, _ = mask_inline_code_and_links(joined)
        sentences = split_sentences(masked)

        if len(sentences) > 6:
            errors.append(
                f"{path.name}:{start_line}: paragraph has {len(sentences)} sentences (max 6)"
            )

        if mode == "procedural" and is_step and sentences:
            first_plain = re.sub(r"^\*\*|\*\*$", "", sentences[0].lstrip())
            first_word = re.findall(r"[A-Za-z]+", first_plain)
            verb = first_word[0] if first_word else ""
            if verb not in APPROVED_IMPERATIVES:
                errors.append(
                    f"{path.name}:{start_line}: procedural step must start with approved imperative verb or 'If', found `{verb}`"
                )

        limit = 25 if (mode != "procedural" or is_callout) else 20
        for sent in sentences:
            wc = count_words(sent)
            total_sentences += 1
            if mode == "procedural" and not is_callout:
                max_proc = max(max_proc, wc)
            else:
                max_desc = max(max_desc, wc)

            if wc > limit:
                errors.append(
                    f"{path.name}:{start_line}: {mode} sentence has {wc} words (max {limit}): `{sent}`"
                )
            b_match = BANNED_RE.search(sent)
            if b_match:
                errors.append(
                    f"{path.name}:{start_line}: banned filler/unapproved term `{b_match.group(0)}` in `{sent}`"
                )
            p_match = PASSIVE_RE.search(sent)
            if p_match:
                errors.append(
                    f"{path.name}:{start_line}: passive voice `{p_match.group(0)}` in `{sent}`"
                )

        para_lines.clear()

    for idx, raw in enumerate(lines, start=1):
        stripped = raw.strip()
        if stripped.startswith("```"):
            flush_paragraph()
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        if not stripped or stripped == "---":
            flush_paragraph()
            continue
        if stripped.startswith("#"):
            flush_paragraph()
            upper = stripped.upper()
            if "[PROCEDURAL]" in upper:
                mode = "procedural"
            elif "[DESCRIPTIVE]" in upper or "[REFERENCE]" in upper:
                mode = "descriptive"
            continue
        if stripped.startswith("|"):
            flush_paragraph()
            continue
        if stripped.startswith(">"):
            body = re.sub(r"^>\s*", "", stripped)
            if re.match(r"^\*\*(WARNING|CAUTION|NOTE):\*\*", body):
                flush_paragraph()
            para_lines.append((idx, body, False, True))
            continue

        step_m = re.match(r"^\d+\.\s+(.*)$", stripped)
        bullet_m = re.match(r"^[-*]\s+(.*)$", stripped)
        if step_m:
            flush_paragraph()
            para_lines.append((idx, step_m.group(1), True, False))
            continue
        if bullet_m:
            flush_paragraph()
            para_lines.append((idx, bullet_m.group(1), False, False))
            continue
        para_lines.append((idx, stripped, False, False))

    flush_paragraph()
    return errors, total_sentences, max_desc, max_proc


def main() -> int:
    files = sorted(STE_DIR.glob("*.md"))
    if not files:
        print(f"No markdown files found in {STE_DIR}", file=sys.stderr)
        return 1

    all_errors: list[str] = []
    grand_sentences = 0
    overall_max_desc = 0
    overall_max_proc = 0

    for f in files:
        errs, cnt, md, mp = audit_file(f)
        all_errors.extend(errs)
        grand_sentences += cnt
        overall_max_desc = max(overall_max_desc, md)
        overall_max_proc = max(overall_max_proc, mp)
        print(
            f"CHECK {f.relative_to(ROOT)}: sentences={cnt}, max_desc={md}/25, max_proc={mp}/20, violations={len(errs)}"
        )

    if all_errors:
        print("\nASD-STE100 VIOLATIONS:", file=sys.stderr)
        for err in all_errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    print(
        f"\nOK: ASD-STE100 score 100/100 ({len(files)} files, {grand_sentences} sentences, "
        f"max descriptive={overall_max_desc}/25 words, max procedural={overall_max_proc}/20 words, "
        "0 banned terms, 0 passive forms)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
