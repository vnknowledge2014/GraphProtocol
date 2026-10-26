#!/usr/bin/env python3
"""Spec-to-test traceability checker for GAP (WP-M0-07; spec: ARC-004, ADR-022, VER-001).

Usage:
  python3 tools/spec_trace.py                        # default (--warn until M1 exit)
  python3 tools/spec_trace.py --warn                 # report issues, exit 0
  python3 tools/spec_trace.py --strict               # fail (exit 1) on uncovered MUSTs or unknown IDs
  python3 tools/spec_trace.py --strict --milestone M1  # limit MUST check to M1 WP spec_refs

Extracts requirement IDs from spec/*.md:
  - Bold requirement bullets/headers: **ARC-001**, **RTE-040 (title).**, etc.
  - Table-defined invariants and threats: | INV-01 | ... | and | TM-01 | ... |
  - A requirement is classified as MUST iff its text contains uppercase MUST / MUST NOT
    (RFC 2119 / RFC 8174) and is not marked (withdrawn).

Scans crates/**, adapters/**, tests/**, conformance/** for:
  - Comment / attribute markers: `spec: <ID>` (including comma/semicolon/space lists)
  - Conformance JSON fixture fields: `"spec": ["GQL-021", ...]`
"""
from __future__ import annotations

import argparse
import bisect
import collections
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import sys
import tomllib
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
SCAN_DIRS = ("crates", "adapters", "tests", "conformance")
SKIP_DIRS = {"target", "node_modules", "__pycache__", ".git"}

# Technical tokens that match [A-Z]{2,3}-\d{2,3} in prose if bolded, never GAP spec areas.
NON_REQ_PREFIXES = frozenset(
    {"SHA", "AES", "UTF", "RSA", "TLS", "DNS", "TCP", "UDP", "CPU", "GPU", "RAM", "HTTP"}
)

FENCE_OPEN_RE = re.compile(r"^\s*(`{3,}|~{3,})(.*)$")
HEADING_RE = re.compile(r"^\s*#{1,6}\s")
HR_RE = re.compile(r"^\s*(?:-{3,}|\*{3,}|_{3,})\s*$")
LIST_ITEM_RE = re.compile(r"^(?P<indent>[ \t]*)(?:[-*+]|\d+[.)])\s+(?P<rest>.*)$")
TABLE_SEP_RE = re.compile(r"^\s*\|[\s:-]+(?:\|[\s:-]+)*\|?\s*$")
TABLE_REQ_RE = re.compile(r"^\s*\|\s*`?((?:INV|TM)-\d{2,3})`?\s*\|(.*)$")
BOLD_REQ_RE = re.compile(r"\*\*([A-Z]{2,3})-(\d{2,3})\b(?:(?!\*\*)[^\n])*\*\*")
LEADING_BOLD_REQ_RE = re.compile(r"^\s*\*\*([A-Z]{2,3})-(\d{2,3})\b(?:(?!\*\*)[^\n])*\*\*")

MUST_RE = re.compile(r"\bMUST(?:\s+NOT)?\b")
WITHDRAWN_RE = re.compile(r"\(withdrawn\)", re.IGNORECASE)

SPEC_MARKER_RE = re.compile(
    r"\bspec\s*:\s*([A-Z]{2,6}-\d{1,5}(?:\s*[,;/\s]\s*[A-Z]{2,6}-\d{1,5})*)"
)
MARKER_TOKEN_RE = re.compile(r"[A-Z]{2,6}-\d{1,5}")
MARKER_ID_FULL_RE = re.compile(r"^[A-Z]{2,6}-\d{1,5}$")


@dataclass(frozen=True)
class Requirement:
    id: str
    area: str
    file: str
    line: int
    is_must: bool
    withdrawn: bool
    text: str


@dataclass(frozen=True)
class MarkerHit:
    id: str
    file: str
    line: int


@dataclass(frozen=True)
class TraceReport:
    all_requirements: dict[str, Requirement]
    scoped_requirements: dict[str, Requirement]
    markers_by_id: dict[str, list[MarkerHit]]
    uncovered_musts: list[Requirement]
    untested_requirements: list[Requirement]
    unknown_markers: dict[str, list[MarkerHit]]
    milestone: str | None


def _id_sort_key(req_id: str) -> tuple[str, int, str]:
    if "-" in req_id:
        area, rest = req_id.split("-", 1)
        if rest.isdigit():
            return (area, int(rest), req_id)
    return (req_id, 0, req_id)


def _indent_width(line: str) -> int:
    expanded = line.expandtabs(4)
    return len(expanded) - len(expanded.lstrip(" "))


