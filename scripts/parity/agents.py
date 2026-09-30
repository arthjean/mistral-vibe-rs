#!/usr/bin/env python3
"""Black-box capture of agent profiles, their selection and delegation.

Every scenario starts an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in the hooks oracle already runs, lays out the
agent files it declares (the workspace's ``.vibe/agents``, the user's
``$VIBE_HOME/agents`` and any ``agent_paths`` directory), and drives the
methods and turns row 20 of ``docs/parity.md`` is about:

- ``registry``: which profiles a server discovers, what it publishes for each
  on ``agents/list`` and ``runtime/read``, and what a legacy profile file reads
  as once the server has migrated it;
- ``selection``: which profiles a session may start under or switch to, what
  ``enabled_agents``, ``disabled_agents``, ``installed_agents`` and the smart
  approve keys decide, and which refusals the server answers with;
- ``profile``: what a profile changes in the requests a session sends, the
  model, its sampling settings and the tools it is offered;
- ``delegation``: a ``task`` call as the parent's client sees it (the progress
  it streams, the result, the child's link), as the child runs it (its prompt,
  its tools, its approvals) and as it is saved;
- ``switch``: the profile ``exit_plan_mode`` moves a session to, and when the
  model starts running under it.

Nothing is imported from the reference: its own ``vibe-app-server`` is the
oracle, which is what makes the same scenarios replayable against this port by
``crates/vibe-app-server/tests/agents_parity_tests.rs``.

Normalization maps identifiers, paths and times to placeholders and reduces
every string a server authored to its length and SHA-256, which is what
``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/agents.py                  # capture the reference
    python3 scripts/parity/agents.py --check          # recapture and compare
    python3 scripts/parity/agents.py --server target/debug/vibe-app-server-stdio-fixture \\
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
import tomllib
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
import hooks  # noqa: E402
import rewind  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/agents-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(rewind.Server):
    """A server whose callbacks the scenario answers from a script.

    A ``callback/call`` is acknowledged at once and answered with the next
    scripted output, the last one repeating, so an approval or a question the
    session raises settles without a step of its own.
    """

    def __init__(self, *args: Any, callbacks: list[dict[str, Any]], **kwargs: Any) -> None:
        self.callbacks = callbacks
        self.answered_callbacks = 0
        self.callback_log: list[dict[str, Any]] = []
        super().__init__(*args, **kwargs)

    def _answer(self, request: dict[str, Any]) -> None:
        if request.get("method") != "callback/call":
            super()._answer(request)
            return
        params = request.get("params") or {}
        self.callback_log.append(params)
        callback_id = (params.get("callback") or {}).get("callbackId")
        self.send({"jsonrpc": "2.0", "id": request["id"],
                   "result": {"callbackId": callback_id, "accepted": True}})
        if not self.callbacks:
            return
        script = self.callbacks[min(self.answered_callbacks, len(self.callbacks) - 1)]
        self.answered_callbacks += 1
        self.send({"jsonrpc": "2.0", "id": f"callback-{self.answered_callbacks}",
                   "method": "callback/result",
                   "params": {
                       "sessionId": (params.get("callback") or {}).get("sessionId"),
                       "result": {"callbackId": callback_id, **callback_output(script, params)},
                   }})


def callback_output(script: dict[str, Any], params: dict[str, Any]) -> dict[str, Any]:
    """The answer a scripted client gives one callback.

    ``approve`` and ``deny`` settle an approval; ``choose`` answers every
    question of a user input callback with the option carrying that label.
    """

    if "approve" in script:
        decision = "approve" if script["approve"] else "deny"
        return {"output": {"type": "approval", "decision": {"type": decision}}}
    if "choose" in script:
        detail = (params.get("callback") or {}).get("detail") or {}
        questions = (detail.get("request") or {}).get("questions") or []
        return {"output": {"type": "user_input", "result": {
            "answers": [
                {"question": question.get("question", ""), "answer": script["choose"],
                 "isOther": False}
                for question in questions
            ],
            "cancelled": False,
        }}}
    return script


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


class Session(rewind.Session):
    """What a scenario learned, plus the child sessions a parent linked."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.children: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        for operation in (message.get("params") or {}).get("patch") or []:
            if (
                isinstance(operation, dict)
                and operation.get("path") == "/detail/childSessionId"
                and isinstance(operation.get("value"), str)
                and operation["value"] not in self.children
            ):
                self.children.append(operation["value"])

    def substitute(self, value: Any) -> Any:
        value = super().substitute(value)
        if isinstance(value, str):
            for index in range(len(self.children), 0, -1):
                value = value.replace(f"$K{index}", self.children[index - 1])
        elif isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        elif isinstance(value, list):
            return [self.substitute(item) for item in value]
        return value


