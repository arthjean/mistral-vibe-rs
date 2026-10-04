#!/usr/bin/env python3
"""Capture what the pinned Python reference's file index answers.

The reference splits `@` completion into a store, a rules compiler, a watcher
and a ranking completer, and three of those four take everything they need as
arguments. ``FileIndexStore`` receives its ignore rules and its stats object,
``IgnoreRules.should_ignore`` is a pure function of three strings once a root
is compiled, and ``PathCompleter._score_matches`` takes a list of entries and a
search context. A capture therefore drives the real reference objects over
scratch trees this script writes itself, with no watcher thread, no network and
no editor. The store lists a git work tree through `git ls-files` and walks
the tree only outside one, so the walked scratch trees must sit outside any
work tree: this script fails rather than record the git-backed index under
those names. The git-backed index has families of its own, measured over a
repository this script initializes with every git configuration source but
its own pinned, so a workstation's global excludes never reach the corpus.

Thirteen families come out, and are what the Rust replay compares:

``constants``     the caps, the threshold, the ASCII limit and the 36 defaults
``ignoreRules``   the ``should_ignore`` verdict per ``(rel, name, is_dir)``
``walk``          the entry set a rebuild holds per fixture tree
``changes``       the entry set and the stats after each ``apply_changes`` call
``ranking``       the ordered candidates and the ``MatchRank`` tuple per query
``gitWalk``       the entry set a rebuild holds inside a scripted git work tree
``gitChanges``    the dirty flag, entries and stats around a watched change
``collect``       the labels and the range ``PathCompleter`` answers per prompt
``controller``    what the path controller shows and answers per key
``inlineSkill``   the mid-prompt skill ghost and what accepting it replaces
``pathPrompt``    the resources ``build_path_prompt_payload`` finds per message
``watchFilter``   the ``watchfiles.DefaultFilter`` verdict per changed path
``fuzzy``         ``fuzzy_match`` over a seeded sweep of pattern and text pairs

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
    "edgeignore": {
        # The rules file as an editor can leave it: behind a byte-order mark,
        # with line boundaries `splitlines` honors and `\n` does not, a lone
        # carriage return, a doubled negation and a doubled anchor.
        "tree": [
            "double.txt",
            "!double.txt",
            "slashed",
            "x.bom",
            "a.vt",
            "a.ls",
            "a.us",
            "a.cr2",
            "keep.md",
        ],
        "gitignore": (
            "\ufeff*.bom\n"
            "*.txt\n"
            "!!double.txt\n"
            "//slashed\n"
            "*.ff\x0c*.vt\n"
            "*.ps\u2028*.ls\n"
            "\x1f*.us\n"
            "*.cr\r*.cr2\n"
        ),
    },
    "collect": {
        "tree": [
            "README.md",
            "Makefile",
            ".env",
            "src/main.rs",
            "src/parser/mod.rs",
            "src/parser/lexer.rs",
            "docs/guide.md",
            "empty-dir/",
        ],
        "gitignore": None,
    },
}

#: What surrounds a `collect` root, relative to an enclosure holding the root at
#: `top/root`, so a `..` query lists directories this script authored.
COLLECT_OUTSIDE: list[str] = [
    "top/sibling/alpha.md",
    "top/sibling/a.txt",
    "top/sibling/Beta/inner.txt",
    "top/sibling/.hidden",
    "top/peer.txt",
    "other/x.txt",
]
COLLECT_ROOT = "top/root"

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


#: The edge cases the ignore family adds over the `edgeignore` rules file.
IGNORE_PROBES += [
    ("edgeignore", "x.bom", False),
    ("edgeignore", "double.txt", False),
    ("edgeignore", "!double.txt", False),
    ("edgeignore", "slashed", False),
    ("edgeignore", "a.ff", False),
    ("edgeignore", "a.vt", False),
    ("edgeignore", "a.ps", False),
    ("edgeignore", "a.ls", False),
    ("edgeignore", "a.us", False),
    ("edgeignore", "a.cr", False),
    ("edgeignore", "a.cr2", False),
    ("edgeignore", "keep.md", False),
]

#: The work tree the git families are measured over. Every path is relative to
#: the repository root; `tracked` is staged with `git add -f`, `deleted` is
#: removed from disk after staging, and `excludesFile` is wired through the
#: repository's own `core.excludesFile`, which outranks any global one.
GIT_FIXTURE: dict[str, Any] = {
    "tracked": [
        "src/main.rs",
        "vendor/pkg/lib.go",
        "logs/app.log",
        "build/out.txt",
        ".vscode/settings.json",
        "docs/guide.md",
        "docs/gone.md",
        "nested/inner/keep.txt",
        "café/menu.txt",
        "spaced name.txt",
        "ignored-dir/forced.txt",
    ],
    "untracked": [
        "notes.txt",
        "nested/scratch.draft",
        "nested/inner/extra.txt",
        "nested/inner/deep/new.txt",
        "local.secret",
        "cache.junk",
        "node_modules/x/index.js",
        "a.tmp",
        "ignored-dir/file.txt",
    ],
    "deleted": ["docs/gone.md"],
    "emptyDirs": ["empty-dir", "nested/empty"],
    "gitignores": {
        ".gitignore": "*.tmp\nignored-dir/\n",
        "nested/.gitignore": "*.draft\n",
    },
    "infoExclude": "*.secret\n",
    "excludesFile": "*.junk\n",
}

#: The roots the git walk family lists, relative to the repository root.
GIT_ROOTS: list[str] = ["", "nested", "ignored-dir", "docs"]

#: The watched change sequences the git-backed index is driven through, each
#: from a fresh repository, through the indexer's own change handler.
GIT_CHANGE_SEQUENCES: list[dict[str, Any]] = [
    {
        "case": "untracked-file-appears-after-the-next-query",
        "mutations": [{"op": "createFile", "path": "added.txt"}],
        "changes": [["added", "added.txt"]],
    },
    {
        "case": "ignored-file-stays-out",
        "mutations": [{"op": "createFile", "path": "fresh.tmp"}],
        "changes": [["added", "fresh.tmp"]],
    },
    {
        "case": "deleted-tracked-file-leaves",
        "mutations": [{"op": "delete", "path": "src/main.rs"}],
        "changes": [["deleted", "src/main.rs"]],
    },
    {
        "case": "modified-file-marks-dirty",
        "mutations": [{"op": "modifyFile", "path": "notes.txt"}],
        "changes": [["modified", "notes.txt"]],
    },
    {
        "case": "no-change-no-rebuild",
        "mutations": [],
        "changes": [],
    },
    {
        "case": "change-outside-the-root-still-marks-dirty",
        "mutations": [],
        "changes": [["added", "../outside.txt"]],
    },
]

#: The prompts the `collect` family hands `PathCompleter`, as `(fixture, text,
#: cursor)`; a cursor of `None` sits at the end of the text.
COLLECT_QUERIES: list[tuple[str, str, int | None]] = [
    ("collect", "@", None),
    ("collect", "@..", None),
    ("collect", "@../", None),
    ("collect", "@../s", None),
    ("collect", "@../SIB", None),
    ("collect", "@../sibling/", None),
    ("collect", "@../sibling/a", None),
    ("collect", "@../sibling/.", None),
    ("collect", "@../sibling/b", None),
    ("collect", "@../sibling/Beta", None),
    ("collect", "@../sibling/Beta/", None),
    ("collect", "@../missing", None),
    ("collect", "@../missing/x", None),
    ("collect", "@../..", None),
    ("collect", "@../../", None),
    ("collect", "@../../o", None),
    ("collect", "@..x", None),
    ("collect", "@../root/", None),
    ("collect", "@../root/src", None),
    ("collect", "@./src/", None),
    ("collect", "@src\\par", None),
    ("collect", "@src\\parser\\", None),
    ("collect", "@srx/", None),
    ("collect", "@/", None),
    ("collect", "@empty-dir/", None),
    ("collect", "@parser/", None),
    ("collect", "@Src/", None),
    ("collect", "@src/", None),
    ("collect", "@main", None),
    ("collect", "look at @src/ma", None),
    ("collect", "@src/main.rs tail", 6),
    ("collect", "@foo bar", None),
    ("collect", "no mention here", None),
    ("collect", "a@src/ma", None),
    ("collect", "@.e", None),
    ("wide", "@", None),
    ("wide", "@entry", None),
]

#: The skills the inline family offers, in the `(alias, description)` shape
#: the chat input hands its controller.
INLINE_SKILLS: list[tuple[str, str]] = [
    ("/implement", ""),
    ("/implement-plan", ""),
    ("/Review", ""),
    ("/review-pr", ""),
    ("/deploy", ""),
    ("/a", ""),
]

#: The prompts the inline family types, as `(text, cursor, default_mode)`.
INLINE_CASES: list[tuple[str, int | None, bool]] = [
    ("fix /imp", None, True),
    ("fix /implement", None, True),
    ("fix /implement-plan", None, True),
    ("/imp", None, True),
    ("   /imp", None, True),
    ("fix /IMP", None, True),
    ("fix /rev", None, True),
    ("fix /imp tail", 8, True),
    ("fix /imptail", 8, True),
    ("fix /", None, True),
    ("fix /a/b", None, True),
    ("fix /a@b", None, True),
    ("mail@x /dep", None, True),
    ("fix\n/imp", None, True),
    ("fix /imp", None, False),
    ("fix /imp", 0, True),
    ("fix /a", None, True),
    ("fix /zzz", None, True),
    ("fix\t/dep", None, True),
]

#: The fixture the prompt family resolves mentions against.
PROMPT_TREE: list[str] = [
    "docs/guide.md",
    "img/shot.PNG",
    "dir with space/file.txt",
    "it's.txt",
    "notes.md",
    "archive.tar.gz",
]

#: The messages the prompt family parses. `{root}` stands for the resolved
#: fixture root, substituted on both sides.
PROMPT_MESSAGES: list[str] = [
    "",
    "read @docs/guide.md please",
    "@docs/guide.md and @docs/guide.md",
    "@docs/guide.md then @./docs/../docs/guide.md",
    "email a@docs/guide.md",
    "x_@docs/guide.md",
    "é@docs/guide.md",
    "(@docs/guide.md)",
    "see @docs",
    "see @img/shot.PNG",
    'see @"dir with space/file.txt"',
    "see @'dir with space/file.txt'",
    "see @'it\\'s.txt'",
    'see @"it\'s.txt"',
    'see @"unterminated',
    'see @"" now',
    "see @'' now",
    'see @"missing @docs/guide.md"',
    "see @missing.md",
    "see @ alone",
    "see @@docs/guide.md",
    'see @"{root}/notes.md"',
    "see @docs/guide.md,@notes.md",
    "see @'docs/guide.md'@notes.md",
    "see @archive.tar.gz and @img",
    "trailing @",
]

#: The changed paths the watch filter is asked about, as segments under a
#: watched root.
WATCH_FILTER_PROBES: list[list[str]] = [
    ["src", "main.rs"],
    [".git", "index"],
    [".git"],
    ["a", ".git", "HEAD"],
    ["node_modules", "x", "i.js"],
    ["__pycache__", "m.cpython-312.pyc"],
    ["m.pyc"],
    ["m.pyo"],
    ["m.pyd"],
    ["m.py"],
    ["x.pyc.txt"],
    [".venv", "lib"],
    [".idea", "ws.xml"],
    [".hg"],
    [".svn", "x"],
    [".tox"],
    [".mypy_cache", "x"],
    [".pytest_cache"],
    [".hypothesis", "x"],
    ["f.swp"],
    ["f.swx"],
    ["f.sw"],
    ["notes~"],
    ["a~b"],
    [".#lock"],
    [".DS_Store"],
    ["a", ".DS_Store"],
    ["DS_Store"],
    ["flycheck_main.rs"],
    ["src", "flycheck_x"],
    ["x.___jb_tmp___"],
    ["x.___jb_old___"],
    ["x.___jb_ld___"],
    ["venv", "x"],
    ["target", "x"],
    ["git"],
    ["my.git"],
]

#: The seeded sweep the fuzzy family records. Both sides draw the same pairs
#: from a SplitMix64 stream with integer arithmetic only, so nothing but the
#: seed and the alphabet has to travel in the corpus.
FUZZY_SEED = 0x5EED_A11C
FUZZY_COUNT = 4096
FUZZY_ALPHABET = "abcdeABCDE/-_.012éÉßİΣσǅⒶ"
_MASK64 = (1 << 64) - 1


class SplitMix64:
    """The generator the fuzzy sweep draws from, mirrored in the Rust replay."""

    def __init__(self, seed: int) -> None:
        self.state = seed & _MASK64

    def next(self) -> int:
        self.state = (self.state + 0x9E3779B97F4A7C15) & _MASK64
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & _MASK64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & _MASK64
        return z ^ (z >> 31)

    def below(self, bound: int) -> int:
        return self.next() % bound


def fuzzy_pairs() -> list[tuple[str, str]]:
    """The sweep's `(pattern, text)` pairs: half drawn as a subsequence of the
    text, so most of them match, and half drawn at random."""

    stream = SplitMix64(FUZZY_SEED)
    alphabet = list(FUZZY_ALPHABET)
    pairs: list[tuple[str, str]] = []
    for _ in range(FUZZY_COUNT):
        text = "".join(
            alphabet[stream.below(len(alphabet))] for _ in range(stream.below(17))
        )
        length = 1 + stream.below(4)
        if text and stream.below(2) == 0:
            indices = sorted(stream.below(len(text)) for _ in range(length))
            pattern = "".join(text[index] for index in indices)
        else:
            pattern = "".join(
                alphabet[stream.below(len(alphabet))] for _ in range(length)
            )
        pairs.append((pattern, text))
    return pairs


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
    import inspect

    from vibe.cli.autocompletion.completers import DEFAULT_TARGET_MATCHES, PathCompleter
    from vibe.cli.autocompletion.file_indexer.ignore_rules import (
        DEFAULT_IGNORE_PATTERNS,
        WALK_SKIP_DIR_NAMES,
        IgnoreRules,
    )
    from vibe.cli.autocompletion.file_indexer.indexer import FileIndexer
    from vibe.cli.autocompletion.file_indexer.store import ASCII_CODEPOINT_LIMIT

    # The processing cap is no longer a module constant: it is the default of
    # the completer's own parameter, which is `None` (uncapped) at the pin
    # (vibe/cli/autocompletion/completers.py:117-125) and serializes as null.
    max_entries = (
        inspect.signature(PathCompleter.__init__)
        .parameters["max_entries_to_process"]
        .default
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
        "maxEntriesToProcess": max_entries,
        "targetMatches": DEFAULT_TARGET_MATCHES,
        "massChangeThreshold": threshold,
        "asciiCodepointLimit": ASCII_CODEPOINT_LIMIT,
        "scoreScale": SCORE_SCALE,
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
        _rebuild_walked(store, root)
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
    # The watcher binds `Change` only for type checking at the pin
    # (vibe/cli/autocompletion/file_indexer/watcher.py:6-9); the store compares
    # against watchfiles' own members by identity (store.py:127,146).
    from watchfiles import Change

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
            _rebuild_walked(store, root)
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
        _rebuild_walked(store, root)
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


def _rebuild_walked(store: Any, root: Path) -> None:
    """Rebuild through the ignore-rule walk this corpus measures.

    A rebuild tries `git ls-files` first and walks the tree only when that
    fails (vibe/cli/autocompletion/file_indexer/store.py:80-103). Inside a work
    tree the defaults and the rules compiler are bypassed and a change only
    marks the store dirty (store.py:116-124), which would record a different
    contract under the same case names, so a git-backed rebuild fails the run.
    """

    if not store.rebuild(root):
        raise OracleError(f"the rebuild of {root.name} was canceled")
    if store.is_git_backed:
        raise OracleError(
            f"{root.name} sits inside a git work tree, so the index came from "
            "`git ls-files` instead of the ignore-rule walk; move TMPDIR outside it"
        )


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


# --------------------------------------------------------------------------
# The git-backed index
# --------------------------------------------------------------------------

#: The environment every git call of the git families runs under: no system
#: or global configuration, so only what this script writes into the
#: repository reaches the listing.
_GIT_ISOLATION = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": os.devnull,
}


def _run_git(repository: Path, *arguments: str) -> None:
    environment = {**os.environ, **_GIT_ISOLATION}
    result = subprocess.run(
        ["git", *arguments],
        cwd=repository,
        capture_output=True,
        text=True,
        check=False,
        env=environment,
    )
    if result.returncode != 0:
        raise OracleError(f"git {' '.join(arguments)} failed: {result.stderr.strip()}")


def materialize_git(enclosure: Path) -> Path:
    """Writes `GIT_FIXTURE` as a repository under `enclosure` and returns it."""

    repository = enclosure / "repo"
    repository.mkdir()
    for relative in GIT_FIXTURE["tracked"] + GIT_FIXTURE["untracked"]:
        target = repository / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8")
    for relative in GIT_FIXTURE["emptyDirs"]:
        (repository / relative).mkdir(parents=True, exist_ok=True)
    for relative, text in GIT_FIXTURE["gitignores"].items():
        (repository / relative).write_text(text, encoding="utf-8")
    _run_git(repository, "init", "-q")
    _run_git(repository, "add", "-f", "--", *GIT_FIXTURE["tracked"])
    for relative in GIT_FIXTURE["deleted"]:
        (repository / relative).unlink()
    info = repository / ".git" / "info"
    info.mkdir(parents=True, exist_ok=True)
    (info / "exclude").write_text(GIT_FIXTURE["infoExclude"], encoding="utf-8")
    excludes = enclosure / "excludes"
    excludes.write_text(GIT_FIXTURE["excludesFile"], encoding="utf-8")
    _run_git(repository, "config", "core.excludesFile", excludes.as_posix())
    return repository


class _IsolatedGit:
    """Pins the git configuration sources for the reference's own `git` calls,
    which inherit this process's environment."""

    def __enter__(self) -> None:
        self._saved = {key: os.environ.get(key) for key in _GIT_ISOLATION}
        os.environ.update(_GIT_ISOLATION)

    def __exit__(self, *exc: Any) -> None:
        for key, value in self._saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def capture_git_walk() -> dict[str, Any]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="autocompletion-git-") as scratch_dir:
        repository = materialize_git(Path(scratch_dir))
        with _IsolatedGit():
            for relative in GIT_ROOTS:
                root = repository / relative if relative else repository
                stats = FileIndexStats()
                store = FileIndexStore(IgnoreRules(), stats)
                if not store.rebuild(root):
                    raise OracleError(f"the git rebuild of `{relative}` was canceled")
                records.append(
                    {
                        "case": relative or "<root>",
                        "root": relative,
                        "gitBacked": store.is_git_backed,
                        "entries": _entries(store.snapshot()),
                        "stats": {
                            "rebuilds": stats.rebuilds,
                            "incrementalUpdates": stats.incremental_updates,
                        },
                    }
                )
    if not all(record["gitBacked"] for record in records):
        raise OracleError("a git walk root was not listed through `git ls-files`")
    return {"fixture": GIT_FIXTURE, "cases": records}


