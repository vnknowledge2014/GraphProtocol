#!/usr/bin/env python3
"""Static integrity validator for arXiv LaTeX paper sources (`paper/main.tex` and `paper/figures/*.tex`)."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
PAPER_DIR = ROOT / "paper"


def strip_comments(tex: str) -> str:
    out_lines = []
    for line in tex.splitlines():
        # Strip unescaped % comments
        buf = []
        i = 0
        while i < len(line):
            ch = line[i]
            if ch == "\\" and i + 1 < len(line):
                buf.append(line[i : i + 2])
                i += 2
                continue
            if ch == "%":
                break
            buf.append(ch)
            i += 1
        out_lines.append("".join(buf))
    return "\n".join(out_lines)


def expand_inputs(path: Path, seen: set[Path] | None = None) -> str:
    seen = seen or set()
    resolved = path.resolve()
    if resolved in seen:
        raise AssertionError(f"Cyclic \\input detected at {path}")
    seen.add(resolved)
    text = path.read_text(encoding="utf-8")

    def repl(match: re.Match[str]) -> str:
        rel = match.group(1).strip()
        cand = path.parent / (rel if rel.endswith(".tex") else f"{rel}.tex")
        if not cand.is_file():
            cand_root = PAPER_DIR / (rel if rel.endswith(".tex") else f"{rel}.tex")
            if not cand_root.is_file():
                raise AssertionError(f"Missing \\input file referenced from {path.name}: {rel}")
            cand = cand_root
        return expand_inputs(cand, set(seen))

    return re.sub(r"\\input\{([^}]+)\}", repl, text)


def check_braces(text: str) -> None:
    depth = 0
    i = 0
    n = len(text)
    in_verbatim = False
    for line in text.splitlines():
        if "\\begin{lstlisting}" in line:
            in_verbatim = True
        if "\\end{lstlisting}" in line:
            in_verbatim = False
            continue
        if in_verbatim:
            continue
        j = 0
        while j < len(line):
            ch = line[j]
            if ch == "\\" and j + 1 < len(line) and line[j + 1] in "{}%&$#_\\":
                j += 2
                continue
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth < 0:
                    raise AssertionError(f"Unmatched closing brace `}}` in line: {line}")
            j += 1
    if depth != 0:
        raise AssertionError(f"Unbalanced braces in expanded LaTeX (final depth={depth})")


def check_environments(text: str) -> None:
    stack: list[str] = []
    token_re = re.compile(r"\\(begin|end)\{([^}]+)\}")
    for match in token_re.finditer(text):
        kind, env = match.group(1), match.group(2)
        if kind == "begin":
            stack.append(env)
        else:
            if not stack or stack[-1] != env:
                raise AssertionError(f"Mismatched environment \\end{{{env}}} (stack={stack})")
            stack.pop()
    if stack:
        raise AssertionError(f"Unclosed LaTeX environment(s): {stack}")


def check_labels_refs_and_cites(text: str) -> tuple[int, int, int]:
    labels = set(re.findall(r"\\label\{([^}]+)\}", text))
    refs = set(re.findall(r"\\(?:ref|eqref|autoref)\{([^}]+)\}", text))
    missing_refs = sorted(r for r in refs if r not in labels)
    if missing_refs:
        raise AssertionError(f"Undefined \\ref labels: {missing_refs}")

    bibitems = set(re.findall(r"\\bibitem\{([^}]+)\}", text))
    bib_keys = set(
        re.findall(r"@\w+\{([^,\s]+),", (PAPER_DIR / "references.bib").read_text(encoding="utf-8"))
    )
    if bibitems != bib_keys:
        raise AssertionError(
            f"Mismatch between embedded thebibliography ({sorted(bibitems)}) "
            f"and references.bib ({sorted(bib_keys)})"
        )

    cites: set[str] = set()
    for grp in re.findall(r"\\cite(?:\[[^\]]*\])?\{([^}]+)\}", text):
        for key in grp.split(","):
            cites.add(key.strip())
    missing_cites = sorted(c for c in cites if c not in bibitems)
    if missing_cites:
        raise AssertionError(f"Undefined \\cite keys: {missing_cites}")

    return len(labels), len(refs), len(cites)


def main() -> int:
    main_tex = PAPER_DIR / "main.tex"
    if not main_tex.is_file():
        raise AssertionError("paper/main.tex not found")
    expanded = expand_inputs(main_tex)
    clean = strip_comments(expanded)
    check_braces(clean)
    check_environments(clean)
    n_labels, n_refs, n_cites = check_labels_refs_and_cites(clean)
    print(
        f"OK: paper/main.tex validated cleanly "
        f"({len(expanded.splitlines())} expanded lines, {n_labels} labels, "
        f"{n_refs} resolved refs, {n_cites} resolved citations matching references.bib)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
