#!/usr/bin/env python3
"""Capture how the pinned Python reference resolves tool description overrides.

The reference checkout is a read-only behavioral oracle. This script asks it two
questions over a scratch tree this file authors:

``ToolManager._compute_search_paths`` — in which order are the builtin
directory, the ``tool_paths`` entries, the project tool directories and the user
tool directory walked, and how are duplicates folded away?

``ToolManager._iter_tool_descriptions`` and ``available_tool_specs`` — which
``<tools-dir>/prompts/<name>.md`` file ends up describing a tool, and which ones
are skipped for being blank, unreadable or named after nothing?

Every description file the capture reads is authored here, so the committed
corpus carries this repository's own prose and never the reference's, which is
what ``NOTICE`` asks for. The one reference-authored quantity the corpus holds
is a *count*: how many builtin tools the reference describes from its package
directory, which is the position this port has no counterpart for because it
compiles its builtin descriptions in.

Usage::

    scripts/parity/tool_descriptions.py --corpus
    scripts/parity/tool_descriptions.py --check

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
an interpreter that imports the pinned tree rather than the working checkout.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/tool-descriptions-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/tool-descriptions/corpus.json")
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: The label standing in for the reference's builtin tool directory, which is a
#: path inside the pinned tree and therefore machine-dependent.
BUILTIN_LABEL = "<builtin>"

#: Where the scratch vibe home lives inside the tree, and therefore where the
#: user tool directory hangs off.
VIBE_HOME_LABEL = "home/.vibe"


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
    """The pinned commit, taken from the checkout without depending on its HEAD."""

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE to the checkout "
            "path or pass --reference"
        )
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs."""

    import tarfile

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


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


# --------------------------------------------------------------------------
# The network boundary
# --------------------------------------------------------------------------


class SocketGuard:
    """Refuses every destination, and remembers what was attempted.

    Nothing here should reach for a socket: the capture reads directories and
    constructs a tool manager with its MCP integration deferred. An attempt is
    remembered and fails the run by name before anything is written.
    """

    def __init__(self) -> None:
        self.attempts: list[str] = []
        self._installed = False

    def _record(self, address: Any, name: str) -> None:
        self.attempts.append(f"{name} to {address!r}")

    def install(self) -> None:
        if self._installed:
            return
        self._installed = True
        guard = self

        def guarded_connect(self: socket.socket, address: Any) -> Any:
            guard._record(address, "socket.connect")
            raise OracleError(f"the capture attempted to reach {address!r}")

        def guarded_connect_ex(self: socket.socket, address: Any) -> Any:
            guard._record(address, "socket.connect_ex")
            raise OracleError(f"the capture attempted to reach {address!r}")

        def guarded_create_connection(
            address: Any, *arguments: Any, **keywords: Any
        ) -> Any:
            guard._record(address, "socket.create_connection")
            raise OracleError(f"the capture attempted to reach {address!r}")

        def guarded_getaddrinfo(host: Any, *arguments: Any, **keywords: Any) -> Any:
            guard._record(host, "socket.getaddrinfo")
            raise OracleError(f"the capture attempted to resolve {host!r}")

        socket.socket.connect = guarded_connect  # type: ignore[method-assign]
        socket.socket.connect_ex = guarded_connect_ex  # type: ignore[method-assign]
        socket.create_connection = guarded_create_connection  # type: ignore[assignment]
        socket.getaddrinfo = guarded_getaddrinfo  # type: ignore[assignment]


#: One guard for the process, consulted by ``main`` after the capture.
GUARD = SocketGuard()


# --------------------------------------------------------------------------
# The scratch tree
# --------------------------------------------------------------------------

#: Every description file the capture writes, as ``label -> text``. The text is
#: this repository's own prose: a corpus carrying the reference's would be the
#: thing ``NOTICE`` forbids.
TREE_FILES: dict[str, str] = {
    "extra/prompts/read_file.md": "read a file, from a tool_paths entry\n",
    "extra/prompts/bash.md": "run a command, from a tool_paths entry\n",
    "module/custom.py": "# A tool module that declares no tool class.\n",
    "module/prompts/todo_write.md": "track a plan, from a module sibling\n",
    ".vibe/tools/prompts/read_file.md": "read a file, per project\n",
    ".vibe/tools/prompts/grep.md": "search, per project\n",
    ".vibe/tools/prompts/write_file.md": "   \n\t\n",
    ".vibe/tools/prompts/weather.md": "no tool answers to this name\n",
    "tools/prompts/glob.md": "match paths, from a relative entry\n",
    "home/.vibe/tools/prompts/read_file.md": "read a file, per user\n",
}