def write_agents(world: acp.World, root: Path, agents: dict[str, dict[str, str]]) -> None:
    targets = {
        "project": world.workspace / ".vibe/agents",
        "user": world.vibe_home / "agents",
        "extra": root / "extra-agents",
    }
    for where, files in agents.items():
        directory = targets[where]
        directory.mkdir(parents=True, exist_ok=True)
        for name, text in files.items():
            (directory / name).write_text(text, encoding="utf-8")


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = hooks.Backend()
    root = Path(tempfile.mkdtemp(prefix="vibe-agents-oracle-"))
    session = Session(root)
    world = session.world
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
        # The scenario's keys lead, so none of them lands inside the provider
        # table the base configuration ends on.
        (world.vibe_home / "config.toml").write_text(
            session.substitute(scenario.get("config", "")) + acp.base_config(backend),
            encoding="utf-8",
        )
        if scenario.get("trusted", True):
            (world.vibe_home / "trusted_folders.toml").write_text(
                f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n",
                encoding="utf-8",
            )
        acp.write_tree(world.workspace, scenario.get("files", {}))
        write_agents(world, root, scenario.get("agents", {}))
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
        server = Server(command, env, world.workspace, {}, world,
                        callbacks=copy.deepcopy(scenario.get("callbacks", [])))
        steps: list[dict[str, Any]] = []
        try:
            run_steps(scenario, server, session, steps, quiet)
        except (rewind.OracleError, acp.OracleError) as error:
            steps.append({"failure": str(error)})
        finally:
            server.stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
            "children": session.children,
            "callbacks": server.callback_log,
            "requests": [request_view(body) for body in backend.bodies],
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


def request_view(body: dict[str, Any]) -> dict[str, Any]:
    """What a model request carries that a profile decides."""

    tools = sorted(
        (tool.get("function") or {}).get("name", "")
        for tool in body.get("tools") or []
        if isinstance(tool, dict)
    )
    view: dict[str, Any] = {
        "model": body.get("model"),
        "tools": tools,
        "conversation": hooks.transcript(body.get("messages", [])),
    }
    for key in ("temperature", "reasoning_effort", "max_tokens"):
        if key in body:
            view[key] = body[key]
    return view


