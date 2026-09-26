#!/usr/bin/env python3
"""Black-box capture of programmatic mode, ``vibe -p``.

Row 13 of ``docs/parity.md`` is programmatic mode. Every scenario builds a
fresh home and workspace behind a local stand-in for the chat-completions API,
runs the ``vibe`` entry point one or more times with the scenario's arguments
and standard input, and records what a caller of ``vibe -p`` meets: the exit
code, standard output, standard error, the requests the model endpoint
received, and the files the run left in the workspace. Nothing is imported
from the reference: its installed ``vibe`` is the oracle, which is what makes
the same scenarios replayable against this port's binary by
``crates/vibe-cli/tests/programmatic_parity_tests.rs``.

The reference modules behind the contract are ``vibe/cli/programmatic.py``
(``ProgrammaticOutput`` and ``run_programmatic``), ``vibe/cli/cli.py``
(``_run_programmatic_mode``, ``_agent_selection``, ``_session_intent``) and
``vibe/cli/entrypoint.py``; the budgets are ``vibe/core/middleware.py``.

Output is recorded three ways. ``json`` and ``streaming`` documents are parsed
and kept as values, with each object's key order kept apart as a layout and a
flag saying whether the bytes are exactly what Python's ``json.dumps`` writes
for that value, so separators, indentation and key order are all measured.
Identities, times and paths become placeholders, and every string the
reference authored (one carrying whitespace that no scenario input holds) is a
length and a SHA-256, which is what ``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/programmatic.py                  # capture the reference
    python3 scripts/parity/programmatic.py --check          # recapture and compare
    python3 scripts/parity/programmatic.py --binary target/debug/vibe --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-cli/tests/programmatic-parity/corpus.json"
SCHEMA_VERSION = 1
RUN_TIMEOUT = 120.0

#: Keys whose string values are identities minted by the program.
ID_KEYS = {"id", "sessionId", "turnId", "relatedEntryId", "callbackId", "operationId"}
#: Keys whose values are wall-clock readings.
TIME_KEYS = {"createdAt", "updatedAt"}
#: Keys whose numeric values measure elapsed time.
DURATION_KEYS = {"durationMs"}
#: The prefixes a line on stderr opens with, which name the kind of failure
#: and are kept whatever the sentence after them says.
STDERR_PREFIXES = ("Error: ", "Warning: ", "Teleport error: ")
STOP_TAG = re.compile(r"^<vibe_stop_event>(.*)</vibe_stop_event>$", re.DOTALL)
UUID = acp.UUID


class OracleError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# The chat-completions stand-in
# --------------------------------------------------------------------------


class Backend:
    """Serves scripted completions in order, then a fixed closing answer, and
    records what each request asked for."""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = list(responses)
        self.requests: list[dict[str, Any]] = []
        self.whoami: dict[str, Any] | None = None
        self.lock = threading.Lock()
        backend = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                return

            def do_GET(self) -> None:  # noqa: N802
                # The console's account answer, which teleport reads first.
                whoami = backend.whoami
                if self.path.rstrip("/").endswith("/api/vibe/whoami") and whoami:
                    self.reply(whoami.get("status", 200), whoami.get("body", {}))
                    return
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
                    backend.requests.append(
                        {
                            "model": body.get("model"),
                            "maxTokens": body.get("max_tokens"),
                            "roles": [message.get("role") for message in body.get("messages") or []],
                            "tools": sorted(
                                tool.get("function", {}).get("name", "")
                                for tool in body.get("tools") or []
                            ),
                        }
                    )
                    response = backend.responses.pop(0) if backend.responses else {"text": "Done."}
                if response.get("delay"):
                    time.sleep(float(response["delay"]))
                if response.get("status"):
                    self.reply(response["status"], response.get("body", {}))
                    return
                model = str(body.get("model", "model"))
                payload = b"".join(
                    b"data: " + json.dumps(item).encode() + b"\n\n"
                    for item in acp.completion_chunks(response, model)
                ) + b"data: [DONE]\n\n"
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def reply(self, status: int, body: Any) -> None:
                payload = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        class Server(http.server.ThreadingHTTPServer):
            def handle_error(self, request: Any, client_address: Any) -> None:
                # A client interrupted mid-answer hangs up; that is the scenario.
                return

        self.server = Server(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------


def text(value: str) -> dict[str, Any]:
    return {"text": value}


def call(identifier: str, name: str, **arguments: Any) -> dict[str, Any]:
    return {"toolCalls": [{"id": identifier, "name": name, "arguments": arguments}]}


def run(*args: str, **extra: Any) -> dict[str, Any]:
    return {"args": list(args), **extra}


READ_ME = {"notes.txt": "remember the milk\n"}
#: A provider and model of the scenario's own, so the request shows the launch
#: followed the configuration rather than a default.
CUSTOM_PROVIDER = (
    'active_model = "oracle-model"\n'
    "\n[[providers]]\n"
    'name = "oracle-provider"\n'
    'api_base = "$BACKEND/v1"\n'
    'api_key_env_var = "ORACLE_PROVIDER_KEY"\n'
    'api_style = "openai"\n'
    'backend = "generic"\n'
    "\n[[models]]\n"
    'name = "oracle-model-v1"\n'
    'provider = "oracle-provider"\n'
    'alias = "oracle-model"\n'
    "input_price = 1000000.0\n"
    "output_price = 0.0\n"
)

#: Where teleport asks who the key belongs to and what it links to.
TELEPORT_URLS = 'console_base_url = "$BACKEND"\nvibe_base_url = "$BACKEND"\n'


def scenarios() -> list[dict[str, Any]]:
    many_reads = [call(f"call_read_{index}", "read_file", file_path="notes.txt") for index in range(22)]
    return [
        # -- The three output formats ------------------------------------------
        {"name": "output/text", "runs": [run("-p", "hello", backend=[text("Hi there.")])]},
        {"name": "output/json", "runs": [run("-p", "hello", "--output", "json", backend=[text("Hi there.")])]},
        {
            "name": "output/streaming",
            "runs": [run("-p", "hello", "--output", "streaming", backend=[text("Hi there.")])],
        },
        {
            "name": "output/text-unicode",
            "runs": [run("-p", "héllo", backend=[text("Ça va, 世界   ok")])],
        },
        {
            "name": "output/json-unicode",
            "runs": [run("-p", "héllo", "--output", "json", backend=[text("Ça va, 世界 \"q\" \\ ok")])],
        },
        {
            "name": "output/text-no-answer",
            "runs": [run("-p", "hello", backend=[text("")])],
        },
        {
            "name": "output/json-tool-call",
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--output", "json",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("It says milk.")],
                )
            ],
        },
        {
            "name": "output/streaming-tool-call",
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--output", "streaming",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("It says milk.")],
                )
            ],
        },
        {"name": "output/ndjson-is-no-format", "runs": [run("-p", "hello", "--output", "ndjson")]},
        # -- Where the prompt comes from ---------------------------------------
        {"name": "prompt/stdin", "runs": [run("-p", stdin="from stdin\n", backend=[text("Piped.")])]},
        {
            "name": "prompt/argument-wins-over-stdin",
            "runs": [run("-p", "from argument", stdin="from stdin\n", backend=[text("Argued.")])],
        },
        {"name": "prompt/empty", "runs": [run("-p", "")]},
        {"name": "prompt/empty-stdin", "runs": [run("-p", stdin="  \n")]},
        {"name": "prompt/blank", "runs": [run("-p", "   ", backend=[text("Blank.")])]},
        # -- Sessions ------------------------------------------------------------
        {"name": "session/resume-without-id", "runs": [run("-p", "x", "--resume")]},
        {"name": "session/resume-unknown", "runs": [run("-p", "x", "--resume", "nope")]},
        {"name": "session/continue-nothing", "runs": [run("-p", "x", "-c")]},
        {
            "name": "session/continue-json",
            "runs": [
                run("-p", "first", backend=[text("One.")]),
                run("-p", "second", "-c", "--output", "json", backend=[text("Two.")]),
            ],
        },
        {
            "name": "session/continue-streaming",
            "runs": [
                run("-p", "first", backend=[text("One.")]),
                run("-p", "second", "-c", "--output", "streaming", backend=[text("Two.")]),
            ],
        },
        {
            "name": "session/continue-text-without-answer",
            "runs": [
                run("-p", "first", backend=[text("One.")]),
                run("-p", "second", "-c", backend=[text("")]),
            ],
        },
        {
            "name": "session/resume-by-id",
            "runs": [
                run("-p", "first", "--output", "json", backend=[text("One.")]),
                run("-p", "second", "--resume", "$SESSION", backend=[text("Two.")]),
            ],
        },
        # -- Budgets -----------------------------------------------------------
        {
            "name": "budget/none-is-unbounded",
            "files": READ_ME,
            "runs": [run("-p", "read a lot", backend=[*many_reads, text("Read them all.")])],
        },
        {
            "name": "budget/max-turns",
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--max-turns", "1", "--output", "streaming",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("Unreached.")],
                )
            ],
        },
        {
            "name": "budget/max-turns-json",
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--max-turns", "1", "--output", "json",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("Unreached.")],
                )
            ],
        },
        {"name": "budget/max-turns-zero", "runs": [run("-p", "hello", "--max-turns", "0")]},
        {
            "name": "budget/max-tokens",
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--max-tokens", "12",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("Unreached.")],
                )
            ],
        },
        {
            "name": "budget/max-tokens-large",
            "runs": [run("-p", "hello", "--max-tokens", "12345678", backend=[text("Plenty.")])],
        },
        {
            "name": "budget/max-price",
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--max-price", "0.00001",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("Unreached.")],
                )
            ],
        },
        {
            "name": "budget/max-price-configured-model",
            "config": CUSTOM_PROVIDER,
            "env": {"ORACLE_PROVIDER_KEY": "oracle-provider-key"},
            "files": READ_ME,
            "runs": [
                run(
                    "-p", "read it", "--max-price", "0.005",
                    backend=[call("call_read", "read_file", file_path="notes.txt"), text("Unreached.")],
                )
            ],
        },
        {"name": "budget/negative-price", "runs": [run("-p", "hello", "--max-price", "-1")]},
        {"name": "budget/negative-tokens", "runs": [run("-p", "hello", "--max-tokens", "-5")]},
        # -- Approvals ----------------------------------------------------------
        {
            "name": "approval/denied",
            "runs": [
                run(
                    "-p", "write it", "--agent", "ask", "--output", "streaming",
                    backend=[call("call_write", "write_file", file_path="a.txt", content="x"), text("Unreached.")],
                )
            ],
        },
        {
            "name": "approval/default-agent-applies-edits",
            "runs": [
                run(
                    "-p", "write it",
                    backend=[call("call_write", "write_file", file_path="a.txt", content="x"), text("Written.")],
                )
            ],
        },
        {
            "name": "approval/auto-approve-selects-its-agent",
            "runs": [
                run(
                    "-p", "run it", "--auto-approve", "--output", "json",
                    backend=[call("call_shell", "bash", command="touch made.txt"), text("Ran.")],
                )
            ],
        },
        {
            "name": "approval/auto-approve-over-an-agent",
            "runs": [
                run(
                    "-p", "run it", "--auto-approve", "--agent", "ask", "--output", "json",
                    backend=[call("call_shell", "bash", command="touch made.txt"), text("Ran.")],
                )
            ],
        },
        # -- Tools -------------------------------------------------------------
        {
            "name": "tools/enabled-only",
            "runs": [run("-p", "hello", "--enabled-tools", "read_file", backend=[text("Hi.")])],
        },
        {
            "name": "tools/disabled",
            "runs": [run("-p", "hello", "--disabled-tools", "bash", backend=[text("Hi.")])],
        },
        {
            "name": "tools/edit",
            "files": {"quotes.txt": "say 'hi'\n"},
            "runs": [
                run(
                    "-p", "edit it", "--output", "json",
                    backend=[
                        call(
                            "call_edit", "edit", file_path="quotes.txt",
                            old_string="say 'hi'", new_string='say "hi"\tnow',
                        ),
                        text("Edited."),
                    ],
                )
            ],
        },
        {
            "name": "tools/failed-read",
            "runs": [
                run(
                    "-p", "read it", "--output", "json",
                    backend=[call("call_read", "read_file", file_path="missing.txt"), text("Gone.")],
                )
            ],
        },
        {
            "name": "tools/withheld-question",
            "runs": [
                run(
                    "-p", "ask me", "--output", "json",
                    backend=[
                        call("call_ask", "ask_user_question", questions=[{"question": "Which?", "options": []}]),
                        text("Asked."),
                    ],
                )
            ],
        },
        {
            "name": "tools/todo",
            "runs": [
                run(
                    "-p", "plan it", "--output", "streaming",
                    backend=[
                        call("call_todo", "todo", action="write", todos=[{"id": "1", "content": "Step one"}]),
                        text("Planned."),
                    ],
                )
            ],
        },
        # -- Failures ------------------------------------------------------------
        {
            "name": "failure/unauthorized",
            "runs": [run("-p", "hello", backend=[{"status": 401, "body": {"message": "Unauthorized"}}])],
        },
        {
            "name": "failure/bad-request-streaming",
            "runs": [
                run(
                    "-p", "hello", "--output", "streaming",
                    backend=[{"status": 400, "body": {"message": "bad"}}],
                )
            ],
        },
        {
            "name": "failure/bad-request-json",
            "runs": [run("-p", "hello", "--output", "json", backend=[{"status": 400, "body": {"message": "bad"}}])],
        },
        {"name": "failure/missing-key", "env": {"MISTRAL_API_KEY": None}, "runs": [run("-p", "hello")]},
        {
            "name": "failure/missing-key-before-empty-prompt",
            "env": {"MISTRAL_API_KEY": None},
            "runs": [run("-p", "")],
        },
        {
            "name": "failure/key-in-dotenv",
            "env": {"MISTRAL_API_KEY": None},
            "vibeHomeFiles": {".env": "MISTRAL_API_KEY=from-dotenv\n"},
            "runs": [run("-p", "hello", backend=[text("Dotenv.")])],
        },
        # -- Configuration ----------------------------------------------------------
        {
            "name": "config/active-provider",
            "config": CUSTOM_PROVIDER,
            "env": {"ORACLE_PROVIDER_KEY": "oracle-provider-key", "MISTRAL_API_KEY": None},
            "runs": [run("-p", "hello", backend=[text("Configured.")])],
        },
        {
            "name": "config/active-provider-missing-key",
            "config": CUSTOM_PROVIDER,
            "runs": [run("-p", "hello")],
        },
        # -- Workspace trust --------------------------------------------------------
        {
            "name": "trust/untrusted-project-config",
            "files": {"AGENTS.md": "# notes\n", ".vibe/config.toml": "worktree_limit = 3\n"},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "trust/trusted-for-the-run",
            "files": {"AGENTS.md": "# notes\n"},
            "runs": [run("-p", "hello", "--trust", backend=[text("Hi.")])],
        },
        # -- Interruption -------------------------------------------------------
        {
            "name": "interrupt/during-a-turn",
            "runs": [
                run(
                    "-p", "hello", "--output", "streaming",
                    backend=[{"text": "Late.", "delay": 4}],
                    interrupt=True,
                )
            ],
        },
        # -- Teleport ------------------------------------------------------------
        {
            "name": "teleport/unverified-key",
            "config": TELEPORT_URLS,
            "runs": [run("-p", "hello", "--teleport", backend=[text("Unreached.")])],
        },
        {
            "name": "teleport/codestral-key",
            "config": TELEPORT_URLS,
            "whoami": {"body": {"plan_type": "mistral_code", "plan_name": "Codestral"}},
            "runs": [run("-p", "hello", "--teleport", backend=[text("Unreached.")])],
        },
        {
            "name": "teleport/outside-a-repository",
            "config": TELEPORT_URLS,
            "whoami": {"body": {"plan_type": "api", "plan_name": "Scale"}},
            "runs": [run("-p", "hello", "--teleport", backend=[text("Unreached.")])],
        },
        {
            "name": "teleport/non-mistral-model",
            "config": CUSTOM_PROVIDER,
            "env": {"ORACLE_PROVIDER_KEY": "oracle-provider-key"},
            "runs": [run("-p", "hello", "--teleport", backend=[text("Unreached.")])],
        },
    ]


# --------------------------------------------------------------------------
# One scenario
# --------------------------------------------------------------------------


def compose_config(scenario: dict[str, Any], backend: Backend) -> str:
    """The shared configuration with the scenario's own keys merged in: its
    top-level keys ahead of the tables, replacing a shared key they name, and
    its tables after them."""

    def split(text: str) -> tuple[list[str], str]:
        lines = text.splitlines(keepends=True)
        at = next((i for i, line in enumerate(lines) if line.lstrip().startswith("[")), len(lines))
        return lines[:at], "".join(lines[at:])

    def key(line: str) -> str:
        return line.split("=", 1)[0].strip()

    own_top, own_tables = split(scenario.get("config", "").replace("$BACKEND", backend.base))
    shared_top, shared_tables = split(acp.base_config(backend))
    named = {key(line) for line in own_top if "=" in line}
    kept = [line for line in shared_top if "=" not in line or key(line) not in named]
    return "".join(own_top + kept) + shared_tables + ("\n" + own_tables if own_tables else "")


class World:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.vibe_home = root / "vibe-home"
        self.workspace = root / "workspace"
        for directory in (self.home, self.vibe_home, self.workspace):
            directory.mkdir(parents=True, exist_ok=True)


def write_tree(root: Path, files: dict[str, str]) -> None:
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


def workspace_files(root: Path) -> list[str]:
    return sorted(
        str(path.relative_to(root))
        for path in root.rglob("*")
        if path.is_file() and ".git" not in path.parts
    )


def run_once(
    command: list[str],
    step: dict[str, Any],
    env: dict[str, str],
    world: World,
    backend: Backend,
    session: str | None,
) -> dict[str, Any]:
    args = [arg.replace("$SESSION", session or "") for arg in step["args"]]
    backend.responses = copy.deepcopy(step.get("backend", []))
    backend.requests = []
    before = workspace_files(world.workspace)
    process = subprocess.Popen(
        [*command, *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=world.workspace,
    )
    if step.get("interrupt"):
        assert process.stdin is not None
        process.stdin.close()
        deadline = time.monotonic() + RUN_TIMEOUT
        while not backend.requests and time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.05)
        # The request is in flight; the turn is running.
        time.sleep(0.5)
        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=RUN_TIMEOUT)
    else:
        stdout, stderr = process.communicate(
            input=step.get("stdin", "").encode("utf-8"), timeout=RUN_TIMEOUT
        )
    after = workspace_files(world.workspace)
    return {
        "args": args,
        "exit": process.returncode,
        "stdout": stdout.decode("utf-8", errors="replace"),
        "stderr": stderr.decode("utf-8", errors="replace"),
        "requests": list(backend.requests),
        "created": sorted(set(after) - set(before)),
    }


def learned_session(stdout: str) -> str | None:
    """The session a `json` run printed, which a later run resumes."""

    try:
        document = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    entries = document.get("history") if isinstance(document, dict) else document
    for entry in entries or []:
        if isinstance(entry, dict) and isinstance(entry.get("sessionId"), str):
            return entry["sessionId"]
    return None


def run_scenario(scenario: dict[str, Any], command: list[str]) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="vibe-programmatic-oracle-")).resolve()
    world = World(root)
    backend = Backend([])
    try:
        (world.vibe_home / "config.toml").write_text(compose_config(scenario, backend), encoding="utf-8")
        backend.whoami = scenario.get("whoami")
        write_tree(world.workspace, scenario.get("files", {}))
        write_tree(world.vibe_home, scenario.get("vibeHomeFiles", {}))
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(world.home),
            "VIBE_HOME": str(world.vibe_home),
            "MISTRAL_API_KEY": "oracle-key",
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
        runs = []
        session: str | None = None
        for step in scenario["runs"]:
            observed = run_once(command, step, env, world, backend, session)
            session = learned_session(observed["stdout"]) or session
            runs.append(observed)
        return {
            "runs": runs,
            "paths": {
                "workspace": str(world.workspace),
                "vibeHome": str(world.vibe_home),
                "home": str(world.home),
                "root": str(root),
                "backend": backend.base,
            },
        }
    finally:
        backend.close()
        shutil.rmtree(root, ignore_errors=True)


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def digest(value: str) -> dict[str, Any]:
    return {"prose": len(value), "sha256": hashlib.sha256(value.encode("utf-8")).hexdigest()}


class Normalizer:
    def __init__(self, scenario: dict[str, Any], paths: dict[str, str]) -> None:
        self.authored = acp.authored_strings(scenario)
        self.paths = sorted(
            (
                (paths["workspace"], "<ws>"),
                (paths["vibeHome"], "<vibe-home>"),
                (paths["home"], "<home>"),
                (paths["root"], "<root>"),
                (paths["backend"], "<backend>"),
            ),
            key=lambda item: -len(item[0]),
        )
        self.ids: dict[str, str] = {}

    def identity(self, value: str) -> str:
        if value in self.authored:
            return value
        if value not in self.ids:
            self.ids[value] = f"<id-{len(self.ids) + 1}>"
        return self.ids[value]

    def text(self, value: str) -> str:
        for path, placeholder in self.paths:
            value = value.replace(path, placeholder)
        for original, placeholder in sorted(self.ids.items(), key=lambda item: -len(item[0])):
            if len(original) >= 8:
                value = value.replace(original, placeholder)
        value = UUID.sub(lambda match: self.identity(match.group(0)), value)
        value = acp.APPROX_CHARS.sub(r"\1<n>", value)
        return value

    def prose(self, value: str) -> Any:
        if value in self.authored:
            return value
        normalized = self.text(value)
        if normalized in self.authored or not re.search(r"\s", normalized):
            return normalized
        tagged = STOP_TAG.match(normalized)
        if tagged:
            return {"stopEvent": self.prose(tagged.group(1))}
        return digest(normalized)

    def value(self, value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            return {name: self.value(item, name) for name, item in value.items()}
        if isinstance(value, list):
            return [self.value(item, key) for item in value]
        if key in TIME_KEYS and isinstance(value, int | float) and not isinstance(value, bool):
            return "<time>"
        if key in DURATION_KEYS and isinstance(value, int | float) and not isinstance(value, bool):
            return "<duration>"
        if isinstance(value, str):
            if key in ID_KEYS and value:
                return self.identity(value)
            return self.prose(value)
        return value

    def collect_ids(self, value: Any, key: str | None = None) -> None:
        if isinstance(value, dict):
            for name, item in value.items():
                self.collect_ids(item, name)
        elif isinstance(value, list):
            for item in value:
                self.collect_ids(item, key)
        elif isinstance(value, str) and key in ID_KEYS and value:
            self.identity(value)


def layout(value: Any) -> Any:
    """The shape of a document with every scalar erased: key order and nesting."""

    if isinstance(value, dict):
        return [[key, layout(item)] for key, item in value.items()]
    if isinstance(value, list):
        return [layout(item) for item in value]
    return None


def parse_document(raw: str, indent: int | None) -> tuple[Any, bool] | None:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return None
    rendered = json.dumps(value, indent=indent, ensure_ascii=False)
    return value, rendered == raw


def output_mode(args: list[str]) -> str:
    for index, arg in enumerate(args):
        if arg == "--output" and index + 1 < len(args):
            return args[index + 1]
        if arg.startswith("--output="):
            return arg.split("=", 1)[1]
    return "text"


def normalize_stdout(raw: str, mode: str, exit_code: int, normalizer: Normalizer) -> Any:
    if exit_code == 2 or not raw:
        return normalizer.text(raw)
    if mode == "json":
        body = raw[:-1] if raw.endswith("\n") else raw
        parsed = parse_document(body, 2)
        if parsed is not None:
            value, exact = parsed
            return {
                "json": normalizer.value(value),
                "layout": layout(value),
                "pythonRendering": exact,
                "trailingNewline": raw.endswith("\n"),
            }
    if mode == "streaming":
        lines = raw.split("\n")
        documents: list[Any] = []
        for line in lines[:-1] if raw.endswith("\n") else lines:
            parsed = parse_document(line, None)
            if parsed is None:
                documents.append({"line": normalizer.prose(line) if line else ""})
                continue
            value, exact = parsed
            documents.append(
                {"json": normalizer.value(value), "layout": layout(value), "pythonRendering": exact}
            )
        return {"lines": documents, "trailingNewline": raw.endswith("\n")}
    # `text`: what the assistant said is scenario input; anything else is prose.
    return [normalizer.prose(line) if line else "" for line in raw.split("\n")]


def normalize_stderr(raw: str, exit_code: int, normalizer: Normalizer) -> Any:
    if exit_code == 2:
        # A parse failure: the usage block, which argparse and clap wrap each
        # their own way (row 7 owns it), then the error line verbatim.
        block, _, error = raw.rstrip("\n").rpartition("\n")
        return {"usage": normalizer.prose(block), "error": normalizer.text(error)}
    lines: list[Any] = []
    for line in raw.split("\n"):
        prefix = next((p for p in STDERR_PREFIXES if line.startswith(p)), None)
        if prefix is not None:
            lines.append({"prefix": prefix, "message": normalizer.prose(line[len(prefix):])})
        elif line:
            lines.append(normalizer.prose(line))
        else:
            lines.append("")
    return lines


def normalize(scenario: dict[str, Any], captured: dict[str, Any]) -> list[dict[str, Any]]:
    normalizer = Normalizer(scenario, captured["paths"])
    for observed in captured["runs"]:
        for raw in observed["stdout"].split("\n"):
            try:
                normalizer.collect_ids(json.loads(raw))
            except json.JSONDecodeError:
                pass
        try:
            normalizer.collect_ids(json.loads(observed["stdout"]))
        except json.JSONDecodeError:
            pass
    runs = []
    for observed in captured["runs"]:
        mode = output_mode(observed["args"])
        runs.append(
            {
                "exit": observed["exit"],
                "stdout": normalize_stdout(observed["stdout"], mode, observed["exit"], normalizer),
                "stderr": normalize_stderr(observed["stderr"], observed["exit"], normalizer),
                "requests": observed["requests"],
                "created": observed["created"],
            }
        )
    return runs


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def capture(scenario: dict[str, Any], command: list[str], raw: bool) -> dict[str, Any]:
    captured = run_scenario(scenario, command)
    entry = {"name": scenario["name"], "scenario": scenario, "observed": normalize(scenario, captured)}
    if raw:
        entry["raw"] = captured["runs"]
    return entry


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--binary", type=Path, default=None, help="drive this port's `vibe` instead")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--raw", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.binary is not None:
            command = [str(arguments.binary.resolve())]
            reference = {"commit": "binary-override"}
            dialect = "port"
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
            dialect = "reference"
        selected = [
            s for s in scenarios() if not arguments.only or any(name in s["name"] for name in arguments.only)
        ]
        captured = []
        for scenario in selected:
            started = time.monotonic()
            entry = capture(scenario, command, arguments.raw)
            if dialect == "reference" and not arguments.check:
                # A corpus holds only what the reference answers every time.
                again = capture(scenario, command, False)
                if again["observed"] != entry["observed"]:
                    raise OracleError(f"scenario {scenario['name']} is not deterministic across two captures")
            captured.append(entry)
            print(f"{scenario['name']}: {time.monotonic() - started:.1f}s", file=sys.stderr)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "note": (
                "Captured by scripts/parity/programmatic.py from the pinned reference's `vibe` "
                "entry point. Scenario inputs, paths and identities as placeholders, JSON documents "
                "with their key order and rendering; every other string the program authored is a "
                "length and a SHA-256."
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
    except (OracleError, acp.OracleError, subprocess.TimeoutExpired) as error:
        print(f"programmatic capture failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
