#!/usr/bin/env python3
"""Black-box capture of the saved-session surface served by the app server.

Row 16 of ``docs/parity.md`` is what a client reaches about sessions beyond
the turn itself: the catalog (``session/list``), the history and turn pages
(``session/history/list``, ``session/history/get``, ``session/turns/list``),
reopening one (``session/resume``, ``session/continue``, ``session/fork``),
renaming, pinning, relocating and deleting one, and what all of that leaves on
disk under ``session_logging.save_dir``: the session directories, their
``meta.json`` and ``messages.jsonl``, the listing index, the last-session
pointers and the session leases.

The driver is the rewind oracle's (``rewind.py``): a fresh home per scenario,
the scripted chat-completions stand-in, and the same abstract ``start`` and
``turn`` steps rendered in the dialect of the server being driven. It adds
what a saved session needs to be observed as saved: a ``restart`` that stops
the server and opens a new one over the same home, a second server running
beside the first (``"on": 2``), a ``seed`` that writes a session directory the
way the reference lays one out, so both implementations read the same bytes,
and a ``store`` step recording the session directory tree. Nothing is
imported from the reference; its own ``vibe-app-server`` is the oracle, which
is what lets ``crates/vibe-app-server/tests/session_parity_tests.rs`` replay
the same scenarios against this port.

A client names what an earlier answer told it, so a scenario can too: ``$S1``
is the first session a state answered with, ``$U1`` the first user entry, and
``$C1`` the first page cursor an answer carried.

Normalization is the rewind oracle's, plus the two opaque shapes this surface
mints: a page cursor is replaced by the identity it encodes, and a session
directory name keeps its layout with its timestamp and short identifier
replaced by placeholders.

Usage::

    python3 scripts/parity/sessions.py                  # capture the reference
    python3 scripts/parity/sessions.py --check          # recapture and compare
    python3 scripts/parity/sessions.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import copy
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
import rewind  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/session-parity/corpus.json"
SCHEMA_VERSION = 1

#: Keys whose string values are opaque page cursors.
CURSOR_KEYS = {"cursor", "nextCursor", "previousCursor", "historyBeforeCursor"}
#: The directory name a session is saved under: prefix, timestamp, short id.
SESSION_DIRECTORY = re.compile(r"^(?P<prefix>[A-Za-z0-9-]+)_\d{8}_\d{6}_(?P<short>[A-Za-z0-9_-]+)$")
#: `meta.json` keys whose values are wall-clock readings.
META_TIME_KEYS = {"start_time", "end_time", "bumped_at", "pinned_at", "acquired_at", "created_at"}
#: `meta.json` keys whose values are identities.
META_ID_KEYS = {"session_id", "parent_session_id"}
#: `meta.json` keys whose values another row owns (the statistics, the tool
#: surface, the prompt, the configuration snapshot and the rollout state); only
#: their presence and type are this row's.
META_OPAQUE_KEYS = {
    "last_message_fingerprint",
    "stats",
    "tools_available",
    "system_prompt",
    "config",
    "agent_profile",
    "experiments",
    "git_commit",
    "git_branch",
    "username",
}
TOKEN = re.compile(r"\$C(\d+)")

TURN_BACKEND = [{"text": f"Answer {index}."} for index in range(1, 13)]


class OracleError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# What a client learns from the answers
# --------------------------------------------------------------------------


class Sessions(rewind.Session):
    """The rewind oracle's session, also learning page cursors and forks."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.cursors: list[str] = []
        self.turns: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        if message.get("method") == "turn/started":
            turn_id = ((message.get("params") or {}).get("turn") or {}).get("id")
            if isinstance(turn_id, str) and turn_id not in self.turns:
                self.turns.append(turn_id)
        result = message.get("result")
        if not isinstance(result, dict):
            return
        for key in ("nextCursor", "previousCursor"):
            cursor = result.get(key)
            if isinstance(cursor, str) and cursor not in self.cursors:
                self.cursors.append(cursor)

    def substitute(self, value: Any) -> Any:
        if isinstance(value, str) and (match := TOKEN.fullmatch(value)):
            index = int(match.group(1)) - 1
            return self.cursors[index] if index < len(self.cursors) else value
        value = super().substitute(value)
        if isinstance(value, str):
            for index in range(len(self.turns), 0, -1):
                value = value.replace(f"$T{index}", self.turns[index - 1])
        return value


