#!/usr/bin/env python3
"""Enforce crate layering (ARC-001) and the no-ML-on-execution-path rule (ARC-002 / ADR-003).

Reads tools/layers.toml and `cargo metadata`. Exit 0 when OK (or when there is no Cargo workspace yet).
Dev-dependencies are exempt from the layer rule (but not from ARC-002); crates in [exempt_dev] may be used anywhere as dev-deps.
"""
from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    if not (ROOT / "Cargo.toml").exists():
        print("check_layers: no Cargo.toml yet (pre-M0) - skipping")
        return 0
    cfg = tomllib.loads((ROOT / "tools" / "layers.toml").read_text(encoding="utf-8"))
    layers: dict[str, int] = cfg["layers"]
    exempt_dev = set(cfg.get("exempt_dev", {}).get("crates", []))
    no_ml = set(cfg["forbidden"]["no_ml"])
    ml = set(cfg["forbidden"]["ml_crates"])

    meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT))
    pkgs = {p["id"]: p for p in meta["packages"]}
    ws = {p["name"]: p for p in meta["packages"] if p["id"] in set(meta["workspace_members"])}
    errs: list[str] = []

    for name, pkg in ws.items():
        if name not in layers and name not in exempt_dev:
            errs.append(f"{name}: not listed in tools/layers.toml")
            continue
        if name in exempt_dev:
            continue
        for dep in pkg["dependencies"]:
            dn = dep["name"]
            if dn not in ws:
                continue
            if dep.get("kind") == "dev":
                continue
            if dn in exempt_dev:
                errs.append(f"{name}: depends on dev-only crate {dn} in a non-dev dependency")
            elif layers.get(dn, 99) >= layers[name]:
                errs.append(f"{name} (L{layers[name]}) depends on {dn} (L{layers.get(dn)}): layer violation")

    resolve = {n["id"]: n for n in meta["resolve"]["nodes"]}
    name_of = {pid: p["name"] for pid, p in pkgs.items()}

    def reach(pid: str, seen: set[str]) -> None:
        for d in resolve[pid]["deps"]:
            kinds = {k.get("kind") for k in d["dep_kinds"]}
            if kinds == {"dev"}:
                continue
            if d["pkg"] not in seen:
                seen.add(d["pkg"])
                reach(d["pkg"], seen)

    for name in no_ml & set(ws):
        pid = ws[name]["id"]
        seen: set[str] = set()
        reach(pid, seen)
        bad = sorted({name_of[s] for s in seen} & ml)
        if bad:
            errs.append(f"ARC-002: {name} transitively depends on ML/discovery crates: {', '.join(bad)}")

    if errs:
        print("LAYER CHECK FAILED:")
        for e in errs:
            print("  -", e)
        return 1
    print(f"OK: {len(ws)} workspace crates respect layering and ARC-002")
    return 0


if __name__ == "__main__":
    sys.exit(main())
