#!/usr/bin/env python3
"""Capture what the pinned Python reference does with a tool directory's files.

The reference checkout is a read-only behavioral oracle. Up to the v2 line this
script measured how ``ToolManager`` walked the builtin directory, the
``tool_paths`` entries and the project and user tool directories, and which
``<tools-dir>/prompts/<name>.md`` file then described a tool. The v3 line
deleted that machinery: custom Python tools and the ``tool_paths`` setting are
gone (``vibe/core/config/_migration.py`` drops the key at startup), and
``vibe/core/tools/manager.py`` no longer computes search paths, reads override
files or publishes tool specs. A builtin's description is its packaged prompt
(``BaseTool.get_full_description`` in ``vibe/core/tools/base.py``).

What remains, and what schema version 2 records, is the successor the session
runtime wires in ``vibe/app_server/_runtime.py``: the project and user tool
directories the harness files manager lists, deduplicated once resolved, and
the files ``vibe/app_server/_ignored_tool_settings.py`` reports there as
having no effect (a custom tool module, and the one description override it
names). The warning sentences are reference-authored, so only the branch that
produced each one is recorded. Each scenario also records whether every
builtin tool the tree redescribes still publishes its packaged description,
which is the observable trace of the removed overrides.

``toolPaths`` stays a scenario input although the reference no longer reads
it: this port still honors it, and the replay holds that difference as a gap.

Every file the capture writes is authored here, so the committed corpus
carries this repository's own text and never the reference's, which is what
``NOTICE`` asks for.

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
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from inprocess import SocketGuard

SCHEMA_VERSION = 2
DEFAULT_OUTPUT = Path(".parity/tool-descriptions-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/tool-descriptions/corpus.json")
#: Where the scratch vibe home lives inside the tree, and therefore where the
#: user tool directory hangs off.
VIBE_HOME_LABEL = "home/.vibe"


# --------------------------------------------------------------------------
# The network boundary
# --------------------------------------------------------------------------


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
    ".vibe/tools/weather.py": "# A custom tool module, listed and never imported.\n",
    ".vibe/tools/_helpers.py": "# A private module, which the listing skips.\n",
    "home/.vibe/tools/forecast.py": "# A custom tool module in the user directory.\n",
    "home/.vibe/tools/prompts/task.md": "delegate work, overriding the former task tool\n",
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


def label_of(root: Path, path: Path) -> str:
    """``path`` as a tree-relative label."""

    resolved = Path(path).resolve()
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


def ignored_kind(file: Path, message: str) -> str:
    """The branch of ``ignored_tool_settings`` that reported ``file``.

    The sentence is reference-authored, so it is matched against the
    reference's own builders and never recorded.
    """

    from vibe.app_server._ignored_tool_settings import (
        SPAWN_DESCRIPTION_OVERRIDE_IGNORED,
        custom_tool_unsupported,
    )

    if message == custom_tool_unsupported(Path(file).stem):
        return "customTool"
    if message == SPAWN_DESCRIPTION_OVERRIDE_IGNORED:
        return "descriptionOverride"
    raise OracleError(f"unknown ignored-tool-settings branch for {file}")


def redescribable_tools() -> dict[str, Any]:
    """The builtin tool classes a tree file is named after, by tool name."""

    from vibe.core.tools.manager import builtin_tool_classes

    stems = {Path(label).stem for label in TREE_FILES if label.endswith(".md")}
    classes: dict[str, Any] = {}
    for cls in builtin_tool_classes():
        name = cls.get_name()
        if name in stems:
            classes.setdefault(name, cls)
    if not classes:
        raise OracleError("no tree file is named after a builtin tool")
    return classes


def capture(root: Path) -> tuple[list[str], list[dict[str, Any]]]:
    """What the reference answers for every case, over the scratch tree."""

    from vibe.app_server._ignored_tool_settings import ignored_tool_settings
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.core.config.harness_files._harness_manager import HarnessFilesManager

    init_harness_files_manager("user", "project")
    authored = {text for text in TREE_FILES.values() if text.strip()}
    redescribable = redescribable_tools()

    records: list[dict[str, Any]] = []
    for case in CASES:
        harness = HarnessFilesManager(
            sources=tuple(case["sources"]),
            cwd=root,
            trust_store=AlwaysTrusted(case["trusted"]),
        )
        with contextlib.chdir(root):
            # The same directories, in the same order and deduplicated the
            # same way, as the session runtime hands the listing.
            tool_dirs = list(
                dict.fromkeys(
                    directory.resolve()
                    for directory in (
                        *harness.project_tools_dirs,
                        *harness.user_tools_dirs,
                    )
                )
            )
            issues = ignored_tool_settings(tool_dirs)
            unchanged = all(
                cls.get_full_description() not in authored
                for cls in redescribable.values()
            )
        records.append(
            {
                "name": case["name"],
                "scenario": {
                    "sources": sorted(case["sources"]),
                    "trusted": case["trusted"],
                    "toolPaths": list(case["toolPaths"]),
                },
                "observed": {
                    "toolDirs": [label_of(root, path) for path in tool_dirs],
                    "ignored": sorted(
                        (
                            {
                                "file": label_of(root, Path(issue.file)),
                                "kind": ignored_kind(Path(issue.file), issue.message),
                            }
                            for issue in issues
                        ),
                        key=lambda entry: (entry["file"], entry["kind"]),
                    ),
                    "descriptionsUnchanged": unchanged,
                },
            }
        )
    return sorted(redescribable), records


def build_corpus(
    redescribable: list[str], records: list[dict[str, Any]], reference: dict[str, str]
) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Captured from the pinned reference by scripts/parity/tool_descriptions.py. "
            "Every file and every path label is authored by this repository; the "
            "reference contributes the tool directories it lists, the branch that "
            "reported each ignored file and whether its builtin descriptions stayed "
            "its own."
        ),
        "vibeHome": VIBE_HOME_LABEL,
        "tree": {
            "files": dict(sorted(TREE_FILES.items())),
            "obstructions": sorted(TREE_OBSTRUCTIONS),
            "directories": sorted(TREE_DIRECTORIES),
        },
        "redescribable": redescribable,
        "scenarios": records,
    }


def rendered_corpus(
    redescribable: list[str], records: list[dict[str, Any]], reference: dict[str, str]
) -> str:
    return (
        json.dumps(
            build_corpus(redescribable, records, reference),
            indent=2,
            sort_keys=True,
            ensure_ascii=False,
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
        commit, _ = reexecute_pinned(
            arguments.reference,
            arguments.expected_commit,
            cache=arguments.cache,
            override=arguments.python,
        )
        reference = {"commit": commit, "path": str(arguments.reference)}
        GUARD.install()
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch).resolve()
            materialize(root)
            os.environ["VIBE_HOME"] = str(root / VIBE_HOME_LABEL)
            redescribable, records = capture(root)
        if GUARD.attempts:
            raise OracleError(
                "the capture attempted network access: "
                + "; ".join(sorted(set(GUARD.attempts)))
            )
        if arguments.check:
            target = arguments.corpus or DEFAULT_CORPUS
            if not target.is_file():
                raise OracleError(f"no committed corpus at {target} to check against")
            fresh = rendered_corpus(redescribable, records, reference)
            if target.read_text(encoding="utf-8") != fresh:
                raise OracleError(
                    f"a fresh capture differs from the committed corpus at {target}"
                )
            print(f"{target} matches a fresh capture of {len(records)} cases")
            return 0
        full = {
            **build_corpus(redescribable, records, reference),
            "platform": platform.system().lower(),
            "python": platform.python_version(),
        }
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(
            json.dumps(full, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
            encoding="utf-8",
            newline="",
        )
        os.replace(staged, arguments.output)
        if arguments.corpus is not None:
            arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
            arguments.corpus.write_text(
                rendered_corpus(redescribable, records, reference),
                encoding="utf-8",
                newline="",
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
