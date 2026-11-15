"""Deterministic GraphQL SDL to Python TypedDict/Literal code generator (GQL-003)."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re

_SCALAR_MAP: dict[str, str] = {
    "Int": "int",
    "Float": "float",
    "String": "str",
    "Boolean": "bool",
    "ID": "str",
    "JSON": "Any",
}


@dataclass(frozen=True)
class ParsedField:
    name: str
    py_type: str
    required: bool


@dataclass(frozen=True)
class ParsedObject:
    kind: str
    name: str
    fields: list[ParsedField]


@dataclass(frozen=True)
class ParsedEnum:
    name: str
    values: list[str]


@dataclass(frozen=True)
class ParsedSchema:
    enums: list[ParsedEnum]
    objects: list[ParsedObject]


def _strip_comments(sdl: str) -> str:
    no_docstrings = re.sub(r'"""[\s\S]*?"""', "", sdl)
    return re.sub(r"#[^\n]*", "", no_docstrings)


def _map_graphql_type(raw_type: str) -> tuple[str, bool]:
    cleaned = re.sub(r"@[^\s]+(\([^)]*\))?", "", raw_type).strip()
    non_null = cleaned.endswith("!")
    inner = cleaned[:-1].strip() if non_null else cleaned
    if inner.startswith("[") and inner.endswith("]"):
        elem_type, _ = _map_graphql_type(inner[1:-1])
        base = f"list[{elem_type}]"
        return (base, True) if non_null else (f"{base} | None", False)
    base_name = inner.rstrip("!")
    py_base = _SCALAR_MAP.get(base_name, base_name)
    return (py_base, True) if non_null else (f"{py_base} | None", False)


def parse_sdl_to_model(sdl: str) -> ParsedSchema:
    cleaned = _strip_comments(sdl)
    enums: list[ParsedEnum] = []
    objects: list[ParsedObject] = []

    for match in re.finditer(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{([^}]*)\}", cleaned):
        values = [
            tok.strip()
            for tok in match.group(2).split()
            if tok.strip() and not tok.strip().startswith("@")
        ]
        enums.append(ParsedEnum(name=match.group(1), values=values))

    for match in re.finditer(
        r"\b(type|input)\s+([A-Za-z_][A-Za-z0-9_]*)[^{]*\{([^}]*)\}", cleaned
    ):
        kind = match.group(1)
        name = match.group(2)
        body = match.group(3)
        fields: list[ParsedField] = []
        for raw_line in body.splitlines():
            line = raw_line.strip()
            if not line:
                continue
            f_match = re.match(
                r"^([A-Za-z_][A-Za-z0-9_]*)(?:\([^)]*\))?\s*:\s*([^\s@=]+(?:\s*!\s*|\s*\]\s*!?)?)",
                line,
            )
            if not f_match:
                continue
            py_type, required = _map_graphql_type(f_match.group(2))
            fields.append(ParsedField(name=f_match.group(1), py_type=py_type, required=required))
        objects.append(ParsedObject(kind=kind, name=name, fields=fields))

    return ParsedSchema(enums=enums, objects=objects)


def generate_types_from_sdl(sdl: str, source_label: str = "supergraph.graphql") -> str:
    model = parse_sdl_to_model(sdl)
    lines = [
        f'"""Code generated from {source_label} by gap-sdk codegen. DO NOT EDIT BY HAND."""',
        "",
        "from __future__ import annotations",
        "",
        "from typing import Any, Literal, TypedDict",
        "",
    ]

    for en in model.enums:
        if not en.values:
            lines.append(f"{en.name} = str")
        else:
            literals = ", ".join(repr(v) for v in en.values)
            lines.append(f"{en.name} = Literal[{literals}]")
        lines.append("")

    for obj in model.objects:
        if not obj.fields:
            lines.append(f"class {obj.name}(TypedDict):")
            lines.append("    pass")
            lines.append("")
            continue
        all_required = all(f.required for f in obj.fields)
        total_flag = "" if all_required else ", total=False"
        lines.append(f"class {obj.name}(TypedDict{total_flag}):")
        for f in obj.fields:
            lines.append(f"    {f.name}: {f.py_type}")
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate Python TypedDicts from GAP SDL")
    parser.add_argument("--sdl", required=True, help="Path to GraphQL SDL file")
    parser.add_argument("--out", help="Output .py file path (defaults to stdout)")
    args = parser.parse_args()
    sdl_text = Path(args.sdl).read_text(encoding="utf-8")
    rendered = generate_types_from_sdl(sdl_text, args.sdl)
    if args.out:
        Path(args.out).write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")


if __name__ == "__main__":
    main()
