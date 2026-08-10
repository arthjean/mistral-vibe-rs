#!/usr/bin/env python3
"""Capture what the pinned Python reference's file index answers.

The reference splits `@` completion into a store, a rules compiler, a watcher
and a ranking completer, and three of those four take everything they need as
arguments. ``FileIndexStore`` receives its ignore rules and its stats object,
``IgnoreRules.should_ignore`` is a pure function of three strings once a root
is compiled, and ``PathCompleter._score_matches`` takes a list of entries and a
search context. A capture therefore drives the real reference objects over
scratch trees this script writes itself, with no watcher thread, no network and
no editor.

Five families come out, and are what the Rust replay compares:

``constants``    the caps, the threshold, the ASCII limit and the 36 defaults
``ignoreRules``  the ``should_ignore`` verdict per ``(rel, name, is_dir)``
``walk``         the entry set a rebuild holds per fixture tree
``changes``      the entry set and the stats after each ``apply_changes`` call
``ranking``      the ordered candidates and the ``MatchRank`` tuple per query

Two artifacts come out of a run::

    .parity/autocompletion-corpus.json                the full capture, gitignored
    crates/vibe-cli/tests/autocompletion/corpus.json  the committed corpus

Every name in the corpus is supplied by the fixtures below, which this script
authors: no reference prose, no reference source and no machine path is
recorded, only relative paths, verdicts, counts and scores.

Two normalizations are applied and both are recorded in the corpus ``note``.
The reference holds its index in ``scandir`` order after a rebuild and in
relative-path order only after an incremental update invalidated the cached
order, so entry lists are sorted by relative path before being recorded and the
ranking family is driven over a sorted entry list. Fuzzy scores are floats
upstream and integers here, so a score is recorded in hundredths, which is the
scale ``crates/vibe-cli/src/tui/completion/fuzzy.rs`` already works in; the
capture fails if a score is not an exact hundredth.

Usage::

    scripts/parity/autocompletion.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter, reading the pinned commit through ``git archive`` so
the checkout is never moved.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

#: The variable that relocates the checkout, named in the failure a machine
#: without one reads first.
REFERENCE_VARIABLE = "VIBE_REFERENCE"

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/autocompletion-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-cli/tests/autocompletion/corpus.json")
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Fuzzy scores are floats upstream. Hundredths make them exact integers, which
#: is the scale this port's matcher already computes in.
SCORE_SCALE = 100


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def _git(reference: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git {' '.join(arguments)} failed in {reference}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def resolve_reference(reference: Path, expected: str) -> dict[str, str]:
    """The pinned commit, read out of the checkout without depending on its HEAD."""

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; point {REFERENCE_VARIABLE} or "
            "--reference at one"
        )
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs.

    ``git archive`` writes the commit's contents without moving HEAD, creating a
    branch or adding a worktree, so a checkout parked on another revision is
    still an oracle for the pin.
    """

    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = tree / "vibe" / "__init__.py"
    if marker.is_file():
        return tree
    tree.mkdir(parents=True, exist_ok=True)
    archive = tree.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(tree, filter="data")
    archive.unlink(missing_ok=True)
    if not marker.is_file():
        raise OracleError(f"the extracted tree at {tree} carries no `vibe` package")
    return tree


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


