#!/usr/bin/env python3
"""Capture how the pinned Python reference *presents* every tool it publishes.

The tool-surface oracle compares declarations and the tool-execution oracle
compares results. Neither one looks at the header a client draws while a call
runs or after it settles, which is the contract `vibe/core/tools/ui.py`
publishes through ``ToolUIDataAdapter``: an effect kind, eight call-display
fields and five result-display fields, plus the projected output a widget
renders.

This oracle builds one adapter per published tool and drives both presentation
entry points over six cases: a call with valid arguments, a call whose arguments
never arrived, a call carrying arguments of the wrong class, a successful
result, an errored result and a skipped one. The tool list is the Linux builtin
surface plus two stubs built by the reference's own remote factories, so the
remote half of the contract is captured without a live server.

Two artifacts come out of a run:

``.parity/tool-presentation-corpus.json``
    The full capture, gitignored, because a display carries reference-authored
    prose and ``NOTICE`` forbids shipping that.

``crates/vibe-core/tests/tool-presentation/corpus.json``
    The committed projection, which the Rust replay reads unconditionally. A
    captured string survives verbatim only when it is a value this capture
    supplied or an identifier-shaped token; everything else becomes a
    ``{"described": "sha256:...", "length": n}`` marker. The corpus therefore
    carries names, pointers, counts and digests, and no reference sentence,
    while a digest still fails the replay on any change, which is what a
    conformance target has to do.

Usage::

    scripts/parity/tool_presentation.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter when the current
one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import socket
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/tool-presentation-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/tool-presentation/corpus.json")

#: Where the extracted pinned tree is cached between runs. Gitignored, and
#: keyed by commit so a re-pin extracts a new one instead of reusing the old.
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: The marker a projected string carries in place of its content. The key is
#: what makes a described value unmistakable in the corpus and in Rust, where a
#: literal is a JSON string and a described value is a JSON object.
DESCRIBED = "described"

#: An identifier-shaped token: a verb, a status word, an enum member. The bright
#: line is that it carries no whitespace and no punctuation beyond the hyphen,
#: so no sentence can pass for one, while the names the corpus is allowed to
#: carry stay readable in a divergence report.
_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,31}")

#: A path or a request target: a pointer, and nothing that could hold a
#: sentence.
_POINTER = re.compile(r"/[A-Za-z0-9._~/-]{0,95}")


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

    The checkout only has to *contain* the pin, not sit on it, so a workstation
    that moved HEAD can still capture and is never tempted into a re-pin by
    accident. ``git archive`` reads the commit out below without touching the
    working tree, which is what the read-only constraint asks for.
    """

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
# The network boundary
# --------------------------------------------------------------------------


class SocketGuard:
    """Refuses every destination, and remembers what was attempted.

    Nothing this capture drives should reach for a socket: every presentation
    entry point is a classmethod over authored arguments, and the two remote
    stubs are built from a descriptor written below rather than discovered from
    a server. A capture that connected anyway would be recording an answer
    nobody can reproduce, so every attempt is remembered and fails the run by
    name before anything is written.
    """

    def __init__(self) -> None:
        self.attempts: list[str] = []
        self._installed = False

    @staticmethod
    def _host(address: Any) -> str | None:
        if not isinstance(address, (tuple, list)) or not address:
            return None
        host = address[0]
        if isinstance(host, bytes):
            return host.decode("ascii", "replace")
        return None if host is None else str(host)

    def _record(self, address: Any, name: str) -> None:
        host = self._host(address)
        self.attempts.append(f"{name} to {host if host is not None else address!r}")

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

        def guarded_create_connection(address: Any, *arguments: Any, **keywords: Any) -> Any:
            guard._record(address, "socket.create_connection")
            raise OracleError(f"the capture attempted to reach {address!r}")

        def guarded_getaddrinfo(host: Any, *arguments: Any, **keywords: Any) -> Any:
            guard._record((host,), "socket.getaddrinfo")
            raise OracleError(f"the capture attempted to resolve {host!r}")

        socket.socket.connect = guarded_connect  # type: ignore[method-assign]
        socket.socket.connect_ex = guarded_connect_ex  # type: ignore[method-assign]
        socket.create_connection = guarded_create_connection  # type: ignore[assignment]
        socket.getaddrinfo = guarded_getaddrinfo  # type: ignore[assignment]


