#!/usr/bin/env python3
"""Black-box capture of the conversation loop a session runs its turns through.

Every scenario starts an app server over stdio in a fresh home, behind a
scripted stand-in for the chat-completions API, and drives turns the stand-in
answers from a script: text, reasoning, tool calls, cached-token usage. A
client answers the approvals and questions the session raises, and the
scenario may interrupt a turn, steer it, compact the session, move it to a
worktree or restart the server and read the session back.

What is recorded is what row 34 of ``docs/parity.md`` is about, the loop
reference ``vibe/core/agent_loop/_loop.py`` runs and the records
``vibe/core/types.py`` declares: the transcript entries, turn and session
notifications each turn raised, every message the session persisted to its
``messages.jsonl`` with the keys in the order it wrote them, the statistics,
child links and title its ``meta.json`` holds, the conversation each model
request carried (and, apart, each utility request such as a title), the
callbacks the client was asked, and any file a step reads back. Nothing is
imported from the reference: its own ``vibe-app-server`` is the oracle, which
is what makes the same scenarios replayable against this port by
``crates/vibe-app-server/tests/agent_loop_parity_tests.rs``.

Normalization maps identifiers, paths, times, durations and rates to
placeholders and reduces every string a server authored to its length and
SHA-256, which is what ``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/agent_loop.py                  # capture the reference
    python3 scripts/parity/agent_loop.py --check          # recapture and compare
    python3 scripts/parity/agent_loop.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
import agents  # noqa: E402
import hooks  # noqa: E402
import rewind  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/agent-loop-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5

GIT = ["git", "-c", "user.name=Oracle", "-c", "user.email=oracle@example.invalid"]


# --------------------------------------------------------------------------
# The chat-completions stand-in
# --------------------------------------------------------------------------


#: What the stand-in answers a fast-model probe with. Since v2.26.0 a root
#: session opened with titles on first asks the provider whether it serves a
#: fast model, with a one-token completion that offers no tools (reference
#: ``vibe/core/llm/model_probe.py:189-211``).
PROBE_ANSWER = {"text": "Available."}


def is_model_probe(body: dict[str, Any]) -> bool:
    return body.get("max_tokens") == 1 and not body.get("tools")


class Backend:
    """Serves scripted completions, keeping every request it was sent.

    A request that offers tools is a model step of the conversation and takes
    the next entry of ``responses``; one that offers none is a utility call
    (a title, a summary) and takes the next entry of ``utility``, so a title
    generated in the background never consumes the answer the next step was
    scripted with. A fast-model probe is recorded as a utility call but takes
    :data:`PROBE_ANSWER`, so the probe never consumes the title a scenario
    scripted either. An entry may carry ``text`` (a string or its chunks),
    ``reasoning``, ``toolCalls``, ``promptTokens``, ``completionTokens``,
    ``cachedTokens``, a ``finish`` reason, a ``delay`` in seconds, or a
    ``status`` and ``body`` to fail with.
    """

    def __init__(self) -> None:
        self.responses: list[dict[str, Any]] = []
        self.utility: list[dict[str, Any]] = []
        self.bodies: list[dict[str, Any]] = []
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
                utility = not body.get("tools")
                with backend.lock:
                    backend.bodies.append({"utility": utility, "body": body})
                    queue = backend.utility if utility else backend.responses
                    if is_model_probe(body):
                        response = PROBE_ANSWER
                    else:
                        response = queue.pop(0) if queue else (
                            {"text": "Oracle title"} if utility else {"text": "Done."}
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
                        for item in chunks(response, model)
                    ) + b"data: [DONE]\n\n"
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                    self.send_header("content-length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                self.reply(200, completion(response, model))

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


def usage(response: dict[str, Any]) -> dict[str, Any]:
    prompt = response.get("promptTokens", 10)
    completion_tokens = response.get("completionTokens", 5)
    body: dict[str, Any] = {
        "prompt_tokens": prompt,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt + completion_tokens,
    }
    if "cachedTokens" in response:
        body["prompt_tokens_details"] = {"cached_tokens": response["cachedTokens"]}
    return body


def tool_calls(response: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        {
            "id": call.get("id", f"call_{index}"),
            "type": "function",
            "index": index,
            "function": {
                "name": call["name"],
                "arguments": call["raw"] if "raw" in call else json.dumps(call.get("arguments", {})),
            },
        }
        for index, call in enumerate(response.get("toolCalls", []))
    ]


def text_pieces(response: dict[str, Any]) -> list[str]:
    text = response.get("text")
    if isinstance(text, list):
        return text
    return [text] if text else []


def chunks(response: dict[str, Any], model: str) -> list[dict[str, Any]]:
    """The stream a scripted response is served as, one chunk per delta."""

    served: list[dict[str, Any]] = []

    def chunk(delta: dict[str, Any], finish: str | None = None, with_usage: bool = False) -> None:
        body: dict[str, Any] = {
            "id": "cmpl-oracle",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }
        if with_usage:
            body["usage"] = usage(response)
        served.append(body)

    chunk({"role": "assistant", "content": ""})
    if response.get("reasoning"):
        # Mistral streams reasoning as a thinking chunk.
        chunk({"content": [{"type": "thinking", "thinking": [
            {"type": "text", "text": response["reasoning"]}]}]})
    for piece in text_pieces(response):
        chunk({"content": piece})
    for call in tool_calls(response):
        chunk({"tool_calls": [call]})
    finish = "tool_calls" if response.get("toolCalls") else response.get("finish", "stop")
    chunk({"content": ""}, finish=finish, with_usage=True)
    return served


def completion(response: dict[str, Any], model: str) -> dict[str, Any]:
    message: dict[str, Any] = {"role": "assistant", "content": "".join(text_pieces(response))}
    if response.get("toolCalls"):
        message["tool_calls"] = tool_calls(response)
    return {
        "id": "cmpl-oracle",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": "tool_calls" if response.get("toolCalls") else response.get("finish", "stop"),
        }],
        "usage": usage(response),
    }


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(agents.Server):
    """A server whose callbacks the scenario answers from a script.

    Beyond what ``agents.Server`` answers, a scripted answer may first edit the
    plan file the session works on (``editPlan``), the way a user edits it
    during a plan review, and may hold its answer (``holdSeconds``).
    """

    def __init__(self, *args: Any, plans: Path, **kwargs: Any) -> None:
        self.plans = plans
        super().__init__(*args, **kwargs)

    def _answer(self, request: dict[str, Any]) -> None:
        if request.get("method") == "callback/call" and self.callbacks:
            script = self.callbacks[min(self.answered_callbacks, len(self.callbacks) - 1)]
            if "editPlan" in script:
                # The plan a review names, written or not yet, else every plan
                # already on disk.
                named = {Path(path) for text in strings(request.get("params"))
                         for path in PLAN_PATH.findall(text)}
                for plan in sorted(named or set(self.plans.glob("*.md"))):
                    plan.parent.mkdir(parents=True, exist_ok=True)
                    plan.write_text(script["editPlan"], encoding="utf-8")
            if script.get("ignore"):
                # Never answered: the turn waits until it is interrupted.
                params = request.get("params") or {}
                self.callback_log.append(params)
                callback_id = (params.get("callback") or {}).get("callbackId")
                self.send({"jsonrpc": "2.0", "id": request["id"],
                           "result": {"callbackId": callback_id, "accepted": True}})
                self.answered_callbacks += 1
                return
        super()._answer(request)


class Session(agents.Session):
    """What a scenario learned, plus the turn each `turn/started` opened."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.turns: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        if message.get("method") == "turn/started":
            turn = ((message.get("params") or {}).get("turn") or {}).get("id")
            if isinstance(turn, str) and turn not in self.turns:
                self.turns.append(turn)