# --------------------------------------------------------------------------
# The driver
# --------------------------------------------------------------------------


def environment(world: acp.World, backend: acp.Backend) -> dict[str, str]:
    return {
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


class Connection:
    """One server process and the request numbering of its connection."""

    def __init__(
        self,
        command: list[str],
        env: dict[str, str],
        world: acp.World,
        quiet: float,
        client: dict[str, Any] | None = None,
    ) -> None:
        self.server = rewind.Server(command, env, world.workspace, {}, world)
        self.identifier = 1
        self.quiet = quiet
        client_info = {"name": "session-oracle", "version": "0", **(client or {})}
        self.server.send({"jsonrpc": "2.0", "id": self.identifier, "method": "initialize",
                          "params": {"clientInfo": client_info}})
        self.server.collect(self.identifier, quiet)
        self.server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    def next_id(self) -> int:
        self.identifier += 1
        return self.identifier

    def stop(self) -> None:
        self.server.stop()


def run_scenario(scenario: dict[str, Any], command: list[str], dialect: str, quiet: float) -> dict[str, Any]:
    backend = rewind.RecordingBackend()
    root = Path(tempfile.mkdtemp(prefix="vibe-session-oracle-"))
    session = Sessions(root)
    world = session.world
    connections: dict[int, Connection] = {}
    try:
        backend.responses = session.substitute(copy.deepcopy(scenario.get("backend", TURN_BACKEND)))
        config = session.substitute(scenario.get("config", ""))
        (world.vibe_home / "config.toml").write_text(
            rewind.base_config(backend, config), encoding="utf-8"
        )
        acp.write_tree(world.workspace, scenario.get("files", {}))
        env = environment(world, backend)
        steps: list[dict[str, Any]] = []
        try:
            connections[1] = Connection(command, env, world, quiet, scenario.get("client"))
            run_steps(scenario, connections, command, env, session, steps, dialect, quiet)
        except (OracleError, rewind.OracleError, acp.OracleError) as error:
            steps.append({"failure": str(error)})
        finally:
            for connection in connections.values():
                connection.stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
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


def run_steps(
    scenario: dict[str, Any],
    connections: dict[int, Connection],
    command: list[str],
    env: dict[str, str],
    session: Sessions,
    steps: list[dict[str, Any]],
    dialect: str,
    quiet: float,
) -> None:
    world = session.world
    for step in scenario["steps"]:
        on = step.get("on", 1)
        if "restart" in step:
            connections.pop(on).stop()
            connections[on] = Connection(command, env, world, quiet, scenario.get("client"))
            continue
        if "open" in step:
            connections[on] = Connection(command, env, world, quiet, scenario.get("client"))
            continue
        if "close" in step:
            connections.pop(on).stop()
            continue
        if "seed" in step:
            seed(world, step["seed"])
            continue
        if "mkdir" in step:
            (world.root / step["mkdir"]).mkdir(parents=True, exist_ok=True)
            continue
        if "run" in step:
            # A command the scenario needs run in its workspace, such as the
            # git setup a worktree counterpart needs.
            subprocess.run(
                session.substitute(step["run"]), cwd=world.workspace, env=env,
                capture_output=True, check=True,
            )
            continue
        if "sleep" in step:
            time.sleep(step["sleep"])
            continue
        if "store" in step:
            steps.append({"store": store(world, step["store"])})
            continue
        connection = connections[on]
        identifier = connection.next_id()
        server = connection.server
        if "start" in step:
            params = rewind.render_start(dialect, world.workspace, step["start"].get("agent", "auto-approve"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start", "params": params})
            for message in server.collect(identifier, quiet):
                session.learn(message)
            continue
        if "turn" in step:
            target = session.substitute(step.get("session", "$S1"))
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "turn/start",
                         "params": rewind.render_turn(dialect, target, step["turn"])})
            for message in server.wait_for(rewind.settles({})):
                session.learn(message)
            for message in server.collect(None, quiet):
                session.learn(message)
            continue
        request = session.substitute(step["send"])
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            session.learn(message)
        response = next(
            (m for m in observed if "method" not in m and m.get("id") == identifier), None
        )
        notifications = [m.get("method") for m in observed if "method" in m and "id" not in m]
        steps.append({
            "method": request["method"],
            "response": {k: v for k, v in (response or {}).items() if k not in {"jsonrpc", "id"}},
            "notifications": notifications,
        })


def save_dir(world: acp.World, relative: str | None) -> Path:
    """The save directory a step names relative to the scenario root, or the
    one the shipped configuration resolves."""
    return world.root / relative if relative else world.vibe_home / "logs/session"


def seed(world: acp.World, spec: dict[str, Any]) -> None:
    """Writes one session the way the reference lays it out on disk."""
    directory = save_dir(world, spec.get("saveDir")) / spec["directory"]
    directory.mkdir(parents=True, exist_ok=True)
    metadata = json.loads(
        json.dumps(spec["meta"])
        .replace("$WS", str(world.workspace))
        .replace("$ROOT", str(world.root))
    )
    (directory / "meta.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    (directory / "messages.jsonl").write_text(
        "".join(json.dumps(message) + "\n" for message in spec.get("messages", [])),
        encoding="utf-8",
    )


def store(world: acp.World, spec: Any) -> dict[str, Any]:
    """What the session store holds: every path under the save directory, each
    session's metadata and the roles its log records."""
    relative = spec if isinstance(spec, str) else None
    root = save_dir(world, relative)
    if not root.exists():
        return {"exists": False}
    tree: list[str] = []
    sessions: dict[str, Any] = {}
    for path in sorted(root.rglob("*")):
        name = path.relative_to(root).as_posix()
        tree.append(name + ("/" if path.is_dir() else ""))
        if path.is_dir() and (path / "meta.json").is_file():
            try:
                metadata = json.loads((path / "meta.json").read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                metadata = {"<unreadable>": True}
            roles: list[Any] = []
            messages = path / "messages.jsonl"
            if messages.is_file():
                for line in messages.read_text(encoding="utf-8").splitlines():
                    try:
                        roles.append(json.loads(line).get("role"))
                    except json.JSONDecodeError:
                        roles.append("<unparsed>")
            sessions[name] = {"meta": metadata, "roles": roles}
    index = root / ".session_index.json"
    indexed: Any = None
    if index.is_file():
        try:
            indexed = json.loads(index.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            indexed = "<unparsed>"
    diagnostics: dict[str, Any] = {}
    active = root / "active"
    if active.is_dir():
        for path in sorted(active.glob("*.lock.json")):
            try:
                diagnostics[path.name] = json.loads(path.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                diagnostics[path.name] = "<torn>"
    return {"exists": True, "tree": tree, "sessions": sessions, "index": indexed, "leases": diagnostics}


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


class Normalizer(rewind.Normalizer):
    def cursor(self, value: str) -> str:
        padded = value + "=" * (-len(value) % 4)
        try:
            decoded = base64.urlsafe_b64decode(padded).decode()
        except (ValueError, UnicodeDecodeError):
            decoded = ""
        if "\0" in decoded:
            _, session_id = decoded.split("\0", 1)
            return f"<cursor:{self.identity(session_id)}>"
        return f"<cursor:{self.identity(value)}>"

    def value(self, value: Any, key: str | None = None) -> Any:
        if key in CURSOR_KEYS and isinstance(value, str):
            return self.cursor(value)
        return super().value(value, key)

    def directory(self, name: str) -> str:
        """A path under the save directory, with the parts a run mints replaced."""
        trailing = "/" if name.endswith("/") else ""
        parts = name.rstrip("/").split("/")
        match = SESSION_DIRECTORY.match(parts[0])
        if match is not None:
            short = match.group("short")
            known = next((identity for identity in self.ids if identity.startswith(short)), None)
            label = self.identity(known) if known is not None else f"<short:{len(short)}>"
            parts[0] = f"{match.group('prefix')}_<time>_{label}"
        elif parts[0] == "active" and len(parts) > 1:
            stem, dot, suffix = parts[1].partition(".")
            if stem:
                parts[1] = f"{self.identity(stem)}{dot}{suffix}"
        elif parts[0] == ".last_session" and len(parts) > 1:
            parts[1] = "<tty>"
        return "/".join(parts) + trailing

    def meta(self, metadata: Any) -> Any:
        if not isinstance(metadata, dict):
            return metadata
        rendered: dict[str, Any] = {}
        for key, item in metadata.items():
            if key in META_TIME_KEYS:
                rendered[key] = "<time>" if item is not None else None
            elif key in META_ID_KEYS:
                rendered[key] = self.identity(item) if isinstance(item, str) and item else item
            elif key in META_OPAQUE_KEYS:
                rendered[key] = type(item).__name__
            elif key == "created_worktree" and isinstance(item, dict):
                rendered[key] = sorted(item)
            else:
                rendered[key] = self.value(copy.deepcopy(item), key)
        return {"keys": list(metadata), "values": rendered}

    def store(self, observed: dict[str, Any]) -> dict[str, Any]:
        if not observed.get("exists"):
            return observed
        index = observed.get("index")
        if isinstance(index, dict):
            index = {
                self.directory(name): sorted(entry) if isinstance(entry, dict) else entry
                for name, entry in index.items()
            }
        leases = {
            self.directory(f"active/{name}"): {
                "keys": sorted(diagnostic),
                "session_id": self.identity(diagnostic.get("session_id", "")),
                "lease_version": diagnostic.get("lease_version"),
            } if isinstance(diagnostic, dict) else diagnostic
            for name, diagnostic in observed.get("leases", {}).items()
        }
        return {
            "exists": True,
            "tree": sorted({self.directory(name) for name in observed["tree"]}),
            "sessions": {
                self.directory(name): {"meta": self.meta(entry["meta"]), "roles": entry["roles"]}
                for name, entry in observed["sessions"].items()
            },
            "index": index,
            "leases": leases,
        }


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> list[Any]:
    normalizer = Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    for step in run["steps"]:
        normalizer.collect_ids(step.get("response"))
    observed: list[Any] = []
    for step in run["steps"]:
        if "failure" in step:
            observed.append({"failure": normalizer.text(step["failure"])})
            continue
        if "store" in step:
            observed.append({"store": normalizer.store(step["store"])})
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


def send(method: str, **params: Any) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


def on(server: int, step: dict[str, Any]) -> dict[str, Any]:
    return {**step, "on": server}


START = {"start": {}}
RESTART = {"restart": True}
STORE = {"store": True}


def turn(text: str, session: str = "$S1") -> dict[str, Any]:
    return {"turn": text, "session": session}


GIT = ["git", "-c", "user.name=Oracle", "-c", "user.email=oracle@example.invalid"]

#: A session laid out the way the reference writes one, with two exchanges.
SEEDED_META = {
    "session_id": "5eed0000-0000-4000-8000-00000000abcd",
    "parent_session_id": None,
    "start_time": "2026-01-02T03:04:05.000000+00:00",
    "end_time": "2026-01-02T03:05:06.000000+00:00",
    "git_commit": None,
    "git_branch": None,
    "environment": {"working_directory": "$WS"},
    "origin_directory": "$WS",
    "username": "oracle",
    "child_sessions": [],
    "loops": [],
    "title": "Seeded session",
    "title_source": "manual",
    "bumped_at": "2026-01-02T03:04:05.500000+00:00",
    "pinned_at": None,
    "experiments": None,
    "config": {"active_model": "mistral-medium-3.5"},
    "import_provenance": None,
    "created_worktree": None,
    "stats": {},
    "total_messages": 4,
    "last_message_fingerprint": None,
    "tools_available": [],
    "agent_profile": {"name": "default", "overrides": {}},
    "system_prompt": None,
}
SEEDED_MESSAGES = [
    {"role": "user", "content": "Seeded question", "message_id": "5eed0000-0000-4000-8000-000000000001"},
    {"role": "assistant", "content": "Seeded answer", "message_id": "5eed0000-0000-4000-8000-000000000002"},
    {"role": "user", "content": "Seeded follow-up", "message_id": "5eed0000-0000-4000-8000-000000000003"},
    {"role": "assistant", "content": "Seeded closing", "message_id": "5eed0000-0000-4000-8000-000000000004"},
]
SEEDED = {
    "directory": "session_20260102_030405_5eed0000",
    "meta": SEEDED_META,
    "messages": SEEDED_MESSAGES,
}


def three_sessions() -> list[dict[str, Any]]:
    return [
        START, turn("Question one"),
        RESTART, START, turn("Question two", "$S2"),
        RESTART, START, turn("Question three", "$S3"),
    ]


def scenarios() -> list[dict[str, Any]]:
    return [
        # ------------------------------------------------------------ listing
        {
            "name": "list/before-any-session",
            "steps": [send("session/list"), STORE, START, send("session/list"), STORE],
        },
        {
            "name": "list/after-a-turn",
            "steps": [
                START,
                turn("First question"),
                send("session/list"),
                send("session/list", cwd="$WS"),
                send("session/list", cwd="$ROOT"),
                send("session/list", cwds=[]),
                send("session/list", cwds=["$ROOT", "$WS"]),
                send("session/list", pinned=True),
                send("session/list", pinned=False),
                STORE,
            ],
        },
        {
            "name": "list/pagination",
            "steps": [
                *three_sessions(),
                send("session/list", limit=1),
                send("session/list", limit=1, cursor="$C1"),
                send("session/list", limit=1, cursor="$C2"),
                send("session/list", limit=2),
                send("session/list", cursor="not-a-cursor"),
                send("session/list", cursor="bm90aGluZw"),
                STORE,
            ],
        },
        {
            "name": "list/refusals",
            "steps": [
                send("session/list", limit=0),
                send("session/list", limit=501),
                send("session/list", limit="3"),
                send("session/list", offset=1),
                send("session/list", cwd=7),
            ],
        },
        {
            "name": "list/lineage",
            "steps": [
                START,
                turn("Parent question"),
                send("session/fork", sourceSessionId="$S1"),
                turn("Child question", "$S2"),
                send("session/list"),
                send("session/list", parentSessionId="$S1"),
                send("session/list", rootSessionId="$S1"),
                send("session/list", rootSessionId="$S2"),
                STORE,
            ],
        },
        # ------------------------------------------------------ pin and rename
        {
            "name": "pin/is-not-served-by-the-legacy-backend",
            "steps": [
                send("session/pin", sessionId="missing", pinned=True),
                START,
                turn("A question"),
                send("session/pin", sessionId="$S1", pinned=True),
                send("session/pin", sessionId="$S1", pinned=False),
                send("session/list", pinned=True),
            ],
        },
        {
            "name": "rename/attached",
            "steps": [
                START,
                send("session/rename", sessionId="$S1", title="Before any turn"),
                turn("A question"),
                send("session/rename", sessionId="$S1", title="  Named session  "),
                send("session/title/update", sessionId="$S1", title="Renamed again"),
                send("session/rename", sessionId="$S1", title="   "),
                send("session/rename", sessionId="missing", title="Nobody"),
                send("session/list"),
                STORE,
            ],
        },
        {
            "name": "rename/saved",
            "steps": [
                START,
                turn("A question"),
                RESTART,
                send("session/rename", sessionId="$S1", title="Saved title"),
                send("session/title/update", sessionId="$S1", title="Saved again"),
                send("session/rename", sessionId="$S1", title=""),
                send("session/rename", sessionId="missing", title="Nobody"),
                send("session/list"),
                STORE,
            ],
        },
        # --------------------------------------------------- history and turns
        {
            "name": "history/attached",
            "steps": [
                START,
                turn("Question one"),
                turn("Question two"),
                turn("Question three"),
                send("session/history/get", sessionId="$S1"),
                send("session/history/get", sessionId="$S1", historyLimit=2),
                send("session/history/get", sessionId="$S1", historyLimit=0),
                send("session/history/list", sessionId="$S1"),
                send("session/history/list", sessionId="$S1", page={"limit": 2}),
                send("session/history/list", sessionId="$S1", page={"limit": 2, "cursor": "$C1"}),
                send("session/history/list", sessionId="$S1", page={"limit": 2, "direction": "forward"}),
                send("session/history/list", sessionId="$S1", page={"limit": 2, "direction": "forward", "cursor": "$C3"}),
                send("session/history/list", sessionId="$S1", turnId="$T2"),
                send("session/history/list", sessionId="$S1", page={"limit": 501}),
                send("session/history/list", sessionId="missing"),
                send("session/turns/list", sessionId="$S1"),
                send("session/turns/list", sessionId="$S1", page={"limit": 1}),
                send("session/turns/list", sessionId="$S1", page={"limit": 1, "direction": "forward"}),
                send("session/turns/list", sessionId="$S1", page={"limit": 2, "cursor": "$T3"}),
                send("session/turns/list", sessionId="$S1", page={"limit": 2, "cursor": "$T1", "direction": "forward"}),
                send("session/turns/list", sessionId="missing"),
            ],
        },
        {
            "name": "history/saved",
            "steps": [
                START,
                turn("Question one"),
                turn("Question two"),
                RESTART,
                send("session/history/get", sessionId="$S1"),
                send("session/history/get", sessionId="$S1", historyLimit=1),
                send("session/history/list", sessionId="$S1"),
                send("session/history/list", sessionId="$S1", page={"limit": 1}),
                send("session/history/list", sessionId="$S1", page={"limit": 1, "cursor": "$C1"}),
                send("session/history/get", sessionId="missing"),
                send("session/history/list", sessionId="missing"),
                send("session/turns/list", sessionId="$S1"),
                send("session/read", sessionId="$S1"),
            ],
        },
        {
            "name": "history/seeded",
            "steps": [
                {"seed": SEEDED},
                send("session/list"),
                send("session/history/get", sessionId="5eed0000-0000-4000-8000-00000000abcd"),
                send("session/history/list", sessionId="5eed0000-0000-4000-8000-00000000abcd", page={"limit": 1}),
                send("session/resume", sessionId="5eed0000-0000-4000-8000-00000000abcd"),
                turn("A new question", "5eed0000-0000-4000-8000-00000000abcd"),
                send("session/list"),
                STORE,
            ],
        },
        # --------------------------------------------------- reopening sessions
        {
            "name": "resume/after-a-restart",
            "steps": [
                START,
                turn("First question"),
                RESTART,
                send("session/resume", sessionId="$S1"),
                turn("Second question"),
                send("session/list"),
                STORE,
            ],
        },
        {
            "name": "resume/refusals",
            "steps": [
                send("session/resume", sessionId="missing"),
                START,
                turn("First question"),
                send("session/resume", sessionId="missing"),
                send("session/resume"),
                send("session/resume", sessionId="$S1", unknown=True),
            ],
        },
        {
            "name": "continue/after-a-restart",
            "steps": [
                START,
                turn("First question"),
                RESTART,
                send("session/continue"),
                turn("Second question"),
                STORE,
            ],
        },
        {
            "name": "continue/refusals",
            "steps": [
                send("session/continue"),
                {"mkdir": "elsewhere"},
                send("session/continue", agentConfig={"cwd": "$ROOT/elsewhere"}),
                START,
                send("session/continue"),
            ],
        },
        {
            "name": "fork/attached-and-detached",
            "steps": [
                START,
                turn("Question one"),
                turn("Question two"),
                send("session/fork", sourceSessionId="$S1", entryId="$U2", attach=False),
                send("session/fork", sourceSessionId="$S1", entryId="missing"),
                send("session/fork", sourceSessionId="missing"),
                send("session/fork", sourceSessionId="$S1"),
                send("session/list"),
                STORE,
            ],
        },
        # ------------------------------------------------------------ leases
        {
            "name": "lease/second-server",
            "steps": [
                START,
                turn("First question"),
                {"open": True, "on": 2},
                on(2, send("session/resume", sessionId="$S1")),
                on(2, send("session/list")),
                STORE,
                {"close": True},
                on(2, send("session/resume", sessionId="$S1")),
                STORE,
            ],
        },
        # ------------------------------------------------------ moving sessions
        {
            "name": "relocate/refusals",
            "steps": [
                START,
                turn("A question"),
                {"mkdir": "elsewhere"},
                send("session/relocate", sessionId="$S1", cwd="$ROOT/nowhere"),
                send("session/relocate", sessionId="$S1", cwd="$ROOT/elsewhere"),
                send("session/relocate", sessionId="missing", cwd="$ROOT/elsewhere"),
                RESTART,
                send("session/relocate", sessionId="$S1", cwd="$ROOT/nowhere"),
                send("session/relocate", sessionId="$S1", cwd="$ROOT/elsewhere"),
                send("session/relocate", sessionId="missing", cwd="$ROOT/elsewhere"),
            ],
        },
        {
            "name": "relocate/to-a-worktree",
            "files": {"notes.txt": "tracked\n"},
            "steps": [
                {"run": [*GIT, "init", "-q", "-b", "main"]},
                {"run": [*GIT, "add", "."]},
                {"run": [*GIT, "commit", "-q", "-m", "initial"]},
                {"run": [*GIT, "worktree", "add", "-q", "$ROOT/checkout", "-b", "side"]},
                START,
                turn("A question"),
                RESTART,
                send("session/relocate", sessionId="$S1", cwd="$ROOT/checkout"),
                send("session/list", cwd="$WS"),
                send("session/list", cwd="$ROOT/checkout"),
                STORE,
                send("session/relocate", sessionId="$S1", cwd="$WS"),
                STORE,
            ],
        },
        # ------------------------------------------------------ other methods
        {
            "name": "delete/saved",
            "steps": [
                START,
                turn("A question"),
                RESTART,
                send("session/delete", sessionId="$S1"),
                send("session/list"),
                send("session/delete", sessionId="$S1"),
                STORE,
            ],
        },
        {
            "name": "log/read",
            "steps": [
                send("session/log/read", sessionId="missing"),
                START,
                send("session/log/read", sessionId="$S1"),
                turn("A question"),
                send("session/log/read", sessionId="$S1"),
            ],
        },
        {
            "name": "history/clear",
            "steps": [
                START,
                turn("A question"),
                send("session/history/clear", sessionId="$S1"),
                send("session/list"),
                STORE,
            ],
        },
        # --------------------------------------------------- the configuration
        {
            "name": "logging/disabled",
            "config": "\n[session_logging]\nenabled = false\n",
            "steps": [START, turn("A question"), send("session/list"), STORE, RESTART, send("session/continue")],
        },
        {
            "name": "logging/elsewhere",
            "config": '\n[session_logging]\nsave_dir = "$ROOT/elsewhere"\nsession_prefix = "chat"\n',
            "steps": [
                START,
                turn("A question"),
                send("session/list"),
                {"store": "elsewhere"},
                RESTART,
                send("session/continue"),
                send("session/resume", sessionId="$S1"),
            ],
        },
        {
            "name": "titles/generated",
            "client": {"entrypoint": "cli"},
            "config": "\n[session_logging]\ngenerate_titles = true\n",
            "backend": [{"text": "Answer one."}, {"text": "Parser refactor plan"}, {"text": "Answer two."}, {"text": "Parser refactor"}],
            "steps": [
                START,
                turn("Plan a refactor of the parser"),
                {"sleep": 1.0},
                send("session/list"),
                STORE,
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
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except (OracleError, acp.OracleError, rewind.OracleError) as error:
        print(f"session oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