#: One guard for the process, consulted by ``main`` after the capture.
GUARD = SocketGuard()


# --------------------------------------------------------------------------
# The remote stubs
# --------------------------------------------------------------------------

#: The descriptor both remote stubs are built from. Every field is authored
#: here rather than discovered from a server, so no third-party name, schema or
#: sentence enters the corpus and a re-run on a machine with no network is
#: identical to one on a machine with it.
REMOTE_NAME = "create_issue"
REMOTE_ALIAS = "acme"
REMOTE_DESCRIPTION = "a stub the capture wrote"
REMOTE_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "owner": {"type": "string"},
        "repo": {"type": "string"},
    },
    "required": ["owner", "repo"],
}

#: A destination that cannot resolve, so a stub built with it is inert even if
#: the socket guard were removed.
REMOTE_URL = "https://mcp.invalid/mcp"

#: The arguments and the result both remote stubs are driven with.
REMOTE_ARGUMENTS: dict[str, Any] = {"owner": "acme", "repo": "api"}
REMOTE_RESULT: dict[str, Any] = {
    "ok": True,
    "server": REMOTE_ALIAS,
    "tool": REMOTE_NAME,
    "text": "the stub answer",
    "structured": None,
}


# --------------------------------------------------------------------------
# The case list
# --------------------------------------------------------------------------

#: Valid arguments per builtin, authored so every field the display reads is
#: populated and every value is host-independent. A path here names nothing on
#: this machine: no tool runs, only its presentation is computed.
ARGUMENTS: dict[str, dict[str, Any]] = {
    "ask_user_question": {
        "questions": [
            {
                "question": "Which target?",
                "header": "Target",
                "options": [
                    {"label": "alpha", "description": "the first target"},
                    {"label": "beta", "description": "the second target"},
                ],
                "multiSelect": False,
            }
        ],
        "footerNote": "pick one",
    },
    "bash": {"command": "echo alpha", "timeout": 30},
    "edit": {
        "file_path": "/workspace/alpha.txt",
        "old_string": "one",
        "new_string": "two",
        "replace_all": False,
    },
    "exit_plan_mode": {},
    "grep": {"pattern": "alpha", "path": "/workspace", "max_matches": 20},
    "read_file": {"file_path": "/workspace/alpha.txt", "offset": 3, "limit": 20},
    "skill": {"name": "reviewer"},
    "task": {"task": "summarize the corpus", "agent": "explorer"},
    "todo": {
        "action": "write",
        "todos": [
            {
                "id": "t1",
                "content": "capture the presentation",
                "status": "in_progress",
                "priority": "high",
            }
        ],
    },
    "web_fetch": {"url": "https://example.invalid/doc", "timeout": 15},
    "web_search": {"query": "rust parity oracle"},
    "write_file": {"file_path": "/workspace/alpha.txt", "content": "alpha"},
}

#: A successful result per builtin, authored against the tool's declared result
#: model so ``format_result_display`` and ``project_result`` both run over a
#: fully populated instance.
RESULTS: dict[str, dict[str, Any]] = {
    "ask_user_question": {
        "answers": [
            {"question": "Which target?", "answer": "alpha", "isOther": False}
        ],
        "cancelled": False,
    },
    "bash": {"command": "echo alpha", "stdout": "alpha", "stderr": "", "returncode": 0},
    "edit": {
        "file": "/workspace/alpha.txt",
        "message": "replaced one occurrence",
        "old_string": "one",
        "new_string": "two",
    },
    "exit_plan_mode": {"switched": True, "message": "the plan was accepted"},
    "grep": {
        "matches": "/workspace/alpha.txt:1:alpha",
        "match_count": 1,
        "pattern": "alpha",
        "was_truncated": False,
        "cwd": "/workspace",
    },
    "read_file": {
        "file_path": "/workspace/alpha.txt",
        "content": "alpha",
        "num_lines": 1,
        "start_line": 3,
        "requested_offset": 3,
        "requested_limit": 20,
        "total_lines": 4,
        "was_truncated": False,
    },
    "skill": {
        "name": "reviewer",
        "content": "the skill body",
        "skill_dir": "/workspace/skills/reviewer",
    },
    "task": {"response": "the subagent answer", "turns_used": 2, "completed": True},
    "todo": {
        "verb": "Updated",
        "todos": [
            {
                "id": "t1",
                "content": "capture the presentation",
                "status": "in_progress",
                "priority": "high",
            }
        ],
        "total_count": 1,
    },
    "web_fetch": {
        "url": "https://example.invalid/doc",
        "content": "the fetched body",
        "content_type": "text/html; charset=utf-8",
        "was_truncated": False,
    },
    "web_search": {
        "query": "rust parity oracle",
        "answer": "the search answer",
        "sources": [{"title": "a source", "url": "https://example.invalid/doc"}],
    },
    "write_file": {
        "file_path": "/workspace/alpha.txt",
        "bytes_written": 5,
        "content": "alpha",
    },
}