def client_info(scenario: dict[str, Any]) -> dict[str, Any]:
    return {"name": "agent-loop-oracle", "version": "0", **scenario.get("client", {})}


def connect(scenario: dict[str, Any], command: list[str], env: dict[str, str],
            world: acp.World, quiet: float) -> Server:
    server = Server(command, env, world.workspace, {}, world,
                    callbacks=copy.deepcopy(scenario.get("callbacks", [])),
                    plans=world.vibe_home / "plans")
    server.send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                 "params": {"clientInfo": client_info(scenario),
                            "capabilities": {"callbackKinds": ["approval", "user_input"]}}})
    server.collect(1, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    return server


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = Backend()
    root = Path(tempfile.mkdtemp(prefix="vibe-loop-oracle-"))
    session = Session(root)
    world = session.world
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        backend.utility = session.substitute(copy.deepcopy(scenario.get("utility", [])))
        # The scenario's keys lead, so none of them lands inside the provider
        # table the base configuration ends on.
        (world.vibe_home / "config.toml").write_text(
            session.substitute(scenario.get("config", "")) + acp.base_config(backend)
            + session.substitute(scenario.get("tables", "")),
            encoding="utf-8",
        )
        if scenario.get("trusted", True):
            (world.vibe_home / "trusted_folders.toml").write_text(
                f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n",
                encoding="utf-8",
            )
        acp.write_tree(world.workspace, scenario.get("files", {}))
        (root / "hook.py").write_text(hooks.RECORDER, encoding="utf-8")
        hooks.write_responses(root, scenario.get("responses", {}))
        hooks.write_hooks(session, scenario.get("hooks", {}))
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
            **({} if scenario.get("titles") else {"VIBE_TEST_DISABLE_AUTO_TITLE": "1"}),
        }
        steps: list[dict[str, Any]] = []
        holder: dict[str, Server] = {}
        try:
            holder["server"] = connect(scenario, command, env, world, quiet)
            run_steps(scenario, holder, command, env, session, steps, quiet)
        except (rewind.OracleError, acp.OracleError) as error:
            steps.append({"failure": str(error)})
        finally:
            if "server" in holder:
                holder["server"].stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
            "callbacks": holder["server"].callback_log if "server" in holder else [],
            "requests": [
                {"utility": entry["utility"], **request_view(entry["body"], entry["utility"])}
                for entry in backend.bodies
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


def request_view(body: dict[str, Any], utility: bool) -> dict[str, Any]:
    """The conversation a request carried; for a model step, also its tools."""

    view: dict[str, Any] = {"conversation": conversation(body.get("messages", []), utility)}
    if not utility:
        view["tools"] = sorted(
            (tool.get("function") or {}).get("name", "")
            for tool in body.get("tools") or []
            if isinstance(tool, dict)
        )
    return view


def conversation(messages: list[Any], utility: bool) -> list[dict[str, Any]]:
    """Every message a request sent except the system prompt, which is row
    19's; a utility request keeps only the count of what it carried, since its
    prompt is the server's own prose."""

    turns = hooks.transcript(messages)
    if utility:
        return [{"role": turn["role"]} for turn in turns]
    return turns


def run_steps(
    scenario: dict[str, Any],
    holder: dict[str, Server],
    command: list[str],
    env: dict[str, str],
    session: Session,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    identifier = 100
    pending: list[dict[str, Any]] = []
    for step in scenario["steps"]:
        server = holder["server"]
        identifier += 1
        if "start" in step:
            options = step["start"]
            config: dict[str, Any] = {
                "cwd": session.substitute(options.get("cwd", "$WS")),
                "agent": options.get("agent", "auto-approve"),
            }
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start",
                         "params": {"agentConfig": config}})
            for message in server.collect(identifier, quiet):
                session.learn(message)
            continue
        if "restart" in step:
            server.stop()
            holder["server"] = connect(scenario, command, env, world, quiet)
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            params = rewind.render_turn("reference", target, step["turn"])
            params.update(step.get("params", {}))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": params})
            if step.get("background"):
                # The turn is left running until an `interrupt` or a `wait`.
                time.sleep(step.get("settle", 1.0))
                pending = server.collect(None, quiet)
                for message in pending:
                    session.learn(message)
                continue
            observed = server.wait_for(rewind.settles({}))
            observed += server.collect(None, step.get("linger", quiet))
            for message in observed:
                session.learn(message)
            steps.append({"turn": notifications(observed)})
            continue
        if "steer" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/steer", "params": {
                "sessionId": target,
                "expectedTurnId": session.turns[-1] if session.turns else "",
                "message": [{"type": "text", "text": step["steer"]}],
            }})
            pending += server.collect(identifier, quiet)
            for message in pending:
                session.learn(message)
            continue
        if "interrupt" in step:
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/interrupt", "params": {
                "sessionId": session.substitute("$S1"),
                "expectedTurnId": session.turns[-1] if session.turns else "",
            }})
            observed = pending + server.wait_for(rewind.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": notifications(observed)})
            pending = []
            continue
        if "wait" in step:
            # A background turn settles on its own.
            observed = pending + server.wait_for(rewind.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": notifications(observed)})
            pending = []
            continue
        if "sleep" in step:
            time.sleep(step["sleep"])
            observed = server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"idle": notifications(observed)})
            continue
        if "run" in step:
            subprocess.run(session.substitute(step["run"]), cwd=world.workspace, env=env,
                           capture_output=True, check=True)
            continue
        if "write" in step:
            for relative, text in step["write"].items():
                target = Path(session.substitute(relative))
                if not target.is_absolute():
                    target = world.workspace / target
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(session.substitute(text), encoding="utf-8")
            continue
        if "persisted" in step:
            steps.append({"persisted": persisted(world)})
            continue
        if "file" in step:
            path = Path(session.substitute(step["file"]))
            steps.append({"file": path.read_text(encoding="utf-8") if path.is_file() else None})
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, step.get("linger", quiet))
        for message in observed:
            session.learn(message)
        response = agents.response_to(observed, identifier)
        steps.append({
            "method": request["method"],
            "pick": step.get("pick"),
            "response": {k: v for k, v in (response or {}).items() if k != "jsonrpc"},
            "notifications": notifications(observed),
        })


