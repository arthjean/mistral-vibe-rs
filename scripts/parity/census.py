#!/usr/bin/env python3
"""A static census of the public boundaries a reference tree publishes.

The upstream sentinel (``.github/workflows/upstream-sentinel.yml``) compares
the pin with the newest upstream tag every day, and it must do so without
executing upstream code. This module reads an exported tree as text and
syntax only, through Python's ``ast`` parser and a few patterns, and never
imports from it. It records:

* ``methods``: the app-server methods ``SERVER_METHODS`` lists
  (``vibe/app_server/protocol.py``),
* ``config``: the fields every configuration schema class declares
  (``vibe/core/config/vibe_schema.py``),
* ``tools``: the builtin tool names, derived from each tool class the way the
  reference's ``BaseTool.get_name`` derives them (``vibe/core/tools/base.py``),
* ``cliFlags``: the long and short flags the Rust terminal client's argument
  definitions declare (``vibe/cli-rust/src``),
* ``scorecardPaths``: which reference paths the rows of ``docs/parity.md``
  cite exist in the tree.

Only names and paths are recorded, which the licensing boundary allows.

Usage::

    python3 scripts/parity/census.py <tree> [--against <tree>]

prints the census of ``<tree>`` as JSON, or with ``--against`` the diff from
that older tree to ``<tree>``. A shared module, not a driver.
"""

from __future__ import annotations

import argparse
import ast
import json
from pathlib import Path
import re
import sys
from typing import Any

#: The workspace root, whichever directory this runs from.
REPOSITORY = Path(__file__).resolve().parents[2]

PROTOCOL = "vibe/app_server/protocol.py"
CONFIG_SCHEMA = "vibe/core/config/vibe_schema.py"
BUILTINS = "vibe/core/tools/builtins"
CLI_SOURCES = "vibe/cli-rust/src"
SCORECARD = REPOSITORY / "docs/parity.md"

#: The boundaries a census records, in the order a summary lists them.
BOUNDARIES = ("methods", "config", "tools", "cliFlags", "scorecardPaths")


def _parse(path: Path) -> ast.Module | None:
    try:
        return ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (OSError, SyntaxError, UnicodeDecodeError):
        return None


def _strings(node: ast.AST | None) -> list[str]:
    if isinstance(node, (ast.Tuple, ast.List, ast.Set)):
        return [
            element.value
            for element in node.elts
            if isinstance(element, ast.Constant) and isinstance(element.value, str)
        ]
    return []


def server_methods(tree: Path) -> list[str]:
    module = _parse(tree / PROTOCOL)
    if module is None:
        return []
    for node in module.body:
        target = node.target if isinstance(node, ast.AnnAssign) else None
        if isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
        if isinstance(target, ast.Name) and target.id == "SERVER_METHODS":
            return sorted(set(_strings(node.value)))
    return []


def config_fields(tree: Path) -> list[str]:
    module = _parse(tree / CONFIG_SCHEMA)
    if module is None:
        return []
    fields = set()
    for node in module.body:
        if not isinstance(node, ast.ClassDef):
            continue
        for statement in node.body:
            if (
                isinstance(statement, ast.AnnAssign)
                and isinstance(statement.target, ast.Name)
                and not statement.target.id.startswith("_")
                and not _is_class_variable(statement.annotation)
            ):
                fields.add(f"{node.name}.{statement.target.id}")
    return sorted(fields)


def _is_class_variable(annotation: ast.expr) -> bool:
    if isinstance(annotation, ast.Subscript):
        annotation = annotation.value
    return isinstance(annotation, (ast.Name, ast.Attribute)) and (
        getattr(annotation, "id", None) == "ClassVar" or getattr(annotation, "attr", None) == "ClassVar"
    )


def _base_names(node: ast.ClassDef) -> list[str]:
    names = []
    for base in node.bases:
        if isinstance(base, ast.Subscript):
            base = base.value
        if isinstance(base, ast.Name):
            names.append(base.id)
        elif isinstance(base, ast.Attribute):
            names.append(base.attr)
    return names


