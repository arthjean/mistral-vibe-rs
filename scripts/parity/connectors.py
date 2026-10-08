#!/usr/bin/env python3
"""Black-box capture of connectors: the account catalog, its selection, and the
tools a session publishes and calls through it.

Every scenario serves an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in the hooks oracle already runs. The same
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
import shutil
import sys
import tempfile
import threading
import time
import tomllib
from typing import Any
from urllib.parse import parse_qsl, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
import agents  # noqa: E402
import hooks  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
import rewind  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/connectors-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5
API_KEY = "oracle-key"
#: The cache file both servers keep the account's bootstrap in.
CACHE_FILE = "connector_bootstrap_cache.json"

GATEWAY = re.compile(r"^/v1/connectors-gateway/(?P<id>[^/]+)/mcp$")
AUTH_URL = re.compile(r"^/v1/connectors/(?P<id>[^/]+)/auth_url$")


# --------------------------------------------------------------------------
# The stand-in, playing the model and the three connector endpoints
# --------------------------------------------------------------------------


class Backend(hooks.Backend):
    """The hooks stand-in, also serving the connector endpoints a scenario
    scripts, and logging every request they receive."""

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
        handler = self.server.RequestHandlerClass
        backend = self
        chat = handler.do_POST
        fallback = handler.do_GET

        def do_get(self: Any) -> None:
            parts = urlsplit(self.path)
            authorized = self.headers.get("authorization") == f"Bearer {API_KEY}"
            if parts.path == "/v1/connectors/bootstrap":
                with backend.lock:
                    backend.bootstrap_log.append({
                        "query": sorted(parse_qsl(parts.query)),
                        "authorized": authorized,
                    })
                    answer = backend.next_bootstrap()
                backend.answer(self, answer)
                return
            match = AUTH_URL.match(parts.path)
            if match is not None:
                connector = match.group("id")
                with backend.lock:
                    backend.auth_log.append({
                        "connector": connector,
                        "query": sorted(parse_qsl(parts.query)),
                        "authorized": authorized,
                    })
                    answer = backend.auth_urls.get(connector, {"status": 404, "body": {"detail": "no"}})
                backend.answer(self, answer)
                return
            if parts.path.rstrip("/") == "/v1/users/me" and backend.identity is not None:
                backend.answer(self, backend.identity)
                return
            if GATEWAY.match(parts.path):
                self.send_response(405)
                self.send_header("content-length", "0")
                self.end_headers()
                return
            fallback(self)

        def do_post(self: Any) -> None:
            path = urlsplit(self.path).path
            match = GATEWAY.match(path)
            if match is None:
                if not path.rstrip("/").endswith("/chat/completions"):
                    # Telemetry and anything else the servers post: the
                    # scripted completions are only the model's.
                    length = int(self.headers.get("content-length") or 0)
                    if length:
                        self.rfile.read(length)
                    self.reply(404, {"error": "not found"})
                    return
                chat(self)
                return
            length = int(self.headers.get("content-length") or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                message = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                message = {}
            backend.gateway_exchange(self, match.group("id"), message)

        def do_delete(self: Any) -> None:
            self.send_response(200)
            self.send_header("content-length", "0")
            self.end_headers()

        handler.do_GET = do_get
        handler.do_POST = do_post
        handler.do_DELETE = do_delete

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


def seed_cache(world: acp.World, base: str, entries: dict[str, Any]) -> None:
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

    (world.vibe_home / CACHE_FILE).write_text(json.dumps(resolve(entries)), encoding="utf-8")


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = Backend(scenario.get("connectors", {}))
    root = Path(tempfile.mkdtemp(prefix="vibe-connectors-oracle-"))
    session = rewind.Session(root)
    world = session.world
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        base_config = scenario.get("base") or acp.base_config(backend)
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
        )
        (world.vibe_home / "trusted_folders.toml").write_text(
            f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n", encoding="utf-8"
        )
        if "cache" in scenario:
            seed_cache(world, backend.base, scenario["cache"])
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(world.home),
            "VIBE_HOME": str(world.vibe_home),
            "MISTRAL_API_KEY": API_KEY,
            "VIBE_API_BASE": f"{backend.base}/v1/chat/completions",
            "LANG": "C.UTF-8",
            "TERM": "dumb",
            "NO_COLOR": "1",
            "CI": "true",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
        }
        for key, value in scenario.get("env", {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        server = agents.Server(command, env, world.workspace, {}, world,
                               callbacks=copy.deepcopy(scenario.get("callbacks", [])))
        steps: list[dict[str, Any]] = []
        try:
            run_steps(scenario, server, session, backend, steps, quiet)
        except (rewind.OracleError, acp.OracleError) as error:
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
        shutil.rmtree(root, ignore_errors=True)


def run_steps(
    scenario: dict[str, Any],
    server: rewind.Server,
    session: rewind.Session,
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
                         "params": rewind.render_turn("reference", target, step["turn"])})
            if step.get("background"):
                time.sleep(step.get("settle", 0.5))
                observed = server.collect(None, quiet)
                for message in observed:
                    session.learn(message)
                steps.append({"turn": [m for m in observed if "method" in m and "id" not in m],
                              "background": True})
                continue
            observed = server.wait_for(rewind.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": [m for m in observed if "method" in m and "id" not in m]})
            continue
        if "await" in step:
            observed = server.wait_for(rewind.settles({}))
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
            if str(operation.get("path", "")).rsplit("/", 1)[-1] in rewind.TIME_KEYS
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
        "conversation": hooks.transcript(body.get("messages", [])),
    }


class Normalizer(hooks.Normalizer):
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
# Scenarios
# --------------------------------------------------------------------------


def send(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    step: dict[str, Any] = {"send": {"method": method, "params": params}}
    if pick:
        step["pick"] = pick
    return step


def on(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    """A method addressed to the first session."""

    return send(method, pick, sessionId="$S1", **params)


def runtime() -> dict[str, Any]:
    return on("runtime/read")


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def tool(name: str, description: str | None = None, schema: Any = None) -> dict[str, Any]:
    entry: dict[str, Any] = {"name": name}
    if description is not None:
        entry["description"] = description
    if schema is not None:
        entry["inputSchema"] = schema
    return entry


SEARCH_SCHEMA = {
    "type": "object",
    "properties": {"query": {"type": "string", "description": "What to look for"}},
    "required": ["query"],
}

DRIVE = {
    "id": "c-drive", "name": "Drive", "display_name": "Google Drive", "protocol": "mcp",
    "status": {"is_ready": True},
    "tools": [tool("search", "Search files", SEARCH_SCHEMA), tool("list")],
}
MAIL = {
    "id": "c-mail", "name": "Mail box", "protocol": "mcp", "status": {"is_ready": False},
    "auth_action": {"type": "oauth"}, "tools": [tool("send", "Send a mail")],
}
SHEETS = {
    "id": "c-sheets", "name": "Sheets", "protocol": "mcp", "status": {"is_ready": False},
    "auth_action": {"type": "credentials_setup"},
    "bootstrap_errors": ["missing_credentials: set up the sheet access"],
    "tools": [tool("read", "Read a sheet")],
}
DOCS = {
    "id": "c-docs", "name": "Docs", "protocol": "mcp", "status": {"is_ready": True},
    "tools": [tool("fetch", "Fetch a document", SEARCH_SCHEMA)],
}
WEB = {
    "id": "web_search", "name": "web_search", "display_name": "Web search", "protocol": "mcp",
    "status": {"is_ready": True}, "tools": [tool("web_search", "Search the web", SEARCH_SCHEMA)],
}
CATALOG = {"connectors": [DRIVE, MAIL]}
WIDE = {"connectors": [DRIVE, MAIL, SHEETS, DOCS, WEB]}

#: The configuration that opts the two catalog connectors in.
OPT_IN = '[[connectors]]\nname = "Drive"\n\n[[connectors]]\nname = "Mail_box"\n'
#: Every connector of the wide catalog opted in.
OPT_IN_WIDE = OPT_IN + (
    '\n[[connectors]]\nname = "Sheets"\n\n[[connectors]]\nname = "Docs"\n'
    '\n[[connectors]]\nname = "web_search"\n'
)

#: Every shape the reference resolves, isolates or drops, in one payload.
SHAPES = {"connectors": [
    DRIVE,
    WEB,
    # Two names normalizing to one alias: the second by id takes a suffix.
    {"id": "c-my-2", "name": "My_Drive", "status": {"is_ready": True}, "tools": [tool("a")]},
    {"id": "c-my-1", "name": "My Drive", "status": {"is_ready": True}, "tools": [tool("b")]},
    # A name that normalizes to nothing, and one with no name at all.
    {"id": "c-bang", "name": "!!!", "status": {"is_ready": True}},
    {"id": "c-nameless", "display_name": "Nameless", "status": {"is_ready": True}},
    {"id": "c-accents", "name": "Été / Calendar!", "status": {"is_ready": True}},
    # No id, a blank id, and an id two connectors share: all dropped.
    {"name": "Orphan", "status": {"is_ready": True}},
    {"id": "   ", "name": "Blank", "status": {"is_ready": True}},
    {"id": "c-dup", "name": "Dup one"},
    {"id": "c-dup", "name": "Dup two"},
    # Every auth action, an unknown one, and a status that is missing.
    {"id": "c-oauth", "name": "OAuth", "status": {"is_ready": False}, "auth_action": {"type": "oauth"}},
    {"id": "c-setup", "name": "Setup", "status": {"is_ready": False},
     "auth_action": {"type": "credentials_setup"}},
    {"id": "c-weird", "name": "Weird", "status": {"is_ready": False}, "auth_action": {"type": "magic"}},
    {"id": "c-ready-weird", "name": "Ready weird", "status": {"is_ready": True},
     "auth_action": {"type": "magic"}},
    {"id": "c-nostatus", "name": "No status"},
    # Diagnostics: codes, prose, non-strings, blanks and more than three.
    {"id": "c-diag", "name": "Diag", "status": {"is_ready": True},
     "bootstrap_errors": ["token_expired: gone", "Plain words here", 42, "   ", "rate_limited",
                          "Connector bootstrap issue: quota", "Connector failed to bootstrap."]},
    {"id": "c-diag-text", "name": "Diag text", "status": {"is_ready": True},
     "bootstrap_errors": "upstream_down: try later"},
    # Tools: unnamed, blank, malformed, oversized, duplicated.
    {"id": "c-tools", "name": "Tools", "status": {"is_ready": True}, "tools": [
        tool("zeta", "Last"), tool("alpha"), {"description": "no name"}, tool("   "),
        tool("big", "Too big", {"type": "object", "properties": {"x": {"description": "y" * 70000}}}),
        "not a tool", tool("nulls", None, None),
    ]},
    {"id": "c-dupe-tools", "name": "Dupe tools", "status": {"is_ready": True},
     "tools": [tool("same"), tool("same")]},
    {"id": "c-bad-tools", "name": "Bad tools", "tools": "nope"},
    {"id": "c-many", "name": "Many", "status": {"is_ready": True},
     "tools": [tool(f"t{index:03}") for index in range(129)]},
    "not a connector",
]}

#: The configuration of a session whose model runs on a provider that is not
#: Mistral's, while the Mistral provider has no key to reach connectors with.
NO_MISTRAL_KEY_BASE = (
    'active_model = "standin"\n'
    "enable_telemetry = false\n"
    "enable_update_checks = false\n"
    "\n[[models]]\n"
    'name = "standin-model"\n'
    'provider = "standin"\n'
    'alias = "standin"\n'
    "\n[[providers]]\n"
    'name = "mistral"\n'
    'api_base = "$BACKEND/v1"\n'
    'api_key_env_var = "ORACLE_MISSING_KEY"\n'
    'backend = "mistral"\n'
    "\n[[providers]]\n"
    'name = "standin"\n'
    'api_base = "$BACKEND/v1"\n'
    'api_key_env_var = "ORACLE_STANDIN_KEY"\n'
    'backend = "generic"\n'
)

IDENTITY = {
    "id": "user-1", "email": "user@example.com",
    "workspace": {"id": "ws-1", "name": "Workspace"},
    "organization": {"id": "org-1", "name": "Org"},
}


def fresh_entry(payload: dict[str, Any], age: int = 60) -> dict[str, Any]:
    return {"format": 2, "stored_at": f"$NOW-{age}", "payload": payload}


def search(identifier: str, name: str = "connector_Drive_search") -> dict[str, Any]:
    return {"toolCalls": [call(name, {"query": "report"}, identifier)]}


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- catalog ---------------------------------------------------------
        {
            "name": "catalog/sessionless-refresh",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                send("connector_catalog/read"),
                send("connector_catalog/refresh"),
                send("connector_catalog/read"),
                {"cacheFile": True},
            ],
        },
        {
            "name": "catalog/shapes",
            "connectors": {"bootstrap": [SHAPES]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "catalog/selections",
            "config": OPT_IN + '\n[[connectors]]\nname = "Ghost"\ndisabled = true\ndisabled_tools = ["x"]\n',
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"),
                      send("connector_catalog/read")],
        },
        {
            "name": "catalog/empty",
            "connectors": {"bootstrap": [{"connectors": []}, {"connectors": None}, {}]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/read"),
                      send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "catalog/malformed",
            "connectors": {"bootstrap": [[1, 2], {"connectors": "nope"}, "text", {"raw": "not json", "status": 200}]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "catalog/http-errors",
            "connectors": {"bootstrap": [
                {"status": 500, "body": {"detail": "boom"}},
                {"status": 401, "body": {"detail": "who"}},
                {"status": 404, "body": {"detail": "none"}},
                CATALOG,
            ]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/refresh"), send("connector_catalog/read"),
                      send("connector_catalog/refresh"), send("connector_catalog/read")],
        },
        {
            "name": "catalog/disabled",
            "config": "enable_connectors = false\n" + OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"),
                      {"start": {}}, on("connector_catalog/read"), on("connectors/read"),
                      runtime(), {"turn": "Hello"}],
        },
        {
            "name": "catalog/no-mistral-key",
            "base": NO_MISTRAL_KEY_BASE,
            "env": {"ORACLE_STANDIN_KEY": API_KEY, "MISTRAL_API_KEY": None,
                    # The port's fixture authenticates its turns with this
                    # variable; the reference never reads it.
                    "VIBE_ORACLE_CREDENTIAL": "ORACLE_STANDIN_KEY"},
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"),
                      {"start": {}}, on("connector_catalog/read"), on("connector_catalog/refresh"),
                      on("connectors/read"), runtime()],
        },
        {
            "name": "catalog/manage-url",
            "connectors": {"bootstrap": [CATALOG], "identity": IDENTITY},
            "steps": [send("connector_catalog/read"), {"start": {}}, on("connector_catalog/read")],
        },
        {
            "name": "catalog/manage-url-partial",
            "connectors": {"bootstrap": [CATALOG], "identity": {"id": "user-1"}},
            "steps": [send("connector_catalog/read")],
        },
        # -- cache -----------------------------------------------------------
        {
            "name": "cache/fresh",
            "cache": {"$FP": fresh_entry(CATALOG)},
            "connectors": {"bootstrap": [{"connectors": [DOCS]}]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/read"), {"start": {}},
                      on("connector_catalog/read"), send("connector_catalog/refresh"),
                      send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "cache/legacy-entry",
            "cache": {"$FP": {"stored_at_timestamp": "$NOW-30", "payload": CATALOG}},
            "connectors": {"bootstrap": [{"connectors": [DOCS]}]},
            "steps": [send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "cache/stale-and-future",
            "cache": {
                "$FP": fresh_entry(CATALOG, age=601),
                "other-stale": fresh_entry(CATALOG, age=900),
                "other-future": {"format": 2, "stored_at": "$NOW+600", "payload": CATALOG},
                "other-fresh": fresh_entry({"connectors": [DOCS]}),
                "other-legacy": {"stored_at_timestamp": "$NOW-10", "payload": {"connectors": [DOCS]}},
                "other-format": {"format": 3, "stored_at": "$NOW-10", "payload": CATALOG},
                "$RK": {"stored_at_timestamp": "$NOW-10", "payload": CATALOG},
            },
            "connectors": {"bootstrap": [{"connectors": [DOCS]}]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"), {"cacheFile": True}],
        },
        {
            "name": "cache/diagnostics-roundtrip",
            "cache": {"$FP": fresh_entry({"connectors": [
                {**DOCS, "diagnostics": ["quota_exceeded: slow down"]},
                {**DRIVE, "bootstrap_errors": ["token_expired: gone"]},
            ]})},
            "steps": [send("connector_catalog/read")],
        },
        {
            "name": "cache/corrupt",
            "cache": {"$FP": {"format": 2, "stored_at": "$NOW-10", "payload": {"connectors": "nope"}}},
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), {"start": {}}, runtime(), {"cacheFile": True}],
        },
        # -- publish ---------------------------------------------------------
        {
            "name": "publish/opt-in",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connector_catalog/read"),
                      on("connectors/read"), {"cacheFile": True}],
        },
        {
            "name": "publish/no-entries",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read")],
        },
        {
            "name": "publish/wide",
            "config": OPT_IN_WIDE,
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read"),
                      on("connector_catalog/read")],
        },
        {
            "name": "publish/entry-filters",
            "config": (
                '[[connectors]]\nname = "Drive"\ndisabled_tools = ["list"]\n'
                '\n[[connectors]]\nname = "Docs"\ndisabled = true\n'
                '\n[[connectors]]\nname = "web_search"\ndisabled_tools = ["web_search", "unknown"]\n'
                '\n[[connectors]]\nname = "Absent"\n'
            ),
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read"),
                      on("connector_catalog/read")],
        },
        {
            "name": "publish/global-filters",
            "config": 'disabled_tools = ["connector_Drive_l*"]\n' + OPT_IN_WIDE,
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connector_catalog/read")],
        },
        {
            "name": "publish/enabled-tools",
            "config": 'enabled_tools = ["connector_Docs_*", "grep"]\n' + OPT_IN_WIDE,
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connector_catalog/read")],
        },
        {
            "name": "publish/bootstrap-failure",
            "config": OPT_IN,
            "connectors": {"bootstrap": [{"status": 503, "body": {"detail": "down"}}]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, runtime(),
                      on("connector_catalog/read"), on("connectors/read")],
        },
        {
            "name": "publish/bootstrap-recovers",
            "config": OPT_IN,
            "connectors": {"bootstrap": [{"status": 500, "body": {}}, CATALOG]},
            "steps": [{"start": {}}, on("connector_catalog/refresh"), runtime(), {"turn": "Hello"}],
        },
        {
            "name": "publish/shapes",
            "config": "".join(
                f'[[connectors]]\nname = "{alias}"\n\n'
                for alias in ("Drive", "web_search", "My_Drive", "My_Drive_2", "unnamed",
                              "c-nameless", "t____Calendar", "Ready_weird", "Diag", "Diag_text",
                              "Tools", "OAuth", "Setup", "Weird", "No_status")
            ),
            "connectors": {"bootstrap": [SHAPES]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read")],
        },
        # -- call ------------------------------------------------------------
        {
            "name": "call/success",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "found report.pdf"}]}},
            },
            "backend": [search("call_c1"), {"text": "Found it."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
        {
            "name": "call/tool-error",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "quota exceeded"}],
                                               "isError": True}},
            },
            "backend": [search("call_c1"), {"text": "It failed."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/structured",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {
                    "content": [{"type": "text", "text": "two files"},
                                {"type": "text", "text": "and more"}],
                    "structuredContent": {"files": ["a.pdf", "b.pdf"]},
                }},
            },
            "backend": [search("call_c1"), {"text": "Two files."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/gateway-401",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 401, "body": {"detail": "bad key"}}}},
            "backend": [search("call_c1"), {"text": "Denied."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
        {
            "name": "call/gateway-404",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 404, "body": {"detail": "gone"}}}},
            "backend": [search("call_c1"), {"text": "Gone."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/gateway-500",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 500, "raw": "upstream exploded"}}},
            "backend": [search("call_c1"), {"text": "Broken."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/ask-approval",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "found report.pdf"}]}},
            },
            "backend": [search("call_c1"), {"text": "Found it."}],
            "callbacks": [{"approve": True}],
            "steps": [{"start": {"agent": "ask"}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/disabled-tool",
            "config": '[[connectors]]\nname = "Drive"\ndisabled_tools = ["search"]\n',
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [search("call_c1"), {"text": "Could not."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/not-ready",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [{"toolCalls": [call("connector_Mail_box_send", {}, "call_m1")]}, {"text": "No."}],
            "steps": [{"start": {}}, {"turn": "Send a mail"}],
        },
        # -- lifecycle -------------------------------------------------------
        {
            "name": "lifecycle/toggle-session",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="list"),
                {"configFile": True},
                {"turn": "Hello"},
                on("connector_catalog/toggle", alias="Drive", disabled=True),
                on("connectors/read"),
                {"turn": "Hello again"},
                on("connector_catalog/toggle", alias="Drive", disabled=False),
                on("connector_catalog/toggle", alias="Drive", disabled=False, toolName="list"),
                {"configFile": True},
                on("connector_catalog/read"),
                {"turn": "And again"},
            ],
        },
        {
            "name": "lifecycle/toggle-new-entry",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                on("connector_catalog/toggle", alias="Drive", disabled=False),
                {"configFile": True},
                runtime(),
                {"turn": "Hello"},
                on("connector_catalog/toggle", alias="Mail_box", disabled=True, toolName="send"),
                {"configFile": True},
            ],
        },
        {
            "name": "lifecycle/toggle-refusals",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                send("connector_catalog/toggle", alias="Drive", disabled=True),
                {"configFile": True},
                {"start": {}},
                on("connector_catalog/toggle", alias="Nope", disabled=True),
                on("connector_catalog/toggle", alias="Bad name", disabled=True),
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="   "),
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="x" * 257),
                send("connector_catalog/toggle", alias="Drive", disabled=False),
                send("connector_catalog/toggle", alias="Other", disabled=True),
                send("connector_catalog/toggle", alias="Drive", disabled=False, toolName="list"),
                {"configFile": True},
                send("connector_catalog/toggle", sessionId="nobody", alias="Drive", disabled=True),
            ],
        },
        {
            "name": "lifecycle/stale-session-catalog",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                {"bootstrap": [{"connectors": [DRIVE, MAIL, DOCS]}]},
                send("connector_catalog/refresh"),
                on("connector_catalog/toggle", alias="Drive", disabled=True),
                on("connector_catalog/read"),
                on("connector_catalog/refresh"),
                on("connector_catalog/toggle", alias="Drive", disabled=True),
            ],
        },
        {
            "name": "lifecycle/refresh-changed-catalog",
            "config": OPT_IN_WIDE,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                {"turn": "Hello"},
                {"bootstrap": [{"connectors": [
                    {**DRIVE, "tools": [tool("search", "Search everything", SEARCH_SCHEMA)]},
                    {**MAIL, "status": {"is_ready": True}},
                    DOCS,
                ]}]},
                on("connector_catalog/refresh"),
                {"turn": "Hello again"},
                on("connectors/refresh", name="Mail_box"),
                on("connectors/refresh", name="Nope"),
                {"bootstrap": [{"status": 500, "body": {}}]},
                on("connector_catalog/refresh"),
                on("connectors/refresh", name="Drive"),
                runtime(),
            ],
        },
        {
            "name": "lifecycle/toggle-during-turn",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [{"text": "Thinking slowly.", "delay": 2.5}, {"text": "Done."}],
            "steps": [
                {"start": {}},
                {"turn": "Take your time", "background": True, "settle": 0.8},
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="list"),
                on("connector_catalog/refresh"),
                {"configFile": True},
                {"await": True},
                on("connector_catalog/read"),
                {"turn": "Quick one"},
            ],
        },
        {
            "name": "lifecycle/auth",
            "config": OPT_IN_WIDE,
            "connectors": {
                "bootstrap": [WIDE],
                "authUrls": {"c-mail": {"auth_url": "https://auth.example/mail", "ttl": 600},
                             "c-sheets": {"auth_url": "https://auth.example/sheets", "ttl": 60}},
            },
            "steps": [
                {"start": {}},
                on("connector_catalog/auth/request", alias="Mail_box"),
                {"sleep": 0.5},
                on("connector_catalog/auth/request", alias="Sheets"),
                {"sleep": 0.5},
                on("connector_catalog/auth/request", alias="Drive"),
                on("connector_catalog/auth/request", alias="Nope"),
                on("connector_catalog/auth/request", alias="Bad name"),
                send("connector_catalog/auth/request", sessionId="nobody", alias="Mail_box"),
                on("connectors/auth/read", name="Mail_box"),
                on("connectors/auth/read", name="Sheets"),
                on("connectors/auth/read", name="Drive"),
                on("connectors/auth/read", name="Nope"),
            ],
        },
        {
            # The SDK answers no page for a failing status or for a body its
            # response model refuses, here one without its `ttl`.
            "name": "lifecycle/auth-failures",
            "config": OPT_IN_WIDE,
            "connectors": {
                "bootstrap": [WIDE],
                "authUrls": {"c-mail": {"status": 500, "body": {"detail": "nope"}},
                             "c-sheets": {"auth_url": "https://auth.example/sheets"}},
            },
            "steps": [
                {"start": {}},
                on("connector_catalog/auth/request", alias="Mail_box"),
                {"sleep": 0.5},
                on("connector_catalog/auth/request", alias="Sheets"),
                {"sleep": 0.5},
                on("connectors/auth/read", name="Mail_box"),
                on("connectors/auth/read", name="Sheets"),
            ],
        },
        {
            "name": "lifecycle/auth-disabled-source",
            "config": '[[connectors]]\nname = "Mail_box"\ndisabled = true\n',
            "connectors": {"bootstrap": [CATALOG],
                           "authUrls": {"c-mail": {"auth_url": "https://auth.example/mail", "ttl": 600}}},
            "steps": [{"start": {}}, on("connector_catalog/auth/request", alias="Mail_box"),
                      on("connectors/auth/read", name="Mail_box")],
        },
        {
            "name": "lifecycle/sessionless-methods",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                send("connectors/read", sessionId="nobody"),
                send("connectors/refresh", sessionId="nobody", name="Drive"),
                send("connectors/auth/read", sessionId="nobody", name="Drive"),
                send("connector_catalog/read", sessionId="nobody"),
                send("connector_catalog/refresh", sessionId="nobody"),
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
            command = [str(arguments.server.resolve()), *HARNESS_FLAGS]
            reference = {"commit": "server-override"}
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise rewind.OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary), *HARNESS_FLAGS]
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
            # A capture holds tuples where the corpus holds lists, so both
            # sides are compared as JSON.
            differing = [
                entry["name"]
                for entry in captured
                if by_name.get(entry["name"], {}).get("observed")
                != json.loads(json.dumps(entry["observed"]))
            ]
            if differing:
                raise rewind.OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except (rewind.OracleError, acp.OracleError) as error:
        print(f"connectors oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