def notifications(observed: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [m for m in observed if "method" in m and "id" not in m]


#: The `meta.json` keys a loop writes or moves.
META_KEYS = ("stats", "child_sessions", "title", "title_source", "total_messages",
             "parent_session_id")


def persisted(world: acp.World) -> list[dict[str, Any]]:
    """Every session the save directory holds, a child under its parent's
    directory included, in the order they were created: each message line with
    its keys in the order the server wrote them, and the metadata a loop keeps."""

    found = []
    root = world.vibe_home / "logs/session"
    for meta_path in root.glob("**/meta.json"):
        directory = meta_path.parent
        if directory.name == "active" or "active" in directory.relative_to(root).parts:
            continue
        try:
            metadata = json.loads(meta_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            metadata = {}
        lines = []
        log = directory / "messages.jsonl"
        if log.is_file():
            for line in log.read_text(encoding="utf-8").splitlines():
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    lines.append({"unparsed": line})
                    continue
                lines.append({"keys": list(message), "message": message})
        found.append({
            "start": metadata.get("start_time") or "",
            "depth": len(directory.relative_to(root).parts),
            "record": {
                "child": len(directory.relative_to(root).parts) > 1,
                "messages": lines,
                "meta": {key: metadata.get(key) for key in META_KEYS},
            },
        })
    found.sort(key=lambda item: (item["depth"], item["start"]))
    return [item["record"] for item in found]


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: Keys whose numeric values measure elapsed time or throughput.
RATE_KEYS = {"duration", "last_turn_duration", "tokens_per_second", "durationMs", "duration_ms"}

#: Notification fields that count or date events, which every other part of
#: the server moves.
DROPPED = {"emittedAt", "eventId"}

#: Keys whose string values are identities.
ID_KEYS = rewind.ID_KEYS | {"tool_call_id", "session_id", "parent_session_id", "message_id",
                            "reasoning_message_id", "workId", "callbackId", "childSessionId"}

#: The notification families a loop raises.
FAMILIES = ("history/", "turn/", "session/")


def strings(value: Any) -> list[str]:
    """Every string a JSON value holds."""

    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        return [text for item in value.values() for text in strings(item)]
    if isinstance(value, list):
        return [text for item in value for text in strings(item)]
    return []


#: A plan file's name: when it was created and three random words.
PLAN_FILE = re.compile(r"plans/\d+-[a-z]+(?:-[a-z]+)*\.md")
#: A plan file's absolute path, as a review's footer names it.
PLAN_PATH = re.compile(r"/\S*?plans/\d+-[a-z]+(?:-[a-z]+)*\.md")
#: A child's log directory beneath its parent's: its agent, when it started
#: and the head of its random identifier.
CHILD_DIRECTORY = re.compile(r"((?:^|/)agents/[A-Za-z0-9_.-]+?_)(?:\d{8}_\d{6}|<time>)_[0-9a-f]{8}")


class Normalizer(hooks.Normalizer):
    def value(self, value: Any, key: str | None = None) -> Any:
        if key in RATE_KEYS and isinstance(value, int | float) and not isinstance(value, bool):
            return "<rate>" if value else 0
        if isinstance(value, str) and PLAN_FILE.search(value):
            value = PLAN_FILE.sub("plans/<plan>.md", value)
        if isinstance(value, str) and CHILD_DIRECTORY.search(value):
            value = CHILD_DIRECTORY.sub(r"\1<time>_<short>", value)
        if key in ID_KEYS and isinstance(value, str) and value:
            if value in self.authored:
                return value
            return self.identity(value)
        return super().value(value, key)

    def collect_ids(self, value: Any, key: str | None = None) -> None:
        if isinstance(value, dict):
            for name, item in sorted(value.items()):
                self.collect_ids(item, name)
            return
        if isinstance(value, list):
            for item in value:
                self.collect_ids(item, key)
            return
        if key in ID_KEYS and isinstance(value, str) and value and value not in self.authored:
            self.identity(value)
            return
        super().collect_ids(value, key)


def reduce_notification(message: dict[str, Any]) -> dict[str, Any]:
    params = {k: v for k, v in (message.get("params") or {}).items() if k not in DROPPED}
    for operation in params.get("patch") or []:
        if isinstance(operation, dict) and str(operation.get("path", "")).rsplit("/", 1)[-1] in rewind.TIME_KEYS:
            operation["value"] = "<time>"
    return {"method": message.get("method"), "params": params}


def reduce_state(params: dict[str, Any]) -> dict[str, Any]:
    """A session state snapshot reduced to what the loop moves in it."""

    state = params.get("state") if isinstance(params.get("state"), dict) else params
    session = state.get("session") or {}
    return {
        "isQuiescent": state.get("isQuiescent"),
        "title": session.get("title"),
        "stats": session.get("stats"),
    }


def lands_in_background(message: dict[str, Any]) -> bool:
    """Whether ``message`` reports background title work finishing.

    A title is generated beside the turn, so where its result lands among the
    turn's own notifications depends on how fast the utility request answers.
    The title patch, its notice and the snapshot that turns the session
    quiescent again are kept apart, in their own order, after the turn's.
    """

    method = message.get("method")
    params = message.get("params") or {}
    if method == "session/updated":
        return [o.get("path") for o in params.get("patch") or []] == ["/title"]
    if method == "history/entryAdded":
        detail = (params.get("entry") or {}).get("detail")
        return isinstance(detail, dict) and detail.get("kind") == "session_title_updated"
    if method == "session/snapshot":
        return (params.get("state") or {}).get("isQuiescent") is True
    return False


def reduce_turn(scenario: dict[str, Any], messages: list[dict[str, Any]]) -> list[Any]:
    kept, landed = [], []
    for message in messages:
        method = str(message.get("method", ""))
        if not method.startswith(FAMILIES):
            continue
        if scenario.get("observe") and not any(method.startswith(m) for m in scenario["observe"]):
            continue
        reduced = reduce_notification(message)
        if method == "session/state":
            reduced["params"] = reduce_state(reduced["params"])
        if lands_in_background(reduced):
            if method == "session/snapshot":
                # The rest of a landing snapshot is the session at whatever
                # step the turn had reached when the title came back.
                state = reduced["params"]["state"]
                reduced["params"] = {"isQuiescent": True,
                                     "title": (state.get("session") or {}).get("title")}
            landed.append(reduced)
        else:
            kept.append(reduced)
    return kept + landed


def reduce_result(result: Any, pick: list[str] | None) -> Any:
    if not isinstance(result, dict) or not pick:
        return result
    return {key: result.get(key) for key in pick}


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    normalizer = Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    reduced: list[dict[str, Any]] = []
    for step in run["steps"]:
        if "failure" in step:
            reduced.append({"failure": step["failure"]})
        elif "turn" in step:
            reduced.append({"turn": reduce_turn(scenario, step["turn"])})
        elif "idle" in step:
            reduced.append({"idle": reduce_turn(scenario, step["idle"])})
        elif "persisted" in step:
            persisted = copy.deepcopy(step)
            for record in persisted["persisted"]:
                # A child opens on the task prompt its parent composed, row
                # 20's contract, as its requests do below.
                first = (record.get("messages") or [{}])[0].get("message") or {}
                if record.get("child") and first.get("role") == "user":
                    first["content"] = "<task prompt>"
            reduced.append(persisted)
        elif "file" in step:
            reduced.append(copy.deepcopy(step))
        else:
            response = copy.deepcopy(step["response"])
            if "result" in response:
                response["result"] = reduce_result(response["result"], step.get("pick"))
            reduced.append({
                "method": step["method"],
                "response": response,
                "notifications": reduce_turn(scenario, step["notifications"]),
            })
    turns = {step["turn"] for step in scenario["steps"] if "turn" in step}
    # A utility request (a background title) races the model steps it runs
    # beside, so the model requests keep their order and the utility ones
    # follow in theirs.
    requests = sorted(copy.deepcopy(run["requests"]), key=lambda request: request["utility"])
    for request in requests:
        conversation_ = request.get("conversation") or []
        # A subagent's conversation opens on the task prompt its parent
        # composed, which is row 20's contract.
        if (not request["utility"] and conversation_ and conversation_[0].get("role") == "user"
                and conversation_[0].get("content") not in turns
                and not str(conversation_[0].get("content", "")).startswith("<")):
            conversation_[0]["content"] = "<task prompt>"
    callbacks = [
        {
            "kind": ((c.get("callback") or {}).get("detail") or {}).get("kind"),
            "requiredPermissions": ((c.get("callback") or {}).get("detail") or {}).get(
                "requiredPermissions"),
            "choices": ((c.get("callback") or {}).get("detail") or {}).get("choices"),
        }
        for c in run["callbacks"]
    ]
    for part in (reduced, requests):
        normalizer.collect_ids(part)
    steps: list[Any] = []
    for step in reduced:
        if "failure" in step:
            steps.append({"failure": normalizer.text(step["failure"])})
        elif "file" in step:
            text = step["file"]
            steps.append({"file": normalizer.text(text) if isinstance(text, str) else text})
        else:
            steps.append(normalizer.value(step))
    return {
        "steps": steps,
        "requests": normalizer.value(requests),
        "callbacks": normalizer.value(callbacks),
    }


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def send(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    step: dict[str, Any] = {"send": {"method": method, "params": params}}
    if pick:
        step["pick"] = pick
    return step


START = {"start": {}}
PERSISTED = {"persisted": True}
TITLES = {"titles": True, "client": {"entrypoint": "cli"},
          "config": "", "tables": "\n[session_logging]\ngenerate_titles = true\n"}

#: Files outside the workdir, a directory holding one and a nested one, which
#: the path scope scenarios reach.
OUTSIDE_FILES = {"write": {"$ROOT/outside/data/a.txt": "alpha\n",
                           "$ROOT/outside/data/nested/b.txt": "beta\n"}}

ECHO = [{"toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")]}, {"text": "Finished."}]
#: A step running a command outside the shell allowlist, which the `ask`
#: agent sends to the operator.
TOUCH = [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_t1")]},
         {"text": "Finished."}]


def echo_steps(count: int) -> list[dict[str, Any]]:
    """A turn of `count` tool steps and a closing answer."""

    return [
        {"toolCalls": [call("bash", {"command": f"echo step{index}"}, f"call_s{index}")]}
        for index in range(1, count + 1)
    ] + [{"text": "All steps ran."}]


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- what a turn persists ------------------------------------------
        {
            "name": "persist/text",
            "backend": [{"text": "Hello there."}],
            "steps": [START, {"turn": "Say hello"}, PERSISTED],
        },
        {
            "name": "persist/tool",
            "backend": ECHO,
            "steps": [START, {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "persist/reasoning",
            "backend": [{"reasoning": "Thinking it over.", "text": "Answered."}],
            "steps": [START, {"turn": "Think first"}, PERSISTED],
        },
        {
            "name": "persist/parallel",
            "files": {"a.txt": "alpha\n", "b.txt": "beta\n"},
            "backend": [
                # The first call outlasts the second, so the order they
                # settle in is the same on every run.
                {"toolCalls": [call("bash", {"command": "sleep 0.5; cat a.txt"}, "call_r1"),
                               call("read_file", {"file_path": "b.txt"}, "call_r2")]},
                {"text": "Both read."},
            ],
            "steps": [START, {"turn": "Read both"}, PERSISTED],
        },
        {
            "name": "persist/tool-failure",
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "missing.txt"}, "call_r1")]},
                {"text": "It is missing."},
            ],
            "steps": [START, {"turn": "Read a missing file"}, PERSISTED],
        },
        {
            "name": "persist/unknown-tool",
            "backend": [
                {"toolCalls": [call("no_such_tool", {}, "call_u1")]},
                {"text": "That failed."},
            ],
            "steps": [START, {"turn": "Call nothing"}, PERSISTED],
        },
        {
            "name": "persist/invalid-arguments",
            "files": {"a.txt": "alpha\n"},
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "a.txt"}, "call_r1"),
                               call("read_file", {"bogus": 1}, "call_r2")]},
                {"text": "One failed."},
            ],
            "steps": [START, {"turn": "Read with a bad call"}, PERSISTED],
        },
        {
            "name": "persist/denied",
            "backend": TOUCH,
            "callbacks": [{"approve": False}],
            "steps": [{"start": {"agent": "ask"}}, {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "persist/hook-denied",
            "hooks": {"project": hooks.hook("guard", "pre_tool")},
            "responses": {"guard": [hooks.answer(decision="deny", reason="Not today")]},
            "backend": ECHO,
            "steps": [START, {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "persist/two-turns",
            "backend": [{"text": "First."}, *ECHO],
            "steps": [START, {"turn": "One"}, {"turn": "Two"}, PERSISTED],
        },
        {
            "name": "stats/cached-tokens",
            "backend": [
                {"toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")],
                 "promptTokens": 1200, "completionTokens": 40, "cachedTokens": 800},
                {"text": "Finished.", "promptTokens": 1300, "completionTokens": 60,
                 "cachedTokens": 1000},
            ],
            "steps": [START, {"turn": "Run echo"}, PERSISTED,
                      send("stats/read", sessionId="$S1")],
        },
        {
            "name": "persist/reload",
            "files": {"a.txt": "alpha\n"},
            "backend": [
                {"text": "Reading.", "toolCalls": [call("bash", {"command": "sleep 0.5; cat a.txt"}, "call_r1"),
                                                   call("read_file", {"file_path": "gone.txt"}, "call_r2")]},
                {"text": "Done reading."},
            ],
            "steps": [START, {"turn": "Read files"}, {"restart": True},
                      send("session/resume", sessionId="$S1"),
                      send("session/history/get", sessionId="$S1")],
        },
        {
            # A resource the operator attached joins the prompt, the words
            # typed and the content shown are kept apart, and a reload shows
            # both again.
            "name": "persist/resource-and-display",
            "backend": [{"text": "Noted."}],
            "steps": [START, {"turn": "Summarize the note", "params": {
                "message": [
                    {"type": "text", "text": "Summarize the note"},
                    {"type": "resource", "resource": {
                        "kind": "text", "uri": "file:///notes/today.txt", "text": "Ship on Friday."}},
                ],
                "userDisplayContent": {"version": "1", "host": "oracle", "content": [
                    {"type": "text", "text": "Summarize the note"}]},
            }}, PERSISTED, {"restart": True},
                      send("session/resume", sessionId="$S1")],
        },
        # -- interruption --------------------------------------------------
        {
            "name": "interrupt/during-tool",
            "backend": [
                {"toolCalls": [call("bash", {"command": "sleep 5; echo late"}, "call_s1")]},
                {"text": "Continuing."},
            ],
            "steps": [START, {"turn": "Run something slow", "background": True, "settle": 1.5},
                      {"interrupt": True}, PERSISTED, {"turn": "Go on"}, PERSISTED],
        },
        {
            "name": "interrupt/before-run",
            "hooks": {"project": hooks.hook("slow", "pre_tool")},
            "responses": {"slow": [{"sleep": 4}, {}]},
            "backend": [*ECHO[:1], {"text": "Continuing."}],
            "steps": [START, {"turn": "Run echo", "background": True, "settle": 1.5},
                      {"interrupt": True}, {"sleep": 3.0}, PERSISTED, {"turn": "Go on"}, PERSISTED],
        },
        {
            "name": "interrupt/awaiting-approval",
            # A command outside the shell allowlist, so the call waits on the
            # operator.
            "backend": [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_t1")]},
                        {"text": "Continuing."}],
            "callbacks": [{"ignore": True}],
            "steps": [{"start": {"agent": "ask"}},
                      {"turn": "Run echo", "background": True, "settle": 1.5},
                      {"interrupt": True}, PERSISTED],
        },
        # -- approvals -----------------------------------------------------
        {
            "name": "approval/permanent",
            "backend": [
                {"toolCalls": [call("web_search", {"query": "vibe"}, "call_w1")]},
                {"text": "Searched."},
            ],
            "callbacks": [{"output": {"type": "approval", "decision": {"type": "approve_permanently"}}}],
            "steps": [{"start": {"agent": "ask"}}, {"turn": "Search"},
                      {"file": "$ROOT/vibe-home/config.toml"}],
        },
        {
            "name": "approval/session",
            "backend": [*TOUCH,
                        {"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_t2")]},
                        {"text": "Finished."}],
            "callbacks": [{"output": {"type": "approval", "decision": {"type": "approve_for_session"}}}],
            "steps": [{"start": {"agent": "ask"}}, {"turn": "Run echo"}, {"turn": "Again"},
                      PERSISTED],
        },
        # -- path scopes ---------------------------------------------------
        # An approval of a path outside the workdir offers the scope it may be
        # granted under (reference `vibe/app_server/_approval_permissions.py`):
        # a directory the recursive one, a file the exact one, and a scope it
        # did not offer fails the turn once the answer is in.
        {
            "name": "approval/path-scope-recursive",
            "backend": [
                {"toolCalls": [call("grep", {"pattern": "alpha", "path": "$ROOT/outside/data"},
                                    "call_g1")]},
                # A grant is the tool's own, so the nested search is covered
                # and asks nothing.
                {"toolCalls": [call("grep", {"pattern": "beta",
                                             "path": "$ROOT/outside/data/nested/b.txt"},
                                    "call_g2")]},
                {"text": "Searched both."},
            ],
            "callbacks": [{"output": {"type": "approval", "decision": {
                "type": "approve_for_session", "pathScope": "directory_recursive"}}}],
            "steps": [OUTSIDE_FILES, {"start": {"agent": "accept-edits"}}, {"turn": "Look outside"},
                      PERSISTED],
        },
        {
            "name": "approval/path-scope-permanent",
            "backend": [
                {"toolCalls": [call("grep", {"pattern": "alpha", "path": "$ROOT/outside/data"},
                                    "call_g1")]},
                {"text": "Searched."},
            ],
            "callbacks": [{"output": {"type": "approval", "decision": {
                "type": "approve_permanently", "pathScope": "directory_recursive"}}}],
            "steps": [OUTSIDE_FILES, {"start": {"agent": "accept-edits"}}, {"turn": "Look outside"},
                      {"file": "$ROOT/vibe-home/config.toml"}],
        },
        {
            "name": "approval/path-scope-exact",
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "$ROOT/outside/data/a.txt"},
                                    "call_r1")]},
                {"toolCalls": [call("read_file", {"file_path": "$ROOT/outside/data/a.txt"},
                                    "call_r2")]},
                {"toolCalls": [call("read_file", {"file_path": "$ROOT/outside/data/nested/b.txt"},
                                    "call_r3")]},
                {"text": "Read them."},
            ],
            "callbacks": [{"output": {"type": "approval", "decision": {
                "type": "approve_for_session", "pathScope": "exact"}}}],
            "steps": [OUTSIDE_FILES, {"start": {"agent": "accept-edits"}}, {"turn": "Look outside"},
                      PERSISTED],
        },
        {
            "name": "approval/path-scope-not-offered",
            "backend": [
                {"toolCalls": [call("read_file", {"file_path": "$ROOT/outside/data/a.txt"},
                                    "call_r1")]},
                {"text": "Read it."},
            ],
            "callbacks": [{"output": {"type": "approval", "decision": {
                "type": "approve_for_session", "pathScope": "directory_recursive"}}}],
            "steps": [OUTSIDE_FILES, {"start": {"agent": "accept-edits"}}, {"turn": "Look outside"},
                      PERSISTED],
        },
        # -- steering ------------------------------------------------------
        {
            "name": "steer/mention",
            "files": {"notes.txt": "steered notes\n"},
            "backend": [
                {"toolCalls": [call("bash", {"command": "sleep 2; echo slow"}, "call_s1")]},
                {"text": "Read the notes too."},
            ],
            "steps": [START, {"turn": "Run something slow", "background": True, "settle": 0.8},
                      {"steer": "Also look at @notes.txt"}, {"wait": True}, PERSISTED],
        },
        {
            "name": "inject/context",
            "backend": [{"text": "Noted."}],
            "steps": [START,
                      send("context/inject", sessionId="$S1",
                           input=[{"type": "text", "text": "Background fact"}]),
                      {"turn": "Use it"}, PERSISTED],
        },
        # -- plan review ---------------------------------------------------
        {
            "name": "plan/edited-during-review",
            "backend": [
                {"text": "The plan is ready.", "toolCalls": [call("exit_plan_mode", {}, "call_x1")]},
                {"text": "Staying in plan mode."},
            ],
            "callbacks": [{"choose": "No", "editPlan": "# Edited plan\n\n1. Do it by hand.\n"}],
            "steps": [{"start": {"agent": "plan"}}, {"turn": "Plan it"}, PERSISTED],
        },
        {
            "name": "plan/unchanged-during-review",
            "backend": [
                {"text": "The plan is ready.", "toolCalls": [call("exit_plan_mode", {}, "call_x1")]},
                {"text": "Staying in plan mode."},
            ],
            "callbacks": [{"choose": "No"}],
            "steps": [{"start": {"agent": "plan"}}, {"turn": "Plan it"}, PERSISTED],
        },
        # -- background titles ---------------------------------------------
        {
            **TITLES,
            "name": "titles/first-answer",
            "backend": [{"text": "Answer one."}],
            "utility": [{"text": "Parser plan"}],
            "steps": [START, {"turn": "Plan the parser", "linger": 1.5}, PERSISTED],
        },
        {
            **TITLES,
            "name": "titles/tool-heavy-turn",
            # The step after the first title is due answers slowly, so the
            # title lands within it on either server and the periodic refresh
            # counts every step.
            "backend": [{**step, "delay": 0.6} if index == 3 else step
                        for index, step in enumerate(echo_steps(8))],
            "utility": [{"text": "Echo marathon"}, {"text": "Echo marathon two"}],
            "steps": [START, {"turn": "Echo many times", "linger": 1.5}, PERSISTED],
        },
        {
            **TITLES,
            "name": "titles/after-compaction",
            # The summary rides on the conversation's tool surface, so it takes
            # a model step's answer.
            "backend": [{"text": "Answer one."}, {"text": "<summary>A summary of the session.</summary>"},
                        {"text": "Answer two."}],
            "utility": [{"text": "First title"}, {"text": "Second title"}],
            "steps": [START, {"turn": "First question", "linger": 1.5},
                      {**send("session/compact", sessionId="$S1"), "linger": 1.0},
                      {"turn": "Second question", "linger": 1.5}, PERSISTED],
        },
        {
            **TITLES,
            "name": "titles/manual-title-wins",
            "backend": [{"text": "Answer one."}],
            "steps": [START, send("session/rename", sessionId="$S1", title="Mine"),
                      {"turn": "First question", "linger": 1.5}, PERSISTED],
        },
        # -- relocation ----------------------------------------------------
        {
            "name": "relocate/live-session",
            "files": {"notes.txt": "tracked\n", ".vibe/config.toml": ""},
            "backend": [{"text": "Before."}, {"text": "After."}],
            "steps": [
                {"run": [*GIT, "init", "-q", "-b", "main"]},
                {"run": [*GIT, "add", "."]},
                {"run": [*GIT, "commit", "-q", "-m", "initial"]},
                {"run": [*GIT, "worktree", "add", "-q", "$ROOT/checkout", "-b", "side"]},
                {"write": {"$ROOT/checkout/.vibe/config.toml": 'disabled_tools = ["grep"]\n'}},
                START,
                {"turn": "Before moving"},
                send("session/relocate", sessionId="$S1", cwd="$ROOT/checkout"),
                {"turn": "After moving"},
                PERSISTED,
            ],
        },
        {
            "name": "relocate/during-turn",
            "files": {"notes.txt": "tracked\n"},
            "backend": [
                {"toolCalls": [call("bash", {"command": "sleep 2; echo slow"}, "call_s1")]},
                {"text": "Done."},
            ],
            "steps": [
                {"run": [*GIT, "init", "-q", "-b", "main"]},
                {"run": [*GIT, "add", "."]},
                {"run": [*GIT, "commit", "-q", "-m", "initial"]},
                {"run": [*GIT, "worktree", "add", "-q", "$ROOT/checkout", "-b", "side"]},
                START,
                {"turn": "Run something slow", "background": True, "settle": 0.8},
                send("session/relocate", sessionId="$S1", cwd="$ROOT/checkout"),
                {"wait": True},
            ],
        },
        # -- delegation ----------------------------------------------------
        {
            "name": "children/delegation",
            "files": {"notes.txt": "needle\n"},
            "backend": [
                {"toolCalls": [call("task", {"task": "Find the needle", "agent": "explore"}, "call_t1")]},
                {"text": "Looking.", "toolCalls": [call("grep", {"pattern": "needle", "path": "."}, "call_g1")]},
                {"text": "Found it in notes.txt."},
                {"text": "The subagent found it."},
            ],
            "steps": [START, {"turn": "Delegate the search"}, PERSISTED],
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
        print(f"agent loop oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
