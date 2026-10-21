#!/usr/bin/env python3
"""Black-box capture of the app-server protocol itself.

Row 17 of ``docs/parity.md`` is the wire a client speaks to the app server: the
connection lifecycle, the envelope rules, how every declared method is routed
and refused, and the methods no other row measures. The surface census
(``scripts/parity/app_server_surface.py``) compares names and shapes; this
script compares answers. Every scenario serves an app server over stdio in a
fresh home, behind the recording chat-completions stand-in of
``app_server_client.py``, and records what the server wrote back: each
response, the notifications it raised, and whether it still serves afterward.

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

Normalization is the one ``app_server_client.py`` shares: identities, paths
and times become placeholders and every string a server authored is reduced
to its length and SHA-256, which is what ``NOTICE`` requires of a committed
corpus.

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
import re
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import app_server_client  # noqa: E402
from app_server_protocol_scenarios import (  # noqa: E402
    INIT,
    START,
    flows,
    frames,
    queue_flows,
    send,
    session_flows,
)
import blackbox  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
from reference import OracleError, reference_command  # noqa: E402
import host_env  # noqa: E402
import stand_in  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/protocol-parity/corpus.json"
CENSUS = REPOSITORY / "crates/vibe-app-server/tests/app-server-surface/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5
#: How long a server that still serves may take to answer a liveness probe.
PING_TIMEOUT = 6.0
#: A session identifier no server minted.
UNKNOWN_SESSION = "probe-unknown-session"
#: What the fixture repository and the server commit as. The fixture home holds
#: no ``.gitconfig``, so git reads its global file from there.
GIT_IDENTITY = {
    "GIT_AUTHOR_NAME": "Oracle",
    "GIT_AUTHOR_EMAIL": "oracle@example.invalid",
    "GIT_COMMITTER_NAME": "Oracle",
    "GIT_COMMITTER_EMAIL": "oracle@example.invalid",
}


def inventory() -> list[str]:
    """The methods the pinned reference declares, from the committed census."""
    census = json.loads(CENSUS.read_text(encoding="utf-8"))
    return [entry["name"] for entry in census["methods"]]


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(app_server_client.Server):
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
        self, request_id: Any, quiet: float, timeout: float = blackbox.RESPONSE_TIMEOUT
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

    def alive(self, identifier: int, quiet: float) -> bool:
        """Whether the server still answers a request."""
        self.send({"jsonrpc": "2.0", "id": identifier, "method": "session/list",
                   "params": {"cwd": "/probe-empty"}})
        _, answered = self.drain(identifier, quiet, PING_TIMEOUT)
        return answered


SETTLES = frozenset({"turn/completed", "turn/failed", "turn/interrupted"})


class Session(app_server_client.Session):
    """What a scenario learned, plus the callbacks and queue items it saw."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.callbacks: list[str] = []
        self.queue_items: list[str] = []
        self.turns: list[str] = []
        self.server_requests: list[Any] = []
        self.worktrees: list[str] = []
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
        # The linked worktrees a `workspace/git/worktrees/list` answer names.
        result = message.get("result")
        if isinstance(result, dict) and isinstance(result.get("worktrees"), list):
            for found in find_values(result["worktrees"], "root"):
                if found not in self.worktrees:
                    self.worktrees.append(found)

    def substitute(self, value: Any) -> Any:
        value = super().substitute(value)
        if isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.substitute(item) for item in value]
        if not isinstance(value, str):
            return value
        for prefix, table in (("$C", self.callbacks), ("$Q", self.queue_items), ("$T", self.turns),
                              ("$W", self.worktrees)):
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


def start_params(world: blackbox.World, agent: str = "auto-approve") -> dict[str, Any]:
    return {"agentConfig": {"cwd": str(world.workspace), "agent": agent}}


def oracle_config(backend: stand_in.Backend, extra: str) -> str:
    """The shared app-server configuration, with every remote base URL the app
    server may reach pointed at the stand-in, so no probe leaves the host."""
    return (
        app_server_client.base_config(backend, "")
        .replace(
            "enable_update_checks = false\n",
            "enable_update_checks = false\n"
            f'console_base_url = "{backend.base}"\n'
            f'vibe_base_url = "{backend.base}"\n'
            f'vibe_code_sessions_base_url = "{backend.base}"\n',
        )
        + extra
    )


class Backend(stand_in.Backend):
    """The stand-in, answering a GET for one of ``gets``' paths with its
    payload, and any other with a 404."""

    def __init__(self, gets: dict[str, Any]) -> None:
        super().__init__()
        self.gets = gets

    def get(self, handler: stand_in.Handler, path: str) -> None:
        if path in self.gets:
            handler.reply(200, self.gets[path])
        else:
            super().get(handler, path)


