"""What every black-box capture of the app server drives it with.

Each scenario starts the server over stdio in a fresh home, behind the
chat-completions stand-in of :mod:`blackbox`, and talks to it as a client
would. This module holds what those captures share: the stand-in that also
records the conversation every request carried (:class:`RecordingBackend`),
one server process (:class:`Server`), what a scenario learned from the answers
so a later step can name it (:class:`Session`), the abstract ``start`` and
``turn`` steps rendered in the dialect of the server being driven, a scenario
runner over the shared step vocabulary (:func:`run_scenario`), and the
normalizer of app-server answers (:class:`Normalizer`). Nothing here imports
the reference.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
import queue
import re
import shutil
import tempfile
import time
from typing import Any

import blackbox
import host_env
from reference import OracleError


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


# --------------------------------------------------------------------------
# The chat-completions stand-in, recording what each request carried
# --------------------------------------------------------------------------


class RecordingBackend(blackbox.Backend):
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
            self.rfile = Replay(raw, stream)
            try:
                original(self)
            finally:
                # The connection is kept alive, so the next request is read
                # from the socket again.
                self.rfile = stream

        handler.do_POST = do_post


class Replay:
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


class Server(blackbox.Agent):
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
        self.world = blackbox.World(root)
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
        text = value.replace("$WS", str(self.world.workspace)).replace(
            "$ROOT", str(self.world.root)
        )
        for prefix, table in (("$S", self.sessions), ("$U", self.users), ("$A", self.assistants)):
            for index in range(len(table), 0, -1):
                text = text.replace(f"{prefix}{index}", table[index - 1])
        return text


def render_start(dialect: str, workspace: Path, agent: str) -> dict[str, Any]:
    # Both servers read `SessionStartParams.agentConfig` over stdio
    # (vibe/app_server/protocol.py).
    del dialect
    return {"agentConfig": {"cwd": str(workspace), "agent": agent}}


def render_turn(dialect: str, session_id: str, text: str) -> dict[str, Any]:
    # Both servers read `TurnStartParams.message` (vibe/app_server/protocol.py).
    del dialect
    return {"sessionId": session_id, "message": [{"type": "text", "text": text}]}


def settles(session_id_holder: dict[str, str]) -> Any:
    def predicate(message: dict[str, Any]) -> bool:
        return message.get("method") in {"turn/completed", "turn/failed", "turn/interrupted"}

    return predicate


def base_config(backend: blackbox.Backend, extra: str) -> str:
    return blackbox.base_config(backend) + extra


def run_scenario(
    scenario: dict[str, Any],
    command: list[str],
    dialect: str,
    quiet: float,
    session_factory: Any = None,
) -> dict[str, Any]:
    """Runs one scenario against a fresh server.

    ``session_factory`` lets another capture script learn more from the answers
    than rewinds need, such as the review oracle's owners and regions.
    """
    backend = RecordingBackend()
    root = Path(tempfile.mkdtemp(prefix="vibe-rewind-oracle-")).resolve()
    session = (session_factory or Session)(root)
    world = session.world
    try:
        # A tool call may name a scenario directory, which only exists now.
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        (world.vibe_home / "config.toml").write_text(
            base_config(backend, scenario.get("config", "")), encoding="utf-8", newline=""
        )
        blackbox.write_tree(world.workspace, scenario.get("files", {}))
        env = host_env.isolated(
            world.home,
            world.vibe_home,
            MISTRAL_API_KEY="oracle-key",
            VIBE_API_BASE=f"{backend.base}/v1/chat/completions",
        )
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


def watched_file(path: Path) -> Any:
    """A watched file's content: its text, its bytes in hexadecimal when they
    are not UTF-8, or ``None`` when nothing is there."""
    if not path.is_file():
        return None
    data = path.read_bytes()
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return {"hex": data.hex()}


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
            blackbox.write_tree(world.workspace, step["write"])
            continue
        if "writeBytes" in step:
            # A hand edit no text encoding can carry, given as hexadecimal.
            for name, data in step["writeBytes"].items():
                target = world.workspace / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(bytes.fromhex(data))
            continue
        if "symlink" in step:
            # A link inside the workspace, pointing at a path relative to it.
            for name, target in step["symlink"].items():
                (world.workspace / name).symlink_to(target)
            continue
        if "remove" in step:
            # A hand deletion, of a file or of a whole directory.
            for name in step["remove"]:
                target = world.workspace / name
                if target.is_dir():
                    host_env.remove_tree(target)
                elif target.exists():
                    target.unlink()
            continue
        if "files" in step:
            steps.append({"files": {
                name: watched_file(world.workspace / name) for name in step["files"]
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


class Normalizer(blackbox.Normalizer):
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


# --------------------------------------------------------------------------
# Scenario steps
# --------------------------------------------------------------------------


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def rewind(entry: str, session: str = "$S1", **options: Any) -> dict[str, Any]:
    return {"send": {"method": "session/rewind", "params": {"sessionId": session, "entryId": entry, **options}}}