#: The error a failed call reports. Authored here, so the corpus can carry it in
#: cleartext and a replay can assert the adapter forwards it unchanged.
ERROR_MESSAGE = "the capture authored this failure"

#: The six cases every tool is driven through, named as they appear in the
#: corpus and in a ledger entry.
CALL_CASES = ("valid-arguments", "absent-arguments", "wrong-argument-type")
RESULT_CASES = ("successful-result", "error-result", "skipped-result")


def capture(tree: Path) -> list[dict[str, Any]]:
    """One record per tool and case, in tool order then case order."""

    from pydantic import BaseModel

    from vibe.core.config import VibeConfigSchema
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.core.tools.connectors.connector_registry import (
        create_connector_proxy_tool_class,
    )
    from vibe.core.tools.manager import ToolManager
    from vibe.core.tools.mcp.tools import create_mcp_http_proxy_tool_class
    from vibe.core.tools.remote import RemoteTool
    from vibe.core.tools.ui import ToolUIDataAdapter
    from vibe.core.types import ToolCallEvent, ToolResultEvent

    class ForeignArgs(BaseModel):
        """An argument model no tool declares, which is what makes it wrong."""

        unexpected: str = "wrong"

    init_harness_files_manager()
    config = VibeConfigSchema()
    with tempfile.TemporaryDirectory() as workdir:
        manager = ToolManager(lambda: config, defer_mcp=True, cwd=Path(workdir))
        builtins = dict(manager.available_tools)

    remote = RemoteTool(
        name=REMOTE_NAME,
        description=REMOTE_DESCRIPTION,
        inputSchema=REMOTE_SCHEMA,
    )
    stubs = {
        "mcp": create_mcp_http_proxy_tool_class(
            url=REMOTE_URL, remote=remote, alias=REMOTE_ALIAS
        ),
        "connector": create_connector_proxy_tool_class(
            connector_name="Acme",
            connector_alias=REMOTE_ALIAS,
            connector_id="the-stub-connector",
            remote=remote,
            api_key="the-capture-authored-this-key",
        ),
    }

    entries: list[tuple[str, str, Any, dict[str, Any], dict[str, Any]]] = [
        ("builtin", name, cls, ARGUMENTS[name], RESULTS[name])
        for name, cls in sorted(builtins.items())
        if name in ARGUMENTS
    ]
    missing = sorted(set(builtins) - set(ARGUMENTS))
    if missing:
        raise OracleError(
            "the published surface grew and this capture has no case for "
            + ", ".join(missing)
        )
    entries += [
        (source, cls.get_name(), cls, REMOTE_ARGUMENTS, REMOTE_RESULT)
        for source, cls in stubs.items()
    ]

    records: list[dict[str, Any]] = []
    for source, name, cls, arguments, result in entries:
        adapter = ToolUIDataAdapter(cls)
        args_model, result_model = cls._get_tool_args_results()
        valid_args = args_model.model_validate(arguments)
        valid_result = result_model.model_validate(result)
        for case in CALL_CASES:
            event_args: BaseModel | None
            if case == "valid-arguments":
                event_args = valid_args
            elif case == "absent-arguments":
                event_args = None
            else:
                event_args = ForeignArgs()
            event = ToolCallEvent(
                tool_call_id="the-capture-call",
                tool_name=name,
                tool_class=cls,
                args=event_args,
            )
            records.append(
                {
                    "tool": name,
                    "source": source,
                    "case": case,
                    "phase": "call",
                    "arguments": arguments if case == "valid-arguments" else None,
                    "presentation": adapter.get_call_presentation(event).model_dump(
                        mode="json"
                    ),
                }
            )
        for case in RESULT_CASES:
            event = ToolResultEvent(
                tool_call_id="the-capture-call",
                tool_name=name,
                tool_class=cls,
                result=valid_result if case == "successful-result" else None,
                error=ERROR_MESSAGE if case == "error-result" else None,
                skipped=case == "skipped-result",
                skip_reason=None,
            )
            records.append(
                {
                    "tool": name,
                    "source": source,
                    "case": case,
                    "phase": "result",
                    "arguments": arguments,
                    "output": result if case == "successful-result" else None,
                    "error": ERROR_MESSAGE if case == "error-result" else None,
                    "skipped": case == "skipped-result",
                    "presentation": adapter.get_result_presentation(event).model_dump(
                        mode="json"
                    ),
                }
            )
    return records


