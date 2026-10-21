"""What a black-box capture of the conversation loop drives the app server with.

``agent_loop.py`` captures the loop a session runs its turns through and
``unified_harness.py`` the same loop on the Unified Harness; both run their
scenarios here, behind the chat-completions stand-in of :mod:`stand_in` with
its utility queue and its fast-model probe answer. This module holds one
server process whose callbacks a scripted client answers (:class:`Server`),
the scenario runner (:func:`run_scenario`), the reductions of what a turn
raised and a session persisted (:class:`Normalizer`, :func:`reduce_turn`), and
the views of Unified Harness answers that ``connectors.py`` reads too. Nothing
here imports the reference.
"""

from __future__ import annotations

from collections.abc import Callable
import copy
import json
import os
from pathlib import Path
import re
import subprocess
import time
from typing import Any

import app_server_client
import blackbox
import hook_recorder
import host_env
from reference import OracleError
import stand_in
import subagent_client


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(subagent_client.Server):
    """A server whose callbacks the scenario answers from a script.

    Beyond what ``subagent_client.Server`` answers, a scripted answer may
    first edit the plan file the session works on (``editPlan``), the way a
    user edits it during a plan review, and may hold its answer
    (``holdSeconds``).
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
                    plan.write_text(script["editPlan"], encoding="utf-8", newline="")
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


class Session(subagent_client.Session):
    """What a scenario learned, plus the turn each `turn/started` opened."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.turns: list[str] = []
        #: Whether the last turn that started has settled.
        self.settled = False

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        if app_server_client.settles({})(message):
            self.settled = True
        if message.get("method") == "turn/started":
            self.settled = False
            turn = ((message.get("params") or {}).get("turn") or {}).get("id")
            if isinstance(turn, str) and turn not in self.turns:
                self.turns.append(turn)


def client_info(scenario: dict[str, Any]) -> dict[str, Any]:
    return {"name": "agent-loop-oracle", "version": "0", **scenario.get("client", {})}