def capture_git_changes() -> dict[str, Any]:
    from vibe.cli.autocompletion.file_indexer import FileIndexer
    from watchfiles import Change

    kinds = {
        "added": Change.added,
        "modified": Change.modified,
        "deleted": Change.deleted,
    }
    records: list[dict[str, Any]] = []
    for sequence in GIT_CHANGE_SEQUENCES:
        with tempfile.TemporaryDirectory(prefix="autocompletion-gitchanges-") as scratch_dir:
            repository = materialize_git(Path(scratch_dir))
            with _IsolatedGit():
                indexer = FileIndexer()
                try:
                    root = repository.resolve()
                    indexer.get_index(root)
                    for mutation in sequence["mutations"]:
                        mutate(repository, mutation)
                    indexer._handle_watch_changes(
                        root,
                        [
                            (kinds[kind], str(root / relative))
                            for kind, relative in sequence["changes"]
                        ],
                    )
                    applied = {
                        "dirty": indexer._store.is_dirty,
                        "entries": _entries(indexer._store.snapshot()),
                        "stats": {
                            "rebuilds": indexer.stats.rebuilds,
                            "incrementalUpdates": indexer.stats.incremental_updates,
                        },
                    }
                    queried = _entries(indexer.get_index(root))
                    after = {
                        "dirty": indexer._store.is_dirty,
                        "entries": queried,
                        "stats": {
                            "rebuilds": indexer.stats.rebuilds,
                            "incrementalUpdates": indexer.stats.incremental_updates,
                        },
                    }
                finally:
                    indexer.shutdown()
        records.append(
            {
                "case": sequence["case"],
                "mutations": sequence["mutations"],
                "changes": sequence["changes"],
                "applied": applied,
                "queried": after,
            }
        )
    return {"cases": records}


