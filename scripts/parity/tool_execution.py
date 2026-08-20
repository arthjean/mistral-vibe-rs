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
import socket
import subprocess
import sys
import tempfile
import threading
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 2
DEFAULT_OUTPUT = Path(".parity/tool-execution-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-app-server/tests/tool-execution/corpus.json")
DEFAULT_TREE = Path("crates/vibe-app-server/tests/tool-execution/tree")

#: What a materialized dotfile is called in the checked-in tree, so a fixture
#: `.gitignore` cannot affect this repository's own git behavior.
DOT_PREFIX = "dot-"

#: Stands in for the materialized tree root, which is a fresh temporary
#: directory on every run and on every machine.
TREE_PLACEHOLDER = "{tree}"

#: Stands in for the loopback server's ``host:port``, which is an ephemeral port
#: chosen by the kernel on every run. A case writes it into its URL and the
#: capture substitutes the live authority in, so no port number reaches the
#: corpus and a re-run on another port is byte-identical.
SERVER_PLACEHOLDER = "{server}"

#: The environment variable the loopback provider reads its key from. The value
#: is a fixture string this script authors and hands to a socket bound to
#: ``127.0.0.1``; no real credential is read and no real endpoint is contacted.
SEARCH_KEY_VARIABLE = "VIBE_PARITY_SEARCH_KEY"
SEARCH_KEY_VALUE = "parity-loopback-key"

#: The only destination a capture may reach. Everything else fails the run.
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "::1", "localhost"})

#: Where the extracted pinned tree is cached between runs. Gitignored, and
#: keyed by commit so a re-pin extracts a new one instead of reusing the old.
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: An identifier-shaped token: an enum member, a status word, an exception class
#: name, an HTTP header name. The bright line is that it carries no whitespace
#: and no punctuation beyond the hyphen a header name spells, so no sentence can
#: pass for one, while the names AC2 allows stay readable in a divergence report.
_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,31}")

#: A request target: the path an HTTP request line carries, and nothing that
#: could hold a sentence.
_REQUEST_TARGET = re.compile(r"/[A-Za-z0-9._~/-]{0,63}")


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
# The network boundary
# --------------------------------------------------------------------------