def _declared_name(node: ast.ClassDef) -> str | None:
    """A name the class returns from its own ``get_name``, when it is a
    constant."""

    for statement in node.body:
        if isinstance(statement, ast.FunctionDef) and statement.name == "get_name":
            for inner in ast.walk(statement):
                if isinstance(inner, ast.Return) and isinstance(inner.value, ast.Constant):
                    if isinstance(inner.value.value, str):
                        return inner.value.value
    return None


def tool_names(tree: Path) -> list[str]:
    names = set()
    for path in sorted((tree / BUILTINS).rglob("*.py")):
        if any(part.startswith("_") for part in path.relative_to(tree / BUILTINS).parts):
            continue
        module = _parse(path)
        if module is None:
            continue
        for node in module.body:
            if isinstance(node, ast.ClassDef) and "BaseTool" in _base_names(node):
                names.add(_declared_name(node) or re.sub(r"(?<!^)(?=[A-Z])", "_", node.name).lower())
    return sorted(names)


ARGUMENT = re.compile(r"#\[arg\((?P<body>.*?)\)\]", re.DOTALL)
LONG = re.compile(r"\blong\s*=\s*\"(?P<name>[^\"]+)\"")
SHORT = re.compile(r"\bshort\s*=\s*'(?P<name>.)'")


def cli_flags(tree: Path) -> list[str]:
    flags = set()
    for path in sorted((tree / CLI_SOURCES).rglob("*.rs")):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for argument in ARGUMENT.finditer(text):
            body = argument.group("body")
            flags.update(f"--{match.group('name')}" for match in LONG.finditer(body))
            flags.update(f"-{match.group('name')}" for match in SHORT.finditer(body))
    return sorted(flags)


CITED = re.compile(r"`(?P<path>(?:vibe|harness|tests|build_backend)/[A-Za-z0-9_./-]+?)(?::\d[\d-]*)?`")


def cited_paths(scorecard: Path = SCORECARD) -> list[str]:
    """The reference paths the rows of the parity table cite."""

    paths = set()
    inside = False
    for line in scorecard.read_text(encoding="utf-8").splitlines():
        if line.startswith("## "):
            inside = line.strip() == "## Parity by part"
            continue
        if inside and line.startswith("| "):
            paths.update(match.group("path").rstrip("/.") for match in CITED.finditer(line))
    return sorted(paths)


def census(tree: Path, scorecard: Path = SCORECARD) -> dict[str, Any]:
    cited = cited_paths(scorecard)
    return {
        "methods": server_methods(tree),
        "config": config_fields(tree),
        "tools": tool_names(tree),
        "cliFlags": cli_flags(tree),
        "scorecardPaths": {
            "cited": len(cited),
            "missing": [path for path in cited if not (tree / path).exists()],
        },
    }


def diff(older: dict[str, Any], newer: dict[str, Any]) -> dict[str, Any]:
    """What ``newer`` adds and removes against ``older``, by boundary."""

    result: dict[str, Any] = {}
    for boundary in BOUNDARIES[:-1]:
        before, after = set(older.get(boundary, [])), set(newer.get(boundary, []))
        result[boundary] = {"added": sorted(after - before), "removed": sorted(before - after)}
    result["scorecardPaths"] = {"missing": newer["scorecardPaths"]["missing"]}
    return result


def summary(difference: dict[str, Any]) -> str:
    """One clause per boundary, such as ``methods +33/-5``."""

    parts = [
        f"{boundary} +{len(difference[boundary]['added'])}/-{len(difference[boundary]['removed'])}"
        for boundary in BOUNDARIES[:-1]
    ]
    parts.append(f"scorecard paths missing {len(difference['scorecardPaths']['missing'])}")
    return ", ".join(parts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tree", type=Path)
    parser.add_argument("--against", type=Path, help="an older tree to diff from")
    arguments = parser.parse_args()
    newer = census(arguments.tree)
    if arguments.against is None:
        print(json.dumps(newer, indent=2))
        return 0
    difference = diff(census(arguments.against), newer)
    print(json.dumps(difference, indent=2))
    print(summary(difference), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
