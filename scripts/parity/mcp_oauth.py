#!/usr/bin/env python3
"""Capture what the pinned reference's MCP OAuth client sends and stores.

Row 11 of ``docs/parity.md`` is MCP. This capture drives the reference's
``MCPAuthenticationService`` (``vibe/app_server/_mcp_auth.py``), which signs in
through ``perform_oauth_login`` and refreshes through the MCP SDK's
``OAuthClientProvider`` (``vibe/core/auth/mcp_oauth.py``), against
``vibe-oauth-script-fixture``: a scripted HTTP server built from this
repository that plays the MCP resource and its authorization server. The
replay in ``crates/vibe-app-server/tests/mcp_oauth_parity_tests.rs`` drives
this port's service against the very same server.

The keyring is an in-memory backend the capture installs (or the reference's
``fail`` backend, for a host without one), seeded per scenario. A simulated
browser answers every authorization URL by requesting the loopback callback
the way an authorization server's redirect would.

What the capture records per scenario:

``steps``    each operation (login, resolve, reject, logout): its outcome or
             the class of its failure, the revisions it left, the
             authorization URLs published with their parameters, what the
             browser got back, and the keyring afterwards
``wire``     every request the fixture received, in order, normalized

The committed corpus carries only values the scenarios supplied, names and
digests: the callback pages the reference authors are reduced to a length and
a SHA-256, which is what ``NOTICE`` requires.

Usage::

    scripts/parity/mcp_oauth.py --corpus          # capture and write the corpus
    scripts/parity/mcp_oauth.py --check           # recapture and compare
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any
from urllib.parse import parse_qsl, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compaction import (  # noqa: E402
    OracleError,
    extract_pinned_tree,
    reexecute_with_reference_interpreter,
    resolve_reference,
)
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

SCHEMA_VERSION = 1
DEFAULT_CORPUS = Path("crates/vibe-app-server/tests/mcp-oauth/corpus.json")
DEFAULT_CACHE = Path(".parity")
FIXTURE_VARIABLE = "VIBE_OAUTH_PARITY_FIXTURE"
REPOSITORY = Path(__file__).resolve().parents[2]
SERVICE = "ai.mistral.vibe"
STEP_TIMEOUT = 30.0

#: Placeholders for what differs between two runs of the same scenario.
BASE_PLACEHOLDER = "http://__BASE__"
REDIRECT_PLACEHOLDER = "__REDIRECT__"


def digest(text: str) -> dict[str, Any]:
    return {"length": len(text), "sha256": hashlib.sha256(text.encode()).hexdigest()}


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------

PRM_PATH = "GET /.well-known/oauth-protected-resource/mcp"
PRM_ROOT = "GET /.well-known/oauth-protected-resource"
ASM_ROOT = "GET /.well-known/oauth-authorization-server"
OIDC_ROOT = "GET /.well-known/openid-configuration"
CHALLENGE = {"status": 401, "headers": {"www-authenticate": 'Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp"'}}
BARE_CHALLENGE = {"status": 401, "headers": {"www-authenticate": "Bearer"}}
INITIALIZED = {"status": 200, "json": {"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "fixture", "version": "0"}}}}
NOT_FOUND = {"status": 404, "text": "missing"}


def prm(**extra: Any) -> dict[str, Any]:
    return {"status": 200, "json": {"resource": "{base}/mcp", "authorization_servers": ["{base}"], **extra}}


def asm(**extra: Any) -> dict[str, Any]:
    return {
        "status": 200,
        "json": {
            "issuer": "{base}",
            "authorization_endpoint": "{base}/oauth/authorize",
            "token_endpoint": "{base}/oauth/token",
            "registration_endpoint": "{base}/oauth/register",
            "code_challenge_methods_supported": ["S256"],
            **extra,
        },
    }


def registered(**extra: Any) -> dict[str, Any]:
    return {"status": 201, "json": {"client_id": "client-1", "redirect_uris": ["http://127.0.0.1:{redirect}/callback"], **extra}}


def token(access: str = "at-1", **extra: Any) -> dict[str, Any]:
    return {"status": 200, "json": {"access_token": access, "token_type": "bearer", "expires_in": 3600, "refresh_token": "rt-1", **extra}}


def full_routes(**overrides: Any) -> dict[str, Any]:
    routes = {
        "POST /mcp": [CHALLENGE, INITIALIZED],
        PRM_PATH: [prm(scopes_supported=["read", "write"])],
        ASM_ROOT: [asm()],
        "POST /oauth/register": [registered()],
        "POST /oauth/token": [token()],
    }
    routes.update(overrides)
    return routes


def oauth(**auth: Any) -> dict[str, Any]:
    return {"transport": "streamable-http", "auth": {"type": "oauth", "scopes": [], **auth}}


LOGIN = {"do": "login"}
RESOLVE = {"do": "resolve"}
LOGOUT = {"do": "logout"}
REJECT = {"do": "reject"}
STORED_TOKENS = {"access_token": "at-0", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "rt-0", "expires_at": "+3600"}
EXPIRED_TOKENS = {**STORED_TOKENS, "expires_at": "-60"}
STORED_CLIENT = {"client_id": "client-0", "redirect_uris": ["http://127.0.0.1:{redirect}/callback"], "token_endpoint_auth_method": "none"}


def scenario(name: str, routes: dict[str, Any], steps: list[dict[str, Any]], *, server: dict[str, Any] | None = None, keyring: Any = None, browser: str = "code") -> dict[str, Any]:
    return {
        "name": name,
        "server": server or oauth(),
        "script": {"routes": routes},
        "keyring": keyring if keyring is not None else {},
        "browser": browser,
        "steps": steps,
    }


def all_scenarios() -> list[dict[str, Any]]:
    stored = {"tokens": STORED_TOKENS, "client_info": STORED_CLIENT, "fingerprint": "current"}
    expired = {"tokens": EXPIRED_TOKENS, "client_info": STORED_CLIENT, "fingerprint": "current"}
    return [
        # Signing in.
        scenario("login/dcr", full_routes(), [LOGIN, RESOLVE]),
        scenario(
            "login/discovery-fallbacks",
            full_routes(**{
                "POST /mcp": [BARE_CHALLENGE, INITIALIZED],
                PRM_PATH: [NOT_FOUND],
                PRM_ROOT: [{"status": 200, "json": {"resource": "{base}/", "authorization_servers": ["{base}/tenant"]}}],
                "GET /.well-known/oauth-authorization-server/tenant": [NOT_FOUND],
                "GET /.well-known/openid-configuration/tenant": [asm(issuer="{base}/tenant")],
            }),
            [LOGIN],
        ),
        scenario(
            "login/legacy-authorization-server",
            full_routes(**{"POST /mcp": [BARE_CHALLENGE, INITIALIZED], PRM_PATH: [NOT_FOUND], ASM_ROOT: [asm(registration_endpoint=None)], "POST /register": [registered()]}),
            [LOGIN],
        ),
        scenario(
            "login/no-metadata",
            {
                "POST /mcp": [BARE_CHALLENGE, INITIALIZED],
                "POST /register": [registered()],
                "POST /token": [token()],
            },
            [LOGIN],
        ),
        scenario(
            "login/metadata-server-error",
            {
                "POST /mcp": [BARE_CHALLENGE, INITIALIZED],
                PRM_PATH: [{"status": 500, "text": "down"}],
                ASM_ROOT: [{"status": 500, "text": "down"}],
                "POST /register": [registered()],
                "POST /token": [token()],
            },
            [LOGIN],
        ),
        scenario("login/invalid-metadata", full_routes(**{PRM_PATH: [{"status": 200, "json": {"resource": "{base}/mcp", "authorization_servers": []}}], OIDC_ROOT: [NOT_FOUND]}), [LOGIN]),
        scenario("login/client-id", full_routes(), [LOGIN, RESOLVE], server=oauth(client_id="pre-registered", scopes=["read"])),
        scenario(
            "login/client-metadata-document",
            full_routes(**{ASM_ROOT: [asm(client_id_metadata_document_supported=True)]}),
            [LOGIN],
            server=oauth(client_metadata_url="https://client.example/vibe.json"),
        ),
        scenario("login/client-metadata-unsupported", full_routes(), [LOGIN], server=oauth(client_metadata_url="https://client.example/vibe.json")),
        scenario(
            "login/challenge-scope",
            full_routes(**{"POST /mcp": [{"status": 401, "headers": {"www-authenticate": 'Bearer scope="alpha beta", resource_metadata="{base}/.well-known/oauth-protected-resource/mcp"'}}, INITIALIZED]}),
            [LOGIN],
        ),
        scenario("login/configured-scopes-dropped", full_routes(**{PRM_PATH: [prm()]}), [LOGIN], server=oauth(scopes=["declared"])),
        scenario("login/empty-scopes-supported", full_routes(**{PRM_PATH: [prm(scopes_supported=[])]}), [LOGIN]),
        scenario("login/server-scopes", full_routes(**{PRM_PATH: [prm()], ASM_ROOT: [asm(scopes_supported=["as-scope"])]}), [LOGIN]),
        scenario("login/resource-mismatch", full_routes(**{PRM_PATH: [{"status": 200, "json": {"resource": "{base}/other", "authorization_servers": ["{base}"]}}]}), [LOGIN]),
        scenario("login/no-challenge", {"POST /mcp": [INITIALIZED]}, [LOGIN]),
        scenario("login/callback-without-code", full_routes(), [LOGIN], browser="no-code"),
        scenario("login/callback-wrong-state", full_routes(), [LOGIN], browser="wrong-state"),
        scenario("login/callback-empty-request", full_routes(), [LOGIN], browser="empty"),
        scenario("login/port-in-use", full_routes(), [LOGIN], browser="busy"),
        scenario("login/token-refused", full_routes(**{"POST /oauth/token": [{"status": 400, "json": {"error": "invalid_request"}}]}), [LOGIN]),
        scenario("login/token-type-refused", full_routes(**{"POST /oauth/token": [token(token_type="mac")]}), [LOGIN]),
        scenario("login/token-without-lifetime", full_routes(**{"POST /oauth/token": [{"status": 200, "json": {"access_token": "at-9"}}]}), [LOGIN, RESOLVE]),
        scenario("login/registration-refused", full_routes(**{"POST /oauth/register": [{"status": 400, "json": {"error": "invalid_client_metadata"}}]}), [LOGIN]),
        scenario(
            "login/client-secret-basic",
            full_routes(**{"POST /oauth/register": [registered(client_secret="s3cr:t", token_endpoint_auth_method="client_secret_basic")]}),
            [LOGIN],
        ),
        scenario(
            "login/client-secret-post",
            full_routes(**{"POST /oauth/register": [registered(client_secret="s3cret", token_endpoint_auth_method="client_secret_post")]}),
            [LOGIN],
        ),
        scenario("login/already-authorized", {"POST /mcp": [INITIALIZED]}, [LOGIN, RESOLVE], keyring=stored, browser="none"),
        scenario(
            "login/refreshes-first",
            {"POST /mcp": [INITIALIZED], "POST /token": [{"status": 200, "json": {"access_token": "at-2", "token_type": "Bearer", "expires_in": 60}}]},
            [LOGIN, RESOLVE],
            keyring=expired,
            browser="none",
        ),
        scenario(
            "login/refresh-invalid-grant",
            full_routes(**{"POST /token": [{"status": 400, "json": {"error": "invalid_grant", "error_description": "revoked"}}], "POST /mcp": [BARE_CHALLENGE, CHALLENGE, INITIALIZED]}),
            [LOGIN],
            keyring=expired,
        ),
        scenario(
            "login/refresh-transient",
            full_routes(**{"POST /token": [{"status": 503, "text": "busy"}]}),
            [LOGIN],
            keyring=expired,
        ),
        scenario(
            "login/insufficient-scope",
            full_routes(**{"POST /mcp": [{"status": 403, "headers": {"www-authenticate": 'Bearer error="insufficient_scope", scope="more"'}}, INITIALIZED]}),
            [LOGIN],
            keyring=stored,
        ),
        scenario("login/forbidden", {"POST /mcp": [{"status": 403}, INITIALIZED]}, [LOGIN], keyring=stored, browser="none"),
        scenario("login/not-oauth", {}, [LOGIN], server={"transport": "streamable-http", "auth": {"type": "static", "headers": {"X-Key": "k"}}}, browser="none"),
        scenario("login/headless", {}, [LOGIN, RESOLVE], keyring="headless", browser="none"),
        # Resolving a stored credential.
        scenario("resolve/missing", {}, [RESOLVE], browser="none"),
        scenario("resolve/valid", {}, [RESOLVE, RESOLVE], keyring=stored, browser="none"),
        scenario("resolve/lowercase-type", {}, [RESOLVE], keyring={**stored, "tokens": {**STORED_TOKENS, "token_type": "bearer"}}, browser="none"),
        scenario("resolve/stale-fingerprint", {}, [RESOLVE, RESOLVE], keyring={**stored, "fingerprint": "stale"}, browser="none"),
        scenario("resolve/fingerprint-without-tokens", {}, [RESOLVE], keyring={"fingerprint": "current"}, browser="none"),
        scenario(
            "resolve/expired-refreshed",
            {"POST /token": [{"status": 200, "json": {"access_token": "at-2", "token_type": "Bearer", "expires_in": 60}}], "GET /mcp": [{"status": 405}]},
            [RESOLVE],
            keyring=expired,
            browser="none",
        ),
        scenario(
            "resolve/expired-invalid-grant",
            {"POST /token": [{"status": 400, "json": {"error": "invalid_grant"}}], "GET /mcp": [{"status": 405}]},
            [RESOLVE, RESOLVE],
            keyring=expired,
            browser="none",
        ),
        scenario("resolve/expired-transient", {"POST /token": [{"status": 502, "text": "gateway"}]}, [RESOLVE], keyring=expired, browser="none"),
        scenario(
            "resolve/expired-refresh-unreadable",
            {"POST /token": [{"status": 200, "text": "not json"}], "GET /mcp": [{"status": 405}]},
            [RESOLVE],
            keyring=expired,
            browser="none",
        ),
        scenario(
            "resolve/expired-then-challenged",
            full_routes(**{"POST /token": [{"status": 200, "json": {"access_token": "at-3", "expires_in": 60}}], "GET /mcp": [BARE_CHALLENGE]}),
            [RESOLVE, RESOLVE],
            keyring=expired,
            browser="none",
        ),
        scenario(
            "resolve/expired-without-refresh",
            full_routes(**{"GET /mcp": [CHALLENGE]}),
            [RESOLVE],
            keyring={"tokens": {**EXPIRED_TOKENS, "refresh_token": None}, "fingerprint": "current"},
            browser="none",
        ),
        scenario(
            "resolve/lifetime-without-deadline",
            {"POST /token": [{"status": 200, "json": {"access_token": "at-4", "expires_in": 60}}], "GET /mcp": [{"status": 405}]},
            [RESOLVE],
            keyring={**stored, "tokens": {**STORED_TOKENS, "expires_at": None}},
            browser="none",
        ),
        scenario("resolve/headless", {}, [RESOLVE], keyring="headless", browser="none"),
        # Rejections and signing out.
        scenario("reject/after-resolve", {}, [RESOLVE, REJECT, RESOLVE], keyring=stored, browser="none"),
        scenario("logout/stored", {}, [RESOLVE, LOGOUT, RESOLVE], keyring=stored, browser="none"),
        scenario("logout/nothing-stored", {}, [LOGOUT], browser="none"),
    ]


# --------------------------------------------------------------------------
# Running one scenario against the reference
# --------------------------------------------------------------------------


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def install_keyring(headless: bool) -> Any:
    import keyring
    import keyring.backend
    import keyring.backends.fail
    import keyring.errors

    from vibe.utils import keyring as vibe_keyring

    class MemoryKeyring(keyring.backend.KeyringBackend):
        priority = 1  # type: ignore[assignment]

        def __init__(self) -> None:
            super().__init__()
            self.entries: dict[tuple[str, str], str] = {}

        def get_password(self, service: str, username: str) -> str | None:
            return self.entries.get((service, username))

        def set_password(self, service: str, username: str, password: str) -> None:
            self.entries[(service, username)] = password

        def delete_password(self, service: str, username: str) -> None:
            if (service, username) not in self.entries:
                raise keyring.errors.PasswordDeleteError(username)
            del self.entries[(service, username)]

    vibe_keyring.clear_api_key_keyring_cache()
    backend: Any = keyring.backends.fail.Keyring() if headless else MemoryKeyring()
    keyring.set_keyring(backend)
    return backend


class Browser:
    """What the operator's browser does with each authorization URL."""

    def __init__(self, action: str, port: int) -> None:
        self.action = action
        self.port = port
        self.visits: list[dict[str, Any]] = []
        self.threads: list[threading.Thread] = []

    def visit(self, url: str) -> None:
        if self.action in {"none", "busy"}:
            return
        thread = threading.Thread(target=self._visit, args=(url,), daemon=True)
        self.threads.append(thread)
        thread.start()

    def _visit(self, url: str) -> None:
        parameters = dict(parse_qsl(urlsplit(url).query))
        state = parameters.get("state", "")
        target = {
            "code": f"/callback?code=code-1&state={state}",
            "no-code": f"/callback?error=access_denied&state={state}",
            "wrong-state": "/callback?code=code-1&state=forged",
            "empty": None,
        }[self.action]
        deadline = time.monotonic() + 10
        while True:
            try:
                connection = socket.create_connection(("127.0.0.1", self.port), timeout=5)
                break
            except OSError:
                if time.monotonic() > deadline:
                    self.visits.append({"status": None})
                    return
                time.sleep(0.02)
        with connection:
            if target is not None:
                connection.sendall(f"GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n".encode())
            else:
                connection.shutdown(socket.SHUT_WR)
            received = b""
            while chunk := connection.recv(65536):
                received += chunk
        head, _, body = received.partition(b"\r\n\r\n")
        lines = head.decode("latin-1").split("\r\n")
        headers = {}
        for line in lines[1:]:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        self.visits.append({
            "status": int(lines[0].split(" ")[1]) if lines and lines[0] else None,
            "headers": {name: headers.get(name) for name in ("content-type", "cache-control", "connection")},
            "page": digest(body.decode("utf-8", "replace")),
        })

    def settle(self) -> list[dict[str, Any]]:
        for thread in self.threads:
            thread.join(timeout=15)
        self.threads = []
        visits, self.visits = self.visits, []
        return visits