def _has_bold_req(line: str) -> bool:
    return any(m.group(1) not in NON_REQ_PREFIXES for m in BOLD_REQ_RE.finditer(line))


def _record_requirement(
    store: dict[str, Requirement],
    req_id: str,
    rel_file: str,
    line: int,
    text: str,
) -> None:
    area = req_id.split("-", 1)[0]
    cleaned = text.strip()
    withdrawn = bool(WITHDRAWN_RE.search(cleaned))
    is_must = (not withdrawn) and bool(MUST_RE.search(cleaned))
    existing = store.get(req_id)
    if existing is None:
        store[req_id] = Requirement(
            id=req_id,
            area=area,
            file=rel_file,
            line=line,
            is_must=is_must,
            withdrawn=withdrawn,
            text=cleaned,
        )
    else:
        merged_withdrawn = existing.withdrawn and withdrawn
        merged_must = (not merged_withdrawn) and (existing.is_must or is_must)
        store[req_id] = Requirement(
            id=req_id,
            area=existing.area,
            file=existing.file,
            line=existing.line,
            is_must=merged_must,
            withdrawn=merged_withdrawn,
            text=f"{existing.text}\n{cleaned}",
        )


def _extract_bold_from_block(
    store: dict[str, Requirement],
    rel_file: str,
    block_lines: list[tuple[int, str]],
) -> None:
    if not block_lines:
        return
    offsets: list[int] = []
    pos = 0
    for _, text in block_lines:
        offsets.append(pos)
        pos += len(text) + 1
    full_text = "\n".join(text for _, text in block_lines)
    matches = [m for m in BOLD_REQ_RE.finditer(full_text) if m.group(1) not in NON_REQ_PREFIXES]
    for idx, match in enumerate(matches):
        req_id = f"{match.group(1)}-{match.group(2)}"
        seg_start = match.start()
        seg_end = matches[idx + 1].start() if idx + 1 < len(matches) else len(full_text)
        line_idx = bisect.bisect_right(offsets, seg_start) - 1
        lineno = block_lines[max(0, line_idx)][0]
        _record_requirement(store, req_id, rel_file, lineno, full_text[seg_start:seg_end])


def _strip_fenced_lines(text: str) -> list[tuple[int, str]]:
    out: list[tuple[int, str]] = []
    active_fence: str | None = None
    for lineno, raw_line in enumerate(text.splitlines(), start=1):
        if active_fence is None:
            m = FENCE_OPEN_RE.match(raw_line)
            if m:
                fence_chars, info = m.group(1), m.group(2)
                if not (fence_chars[0] == "`" and "`" in info):
                    active_fence = fence_chars
                    continue
            out.append((lineno, raw_line))
        else:
            close_pat = rf"^\s*{re.escape(active_fence[0])}{{{len(active_fence)},}}\s*$"
            if re.match(close_pat, raw_line):
                active_fence = None
    return out


def extract_requirements(root: Path) -> dict[str, Requirement]:
    """Extract requirement IDs and MUST classifications from root/spec/*.md."""
    spec_dir = root / "spec"
    if not spec_dir.is_dir():
        raise FileNotFoundError(f"spec directory not found: {spec_dir}")

    raw_store: dict[str, Requirement] = {}
    for spec_path in sorted(spec_dir.glob("*.md")):
        rel_file = spec_path.relative_to(root).as_posix()
        lines = _strip_fenced_lines(spec_path.read_text(encoding="utf-8"))
        idx = 0
        total = len(lines)
        while idx < total:
            lineno, line = lines[idx]
            stripped = line.strip()
            if not stripped:
                idx += 1
                continue

            if line.lstrip().startswith("|"):
                if not TABLE_SEP_RE.match(line):
                    tm = TABLE_REQ_RE.match(line)
                    if tm:
                        _record_requirement(raw_store, tm.group(1), rel_file, lineno, tm.group(2))
                    else:
                        _extract_bold_from_block(raw_store, rel_file, [(lineno, line)])
                idx += 1
                continue

            if HEADING_RE.match(line) or HR_RE.match(line):
                idx += 1
                continue

            list_match = LIST_ITEM_RE.match(line)
            if list_match:
                base_indent = len(list_match.group("indent").expandtabs(4))
                block: list[tuple[int, str]] = [(lineno, line)]
                j = idx + 1
                while j < total:
                    n_lineno, n_line = lines[j]
                    if not n_line.strip():
                        k = j + 1
                        while k < total and not lines[k][1].strip():
                            k += 1
                        if k < total:
                            k_line = lines[k][1]
                            if (
                                not k_line.lstrip().startswith("|")
                                and not HEADING_RE.match(k_line)
                                and not HR_RE.match(k_line)
                                and _indent_width(k_line) > base_indent
                                and not (LIST_ITEM_RE.match(k_line) and _has_bold_req(k_line))
                            ):
                                block.append((n_lineno, n_line))
                                j += 1
                                continue
                        break
                    if (
                        n_line.lstrip().startswith("|")
                        or HEADING_RE.match(n_line)
                        or HR_RE.match(n_line)
                    ):
                        break
                    if _indent_width(n_line) > base_indent:
                        if LIST_ITEM_RE.match(n_line) and _has_bold_req(n_line):
                            break
                        block.append((n_lineno, n_line))
                        j += 1
                    else:
                        break
                _extract_bold_from_block(raw_store, rel_file, block)
                idx = j
                continue

            # Plain paragraph block
            para: list[tuple[int, str]] = [(lineno, line)]
            j = idx + 1
            while j < total:
                n_lineno, n_line = lines[j]
                if (
                    not n_line.strip()
                    or n_line.lstrip().startswith("|")
                    or HEADING_RE.match(n_line)
                    or HR_RE.match(n_line)
                    or LIST_ITEM_RE.match(n_line)
                    or LEADING_BOLD_REQ_RE.match(n_line)
                ):
                    break
                para.append((n_lineno, n_line))
                j += 1
            _extract_bold_from_block(raw_store, rel_file, para)
            idx = j

    return {k: raw_store[k] for k in sorted(raw_store, key=_id_sort_key)}


