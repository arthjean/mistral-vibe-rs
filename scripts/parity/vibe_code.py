"""What a black-box capture of the Vibe Code surfaces drives the app server with.

Teleport and the project picker reach past the model to the account, the Vibe
Code API and the datalake. This module holds what their captures share: one
stand-in that plays every one of those endpoints on top of the chat
completions (:class:`Backend`), the answers a scenario scripts for it, the
configuration that points a server at it, the Git environment of a scenario's
checkouts (:func:`git_env`), and the normalizer of what those endpoints carry
(:class:`Normalizer`). Nothing here imports the reference.
"""

from __future__ import annotations

import base64
import copy
import hashlib
import json
import os
import subprocess
import threading
import time
from typing import Any
from urllib.parse import parse_qsl, urlsplit

import blackbox
import host_env
import stand_in
import subagent_client


EVENT_TIMEOUT = 40.0
API_KEY = "oracle-key"
GITHUB_URL = "https://github.com/oracle/repo.git"
#: A proxy nothing listens on, so a fetch of the GitHub remote fails at once.
DEAD_PROXY = "http://127.0.0.1:9"


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


class Backend(stand_in.Backend):
    """The chat-completions stand-in, also playing the account, the Vibe Code
    API and the datalake, and logging every request they receive."""

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
# The checkout and the server's configuration
# --------------------------------------------------------------------------


def git_env(world: blackbox.World) -> dict[str, str]:
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(world.home),
        **host_env.windows_base(str(world.home)),
        # The fixture home carries the `.gitconfig` the scenario lays out.
        **host_env.git_isolated(
            None,
            GIT_AUTHOR_DATE="2026-01-01T00:00:00Z",
            GIT_COMMITTER_DATE="2026-01-01T00:00:00Z",
        ),
        "LANG": "C.UTF-8",
    }


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
            # A host without the CLI (a stock Windows one) still has the
            # standard library's decoder from Python 3.14 on.
            try:
                from compression import zstd

                decompressed = zstd.decompress(raw)
            except Exception:  # noqa: BLE001
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


class Normalizer(subagent_client.Normalizer):
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
