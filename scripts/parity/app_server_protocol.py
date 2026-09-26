#!/usr/bin/env python3
"""Black-box capture of the app-server protocol itself.

Row 17 of ``docs/parity.md`` is the wire a client speaks to the app server:
the connection lifecycle, the envelope rules, how every declared method is
routed and refused, and the methods no other row measures. The surface census
(``scripts/parity/app_server_surface.py``) compares names and shapes; this
script compares answers. Every scenario serves an app server over stdio in a
fresh home, behind the scripted chat-completions stand-in the rewind oracle
runs, and records what the server wrote back: each response, the
notifications it raised, and whether it still serves afterward.

Three families:

``frames``
    The lifecycle and the envelope: requests before ``initialize``, a second
    ``initialize``, notifications the server does not know, answers to
    requests it never sent, and lines that are not JSON-RPC at all.

``probe``
    Every method the reference declares, read from the committed census so
    the list follows a re-pin, sent twice on a connection with no session and
    three times on one rooted in a session: with no parameters, naming the
    root, and naming a session nobody opened. This is the routing contract:
    which methods the host answers without a session, which need a root, and
    how each one refuses what it cannot take.

``flows``
    The methods whose behavior no other row measures, driven with real
    parameters: ``events/read``, the turn queue, ``callback/result``,
    ``session/compact``, ``session/stop``, ``config/write``,
    ``session/shellCommand``, ``workspace/git/checkouts`` and the others.

Both servers are driven with the same parameters: the reference's own wire is
the only dialect this script speaks, which is what makes it a measurement of
the wire rather than of two request builders. Nothing is imported from the
reference; its ``vibe-app-server`` is the oracle, and
``crates/vibe-app-server/tests/protocol_parity_tests.rs`` replays the same
scenarios against this port's ``vibe-app-server-stdio-fixture``.

Normalization is the rewind oracle's: identities, paths and times become
placeholders and every string a server authored is reduced to its length and
SHA-256, which is what ``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/app_server_protocol.py                  # capture the reference
    python3 scripts/parity/app_server_protocol.py --check          # recapture and compare
    python3 scripts/parity/app_server_protocol.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
import rewind  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/protocol-parity/corpus.json"
CENSUS = REPOSITORY / "crates/vibe-app-server/tests/app-server-surface/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5
#: How long a server that still serves may take to answer a liveness probe.
PING_TIMEOUT = 6.0
#: A session identifier no server minted.
UNKNOWN_SESSION = "probe-unknown-session"


class OracleError(RuntimeError):
    pass


def inventory() -> list[str]:
    """The methods the pinned reference declares, from the committed census."""
    census = json.loads(CENSUS.read_text(encoding="utf-8"))
    return [entry["name"] for entry in census["methods"]]


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(rewind.Server):
    """A server whose silence is an observation rather than a failure.

    A server that stops serving is not required to close its output: the
    reference keeps its process alive until standard input closes. So whether
    a server still serves is measured by asking it something, never by waiting
    for end of file.
    """

    closed = False

    def _answer(self, request: dict[str, Any]) -> None:
        # A callback is the scenario's to answer, with an `ack` step.
        if request.get("method") == "callback/call":
            return
        super()._answer(request)

    def drain(
        self, request_id: Any, quiet: float, timeout: float = acp.RESPONSE_TIMEOUT
    ) -> tuple[list[dict[str, Any]], bool]:
        """Everything written until the response and then silence, and whether
        the response arrived."""
        messages: list[dict[str, Any]] = []
        answered = request_id is None
        if self.closed:
            return messages, answered
        deadline = time.monotonic() + timeout
        while True:
            wait = quiet if answered else max(0.0, deadline - time.monotonic())
            try:
                message = self.inbox.get(timeout=wait)
            except queue.Empty:
                return messages, answered
            if message is None:
                self.closed = True
                return messages, answered
            messages.append(message)
            if not answered and "method" not in message and message.get("id") == request_id:
                answered = True

    def raw(self, line: bytes) -> None:
        with self.write_lock:
            assert self.process.stdin is not None
            try:
                self.process.stdin.write(line)
                self.process.stdin.flush()
            except (BrokenPipeError, ValueError):
                pass

    def alive(self, identifier: int, quiet: float) -> bool:
        """Whether the server still answers a request."""
        self.send({"jsonrpc": "2.0", "id": identifier, "method": "session/list",
                   "params": {"cwd": "/probe-empty"}})
        _, answered = self.drain(identifier, quiet, PING_TIMEOUT)
        return answered


SETTLES = frozenset({"turn/completed", "turn/failed", "turn/interrupted"})


class Session(rewind.Session):
    """What a scenario learned, plus the callbacks and queue items it saw."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.callbacks: list[str] = []
        self.queue_items: list[str] = []
        self.turns: list[str] = []
        self.server_requests: list[Any] = []
        self.running = 0

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        method = message.get("method")
        if method is not None and "id" in message:
            self.server_requests.append(message["id"])
        if method == "turn/started":
            self.running += 1
        elif method in SETTLES:
            self.running = max(0, self.running - 1)
        for key, table in (("callbackId", self.callbacks), ("queueItemId", self.queue_items)):
            for found in find_values(message, key):
                if found not in table:
                    table.append(found)
        for found in find_values(message, "turnId"):
            if found not in self.turns:
                self.turns.append(found)

    def substitute(self, value: Any) -> Any:
        value = super().substitute(value)
        if isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.substitute(item) for item in value]
        if not isinstance(value, str):
            return value
        for prefix, table in (("$C", self.callbacks), ("$Q", self.queue_items), ("$T", self.turns)):
            for index in range(len(table), 0, -1):
                value = value.replace(f"{prefix}{index}", table[index - 1])
        return value


