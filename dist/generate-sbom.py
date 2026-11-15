#!/usr/bin/env python3
"""Generate a CycloneDX 1.5 JSON SBOM from Cargo.toml + Cargo.lock (SEC-101, ADR-020)."""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
import uuid
from datetime import datetime, timezone
from pathlib import Path

FORBIDDEN_DEFAULT_PACKAGES = {"surrealdb", "surrealdb-core"}


def build_sbom(
    root: Path,
    allow_bsl: bool = False,
    timestamp_override: str | None = None,
) -> dict:
    cargo_toml = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    cargo_lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))

    ws_pkg = cargo_toml.get("workspace", {}).get("package", {})
    ws_version = ws_pkg.get("version", "0.1.0")
    ws_license = ws_pkg.get("license", "Apache-2.0")

    packages = cargo_lock.get("package", [])
    if not allow_bsl:
        present_bsl = sorted(
            pkg["name"] for pkg in packages if pkg.get("name") in FORBIDDEN_DEFAULT_PACKAGES
        )
        if present_bsl:
            raise SystemExit(
                "SEC-101 / ADR-020 violation: forbidden BSL package(s) in default Cargo.lock: "
                + ", ".join(present_bsl)
            )

    if timestamp_override is not None:
        ts = timestamp_override
    else:
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    # Disambiguate duplicate package names with different versions in bom-ref/purl.
    ref_counts: dict[tuple[str, str], str] = {}
    for pkg in packages:
        name = pkg["name"]
        version = pkg["version"]
        ref = f"pkg:cargo/{name}@{version}"
        ref_counts[(name, version)] = ref

    # Fallback lookup by name for lockfile dependency entries without explicit version suffix.
    versions_by_name: dict[str, list[str]] = {}
    for pkg in packages:
        versions_by_name.setdefault(pkg["name"], []).append(pkg["version"])

    components = []
    dependencies = []
    for pkg in packages:
        name = pkg["name"]
        version = pkg["version"]
        bom_ref = ref_counts[(name, version)]
        is_workspace_member = "source" not in pkg

        comp: dict = {
            "bom-ref": bom_ref,
            "type": "library",
            "name": name,
            "version": version,
            "purl": bom_ref,
        }
        if is_workspace_member:
            comp["licenses"] = [{"license": {"id": ws_license}}]
        checksum = pkg.get("checksum")
        if checksum:
            comp["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        source = pkg.get("source")
        if source:
            comp["externalReferences"] = [{"type": "distribution", "url": source}]
        components.append(comp)

        dep_refs = []
        for raw_dep in pkg.get("dependencies", []):
            parts = raw_dep.split()
            dep_name = parts[0]
            if len(parts) >= 2:
                dep_ref = f"pkg:cargo/{dep_name}@{parts[1]}"
            else:
                candidates = versions_by_name.get(dep_name, [])
                dep_ref = f"pkg:cargo/{dep_name}@{candidates[0]}" if candidates else dep_name
            dep_refs.append(dep_ref)

        dependencies.append({"ref": bom_ref, "dependsOn": sorted(set(dep_refs))})

    root_ref = f"pkg:cargo/gap-cli@{ws_version}"
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, root_ref)}",
        "version": 1,
        "metadata": {
            "timestamp": ts,
            "tools": [
                {
                    "vendor": "Graph Agent Protocol",
                    "name": "dist/generate-sbom.py",
                    "version": "1.0.0",
                }
            ],
            "component": {
                "bom-ref": root_ref,
                "type": "application",
                "name": "gap",
                "version": ws_version,
                "licenses": [{"license": {"id": ws_license}}],
                "purl": root_ref,
            },
        },
        "components": components,
        "dependencies": dependencies,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate CycloneDX 1.5 JSON SBOM for GAP")
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Repository root containing Cargo.toml and Cargo.lock",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=Path("target/sbom.cdx.json"),
        help="Output CycloneDX JSON path",
    )
    parser.add_argument(
        "--allow-bsl",
        action="store_true",
        help="Permit BSL-1.1 packages (explicitly disabled for default release gate SEC-101)",
    )
    parser.add_argument(
        "--timestamp",
        default=None,
        help="Optional ISO-8601 UTC timestamp for deterministic SBOM output",
    )
    args = parser.parse_args()

    sbom = build_sbom(
        args.root.resolve(),
        allow_bsl=args.allow_bsl,
        timestamp_override=args.timestamp,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(sbom, indent=2) + "\n", encoding="utf-8")
    print(
        f"Wrote CycloneDX {sbom['specVersion']} SBOM to {args.out} "
        f"({len(sbom['components'])} components, BSL check OK)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
