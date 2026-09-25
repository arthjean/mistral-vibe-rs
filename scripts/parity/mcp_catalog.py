#!/usr/bin/env python3
"""Black-box capture of the MCP catalog methods served by the app server.

Row 11 of ``docs/parity.md`` is MCP. This capture serves the reference's own
``vibe-app-server`` over stdio in a fresh home and drives its catalog: the
``mcp_catalog/*`` methods, their ``mcp/*`` aliases, the sessionless mutations
that edit the user configuration before any session exists, and the
notifications a sign-in raises (``mcp_catalog/authUrl`` and ``mcp/authUrl``)
or a source that needs one does (``mcp_catalog/authRequired``). The MCP
servers are ``vibe-oauth-script-fixture`` processes, which play both an MCP
endpoint and its authorization server; a simulated browser answers every
authorization URL by requesting the loopback callback. The keyring is a file
the capture installs as the reference's keyring backend, and which this port's
``vibe-app-server-stdio-fixture`` reads through ``VIBE_ORACLE_KEYRING``.

What each step records: the answer (a runtime reduced to its ``mcp`` state),
the notifications it raised (the MCP ones with their parameters, a
``runtime/updated`` with its ``mcp`` state), and on demand the ``mcp_servers``
the configuration files hold and the keyring entries. Identifiers, ports and
times are placeholders, and every string the server authored is a length and
a SHA-256, which is what ``NOTICE`` requires of a committed corpus.

``crates/vibe-app-server/tests/mcp_catalog_parity_tests.rs`` runs the same
scenarios against this port with ``--server`` and compares.

Usage::

    python3 scripts/parity/mcp_catalog.py                 # capture the reference
    python3 scripts/parity/mcp_catalog.py --check         # recapture and compare
    python3 scripts/parity/mcp_catalog.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from typing import Any
from urllib.parse import parse_qsl, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/mcp-catalog/corpus.json"
SCHEMA_VERSION = 1
QUIET_SECONDS = 1.0
RESPONSE_TIMEOUT = 40.0
SERVICE = "ai.mistral.vibe"
KEYRING_VARIABLE = "VIBE_ORACLE_KEYRING"

#: Keys whose values are wall-clock readings.
TIME_KEYS = {"createdAt", "updatedAt", "emittedAt", "startedAt", "completedAt"}
DESCRIPTOR = re.compile(r"mcp-auth-descriptor:([0-9a-f]+):(\d+)")
UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
BASE = re.compile(r"http://127\.0\.0\.1:(\d+)")

#: The keyring backend the reference loads through ``PYTHON_KEYRING_BACKEND``.
KEYRING_MODULE = '''
import json
import os

import keyring.backend
import keyring.errors


class FileKeyring(keyring.backend.KeyringBackend):
    priority = 1

    def _load(self):
        path = os.environ["VIBE_ORACLE_KEYRING"]
        try:
            with open(path, encoding="utf-8") as handle:
                return json.load(handle)
        except FileNotFoundError:
            return {}

    def _store(self, entries):
        with open(os.environ["VIBE_ORACLE_KEYRING"], "w", encoding="utf-8") as handle:
            json.dump(entries, handle, sort_keys=True)

    def get_password(self, service, username):
        return self._load().get(service, {}).get(username)

    def set_password(self, service, username, password):
        entries = self._load()
        entries.setdefault(service, {})[username] = password
        self._store(entries)

    def delete_password(self, service, username):
        entries = self._load()
        if username not in entries.get(service, {}):
            raise keyring.errors.PasswordDeleteError(username)
        del entries[service][username]
        self._store(entries)
'''


class OracleError(RuntimeError):
    pass


def digest(text: str) -> dict[str, Any]:
    return {"prose": len(text), "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()}


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


# --------------------------------------------------------------------------
# Scripted MCP servers
# --------------------------------------------------------------------------

TOOLS = [
    {"name": "search", "description": "Search the docs.\nMatches titles and bodies.", "inputSchema": {"type": "object"}},
    {"name": "fetch", "description": "Fetch one page", "inputSchema": {"type": "object"}},
]


def oauth_routes() -> dict[str, Any]:
    return {
        "GET /.well-known/oauth-protected-resource/mcp": [
            {"status": 200, "json": {"resource": "{base}/mcp", "authorization_servers": ["{base}"]}}
        ],
        "GET /.well-known/oauth-authorization-server": [
            {"status": 200, "json": {
                "issuer": "{base}",
                "authorization_endpoint": "{base}/authorize",
                "token_endpoint": "{base}/token",
                "registration_endpoint": "{base}/register",
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
            }}
        ],
        "POST /register": [
            {"status": 201, "json": {"client_id": "client-1", "redirect_uris": ["http://127.0.0.1:{redirect}/callback"], "token_endpoint_auth_method": "none"}}
        ],
        "POST /token": [
            {"status": 200, "json": {"access_token": "at-1", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "rt-1"}}
        ],
    }


OPEN = {"mcp": {"path": "/mcp", "tools": TOOLS}}
SECURE = {"mcp": {"path": "/mcp", "tools": TOOLS, "token": "at-1"}, "routes": oauth_routes()}


class Fixture:
    """One scripted server process of a scenario."""

    def __init__(self, binary: Path, root: Path, name: str, script: dict[str, Any], redirect: int) -> None:
        directory = root / f"fixture-{name}"
        directory.mkdir()
        rendered = json.dumps(script).replace("{redirect}", str(redirect))
        (directory / "script.json").write_text(rendered, encoding="utf-8")
        self.log = directory / "log.jsonl"
        port_file = directory / "port"
        environment = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "VIBE_OAUTH_SCRIPT": str(directory / "script.json"),
            "VIBE_OAUTH_LOG": str(self.log),
            "VIBE_OAUTH_PORT_FILE": str(port_file),
        }
        self.process = subprocess.Popen([str(binary)], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        while not port_file.exists():
            if time.monotonic() > deadline:
                raise OracleError("the scripted server never bound")
            time.sleep(0.02)
        self.base = f"http://{port_file.read_text(encoding='utf-8').strip()}"

    def requests(self) -> list[str]:
        if not self.log.exists():
            return []
        lines = [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines() if line]
        observed = []
        for entry in lines:
            body = entry.get("body")
            observed.append([
                entry["method"],
                entry["path"],
                body.get("method") if isinstance(body, dict) else None,
                "authorization" in entry.get("headers", {}),
            ])
        return observed

    def close(self) -> None:
        self.process.kill()
        self.process.wait(timeout=5)


# --------------------------------------------------------------------------
# The browser
# --------------------------------------------------------------------------


class Browser:
    """Answers each authorization URL by requesting the loopback callback."""

    def __init__(self) -> None:
        self.visits: list[dict[str, Any]] = []
        self.threads: list[threading.Thread] = []
        self.ports: dict[str, int] = {}

    def visit(self, url: str, port: int | None) -> None:
        if port is None:
            self.visits.append({"status": None})
            return
        thread = threading.Thread(target=self._visit, args=(url, port), daemon=True)
        self.threads.append(thread)
        thread.start()

    def _visit(self, url: str, port: int) -> None:
        state = dict(parse_qsl(urlsplit(url).query)).get("state", "")
        deadline = time.monotonic() + 10
        while True:
            try:
                connection = socket.create_connection(("127.0.0.1", port), timeout=5)
                break
            except OSError:
                if time.monotonic() > deadline:
                    self.visits.append({"status": None})
                    return
                time.sleep(0.02)
        with connection:
            connection.sendall(f"GET /callback?code=code-1&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n".encode())
            received = b""
            while chunk := connection.recv(65536):
                received += chunk
        status_line = received.split(b"\r\n", 1)[0].decode("latin-1")
        self.visits.append({"status": int(status_line.split(" ")[1]) if status_line else None})

    def settle(self) -> list[dict[str, Any]]:
        for thread in self.threads:
            thread.join(timeout=15)
        self.threads = []
        visits, self.visits = self.visits, []
        return visits


# --------------------------------------------------------------------------
# One scenario
# --------------------------------------------------------------------------


class Run:
    def __init__(self, scenario: dict[str, Any], command: list[str], dialect: str, fixture: Path, quiet: float) -> None:
        self.scenario = scenario
        self.dialect = dialect
        self.quiet = quiet
        self.root = Path(tempfile.mkdtemp(prefix="vibe-mcp-catalog-oracle-"))
        self.world = acp.World(self.root)
        self.backend = acp.Backend()
        self.fixtures: dict[str, Fixture] = {}
        self.redirects: dict[str, int] = {}
        self.browser = Browser()
        self.sessions: list[str] = []
        self.keyring = self.root / "keyring.json"
        try:
            for name, script in scenario.get("servers", {}).items():
                self.redirects[name] = free_port()
                self.fixtures[name] = Fixture(fixture, self.root, name, script, self.redirects[name])
            config = acp.base_config(self.backend) + self.substitute(scenario.get("config", ""))
            (self.world.vibe_home / "config.toml").write_text(config, encoding="utf-8")
            if "keyring" in scenario:
                entries = {
                    SERVICE: {
                        self.substitute(account): self.substitute(json.dumps(value) if not isinstance(value, str) else value)
                        for account, value in scenario["keyring"].items()
                    }
                }
                self.keyring.write_text(json.dumps(entries), encoding="utf-8")
            module = self.root / "keyring-module"
            module.mkdir()
            (module / "oracle_keyring.py").write_text(KEYRING_MODULE, encoding="utf-8")
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(self.world.home),
                "VIBE_HOME": str(self.world.vibe_home),
                "MISTRAL_API_KEY": "oracle-key",
                "VIBE_API_BASE": f"{self.backend.base}/v1/chat/completions",
                "LANG": "C.UTF-8",
                "TERM": "dumb",
                "NO_COLOR": "1",
                "CI": "true",
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
                "PYTHONPATH": str(module),
                "PYTHON_KEYRING_BACKEND": "oracle_keyring.FileKeyring",
                KEYRING_VARIABLE: str(self.keyring),
            }
            self.server = acp.Agent(command, env, self.world.workspace, {}, self.world)
        except BaseException:
            self.close()
            raise

    def substitute(self, text: str) -> str:
        for name, fixture in self.fixtures.items():
            text = text.replace(f"{{server:{name}}}", fixture.base)
            text = text.replace(f"{{redirect:{name}}}", str(self.redirects[name]))
        for index in range(len(self.sessions), 0, -1):
            text = text.replace(f"$S{index}", self.sessions[index - 1])
        return text

    def substitute_value(self, value: Any) -> Any:
        return json.loads(self.substitute(json.dumps(value)))

    def redirect_for(self, name: str) -> int | None:
        """The loopback port a sign-in of `name` listens on."""

        for server, port in self.redirects.items():
            if server == name or name.startswith(server):
                return port
        return None

    def collect(self, request_id: Any, wait: float | None = None) -> list[dict[str, Any]]:
        """Everything the server writes until the answer and then silence.

        A sign-in answers only once the browser came back, so the browser
        visits every authorization URL the moment it is published.
        """

        messages: list[dict[str, Any]] = []
        answered = request_id is None
        deadline = time.monotonic() + (RESPONSE_TIMEOUT if wait is None else wait)
        while True:
            timeout = self.quiet if answered else max(0.0, deadline - time.monotonic())
            try:
                message = self.server.inbox.get(timeout=timeout)
            except queue.Empty:
                if answered:
                    return messages
                if wait is not None:
                    return messages
                raise OracleError(f"no response to request {request_id!r}") from None
            if message is None:
                if not answered:
                    raise OracleError(f"server exited before answering {request_id!r}")
                return messages
            messages.append(message)
            if message.get("method") == "mcp_catalog/authUrl":
                params = message.get("params") or {}
                self.browser.visit(params.get("url", ""), self.redirect_for(params.get("name", "")))
            if not answered and "method" not in message and message.get("id") == request_id:
                answered = True

    def learn(self, message: dict[str, Any]) -> None:
        result = message.get("result")
        if isinstance(result, dict):
            state = result.get("state")
            if isinstance(state, dict):
                session = state.get("session")
                if isinstance(session, dict) and isinstance(session.get("id"), str):
                    if session["id"] not in self.sessions:
                        self.sessions.append(session["id"])

    def config_state(self) -> dict[str, Any]:
        files = {
            "user": self.world.vibe_home / "config.toml",
            "project": self.world.workspace / ".vibe" / "config.toml",
        }
        observed: dict[str, Any] = {}
        for label, path in files.items():
            if not path.is_file():
                observed[label] = None
                continue
            document = tomllib.loads(path.read_text(encoding="utf-8"))
            observed[label] = document.get("mcp_servers", [])
        return observed

    def keyring_state(self) -> dict[str, Any]:
        if not self.keyring.exists():
            return {}
        entries = json.loads(self.keyring.read_text(encoding="utf-8")).get(SERVICE, {})
        observed: dict[str, Any] = {}
        for account, secret in sorted(entries.items()):
            try:
                value = json.loads(secret)
            except json.JSONDecodeError:
                value = secret
            if isinstance(value, dict) and value.get("expires_at") is not None:
                value["expires_at"] = "<time>"
            observed[account] = value
        return observed

    def run(self) -> list[dict[str, Any]]:
        steps: list[dict[str, Any]] = []
        identifier = 1
        self.server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                          "params": {"clientInfo": {"name": "mcp-catalog-oracle", "version": "0"}}})
        self.collect(identifier)
        self.server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        try:
            for step in self.scenario["steps"]:
                identifier += 1
                if "start" in step:
                    params = (
                        {"agentConfig": {"cwd": str(self.world.workspace), "agent": "auto-approve"}}
                        if self.dialect == "reference"
                        else {"cwd": str(self.world.workspace), "agent": "auto-approve"}
                    )
                    self.server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start", "params": params})
                    observed = self.collect(identifier)
                    for message in observed:
                        self.learn(message)
                    response = next((m for m in observed if "method" not in m and m.get("id") == identifier), {})
                    # What a start publishes about MCP races the start itself in
                    # the reference: a source discovered before the session is
                    # attached raises its `mcp_catalog/authRequired` into no
                    # connection. Only whether the start succeeded is kept, and
                    # the state it left is read by a later step.
                    started: dict[str, Any] = {"ok": "result" in response}
                    if "error" in response:
                        started["error"] = response["error"]
                    if os.environ.get("VIBE_CATALOG_ORACLE_DEBUG"):
                        print(json.dumps(response)[:3000], file=sys.stderr)
                    steps.append({"start": started})
                    continue
                if "config" in step:
                    steps.append({"config": self.config_state()})
                    continue
                if "keyring" in step:
                    steps.append({"keyring": self.keyring_state()})
                    continue
                if "wire" in step:
                    steps.append({"wire": {name: self.fixtures[name].requests() for name in step["wire"]}})
                    continue
                if "settle" in step:
                    # Lets a start's discovery finish; what it published is
                    # dropped for the reason `start` gives.
                    time.sleep(step["settle"])
                    self.collect(None)
                    continue
                request = self.substitute_value(step["send"])
                self.server.send({"jsonrpc": "2.0", "id": identifier, **request})
                # A request the server may leave unanswered is given a bounded
                # wait, and whether it answered within it is the observation.
                observed = self.collect(identifier, step.get("wait"))
                response = next((m for m in observed if "method" not in m and m.get("id") == identifier), None)
                entry: dict[str, Any] = {
                    "method": request["method"],
                    "response": (
                        {"pending": True}
                        if response is None
                        else reduce_runtime({key: value for key, value in response.items() if key not in {"jsonrpc", "id"}})
                    ),
                    "notifications": self.notifications(observed, identifier),
                }
                visits = self.browser.settle()
                if visits:
                    entry["browser"] = visits
                steps.append(entry)
        except OracleError as error:
            steps.append({"failure": str(error)})
        return steps

    def notifications(self, observed: list[dict[str, Any]], identifier: Any) -> list[Any]:
        recorded: list[Any] = []
        for message in observed:
            method = message.get("method")
            if method is None or "id" in message:
                continue
            params = message.get("params") or {}
            if method.startswith("mcp_catalog/") or method.startswith("mcp/"):
                recorded.append({"method": method, "params": params})
            elif method == "runtime/updated":
                recorded.append({"method": method, "mcp": (params.get("runtime") or {}).get("mcp")})
            elif method == "warning":
                recorded.append({"method": method})
        return recorded

    def close(self) -> None:
        server = getattr(self, "server", None)
        if server is not None:
            server.stop()
        for fixture in self.fixtures.values():
            fixture.close()
        self.backend.close()
        shutil.rmtree(self.root, ignore_errors=True)


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def reduce_runtime(value: Any) -> Any:
    """A runtime snapshot reduced to its MCP state, the part this row owns."""

    if isinstance(value, dict):
        return {
            key: ({"mcp": item.get("mcp")} if key == "runtime" and isinstance(item, dict) else reduce_runtime(item))
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [reduce_runtime(item) for item in value]
    return value


class Normalizer:
    def __init__(self, scenario: dict[str, Any], run: Run) -> None:
        self.authored = authored_strings(scenario)
        self.bases = {fixture.base: f"http://<server:{name}>" for name, fixture in run.fixtures.items()}
        self.redirects = {str(port): f"<redirect:{name}>" for name, port in run.redirects.items()}
        self.sessions = {session: f"<session-{index + 1}>" for index, session in enumerate(run.sessions)}
        # A descriptor revision hashes the source's authorization identity; the
        # generation after it is what a sign-in or a sign-out moves.
        self.descriptors: dict[str, str] = {}
        self.paths = sorted(
            ((str(run.world.workspace), "<ws>"), (str(run.world.vibe_home), "<vibe-home>"),
             (str(run.world.home), "<home>"), (str(run.root), "<root>")),
            key=lambda item: -len(item[0]),
        )

    def text(self, value: str) -> str:
        for base, placeholder in self.bases.items():
            value = value.replace(base, placeholder)
        for port, placeholder in self.redirects.items():
            value = value.replace(f"127.0.0.1:{port}", f"127.0.0.1:{placeholder}")
        for session, placeholder in self.sessions.items():
            value = value.replace(session, placeholder)
        for path, placeholder in self.paths:
            value = value.replace(path, placeholder)

        def descriptor(match: re.Match[str]) -> str:
            identity = self.descriptors.setdefault(match.group(1), f"<descriptor-{len(self.descriptors) + 1}>")
            return f"mcp-auth-descriptor:{identity}:{match.group(2)}"

        return DESCRIPTOR.sub(descriptor, value)

    def value(self, value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            if key == "url" and False:
                return value
            return {name: self.value(item, name) for name, item in value.items()}
        if isinstance(value, list):
            return [self.value(item, key) for item in value]
        if key in TIME_KEYS and value is not None and not isinstance(value, bool):
            return "<time>"
        if isinstance(value, int) and not isinstance(value, bool) and str(value) in self.redirects:
            return self.redirects[str(value)]
        if not isinstance(value, str):
            return value
        text = self.text(value)
        if key == "url" and "/authorize?" in value:
            return self.authorization_url(value)
        if text in self.authored or value in self.authored:
            return text
        if re.search(r"\s", text):
            return digest(text)
        return text


    def authorization_url(self, url: str) -> dict[str, Any]:
        parts = urlsplit(url)
        parameters = {}
        for name, value in parse_qsl(parts.query):
            parameters[name] = f"<{name}>" if name in {"state", "code_challenge"} else self.text(value)
        return {"authorize": self.text(f"{parts.scheme}://{parts.netloc}{parts.path}"), "parameters": parameters}


def authored_strings(scenario: dict[str, Any]) -> set[str]:
    found: set[str] = set()

    def walk(value: Any) -> None:
        if isinstance(value, str):
            found.add(value)
        elif isinstance(value, dict):
            for key, item in value.items():
                found.add(key)
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)

    walk(scenario)
    for tool in TOOLS:
        walk(tool)
        found.add(tool["description"].split("\n", 1)[0])
    return found


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def send(method: str, **params: Any) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


CONFIG = {"config": True}
KEYRING = {"keyring": True}
START = {"start": {}}

OPEN_SERVER = '''
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "{server:open}/mcp"
'''

SECURE_SERVER = '''
[[mcp_servers]]
name = "secure"
transport = "streamable-http"
url = "{server:secure}/mcp"
auth = { type = "oauth", scopes = [], redirect_port = {redirect:secure} }
'''

REMOTE_SERVER = '''
[[mcp_servers]]
name = "remote"
transport = "streamable-http"
url = "https://docs.example/mcp"
auth = { type = "oauth", scopes = ["read"] }
'''

STDIO_SERVER = '''
[[mcp_servers]]
name = "local"
transport = "stdio"
command = "true"
'''

STORED_TOKENS = {"access_token": "at-1", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "rt-1", "expires_at": 4102444800.0}
STORED_FINGERPRINT = '{"url":"{server:secure}/mcp","scopes_sorted":[],"client_marker":"<dcr>"}'
STORED_CLIENT = {"client_id": "client-1", "redirect_uris": ["http://127.0.0.1:{redirect:secure}/callback"], "token_endpoint_auth_method": "none"}


def scenarios() -> list[dict[str, Any]]:
    return [
        {
            "name": "sessionless/reads-need-a-session",
            "steps": [
                send("mcp_catalog/read", sessionId="missing"),
                send("mcp/read", sessionId="missing"),
                send("mcp_catalog/read"),
                send("mcp_catalog/refresh", sessionId="missing"),
                send("mcp/refresh", sessionId="missing"),
                send("mcp_catalog/unknown", sessionId="missing"),
                send("mcp/remove", name="docs"),
                send("mcp/auth/complete", sessionId="missing", name="docs"),
            ],
        },
        {
            "name": "sessionless/add",
            "steps": [
                send("mcp_catalog/add", url="https://docs.example/mcp"),
                CONFIG,
                send("mcp_catalog/add", url="https://docs.example/mcp/"),
                send("mcp_catalog/add", url="https://docs.example/mcp", name="other"),
                send("mcp_catalog/add", url="https://docs.example/second", name="docs-example"),
                send("mcp_catalog/add", url="https://api.example/v1/mcp", name="Team Docs!", scopes=["read", "write"]),
                send("mcp/add", url="https://legacy.example/sse", transport="http"),
                CONFIG,
            ],
        },
        {
            "name": "sessionless/add-refusals",
            "config": STDIO_SERVER,
            "steps": [
                send("mcp_catalog/add", url="http://docs.example/mcp"),
                send("mcp_catalog/add", url="http://docs.example/mcp", allowInsecureHttp=True),
                send("mcp_catalog/add", url="ftp://docs.example/mcp"),
                send("mcp_catalog/add", url="not a url"),
                send("mcp_catalog/add", url="https://docs.example/x", name="!!!"),
                send("mcp_catalog/add", url="https://docs.example/y", name="local"),
                send("mcp_catalog/add", url="https://docs.example/z", transport="stdio"),
                send("mcp_catalog/add", url="https://docs.example/z", headers={"x": "y"}),
                send("mcp_catalog/add"),
                CONFIG,
            ],
        },
        {
            "name": "sessionless/toggle",
            "config": REMOTE_SERVER + STDIO_SERVER,
            "steps": [
                send("mcp_catalog/toggle", name="remote", source="server", disabled=True),
                CONFIG,
                send("mcp_catalog/toggle", name="remote", source="server", disabled=False),
                send("mcp_catalog/toggle", name="remote", source="server", disabled=True, toolName="search"),
                send("mcp/toggle", name="local", source="server", disabled=True, toolName="run"),
                CONFIG,
                send("mcp_catalog/toggle", name="remote", source="server", disabled=False, toolName="search"),
                CONFIG,
                send("mcp_catalog/toggle", name="remote", source="connector", disabled=True),
                send("mcp_catalog/toggle", name="missing", source="server", disabled=True),
                CONFIG,
                send("mcp_catalog/toggle", name="missing", source="server", disabled=False, toolName="x"),
                CONFIG,
                send("mcp_catalog/toggle", name="remote", source="plugin", disabled=True),
                send("mcp_catalog/toggle", name="remote", disabled=True),
            ],
        },
        {
            "name": "sessionless/remove",
            "servers": {"secure": SECURE},
            "config": REMOTE_SERVER + STDIO_SERVER + SECURE_SERVER,
            "keyring": {
                "mcp-oauth:secure:tokens": STORED_TOKENS,
                "mcp-oauth:secure:client_info": STORED_CLIENT,
                "mcp-oauth:remote:tokens": STORED_TOKENS,
            },
            "steps": [
                send("mcp_catalog/remove", name="local"),
                send("mcp_catalog/remove", name="secure"),
                KEYRING,
                send("mcp_catalog/remove", name="missing"),
                send("mcp_catalog/remove", name="remote"),
                CONFIG,
                KEYRING,
            ],
        },
        {
            "name": "sessionless/login-refusals",
            "config": REMOTE_SERVER + STDIO_SERVER,
            "keyring": {"mcp-oauth:remote:tokens": STORED_TOKENS},
            "steps": [
                send("mcp_catalog/login", name="missing"),
                send("mcp_catalog/login", name="local"),
                send("mcp_catalog/logout", name="local"),
                send("mcp_catalog/logout", name="missing"),
                send("mcp/logout", name="remote"),
                KEYRING,
                send("mcp_catalog/logout", name="remote"),
            ],
        },
        {
            # Before a session attaches the server publishes no notification,
            # so the authorization URL of a sessionless sign-in reaches no one.
            "name": "sessionless/login-publishes-nothing",
            "servers": {"secure": SECURE},
            "config": SECURE_SERVER,
            "steps": [
                {**send("mcp_catalog/login", name="secure"), "wait": 5.0},
                {"wire": ["secure"]},
                send("mcp_catalog/read", sessionId="missing"),
                KEYRING,
            ],
        },
        {
            "name": "session/read",
            "servers": {"open": OPEN},
            "config": OPEN_SERVER + REMOTE_SERVER,
            "steps": [
                START,
                {"settle": 1.0},
                send("mcp_catalog/read", sessionId="$S1"),
                send("mcp/read", sessionId="$S1"),
                send("mcp_catalog/read", sessionId="other"),
            ],
        },
        {
            "name": "session/sessionless-mutations-conflict",
            "config": REMOTE_SERVER,
            "steps": [
                START,
                send("mcp_catalog/add", url="https://docs.example/other"),
                send("mcp_catalog/toggle", name="remote", source="server", disabled=True),
                send("mcp_catalog/remove", name="remote"),
                send("mcp_catalog/login", name="remote"),
                send("mcp_catalog/logout", name="remote"),
                send("mcp_catalog/toggle", sessionId="other", name="remote", source="server", disabled=True),
                CONFIG,
            ],
        },
        {
            "name": "session/toggle",
            "servers": {"open": OPEN},
            "config": OPEN_SERVER,
            "steps": [
                START,
                {"settle": 1.0},
                send("mcp_catalog/toggle", sessionId="$S1", name="docs", source="server", disabled=True, toolName="search"),
                CONFIG,
                send("mcp_catalog/toggle", sessionId="$S1", name="docs", source="server", disabled=True),
                send("mcp_catalog/toggle", sessionId="$S1", name="docs", source="server", disabled=False),
                send("mcp/toggle", sessionId="$S1", name="docs", source="server", disabled=False, toolName="search"),
                CONFIG,
                send("mcp_catalog/toggle", sessionId="$S1", name="docs", source="connector", disabled=True),
            ],
        },
        {
            "name": "session/refresh",
            "servers": {"open": OPEN},
            "config": OPEN_SERVER,
            "steps": [
                START,
                {"settle": 1.0},
                {"wire": ["open"]},
                send("mcp_catalog/refresh", sessionId="$S1"),
                {"wire": ["open"]},
                send("mcp/refresh", sessionId="$S1"),
            ],
        },
        {
            "name": "session/add-and-remove",
            "servers": {"open": OPEN},
            "steps": [
                START,
                send("mcp_catalog/add", sessionId="$S1", url="{server:open}/mcp", allowInsecureHttp=True),
                {"settle": 1.0},
                CONFIG,
                send("mcp_catalog/add", sessionId="$S1", url="{server:open}/mcp", allowInsecureHttp=True),
                send("mcp_catalog/remove", sessionId="$S1", name="127"),
                send("mcp_catalog/remove", sessionId="$S1", name="missing"),
                CONFIG,
            ],
        },
        {
            "name": "session/auth-required-then-login",
            "servers": {"secure": SECURE},
            "config": SECURE_SERVER,
            "steps": [
                START,
                {"settle": 1.5},
                send("mcp_catalog/read", sessionId="$S1"),
                send("mcp_catalog/login", sessionId="$S1", name="secure"),
                KEYRING,
                send("mcp_catalog/logout", sessionId="$S1", name="secure"),
                {"settle": 1.0},
                KEYRING,
                send("mcp_catalog/login", sessionId="$S1", name="missing"),
            ],
        },
        {
            "name": "session/credentials-without-fingerprint-are-dropped",
            "servers": {"secure": SECURE},
            "config": SECURE_SERVER,
            "keyring": {
                "mcp-oauth:secure:tokens": STORED_TOKENS,
                "mcp-oauth:secure:client_info": STORED_CLIENT,
            },
            "steps": [
                START,
                {"settle": 1.0},
                send("mcp_catalog/read", sessionId="$S1"),
                KEYRING,
            ],
        },
        {
            "name": "session/stored-credentials-connect",
            "servers": {"secure": SECURE},
            "config": SECURE_SERVER,
            "keyring": {
                "mcp-oauth:secure:tokens": STORED_TOKENS,
                "mcp-oauth:secure:client_info": STORED_CLIENT,
                "mcp-oauth:secure:fingerprint": STORED_FINGERPRINT,
            },
            "steps": [
                START,
                {"settle": 1.0},
                send("mcp_catalog/read", sessionId="$S1"),
                {"wire": ["secure"]},
                send("mcp_catalog/logout", sessionId="$S1", name="secure"),
                KEYRING,
                send("mcp_catalog/refresh", sessionId="$S1"),
            ],
        },
    ]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def build_fixture() -> Path:
    result = subprocess.run(
        ["cargo", "build", "-p", "vibe-app-server", "--features", "test-fixtures", "--bin", "vibe-oauth-script-fixture", "--message-format", "json"],
        cwd=REPOSITORY, capture_output=True, text=True, check=False,
    )
    if result.returncode != 0:
        raise OracleError(f"cannot build the scripted server: {result.stderr[-2000:]}")
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            if message["target"]["name"] == "vibe-oauth-script-fixture":
                return Path(message["executable"])
    raise OracleError("cargo built no vibe-oauth-script-fixture executable")


def capture_scenario(scenario: dict[str, Any], command: list[str], dialect: str, fixture: Path, quiet: float, raw: bool) -> dict[str, Any]:
    run = Run(scenario, command, dialect, fixture, quiet)
    try:
        steps = run.run()
        normalizer = Normalizer(scenario, run)
        entry = {"name": scenario["name"], "scenario": scenario, "observed": normalizer.value(copy.deepcopy(steps))}
        if raw:
            entry["raw"] = steps
        return entry
    finally:
        run.close()


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--server", type=Path, default=None, help="drive this port's stdio fixture instead")
    parser.add_argument("--fixture", type=Path, default=None, help="the vibe-oauth-script-fixture binary")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--raw", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        fixture = (arguments.fixture or build_fixture()).resolve()
        if arguments.server is not None:
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
            dialect = "port"
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
            dialect = "reference"
        selected = [s for s in scenarios() if not arguments.only or any(name in s["name"] for name in arguments.only)]
        captured = []
        for scenario in selected:
            started = time.monotonic()
            entry = capture_scenario(scenario, command, dialect, fixture, arguments.quiet, arguments.raw)
            if dialect == "reference":
                # A corpus holds only what the reference answers every time.
                again = capture_scenario(scenario, command, dialect, fixture, arguments.quiet, False)
                if again["observed"] != entry["observed"]:
                    raise OracleError(f"scenario {scenario['name']} is not deterministic across two captures")
            captured.append(entry)
            print(f"{scenario['name']}: {time.monotonic() - started:.1f}s", file=sys.stderr)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "note": (
                "Captured by scripts/parity/mcp_catalog.py from the pinned reference's vibe-app-server "
                "against vibe-oauth-script-fixture. Scenario inputs, names and digests only: every "
                "string the server authored is a length and a SHA-256."
            ),
            "scenarios": captured,
        }
        if arguments.check:
            committed = json.loads(arguments.output.read_text(encoding="utf-8"))
            by_name = {entry["name"]: entry for entry in committed["scenarios"]}
            differing = [e["name"] for e in captured if by_name.get(e["name"], {}).get("observed") != e["observed"]]
            if differing:
                raise OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        print(f"captured {len(captured)} scenarios into {arguments.output}")
    except (OracleError, acp.OracleError) as error:
        print(f"MCP catalog capture failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
