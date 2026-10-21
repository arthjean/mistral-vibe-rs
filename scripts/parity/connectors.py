#!/usr/bin/env python3
"""Black-box capture of connectors: the account catalog, its selection, and the
tools a session publishes and calls through it.

Every scenario serves an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in of ``stand_in.py``. The same
stand-in also plays the three Mistral endpoints connectors reach:

- ``GET /v1/connectors/bootstrap``, answered from the scenario's scripted
  payloads in order (the last one repeating), each request logged with the
  query it carried and whether it was authorized;
- ``GET /v1/connectors/{id}/auth_url``, answered from the scenario;
- ``POST /v1/connectors-gateway/{id}/mcp``, a minimal streamable HTTP MCP
  server answering ``initialize`` and ``tools/call`` from the scenario, each
  JSON-RPC message logged.

What is recorded is what row 21 of ``docs/parity.md`` is about:

- ``catalog``: what the ``connector_catalog/*`` and ``connectors/*`` methods
  answer, with and without a session, over the bootstrap shapes the reference
  resolves, drops and caches, and the cache file they leave;
- ``publish``: which connector tools a session offers the model, under which
  names, descriptions and schemas, and what its runtime reports about them;
- ``call``: what a connector tool call sends the gateway and what the model
  reads back, on success and on each failure;
- ``lifecycle``: toggles, refreshes and authorization requests on a live
  session, including while a turn runs.

Nothing is imported from the reference: its own ``vibe-app-server`` is the
oracle, which is what makes the same scenarios replayable against this port by
``crates/vibe-app-server/tests/connectors_parity_tests.rs``.

Normalization maps identifiers, paths and times to placeholders and reduces
every string a server authored to its length and SHA-256, which is what
``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/connectors.py                  # capture the reference
    python3 scripts/parity/connectors.py --check          # recapture and compare
    python3 scripts/parity/connectors.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
    python3 scripts/parity/connectors.py --harness unified   # the Unified Harness family

The legacy family runs every session with ``--legacy-harness``, which is what
row 21 measures. ``--harness unified`` runs its own scenarios on the default
backend, where a connector is a ``connector_<alias>`` group a
``run_typescript`` program reaches, into ``unified-corpus.json`` beside the
legacy corpus: that is row 36's measure of the connector path.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time
import tomllib
from typing import Any
from urllib.parse import parse_qsl, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))

import app_server_client  # noqa: E402
import blackbox  # noqa: E402
from connectors_scenarios import API_KEY, scenarios, unified_scenarios  # noqa: E402
import loop_capture  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
from reference import OracleError, reference_command  # noqa: E402
import host_env  # noqa: E402
import stand_in  # noqa: E402
import subagent_client  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/connectors-parity/corpus.json"
#: Where ``--harness unified`` writes its family.
UNIFIED_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/connectors-parity/unified-corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5
#: The cache file both servers keep the account's bootstrap in.
CACHE_FILE = "connector_bootstrap_cache.json"

GATEWAY = re.compile(r"^/v1/connectors-gateway/(?P<id>[^/]+)/mcp$")
AUTH_URL = re.compile(r"^/v1/connectors/(?P<id>[^/]+)/auth_url$")


# --------------------------------------------------------------------------
# The stand-in, playing the model and the three connector endpoints
# --------------------------------------------------------------------------


class Backend(stand_in.Backend):
    """The chat-completions stand-in, also serving the connector endpoints a
    scenario scripts, and logging every request they receive."""

    def __init__(self, connectors: dict[str, Any]) -> None:
        super().__init__()
        self.bootstraps: list[Any] = list(connectors.get("bootstrap", []))
        self.auth_urls: dict[str, Any] = dict(connectors.get("authUrls", {}))
        self.gateway: dict[str, Any] = dict(connectors.get("gateway", {}))
        self.identity: Any = connectors.get("identity")
        self.bootstrap_log: list[dict[str, Any]] = []
        self.auth_log: list[dict[str, Any]] = []
        self.gateway_log: list[dict[str, Any]] = []
        self.served = 0

    def get(self, handler: stand_in.Handler, path: str) -> None:
        query = sorted(parse_qsl(urlsplit(handler.path).query))
        authorized = handler.headers.get("authorization") == f"Bearer {API_KEY}"
        if path == "/v1/connectors/bootstrap":
            with self.lock:
                self.bootstrap_log.append({"query": query, "authorized": authorized})
                answer = self.next_bootstrap()
            self.answer(handler, answer)
            return
        match = AUTH_URL.match(path)
        if match is not None:
            connector = match.group("id")
            with self.lock:
                self.auth_log.append({
                    "connector": connector,
                    "query": query,
                    "authorized": authorized,
                })
                answer = self.auth_urls.get(connector, {"status": 404, "body": {"detail": "no"}})
            self.answer(handler, answer)
            return
        if path == "/v1/users/me" and self.identity is not None:
            self.answer(handler, self.identity)
            return
        if GATEWAY.match(path):
            handler.send_response(405)
            handler.send_header("content-length", "0")
            handler.end_headers()
            return
        super().get(handler, path)

    def post(self, handler: stand_in.Handler, path: str, body: Any) -> None:
        match = GATEWAY.match(path)
        if match is None:
            # Telemetry and anything else the servers post: the scripted
            # completions are only the model's.
            super().post(handler, path, body)
            return
        self.gateway_exchange(handler, match.group("id"), body)

    def delete(self, handler: stand_in.Handler, path: str) -> None:
        handler.send_response(200)
        handler.send_header("content-length", "0")
        handler.end_headers()

    def next_bootstrap(self) -> Any:
        if not self.bootstraps:
            return {"status": 404, "body": {"detail": "no bootstrap"}}
        answer = self.bootstraps[min(self.served, len(self.bootstraps) - 1)]
        self.served += 1
        return answer

    @staticmethod
    def answer(handler: Any, answer: Any) -> None:
        """A scripted answer: `{"status", "body"}` sends that status, anything
        else is a 200 carrying it."""

        if isinstance(answer, dict) and "status" in answer and set(answer) <= {"status", "body", "raw"}:
            if "raw" in answer:
                payload = str(answer["raw"]).encode()
                handler.send_response(answer["status"])
                handler.send_header("content-type", "text/plain")
                handler.send_header("content-length", str(len(payload)))
                handler.end_headers()
                handler.wfile.write(payload)
                return
            handler.reply(answer["status"], answer.get("body", {}))
            return
        handler.reply(200, answer)

    def gateway_exchange(self, handler: Any, connector: str, message: Any) -> None:
        authorized = handler.headers.get("authorization") == f"Bearer {API_KEY}"
        method = message.get("method") if isinstance(message, dict) else None
        entry: dict[str, Any] = {"connector": connector, "method": method, "authorized": authorized}
        if method == "tools/call":
            entry["params"] = message.get("params")
        with self.lock:
            self.gateway_log.append(entry)
            failure = self.gateway.get(connector)
        if isinstance(failure, dict) and "status" in failure:
            self.answer(handler, failure)
            return
        if not isinstance(message, dict) or "id" not in message:
            # A notification or a response: accepted, nothing to answer.
            handler.send_response(202)
            handler.send_header("content-length", "0")
            handler.end_headers()
            return
        if method == "initialize":
            params = message.get("params") or {}
            result: Any = {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "oracle-gateway", "version": "0"},
            }
        elif method == "tools/call":
            params = message.get("params") or {}
            result = self.gateway.get(f"{connector}/{params.get('name')}")
            if result is None:
                result = {"content": [{"type": "text", "text": "no scripted result"}], "isError": True}
            if isinstance(result, dict) and "delay" in result:
                time.sleep(float(result["delay"]))
                result = {k: v for k, v in result.items() if k != "delay"}
        elif method == "tools/list":
            result = {"tools": []}
        else:
            result = {}
        handler.reply(200, {"jsonrpc": "2.0", "id": message["id"], "result": result})


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


def fingerprint(base: str) -> str:
    """Reference `connector_cache_fingerprint`: the key a provider's catalog
    is cached under."""

    return hashlib.sha256(f"{base.rstrip('/')}\0{API_KEY}".encode()).hexdigest()


def registry_key(base: str) -> str:
    """Reference `_bootstrap_cache_key`: the key the self-bootstrapping
    registry caches its payload under."""

    return hashlib.sha256(f"{base}\0{API_KEY}\0supports_mcp=true".encode()).hexdigest()


def seed_cache(world: blackbox.World, base: str, entries: dict[str, Any]) -> None:
    """Writes a cache file before the server starts. `$FP` and `$RK` name the
    two keys this provider's entries are filed under, and `$NOW` offsets are
    seconds from now."""

    now = int(time.time())

    def resolve(value: Any) -> Any:
        if isinstance(value, dict):
            return {resolve(k): resolve(v) for k, v in value.items()}
        if isinstance(value, list):
            return [resolve(v) for v in value]
        if isinstance(value, str):
            if value == "$FP":
                return fingerprint(base)
            if value == "$RK":
                return registry_key(base)
            match = re.fullmatch(r"\$NOW([+-]\d+)?", value)
            if match is not None:
                return now + int(match.group(1) or 0)
        return value

    (world.vibe_home / CACHE_FILE).write_text(json.dumps(resolve(entries)), encoding="utf-8", newline="")


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = Backend(scenario.get("connectors", {}))
    with host_env.scratch_root("vibe-connectors-oracle-") as root:
        session = app_server_client.Session(root)
        world = session.world
        try:
            backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
            base_config = scenario.get("base") or blackbox.base_config(backend)
            base_config = base_config.replace("$BACKEND", backend.base).replace(
                "enable_update_checks = false\n",
                "enable_update_checks = false\n" f'console_base_url = "{backend.base}"\n',
            )
            # The base keys lead and its tables close the file, so the scenario's
            # top-level keys and tables sit between them and neither side lands
            # inside a table of the other.
            split = base_config.index("\n[[")
            (world.vibe_home / "config.toml").write_text(
                base_config[:split] + "\n" + session.substitute(scenario.get("config", ""))
                + base_config[split:],
                encoding="utf-8",
                newline="",
            )
            (world.vibe_home / "trusted_folders.toml").write_text(
                f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n", encoding="utf-8", newline=""
            )
            if "cache" in scenario:
                seed_cache(world, backend.base, scenario["cache"])
            env = host_env.isolated(
                world.home,
                world.vibe_home,
                MISTRAL_API_KEY=API_KEY,
                VIBE_API_BASE=f"{backend.base}/v1/chat/completions",
            )
            for key, value in scenario.get("env", {}).items():
                if value is None:
                    env.pop(key, None)
                else:
                    env[key] = value
            server = subagent_client.Server(command, env, world.workspace, {}, world,
                                   callbacks=copy.deepcopy(scenario.get("callbacks", [])))
            steps: list[dict[str, Any]] = []
            try:
                run_steps(scenario, server, session, backend, steps, quiet)
            except OracleError as error:
                steps.append({"failure": str(error)})
            finally:
                server.stop()
            return {
                "steps": steps,
                "sessions": session.sessions,
                "requests": list(backend.bodies),
                "bootstraps": backend.bootstrap_log,
                "authUrls": backend.auth_log,
                "gateway": backend.gateway_log,
                "callbacks": server.callback_log,
                "fingerprint": fingerprint(backend.base),
                "registryKey": registry_key(backend.base),
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


def run_steps(
    scenario: dict[str, Any],
    server: app_server_client.Server,
    session: app_server_client.Session,
    backend: Backend,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    identifier = 1
    server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                 "params": {"clientInfo": {"name": "connectors-oracle", "version": "0"},
                            "capabilities": {"callbackKinds": ["approval", "user_input"]}}})
    server.collect(identifier, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    for step in scenario["steps"]:
        identifier += 1
        if "start" in step:
            options = step["start"]
            config = {"cwd": str(world.workspace), "agent": options.get("agent", "auto-approve")}
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start",
                         "params": {"agentConfig": config}})
            observed = server.collect(identifier, quiet)
            for message in observed:
                session.learn(message)
            response = next((m for m in observed if m.get("id") == identifier and "method" not in m), None)
            steps.append({
                "method": "session/start",
                "response": {"error": response["error"]} if response and "error" in response else {"started": response is not None},
                "notifications": [m for m in observed if "method" in m and "id" not in m],
            })
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": app_server_client.render_turn("reference", target, step["turn"])})
            if step.get("background"):
                time.sleep(step.get("settle", 0.5))
                observed = server.collect(None, quiet)
                for message in observed:
                    session.learn(message)
                steps.append({"turn": [m for m in observed if "method" in m and "id" not in m],
                              "background": True})
                continue
            observed = server.wait_for(app_server_client.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": [m for m in observed if "method" in m and "id" not in m]})
            continue
        if "await" in step:
            observed = server.wait_for(app_server_client.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": [m for m in observed if "method" in m and "id" not in m]})
            continue
        if "cacheFile" in step:
            path = world.vibe_home / CACHE_FILE
            steps.append({
                "cacheFile": json.loads(path.read_text(encoding="utf-8")) if path.is_file() else None,
                # What else the cache left beside itself: its lock, no temporary.
                "siblings": sorted(
                    entry.name for entry in world.vibe_home.iterdir()
                    if entry.name.startswith(f".{CACHE_FILE}")
                    and not entry.name.endswith(".tmp")
                ),
                "temporaries": sum(1 for entry in world.vibe_home.iterdir()
                                   if entry.name.startswith(f".{CACHE_FILE}") and entry.name.endswith(".tmp")),
            })
            continue
        if "configFile" in step:
            path = world.vibe_home / "config.toml"
            try:
                parsed = tomllib.loads(path.read_text(encoding="utf-8"))
            except (FileNotFoundError, tomllib.TOMLDecodeError):
                parsed = None
            steps.append({"configFile": {
                key: parsed.get(key) for key in ("connectors", "enabled_tools", "disabled_tools")
            } if isinstance(parsed, dict) else parsed})
            continue
        if "bootstrap" in step:
            # The account's catalog changes from here on.
            with backend.lock:
                backend.bootstraps = list(step["bootstrap"])
                backend.served = 0
            continue
        if "gateway" in step:
            with backend.lock:
                backend.gateway.update(step["gateway"])
            continue
        if "sleep" in step:
            time.sleep(float(step["sleep"]))
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            session.learn(message)
        response = next((m for m in observed if m.get("id") == identifier and "method" not in m), None)
        steps.append({
            "method": request["method"],
            "pick": step.get("pick"),
            "response": {k: v for k, v in (response or {"missing": True}).items() if k != "jsonrpc"},
            "notifications": [m for m in observed if "method" in m and "id" not in m],
        })


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: Notification fields that count or date events, which every other part of
#: the server moves.
DROPPED = {"emittedAt", "eventId"}


def connector_view(runtime: Any) -> Any:
    """The part of a runtime snapshot connectors decide: the MCP state (every
    source is a connector here, no server being configured), the counts and the
    connector tools the session lists."""

    if not isinstance(runtime, dict):
        return runtime
    return {
        "mcp": runtime.get("mcp"),
        "connectors": runtime.get("connectors"),
        "tools": [
            str(tool.get("name")) for tool in runtime.get("tools") or []
            if isinstance(tool, dict) and str(tool.get("name", "")).startswith("connector_")
        ],
    }


def reduce_result(method: str, result: Any, pick: list[str] | None) -> Any:
    if not isinstance(result, dict):
        return result
    if pick:
        result = {key: result.get(key) for key in pick}
    if isinstance(result.get("runtime"), dict):
        result = {**result, "runtime": connector_view(result["runtime"])}
    return result


def reduce_notification(message: dict[str, Any]) -> dict[str, Any] | None:
    method = message.get("method")
    params = {k: v for k, v in (message.get("params") or {}).items() if k not in DROPPED}
    if method == "runtime/updated":
        return {"method": method, "sessionId": params.get("sessionId"),
                "runtime": connector_view(params.get("runtime"))}
    if str(method).startswith("connector_catalog/"):
        return {"method": method, "params": params}
    if method in {"turn/completed", "turn/failed", "turn/interrupted"}:
        return {"method": method, "status": (params.get("turn") or {}).get("status")}
    if method in {"history/entryAdded", "history/entryUpdated"}:
        entry = params.get("entry") or {}
        if method == "history/entryAdded" and entry.get("type") != "effect":
            return None
        patch = [
            {**operation, "value": "<time>"}
            if str(operation.get("path", "")).rsplit("/", 1)[-1] in app_server_client.TIME_KEYS
            else operation
            for operation in params.get("patch") or []
            if isinstance(operation, dict)
        ]
        view = {"method": method}
        if entry:
            view["entry"] = entry
        if patch:
            view["patch"] = patch
            view["entryId"] = params.get("entryId")
        return view
    return None


def request_view(body: dict[str, Any]) -> dict[str, Any]:
    """What a model request carries that connectors decide: the connector
    tools it offers, as the model reads them, and the conversation."""

    tools = []
    for tool in body.get("tools") or []:
        function = tool.get("function") if isinstance(tool, dict) else None
        if not isinstance(function, dict):
            continue
        name = str(function.get("name", ""))
        if not name.startswith("connector_"):
            continue
        tools.append({
            "name": name,
            "description": function.get("description"),
            "parameters": function.get("parameters"),
        })
    return {
        "connectorTools": tools,
        "conversation": subagent_client.transcript(body.get("messages", [])),
    }


class Normalizer(subagent_client.Normalizer):
    def __init__(self, scenario: dict[str, Any], run: dict[str, Any]) -> None:
        super().__init__(scenario, run["paths"])
        self.keys = {run["fingerprint"]: "<fingerprint>", run["registryKey"]: "<registry-key>"}

    def text(self, value: str) -> str:
        for original, placeholder in self.keys.items():
            value = value.replace(original, placeholder)
        return super().text(value)

    def value(self, value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            return {self.keys.get(name, name): self.value(item, name) for name, item in value.items()}
        if isinstance(value, str) and value in self.keys:
            return self.keys[value]
        return super().value(value, key)


def cache_view(cache: Any, keys: dict[str, str]) -> Any:
    """A cache file with its timestamps reduced to whether they are fresh and
    its entries ordered by their normalized keys: the provider fingerprints
    derive from the scripted backend's random port, so the file's own key
    order is not stable from one run to the next."""

    if not isinstance(cache, dict):
        return cache
    view: dict[str, Any] = {}
    for key, entry in cache.items():
        if isinstance(entry, dict):
            entry = {k: ("<time>" if k in {"stored_at", "stored_at_timestamp"} and isinstance(v, int) else v)
                     for k, v in entry.items()}
        view[keys.get(key, key)] = entry
    return dict(sorted(view.items()))


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    normalizer = Normalizer(scenario, run)
    for identity in run["sessions"]:
        normalizer.identity(identity)
    steps: list[Any] = []
    for step in run["steps"]:
        if "failure" in step:
            steps.append({"failure": step["failure"]})
        elif "turn" in step:
            steps.append({"turn": [n for n in (reduce_notification(m) for m in step["turn"]) if n]})
        elif "cacheFile" in step:
            keys = {run["fingerprint"]: "<fingerprint>", run["registryKey"]: "<registry-key>"}
            steps.append({**step, "cacheFile": cache_view(step["cacheFile"], keys)})
        elif "configFile" in step:
            steps.append(step)
        else:
            response = copy.deepcopy(step["response"])
            if "result" in response:
                response["result"] = reduce_result(step["method"], response["result"], step.get("pick"))
            steps.append({
                "method": step["method"],
                "response": response,
                "notifications": [n for n in (reduce_notification(m) for m in step["notifications"]) if n],
            })
    requests = [
        request_view(body) for body in run["requests"]
        if isinstance(body, dict) and "messages" in body
    ]
    observed = {
        "steps": steps,
        "requests": requests,
        "bootstraps": run["bootstraps"],
        "authUrls": run["authUrls"],
        "gateway": run["gateway"],
        "callbacks": [(c.get("callback") or {}).get("detail") for c in run["callbacks"]],
    }
    normalizer.collect_ids(observed)
    return normalizer.value(observed)


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def unified_view(run: dict[str, Any]) -> dict[str, Any]:
    """A unified run as ``unified_harness.py`` reads one: patches by path with
    the clock's operations dropped, and a program's failure without the frames
    that name lines of each side's own sandbox."""

    for step in run["steps"]:
        for key in ("turn", "notifications"):
            if isinstance(step.get(key), list):
                step[key] = loop_capture.reduce_patches(step[key])
    for body in run["requests"]:
        for message in (body.get("messages") or []) if isinstance(body, dict) else []:
            if (message.get("role") == "tool" and message.get("name") == "run_typescript"
                    and isinstance(message.get("content"), str)):
                message["content"] = loop_capture.program_failure_view(message["content"])
            elif (message.get("role") == "tool" and message.get("name") == "search_tool_functions"
                    and isinstance(message.get("content"), str)):
                message["content"] = loop_capture.search_view(message["content"])
    return run


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--server", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--harness", choices=("legacy", "unified"), default="legacy",
                        help="the backend the sessions run on, each with its own scenarios")
    parser.add_argument("--only", action="append", default=[], help="scenario name filter")
    parser.add_argument("--raw", action="store_true", help="also keep the raw answers")
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--jobs", type=int, default=4, help="scenarios run side by side")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    unified = arguments.harness == "unified"
    if arguments.output is None:
        arguments.output = UNIFIED_OUTPUT if unified else DEFAULT_OUTPUT
    flags = () if unified else HARNESS_FLAGS
    try:
        if arguments.server is not None:
            command = [str(arguments.server.resolve()), *flags]
            reference = {"commit": "server-override"}
        else:
            commit, binary = reference_command(
                arguments.reference, "vibe-app-server", arguments.expected_commit
            )
            reference = {"commit": commit}
            command = [*binary, *flags]
        selected = [
            scenario
            for scenario in (unified_scenarios() if unified else scenarios())
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started = time.monotonic()
            run = run_scenario(scenario, command, arguments.quiet)
            if unified:
                run = unified_view(run)
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
            # A capture holds tuples where the corpus holds lists, so both
            # sides are compared as JSON.
            differing = [
                entry["name"]
                for entry in captured
                if by_name.get(entry["name"], {}).get("observed")
                != json.loads(json.dumps(entry["observed"]))
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
        print(f"connectors oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