#: Directories created where a file is expected, which is the one spelling of
#: "cannot be read" every host and every privilege level agrees on: the
#: reference swallows the resulting ``OSError`` and falls back.
TREE_OBSTRUCTIONS: tuple[str, ...] = (".vibe/tools/prompts/unreadable.md",)

#: Directories the tree needs beyond the ones its files imply.
TREE_DIRECTORIES: tuple[str, ...] = ("absent-parent",)

#: The scenarios, each one a configuration the reference is asked to resolve.
CASES: tuple[dict[str, Any], ...] = (
    {
        "name": "no-configured-paths",
        "sources": ["user", "project"],
        "trusted": True,
        "toolPaths": [],
    },
    {
        "name": "configured-paths-precede-the-discovered-ones",
        "sources": ["user", "project"],
        "trusted": True,
        "toolPaths": ["extra", "module/custom.py"],
    },
    {
        "name": "a-directory-named-twice-keeps-its-first-position",
        "sources": ["user", "project"],
        "trusted": True,
        "toolPaths": [".vibe/tools", "extra"],
    },
    {
        "name": "an-entry-naming-nothing-is-skipped",
        "sources": ["user", "project"],
        "trusted": True,
        "toolPaths": ["absent-parent/absent", "extra"],
    },
    {
        "name": "a-relative-entry-anchors-on-the-working-directory",
        "sources": ["user", "project"],
        "trusted": True,
        "toolPaths": ["tools"],
    },
    {
        "name": "the-user-source-disabled",
        "sources": ["project"],
        "trusted": True,
        "toolPaths": ["extra"],
    },
    {
        "name": "an-untrusted-workspace",
        "sources": ["user", "project"],
        "trusted": False,
        "toolPaths": ["extra"],
    },
)


def materialize(root: Path) -> None:
    """Writes the scratch tree under ``root``."""

    for relative in TREE_DIRECTORIES:
        (root / relative).mkdir(parents=True, exist_ok=True)
    for label, text in TREE_FILES.items():
        target = root / label
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8", newline="\n")
    for label in TREE_OBSTRUCTIONS:
        (root / label).mkdir(parents=True, exist_ok=True)


def label_of(root: Path, path: Path, builtin: Path) -> str:
    """``path`` as a tree-relative label, or the builtin placeholder."""

    resolved = Path(path)
    if resolved == builtin or resolved.is_relative_to(builtin):
        return BUILTIN_LABEL
    try:
        return resolved.relative_to(root).as_posix()
    except ValueError:
        raise OracleError(f"the capture resolved a path outside the tree: {resolved}")


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


class AlwaysTrusted:
    """The trust verdict a trusted workspace gets, without a trust file."""

    def __init__(self, trusted: bool) -> None:
        self._trusted = trusted

    def is_trusted(self, path: Path) -> bool | None:
        return True if self._trusted else None