def reexecute_with_reference_interpreter(
    reference: Path, override: Path | None, tree: Path
) -> None:
    """Re-runs this script under an interpreter importing the *pinned* tree."""

    if os.environ.get(_REEXEC_MARKER) == str(tree):
        if not _imports_pinned_vibe(tree):
            raise OracleError(
                f"the reference interpreter did not import `vibe` from {tree}"
            )
        return
    candidates = [override] if override else []
    candidates += [
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    interpreter = next((c for c in candidates if c and c.is_file()), None)
    if interpreter is None:
        raise OracleError(
            f"no interpreter can import `vibe`; looked for a virtual environment in {reference}"
        )
    environment = dict(os.environ)
    environment[_REEXEC_MARKER] = str(tree)
    environment["PYTHONPATH"] = os.pathsep.join(
        [
            str(tree),
            *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else []),
        ]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


class _SocketGuard:
    """Fails the capture the moment anything tries to reach a network.

    Nothing in the index has a reason to open a socket, which is exactly why an
    attempt must fail the run rather than pass unnoticed: a corpus written from
    a live backend is not a corpus of the pinned reference.
    """

    def __init__(self) -> None:
        self.attempts: list[str] = []

    def install(self) -> None:
        guard = self

        def refuse(name: str) -> Any:
            def raiser(*arguments: Any, **keywords: Any) -> Any:
                guard.attempts.append(name)
                raise OracleError(
                    f"the capture attempted network access through {name}"
                )

            return raiser

        socket.socket.connect = refuse("socket.connect")  # type: ignore[method-assign]
        socket.socket.connect_ex = refuse("socket.connect_ex")  # type: ignore[method-assign]
        socket.create_connection = refuse("socket.create_connection")  # type: ignore[assignment]
        socket.getaddrinfo = refuse("socket.getaddrinfo")  # type: ignore[assignment]


GUARD = _SocketGuard()


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------

#: What a fixture file holds. The index never reads a file's bytes, so one
#: constant covers every fixture and keeps the corpus free of invented prose.
FILE_BODY = "fixture\n"

#: The scratch trees every family is measured over. A path ending in `/` is a
#: directory; every other path is a file whose parents are created with it.
#: Names are chosen here so the corpus carries fixture-supplied vocabulary and
#: nothing the reference authored.
FIXTURES: dict[str, dict[str, Any]] = {
    "plain": {
        "tree": [
            "readme.md",
            "src/main.rs",
            "src/lib.rs",
            "src/render/mod.rs",
            "src/render/table.rs",
            "docs/guide.md",
            "docs/api/index.md",
            "empty-dir/",
        ],
        "gitignore": None,
    },
    "ignored": {
        "tree": [
            "keep.txt",
            "build/output.bin",
            "target/debug/app",
            "node_modules/left-pad/index.js",
            "__pycache__/module.cpython-312.pyc",
            "src/app.pyc",
            "logs/run.log",
            "notes.log",
            "vendor/pkg/file.go",
            ".coverage",
            "bundle.min.js",
            "src/vendor/keep.go",
        ],
        "gitignore": None,
    },
    "gitignored": {
        "tree": [
            "keep.log",
            "drop.log",
            "report-7.txt",
            "report-x.txt",
            "alpha.tmp",
            "blpha.tmp",
            "secrets/key.pem",
            "public/asset.css",
            "nested/build/artifact.o",
            "build/artifact.o",
            "hash#name.txt",
            "spaced name.txt",
        ],
        "gitignore": (
            "# a comment line\n"
            "\n"
            "*.log\n"
            "!keep.log\n"
            "report-[0-9].txt\n"
            "[!a]lpha.tmp\n"
            "secrets/\n"
            "/build/\n"
            "trailing.txt # an inline comment\n"
            "!  \n"
        ),
    },
    "hidden": {
        "tree": [
            ".config/settings.toml",
            ".hidden-file",
            "visible.txt",
            ".git/HEAD",
            "src/.env",
            "src/index.ts",
        ],
        "gitignore": None,
    },
    "ranking": {
        "tree": [
            "Cargo.toml",
            "Cargo.lock",
            "src/main.rs",
            "src/lib.rs",
            "src/parser/mod.rs",
            "src/parser/tokens.rs",
            "src/parser/tokenStream.rs",
            "src/render/table.rs",
            "src/render/tableCell.rs",
            "tests/parser_test.rs",
            "tests/render_test.rs",
            "docs/parser.md",
            "docs/render.md",
            "scripts/build.sh",
            "packages/parser/package.json",
            "packages/parser/src/index.ts",
            "packages/render/package.json",
            "café/menu.txt",
            "café/naïve.txt",
        ],
        "gitignore": None,
    },
    "wide": {
        # 130 top-level files, past the 100-match cap, so the cap is measured
        # rather than assumed.
        "tree": [f"entry-{index:03}.txt" for index in range(130)],
        "gitignore": None,
    },
}

#: The `(rel, name, is_dir)` triples the ignore family asks the reference about.
#: Each is a fixture id and a probe; a probe need not exist on disk, since
#: `should_ignore` is a pure function of the three strings once the root is
#: compiled.
IGNORE_PROBES: list[tuple[str, str, bool]] = [
    ("plain", "src", True),
    ("plain", "src/main.rs", False),
    ("plain", "readme.md", False),
    ("ignored", ".git", True),
    ("ignored", "src/.git", True),
    ("ignored", ".git", False),
    ("ignored", "__pycache__", True),
    ("ignored", "src/__pycache__", True),
    ("ignored", "node_modules", True),
    ("ignored", "src/node_modules", True),
    ("ignored", ".DS_Store", False),
    ("ignored", "src/app.pyc", False),
    ("ignored", "notes.log", False),
    ("ignored", "logs", True),
    ("ignored", "logs", False),
    ("ignored", ".vscode", True),
    ("ignored", ".idea", True),
    ("ignored", "build", True),
    ("ignored", "src/build", True),
    ("ignored", "dist", True),
    ("ignored", "target", True),
    ("ignored", "src/target", True),
    ("ignored", ".next", True),
    ("ignored", ".nuxt", True),
    ("ignored", "coverage", True),
    ("ignored", ".nyc_output", True),
    ("ignored", "vibe.egg-info", False),
    ("ignored", "vibe.egg-info", True),
    ("ignored", ".pytest_cache", True),
    ("ignored", ".tox", True),
    ("ignored", "vendor", True),
    ("ignored", "src/vendor", True),
    ("ignored", "third_party", True),
    ("ignored", "deps", True),
    ("ignored", "bundle.min.js", False),
    ("ignored", "site.min.css", False),
    ("ignored", "app.bundle.js", False),
    ("ignored", "app.chunk.js", False),
    ("ignored", ".cache", True),
    ("ignored", "tmp", True),
    ("ignored", "temp", True),
    ("ignored", ".uv-cache", True),
    ("ignored", ".ruff_cache", True),
    ("ignored", ".venv", True),
    ("ignored", "venv", True),
    ("ignored", ".mypy_cache", True),
    ("ignored", "htmlcov", True),
    ("ignored", ".coverage", False),
    ("ignored", "keep.txt", False),
    ("gitignored", "drop.log", False),
    ("gitignored", "keep.log", False),
    ("gitignored", "nested/keep.log", False),
    ("gitignored", "report-7.txt", False),
    ("gitignored", "report-x.txt", False),
    ("gitignored", "alpha.tmp", False),
    ("gitignored", "blpha.tmp", False),
    ("gitignored", "secrets", True),
    ("gitignored", "secrets", False),
    ("gitignored", "nested/secrets", True),
    ("gitignored", "build", True),
    ("gitignored", "nested/build", True),
    ("gitignored", "trailing.txt", False),
    ("gitignored", "public/asset.css", False),
    ("gitignored", "hash#name.txt", False),
    ("gitignored", "spaced name.txt", False),
]

#: The change sequences the incremental store is driven through. Each step
#: mutates the tree, hands `apply_changes` a list of `(kind, path)` pairs, and
#: the capture records the entry set and the stats that result.
#:
#: `path` is relative to the index root; a path starting with `../` deliberately
#: escapes it, which is the change category the store skips.
CHANGE_SEQUENCES: list[dict[str, Any]] = [
    {
        "case": "add-one-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "src/added.rs"}],
                "changes": [["added", "src/added.rs"]],
            }
        ],
    },
    {
        "case": "modify-one-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "modifyFile", "path": "src/main.rs"}],
                "changes": [["modified", "src/main.rs"]],
            }
        ],
    },
    {
        "case": "delete-one-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "delete", "path": "src/lib.rs"}],
                "changes": [["deleted", "src/lib.rs"]],
            }
        ],
    },
    {
        "case": "delete-directory-by-prefix",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "delete", "path": "src/render"}],
                "changes": [["deleted", "src/render"]],
            }
        ],
    },
    {
        "case": "add-directory-recursively",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": "added/one.txt"},
                    {"op": "createFile", "path": "added/deep/two.txt"},
                    {"op": "createDir", "path": "added/deep/empty"},
                ],
                "changes": [["added", "added"]],
            }
        ],
    },
    {
        "case": "add-directory-whose-descendants-are-ignored",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": "generated/keep.txt"},
                    {"op": "createFile", "path": "generated/target/app"},
                    {"op": "createFile", "path": "generated/notes.log"},
                ],
                "changes": [["added", "generated"]],
            }
        ],
    },
    {
        "case": "add-ignored-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "run.log"}],
                "changes": [["added", "run.log"]],
            }
        ],
    },
    {
        "case": "change-outside-the-root",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [],
                "changes": [["added", "../outside/stranger.txt"]],
            }
        ],
    },
    {
        "case": "add-a-path-that-does-not-exist",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [],
                "changes": [["added", "src/never-written.rs"]],
            }
        ],
    },
    {
        "case": "delete-a-path-the-index-never-held",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [],
                "changes": [["deleted", "src/never-written.rs"]],
            }
        ],
    },
    {
        "case": "rename-a-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "rename", "path": "docs/guide.md", "to": "docs/manual.md"}
                ],
                "changes": [
                    ["deleted", "docs/guide.md"],
                    ["added", "docs/manual.md"],
                ],
            }
        ],
    },
    {
        "case": "rename-a-directory",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "rename", "path": "src/render", "to": "src/draw"}],
                "changes": [
                    ["deleted", "src/render"],
                    ["added", "src/draw"],
                ],
            }
        ],
    },
    {
        "case": "two-steps-add-then-delete",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "src/scratch.rs"}],
                "changes": [["added", "src/scratch.rs"]],
            },
            {
                "mutations": [{"op": "delete", "path": "src/scratch.rs"}],
                "changes": [["deleted", "src/scratch.rs"]],
            },
        ],
    },
    {
        "case": "unreported-change-leaves-the-index-stale",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "src/silent.rs"}],
                "changes": [],
            }
        ],
    },
    {
        "case": "batch-at-the-mass-change-threshold",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": f"bulk/file-{index:03}.txt"}
                    for index in range(200)
                ],
                "changes": [
                    ["added", f"bulk/file-{index:03}.txt"] for index in range(200)
                ],
            }
        ],
    },
    {
        "case": "batch-past-the-mass-change-threshold",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": f"bulk/file-{index:03}.txt"}
                    for index in range(201)
                ],
                "changes": [
                    ["added", f"bulk/file-{index:03}.txt"] for index in range(201)
                ],
            }
        ],
    },
]