# --------------------------------------------------------------------------
# The completer, its controllers and the prompt payload
# --------------------------------------------------------------------------


class _Chdir:
    def __init__(self, target: Path) -> None:
        self._target = target

    def __enter__(self) -> None:
        self._saved = Path.cwd()
        os.chdir(self._target)

    def __exit__(self, *exc: Any) -> None:
        os.chdir(self._saved)


def materialize_collect(enclosure: Path, fixture_id: str) -> Path:
    """Writes a fixture at `top/root` under `enclosure`, with the directories a
    `..` query lists around it."""

    root = enclosure / COLLECT_ROOT
    root.mkdir(parents=True)
    materialize(root, FIXTURES[fixture_id])
    for relative in COLLECT_OUTSIDE:
        target = enclosure / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8")
    return root


def capture_collect() -> dict[str, Any]:
    from vibe.cli.autocompletion.completers import PathCompleter

    records: list[dict[str, Any]] = []
    for fixture_id in dict.fromkeys(query[0] for query in COLLECT_QUERIES):
        with tempfile.TemporaryDirectory(prefix="autocompletion-collect-") as scratch_dir:
            root = materialize_collect(Path(scratch_dir), fixture_id)
            with _Chdir(root):
                completer = PathCompleter()
                try:
                    for query_fixture, text, cursor in COLLECT_QUERIES:
                        if query_fixture != fixture_id:
                            continue
                        position = len(text) if cursor is None else cursor
                        labels = [
                            entry.label
                            for entry in completer.get_completion_items(text, position)
                        ]
                        replacement = completer.get_replacement_range(text, position)
                        records.append(
                            {
                                "case": f"{fixture_id}/{text}@{position}",
                                "fixture": fixture_id,
                                "text": text,
                                "cursor": position,
                                "labels": labels,
                                "replacementRange": (
                                    list(replacement) if replacement else None
                                ),
                            }
                        )
                finally:
                    completer._indexer.shutdown()
    return {"root": COLLECT_ROOT, "outside": COLLECT_OUTSIDE, "cases": records}


