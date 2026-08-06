#!/usr/bin/env python3
"""Capture what the pinned Python reference's tools actually *do*.

The three existing oracles compare declarations: method inventories,
configuration censuses, tool schemas. None of them answers "does this tool
behave the same". This one drives the reference tools over a fixture tree
checked into this repository and records, per case, the typed result, the text
the agent loop would send to the model, and whether the call returned or raised.

Two artifacts come out of a run:

``.parity/tool-execution-corpus.json``
    The full capture, gitignored, because a tool result carries
    reference-authored prose (error messages, warning banners, truncation
    notices) and ``NOTICE`` forbids shipping that.

``crates/vibe-app-server/tests/tool-execution/corpus.json``
    The committed projection, which the Rust replay reads unconditionally. A
    captured string survives verbatim only when it is a value the case supplied,
    a normalized path or an identifier-shaped token; everything else is replaced
    by its SHA-256 digest. The corpus therefore carries names, pointers, counts
    and digests, and no reference sentence, while a digest still detects any
    change, which is what a conformance target has to do.

Usage::

    scripts/parity/tool_execution.py --reference /path/to/reference

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter when the current
one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/tool-execution-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-app-server/tests/tool-execution/corpus.json")
DEFAULT_TREE = Path("crates/vibe-app-server/tests/tool-execution/tree")

#: What a materialized dotfile is called in the checked-in tree, so a fixture
#: `.gitignore` cannot affect this repository's own git behavior.
DOT_PREFIX = "dot-"

#: Stands in for the materialized tree root, which is a fresh temporary
#: directory on every run and on every machine.
TREE_PLACEHOLDER = "{tree}"

#: Where the extracted pinned tree is cached between runs. Gitignored, and
#: keyed by commit so a re-pin extracts a new one instead of reusing the old.
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: An identifier-shaped token: an enum member, a status word, an exception class
#: name. The bright line is that it carries no whitespace and no punctuation, so
#: no sentence can pass for one, while the names AC2 allows stay readable in a
#: divergence report.
_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9_]{0,31}")


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
    """The pinned commit, taken from the checkout without depending on its HEAD.

    The other capture scripts require the checkout to *be* on the pin, which
    makes a workstation that moved it unable to capture at all, and tempts a
    re-pin by accident. This one only requires the checkout to *contain* the pin:
    it reads the commit out with ``git archive`` and never touches the working
    tree, which is what the read-only constraint asks for.
    """

    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
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
    branch or adding a worktree, so the reference checkout is observed and never
    modified.
    """

    import tarfile

    # Absolute, because `_git` runs with the reference as its working directory
    # and a relative output path would resolve inside the read-only checkout.
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
    """Re-runs this script under an interpreter importing the *pinned* tree.

    The virtual environment supplies the third-party dependencies and the
    extracted tree supplies ``vibe`` itself, so the capture measures the pinned
    commit even when the checkout sits elsewhere.
    """

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
        [str(tree), *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else [])]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


# --------------------------------------------------------------------------
# The fixture tree
# --------------------------------------------------------------------------


def materialize_tree(source: Path, destination: Path) -> None:
    """Copies the checked-in tree, restoring the dotfiles it stores prefixed.

    A fixture `.gitignore` committed as such would apply to this repository, and
    a fixture dotfile would be invisible to a casual reader. Both are stored as
    ``dot-<name>`` and restored here, so the materialized tree is what the tools
    actually see.
    """

    if not source.is_dir():
        raise OracleError(f"no fixture tree at {source}")
    for entry in sorted(source.rglob("*")):
        relative = entry.relative_to(source)
        parts = [
            f".{part[len(DOT_PREFIX):]}" if part.startswith(DOT_PREFIX) else part
            for part in relative.parts
        ]
        target = destination.joinpath(*parts)
        if entry.is_dir():
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(entry, target)


# --------------------------------------------------------------------------
# Cases
# --------------------------------------------------------------------------


