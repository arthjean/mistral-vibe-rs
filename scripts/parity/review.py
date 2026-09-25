#!/usr/bin/env python3
"""Black-box capture of the six review methods served by the app server.

The checkpoint oracle beside this one pins the engine a review reads: which
regions each edit produces, what each decision drags, what a file projects to.
This one pins what a client reaches through it: ``review/state``,
``review/baseline``, ``review/turnDiff``, ``review/hunks``, ``review/approve``
and ``review/revert``, answered by a real server over stdio after real turns,
hand edits, shell commands, a compaction and a rewind, together with the files
on disk each decision leaves behind.

The driver is the rewind oracle's (``rewind.py``): a fresh home per scenario,
the scripted chat-completions stand-in, and the same abstract ``start`` and
``turn`` steps rendered in the dialect of the server being driven. Nothing is
imported from the reference; its own ``vibe-app-server`` is the oracle, which
is what lets ``crates/vibe-app-server/tests/review_parity_tests.rs`` replay the
same scenarios against this port.

A client names a path, an owner or a region by what an earlier answer told it,
so a scenario can too: ``$P1`` is the first path a ``review/state`` answer
listed, ``$O1`` the first owner, and ``$V1`` and ``$N1`` the version index and
ordinal of the first region, each numbered in the order the answers revealed
them. A path can also be written out, ``$WS/notes.txt`` or ``notes.txt``, to
pin what a server does with a path it never published.

Normalization is the rewind oracle's, with one exception: a file's content is
the scenario's own text, not something a server authored, so ``content``,
``baseline`` and ``current`` are recorded verbatim rather than as digests.

Usage::

    python3 scripts/parity/review.py                  # capture the reference
    python3 scripts/parity/review.py --check          # recapture and compare
    python3 scripts/parity/review.py --server target/debug/vibe-app-server-stdio-fixture \\
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
import sys
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
import rewind  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/review-parity/corpus.json"
SCHEMA_VERSION = 1

#: The keys whose strings are file content, which the scenario authored.
CONTENT_KEYS = {"content", "baseline", "current"}

TOKEN = re.compile(r"\$(O|V|N|P)(\d+)")


class OracleError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# What a client learns from the answers
# --------------------------------------------------------------------------


class ReviewSession(rewind.Session):
    """The rewind oracle's session, also learning what review answers name."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.owners: list[dict[str, Any]] = []
        self.regions: list[tuple[int, int]] = []
        self.paths: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        result = message.get("result")
        if not isinstance(result, dict) or "scopes" not in result or "files" not in result:
            return
        for scope in result.get("scopes") or []:
            owner = scope.get("owner")
            if isinstance(owner, dict) and owner not in self.owners:
                self.owners.append(owner)
        for review_file in result.get("files") or []:
            path = review_file.get("path")
            if isinstance(path, str) and path not in self.paths:
                self.paths.append(path)
            for region in review_file.get("regions") or []:
                reference = (region.get("versionIndex"), region.get("ordinal"))
                if reference not in self.regions:
                    self.regions.append(reference)
                # A turn still running owns regions before it has a scope, so
                # an owner is also learned from the regions it produced.
                owner = region.get("owner")
                if isinstance(owner, dict) and owner not in self.owners:
                    self.owners.append(owner)

    def substitute(self, value: Any) -> Any:
        if isinstance(value, str) and (match := TOKEN.fullmatch(value)):
            kind, index = match.group(1), int(match.group(2)) - 1
            table: list[Any] = {
                "O": self.owners,
                "V": self.regions,
                "N": self.regions,
                "P": self.paths,
            }[kind]
            if index >= len(table):
                # A token the answers never revealed stays as written, so the
                # request still goes out and its refusal is the observation.
                return value
            learned = table[index]
            if kind == "V":
                return learned[0]
            if kind == "N":
                return learned[1]
            return copy.deepcopy(learned)
        return super().substitute(value)


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


class Normalizer(rewind.Normalizer):
    #: Set while a review answer is normalized, the only place where
    #: `CONTENT_KEYS` hold file content rather than a server's prose.
    verbatim = False

    def value(self, value: Any, key: str | None = None) -> Any:
        if self.verbatim and key in CONTENT_KEYS and isinstance(value, str):
            return value
        return super().value(value, key)

    def review(self, value: Any) -> Any:
        self.verbatim = True
        try:
            return self.value(value)
        finally:
            self.verbatim = False


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[Any]:
    normalizer = Normalizer(scenario, run["paths"])
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
            observed.append({"context": normalizer.value(copy.deepcopy(step["context"]))})
            continue
        response = copy.deepcopy(step["response"])
        observed.append({
            "method": step["method"],
            "response": normalizer.review(response)
            if step["method"].startswith("review/")
            else normalizer.value(response),
            "notifications": step["notifications"],
        })
    return observed


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


