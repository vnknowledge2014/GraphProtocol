#!/usr/bin/env python3
"""Unit tests for tools/wp.py (WP-M0-05; spec: ADR-022)."""
from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools import wp


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _toml_str(val: str) -> str:
    return json.dumps(val)


def _toml_list(vals: list[str]) -> str:
    return "[" + ", ".join(_toml_str(v) for v in vals) + "]"


def _render_wps(entries: list[dict[str, object]]) -> str:
    blocks: list[str] = []
    for entry in entries:
        lines = ["[[wp]]"]
        for key, value in entry.items():
            if isinstance(value, str):
                lines.append(f"{key} = {_toml_str(value)}")
            elif isinstance(value, list):
                lines.append(f"{key} = {_toml_list(value)}")
            else:
                raise TypeError(f"unsupported value type for {key}: {type(value)}")
        blocks.append("\n".join(lines))
    return "\n\n".join(blocks) + "\n"


def _make_wp(**overrides: object) -> dict[str, object]:
    base: dict[str, object] = {
        "id": "WP-M0-01",
        "title": "Bootstrap workspace",
        "milestone": "M0",
        "role": "coder",
        "size": "S",
        "depends_on": [],
        "owns": ["crates/gap-core/**"],
        "spec_refs": ["ARC-001", "ADR-022"],
        "deliverables": ["skeleton crate"],
        "acceptance": ["cargo test -p gap-core"],
    }
    base.update(overrides)
    return base


def _seed_spec_tree(root: Path) -> None:
    _write(
        root / "spec" / "01-architecture.md",
        "- **ARC-001** Layering MUST hold.\n| ID | Invariant |\n|---|---|\n| INV-01 | Pinned snapshot. |\n",
    )
    _write(
        root / "spec" / "02-gap-ql.md",
        "- **GQL-001** Valid GraphQL.\n",
    )
    _write(
        root / "docs" / "01-DECISIONS.md",
        "## ADR-022 — Agent execution protocol\n",
    )


@contextlib.contextmanager
def _patched_wp(root: Path):
    with (
        mock.patch.object(wp, "ROOT", root),
        mock.patch.object(wp, "WP_FILE", root / "work" / "work-packages.toml"),
        mock.patch.object(wp, "STATUS_FILE", root / "work" / "status.toml"),
    ):
        yield


def _run_validate(root: Path) -> tuple[int, str]:
    out = io.StringIO()
    with _patched_wp(root), contextlib.redirect_stdout(out):
        rc = wp.validate()
    return rc, out.getvalue()