def run_steps(
    scenario: dict[str, Any],
    server: Server,
    session: Session,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    identifier = 1
    server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                 "params": {"clientInfo": {"name": "agents-oracle", "version": "0"},
                            "capabilities": {"callbackKinds": ["approval", "user_input"]}}})
    server.collect(identifier, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    for step in scenario["steps"]:
        identifier += 1
        if "start" in step:
            options = step["start"]
            config: dict[str, Any] = {"cwd": session.substitute(options.get("cwd", "$WS"))}
            if "agent" in options:
                config["agent"] = options["agent"]
            request = {"method": "session/start", "params": {"agentConfig": config}}
            server.send({"jsonrpc": "2.0", "id": identifier, **request})
            observed = server.collect(identifier, quiet)
            for message in observed:
                session.learn(message)
            response = response_to(observed, identifier)
            steps.append({"method": "session/start", "response": start_view(response)})
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
        if "tree" in step:
            steps.append({"tree": saved_tree(world)})
            continue
        if "file" in step:
            path = Path(session.substitute(step["file"]))
            steps.append({"file": read_toml(path)})
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            session.learn(message)
        steps.append({
            "method": request["method"],
            "pick": step.get("pick"),
            "response": {k: v for k, v in (response_to(observed, identifier) or {}).items()
                         if k != "jsonrpc"},
        })


def response_to(observed: list[dict[str, Any]], identifier: int) -> dict[str, Any] | None:
    return next(
        (m for m in observed if "method" not in m and m.get("id") == identifier), None
    )


def start_view(response: dict[str, Any] | None) -> dict[str, Any]:
    """Whether a session started, and under which agent, or how it was refused."""

    if response is None:
        return {"missing": True}
    if "error" in response:
        return {"error": response["error"]}
    state = (response.get("result") or {}).get("state") or {}
    agent = (state.get("session") or {}).get("agent")
    return {"agent": agent}


def read_toml(path: Path) -> Any:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    except tomllib.TOMLDecodeError:
        return {"unparsed": True}


def saved_tree(world: acp.World) -> list[Any]:
    """The saved sessions: every directory's files, and each `meta.json`'s
    child links."""

    base = world.vibe_home / "logs/session"
    tree: list[Any] = []
    if not base.is_dir():
        return tree
    for path in sorted(base.rglob("*")):
        relative = path.relative_to(base)
        if relative.parts and relative.parts[0] in {"active", ".session_index.json"}:
            continue
        if path.is_dir():
            tree.append({"dir": str(relative)})
        elif path.name == "meta.json":
            meta = json.loads(path.read_text(encoding="utf-8"))
            tree.append({"meta": str(relative), "children": meta.get("child_sessions"),
                         "parent": meta.get("parent_session_id"),
                         "agent": (meta.get("agent_profile") or {}).get("name")
                         if isinstance(meta.get("agent_profile"), dict) else meta.get("agent_profile")})
        else:
            tree.append({"file": str(relative)})
    return tree


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: The builtin profiles, in the order the reference declares them. Custom
#: profiles follow in the order the filesystem lists them upstream, which no
#: fixed order reproduces (the accepted divergence "Custom subagents are
#: listed by name"), so both sides are compared with the custom ones sorted.
BUILTINS = ["ask", "plan", "accept-edits", "smart-approve", "auto-approve", "explore", "lean"]

#: A child's log directory beneath its parent's, named after its agent.
CHILD_DIRECTORY = re.compile(r"((?:^|/)agents/[A-Za-z0-9_.-]+?_)(?:\d{8}_\d{6}|<time>)_[0-9a-f]{8}")

#: A plan file: a Unix timestamp and a slug of random words, drawn afresh on
#: every run by both servers (reference ``PlanSession.plan_file_path``).
PLAN_FILE = re.compile(r"((?:^|/)plans/)\d+-[a-z]+-[a-z]+-[a-z]+\.md")

#: The names a refusal may mention: configuration keys and agent names.
CONFIG_NAMES = ("installed_agents", "enabled_agents", "disabled_agents", "default_agent")

#: How a subagent's run reaches its parent's model: one field per line.
TASK_RESULT = re.compile(
    r"\Aresponse: (?P<response>.*)\nturns_used: (?P<turns>\d+)\ncompleted: (?P<completed>True|False)\Z",
    re.DOTALL,
)
#: The marker a failed child's error is appended to its answer under.
SUBAGENT_ERROR = re.compile(r"\n\[Subagent error: .*\]\Z", re.DOTALL)

#: Notifications that count or date events, which every other part moves.
DROPPED = {"emittedAt", "eventId"}


def sort_agents(agents: Any) -> Any:
    if not isinstance(agents, list):
        return agents
    builtin = [a for a in agents if isinstance(a, dict) and a.get("name") in BUILTINS]
    custom = [a for a in agents if not (isinstance(a, dict) and a.get("name") in BUILTINS)]
    return builtin + sorted(custom, key=lambda a: str(a.get("name") if isinstance(a, dict) else a))


def agent_view(runtime: Any, extra: list[str] | None = None) -> Any:
    """The part of a runtime snapshot a profile decides."""

    if not isinstance(runtime, dict):
        return runtime
    view = {
        "activeAgent": runtime.get("activeAgent"),
        "agents": sort_agents(runtime.get("agents")),
        "bypassToolPermissions": runtime.get("bypassToolPermissions"),
    }
    config = runtime.get("config") or {}
    for key in extra or []:
        view[key] = config.get(key)
    return view


def refusal(error: dict[str, Any], mentioned: list[str]) -> dict[str, Any]:
    """A refusal reduced to its code and the names its message mentions.

    The sentence is each server's own (``NOTICE``); which configuration key and
    which agent it points the operator at is the contract.
    """

    message = str(error.get("message", ""))
    names = sorted(
        name for name in {*CONFIG_NAMES, *mentioned}
        if re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", message)
    )
    return {"code": error.get("code"), "names": names}


def task_result(text: Any) -> Any:
    """The text a subagent run hands its parent's model, as its fields."""

    if not isinstance(text, str):
        return text
    match = TASK_RESULT.match(text)
    if match is None:
        return text
    return {
        **answer(match.group("response")),
        "turnsUsed": int(match.group("turns")),
        "completed": match.group("completed") == "True",
    }


def answer(response: str) -> dict[str, Any]:
    error = SUBAGENT_ERROR.search(response)
    if error is None:
        return {"response": response}
    return {"response": response[: error.start()], "subagentError": True}


def child_prompt(text: Any, tasks: set[str]) -> Any:
    """The first message of a subagent's conversation, as what it carries: the
    task, and whether the scratchpad directory was named before it."""

    if not isinstance(text, str):
        return text
    for task in tasks:
        if text.endswith(task):
            head = text[: len(text) - len(task)]
            return {"task": task, "scratchpad": bool(re.search(r"scratchpad", head))}
    return text


class Reducer:
    """What one scenario's observations reduce to before normalization."""

    def __init__(self, scenario: dict[str, Any]) -> None:
        self.scenario = scenario
        self.mentioned = sorted(
            {*BUILTINS, *(name.removesuffix(".toml")
                          for files in scenario.get("agents", {}).values() for name in files),
             "nope"}
        )
        self.tasks = {
            call_["arguments"]["task"]
            for response in scenario.get("backend", [])
            for call_ in response.get("toolCalls", [])
            if call_.get("name") == "task"
        }
        self.kinds: dict[str, str] = {}

    def step(self, step: dict[str, Any]) -> dict[str, Any]:
        if "failure" in step or "tree" in step or "file" in step:
            return step
        if "turn" in step:
            return {"turn": [n for n in (self.notification(m) for m in step["turn"]) if n]}
        method = step["method"]
        response = step["response"]
        if method == "session/start":
            if "error" in response:
                return {"method": method, "response": {"error": refusal(response["error"], self.mentioned)}}
            return {"method": method, "response": response}
        if "error" in response:
            return {"method": method, "response": {"error": refusal(response["error"], self.mentioned)}}
        result = response.get("result")
        if method == "agents/list" or (isinstance(result, dict) and "agents" in result and "active" in result):
            result = {**result, "agents": sort_agents(result.get("agents"))}
        elif isinstance(result, dict) and "runtime" in result:
            result = {"runtime": agent_view(result["runtime"], self.scenario.get("runtimeConfig"))}
        elif method == "session/history/get" and isinstance(result, dict):
            result = {"entries": [self.entry(e) for e in result.get("entries") or result.get("history") or []]}
        elif method == "session/turns/list" and isinstance(result, dict):
            result = {"turns": [t.get("status") for t in result.get("turns") or result.get("items") or []]}
        return {"method": method, "response": {"result": result}}

    def entry(self, entry: dict[str, Any]) -> dict[str, Any]:
        kind = entry.get("type")
        view: dict[str, Any] = {"type": kind, "id": entry.get("id"), "sessionId": entry.get("sessionId")}
        if kind == "message":
            view["role"] = entry.get("role")
            view["content"] = entry.get("content")
        elif kind in {"effect", "callback", "notice"}:
            for key in ("detail", "state", "level", "message"):
                if key in entry:
                    view[key] = self.state(entry[key])
        return view

    def state(self, value: Any) -> Any:
        if isinstance(value, dict):
            reduced = {k: self.state(v) for k, v in value.items() if k != "durationMs"}
            output = reduced.get("output")
            if isinstance(output, dict) and isinstance(output.get("response"), str):
                reduced["output"] = {**output, **answer(output["response"])}
            return reduced
        if isinstance(value, list):
            return [self.state(v) for v in value]
        return value

    def notification(self, message: dict[str, Any]) -> dict[str, Any] | None:
        method = message.get("method")
        params = {k: v for k, v in (message.get("params") or {}).items() if k not in DROPPED}
        if method == "runtime/updated":
            return {"method": method, "runtime": agent_view(params.get("runtime"))}
        if method in {"turn/completed", "turn/failed", "turn/interrupted"}:
            return {"method": method, "status": (params.get("turn") or {}).get("status")}
        if method == "history/entryAdded":
            entry = params.get("entry") or {}
            self.kinds[str(entry.get("id"))] = str(entry.get("type"))
            if entry.get("type") == "message":
                return {"method": method, **self.entry(entry)}
            return {"method": method, **self.entry(entry)}
        if method == "history/entryUpdated":
            if self.kinds.get(str(params.get("entryId"))) == "message":
                return None
            patch = [
                {**operation, "value": self.state(operation.get("value"))}
                for operation in params.get("patch") or []
                if isinstance(operation, dict)
                and str(operation.get("path", "")).rsplit("/", 1)[-1] not in rewind.TIME_KEYS
            ]
            return {"method": method, "entryId": params.get("entryId"), "patch": patch}
        return None

    def request(self, request: dict[str, Any]) -> dict[str, Any]:
        observe = set(self.scenario.get("observe", []))
        conversation = []
        for index, message in enumerate(request["conversation"]):
            message = dict(message)
            if index == 0 and message.get("role") == "user":
                message["content"] = child_prompt(message.get("content"), self.tasks)
            if message.get("role") == "tool":
                message["content"] = task_result(message.get("content"))
            conversation.append(message)
        view: dict[str, Any] = {"conversation": conversation}
        for key in sorted(observe):
            view[key] = request.get(key)
        return view


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    reducer = Reducer(scenario)
    steps = [reducer.step(step) for step in run["steps"]]
    requests = [reducer.request(request) for request in run["requests"]]
    callbacks = [
        {
            "kind": ((c.get("callback") or {}).get("detail") or {}).get("kind"),
            "sessionId": (c.get("callback") or {}).get("sessionId"),
            "relatedEntryId": (c.get("callback") or {}).get("relatedEntryId"),
        }
        for c in run["callbacks"]
    ]
    normalizer = Normalizer(scenario, run["paths"])
    for identity in [*run["sessions"], *run["children"]]:
        normalizer.identity(identity)
    for part in (steps, callbacks, requests):
        normalizer.collect_ids(part)
    return {
        "steps": normalizer.value(steps),
        "callbacks": normalizer.value(callbacks),
        "requests": normalizer.value(requests),
    }


class Normalizer(hooks.Normalizer):
    def text(self, value: str) -> str:
        value = CHILD_DIRECTORY.sub(r"\1<time>_<short>", super().text(value))
        return PLAN_FILE.sub(r"\1<plan>.md", value)


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def send(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    step: dict[str, Any] = {"send": {"method": method, "params": params}}
    if pick:
        step["pick"] = pick
    return step


def agents_list(session: str | None = "$S1") -> dict[str, Any]:
    return send("agents/list", **({"sessionId": session} if session else {}))


def runtime(session: str = "$S1") -> dict[str, Any]:
    return send("runtime/read", pick=["runtime"], sessionId=session)


def switch(name: str, session: str = "$S1") -> dict[str, Any]:
    return send("session/agent/update", sessionId=session, agentName=name)


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def turn(text: str, **options: Any) -> dict[str, Any]:
    return {"turn": text, **options}


def child(method: str, **params: Any) -> dict[str, Any]:
    """A method addressed to the first child session the parent linked."""

    return send(method, sessionId="$K1", **params)


#: A turn that delegates to `explore`, which greps once and answers.
EXPLORE_BACKEND = [
    {"toolCalls": [call("task", {"task": "Find the needle", "agent": "explore"}, "call_t1")]},
    {"text": "Looking.", "toolCalls": [call("grep", {"pattern": "needle", "path": "."}, "call_g1")]},
    {"text": "Found it in notes.txt."},
    {"text": "The subagent found it."},
]
NEEDLE = {"notes.txt": "needle\n"}

#: A subagent that may run the shell but is not allowed to run it unasked.
SHELL_SUBAGENT = {
    "shell-helper.toml": (
        'description = "Runs one command"\n'
        'agent_type = "subagent"\n'
        'enabled_tools = ["bash"]\n'
    ),
}
SHELL_BACKEND = [
    {"toolCalls": [call("task", {"task": "Echo hi", "agent": "shell-helper"}, "call_t1")]},
    {"text": "Running.", "toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")]},
    {"text": "It printed hi."},
    {"text": "Done delegating."},
]