def find_values(value: Any, key: str) -> list[str]:
    found: list[str] = []
    if isinstance(value, dict):
        for name, item in value.items():
            if name == key and isinstance(item, str) and item:
                found.append(item)
            else:
                found.extend(find_values(item, key))
    elif isinstance(value, list):
        for item in value:
            found.extend(find_values(item, key))
    return found


def start_params(world: acp.World, agent: str = "auto-approve") -> dict[str, Any]:
    return {"agentConfig": {"cwd": str(world.workspace), "agent": agent}}


def oracle_config(backend: acp.Backend, extra: str) -> str:
    """The rewind oracle's configuration, with every remote base URL the app
    server may reach pointed at the stand-in, so no probe leaves the host."""
    return (
        rewind.base_config(backend, "")
        .replace(
            "enable_update_checks = false\n",
            "enable_update_checks = false\n"
            f'console_base_url = "{backend.base}"\n'
            f'vibe_base_url = "{backend.base}"\n'
            f'vibe_code_sessions_base_url = "{backend.base}"\n',
        )
        + extra
    )


def serve_gets(backend: rewind.RecordingBackend, gets: dict[str, Any]) -> None:
    """Answers a GET for one of `gets`' paths with its payload, and any
    other with the stand-in's 404."""
    handler = backend.server.RequestHandlerClass
    original = handler.do_GET

    def do_get(self: Any) -> None:
        path = self.path.split("?", 1)[0]
        if path in gets:
            self.reply(200, gets[path])
        else:
            original(self)

    handler.do_GET = do_get


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = rewind.RecordingBackend()
    serve_gets(backend, scenario.get("gets", {}))
    root = Path(tempfile.mkdtemp(prefix="vibe-protocol-oracle-"))
    session = Session(root)
    world = session.world
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        (world.vibe_home / "config.toml").write_text(
            oracle_config(backend, scenario.get("config", "")), encoding="utf-8"
        )
        acp.write_tree(world.workspace, scenario.get("files", {}))
        if scenario.get("git"):
            init_repository(world.workspace, scenario.get("remote"))
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
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Oracle",
            "GIT_AUTHOR_EMAIL": "oracle@example.invalid",
            "GIT_COMMITTER_NAME": "Oracle",
            "GIT_COMMITTER_EMAIL": "oracle@example.invalid",
        }
        server = Server(command, env, world.workspace, scenario.get("client", {}), world)
        steps: list[dict[str, Any]] = []
        try:
            run_steps(scenario, server, session, steps, quiet)
        except (OracleError, rewind.OracleError, acp.OracleError) as error:
            steps.append({"failure": str(error)})
        finally:
            server.stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
            "conversations": backend.conversations,
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


def init_repository(workspace: Path, remote: str | None = None) -> None:
    def git(*args: str) -> None:
        subprocess.run(
            ["git", *args],
            cwd=workspace,
            check=True,
            capture_output=True,
            env={
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(workspace.parent / "home"),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "Oracle",
                "GIT_AUTHOR_EMAIL": "oracle@example.invalid",
                "GIT_COMMITTER_NAME": "Oracle",
                "GIT_COMMITTER_EMAIL": "oracle@example.invalid",
            },
        )

    git("init", "-q", "-b", "main")
    git("add", "-A")
    git("commit", "-q", "--allow-empty", "-m", "initial")
    if remote:
        git("remote", "add", "origin", remote)


# Result members the reference types as `JsonValue` (free JSON the method
# carries rather than a shape it declares), by method and pointer into the
# shaped result: `ConfigSchemaResponse.config_schema` (protocol.py:749) and the
# `value` of `ConfigFieldWire` and `ConfigLayerValueWire` (protocol.py:906,913).
# They are compared as present or absent, not by what they hold.
OPAQUE = {
    "config/schema": {"/schema"},
    "config/fields/read": {"/fields/<list>/value", "/fields/<list>/layerValues/<list>/value"},
}