def _find_token_line(lines: list[str], token: str) -> int:
    needle = f'"{token}"'
    for idx, line in enumerate(lines, start=1):
        if needle in line or token in line:
            return idx
    return 1


def _collect_json_spec_tokens(
    obj: Any,
    is_conformance: bool,
    out: list[str],
) -> None:
    if isinstance(obj, dict):
        for key, val in obj.items():
            if key == "spec":
                candidates: list[str] = []
                if isinstance(val, str):
                    candidates.append(val)
                elif isinstance(val, list) and all(isinstance(x, str) for x in val):
                    candidates.extend(val)
                for raw in candidates:
                    s = raw.strip()
                    if not s:
                        continue
                    marker_match = SPEC_MARKER_RE.search(s)
                    if marker_match:
                        out.extend(MARKER_TOKEN_RE.findall(marker_match.group(1)))
                    elif MARKER_ID_FULL_RE.match(s):
                        out.append(s)
                    elif is_conformance and "/" not in s and " " not in s:
                        out.append(s)
            else:
                _collect_json_spec_tokens(val, is_conformance, out)
    elif isinstance(obj, list):
        for item in obj:
            _collect_json_spec_tokens(item, is_conformance, out)


def scan_test_markers(root: Path) -> dict[str, list[MarkerHit]]:
    """Scan crates/**, adapters/**, tests/**, conformance/** for spec markers."""
    hits: dict[str, list[MarkerHit]] = collections.defaultdict(list)
    seen: set[tuple[str, str, int]] = set()

    for top_name in SCAN_DIRS:
        top_dir = root / top_name
        if not top_dir.is_dir():
            continue
        is_conformance = top_name == "conformance"
        for dirpath, dirnames, filenames in os.walk(top_dir):
            dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
            for fname in sorted(filenames):
                if fname.lower().endswith(".md"):
                    continue
                fpath = Path(dirpath) / fname
                try:
                    content = fpath.read_text(encoding="utf-8")
                except (UnicodeDecodeError, OSError):
                    continue

                rel_file = fpath.relative_to(root).as_posix()
                lines = content.splitlines()
                recorded_in_file: set[str] = set()

                if fname.lower().endswith(".json"):
                    try:
                        parsed = json.loads(content)
                    except json.JSONDecodeError:
                        parsed = None
                    if parsed is not None:
                        tokens: list[str] = []
                        _collect_json_spec_tokens(parsed, is_conformance, tokens)
                        for tok in tokens:
                            lineno = _find_token_line(lines, tok)
                            key = (tok, rel_file, lineno)
                            if key not in seen:
                                seen.add(key)
                                recorded_in_file.add(tok)
                                hits[tok].append(MarkerHit(id=tok, file=rel_file, line=lineno))

                for lineno, line in enumerate(lines, start=1):
                    for match in SPEC_MARKER_RE.finditer(line):
                        for tok in MARKER_TOKEN_RE.findall(match.group(1)):
                            if fname.lower().endswith(".json") and tok in recorded_in_file:
                                continue
                            key = (tok, rel_file, lineno)
                            if key not in seen:
                                seen.add(key)
                                hits[tok].append(MarkerHit(id=tok, file=rel_file, line=lineno))

    return {k: hits[k] for k in sorted(hits, key=_id_sort_key)}