class _RecordingView:
    """The completion view the controllers draw into, reduced to a log."""

    def __init__(self) -> None:
        self.suggestions: list[str] | None = None
        self.selected: int | None = None
        self.ghost: str | None = None
        self.replaced: list[Any] | None = None

    def render_completion_suggestions(self, suggestions: Any, selected_index: int) -> None:
        self.suggestions = [entry.label for entry in suggestions]
        self.selected = selected_index

    def clear_completion_suggestions(self) -> None:
        self.suggestions = None
        self.selected = None

    def show_inline_suggestion(self, suggestion: str) -> None:
        self.ghost = suggestion

    def clear_inline_suggestion(self) -> None:
        self.ghost = None

    def replace_completion_range(
        self, start: int, end: int, replacement: str, *, suppress_update: bool = False
    ) -> None:
        self.replaced = [start, end, replacement]


def _key(name: str) -> Any:
    from textual import events

    return events.Key(name, None)


def capture_controller() -> list[dict[str, Any]]:
    from concurrent.futures import Future

    from vibe.cli.autocompletion.completers import PathCompleter
    from vibe.cli.autocompletion.path_completion import PathCompletionController

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="autocompletion-controller-") as scratch_dir:
        root = materialize_collect(Path(scratch_dir), "wide")
        with _Chdir(root):
            completer = PathCompleter()
            try:
                view = _RecordingView()
                controller = PathCompletionController(completer, view)
                controller.on_text_changed("@entry", 6)
                records.append(
                    {
                        "case": "a-wide-answer-keeps-every-match",
                        "observed": {
                            "count": len(view.suggestions or []),
                            "selected": view.selected,
                        },
                    }
                )
                controller._move_selection(1)
                controller.on_text_changed("@entr", 5)
                records.append(
                    {
                        "case": "the-same-list-keeps-the-highlight",
                        "observed": {
                            "count": len(view.suggestions or []),
                            "selected": view.selected,
                        },
                    }
                )
                controller.on_text_changed("@entry-12", 9)
                records.append(
                    {
                        "case": "a-different-list-resets-the-highlight",
                        "observed": {
                            "count": len(view.suggestions or []),
                            "selected": view.selected,
                        },
                    }
                )
                for key in ("tab", "enter", "down", "escape"):
                    pending = PathCompletionController(completer, _RecordingView())
                    pending._pending_future = Future()
                    records.append(
                        {
                            "case": f"pending-{key}",
                            "observed": {
                                "result": str(
                                    pending.on_key(_key(key), "@entry", 6)
                                ),
                            },
                        }
                    )
                for key in ("tab", "enter"):
                    shown = PathCompletionController(completer, _RecordingView())
                    shown.on_text_changed("@entry-00", 9)
                    shown._pending_future = Future()
                    records.append(
                        {
                            "case": f"pending-{key}-over-shown-suggestions",
                            "observed": {
                                "result": str(
                                    shown.on_key(_key(key), "@entry-00", 9)
                                ),
                            },
                        }
                    )
            finally:
                completer._indexer.shutdown()
    return records


