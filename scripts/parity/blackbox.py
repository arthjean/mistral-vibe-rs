"""What every black-box capture drives a server with.

A black-box capture starts the reference's own console script, or this port's
binary, over stdio in a fresh home, behind a local stand-in for the
chat-completions API, and records what the server writes. This module holds the
pieces every such capture shares: the stand-in (:class:`Backend`), the
directories a scenario runs in (:class:`World`), one server process and the
client side of its connection (:class:`Agent`), the normalizer that maps
identifiers, paths and times to placeholders and reduces server-authored prose
to a length and a digest (:class:`Normalizer`), and the requests an editor
client sends over ACP. Nothing here imports the reference.
"""

from __future__ import annotations

import errno
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import threading
import time
from typing import Any

import host_env
from reference import OracleError


#: How long a request may take before the scenario fails.
RESPONSE_TIMEOUT = 30.0

#: Keys whose string values are identities minted by the agent.
ID_KEYS = {
    "sessionId",
    "session_id",
    "messageId",
    "toolCallId",
    "callbackId",
    "entryId",
    "effect_entry_id",
    "loopId",
    "attemptId",
    "terminalId",
    "child_session_id",
    "operationId",
}
#: Keys whose values are wall-clock readings.
TIME_KEYS = {
    "updatedAt",
    "updated_at",
    "createdAt",
    "created_at",
    "expiresAt",
    "nextFireAt",
    "ts",
}
#: Keys whose numeric values measure elapsed time or throughput.
DURATION_KEYS = {"tokensPerSecond", "lastTurnDuration"}

ISO_TIME = re.compile(
    r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?"
)
COMPACT_TIME = re.compile(r"\d{8}_\d{6}")
# A session log directory ends in the first eight hex digits of its identifier.
SESSION_DIRECTORY = re.compile(r"(session_<time>_)[0-9a-f]{8}")
#: A file URI over a fixture root as `Path.as_uri()` spells it on Windows.
WINDOWS_FILE_URI = re.compile(r"file:///(<(?:ws|vibe-home|home|root|bin)>)")
# Loops are named by eight hex digits of a fresh identifier.
SHORT_ID = re.compile(r"[0-9a-f]{8}")
UUID = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)
#: An identifier pydantic elided in the middle when it quoted a rejected input.
ELIDED_UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]\.\.\.[0-9a-fA-F-]{4,}")
#: The payload size a provider error summarizes, which moves with the calendar
#: because the system prompt it counts names the weekday.
APPROX_CHARS = re.compile(r'("approx_chars":)\d+')


# --------------------------------------------------------------------------
# The chat-completions stand-in
# --------------------------------------------------------------------------


def completion_chunks(response: dict[str, Any], model: str) -> list[dict[str, Any]]:
    """The stream a scripted response is served as, one chunk per delta."""

    chunks: list[dict[str, Any]] = []

    def chunk(delta: dict[str, Any], finish: str | None = None, usage: bool = False):
        body: dict[str, Any] = {
            "id": "cmpl-oracle",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }
        if usage:
            body["usage"] = {
                "prompt_tokens": response.get("promptTokens", 10),
                "completion_tokens": response.get("completionTokens", 5),
                "total_tokens": response.get("promptTokens", 10)
                + response.get("completionTokens", 5),
            }
        chunks.append(body)

    chunk({"role": "assistant", "content": ""})
    for piece in response.get("text", []) if isinstance(response.get("text"), list) else (
        [response["text"]] if response.get("text") else []
    ):
        chunk({"content": piece})
    for index, call in enumerate(response.get("toolCalls", [])):
        chunk(
            {
                "tool_calls": [
                    {
                        "id": call.get("id", f"call_{index}"),
                        "type": "function",
                        "index": index,
                        "function": {
                            "name": call["name"],
                            "arguments": json.dumps(call.get("arguments", {})),
                        },
                    }
                ]
            }
        )
    finish = "tool_calls" if response.get("toolCalls") else response.get("finish", "stop")
    chunk({"content": ""}, finish=finish, usage=True)
    return chunks