def shape(value: Any, pointer: str = "", opaque: frozenset[str] = frozenset()) -> Any:
    """The JSON shape of a result: its keys and value types, not its values.

    A result's values belong to the row that owns the method (the config, the
    sessions, the tools); what this row measures is the wire contract, so a
    success is compared by the keys it carries and the type each one holds.
    A list is the shape its items take together: every key any item carries,
    with every type it holds, so two lists of different lengths or orders
    compare equal when their items do.
    """
    if pointer in opaque:
        return "<json>"
    if isinstance(value, dict) and set(value) == {"prose", "sha256"}:
        return "<string>"
    # A JSON patch is its operations in order: which field each one touches
    # and how is the contract, only the value it writes is a shape.
    if isinstance(value, list) and pointer.endswith("/patch"):
        return [
            {**item, "value": shape(item.get("value"), f"{pointer}/<op>", opaque)}
            if isinstance(item, dict) else shape(item, f"{pointer}/<op>", opaque)
            for item in value
        ]
    if isinstance(value, dict):
        return {key: shape(item, f"{pointer}/{key}", opaque) for key, item in value.items()}
    if isinstance(value, list):
        merged: Any = None
        for item in value:
            item_shape = shape(item, f"{pointer}/<list>", opaque)
            merged = merge_shapes(merged, item_shape) if merged is not None else item_shape
        return {"<list>": merged}
    if isinstance(value, bool):
        return "<bool>"
    if isinstance(value, (int, float)):
        return "<number>"
    if isinstance(value, str):
        return "<string>"
    return None


def merge_shapes(left: Any, right: Any) -> Any:
    """Two shapes read as one: objects merge key by key, and anything else
    becomes the sorted set of the shapes seen."""
    if isinstance(left, dict) and isinstance(right, dict) and "<list>" not in left and "<list>" not in right:
        return {
            key: merge_shapes(left[key], right[key]) if key in left and key in right else left.get(key, right.get(key))
            for key in sorted(set(left) | set(right))
        }
    if isinstance(left, dict) and isinstance(right, dict) and "<list>" in left and "<list>" in right:
        inner = left["<list>"] if right["<list>"] is None else (
            right["<list>"] if left["<list>"] is None else merge_shapes(left["<list>"], right["<list>"]))
        return {"<list>": inner}
    options = {json.dumps(option, sort_keys=True)
               for side in (left, right)
               for option in (side["<any>"] if isinstance(side, dict) and "<any>" in side else [side])}
    if len(options) == 1:
        return left
    return {"<any>": [json.loads(option) for option in sorted(options)]}


def observe(
    steps: list[dict[str, Any]],
    label: str,
    observed: list[dict[str, Any]],
    identifier: Any,
    alive: bool | None = None,
    exact: bool = False,
) -> None:
    response = next(
        (m for m in observed if "method" not in m and m.get("id") == identifier), None
    )
    notifications = [
        {"method": m.get("method"), "params": m.get("params")}
        for m in observed
        if "method" in m and "id" not in m
    ]
    requests = [m.get("method") for m in observed if "method" in m and "id" in m]
    # Answers to earlier requests posted without waiting, in arrival order.
    replies = [
        {k: v for k, v in m.items() if k not in {"jsonrpc", "id"}}
        for m in observed
        if "method" not in m and m.get("id") != identifier
    ]
    entry: dict[str, Any] = {"step": label, "notifications": notifications}
    if identifier is not None:
        entry["response"] = (
            {k: v for k, v in response.items() if k not in {"jsonrpc", "id"}}
            if response is not None
            else None
        )
    if requests:
        entry["requests"] = requests
    if replies:
        entry["replies"] = replies
    if exact:
        entry["exact"] = True
    if alive is not None:
        entry["alive"] = alive
    steps.append(entry)


