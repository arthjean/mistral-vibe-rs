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
import json
import os
from pathlib import Path
import sys
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import blackbox  # noqa: E402
import hook_recorder  # noqa: E402
from loop_capture import (  # noqa: E402
    Normalizer,
    PERSISTED,
    START,
    TITLES,
    call,
    echo_steps,
    reduce_result,
    reduce_turn,
    run_scenario,
    send,
)
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
from reference import OracleError, require_pin  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/agent-loop-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.5

GIT = ["git", "-c", "user.name=Oracle", "-c", "user.email=oracle@example.invalid"]


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


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


#: Files outside the workdir, a directory holding one and a nested one, which
#: the path scope scenarios reach.
OUTSIDE_FILES = {"write": {"$ROOT/outside/data/a.txt": "alpha\n",
                           "$ROOT/outside/data/nested/b.txt": "beta\n"}}

ECHO = [{"toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")]}, {"text": "Finished."}]
#: A step running a command outside the shell allowlist, which the `ask`
#: agent sends to the operator.
TOUCH = [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_t1")]},
         {"text": "Finished."}]


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
            "hooks": {"project": hook_recorder.hook("guard", "pre_tool")},
            "responses": {"guard": [hook_recorder.answer(decision="deny", reason="Not today")]},
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
            "hooks": {"project": hook_recorder.hook("slow", "pre_tool")},
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
            reference = {"commit": require_pin(arguments.reference, arguments.expected_commit)}
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
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
                raise OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(blackbox.rendered(corpus), encoding="utf-8", newline="")
        os.replace(staged, arguments.output)
        return 0
    except OracleError as error:
        print(f"agent loop oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