#: The queries the ranking family is measured over, as the fragment that
#: follows `@` in the prompt.
RANKING_QUERIES: list[tuple[str, str]] = [
    ("ranking", ""),
    ("ranking", "src/"),
    ("ranking", "src/parser/"),
    ("ranking", "packages/parser/"),
    ("ranking", "main"),
    ("ranking", "main.rs"),
    ("ranking", "lib"),
    ("ranking", "cargo"),
    ("ranking", "Cargo.toml"),
    ("ranking", "cargo.lock"),
    ("ranking", "parser"),
    ("ranking", "parser/"),
    ("ranking", "src/parser"),
    ("ranking", "tokens"),
    ("ranking", "tokenstream"),
    ("ranking", "tkst"),
    ("ranking", "table"),
    ("ranking", "tablecell"),
    ("ranking", "render"),
    ("ranking", "rendertest"),
    ("ranking", "srcmain"),
    ("ranking", "pkgjson"),
    ("ranking", "package.json"),
    ("ranking", ".ts"),
    ("ranking", "md"),
    ("ranking", "docs/render.md"),
    ("ranking", "zzzz"),
    ("ranking", "café"),
    ("ranking", "naïve"),
    ("ranking", "cafe"),
    ("hidden", ""),
    ("hidden", "."),
    ("hidden", ".env"),
    ("hidden", "visible"),
    ("hidden", "src/"),
    ("plain", ""),
    ("plain", "empty-dir/"),
    ("plain", "guide"),
    ("wide", ""),
    ("wide", "entry-0"),
]


