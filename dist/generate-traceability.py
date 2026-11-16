#!/usr/bin/env python3
"""Generate docs/traceability.md from tools/spec_trace.py (`VER-001`..`VER-003`)."""

from __future__ import annotations

import collections
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

from spec_trace import run_trace  # noqa: E402


def generate(out_path: Path) -> None:
    report = run_trace(ROOT, milestone=None)
    reqs = sorted(report.scoped_requirements.values(), key=lambda r: (r.file, r.line, r.id))

    total_reqs = len(reqs)
    must_reqs = sum(1 for r in reqs if r.is_must)
    non_must_reqs = total_reqs - must_reqs
    total_markers = sum(len(v) for v in report.markers_by_id.values())
    uncovered_musts = len(report.uncovered_musts)
    unknown_markers = len(report.unknown_markers)

    by_file: dict[str, list] = collections.defaultdict(list)
    for req in reqs:
        by_file[req.file].append(req)

    lines: list[str] = [
        "# Spec-to-Test Traceability Matrix (`VER-001`..`VER-003`)",
        "",
        "Generated mechanically by [`dist/generate-traceability.py`](../dist/generate-traceability.py) from `tools/spec_trace.py --strict`.",
        "",
        "## Summary Metrics",
        "",
        "| Metric | Value |",
        "|---|---:|",
        f"| Total normative requirements | {total_reqs} |",
        f"| `MUST` / `MUST NOT` requirements | {must_reqs} |",
        f"| Non-`MUST` requirements (`SHOULD`/`MAY`) | {non_must_reqs} |",
        f"| Total test/conformance `spec:` markers | {total_markers} |",
        f"| Uncovered `MUST` requirements | {uncovered_musts} |",
        f"| Unknown `spec:` test markers | {unknown_markers} |",
        "",
        "## Coverage Breakdown by Specification File",
        "",
        "| Specification File | Requirements | `MUST` | Tested | Uncovered `MUST` | Markers |",
        "|---|---:|---:|---:|---:|---:|",
    ]

    for spec_file in sorted(by_file):
        file_reqs = by_file[spec_file]
        f_must = sum(1 for r in file_reqs if r.is_must)
        f_tested = sum(1 for r in file_reqs if report.markers_by_id.get(r.id))
        f_uncovered_must = sum(
            1 for r in file_reqs if r.is_must and not r.withdrawn and not report.markers_by_id.get(r.id)
        )
        f_markers = sum(len(report.markers_by_id.get(r.id, [])) for r in file_reqs)
        lines.append(
            f"| `{spec_file}` | {len(file_reqs)} | {f_must} | {f_tested} | {f_uncovered_must} | {f_markers} |"
        )

    lines.append("")
    lines.append("## Requirement-to-Test Mapping")
    lines.append("")

    for spec_file in sorted(by_file):
        lines.append(f"### `{spec_file}`")
        lines.append("")
        lines.append("| ID | Spec Line | Obligation | Markers | Primary Verification Locations |")
        lines.append("|---|---:|---|---:|---|")
        for req in by_file[spec_file]:
            hits = sorted(
                {(h.file, h.line) for h in report.markers_by_id.get(req.id, [])},
                key=lambda x: (x[0], x[1]),
            )
            obligation = "WITHDRAWN" if req.withdrawn else ("`MUST`" if req.is_must else "non-MUST")
            if hits:
                shown = ", ".join(f"`{f}:{ln}`" for f, ln in hits[:4])
                if len(hits) > 4:
                    shown += f" (+{len(hits) - 4} more)"
            else:
                shown = "_none_"
            lines.append(f"| `{req.id}` | {req.line} | {obligation} | {len(report.markers_by_id.get(req.id, []))} | {shown} |")
        lines.append("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines), encoding="utf-8")
    print(f"Wrote {out_path} ({total_reqs} requirements, {total_markers} markers, {uncovered_musts} uncovered MUSTs)")


if __name__ == "__main__":
    target = ROOT / "docs" / "traceability.md"
    generate(target)
