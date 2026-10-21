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
import json
import os
from pathlib import Path
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
import host_env  # noqa: E402
from blackbox import (  # noqa: E402
    APPROVE_FILE_PERMANENT,
    APPROVE_FOLDER_SESSION,
    APPROVE_ONCE,
    APPROVE_SESSION,
    Agent,
    Backend,
    CLIENT_CAPABILITIES,
    DISMISS,
    ELICITING,
    INITIALIZE,
    NEW_SESSION,
    Normalizer,
    REJECT,
    RESPONSE_TIMEOUT,
    World,
    base_config,
    ext,
    initialize,
    new_session,
    notify,
    prompt,
    rendered,
    request,
    write_tree,
)
from reference import OracleError, require_pin  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-acp/tests/acp-parity/corpus.json"
UNIFIED_OUTPUT = REPOSITORY / "crates/vibe-acp/tests/acp-parity/unified-corpus.json"
SCHEMA_VERSION = 1

#: How long the agent must stay silent before a step is considered settled.
QUIET_SECONDS = 0.6

#: Notifications the reference emits from detached tasks, whose position
#: relative to the response is not part of the contract.
ASYNC_UPDATES = {"usage_update", "available_commands_update"}


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


def run_scenario(
    scenario: dict[str, Any], command: list[str], quiet: float
) -> dict[str, Any]:
    backend = Backend()
    root = Path(tempfile.mkdtemp(prefix="vibe-acp-oracle-")).resolve()
    world = World(root)
    try:
        backend.responses = world.substitute(copy.deepcopy(scenario.get("backend", [])))
        config = base_config(backend) + scenario.get("config", "")
        (world.vibe_home / "config.toml").write_text(config, encoding="utf-8", newline="")
        write_tree(world.workspace, scenario.get("files", {}))
        write_tree(world.vibe_home, scenario.get("vibeHomeFiles", {}))
        write_tree(world.home, scenario.get("homeFiles", {}))
        env = host_env.isolated(
            world.home,
            world.vibe_home,
            MISTRAL_API_KEY="oracle-key",
            VIBE_API_BASE=f"{backend.base}/v1/chat/completions",
        )
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
                message = world.substitute(step["send"], native=True)
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


def call(name: str, arguments: dict[str, Any], identifier: str = "call_1") -> dict[str, Any]:
    return {"toolCalls": [{"id": identifier, "name": name, "arguments": arguments}]}


#: Files outside the workdir, under the scenario's home directory.
OUTSIDE_FILES = {"outside/data/a.txt": "alpha\n", "outside/data/nested/b.txt": "beta\n"}
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
            # A folder outside the workdir offers its recursive grant, which
            # covers the nested search that follows.
            "name": "prompt/grep-outside-folder",
            "homeFiles": OUTSIDE_FILES,
            "backend": [call("grep", {"pattern": "alpha", "path": "$HOME/outside/data"}),
                        call("grep", {"pattern": "beta", "path": "$HOME/outside/data/nested/b.txt"},
                             "call_2"),
                        {"text": "Found"}],
            "client": {"session/request_permission": APPROVE_FOLDER_SESSION},
            "steps": [*BASE, prompt(2, "Search outside")],
        },
        {
            # A file outside the workdir offers its exact grant, kept in the
            # configuration, which covers the same file again.
            "name": "prompt/read-outside-file",
            "homeFiles": OUTSIDE_FILES,
            "backend": [call("read_file", {"file_path": "$HOME/outside/data/a.txt"}),
                        call("read_file", {"file_path": "$HOME/outside/data/a.txt"}, "call_2"),
                        {"text": "Read"}],
            "client": {"session/request_permission": APPROVE_FILE_PERMANENT},
            "steps": [*BASE, prompt(2, "Read outside")],
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
    root = Path(tempfile.mkdtemp(prefix="vibe-acp-argv-")).resolve()
    try:
        env = host_env.isolated(root / "home", root / "vibe-home")
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


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--agent", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument(
        "--harness",
        choices=["legacy", "unified"],
        default="legacy",
        help="the backend both sides run: the legacy one through the escape hatch, or the "
        "Unified Harness every session opens on by default",
    )
    parser.add_argument("--only", action="append", default=[], help="scenario name filter")
    parser.add_argument("--raw", action="store_true", help="also keep the raw messages")
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--jobs", type=int, default=1, help="scenarios run side by side")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    flags = HARNESS_FLAGS if arguments.harness == "legacy" else ()
    if arguments.output is None:
        arguments.output = DEFAULT_OUTPUT if arguments.harness == "legacy" else UNIFIED_OUTPUT
    try:
        if arguments.agent is not None:
            command = [str(arguments.agent.resolve())]
            reference = {"commit": "agent-override"}
        else:
            reference = {"commit": require_pin(arguments.reference, arguments.expected_commit)}
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
            run = run_scenario(scenario, [*command, *flags], arguments.quiet)
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
        staged.write_text(rendered(corpus), encoding="utf-8", newline="")
        os.replace(staged, arguments.output)
        return 0
    except OracleError as error:
        print(f"acp oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
