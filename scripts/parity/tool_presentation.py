#!/usr/bin/env python3
"""Capture how the pinned Python reference *presents* a tool call and its result.

The tool-surface oracle compares declarations and the tool-execution oracle
compares results. Neither one looks at the header a client draws while a call
runs or after it settles. Since v3.0 that header comes from one place: the
harness reports every call generically, under its namespaced key with a raw
display, and the app server rewrites the calls a client renders richly into a
typed effect detail and settles their results
(`vibe/app_server/_unified_tool_projection.py`, ``project_unified_history_entry``,
over the wire models of `vibe/app_server/models.py` and
`vibe/app_server/_effect_models.py`). This port mirrors it in
`crates/vibe-harness/src/tool_projection.rs`.

Up to v2.26 this oracle drove the legacy ``ToolUIDataAdapter``
(`vibe/core/tools/ui.py`) over every builtin tool class. v3.0 deleted that
adapter with the legacy harness, and nothing in the ``vibe`` package builds a
``ToolCallPresentation`` any more, so the oracle now measures its successor.
The families, each a scenario name prefix (`tool_presentation_cases.py`):

``call-``
    Every projected tool as it starts, both spellings where the harness has
    two (``read_file`` and ``file_system.read_file``).
``invalid-``, ``unknown-``
    Arguments a projector refuses and a tool no projector knows, which stay as
    the harness sent them.
``result-``, ``failed-``
    Each projected tool settling: the typed output and the settled header, the
    large-output receipt, and a failed call whose state is kept.
``shell-grep-``
    Shell calls the projection renders as a file search, and the ones whose
    flags, pipes or output shape keep the plain shell display.
``path-``
    The file tools over the path shapes a header renders differently, with the
    session directory as the display root.
``label-``
    Namespaced tools that only get a human header, as they start and settle.
``callback-``
    The call an approval request describes.

The capture validates each authored entry with the reference's own wire model
(``validate_history_entry``), so the scenario a replay starts from carries
every default the model fills. A scenario declares whether the reference
rewrites the entry or leaves it as it came, and the capture fails when it
disagrees, so a scenario cannot drift into measuring nothing.

Two artifacts come out of a run:

``.parity/tool-presentation-corpus.json``
    The full capture, gitignored, because a header carries reference-authored
    prose and ``NOTICE`` forbids shipping that.

``crates/vibe-harness/tests/tool-presentation/corpus.json``
    The committed projection, which the Rust replay reads unconditionally. A
    captured string survives verbatim only when the scenario supplied it, or it
    is an identifier or a path; everything else becomes a ``{prose, sha256}``
    digest. The corpus therefore carries names, pointers, counts and digests,
    and no reference sentence, while a digest still fails the replay on any
    change.

Usage::

    scripts/parity/tool_presentation.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter, importing the
pinned commit extracted with ``git archive`` (``scripts/parity/reference.py``).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from inprocess import SocketGuard
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from tool_presentation_cases import scenarios

SCHEMA_VERSION = 3
DEFAULT_OUTPUT = Path(".parity/tool-presentation-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-harness/tests/tool-presentation/corpus.json")

#: The session every entry belongs to.
SESSION_ID = "parity-session"

#: An identifier-shaped token: a verb, a status word, an enum member. The bright
#: line is that it carries no whitespace and no punctuation beyond the hyphen,
#: so no sentence can pass for one.
_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,31}")

#: A path or a size: segments, dots and separators, and nothing that could
#: hold a sentence.
_POINTER = re.compile(r"[A-Za-z0-9._~/-]{1,128}")


#: One guard for the process, consulted by ``main`` after the capture. A
#: projection is pure, so nothing here has a reason to open a socket.
GUARD = SocketGuard()


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------


def generic_display(tool: str) -> dict[str, Any]:
    """The raw header a harness reports before any projection: the tool key."""

    return {
        "summary": tool,
        "verb": "Running",
        "message": tool,
        "settledVerb": "Ran",
        "settledMessage": tool,
        "statusText": f"Running {tool}",
    }


def raw_entry(case: dict[str, Any]) -> dict[str, Any]:
    """The wire entry a harness would report for ``case``."""

    detail = {
        "kind": "tool",
        "toolName": case["toolName"],
        "input": case["input"],
        "display": generic_display(case["toolName"]),
    }
    base = {
        "id": case["name"],
        "sessionId": SESSION_ID,
        "createdAt": 1,
        "updatedAt": 1,
        "title": case["toolName"],
    }
    if case["kind"] == "callback":
        return {
            **base,
            "type": "callback",
            "generationStatus": "in_progress",
            "callbackId": f"callback-{case['name']}",
            "detail": {"kind": "approval", "effect": detail},
            "state": {"status": "open"},
        }
    return {
        **base,
        "type": "effect",
        "generationStatus": "completed",
        "detail": detail,
        "state": case["state"],
    }


def capture() -> list[dict[str, Any]]:
    """One record per scenario, in table order."""

    from vibe.app_server._unified_tool_projection import project_unified_history_entry
    from vibe.app_server.models import validate_history_entry

    records: list[dict[str, Any]] = []
    names: set[str] = set()
    for case in scenarios():
        if case["name"] in names:
            raise OracleError(f"two scenarios are named {case['name']}")
        names.add(case["name"])
        source = validate_history_entry(raw_entry(case))
        roots = [Path(root) for root in case["displayRoots"]]
        projected = project_unified_history_entry(source, roots)
        rewritten = projected != source
        if rewritten != (case["expect"] == "projected"):
            raise OracleError(
                f"{case['name']} expects the entry {case['expect']}, and the reference "
                f"{'rewrote' if rewritten else 'kept'} it"
            )
        dumped = projected.model_dump(mode="json", by_alias=True)
        observed: dict[str, Any] = {"detail": dumped["detail"]}
        if case["kind"] == "effect":
            observed["state"] = dumped["state"]
        records.append(
            {
                "name": case["name"],
                "scenario": {
                    "entry": source.model_dump(mode="json", by_alias=True),
                    "displayRoots": case["displayRoots"],
                },
                "observed": observed,
            }
        )
    return records


# --------------------------------------------------------------------------
# The committable projection
# --------------------------------------------------------------------------


def digest(value: str) -> dict[str, Any]:
    """Prose as its length in code points and its SHA-256, never its content."""

    return {"prose": len(value), "sha256": hashlib.sha256(value.encode("utf-8")).hexdigest()}


def literal_values(value: Any, into: set[str]) -> None:
    """Every string ``value`` holds, at any depth."""

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

    Three shapes may: a value the scenario supplied and the projection reads
    back, an identifier-shaped token, and a path or size. Each is a name or a
    pointer, which is what ``NOTICE`` allows. Anything else is treated as
    reference-authored prose and committed as a digest.
    """

    return (
        value in authored
        or bool(_IDENTIFIER.fullmatch(value))
        or bool(_POINTER.fullmatch(value))
    )


