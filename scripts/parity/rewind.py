#!/usr/bin/env python3
"""Black-box capture of the rewind methods served by the app server.

Every scenario starts an app server over stdio in a fresh home, behind the
recording chat-completions stand-in of ``app_server_client.py``, drives real
turns through it so the session holds real history and real file checkpoints,
and then calls ``session/rewind/read`` and ``session/rewind``. What is recorded
is what row 9 of ``docs/parity.md`` is about: each rewind answer, the
notifications it raised, the files on disk afterward, and the conversation the
model is sent on the next turn. Nothing is imported from the reference: the
reference's own ``vibe-app-server`` is the oracle, which is what makes the same
scenarios replayable against this port by
``crates/vibe-app-server/tests/rewind_parity_tests.rs``.

Opening a session and starting a turn are the setup every scenario needs, not
what it measures: their wire is row 17's contract. The two are written here as
abstract steps that the driver renders in the dialect of the server it drives,
and their answers are never recorded.

Normalization maps identifiers, paths and times to placeholders and reduces
every string the server authored to its length and SHA-256, which is what
``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/rewind.py                  # capture the reference
    python3 scripts/parity/rewind.py --check          # recapture and compare
    python3 scripts/parity/rewind.py --server target/debug/vibe-app-server-stdio-fixture \\
        --dialect port --output /tmp/port.json
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

from app_server_client import Normalizer, QUIET_SECONDS, call, rewind, run_scenario  # noqa: E402
import blackbox  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
from reference import OracleError, require_pin  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/rewind-parity/corpus.json"
SCHEMA_VERSION = 1


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[Any]:
    normalizer = Normalizer(scenario, run["paths"])
    # Identities are numbered in the order the scenario learned them, so the
    # same session or entry carries the same placeholder on both sides.
    for identity in run["sessions"]:
        normalizer.identity(identity)
    for step in run["steps"]:
        normalizer.collect_ids(step.get("response"))
    observed: list[Any] = []
    for step in run["steps"]:
        if "files" in step:
            observed.append(step)
            continue
        if "failure" in step:
            observed.append({"failure": normalizer.text(step["failure"])})
            continue
        if "context" in step:
            # A message the server wrote into the conversation, such as a
            # compaction envelope, is its own prose.
            observed.append({"context": normalizer.value(copy.deepcopy(step["context"]))})
            continue
        observed.append({
            "method": step["method"],
            "response": normalizer.value(copy.deepcopy(step["response"])),
            "notifications": step["notifications"],
        })
    return observed


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def rewind_read(entry: str, session: str = "$S1") -> dict[str, Any]:
    return {"send": {"method": "session/rewind/read", "params": {"sessionId": session, "entryId": entry}}}


#: Two turns, each changing files through tools, and a third that only talks.
EDITING_BACKEND = [
    {"toolCalls": [
        call("write_file", {"file_path": "created.txt", "content": "made in turn one\n"}, "call_w1"),
        call("edit", {"file_path": "notes.txt", "old_string": "original", "new_string": "first edit"}, "call_e1"),
    ]},
    {"text": "Turn one done."},
    {"toolCalls": [
        call("edit", {"file_path": "notes.txt", "old_string": "first edit", "new_string": "second edit"}, "call_e2"),
    ]},
    {"text": "Turn two done."},
    {"text": "Turn three only talks."},
]
EDITING_FILES = {"notes.txt": "original\n"}
EDITING_STEPS = [
    {"start": {}},
    {"turn": "Create a file and edit the notes"},
    {"turn": "Edit the notes again"},
    {"turn": "Just answer"},
]
WATCHED = ["notes.txt", "created.txt"]


def scenarios() -> list[dict[str, Any]]:
    return [
        {
            "name": "read/each-user-entry",
            "backend": EDITING_BACKEND,
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind_read("$U1"),
                rewind_read("$U2"),
                rewind_read("$U3"),
            ],
        },
        {
            "name": "read/refusals",
            "backend": EDITING_BACKEND,
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind_read("$A1"),
                rewind_read("unknown-entry"),
                rewind_read("history:1:user"),
                rewind_read("$U1", session="unknown-session"),
            ],
        },
        {
            "name": "rewind/fork-restoring-files",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U2", restoreFiles=True),
                {"files": WATCHED},
                {"turn": "Continue from the fork", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/fork-keeping-files",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U2"),
                {"files": WATCHED},
                {"turn": "Continue from the fork", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/inplace-restoring-files",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U2", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "Continue in place"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/inplace-to-the-first-message",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U1", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "Start over"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/fork-to-the-first-message",
            "backend": [*EDITING_BACKEND, {"text": "After the rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U1", restoreFiles=True),
                {"files": WATCHED},
                {"turn": "Start over", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            "name": "rewind/refusals",
            "backend": EDITING_BACKEND,
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$A1"),
                rewind("unknown-entry", restoreFiles=True),
                rewind("history:1:user"),
                rewind("$U1", session="unknown-session"),
                {"send": {"method": "session/rewind", "params": {"sessionId": "$S1"}}},
                {"send": {"method": "session/rewind", "params": {
                    "sessionId": "$S1", "entryId": "$U1", "restoreFiles": "yes"}}},
                {"send": {"method": "session/rewind", "params": {
                    "sessionId": "$S1", "entryId": "$U1", "extra": True}}},
                {"files": WATCHED},
            ],
        },
        {
            "name": "rewind/while-a-turn-runs",
            "backend": [
                {"text": "First answer."},
                {"text": "Slow answer.", "delay": 3.0},
            ],
            "steps": [
                {"start": {}},
                {"turn": "First question"},
                {"turn": "Slow question", "background": True, "settle": 1.0},
                rewind_read("$U1"),
                rewind("$U1"),
                {"await": True},
            ],
        },
        {
            "name": "rewind/twice",
            "backend": [*EDITING_BACKEND, {"text": "After the second rewind."}],
            "files": EDITING_FILES,
            "steps": [
                *EDITING_STEPS,
                rewind("$U3", restoreFiles=True),
                rewind_read("$U1", session="$S2"),
                rewind("$U1", session="$S2", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "After two rewinds", "session": "$S2"},
                {"context": True},
            ],
        },
        {
            # Upstream compaction appends a boundary and keeps every earlier
            # message and the checkpoint log, so a rewind can reach a turn the
            # model no longer sees. The first request reports enough context to
            # cross the threshold, so the compaction runs before the second.
            "name": "rewind/across-a-compaction",
            "config": (
                "\n[[models]]\n"
                'name = "mistral-vibe-cli-latest"\n'
                'provider = "mistral"\n'
                'alias = "mistral-medium-3.5"\n'
                "auto_compact_threshold = 16\n"
            ),
            "backend": [
                {"toolCalls": [
                    call("write_file", {"file_path": "created.txt", "content": "made in turn one\n"}, "call_w1"),
                ], "promptTokens": 12, "completionTokens": 8},
                {"text": "<summary>The user created a file.</summary>", "promptTokens": 1, "completionTokens": 1},
                {"text": "Turn one done.", "promptTokens": 2, "completionTokens": 1},
                {"toolCalls": [
                    call("edit", {"file_path": "notes.txt", "old_string": "original", "new_string": "after compaction"}, "call_e1"),
                ], "promptTokens": 2, "completionTokens": 1},
                {"text": "Turn two done.", "promptTokens": 2, "completionTokens": 1},
                {"text": "After the rewind.", "promptTokens": 2, "completionTokens": 1},
            ],
            "files": EDITING_FILES,
            "steps": [
                {"start": {}},
                {"turn": "Create a file"},
                {"turn": "Edit the notes"},
                {"context": True},
                rewind_read("$U1"),
                rewind_read("$U2"),
                rewind("$U1", restoreFiles=True, inplace=True),
                {"files": WATCHED},
                {"turn": "Start again"},
                {"context": True},
            ],
        },
        {
            # A rewind to a turn after the boundary keeps the boundary: the
            # forked conversation still starts from the summary.
            "name": "rewind/after-a-compaction",
            "config": (
                "\n[[models]]\n"
                'name = "mistral-vibe-cli-latest"\n'
                'provider = "mistral"\n'
                'alias = "mistral-medium-3.5"\n'
                "auto_compact_threshold = 16\n"
            ),
            "backend": [
                {"toolCalls": [
                    call("write_file", {"file_path": "created.txt", "content": "made in turn one\n"}, "call_w1"),
                ], "promptTokens": 12, "completionTokens": 8},
                {"text": "<summary>The user created a file.</summary>", "promptTokens": 1, "completionTokens": 1},
                {"text": "Turn one done.", "promptTokens": 2, "completionTokens": 1},
                {"toolCalls": [
                    call("edit", {"file_path": "notes.txt", "old_string": "original", "new_string": "after compaction"}, "call_e1"),
                ], "promptTokens": 2, "completionTokens": 1},
                {"text": "Turn two done.", "promptTokens": 2, "completionTokens": 1},
                {"text": "After the rewind.", "promptTokens": 2, "completionTokens": 1},
            ],
            "files": EDITING_FILES,
            "steps": [
                {"start": {}},
                {"turn": "Create a file"},
                {"turn": "Edit the notes"},
                {"context": True},
                rewind("$U2", restoreFiles=True),
                {"files": WATCHED},
                {"turn": "Try the edit again", "session": "$S2"},
                {"context": True},
            ],
        },
    ]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--server", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--dialect", choices=["reference", "port"], default=None)
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
            dialect = arguments.dialect or "port"
        else:
            reference = {"commit": require_pin(arguments.reference, arguments.expected_commit)}
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary), *HARNESS_FLAGS]
            dialect = arguments.dialect or "reference"
        selected = [
            scenario
            for scenario in scenarios()
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started = time.monotonic()
            run = run_scenario(scenario, command, dialect, arguments.quiet)
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
        print(f"rewind oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
