#!/usr/bin/env python3
"""Black-box capture of the rewind methods served by the app server.

Every scenario starts an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in ``acp.py`` already runs, drives real turns
through it so the session holds real history and real file checkpoints, and
then calls ``session/rewind/read`` and ``session/rewind``. What is recorded is
what row 9 of ``docs/parity.md`` is about: each rewind answer, the notifications
it raised, the files on disk afterward, and the conversation the model is sent
on the next turn. Nothing is imported from the reference: the reference's own
``vibe-app-server`` is the oracle, which is what makes the same scenarios
replayable against this port by
``crates/vibe-app-server/tests/rewind_parity_tests.rs``.

Opening a session and starting a turn are the setup every scenario needs, not
what it measures: their wire is row 17's contract. The two are written here as
abstract steps that the driver renders in the dialect of the server it drives,
and their answers are never recorded.

Normalization maps identifiers, paths and times to placeholders and reduces
every string the server authored to its length and SHA-256, which is what
``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/rewind.py                  # capture the reference
    python3 scripts/parity/rewind.py --check          # recapture and compare
    python3 scripts/parity/rewind.py --server target/debug/vibe-app-server-stdio-fixture \\
        --dialect port --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import json
import os
from pathlib import Path
import queue
import re
import shutil
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/rewind-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5
TURN_TIMEOUT = 30.0

#: Keys whose string values are identities the server minted.
ID_KEYS = {
    "id",
    "sessionId",
    "rootSessionId",
    "parentSessionId",
    "turnId",
    "entryId",
    "relatedEntryId",
    "activeTurnId",
    "toolCallId",
    "callbackId",
    "queueItemId",
}
#: Keys whose values are wall-clock readings.
TIME_KEYS = {"createdAt", "updatedAt", "bumpedAt", "pinnedAt", "emittedAt", "startedAt", "completedAt"}
#: Keys whose values count events, which depends on every notification a server
#: raised before the call; whether one is zero is what a rewind decides.
COUNTER_KEYS = {"eventId", "lastEventId"}
#: Keys whose numeric values measure elapsed time or throughput.
DURATION_KEYS = {"durationMs"}

UUID = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)


class OracleError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# The chat-completions stand-in, recording what each request carried
# --------------------------------------------------------------------------


class RecordingBackend(acp.Backend):
    """The ACP stand-in, keeping the conversation of every request it served."""

    def __init__(self) -> None:
        super().__init__()
        self.conversations: list[list[dict[str, Any]]] = []
        self.bodies: list[Any] = []
        handler = self.server.RequestHandlerClass
        backend = self
        original = handler.do_POST

        def do_post(self: Any) -> None:
            length = int(self.headers.get("content-length") or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {}
            with backend.lock:
                backend.conversations.append(conversation(body.get("messages", [])))
                backend.bodies.append(body)
            # The parent handler reads the body again, so it is handed back.
            self.headers.replace_header("content-length", str(len(raw)))
            stream = self.rfile
            self.rfile = _Replay(raw, stream)
            try:
                original(self)
            finally:
                # The connection is kept alive, so the next request is read
                # from the socket again.
                self.rfile = stream

        handler.do_POST = do_post


class _Replay:
    def __init__(self, raw: bytes, rest: Any) -> None:
        self.raw = raw
        self.rest = rest

    def read(self, size: int = -1) -> bytes:
        if size < 0 or size >= len(self.raw):
            data, self.raw = self.raw, b""
            return data
        data, self.raw = self.raw[:size], self.raw[size:]
        return data

    def __getattr__(self, name: str) -> Any:
        return getattr(self.rest, name)


def conversation(messages: list[Any]) -> list[dict[str, Any]]:
    """The turns a request sends, without the prose each server authors.

    The system prompt and every tool result are the server's own text, so only
    their roles are kept; what the user and the assistant said is the
    scenario's, and is kept verbatim.
    """

    turns: list[dict[str, Any]] = []
    for message in messages:
        if not isinstance(message, dict):
            continue
        role = message.get("role")
        if role == "system":
            continue
        content = message.get("content")
        if isinstance(content, list):
            content = "".join(
                part.get("text", "") for part in content if isinstance(part, dict)
            )
        entry: dict[str, Any] = {"role": role}
        if role in {"user", "assistant"}:
            entry["content"] = content or ""
        if message.get("tool_calls"):
            entry["toolCalls"] = [
                call.get("function", {}).get("name") for call in message["tool_calls"]
            ]
        turns.append(entry)
    return turns


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(acp.Agent):
    """One app server process and the client side of its connection."""

    def wait_for(self, predicate: Any, timeout: float = TURN_TIMEOUT) -> list[dict[str, Any]]:
        """Everything the server writes until one message satisfies `predicate`."""

        messages: list[dict[str, Any]] = []
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise OracleError("the server never settled the turn")
            try:
                message = self.inbox.get(timeout=remaining)
            except queue.Empty:
                raise OracleError("the server never settled the turn") from None
            if message is None:
                raise OracleError("the server exited during a turn")
            messages.append(message)
            if predicate(message):
                return messages


class Session:
    """What a scenario learned: sessions, and the entries each turn added."""

    def __init__(self, root: Path) -> None:
        self.world = acp.World(root)
        self.sessions: list[str] = []
        self.users: list[str] = []
        self.assistants: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        result = message.get("result")
        if isinstance(result, dict):
            state = result.get("state")
            if isinstance(state, dict):
                session = state.get("session")
                if isinstance(session, dict) and isinstance(session.get("id"), str):
                    if session["id"] not in self.sessions:
                        self.sessions.append(session["id"])
        if message.get("method") == "history/entryAdded":
            entry = (message.get("params") or {}).get("entry") or {}
            if entry.get("type") == "message":
                table = self.users if entry.get("role") == "user" else self.assistants
                if entry.get("id") not in table:
                    table.append(entry["id"])

    def substitute(self, value: Any) -> Any:
        if isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.substitute(item) for item in value]
        if not isinstance(value, str):
            return value
        text = value.replace("$WS", str(self.world.workspace))
        for prefix, table in (("$S", self.sessions), ("$U", self.users), ("$A", self.assistants)):
            for index in range(len(table), 0, -1):
                text = text.replace(f"{prefix}{index}", table[index - 1])
        return text


def render_start(dialect: str, workspace: Path, agent: str) -> dict[str, Any]:
    if dialect == "reference":
        return {"agentConfig": {"cwd": str(workspace), "agent": agent}}
    return {"cwd": str(workspace), "agent": agent}


def render_turn(dialect: str, session_id: str, text: str) -> dict[str, Any]:
    # Both servers read `TurnStartParams.message` (vibe/app_server/protocol.py).
    del dialect
    return {"sessionId": session_id, "message": [{"type": "text", "text": text}]}


def settles(session_id_holder: dict[str, str]) -> Any:
    def predicate(message: dict[str, Any]) -> bool:
        return message.get("method") in {"turn/completed", "turn/failed", "turn/interrupted"}

    return predicate


def base_config(backend: acp.Backend, extra: str) -> str:
    return acp.base_config(backend) + extra


def run_scenario(
    scenario: dict[str, Any], command: list[str], dialect: str, quiet: float
) -> dict[str, Any]:
    backend = RecordingBackend()
    root = Path(tempfile.mkdtemp(prefix="vibe-rewind-oracle-"))
    session = Session(root)
    world = session.world
    try:
        backend.responses = copy.deepcopy(scenario.get("backend", []))
        (world.vibe_home / "config.toml").write_text(
            base_config(backend, scenario.get("config", "")), encoding="utf-8"
        )
        acp.write_tree(world.workspace, scenario.get("files", {}))
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(world.home),
            "VIBE_HOME": str(world.vibe_home),
            "MISTRAL_API_KEY": "oracle-key",
            "VIBE_API_BASE": f"{backend.base}/v1/chat/completions",
            "LANG": "C.UTF-8",
            "TERM": "dumb",
            "NO_COLOR": "1",
            "CI": "true",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
        }
        server = Server(command, env, world.workspace, {}, world)
        steps: list[dict[str, Any]] = []
        try:
            run_steps(scenario, server, session, backend, steps, dialect, quiet)
        except OracleError as error:
            # A step that never settles is an observation too: the rest of the
            # scenario cannot run, but the others still can.
            steps.append({"failure": str(error)})
        finally:
            server.stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
            "conversations": backend.conversations,
            "bodies": backend.bodies,
            "paths": {
                "workspace": str(world.workspace),
                "vibeHome": str(world.vibe_home),
                "home": str(world.home),
                "root": str(root),
                "bin": str(Path(command[0]).parent),
                "backend": backend.base,
            },
        }
    finally:
        backend.close()
        shutil.rmtree(root, ignore_errors=True)


def run_steps(
    scenario: dict[str, Any],
    server: Server,
    session: Session,
    backend: RecordingBackend,
    steps: list[dict[str, Any]],
    dialect: str,
    quiet: float,
) -> None:
    world = session.world
    identifier = 1
    server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                 "params": {"clientInfo": {"name": "rewind-oracle", "version": "0"}}})
    server.collect(identifier, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    for step in scenario["steps"]:
        identifier += 1
        if "start" in step:
            params = render_start(dialect, world.workspace, step["start"].get("agent", "auto-approve"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start", "params": params})
            for message in server.collect(identifier, quiet):
                session.learn(message)
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": render_turn(dialect, target, step["turn"])})
            if step.get("background"):
                # The turn is left running; its answer arrives later.
                # What it announced so far is drained, so the next
                # observation holds only what that call raised.
                time.sleep(step.get("settle", 0.5))
                for message in server.collect(None, quiet):
                    session.learn(message)
                continue
            for message in server.wait_for(settles({})):
                session.learn(message)
            for message in server.collect(None, quiet):
                session.learn(message)
            continue
        if "await" in step:
            for message in server.wait_for(settles({})):
                session.learn(message)
            for message in server.collect(None, quiet):
                session.learn(message)
            continue
        if "write" in step:
            acp.write_tree(world.workspace, step["write"])
            continue
        if "files" in step:
            steps.append({"files": {
                name: (world.workspace / name).read_text(encoding="utf-8")
                if (world.workspace / name).is_file() else None
                for name in step["files"]
            }})
            continue
        if "context" in step:
            # The conversation the most recent model request carried.
            with backend.lock:
                latest = backend.conversations[-1] if backend.conversations else []
            steps.append({"context": latest})
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            session.learn(message)
        response = next(
            (m for m in observed if "method" not in m and m.get("id") == identifier), None
        )
        notifications = [m.get("method") for m in observed if "method" in m and "id" not in m]
        steps.append({
            "method": request["method"],
            "response": {k: v for k, v in (response or {}).items() if k != "jsonrpc"},
            "notifications": notifications,
        })


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


class Normalizer(acp.Normalizer):
    def value(self, value: Any, key: str | None = None) -> Any:
        if key in COUNTER_KEYS and isinstance(value, int):
            return "<nonzero>" if value else 0
        if key in DURATION_KEYS and isinstance(value, int | float):
            return "<duration>"
        if key in TIME_KEYS and value is not None:
            return "<time>"
        if key in ID_KEYS and isinstance(value, str) and value:
            return self.identity(value)
        if isinstance(value, str) and key not in ID_KEYS:
            value = UUID.sub(lambda match: self.identity(match.group(0)), value)
        return super().value(value, key)

    def collect_ids(self, value: Any, key: str | None = None) -> None:
        # Keys are walked sorted: an object's key order is not part of the
        # contract, and numbering by it would make two equal states differ.
        if isinstance(value, dict):
            for name, item in sorted(value.items()):
                self.collect_ids(item, name)
        elif isinstance(value, list):
            for item in value:
                self.collect_ids(item)
        elif isinstance(value, str) and value:
            if key in ID_KEYS:
                self.identity(value)
            else:
                for match in UUID.finditer(value):
                    self.identity(match.group(0))


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[Any]:
    normalizer = Normalizer(scenario, run["paths"])
    # Identities are numbered in the order the scenario learned them, so the
    # same session or entry carries the same placeholder on both sides.
    for identity in run["sessions"]:
        normalizer.identity(identity)
    for step in run["steps"]:
        normalizer.collect_ids(step.get("response"))
    observed: list[Any] = []
    for step in run["steps"]:
        if "files" in step:
            observed.append(step)
            continue
        if "failure" in step:
            observed.append({"failure": normalizer.text(step["failure"])})
            continue
        if "context" in step:
            # A message the server wrote into the conversation, such as a
            # compaction envelope, is its own prose.
            observed.append({"context": normalizer.value(copy.deepcopy(step["context"]))})
            continue
        observed.append({
            "method": step["method"],
            "response": normalizer.value(copy.deepcopy(step["response"])),
            "notifications": step["notifications"],
        })
    return observed


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def rewind_read(entry: str, session: str = "$S1") -> dict[str, Any]:
    return {"send": {"method": "session/rewind/read", "params": {"sessionId": session, "entryId": entry}}}


def rewind(entry: str, session: str = "$S1", **options: Any) -> dict[str, Any]:
    return {"send": {"method": "session/rewind", "params": {"sessionId": session, "entryId": entry, **options}}}


#: Two turns, each changing files through tools, and a third that only talks.
EDITING_BACKEND = [
    {"toolCalls": [
        call("write_file", {"file_path": "created.txt", "content": "made in turn one\n"}, "call_w1"),
        call("edit", {"file_path": "notes.txt", "old_string": "original", "new_string": "first edit"}, "call_e1"),
    ]},
    {"text": "Turn one done."},
    {"toolCalls": [
        call("edit", {"file_path": "notes.txt", "old_string": "first edit", "new_string": "second edit"}, "call_e2"),
    ]},
    {"text": "Turn two done."},
    {"text": "Turn three only talks."},
]
EDITING_FILES = {"notes.txt": "original\n"}
EDITING_STEPS = [
    {"start": {}},
    {"turn": "Create a file and edit the notes"},
    {"turn": "Edit the notes again"},
    {"turn": "Just answer"},
]
WATCHED = ["notes.txt", "created.txt"]


def scenarios() -> list[dict[str, Any]]:
    return [
        {
            "name": "read/each-user-entry",
            "backend": EDITING_BACKEND,
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind_read("$U1"),
                rewind_read("$U2"),
                rewind_read("$U3"),
            ],
        },
        {
            "name": "read/refusals",
            "backend": EDITING_BACKEND,
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind_read("$A1"),
                rewind_read("unknown-entry"),
                rewind_read("history:1:user"),
                rewind_read("$U1", session="unknown-session"),
            ],
        },
        {
            "name": "rewind/fork-restoring-files",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U2", restoreFiles=True),
                {"files": WATCHED},
                {"turn": "Continue from the fork", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/fork-keeping-files",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U2"),
                {"files": WATCHED},
                {"turn": "Continue from the fork", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/inplace-restoring-files",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U2", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "Continue in place"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/inplace-to-the-first-message",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U1", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "Start over"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/fork-to-the-first-message",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U1", restoreFiles=True),
                {"files": WATCHED},
                {"turn": "Start over", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/refusals",
            "backend": EDITING_BACKEND,
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$A1"),
                rewind("unknown-entry", restoreFiles=True),
                rewind("history:1:user"),
                rewind("$U1", session="unknown-session"),
                {"send": {"method": "session/rewind", "params": {"sessionId": "$S1"}}},
                {"send": {"method": "session/rewind", "params": {
                    "sessionId": "$S1", "entryId": "$U1", "restoreFiles": "yes"}}},
                {"send": {"method": "session/rewind", "params": {
                    "sessionId": "$S1", "entryId": "$U1", "extra": True}}},
                {"files": WATCHED},
            ],
        },
        {
            "name": "rewind/while-a-turn-runs",
            "backend": [
                {"text": "First answer."},
                {"text": "Slow answer.", "delay": 3.0},
            ],
            "steps": [
                {"start": {}},
                {"turn": "First question"},
                {"turn": "Slow question", "background": True, "settle": 1.0},
                rewind_read("$U1"),
                rewind("$U1"),
                {"await": True},
            ],
        },
        {
            "name": "rewind/twice",
            "backend": [*EDITING_BACKEND, {"text": "After the second rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U3", restoreFiles=True),
                rewind_read("$U1", session="$S2"),
                rewind("$U1", session="$S2", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "After two rewinds", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            # Upstream compaction appends a boundary and keeps every earlier
            # message and the checkpoint log, so a rewind can reach a turn the
            # model no longer sees. The first request reports enough context to
            # cross the threshold, so the compaction runs before the second.
            "name": "rewind/across-a-compaction",
            "config": (
                "\n[[models]]\n"
                'name = "mistral-vibe-cli-latest"\n'
                'provider = "mistral"\n'
                'alias = "mistral-medium-3.5"\n'
                "auto_compact_threshold = 16\n"
            ),
            "backend": [
                {"toolCalls": [
                    call("write_file", {"file_path": "created.txt", "content": "made in turn one\n"}, "call_w1"),
                ], "promptTokens": 12, "completionTokens": 8},
                {"text": "<summary>The user created a file.</summary>", "promptTokens": 1, "completionTokens": 1},
                {"text": "Turn one done.", "promptTokens": 2, "completionTokens": 1},
                {"toolCalls": [
                    call("edit", {"file_path": "notes.txt", "old_string": "original", "new_string": "after compaction"}, "call_e1"),
                ], "promptTokens": 2, "completionTokens": 1},
                {"text": "Turn two done.", "promptTokens": 2, "completionTokens": 1},
                {"text": "After the rewind.", "promptTokens": 2, "completionTokens": 1},
            ],
            "files": EDITING_FILES,
            "steps": [
                {"start": {}},
                {"turn": "Create a file"},
                {"turn": "Edit the notes"},
                {"context": True},
                rewind_read("$U1"),
                rewind_read("$U2"),
                rewind("$U1", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "Start again"},
                {"context": True},
            ],
        },
        {
            # A rewind to a turn after the boundary keeps the boundary: the
            # forked conversation still starts from the summary.
            "name": "rewind/after-a-compaction",
            "config": (
                "\n[[models]]\n"
                'name = "mistral-vibe-cli-latest"\n'
                'provider = "mistral"\n'
                'alias = "mistral-medium-3.5"\n'
                "auto_compact_threshold = 16\n"
            ),
            "backend": [
                {"toolCalls": [
                    call("write_file", {"file_path": "created.txt", "content": "made in turn one\n"}, "call_w1"),
                ], "promptTokens": 12, "completionTokens": 8},
                {"text": "<summary>The user created a file.</summary>", "promptTokens": 1, "completionTokens": 1},
                {"text": "Turn one done.", "promptTokens": 2, "completionTokens": 1},
                {"toolCalls": [
                    call("edit", {"file_path": "notes.txt", "old_string": "original", "new_string": "after compaction"}, "call_e1"),
                ], "promptTokens": 2, "completionTokens": 1},
                {"text": "Turn two done.", "promptTokens": 2, "completionTokens": 1},
                {"text": "After the rewind.", "promptTokens": 2, "completionTokens": 1},
            ],
            "files": EDITING_FILES,
            "steps": [
                {"start": {}},
                {"turn": "Create a file"},
                {"turn": "Edit the notes"},
                {"context": True},
                rewind("$U2", restoreFiles=True),
                {"files": WATCHED},
                {"turn": "Try the edit again", "session": "$S2"},
                {"context": True},
            ],
        },
    ]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--server", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--dialect", choices=["reference", "port"], default=None)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--only", action="append", default=[], help="scenario name filter")
    parser.add_argument("--raw", action="store_true", help="also keep the raw answers")
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--jobs", type=int, default=4, help="scenarios run side by side")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.server is not None:
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
            dialect = arguments.dialect or "port"
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
            dialect = arguments.dialect or "reference"
        selected = [
            scenario
            for scenario in scenarios()
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started = time.monotonic()
            run = run_scenario(scenario, command, dialect, arguments.quiet)
            entry = {
                "name": scenario["name"],
                "scenario": scenario,
                "observed": normalize_run(scenario, run),
            }
            if arguments.raw:
                entry["raw"] = run
            print(
                f"{scenario['name']}: {len(run['steps'])} observations in "
                f"{time.monotonic() - started:.1f}s",
                file=sys.stderr,
            )
            return entry

        with concurrent.futures.ThreadPoolExecutor(max_workers=arguments.jobs) as pool:
            captured = list(pool.map(capture, selected))
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "quietSeconds": arguments.quiet,
            "scenarios": captured,
        }
        if arguments.check:
            committed = json.loads(arguments.output.read_text(encoding="utf-8"))
            by_name = {entry["name"]: entry for entry in committed["scenarios"]}
            differing = [
                entry["name"]
                for entry in captured
                if by_name.get(entry["name"], {}).get("observed") != entry["observed"]
            ]
            if differing:
                raise OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except (OracleError, acp.OracleError) as error:
        print(f"rewind oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