def capture_inline_skill() -> dict[str, Any]:
    from vibe.cli.autocompletion.inline_skill_completion import (
        InlineSkillCompletionController,
    )

    records: list[dict[str, Any]] = []
    for text, cursor, default_mode in INLINE_CASES:
        position = len(text) if cursor is None else cursor
        view = _RecordingView()
        controller = InlineSkillCompletionController(
            lambda: list(INLINE_SKILLS), view, lambda: default_mode
        )
        controller.on_text_changed(text, position)
        ghost = view.ghost
        result = str(controller.on_key(_key("tab"), text, position))
        records.append(
            {
                "case": f"{text}@{position}{'' if default_mode else '#mode'}",
                "text": text,
                "cursor": position,
                "defaultMode": default_mode,
                "ghost": ghost,
                "accept": {"result": result, "replaced": view.replaced},
            }
        )
    # A ghost computed for one prompt and accepted against another, the way a
    # caret move that never reached the manager leaves it.
    view = _RecordingView()
    controller = InlineSkillCompletionController(
        lambda: list(INLINE_SKILLS), view, lambda: True
    )
    controller.on_text_changed("fix /imp", 8)
    stale = str(controller.on_key(_key("tab"), "fix /imx", 8))
    records.append(
        {
            "case": "stale-ghost",
            "text": "fix /imx",
            "cursor": 8,
            "defaultMode": True,
            "ghost": None,
            "accept": {"result": stale, "replaced": view.replaced},
            "shownFor": "fix /imp",
        }
    )
    return {"skills": [list(skill) for skill in INLINE_SKILLS], "cases": records}


