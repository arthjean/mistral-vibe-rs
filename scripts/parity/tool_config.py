#!/usr/bin/env python3
"""Capture the per-tool configuration the pinned Python reference declares.

The reference checkout is a read-only behavioral oracle. This script asks it
what every tool class exposes as configuration: ``ToolManager`` enumerates the
builtin directory and each class answers with its own config model, whose
defaults are what an operator overrides through ``tools.<name>``. The Rust
differential runner replays that corpus against the declaration table in
``vibe_core::tools::config``.

The corpus is committed, unlike the tool-surface one: it records key names and
default values, which are observations, and never a field description, which is
reference-authored prose ``NOTICE`` forbids shipping.

Two shell list sets are captured rather than one. The reference composes the
shell allowlist, denylist and standalone denylist from ``uses_posix_shell()``,
so the values a capture sees are the ones its own host produces. Both branches
are recorded so the port can declare either without a second machine.

Usage::

    scripts/parity/tool_config.py --reference /path/to/reference
    scripts/parity/tool_config.py --interpreter /path/to/python

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with an interpreter that can import ``vibe`` when
the current one cannot.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/tool-config/defaults.json")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"
#: The keys whose defaults the reference composes from the running shell rather
#: than from a constant, captured once per branch.
SHELL_LIST_KEYS = ("allowlist", "denylist", "denylist_standalone")


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def resolve_reference(reference: Path, expected_commit: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"reference checkout is missing: {reference}")
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git rev-parse failed in {reference}: {result.stderr.strip()}"
        )
    commit = result.stdout.strip()
    if expected_commit and commit != expected_commit:
        raise OracleError(
            f"reference checkout is at {commit}, not the pinned {expected_commit}"
        )
    return {"commit": commit}


def reexecute_with_reference_interpreter(
    reference: Path, interpreter: Path | None
) -> None:
    """Re-runs this script under an interpreter that can import ``vibe``."""
    try:
        import vibe  # noqa: F401

        return
    except ImportError:
        pass
    candidates = [
        interpreter,
        Path(os.environ[INTERPRETER_VARIABLE])
        if os.environ.get(INTERPRETER_VARIABLE)
        else None,
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    candidate = next(
        (path for path in candidates if path is not None and path.is_file()), None
    )
    if candidate is None:
        raise OracleError(
            f"cannot import `vibe` and no reference interpreter under {reference}"
        )
    if Path(sys.executable).resolve() == candidate.resolve():
        raise OracleError(f"{candidate} cannot import `vibe`")
    os.execv(
        str(candidate), [str(candidate), str(Path(__file__).resolve()), *sys.argv[1:]]
    )


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture_declarations(reference: Path) -> dict[str, dict[str, Any]]:
    """What ``discover_tool_defaults`` answers for the builtin directory."""
    sys.path.insert(0, str(reference))
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.core.tools.manager import ToolManager

    init_harness_files_manager()
    declarations = ToolManager.discover_tool_defaults()
    if not declarations:
        raise OracleError("the reference enumerated no tool configuration")
    return {name: declarations[name] for name in sorted(declarations)}


def capture_shell_lists(reference: Path) -> dict[str, dict[str, list[str]]]:
    """The three host-dependent shell lists, under both shell branches.

    ``uses_posix_shell()`` is read by the default factories at model
    construction, so the branch is selected by replacing that name in the module
    that reads it. Nothing is written to the checkout: the substitution lives in
    this process only.
    """
    sys.path.insert(0, str(reference))
    from vibe.core.tools.builtins import bash as reference_bash

    original = reference_bash.uses_posix_shell
    captured: dict[str, dict[str, list[str]]] = {}
    try:
        for branch, posix in (("posix", True), ("windows", False)):
            reference_bash.uses_posix_shell = lambda posix=posix: posix
            captured[branch] = {
                "allowlist": list(reference_bash._get_default_allowlist()),
                "denylist": list(reference_bash._get_default_denylist()),
                "denylist_standalone": list(
                    reference_bash._get_default_denylist_standalone()
                ),
            }
    finally:
        reference_bash.uses_posix_shell = original
    return captured


def capture_default_document_tools(reference: Path) -> dict[str, dict[str, Any]]:
    """The ``tools`` table the shipped default document carries.

    ``create_default_config`` fills it from the same enumeration, so this is the
    published half of the same observation: what an operator reads back from a
    fresh configuration.
    """
    sys.path.insert(0, str(reference))
    from vibe.core.config.vibe_schema import create_default_config

    tools = create_default_config().get("tools")
    if not isinstance(tools, dict):
        raise OracleError("the default document carries no tools table")
    return {name: tools[name] for name in sorted(tools)}


def build_corpus(reference: Path, expected_commit: str | None) -> dict[str, Any]:
    pin = resolve_reference(reference, expected_commit)
    declarations = capture_declarations(reference)
    published = capture_default_document_tools(reference)
    if declarations != published:
        raise OracleError(
            "the enumerated defaults and the shipped default document disagree"
        )
    shell_lists = capture_shell_lists(reference)
    keys = sorted({key for entry in declarations.values() for key in entry})
    branch = "posix" if os.name != "nt" else "windows"
    # The shell families are the ones carrying a standalone denylist, and the
    # three lists they declare must be exactly what the host branch composes.
    for tool, entry in declarations.items():
        if "denylist_standalone" not in entry:
            continue
        for key in SHELL_LIST_KEYS:
            if entry[key] != shell_lists[branch][key]:
                raise OracleError(
                    f"tool `{tool}` declares a `{key}` the shell branches do not explain"
                )
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": (
            "Captured from the pinned reference by scripts/parity/tool_config.py. "
            "Tool names, configuration key names and default values are "
            "observations; no reference-authored description text is recorded here."
        ),
        "shellBranch": branch,
        "counts": {
            "tools": len(declarations),
            "keys": len(keys),
            "pairs": sum(len(entry) for entry in declarations.values()),
        },
        "keys": keys,
        "shellLists": shell_lists,
        "tools": declarations,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--interpreter",
        type=Path,
        default=None,
        help="Python that can import `vibe`; also read from " + INTERPRETER_VARIABLE,
    )
    parser.add_argument(
        "--allow-unpinned",
        action="store_true",
        help="capture from a checkout at another revision, for a re-pin",
    )
    arguments = parser.parse_args()

    try:
        reexecute_with_reference_interpreter(arguments.reference, arguments.interpreter)
        corpus = build_corpus(
            arguments.reference,
            None if arguments.allow_unpinned else EXPECTED_COMMIT,
        )
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    counts = corpus["counts"]
    print(
        f"wrote {arguments.output} "
        f"({counts['tools']} tools, {counts['keys']} keys, {counts['pairs']} pairs)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
