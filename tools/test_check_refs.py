#!/usr/bin/env python3
"""Unit tests for tools/check_refs.py (WP-M0-05; spec: ADR-022)."""
from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools import check_refs


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _seed_clean_repo(root: Path) -> None:
    _write(
        root / "spec" / "01-architecture.md",
        "\n".join(
            [
                "# 01 Architecture",
                "- **ARC-001** The layer rule MUST hold.",
                "| ID | Invariant |",
                "|---|---|",
                "| INV-01 | Single snapshot generation per request. |",
            ]
        )
        + "\n",
    )
    _write(
        root / "spec" / "02-gap-ql.md",
        "\n".join(
            [
                "# 02 GAP-QL",
                "- **GQL-001** Executable GraphQL profile.",
                "- **RTE-040 (Unmapped domain).** Deduplicated event.",
            ]
        )
        + "\n",
    )
    _write(
        root / "spec" / "07-security.md",
        "\n".join(
            [
                "# 07 Security",
                "| ID | Threat | Mitigation |",
                "|---|---|---|",
                "| TM-01 | Tool poisoning | Hash pinning |",
            ]
        )
        + "\n",
    )
    _write(
        root / "docs" / "03-VERIFICATION.md",
        "- **VER-001** Every MUST maps to an automated test.\n",
    )
    _write(
        root / "docs" / "01-DECISIONS.md",
        "\n".join(
            [
                "# Decisions",
                "## ADR-001 — GraphQL profile",
                "## ADR-022 — Agent execution protocol",
            ]
        )
        + "\n",
    )


def _run_check_refs(root: Path) -> tuple[int, str]:
    out = io.StringIO()
    with mock.patch.object(check_refs, "ROOT", root), contextlib.redirect_stdout(out):
        rc = check_refs.main()
    return rc, out.getvalue()