#: The four answers `exit_plan_mode` offers, by the label a client returns.
CLEAR_AUTO = "Yes, clear context and auto approve edits"
AUTO = "Yes, and auto approve edits"
MANUAL = "Yes, and request approval for edits"
STAY = "No"

EXIT_PLAN_BACKEND = [
    {"text": "The plan is ready.", "toolCalls": [call("exit_plan_mode", {}, "call_x1")]},
    {"text": "Now implementing."},
]


def scenarios() -> list[dict[str, Any]]:
    tools = ["tools"]
    return [
        # -- registry -------------------------------------------------------
        {
            "name": "registry/builtins",
            "steps": [agents_list(None), {"start": {}}, agents_list(), runtime()],
        },
        {
            "name": "registry/custom-files",
            "agents": {
                "project": {
                    "code_review-v2x.toml": 'description = "Reviews"\nagent_type = "subagent"\n',
                    "my-AGENT.toml": 'safety = "safe"\n',
                    "legacy.toml": 'base_disabled = ["bash"]\ndisabled_tools = ["grep"]\n',
                    "reckless.toml": 'safety = "reckless"\n',
                    "broken.toml": 'enabled_tools = "grep"\n',
                    "boss.toml": 'agent_type = "boss"\n',
                    "unparsable.toml": "this is = = not toml\n",
                    "notes.txt": "not an agent\n",
                },
            },
            "steps": [{"start": {}}, agents_list(), {"file": "$WS/.vibe/agents/legacy.toml"},
                      switch("legacy"), turn("Hello")],
            "observe": tools,
        },
        {
            "name": "registry/legacy-only",
            "agents": {"user": {"old.toml": 'base_disabled = ["bash", "grep"]\n'}},
            "steps": [{"start": {"agent": "old"}}, {"file": "$ROOT/vibe-home/agents/old.toml"},
                      turn("Hello")],
            "observe": tools,
        },
        {
            "name": "registry/precedence",
            "config": 'agent_paths = ["$ROOT/extra-agents"]\n',
            "agents": {
                "extra": {
                    "plan.toml": 'display_name = "Extra Plan"\n',
                    "helper.toml": 'display_name = "Extra Helper"\n',
                },
                "project": {
                    "plan.toml": 'display_name = "Project Plan"\n',
                    "helper.toml": 'display_name = "Project Helper"\n',
                },
                "user": {
                    "plan.toml": 'display_name = "User Plan"\n',
                    "helper.toml": 'display_name = "User Helper"\n',
                },
            },
            "steps": [{"start": {}}, agents_list()],
        },
        {
            "name": "registry/untrusted-project",
            "trusted": False,
            "agents": {
                "project": {"local.toml": 'display_name = "Local"\n'},
                "user": {"mine.toml": 'display_name = "Mine"\n'},
            },
            "steps": [{"start": {}}, agents_list()],
        },
        {
            "name": "registry/custom-lean",
            "agents": {"project": {"lean.toml": 'display_name = "My Lean"\n'}},
            "steps": [{"start": {}}, agents_list(), switch("lean")],
        },
        {
            "name": "registry/lean-installed",
            "config": 'installed_agents = ["lean"]\n',
            "steps": [{"start": {}}, agents_list()],
        },
        # -- selection ------------------------------------------------------
        {
            "name": "selection/disabled",
            "config": 'disabled_agents = ["plan", "auto-*"]\n',
            "steps": [{"start": {}}, agents_list(), runtime(), switch("plan"),
                      switch("auto-approve"), switch("ask")],
        },
        {
            "name": "selection/enabled",
            "config": 'enabled_agents = ["plan", "re:^a[su].*"]\n',
            "steps": [{"start": {}}, agents_list(), switch("accept-edits"),
                      {"file": "$ROOT/vibe-home/config.toml"}],
        },
        {
            "name": "selection/enabled-default-excluded",
            "config": 'default_agent = "accept-edits"\nenabled_agents = ["plan"]\n',
            "steps": [{"start": {}}],
        },
        {
            "name": "selection/start-disabled",
            "config": 'disabled_agents = ["plan"]\n',
            "steps": [{"start": {"agent": "plan"}}],
        },
        {
            "name": "selection/start-subagent",
            "steps": [{"start": {"agent": "explore"}}],
        },
        {
            "name": "selection/start-unknown",
            "steps": [{"start": {"agent": "nope"}}],
        },
        {
            "name": "selection/start-lean-uninstalled",
            "steps": [{"start": {"agent": "lean"}}],
        },
        {
            "name": "selection/default-agent",
            "config": 'default_agent = "plan"\n',
            "steps": [{"start": {}}, agents_list()],
        },
        {
            "name": "selection/renamed-default",
            "config": 'default_agent = "default"\ndisabled_agents = ["default"]\n',
            "steps": [{"start": {}}, agents_list(), {"file": "$ROOT/vibe-home/config.toml"}],
        },
        {
            "name": "selection/smart-approve-forced",
            "steps": [{"start": {"agent": "smart-approve"}}, agents_list(), switch("plan"),
                      agents_list(), runtime(), switch("smart-approve")],
        },
        {
            "name": "selection/smart-approve-hidden",
            "steps": [{"start": {}}, agents_list(), switch("smart-approve"), switch("lean"),
                      switch("explore"), switch("nope")],
        },
        {
            "name": "selection/smart-approve-available",
            "config": "smart_approve_available = true\n",
            "steps": [{"start": {}}, agents_list(), switch("smart-approve")],
        },
        {
            "name": "selection/smart-approve-default",
            "config": "smart_approve_default = true\n",
            "steps": [{"start": {}}, agents_list(), runtime()],
        },
        {
            "name": "selection/install",
            "config": 'installed_agents = ["zeta"]\n',
            "steps": [{"start": {}}, send("agents/install", sessionId="$S1", agentName="lean"),
                      {"file": "$ROOT/vibe-home/config.toml"}, switch("lean"),
                      send("agents/uninstall", sessionId="$S1", agentName="lean"),
                      {"file": "$ROOT/vibe-home/config.toml"}, agents_list()],
        },
        # -- profile --------------------------------------------------------
        {
            "name": "profile/custom-model",
            "agents": {
                "project": {
                    "tuned.toml": (
                        'active_model = "tuned"\n'
                        "[[models]]\n"
                        'name = "tuned-model-1"\n'
                        'provider = "mistral"\n'
                        'alias = "tuned"\n'
                        "temperature = 0.4\n"
                        'thinking = "off"\n'
                    ),
                },
            },
            "steps": [{"start": {"agent": "tuned"}}, turn("Hello"),
                      send("runtime/read", pick=["runtime"], sessionId="$S1")],
            "observe": ["model", "temperature", "reasoning_effort", "tools"],
            "runtimeConfig": ["activeModel"],
        },
        {
            "name": "profile/custom-tools",
            "agents": {
                "project": {
                    "reader.toml": 'enabled_tools = ["read_*", "grep", "task"]\n'
                                   'disabled_tools = ["task"]\n',
                },
            },
            "steps": [{"start": {"agent": "reader"}}, turn("Hello")],
            "observe": tools,
        },
        {
            "name": "profile/builtin-tools",
            "steps": [{"start": {"agent": "plan"}}, turn("Hello"), switch("ask"), turn("Again"),
                      switch("auto-approve"), turn("Once more")],
            "observe": tools,
        },
        {
            "name": "profile/switch-model",
            "agents": {
                "project": {
                    "small.toml": (
                        'active_model = "small"\n'
                        "[[models]]\n"
                        'name = "small-model-1"\n'
                        'provider = "mistral"\n'
                        'alias = "small"\n'
                        "temperature = 0.1\n"
                    ),
                },
            },
            "steps": [{"start": {}}, turn("Hello"), switch("small"), turn("Again")],
            "observe": ["model", "temperature"],
        },
        # -- delegation -----------------------------------------------------
        {
            "name": "delegation/explore",
            "files": NEEDLE,
            "backend": EXPLORE_BACKEND,
            "steps": [{"start": {"agent": "auto-approve"}}, turn("Delegate the search"),
                      {"tree": True}, child("session/history/get"), child("session/turns/list"),
                      child("runtime/read")],
            "observe": tools,
        },
        {
            "name": "delegation/child-error",
            "files": NEEDLE,
            "backend": [
                EXPLORE_BACKEND[0],
                EXPLORE_BACKEND[1],
                {"status": 400, "body": {"message": "bad request", "type": "invalid_request_error"}},
                {"text": "The subagent failed."},
            ],
            "steps": [{"start": {"agent": "auto-approve"}}, turn("Delegate the search"),
                      {"tree": True}],
        },
        {
            "name": "delegation/unknown-agent",
            "backend": [
                {"toolCalls": [call("task", {"task": "Do it", "agent": "nope"}, "call_t1")]},
                {"text": "No such agent."},
            ],
            "steps": [{"start": {"agent": "auto-approve"}}, turn("Delegate")],
        },
        {
            "name": "delegation/primary-agent",
            "backend": [
                {"toolCalls": [call("task", {"task": "Do it", "agent": "plan"}, "call_t1")]},
                {"text": "Not a subagent."},
            ],
            "steps": [{"start": {"agent": "auto-approve"}}, turn("Delegate")],
        },
        {
            "name": "delegation/disabled-subagent",
            "config": 'disabled_agents = ["explore"]\n',
            "backend": [
                {"toolCalls": [call("task", {"task": "Do it", "agent": "explore"}, "call_t1")]},
                {"text": "Disabled."},
            ],
            "steps": [{"start": {"agent": "auto-approve"}}, turn("Delegate")],
            "observe": tools,
        },
        {
            "name": "delegation/child-approval",
            "agents": {"project": SHELL_SUBAGENT},
            "backend": SHELL_BACKEND,
            "callbacks": [{"approve": True}],
            "steps": [{"start": {"agent": "ask"}}, turn("Delegate the echo"),
                      {"tree": True}],
            "observe": tools,
        },
        {
            "name": "delegation/child-denied",
            "agents": {"project": SHELL_SUBAGENT},
            "backend": SHELL_BACKEND,
            "callbacks": [{"approve": True}, {"approve": False}],
            "steps": [{"start": {"agent": "ask"}}, turn("Delegate the echo")],
            "observe": tools,
        },
        {
            "name": "delegation/depth",
            "agents": {"project": {"nested.toml": 'agent_type = "subagent"\n'
                                                  'enabled_tools = ["task"]\n'}},
            "backend": [
                {"toolCalls": [call("task", {"task": "Go deeper", "agent": "nested"}, "call_t1")]},
                {"toolCalls": [call("task", {"task": "Deeper", "agent": "explore"}, "call_t2")]},
                {"text": "Could not go deeper."},
                {"text": "Done."},
            ],
            "steps": [{"start": {"agent": "auto-approve"}}, turn("Delegate twice")],
            "observe": tools,
        },
        {
            "name": "delegation/ask-parent",
            "files": NEEDLE,
            "backend": EXPLORE_BACKEND,
            "callbacks": [{"approve": True}],
            "steps": [{"start": {"agent": "ask"}}, turn("Delegate the search")],
        },
        # -- switch ---------------------------------------------------------
        {
            "name": "switch/exit-plan-auto",
            "backend": EXIT_PLAN_BACKEND,
            "callbacks": [{"choose": AUTO}],
            "steps": [{"start": {"agent": "plan"}}, turn("Plan it"), agents_list(), {"tree": True}],
            "observe": tools,
        },
        {
            "name": "switch/exit-plan-manual",
            "backend": EXIT_PLAN_BACKEND,
            "callbacks": [{"choose": MANUAL}],
            "steps": [{"start": {"agent": "plan"}}, turn("Plan it"), agents_list(), {"tree": True}],
            "observe": tools,
        },
        {
            "name": "switch/exit-plan-clear",
            "backend": EXIT_PLAN_BACKEND,
            "callbacks": [{"choose": CLEAR_AUTO}],
            "steps": [{"start": {"agent": "plan"}}, turn("Plan it"), agents_list(), {"tree": True}],
            "observe": tools,
        },
        {
            "name": "switch/exit-plan-stay",
            "backend": EXIT_PLAN_BACKEND,
            "callbacks": [{"choose": STAY}],
            "steps": [{"start": {"agent": "plan"}}, turn("Plan it"), agents_list(), {"tree": True}],
            "observe": tools,
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
        print(f"agents oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