def capture_path_prompt() -> dict[str, Any]:
    from vibe.core.autocompletion.path_prompt import build_path_prompt_payload

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="autocompletion-prompt-") as scratch_dir:
        root = Path(scratch_dir) / "root"
        root.mkdir()
        materialize(root, {"tree": PROMPT_TREE, "gitignore": None})
        resolved_root = root.resolve()

        def placed(path: Path) -> str:
            try:
                return path.relative_to(resolved_root).as_posix()
            except ValueError as error:
                raise OracleError(f"{path} resolved outside the prompt fixture") from error

        for template in PROMPT_MESSAGES:
            message = template.replace("{root}", resolved_root.as_posix())
            payload = build_path_prompt_payload(message, base_dir=root)
            stats_types: dict[str, int] = {}
            stats_extensions: dict[str, int] = {}
            for resource in payload.all_resources:
                stats_types[resource.kind] = stats_types.get(resource.kind, 0) + 1
                if resource.kind == "file":
                    suffix = resource.path.suffix
                    stats_extensions[suffix] = stats_extensions.get(suffix, 0) + 1
            records.append(
                {
                    "case": template or "<empty>",
                    "message": template,
                    "resources": [
                        {
                            "alias": resource.alias.replace(
                                resolved_root.as_posix(), "{root}"
                            ),
                            "kind": resource.kind,
                            "path": placed(resource.path),
                        }
                        for resource in payload.resources
                    ],
                    "allAliases": [
                        resource.alias.replace(resolved_root.as_posix(), "{root}")
                        for resource in payload.all_resources
                    ],
                    "mentions": {
                        "count": len(payload.all_resources),
                        "contextTypes": stats_types,
                        "fileExtensions": stats_extensions,
                    },
                }
            )
    return {"tree": PROMPT_TREE, "cases": records}