def cases() -> list[dict[str, Any]]:
    """Every execution case, ordered by tool then by name.

    ``mutates`` marks a case that writes, so the tree is materialized fresh for
    it and one case can never observe another's side effect.
    """

    return [
        # -- read_file ---------------------------------------------------
        {"tool": "read_file", "case": "whole-file", "args": {"file_path": "alpha.txt"}},
        {"tool": "read_file", "case": "offset", "args": {"file_path": "alpha.txt", "offset": 3}},
        {"tool": "read_file", "case": "limit", "args": {"file_path": "alpha.txt", "limit": 2}},
        {
            "tool": "read_file",
            "case": "offset-and-limit",
            "args": {"file_path": "alpha.txt", "offset": 2, "limit": 2},
        },
        {
            "tool": "read_file",
            "case": "offset-past-the-end",
            "args": {"file_path": "alpha.txt", "offset": 99},
        },
        {
            "tool": "read_file",
            "case": "offset-one-past-the-last-line",
            "args": {"file_path": "alpha.txt", "offset": 6},
        },
        {"tool": "read_file", "case": "empty-file", "args": {"file_path": "empty.txt"}},
        {"tool": "read_file", "case": "crlf-file", "args": {"file_path": "crlf.txt"}},
        {"tool": "read_file", "case": "nested-file", "args": {"file_path": "nested/beta.py"}},
        {"tool": "read_file", "case": "missing-file", "args": {"file_path": "nowhere.txt"}},
        {"tool": "read_file", "case": "a-directory", "args": {"file_path": "nested"}},
        {"tool": "read_file", "case": "empty-path", "args": {"file_path": ""}},
        # -- grep --------------------------------------------------------
        {"tool": "grep", "case": "lowercase-is-case-insensitive", "args": {"pattern": "gather"}},
        {"tool": "grep", "case": "uppercase-is-case-sensitive", "args": {"pattern": "Gather"}},
        {"tool": "grep", "case": "no-match", "args": {"pattern": "zzz-absent-zzz"}},
        {
            "tool": "grep",
            "case": "scoped-to-a-subdirectory",
            "args": {"pattern": "gather", "path": "nested"},
        },
        {
            "tool": "grep",
            "case": "scoped-to-one-file",
            "args": {"pattern": "value", "path": "nested/beta.py"},
        },
        {"tool": "grep", "case": "max-matches", "args": {"pattern": "value", "max_matches": 2}},
        {
            "tool": "grep",
            "case": "ignore-honored",
            "args": {"pattern": "gather", "use_default_ignore": True},
        },
        {
            "tool": "grep",
            "case": "ignore-disabled",
            "args": {"pattern": "gather", "use_default_ignore": False},
        },
        {"tool": "grep", "case": "invalid-regex", "args": {"pattern": "gather("}},
        {
            "tool": "grep",
            "case": "missing-path",
            "args": {"pattern": "gather", "path": "nowhere"},
        },
        {"tool": "grep", "case": "regex-alternation", "args": {"pattern": "gather|scatter"}},
        {"tool": "grep", "case": "anchored", "args": {"pattern": "^def "}},
        # -- write_file --------------------------------------------------
        {
            "tool": "write_file",
            "case": "new-file",
            "mutates": True,
            "args": {"file_path": "written.txt", "content": "written body\n"},
        },
        {
            "tool": "write_file",
            "case": "existing-file-is-refused",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "content": "overwrite\n"},
        },
        {
            "tool": "write_file",
            "case": "missing-parent",
            "mutates": True,
            "args": {"file_path": "made/up/deep.txt", "content": "deep body\n"},
        },
        {
            "tool": "write_file",
            "case": "empty-content",
            "mutates": True,
            "args": {"file_path": "blank.txt", "content": ""},
        },
        # -- edit --------------------------------------------------------
        {
            "tool": "edit",
            "case": "single-replacement",
            "mutates": True,
            "args": {
                "file_path": "alpha.txt",
                "old_string": "alpha two",
                "new_string": "alpha II",
            },
        },
        {
            "tool": "edit",
            "case": "replace-all",
            "mutates": True,
            "args": {
                "file_path": "alpha.txt",
                "old_string": "alpha",
                "new_string": "ALPHA",
                "replace_all": True,
            },
        },
        {
            "tool": "edit",
            "case": "ambiguous-without-replace-all",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "alpha", "new_string": "ALPHA"},
        },
        {
            "tool": "edit",
            "case": "absent-old-string",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "absent", "new_string": "x"},
        },
        {
            "tool": "edit",
            "case": "identical-strings",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "alpha one", "new_string": "alpha one"},
        },
        {
            "tool": "edit",
            "case": "empty-old-string",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "", "new_string": "x"},
        },
        {
            "tool": "edit",
            "case": "crlf-is-preserved",
            "mutates": True,
            "args": {
                "file_path": "crlf.txt",
                "old_string": "second line",
                "new_string": "changed line",
            },
        },
        {
            "tool": "edit",
            "case": "missing-file",
            "mutates": True,
            "args": {"file_path": "nowhere.txt", "old_string": "a", "new_string": "b"},
        },
        # -- todo --------------------------------------------------------
        {
            "tool": "todo",
            "case": "write-two",
            "args": {
                "action": "write",
                "todos": [
                    {"id": "one", "content": "first task", "status": "pending"},
                    {"id": "two", "content": "second task", "status": "in_progress"},
                ],
            },
        },
        {"tool": "todo", "case": "read-empty", "args": {"action": "read"}},
        {
            "tool": "todo",
            "case": "duplicate-identifiers",
            "args": {
                "action": "write",
                "todos": [
                    {"id": "same", "content": "first", "status": "pending"},
                    {"id": "same", "content": "second", "status": "pending"},
                ],
            },
        },
        {
            "tool": "todo",
            "case": "write-empty-list",
            "args": {"action": "write", "todos": []},
        },
        {"tool": "todo", "case": "unknown-action", "args": {"action": "delete"}},
    ]


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------