def run_steps(
    scenario: dict[str, Any],
    server: Server,
    session: Session,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    identifier = 0
    for step in scenario["steps"]:
        identifier += 1
        if step.get("init"):
            params = step.get("params", {"clientInfo": {"name": "protocol-oracle", "version": "0"}})
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize", "params": params})
            observed, _ = server.drain(identifier, quiet)
            if step.get("record"):
                observe(steps, "initialize", observed, identifier)
            server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
            observed, _ = server.drain(None, quiet)
            if observed:
                observe(steps, "initialized", observed, None)
            continue
        if "start" in step:
            params = session.substitute(step["start"]) if step["start"] else start_params(world)
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start", "params": params})
            observed, _ = server.drain(identifier, quiet)
            for message in observed:
                session.learn(message)
            if step.get("record"):
                observe(steps, "session/start", observed, identifier)
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": {"sessionId": target,
                                    "message": [{"type": "text", "text": step["turn"]}]}})
            if step.get("background"):
                time.sleep(step.get("settle", 0.5))
                observed, _ = server.drain(None, quiet)
                for message in observed:
                    session.learn(message)
                continue
            for message in server.wait_for(rewind.settles({})):
                session.learn(message)
            observed, _ = server.drain(None, quiet)
            for message in observed:
                session.learn(message)
            continue
        if "await" in step:
            # A queued turn can start and settle inside the drain of the
            # request that queued it, so wait only for turns still running.
            while session.running:
                for message in server.wait_for(rewind.settles({})):
                    session.learn(message)
            observed, _ = server.drain(None, quiet)
            for message in observed:
                session.learn(message)
            continue
        if "sleep" in step:
            time.sleep(step["sleep"])
            continue
        if "ack" in step:
            # Acknowledges the latest server request, a `callback/call`,
            # whose callback the scenario has learned.
            observed, _ = server.drain(None, quiet)
            for message in observed:
                session.learn(message)
            if observed:
                observe(steps, "before-ack", observed, None)
            if not session.server_requests:
                observe(steps, "ack", [], None)
                continue
            server.send({"jsonrpc": "2.0", "id": session.server_requests[-1],
                         **session.substitute(step["ack"])})
            observed, _ = server.drain(None, quiet)
            for message in observed:
                session.learn(message)
            observe(steps, "ack", observed, None)
            continue
        if "write" in step:
            acp.write_tree(world.workspace, step["write"])
            continue
        if "raw" in step:
            server.raw(step["raw"].encode("utf-8"))
            observed, _ = server.drain(None, quiet)
            identifier += 1
            observe(steps, "raw", observed, None, server.alive(identifier, quiet))
            continue
        if "notify" in step:
            server.send({"jsonrpc": "2.0", **session.substitute(step["notify"])})
            observed, _ = server.drain(None, quiet)
            identifier += 1
            observe(steps, f"notify:{step['notify'].get('method')}", observed, None,
                    server.alive(identifier, quiet))
            continue
        if "post" in step:
            # A request sent without waiting for its answer, which a later
            # step collects among its replies.
            request = session.substitute(step["post"])
            server.send({"jsonrpc": "2.0", "id": identifier, **request})
            observed, _ = server.drain(None, quiet)
            observe(steps, f"post:{request['method']}", observed, None)
            continue
        if "frame" in step:
            # A whole frame, sent as written: an envelope the server may refuse.
            frame = session.substitute(step["frame"])
            server.send(frame)
            request_id = frame.get("id") if "method" in frame else None
            observed, _ = server.drain(request_id, quiet, PING_TIMEOUT)
            identifier += 1
            observe(steps, "frame", observed, request_id, server.alive(identifier, quiet))
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed, answered = server.drain(identifier, quiet)
        for message in observed:
            session.learn(message)
        observe(steps, request["method"], observed, identifier, exact=step.get("exact", False))
        if not answered:
            return


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[Any]:
    normalizer = rewind.Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    for step in run["steps"]:
        normalizer.collect_ids(step.get("response"))
    observed: list[Any] = []
    for step in run["steps"]:
        if "failure" in step:
            observed.append({"failure": normalizer.text(step["failure"])})
            continue
        entry = normalizer.value(copy.deepcopy(step))
        # A notification is compared by its name and the shape of what it
        # carries, whatever the step: its values are ids, clocks and counters.
        entry["notifications"] = [
            {"method": item["method"], "params": shape(item.get("params"), "", frozenset())}
            if isinstance(item, dict) else item
            for item in entry.get("notifications", [])
        ]
        response = entry.get("response")
        if not entry.pop("exact", False) and isinstance(response, dict) and "result" in response:
            response["result"] = shape(
                response["result"], "", frozenset(OPAQUE.get(entry.get("step", ""), ()))
            )
        for reply in entry.get("replies", []):
            if "result" in reply:
                reply["result"] = shape(reply["result"], "", frozenset())
        observed.append(entry)
    return observed


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------

INIT = {"init": True}
START = {"start": {}}


def send(method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    return {"send": {"method": method, "params": params if params is not None else {}}}


def exact(step: dict[str, Any]) -> dict[str, Any]:
    """A step whose result is compared by value, not only by shape."""
    return {**step, "exact": True}


def frames() -> list[dict[str, Any]]:
    client = {"clientInfo": {"name": "protocol-oracle", "version": "0"}}
    return [
        {"name": "frames/request-before-initialize",
         "steps": [send("session/list"), {"init": True, "record": True}, send("session/list", {"cwd": "/probe-empty"})]},
        {"name": "frames/initialize-twice",
         "steps": [{"init": True, "record": True}, {"send": {"method": "initialize", "params": client}}]},
        {"name": "frames/initialize-invalid",
         "steps": [
             {"send": {"method": "initialize", "params": {}}},
             {"send": {"method": "initialize", "params": {"clientInfo": {"name": "x"}, "extra": 1}}},
             {"send": {"method": "initialize", "params": {**client, "capabilities": {"callbackKinds": ["nope"]}}}},
             {"init": True, "record": True},
         ]},
        {"name": "frames/initialize-with-capabilities",
         "steps": [
             {"init": True, "record": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0", "title": "Oracle",
                                "entrypoint": "desktop", "terminalEmulator": "kitty"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"],
                                  "clientTools": ["filesystem/read"],
                                  "disabledNotifications": ["warning"]}}},
             send("session/list", {"cwd": "/probe-empty"}),
         ]},
        {"name": "frames/request-before-initialized",
         "steps": [
             {"send": {"method": "initialize", "params": client}},
             send("session/list", {"cwd": "/probe-empty"}),
         ]},
        {"name": "frames/initialized-twice",
         "steps": [INIT, {"notify": {"method": "initialized", "params": {}}}]},
        {"name": "frames/initialized-before-initialize",
         "steps": [{"notify": {"method": "initialized", "params": {}}}]},
        {"name": "frames/unknown-notification",
         "steps": [INIT, {"notify": {"method": "probe/unknown", "params": {}}}]},
        {"name": "frames/exit-notification",
         "steps": [INIT, {"notify": {"method": "exit", "params": {}}}]},
        {"name": "frames/shutdown-request",
         "steps": [INIT, send("shutdown")]},
        {"name": "frames/unknown-method",
         "steps": [INIT, send("probe/unknown"), send("internal/session/list")]},
        {"name": "frames/unknown-method-before-initialize",
         "steps": [send("probe/unknown")]},
        {"name": "frames/string-id",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": "text-id", "method": "session/list",
                                    "params": {"cwd": "/probe-empty"}}}]},
        {"name": "frames/missing-params",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 90, "method": "session/list"}}]},
        {"name": "frames/extra-envelope-field",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 91, "method": "session/list",
                                    "params": {}, "extra": 1}}]},
        {"name": "frames/wrong-version",
         "steps": [INIT, {"frame": {"jsonrpc": "1.0", "id": 92, "method": "session/list",
                                    "params": {}}}]},
        {"name": "frames/response-to-nothing",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 93, "result": {}}}]},
        {"name": "frames/error-to-nothing",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 94, "error": {
             "code": "internal_error", "message": "x", "data": None}}}]},
        {"name": "frames/malformed-json",
         "steps": [INIT, {"raw": "{not json\n"}]},
        {"name": "frames/non-object",
         "steps": [INIT, {"raw": "[1, 2]\n"}]},
        {"name": "frames/blank-line",
         "steps": [INIT, {"raw": "\n"}]},
    ]