class SocketGuard:
    """Refuses every destination but the loopback server this capture runs.

    Six of the eleven tools reach for a collaborator and two reach for the
    network. A capture that silently contacted a real endpoint would record an
    answer nobody can reproduce, so every connection is checked here and any
    non-loopback attempt is remembered and fails the run by name.
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

    def _allows(self, address: Any, name: str) -> bool:
        host = self._host(address)
        if host is not None and host in LOOPBACK_HOSTS:
            return True
        self.attempts.append(f"{name} to {host if host is not None else address!r}")
        return False

    def install(self) -> None:
        if self._installed:
            return
        self._installed = True
        guard = self
        connect = socket.socket.connect
        connect_ex = socket.socket.connect_ex
        create_connection = socket.create_connection
        getaddrinfo = socket.getaddrinfo

        def guarded_connect(self: socket.socket, address: Any) -> Any:
            if not guard._allows(address, "socket.connect"):
                raise OracleError(f"the capture attempted to reach {address!r}")
            return connect(self, address)

        def guarded_connect_ex(self: socket.socket, address: Any) -> Any:
            if not guard._allows(address, "socket.connect_ex"):
                raise OracleError(f"the capture attempted to reach {address!r}")
            return connect_ex(self, address)

        def guarded_create_connection(address: Any, *arguments: Any, **keywords: Any) -> Any:
            if not guard._allows(address, "socket.create_connection"):
                raise OracleError(f"the capture attempted to reach {address!r}")
            return create_connection(address, *arguments, **keywords)

        def guarded_getaddrinfo(host: Any, *arguments: Any, **keywords: Any) -> Any:
            if not guard._allows((host,), "socket.getaddrinfo"):
                raise OracleError(f"the capture attempted to resolve {host!r}")
            return getaddrinfo(host, *arguments, **keywords)

        socket.socket.connect = guarded_connect  # type: ignore[method-assign]
        socket.socket.connect_ex = guarded_connect_ex  # type: ignore[method-assign]
        socket.create_connection = guarded_create_connection  # type: ignore[assignment]
        socket.getaddrinfo = guarded_getaddrinfo  # type: ignore[assignment]


#: One guard for the process, consulted by ``main`` after the capture.
GUARD = SocketGuard()


def _response_body(spec: dict[str, Any]) -> bytes:
    """A response body, either written out or repeated to a declared size.

    The oversized case has to exceed ``max_content_bytes``, and spelling six
    figures of text into the case list would put six figures of text into the
    committed corpus for no gain. The repetition is declared instead.
    """

    repeat = spec.get("bodyRepeat")
    if repeat is not None:
        return (repeat["unit"] * repeat["count"]).encode("utf-8")
    if "json" in spec:
        return json.dumps(spec["json"], sort_keys=True).encode("utf-8")
    return spec.get("body", "").encode("utf-8")


class LoopbackServer:
    """A single-purpose HTTP origin bound to an ephemeral port on 127.0.0.1.

    It answers the scripted responses in order, repeating the last one when the
    client asks again, and records what actually went out on the wire so the
    corpus can carry the request the reference builds rather than only the
    answer it parsed.
    """

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = responses or [{"status": 200, "body": ""}]
        self.requests: list[dict[str, Any]] = []
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(8)
        self.port = self._listener.getsockname()[1]
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    @property
    def authority(self) -> str:
        return f"127.0.0.1:{self.port}"

    def close(self) -> None:
        self._listener.close()
        self._thread.join(timeout=1)

    def _serve(self) -> None:
        served = 0
        while True:
            try:
                stream, _ = self._listener.accept()
            except OSError:
                return
            with stream:
                try:
                    self._exchange(stream, served)
                except OSError:
                    return
            served += 1

    def _exchange(self, stream: socket.socket, served: int) -> None:
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = stream.recv(65536)
            if not chunk:
                return
            head += chunk
            # A client that opened a TLS handshake against this plain origin is
            # never going to send a request line. Hanging up now is what turns
            # the no-scheme case into a connection error instead of a timeout.
            if not head[:1].isalpha():
                return
        head, _, rest = head.partition(b"\r\n\r\n")
        lines = head.decode("latin-1").split("\r\n")
        method, _, target = lines[0].partition(" ")
        headers = [
            {"name": name, "value": value.strip()}
            for name, _, value in (line.partition(":") for line in lines[1:])
            if name
        ]
        length = next(
            (
                int(header["value"] or 0)
                for header in headers
                if header["name"].lower() == "content-length"
            ),
            0,
        )
        body = rest
        while len(body) < length:
            body += stream.recv(65536)
        self.requests.append(
            {
                "method": method,
                "target": target.partition(" ")[0],
                "headers": headers,
            }
        )
        spec = self.responses[min(served, len(self.responses) - 1)]
        payload = _response_body(spec)
        status = spec.get("status", 200)
        reason = spec.get("reason", "OK")
        rendered = [f"HTTP/1.1 {status} {reason}"]
        rendered += [f"{name}: {value}" for name, value in spec.get("headers", {}).items()]
        rendered += [f"Content-Length: {len(payload)}", "Connection: close", "", ""]
        stream.sendall("\r\n".join(rendered).encode("latin-1") + payload)


# --------------------------------------------------------------------------
# Scripted collaborators
# --------------------------------------------------------------------------


class ScriptedSkills:
    """The slice of the reference's skill manager that ``skill`` reads."""

    def __init__(self, skills: dict[str, Any]) -> None:
        self.available_skills = skills

    def get_skill(self, name: str) -> Any:
        return self.available_skills.get(name)


class ScriptedAnswers:
    """A user who answers by option index, never by reading a label.

    Selecting by index is what keeps this repository free of the reference's
    own option strings: ``exit_plan_mode`` builds its four labels itself, and
    the case says "the first one" rather than repeating what it says.
    """

    def __init__(self, plan: dict[str, Any]) -> None:
        self.plan = plan

    async def request_user_input(self, args: Any, tool_call_id: str) -> Any:
        from vibe.questions import UserAnswer, UserQuestionResult

        if self.plan.get("cancelled"):
            return UserQuestionResult(answers=[], cancelled=True)
        specifications = self.plan.get("answers", [])
        answers = []
        for index, question in enumerate(args.questions):
            spec = specifications[min(index, len(specifications) - 1)]
            if "other" in spec:
                answers.append(
                    UserAnswer(
                        question=question.question, answer=spec["other"], is_other=True
                    )
                )
                continue
            chosen = spec["options"] if "options" in spec else [spec["option"]]
            answers.append(
                UserAnswer(
                    question=question.question,
                    answer=", ".join(question.options[i].label for i in chosen),
                )
            )
        return UserQuestionResult(answers=answers)