def materialize(root: Path, fixture: dict[str, Any]) -> None:
    """Writes one fixture tree under `root`."""

    for entry in fixture["tree"]:
        if entry.endswith("/"):
            (root / entry.rstrip("/")).mkdir(parents=True, exist_ok=True)
            continue
        target = root / entry
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8")
    gitignore = fixture.get("gitignore")
    if gitignore is not None:
        (root / ".gitignore").write_text(gitignore, encoding="utf-8")


def mutate(root: Path, mutation: dict[str, Any]) -> None:
    """Applies one scripted filesystem mutation to a materialized tree."""

    operation = mutation["op"]
    target = root / mutation["path"]
    if operation == "createFile":
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8")
        return
    if operation == "createDir":
        target.mkdir(parents=True, exist_ok=True)
        return
    if operation == "modifyFile":
        target.write_text(FILE_BODY + "modified\n", encoding="utf-8")
        return
    if operation == "delete":
        if target.is_dir():
            shutil.rmtree(target)
        else:
            target.unlink(missing_ok=True)
        return
    if operation == "rename":
        destination = root / mutation["to"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        target.rename(destination)
        return
    raise OracleError(f"unknown fixture mutation `{operation}`")


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def _entries(snapshot: list[Any]) -> list[dict[str, Any]]:
    """One entry list, normalized to relative-path order.

    The reference holds a rebuilt index in `scandir` order and a mutated one in
    relative-path order, so its order is not a contract; sorting both sides is
    the same normalization the `grep` row of `docs/parity.md` already records.
    """

    return [
        {"rel": entry.rel, "name": entry.name, "isDir": entry.is_dir}
        for entry in sorted(snapshot, key=lambda entry: entry.rel)
    ]


def capture_constants() -> dict[str, Any]:
    from vibe.cli.autocompletion.completers import (
        DEFAULT_MAX_ENTRIES_TO_PROCESS,
        DEFAULT_TARGET_MATCHES,
    )
    from vibe.cli.autocompletion.file_indexer.ignore_rules import (
        DEFAULT_IGNORE_PATTERNS,
        WALK_SKIP_DIR_NAMES,
        IgnoreRules,
    )
    from vibe.cli.autocompletion.file_indexer.indexer import FileIndexer
    from vibe.cli.autocompletion.file_indexer.store import ASCII_CODEPOINT_LIMIT
    from vibe.cli.autocompletion.fuzzy import (
        CONSECUTIVE_MULTIPLIER,
        PREFIX_MULTIPLIER,
        WORD_BOUNDARY_MULTIPLIER,
    )

    indexer = FileIndexer()
    try:
        threshold = indexer._store._mass_change_threshold
    finally:
        indexer.shutdown()

    compiled = [
        {
            "stripped": pattern.stripped,
            "isExclude": pattern.is_exclude,
            "dirOnly": pattern.dir_only,
            "nameOnly": pattern.name_only,
            "anchorRoot": pattern.anchor_root,
        }
        for pattern in IgnoreRules()._compile_default_patterns()
    ]
    if len(compiled) != len(DEFAULT_IGNORE_PATTERNS):
        raise OracleError("the compiled default patterns lost an entry")

    return {
        "maxEntriesToProcess": DEFAULT_MAX_ENTRIES_TO_PROCESS,
        "targetMatches": DEFAULT_TARGET_MATCHES,
        "massChangeThreshold": threshold,
        "asciiCodepointLimit": ASCII_CODEPOINT_LIMIT,
        "scoreScale": SCORE_SCALE,
        "fuzzyMultipliers": {
            "prefix": _centis(PREFIX_MULTIPLIER),
            "wordBoundary": _centis(WORD_BOUNDARY_MULTIPLIER),
            "consecutive": _centis(CONSECUTIVE_MULTIPLIER),
        },
        "defaultIgnorePatterns": [
            {"raw": raw, "isExclude": is_exclude}
            for raw, is_exclude in DEFAULT_IGNORE_PATTERNS
        ],
        "compiledDefaultPatterns": compiled,
        "walkSkipDirNames": sorted(WALK_SKIP_DIR_NAMES),
    }


def _centis(value: float) -> int:
    scaled = value * SCORE_SCALE
    rounded = round(scaled)
    if abs(scaled - rounded) > 1e-6:
        raise OracleError(f"{value} is not an exact hundredth")
    return int(rounded)


def capture_ignore_rules(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules

    records: list[dict[str, Any]] = []
    for fixture_id, root in _roots(scratch).items():
        probes = [probe for probe in IGNORE_PROBES if probe[0] == fixture_id]
        if not probes:
            continue
        rules = IgnoreRules()
        rules.ensure_for_root(root)
        for _, rel, is_dir in probes:
            name = rel.rsplit("/", 1)[-1]
            records.append(
                {
                    "case": f"{fixture_id}/{rel}{'/' if is_dir else ''}",
                    "fixture": fixture_id,
                    "rel": rel,
                    "name": name,
                    "isDir": is_dir,
                    "ignored": rules.should_ignore(rel, name, is_dir),
                }
            )
    return records


def capture_walk(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore

    records: list[dict[str, Any]] = []
    for fixture_id, root in _roots(scratch).items():
        stats = FileIndexStats()
        store = FileIndexStore(IgnoreRules(), stats)
        store.rebuild(root)
        records.append(
            {
                "case": fixture_id,
                "fixture": fixture_id,
                "entries": _entries(store.snapshot()),
                "stats": {
                    "rebuilds": stats.rebuilds,
                    "incrementalUpdates": stats.incremental_updates,
                },
            }
        )
    return records


def capture_changes() -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore
    from vibe.cli.autocompletion.file_indexer.watcher import Change

    kinds = {
        "added": Change.added,
        "modified": Change.modified,
        "deleted": Change.deleted,
    }
    records: list[dict[str, Any]] = []
    for sequence in CHANGE_SEQUENCES:
        case = sequence["case"]
        with tempfile.TemporaryDirectory(prefix="autocompletion-changes-") as scratch_dir:
            enclosure = Path(scratch_dir)
            root = enclosure / "root"
            root.mkdir()
            materialize(root, FIXTURES[sequence["fixture"]])
            stats = FileIndexStats()
            store = FileIndexStore(IgnoreRules(), stats)
            store.rebuild(root)
            steps: list[dict[str, Any]] = []
            for index, step in enumerate(sequence["steps"]):
                for mutation in step["mutations"]:
                    mutate(root, mutation)
                changes = [
                    (kinds[kind], (root / relative).resolve())
                    for kind, relative in step["changes"]
                ]
                store.apply_changes(changes)
                steps.append(
                    {
                        "step": index,
                        "mutations": step["mutations"],
                        "changes": step["changes"],
                        "entries": _entries(store.snapshot()),
                        "stats": {
                            "rebuilds": stats.rebuilds,
                            "incrementalUpdates": stats.incremental_updates,
                        },
                    }
                )
        records.append(
            {"case": case, "fixture": sequence["fixture"], "steps": steps}
        )
    return records


def capture_ranking(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.completers import PathCompleter
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore

    roots = _roots(scratch)
    indexes: dict[str, list[Any]] = {}
    for fixture_id, root in roots.items():
        store = FileIndexStore(IgnoreRules(), FileIndexStats())
        store.rebuild(root)
        indexes[fixture_id] = sorted(store.snapshot(), key=lambda entry: entry.rel)

    completer = PathCompleter()
    try:
        records: list[dict[str, Any]] = []
        for fixture_id, query in RANKING_QUERIES:
            context = completer._build_search_context(query)
            scored = completer._score_matches(indexes[fixture_id], context)
            records.append(
                {
                    "case": f"{fixture_id}/{query or '<empty>'}",
                    "fixture": fixture_id,
                    "query": query,
                    "candidates": [
                        {"label": label, "rank": _rank(rank)} for label, rank in scored
                    ],
                }
            )
        return records
    finally:
        completer._indexer.shutdown()


def _rank(rank: Any) -> dict[str, Any]:
    return {
        "exactDirectory": int(rank.exact_directory),
        "immediateChildOfExactPath": int(rank.immediate_child_of_exact_path),
        "exactFilename": int(rank.exact_filename),
        "preferredStemMatch": int(rank.preferred_stem_match),
        "exactStem": int(rank.exact_stem),
        "stemPrefix": int(rank.stem_prefix),
        "namePrefix": int(rank.name_prefix),
        "extensionMatch": int(rank.extension_match),
        "fuzzyScore": _centis(rank.fuzzy_score),
        "shallowPath": rank.shallow_path,
    }


def _roots(scratch: Path) -> dict[str, Path]:
    return {fixture_id: scratch / fixture_id for fixture_id in FIXTURES}


def capture_fixtures() -> list[dict[str, Any]]:
    return [
        {
            "id": fixture_id,
            "tree": fixture["tree"],
            "gitignore": fixture["gitignore"],
            "fileBody": FILE_BODY,
        }
        for fixture_id, fixture in FIXTURES.items()
    ]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="?",
        const=DEFAULT_CORPUS,
        default=None,
        help=(
            "also write the committed corpus, which the Rust replay reads "
            f"unconditionally (default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def build_corpus(reference: dict[str, str], families: dict[str, Any]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Autocompletion corpus: what the pinned reference's ignore rules, index store and "
            "path ranking answer for each scripted fixture tree, change sequence and query. "
            "Every name here is supplied by the fixtures scripts/parity/autocompletion.py "
            "authors; no reference-authored text, no reference source and no machine path is "
            "recorded. Two normalizations apply on both sides of the replay: entry lists are "
            "sorted by relative path, because the reference holds a rebuilt index in scandir "
            "order and a mutated one in relative-path order, so its order is not a contract; "
            "and fuzzy scores are recorded in hundredths, the integer scale this port's "
            "matcher computes in. Regenerate with scripts/parity/autocompletion.py --corpus "
            "when the pinned reference moves."
        ),
        **families,
    }


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(
            arguments.reference, arguments.python, pinned
        )
        GUARD.install()
        with tempfile.TemporaryDirectory(prefix="autocompletion-oracle-") as scratch_dir:
            scratch = Path(scratch_dir)
            for fixture_id, root in _roots(scratch).items():
                root.mkdir()
                materialize(root, FIXTURES[fixture_id])
            constants = capture_constants()
            ignore_rules = capture_ignore_rules(scratch)
            walk = capture_walk(scratch)
            ranking = capture_ranking(scratch)
        changes = capture_changes()
    except OracleError as error:
        print(f"autocompletion capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(
            f"autocompletion capture failed: network attempts {GUARD.attempts}",
            file=sys.stderr,
        )
        return 1

    shortfalls = []
    if len(ignore_rules) < 40:
        shortfalls.append(f"ignoreRules holds {len(ignore_rules)} probes, under 40")
    if len(changes) < 12:
        shortfalls.append(f"changes holds {len(changes)} sequences, under 12")
    if len(ranking) < 30:
        shortfalls.append(f"ranking holds {len(ranking)} queries, under 30")
    if shortfalls:
        print(
            "autocompletion capture failed: " + "; ".join(shortfalls),
            file=sys.stderr,
        )
        return 1

    families = {
        "constants": constants,
        "fixtures": capture_fixtures(),
        "ignoreRules": ignore_rules,
        "walk": walk,
        "changes": changes,
        "ranking": ranking,
    }
    corpus = build_corpus(reference, families)

    full = {
        **corpus,
        "reference": reference,
        "platform": platform.system().lower(),
        "python": platform.python_version(),
    }
    _write(arguments.output, full)
    if arguments.corpus is not None:
        _write(arguments.corpus, corpus)

    counted = {
        "ignoreRules": len(ignore_rules),
        "walk": len(walk),
        "changes": sum(len(record["steps"]) for record in changes),
        "ranking": len(ranking),
    }
    total = sum(counted.values())
    print(
        f"captured {total} scenarios across {len(counted)} families plus the constants block "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family, count in counted.items():
        print(f"  {family}: {count}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


def _write(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(staged, path)


if __name__ == "__main__":
    sys.exit(main())