# --------------------------------------------------------------------------
# The committable projection
# --------------------------------------------------------------------------


def digest(value: str) -> str:
    """A string's identity without its content."""

    return "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:32]


def literal_values(value: Any, into: set[str]) -> None:
    """Every string this capture supplied, at any depth."""

    if isinstance(value, dict):
        for key, item in value.items():
            into.add(key)
            literal_values(item, into)
    elif isinstance(value, list):
        for item in value:
            literal_values(item, into)
    elif isinstance(value, str):
        into.add(value)


def keeps_literal(value: str, authored: set[str]) -> bool:
    """Whether a captured string may be committed as it stands.

    Four shapes may: the empty string, which carries nothing; a value this
    capture supplied and is only reading back; a path or request target; and an
    identifier-shaped token such as a display verb. Every one of those is a name
    or a pointer, which is what ``NOTICE`` allows. Anything else is treated as
    reference-authored prose, including a two-word header, and is committed as a
    digest.
    """

    if not value or value in authored:
        return True
    if _POINTER.fullmatch(value):
        return True
    return bool(_IDENTIFIER.fullmatch(value))


def project(value: Any, authored: set[str]) -> Any:
    """The committable form: names and pointers verbatim, prose as a marker.

    A digest still fails the replay on any change, so the corpus stays a
    conformance target while shipping none of the reference's text.
    """

    if isinstance(value, dict):
        return {key: project(item, authored) for key, item in value.items()}
    if isinstance(value, list):
        return [project(item, authored) for item in value]
    if isinstance(value, str) and not keeps_literal(value, authored):
        return {DESCRIBED: digest(value), "length": len(value)}
    return value


def project_case(record: dict[str, Any]) -> dict[str, Any]:
    """One case projected, with its own inputs as the authored vocabulary.

    The arguments, the output and the error are this repository's own text and
    stay verbatim: a replay has to feed them to this port to compute the display
    it compares. Only the presentation is projected, because only the
    presentation is the reference's writing.
    """

    authored: set[str] = {ERROR_MESSAGE, REMOTE_NAME, REMOTE_ALIAS, record["tool"]}
    literal_values(record.get("arguments"), authored)
    literal_values(record.get("output"), authored)
    return {
        key: (project(value, authored) if key == "presentation" else value)
        for key, value in record.items()
    }


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": reference["commit"],
        "platform": platform.system().lower(),
        "note": (
            "Tool presentation corpus: what the pinned reference's ToolUIDataAdapter publishes "
            "for each tool and case, from get_call_presentation and get_result_presentation. A "
            "presentation string is committed as it stands only when it is a value this capture "
            "supplied, a path or an identifier-shaped token; everything else is committed as a "
            "{described, length} marker carrying a SHA-256 digest, so no reference prose ships "
            "while any change still fails the replay. Regenerate with "
            "scripts/parity/tool_presentation.py --corpus when the pinned reference moves."
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
    # No capture may read this machine's stored credentials: the only key any
    # stub here carries is the fixture one this script authors.
    os.environ.setdefault("VIBE_TEST_DISABLE_KEYRING", "1")
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        GUARD.install()
        records = capture(pinned)
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
        )
        os.replace(staged, arguments.output)
        if arguments.corpus is not None:
            arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
            arguments.corpus.write_text(
                rendered_corpus(records, reference), encoding="utf-8"
            )
    except OracleError as error:
        print(f"tool-presentation capture failed: {error}", file=sys.stderr)
        return 1
    calls = sum(1 for record in records if record["phase"] == "call")
    print(
        f"captured {len(records)} cases ({calls} calls, {len(records) - calls} results) "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
