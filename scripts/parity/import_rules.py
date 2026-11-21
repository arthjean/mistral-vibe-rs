"""The import rules of the parity scripts, read from their syntax.

AGENTS.md ("The behavioral oracle") states how the modules under
``scripts/parity/`` may import one another. This checks it without running
any of them, through :mod:`ast`, so it needs no reference environment.

A *driver* is a capture entry point. It is a module that either

- ``scripts/parity/manifest.json`` names in an entry's ``driver`` command or
  ``scripts/parity/repin_plan.json`` names in a stage (a runtime oracle such as
  ``crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py`` included), or
- parses a command line: a module of ``scripts/parity/`` that imports
  :mod:`argparse` and runs under a top-level ``if __name__ == "__main__":``
  guard (``repin.py``, ``census.py`` and ``live_summary.py`` among them).

A *table* is a module of ``scripts/parity/`` named ``<driver>_cases.py`` or
``<driver>_scenarios.py``; ``<driver>.py`` owns it. Every other module of
``scripts/parity/`` is *shared*.

The rules, over every ``import`` and ``from ... import`` a module holds, at
its top level or inside a function:

1. A driver never imports another driver.
2. A table imports the standard library only (``sys.stdlib_module_names``).
3. Only the driver that owns a table imports it.

A dynamic import (``importlib.import_module``) escapes the check.

Run ``python3 scripts/parity/import_rules.py [<repository root>]``: it prints
one line per broken rule, naming the module, the line and the import, and
exits 1 when any rule is broken. ``crates/vibe-core/src/parity/import_rules_tests.rs``
runs it inside ``cargo test``.
"""

from __future__ import annotations

import ast
from dataclasses import dataclass
import json
from pathlib import Path
import sys

SCRIPTS = Path("scripts/parity")
TABLE_SUFFIXES = ("_cases", "_scenarios")


@dataclass(frozen=True)
class Import:
    module: str
    line: int

    @property
    def top(self) -> str:
        return self.module.split(".", 1)[0]


def imports(tree: ast.AST) -> list[Import]:
    """Every module ``tree`` imports, a relative import written with its
    leading dots."""

    found: list[Import] = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            found.extend(Import(alias.name, node.lineno) for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            found.append(Import("." * node.level + (node.module or ""), node.lineno))
    return sorted(found, key=lambda item: (item.line, item.module))


def parses_a_command_line(tree: ast.Module) -> bool:
    guarded = any(
        isinstance(node, ast.If)
        and isinstance(node.test, ast.Compare)
        and isinstance(node.test.left, ast.Name)
        and node.test.left.id == "__name__"
        for node in tree.body
    )
    return guarded and any(item.top == "argparse" for item in imports(tree))


def named_drivers(root: Path) -> set[Path]:
    """The ``.py`` files the manifest's driver commands and the re-pin plan
    name, relative to ``root``."""

    named: set[Path] = set()
    manifest = root / SCRIPTS / "manifest.json"
    if manifest.is_file():
        for entry in json.loads(manifest.read_text(encoding="utf-8")).get("entries", []):
            named.update(Path(word) for word in entry.get("driver") or [] if word.endswith(".py"))
    plan = root / SCRIPTS / "repin_plan.json"
    if plan.is_file():
        by_name = {path.name: path for path in named}
        for stage in json.loads(plan.read_text(encoding="utf-8")).get("stages", []):
            for word in stage.get("drivers", []):
                if word.endswith(".py"):
                    named.add(by_name.get(word, SCRIPTS / word))
    return named


def check(root: Path) -> tuple[list[str], dict[str, int]]:
    """The broken rules under ``root``, one line each, and how many modules
    of each kind the check read."""

    trees: dict[Path, ast.Module] = {}
    for path in sorted((root / SCRIPTS).glob("*.py")):
        trees[path.relative_to(root)] = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    named = named_drivers(root)
    problems: list[str] = []
    for path in sorted(named - set(trees)):
        if (root / path).is_file():
            trees[path] = ast.parse((root / path).read_text(encoding="utf-8"), filename=str(path))
        else:
            problems.append(f"{path.as_posix()}: named as a driver, but no such file")

    local = {path.stem for path in trees if path.parent == SCRIPTS}
    drivers = {path for path in trees if path in named or (path.parent == SCRIPTS and parses_a_command_line(trees[path]))}
    driver_names = {path.stem for path in drivers if path.parent == SCRIPTS}
    tables = {path for path in trees if path.parent == SCRIPTS and path.stem.endswith(TABLE_SUFFIXES)}
    owners = {path.stem: path.stem.rsplit("_", 1)[0] for path in tables}
    for path in sorted(tables):
        if owners[path.stem] not in driver_names:
            problems.append(f"{path.as_posix()}: table without a driver `{owners[path.stem]}.py` to own it")

    stdlib = sys.stdlib_module_names
    for path, tree in sorted(trees.items()):
        where = path.as_posix()
        for item in imports(tree):
            if path in drivers and item.top in driver_names and item.top != path.stem:
                problems.append(f"{where}:{item.line}: driver imports driver `{item.module}`")
            if path in tables and (item.module.startswith(".") or item.top not in stdlib):
                kind = "shared module" if item.top in local else "module outside the standard library"
                problems.append(f"{where}:{item.line}: table imports {kind} `{item.module}`")
            if item.top in owners and owners[item.top] != path.stem and item.top != path.stem:
                problems.append(
                    f"{where}:{item.line}: imports `{item.module}`, the table of `{owners[item.top]}.py`"
                )
    counts = {"drivers": len(drivers), "tables": len(tables), "shared": len(trees) - len(drivers) - len(tables)}
    return problems, counts


def main(arguments: list[str]) -> int:
    if len(arguments) > 1:
        print("usage: import_rules.py [<repository root>]", file=sys.stderr)
        return 2
    root = Path(arguments[0]) if arguments else Path(__file__).resolve().parents[2]
    problems, counts = check(root)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        return 1
    print(
        f"{counts['drivers']} drivers, {counts['tables']} tables and {counts['shared']} shared modules "
        "keep the import rules"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