call = rewind.call


def send(method: str, session: str = "$S1", **params: Any) -> dict[str, Any]:
    return {"send": {"method": method, "params": {"sessionId": session, **params}}}


def raw(method: str, params: dict[str, Any]) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


def state(session: str = "$S1") -> dict[str, Any]:
    return send("review/state", session)


def approve(target: dict[str, Any], session: str = "$S1") -> dict[str, Any]:
    return send("review/approve", session, target=target)


def revert(target: dict[str, Any], session: str = "$S1") -> dict[str, Any]:
    return send("review/revert", session, target=target)


def edit(path: str, old: str, new: str, identifier: str) -> dict[str, Any]:
    return call("edit", {"file_path": path, "old_string": old, "new_string": new}, identifier)


def write(path: str, content: str, identifier: str) -> dict[str, Any]:
    return call("write_file", {"file_path": path, "content": content}, identifier)


def bash(command: str, identifier: str) -> dict[str, Any]:
    return call("bash", {"command": command}, identifier)


#: Three lines, so two turns can change different hunks of one file.
NOTES = "alpha\nbeta\ngamma\n"
FILES = {"notes.txt": NOTES}
WATCHED = ["notes.txt", "created.txt"]

#: Turn one creates a file and edits the first line of the notes, turn two edits
#: the last line and then rewrites the first again, and turn three only talks.
EDITING_BACKEND = [
    {"toolCalls": [
        write("created.txt", "made in turn one\n", "call_w1"),
        edit("notes.txt", "alpha", "ALPHA", "call_e1"),
    ]},
    {"text": "Turn one done."},
    {"toolCalls": [
        edit("notes.txt", "gamma", "GAMMA", "call_e2"),
        edit("notes.txt", "ALPHA", "Alpha", "call_e3"),
    ]},
    {"text": "Turn two done."},
    {"text": "Turn three only talks."},
]
EDITING_STEPS = [
    {"start": {}},
    {"turn": "Create a file and edit the notes"},
    {"turn": "Edit the notes again"},
    {"turn": "Just answer"},
    state(),
]

COMPACTING_CONFIG = (
    "\n[[models]]\n"
    'name = "mistral-vibe-cli-latest"\n'
    'provider = "mistral"\n'
    'alias = "mistral-medium-3.5"\n'
    "auto_compact_threshold = 16\n"
)
#: Turn one edits and reports enough context that a compaction runs before turn
#: two, which edits again after the boundary.
COMPACTING_BACKEND = [
    {"toolCalls": [
        write("created.txt", "made in turn one\n", "call_w1"),
        edit("notes.txt", "alpha", "ALPHA", "call_e1"),
    ], "promptTokens": 12, "completionTokens": 8},
    {"text": "<summary>The user edited the notes.</summary>", "promptTokens": 1, "completionTokens": 1},
    {"text": "Turn one done.", "promptTokens": 2, "completionTokens": 1},
    {"toolCalls": [
        edit("notes.txt", "gamma", "GAMMA", "call_e2"),
    ], "promptTokens": 2, "completionTokens": 1},
    {"text": "Turn two done.", "promptTokens": 2, "completionTokens": 1},
]
COMPACTING_STEPS = [
    {"start": {}},
    {"turn": "Create a file and edit the notes"},
    {"turn": "Edit the notes again"},
    {"context": True},
    state(),
]


def edited(name: str, *steps: dict[str, Any], **extra: Any) -> dict[str, Any]:
    """A scenario over the three editing turns, then `steps`."""

    return {
        "name": name,
        "backend": copy.deepcopy(EDITING_BACKEND),
        "files": FILES,
        "steps": [*EDITING_STEPS, *steps],
        **extra,
    }


def compacted(name: str, *steps: dict[str, Any]) -> dict[str, Any]:
    """A scenario over two editing turns with a compaction between them."""

    return {
        "name": name,
        "config": COMPACTING_CONFIG,
        "backend": copy.deepcopy(COMPACTING_BACKEND),
        "files": FILES,
        "steps": [*COMPACTING_STEPS, *steps],
    }