class ScriptedAgents:
    """The slice of the reference's agent manager the tools read."""

    def __init__(self, profile: Any, agents: dict[str, Any], config: Any = None) -> None:
        self.active_profile = profile
        self.available_agents = agents
        self.config = config

    def switch_profile(self, target: str) -> None:
        self.active_profile = self.available_agents[target]

    def get_agent(self, name: str) -> Any:
        profile = self.available_agents.get(name)
        if profile is None:
            raise ValueError(name)
        return profile


class ScriptedRunner:
    """A ``SubagentRunnerPort`` that returns a declared run and calls no model."""

    def __init__(self, plan: dict[str, Any]) -> None:
        self.plan = plan

    async def run(self, args: Any, ctx: Any) -> Any:
        from vibe.core.subagents import TaskResult
        from vibe.core.types import ToolStreamEvent

        for message in self.plan.get("stream", []):
            yield ToolStreamEvent(
                tool_name="task", message=message, tool_call_id=ctx.tool_call_id
            )
        yield TaskResult(
            response=self.plan.get("response", ""),
            turns_used=self.plan.get("turns", 1),
            completed=self.plan.get("completed", True),
        )


def _skill_prompt(path: Path) -> str:
    """A fixture ``SKILL.md`` body, with the metadata block the loader eats."""

    text = path.read_text(encoding="utf-8")
    if not text.startswith("---\n"):
        return text
    _, _, rest = text[4:].partition("\n---\n")
    return rest


def scripted_skills(entries: list[dict[str, Any]], tree: Path) -> ScriptedSkills:
    from vibe.core.skills.models import SkillInfo

    skills: dict[str, Any] = {}
    for entry in entries:
        directory = entry.get("directory")
        if directory is None:
            skills[entry["name"]] = SkillInfo(
                name=entry["name"],
                description=entry["description"],
                prompt=entry["prompt"],
            )
            continue
        path = tree / directory / "SKILL.md"
        skills[entry["name"]] = SkillInfo(
            name=entry["name"],
            description=entry["description"],
            prompt=_skill_prompt(path),
            skill_path=path,
        )
    return ScriptedSkills(skills)


def scripted_agents(script: dict[str, Any], config: Any = None) -> ScriptedAgents:
    from vibe.core.agents.models import BUILTIN_AGENTS

    return ScriptedAgents(BUILTIN_AGENTS[script["agent"]], BUILTIN_AGENTS, config)


def loopback_config(authority: str, variable: str) -> Any:
    """A configuration whose only Mistral provider is the loopback server."""

    from vibe.core.config import VibeConfigSchema
    from vibe.core.config.models import Backend, ProviderConfig

    return VibeConfigSchema(
        providers=[
            ProviderConfig(
                name="loopback",
                api_base=f"http://{authority}/v1",
                api_key_env_var=variable,
                backend=Backend.MISTRAL,
            )
        ]
    )


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
        *_context_cases(),
        *_network_cases(),
    ]


#: The fixture skills the committed tree carries, described here rather than
#: read out of a manifest so the corpus states what the capture offered.
_SMALL_SKILL = {
    "name": "small",
    "directory": "skills/small",
    "description": "A fixture skill carrying one extra file.",
}
_LARGE_SKILL = {
    "name": "large",
    "directory": "skills/large",
    "description": "A fixture skill carrying more files than the listing cap.",
}
_DETACHED_SKILL = {
    "name": "detached",
    "description": "A fixture skill with no directory on disk.",
    "prompt": "Fixture instructions with no base directory.",
}
_BOTH_SKILLS = [_SMALL_SKILL, _LARGE_SKILL]