def tool_classes() -> dict[str, Any]:
    """The reference tool classes this oracle drives, by published name."""

    from vibe.core.tools.builtins.edit import Edit
    from vibe.core.tools.builtins.grep import Grep
    from vibe.core.tools.builtins.read_file import ReadFile
    from vibe.core.tools.builtins.todo import Todo
    from vibe.core.tools.builtins.write_file import WriteFile

    return {
        cls.get_name(): cls for cls in (ReadFile, Grep, WriteFile, Edit, Todo)
    }


def build_tool(cls: Any, tree: Path, scratchpad: Path) -> Any:
    """A tool instance bound to the materialized tree, with declared defaults.

    The configuration is the tool's own default: this oracle measures behavior,
    and per-tool configuration is EP-031's subject.
    """

    from vibe.core.config.harness_files import HarnessFilesManager

    config_class = cls._get_tool_config_class()
    config = config_class()
    harness = HarnessFilesManager(sources=("project",)).for_session(tree)
    return cls.from_config(
        lambda: config,
        cwd=tree,
        harness_files=harness,
        scratchpad_dir=scratchpad,
    )


async def run_case(case: dict[str, Any], tree: Path, scratchpad: Path) -> dict[str, Any]:
    """Drives one case and records what an agent loop would have observed."""

    from vibe.core.tools.base import InvokeContext, ToolError
    from vibe.core.types import ToolStreamEvent

    classes = tool_classes()
    cls = classes[case["tool"]]
    tool = build_tool(cls, tree, scratchpad)

    # A path argument is written relative to the tree so the case list stays
    # host-independent; the reference wants the absolute form its cwd resolves.
    arguments = dict(case["args"])
    for key in ("file_path", "path"):
        if key in arguments and isinstance(arguments[key], str) and arguments[key]:
            arguments[key] = str(tree / arguments[key])

    context = InvokeContext(tool_call_id="call-1", scratchpad_dir=scratchpad)
    record: dict[str, Any] = {
        "tool": case["tool"],
        "case": case["case"],
        "arguments": case["args"],
    }
    try:
        result_model = None
        async for item in tool.invoke(context, **arguments):
            if not isinstance(item, ToolStreamEvent):
                result_model = item
        if result_model is None:
            raise ToolError("Tool did not yield a result")
    except Exception as error:  # noqa: BLE001 - the outcome is the measurement
        record["outcome"] = "raised"
        record["error"] = {"type": type(error).__name__, "message": str(error)}
        return record

    typed = result_model.model_dump(mode="json")
    # Exactly what `_loop.py` sends to the model: the field-per-line rendering
    # plus whatever the tool appends through `get_result_extra`.
    text = "\n".join(f"{key}: {value}" for key, value in typed.items())
    extra = tool.get_result_extra(result_model)
    if extra:
        text += "\n\n" + extra
    record["outcome"] = "returned"
    record["typedResult"] = typed
    record["modelText"] = text
    return record


async def capture(tree_source: Path) -> list[dict[str, Any]]:
    """Every case, each mutating one running against a freshly copied tree."""

    records = []
    with tempfile.TemporaryDirectory(prefix="vibe-parity-") as scratch:
        scratchpad = Path(scratch) / "scratchpad"
        scratchpad.mkdir(parents=True, exist_ok=True)
        shared = Path(scratch) / "shared"
        materialize_tree(tree_source, shared)
        for index, case in enumerate(cases()):
            if case.get("mutates"):
                tree = Path(scratch) / f"case-{index}"
                materialize_tree(tree_source, tree)
            else:
                tree = shared
            record = await run_case(case, tree.resolve(), scratchpad)
            records.append(normalize(record, tree.resolve(), scratchpad))
    return records


# --------------------------------------------------------------------------
# Normalization and the committed projection
# --------------------------------------------------------------------------