def reads(path: str) -> list[dict[str, Any]]:
    """Every read a panel makes about one file, for each owner."""

    return [
        send("review/baseline", path=path),
        send("review/hunks", path=path),
        send("review/hunks", path=path, owner="$O1"),
        send("review/hunks", path=path, owner="$O2"),
        send("review/turnDiff", path=path, owner="$O1"),
        send("review/turnDiff", path=path, owner="$O2"),
    ]


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- Reads ------------------------------------------------------------
        {
            "name": "read/before-any-session",
            "steps": [
                raw("review/state", {"sessionId": "no-such-session"}),
                raw("review/baseline", {"sessionId": "no-such-session", "path": "notes.txt"}),
            ],
        },
        {
            "name": "read/before-any-turn",
            "files": FILES,
            "steps": [
                {"start": {}},
                state(),
                send("review/baseline", path="$WS/notes.txt"),
                send("review/hunks", path="$WS/notes.txt"),
                send("review/turnDiff", path="$WS/notes.txt", owner={"kind": "agent", "turnId": 1}),
            ],
        },
        edited("read/after-three-turns", *reads("$P1"), *reads("$P2")),
        edited(
            "read/path-spellings",
            send("review/baseline", path="$WS/notes.txt"),
            send("review/baseline", path="notes.txt"),
            send("review/baseline", path="./notes.txt"),
            send("review/hunks", path="notes.txt"),
            send("review/turnDiff", path="notes.txt", owner="$O1"),
            send("review/turnDiff", path="$WS/missing.txt", owner="$O1"),
        ),
        # -- Decisions ----------------------------------------------------------
        edited(
            "decide/region",
            approve({"kind": "region", "path": "$P1", "versionIndex": "$V1", "ordinal": "$N1"}),
            state(),
            revert({"kind": "region", "path": "$P2", "versionIndex": "$V2", "ordinal": "$N2"}),
            {"files": WATCHED},
            state(),
        ),
        edited(
            "decide/regions",
            revert({"kind": "regions", "path": "$P2", "regions": [
                {"versionIndex": "$V2", "ordinal": "$N2"},
                {"versionIndex": "$V3", "ordinal": "$N3"},
            ]}),
            {"files": WATCHED},
            state(),
        ),
        edited(
            "decide/scope",
            revert({"kind": "scope", "owner": "$O2"}),
            {"files": WATCHED},
            state(),
            approve({"kind": "scope", "owner": "$O1"}),
            {"files": WATCHED},
            state(),
        ),
        edited(
            "decide/scope-file",
            revert({"kind": "scopeFile", "owner": "$O1", "path": "$P2"}),
            {"files": WATCHED},
            state(),
            *reads("$P2"),
        ),
        edited(
            "decide/file",
            approve({"kind": "file", "path": "$P1"}),
            revert({"kind": "file", "path": "$P2"}),
            {"files": WATCHED},
            state(),
        ),
        edited(
            "decide/all-revert",
            revert({"kind": "all"}),
            {"files": WATCHED},
            state(),
        ),
        edited(
            "decide/all-approve",
            approve({"kind": "all"}),
            {"files": WATCHED},
            state(),
            send("review/baseline", path="$WS/notes.txt"),
        ),
        edited(
            "decide/last-turns",
            revert({"kind": "lastTurns", "count": 0}),
            revert({"kind": "lastTurns", "count": -2}),
            approve({"kind": "lastTurns", "count": 1}),
            state(),
            revert({"kind": "lastTurns", "count": 5}),
            {"files": WATCHED},
            state(),
        ),
        edited(
            "decide/path-spellings",
            revert({"kind": "file", "path": "notes.txt"}),
            {"files": WATCHED},
            revert({"kind": "file", "path": "$WS/notes.txt"}),
            {"files": WATCHED},
            state(),
        ),
        # -- Hand edits, the shell and opaque files ---------------------------
        {
            "name": "manual/between-turns",
            "backend": [
                {"toolCalls": [edit("notes.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
                {"toolCalls": [edit("notes.txt", "gamma", "GAMMA", "call_e2")]},
                {"text": "Turn two done."},
            ],
            "files": FILES,
            "steps": [
                {"start": {}},
                {"turn": "Edit the first line"},
                {"write": {"notes.txt": "ALPHA\nbeta by hand\ngamma\n"}},
                state(),
                {"turn": "Edit the last line"},
                state(),
                *reads("$P1"),
                revert({"kind": "scope", "owner": "$O2"}),
                {"files": ["notes.txt"]},
                state(),
            ],
        },
        {
            "name": "manual/before-a-decision",
            "backend": [
                {"toolCalls": [edit("notes.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
            ],
            "files": FILES,
            "steps": [
                {"start": {}},
                {"turn": "Edit the first line"},
                {"write": {"notes.txt": "ALPHA\nbeta\ngamma by hand\n"}},
                revert({"kind": "all"}),
                {"files": ["notes.txt"]},
                state(),
            ],
        },
        {
            "name": "shell/deletion",
            "backend": [
                {"toolCalls": [edit("notes.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
                {"toolCalls": [bash("rm notes.txt", "call_b1")]},
                {"text": "Turn two done."},
            ],
            "files": FILES,
            "steps": [
                {"start": {}},
                {"turn": "Edit the first line"},
                {"turn": "Delete the notes"},
                state(),
                *reads("$P1"),
                revert({"kind": "scope", "owner": "$O2"}),
                {"files": ["notes.txt"]},
                state(),
            ],
        },
        {
            "name": "shell/revert-into-a-removed-directory",
            "backend": [
                {"toolCalls": [write("dir/sub.txt", "nested\n", "call_w1")]},
                {"text": "Turn one done."},
                {"toolCalls": [bash("rm -r dir", "call_b1")]},
                {"text": "Turn two done."},
            ],
            "steps": [
                {"start": {}},
                {"turn": "Create a nested file"},
                {"turn": "Remove its directory"},
                state(),
                revert({"kind": "scope", "owner": "$O2"}),
                {"files": ["dir/sub.txt"]},
                state(),
            ],
        },
        {
            "name": "opaque/hand-written-bytes",
            "backend": [
                {"toolCalls": [edit("notes.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
            ],
            "files": FILES,
            "steps": [
                {"start": {}},
                {"turn": "Edit the first line"},
                {"writeBytes": {"notes.txt": "00ff00fe41"}},
                state(),
                *reads("$P1"),
                revert({"kind": "scope", "owner": "$O2"}),
                {"files": ["notes.txt"]},
                state(),
            ],
        },
        {
            "name": "manual/reading-an-untracked-file",
            "files": {**FILES, "other.txt": "untouched\n"},
            "steps": [
                {"start": {}},
                send("review/hunks", path="$WS/other.txt"),
                state(),
                *reads("$P1"),
                revert({"kind": "scope", "owner": "$O1"}),
                {"files": ["other.txt"]},
                state(),
            ],
        },
        {
            "name": "paths/outside-the-workspace",
            "backend": [
                {"toolCalls": [write("$ROOT/outside/far.txt", "far away\n", "call_w1")]},
                {"text": "Turn one done."},
            ],
            "steps": [
                {"start": {}},
                {"turn": "Write outside the workspace"},
                state(),
                *reads("$P1"),
                revert({"kind": "all"}),
                state(),
            ],
        },
        {
            "name": "paths/through-a-symlink",
            "backend": [
                {"toolCalls": [edit("link.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
            ],
            "files": {"real.txt": NOTES},
            "steps": [
                {"symlink": {"link.txt": "real.txt"}},
                {"start": {}},
                {"turn": "Edit through the link"},
                state(),
                send("review/baseline", path="$WS/link.txt"),
                send("review/hunks", path="$WS/link.txt"),
                *reads("$P1"),
                revert({"kind": "all"}),
                {"files": ["real.txt"]},
                state(),
            ],
        },
        {
            "name": "encoding/hand-edits",
            "backend": [
                {"toolCalls": [edit("notes.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
            ],
            "files": FILES,
            "steps": [
                {"start": {}},
                {"turn": "Edit the first line"},
                # CRLF line endings.
                {"writeBytes": {"notes.txt": "414c5048410d0a626574610d0a67616d6d610d0a"}},
                state(),
                *reads("$P1"),
                # A byte-order mark, then Latin-1, then UTF-16.
                {"writeBytes": {"notes.txt": "efbbbf414c5048410a626574610a63616674c3a90a"}},
                state(),
                send("review/turnDiff", path="$P1", owner="$O3"),
                {"writeBytes": {"notes.txt": "414c5048410a6361667465e90a"}},
                state(),
                send("review/turnDiff", path="$P1", owner="$O4"),
                {"writeBytes": {"notes.txt": "fffe41004c00500048004100"}},
                state(),
                send("review/turnDiff", path="$P1", owner="$O5"),
                send("review/baseline", path="$P1"),
                revert({"kind": "scope", "owner": "$O3"}),
                {"files": ["notes.txt"]},
                state(),
            ],
        },
        # -- While a turn runs ------------------------------------------------
        {
            "name": "turn/reads-and-decisions-while-it-runs",
            "backend": [
                {"toolCalls": [edit("notes.txt", "alpha", "ALPHA", "call_e1")]},
                {"text": "Turn one done."},
                {"toolCalls": [
                    edit("notes.txt", "gamma", "GAMMA", "call_e2"),
                    write("created.txt", "made mid-turn\n", "call_w1"),
                ]},
                # Long enough for every read below to land while it waits.
                {"text": "Turn two done.", "delay": 15.0},
            ],
            "files": FILES,
            "steps": [
                {"start": {}},
                {"turn": "Edit the first line"},
                state(),
                {"turn": "Edit the last line slowly", "background": True, "settle": 1.5},
                state(),
                *reads("$P1"),
                send("review/baseline", path="$WS/created.txt"),
                send("review/turnDiff", path="$WS/created.txt", owner="$O2"),
                approve({"kind": "all"}),
                revert({"kind": "bogus"}),
                {"await": True},
                state(),
            ],
        },
        # -- Refusals -----------------------------------------------------------
        edited(
            "refuse/read-parameters",
            raw("review/state", {}),
            send("review/state", "no-such-session"),
            send("review/state", ""),
            send("review/state", extra=1),
            send("review/baseline"),
            send("review/baseline", path=42),
            send("review/turnDiff", path="$WS/notes.txt"),
            send("review/turnDiff", path="$WS/notes.txt", owner={"kind": "agent", "turnId": -1}),
            send("review/turnDiff", path="$WS/notes.txt", owner={"kind": "agent", "turnId": "1"}),
            send("review/turnDiff", path="$WS/notes.txt", owner={"kind": "robot"}),
            send("review/turnDiff", path="$WS/notes.txt", owner={"kind": "manual", "index": 1, "x": 1}),
            send("review/turnDiff", path="$WS/notes.txt", owner="agent"),
            send("review/hunks", path="$WS/notes.txt", owner=None),
            send("review/hunks", path="$WS/notes.txt", owner={"kind": "manual", "index": 0}),
        ),
        edited(
            "refuse/decision-parameters",
            send("review/approve"),
            approve({"kind": "bogus"}),
            approve({"kind": "all", "extra": 1}),
            approve({"kind": "region", "path": "$WS/notes.txt", "versionIndex": 999, "ordinal": 0}),
            approve({"kind": "region", "path": "$WS/notes.txt", "versionIndex": -1, "ordinal": 0}),
            revert({"kind": "region", "path": "$WS/notes.txt", "versionIndex": "1", "ordinal": 0}),
            revert({"kind": "regions", "path": "$WS/notes.txt", "regions": [{"versionIndex": -1, "ordinal": 0}]}),
            revert({"kind": "regions", "path": "$WS/notes.txt", "regions": []}),
            approve({"kind": "lastTurns", "count": "2"}),
            approve({"kind": "lastTurns", "count": 10**30}),
            approve({"kind": "lastTurns"}),
            revert({"kind": "file", "path": "$WS/unknown.txt"}),
            revert({"kind": "scope", "owner": {"kind": "agent", "turnId": 999}}),
            revert({"kind": "scopeFile", "owner": {"kind": "manual", "index": 7}, "path": "$WS/notes.txt"}),
            send("review/revert", target={"kind": "all"}, extra=True),
            {"files": WATCHED},
            state(),
        ),
        # -- Across a compaction and a rewind --------------------------------
        compacted(
            "compaction/reads-across-the-boundary",
            *reads("$P1"),
            *reads("$P2"),
        ),
        compacted(
            "compaction/decisions-across-the-boundary",
            revert({"kind": "lastTurns", "count": 1}),
            {"files": WATCHED},
            state(),
            approve({"kind": "scope", "owner": "$O1"}),
            {"files": WATCHED},
            state(),
            send("review/baseline", path="$WS/notes.txt"),
        ),
        edited(
            "rewind/in-place",
            rewind.rewind("$U2", inplace=True),
            state(),
            *reads("$P2"),
            revert({"kind": "all"}),
            {"files": WATCHED},
        ),
        edited(
            "rewind/fork-restoring-files",
            rewind.rewind("$U2", restoreFiles=True),
            {"files": WATCHED},
            state("$S2"),
            state("$S1"),
            revert({"kind": "all"}, session="$S2"),
            {"files": WATCHED},
            state("$S2"),
        ),
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
    parser.add_argument("--quiet", type=float, default=rewind.QUIET_SECONDS)
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
            dialect = arguments.dialect or "port"
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
            dialect = arguments.dialect or "reference"
        selected = [
            scenario
            for scenario in scenarios()
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]
        names = [scenario["name"] for scenario in scenarios()]
        if len(names) != len(set(names)):
            raise OracleError("two scenarios share a name, so a divergence could not name one")

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started = time.monotonic()
            run = rewind.run_scenario(
                scenario, command, dialect, arguments.quiet, session_factory=ReviewSession
            )
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
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except (OracleError, acp.OracleError, rewind.OracleError) as error:
        print(f"review oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
