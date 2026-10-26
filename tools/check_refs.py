#!/usr/bin/env python3
"""Fail on dangling requirement/ADR/invariant/threat references in docs, work packages and tools.

Defined ids:
  - bold bullets in spec/*.md and docs/03-VERIFICATION.md:   **RTE-040 (title).**  -> RTE-040
  - table-defined invariants (spec/01) `| INV-nn |` and threats (spec/07) `| TM-nn |`
  - ADR headings in docs/01-DECISIONS.md: `## ADR-nnn`
Referenced ids: any `AREA-nnn` token in *.md, work/*.toml, tools/*.py, *.proto (docs/evidence is ignored).
Exit code 1 when something is referenced but not defined.
"""
from __future__ import annotations

import collections
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
AREAS = "ARC|GQL|NGC|RTE|ING|RUN|SEC|OPS|CMP|VER|INV|TM|ADR"


def read(p: Path) -> str:
    return p.read_text(encoding="utf-8")


def defined_ids() -> set[str]:
    out: set[str] = set()
    for p in [*sorted((ROOT / "spec").glob("*.md")), ROOT / "docs" / "03-VERIFICATION.md"]:
        out |= {f"{a}-{n}" for a, n in re.findall(r"\*\*([A-Z]{2,3})-(\d{2,3})\b", read(p))}
    out |= {f"INV-{n}" for n in re.findall(r"^\| INV-(\d{2}) \|", read(ROOT / "spec" / "01-architecture.md"), re.M)}
    out |= {f"TM-{n}" for n in re.findall(r"^\| TM-(\d{2}) \|", read(ROOT / "spec" / "07-security.md"), re.M)}
    out |= {f"ADR-{n}" for n in re.findall(r"^## ADR-(\d{3})", read(ROOT / "docs" / "01-DECISIONS.md"), re.M)}
    return out


def main() -> int:
    defined = defined_ids()
    pat = re.compile(rf"\b({AREAS})-(\d{{2,3}})\b")
    files = [*ROOT.glob("**/*.md"), *ROOT.glob("work/*.toml"), *ROOT.glob("tools/*.py"), *ROOT.glob("proto/**/*.proto")]
    dangling: dict[str, set[str]] = collections.defaultdict(set)
    for f in files:
        rel = f.relative_to(ROOT).as_posix()
        if rel.startswith(("docs/evidence", "third_party", "target", "node_modules")) or f.name == "check_refs.py":
            continue
        for a, n in pat.findall(read(f)):
            i = f"{a}-{n}"
            if i not in defined:
                dangling[i].add(rel)
    if dangling:
        print("DANGLING REFERENCES:")
        for i in sorted(dangling):
            print(f"  {i}: {', '.join(sorted(dangling[i]))}")
        return 1
    print(f"OK: {len(defined)} ids defined, no dangling references")
    return 0


if __name__ == "__main__":
    sys.exit(main())