def probes() -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []
    for method in inventory():
        scenarios.append({
            "name": f"probe/{method}/bare",
            "steps": [INIT, send(method), send(method, {"sessionId": UNKNOWN_SESSION})],
        })
        scenarios.append({
            "name": f"probe/{method}/rooted",
            "steps": [
                INIT,
                START,
                send(method),
                send(method, {"sessionId": UNKNOWN_SESSION}),
                send(method, {"sessionId": "$S1"}),
            ],
        })
    return scenarios


def flows() -> list[dict[str, Any]]:
    return [
        {"name": "flows/session-start",
         "steps": [INIT, {"start": {}, "record": True}, send("session/start", start_params_placeholder())]},
        {"name": "flows/session-start-refusals",
         "steps": [
             INIT,
             send("session/start", {"cwd": "$WS"}),
             send("session/start", {"agentConfig": {"cwd": "$WS", "bogus": 1}}),
             send("session/start", {"agentConfig": {"cwd": "$WS"}, "historyLimit": 0}),
             send("session/start", {"agentConfig": {"cwd": "$WS"}, "kind": "nope"}),
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve"}, "kind": "ephemeral"},
              "record": True},
         ]},
        {"name": "flows/events-read",
         "steps": [
             INIT,
             exact(send("events/read")),
             exact(send("events/read", {"afterEventId": 3, "batchSize": 10,
                                        "filters": {"sessionIds": ["x"], "eventTypes": ["turn/started"]}})),
             send("events/read", {"batchSize": 0}),
             send("events/read", {"bogus": True}),
             START,
             exact(send("events/read", {"afterEventId": 0})),
         ]},
    ]


def user_entry(text: str) -> dict[str, Any]:
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def context_entry(text: str) -> dict[str, Any]:
    return {"role": "context", "content": [{"type": "text", "text": text}]}