def capture(root: Path) -> list[dict[str, Any]]:
    """What the reference answers for every case, over the scratch tree."""

    from vibe.core.config import VibeConfigSchema
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.core.config.harness_files._harness_manager import HarnessFilesManager
    from vibe.core.paths import DEFAULT_TOOL_DIR
    from vibe.core.tools.manager import ToolManager  # noqa: F401

    init_harness_files_manager("user", "project")
    builtin = DEFAULT_TOOL_DIR.path.resolve()
    #: The authored text of every file in the tree, which is how a resolved
    #: description is traced back to the file that won without re-walking the
    #: search paths in this script.
    authored = {text: label for label, text in TREE_FILES.items() if text.strip()}
    if len(authored) != len({t for t in TREE_FILES.values() if t.strip()}):
        raise OracleError("two description files share their text")

    records: list[dict[str, Any]] = []
    for case in CASES:
        workdir = root
        harness = HarnessFilesManager(
            sources=tuple(case["sources"]),
            cwd=workdir,
            trust_store=AlwaysTrusted(case["trusted"]),
        )
        with contextlib.chdir(workdir):
            # `tool_paths` is expanded by the schema against the process
            # directory, so the capture stands in the session's directory for
            # exactly as long as the configuration is built and read.
            config = VibeConfigSchema(tool_paths=list(case["toolPaths"]))
            manager = ToolManager(
                lambda config=config: config,
                defer_mcp=True,
                cwd=workdir,
                harness_files=harness,
            )
            search_paths = list(manager._search_paths)
            resolved = dict(manager._tool_descriptions)
            specs = {
                spec.name: spec.description for spec in manager.available_tool_specs()
            }

        overrides: dict[str, str] = {}
        builtin_described = 0
        for stem, text in sorted(resolved.items()):
            label = authored.get(text)
            if label is None:
                builtin_described += 1
                continue
            overrides[stem] = label
        stems_in_tree = sorted(
            {Path(label).stem for label in TREE_FILES if label.endswith(".md")}
            | {Path(label).stem for label in TREE_OBSTRUCTIONS}
        )
        records.append(
            {
                "name": case["name"],
                "sources": sorted(case["sources"]),
                "trusted": case["trusted"],
                "toolPaths": list(case["toolPaths"]),
                "searchPaths": [label_of(root, path, builtin) for path in search_paths],
                "overrides": overrides,
                # A stem whose file the search paths reached and which still
                # describes nothing: blank, unreadable, or in a directory this
                # case does not walk.
                "withheldStems": [
                    stem for stem in stems_in_tree if stem not in overrides
                ],
                # A stem that describes something no tool answers to, which the
                # reference carries in its map and drops at publication.
                "unmatchedStems": sorted(
                    stem for stem in overrides if stem not in specs
                ),
                # What the model is shown, restricted to the tools this tree
                # redescribes, so no reference-authored description is recorded.
                "publishedDescriptions": {
                    name: specs[name] for name in sorted(overrides) if name in specs
                },
                "builtinDescribedCount": builtin_described,
            }
        )
    return records


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Captured from the pinned reference by scripts/parity/tool_descriptions.py. "
            "Every description file and every path label is authored by this repository; "
            "the reference contributes the resolution order, the winning file and the "
            "count of builtin tools it describes from its own package directory."
        ),
        "builtinLabel": BUILTIN_LABEL,
        "vibeHome": VIBE_HOME_LABEL,
        "tree": {
            "files": dict(sorted(TREE_FILES.items())),
            "obstructions": sorted(TREE_OBSTRUCTIONS),
            "directories": sorted(TREE_DIRECTORIES),
        },
        "cases": records,
    }


def rendered_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> str:
    return (
        json.dumps(
            build_corpus(records, reference), indent=2, sort_keys=True, ensure_ascii=False
        )
        + "\n"
    )


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
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "capture and compare against the committed corpus instead of rewriting it; "
            "a difference exits non-zero, which is what proves a re-run with no change "
            "in between is byte-identical"
        ),
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    # No capture may read this machine's stored credentials or its real vibe
    # home: both are replaced before `vibe` is imported.
    os.environ.setdefault("VIBE_TEST_DISABLE_KEYRING", "1")
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(
            arguments.reference, arguments.python, pinned
        )
        GUARD.install()
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch).resolve()
            materialize(root)
            os.environ["VIBE_HOME"] = str(root / VIBE_HOME_LABEL)
            records = capture(root)
        if GUARD.attempts:
            raise OracleError(
                "the capture attempted network access: "
                + "; ".join(sorted(set(GUARD.attempts)))
            )
        if arguments.check:
            target = arguments.corpus or DEFAULT_CORPUS
            if not target.is_file():
                raise OracleError(f"no committed corpus at {target} to check against")
            if target.read_text(encoding="utf-8") != rendered_corpus(records, reference):
                raise OracleError(
                    f"a fresh capture differs from the committed corpus at {target}"
                )
            print(f"{target} matches a fresh capture of {len(records)} cases")
            return 0
        full = {
            **build_corpus(records, reference),
            "platform": platform.system().lower(),
            "python": platform.python_version(),
        }
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(
            json.dumps(full, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        os.replace(staged, arguments.output)
        if arguments.corpus is not None:
            arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
            arguments.corpus.write_text(
                rendered_corpus(records, reference), encoding="utf-8"
            )
    except OracleError as error:
        print(f"tool-descriptions capture failed: {error}", file=sys.stderr)
        return 1
    print(
        f"captured {len(records)} cases from {reference['commit'][:12]} "
        f"into {arguments.output}"
    )
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
