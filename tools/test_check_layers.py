#!/usr/bin/env python3
"""Unit tests for tools/check_layers.py (WP-M0-05; spec: ARC-001, ARC-002, ADR-022)."""
from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest import mock

from tools import check_layers

REPO_ROOT = Path(__file__).resolve().parent.parent
PACKAGE_SCAN_DIRS = ("crates", "adapters", "bench", "fuzz", "sdks")


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _toml_list(items: list[str]) -> str:
    return "[" + ", ".join(json.dumps(x) for x in items) + "]"


def _build_and_check_workspace(
    root: Path,
    crates: dict[str, tuple[list[str], list[str]]],
    layers: dict[str, int],
    *,
    exempt_dev: list[str] | None = None,
    no_ml: list[str] | None = None,
    ml_crates: list[str] | None = None,
) -> tuple[int, str]:
    members = [f"crates/{name}" for name in sorted(crates)]
    _write(
        root / "Cargo.toml",
        "\n".join(
            [
                "[workspace]",
                'resolver = "2"',
                f"members = {_toml_list(members)}",
            ]
        )
        + "\n",
    )

    for name, (deps, dev_deps) in sorted(crates.items()):
        manifest_lines = [
            "[package]",
            f"name = {json.dumps(name)}",
            'version = "0.1.0"',
            'edition = "2021"',
            "",
            "[dependencies]",
            *[f'{d} = {{ path = "../{d}" }}' for d in deps],
            "",
            "[dev-dependencies]",
            *[f'{d} = {{ path = "../{d}" }}' for d in dev_deps],
        ]
        _write(root / "crates" / name / "Cargo.toml", "\n".join(manifest_lines) + "\n")
        _write(root / "crates" / name / "src" / "lib.rs", "//! stub crate\n")

    layers_lines = [
        "[layers]",
        *[f"{json.dumps(k)} = {v}" for k, v in sorted(layers.items())],
        "",
        "[exempt_dev]",
        f"crates = {_toml_list(exempt_dev or [])}",
        "",
        "[forbidden]",
        f"no_ml = {_toml_list(no_ml or [])}",
        f"ml_crates = {_toml_list(ml_crates or [])}",
    ]
    _write(root / "tools" / "layers.toml", "\n".join(layers_lines) + "\n")

    hermetic_env = {
        "CARGO_HOME": str(root / ".cargo-home"),
        "__CARGO_TEST_ROOT": str(root),
    }
    with mock.patch.dict(os.environ, hermetic_env):
        subprocess.run(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        )
        out = io.StringIO()
        with mock.patch.object(check_layers, "ROOT", root), contextlib.redirect_stdout(out):
            rc = check_layers.main()
    return rc, out.getvalue()


