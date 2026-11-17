#!/usr/bin/env python3
"""Update authorship, affiliation, ORCID, and repository URL across CITATION.cff, .zenodo.json, NOTICE,
paper/author_config.tex, and docs/paper/GAP-PAPER.md cleanly."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent


def validate_artifacts() -> None:
    cff = yaml.safe_load((ROOT / "CITATION.cff").read_text(encoding="utf-8"))
    if not cff.get("authors") or not cff.get("preferred-citation"):
        raise AssertionError("CITATION.cff missing authors or preferred-citation")
    zen = json.loads((ROOT / ".zenodo.json").read_text(encoding="utf-8"))
    if not zen.get("creators"):
        raise AssertionError(".zenodo.json missing creators")
    author_tex = (ROOT / "paper" / "author_config.tex").read_text(encoding="utf-8")
    if "\\GapAuthorFullName" not in author_tex:
        raise AssertionError("paper/author_config.tex missing \\GapAuthorFullName")
    print("OK: CITATION.cff, .zenodo.json, and paper/author_config.tex validated")


def update_all(
    given: str,
    family: str,
    affiliation: str,
    email: str,
    orcid: str | None,
    repo_url: str | None,
) -> None:
    full_name = f"{given} {family}".strip()

    # 1. CITATION.cff
    cff_path = ROOT / "CITATION.cff"
    cff = yaml.safe_load(cff_path.read_text(encoding="utf-8"))
    author_entry: dict[str, str] = {
        "given-names": given,
        "family-names": family,
        "affiliation": affiliation,
        "email": email,
    }
    if orcid:
        author_entry["orcid"] = orcid if orcid.startswith("https://") else f"https://orcid.org/{orcid}"
    cff["authors"] = [dict(author_entry)]
    cff["preferred-citation"]["authors"] = [dict(author_entry)]
    if repo_url:
        cff["repository-code"] = repo_url
        cff["url"] = repo_url
    cff_path.write_text(yaml.safe_dump(cff, sort_keys=False, allow_unicode=True), encoding="utf-8")

    # 2. .zenodo.json
    zen_path = ROOT / ".zenodo.json"
    zen = json.loads(zen_path.read_text(encoding="utf-8"))
    zen_name = full_name if family.startswith("(") else f"{family}, {given}"
    creator: dict[str, str] = {
        "name": zen_name,
        "affiliation": affiliation,
    }
    if orcid:
        creator["orcid"] = orcid.removeprefix("https://orcid.org/")
    zen["creators"] = [creator]
    if repo_url and zen.get("related_identifiers"):
        zen["related_identifiers"][0]["identifier"] = repo_url
    zen_path.write_text(json.dumps(zen, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    # 3. NOTICE line 2 copyright holder
    notice_path = ROOT / "NOTICE"
    notice_txt = notice_path.read_text(encoding="utf-8")
    notice_txt = re.sub(
        r"^Copyright 2026 .*$",
        f"Copyright 2026 {full_name} and GAP Contributors",
        notice_txt,
        flags=re.MULTILINE,
    )
    notice_path.write_text(notice_txt, encoding="utf-8")

    # 4. paper/author_config.tex
    orcid_disp = (
        f"https://orcid.org/{orcid.removeprefix('https://orcid.org/')}"
        if orcid
        else "ORCID pending registration"
    )
    tex_url = repo_url or cff.get("repository-code", "https://github.com/mike/GraphProtocol")
    (ROOT / "paper" / "author_config.tex").write_text(
        "% Author configuration updated automatically by dist/set-author.py\n"
        f"\\newcommand{{\\GapAuthorFullName}}{{{full_name}}}\n"
        f"\\newcommand{{\\GapAuthorAffiliation}}{{{affiliation}}}\n"
        f"\\newcommand{{\\GapAuthorEmail}}{{{email}}}\n"
        f"\\newcommand{{\\GapAuthorOrcid}}{{{orcid_disp}}}\n"
        f"\\newcommand{{\\GapRepoUrl}}{{{tex_url}}}\n",
        encoding="utf-8",
    )

    # 5. docs/paper/GAP-PAPER.md header line if present
    paper_md = ROOT / "docs" / "paper" / "GAP-PAPER.md"
    if paper_md.exists():
        txt = paper_md.read_text(encoding="utf-8")
        txt = re.sub(
            r"^\*\*Author:\*\* .*$",
            f"**Author:** {full_name} ({affiliation}) · `{email}` · {orcid_disp}",
            txt,
            flags=re.MULTILINE,
        )
        paper_md.write_text(txt, encoding="utf-8")

    print(f"Updated authorship metadata for `{full_name}` across CITATION.cff, .zenodo.json, NOTICE, and paper/author_config.tex")


def main() -> int:
    parser = argparse.ArgumentParser(description="Configure or validate GAP authorship & DOI metadata")
    parser.add_argument("--check", action="store_true", help="Validate existing citation metadata without editing")
    parser.add_argument("--given-names", help="Author given name(s)")
    parser.add_argument("--family-names", help="Author family name(s)")
    parser.add_argument("--affiliation", default="Independent Researcher", help="Author affiliation")
    parser.add_argument("--email", default="contributors@gap-protocol.local", help="Author contact email")
    parser.add_argument("--orcid", default=None, help="ORCID identifier (0000-0000-0000-0000)")
    parser.add_argument("--repo-url", default=None, help="Canonical public GitHub repository URL")
    args = parser.parse_args()

    if args.check:
        validate_artifacts()
        return 0

    if not args.given_names or not args.family_names:
        parser.error("--given-names and --family-names are required unless --check is passed")
    update_all(args.given_names, args.family_names, args.affiliation, args.email, args.orcid, args.repo_url)
    validate_artifacts()
    return 0


if __name__ == "__main__":
    sys.exit(main())