class CheckRefsTests(unittest.TestCase):
    def test_defined_ids_accepted_across_spec_tables_verification_and_adrs(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(root / "README.md", "Covers ARC-001, GQL-001, RTE-040 and VER-001.\n")
            _write(root / "work" / "work-packages.toml", 'spec_refs = ["INV-01", "ADR-022"]\n')
            _write(root / "tools" / "sample.py", "# spec: TM-01, ADR-001\n")
            _write(root / "proto" / "gap" / "ngc" / "v1" / "ngc.proto", "// See ARC-001 and INV-01.\n")

            with mock.patch.object(check_refs, "ROOT", root):
                defined = check_refs.defined_ids()
            self.assertEqual(
                defined,
                {
                    "ARC-001",
                    "GQL-001",
                    "RTE-040",
                    "VER-001",
                    "INV-01",
                    "TM-01",
                    "ADR-001",
                    "ADR-022",
                },
            )

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 8 ids defined, no dangling references")

    def test_dangling_reference_in_markdown_fails_with_exit_one(self) -> None:
        # spec: ADR-022
        dangling_id = "GQL" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(root / "docs" / "notes.md", f"Broken reference to {dangling_id}.\n")

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 1)
            self.assertIn("DANGLING REFERENCES:", out)
            self.assertIn(f"{dangling_id}: docs/notes.md", out)

    def test_dangling_reference_in_work_toml_fails_with_exit_one(self) -> None:
        # spec: ADR-022
        dangling_id = "ARC" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(root / "work" / "work-packages.toml", f'spec_refs = ["{dangling_id}"]\n')

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 1)
            self.assertIn("DANGLING REFERENCES:", out)
            self.assertIn(f"{dangling_id}: work/work-packages.toml", out)

    def test_dangling_reference_in_tools_py_fails_with_exit_one(self) -> None:
        # spec: ADR-022
        dangling_id = "SEC" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(root / "tools" / "helper.py", f"# spec: {dangling_id}\n")

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 1)
            self.assertIn("DANGLING REFERENCES:", out)
            self.assertIn(f"{dangling_id}: tools/helper.py", out)

    def test_dangling_reference_in_proto_fails_with_exit_one(self) -> None:
        # spec: ADR-022
        dangling_id = "NGC" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(
                root / "proto" / "gap" / "ngc" / "v1" / "ngc.proto",
                f'syntax = "proto3"; // {dangling_id}\n',
            )

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 1)
            self.assertIn("DANGLING REFERENCES:", out)
            self.assertIn(f"{dangling_id}: proto/gap/ngc/v1/ngc.proto", out)

    def test_docs_evidence_and_excluded_trees_are_ignored(self) -> None:
        # spec: ADR-022
        d1 = "OPS" + "-" + "999"
        d2 = "CMP" + "-" + "999"
        d3 = "RUN" + "-" + "999"
        d4 = "ING" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(root / "docs" / "evidence" / "crate-versions.md", f"Mentions {d1}.\n")
            _write(root / "third_party" / "specs" / "notes.md", f"Mentions {d2}.\n")
            _write(root / "target" / "doc" / "summary.md", f"Mentions {d3}.\n")
            _write(root / "node_modules" / "pkg" / "README.md", f"Mentions {d4}.\n")

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 8 ids defined, no dangling references")

    def test_check_refs_py_itself_is_ignored(self) -> None:
        # spec: ADR-022
        dangling_id = "RTE" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            _write(root / "tools" / "check_refs.py", f"# docstring example {dangling_id}\n")

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 8 ids defined, no dangling references")

            # Same content in another tools/*.py file must fail.
            _write(root / "tools" / "other.py", f"# docstring example {dangling_id}\n")
            rc_bad, out_bad = _run_check_refs(root)
            self.assertEqual(rc_bad, 1)
            self.assertIn(f"{dangling_id}: tools/other.py", out_bad)

    def test_all_area_prefixes_and_unformatted_mentions_are_enforced(self) -> None:
        # spec: ADR-022
        areas = (
            "ARC",
            "GQL",
            "NGC",
            "RTE",
            "ING",
            "RUN",
            "SEC",
            "OPS",
            "CMP",
            "VER",
            "INV",
            "TM",
            "ADR",
        )
        for area in areas:
            suffix = "99" if area in ("INV", "TM") else "999"
            fake = f"{area}-{suffix}"
            with self.subTest(area=area), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                _seed_clean_repo(root)
                _write(root / "README.md", f"Reference {fake}.\n")

                rc, out = _run_check_refs(root)
                self.assertEqual(rc, 1)
                self.assertIn(f"{fake}: README.md", out)

        # Unbolded requirement, inline INV/TM, and H3 ADR heading do not define IDs.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_clean_repo(root)
            unbold_gql = "GQL" + "-" + "888"
            inline_inv = "INV" + "-" + "88"
            inline_tm = "TM" + "-" + "88"
            h3_adr = "ADR" + "-" + "888"
            _write(root / "spec" / "03-extra.md", f"- {unbold_gql} Not bolded.\n")
            _write(
                root / "spec" / "01-architecture.md",
                (root / "spec" / "01-architecture.md").read_text(encoding="utf-8")
                + f"Prose mention of {inline_inv}.\n",
            )
            _write(
                root / "spec" / "07-security.md",
                (root / "spec" / "07-security.md").read_text(encoding="utf-8")
                + f"Prose mention of {inline_tm}.\n",
            )
            _write(
                root / "docs" / "01-DECISIONS.md",
                (root / "docs" / "01-DECISIONS.md").read_text(encoding="utf-8")
                + f"### {h3_adr} — Wrong heading level\n",
            )

            rc, out = _run_check_refs(root)
            self.assertEqual(rc, 1)
            for expected_missing in (unbold_gql, inline_inv, inline_tm, h3_adr):
                self.assertIn(expected_missing, out)


if __name__ == "__main__":
    unittest.main()