class WpHelperTests(unittest.TestCase):
    def test_static_prefix_handles_literals_and_earliest_metachar(self) -> None:
        # spec: ADR-022
        self.assertEqual(wp.static_prefix("tools/wp.py"), "tools/wp.py")
        self.assertEqual(wp.static_prefix("crates/*/Cargo.toml"), "crates/")
        self.assertEqual(wp.static_prefix("crates/gap-core/**"), "crates/gap-core/")
        self.assertEqual(wp.static_prefix("docs/file?.md"), "docs/file")
        self.assertEqual(wp.static_prefix("dir/[a-z].rs"), "dir/")
        self.assertEqual(wp.static_prefix("sdks/{ts,py}/**"), "sdks/")
        # Earliest metacharacter wins regardless of order in "*?[{"
        self.assertEqual(wp.static_prefix("ab?cd*ef[g]{h}"), "ab")
        self.assertEqual(wp.static_prefix("ab*cd?ef[g]{h}"), "ab")
        self.assertEqual(wp.static_prefix("ab[c]d?ef*g{h}"), "ab")
        self.assertEqual(wp.static_prefix("ab{c}d[e]?f*g"), "ab")

    def test_globs_overlap_symmetric_and_disjoint(self) -> None:
        # spec: ADR-022
        self.assertTrue(wp.globs_overlap("Cargo.toml", "Cargo.toml"))
        self.assertTrue(wp.globs_overlap("crates/gap-core/**", "crates/gap-core/src/lib.rs"))
        self.assertTrue(wp.globs_overlap("crates/gap-core/src/lib.rs", "crates/gap-core/**"))
        self.assertTrue(wp.globs_overlap("crates/*/Cargo.toml", "crates/gap-ql/src/**"))
        self.assertFalse(wp.globs_overlap("crates/gap-core/**", "crates/gap-ql/**"))
        self.assertFalse(wp.globs_overlap("tools/wp.py", "tools/check_refs.py"))

    def test_ancestors_diamond_dag_and_cycle_detection(self) -> None:
        # spec: ADR-022
        dag = {
            "WP-M0-01": {"depends_on": []},
            "WP-M0-02": {"depends_on": ["WP-M0-01"]},
            "WP-M0-03": {"depends_on": ["WP-M0-01"]},
            "WP-M0-04": {"depends_on": ["WP-M0-02", "WP-M0-03"]},
        }
        anc = wp.ancestors(dag)
        self.assertEqual(anc["WP-M0-01"], set())
        self.assertEqual(anc["WP-M0-02"], {"WP-M0-01"})
        self.assertEqual(anc["WP-M0-03"], {"WP-M0-01"})
        self.assertEqual(anc["WP-M0-04"], {"WP-M0-01", "WP-M0-02", "WP-M0-03"})

        two_cycle = {
            "WP-M0-01": {"depends_on": ["WP-M0-02"]},
            "WP-M0-02": {"depends_on": ["WP-M0-01"]},
        }
        with self.assertRaisesRegex(ValueError, r"^cycle: WP-M0-01 -> WP-M0-02 -> WP-M0-01$"):
            wp.ancestors(two_cycle)

        self_cycle = {"WP-M0-01": {"depends_on": ["WP-M0-01"]}}
        with self.assertRaisesRegex(ValueError, r"^cycle: WP-M0-01 -> WP-M0-01$"):
            wp.ancestors(self_cycle)