class CheckLayersPreWorkspaceTests(unittest.TestCase):
    def test_missing_cargo_toml_exits_zero_with_skip_message(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            out = io.StringIO()
            with mock.patch.object(check_layers, "ROOT", root), contextlib.redirect_stdout(out):
                rc = check_layers.main()
            self.assertEqual(rc, 0)
            self.assertEqual(
                out.getvalue().strip(),
                "check_layers: no Cargo.toml yet (pre-M0) - skipping",
            )


class CheckLayersWorkspaceTests(unittest.TestCase):
    def setUp(self) -> None:
        if shutil.which("cargo") is None:
            self.skipTest("cargo not found on PATH")

    def test_clean_workspace_passes(self) -> None:
        # spec: ARC-001, ARC-002, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-core": ([], []),
                    "gap-models": (["gap-core"], []),
                    "gap-plan": (["gap-core"], []),
                    "gap-exec": (["gap-plan"], ["gap-testkit"]),
                    "gap-testkit": (["gap-exec"], []),
                },
                layers={
                    "gap-core": 0,
                    "gap-models": 1,
                    "gap-plan": 2,
                    "gap-exec": 3,
                },
                exempt_dev=["gap-testkit"],
                no_ml=["gap-plan", "gap-exec"],
                ml_crates=["gap-models"],
            )
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 5 workspace crates respect layering and ARC-002")

    def test_external_non_workspace_path_dependency_accepted(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            # External crate outside [workspace] members (sibling directory ../ext-serde).
            _write(
                base / "ext-serde" / "Cargo.toml",
                '[package]\nname = "ext-serde"\nversion = "0.1.0"\nedition = "2021"\n',
            )
            _write(base / "ext-serde" / "src" / "lib.rs", "//! external dependency stub\n")

            ws_root = base / "ws"
            _write(
                ws_root / "Cargo.toml",
                '[workspace]\nresolver = "2"\nmembers = ["crates/gap-core", "crates/gap-ql"]\n',
            )
            _write(
                ws_root / "crates" / "gap-core" / "Cargo.toml",
                "\n".join(
                    [
                        "[package]",
                        'name = "gap-core"',
                        'version = "0.1.0"',
                        'edition = "2021"',
                        "",
                        "[dependencies]",
                        'ext-serde = { path = "../../../ext-serde" }',
                    ]
                )
                + "\n",
            )
            _write(ws_root / "crates" / "gap-core" / "src" / "lib.rs", "//! core stub\n")
            _write(
                ws_root / "crates" / "gap-ql" / "Cargo.toml",
                "\n".join(
                    [
                        "[package]",
                        'name = "gap-ql"',
                        'version = "0.1.0"',
                        'edition = "2021"',
                        "",
                        "[dependencies]",
                        'gap-core = { path = "../gap-core" }',
                        'ext-serde = { path = "../../../ext-serde" }',
                    ]
                )
                + "\n",
            )
            _write(ws_root / "crates" / "gap-ql" / "src" / "lib.rs", "//! ql stub\n")
            _write(
                ws_root / "tools" / "layers.toml",
                "\n".join(
                    [
                        "[layers]",
                        '"gap-core" = 0',
                        '"gap-ql" = 1',
                        "",
                        "[exempt_dev]",
                        "crates = []",
                        "",
                        "[forbidden]",
                        'no_ml = ["gap-ql"]',
                        "ml_crates = []",
                    ]
                )
                + "\n",
            )

            hermetic_env = {
                "CARGO_HOME": str(base / ".cargo-home"),
                "__CARGO_TEST_ROOT": str(base),
            }
            with mock.patch.dict(os.environ, hermetic_env):
                subprocess.run(
                    ["cargo", "generate-lockfile", "--offline"],
                    cwd=ws_root,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                out = io.StringIO()
                with mock.patch.object(check_layers, "ROOT", ws_root), contextlib.redirect_stdout(out):
                    rc = check_layers.main()

            self.assertEqual(rc, 0, msg=out.getvalue())
            self.assertEqual(
                out.getvalue().strip(),
                "OK: 2 workspace crates respect layering and ARC-002",
            )

    def test_dependency_on_higher_layer_fails(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-core": (["gap-ql"], []),
                    "gap-ql": ([], []),
                },
                layers={"gap-core": 0, "gap-ql": 1},
            )
            self.assertEqual(rc, 1)
            self.assertIn("LAYER CHECK FAILED:", out)
            self.assertIn("gap-core (L0) depends on gap-ql (L1): layer violation", out)

    def test_dependency_on_same_layer_fails(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-ql": (["gap-store"], []),
                    "gap-store": ([], []),
                },
                layers={"gap-ql": 1, "gap-store": 1},
            )
            self.assertEqual(rc, 1)
            self.assertIn("LAYER CHECK FAILED:", out)
            self.assertIn("gap-ql (L1) depends on gap-store (L1): layer violation", out)

    def test_dev_dependency_on_higher_and_same_layer_accepted(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-core": ([], ["gap-exec", "gap-core-peer"]),
                    "gap-core-peer": ([], []),
                    "gap-exec": (["gap-core"], []),
                },
                layers={"gap-core": 0, "gap-core-peer": 0, "gap-exec": 3},
            )
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 3 workspace crates respect layering and ARC-002")

    def test_exempt_dev_crate_used_as_normal_dependency_fails(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-ql": (["gap-testkit"], []),
                    "gap-testkit": ([], []),
                },
                layers={"gap-ql": 1},
                exempt_dev=["gap-testkit"],
            )
            self.assertEqual(rc, 1)
            self.assertIn("LAYER CHECK FAILED:", out)
            self.assertIn(
                "gap-ql: depends on dev-only crate gap-testkit in a non-dev dependency",
                out,
            )

    def test_workspace_crate_missing_from_layers_toml_fails(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-core": ([], []),
                    "gap-unlisted": (["gap-core"], []),
                },
                layers={"gap-core": 0},
            )
            self.assertEqual(rc, 1)
            self.assertIn("LAYER CHECK FAILED:", out)
            self.assertIn("gap-unlisted: not listed in tools/layers.toml", out)

    def test_inv_03_arc_002_transitive_ml_dependency_fails(self) -> None:
        # spec: ARC-002, INV-03, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            # L3 -> L2 -> L1 respects ARC-001 layering; only ARC-002 transitive reachability fails.
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-models": ([], []),
                    "gap-ingest": (["gap-models"], []),
                    "gap-exec": (["gap-ingest"], []),
                },
                layers={"gap-models": 1, "gap-ingest": 2, "gap-exec": 3},
                no_ml=["gap-exec"],
                ml_crates=["gap-models"],
            )
            self.assertEqual(rc, 1)
            self.assertIn("LAYER CHECK FAILED:", out)
            self.assertNotIn("layer violation", out)
            self.assertIn(
                "ARC-002: gap-exec transitively depends on ML/discovery crates: gap-models",
                out,
            )

    def test_inv_03_arc_002_dev_only_path_accepted(self) -> None:
        # spec: ARC-002, INV-03, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            rc, out = _build_and_check_workspace(
                Path(tmp),
                crates={
                    "gap-models": ([], []),
                    "gap-route": (["gap-models"], []),
                    "gap-ingest": ([], ["gap-models"]),
                    "gap-exec": (["gap-ingest"], ["gap-route"]),
                },
                layers={"gap-models": 1, "gap-route": 2, "gap-ingest": 2, "gap-exec": 3},
                no_ml=["gap-exec"],
                ml_crates=["gap-models", "gap-route"],
            )
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 4 workspace crates respect layering and ARC-002")


