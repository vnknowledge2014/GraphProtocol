#!/usr/bin/env python3
"""Work-package tooling for GAP (ADR-022).

Usage:
  python3 tools/wp.py validate                 # structural + cross-reference + concurrency checks (CI gate)
  python3 tools/wp.py dag --milestone M1 [--out work/dag/m1.json]   # omni-compatible DAG (only documented fields)
  python3 tools/wp.py ready                    # WPs whose dependencies are done and that are not started
  python3 tools/wp.py modules                  # per-crate module stubs implied by all `owns` globs (used by WP-M0-02)
  python3 tools/wp.py set WP-M1-03 doing|done|blocked|todo
  python3 tools/wp.py board                    # status overview per milestone
  python3 tools/wp.py critical-path [--milestone M3]   # longest chain by size-weighted days (planning aid)

Source of truth: work/work-packages.toml (plan) and work/status.toml (progress, one line per WP).
Python >= 3.11 (tomllib). No third-party dependencies.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WP_FILE = ROOT / "work" / "work-packages.toml"
STATUS_FILE = ROOT / "work" / "status.toml"

SIZE_DAYS = {"S": 0.5, "M": 2.0, "L": 5.0}  # agent-days; XL is forbidden (split before starting)
ROLES = {"coder", "tester", "reviewer", "analyzer"}
STATES = {"todo", "doing", "done", "blocked"}
ID_RE = re.compile(r"^WP-(M[0-7]|R1)-\d{2}$")
SPEC_ID_RE = re.compile(r"\b(ARC|GQL|NGC|RTE|ING|RUN|SEC|OPS|CMP|INV|ADR|TM)-(\d{2,3})\b")
REQUIRED = ["id", "title", "milestone", "role", "size", "depends_on", "owns", "spec_refs", "deliverables", "acceptance"]


def load_wps() -> list[dict]:
    with WP_FILE.open("rb") as f:
        return tomllib.load(f).get("wp", [])


def load_status() -> dict[str, str]:
    if not STATUS_FILE.exists():
        return {}
    with STATUS_FILE.open("rb") as f:
        return {k: v.get("state", "todo") for k, v in tomllib.load(f).items()}


def known_spec_ids() -> set[str]:
    ids: set[str] = set()
    for p in list((ROOT / "spec").glob("*.md")) + [ROOT / "docs" / "01-DECISIONS.md"]:
        if p.exists():
            text = p.read_text(encoding="utf-8")
            ids.update(f"{a}-{n}" for a, n in SPEC_ID_RE.findall(text))
    return ids


def static_prefix(glob: str) -> str:
    cut = len(glob)
    for ch in "*?[{":
        i = glob.find(ch)
        if i != -1:
            cut = min(cut, i)
    return glob[:cut]


def globs_overlap(a: str, b: str) -> bool:
    pa, pb = static_prefix(a), static_prefix(b)
    return pa.startswith(pb) or pb.startswith(pa)


def ancestors(wps_by_id: dict[str, dict]) -> dict[str, set[str]]:
    memo: dict[str, set[str]] = {}

    def visit(i: str, stack: tuple[str, ...]) -> set[str]:
        if i in memo:
            return memo[i]
        if i in stack:
            raise ValueError("cycle: " + " -> ".join(stack + (i,)))
        out: set[str] = set()
        for d in wps_by_id[i]["depends_on"]:
            out.add(d)
            out |= visit(d, stack + (i,))
        memo[i] = out
        return out

    for i in wps_by_id:
        visit(i, ())
    return memo


def validate() -> int:
    errs: list[str] = []
    wps = load_wps()
    by_id: dict[str, dict] = {}
    spec_ids = known_spec_ids()
    for w in wps:
        wid = w.get("id", "<missing id>")
        for k in REQUIRED:
            if k not in w:
                errs.append(f"{wid}: missing field '{k}'")
        if "id" not in w:
            continue
        if not ID_RE.match(wid):
            errs.append(f"{wid}: bad id format")
        if wid in by_id:
            errs.append(f"{wid}: duplicate id")
        by_id[wid] = w
        if w.get("size") not in SIZE_DAYS:
            errs.append(f"{wid}: size must be one of {sorted(SIZE_DAYS)} (XL must be split)")
        if w.get("role") not in ROLES:
            errs.append(f"{wid}: role must be one of {sorted(ROLES)}")
        if not w.get("acceptance"):
            errs.append(f"{wid}: needs >=1 executable acceptance command")
        if not w.get("deliverables"):
            errs.append(f"{wid}: needs deliverables")
        if w.get("role") == "coder" and not w.get("owns"):
            errs.append(f"{wid}: coder WP must declare 'owns'")
        for ref in w.get("spec_refs", []):
            if ref.endswith("-*"):
                continue
            if ref not in spec_ids:
                errs.append(f"{wid}: unknown spec ref '{ref}'")
        if w.get("milestone") and not wid.startswith(f"WP-{w['milestone']}-"):
            errs.append(f"{wid}: id prefix does not match milestone {w['milestone']}")
    for wid, w in by_id.items():
        for d in w.get("depends_on", []):
            if d not in by_id:
                errs.append(f"{wid}: depends on unknown {d}")
            else:
                dm, wm = by_id[d]["milestone"], w["milestone"]
                if dm != "R1" and wm != "R1" and int(dm[1:]) > int(wm[1:]):
                    errs.append(f"{wid}: depends on later-milestone {d}")
    if not errs:
        try:
            anc = ancestors(by_id)
        except ValueError as e:
            errs.append(str(e))
            anc = {}
        ids = sorted(by_id)
        for i, a in enumerate(ids):
            for b in ids[i + 1:]:
                if b in anc.get(a, set()) or a in anc.get(b, set()):
                    continue  # ordered by dependency => may overlap
                for ga in by_id[a].get("owns", []):
                    for gb in by_id[b].get("owns", []):
                        if globs_overlap(ga, gb):
                            errs.append(f"concurrent WPs {a} and {b} have overlapping ownership: '{ga}' vs '{gb}'")
    status = load_status()
    for sid in status:
        if sid not in by_id:
            errs.append(f"status.toml references unknown {sid}")
    if errs:
        print("VALIDATION FAILED:")
        for e in errs:
            print("  -", e)
        return 1
    ms = defaultdict(int)
    for w in wps:
        ms[w["milestone"]] += 1
    print(f"OK: {len(wps)} work packages " + ", ".join(f"{m}={n}" for m, n in sorted(ms.items())))
    return 0


def cmd_dag(milestone: str, out: str | None) -> int:
    wps = [w for w in load_wps() if w["milestone"] == milestone]
    if not wps:
        print(f"no WPs for {milestone}", file=sys.stderr)
        return 1
    idx = {w["id"]: n for n, w in enumerate(sorted(wps, key=lambda x: x["id"]), start=1)}
    tasks = []
    for w in sorted(wps, key=lambda x: x["id"]):
        title = (
            f"{w['id']} {w['title']}. Deliverables: " + "; ".join(w["deliverables"])
            + ". Acceptance (must pass): " + " && ".join(w["acceptance"])
        )
        t = {
            "id": idx[w["id"]],
            "title": title,
            "role": w["role"],
            "depends_on": sorted(idx[d] for d in w["depends_on"] if d in idx),
            "system_prompt": (
                "Read AGENTS.md and docs/AGENT-GUIDE.md first. Work ONLY inside these paths: "
                + ", ".join(w["owns"] or ["(read-only task)"])
                + ". Spec refs: " + ", ".join(w["spec_refs"]) + ". Expand this work package into 2-5 minute steps before coding."
            ),
        }
        tasks.append(t)
    dag = {"project_root": str(ROOT), "tdd": True, "auto_commit": False, "tasks": tasks}
    text = json.dumps(dag, indent=2, ensure_ascii=False)
    if out:
        Path(out).write_text(text + "\n", encoding="utf-8")
        print(f"wrote {out} ({len(tasks)} tasks)")
    else:
        print(text)
    return 0


MODULE_RE = re.compile(r"^(?P<root>crates|adapters|bench)/(?P<crate>[^/*]+)/src/(?P<mod>[A-Za-z0-9_]+)(?:/|\.rs|\*|$)")


def cmd_modules() -> int:
    """Print, per crate, the top-level modules implied by the `owns` globs of all WPs.

    WP-M0-02 uses this to create empty module stubs (and `mod` declarations in lib.rs/main.rs) so that
    concurrent WPs never edit the same lib.rs (docs/AGENT-GUIDE.md section 6).
    """
    mods: dict[str, set[str]] = defaultdict(set)
    for w in load_wps():
        for g in w.get("owns", []):
            m = MODULE_RE.match(g)
            if m and m.group("mod") not in ("lib", "main"):
                mods[f"{m.group('root')}/{m.group('crate')}"].add(m.group("mod"))
    for crate in sorted(mods):
        print(f"{crate}: " + ", ".join(sorted(mods[crate])))
    return 0


def cmd_ready() -> int:
    wps = {w["id"]: w for w in load_wps()}
    status = load_status()
    ready = [
        i for i, w in wps.items()
        if status.get(i, "todo") == "todo" and all(status.get(d) == "done" for d in w["depends_on"])
    ]
    for i in sorted(ready):
        print(f"{i}  [{wps[i]['size']}]  {wps[i]['title']}")
    if not ready:
        print("(nothing ready)")
    return 0


def cmd_set(wid: str, state: str) -> int:
    if state not in STATES:
        print(f"state must be one of {sorted(STATES)}", file=sys.stderr)
        return 1
    ids = {w["id"] for w in load_wps()}
    if wid not in ids:
        print(f"unknown {wid}", file=sys.stderr)
        return 1
    status = load_status()
    status[wid] = state
    lines = ["# Progress ledger (one table per WP). Edit via: python3 tools/wp.py set <ID> <state>"]
    for k in sorted(status):
        lines.append(f'["{k}"]\nstate = "{status[k]}"')
    STATUS_FILE.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"{wid} -> {state}")
    return 0


def cmd_board() -> int:
    wps = load_wps()
    status = load_status()
    by_m: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    for w in wps:
        by_m[w["milestone"]][status.get(w["id"], "todo")] += 1
    print(f"{'milestone':<10}{'todo':>6}{'doing':>7}{'blocked':>9}{'done':>6}")
    for m in sorted(by_m):
        c = by_m[m]
        print(f"{m:<10}{c['todo']:>6}{c['doing']:>7}{c['blocked']:>9}{c['done']:>6}")
    return 0


def cmd_critical(milestone: str | None) -> int:
    wps = {w["id"]: w for w in load_wps() if milestone is None or w["milestone"] == milestone}
    memo: dict[str, tuple[float, list[str]]] = {}

    def best(i: str) -> tuple[float, list[str]]:
        if i in memo:
            return memo[i]
        w = wps[i]
        cands = [best(d) for d in w["depends_on"] if d in wps]
        base = max(cands, key=lambda x: x[0], default=(0.0, []))
        memo[i] = (base[0] + SIZE_DAYS[w["size"]], base[1] + [i])
        return memo[i]

    if not wps:
        print("no WPs")
        return 1
    tot, path = max((best(i) for i in wps), key=lambda x: x[0])
    work = sum(SIZE_DAYS[w["size"]] for w in wps.values())
    print(f"critical path: {tot:.1f} agent-days over {len(path)} WPs; total effort {work:.1f} agent-days; max parallelism ~{work / tot:.1f}x")
    print(" -> ".join(path))
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("validate")
    d = sub.add_parser("dag")
    d.add_argument("--milestone", required=True)
    d.add_argument("--out")
    sub.add_parser("ready")
    sub.add_parser("modules")
    s = sub.add_parser("set")
    s.add_argument("id")
    s.add_argument("state")
    sub.add_parser("board")
    c = sub.add_parser("critical-path")
    c.add_argument("--milestone")
    a = ap.parse_args()
    if a.cmd == "validate":
        return validate()
    if a.cmd == "dag":
        return cmd_dag(a.milestone, a.out)
    if a.cmd == "ready":
        return cmd_ready()
    if a.cmd == "modules":
        return cmd_modules()
    if a.cmd == "set":
        return cmd_set(a.id, a.state)
    if a.cmd == "board":
        return cmd_board()
    if a.cmd == "critical-path":
        return cmd_critical(a.milestone)
    return 2


if __name__ == "__main__":
    sys.exit(main())
