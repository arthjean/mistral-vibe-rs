#!/usr/bin/env python3
"""Black-box capture of the hooks the app server runs around tool calls and turns.

Every scenario starts an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in ``acp.py`` already runs, writes the
``hooks.toml`` files it declares (the workspace's ``.vibe/hooks.toml`` and the
user's ``$VIBE_HOME/hooks.toml``), and drives real turns whose tool calls the
hooks guard. Each hook is one command running a small recorder this script
writes beside the scenario: it keeps the invocation it read on stdin and
answers with what the scenario scripted for that call, so a hook can deny,
rewrite, replace, append, fail, time out or answer garbage.

What is recorded is what row 18 of ``docs/parity.md`` is about: the hook
notices each turn raised and where they fell among the transcript entries, the
invocations every hook read, the conversation the model was sent next (where a
denial, a rewrite, a replaced result or a retry lands), the tool call arguments
the session persisted, and the count and issues ``runtime/read``,
``diagnostics/list`` and ``config/read`` report before and after the hook files
change. Nothing is imported from the reference: its own ``vibe-app-server`` is
the oracle, which is what makes the same scenarios replayable against this port
by ``crates/vibe-app-server/tests/hooks_parity_tests.rs``.

Normalization maps identifiers, paths, times and durations to placeholders and
reduces every string the server authored to its length and SHA-256, which is
what ``NOTICE`` requires of a committed corpus. Hook messages are part of the
contract, so the replay compares those digests exactly.

Usage::

    python3 scripts/parity/hooks.py                  # capture the reference
    python3 scripts/parity/hooks.py --check          # recapture and compare
    python3 scripts/parity/hooks.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
import rewind  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/hooks-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5

#: The recorder every hook command runs. It is this script's own code: it
#: appends the invocation to `stdin/<name>.jsonl` and answers with the entry of
#: `responses/<name>.json` at the index of that invocation, the last entry
#: repeating once the list runs out.
RECORDER = r'''
import fcntl, json, os, sys, time
name = sys.argv[1]
root = os.path.dirname(os.path.abspath(__file__))
data = sys.stdin.buffer.read()
os.makedirs(os.path.join(root, "stdin"), exist_ok=True)
with open(os.path.join(root, "stdin", name + ".jsonl"), "ab+") as log:
    fcntl.flock(log, fcntl.LOCK_EX)
    log.seek(0)
    index = log.read().count(b"\n")
    log.write(data + b"\n")
try:
    with open(os.path.join(root, "responses", name + ".json"), encoding="utf-8") as spec:
        responses = json.load(spec)
except FileNotFoundError:
    responses = [{}]
answer = responses[min(index, len(responses) - 1)]
if answer.get("sleep"):
    time.sleep(answer["sleep"])
    # Only a hook that outlived its wait gets here: a killed one never does.
    with open(os.path.join(root, "finished.log"), "a", encoding="utf-8") as finished:
        finished.write(name + "\n")
sys.stdout.write(answer.get("stdout", ""))
sys.stderr.write(answer.get("stderr", ""))
sys.stdout.flush()
sys.stderr.flush()
sys.exit(answer.get("exit", 0))
'''

#: Transcript keys whose values are identities.
ID_KEYS = rewind.ID_KEYS | {"tool_call_id", "session_id", "parent_session_id"}


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


class Backend(rewind.RecordingBackend):
    """The recording stand-in, also answering a non-streaming request with the
    tool calls its scripted response holds.

    The reference runs a subagent's loop without streaming, and the shared
    stand-in serves only text that way, so a child could never call a tool.
    """

    def __init__(self) -> None:
        super().__init__()
        handler = self.server.RequestHandlerClass
        backend = self
        streaming = handler.do_POST

        def do_post(self: Any) -> None:
            length = int(self.headers.get("content-length") or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {}
            with backend.lock:
                scripted = backend.responses[0] if backend.responses else {}
                answers = not body.get("stream") and bool(scripted.get("toolCalls"))
                if answers:
                    backend.responses.pop(0)
                    backend.requests += 1
                    backend.conversations.append(rewind.conversation(body.get("messages", [])))
                    backend.bodies.append(body)
            if not answers:
                # The body is handed back, and the socket after it.
                stream = self.rfile
                self.rfile = rewind._Replay(raw, stream)
                try:
                    streaming(self)
                finally:
                    self.rfile = stream
                return
            message = {
                "role": "assistant",
                "content": scripted.get("text") or "",
                "tool_calls": [
                    {
                        "id": call.get("id", f"call_{index}"),
                        "type": "function",
                        "function": {"name": call["name"], "arguments": json.dumps(call.get("arguments", {}))},
                    }
                    for index, call in enumerate(scripted["toolCalls"])
                ],
            }
            self.reply(200, {
                "id": "cmpl-oracle",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": str(body.get("model", "model")),
                "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            })

        handler.do_POST = do_post


def write_hooks(session: rewind.Session, hooks: dict[str, str]) -> None:
    world = session.world
    targets = {
        "project": world.workspace / ".vibe/hooks.toml",
        "user": world.vibe_home / "hooks.toml",
    }
    for where, text in hooks.items():
        target = targets[where]
        if text is None:
            if target.exists():
                target.unlink()
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(session.substitute(text), encoding="utf-8")


def write_responses(root: Path, responses: dict[str, list[dict[str, Any]]]) -> None:
    directory = root / "responses"
    directory.mkdir(parents=True, exist_ok=True)
    for name, answers in responses.items():
        (directory / f"{name}.json").write_text(json.dumps(answers), encoding="utf-8")


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = Backend()
    root = Path(tempfile.mkdtemp(prefix="vibe-hooks-oracle-"))
    session = rewind.Session(root)
    world = session.world
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        (world.vibe_home / "config.toml").write_text(
            rewind.base_config(backend, scenario.get("config", "")), encoding="utf-8"
        )
        if scenario.get("trusted", True):
            (world.vibe_home / "trusted_folders.toml").write_text(
                f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n",
                encoding="utf-8",
            )
        acp.write_tree(world.workspace, scenario.get("files", {}))
        for relative, text in scenario.get("rootFiles", {}).items():
            acp.write_tree(root, {relative: session.substitute(text)})
        (root / "hook.py").write_text(RECORDER, encoding="utf-8")
        write_responses(root, scenario.get("responses", {}))
        write_hooks(session, scenario.get("hooks", {}))
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
        server = rewind.Server(command, env, world.workspace, {}, world)
        steps: list[dict[str, Any]] = []
        try:
            run_steps(scenario, server, session, backend, steps, quiet)
            # Long enough for a hook nobody killed to finish its wait.
            time.sleep(scenario.get("linger", 0))
        except rewind.OracleError as error:
            steps.append({"failure": str(error)})
        finally:
            server.stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
            "stdin": recorded_stdin(root),
            "finished": sorted(
                (root / "finished.log").read_text(encoding="utf-8").split()
                if (root / "finished.log").is_file()
                else []
            ),
            "conversations": [
                transcript(body.get("messages", [])) for body in backend.bodies
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
        shutil.rmtree(root, ignore_errors=True)


def recorded_stdin(root: Path) -> dict[str, list[Any]]:
    found: dict[str, list[Any]] = {}
    directory = root / "stdin"
    if not directory.is_dir():
        return found
    for path in sorted(directory.glob("*.jsonl")):
        entries = []
        for line in path.read_text(encoding="utf-8").splitlines():
            try:
                entries.append(json.loads(line))
            except json.JSONDecodeError:
                entries.append({"unparsed": line})
        found[path.stem] = entries
    return found


def transcript(messages: list[Any]) -> list[dict[str, Any]]:
    """The conversation a request sends, the system prompt aside."""

    turns: list[dict[str, Any]] = []
    for message in messages:
        if not isinstance(message, dict) or message.get("role") == "system":
            continue
        content = message.get("content")
        if isinstance(content, list):
            content = "".join(
                part.get("text", "") for part in content if isinstance(part, dict)
            )
        entry: dict[str, Any] = {"role": message.get("role"), "content": content or ""}
        if message.get("tool_calls"):
            # A request nests the call under `function`; the port's own
            # `messages.jsonl` does not, a layout row 16 answers for.
            entry["toolCalls"] = [
                {
                    "id": call.get("id"),
                    "name": call.get("function", call).get("name"),
                    "arguments": call.get("function", call).get("arguments"),
                }
                for call in message["tool_calls"]
            ]
        if message.get("tool_call_id") or message.get("call_id"):
            entry["toolCallId"] = message.get("tool_call_id") or message.get("call_id")
        turns.append(entry)
    return turns


def persisted(world: acp.World) -> list[Any]:
    """The messages every session wrote to its `messages.jsonl`, in the order
    the sessions were created."""

    logs = sorted(
        (world.vibe_home / "logs/session").glob("*/messages.jsonl"),
        key=lambda path: path.parent.name,
    )
    sessions = []
    for log in logs:
        lines = []
        for line in log.read_text(encoding="utf-8").splitlines():
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            lines.append(message)
        sessions.append(transcript(lines) if lines else [])
        for entry, message in zip(sessions[-1], [m for m in lines if m.get("role") != "system"]):
            if message.get("injected"):
                entry["injected"] = True
    return sessions


def run_steps(
    scenario: dict[str, Any],
    server: rewind.Server,
    session: rewind.Session,
    backend: rewind.RecordingBackend,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    identifier = 1
    server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                 "params": {"clientInfo": {"name": "hooks-oracle", "version": "0"}}})
    server.collect(identifier, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    pending: list[dict[str, Any]] = []
    for step in scenario["steps"]:
        identifier += 1
        if "start" in step:
            options = step["start"]
            config: dict[str, Any] = {
                "cwd": session.substitute(options.get("cwd", "$WS")),
                "agent": options.get("agent", "auto-approve"),
            }
            if "addDirs" in options:
                config["workspaceRoots"] = session.substitute(options["addDirs"])
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start",
                         "params": {"agentConfig": config}})
            for message in server.collect(identifier, quiet):
                session.learn(message)
            continue
        if "turn" in step and step.get("background"):
            # The turn is left running until an `interrupt` step ends it.
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": rewind.render_turn("reference", target, step["turn"])})
            time.sleep(step.get("settle", 1.0))
            pending = server.collect(None, quiet)
            for message in pending:
                session.learn(message)
            continue
        if "interrupt" in step:
            started = next(
                (m for m in reversed(pending) if m.get("method") == "turn/started"), {}
            )
            turn_id = ((started.get("params") or {}).get("turn") or {}).get("id")
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/interrupt",
                         "params": {"sessionId": session.substitute("$S1"), "expectedTurnId": turn_id}})
            observed = pending + server.wait_for(rewind.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": [m for m in observed if "method" in m and "id" not in m]})
            pending = []
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": rewind.render_turn("reference", target, step["turn"])})
            observed = server.wait_for(rewind.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": [m for m in observed if "method" in m and "id" not in m]})
            continue
        if "hooks" in step:
            write_hooks(session, step["hooks"])
            continue
        if "responses" in step:
            write_responses(world.root, step["responses"])
            continue
        if "write" in step:
            acp.write_tree(world.workspace, step["write"])
            continue
        if "persisted" in step:
            steps.append({"persisted": persisted(world)})
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            session.learn(message)
        response = next(
            (m for m in observed if "method" not in m and m.get("id") == identifier), None
        )
        steps.append({
            "method": request["method"],
            "pick": step.get("pick"),
            "response": {k: v for k, v in (response or {}).items() if k != "jsonrpc"},
            "notifications": [m for m in observed if "method" in m and "id" not in m],
        })


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: A subagent's log directory under its parent's, ending in eight hex digits
#: of its identifier.
CHILD_DIRECTORY = re.compile(r"(/agents/[A-Za-z0-9_-]+_<time>_)[0-9a-f]{8}")


class Normalizer(rewind.Normalizer):
    def text(self, value: str) -> str:
        return CHILD_DIRECTORY.sub(r"\1<short>", super().text(value))

    def value(self, value: Any, key: str | None = None) -> Any:
        if key == "duration_ms" and isinstance(value, int | float):
            return "<duration>"
        if key in HOOK_ID_KEYS and isinstance(value, str) and value:
            if value in self.authored:
                return value
            return self.identity(value)
        if isinstance(value, str) and value.startswith(TOML_FAILURE):
            # The TOML reader's own words are the library's, not the hook
            # contract's (the ledger row on library error text).
            return TOML_FAILURE + "<toml-error>"
        return super().value(value, key)

    def collect_ids(self, value: Any, key: str | None = None) -> None:
        if key in HOOK_ID_KEYS and isinstance(value, str) and value:
            if value not in self.authored:
                self.identity(value)
            return
        if isinstance(value, dict):
            for name, item in sorted(value.items()):
                self.collect_ids(item, name)
            return
        super().collect_ids(value, key)


#: The identifiers a hook invocation carries under its own snake_case keys.
HOOK_ID_KEYS = {"tool_call_id", "session_id", "parent_session_id"}

#: How a hook file that is not TOML is reported, before the reader's message.
TOML_FAILURE = "Failed to parse: "

#: Notification fields that count or date events, which every other part of
#: the server moves.
DROPPED = {"emittedAt", "eventId"}


def reduce_notification(message: dict[str, Any]) -> dict[str, Any]:
    params = {k: v for k, v in (message.get("params") or {}).items() if k not in DROPPED}
    for operation in params.get("patch") or []:
        # A patch that stamps a time names the key in its path.
        if isinstance(operation, dict) and str(operation.get("path", "")).rsplit("/", 1)[-1] in rewind.TIME_KEYS:
            operation["value"] = "<time>"
    return {"method": message.get("method"), "params": params}


def is_notice(message: dict[str, Any]) -> bool:
    entry = (message.get("params") or {}).get("entry") or {}
    return entry.get("type") == "notice"


def reduce_result(result: Any, pick: list[str] | None) -> Any:
    """A runtime snapshot reduced to what hooks move in it; a picked answer to
    the keys named."""

    if not isinstance(result, dict):
        return result
    if pick:
        return {key: result.get(key) for key in pick}
    if isinstance(result.get("runtime"), dict):
        runtime = result["runtime"]
        result = {**result, "runtime": {"hooksCount": runtime.get("hooksCount"), "issues": runtime.get("issues")}}
        result.pop("sessionLog", None)
    return result


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    normalizer = Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    reduced: list[dict[str, Any]] = []
    for step in run["steps"]:
        if "failure" in step:
            reduced.append({"failure": step["failure"]})
        elif "turn" in step:
            reduced.append({"turn": [
                reduce_notification(message)
                for message in step["turn"]
                if str(message.get("method", "")).startswith(("history/", "turn/"))
                and (not scenario.get("noticesOnly") or is_notice(message))
            ]})
        elif "persisted" in step:
            reduced.append({"persisted": copy.deepcopy(step["persisted"])})
        else:
            response = copy.deepcopy(step["response"])
            if "result" in response:
                response["result"] = reduce_result(response["result"], step.get("pick"))
            reduced.append({
                "method": step["method"],
                "response": response,
                "notifications": [m.get("method") for m in step["notifications"]],
            })
    # A subagent's conversation opens on the task prompt its parent composed,
    # which is row 20's contract; what hooks add after it is this one's.
    turns = {step["turn"] for step in scenario["steps"] if "turn" in step}
    conversations = copy.deepcopy(run["conversations"])
    for conversation in conversations:
        if conversation and conversation[0].get("role") == "user" and conversation[0].get("content") not in turns:
            conversation[0]["content"] = "<task prompt>"
    stdin = copy.deepcopy(run["stdin"])
    # Identities are numbered before anything is rewritten, walking keys
    # sorted: the order a server writes an object's keys in is not part of
    # the contract, so it must not decide which placeholder an id gets.
    for part in (reduced, stdin, conversations):
        normalizer.collect_ids(part)
    steps: list[Any] = []
    for step in reduced:
        if "failure" in step:
            steps.append({"failure": normalizer.text(step["failure"])})
        elif "method" in step:
            steps.append({**step, "response": normalizer.value(step["response"])})
        else:
            steps.append(normalizer.value(step))
    return {
        "steps": steps,
        "stdin": normalizer.value(stdin),
        "finished": run["finished"],
        "conversations": normalizer.value(conversations),
    }


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def hook(name: str, kind: str, recorder: str | None = None, **extra: Any) -> str:
    """One `[[hooks]]` entry running the recorder under `recorder` (the hook's
    own name by default)."""

    lines = [
        "[[hooks]]",
        f"name = {json.dumps(name)}",
        f"type = {json.dumps(kind)}",
        f"command = 'python3 \"$ROOT/hook.py\" {recorder or name}'",
    ]
    for key, value in extra.items():
        lines.append(f"{key} = {json.dumps(value)}")
    return "\n".join(lines) + "\n\n"


def answer(**fields: Any) -> dict[str, Any]:
    return {"stdout": json.dumps(fields)}


SILENT: dict[str, Any] = {}


def send(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    step: dict[str, Any] = {"send": {"method": method, "params": params}}
    if pick:
        step["pick"] = pick
    return step


def runtime(session: str = "$S1") -> dict[str, Any]:
    return send("runtime/read", sessionId=session)


def diagnostics(session: str = "$S1") -> dict[str, Any]:
    return {"send": {"method": "diagnostics/list", "params": {"sessionId": session}}}


def config_count(**params: Any) -> dict[str, Any]:
    return send("config/read", pick=["hooksCount"], **params)


BASH_ECHO = [
    {"toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")]},
    {"text": "Finished."},
]
ONE_TURN = [{"start": {}}, {"turn": "Run echo"}, {"persisted": True}]


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- pre_tool ------------------------------------------------------
        {
            "name": "pre_tool/deny",
            "hooks": {"project": hook("guard", "pre_tool", match="bash") + hook("after", "pre_tool")},
            "responses": {"guard": [answer(decision="deny", reason="bash is off limits")]},
            "backend": BASH_ECHO,
            "steps": [*ONE_TURN, runtime(), diagnostics()],
        },
        {
            "name": "pre_tool/allow",
            "hooks": {"project": hook("silent", "pre_tool") + hook("noted", "pre_tool")
                      + hook("explicit", "pre_tool")},
            "responses": {
                "noted": [answer(decision="allow", system_message="Checked by noted")],
                "explicit": [answer(decision="allow", reason="ignored when allowing")],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/rewrite",
            "hooks": {"project": hook("rewriter", "pre_tool") + hook("second", "pre_tool")
                      + hook("third", "pre_tool")},
            "responses": {
                "rewriter": [answer(hook_specific_output={"tool_input": {"command": "echo rewritten"}})],
                "second": [answer(system_message="Second rewrite",
                                  hook_specific_output={"tool_input": {"command": "echo twice", "timeout": 30}})],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/rewrite-declaration-order",
            "files": {"notes.txt": "one\ntwo\nthree\n"},
            "hooks": {"project": hook("rewriter", "pre_tool")},
            "responses": {
                "rewriter": [answer(hook_specific_output={"tool_input": {
                    "limit": 2, "offset": 1, "file_path": "notes.txt"}})],
            },
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "notes.txt"}, "call_r1")]},
                {"text": "Read."},
            ],
            "steps": [{"start": {}}, {"turn": "Read the notes"}, {"persisted": True}],
        },
        {
            "name": "pre_tool/rewrite-invalid",
            "hooks": {"project": hook("rewriter", "pre_tool") + hook("after", "pre_tool")},
            "responses": {
                "rewriter": [answer(hook_specific_output={"tool_input": {"command": ["echo", "x"]}})],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/rewrite-unknown-field",
            "hooks": {"project": hook("rewriter", "pre_tool")},
            "responses": {
                "rewriter": [answer(hook_specific_output={"tool_input": {"command": "echo x", "bogus": 1}})],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/strict-failure",
            "hooks": {"project": hook("strict", "pre_tool", strict=True) + hook("after", "pre_tool")},
            "responses": {"strict": [{"stderr": "policy engine down\n", "exit": 2}]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/failures-warn",
            "hooks": {"project": hook("stderr", "pre_tool") + hook("stdout", "pre_tool")
                      + hook("bare", "pre_tool") + hook("both", "pre_tool")},
            "responses": {
                "stderr": [{"stderr": "  something broke  \n", "stdout": "ignored", "exit": 1}],
                "stdout": [{"stdout": "only stdout\n", "exit": 4}],
                "bare": [{"exit": 3}],
                "both": [{"stderr": "deny on stderr", "stdout": json.dumps({"decision": "deny"}), "exit": 1}],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/timeouts",
            "linger": 3.5,
            "hooks": {"project": hook("slow", "pre_tool", timeout=0.5)
                      + hook("slow_int", "pre_tool", timeout=1)
                      + hook("slow_strict", "pre_tool", timeout=0.25, strict=True)},
            "responses": {
                "slow": [{"sleep": 3, "stdout": answer(decision="deny", reason="late")["stdout"]}],
                "slow_int": [{"sleep": 3}],
                "slow_strict": [{"sleep": 3}],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/invalid-responses",
            "hooks": {"project": "".join(hook(name, "pre_tool") for name in (
                "text", "array", "string", "number", "null", "decision", "reason",
                "specific", "specific_null", "tool_input", "many", "long", "trailing",
                "unterminated", "nan", "bom"))},
            "responses": {
                "text": [{"stdout": "not json at all"}],
                "array": [{"stdout": "[1, 2]"}],
                "string": [{"stdout": "\"allow\""}],
                "number": [{"stdout": "4.5"}],
                "null": [{"stdout": "null"}],
                "decision": [answer(decision="maybe")],
                "reason": [answer(decision="deny", reason=5)],
                "specific": [answer(hook_specific_output="rewrite")],
                "specific_null": [answer(hook_specific_output=None)],
                "tool_input": [answer(hook_specific_output={"tool_input": ["x"]})],
                "many": [answer(decision=1, reason=[], system_message={}, hook_specific_output={"additional_context": 3})],
                "long": [answer(decision="a" * 80)],
                "trailing": [{"stdout": "{\"decision\": \"allow\"} extra"}],
                "unterminated": [{"stdout": "{\"decision\": \"allo"}],
                "nan": [{"stdout": "NaN"}],
                "bom": [{"stdout": "﻿{}"}],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/matchers",
            "hooks": {"project": hook("any", "pre_tool") + hook("star", "pre_tool", match="*")
                      + hook("upper", "pre_tool", match="BASH") + hook("glob", "pre_tool", match="ba?h")
                      + hook("regex", "pre_tool", match="re:^(bash|grep)$")
                      + hook("partial_regex", "pre_tool", match="re:bas")
                      + hook("other", "pre_tool", match="read_file")
                      + hook("list", "pre_tool", match="grep,bash")
                      + hook("agent", "post_agent", match="bash")},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "pre_tool/deny-without-reason",
            "hooks": {"project": hook("guard", "pre_tool")},
            "responses": {"guard": [answer(decision="deny", system_message="Shown instead")]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        # -- post_tool -----------------------------------------------------
        {
            "name": "post_tool/replace",
            "hooks": {"project": hook("redact", "post_tool") + hook("after", "post_tool")},
            "responses": {"redact": [answer(decision="deny", reason="output redacted")]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "post_tool/replace-with-context",
            "hooks": {"project": hook("redact", "post_tool")},
            "responses": {"redact": [answer(decision="deny", reason="output redacted",
                                            hook_specific_output={"additional_context": "see the audit log"})]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "post_tool/replace-without-reason",
            "hooks": {"project": hook("redact", "post_tool")},
            "responses": {"redact": [answer(decision="deny")]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "post_tool/append",
            "hooks": {"project": hook("annotate", "post_tool") + hook("again", "post_tool")},
            "responses": {
                "annotate": [answer(hook_specific_output={"additional_context": "Checked by annotate"})],
                "again": [answer(system_message="Second note",
                                 hook_specific_output={"additional_context": "and again"})],
            },
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "post_tool/strict-failure",
            "hooks": {"project": hook("strict", "post_tool", strict=True) + hook("after", "post_tool")},
            "responses": {"strict": [{"stderr": "scanner crashed", "exit": 1}]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "post_tool/failure-warns",
            "linger": 3.5,
            "hooks": {"project": hook("broken", "post_tool") + hook("after", "post_tool", timeout=0.3)},
            "responses": {"broken": [{"stderr": "scanner crashed", "exit": 1}], "after": [{"sleep": 2}]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        {
            "name": "post_tool/tool-failure",
            "hooks": {"project": hook("observe", "post_tool")},
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "missing.txt"}, "call_r1")]},
                {"text": "It is missing."},
            ],
            "steps": [{"start": {}}, {"turn": "Read a missing file"}, {"persisted": True}],
        },
        {
            "name": "post_tool/after-a-rewrite",
            "files": {"notes.txt": "one\ntwo\n"},
            "hooks": {"project": hook("rewriter", "pre_tool") + hook("observe", "post_tool")},
            "responses": {"rewriter": [answer(hook_specific_output={"tool_input": {"file_path": "notes.txt", "limit": 1}})]},
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "other.txt"}, "call_r1")]},
                {"text": "Read."},
            ],
            "steps": [{"start": {}}, {"turn": "Read a file"}, {"persisted": True}],
        },
        {
            "name": "post_tool/denied-call-runs-no-post-hook",
            "hooks": {"project": hook("guard", "pre_tool") + hook("observe", "post_tool")},
            "responses": {"guard": [answer(decision="deny", reason="no")]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        # -- post_agent ----------------------------------------------------
        {
            "name": "post_agent/retry-once",
            "hooks": {"project": hook("review", "post_agent") + hook("after", "post_agent")},
            "responses": {"review": [answer(decision="deny", reason="Add a summary line"), SILENT]},
            "backend": [{"text": "First answer."}, {"text": "Summary: done."}],
            "steps": [{"start": {}}, {"turn": "Answer"}, {"persisted": True}],
        },
        {
            "name": "post_agent/retries-exhausted-then-reset",
            "hooks": {"project": hook("review", "post_agent")},
            "responses": {"review": [
                answer(decision="deny", reason="Try again"),
                answer(decision="deny", reason="Try again"),
                answer(decision="deny", reason="Try again"),
                answer(decision="deny", reason="Try again"),
                answer(decision="deny", reason="Next turn"),
                SILENT,
            ]},
            "backend": [
                {"text": "Attempt one."}, {"text": "Attempt two."}, {"text": "Attempt three."},
                {"text": "Attempt four."}, {"text": "Second turn."}, {"text": "Second turn again."},
            ],
            "steps": [{"start": {}}, {"turn": "Answer"}, {"turn": "Answer again"}, {"persisted": True}],
        },
        {
            "name": "post_agent/deny-without-reason",
            "hooks": {"project": hook("review", "post_agent")},
            "responses": {"review": [answer(decision="deny"), SILENT]},
            "backend": [{"text": "First answer."}, {"text": "Second answer."}],
            "steps": [{"start": {}}, {"turn": "Answer"}],
        },
        {
            "name": "post_agent/failures",
            "hooks": {"project": hook("broken", "post_agent") + hook("strict", "post_agent", strict=True)
                      + hook("after", "post_agent")},
            "responses": {"broken": [{"stderr": "linter missing", "exit": 127}],
                          "strict": [{"stdout": "garbage"}]},
            "backend": [{"text": "Answer."}],
            "steps": [{"start": {}}, {"turn": "Answer"}],
        },
        {
            "name": "post_agent/after-tool-calls",
            "hooks": {"project": hook("review", "post_agent") + hook("observe", "post_tool")},
            "responses": {"review": [answer(system_message="Reviewed")]},
            "backend": BASH_ECHO,
            "steps": ONE_TURN,
        },
        # -- configuration -------------------------------------------------
        {
            "name": "config/project-and-user",
            "hooks": {
                "project": hook("guard", "pre_tool", recorder="guard_project") + hook("audit", "post_tool"),
                "user": hook("guard", "pre_tool", recorder="guard_user") + hook("user_only", "pre_tool"),
            },
            "backend": BASH_ECHO,
            "steps": [
                {"start": {}}, runtime(), diagnostics(), config_count(),
                config_count(sessionId="$S1"), {"turn": "Run echo"},
            ],
        },
        {
            "name": "config/invalid-entries",
            "hooks": {
                "project": (
                    "[[hooks]]\ntype = \"pre_tool\"\ncommand = \"true\"\n\n"
                    "[[hooks]]\nname = 5\ntype = \"pre_tool\"\ncommand = \"true\"\n\n"
                    "[[hooks]]\nname = \"bad_type\"\ntype = \"pre_turn\"\ncommand = \"true\"\n\n"
                    "[[hooks]]\nname = \"empty_command\"\ntype = \"pre_tool\"\ncommand = \"  \"\n\n"
                    "[[hooks]]\nname = \"empty_match\"\ntype = \"pre_tool\"\ncommand = \"true\"\nmatch = \"\"\n\n"
                    "[[hooks]]\nname = \"agent_match\"\ntype = \"post_agent\"\ncommand = \"true\"\nmatch = \"bash\"\n\n"
                    "[[hooks]]\nname = \"bad_timeout\"\ntype = \"pre_tool\"\ncommand = \"true\"\ntimeout = \"soon\"\n\n"
                    "[[hooks]]\nname = \"string_timeout\"\ntype = \"pre_tool\"\ncommand = \"true\"\ntimeout = \"1_5\"\n\n"
                    "[[hooks]]\nname = \"bad_strict\"\ntype = \"pre_tool\"\ncommand = \"true\"\nstrict = \"maybe\"\n\n"
                    "[[hooks]]\nname = \"word_strict\"\ntype = \"pre_tool\"\ncommand = \"true\"\nstrict = \"yes\"\n\n"
                    "[[hooks]]\nname = \"number_strict\"\ntype = \"pre_tool\"\ncommand = \"true\"\nstrict = 2\n\n"
                    "[[hooks]]\nname = \"extra\"\ntype = \"pre_tool\"\ncommand = \"true\"\nunknown = 1\n\n"
                    "[[hooks]]\nname = \"many\"\ntype = 3\ncommand = []\ntimeout = []\n\n"
                    "[[hooks]]\nname = \"\"\ntype = \"pre_tool\"\ncommand = \"true\"\n\n"
                    "[[hooks]]\nname = \"described\"\ntype = \"pre_tool\"\ncommand = \"true\"\ndescription = 4\n\n"
                    "[[hooks]]\nname = \"valid\"\ntype = \"pre_tool\"\ncommand = \"true\"\n\n"
                    "[[hooks]]\nname = \"valid\"\ntype = \"post_tool\"\ncommand = \"true\"\n\n"
                ),
                "user": "hooks = 5\n",
            },
            "steps": [{"start": {}}, runtime(), diagnostics(), config_count()],
        },
        {
            "name": "config/unreadable-files",
            "hooks": {"project": "[[hooks]\nname = \"x\"\n", "user": "hooks = [1, \"two\"]\n"},
            "steps": [{"start": {}}, runtime(), diagnostics(), config_count()],
        },
        {
            "name": "config/untrusted-project-then-granted",
            "trusted": False,
            # Project instructions are what makes the directory's trust a
            # decision to take.
            "files": {"AGENTS.md": "# notes\n"},
            "hooks": {
                "project": hook("project_guard", "pre_tool"),
                "user": hook("user_guard", "pre_tool"),
            },
            "backend": [*BASH_ECHO, {"toolCalls": [call("bash", {"command": "echo again"}, "call_b2")]},
                        {"text": "Again."}],
            "steps": [
                {"start": {}}, runtime(), config_count(), config_count(cwd="$WS"), {"turn": "Run echo"},
                send("workspace/trust/decision", decision="trust_cwd", cwd="$WS", sessionId="$S1"),
                runtime(), {"turn": "Run it again"},
            ],
        },
        {
            "name": "config/additional-root",
            "rootFiles": {"extra/.vibe/hooks.toml": hook("extra_guard", "pre_tool"),
                          "extra/readme.txt": "extra\n"},
            "hooks": {"project": hook("project_guard", "pre_tool")},
            "backend": BASH_ECHO,
            "steps": [
                {"start": {"addDirs": ["$ROOT/extra"]}}, runtime(), diagnostics(),
                {"turn": "Run echo"},
            ],
        },
        {
            "name": "config/transcript-disabled",
            "config": "\n[session_logging]\nenabled = false\n",
            "hooks": {"project": hook("observe", "pre_tool") + hook("review", "post_agent")},
            "backend": BASH_ECHO,
            "steps": [{"start": {}}, {"turn": "Run echo"}],
        },
        # -- reloading -----------------------------------------------------
        {
            "name": "reload/config-reload",
            "backend": [*BASH_ECHO, {"toolCalls": [call("bash", {"command": "echo again"}, "call_b2")]},
                        {"text": "Again."}],
            "steps": [
                {"start": {}},
                {"hooks": {"project": hook("late", "pre_tool")}},
                runtime(), diagnostics(), config_count(), config_count(sessionId="$S1"),
                {"turn": "Run echo"},
                send("config/reload", sessionId="$S1", reloadRuntime=False),
                runtime(),
                send("config/reload", sessionId="$S1"),
                runtime(), diagnostics(), config_count(sessionId="$S1"),
                {"turn": "Run it again"},
            ],
        },
        {
            "name": "reload/config-write",
            "steps": [
                {"start": {}},
                {"hooks": {"user": hook("late", "pre_tool")}},
                send("config/write", sessionId="$S1", ops=[{"op": "set", "path": "/theme", "value": "dracula"}]),
                runtime(),
                send("config/write", sessionId="$S1", ops=[{"op": "set", "path": "/theme", "value": 3}],
                     reloadRuntime=True),
                runtime(),
                send("config/write", sessionId="$S1", ops=[{"op": "set", "path": "/theme", "value": "textual-dark"}],
                     reloadRuntime=True),
                runtime(),
                {"hooks": {"user": None}},
                send("config/model/write", sessionId="$S1", modelAlias="mistral-medium-3.5"),
                runtime(),
            ],
        },
        {
            "name": "reload/hooks-removed-keep-running",
            "hooks": {"project": hook("early", "pre_tool")},
            "backend": [*BASH_ECHO, {"toolCalls": [call("bash", {"command": "echo again"}, "call_b2")]},
                        {"text": "Again."}],
            "steps": [
                {"start": {}},
                {"hooks": {"project": None}},
                runtime(), config_count(),
                {"turn": "Run echo"},
                send("config/reload", sessionId="$S1"),
                runtime(),
                {"turn": "Run it again"},
            ],
        },
        {
            "name": "reload/skill-toggle",
            "rootFiles": {"vibe-home/skills/demo/SKILL.md": "---\nname: demo\ndescription: A demo skill\n---\n\nSay hello.\n"},
            "steps": [
                {"start": {}},
                {"hooks": {"project": hook("late", "pre_tool")}},
                runtime(),
                send("skills/setEnabled", sessionId="$S1", name="demo", enabled=False),
                runtime(),
            ],
        },
        # -- cancellation --------------------------------------------------
        {
            "name": "cancel/interrupted-tool",
            # How an interrupted turn settles its calls is row 17's.
            "noticesOnly": True,
            "hooks": {"project": hook("observe", "post_tool") + hook("review", "post_agent")},
            "backend": [
                {"toolCalls": [call("bash", {"command": "sleep 5; echo late"}, "call_s1")]},
                {"text": "Never read."},
            ],
            "steps": [
                {"start": {}},
                {"turn": "Run something slow", "background": True, "settle": 1.5},
                {"interrupt": True},
                {"persisted": True},
            ],
        },
        {
            "name": "cancel/interrupted-hook",
            # How an interrupted turn settles its calls is row 17's.
            "noticesOnly": True,
            "linger": 3.5,
            "hooks": {"project": hook("slow", "pre_tool")},
            "responses": {"slow": [{"sleep": 5}]},
            "backend": BASH_ECHO,
            "steps": [
                {"start": {}},
                {"turn": "Run echo", "background": True, "settle": 1.5},
                {"interrupt": True},
                {"persisted": True},
            ],
        },
        # -- subagents -----------------------------------------------------
        {
            "name": "subagent/inherits-hooks",
            # How the parent shows a delegation is row 20's; the hooks are
            # observed through their notices and what each one read.
            "noticesOnly": True,
            "files": {"notes.txt": "needle\n"},
            "hooks": {"project": hook("observe", "pre_tool") + hook("review", "post_agent")
                      + hook("after", "post_tool")},
            "backend": [
                {"toolCalls": [call("task", {"task": "Find the needle", "agent": "explore"}, "call_t1")]},
                {"toolCalls": [call("grep", {"pattern": "needle", "path": "."}, "call_g1")]},
                {"text": "Found it in notes.txt."},
                {"text": "The subagent found it."},
            ],
            "steps": [{"start": {}}, {"turn": "Delegate the search"}],
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
            differing = [
                entry["name"]
                for entry in captured
                if by_name.get(entry["name"], {}).get("observed") != entry["observed"]
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
        print(f"hooks oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