def connect(scenario: dict[str, Any], command: list[str], env: dict[str, str],
            world: blackbox.World, quiet: float) -> Server:
    server = Server(command, env, world.workspace, {}, world,
                    callbacks=copy.deepcopy(scenario.get("callbacks", [])),
                    plans=world.vibe_home / "plans")
    server.send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                 "params": {"clientInfo": client_info(scenario),
                            "capabilities": {"callbackKinds": ["approval", "user_input"]}}})
    server.collect(1, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    return server


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

    turns = subagent_client.transcript(messages)
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
    persisted: Callable[[blackbox.World], Any],
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
            params = app_server_client.render_turn("reference", target, step["turn"])
            params.update(session.substitute(step.get("params", {})))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": params})
            if step.get("background"):
                # The turn is left running until an `interrupt` or a `wait`.
                time.sleep(step.get("settle", 1.0))
                pending = server.collect(None, quiet)
                for message in pending:
                    session.learn(message)
                continue
            observed = server.wait_for(app_server_client.settles({}))
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
            observed = pending + server.wait_for(app_server_client.settles({}))
            observed += server.collect(None, quiet)
            for message in observed:
                session.learn(message)
            steps.append({"turn": notifications(observed)})
            pending = []
            continue
        if "wait" in step:
            # A background turn settles on its own, possibly while an earlier
            # step was still collecting.
            observed = pending + ([] if session.settled else server.wait_for(app_server_client.settles({})))
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
                if os.name == "nt" and target.suffix in (".json", ".jsonl"):
                    # The placeholders sit inside JSON strings, where a Windows
                    # separator would read as an escape sequence.
                    for token, path in (("$WS", world.workspace), ("$ROOT", world.root)):
                        text = text.replace(token, json.dumps(str(path))[1:-1])
                target.write_text(session.substitute(text), encoding="utf-8", newline="")
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
        response = subagent_client.response_to(observed, identifier)
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


def persisted(world: blackbox.World) -> list[dict[str, Any]]:
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


def run_scenario(
    scenario: dict[str, Any],
    command: list[str],
    quiet: float,
    *,
    session_class: type[Session] = Session,
    persisted: Callable[[blackbox.World], Any] = persisted,
    request_view: Callable[[dict[str, Any], bool], dict[str, Any]] = request_view,
) -> dict[str, Any]:
    """Runs one scenario against a fresh server.

    The Unified Harness capture learns more from the answers and reads the
    store and the requests its own way, so it passes its session class and its
    two views in place of :class:`Session`, :func:`persisted` and
    :func:`request_view`.
    """

    backend = stand_in.Backend(utility=True, probe=True, scripted_usage=True)
    with host_env.scratch_root("vibe-loop-oracle-") as root:
        session = session_class(root)
        world = session.world
        try:
            backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", [])))
            backend.utility = session.substitute(copy.deepcopy(scenario.get("utility", [])))
            # The scenario's keys lead, so none of them lands inside the provider
            # table the base configuration ends on.
            (world.vibe_home / "config.toml").write_text(
                session.substitute(scenario.get("config", "")) + blackbox.base_config(backend)
                + session.substitute(scenario.get("tables", "")),
                encoding="utf-8",
                newline="",
            )
            if scenario.get("trusted", True):
                (world.vibe_home / "trusted_folders.toml").write_text(
                    f"trusted = [{json.dumps(str(world.workspace))}]\nuntrusted = []\n",
                    encoding="utf-8",
                    newline="",
                )
            blackbox.write_tree(world.workspace, scenario.get("files", {}))
            (root / "hook.py").write_text(hook_recorder.host_recorder(), encoding="utf-8", newline="")
            hook_recorder.write_responses(root, scenario.get("responses", {}))
            hook_recorder.write_hooks(session, scenario.get("hooks", {}))
            env = {
                **host_env.isolated(
                    world.home,
                    world.vibe_home,
                    MISTRAL_API_KEY="oracle-key",
                    VIBE_API_BASE=f"{backend.base}/v1/chat/completions",
                ),
                **({} if scenario.get("titles") else {"VIBE_TEST_DISABLE_AUTO_TITLE": "1"}),
            }
            steps: list[dict[str, Any]] = []
            holder: dict[str, Server] = {}
            try:
                holder["server"] = connect(scenario, command, env, world, quiet)
                run_steps(scenario, holder, command, env, session, steps, quiet, persisted)
            except OracleError as error:
                steps.append({"failure": str(error)})
            finally:
                if "server" in holder:
                    holder["server"].stop()
            return {
                "steps": steps,
                "sessions": session.sessions,
                "callbacks": holder["server"].callback_log if "server" in holder else [],
                "requests": [
                    {"utility": stand_in.is_utility(body), **request_view(body, stand_in.is_utility(body))}
                    for body in backend.bodies
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


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: Keys whose numeric values measure elapsed time or throughput.
RATE_KEYS = {"duration", "last_turn_duration", "tokens_per_second", "durationMs", "duration_ms"}

#: Notification fields that count or date events, which every other part of
#: the server moves.
DROPPED = {"emittedAt", "eventId"}

#: Keys whose string values are identities.
ID_KEYS = app_server_client.ID_KEYS | {"tool_call_id", "session_id", "parent_session_id", "message_id",
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
PLAN_FILE = re.compile(r"plans[/\\]+\d+-[a-z]+(?:-[a-z]+)*\.md")
#: A plan file's absolute path, as a review's footer names it.
PLAN_PATH = re.compile(r"/\S*?plans/\d+-[a-z]+(?:-[a-z]+)*\.md")
#: A child's log directory beneath its parent's: its agent, when it started
#: and the head of its random identifier.
CHILD_DIRECTORY = re.compile(r"((?:^|/)agents/[A-Za-z0-9_.-]+?_)(?:\d{8}_\d{6}|<time>)_[0-9a-f]{8}")


class Normalizer(subagent_client.Normalizer):
    #: The keys whose string values are identities.
    id_keys = ID_KEYS

    def value(self, value: Any, key: str | None = None) -> Any:
        if key in RATE_KEYS and isinstance(value, int | float) and not isinstance(value, bool):
            return "<rate>" if value else 0
        if isinstance(value, str) and PLAN_FILE.search(value):
            value = PLAN_FILE.sub("plans/<plan>.md", value)
        if isinstance(value, str) and CHILD_DIRECTORY.search(value):
            value = CHILD_DIRECTORY.sub(r"\1<time>_<short>", value)
        if key in self.id_keys and isinstance(value, str) and value:
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
        if key in self.id_keys and isinstance(value, str) and value and value not in self.authored:
            self.identity(value)
            return
        super().collect_ids(value, key)


def reduce_notification(message: dict[str, Any], time_keys: set[str]) -> dict[str, Any]:
    params = {k: v for k, v in (message.get("params") or {}).items() if k not in DROPPED}
    for operation in params.get("patch") or []:
        if isinstance(operation, dict) and str(operation.get("path", "")).rsplit("/", 1)[-1] in time_keys:
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


def reduce_turn(
    scenario: dict[str, Any],
    messages: list[dict[str, Any]],
    time_keys: set[str] = app_server_client.TIME_KEYS,
) -> list[Any]:
    """A turn's notifications in the loop's families, patch operations on
    ``time_keys`` reduced to a placeholder and background title work last."""

    kept, landed = [], []
    for message in messages:
        method = str(message.get("method", ""))
        if not method.startswith(FAMILIES):
            continue
        if scenario.get("observe") and not any(method.startswith(m) for m in scenario["observe"]):
            continue
        reduced = reduce_notification(message, time_keys)
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


# --------------------------------------------------------------------------
# Scenario steps
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


def echo_steps(count: int) -> list[dict[str, Any]]:
    """A turn of `count` tool steps and a closing answer."""

    return [
        {"toolCalls": [call("bash", {"command": f"echo step{index}"}, f"call_s{index}")]}
        for index in range(1, count + 1)
    ] + [{"text": "All steps ran."}]


# --------------------------------------------------------------------------
# Views of Unified Harness answers
# --------------------------------------------------------------------------


#: Patch keys whose operations depend on the clock alone. Every Core step
#: stamps the session's ``updatedAt`` with the millisecond it was observed in,
#: and a committed entry is rebuilt with that stamp, so whether two steps move
#: them depends on whether they landed in the same millisecond. ``bumpedAt``
#: is published by the host once it has answered a callback, racing the
#: harness's own publication of the state that answer produced: it lands in a
#: patch of its own or inside that state's, from one run to the next. The
#: persisted ``bumped_at`` still records that the bump happened.
CLOCK_KEYS = {"updatedAt", "createdAt", "bumpedAt"}


def reduce_patches(messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Notifications whose patches list their operations by path, with the
    clock's own operations dropped, and a notification left with nothing to
    patch dropped with them.

    The reference diffs two states with the ``jsonpatch`` package, which walks
    an object's keys in the order of a Python set, so the same change comes out
    in another order from one process to the next.
    """

    kept = []
    for message in messages:
        patch = (message.get("params") or {}).get("patch")
        if isinstance(patch, list):
            patch[:] = [
                operation
                for operation in patch
                if str(operation.get("path", "")).rsplit("/", 1)[-1] not in CLOCK_KEYS
            ]
            if not patch:
                continue
            patch.sort(key=lambda operation: str(operation.get("path", "")))
        kept.append(message)
    return kept


#: A ``best_match`` answer's front matter counts, which its BM25 ranking
#: decides.
RANKED_COUNTS = ("relevantFunctionCount", "relatedFunctionCount", "relatedConnectorCount",
                 "relevantConnectorNoticeCount")
GUIDANCE_ENTRY = re.compile(r"<name>(.*?)</name>\s*<status>(.*?)</status>", re.S)


def search_view(text: str) -> str:
    """A ``search_tool_functions`` answer without the prose it quotes.

    Function descriptions and connector guidance are each server's own words,
    and a ``best_match`` ranking is BM25 over them, so the ranking, its counts
    and the descriptions are left out: what stays is the layout, the names,
    the modes and every declaration's types.
    """

    lines = text.split("\n")
    ranked = "# Best Matching Tool Candidates" in lines
    kept: list[str] = []
    in_description = False
    in_doc: list[str] | None = None
    index = 0
    while index < len(lines):
        line = lines[index]
        index += 1
        stripped = line.strip()
        if ranked and any(line.startswith(f"{key}: ") for key in RANKED_COUNTS):
            kept.append(line.split(":")[0] + ": <ranked>")
            continue
        if ranked and re.fullmatch(r"\d+\. \S+ - [0-9.]+", line):
            if not kept or kept[-1] != "<ranked candidates>":
                kept.append("<ranked candidates>")
            continue
        if stripped.startswith("<connector-guidance>"):
            block = [line]
            while index < len(lines) and "</connector-guidance>" not in block[-1]:
                block.append(lines[index])
                index += 1
            entries = sorted(set(GUIDANCE_ENTRY.findall("\n".join(block))))
            kept.append("<connector-guidance " + ", ".join(f"{name}={status}" for name, status in entries) + ">")
            continue
        if line == "// description:":
            kept.append(line)
            in_description = True
            continue
        if in_description:
            if line == "":
                in_description = False
                kept.append(line)
            continue
        if stripped == "/**":
            in_doc = [line]
            continue
        if in_doc is not None:
            if stripped == "*/":
                if len(in_doc) > 1:
                    kept.extend([in_doc[0], *in_doc[1:], line])
                in_doc = None
            elif stripped.startswith("* @"):
                in_doc.append(line)
            continue
        kept.append(line)
    return "\n".join(kept)


def program_failure_view(text: str) -> str:
    """A ``run_typescript`` failure without its stack's frames, which name
    lines of each server's own sandbox source."""

    prefix = "run_typescript failed: "
    if not text.startswith(prefix):
        return text
    head, separator, logs = text[len(prefix):].partition("\n\n")
    try:
        error = json.loads(head)
    except json.JSONDecodeError:
        return text
    if isinstance(error, dict) and isinstance(error.get("stack"), str):
        error["stack"] = error["stack"].split("\n", 1)[0] + "\n<frames>"
    return prefix + json.dumps(error, separators=(",", ":")) + separator + logs