#: Where `flows/setup` stores a key through `setup/store-credential`: the
#: keyring service both implementations use, and the provider's variable.
KEYRING_SERVICE = "ai.mistral.vibe"
STORED_ACCOUNT = "OTHER_ORACLE_KEY"


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = Backend(scenario.get("gets", {}))
    with host_env.scratch_root("vibe-protocol-oracle-") as root:
        session = Session(root)
        world = session.world
        stores_credential = '"setup/store-credential"' in json.dumps(scenario)
        if stores_credential:
            host_env.forget_credential(KEYRING_SERVICE, STORED_ACCOUNT)
        try:
            backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
            (world.vibe_home / "config.toml").write_text(
                oracle_config(backend, scenario.get("config", "")), encoding="utf-8", newline=""
            )
            blackbox.write_tree(world.workspace, scenario.get("files", {}))
            if scenario.get("git"):
                init_repository(world.workspace, scenario.get("remote"))
            env = {
                **host_env.isolated(
                    world.home,
                    world.vibe_home,
                    MISTRAL_API_KEY="oracle-key",
                    VIBE_API_BASE=f"{backend.base}/v1/chat/completions",
                ),
                **host_env.git_isolated(None, **GIT_IDENTITY),
            }
            def spawn() -> Server:
                return Server(command, env, world.workspace, scenario.get("client", {}), world)

            servers = [spawn()]
            steps: list[dict[str, Any]] = []
            try:
                run_steps(scenario, servers, spawn, session, steps, quiet)
            except OracleError as error:
                steps.append({"failure": str(error)})
            finally:
                servers[-1].stop()
            return {
                "steps": steps,
                "sessions": session.sessions,
                "conversations": [
                    app_server_client.conversation(body.get("messages", [])) for body in backend.bodies
                ],
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
            if stores_credential:
                host_env.forget_credential(KEYRING_SERVICE, STORED_ACCOUNT)


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
                **host_env.windows_base(str(workspace.parent / "home")),
                **host_env.git_isolated(None, **GIT_IDENTITY),
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
    servers: list[Server],
    spawn: Any,
    session: Session,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    server = servers[-1]
    identifier = 0
    for step in scenario["steps"]:
        identifier += 1
        if "restart" in step:
            # A fresh server over the same home and workspace, for what
            # outlives the connection that `session/stop` closes.
            server.stop()
            server = spawn()
            servers.append(server)
            continue
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
            for message in server.wait_for(app_server_client.settles({})):
                session.learn(message)
            observed, _ = server.drain(None, quiet)
            for message in observed:
                session.learn(message)
            continue
        if "await" in step:
            # A queued turn can start and settle inside the drain of the
            # request that queued it, so wait only for turns still running.
            while session.running:
                for message in server.wait_for(app_server_client.settles({})):
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
            into = step.get("into", "$WS")
            target = Path(session.substitute(into))
            # A placeholder left unresolved (a worktree that was never
            # created) is a relative path, which would land in the
            # driver's own working directory.
            if not target.is_absolute():
                raise OracleError(f"the write target {into} never resolved to a path")
            blackbox.write_tree(target, step["write"])
            continue
        if "raw" in step:
            server.write_line(step["raw"].encode("utf-8"))
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


# A managed worktree lives under `worktrees/<repository>-<12 hex>/`, the hex
# drawn from the common git directory (`vibe/core/git/worktree/repository.py`),
# so it changes with every scenario's temporary workspace.
MANAGED_ROOT = re.compile(r"/worktrees/[^/]+-[0-9a-f]{12}/")


def managed_roots(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: managed_roots(item) for key, item in value.items()}
    if isinstance(value, list):
        return [managed_roots(item) for item in value]
    if isinstance(value, str):
        return MANAGED_ROOT.sub("/worktrees/<repository>/", value)
    return value


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[Any]:
    normalizer = app_server_client.Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    for step in run["steps"]:
        normalizer.collect_ids(step.get("response"))
    observed: list[Any] = []
    for step in run["steps"]:
        if "failure" in step:
            observed.append({"failure": normalizer.text(step["failure"])})
            continue
        entry = managed_roots(normalizer.value(copy.deepcopy(step)))
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
            command = [str(arguments.server.resolve()), *HARNESS_FLAGS]
            reference = {"commit": "server-override"}
        else:
            commit, binary = reference_command(
                arguments.reference, "vibe-app-server", arguments.expected_commit
            )
            reference = {"commit": commit}
            command = [*binary, *HARNESS_FLAGS]
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
        staged.write_text(blackbox.rendered(corpus), encoding="utf-8", newline="")
        os.replace(staged, arguments.output)
        return 0
    except OracleError as error:
        print(f"protocol oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