def _context_cases() -> list[dict[str, Any]]:
    """The four tools that answer from a collaborator rather than from disk.

    Each one is driven from a scripted collaborator: a skill manager holding
    fixture skills, a user who answers by option index, an agent manager on a
    named builtin profile, and a subagent runner that returns a declared run.
    No model is called and no terminal is attached.
    """

    return [
        # -- skill -------------------------------------------------------
        {
            "tool": "skill",
            "case": "fewer-files-than-the-cap",
            "args": {"name": "small"},
            "script": {"skills": _BOTH_SKILLS},
        },
        {
            "tool": "skill",
            "case": "more-files-than-the-cap",
            "args": {"name": "large"},
            "script": {"skills": _BOTH_SKILLS},
        },
        {
            "tool": "skill",
            "case": "already-loaded-earlier",
            "args": {"name": "small"},
            "script": {"skills": _BOTH_SKILLS, "loaded": ["small"]},
        },
        {
            "tool": "skill",
            "case": "no-directory-on-disk",
            "args": {"name": "detached"},
            "script": {"skills": [_DETACHED_SKILL]},
        },
        {
            "tool": "skill",
            "case": "unknown-name",
            "args": {"name": "absent"},
            "script": {"skills": _BOTH_SKILLS},
        },
        {
            "tool": "skill",
            "case": "unknown-name-with-none-available",
            "args": {"name": "absent"},
            "script": {"skills": []},
        },
        {"tool": "skill", "case": "no-skill-manager", "args": {"name": "small"}, "script": {}},
        # -- ask_user_question -------------------------------------------
        # The argument keys are the published ones. `vibe/questions.py`
        # configures `alias_generator=to_camel` with `populate_by_name`, so
        # the reference accepts both spellings but only the camel one
        # appears in the schema a model reads. Driving the snake spelling
        # would measure an argument no caller can send.
        {
            "tool": "ask_user_question",
            "case": "single-select",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"answers": [{"option": 0}]},
        },
        {
            "tool": "ask_user_question",
            "case": "multi-select",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture parts?",
                        "header": "Parts",
                        "multiSelect": True,
                        "options": [
                            {"label": "First"},
                            {"label": "Second"},
                            {"label": "Third"},
                        ],
                    }
                ]
            },
            "script": {"answers": [{"options": [0, 2]}]},
        },
        {
            "tool": "ask_user_question",
            "case": "other-free-text",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"answers": [{"other": "A typed fixture answer."}]},
        },
        {
            "tool": "ask_user_question",
            "case": "cancelled",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"cancelled": True},
        },
        {
            "tool": "ask_user_question",
            "case": "two-questions",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    },
                    {
                        "question": "Which fixture depth?",
                        "header": "Depth",
                        "options": [{"label": "Shallow"}, {"label": "Deep"}],
                    },
                ],
                "footerNote": "A fixture footer.",
            },
            "script": {"answers": [{"option": 1}, {"option": 0}]},
        },
        {
            "tool": "ask_user_question",
            "case": "other-hidden",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "hideOther": True,
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"answers": [{"option": 1}]},
        },
        {
            "tool": "ask_user_question",
            "case": "options-with-descriptions",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [
                            {"label": "First", "description": "The first fixture branch."},
                            {"label": "Second", "description": "The second fixture branch."},
                        ],
                    }
                ]
            },
            "script": {"answers": [{"option": 0}]},
        },
        {
            "tool": "ask_user_question",
            "case": "no-interaction-source",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {},
        },
        # -- exit_plan_mode ----------------------------------------------
        {
            "tool": "exit_plan_mode",
            "case": "clear-context-and-auto-approve",
            "args": {},
            "script": {
                "agent": "plan",
                "planFile": "plan.md",
                "clearContext": True,
                "answers": [{"option": 0}],
            },
        },
        {
            "tool": "exit_plan_mode",
            "case": "clear-context-without-a-callback",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 0}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "auto-approve",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 1}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "manual-approval",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 2}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "declined",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 3}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "other-feedback",
            "args": {},
            "script": {
                "agent": "plan",
                "planFile": "plan.md",
                "answers": [{"other": "A typed fixture objection."}],
            },
        },
        {
            "tool": "exit_plan_mode",
            "case": "cancelled",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "cancelled": True},
        },
        {
            "tool": "exit_plan_mode",
            "case": "no-plan-file",
            "args": {},
            "script": {"agent": "plan", "answers": [{"option": 1}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "not-in-plan-mode",
            "args": {},
            "script": {"agent": "default", "answers": [{"option": 0}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "no-interaction-source",
            "args": {},
            "script": {"agent": "plan"},
        },
        {"tool": "exit_plan_mode", "case": "no-agent-manager", "args": {}, "script": {}},
        # -- task --------------------------------------------------------
        {
            "tool": "task",
            "case": "completed-in-one-turn",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "default",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "completed-in-several-turns",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "default",
                "runner": {"response": "A fixture finding.", "turns": 3, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "ended-incomplete",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "default",
                "runner": {"response": "A partial finding.", "turns": 2, "completed": False},
            },
        },
        {
            "tool": "task",
            "case": "empty-response",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "default",
                "runner": {"response": "", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "streamed-progress",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "default",
                "runner": {
                    "response": "A fixture finding.",
                    "turns": 2,
                    "completed": True,
                    "stream": ["read_file: a fixture progress line"],
                },
            },
        },
        {
            "tool": "task",
            "case": "default-agent-name",
            "args": {"task": "Inspect the fixture tree."},
            "script": {
                "agent": "default",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "unknown-agent",
            "args": {"task": "Inspect the fixture tree.", "agent": "absent"},
            "script": {
                "agent": "default",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "not-a-subagent",
            "args": {"task": "Inspect the fixture tree.", "agent": "plan"},
            "script": {
                "agent": "default",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "no-subagent-runner",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {"agent": "default"},
        },
        {
            "tool": "task",
            "case": "already-inside-a-subagent",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "explore",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {"tool": "task", "case": "no-agent-manager", "args": {"task": "Inspect."}, "script": {}},
    ]


#: The one response body the oversized case needs, declared rather than spelled
#: out: 3_200 repetitions of a 40-byte line exceed the 120_000-byte cap.
_OVERSIZED_BODY = {"unit": "A fixture line of exactly forty bytes.\n\n", "count": 3200}

#: One search response, parameterized by the entry content the case exercises.
def _conversation(content: Any, entries: int = 1) -> dict[str, Any]:
    return {
        "conversation_id": "conversation-1",
        "object": "conversation.response",
        "outputs": [
            {
                "object": "entry",
                "type": "message.output",
                "role": "assistant",
                "content": content,
            }
            for _ in range(entries)
        ],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


def _citation(title: str, url: str | None) -> dict[str, Any]:
    chunk: dict[str, Any] = {"type": "tool_reference", "tool": "web_search", "title": title}
    if url is not None:
        chunk["url"] = url
    return chunk


def _network_cases() -> list[dict[str, Any]]:
    """The two tools that speak HTTP, driven against the loopback server.

    Every URL is written with ``{server}`` and the capture substitutes the
    ephemeral authority in, so the corpus carries no port number and a re-run on
    another port is byte-identical. The socket guard refuses every other
    destination, so nothing here can silently become a live request.
    """

    return [
        # -- web_fetch ---------------------------------------------------
        {
            "tool": "web_fetch",
            "case": "an-html-page",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.html"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/html; charset=utf-8"},
                        "body": "<html><body><h1>Fixture</h1><p>A fixture paragraph.</p></body></html>",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-plain-text-page",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "A fixture paragraph.",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-json-body",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.json"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": {"fixture": True, "count": 2},
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "larger-than-max-content-bytes",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/large.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "bodyRepeat": _OVERSIZED_BODY,
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-redirect-chain",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/first"},
            "script": {
                "responses": [
                    {"status": 302, "reason": "Found", "headers": {"Location": "/second"}},
                    {"status": 302, "reason": "Found", "headers": {"Location": "/third"}},
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "The fixture destination.",
                    },
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "not-found",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/absent"},
            "script": {
                "responses": [
                    {
                        "status": 404,
                        "reason": "Not Found",
                        "headers": {"Content-Type": "text/plain"},
                        "body": "absent",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-challenge-then-success",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/guarded"},
            "script": {
                "responses": [
                    {
                        "status": 403,
                        "reason": "Forbidden",
                        "headers": {"cf-mitigated": "challenge"},
                        "body": "",
                    },
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "The fixture behind the challenge.",
                    },
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-challenge-that-persists",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/guarded"},
            "script": {
                "responses": [
                    {
                        "status": 403,
                        "reason": "Forbidden",
                        "headers": {"cf-mitigated": "challenge"},
                        "body": "",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "forbidden-without-the-challenge-header",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/guarded"},
            "script": {
                "responses": [{"status": 403, "reason": "Forbidden", "body": ""}]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-server-error",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/broken"},
            "script": {
                "responses": [
                    {"status": 500, "reason": "Internal Server Error", "body": ""}
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "an-empty-body",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/empty.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "no-content-type",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/bare"},
            "script": {"responses": [{"status": 200, "body": "A bare fixture body."}]},
        },
        {
            "tool": "web_fetch",
            "case": "a-url-with-no-scheme",
            "args": {"url": f"{SERVER_PLACEHOLDER}/page.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "A fixture paragraph.",
                    }
                ]
            },
        },
        {"tool": "web_fetch", "case": "an-empty-url", "args": {"url": ""}, "script": {}},
        {
            "tool": "web_fetch",
            "case": "an-unsupported-scheme",
            "args": {"url": "ftp://fixture.invalid/page.txt"},
            "script": {},
        },
        {
            "tool": "web_fetch",
            "case": "a-timeout-above-the-cap",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.txt", "timeout": 300},
            "script": {},
        },
        {
            "tool": "web_fetch",
            "case": "a-non-positive-timeout",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.txt", "timeout": 0},
            "script": {},
        },
        # -- web_search --------------------------------------------------
        {
            "tool": "web_search",
            "case": "a-string-content-answer",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation("A fixture answer."),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "chunked-content-with-citations",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation(
                            [
                                {"type": "text", "text": "A fixture answer."},
                                _citation("The first source", "https://fixture.invalid/one"),
                                _citation("The second source", "https://fixture.invalid/two"),
                            ]
                        ),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "duplicate-citation-urls",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation(
                            [
                                {"type": "text", "text": "A fixture answer."},
                                _citation("The first source", "https://fixture.invalid/one"),
                                _citation("The same source again", "https://fixture.invalid/one"),
                            ]
                        ),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "a-citation-with-no-url",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation(
                            [
                                {"type": "text", "text": "A fixture answer."},
                                _citation("The unlinked source", None),
                            ]
                        ),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "several-message-outputs",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation("A fixture answer.", entries=2),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "no-text-in-the-response",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation([]),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "a-non-2xx-status",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 500,
                        "reason": "Internal Server Error",
                        "headers": {"Content-Type": "application/json"},
                        "json": {"message": "a fixture failure"},
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "no-api-key",
            "args": {"query": "the fixture question"},
            "script": {"apiKeyVariable": "VIBE_PARITY_ABSENT_KEY", "responses": []},
        },
    ]


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------


def tool_classes() -> dict[str, Any]:
    """The reference tool classes this oracle drives, by published name."""

    from vibe.core.tools.builtins.ask_user_question import AskUserQuestion
    from vibe.core.tools.builtins.edit import Edit
    from vibe.core.tools.builtins.exit_plan_mode import ExitPlanMode
    from vibe.core.tools.builtins.grep import Grep
    from vibe.core.tools.builtins.read_file import ReadFile
    from vibe.core.tools.builtins.skill import Skill
    from vibe.core.tools.builtins.task import Task
    from vibe.core.tools.builtins.todo import Todo
    from vibe.core.tools.builtins.web_fetch import WebFetch
    from vibe.core.tools.builtins.web_search import WebSearch
    from vibe.core.tools.builtins.write_file import WriteFile

    return {
        cls.get_name(): cls
        for cls in (
            ReadFile,
            Grep,
            WriteFile,
            Edit,
            Todo,
            Skill,
            AskUserQuestion,
            ExitPlanMode,
            Task,
            WebFetch,
            WebSearch,
        )
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


def invoke_context(
    case: dict[str, Any], tree: Path, scratchpad: Path, server: LoopbackServer | None
) -> Any:
    """The context this case's tool reads, built from the case's own script.

    Every collaborator here is scripted: nothing reaches a terminal, a model or
    a network destination other than the loopback server the capture started.
    """

    from vibe.core.tools.base import InvokeContext

    script = case.get("script", {})
    fields: dict[str, Any] = {"tool_call_id": "call-1", "scratchpad_dir": scratchpad}

    if "skills" in script:
        loaded = set(script.get("loaded", []))
        fields["skill_manager"] = scripted_skills(script["skills"], tree)
        fields["is_skill_loaded"] = loaded.__contains__
    if "answers" in script or "cancelled" in script:
        fields["interaction_requests"] = ScriptedAnswers(script)
    if "planFile" in script:
        fields["plan_file_path"] = tree / script["planFile"]
    if script.get("clearContext"):

        async def cleared() -> None:
            return None

        fields["request_clear_context_callback"] = cleared
    if "runner" in script:
        fields["subagent_runner"] = ScriptedRunner(script["runner"])
    if "agent" in script:
        fields["agent_manager"] = scripted_agents(script)
    if case["tool"] == "web_search" and server is not None:
        variable = script.get("apiKeyVariable", SEARCH_KEY_VARIABLE)
        fields["agent_manager"] = ScriptedAgents(
            None, {}, loopback_config(server.authority, variable)
        )

    return InvokeContext(**fields)


def case_arguments(case: dict[str, Any], tree: Path, server: LoopbackServer | None) -> dict[str, Any]:
    """The case's arguments with the two host-dependent forms substituted in.

    A path argument is written relative to the tree so the case list stays
    host-independent, and a URL is written with ``{server}`` so no ephemeral
    port ever reaches it. The reference wants the resolved form of both.
    """

    arguments = dict(case["args"])
    for key in ("file_path", "path"):
        if isinstance(arguments.get(key), str) and arguments[key]:
            arguments[key] = str(tree / arguments[key])
    if isinstance(arguments.get("url"), str) and server is not None:
        arguments["url"] = arguments["url"].replace(SERVER_PLACEHOLDER, server.authority)
    return arguments


async def run_case(
    case: dict[str, Any], tree: Path, scratchpad: Path, server: LoopbackServer | None = None
) -> dict[str, Any]:
    """Drives one case and records what an agent loop would have observed."""

    from vibe.core.tools.base import ToolError
    from vibe.core.types import ToolStreamEvent

    classes = tool_classes()
    cls = classes[case["tool"]]
    tool = build_tool(cls, tree, scratchpad)
    arguments = case_arguments(case, tree, server)
    context = invoke_context(case, tree, scratchpad, server)

    record: dict[str, Any] = {
        "tool": case["tool"],
        "case": case["case"],
        "arguments": case["args"],
    }
    if "script" in case:
        record["script"] = case["script"]
    outcome: dict[str, Any] = {}
    try:
        result_model = None
        async for item in tool.invoke(context, **arguments):
            if not isinstance(item, ToolStreamEvent):
                result_model = item
        if result_model is None:
            raise ToolError("Tool did not yield a result")
    except Exception as error:  # noqa: BLE001 - the outcome is the measurement
        # A raise records the type and whether a message came with it. The text
        # itself stays out of the committed projection, which `project_case`
        # enforces, so no reference sentence ships.
        outcome["outcome"] = "raised"
        outcome["error"] = {"type": type(error).__name__, "message": str(error)}
    else:
        typed = stabilize(case["tool"], result_model.model_dump(mode="json"))
        # Exactly what `_loop.py` sends to the model: the field-per-line
        # rendering plus whatever the tool appends through `get_result_extra`.
        text = "\n".join(f"{key}: {value}" for key, value in typed.items())
        extra = tool.get_result_extra(result_model)
        if extra:
            text += "\n\n" + extra
        outcome["outcome"] = "returned"
        outcome["typedResult"] = typed
        outcome["modelText"] = text
        outcome["projectedResult"] = projected_result(cls, result_model, typed, case["tool"])

    if server is not None and case["tool"] == "web_fetch":
        # Only `web_fetch` builds its own request; `web_search` hands the call
        # to the vendored SDK, whose header order measures that dependency
        # rather than the reference, so recording it would compare the wrong
        # thing. US-251 closes what this records.
        record["requests"] = server.requests
    record.update(outcome)
    return record


def projected_result(cls: Any, result_model: Any, typed: dict[str, Any], tool: str) -> Any:
    """The second published shape, or the typed result when there is no second.

    ``project_result`` returns ``None`` for nine of the eleven tools, and both
    app-server projection sites fall back to the raw result when it does
    (``vibe/app_server/_tool_projection.py`` and ``vibe/app_server/_projection.py``),
    so the effective projection of those nine *is* the typed result. Recording
    that fallback here is what lets the replay compare one field for all eleven.
    """

    projected = cls.project_result(result_model)
    if projected is None:
        return typed
    if tool != "grep":
        return projected
    stabilized = dict(projected)
    stabilized["matches"] = "\n".join(
        sorted(projected["matches"].splitlines(), key=_match_order)
    )
    stabilized["parsed_matches"] = sorted(
        projected["parsed_matches"], key=lambda match: (match["path"], match["line"] or 0)
    )
    return stabilized


def _match_order(line: str) -> tuple[str, int]:
    path, _, rest = line.partition(":")
    number, _, _ = rest.partition(":")
    return (path, int(number) if number.isdigit() else 0)


def stabilize(tool: str, typed: dict[str, Any]) -> dict[str, Any]:
    """Removes the one nondeterminism a captured result carries.

    ``rg`` walks in parallel and emits whichever file finished first, so the
    order of ``grep``'s match lines is not a contract anything can conform to:
    one capture recorded two different orders for the same query. Sorting by
    path and then by line is the normalization that makes the answer
    comparable, exactly as the tree root and the scratchpad are normalized
    below, and the Rust side sorts the same way. The match *set* is still
    compared byte for byte.
    """

    if tool != "grep":
        return typed
    stabilized = dict(typed)
    stabilized["matches"] = "\n".join(
        sorted(typed["matches"].splitlines(), key=_match_order)
    )
    return stabilized


async def capture(tree_source: Path) -> list[dict[str, Any]]:
    """Every case, each mutating one running against a freshly copied tree."""

    from vibe.core.config.harness_files import init_harness_files_manager

    # `VibeConfigSchema` validates its system prompt through the process-wide
    # harness manager, so the loopback provider cannot be built until it exists.
    init_harness_files_manager("project")
    os.environ[SEARCH_KEY_VARIABLE] = SEARCH_KEY_VALUE

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
            responses = case.get("script", {}).get("responses")
            # A server per network case, so one case's requests can never be
            # read as another's and the recorded exchange is exactly this one.
            server = LoopbackServer(responses) if responses is not None else None
            try:
                record = await run_case(case, tree.resolve(), scratchpad, server)
            finally:
                if server is not None:
                    server.close()
            authority = server.authority if server is not None else None
            records.append(normalize(record, tree.resolve(), scratchpad, authority))
    return records


# --------------------------------------------------------------------------
# Normalization and the committed projection
# --------------------------------------------------------------------------


def normalize(value: Any, tree: Path, scratchpad: Path, authority: str | None = None) -> Any:
    """Replaces host-specific values so the corpus replays on another machine.

    The tree root and the scratchpad are fresh temporary directories on every
    run, so a captured absolute path is meaningless anywhere else. Both collapse
    to a placeholder, and the separator is normalized so a Windows capture and a
    POSIX one produce the same corpus. The loopback authority collapses the same
    way, because the port is whatever the kernel handed out this run.
    """

    if isinstance(value, dict):
        return {
            key: normalize(item, tree, scratchpad, authority)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [normalize(item, tree, scratchpad, authority) for item in value]
    if isinstance(value, str):
        replaced = value.replace(str(tree), TREE_PLACEHOLDER)
        replaced = replaced.replace(str(scratchpad), "{scratchpad}")
        if authority is not None:
            replaced = replaced.replace(authority, SERVER_PLACEHOLDER)
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
    # A request target: a URL path and nothing else. No sentence reaches this
    # shape, and the alternative is a corpus that cannot say which path the
    # reference asked for.
    if _REQUEST_TARGET.fullmatch(value):
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

    An error *message* is recorded as its presence and its digest, never as its
    content. The digest is what makes a re-pin that reworded a message visible
    in this file's diff; it is deliberately not a conformance target, because
    the PRD lists byte-identical error text as a non-goal for the same licensing
    reason as tool descriptions: a message must name the same cause, value and
    limit, not reproduce the reference's wording. The full text stays in the
    gitignored artifact for a human to read.
    """

    authored: set[str] = {TREE_PLACEHOLDER, SERVER_PLACEHOLDER, "{scratchpad}"}
    literal_values(record.get("arguments"), authored)
    # The script is this repository's own text: the fixture bodies a loopback
    # response served, the skill prompts, the free-text answers. A result that
    # only reads one of them back is a value this corpus supplied, not a
    # reference sentence, so it survives as it stands.
    literal_values(record.get("script"), authored)
    projected = {
        key: (
            value
            if key in ("tool", "case", "outcome", "arguments", "script")
            else project(value, authored)
        )
        for key, value in record.items()
    }
    if isinstance(projected.get("error"), dict):
        message = record["error"].get("message") or ""
        projected["error"]["message"] = {
            "present": bool(message),
            **({"digest": digest(message)} if message else {}),
        }
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
    # tool here resolves is the fixture one this script exports.
    os.environ.setdefault("VIBE_TEST_DISABLE_KEYRING", "1")
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        GUARD.install()
        records = asyncio.run(capture(arguments.tree))
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