def project(value: Any, authored: set[str]) -> Any:
    """The committable form: names and pointers verbatim, prose as a digest."""

    if isinstance(value, dict):
        return {key: project(item, authored) for key, item in value.items()}
    if isinstance(value, list):
        return [project(item, authored) for item in value]
    if isinstance(value, str) and not keeps_literal(value, authored):
        return digest(value)
    return value


def project_record(record: dict[str, Any]) -> dict[str, Any]:
    """One scenario projected, with its own inputs as the authored vocabulary.

    The scenario is this repository's own text plus the defaults the
    reference's wire model fills, and stays verbatim: a replay feeds it to this
    port. A default that is not a name would be reference text, so the capture
    refuses it rather than ship it.
    """

    authored: set[str] = set()
    literal_values(record["scenario"], authored)
    supplied: set[str] = set()
    for case in scenarios():
        if case["name"] == record["name"]:
            literal_values(case, supplied)
            literal_values(raw_entry(case), supplied)
    leaked = sorted(
        value
        for value in authored
        if value and value not in supplied and not _IDENTIFIER.fullmatch(value)
    )
    if leaked:
        raise OracleError(f"{record['name']} carries a default that is not a name: {leaked}")
    return {
        "name": record["name"],
        "scenario": record["scenario"],
        "observed": project(record["observed"], authored),
    }


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "platform": platform.system().lower(),
        "note": (
            "Tool presentation corpus: what the pinned reference's unified tool projection "
            "makes of each generic effect entry a harness reports. A presentation string is "
            "committed as it stands only when the scenario supplied it, or it is an identifier "
            "or a path; everything else is committed as a {prose, sha256} digest, so no "
            "reference prose ships while any change still fails the replay. Regenerate with "
            "scripts/parity/tool_presentation.py --corpus when the pinned reference moves."
        ),
        "scenarios": [project_record(record) for record in records],
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
        records = capture()
        if GUARD.attempts:
            # Checked before anything is written, so a capture that reached for
            # the network leaves no partial corpus behind.
            raise OracleError(
                "the capture attempted network access: " + "; ".join(sorted(set(GUARD.attempts)))
            )
        rendered = rendered_corpus(records, reference)
        if arguments.check:
            target = arguments.corpus or DEFAULT_CORPUS
            if not target.is_file():
                raise OracleError(f"no committed corpus at {target} to check against")
            if target.read_text(encoding="utf-8") != rendered:
                raise OracleError(
                    f"a fresh capture differs from the committed corpus at {target}"
                )
            print(f"{target} matches a fresh capture of {len(records)} scenarios")
            return 0
        full = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
            "scenarios": records,
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
            arguments.corpus.write_text(rendered, encoding="utf-8", newline="")
    except OracleError as error:
        print(f"tool-presentation capture failed: {error}", file=sys.stderr)
        return 1
    families: dict[str, int] = {}
    for record in records:
        family = record["name"].split("-", 1)[0]
        families[family] = families.get(family, 0) + 1
    print(
        f"captured {len(records)} scenarios ("
        + ", ".join(f"{family} {count}" for family, count in sorted(families.items()))
        + f") from {reference['commit'][:12]} into {arguments.output}"
    )
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