def load_milestone_filter(root: Path, milestone: str) -> tuple[set[str], set[str]]:
    """Return (exact_ids, wildcard_prefixes) referenced by WPs in `milestone`."""
    wp_file = root / "work" / "work-packages.toml"
    if not wp_file.is_file():
        raise FileNotFoundError(f"work-packages file not found: {wp_file}")
    with wp_file.open("rb") as f:
        wps = tomllib.load(f).get("wp", [])
    matching = [w for w in wps if w.get("milestone") == milestone]
    if not matching:
        raise ValueError(f"no work packages found for milestone '{milestone}'")
    exact: set[str] = set()
    prefixes: set[str] = set()
    for wp in matching:
        for ref in wp.get("spec_refs", []):
            if ref.endswith("-*"):
                prefixes.add(ref[:-1])
            else:
                exact.add(ref)
    return exact, prefixes


def run_trace(root: Path, milestone: str | None = None) -> TraceReport:
    """Compute spec-to-test traceability report for `root`."""
    all_reqs = extract_requirements(root)
    if milestone is None:
        scoped_reqs = all_reqs
    else:
        exact, prefixes = load_milestone_filter(root, milestone)
        scoped_reqs = {
            rid: req
            for rid, req in all_reqs.items()
            if rid in exact or any(rid.startswith(p) for p in prefixes)
        }

    markers = scan_test_markers(root)
    uncovered_musts = [
        req for req in scoped_reqs.values() if req.is_must and req.id not in markers
    ]
    untested_reqs = [
        req for req in scoped_reqs.values() if (not req.withdrawn) and req.id not in markers
    ]
    unknown_markers = {
        mid: mhits for mid, mhits in markers.items() if mid not in all_reqs
    }
    return TraceReport(
        all_requirements=all_reqs,
        scoped_requirements=scoped_reqs,
        markers_by_id=markers,
        uncovered_musts=uncovered_musts,
        untested_requirements=untested_reqs,
        unknown_markers=unknown_markers,
        milestone=milestone,
    )


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument(
        "--warn",
        action="store_true",
        help="report uncovered MUSTs and unknown test IDs as warnings and exit 0 (default)",
    )
    mode.add_argument(
        "--strict",
        action="store_true",
        help="exit 1 if any in-scope MUST lacks a test or any test references an unknown ID",
    )
    ap.add_argument(
        "--milestone",
        metavar="Mx",
        help="limit checked requirements to IDs referenced by that milestone's WPs",
    )
    ap.add_argument(
        "--root",
        type=Path,
        default=ROOT,
        help="repository root directory (default: parent of tools/)",
    )
    return ap


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    strict = bool(args.strict)
    mode_name = "strict" if strict else "warn"
    root = args.root.resolve()

    try:
        report = run_trace(root, milestone=args.milestone)
    except (FileNotFoundError, ValueError) as exc:
        print(f"spec_trace error: {exc}", file=sys.stderr)
        return 1

    if report.uncovered_musts:
        print(f"MUST requirements without tests ({len(report.uncovered_musts)}):")
        for req in report.uncovered_musts:
            print(f"  - {req.id} ({req.file}:{req.line})")

    if report.unknown_markers:
        print(f"Tests referencing unknown spec IDs ({len(report.unknown_markers)}):")
        for mid, hits in report.unknown_markers.items():
            locs = ", ".join(f"{h.file}:{h.line}" for h in hits)
            print(f"  - {mid}: {locs}")

    total_reqs = len(report.scoped_requirements)
    must_reqs = sum(1 for r in report.scoped_requirements.values() if r.is_must)
    non_must_reqs = total_reqs - must_reqs
    untested_count = len(report.untested_requirements)
    uncovered_must_count = len(report.uncovered_musts)
    unknown_count = len(report.unknown_markers)
    marker_hits_count = sum(len(v) for v in report.markers_by_id.values())
    scope_label = args.milestone if args.milestone else "all"

    has_issues = bool(uncovered_must_count or unknown_count)
    verdict = "OK" if not has_issues else ("FAIL" if strict else "WARN")

    print(
        f"Scope: {scope_label} | Requirements: {total_reqs} "
        f"({must_reqs} MUST, {non_must_reqs} non-MUST, {untested_count} without test marker)"
    )
    print(
        f"Issues: {uncovered_must_count} MUST without test, "
        f"{unknown_count} unknown test IDs (across {marker_hits_count} markers)"
    )
    print(f"{verdict}: spec_trace (mode={mode_name}, milestone={scope_label})")

    return 1 if (strict and has_issues) else 0


if __name__ == "__main__":
    sys.exit(main())