def normalize(value: Any, tree: Path, scratchpad: Path) -> Any:
    """Replaces host-specific values so the corpus replays on another machine.

    The tree root and the scratchpad are fresh temporary directories on every
    run, so a captured absolute path is meaningless anywhere else. Both collapse
    to a placeholder, and the separator is normalized so a Windows capture and a
    POSIX one produce the same corpus.
    """

    if isinstance(value, dict):
        return {key: normalize(item, tree, scratchpad) for key, item in value.items()}
    if isinstance(value, list):
        return [normalize(item, tree, scratchpad) for item in value]
    if isinstance(value, str):
        replaced = value.replace(str(tree), TREE_PLACEHOLDER)
        replaced = replaced.replace(str(scratchpad), "{scratchpad}")
        return replaced.replace("\\", "/") if TREE_PLACEHOLDER in replaced else replaced
    return value


def digest(value: str) -> str:
    """A string's identity without its content."""

    return "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:32]


def literal_values(value: Any, into: set[str]) -> None:
    """Every string the case's own arguments supplied, at any depth."""

    if isinstance(value, dict):
        for item in value.values():
            literal_values(item, into)
    elif isinstance(value, list):
        for item in value:
            literal_values(item, into)
    elif isinstance(value, str):
        into.add(value)


def keeps_literal(value: str, authored: set[str]) -> bool:
    """Whether a captured string may be committed as it stands.

    Three shapes may: a value this corpus supplied as an argument and is only
    reading back, a normalized path, and an identifier-shaped token such as an
    enum member. Every one of those is a name or a pointer, which is what
    ``NOTICE`` allows. Anything else is treated as reference-authored prose,
    including a short error message, and is committed as a digest.
    """

    if value in authored:
        return True
    if value.startswith((TREE_PLACEHOLDER, "{scratchpad}")) and " " not in value:
        return True
    # A lowercase identifier carries no sentence: prose has spaces, capitals or
    # punctuation long before it reaches this shape.
    return bool(_IDENTIFIER.fullmatch(value))


def project(value: Any, authored: set[str]) -> Any:
    """The committable form: names and pointers verbatim, prose as a digest.

    A digest still fails the replay on any change, so the corpus stays a
    conformance target while shipping none of the reference's text.
    """

    if isinstance(value, dict):
        return {key: project(item, authored) for key, item in value.items()}
    if isinstance(value, list):
        return [project(item, authored) for item in value]
    if isinstance(value, str) and not keeps_literal(value, authored):
        return {"length": len(value), "digest": digest(value)}
    return value


def project_case(record: dict[str, Any]) -> dict[str, Any]:
    """One case projected, with its own arguments as the authored vocabulary.

    An error *message* is recorded by presence only, never by content. The PRD
    lists byte-identical error text as a non-goal for the same licensing reason
    as tool descriptions: a message must name the same cause, value and limit,
    not reproduce the reference's wording. Comparing a digest would quietly
    demand exactly that. The full text stays in the gitignored artifact for a
    human to read.
    """

    authored: set[str] = set()
    literal_values(record.get("arguments"), authored)
    projected = {
        key: (value if key in ("tool", "case", "outcome", "arguments") else project(value, authored))
        for key, value in record.items()
    }
    if isinstance(projected.get("error"), dict):
        message = record["error"].get("message")
        projected["error"]["message"] = {"present": bool(message)}
    return projected


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": reference["commit"],
        "platform": platform.system().lower(),
        "note": (
            "Tool execution corpus: what the pinned reference's tools return for each case over "
            "the checked-in fixture tree. A captured string is committed as it stands only when "
            "it is a value these arguments supplied, a normalized path, or an identifier-shaped "
            "token; everything else, including every error message, is committed as a SHA-256 "
            "digest and its length, so no reference prose ships while any change still fails the "
            "replay. Regenerate with scripts/parity/tool_execution.py --corpus when the pinned "
            "reference moves."
        ),
        "cases": [project_case(record) for record in records],
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--tree", type=Path, default=DEFAULT_TREE)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="?",
        const=DEFAULT_CORPUS,
        default=None,
        help=(
            "also write the committed corpus, which the Rust replay reads unconditionally "
            f"(default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        records = asyncio.run(capture(arguments.tree))
        full = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
            "cases": records,
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
                json.dumps(
                    build_corpus(records, reference), indent=2, sort_keys=True, ensure_ascii=False
                )
                + "\n",
                encoding="utf-8",
            )
    except OracleError as error:
        print(f"tool-execution capture failed: {error}", file=sys.stderr)
        return 1
    returned = sum(1 for record in records if record["outcome"] == "returned")
    print(
        f"captured {len(records)} cases ({returned} returned, {len(records) - returned} raised) "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
