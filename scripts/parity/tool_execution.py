#!/usr/bin/env python3
"""Capture what the pinned Python reference's tools actually *do*.

The declaration oracles compare method inventories, configuration censuses and
tool schemas. None of them answers "does this tool behave the same". This one
calls the reference's own tool executor, records per case the typed result, the
text handed back for the model, and whether the call succeeded or failed.

What still executes in Python
=============================

Up to v2.26 every builtin tool carried its own body (``BaseTool.run``) and the
legacy agent loop invoked it in process, so this oracle drove eleven tools over
a checked-in fixture tree. v3.0 removed the legacy harness: the builtin classes
under `vibe/core/tools/builtins/` now declare arguments, results, configuration
and permissions only, and `vibe/core/tools/base.py` publishes no ``invoke``.
Where each measured tool went:

- ``read_file``, ``write_file``, ``edit``: the local harness runtime's file
  tools (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/
  _file_tools.py` and `_sandbox_helper.py`), a separate distribution driven by
  the native core under `harness/core/`.
- ``grep``: dropped from the harness on purpose; the model searches through the
  shell, and `vibe/app_server/_unified_tool_projection.py` only renders a plain
  ``grep``/``rg`` shell call as a search.
- ``skill``: the harness runtime answers the call
  (`mistralai_vibe_local_harness/vibe/_skill_tools.py`), serving a body the app
  server renders beforehand (`vibe/app_server/_skills.py` over
  `vibe/core/tools/builtins/skill.py`); the lookup, the unknown-name failure and
  the reuse notice no longer run in ``vibe``.
- ``task``: the harness subagents (`mistralai_vibe_local_harness/vibe/
  _subagents/`).
- ``ask_user_question``, ``web_fetch``, ``web_search``: harness tools; their
  ``vibe`` modules keep only the declarations and the permission resolution.
- ``exit_plan_mode``: deleted, with its module and its prompt.

Those families are dropped rather than redirected into the harness runtime,
which is not the ``vibe`` package this oracle measures. One family resolves:
``todo`` is a tool the app server provides to the harness, executed in
``vibe`` by ``VibeProvidedTools`` (`vibe/app_server/_provided_tools.py`) over
``read_todos`` and ``write_todos`` (`vibe/core/tools/builtins/todo.py`). The
capture calls that executor with the action the harness core would send and
records the event it answers.

Schema version 3 follows from that boundary. A success records the structured
content as ``typedResult`` and the text blocks as ``modelText``; a failure
records the protocol error code as ``error.type``, because the exception class
no longer leaves the executor. ``projectedResult`` is gone: no tool class
publishes a ``project_result`` hook any more, and the app server's projection
of a settled call (`vibe/app_server/_unified_tool_projection.py`) is a
presentation concern, not an execution one. Prose is committed as a
``{prose, sha256}`` digest, the shape `vibe_core::parity::replay` compares as
one value.

Two artifacts come out of a run:

``.parity/tool-execution-corpus.json``
    The full capture, gitignored, because a tool result carries
    reference-authored prose (error messages, result sentences) and ``NOTICE``
    forbids shipping that.

``crates/vibe-app-server/tests/tool-execution/corpus.json``
    The committed projection, which the Rust replay reads unconditionally. A
    captured string survives verbatim only when it is a value the case supplied
    or an identifier-shaped token; everything else is replaced by its SHA-256
    digest. The corpus therefore carries names, counts and digests, and no
    reference sentence, while a digest still detects any change, which is what
    a conformance target has to do.

Usage::

    scripts/parity/tool_execution.py --reference /path/to/reference

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter, importing the
pinned commit extracted with ``git archive`` (``scripts/parity/reference.py``).
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
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from inprocess import SocketGuard
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from tool_execution_cases import cases

SCHEMA_VERSION = 3
DEFAULT_OUTPUT = Path(".parity/tool-execution-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-app-server/tests/tool-execution/corpus.json")

#: The session every case runs in. Each case builds a fresh executor, so the
#: name only has to be stable, not unique.
SESSION_ID = "parity-session"

#: An identifier-shaped token: an enum member, a status word, a protocol error
#: code. The bright line is that it carries no whitespace and no punctuation
#: beyond the hyphen, so no sentence can pass for one, while the names the
#: corpus may carry stay readable in a divergence report.
_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,31}")


#: One guard for the process, consulted by ``main`` after the capture. Nothing
#: the todo executor does has a reason to open a socket.
GUARD = SocketGuard()


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------


async def run_case(case: dict[str, Any], storage: Path) -> dict[str, Any]:
    """Drives one case through the provided-tool executor and records its event."""

    from mistralai_vibe_local_harness.protocol import (
        RustProvidedToolCall,
        RustProvidedToolCallAction,
        RustToolSucceededEvent,
    )

    from vibe.app_server._provided_tools import (
        TODO_TOOL_NAME,
        VIBE_TOOL_GROUP,
        VibeProvidedTools,
        resolved_max_todos,
    )

    if case["tool"] != TODO_TOOL_NAME:
        raise OracleError(f"no executor in the reference's vibe package runs {case['tool']}")

    # The limit the app server reads when `[tools.todo]` sets nothing.
    limit = resolved_max_todos(None)
    execute = VibeProvidedTools().executor_factory(storage, max_todos=lambda: limit)(
        SESSION_ID
    )
    action = RustProvidedToolCallAction(
        action_id="action-1",
        turn_id="turn-1",
        call_id="call-1",
        call=RustProvidedToolCall(
            group_name=VIBE_TOOL_GROUP,
            tool_name=TODO_TOOL_NAME,
            arguments=case["args"],
        ),
    )
    event = await execute(action)

    record: dict[str, Any] = {
        "tool": case["tool"],
        "case": case["case"],
        "arguments": case["args"],
    }
    if isinstance(event, RustToolSucceededEvent):
        record["outcome"] = "returned"
        record["typedResult"] = event.result.structured_content
        record["modelText"] = "\n".join(
            block.text for block in event.result.content if hasattr(block, "text")
        )
    else:
        # A failure records the protocol code and whether a message came with
        # it. The text itself stays out of the committed projection, which
        # `project_case` enforces, so no reference sentence ships.
        error = event.result.error
        record["outcome"] = "raised"
        record["error"] = {"type": error.code, "message": error.message}
    return record


async def capture() -> list[dict[str, Any]]:
    """Every case, each against an executor of its own."""

    records = []
    with tempfile.TemporaryDirectory(prefix="vibe-parity-") as scratch:
        for index, case in enumerate(cases()):
            storage = Path(scratch) / f"case-{index}"
            storage.mkdir(parents=True, exist_ok=True)
            records.append(await run_case(case, storage))
    return records


# --------------------------------------------------------------------------
# The committed projection
# --------------------------------------------------------------------------


def sha256(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def digest(value: str) -> dict[str, Any]:
    """Prose as its length in code points and its SHA-256, never its content:
    the ``{prose, sha256}`` shape `vibe_core::parity::replay` compares as one
    value."""

    return {"prose": len(value), "sha256": sha256(value)}


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

    Two shapes may: a value this corpus supplied as an argument and is only
    reading back, and an identifier-shaped token such as an enum member. Both
    are names, which is what ``NOTICE`` allows. Anything else is treated as
    reference-authored prose, including a short error message, and is committed
    as a digest.
    """

    if value in authored:
        return True
    # An identifier carries no sentence: prose has spaces or punctuation long
    # before it reaches this shape.
    return bool(_IDENTIFIER.fullmatch(value))