def capture_watch_filter() -> list[dict[str, Any]]:
    from watchfiles import Change, DefaultFilter

    accept = DefaultFilter()
    return [
        {
            "case": "/".join(segments),
            "segments": segments,
            "allowed": accept(Change.added, os.sep.join(["", "watched", *segments])),
        }
        for segments in WATCH_FILTER_PROBES
    ]


def capture_fuzzy() -> dict[str, Any]:
    from vibe.cli.autocompletion.fuzzy import fuzzy_match

    pairs = fuzzy_pairs()
    scores: list[Any] = []
    for pattern, text in pairs:
        try:
            result = fuzzy_match(pattern, text)
        except IndexError:
            scores.append("raises")
            continue
        scores.append(_centis(result.score) if result.matched else None)
    return {
        "seed": FUZZY_SEED,
        "count": FUZZY_COUNT,
        "alphabet": FUZZY_ALPHABET,
        "canary": [list(pair) for pair in pairs[:8]],
        "scores": scores,
    }


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
            "matcher computes in. The git families run in a repository the script initializes "
            "with the system and global git configuration disabled; the collect and controller "
            "families run with the working directory at a fixture root, because the reference "
            "completes against the process working directory; the fuzzy family records only the "
            "seed, the alphabet and the scores of a sweep both sides draw from the same SplitMix64 "
            "stream. Regenerate with scripts/parity/autocompletion.py --corpus "
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
        git_walk = capture_git_walk()
        git_changes = capture_git_changes()
        collect = capture_collect()
        controller = capture_controller()
        inline_skill = capture_inline_skill()
        path_prompt = capture_path_prompt()
        watch_filter = capture_watch_filter()
        fuzzy = capture_fuzzy()
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
        "gitWalk": git_walk,
        "gitChanges": git_changes,
        "collect": collect,
        "controller": controller,
        "inlineSkill": inline_skill,
        "pathPrompt": path_prompt,
        "watchFilter": watch_filter,
        "fuzzy": fuzzy,
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
        "gitWalk": len(git_walk["cases"]),
        "gitChanges": len(git_changes["cases"]),
        "collect": len(collect["cases"]),
        "controller": len(controller),
        "inlineSkill": len(inline_skill["cases"]),
        "pathPrompt": len(path_prompt["cases"]),
        "watchFilter": len(watch_filter),
        "fuzzy": len(fuzzy["scores"]),
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