def queue_flows() -> list[dict[str, Any]]:
    """The turn queue: what a queued turn does to an idle session and to a
    busy one, how an interrupt pauses the queue, and what it refuses."""
    read = exact(send("session/turn/queue/read", {"sessionId": "$S1"}))
    return [
        {"name": "flows/turn-lifecycle",
         "backend": [{"text": "Plain answer."}, {"text": "Slow answer.", "delay": 3.0}],
         "steps": [
             INIT, START,
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Plain question"}]}),
             send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T1"}),
             {"turn": "Slow question", "background": True, "settle": 0.5},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Refused"}]}),
             send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T1"}),
             exact(send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T2"})),
             {"await": True},
             exact(send("session/turn/queue/read", {"sessionId": "$S1"})),
         ]},
        {"name": "flows/turn-tools",
         "files": {"notes.txt": "one\ntwo\n"},
         "backend": [
             {"text": "Reading.", "toolCalls": [{"id": "call_read", "name": "read_file",
                                                  "arguments": {"file_path": "notes.txt"}}]},
             {"text": "Read it."},
         ],
         "steps": [
             INIT,
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Read notes"}]}),
             {"await": True},
         ]},
        {"name": "flows/turn-queue-idle",
         "backend": [{"text": "Queued answer."}],
         "steps": [
             INIT, START,
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [user_entry("Queued question")]})),
             {"await": True},
             read,
         ]},
        {"name": "flows/turn-queue-busy",
         "backend": [
             {"text": "Slow answer.", "delay": 5.0},
             {"text": "First queued answer."},
             {"text": "Second queued answer."},
         ],
         "steps": [
             INIT, START,
             {"turn": "Slow question", "background": True, "settle": 0.5},
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [user_entry("First queued")]})),
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [
                 context_entry("Some context"), user_entry("Second queued")]})),
             read,
             exact(send("session/turn/queue/replace", {"sessionId": "$S1", "queueItemId": "$Q2",
                                                       "entries": [user_entry("Second, replaced")]})),
             exact(send("session/turn/queue/remove", {"sessionId": "$S1", "queueItemId": "$Q1"})),
             read,
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Refused"}]}),
             {"await": True},
             read,
         ]},
        {"name": "flows/turn-queue-interrupt",
         "backend": [
             {"text": "Slow answer.", "delay": 3.0},
             {"text": "Resumed answer."},
         ],
         "steps": [
             INIT, START,
             {"turn": "Slow question", "background": True, "settle": 0.5},
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [user_entry("Waiting")]})),
             exact(send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T1"})),
             {"sleep": 1.0},
             read,
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Refused"}]}),
             exact(send("session/turn/queue/resume", {"sessionId": "$S1"})),
             {"await": True},
             read,
             exact(send("session/turn/queue/resume", {"sessionId": "$S1"})),
         ]},
        {"name": "flows/turn-queue-refusals",
         "backend": [{"text": "Keyed answer."}, {"text": "Context answer."}],
         "steps": [
             INIT, START,
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "idempotencyKey": "k1",
                                                 "entries": [user_entry("Keyed")]})),
             {"await": True},
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "idempotencyKey": "k1",
                                                 "entries": [user_entry("Keyed")]})),
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "idempotencyKey": "k1",
                                                 "entries": [user_entry("Different")]})),
             exact(send("session/turn/queue/replace", {"sessionId": "$S1", "queueItemId": "no-such-item",
                                                       "entries": [user_entry("Nothing")]})),
             exact(send("session/turn/queue/remove", {"sessionId": "$S1", "queueItemId": "no-such-item"})),
             send("session/turn/enqueue", {"sessionId": "$S1", "entries": [
                 user_entry("One"), user_entry("Two")]}),
             send("session/turn/enqueue", {"sessionId": "$S1", "entries": [
                 user_entry("First"), context_entry("After")]}),
             send("session/turn/enqueue", {"sessionId": "$S1", "entries": []}),
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [context_entry("Only context")]})),
             {"sleep": 1.0},
             read,
             send("session/turn/queue/steer", {"sessionId": "$S1", "queueItemId": "$Q1", "expectedTurnId": "x"}),
         ]},
    ]