def revision_generation(revision: str | None) -> int | None:
    if revision is None:
        return None
    return int(revision.rsplit(":", 1)[1])


def error_kind(error: BaseException) -> str:
    import keyring.errors

    from vibe.core.auth import mcp_oauth

    for name, kind in (
        ("MCPOAuthLoginFailed", "login_failed"),
        ("MCPOAuthInvalidGrant", "invalid_grant"),
        ("MCPOAuthTransientRefreshError", "transient_refresh"),
        ("MCPOAuthPortInUse", "port_in_use"),
        ("MCPOAuthHeadlessError", "headless"),
        ("MCPOAuthCredentialCleanupFailed", "cleanup_failed"),
        ("MCPOAuthCredentialRestoreFailed", "restore_failed"),
        ("MCPOAuthError", "callback"),
    ):
        if isinstance(error, getattr(mcp_oauth, name)):
            return kind
    if isinstance(error, ValueError):
        return "not_oauth"
    if isinstance(error, keyring.errors.KeyringError):
        return "keyring"
    return f"unexpected:{type(error).__name__}"


class Scenario:
    def __init__(self, spec: dict[str, Any], fixture: Path, root: Path) -> None:
        self.spec = spec
        self.root = root
        self.redirect = free_port()
        script = json.loads(json.dumps(spec["script"]).replace("{redirect}", str(self.redirect)))
        (root / "script.json").write_text(json.dumps(script), encoding="utf-8")
        self.log = root / "log.jsonl"
        port_file = root / "port"
        environment = {
            **os.environ,
            "VIBE_OAUTH_SCRIPT": str(root / "script.json"),
            "VIBE_OAUTH_LOG": str(self.log),
            "VIBE_OAUTH_PORT_FILE": str(port_file),
        }
        self.process = subprocess.Popen([str(fixture)], env=environment)
        deadline = time.monotonic() + 10
        while not port_file.exists():
            if time.monotonic() > deadline:
                raise OracleError("the OAuth fixture never bound a port")
            time.sleep(0.01)
        self.address = port_file.read_text(encoding="utf-8").strip()
        self.base = f"http://{self.address}"

    def server(self) -> Any:
        from vibe.core.config import MCPStreamableHttp

        fields = {**self.spec["server"], "name": "demo", "url": f"{self.base}/mcp"}
        auth = fields.get("auth", {})
        if auth.get("type") == "oauth":
            fields["auth"] = {**auth, "redirect_port": self.redirect}
        return MCPStreamableHttp.model_validate(fields)

    def seed(self, backend: Any, server: Any) -> None:
        from vibe.core.auth.mcp_oauth import Fingerprint

        keyring = self.spec["keyring"]
        if not isinstance(keyring, dict):
            return
        for kind, value in keyring.items():
            if value is None:
                continue
            if kind == "tokens":
                value = dict(value)
                if isinstance(value.get("expires_at"), str):
                    value["expires_at"] = time.time() + float(value["expires_at"])
                text = json.dumps(value)
            elif kind == "fingerprint":
                current = Fingerprint.compute(server)
                if value == "stale":
                    current = Fingerprint(url=f"{self.base}/elsewhere", scopes_sorted=(), client_marker="<dcr>")
                text = current.model_dump_json()
            else:
                text = json.dumps(value).replace("{redirect}", str(self.redirect))
            backend.set_password(SERVICE, f"mcp-oauth:demo:{kind}", text)

    def normalize(self, value: Any) -> Any:
        if isinstance(value, str):
            return value.replace(self.base, BASE_PLACEHOLDER).replace(self.address, "__BASE__").replace(
                f"127.0.0.1:{self.redirect}", f"127.0.0.1:{REDIRECT_PLACEHOLDER}"
            )
        if isinstance(value, list):
            return [self.normalize(item) for item in value]
        if isinstance(value, dict):
            return {self.normalize(key): self.normalize(item) for key, item in value.items()}
        return value

    def keyring_state(self, backend: Any) -> Any:
        entries = getattr(backend, "entries", None)
        if entries is None:
            return "headless"
        state = {}
        for (service, account), text in sorted(entries.items()):
            try:
                value = json.loads(text)
            except ValueError:
                value = text
            if isinstance(value, dict) and isinstance(value.get("expires_at"), (int, float)):
                value["expires_at"] = "__TIME__"
            state[f"{service}|{account}"] = self.normalize(value)
        return state

    def wire(self) -> list[dict[str, Any]]:
        if not self.log.exists():
            return []
        entries = [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines() if line]
        for entry in entries:
            entry["query"] = [[name, mask_parameter(name, value)] for name, value in entry["query"]]
            body = entry["body"]
            if isinstance(body, list):
                entry["body"] = [[name, mask_parameter(name, value)] for name, value in body]
            elif isinstance(body, dict) and isinstance(body.get("params"), dict):
                client = body["params"].get("clientInfo")
                if isinstance(client, dict) and "version" in client:
                    client["version"] = "__VERSION__"
        return self.normalize(entries)

    def close(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def mask_parameter(name: str, value: str) -> str:
    return {"state": "__STATE__", "code_challenge": "__CHALLENGE__", "code_verifier": "__VERIFIER__"}.get(name, value)


def pkce_pairs(urls: list[str], raw: list[dict[str, Any]]) -> list[str]:
    """Whether each token exchange proved the challenge its authorization URL carried."""

    challenges = [dict(parse_qsl(urlsplit(url).query)).get("code_challenge") for url in urls]
    verifiers = [
        dict(entry["body"]).get("code_verifier")
        for entry in raw
        if isinstance(entry["body"], list) and dict(entry["body"]).get("grant_type") == "authorization_code"
    ]
    checks = []
    for challenge, verifier in zip(challenges, verifiers, strict=False):
        computed = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        checks.append("valid" if computed == challenge and len(verifier) == 128 else "invalid")
    return checks


def authorization_url(url: str) -> dict[str, Any]:
    split = urlsplit(url)
    return {
        "endpoint": f"{split.scheme}://{split.netloc}{split.path}",
        "parameters": [[name, mask_parameter(name, value)] for name, value in parse_qsl(split.query, keep_blank_values=True)],
    }


async def run_scenario(spec: dict[str, Any], fixture: Path) -> dict[str, Any]:
    from vibe.app_server._mcp_auth import MCPAuthenticationService

    root = Path(tempfile.mkdtemp(prefix="vibe-oauth-parity-"))
    scenario = Scenario(spec, fixture, root)
    busy: socket.socket | None = None
    try:
        backend = install_keyring(spec["keyring"] == "headless")
        server = scenario.server()
        scenario.seed(backend, server)
        service = MCPAuthenticationService()
        await service.bind_catalog([server])
        browser = Browser(spec["browser"], scenario.redirect)
        if spec["browser"] == "busy":
            busy = socket.socket()
            busy.bind(("127.0.0.1", scenario.redirect))
            busy.listen()
        urls: list[str] = []
        all_urls: list[str] = []
        steps = []
        last_snapshot: Any = None

        async def on_url(url: str) -> None:
            urls.append(url)
            all_urls.append(url)
            browser.visit(url)

        for step in spec["steps"]:
            observed: dict[str, Any] = {"do": step["do"]}
            try:
                if step["do"] == "login":
                    revision = await asyncio.wait_for(service.login("demo", on_url=on_url), STEP_TIMEOUT)
                    observed["outcome"] = "ok"
                    observed["descriptor"] = revision_generation(revision)
                elif step["do"] == "logout":
                    revision = await service.logout("demo")
                    observed["outcome"] = "ok"
                    observed["descriptor"] = revision_generation(revision)
                elif step["do"] == "resolve":
                    result = await asyncio.wait_for(service.resolve(service.reference_for(server)), STEP_TIMEOUT)
                    observed["result"] = describe(result)
                    if hasattr(result, "headers"):
                        last_snapshot = result
                elif step["do"] == "reject":
                    result = await service.reject(
                        service.reference_for(server),
                        observed_connection_revision=last_snapshot.connection_revision if last_snapshot else "none",
                        reason="http_unauthorized",
                    )
                    observed["result"] = describe(result)
            except TimeoutError:
                observed["outcome"] = {"error": "timeout"}
            except Exception as error:  # noqa: BLE001 - the class is the observation
                observed["outcome"] = {"error": error_kind(error)}
            observed["urls"] = [authorization_url(url) for url in urls]
            urls.clear()
            observed["browser"] = browser.settle()
            observed["keyring"] = scenario.keyring_state(backend)
            steps.append(scenario.normalize(observed))
        raw = [json.loads(line) for line in scenario.log.read_text(encoding="utf-8").splitlines() if line] if scenario.log.exists() else []
        return {
            "steps": steps,
            "pkce": pkce_pairs(all_urls, raw),
            "wire": scenario.wire(),
        }
    finally:
        if busy is not None:
            busy.close()
        scenario.close()
        shutil.rmtree(root, ignore_errors=True)


def describe(result: Any) -> dict[str, Any]:
    if hasattr(result, "headers"):
        return {
            "snapshot": {
                "headers": dict(result.headers),
                "connection": revision_generation(result.connection_revision),
                "descriptor": revision_generation(result.descriptor_revision),
            }
        }
    return {
        "required": {
            "reason": result.reason,
            "descriptor": revision_generation(result.descriptor_revision),
            "observed": revision_generation(result.observed_connection_revision),
        }
    }


def capture(reference: dict[str, str], fixture: Path) -> dict[str, Any]:
    import logging

    # The SDK logs every refused flow with a traceback; the class is recorded.
    logging.disable(logging.CRITICAL)
    scenarios = []
    for spec in all_scenarios():
        first = asyncio.run(run_scenario(spec, fixture))
        second = asyncio.run(run_scenario(spec, fixture))
        if first != second:
            if os.environ.get("VIBE_OAUTH_PARITY_DUMP"):
                Path(os.environ["VIBE_OAUTH_PARITY_DUMP"]).write_text(json.dumps([first, second], indent=1, sort_keys=True), encoding="utf-8")
            raise OracleError(f"scenario {spec['name']} is not deterministic across two captures")
        scenarios.append({"name": spec["name"], "spec": copy.deepcopy(spec), "observed": first})
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Captured by scripts/parity/mcp_oauth.py from the pinned reference against "
            "vibe-oauth-script-fixture. Scenario inputs, requests, names and digests only: "
            "the reference's callback pages are a length and a SHA-256."
        ),
        "service": SERVICE,
        "scenarios": scenarios,
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def build_fixture() -> Path:
    result = subprocess.run(
        ["cargo", "build", "-p", "vibe-app-server", "--features", "test-fixtures", "--bin", "vibe-oauth-script-fixture", "--message-format", "json"],
        cwd=REPOSITORY,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(f"cannot build the OAuth fixture: {result.stderr[-2000:]}")
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            if message["target"]["name"] == "vibe-oauth-script-fixture":
                return Path(message["executable"])
    raise OracleError("cargo built no vibe-oauth-script-fixture executable")


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument("--fixture", type=Path, default=None)
    parser.add_argument("--corpus", type=Path, nargs="?", const=DEFAULT_CORPUS, default=None)
    parser.add_argument("--output", type=Path, default=None, help="write the capture here instead of the committed corpus")
    parser.add_argument("--check", action="store_true", help="recapture and fail when the committed corpus differs")
    parser.add_argument("--only", default=None, help="capture only scenarios whose name starts so")
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if not os.environ.get(FIXTURE_VARIABLE):
            fixture = (arguments.fixture or build_fixture()).resolve()
            os.environ[FIXTURE_VARIABLE] = str(fixture)
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(arguments.reference, reference["commit"], arguments.cache)
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        fixture = Path(os.environ[FIXTURE_VARIABLE])
        if arguments.only:
            global all_scenarios  # noqa: PLW0603
            everything = all_scenarios
            all_scenarios = lambda: [s for s in everything() if s["name"].startswith(arguments.only)]  # noqa: E731
        corpus = capture(reference, fixture)
    except OracleError as error:
        print(f"MCP OAuth capture failed: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps(corpus, indent=1, sort_keys=True, ensure_ascii=False) + "\n"
    if arguments.check:
        committed = (REPOSITORY / DEFAULT_CORPUS).read_text(encoding="utf-8")
        if committed != rendered:
            print("the committed corpus differs from a fresh capture", file=sys.stderr)
            return 1
        print(f"corpus matches a fresh capture of {len(corpus['scenarios'])} scenarios")
        return 0
    target = arguments.output or arguments.corpus
    if target is None:
        sys.stdout.write(rendered)
        return 0
    target = target if target.is_absolute() else REPOSITORY / target
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(rendered, encoding="utf-8")
    print(f"captured {len(corpus['scenarios'])} scenarios from {reference['commit'][:12]} into {target}")
    print(f"python {platform.python_version()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
