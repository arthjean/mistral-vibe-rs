#!/usr/bin/env python3
"""Black-box capture of the editor protocol (ACP) served by ``vibe-acp``.

Every scenario starts the agent binary over stdio in a fresh home, behind a
local stand-in for the chat-completions API, sends the scenario's requests,
answers what the agent asks of its client by the scenario's policy, and records
every message the agent writes. Nothing is imported from the reference: the
binary is the oracle, which is what makes the same scenarios replayable
against this port's ``vibe-acp`` by ``crates/vibe-acp/tests/acp_parity_tests.rs``.

The corpus holds the scenarios themselves and the normalized observations.
Normalization maps identifiers, paths, times and the version to placeholders,
and reduces every string the reference authored (a string carrying whitespace
that no scenario input holds verbatim) to its length and SHA-256, which is what
``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/acp.py                    # capture the reference
    python3 scripts/parity/acp.py --check            # recapture and compare
    python3 scripts/parity/acp.py --agent target/debug/vibe-acp --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import copy
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import re
import select
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-acp/tests/acp-parity/corpus.json"
SCHEMA_VERSION = 1

#: How long the agent must stay silent before a step is considered settled.
QUIET_SECONDS = 0.6
#: How long a request may take before the scenario fails.
RESPONSE_TIMEOUT = 30.0

#: Notifications the reference emits from detached tasks, whose position
#: relative to the response is not part of the contract.
ASYNC_UPDATES = {"usage_update", "available_commands_update"}

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
# Loops are named by eight hex digits of a fresh identifier.
SHORT_ID = re.compile(r"[0-9a-f]{8}")
UUID = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)
#: An identifier pydantic elided in the middle when it quoted a rejected input.
ELIDED_UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]\.\.\.[0-9a-fA-F-]{4,}")


class OracleError(RuntimeError):
    pass


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

    def substitute(self, value: Any) -> Any:
        if isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.substitute(item) for item in value]
        if not isinstance(value, str):
            return value
        text = value.replace("$WS", str(self.workspace)).replace(
            "$VIBE_HOME", str(self.vibe_home)
        ).replace("$HOME", str(self.home))
        for prefix, table in (("$S", self.sessions), ("$U", self.user_messages), ("$T", self.tool_calls)):
            for index in range(len(table), 0, -1):
                text = text.replace(f"{prefix}{index}", table[index - 1])
        return text


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
        path.write_text(params.get("content", ""), encoding="utf-8")
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


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


def write_tree(root: Path, files: dict[str, str]) -> None:
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


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


def run_scenario(
    scenario: dict[str, Any], command: list[str], quiet: float
) -> dict[str, Any]:
    backend = Backend()
    root = Path(tempfile.mkdtemp(prefix="vibe-acp-oracle-"))
    world = World(root)
    try:
        backend.responses = world.substitute(copy.deepcopy(scenario.get("backend", [])))
        config = base_config(backend) + scenario.get("config", "")
        (world.vibe_home / "config.toml").write_text(config, encoding="utf-8")
        write_tree(world.workspace, scenario.get("files", {}))
        write_tree(world.vibe_home, scenario.get("vibeHomeFiles", {}))
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
        }
        for key, value in scenario.get("env", {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = world.substitute(value)
        policy = copy.deepcopy(scenario.get("client", {}))
        agent = Agent(command, env, world.workspace, policy, world)
        steps: list[dict[str, Any]] = []
        try:
            for step in scenario["steps"]:
                if "restart" in step:
                    agent.stop()
                    agent = Agent(command, env, world.workspace, policy, world)
                    steps.append({"restart": True})
                    continue
                if "backend" in step:
                    backend.responses.extend(world.substitute(copy.deepcopy(step["backend"])))
                    continue
                if "write" in step:
                    write_tree(world.workspace, world.substitute(step["write"]))
                    continue
                if "sleep" in step:
                    time.sleep(step["sleep"])
                    continue
                if "awaitId" in step:
                    try:
                        observed = agent.collect(step["awaitId"], step.get("quiet", quiet))
                    except OracleError as error:
                        steps.append({"failure": str(error)})
                        break
                    for item in observed:
                        world.learn(item)
                    steps.append({"messages": observed})
                    continue
                message = world.substitute(step["send"])
                message.setdefault("jsonrpc", "2.0")
                agent.send(message)
                awaited = None if step.get("defer") else message.get("id")
                try:
                    observed = agent.collect(awaited, step.get("quiet", quiet))
                except OracleError as error:
                    # A request left unanswered is an observation too: the rest
                    # of the scenario cannot run, but the others still can.
                    steps.append({"failure": str(error)})
                    break
                for item in observed:
                    world.learn(item)
                steps.append({"messages": observed})
        finally:
            exit_code = agent.stop()
        return {
            "steps": steps,
            "exit": exit_code,
            "backendRequests": backend.requests,
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


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


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
                (paths["workspace"], "<ws>"),
                (paths["vibeHome"], "<vibe-home>"),
                (paths["home"], "<home>"),
                (paths["root"], "<root>"),
                # The directory the agent was launched from, which the
                # terminal sign-in method names.
                (paths["bin"], "<bin>"),
                # A provider error names the endpoint, whose port is ephemeral.
                (paths["backend"], "<backend>"),
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
        # Short identifiers are scenario words that also occur inside prose.
        for original, placeholder in sorted(self.ids.items(), key=lambda item: -len(item[0])):
            if len(original) >= 8:
                value = value.replace(original, placeholder)
        value = ISO_TIME.sub("<time>", value)
        value = COMPACT_TIME.sub("<time>", value)
        value = SESSION_DIRECTORY.sub(r"\1<short>", value)
        value = UUID.sub(lambda match: self.identity(match.group(0)), value)
        value = ELIDED_UUID.sub("<elided-id>", value)
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


def message_kind(message: dict[str, Any]) -> str | None:
    params = message.get("params")
    if isinstance(params, dict) and isinstance(params.get("update"), dict):
        return params["update"].get("sessionUpdate")
    return None


def sort_grep_matches(message: dict[str, Any]) -> None:
    """Orders a grep call's matches by path, then line.

    `rg` walks in parallel and prints whichever file finished first, so its
    order is not a contract; the `grep` row of the accepted divergences in
    `docs/parity.md` records this port sorting its own the same way.
    """
    params = message.get("params")
    update = params.get("update") if isinstance(params, dict) else None
    if not isinstance(update, dict) or (update.get("_meta") or {}).get("tool_name") != "grep":
        return

    def key(item: Any) -> tuple[str, int]:
        if not isinstance(item, dict):
            return ("", 0)
        line = item.get("line")
        return (str(item.get("path", "")), line if isinstance(line, int) else 0)

    if isinstance(update.get("locations"), list):
        update["locations"].sort(key=key)
    raw_output = update.get("rawOutput")
    if isinstance(raw_output, dict):
        if isinstance(raw_output.get("parsedMatches"), list):
            raw_output["parsedMatches"].sort(key=key)
        if isinstance(raw_output.get("matches"), str):
            raw_output["matches"] = "\n".join(sorted(raw_output["matches"].split("\n")))


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[dict[str, Any]]:
    normalizer = Normalizer(scenario, run["paths"])
    for step in run["steps"]:
        for message in step.get("messages", []):
            normalizer.collect_ids(message)
    observed: list[dict[str, Any]] = []
    for step in run["steps"]:
        if step.get("restart"):
            observed.append({"restart": True})
            continue
        if "failure" in step:
            observed.append({"failure": normalizer.text(step["failure"])})
            continue
        stream: list[Any] = []
        detached: list[Any] = []
        response: Any = None
        answered = False
        for message in step["messages"]:
            message = copy.deepcopy(message)
            sort_grep_matches(message)
            normalized = normalizer.value(message)
            normalized.pop("jsonrpc", None)
            if "method" not in message and "id" in message:
                agent_info = (normalized.get("result") or {}) if isinstance(normalized.get("result"), dict) else {}
                if isinstance(agent_info.get("agentInfo"), dict):
                    # The version is the distribution's contract (row 1), not
                    # this protocol's, and the two products ship different ones.
                    agent_info["agentInfo"]["version"] = "<version>"
                response = normalized
                answered = True
                continue
            if message_kind(message) in ASYNC_UPDATES:
                detached.append(normalized)
                continue
            stream.append({"after": answered, "message": normalized})
        # An asynchronous update is a snapshot a client renders the latest of,
        # so how often the same one repeats carries nothing.
        unique = {json.dumps(item, sort_keys=True): item for item in detached}
        detached = [unique[key] for key in sorted(unique)]
        observed.append({"response": response, "stream": stream, "detached": detached})
    return observed


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


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


def call(name: str, arguments: dict[str, Any], identifier: str = "call_1") -> dict[str, Any]:
    return {"toolCalls": [{"id": identifier, "name": name, "arguments": arguments}]}


APPROVE_ONCE = {"outcome": {"outcome": "selected", "optionId": "allow_once"}}
APPROVE_SESSION = {"outcome": {"outcome": "selected", "optionId": "allow_always"}}
REJECT = {"outcome": {"outcome": "selected", "optionId": "reject_once"}}
DISMISS = {"outcome": {"outcome": "cancelled"}}
BASE = [INITIALIZE, NEW_SESSION]
SKILL = (
    "---\nname: greet\ndescription: Greets the user politely\n---\n\n"
    "Say hello to the user.\n"
)
QUESTION = {
    "questions": [
        {
            "question": "Which color?",
            "header": "Color",
            "options": [
                {"label": "Red", "description": "Warm"},
                {"label": "Blue", "description": "Cold"},
            ],
        },
        {
            "question": "Which sizes?",
            "header": "Size",
            "multi_select": True,
            "options": [
                {"label": "Small", "description": "S"},
                {"label": "Large", "description": "L"},
            ],
        },
    ]
}
TODOS = {
    "action": "write",
    "todos": [
        {"id": "1", "content": "First task", "status": "completed", "priority": "high"},
        {"id": "2", "content": "Second task", "status": "in_progress", "priority": "medium"},
        {"id": "3", "content": "Dropped task", "status": "cancelled", "priority": "low"},
        {"id": "4", "content": "Last task", "status": "pending", "priority": "low"},
    ],
}
PNG = base64.b64encode(
    bytes.fromhex(
        "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
        "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082"
    )
).decode()


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- handshake ----------------------------------------------------
        {"name": "initialize/plain", "steps": [INITIALIZE]},
        {
            "name": "initialize/terminal-auth",
            "steps": [initialize({**CLIENT_CAPABILITIES, "_meta": {"terminal-auth": True}})],
        },
        {
            "name": "initialize/browser-delegated",
            "steps": [
                initialize({**CLIENT_CAPABILITIES, "_meta": {"browser-auth-delegated": True}})
            ],
        },
        {
            "name": "initialize/jetbrains",
            "steps": [initialize(info={"name": "JetBrains.Rider", "version": "2026.1"})],
        },
        {
            "name": "initialize/jetbrains-without-key",
            "env": {"MISTRAL_API_KEY": None},
            "steps": [initialize(info={"name": "JetBrains.Rider", "version": "2026.1"})],
        },
        {"name": "initialize/other-protocol", "steps": [initialize(version=7)]},
        {"name": "initialize/minimal", "steps": [request(0, "initialize", {"protocolVersion": 1})]},
        {"name": "initialize/twice", "steps": [INITIALIZE, initialize(identifier=1)]},
        {"name": "initialize/skipped", "steps": [NEW_SESSION]},
        {
            "name": "auth/status",
            "steps": [INITIALIZE, ext(1, "auth/status"), ext(2, "auth/signOut"), ext(3, "auth/status")],
        },
        {
            "name": "auth/status-without-key",
            "env": {"MISTRAL_API_KEY": None},
            "steps": [INITIALIZE, ext(1, "auth/status"), ext(2, "auth/signOut")],
        },
        {
            "name": "authenticate/unknown-method",
            "steps": [INITIALIZE, request(1, "authenticate", {"methodId": "nope"})],
        },
        {
            "name": "protocol/unknown-methods",
            "steps": [
                INITIALIZE,
                request(1, "foo/bar"),
                ext(2, "foo/bar"),
                request(3, "session/delete", {"sessionId": "x"}),
                request(4, "logout"),
                notify("_unknown/notice", {"a": 1}),
                notify("unknown/notice", {"a": 1}),
            ],
        },
        {
            "name": "protocol/invalid-params",
            "steps": [
                INITIALIZE,
                request(1, "session/new", {"mcpServers": []}),
                request(2, "session/prompt", {"sessionId": "missing", "prompt": []}),
                request(3, "session/set_mode", {"sessionId": "missing", "modeId": "plan"}),
                request(4, "session/load", {"sessionId": "missing", "cwd": "$WS", "mcpServers": []}),
                request(5, "session/close", {"sessionId": "missing"}),
                request(6, "session/fork", {"sessionId": "missing", "cwd": "$WS"}),
                request(7, "session/resume", {"sessionId": "missing", "cwd": "$WS"}),
                request(8, "session/set_config_option", {"sessionId": "missing", "configId": "mode", "value": "plan"}),
                request(9, "session/new", {"cwd": "$WS"}),
                request(10, "session/new", {"cwd": 7, "mcpServers": "none"}),
                request(11, "session/set_mode", {}),
                request(12, "session/prompt", {"sessionId": "missing", "prompt": "hi"}),
                request(13, "session/prompt", {"sessionId": "missing", "prompt": [{"type": "text"}]}),
                request(14, "session/new", {"cwd": "$WS", "mcpServers": [{"name": "a"}]}),
                request(15, "initialize", {}),
                request(16, "session/list", {"cwd": 3}),
                request(17, "authenticate", {}),
                request(18, "session/cancel", {"sessionId": "missing"}),
            ],
        },
        # -- session lifecycle ---------------------------------------------
        {"name": "session/new", "steps": BASE},
        {
            "name": "session/new-additional-directories",
            "files": {"extra/keep.txt": "keep\n"},
            "steps": [INITIALIZE, new_session(additionalDirectories=["$WS/extra"])],
        },
        {
            "name": "session/new-mcp-sse",
            "steps": [
                INITIALIZE,
                request(1, "session/new", {
                    "cwd": "$WS",
                    "mcpServers": [{"type": "sse", "name": "events", "url": "http://127.0.0.1:9/sse", "headers": []}],
                }),
            ],
        },
        {
            "name": "session/new-mcp-unreachable",
            "steps": [
                INITIALIZE,
                {**request(1, "session/new", {
                    "cwd": "$WS",
                    "mcpServers": [
                        {"type": "http", "name": "remote", "url": "http://127.0.0.1:9/mcp", "headers": [{"name": "X-Token", "value": "t"}]},
                        {"name": "local", "command": "/nonexistent/mcp-server", "args": ["--stdio"], "env": [{"name": "A", "value": "1"}]},
                    ],
                }), "quiet": 3.0},
            ],
        },
        {
            "name": "session/list",
            "backend": [{"text": "First answer"}],
            "steps": [
                *BASE,
                prompt(2, "List me"),
                request(3, "session/list", {}),
                request(4, "session/list", {"cwd": "$WS"}),
                request(5, "session/list", {"cwd": "/elsewhere"}),
            ],
        },
        {
            "name": "session/load",
            "backend": [{"text": "Remembered"}],
            "files": {"notes.txt": "alpha\nbeta\n"},
            "steps": [
                *BASE,
                prompt(2, "Remember this"),
                {"restart": True},
                INITIALIZE,
                request(1, "session/load", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}),
                prompt(2, "Again"),
            ],
        },
        {
            "name": "session/load-live",
            "backend": [{"text": "Remembered"}],
            "steps": [
                *BASE,
                prompt(2, "Remember this"),
                request(3, "session/load", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}),
            ],
        },
        {
            "name": "session/resume",
            "backend": [{"text": "Remembered"}],
            "steps": [
                *BASE,
                prompt(2, "Remember this"),
                request(3, "session/resume", {"sessionId": "$S1", "cwd": "$WS"}),
                {"restart": True},
                INITIALIZE,
                request(1, "session/resume", {"sessionId": "$S1", "cwd": "$WS"}),
                prompt(2, "Again"),
            ],
        },
        {
            "name": "session/fork",
            "backend": [{"text": "One"}, {"text": "Two"}],
            "steps": [
                *BASE,
                prompt(2, "First"),
                prompt(3, "Second"),
                request(4, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}),
                request(5, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": [], "messageId": "$U2"}),
                request(6, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "messageId": "unknown-message"}),
            ],
        },
        {
            # The router merges `_meta` into the handler's arguments, which is
            # where a fork point travels.
            "name": "session/fork-at-message",
            "backend": [{"text": "One"}, {"text": "Two"}],
            "steps": [
                *BASE,
                prompt(2, "First"),
                prompt(3, "Second"),
                request(4, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": [], "_meta": {"messageId": "$U2"}}),
                request(5, "session/close", {"sessionId": "$S2"}),
                request(6, "session/load", {"sessionId": "$S2", "cwd": "$WS", "mcpServers": []}),
                request(7, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": [], "_meta": {"messageId": "unknown-message"}}),
            ],
        },
        {
            "name": "session/close",
            "steps": [
                *BASE,
                request(2, "session/close", {"sessionId": "$S1"}),
                request(3, "session/close", {"sessionId": "$S1"}),
                prompt(4, "After close"),
            ],
        },
        {
            "name": "session/set-mode",
            "steps": [
                *BASE,
                request(2, "session/set_mode", {"sessionId": "$S1", "modeId": "plan"}),
                request(3, "session/set_mode", {"sessionId": "$S1", "modeId": "no-such-mode"}),
                request(4, "session/set_mode", {"sessionId": "$S1", "modeId": "ask"}),
            ],
        },
        {
            "name": "session/set-config-option",
            "steps": [
                *BASE,
                request(2, "session/set_config_option", {"sessionId": "$S1", "configId": "mode", "value": "plan"}),
                request(3, "session/set_config_option", {"sessionId": "$S1", "configId": "model", "value": "local"}),
                request(4, "session/set_config_option", {"sessionId": "$S1", "configId": "thinking", "value": "low"}),
                request(5, "session/set_config_option", {"sessionId": "$S1", "configId": "max_turns", "value": "5"}),
                request(6, "session/set_config_option", {"sessionId": "$S1", "configId": "max_tokens", "value": "abc"}),
                request(7, "session/set_config_option", {"sessionId": "$S1", "configId": "model", "value": "missing"}),
                request(8, "session/set_config_option", {"sessionId": "$S1", "configId": "thinking", "value": "extreme"}),
                request(9, "session/set_config_option", {"sessionId": "$S1", "configId": "colour", "value": "red"}),
                request(10, "session/set_config_option", {"sessionId": "$S1", "configId": "mode", "value": True}),
            ],
        },
        # -- prompt turns --------------------------------------------------
        {
            "name": "prompt/text",
            "backend": [{"text": ["Hello", " there"]}],
            "steps": [*BASE, prompt(2, "Say hello")],
        },
        {
            "name": "prompt/content-blocks",
            "backend": [{"text": "Seen"}],
            "steps": [
                *BASE,
                request(2, "session/prompt", {
                    "sessionId": "$S1",
                    "prompt": [
                        {"type": "text", "text": "Look at these"},
                        {"type": "resource", "resource": {"uri": "file://$WS/a.txt", "mimeType": "text/plain", "text": "embedded text"}},
                        {"type": "resource_link", "uri": "file://$WS/b.txt", "name": "b.txt", "title": "B file", "mimeType": "text/plain", "size": 3},
                        {"type": "image", "data": PNG, "mimeType": "image/png", "uri": "file://$WS/shot.png"},
                        {"type": "text", "text": "automatic context", "_meta": {"automatic": True}},
                    ],
                }),
            ],
        },
        {
            "name": "prompt/rejected-blocks",
            "steps": [
                *BASE,
                request(2, "session/prompt", {"sessionId": "$S1", "prompt": [{"type": "audio", "data": "AAAA", "mimeType": "audio/wav"}]}),
                request(3, "session/prompt", {"sessionId": "$S1", "prompt": [{"type": "image", "data": "not base64!", "mimeType": "image/png"}]}),
                request(4, "session/prompt", {"sessionId": "$S1", "prompt": [{"type": "image", "data": PNG, "mimeType": "image/tiff"}]}),
            ],
        },
        {
            "name": "prompt/user-display-content",
            "backend": [{"text": "Shown"}],
            "steps": [
                *BASE,
                request(2, "session/prompt", {
                    "sessionId": "$S1",
                    "prompt": [{"type": "text", "text": "Expanded prompt"}],
                    "user_display_content": {"text": "Short prompt"},
                }),
                request(3, "session/prompt", {
                    "sessionId": "$S1",
                    "prompt": [{"type": "text", "text": "Bad display"}],
                    "user_display_content": {"unknown": 1},
                }),
            ],
        },
        {
            "name": "prompt/mention",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [{"text": "Read it"}],
            "steps": [*BASE, prompt(2, "Summarize @notes.txt please")],
        },
        {
            "name": "prompt/read-file",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [call("read_file", {"file_path": "$WS/notes.txt"}), {"text": "It says alpha"}],
            "steps": [*BASE, prompt(2, "Read notes")],
        },
        {
            "name": "prompt/read-file-range",
            "files": {"notes.txt": "alpha\nbeta\ngamma\n"},
            "backend": [call("read_file", {"file_path": "$WS/notes.txt", "offset": 2, "limit": 1}), {"text": "Done"}],
            "steps": [*BASE, prompt(2, "Read one line")],
        },
        {
            "name": "prompt/read-file-without-fs",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [call("read_file", {"file_path": "$WS/notes.txt"}), {"text": "It says alpha"}],
            "steps": [initialize({}), NEW_SESSION, prompt(2, "Read notes")],
        },
        {
            "name": "prompt/write-file",
            "backend": [call("write_file", {"file_path": "$WS/out.txt", "content": "written\n"}), {"text": "Wrote it"}],
            "steps": [*BASE, prompt(2, "Write a file")],
        },
        {
            "name": "prompt/edit-file",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [call("edit", {"file_path": "$WS/notes.txt", "old_string": "beta", "new_string": "delta"}), {"text": "Edited"}],
            "steps": [*BASE, prompt(2, "Edit a file")],
        },
        {
            "name": "prompt/grep",
            "files": {"notes.txt": "alpha\nbeta\n", "more.txt": "beta again\n"},
            "backend": [call("grep", {"pattern": "beta", "path": "$WS"}), {"text": "Found"}],
            "steps": [*BASE, prompt(2, "Search")],
        },
        {
            "name": "prompt/bash-approved",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), {"text": "Ran"}],
            "client": {"session/request_permission": APPROVE_ONCE},
            "steps": [*BASE, prompt(2, "Run a command")],
        },
        {
            "name": "prompt/bash-approved-for-session",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), call("bash", {"command": "echo hi > made.txt"}, "call_2"), {"text": "Ran twice"}],
            "client": {"session/request_permission": APPROVE_SESSION},
            "steps": [*BASE, prompt(2, "Run twice")],
        },
        {
            "name": "prompt/bash-rejected",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), {"text": "Understood"}],
            "client": {"session/request_permission": REJECT},
            "steps": [*BASE, prompt(2, "Run a command")],
        },
        {
            "name": "prompt/bash-dismissed",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), {"text": "Understood"}],
            "client": {"session/request_permission": DISMISS},
            "steps": [*BASE, prompt(2, "Run a command")],
        },
        {
            "name": "prompt/bash-ask-mode",
            "backend": [call("read_file", {"file_path": "$WS/notes.txt"}), {"text": "Read"}],
            "files": {"notes.txt": "alpha\n"},
            "client": {"session/request_permission": APPROVE_ONCE},
            "steps": [
                *BASE,
                request(2, "session/set_mode", {"sessionId": "$S1", "modeId": "ask"}),
                prompt(3, "Read under ask"),
            ],
        },
        {
            "name": "prompt/todo",
            "backend": [call("todo", TODOS), {"text": "Planned"}],
            "steps": [*BASE, prompt(2, "Plan the work")],
        },
        {
            "name": "prompt/question-elicited",
            "backend": [call("ask_user_question", QUESTION), {"text": "Thanks"}],
            "client": {
                "elicitation/create": {"action": "accept", "content": {"q0": "Red", "q1": ["Small", "Huge"]}},
            },
            "steps": [initialize(ELICITING), NEW_SESSION, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/question-declined",
            "backend": [call("ask_user_question", QUESTION), {"text": "Ok"}],
            "client": {"elicitation/create": {"action": "decline"}},
            "steps": [initialize(ELICITING), NEW_SESSION, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/question-invalid-answer",
            "backend": [call("ask_user_question", QUESTION), {"text": "Ok"}],
            "client": {"elicitation/create": {"action": "accept", "content": {"q0": "", "q1": ["Small"]}}},
            "steps": [initialize(ELICITING), NEW_SESSION, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/question-without-elicitation",
            "backend": [call("ask_user_question", QUESTION), {"text": "Ok"}],
            "steps": [*BASE, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/backend-error",
            "backend": [{"status": 400, "body": {"message": "bad request", "type": "invalid_request_error"}}],
            "steps": [*BASE, prompt(2, "Fail please")],
        },
        {
            "name": "prompt/backend-unauthorized",
            "backend": [{"status": 401, "body": {"message": "Unauthorized"}}],
            "steps": [*BASE, prompt(2, "Fail please")],
        },
        {
            "name": "prompt/cancel",
            "backend": [{"text": "Too late", "delay": 3.0}],
            "steps": [
                *BASE,
                {**prompt(2, "Slow answer"), "defer": True, "quiet": 0.8},
                {**notify("session/cancel", {"sessionId": "$S1"}), "quiet": 0.0},
                {"awaitId": 2},
            ],
        },
        {
            "name": "prompt/busy",
            "backend": [{"text": "Slow", "delay": 2.0}, {"text": "Second"}],
            "steps": [
                *BASE,
                {**prompt(2, "First"), "defer": True, "quiet": 0.5},
                prompt(3, "Second"),
                {"awaitId": 2},
            ],
        },
        {
            "name": "prompt/session-title",
            "backend": [{"text": "Titled"}],
            "steps": [*BASE, prompt(2, "A fairly long first message that becomes the title of the session")],
        },
        # -- commands --------------------------------------------------------
        {
            "name": "command/help",
            "vibeHomeFiles": {"skills/greet/SKILL.md": SKILL},
            "steps": [*BASE, prompt(2, "/help")],
        },
        {"name": "command/log", "steps": [*BASE, prompt(2, "/log")]},
        {"name": "command/data-retention", "steps": [*BASE, prompt(2, "/data-retention")]},
        {
            "name": "command/reload",
            "steps": [
                *BASE,
                {"write": {}},
                prompt(2, "/reload"),
            ],
        },
        {
            "name": "command/mcp",
            "steps": [
                *BASE,
                prompt(2, "/mcp"),
                prompt(3, "/mcp status extra"),
                prompt(4, "/mcp login"),
                prompt(5, "/mcp login nothing"),
                prompt(6, "/mcp logout nothing"),
                prompt(7, "/mcp logout"),
                prompt(8, "/mcp frobnicate"),
            ],
        },
        {
            "name": "command/proxy-setup",
            "steps": [
                *BASE,
                prompt(2, "/proxy-setup"),
                prompt(3, "/proxy-setup HTTPS_PROXY http://proxy.local:8080"),
                prompt(4, "/proxy-setup"),
                prompt(5, "/proxy-setup https_proxy"),
                prompt(6, "/proxy-setup NOT_A_PROXY value"),
            ],
        },
        {
            "name": "command/compact-empty",
            "steps": [*BASE, prompt(2, "/compact")],
        },
        {
            "name": "command/compact",
            "backend": [{"text": "Answer"}, {"text": "Summary of the conversation"}],
            "steps": [*BASE, prompt(2, "Hello"), prompt(3, "/compact keep it short")],
        },
        {
            "name": "command/retry",
            "backend": [{"text": "Answer"}, {"text": "Continued"}],
            "steps": [*BASE, prompt(2, "/retry"), prompt(3, "Hello"), prompt(4, "/retry be brief")],
        },
        {
            "name": "command/lean",
            "steps": [*BASE, prompt(2, "/leanstall"), prompt(3, "/unleanstall")],
        },
        {"name": "command/teleport", "steps": [*BASE, prompt(2, "/teleport")]},
        {
            "name": "command/unknown-and-case",
            "backend": [{"text": "Plain"}],
            "steps": [*BASE, prompt(2, "/HELP"), prompt(3, "/nonexistent thing")],
        },
        {
            "name": "command/skill",
            "vibeHomeFiles": {"skills/greet/SKILL.md": SKILL},
            "backend": [{"text": "Hello friend"}],
            "steps": [*BASE, prompt(2, "/greet warmly")],
        },
        # -- extensions ------------------------------------------------------
        {"name": "ext/config-schema", "steps": [INITIALIZE, ext(1, "config/schema")]},
        {
            "name": "ext/session-title",
            "backend": [{"text": "Answer"}],
            "steps": [
                *BASE,
                prompt(2, "Hello"),
                ext(3, "session/set_title", {"sessionId": "$S1", "title": "  Renamed  "}),
                ext(4, "session/set_title", {"sessionId": "$S1", "title": "   "}),
                ext(5, "session/set_title", {"sessionId": "unknown", "title": "Nope"}),
                ext(6, "session/set_title", {}),
                request(7, "session/list", {}),
            ],
        },
        {
            "name": "ext/session-delete",
            "backend": [{"text": "Answer"}],
            "steps": [
                *BASE,
                prompt(2, "Hello"),
                ext(3, "session/delete", {"sessionId": "$S1"}),
                request(4, "session/list", {}),
                ext(5, "session/delete", {"sessionId": "unknown"}),
                ext(6, "session/delete", {}),
            ],
        },
        {
            "name": "ext/loops",
            "steps": [
                *BASE,
                ext(2, "loops/list", {"sessionId": "$S1"}),
                ext(3, "loops/create", {"sessionId": "$S1", "interval": "5m", "prompt": "check status"}),
                ext(4, "loops/create", {"sessionId": "$S1", "interval": "never", "prompt": "bad"}),
                ext(5, "loops/list", {"sessionId": "$S1"}),
                ext(6, "loops/delete", {"sessionId": "$S1", "loopId": "unknown"}),
                ext(7, "loops/clear", {"sessionId": "$S1"}),
                ext(8, "loops/list", {"sessionId": "unknown"}),
                ext(9, "loops/create", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/trust",
            "steps": [
                *BASE,
                ext(2, "trust/status", {}),
                ext(3, "trust/status", {"sessionId": "$S1"}),
                ext(4, "trust/status", {"cwd": "$WS"}),
                ext(5, "trust/decision", {"decision": "trust"}),
                ext(6, "trust/decision", {"sessionId": "$S1", "decision": "maybe"}),
                ext(7, "trust/decision", {"sessionId": "$S1", "decision": "trust"}),
                ext(8, "trust/status", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/rewind",
            "files": {"notes.txt": "alpha\n"},
            "backend": [call("write_file", {"file_path": "$WS/notes.txt", "content": "changed\n"}), {"text": "Changed"}],
            "steps": [
                *BASE,
                prompt(2, "Change notes"),
                ext(3, "rewind/preview", {"sessionId": "$S1", "messageId": "$U1"}),
                ext(4, "rewind/preview", {"sessionId": "$S1"}),
                ext(5, "rewind/preview", {"sessionId": "$S1", "messageId": "unknown"}),
                ext(6, "rewind/to", {"sessionId": "$S1", "messageId": "$U1"}),
            ],
        },
        {
            "name": "ext/review",
            "files": {"notes.txt": "alpha\n"},
            "backend": [call("write_file", {"file_path": "$WS/notes.txt", "content": "changed\n"}), {"text": "Changed"}],
            "steps": [
                *BASE,
                ext(2, "review/state", {"sessionId": "$S1"}),
                prompt(3, "Change notes"),
                ext(4, "review/state", {"sessionId": "$S1"}),
                ext(5, "review/baseline", {"sessionId": "$S1", "path": "$WS/notes.txt"}),
                ext(6, "review/turnDiff", {"sessionId": "$S1", "path": "$WS/notes.txt", "owner": "agent"}),
                ext(7, "review/hunks", {"sessionId": "$S1", "path": "$WS/notes.txt", "owner": "agent"}),
                ext(8, "review/approve", {"sessionId": "$S1", "target": {"kind": "all"}}),
                ext(9, "review/revert", {"sessionId": "$S1", "target": {"kind": "all"}}),
                ext(10, "review/state", {}),
            ],
        },
        {
            "name": "ext/connectors",
            "steps": [
                *BASE,
                ext(2, "connectors/list", {"sessionId": "$S1"}),
                ext(3, "connectors/authUrl", {"sessionId": "$S1", "name": "gmail"}),
                ext(4, "connectors/refresh", {"sessionId": "$S1", "names": ["gmail"]}),
                ext(5, "connectors/toggle", {"sessionId": "$S1", "name": "gmail", "disabled": True}),
                ext(6, "connectors/list", {}),
                ext(7, "connectors/unknown", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/project-links",
            "steps": [
                INITIALIZE,
                ext(1, "projectLinks/list", {}),
                ext(2, "projectLinks/resolveRoot", {"rootPath": "$WS"}),
                ext(3, "projectLinks/picker/load", {"rootPath": "$WS"}),
                ext(4, "projectLinks/picker/loadMore", {"rootPath": "$WS", "cursor": "c"}),
                ext(5, "projectLinks/create", {"rootPath": "$WS", "name": "demo", "defaultBranch": "main"}),
                ext(6, "projectLinks/link", {"rootPath": "$WS", "projectId": "p", "projectName": "n"}),
                ext(7, "projectLinks/unlink", {"rootPath": "$WS"}),
                ext(8, "projectLinks/resolveRoot", {}),
                ext(9, "projectLinks/other", {}),
            ],
        },
        {
            "name": "ext/whoami",
            "steps": [
                *BASE,
                ext(2, "identity/read", {"sessionId": "$S1"}),
                ext(3, "account/read", {"sessionId": "$S1"}),
                ext(4, "identity/read", {}),
                ext(5, "account/other", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/log-level",
            "steps": [
                *BASE,
                ext(2, "logLevel/read", {}),
                ext(3, "logLevel/write", {"sessionId": "$S1", "sessionOverride": "DEBUG"}),
                ext(4, "logLevel/write", {"sessionId": "$S1", "configLevel": "WARNING"}),
                ext(5, "logLevel/write", {"sessionId": "$S1", "configLevel": None, "sessionOverride": None}),
                ext(6, "logLevel/write", {}),
                ext(7, "logLevel/other", {}),
            ],
        },
        {
            "name": "ext/voice",
            "steps": [
                INITIALIZE,
                ext(1, "voice/transcribeStart", {}),
                ext(2, "voice/narrate", {"userMessage": "hi", "assistantText": "hello"}),
                ext(3, "voice/transcribeStop", {}),
                ext(4, "voice/transcribeCancel", {}),
                ext(5, "voice/narrateCancel", {}),
                ext(6, "voice/other", {}),
            ],
        },
        {
            "name": "ext/telemetry",
            "steps": [
                *BASE,
                notify("_telemetry/send", {"sessionId": "$S1", "event": "vibe.at_mention_inserted", "properties": {"kind": "file"}}),
                notify("_telemetry/send", {"sessionId": "$S1", "event": "vibe.unknown_event"}),
                notify("_telemetry/send", {"event": "missing session"}),
                request(2, "session/list", {}),
            ],
        },
    ]


# --------------------------------------------------------------------------
# The command line
# --------------------------------------------------------------------------

#: Argument vectors the entry point answers before it serves anything: every
#: one exits after printing, so no server starts and nothing reads stdin.
ARGV_CASES: list[list[str]] = [
    ["--version"],
    ["-v"],
    ["--vers"],
    ["-h"],
    ["--help"],
    ["--he"],
    ["-hv"],
    ["-vh"],
    ["-vx"],
    ["-xv"],
    ["--help", "--bogus"],
    ["--bogus", "--help"],
    ["--bogus", "x"],
    ["extra"],
    ["-x"],
    ["-"],
    ["--"],
    ["--", "x"],
    ["--setup=1"],
    ["--version=1"],
    ["--legacy-harness=yes"],
    ["--experimental-harness", "--legacy-harness"],
    ["--legacy-harness", "--experimental-harness"],
]

VERSION_LINE = re.compile(r"^(vibe-acp )\d+\.\d+\.\d+\S*$", re.MULTILINE)


def normalize_stream(text: str) -> Any:
    """What one output stream of the entry point carries.

    Usage and error lines are argparse's, generated from the option names, so
    they are kept. A help screen also carries each option's description, which
    the reference authored, so it is reduced to a length and a digest.
    """
    text = VERSION_LINE.sub(r"\1<version>", text)
    if "\noptions:\n" in text:
        return {"prose": len(text), "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()}
    return text


def run_argv(command: list[str], args: list[str]) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="vibe-acp-argv-"))
    try:
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(root / "home"),
            "VIBE_HOME": str(root / "vibe-home"),
            "LANG": "C.UTF-8",
            "TERM": "dumb",
            "NO_COLOR": "1",
            "CI": "true",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
        }
        completed = subprocess.run(
            [*command, *args],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            env=env,
            cwd=root,
            timeout=RESPONSE_TIMEOUT,
            check=False,
        )
        return {
            "args": args,
            "exit": completed.returncode,
            "stdout": normalize_stream(completed.stdout),
            "stderr": normalize_stream(completed.stderr),
        }
    finally:
        shutil.rmtree(root, ignore_errors=True)


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def resolve_reference(reference: Path, expected_commit: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=reference, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise OracleError(f"git rev-parse failed in {reference}: {result.stderr.strip()}")
    commit = result.stdout.strip()
    if expected_commit and commit != expected_commit:
        raise OracleError(f"reference checkout is at {commit}, not the pinned {expected_commit}")
    return {"commit": commit}


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--agent", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--only", action="append", default=[], help="scenario name filter")
    parser.add_argument("--raw", action="store_true", help="also keep the raw messages")
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--jobs", type=int, default=1, help="scenarios run side by side")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def rendered(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.agent is not None:
            command = [str(arguments.agent.resolve())]
            reference = {"commit": "agent-override"}
        else:
            reference = resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-acp"
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
                f"{scenario['name']}: {len(run['steps'])} steps in "
                f"{time.monotonic() - started:.1f}s",
                file=sys.stderr,
            )
            return entry

        # Every scenario owns its directories, its backend and its agent, so
        # they run side by side; the corpus keeps the declaration order.
        with concurrent.futures.ThreadPoolExecutor(max_workers=arguments.jobs) as pool:
            captured = list(pool.map(capture, selected))
        with concurrent.futures.ThreadPoolExecutor(max_workers=arguments.jobs) as pool:
            argv = list(pool.map(lambda args: run_argv(command, args), ARGV_CASES))
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "quietSeconds": arguments.quiet,
            "argv": argv,
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
            if committed.get("argv") != argv:
                differing.append("the command line")
            if differing:
                raise OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except OracleError as error:
        print(f"acp oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