class WpValidateTests(unittest.TestCase):
    def test_validate_clean_fixture_passes(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            entries = [
                _make_wp(
                    id="WP-M0-01",
                    milestone="M0",
                    role="coder",
                    size="S",
                    owns=["crates/gap-core/**"],
                    spec_refs=["ARC-001", "INV-01"],
                ),
                _make_wp(
                    id="WP-M0-02",
                    milestone="M0",
                    role="tester",
                    size="M",
                    depends_on=["WP-M0-01"],
                    owns=["tools/test_wp.py"],
                    spec_refs=["ADR-022"],
                ),
                _make_wp(
                    id="WP-M1-01",
                    milestone="M1",
                    role="analyzer",
                    size="L",
                    depends_on=["WP-M0-02"],
                    owns=[],
                    spec_refs=["GQL-*"],
                ),
                _make_wp(
                    id="WP-M5-01",
                    milestone="M5",
                    role="reviewer",
                    size="S",
                    depends_on=["WP-M1-01"],
                    owns=[],
                    spec_refs=["GQL-001"],
                ),
                _make_wp(
                    id="WP-R1-01",
                    milestone="R1",
                    role="analyzer",
                    size="M",
                    depends_on=["WP-M5-01"],
                    owns=[],
                    spec_refs=["ADR-022"],
                ),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))
            _write(root / "work" / "status.toml", '["WP-M0-01"]\nstate = "done"\n')

            rc, out = _run_validate(root)
            self.assertEqual(rc, 0, msg=out)
            self.assertEqual(out.strip(), "OK: 5 work packages M0=2, M1=1, M5=1, R1=1")

    def test_validate_dependency_cycle_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            entries = [
                _make_wp(id="WP-M0-01", depends_on=["WP-M0-02"], owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M0-02", depends_on=["WP-M0-01"], owns=["crates/gap-ql/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("VALIDATION FAILED:", out)
            self.assertIn("cycle: WP-M0-01 -> WP-M0-02 -> WP-M0-01", out)

    def test_validate_duplicate_id_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            entries = [
                _make_wp(id="WP-M0-01", owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M0-01", owns=["crates/gap-ql/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("VALIDATION FAILED:", out)
            self.assertIn("WP-M0-01: duplicate id", out)

    def test_validate_unknown_dependency_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            entries = [_make_wp(id="WP-M0-01", depends_on=["WP-M0-99"])]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: depends on unknown WP-M0-99", out)

    def test_validate_dependency_on_later_milestone_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            entries = [
                _make_wp(id="WP-M0-01", milestone="M0", depends_on=["WP-M1-01"], owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M1-01", milestone="M1", depends_on=[], owns=["crates/gap-ql/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: depends on later-milestone WP-M1-01", out)

    def test_validate_bad_size_including_xl_fails(self) -> None:
        # spec: ADR-022
        for bad_size in ("XL", "XS", ""):
            with self.subTest(size=bad_size), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                _seed_spec_tree(root)
                _write(root / "work" / "work-packages.toml", _render_wps([_make_wp(size=bad_size)]))

                rc, out = _run_validate(root)
                self.assertEqual(rc, 1)
                self.assertIn("WP-M0-01: size must be one of ['L', 'M', 'S'] (XL must be split)", out)

    def test_validate_bad_role_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(root / "work" / "work-packages.toml", _render_wps([_make_wp(role="planner")]))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: role must be one of ['analyzer', 'coder', 'reviewer', 'tester']", out)

    def test_validate_bad_id_format_fails(self) -> None:
        # spec: ADR-022
        cases = [
            ("WP-M8-01", "M8"),
            ("WP-M0-1", "M0"),
            ("WP-M0-001", "M0"),
            ("M0-01", "M0"),
            ("WP-R2-01", "R2"),
        ]
        for bad_id, ms in cases:
            with self.subTest(wid=bad_id), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                _seed_spec_tree(root)
                _write(
                    root / "work" / "work-packages.toml",
                    _render_wps([_make_wp(id=bad_id, milestone=ms)]),
                )

                rc, out = _run_validate(root)
                self.assertEqual(rc, 1)
                self.assertIn(f"{bad_id}: bad id format", out)

    def test_validate_id_prefix_mismatch_milestone_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(
                root / "work" / "work-packages.toml",
                _render_wps([_make_wp(id="WP-M0-01", milestone="M1")]),
            )

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: id prefix does not match milestone M1", out)

    def test_validate_missing_required_field_fails_for_every_field(self) -> None:
        # spec: ADR-022
        for field in wp.REQUIRED:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                _seed_spec_tree(root)
                entry = _make_wp()
                del entry[field]
                _write(root / "work" / "work-packages.toml", _render_wps([entry]))

                rc, out = _run_validate(root)
                self.assertEqual(rc, 1)
                self.assertIn(f"missing field '{field}'", out)

    def test_validate_empty_acceptance_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(root / "work" / "work-packages.toml", _render_wps([_make_wp(acceptance=[])]))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: needs >=1 executable acceptance command", out)

    def test_validate_empty_deliverables_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(root / "work" / "work-packages.toml", _render_wps([_make_wp(deliverables=[])]))

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: needs deliverables", out)

    def test_validate_coder_without_owns_fails_while_non_coder_passes(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(
                root / "work" / "work-packages.toml",
                _render_wps([_make_wp(role="coder", owns=[])]),
            )
            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("WP-M0-01: coder WP must declare 'owns'", out)

            _write(
                root / "work" / "work-packages.toml",
                _render_wps([_make_wp(role="reviewer", owns=[])]),
            )
            rc_ok, out_ok = _run_validate(root)
            self.assertEqual(rc_ok, 0, msg=out_ok)

    def test_validate_unknown_spec_ref_fails_and_wildcard_accepted(self) -> None:
        # spec: ADR-022
        unknown_ref = "GQL" + "-" + "999"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(
                root / "work" / "work-packages.toml",
                _render_wps([_make_wp(spec_refs=[unknown_ref])]),
            )
            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn(f"WP-M0-01: unknown spec ref '{unknown_ref}'", out)

            _write(
                root / "work" / "work-packages.toml",
                _render_wps([_make_wp(spec_refs=["GQL-*", "SEC-*", "ARC-001", "ADR-022"])]),
            )
            rc_ok, out_ok = _run_validate(root)
            self.assertEqual(rc_ok, 0, msg=out_ok)

    def test_validate_unknown_id_in_status_toml_fails(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            _write(root / "work" / "work-packages.toml", _render_wps([_make_wp(id="WP-M0-01")]))
            _write(root / "work" / "status.toml", '["WP-M0-99"]\nstate = "doing"\n')

            rc, out = _run_validate(root)
            self.assertEqual(rc, 1)
            self.assertIn("status.toml references unknown WP-M0-99", out)

    def test_validate_overlapping_ownership_unordered_vs_ordered(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _seed_spec_tree(root)
            # 1. Unordered WPs with overlapping globs must fail.
            unordered = [
                _make_wp(id="WP-M0-01", depends_on=[], owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M0-02", depends_on=[], owns=["crates/gap-core/src/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(unordered))
            rc_bad, out_bad = _run_validate(root)
            self.assertEqual(rc_bad, 1)
            self.assertIn(
                "concurrent WPs WP-M0-01 and WP-M0-02 have overlapping ownership: "
                "'crates/gap-core/**' vs 'crates/gap-core/src/**'",
                out_bad,
            )

            # 2. Same overlap ordered WP-M0-01 -> WP-M0-02 (a in anc[b]) is accepted.
            ordered_fwd = [
                _make_wp(id="WP-M0-01", depends_on=[], owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M0-02", depends_on=["WP-M0-01"], owns=["crates/gap-core/src/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(ordered_fwd))
            rc_fwd, out_fwd = _run_validate(root)
            self.assertEqual(rc_fwd, 0, msg=out_fwd)

            # 3. Same overlap ordered WP-M0-02 -> WP-M0-01 (b in anc[a]) is accepted.
            ordered_rev = [
                _make_wp(id="WP-M0-01", depends_on=["WP-M0-02"], owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M0-02", depends_on=[], owns=["crates/gap-core/src/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(ordered_rev))
            rc_rev, out_rev = _run_validate(root)
            self.assertEqual(rc_rev, 0, msg=out_rev)

            # 4. Transitive ordering WP-M0-01 -> WP-M0-02 -> WP-M0-03 is accepted.
            ordered_trans = [
                _make_wp(id="WP-M0-01", depends_on=[], owns=["crates/gap-core/**"]),
                _make_wp(id="WP-M0-02", depends_on=["WP-M0-01"], owns=["crates/gap-ql/**"]),
                _make_wp(id="WP-M0-03", depends_on=["WP-M0-02"], owns=["crates/gap-core/src/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(ordered_trans))
            rc_trans, out_trans = _run_validate(root)
            self.assertEqual(rc_trans, 0, msg=out_trans)


class WpSubcommandTests(unittest.TestCase):
    def test_cmd_ready_filters_todo_with_all_dependencies_done(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entries = [
                _make_wp(id="WP-M0-01", title="Done root", depends_on=[], owns=["a/**"]),
                _make_wp(id="WP-M0-02", title="Todo root", size="M", depends_on=[], owns=["b/**"]),
                _make_wp(id="WP-M0-03", title="Doing pkg", depends_on=["WP-M0-01"], owns=["c/**"]),
                _make_wp(id="WP-M0-04", title="Blocked pkg", depends_on=["WP-M0-01"], owns=["d/**"]),
                _make_wp(id="WP-M0-05", title="Ready child", size="S", depends_on=["WP-M0-01"], owns=["e/**"]),
                _make_wp(
                    id="WP-M0-06",
                    title="Partial deps child",
                    depends_on=["WP-M0-01", "WP-M0-02"],
                    owns=["f/**"],
                ),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))
            _write(
                root / "work" / "status.toml",
                "\n".join(
                    [
                        '["WP-M0-01"]',
                        'state = "done"',
                        '["WP-M0-03"]',
                        'state = "doing"',
                        '["WP-M0-04"]',
                        'state = "blocked"',
                    ]
                )
                + "\n",
            )

            out = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out):
                rc = wp.cmd_ready()
            self.assertEqual(rc, 0)
            lines = out.getvalue().strip().splitlines()
            self.assertEqual(
                lines,
                [
                    "WP-M0-02  [M]  Todo root",
                    "WP-M0-05  [S]  Ready child",
                ],
            )

            # Mark remaining ready roots doing/done so nothing is ready.
            _write(
                root / "work" / "status.toml",
                "\n".join(
                    [
                        '["WP-M0-01"]',
                        'state = "done"',
                        '["WP-M0-02"]',
                        'state = "doing"',
                        '["WP-M0-03"]',
                        'state = "doing"',
                        '["WP-M0-04"]',
                        'state = "blocked"',
                        '["WP-M0-05"]',
                        'state = "done"',
                    ]
                )
                + "\n",
            )
            out_empty = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out_empty):
                rc_empty = wp.cmd_ready()
            self.assertEqual(rc_empty, 0)
            self.assertEqual(out_empty.getvalue().strip(), "(nothing ready)")

    def test_cmd_set_writes_status_and_rejects_unknown_id_or_state(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entries = [
                _make_wp(id="WP-M0-01", owns=["a/**"]),
                _make_wp(id="WP-M0-02", owns=["b/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            # Unknown state rejected before touching status.toml.
            err_state = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stderr(err_state):
                rc_state = wp.cmd_set("WP-M0-01", "archived")
            self.assertEqual(rc_state, 1)
            self.assertIn("state must be one of ['blocked', 'doing', 'done', 'todo']", err_state.getvalue())
            self.assertFalse((root / "work" / "status.toml").exists())

            # Unknown WP id rejected before touching status.toml.
            err_id = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stderr(err_id):
                rc_id = wp.cmd_set("WP-M0-99", "doing")
            self.assertEqual(rc_id, 1)
            self.assertIn("unknown WP-M0-99", err_id.getvalue())
            self.assertFalse((root / "work" / "status.toml").exists())

            # Exercise all valid states and preserve multiple entries sorted.
            for state in ("doing", "blocked", "done", "todo", "doing"):
                out = io.StringIO()
                with _patched_wp(root), contextlib.redirect_stdout(out):
                    rc = wp.cmd_set("WP-M0-02", state)
                self.assertEqual(rc, 0)
                self.assertEqual(out.getvalue().strip(), f"WP-M0-02 -> {state}")

            with _patched_wp(root), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(wp.cmd_set("WP-M0-01", "done"), 0)
                loaded = wp.load_status()
            self.assertEqual(loaded, {"WP-M0-01": "done", "WP-M0-02": "doing"})
            raw = (root / "work" / "status.toml").read_text(encoding="utf-8")
            self.assertLess(raw.index('["WP-M0-01"]'), raw.index('["WP-M0-02"]'))

    def test_cmd_board_renders_per_milestone_state_counts(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entries = [
                _make_wp(id="WP-M0-01", milestone="M0", owns=["a/**"]),
                _make_wp(id="WP-M0-02", milestone="M0", owns=["b/**"]),
                _make_wp(id="WP-M0-03", milestone="M0", owns=["c/**"]),
                _make_wp(id="WP-M1-01", milestone="M1", owns=["d/**"]),
                _make_wp(id="WP-M1-02", milestone="M1", owns=["e/**"]),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))
            _write(
                root / "work" / "status.toml",
                "\n".join(
                    [
                        '["WP-M0-01"]',
                        'state = "done"',
                        '["WP-M0-02"]',
                        'state = "doing"',
                        '["WP-M1-01"]',
                        'state = "blocked"',
                    ]
                )
                + "\n",
            )

            out = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out):
                rc = wp.cmd_board()
            self.assertEqual(rc, 0)
            lines = out.getvalue().splitlines()
            self.assertEqual(len(lines), 3)
            self.assertEqual(lines[0].split(), ["milestone", "todo", "doing", "blocked", "done"])
            # M0 has 1 implicit todo (WP-M0-03), 1 doing, 0 blocked, 1 done.
            self.assertEqual(lines[1].split(), ["M0", "1", "1", "0", "1"])
            # M1 has 1 implicit todo (WP-M1-02), 0 doing, 1 blocked, 0 done.
            self.assertEqual(lines[2].split(), ["M1", "1", "0", "1", "0"])

    def test_cmd_dag_output_shape_depends_and_no_undocumented_fields(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entries = [
                _make_wp(id="WP-M0-01", milestone="M0", owns=["crates/gap-core/**"]),
                _make_wp(
                    id="WP-M1-02",
                    title="Second task",
                    milestone="M1",
                    role="reviewer",
                    depends_on=["WP-M0-01", "WP-M1-01"],
                    owns=[],
                    spec_refs=["GQL-001"],
                    deliverables=["review report"],
                    acceptance=["python3 tools/wp.py validate"],
                ),
                _make_wp(
                    id="WP-M1-01",
                    title="First task",
                    milestone="M1",
                    role="coder",
                    depends_on=["WP-M0-01"],
                    owns=["crates/gap-ql/**"],
                    spec_refs=["ARC-001"],
                    deliverables=["parser", "validator"],
                    acceptance=["cargo test -p gap-ql", "cargo clippy -p gap-ql"],
                ),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            # Unknown milestone returns exit code 1 with error on stderr.
            err_missing = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stderr(err_missing):
                rc_missing = wp.cmd_dag("M7", None)
            self.assertEqual(rc_missing, 1)
            self.assertIn("no WPs for M7", err_missing.getvalue())

            # Stdout JSON output for M1.
            out_json = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out_json):
                rc = wp.cmd_dag("M1", None)
            self.assertEqual(rc, 0)
            dag = json.loads(out_json.getvalue())
            self.assertEqual(set(dag.keys()), {"project_root", "tdd", "auto_commit", "tasks"})
            self.assertEqual(dag["project_root"], str(root))
            self.assertIs(dag["tdd"], True)
            self.assertIs(dag["auto_commit"], False)
            self.assertEqual(len(dag["tasks"]), 2)

            documented_task_keys = {"id", "title", "role", "depends_on", "system_prompt"}
            for task in dag["tasks"]:
                self.assertEqual(set(task.keys()), documented_task_keys)

            t1, t2 = dag["tasks"]
            # Sorted by WP id: WP-M1-01 -> id 1, WP-M1-02 -> id 2.
            self.assertEqual(t1["id"], 1)
            self.assertIn("WP-M1-01 First task", t1["title"])
            self.assertEqual(t1["role"], "coder")
            # Cross-milestone dependency WP-M0-01 is omitted from M1's intra-DAG depends_on.
            self.assertEqual(t1["depends_on"], [])
            self.assertIn("crates/gap-ql/**", t1["system_prompt"])

            self.assertEqual(t2["id"], 2)
            self.assertIn("WP-M1-02 Second task", t2["title"])
            self.assertEqual(t2["role"], "reviewer")
            self.assertEqual(t2["depends_on"], [1])
            self.assertIn("(read-only task)", t2["system_prompt"])

            # File output via --out matches stdout JSON.
            out_path = root / "dag-m1.json"
            out_msg = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out_msg):
                rc_file = wp.cmd_dag("M1", str(out_path))
            self.assertEqual(rc_file, 0)
            self.assertIn("(2 tasks)", out_msg.getvalue())
            self.assertEqual(json.loads(out_path.read_text(encoding="utf-8")), dag)

    def test_cmd_modules_and_critical_path(self) -> None:
        # spec: ADR-022
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entries = [
                _make_wp(
                    id="WP-M0-01",
                    size="S",
                    owns=["crates/gap-core/src/lib.rs", "crates/gap-core/src/error/**"],
                ),
                _make_wp(
                    id="WP-M0-02",
                    size="L",
                    depends_on=["WP-M0-01"],
                    owns=["crates/gap-core/src/clock.rs", "adapters/gap-mcp/src/server/*"],
                ),
            ]
            _write(root / "work" / "work-packages.toml", _render_wps(entries))

            out_mods = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out_mods):
                self.assertEqual(wp.cmd_modules(), 0)
            self.assertEqual(
                out_mods.getvalue().strip().splitlines(),
                [
                    "adapters/gap-mcp: server",
                    "crates/gap-core: clock, error",
                ],
            )

            out_crit = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out_crit):
                self.assertEqual(wp.cmd_critical("M0"), 0)
            crit_lines = out_crit.getvalue().strip().splitlines()
            self.assertIn("critical path: 5.5 agent-days over 2 WPs", crit_lines[0])
            self.assertEqual(crit_lines[1], "WP-M0-01 -> WP-M0-02")

            out_none = io.StringIO()
            with _patched_wp(root), contextlib.redirect_stdout(out_none):
                self.assertEqual(wp.cmd_critical("M7"), 1)
            self.assertEqual(out_none.getvalue().strip(), "no WPs")


if __name__ == "__main__":
    unittest.main()