def _audit_layers_completeness(repo_root: Path) -> tuple[set[str], set[str], set[str]]:
    excluded_prefixes: set[str] = set()
    root_cargo = repo_root / "Cargo.toml"
    if root_cargo.is_file():
        root_doc = tomllib.loads(root_cargo.read_text(encoding="utf-8"))
        for item in root_doc.get("workspace", {}).get("exclude", []):
            excluded_prefixes.add(item.strip("/"))

    discovered: set[str] = set()
    for top in PACKAGE_SCAN_DIRS:
        top_dir = repo_root / top
        if not top_dir.is_dir():
            continue
        for manifest in sorted(top_dir.rglob("Cargo.toml")):
            rel_parts = manifest.relative_to(repo_root).parts
            if "target" in rel_parts:
                continue
            doc = tomllib.loads(manifest.read_text(encoding="utf-8"))
            rel_dir = manifest.parent.relative_to(repo_root).as_posix()
            is_excluded = any(
                rel_dir == ex or rel_dir.startswith(f"{ex}/") for ex in excluded_prefixes
            )
            if is_excluded and "workspace" in doc:
                continue
            pkg_name = doc.get("package", {}).get("name")
            if pkg_name:
                discovered.add(pkg_name)

    layers_doc = tomllib.loads((repo_root / "tools" / "layers.toml").read_text(encoding="utf-8"))
    layered = set(layers_doc.get("layers", {}).keys())
    exempt = set(layers_doc.get("exempt_dev", {}).get("crates", []))
    configured = layered | exempt
    return discovered - configured, configured - discovered, layered & exempt


class LayersTomlCompletenessTests(unittest.TestCase):
    def test_real_repository_packages_match_layers_toml_without_gaps_or_stale_entries(self) -> None:
        # spec: ARC-001, ADR-022
        missing, stale, overlap = _audit_layers_completeness(REPO_ROOT)
        self.assertEqual(missing, set(), msg=f"packages missing from tools/layers.toml: {sorted(missing)}")
        self.assertEqual(stale, set(), msg=f"stale crates in tools/layers.toml: {sorted(stale)}")
        self.assertEqual(overlap, set(), msg=f"crates in both [layers] and [exempt_dev]: {sorted(overlap)}")

        layers_doc = tomllib.loads((REPO_ROOT / "tools" / "layers.toml").read_text(encoding="utf-8"))
        configured = set(layers_doc["layers"]) | set(layers_doc.get("exempt_dev", {}).get("crates", []))
        self.assertEqual(len(configured), 32)

        # Verify standalone fuzz/Cargo.toml is explicitly excluded in root Cargo.toml and references valid crates.
        fuzz_manifest = REPO_ROOT / "fuzz" / "Cargo.toml"
        if fuzz_manifest.is_file():
            fuzz_doc = tomllib.loads(fuzz_manifest.read_text(encoding="utf-8"))
            self.assertIn("workspace", fuzz_doc)
            for dep_name, dep_spec in fuzz_doc.get("dependencies", {}).items():
                if isinstance(dep_spec, dict) and "path" in dep_spec:
                    self.assertIn(dep_name, configured)

    def test_completeness_audit_detects_missing_and_stale_packages_across_all_roots(self) -> None:
        # spec: ARC-001, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(root / "Cargo.toml", '[workspace]\nexclude = ["fuzz"]\n')
            for folder, pkg in (
                ("crates/c1", "pkg-crates"),
                ("adapters/a1", "pkg-adapters"),
                ("bench/b1", "pkg-bench"),
                ("sdks/py", "pkg-sdks"),
            ):
                _write(root / folder / "Cargo.toml", f'[package]\nname = "{pkg}"\nversion = "0.1.0"\n')
            # Standalone excluded fuzz workspace is ignored, while non-excluded fuzz member is checked.
            _write(root / "fuzz" / "Cargo.toml", '[package]\nname = "standalone-fuzz"\n[workspace]\n')
            _write(
                root / "tools" / "layers.toml",
                "\n".join(
                    [
                        "[layers]",
                        'pkg-crates = 0',
                        'pkg-adapters = 4',
                        'pkg-stale = 5',
                        "[exempt_dev]",
                        'crates = ["pkg-bench"]',
                    ]
                )
                + "\n",
            )

            missing, stale, overlap = _audit_layers_completeness(root)
            self.assertEqual(missing, {"pkg-sdks"})
            self.assertEqual(stale, {"pkg-stale"})
            self.assertEqual(overlap, set())


if __name__ == "__main__":
    unittest.main()