def session_flows() -> list[dict[str, Any]]:
    """The session methods a probe only reaches the validation of, driven
    with parameters that pass it."""
    def on(method: str, **params: Any) -> dict[str, Any]:
        return send(method, {"sessionId": "$S1", **params})

    return [
        {"name": "flows/session-mutations",
         "steps": [
             INIT, START,
             on("session/rename", title="Renamed"),
             on("session/settings/update", maxTurns=5),
             on("session/settings/update", maxTokens=1000, maxTurns=None),
             on("session/agent/update", agentName="plan"),
             on("session/agent/update", agentName="nope"),
             on("agents/install", agentName="nope"),
             on("agents/install", agentName="lean"),
             on("agents/uninstall", agentName="lean"),
             on("feedback/record", action="asked"),
             on("feedback/record", action="snoozed"),
             on("telemetry/record", name="oracle.event", properties={"k": 1}),
         ]},
        {"name": "flows/workspace-prompt",
         "files": {"notes.txt": "one\n", "shot.png": "not an encoded image\n"},
         # The image is snapshot into the session's directory, which exists
         # once a turn has been written.
         "backend": [{"text": "Noted."}],
         "steps": [
             INIT, START,
             on("workspace/prompt/prepare", message="hello there"),
             on("workspace/prompt/prepare", message="read @notes.txt please"),
             {"turn": "hi"},
             on("workspace/prompt/prepare", message="see @shot.png and @shot.png"),
             on("workspace/prompt/prepare", message="",
                titleContent=[{"type": "text", "text": "Title text"}]),
         ]},
        {"name": "flows/context-inject",
         "steps": [
             INIT, START,
             on("session/context/inject", input=[{"type": "text", "text": "ctx"}]),
             on("session/context/inject", input=[{"type": "text", "text": "msg"}], asMessage=True),
             on("session/context/inject", input=[{"type": "text", "text": "again"}], asMessage=True,
                clientUserMessageId="client-1"),
             on("session/history/get"),
         ]},
        {"name": "flows/loops",
         "steps": [
             INIT, START,
             on("loops/create", interval="5m", prompt="ping"),
             on("loops/create", interval="soon", prompt="ping"),
             on("loops/list"),
             on("loops/delete", loopId="nope"),
         ]},
        {"name": "flows/config-writes",
         "steps": [
             INIT, START,
             on("config/proxy/write", changes={"HTTP_PROXY": "http://proxy.invalid:8080"}),
             on("config/proxy/read"),
             on("config/proxy/write", changes={"HTTP_PROXY": None}),
             on("config/write", ops=[{"op": "set", "path": "/theme", "value": "dracula"}]),
             on("config/write", ops=[{"op": "set", "path": "/theme", "value": 3}]),
             on("config/write", ops=[{"op": "remove", "path": "/theme"}], reason="oracle"),
             on("config/model/write", reasoningEffort="low"),
             on("config/model/write", modelAlias="nope"),
             on("config/reload"),
         ]},
        {"name": "flows/history-get",
         "backend": [{"text": "Answer."}],
         "steps": [
             INIT, START,
             on("session/history/get"),
             {"turn": "Question"},
             on("session/history/get"),
             on("session/history/get", historyLimit=1),
         ]},
        {"name": "flows/callback-result",
         "steps": [
             INIT, START,
             on("callback/result", result={"callbackId": "nope", "output": {"x": 1}}),
             on("callback/result", result={"callbackId": "nope",
                                           "output": {"type": "approval", "approved": True}}),
             on("callback/result", result={"callbackId": "nope",
                                           "error": {"message": "refused"}}),
         ]},
        {"name": "flows/callback-approval",
         "backend": [
             {"text": "Writing.", "toolCalls": [{"id": "call_write", "name": "write_file",
                                                 "arguments": {"file_path": "out.txt", "content": "x\n"}}]},
             {"text": "Written."},
         ],
         "steps": [
             {"init": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"]}}},
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "ask"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Write"}]}),
             {"sleep": 1.0},
             {"ack": {"result": {"callbackId": "$C1", "accepted": True}}},
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "user_input", "result": {"answers": []}}}),
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "approval", "decision": {"type": "approve"}}}),
             {"await": True},
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "approval", "decision": {"type": "approve"}}}),
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "approval", "decision": {"type": "deny"}}}),
         ]},
        {"name": "flows/callback-reject",
         "backend": [
             {"text": "Writing.", "toolCalls": [{"id": "call_write", "name": "write_file",
                                                 "arguments": {"file_path": "out.txt", "content": "x\n"}}]},
             {"text": "Refused."},
         ],
         "steps": [
             {"init": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"]}}},
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "ask"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Write"}]}),
             {"sleep": 1.0},
             {"ack": {"result": {"callbackId": "$C1", "accepted": True}}},
             on("callback/result", result={"callbackId": "$C1", "error": {"message": "refused"}}),
             {"await": True},
             on("callback/result", result={"callbackId": "$C1", "error": {"message": "refused"}}),
         ]},
        {"name": "flows/callback-refused-delivery",
         "backend": [
             {"text": "Writing.", "toolCalls": [{"id": "call_write", "name": "write_file",
                                                 "arguments": {"file_path": "out.txt", "content": "x\n"}}]},
             {"text": "Refused."},
         ],
         "steps": [
             {"init": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"]}}},
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "ask"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Write"}]}),
             {"sleep": 1.0},
             {"ack": {"result": {"callbackId": "$C1", "accepted": False}}},
             {"await": True},
         ]},
        {"name": "flows/narration",
         "backend": [{"text": "Narrated summary."}],
         "steps": [
             INIT, START,
             on("narration/summarize", userMessage="hi", assistantText="hello"),
         ]},
        {"name": "flows/shell-command",
         "steps": [
             INIT, START,
             on("session/shellCommand", command="echo hi"),
             on("session/shellCommand", command="echo out; echo err >&2; exit 3"),
             on("session/shellCommand", command="sleep 3", timeoutSeconds=0.5),
             on("session/shellCommand", action="interrupt", operationId="nope"),
             on("session/shellCommand", action="interrupt"),
             on("session/shellCommand"),
             on("session/shellCommand", command="   "),
             on("session/shellCommand", command="pwd", timeoutSeconds=0),
             on("session/shellCommand", command="pwd", cwd="$WS/missing"),
             on("session/shellCommand", command="pwd", cwd="/"),
             on("session/shellCommand", command="pwd", cwd="$WS"),
             {"post": {"method": "session/shellCommand",
                       "params": {"sessionId": "$S1", "command": "sleep 5", "operationId": "op-1"}}},
             on("session/shellCommand", command="echo again", operationId="op-1"),
             on("session/shellCommand", action="interrupt", operationId="op-1"),
             {"sleep": 1.0},
             on("session/history/get"),
         ]},
        {"name": "flows/connector-catalog",
         "config": '[[connectors]]\nname = "Drive"\ndisabled_tools = ["list"]\n\n[[connectors]]\nname = "Ghost"\ndisabled = true\n\n[[connectors]]\nname = "Mail_box"\n',
         "gets": {"/v1/connectors/bootstrap": {"connectors": [
             {"id": "c-drive", "name": "Drive", "display_name": "Google Drive", "protocol": "mcp",
              "status": {"is_ready": True},
              "tools": [{"name": "search", "description": "Search files", "inputSchema": {"type": "object"}},
                        {"name": "list"}]},
             {"id": "c-mail", "name": "Mail box", "status": {"is_ready": False},
              "auth_action": {"type": "oauth"}, "bootstrap_errors": ["token_expired: gone"]},
             {"id": "c-bad", "tools": "nope"},
         ]}},
         "steps": [
             INIT,
             send("connector_catalog/read"),
             send("connector_catalog/refresh"),
             send("connector_catalog/read"),
             START,
             on("connector_catalog/read"),
             on("connector_catalog/refresh"),
             on("connector_catalog/read"),
             on("connectors/read"),
             send("connector_catalog/toggle", {"alias": "Drive", "disabled": False, "toolName": "list"}),
             on("connector_catalog/toggle", alias="Drive", disabled=False, toolName="list"),
             on("connector_catalog/toggle", alias="Nope", disabled=True),
             on("connector_catalog/toggle", alias="Bad name", disabled=True),
             on("connector_catalog/toggle", alias="Drive", disabled=True, toolName=" "),
             send("connector_catalog/toggle", {"alias": "Other", "disabled": True}),
             on("connector_catalog/auth/request", alias="Drive"),
             on("connector_catalog/auth/request", alias="Nope"),
             on("connector_catalog/auth/request", alias="Mail_box"),
             on("connectors/auth/read", name="Mail_box"),
             on("connectors/refresh", name="Drive"),
             on("connectors/refresh", name="Nope"),
             on("connector_catalog/read"),
         ]},
        {"name": "flows/skills-registry",
         "gets": {
             "/v1/skills/sk-1": {
                 "skillId": "sk-1", "version": 2,
                 "skill": {"skillName": "Demo Skill", "skillDescription": "Runs the demo",
                           "skillBody": "Run the demo.\n"},
                 "metadata": {"name": "demo", "latestVersion": 3, "sharingScope": "org",
                              "createdAt": "2026-01-01T00:00:00Z", "createdBy": "someone",
                              "lastModifiedAt": "2026-01-02T00:00:00Z"},
                 "versionMetadata": {"createdAt": "2026-01-01T00:00:00Z"},
                 "versionAttributes": {"aliases": ["stable"], "notes": "first"}},
             "/v1/skills/sk-1/versions": {"items": [
                 {"version": 1}, {"version": 2, "versionAttributes": {"aliases": ["stable"]}}]},
             "/v1/skills/sk-empty": {"skillId": "sk-empty", "version": 1,
                                     "metadata": {"name": "hollow"}},
         },
         "steps": [
             INIT, START,
             on("skills/versions", skillId="sk-1"),
             on("skills/versions", skillId="sk-missing"),
             on("skills/detail", skillId="sk-1"),
             on("skills/detail", skillId="sk-missing", version=4),
             on("skills/import", skillId="sk-empty"),
             on("skills/import", skillId="sk-missing"),
             on("skills/import", skillId="sk-1", scope="project"),
             on("skills/import", skillId="sk-1"),
             on("skills/installed"),
             on("skills/setAlias", name="demo", alias="stable"),
             on("skills/setVersion", name="demo", version=2),
             on("skills/setLatest", name="demo"),
             on("skills/setLatest", name="ghost"),
             on("skills/setEnabled", name="ghost", enabled=False),
             on("skills/remove", name="ghost"),
             on("skills/convertLocal", name="ghost"),
             on("skills/convertLocal", name="demo"),
             on("skills/installed"),
             on("skills/setEnabled", name="demo", enabled=False),
             on("skills/setEnabled", name="demo", enabled=True),
             on("skills/remove", name="demo"),
             on("skills/import", skillId="sk-1", alias="stable"),
             on("skills/remove", name="demo"),
         ]},
        {"name": "flows/git-checkouts",
         "git": True,
         "remote": "git@gitlab.example.com:group/sub/demo.git",
         "files": {"nested/keep.txt": "x\n"},
         "steps": [
             INIT,
             send("workspace/git/checkouts", {"repoLocalPaths": ["$WS", "$ROOT/missing"]}),
             send("workspace/git/checkouts",
                  {"repoLocalPaths": ["$WS", "$ROOT"], "sessionCwd": "$WS/nested"}),
             send("workspace/git/checkouts", {"repoLocalPaths": []}),
             send("workspace/git/checkouts", {"repoLocalPaths": "$WS"}),
         ]},
        {"name": "flows/session-stop",
         "steps": [
             INIT, START,
             on("session/stop", reason="done"),
             on("session/read"),
         ]},
        {"name": "flows/session-stop-unknown",
         "steps": [
             INIT, START,
             send("session/stop", {"sessionId": "nope"}),
             on("session/read"),
         ]},
        {"name": "flows/session-stop-unattached",
         "steps": [
             INIT,
             send("session/stop", {"sessionId": "nope"}),
             send("session/list", {"cwd": "/probe-empty"}),
         ]},
    ]


def start_params_placeholder() -> dict[str, Any]:
    return {"agentConfig": {"cwd": "$WS", "agent": "auto-approve"}}


def scenarios() -> list[dict[str, Any]]:
    return [*frames(), *flows(), *queue_flows(), *session_flows(), *probes()]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--server", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--only", action="append", default=[], help="scenario name filter")
    parser.add_argument("--raw", action="store_true", help="also keep the raw answers")
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--jobs", type=int, default=8, help="scenarios run side by side")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.server is not None:
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
        selected = [
            scenario
            for scenario in scenarios()
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started = time.monotonic()
            run = run_scenario(scenario, command, arguments.quiet)
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
    except (OracleError, acp.OracleError, rewind.OracleError) as error:
        print(f"protocol oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