def project(value: Any, authored: set[str]) -> Any:
    """The committable form: names verbatim, prose as a digest.

    A digest still fails the replay on any change, so the corpus stays a
    conformance target while shipping none of the reference's text.
    """

    if isinstance(value, dict):
        return {key: project(item, authored) for key, item in value.items()}
    if isinstance(value, list):
        return [project(item, authored) for item in value]
    if isinstance(value, str) and not keeps_literal(value, authored):
        return digest(value)
    return value


def project_case(record: dict[str, Any]) -> dict[str, Any]:
    """One case projected, with its own arguments as the authored vocabulary.

    An error *message* is recorded as its presence and its digest, never as its
    content. The digest is what makes a re-pin that reworded a message visible
    in this file's diff; it is deliberately not a conformance target, because
    the PRD lists byte-identical error text as a non-goal for the same licensing
    reason as tool descriptions: a message must name the same cause, value and
    limit, not reproduce the reference's wording. The full text stays in the
    gitignored artifact for a human to read.
    """

    authored: set[str] = set()
    literal_values(record.get("arguments"), authored)
    projected = {
        key: (
            value
            if key in ("tool", "case", "outcome", "arguments")
            else project(value, authored)
        )
        for key, value in record.items()
    }
    if isinstance(projected.get("error"), dict):
        message = record["error"].get("message") or ""
        projected["error"]["message"] = {
            "present": bool(message),
            **({"sha256": sha256(message)} if message else {}),
        }
    return projected


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": reference["commit"],
        "platform": platform.system().lower(),
        "note": (
            "Tool execution corpus: what the pinned reference's provided-tool executor answers "
            "for each case, the only tool execution left in its vibe package. A captured string "
            "is committed as it stands only when it is a value these arguments supplied or an "
            "identifier-shaped token; everything else, including every error message, is "
            "committed as a SHA-256 digest and its length, so no reference prose ships while any "
            "change still fails the replay. Regenerate with scripts/parity/tool_execution.py "
            "--corpus when the pinned reference moves."
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


def rendered_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> str:
    return (
        json.dumps(build_corpus(records, reference), indent=2, sort_keys=True, ensure_ascii=False)
        + "\n"
    )


def main() -> int:
    arguments = parse_arguments()
    # No capture may read this machine's stored credentials.
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
        records = asyncio.run(capture())
        if GUARD.attempts:
            # Checked before anything is written, so a capture that reached for
            # the network leaves no partial corpus behind.
            raise OracleError(
                "the capture attempted network access: " + "; ".join(sorted(set(GUARD.attempts)))
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
            newline="",
        )
        os.replace(staged, arguments.output)
        if arguments.corpus is not None:
            arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
            arguments.corpus.write_text(
                rendered_corpus(records, reference), encoding="utf-8", newline=""
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
