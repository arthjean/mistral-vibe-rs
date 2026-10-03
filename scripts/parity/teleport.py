#!/usr/bin/env python3
"""Black-box capture of Teleport and the Vibe Code project picker.

Every scenario serves an app server over stdio in a fresh home and a fresh Git
checkout, behind one scripted stand-in that plays every endpoint row 22 of
``docs/parity.md`` reaches:

- ``POST /v1/chat/completions``, the turns that give a session its history and
  the call that summarizes it for Teleport;
- ``GET /api/vibe/whoami``, the account the Teleport gate reads;
- ``GET`` and ``POST /api/v1/code/projects``, the project listing and creation
  the picker drives;
- ``POST /api/v1/code/sessions``, the Vibe Code Web start, which a scenario can
  answer, refuse, delay, drop or hold until a later step releases it;
- ``POST /v1/datalake/events``, where the Teleport telemetry lands.

Each checkout has an ``origin`` naming a GitHub repository whose fetches fail
fast through a dead global proxy, so nothing reaches the network, and whose
pushes land in a local bare repository through ``remote.origin.pushurl``.

What is recorded:

- ``gate``: who may open a Teleport picker, under which model, account and
  credential, and with what history;
- ``picker``: what ``vibeCode/projects/*`` answer, in which order, with which
  errors, and which saved link they leave;
- ``run``: the events a Teleport run publishes, in which order relative to the
  start's answer, through each push decision and Git state;
- ``summary``: the summarization call a session with history makes and the
  message context the start carries;
- ``nuage``: the start request on the wire, its retries and how each answer is
  reported;
- ``lifecycle``: cancellation, concurrency, the execution slot and session stop;
- ``git``: hostile repository configuration and remote spellings;
- ``telemetry``: the ``vibe.teleport_*`` and ``vibe.remote_project_configured``
  events the server sends.

Nothing is imported from the reference: its own ``vibe-app-server`` is the
oracle, which is what makes the same scenarios replayable against this port by
``crates/vibe-app-server/tests/teleport_parity_tests.rs``. Every string a server
authored is reduced to its length and SHA-256, which is what ``NOTICE``
requires of a committed corpus.

Usage::

    python3 scripts/parity/teleport.py                  # capture the reference
    python3 scripts/parity/teleport.py --check          # recapture and compare
    python3 scripts/parity/teleport.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import copy
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any
from urllib.parse import parse_qsl, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
import agents  # noqa: E402
import hooks  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
import rewind  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/teleport-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.6
EVENT_TIMEOUT = 40.0
API_KEY = "oracle-key"
GITHUB_URL = "https://github.com/oracle/repo.git"
#: A proxy nothing listens on, so a fetch of the GitHub remote fails at once.
DEAD_PROXY = "http://127.0.0.1:9"

TERMINAL_KINDS = {"complete", "failed"}
TELEMETRY_EVENTS = {
    "vibe.teleport_completed",
    "vibe.teleport_failed",
    "vibe.remote_project_configured",
}


# --------------------------------------------------------------------------
# The stand-in
# --------------------------------------------------------------------------


def project(identifier: str, name: str | None = None, repos: list[str] | None = None,
            read_only: bool = False, branch: str | None = "main") -> dict[str, Any]:
    entry: dict[str, Any] = {
        "id": identifier,
        "name": name or identifier,
        "repositories": [
            {"repoUrl": url, **({"defaultBranch": branch} if branch else {})}
            for url in (repos if repos is not None else [GITHUB_URL])
        ],
    }
    if read_only:
        entry["isReadOnly"] = True
    return entry


def ok(body: Any) -> dict[str, Any]:
    return {"status": 200, "body": body}


def page(*projects: dict[str, Any], cursor: str | None = None) -> dict[str, Any]:
    body: dict[str, Any] = {"items": list(projects)}
    if cursor is not None:
        body["nextCursor"] = cursor
    return ok(body)


def started(url: str = "https://chat.mistral.ai/code/session-1",
            project_id: str = "proj-1") -> dict[str, Any]:
    return ok({
        "sessionId": "nuage-session",
        "webSessionId": "web-session",
        "projectId": project_id,
        "status": "running",
        "url": url,
    })


class Backend(hooks.Backend):
    """The hooks stand-in, also playing the account, the Vibe Code API and the
    datalake, and logging every request they receive."""

    def __init__(self, code: dict[str, Any]) -> None:
        super().__init__()
        self.pages: dict[str, Any] = json.loads(json.dumps(code.get("pages", {"": page()})))
        self.creates: list[Any] = list(code.get("create", []))
        self.starts: list[Any] = list(code.get("start", [started()]))
        self.whoami: Any = code.get("whoami", ok({"plan_type": "API", "plan_name": "Scale"}))
        self.code_log: list[dict[str, Any]] = []
        self.telemetry: list[dict[str, Any]] = []
        self.release = threading.Event()
        handler = self.server.RequestHandlerClass
        backend = self
        chat = handler.do_POST

        def authorized(self: Any) -> bool:
            return self.headers.get("authorization") == f"Bearer {API_KEY}"

        def do_get(self: Any) -> None:
            parts = urlsplit(self.path)
            if parts.path == "/api/v1/code/projects":
                query = dict(parse_qsl(parts.query))
                with backend.lock:
                    backend.code_log.append({
                        "method": "GET", "path": parts.path,
                        "query": sorted(parse_qsl(parts.query)),
                        "authorized": authorized(self),
                    })
                    answer = backend.pages.get(query.get("cursor", ""), page())
                    # A list answers in order, its last answer repeating.
                    if isinstance(answer, list):
                        answer = answer.pop(0) if len(answer) > 1 else answer[0]
                backend.answer(self, answer)
                return
            if parts.path == "/api/vibe/whoami":
                backend.answer(self, backend.whoami)
                return
            self.reply(404, {"error": "not found"})

        def do_post(self: Any) -> None:
            parts = urlsplit(self.path)
            if parts.path.startswith("/api/v1/code/") or parts.path == "/v1/datalake/events":
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw or b"{}")
                except json.JSONDecodeError:
                    body = {"unparsed": raw.decode("utf-8", "replace")}
                if parts.path == "/v1/datalake/events":
                    with backend.lock:
                        backend.telemetry.append(body)
                    self.reply(200, {})
                    return
                with backend.lock:
                    backend.code_log.append({
                        "method": "POST", "path": parts.path,
                        "authorized": authorized(self),
                        "contentType": self.headers.get("content-type"),
                        "body": body,
                    })
                    if parts.path == "/api/v1/code/projects":
                        answer = backend.creates.pop(0) if backend.creates else {"status": 500, "body": {"detail": "no"}}
                    elif parts.path == "/api/v1/code/sessions":
                        answer = backend.starts.pop(0) if len(backend.starts) > 1 else (backend.starts[0] if backend.starts else started())
                    else:
                        answer = {"status": 404, "body": {"detail": "no"}}
                backend.answer(self, answer)
                return
            chat(self)

        handler.do_GET = do_get
        handler.do_POST = do_post

    def answer(self, handler: Any, answer: Any) -> None:
        if answer.get("block"):
            self.release.wait(timeout=EVENT_TIMEOUT)
        if answer.get("delay"):
            time.sleep(float(answer["delay"]))
        if answer.get("drop"):
            # The connection closes with no response at all.
            handler.close_connection = True
            try:
                handler.connection.shutdown(2)
            except OSError:
                pass
            return
        status = int(answer.get("status", 200))
        if "raw" in answer:
            payload = str(answer["raw"]).encode()
            handler.send_response(status)
            handler.send_header("content-type", "text/plain")
            handler.send_header("content-length", str(len(payload)))
            handler.end_headers()
            handler.wfile.write(payload)
            return
        try:
            handler.reply(status, answer.get("body", {}))
        except (BrokenPipeError, ConnectionResetError):
            pass


# --------------------------------------------------------------------------
# The checkout
# --------------------------------------------------------------------------


def git_env(world: acp.World) -> dict[str, str]:
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(world.home),
        "GIT_CONFIG_NOSYSTEM": "1",
        "LANG": "C.UTF-8",
        "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
        "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
    }


def run_git(world: acp.World, *args: str, cwd: Path | None = None, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args], cwd=cwd or world.workspace, env=git_env(world),
        capture_output=True, text=True, check=False,
    )
    if check and result.returncode != 0:
        raise rewind.OracleError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def prepare_git(world: acp.World, spec: dict[str, Any] | None) -> None:
    (world.home / ".gitconfig").write_text(
        "[user]\n\tname = Oracle\n\temail = oracle@example.com\n"
        "[init]\n\tdefaultBranch = main\n"
        f"[http]\n\tproxy = {DEAD_PROXY}\n"
        "[advice]\n\tdetachedHead = false\n",
        encoding="utf-8",
    )
    if spec is None:
        return
    for name, contents in spec.get("files", {"README.md": "hello\n"}).items():
        path = world.workspace / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    run_git(world, "init", "-q")
    if spec.get("noCommits"):
        pass
    else:
        run_git(world, "add", "-A")
        run_git(world, "commit", "-q", "-m", "initial")
    bare = world.root / "origin.git"
    run_git(world, "init", "-q", "--bare", str(bare), cwd=world.root)
    remotes = spec.get("remotes", {"origin": GITHUB_URL})
    for name, url in remotes.items():
        run_git(world, "remote", "add", name, url)
        run_git(world, "config", f"remote.{name}.pushurl", str(bare))
    if not spec.get("noCommits") and "origin" in remotes and spec.get("published", True):
        run_git(world, "push", "-q", str(bare), "main")
        run_git(world, "update-ref", "refs/remotes/origin/main", "HEAD")
        if spec.get("originHead", True):
            run_git(world, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main")
    if spec.get("pushFails"):
        hook = bare / "hooks" / "pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        hook.chmod(0o755)
    if branch := spec.get("branch"):
        run_git(world, "checkout", "-q", "-b", branch)
        if spec.get("branchPublished"):
            run_git(world, "update-ref", f"refs/remotes/origin/{branch}", "HEAD")
    for index in range(int(spec.get("ahead", 0))):
        (world.workspace / f"change-{index}.txt").write_text(f"change {index}\n", encoding="utf-8")
        run_git(world, "add", "-A")
        run_git(world, "commit", "-q", "-m", f"change {index}")
    if other := spec.get("otherRemoteBranch"):
        run_git(world, "update-ref", f"refs/remotes/origin/{other}", "HEAD")
    for key, value in spec.get("config", {}).items():
        run_git(world, "config", key, value)
    for name, contents in spec.get("dirty", {}).items():
        if isinstance(contents, dict):
            # Incompressible text, generated rather than carried in the corpus.
            contents = "".join(
                hashlib.sha256(str(index).encode()).hexdigest() + "\n"
                for index in range(int(contents["random"]))
            )
        path = world.workspace / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    if spec.get("detached"):
        run_git(world, "checkout", "-q", "--detach")


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


def base_config(backend: Backend, scenario: dict[str, Any]) -> str:
    telemetry = "true" if scenario.get("telemetry") else "false"
    active = scenario.get("activeModel", "mistral-medium-3.5")
    return (
        f'active_model = "{active}"\n'
        f"enable_telemetry = {telemetry}\n"
        "enable_update_checks = false\n"
        f'console_base_url = "{backend.base}"\n'
        f'vibe_code_sessions_base_url = "{backend.base}"\n'
        f'vibe_base_url = "{backend.base}/vibe"\n'
        + scenario.get("config", "")
        + "\n[[providers]]\n"
        'name = "mistral"\n'
        f'api_base = "{backend.base}/v1"\n'
        f'api_key_env_var = "{scenario.get("keyVariable", "MISTRAL_API_KEY")}"\n'
        'backend = "mistral"\n'
        + scenario.get("tables", "")
    )


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = Backend(scenario.get("code", {}))
    root = Path(tempfile.mkdtemp(prefix="vibe-teleport-oracle-"))
    session = rewind.Session(root)
    world = session.world
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        prepare_git(world, scenario.get("git", {}))
        (world.vibe_home / "config.toml").write_text(base_config(backend, scenario), encoding="utf-8")
        (world.vibe_home / "trusted_folders.toml").write_text(
            f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n", encoding="utf-8"
        )
        env = {
            **git_env(world),
            "VIBE_HOME": str(world.vibe_home),
            "MISTRAL_API_KEY": API_KEY,
            "VIBE_API_BASE": f"{backend.base}/v1/chat/completions",
            "TERM": "dumb",
            "NO_COLOR": "1",
            "CI": "true",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
            # The port's fixture delivers the events its server raises only
            # when asked; the reference ignores the variable.
            "VIBE_ORACLE_TELEMETRY": "1",
        }
        for key, value in scenario.get("env", {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        server = agents.Server(command, env, world.workspace, {}, world, callbacks=[])
        steps: list[dict[str, Any]] = []
        state: dict[str, Any] = {"picker": None}
        try:
            run_steps(scenario, server, session, backend, steps, quiet, state)
        except (rewind.OracleError, acp.OracleError) as error:
            steps.append({"failure": str(error)})
        finally:
            backend.release.set()
            server.stop()
        # Telemetry is sent in the background; give the last of it a moment.
        if scenario.get("telemetry"):
            time.sleep(quiet)
        return {
            "steps": steps,
            "sessions": session.sessions,
            "requests": list(backend.bodies),
            "code": list(backend.code_log),
            "telemetry": list(backend.telemetry),
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
        backend.release.set()
        backend.close()
        shutil.rmtree(root, ignore_errors=True)


def substitute(value: Any, session: rewind.Session, state: dict[str, Any]) -> Any:
    value = session.substitute(value)
    if isinstance(value, dict):
        return {key: substitute(item, session, state) for key, item in value.items()}
    if isinstance(value, list):
        return [substitute(item, session, state) for item in value]
    if isinstance(value, str) and "$PICKER" in value and state.get("picker"):
        return value.replace("$PICKER", state["picker"])
    return value


def learn(message: dict[str, Any], session: rewind.Session, state: dict[str, Any]) -> None:
    session.learn(message)
    result = message.get("result")
    if isinstance(result, dict) and isinstance(result.get("pickerId"), str):
        state["picker"] = result["pickerId"]


def teleport_event(message: dict[str, Any]) -> dict[str, Any] | None:
    if message.get("method") != "vibeCode/teleport/event":
        return None
    return (message.get("params") or {}).get("event")


def collect_teleport(server: rewind.Server, request_id: Any, stop: set[str],
                     quiet: float) -> list[dict[str, Any]]:
    """Everything the server writes until the request is answered and an event
    of a stopping kind arrived, then silence."""

    messages: list[dict[str, Any]] = []
    answered = request_id is None
    stopped = False
    deadline = time.monotonic() + EVENT_TIMEOUT
    while True:
        settled = answered and stopped
        timeout = quiet if settled else max(0.0, deadline - time.monotonic())
        try:
            message = server.inbox.get(timeout=timeout)
        except queue.Empty:
            if settled or time.monotonic() >= deadline:
                if not settled:
                    messages.append({"timeout": True})
                return messages
            continue
        if message is None:
            messages.append({"exited": True})
            return messages
        messages.append(message)
        if message.get("id") == request_id and "method" not in message:
            answered = True
            # A refused request starts nothing to wait for.
            if "error" in message:
                stopped = True
        event = teleport_event(message)
        if event is not None and event.get("kind") in stop:
            stopped = True


def sequence(messages: list[dict[str, Any]], request_id: Any) -> list[Any]:
    """The messages a teleport step saw, in order: its answer, each Teleport
    event, and the other notifications by method."""

    observed: list[Any] = []
    for message in messages:
        if message.get("timeout") or message.get("exited"):
            observed.append(message)
            continue
        if message.get("id") == request_id and "method" not in message:
            observed.append({"response": {k: v for k, v in message.items() if k not in {"jsonrpc", "id"}}})
            continue
        if (event := teleport_event(message)) is not None:
            observed.append({"event": event})
            continue
        method = message.get("method")
        if method in {"turn/started", "turn/completed", "turn/failed"}:
            observed.append({"notification": method})
    return observed


def run_steps(
    scenario: dict[str, Any],
    server: rewind.Server,
    session: rewind.Session,
    backend: Backend,
    steps: list[dict[str, Any]],
    quiet: float,
    state: dict[str, Any],
) -> None:
    world = session.world
    identifier = 1
    server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                 "params": {"clientInfo": {"name": "teleport-oracle", "version": "0"},
                            "capabilities": {"callbackKinds": ["approval", "user_input"]}}})
    server.collect(identifier, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    for step in scenario["steps"]:
        identifier += 1
        if "start" in step:
            config = {"cwd": str(world.workspace), "agent": step["start"].get("agent", "auto-approve")}
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start",
                         "params": {"agentConfig": config}})
            observed = server.collect(identifier, quiet)
            for message in observed:
                learn(message, session, state)
            response = next((m for m in observed if m.get("id") == identifier and "method" not in m), None)
            steps.append({"start": {"error": response["error"]} if response and "error" in response
                          else {"started": response is not None}})
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": rewind.render_turn("reference", target, step["turn"])})
            if step.get("background"):
                time.sleep(step.get("settle", 0.3))
                observed = server.collect(None, quiet)
                for message in observed:
                    learn(message, session, state)
                steps.append({"turn": "background"})
                continue
            observed = server.wait_for(rewind.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                learn(message, session, state)
            status = next(
                (m.get("method") for m in observed
                 if m.get("method") in {"turn/completed", "turn/failed", "turn/interrupted"}),
                None,
            )
            steps.append({"turn": status})
            continue
        if "teleport" in step or "respond" in step or "await" in step:
            stop = set(step.get("until", TERMINAL_KINDS))
            if "teleport" in step:
                params = substitute(step["teleport"], session, state)
                params.setdefault("sessionId", session.substitute("$S1"))
                params.setdefault("pickerId", state.get("picker"))
                params.setdefault("operationId", "op-1")
                server.send({"jsonrpc": "2.0", "id": identifier,
                             "method": "vibeCode/teleport/start", "params": params})
                request_id: Any = identifier
                label = "vibeCode/teleport/start"
            elif "respond" in step:
                server.send({"jsonrpc": "2.0", "id": identifier,
                             "method": "vibeCode/teleport/push/respond",
                             "params": {"sessionId": session.substitute("$S1"),
                                        "operationId": step.get("operationId", "op-1"),
                                        "approved": step["respond"]}})
                request_id = identifier
                label = "vibeCode/teleport/push/respond"
            else:
                request_id = None
                label = "await"
            messages = collect_teleport(server, request_id, stop, quiet)
            for message in messages:
                if "method" in message or "id" in message:
                    learn(message, session, state)
            steps.append({"method": label, "sequence": sequence(messages, request_id)})
            continue
        if "release" in step:
            backend.release.set()
            continue
        if "sleep" in step:
            time.sleep(float(step["sleep"]))
            continue
        if "checkout" in step:
            steps.append({"checkout": checkout_view(world, step["checkout"])})
            continue
        if "git" in step:
            for command in step["git"]:
                run_git(world, *command)
            continue
        request = substitute(step["send"], session, state)
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            learn(message, session, state)
        response = next((m for m in observed if m.get("id") == identifier and "method" not in m), None)
        steps.append({
            "method": request["method"],
            "response": {k: v for k, v in (response or {"missing": True}).items() if k not in {"jsonrpc", "id"}},
            "events": [event for event in (teleport_event(m) for m in observed) if event is not None],
        })


def checkout_view(world: acp.World, spec: dict[str, Any]) -> dict[str, Any]:
    """What a step asks of the checkout: refs and marker files."""

    view: dict[str, Any] = {}
    head = run_git(world, "rev-parse", "HEAD", check=False)
    for ref in spec.get("refs", []):
        value = run_git(world, "rev-parse", "--verify", "--quiet", ref, check=False)
        view[ref] = None if not value else ("HEAD" if value == head else "other")
    for marker in spec.get("markers", []):
        view[marker] = (world.root / marker).exists()
    if spec.get("bare"):
        bare = world.root / "origin.git"
        view["bareBranches"] = sorted(
            line.split()[-1] for line in run_git(world, "for-each-ref", "--format=%(refname)",
                                                 "refs/heads", cwd=bare, check=False).splitlines()
        )
    if spec.get("index"):
        view["status"] = run_git(world, "status", "--porcelain", check=False).splitlines()
    return view


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: Identifier keys this surface adds to the shared set.
TELEPORT_ID_KEYS = {"pickerId", "idempotencyKey", "conversationId"}


def decode_diff(content: str) -> dict[str, Any]:
    """A diff as the start carries it: the frame header's content-size flag and
    the patch it decompresses to, reduced to a digest."""

    try:
        raw = base64.b64decode(content, validate=True)
    except (ValueError, TypeError):
        return {"invalid": "base64"}
    try:
        import zstandard  # the reference interpreter carries it

        decompressed = zstandard.ZstdDecompressor().decompress(raw, max_output_size=64 * 1024 * 1024)
    except Exception:  # noqa: BLE001
        try:
            decompressed = subprocess.run(
                ["zstd", "-d", "-c"], input=raw, capture_output=True, check=True
            ).stdout
        except (OSError, subprocess.CalledProcessError):
            return {"invalid": "zstd"}
    text = decompressed.decode("utf-8", "replace")
    return {
        "bytes": len(decompressed),
        "sha256": hashlib.sha256(decompressed).hexdigest(),
        "files": sorted({line[6:] for line in text.splitlines() if line.startswith("+++ b/")}),
    }


def code_view(entry: dict[str, Any]) -> dict[str, Any]:
    view = {k: v for k, v in entry.items() if k != "body"}
    body = copy.deepcopy(entry.get("body"))
    if isinstance(body, dict) and entry["path"] == "/api/v1/code/sessions":
        for repository in (body.get("context") or {}).get("repositories") or []:
            diff = repository.get("diff") if isinstance(repository, dict) else None
            if isinstance(diff, dict) and isinstance(diff.get("content"), str):
                diff["content"] = decode_diff(diff["content"])
    if body is not None:
        view["body"] = body
    return view


def telemetry_view(body: Any) -> list[dict[str, Any]]:
    """The Teleport events a datalake post carries, with their properties."""

    events = body if isinstance(body, list) else [body]
    found = []
    for event in events:
        if not isinstance(event, dict):
            continue
        name = event.get("event") or event.get("name")
        if name not in TELEMETRY_EVENTS:
            continue
        properties = event.get("properties") or {}
        keep = {
            key: properties.get(key)
            for key in sorted(properties)
            if key in {
                "stage", "error_class", "push_required", "nb_session_messages",
                "context_summary", "context_summary_chars", "failure_kind",
                "http_status_code", "outcome", "project_picker_shown",
                "project_selection_source", "project_candidate_count_loaded",
                "project_multi_repo_match_count", "saved_project_link_cleared",
                "project_repo_remote_changed",
            }
        }
        found.append({"event": name, "properties": keep})
    return found


def summary_requests(bodies: list[Any]) -> list[Any]:
    """The model calls that summarize a session for Teleport: no tools, and the
    last message a user message the server wrote."""

    found = []
    for body in bodies:
        if not isinstance(body, dict) or "messages" not in body or body.get("tools"):
            continue
        found.append({
            "tools": len(body.get("tools") or []),
            "stream": bool(body.get("stream")),
            "conversation": hooks.transcript(body.get("messages", [])),
        })
    return found


class Normalizer(hooks.Normalizer):
    def value(self, value: Any, key: str | None = None) -> Any:
        if key in TELEPORT_ID_KEYS and isinstance(value, str) and value:
            if value in self.authored:
                return value
            return self.identity(value)
        return super().value(value, key)

    def collect_ids(self, value: Any, key: str | None = None) -> None:
        if key in TELEPORT_ID_KEYS and isinstance(value, str) and value:
            if value not in self.authored:
                self.identity(value)
            return
        super().collect_ids(value, key)


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    normalizer = Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    observed = {
        "steps": run["steps"],
        "code": [code_view(entry) for entry in run["code"]],
        "requests": summary_requests(run["requests"]),
        "telemetry": [event for body in run["telemetry"] for event in telemetry_view(body)],
    }
    normalizer.collect_ids(observed)
    return normalizer.value(observed)


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def send(method: str, **params: Any) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


def on(method: str, **params: Any) -> dict[str, Any]:
    return send(method, sessionId="$S1", **params)


def open_picker(purpose: str = "teleport", **params: Any) -> dict[str, Any]:
    return on("vibeCode/projects/open", purpose=purpose, **params)


def picker(method: str, **params: Any) -> dict[str, Any]:
    return on(f"vibeCode/projects/{method}", pickerId="$PICKER", **params)


def teleport(project_id: str = "proj-1", prompt: str | None = "Ship it", **extra: Any) -> dict[str, Any]:
    params: dict[str, Any] = {"projectId": project_id, **extra}
    if prompt is not None:
        params["prompt"] = prompt
    step: dict[str, Any] = {"teleport": params}
    return step


def until(step: dict[str, Any], *kinds: str) -> dict[str, Any]:
    return {**step, "until": list(kinds)}


START = {"start": {}}
#: A turn that gives the session a history.
HISTORY_TURN = {"turn": "Fix the failing test"}

#: Selecting the scenario's single project through a configure picker leaves
#: a saved link, which is how a later Teleport picker resolves its project.
def linked(project_id: str = "proj-1") -> list[dict[str, Any]]:
    return [open_picker("configure"), picker("select", projectId=project_id)]


ONE_PROJECT = {"pages": {"": page(project("proj-1", "Repo"))}}

#: An active model on a provider of its own, with the Mistral provider second.
OTHER_TABLES = (
    "\n[[providers]]\n"
    'name = "other"\n'
    'api_base = "http://127.0.0.1:9/v1"\n'
    'api_key_env_var = "OTHER_KEY"\n'
    'api_style = "openai"\n'
    "\n[[models]]\n"
    'name = "other-model"\n'
    'provider = "other"\n'
    'alias = "other"\n'
)
OTHER_MODEL: dict[str, Any] = {
    "activeModel": "other",
    "tables": OTHER_TABLES,
    "env": {"OTHER_KEY": "other"},
}


def scenarios() -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []

    def add(name: str, steps: list[dict[str, Any]], **extra: Any) -> None:
        found.append({"name": name, "steps": steps, **extra})

    # ---------------------------------------------------------------- gate
    add("gate/no-history-no-prompt", [START, open_picker("teleport")], code=ONE_PROJECT)
    add("gate/prompt-without-history", [START, open_picker("teleport", prompt="Go")], code=ONE_PROJECT)
    add("gate/history-without-prompt", [START, HISTORY_TURN, open_picker("teleport")], code=ONE_PROJECT)
    add("gate/non-mistral-model", [START, open_picker("teleport", prompt="Go")], code=ONE_PROJECT,
        **OTHER_MODEL)
    add("gate/codestral-key", [START, open_picker("teleport", prompt="Go")],
        code={**ONE_PROJECT, "whoami": ok({"plan_type": "MISTRAL_CODE", "plan_name": "F"})})
    add("gate/unverified-key", [START, open_picker("teleport", prompt="Go")],
        code={**ONE_PROJECT, "whoami": {"status": 401, "body": {"detail": "no"}}})
    add("gate/account-unavailable", [START, open_picker("teleport", prompt="Go")],
        code={**ONE_PROJECT, "whoami": {"status": 500, "body": {"detail": "no"}}})
    add("gate/missing-key-configure", [START, open_picker("configure")], code=ONE_PROJECT,
        **{**OTHER_MODEL, "env": {"OTHER_KEY": "other", "MISTRAL_API_KEY": None,
                                  "VIBE_ORACLE_CREDENTIAL": "OTHER_KEY"}})
    add("gate/mistral-provider-second", [START, open_picker("configure")], code=ONE_PROJECT,
        **OTHER_MODEL)
    add("gate/provider-without-backend", [START, open_picker("teleport", prompt="Go")],
        code=ONE_PROJECT, activeModel="untyped",
        tables=OTHER_TABLES.replace('name = "other"', 'name = "untyped-provider"')
        .replace('provider = "other"', 'provider = "untyped-provider"')
        .replace('alias = "other"', 'alias = "untyped"'),
        env={"OTHER_KEY": "other"})
    add("gate/custom-key-variable", [START, open_picker("configure")], code=ONE_PROJECT,
        keyVariable="CUSTOM_KEY", env={"MISTRAL_API_KEY": None, "CUSTOM_KEY": API_KEY,
                                       "VIBE_ORACLE_CREDENTIAL": "CUSTOM_KEY"})
    add("gate/not-a-repository", [START, open_picker("configure")], code=ONE_PROJECT, git=None)
    add("gate/not-a-repository-teleport", [START, open_picker("teleport", prompt="Go")],
        code=ONE_PROJECT, git=None)
    add("gate/no-github-remote", [START, open_picker("configure")], code=ONE_PROJECT,
        git={"remotes": {"origin": "https://gitlab.com/oracle/repo.git"}})
    add("gate/no-remote", [START, open_picker("configure")], code=ONE_PROJECT,
        git={"remotes": {}})
    add("gate/no-commits", [START, open_picker("configure")], code=ONE_PROJECT,
        git={"noCommits": True})
    add("gate/detached-head", [START, open_picker("configure")], code=ONE_PROJECT,
        git={"detached": True})

    # -------------------------------------------------------------- picker
    add("picker/configure-unlinked", [START, open_picker("configure")], code={
        "pages": {"": page(project("proj-1", "Repo"), project("proj-2", "Other",
                                                               repos=["https://github.com/x/y.git"]),
                           cursor="c2")},
    })
    add("picker/teleport-unlinked", [START, open_picker("teleport", prompt="Go")], code={
        "pages": {"": page(project("proj-1", "Repo"), cursor="c2")},
    })
    add("picker/select-then-teleport-open", [START, *linked(), open_picker("teleport", prompt="Go")],
        code=ONE_PROJECT)
    add("picker/select-then-configure-open", [START, *linked(), open_picker("configure")],
        code=ONE_PROJECT)
    add("picker/remote-changed", [
        START, *linked(),
        {"git": [["remote", "set-url", "origin", "https://github.com/oracle/moved.git"]]},
        open_picker("teleport", prompt="Go"),
        open_picker("configure"),
    ], code=ONE_PROJECT)
    add("picker/load-more", [START, open_picker("configure"), picker("loadMore"), picker("loadMore")],
        code={"pages": {
            "": page(project("proj-1", "Repo"), cursor="c2"),
            "c2": page(project("proj-2", "Other", repos=["https://github.com/x/y.git"]), cursor="c3"),
            "c3": page(project("proj-3", "Third"), project("proj-1", "Repo")),
        }})
    add("picker/create-then-select", [
        START, open_picker("configure"),
        picker("create", name="  New project  ", defaultBranch=" main "),
        picker("select", projectId="proj-9"),
        open_picker("configure"),
    ], code={**ONE_PROJECT, "create": [ok(project("proj-9", "New project"))]})
    add("picker/create-blank", [START, open_picker("configure"),
                                picker("create", name="  ", defaultBranch="main"),
                                picker("create", name="x", defaultBranch=" ")], code=ONE_PROJECT)
    add("picker/create-refused", [START, open_picker("configure"),
                                  picker("create", name="x", defaultBranch="main")],
        code={**ONE_PROJECT, "create": [{"status": 409, "body": {"detail": "exists"}}]})
    add("picker/select-unknown", [START, open_picker("configure"), picker("select", projectId="nope")],
        code=ONE_PROJECT)
    add("picker/select-read-only", [START, open_picker("configure"), picker("select", projectId="proj-1")],
        code={"pages": {"": page(project("proj-1", "Repo", read_only=True))}})
    add("picker/select-other-repository", [START, open_picker("configure"),
                                           picker("select", projectId="proj-2")],
        code={"pages": {"": page(project("proj-2", "Other", repos=["https://github.com/x/y.git"]))}})
    add("picker/unlink", [START, *linked(), open_picker("configure"), picker("unlink"),
                          open_picker("teleport", prompt="Go")], code=ONE_PROJECT)
    add("picker/cancel", [START, open_picker("configure"), picker("cancel"), picker("select", projectId="proj-1")],
        code=ONE_PROJECT)
    add("picker/recover", [START, *linked(), open_picker("teleport", prompt="Go"), picker("recover"),
                           open_picker("configure")], code=ONE_PROJECT)
    add("picker/recover-api-failure", [START, *linked(), open_picker("teleport", prompt="Go"),
                                       picker("recover")],
        code={"pages": {"": [page(project("proj-1", "Repo")), {"status": 503, "body": {"detail": "down"}}]}})
    add("picker/stale-id", [START, open_picker("configure"), open_picker("configure"),
                            on("vibeCode/projects/select", pickerId="stale", projectId="proj-1")],
        code=ONE_PROJECT)
    add("picker/not-ready", [START, on("vibeCode/projects/loadMore", pickerId="nothing")], code=ONE_PROJECT)
    add("picker/list-failures", [START, open_picker("configure")],
        code={"pages": {"": {"status": 401, "body": {"detail": "bad key"}}}})
    add("picker/list-invalid-json", [START, open_picker("configure")],
        code={"pages": {"": {"status": 200, "raw": "not json"}}})
    add("picker/list-invalid-schema", [START, open_picker("configure")],
        code={"pages": {"": ok({"projects": []})}})
    add("picker/order-and-duplicates", [START, open_picker("configure"), picker("loadMore")], code={
        "pages": {
            "": page(project("zeta", "Zeta"), project("alpha", "Alpha"), cursor="c2"),
            "c2": page(project("alpha", "Alpha"), project("mid", "Mid")),
        },
    })
    add("picker/multi-repo", [START, open_picker("configure"), picker("select", projectId="multi")], code={
        "pages": {"": page(project("multi", "Multi", repos=[GITHUB_URL, "https://github.com/x/y.git"]))},
    })
    add("picker/ssh-remote", [START, open_picker("configure"), picker("select", projectId="proj-1")],
        code=ONE_PROJECT, git={"remotes": {"origin": "git@github.com:Oracle/Repo.git"}})

    # ----------------------------------------------------------------- run
    linked_run = [START, *linked()]
    add("run/already-pushed", [*linked_run, open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT)
    add("run/dirty-and-untracked", [*linked_run, open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, git={"dirty": {"README.md": "changed\n", "new.txt": "fresh\n"}})
    add("run/push-approved", [*linked_run, open_picker("teleport", prompt="Go"),
                              until(teleport(), "push_required", "failed", "complete"),
                              {"respond": True},
                              {"checkout": {"refs": ["refs/remotes/origin/main"], "bare": True}}],
        code=ONE_PROJECT, git={"ahead": 2})
    add("run/push-denied", [*linked_run, open_picker("teleport", prompt="Go"),
                            until(teleport(), "push_required", "failed", "complete"),
                            {"respond": False}],
        code=ONE_PROJECT, git={"ahead": 1})
    add("run/push-rejected", [*linked_run, open_picker("teleport", prompt="Go"),
                              until(teleport(), "push_required", "failed", "complete"),
                              {"respond": True}],
        code=ONE_PROJECT, git={"ahead": 1, "pushFails": True})
    add("run/new-branch", [*linked_run, open_picker("teleport", prompt="Go"),
                           until(teleport(), "push_required", "failed", "complete"),
                           {"respond": True},
                           {"checkout": {"refs": ["refs/remotes/origin/feature"], "bare": True}}],
        code=ONE_PROJECT, git={"branch": "feature", "ahead": 2})
    add("run/new-branch-without-origin-head", [*linked_run, open_picker("teleport", prompt="Go"),
                                               until(teleport(), "push_required", "failed", "complete")],
        code=ONE_PROJECT, git={"branch": "feature", "ahead": 1, "originHead": False})
    add("run/head-in-other-remote-branch", [*linked_run, open_picker("teleport", prompt="Go"),
                                            until(teleport(), "push_required", "failed", "complete")],
        code=ONE_PROJECT, git={"ahead": 1, "otherRemoteBranch": "elsewhere"})
    add("run/detached-head", [*linked_run, {"git": [["checkout", "-q", "--detach"]]},
                              open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT)
    add("run/configure-picker", [*linked_run, open_picker("configure"), teleport()], code=ONE_PROJECT)
    add("run/project-mismatch", [*linked_run, open_picker("teleport", prompt="Go"),
                                 teleport(project_id="proj-other")], code=ONE_PROJECT)
    add("run/unresolved-picker", [START, open_picker("teleport", prompt="Go"),
                                  picker("select", projectId="proj-1"), teleport()], code=ONE_PROJECT)
    add("run/no-picker", [START, teleport()], code=ONE_PROJECT)
    add("run/project-id-spaces", [*linked_run, open_picker("teleport", prompt="Go"),
                                  teleport(project_id=" proj-1 ")], code=ONE_PROJECT)

    # ------------------------------------------------------------- summary
    add("summary/history-with-prompt", [START, HISTORY_TURN, *linked(),
                                        open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, backend=[{"text": "Looked at it."}, {"text": "<summary>Fixing a test</summary>"}])
    add("summary/history-without-prompt", [START, HISTORY_TURN, *linked(),
                                           open_picker("teleport"), teleport(prompt=None)],
        code=ONE_PROJECT, backend=[{"text": "Looked at it."}, {"text": "<summary>Fixing a test</summary>"}])
    add("summary/plain-answer", [START, HISTORY_TURN, *linked(),
                                 open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, backend=[{"text": "Looked at it."}, {"text": "Fixing a test, nothing else."}])
    add("summary/empty-answer", [START, HISTORY_TURN, *linked(),
                                 open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, backend=[{"text": "Looked at it."}, {"text": ""}])
    add("summary/model-failure", [START, HISTORY_TURN, *linked(),
                                  open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, backend=[{"text": "Looked at it."},
                                   {"status": 400, "body": {"error": {"message": "bad request"}}}])
    add("summary/too-long", [START, HISTORY_TURN, *linked(),
                             open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, backend=[{"text": "Looked at it."}, {"text": "<summary>" + "x" * 8001 + "</summary>"}])

    # --------------------------------------------------------------- nuage
    run_steps_ = [*linked_run, open_picker("teleport", prompt="Go"), teleport()]
    add("nuage/request-shape", run_steps_, code=ONE_PROJECT,
        git={"dirty": {"src/app.py": "print('hi')\n"}})
    add("nuage/not-found", run_steps_,
        code={**ONE_PROJECT, "start": [{"status": 404, "body": {"detail": "Project not found"}}]})
    add("nuage/forbidden", run_steps_,
        code={**ONE_PROJECT, "start": [{"status": 403, "body": {"detail": "forbidden project"}}]})
    add("nuage/server-error", run_steps_,
        code={**ONE_PROJECT, "start": [{"status": 500, "body": {"detail": "boom"}}]})
    add("nuage/gateway-timeout-then-success", run_steps_,
        code={**ONE_PROJECT, "start": [{"status": 504, "body": {}}, started()]})
    add("nuage/gateway-timeout-exhausted", run_steps_,
        code={**ONE_PROJECT, "start": [{"status": 504, "body": {}}]})
    add("nuage/dropped-then-success", run_steps_,
        code={**ONE_PROJECT, "start": [{"drop": True}, started()]})
    add("nuage/invalid-json", run_steps_,
        code={**ONE_PROJECT, "start": [{"status": 200, "raw": "nope"}]})
    add("nuage/invalid-schema", run_steps_,
        code={**ONE_PROJECT, "start": [ok({"url": "https://x"})]})
    add("nuage/other-project", run_steps_,
        code={**ONE_PROJECT, "start": [started(project_id="proj-elsewhere")]})
    add("nuage/huge-diff", run_steps_, code=ONE_PROJECT,
        git={"dirty": {"big.txt": {"random": 30_000}}})

    # ----------------------------------------------------------- lifecycle
    blocked = {**ONE_PROJECT, "start": [{"block": True, **started()}]}
    add("lifecycle/answer-before-events", [*linked_run, open_picker("teleport", prompt="Go"),
                                           until(teleport(), "starting_workflow"),
                                           {"release": True}, {"await": True}], code=blocked)
    add("lifecycle/turn-during-teleport", [*linked_run, open_picker("teleport", prompt="Go"),
                                           until(teleport(), "starting_workflow"),
                                           on("turn/start", message=[{"type": "text", "text": "hi"}]),
                                           open_picker("configure"),
                                           {"release": True}, {"await": True}], code=blocked)
    add("lifecycle/second-start", [*linked_run, open_picker("teleport", prompt="Go"),
                                   until(teleport(), "starting_workflow"),
                                   teleport(operationId="op-2"),
                                   {"release": True}, {"await": True}], code=blocked)
    add("lifecycle/cancel-while-starting", [*linked_run, open_picker("teleport", prompt="Go"),
                                            until(teleport(), "starting_workflow"),
                                            on("vibeCode/teleport/cancel", operationId="op-1"),
                                            on("vibeCode/teleport/cancel", operationId="op-1"),
                                            {"release": True}, {"sleep": 0.5},
                                            on("vibeCode/projects/open", purpose="configure")],
        code=blocked)
    add("lifecycle/cancel-while-push-pending", [*linked_run, open_picker("teleport", prompt="Go"),
                                                until(teleport(), "push_required"),
                                                on("vibeCode/teleport/cancel", operationId="op-1"),
                                                {"respond": True, "until": []},
                                                {"checkout": {"bare": True}}],
        code=ONE_PROJECT, git={"ahead": 1})
    add("lifecycle/cancel-unknown", [START, on("vibeCode/teleport/cancel", operationId="nope")],
        code=ONE_PROJECT)
    add("lifecycle/respond-without-push", [*linked_run, open_picker("teleport", prompt="Go"), teleport(),
                                           {"respond": True, "until": []},
                                           on("vibeCode/teleport/push/respond", operationId="nope",
                                              approved=True)],
        code=ONE_PROJECT)
    add("lifecycle/operation-reuse", [*linked_run, open_picker("teleport", prompt="Go"), teleport(),
                                      open_picker("teleport", prompt="Go"), teleport()],
        code={**ONE_PROJECT, "start": [started(), started()]})
    add("lifecycle/picker-during-turn", [*linked_run, {"turn": "Slow", "background": True},
                                         open_picker("configure"),
                                         {"sleep": 2.5}],
        code=ONE_PROJECT, backend=[{"text": "Slow answer", "delay": 2}])
    add("lifecycle/stop-during-push", [*linked_run, open_picker("teleport", prompt="Go"),
                                       until(teleport(), "push_required"),
                                       on("session/stop"),
                                       {"sleep": 0.5},
                                       {"checkout": {"bare": True}}],
        code=ONE_PROJECT, git={"ahead": 1})

    # ----------------------------------------------------------------- git
    hostile = {
        "core.fsmonitor": "touch ../fsmonitor-ran; false",
        "core.sshCommand": "touch ../ssh-ran; false",
        "diff.oracle.textconv": "sh -c 'touch ../textconv-ran; cat'",
    }
    add("git/hostile-configuration", [*linked_run, open_picker("teleport", prompt="Go"), teleport(),
                                      {"checkout": {"markers": ["fsmonitor-ran", "ssh-ran", "textconv-ran"]}}],
        code=ONE_PROJECT,
        git={"config": hostile, "files": {"README.md": "hello\n", ".gitattributes": "*.txt diff=oracle\n"},
             "dirty": {"notes.txt": "edited\n"}})
    add("git/repository-http-config", [*linked_run, open_picker("teleport", prompt="Go"),
                                       until(teleport(), "push_required", "failed", "complete")],
        code=ONE_PROJECT, git={"config": {"http.extraHeader": "X-Oracle: 1"}, "branch": "topic"})
    add("git/second-remote-first", [START, open_picker("configure")], code=ONE_PROJECT,
        git={"remotes": {"fork": "https://github.com/someone/fork.git", "origin": GITHUB_URL}})
    add("git/non-github-first", [START, open_picker("configure")], code=ONE_PROJECT,
        git={"remotes": {"alpha": "https://gitlab.com/x/y.git", "origin": GITHUB_URL}})
    for index, spelling in enumerate([
        "git@github.com:oracle/repo.git",
        "ssh://git@github.com/oracle/repo.git",
        "https://github.com/oracle/repo",
        "https://GitHub.com/Oracle/Repo.git",
        "http://github.com/oracle/repo.git",
        "git://github.com/oracle/repo.git",
        "https://user:token@github.com/oracle/repo.git",
        "https://github.com:443/oracle/repo.git",
        "github.com:oracle/repo.git",
        "git+ssh://git@github.com/oracle/repo.git",
    ]):
        add(f"git/remote-spelling-{index}", [START, open_picker("configure")], code=ONE_PROJECT,
            git={"remotes": {"origin": spelling}}, spelling=spelling)

    # ----------------------------------------------------------- telemetry
    add("telemetry/configure-select", [START, *linked()], code=ONE_PROJECT, telemetry=True)
    add("telemetry/configure-cancel", [START, open_picker("configure"), picker("cancel")],
        code=ONE_PROJECT, telemetry=True)
    add("telemetry/teleport-cancel-picker", [START, open_picker("teleport", prompt="Go"), picker("cancel")],
        code=ONE_PROJECT, telemetry=True)
    add("telemetry/gate-refusals", [START, open_picker("teleport")], code=ONE_PROJECT, telemetry=True)
    add("telemetry/completed", [*linked_run, open_picker("teleport", prompt="Go"), teleport()],
        code=ONE_PROJECT, telemetry=True)
    add("telemetry/failed-start", [*linked_run, open_picker("teleport", prompt="Go"), teleport()],
        code={**ONE_PROJECT, "start": [{"status": 404, "body": {"detail": "Project not found"}}]},
        telemetry=True)
    add("telemetry/push-denied", [*linked_run, open_picker("teleport", prompt="Go"),
                                  until(teleport(), "push_required"), {"respond": False}],
        code=ONE_PROJECT, git={"ahead": 1}, telemetry=True)
    add("telemetry/summary", [START, HISTORY_TURN, *linked(), open_picker("teleport", prompt="Go"),
                              teleport()],
        code=ONE_PROJECT, telemetry=True,
        backend=[{"text": "Looked at it."}, {"text": "<summary>Fixing a test</summary>"}])

    return found


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
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise rewind.OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
        selected = [
            scenario
            for scenario in scenarios()
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started_at = time.monotonic()
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
                f"{time.monotonic() - started_at:.1f}s",
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
        print(f"teleport oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