class Backend:
    """Serves scripted completions in order, then a fixed closing answer."""

    def __init__(self) -> None:
        self.responses: list[dict[str, Any]] = []
        self.requests = 0
        self.lock = threading.Lock()
        backend = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                return

            def do_GET(self) -> None:  # noqa: N802
                self.reply(404, {"error": "not found"})

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw or b"{}")
                except json.JSONDecodeError:
                    body = {}
                if not self.path.rstrip("/").endswith("/chat/completions"):
                    self.reply(404, {"error": "not found"})
                    return
                with backend.lock:
                    backend.requests += 1
                    response = (
                        backend.responses.pop(0)
                        if backend.responses
                        else {"text": "Done."}
                    )
                if response.get("delay"):
                    time.sleep(float(response["delay"]))
                if response.get("status"):
                    self.reply(response["status"], response.get("body", {}))
                    return
                model = str(body.get("model", "model"))
                if body.get("stream"):
                    payload = b"".join(
                        b"data: " + json.dumps(item).encode() + b"\n\n"
                        for item in completion_chunks(response, model)
                    ) + b"data: [DONE]\n\n"
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                    self.send_header("content-length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                text = response.get("text", "")
                if isinstance(text, list):
                    text = "".join(text)
                self.reply(
                    200,
                    {
                        "id": "cmpl-oracle",
                        "object": "chat.completion",
                        "created": 1_700_000_000,
                        "model": model,
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": text},
                                "finish_reason": "stop",
                            }
                        ],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 5,
                            "total_tokens": 15,
                        },
                    },
                )

            def reply(self, status: int, body: Any) -> None:
                payload = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


# --------------------------------------------------------------------------
# One agent process
# --------------------------------------------------------------------------


class Agent:
    """One ``vibe-acp`` process and the client side of its connection."""

    def __init__(
        self,
        command: list[str],
        env: dict[str, str],
        cwd: Path,
        policy: dict[str, Any],
        world: World,
    ) -> None:
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
        )
        self.policy = policy
        self.world = world
        self.inbox: queue.Queue[dict[str, Any] | None] = queue.Queue()
        self.answered: set[Any] = set()
        self.write_lock = threading.Lock()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.stderr = threading.Thread(target=self._drain_stderr, daemon=True)
        self.stderr.start()

    def _drain_stderr(self) -> None:
        assert self.process.stderr is not None
        for _ in self.process.stderr:
            pass

    def _read(self) -> None:
        assert self.process.stdout is not None
        for raw in self.process.stdout:
            line = raw.decode("utf-8", errors="replace").strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                message = {"unparsed": line}
            self.inbox.put(message)
            if "method" in message and "id" in message:
                self._answer(message)
        self.inbox.put(None)

    def _answer(self, request: dict[str, Any]) -> None:
        reply = answer_client_request(request, self.policy, self.world)
        self.send(reply)

    def send(self, message: dict[str, Any]) -> None:
        with self.write_lock:
            assert self.process.stdin is not None
            try:
                self.process.stdin.write(json.dumps(message).encode() + b"\n")
                self.process.stdin.flush()
            except (BrokenPipeError, ValueError):
                pass
            except OSError as error:
                # Windows reports a write to an exited child as `EINVAL`
                # where POSIX raises `BrokenPipeError`: the same event.
                if error.errno != errno.EINVAL:
                    raise

    def collect(self, request_id: Any, quiet: float) -> list[dict[str, Any]]:
        """Everything the agent writes until the response and then silence."""

        messages: list[dict[str, Any]] = []
        answered = request_id is None or request_id in self.answered
        deadline = time.monotonic() + RESPONSE_TIMEOUT
        while True:
            timeout = quiet if answered else max(0.0, deadline - time.monotonic())
            try:
                message = self.inbox.get(timeout=timeout)
            except queue.Empty:
                if answered:
                    return messages
                raise OracleError(f"no response to request {request_id!r}") from None
            if message is None:
                if not answered:
                    raise OracleError(f"agent exited before answering {request_id!r}")
                return messages
            messages.append(message)
            if "method" not in message and "id" in message:
                self.answered.add(message["id"])
            if (
                not answered
                and "method" not in message
                and message.get("id") == request_id
            ):
                answered = True

    def stop(self) -> int | None:
        with self.write_lock:
            try:
                assert self.process.stdin is not None
                self.process.stdin.close()
            except (BrokenPipeError, ValueError):
                pass
            except OSError as error:
                # Windows reports a write to an exited child as `EINVAL`
                # where POSIX raises `BrokenPipeError`: the same event.
                if error.errno != errno.EINVAL:
                    raise
        try:
            return self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            return None


