#!/usr/bin/env python3
"""Unit tests for tools/spec_trace.py (WP-M0-07; spec: ARC-004, ADR-022)."""
from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from tools import spec_trace

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / "tools" / "spec_trace.py"


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


class SpecTraceTests(unittest.TestCase):
    def test_extract_requirements_bullets_tables_and_fences(self) -> None:
        # spec: ARC-004, ADR-022
        fake_inv = "INV-" + "99"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(
                root / "spec" / "01-architecture.md",
                "\n".join(
                    [
                        "# 01 Architecture",
                        "- **ARC-001** The layer rule MUST hold; violations fail CI.",
                        "- **ARC-002** Execution plane isolation:",
                        "  - Crates on the typed path MUST NOT depend on ML runtimes.",
                        "- **ARC-004** Public items must have rustdoc; SHOULD pass.",
                        "- **RTE-040 (Unmapped domain).** Lowercase must; MUST NOT execute.",
                        "- **SEC-001** Gateway MUST validate JWT; **SEC-002** Dev keys SHOULD expire.",
                        "- **GQL-001** (withdrawn) Formerly MUST validate.",
                        "",
                        "```markdown",
                        "- **TST-888** Example inside code fence MUST be ignored.",
                        f"| {fake_inv} | Inside code fence MUST be ignored. |",
                        "```",
                        "",
                        "| ID | Invariant |",
                        "|---|---|",
                        "| INV-01 | A request observes one snapshot generation. |",
                        "| INV-02 | No lock guard MUST NOT cross await. |",
                    ]
                )
                + "\n",
            )
            _write(
                root / "spec" / "07-security.md",
                "\n".join(
                    [
                        "# 07 Security",
                        "| ID | Threat | Mitigations |",
                        "|---|---|---|",
                        "| TM-01 | **Tool poisoning** | sanitize + lint |",
                    ]
                )
                + "\n",
            )

            reqs = spec_trace.extract_requirements(root)
            self.assertEqual(
                set(reqs),
                {
                    "ARC-001",
                    "ARC-002",
                    "ARC-004",
                    "RTE-040",
                    "SEC-001",
                    "SEC-002",
                    "GQL-001",
                    "INV-01",
                    "INV-02",
                    "TM-01",
                },
            )
            self.assertNotIn("TST-888", reqs)
            self.assertNotIn(fake_inv, reqs)

            self.assertTrue(reqs["ARC-001"].is_must)
            self.assertTrue(reqs["ARC-002"].is_must)
            self.assertFalse(reqs["ARC-004"].is_must)
            self.assertTrue(reqs["RTE-040"].is_must)
            self.assertTrue(reqs["SEC-001"].is_must)
            self.assertFalse(reqs["SEC-002"].is_must)
            self.assertTrue(reqs["GQL-001"].withdrawn)
            self.assertFalse(reqs["GQL-001"].is_must)
            self.assertFalse(reqs["INV-01"].is_must)
            self.assertTrue(reqs["INV-02"].is_must)
            self.assertFalse(reqs["TM-01"].is_must)

    def test_scan_markers_across_roots_and_formats(self) -> None:
        # spec: ARC-004, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(
                root / "crates" / "gap-core" / "src" / "lib.rs",
                "// spec: ARC-001, INV-01\n#[test]\nfn ok() {}\n",
            )
            _write(
                root / "adapters" / "gap-mcp" / "tests" / "facade.rs",
                '#[doc = "spec: CMP-001"]\n#[test]\nfn f() { /* spec: TM-01; SEC-003 */ }\n',
            )
            _write(
                root / "tests" / "differential" / "run.py",
                "# spec: RUN-001\n",
            )
            _write(
                root / "conformance" / "gql" / "basic.json",
                json.dumps(
                    {
                        "name": "basic",
                        "class": ["Core"],
                        "spec": ["GQL-001", "GQL-002"],
                    },
                    indent=2,
                )
                + "\n",
            )
            # Documentation and build artifacts must not count as executable test markers.
            _write(
                root / "conformance" / "README.md",
                "Fixture format uses spec: GQL-040 in examples.\n",
            )
            _write(
                root / "crates" / "gap-core" / "target" / "debug" / "gen.rs",
                "// spec: GQL-041\n",
            )

            markers = spec_trace.scan_test_markers(root)
            self.assertEqual(
                set(markers),
                {"ARC-001", "INV-01", "CMP-001", "TM-01", "SEC-003", "RUN-001", "GQL-001", "GQL-002"},
            )
            self.assertNotIn("GQL-040", markers)
            self.assertNotIn("GQL-041", markers)
            self.assertEqual(markers["ARC-001"][0].file, "crates/gap-core/src/lib.rs")
            self.assertEqual(markers["ARC-001"][0].line, 1)
            self.assertEqual(markers["GQL-002"][0].file, "conformance/gql/basic.json")

    def test_warn_and_strict_modes(self) -> None:
        # spec: ARC-004, ADR-022
        unknown_arc = "ARC-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(
                root / "spec" / "01-architecture.md",
                "\n".join(
                    [
                        "- **ARC-001** Layering MUST hold.",
                        "- **ARC-002** Typed path MUST NOT use ML.",
                        "- **ARC-004** Public docs SHOULD build.",
                        "| ID | Invariant |",
                        "|---|---|",
                        "| INV-01 | Pinned generation. |",
                    ]
                )
                + "\n",
            )
            _write(
                root / "crates" / "gap-core" / "tests" / "t.rs",
                "// spec: ARC-001\n// spec: TST-999\n",
            )
            _write(
                root / "conformance" / "core" / "bad.json",
                json.dumps({"name": "bad", "spec": [unknown_arc]}) + "\n",
            )

            # Default mode is --warn: reports uncovered MUST and unknown IDs, exits 0.
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc_default = spec_trace.main(["--root", str(root)])
            text_default = out.getvalue()
            self.assertEqual(rc_default, 0)
            self.assertIn("ARC-002", text_default)
            self.assertNotIn("ARC-004", text_default)
            self.assertIn("TST-999", text_default)
            self.assertIn(unknown_arc, text_default)
            self.assertIn("WARN:", text_default)

            # Explicit --strict fails when uncovered MUSTs and/or unknown IDs exist.
            out_strict = io.StringIO()
            with contextlib.redirect_stdout(out_strict):
                rc_strict = spec_trace.main(["--root", str(root), "--strict"])
            self.assertEqual(rc_strict, 1)
            self.assertIn("FAIL:", out_strict.getvalue())

            # Remove unknown IDs while ARC-002 is still uncovered -> --strict still fails.
            (root / "conformance" / "core" / "bad.json").unlink()
            _write(root / "crates" / "gap-core" / "tests" / "t.rs", "// spec: ARC-001\n")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(spec_trace.main(["--root", str(root), "--strict"]), 1)

            # Cover ARC-002, re-introduce unknown ID -> --strict fails on unknown ID alone.
            _write(
                root / "crates" / "gap-core" / "tests" / "t.rs",
                "// spec: ARC-001, ARC-002\n// spec: TST-999\n",
            )
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(spec_trace.main(["--root", str(root), "--strict"]), 1)

            # Cover all MUSTs and remove unknown IDs -> --strict passes (non-MUST ARC-004 & INV-01 need no MUST marker).
            _write(
                root / "crates" / "gap-core" / "tests" / "t.rs",
                "// spec: ARC-001, ARC-002\n",
            )
            out_ok = io.StringIO()
            with contextlib.redirect_stdout(out_ok):
                rc_ok = spec_trace.main(["--root", str(root), "--strict"])
            self.assertEqual(rc_ok, 0)
            self.assertIn("OK:", out_ok.getvalue())

    def test_milestone_filtering(self) -> None:
        # spec: ARC-004, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(
                root / "spec" / "01-architecture.md",
                "\n".join(
                    [
                        "- **ARC-005** Snapshot MUST be immutable.",
                        "- **GQL-001** Documents MUST be valid GraphQL.",
                        "- **GQL-002** Formatting SHOULD be canonical.",
                        "- **ING-060** Code indexer MUST hash files.",
                    ]
                )
                + "\n",
            )
            _write(
                root / "work" / "work-packages.toml",
                "\n".join(
                    [
                        '[[wp]]',
                        'id = "WP-M0-08"',
                        'milestone = "M0"',
                        'spec_refs = ["ARC-005"]',
                        "",
                        '[[wp]]',
                        'id = "WP-M1-01"',
                        'milestone = "M1"',
                        'spec_refs = ["GQL-001", "GQL-002", "ADR-001"]',
                        "",
                        '[[wp]]',
                        'id = "WP-M2-02"',
                        'milestone = "M2"',
                        'spec_refs = ["ING-060"]',
                    ]
                )
                + "\n",
            )
            _write(
                root / "crates" / "gap-core" / "tests" / "snap.rs",
                "// spec: ARC-005\n",
            )
            _write(
                root / "crates" / "gap-ql" / "tests" / "parse.rs",
                "// spec: GQL-001\n",
            )

            # Whole-repo strict fails because M2's ING-060 is uncovered.
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(spec_trace.main(["--root", str(root), "--strict"]), 1)

            # M1 strict passes: GQL-001 is covered, M0's ARC-005 marker is valid, M2's ING-060 is out of M1 scope.
            out_m1 = io.StringIO()
            with contextlib.redirect_stdout(out_m1):
                rc_m1 = spec_trace.main(["--root", str(root), "--strict", "--milestone", "M1"])
            self.assertEqual(rc_m1, 0)
            self.assertIn("milestone=M1", out_m1.getvalue())

            # M2 strict fails because ING-060 has no test marker.
            out_m2 = io.StringIO()
            with contextlib.redirect_stdout(out_m2):
                rc_m2 = spec_trace.main(["--root", str(root), "--strict", "--milestone", "M2"])
            self.assertEqual(rc_m2, 1)
            self.assertIn("ING-060", out_m2.getvalue())

            # Unknown test ID still fails M1 strict.
            _write(
                root / "crates" / "gap-ql" / "tests" / "typo.rs",
                "// spec: TST-999\n",
            )
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(
                    spec_trace.main(["--root", str(root), "--strict", "--milestone", "M1"]),
                    1,
                )

            # Unknown milestone returns non-zero with an error message on stderr.
            err_m9 = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err_m9):
                rc_m9 = spec_trace.main(["--root", str(root), "--strict", "--milestone", "M9"])
            self.assertEqual(rc_m9, 1)
            self.assertIn("M9", err_m9.getvalue())

    def test_cli_missing_scan_dirs_and_footer(self) -> None:
        # spec: ARC-004, ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(
                root / "spec" / "01-architecture.md",
                "- **ARC-001** Layering MUST hold.\n- **ARC-004** Docs build.\n",
            )
            env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
            proc = subprocess.run(
                [sys.executable, "-B", str(SCRIPT_PATH), "--root", str(root), "--warn"],
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            lines = [ln for ln in proc.stdout.strip().splitlines() if ln.strip()]
            self.assertGreaterEqual(len(lines), 3)
            self.assertTrue(lines[-3].startswith("Scope:"))
            self.assertTrue(lines[-2].startswith("Issues:"))
            self.assertTrue(lines[-1].startswith("WARN:"))


if __name__ == "__main__":
    unittest.main()