class World:
    """The directories one scenario runs in, and what it has learned so far."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.vibe_home = root / "vibe-home"
        self.workspace = root / "workspace"
        for directory in (self.home, self.vibe_home, self.workspace):
            directory.mkdir(parents=True, exist_ok=True)
        self.sessions: list[str] = []
        self.terminals: dict[str, tuple[str, int]] = {}
        self.user_messages: list[str] = []
        self.tool_calls: list[str] = []

    def terminal(self, method: str, params: dict[str, Any]) -> Any:
        """A client terminal that runs the command to completion on creation."""

        if method == "terminal/create":
            identifier = f"term-{len(self.terminals) + 1}"
            env = dict(os.environ)
            for variable in params.get("env") or []:
                env[variable["name"]] = variable["value"]
            # Editors run an argument-less command line through a shell.
            args = params.get("args") or []
            completed = subprocess.run(
                [params["command"], *args] if args else ["sh", "-c", params["command"]],
                cwd=params.get("cwd") or self.workspace,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.terminals[identifier] = (completed.stdout + completed.stderr, completed.returncode)
            return {"terminalId": identifier}
        output, code = self.terminals.get(params.get("terminalId", ""), ("", 0))
        if method == "terminal/output":
            return {"output": output, "truncated": False, "exitStatus": {"exitCode": code, "signal": None}}
        if method == "terminal/wait_for_exit":
            return {"exitCode": code, "signal": None}
        return None

    def learn(self, message: dict[str, Any]) -> None:
        result = message.get("result")
        if isinstance(result, dict) and isinstance(result.get("sessionId"), str):
            if result["sessionId"] not in self.sessions:
                self.sessions.append(result["sessionId"])
        params = message.get("params")
        if isinstance(params, dict):
            update = params.get("update")
            if isinstance(update, dict):
                message_id = update.get("messageId")
                if (
                    update.get("sessionUpdate") == "user_message_chunk"
                    and isinstance(message_id, str)
                    and message_id not in self.user_messages
                ):
                    self.user_messages.append(message_id)
                call_id = update.get("toolCallId")
                if isinstance(call_id, str) and call_id not in self.tool_calls:
                    self.tool_calls.append(call_id)

    def substitute(self, value: Any, native: bool = False) -> Any:
        """``value`` with its placeholders filled in; with ``native``, a path
        argument spelled as a Windows client sends it (``host_env.native_path``)."""
        if isinstance(value, dict):
            return {key: self.substitute(item, native) for key, item in value.items()}
        if isinstance(value, list):
            return [self.substitute(item, native) for item in value]
        if not isinstance(value, str):
            return value
        text = value.replace("$WS", str(self.workspace)).replace(
            "$VIBE_HOME", str(self.vibe_home)
        ).replace("$HOME", str(self.home))
        for prefix, table in (("$S", self.sessions), ("$U", self.user_messages), ("$T", self.tool_calls)):
            for index in range(len(table), 0, -1):
                text = text.replace(f"{prefix}{index}", table[index - 1])
        return host_env.native_path(value, text) if native else text


def answer_client_request(
    request: dict[str, Any], policy: dict[str, Any], world: World
) -> dict[str, Any]:
    """What the scripted client answers a request the agent sends it."""

    method = request["method"]
    params = request.get("params") or {}
    answers = policy.get(method)
    if isinstance(answers, list):
        answer = answers.pop(0) if answers else None
    else:
        answer = answers
    if method == "fs/read_text_file" and answer is None:
        path = Path(params.get("path", ""))
        try:
            content = path.read_text(encoding="utf-8")
        except OSError as error:
            return {
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32603, "message": str(error)},
            }
        return {"jsonrpc": "2.0", "id": request["id"], "result": {"content": content}}
    if method == "fs/write_text_file" and answer is None:
        path = Path(params.get("path", ""))
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(params.get("content", ""), encoding="utf-8", newline="")
        return {"jsonrpc": "2.0", "id": request["id"], "result": None}
    if method.startswith("terminal/") and answer is None:
        return {"jsonrpc": "2.0", "id": request["id"], "result": world.terminal(method, params)}
    if answer is None:
        return {
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32601, "message": "Method not found"},
        }
    if isinstance(answer, dict) and "error" in answer:
        return {"jsonrpc": "2.0", "id": request["id"], "error": answer["error"]}
    return {"jsonrpc": "2.0", "id": request["id"], "result": world.substitute(answer)}


def write_tree(root: Path, files: dict[str, str]) -> None:
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="")


def base_config(backend: Backend) -> str:
    """Points the default Mistral provider at the stand-in."""

    return (
        'active_model = "mistral-medium-3.5"\n'
        "enable_telemetry = false\n"
        "enable_update_checks = false\n"
        "\n[[providers]]\n"
        'name = "mistral"\n'
        f'api_base = "{backend.base}/v1"\n'
        'api_key_env_var = "MISTRAL_API_KEY"\n'
        'backend = "mistral"\n'
    )


def authored_strings(scenario: dict[str, Any]) -> set[str]:
    """Every string a scenario itself supplies, which is never hashed."""

    found: set[str] = set()

    def walk(value: Any) -> None:
        if isinstance(value, dict):
            for key, item in value.items():
                found.add(key)
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)
        elif isinstance(value, str):
            found.add(value)
            if value.startswith("{") or value.startswith("["):
                try:
                    walk(json.loads(value))
                except json.JSONDecodeError:
                    pass

    walk(scenario)
    return found


class Normalizer:
    def __init__(self, scenario: dict[str, Any], paths: dict[str, str]) -> None:
        self.authored = authored_strings(scenario)
        self.paths = sorted(
            (
                (spelling, placeholder)
                for path, placeholder in (
                    (paths["workspace"], "<ws>"),
                    (paths["vibeHome"], "<vibe-home>"),
                    (paths["home"], "<home>"),
                    (paths["root"], "<root>"),
                    # The directory the agent was launched from, which the
                    # terminal sign-in method names.
                    (paths["bin"], "<bin>"),
                    # A provider error names the endpoint, whose port is
                    # ephemeral.
                    (paths["backend"], "<backend>"),
                )
                for spelling in host_env.path_spellings(path)
            ),
            key=lambda item: -len(item[0]),
        )
        self.ids: dict[str, str] = {}

    def identity(self, value: str) -> str:
        if value not in self.ids:
            self.ids[value] = f"<id-{len(self.ids) + 1}>"
        return self.ids[value]

    def text(self, value: str) -> str:
        for path, placeholder in self.paths:
            value = value.replace(path, placeholder)
        value = host_env.posix_tails(value, ["<ws>", "<vibe-home>", "<home>", "<root>", "<bin>"])
        if os.name == "nt":
            # `Path.as_uri()` puts a slash before the drive (`file:///C:/...`)
            # where a POSIX root brings its own.
            value = WINDOWS_FILE_URI.sub(r"file://\1", value)
        # Short identifiers are scenario words that also occur inside prose.
        for original, placeholder in sorted(self.ids.items(), key=lambda item: -len(item[0])):
            if len(original) >= 8:
                value = value.replace(original, placeholder)
        value = ISO_TIME.sub("<time>", value)
        value = COMPACT_TIME.sub("<time>", value)
        value = SESSION_DIRECTORY.sub(r"\1<short>", value)
        value = UUID.sub(lambda match: self.identity(match.group(0)), value)
        value = ELIDED_UUID.sub("<elided-id>", value)
        value = APPROX_CHARS.sub(r"\1<n>", value)
        return value

    def collect_ids(self, value: Any) -> None:
        if isinstance(value, dict):
            for key, item in value.items():
                if key in ID_KEYS and isinstance(item, str) and item:
                    self.identity(item)
                self.collect_ids(item)
        elif isinstance(value, list):
            for item in value:
                self.collect_ids(item)

    def value(self, value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            return {name: self.value(item, name) for name, item in value.items()}
        if isinstance(value, list):
            return [self.value(item, key) for item in value]
        if key in TIME_KEYS and value is not None and not isinstance(value, bool):
            return "<time>"
        if key in DURATION_KEYS and isinstance(value, int | float):
            return "<nonzero>" if value else 0
        if isinstance(value, str):
            if key in ID_KEYS and value:
                return self.identity(value)
            if key == "id" and SHORT_ID.fullmatch(value):
                return self.identity(value)
            if value in self.authored:
                return value
            text = self.text(value)
            if text in self.authored:
                return text
            if re.search(r"\s", text):
                digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
                return {"prose": len(text), "sha256": digest}
            return text
        return value


def request(identifier: int, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    return {"send": {"id": identifier, "method": method, "params": params or {}}}


def notify(method: str, params: dict[str, Any]) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


CLIENT_CAPABILITIES = {
    "fs": {"readTextFile": True, "writeTextFile": True},
    "terminal": True,
}
INITIALIZE = request(
    0,
    "initialize",
    {
        "protocolVersion": 1,
        "clientCapabilities": CLIENT_CAPABILITIES,
        "clientInfo": {"name": "oracle-client", "version": "1.0.0"},
    },
)
NEW_SESSION = request(1, "session/new", {"cwd": "$WS", "mcpServers": []})


def prompt(identifier: int, text: str, session: str = "$S1") -> dict[str, Any]:
    return request(
        identifier,
        "session/prompt",
        {"sessionId": session, "prompt": [{"type": "text", "text": text}]},
    )


def initialize(
    capabilities: dict[str, Any] | None = None,
    info: dict[str, Any] | None = None,
    version: int = 1,
    identifier: int = 0,
) -> dict[str, Any]:
    params: dict[str, Any] = {
        "protocolVersion": version,
        "clientCapabilities": CLIENT_CAPABILITIES if capabilities is None else capabilities,
    }
    params["clientInfo"] = info or {"name": "oracle-client", "version": "1.0.0"}
    return request(identifier, "initialize", params)


ELICITING = {**CLIENT_CAPABILITIES, "elicitation": {"form": {}}}


def new_session(identifier: int = 1, **extra: Any) -> dict[str, Any]:
    return request(identifier, "session/new", {"cwd": "$WS", "mcpServers": [], **extra})


def ext(identifier: int, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    return request(identifier, f"_{method}", params or {})


APPROVE_ONCE = {"outcome": {"outcome": "selected", "optionId": "allow_once"}}
APPROVE_SESSION = {"outcome": {"outcome": "selected", "optionId": "allow_always"}}
REJECT = {"outcome": {"outcome": "selected", "optionId": "reject_once"}}
DISMISS = {"outcome": {"outcome": "cancelled"}}
#: The scoped options an approval of a path outside the workdir offers
#: (reference `ToolOption` in `vibe/acp/utils.py`).
APPROVE_FOLDER_SESSION = {"outcome": {"outcome": "selected",
                                      "optionId": "allow_session_directory_recursive"}}
APPROVE_FILE_PERMANENT = {"outcome": {"outcome": "selected", "optionId": "allow_permanent_exact"}}


def rendered(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
