#!/usr/bin/env python3
"""Black-box capture of workspace trust as the app server serves it.

Row 12 of ``docs/parity.md`` is trusted folders and permissions. The permission
half is measured by ``permission_surface.py``; this capture measures the trust
half. It serves the reference's own ``vibe-app-server`` over stdio in a fresh
home and drives the three ``workspace/trust/*`` methods, a session started with
and without ``trustWorkspace``, and ``config/read`` over project configuration
files the trust store has to allow, so what is recorded is the contract a
client meets: the trust answer and its details, the refusals, the
``runtime/updated`` a grant raises, and ``trusted_folders.toml`` itself, its
content and its mode, after every step.

The reference module behind all of it is ``vibe/core/trusted_folders.py``,
answered through ``vibe/app_server/_workspace.py``, ``vibe/app_server/_host.py``
and ``vibe/app_server/_handler.py``; the project configuration gate is
``vibe/core/config/layers/project.py``.

Every scenario builds its tree under a temporary root holding ``home`` (the
``HOME`` the server sees), ``vibe-home`` and ``workspace`` (the server's working
directory). Paths are written with ``$WS``, ``$HOME``, ``$VIBE_HOME`` and
``$ROOT`` and recorded with ``<ws>``, ``<home>``, ``<vibe-home>`` and ``<root>``,
and every string the server authored that holds whitespace is a length and a
SHA-256, which is what ``NOTICE`` requires of a committed corpus.

``crates/vibe-app-server/tests/workspace_trust_parity_tests.rs`` runs the same
scenarios against this port with ``--server`` and compares.

Usage::

    python3 scripts/parity/workspace_trust.py                 # capture the reference
    python3 scripts/parity/workspace_trust.py --check         # recapture and compare
    python3 scripts/parity/workspace_trust.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import stat
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/workspace-trust/corpus.json"
SCHEMA_VERSION = 1
QUIET_SECONDS = 0.5
RESPONSE_TIMEOUT = 30.0
TRUST_FILE = "trusted_folders.toml"

#: The project configuration a `config` step reads back. `worktree_limit` is a
#: plain integer both servers project on `config/read`, so whether the project
#: file was honored is one number.
PROJECT_CONFIG = "worktree_limit = 7\n"
PROJECT_KEY = "worktreeLimit"


class OracleError(RuntimeError):
    pass


def digest(text: str) -> dict[str, Any]:
    return {"prose": len(text), "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()}


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------


def send(method: str, **params: Any) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


def status(**params: Any) -> dict[str, Any]:
    return send("workspace/trust/status", **params)


def decide(decision: str, **params: Any) -> dict[str, Any]:
    return send("workspace/trust/decision", decision=decision, **params)


def untrusted(**params: Any) -> dict[str, Any]:
    return send("workspace/trust/untrustedConfig", **params)


def start(cwd: str = "$WS", trust: bool = False) -> dict[str, Any]:
    return {"start": {"cwd": cwd, "trustWorkspace": trust}}


def config(session: str = "$S1") -> dict[str, Any]:
    return {"config": session}


FILE = {"file": True}
GIT = {".git/HEAD": "ref: refs/heads/main\n"}


def trusted(*paths: str) -> str:
    return "trusted = [" + ", ".join(json.dumps(path) for path in paths) + "]\n"


def trust_file(trusted_paths: list[str], untrusted_paths: list[str]) -> str:
    return (
        "trusted = [" + ", ".join(json.dumps(p) for p in trusted_paths) + "]\n"
        "untrusted = [" + ", ".join(json.dumps(p) for p in untrusted_paths) + "]\n"
    )


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- Reading the trust of a directory --------------------------------
        {
            "name": "status/nothing-to-decide",
            "steps": [FILE, status(cwd="$WS"), FILE],
        },
        {
            "name": "status/agents-md",
            "tree": {"$WS/AGENTS.md": "# notes\n"},
            "steps": [status(cwd="$WS"), status(), FILE],
        },
        {
            "name": "status/config-directories",
            "tree": {
                "$WS/plugins-only/.vibe/plugins/.keep": "",
                "$WS/prompts-only/.vibe/prompts/.keep": "",
                "$WS/config-only/.vibe/config.toml": "",
                "$WS/tools-only/.vibe/tools/.keep": "",
                "$WS/empty-vibe/.vibe/.keep": "",
                "$WS/agents-without-skills/.agents/.keep": "",
                "$WS/agents-skills/.agents/skills/.keep": "",
                "$WS/both/.vibe/skills/.keep": "",
                "$WS/both/.agents/skills/.keep": "",
                "$WS/both/AGENTS.md": "",
            },
            "steps": [
                status(cwd="$WS/plugins-only"),
                status(cwd="$WS/prompts-only"),
                status(cwd="$WS/config-only"),
                status(cwd="$WS/tools-only"),
                status(cwd="$WS/empty-vibe"),
                status(cwd="$WS/agents-without-skills"),
                status(cwd="$WS/agents-skills"),
                status(cwd="$WS/both"),
            ],
        },
        {
            "name": "status/repository",
            "tree": {
                **{f"$WS/{path}": content for path, content in GIT.items()},
                "$WS/AGENTS.md": "",
                "$WS/.vibe/config.toml": "",
                "$WS/pkg/AGENTS.md": "",
                "$WS/pkg/sub/.keep": "",
                "$WS/pkg/sub/deeper/AGENTS.md": "",
            },
            "steps": [
                status(cwd="$WS/pkg/sub"),
                status(cwd="$WS/pkg/sub/deeper"),
                status(cwd="$WS"),
            ],
        },
        {
            "name": "status/repository-markers",
            "tree": {
                "$WS/linked/.git": "gitdir: /elsewhere/.git/worktrees/linked\n",
                "$WS/linked/sub/AGENTS.md": "",
                "$WS/headless/.git/.keep": "",
                "$WS/headless/sub/AGENTS.md": "",
            },
            "steps": [status(cwd="$WS/linked/sub"), status(cwd="$WS/headless/sub")],
        },
        {
            "name": "status/home-and-missing",
            "tree": {"$HOME/AGENTS.md": "", "$HOME/project/AGENTS.md": ""},
            "steps": [
                status(cwd="$HOME"),
                status(cwd="~/project"),
                status(cwd="$WS/missing"),
            ],
        },
        {
            "name": "status/closest-decision",
            "trust": trust_file(["$WS"], ["$WS/declined"]),
            "tree": {
                "$WS/AGENTS.md": "",
                "$WS/inner/AGENTS.md": "",
                "$WS/declined/AGENTS.md": "",
                "$WS/declined/child/AGENTS.md": "",
            },
            "steps": [
                status(cwd="$WS"),
                status(cwd="$WS/inner"),
                status(cwd="$WS/declined"),
                status(cwd="$WS/declined/child"),
            ],
        },
        {
            "name": "status/repository-untrust",
            "trust": trust_file([], ["$ROOT/outer", "$ROOT/declined"]),
            "tree": {
                "$ROOT/outer/repo/.git/HEAD": "ref: refs/heads/main\n",
                "$ROOT/outer/repo/sub/AGENTS.md": "",
                "$ROOT/declined/.git/HEAD": "ref: refs/heads/main\n",
                "$ROOT/declined/sub/AGENTS.md": "",
            },
            "steps": [
                status(cwd="$ROOT/outer/repo/sub"),
                status(cwd="$ROOT/declined/sub"),
            ],
        },
        {
            "name": "status/refusals",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [
                status(cwd="$WS", sessionId="missing"),
                status(cwd="$WS", unexpected=True),
                status(cwd=7),
            ],
        },
        # -- Deciding --------------------------------------------------------
        {
            "name": "decision/trust-cwd",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [decide("trust_cwd", cwd="$WS"), FILE, status(cwd="$WS")],
        },
        {
            "name": "decision/decline-then-trust",
            "tree": {"$WS/a/AGENTS.md": "", "$WS/b/AGENTS.md": ""},
            "steps": [
                decide("decline", cwd="$WS/a"),
                decide("decline", cwd="$WS/b"),
                FILE,
                decide("decline", cwd="$WS/a"),
                FILE,
                decide("trust_cwd", cwd="$WS/a"),
                FILE,
                status(cwd="$WS/a"),
            ],
        },
        {
            "name": "decision/trust-repository",
            "tree": {
                **{f"$WS/{path}": content for path, content in GIT.items()},
                "$WS/AGENTS.md": "",
                "$WS/sub/AGENTS.md": "",
            },
            "steps": [
                decide("trust_repo", cwd="$WS"),
                decide("trust_repo", cwd="$WS/sub"),
                FILE,
                status(cwd="$WS/sub"),
                status(cwd="$WS"),
            ],
        },
        {
            "name": "decision/refusals",
            "tree": {"$WS/AGENTS.md": "", "$WS/plain/.keep": ""},
            "steps": [
                decide("trust_cwd", cwd="$WS/plain"),
                decide("maybe", cwd="$WS"),
                decide("trust_session", cwd="$WS"),
                decide("trust_cwd", cwd="$WS", sessionId="missing"),
                send("workspace/trust/decision", cwd="$WS"),
                decide("trust_cwd", cwd="$WS", unexpected=True),
                FILE,
            ],
        },
        {
            "name": "decision/default-cwd",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [decide("decline"), FILE, status()],
        },
        {
            "name": "decision/unusual-path",
            "tree": {'$WS/q"u\\ote/AGENTS.md': ""},
            "steps": [decide("trust_cwd", cwd='$WS/q"u\\ote'), FILE],
        },
        # -- The trust file --------------------------------------------------
        {
            "name": "file/mode-is-kept",
            "trust": trust_file([], []),
            "trustMode": "0o644",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [FILE, decide("trust_cwd", cwd="$WS"), FILE],
        },
        {
            "name": "file/malformed-is-reset",
            "trust": "trusted = [\n",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [FILE, status(cwd="$WS"), FILE],
        },
        {
            "name": "file/unknown-keys",
            "trust": 'extra = true\ntrusted = ["$WS"]\n',
            "tree": {"$WS/AGENTS.md": "", "$ROOT/other/AGENTS.md": ""},
            "steps": [FILE, status(cwd="$WS"), decide("decline", cwd="$ROOT/other"), FILE],
        },
        {
            "name": "file/unwritable",
            "trustDirectory": True,
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [status(cwd="$WS"), decide("trust_cwd", cwd="$WS"), status(cwd="$WS")],
        },
        # -- Configuration the trust store has to allow ----------------------
        {
            "name": "untrusted-config/listing",
            "trust": trust_file(["$WS"], ["$WS/.vibe", "$WS/.agents", "$WS/declined"]),
            "tree": {
                "$WS/.vibe/config.toml": "",
                "$WS/.agents/skills/.keep": "",
                "$WS/declined/.vibe/config.toml": "",
                "$WS/inner/.vibe/config.toml": "",
            },
            "steps": [
                untrusted(cwd="$WS"),
                untrusted(),
                untrusted(cwd="$WS/declined"),
                untrusted(cwd="$WS/inner"),
                untrusted(cwd="$WS", sessionId="missing"),
                FILE,
            ],
        },
        {
            "name": "untrusted-config/content-and-session",
            "trust": trust_file(["$WS/bare"], ["$WS/bare/.vibe", "$ROOT/other/.vibe"]),
            "tree": {
                "$WS/bare/.vibe/.keep": "",
                "$ROOT/other/.vibe/config.toml": "",
            },
            "steps": [
                untrusted(cwd="$WS/bare"),
                untrusted(cwd="$ROOT/other"),
                start(cwd="$ROOT/other", trust=True),
                untrusted(cwd="$ROOT/other"),
            ],
        },
        # -- Sessions --------------------------------------------------------
        {
            "name": "session/workspace-trust",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [
                start(trust=True),
                status(cwd="$WS"),
                status(cwd="$WS/child"),
                FILE,
            ],
        },
        {
            "name": "session/untrusted",
            "tree": {"$WS/AGENTS.md": ""},
            "steps": [start(), status(cwd="$WS"), FILE],
        },
        {
            "name": "session/decision",
            "tree": {"$WS/AGENTS.md": "", "$WS/.vibe/config.toml": PROJECT_CONFIG},
            "steps": [
                start(),
                config(),
                decide("decline", sessionId="$S1"),
                decide("trust_cwd", sessionId="$S1", cwd="$WS/elsewhere"),
                decide("trust_cwd", sessionId="$S1", cwd="$WS"),
                config(),
                FILE,
            ],
        },
        {
            "name": "config/trusted-by-the-file",
            "trust": trust_file(["$WS"], []),
            "tree": {"$WS/.vibe/config.toml": PROJECT_CONFIG},
            "steps": [start(), config()],
        },
        {
            "name": "config/declined-config-directory",
            "trust": trust_file(["$WS"], ["$WS/.vibe"]),
            "tree": {"$WS/.vibe/config.toml": PROJECT_CONFIG},
            "steps": [start(), config()],
        },
        {
            "name": "config/parent-of-a-trusted-directory",
            "trust": trust_file(["$WS/sub"], []),
            "tree": {"$WS/.vibe/config.toml": PROJECT_CONFIG, "$WS/sub/.keep": ""},
            "steps": [start(cwd="$WS/sub"), config()],
        },
        {
            "name": "config/parent-of-a-session-trust",
            "tree": {"$WS/.vibe/config.toml": PROJECT_CONFIG, "$WS/sub/.keep": ""},
            "steps": [start(cwd="$WS/sub", trust=True), config()],
        },
        {
            "name": "config/trusted-parent",
            "trust": trust_file(["$WS"], []),
            "tree": {"$WS/.vibe/config.toml": PROJECT_CONFIG, "$WS/sub/.keep": ""},
            "steps": [start(cwd="$WS/sub"), config()],
        },
    ]


# --------------------------------------------------------------------------
# One scenario
# --------------------------------------------------------------------------


class Run:
    def __init__(self, scenario: dict[str, Any], command: list[str], dialect: str, quiet: float) -> None:
        self.scenario = scenario
        self.dialect = dialect
        self.quiet = quiet
        # Resolved, so the placeholders match what both servers resolve to.
        self.root = Path(tempfile.mkdtemp(prefix="vibe-trust-oracle-")).resolve()
        self.world = acp.World(self.root)
        self.backend = acp.Backend()
        self.sessions: list[str] = []
        try:
            for relative, content in scenario.get("tree", {}).items():
                path = Path(self.substitute(relative))
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
            (self.world.vibe_home / "config.toml").write_text(acp.base_config(self.backend), encoding="utf-8")
            if scenario.get("trustDirectory"):
                (self.trust_path / "occupied").mkdir(parents=True)
            if "trust" in scenario:
                self.trust_path.write_text(self.substitute(scenario["trust"]), encoding="utf-8")
                os.chmod(self.trust_path, int(scenario.get("trustMode", "0o600"), 8))
            env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(self.world.home),
                "VIBE_HOME": str(self.world.vibe_home),
                "MISTRAL_API_KEY": "oracle-key",
                "VIBE_API_BASE": f"{self.backend.base}/v1/chat/completions",
                "LANG": "C.UTF-8",
                "TERM": "dumb",
                "NO_COLOR": "1",
                "CI": "true",
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
            }
            self.server = acp.Agent(command, env, self.world.workspace, {}, self.world)
        except BaseException:
            self.close()
            raise

    @property
    def trust_path(self) -> Path:
        return self.world.vibe_home / TRUST_FILE

    def substitute(self, text: str) -> str:
        text = (
            text.replace("$WS", str(self.world.workspace))
            .replace("$VIBE_HOME", str(self.world.vibe_home))
            .replace("$HOME", str(self.world.home))
            .replace("$ROOT", str(self.root))
        )
        for index in range(len(self.sessions), 0, -1):
            text = text.replace(f"$S{index}", self.sessions[index - 1])
        return text

    def substitute_value(self, value: Any) -> Any:
        if isinstance(value, dict):
            return {key: self.substitute_value(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.substitute_value(item) for item in value]
        if isinstance(value, str):
            return self.substitute(value)
        return value

    def collect(self, request_id: Any) -> list[dict[str, Any]]:
        messages: list[dict[str, Any]] = []
        answered = request_id is None
        deadline = time.monotonic() + RESPONSE_TIMEOUT
        while True:
            timeout = self.quiet if answered else max(0.0, deadline - time.monotonic())
            try:
                message = self.server.inbox.get(timeout=timeout)
            except queue.Empty:
                if answered:
                    return messages
                raise OracleError(f"no response to request {request_id!r}") from None
            if message is None:
                if not answered:
                    raise OracleError(f"server exited before answering {request_id!r}")
                return messages
            messages.append(message)
            if not answered and "method" not in message and message.get("id") == request_id:
                answered = True

    def request(self, identifier: int, method: str, params: dict[str, Any]) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        self.server.send({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
        observed = self.collect(identifier)
        response = next((m for m in observed if "method" not in m and m.get("id") == identifier), {})
        return response, observed

    def learn(self, response: dict[str, Any]) -> None:
        result = response.get("result")
        if not isinstance(result, dict):
            return
        state = result.get("state")
        session = state.get("session") if isinstance(state, dict) else None
        identifier = session.get("id") if isinstance(session, dict) else result.get("sessionId")
        if isinstance(identifier, str) and identifier not in self.sessions:
            self.sessions.append(identifier)

    def file_state(self) -> dict[str, Any]:
        path = self.trust_path
        if not path.exists():
            return {"exists": False}
        if path.is_dir():
            return {"exists": True, "directory": True}
        return {
            "exists": True,
            "mode": oct(stat.S_IMODE(path.stat().st_mode)),
            "content": path.read_text(encoding="utf-8"),
        }

    def run(self) -> list[dict[str, Any]]:
        steps: list[dict[str, Any]] = []
        identifier = 1
        self.request(identifier, "initialize", {"clientInfo": {"name": "workspace-trust-oracle", "version": "0"}})
        self.server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        try:
            for step in self.scenario["steps"]:
                identifier += 1
                if "file" in step:
                    steps.append({"file": self.file_state()})
                    continue
                if "start" in step:
                    options = self.substitute_value(step["start"])
                    params = (
                        {"agentConfig": {**options, "agent": "auto-approve"}}
                        if self.dialect == "reference"
                        else {**options, "agent": "auto-approve"}
                    )
                    response, _ = self.request(identifier, "session/start", params)
                    self.learn(response)
                    started: dict[str, Any] = {"ok": "result" in response}
                    if "error" in response:
                        started["error"] = response["error"]
                    steps.append({"start": started})
                    continue
                if "config" in step:
                    session = self.substitute(step["config"])
                    response, _ = self.request(identifier, "config/read", {"sessionId": session})
                    view = (response.get("result") or {}).get("config") or {}
                    steps.append({"config": {"ok": "result" in response, PROJECT_KEY: view.get(PROJECT_KEY)}})
                    continue
                request = self.substitute_value(step["send"])
                response, observed = self.request(identifier, request["method"], request["params"])
                steps.append({
                    "method": request["method"],
                    "response": {key: value for key, value in response.items() if key not in {"jsonrpc", "id"}},
                    "notifications": self.notifications(observed),
                })
        except OracleError as error:
            steps.append({"failure": str(error)})
        return steps

    @staticmethod
    def notifications(observed: list[dict[str, Any]]) -> list[str]:
        """Which notifications a step raised, by method.

        A trust decision is observable in what it publishes as much as in what
        it answers: a grant re-derives the session and says so on
        `runtime/updated`. The runtime itself belongs to other rows.
        """

        return sorted(
            {
                message["method"]
                for message in observed
                if "method" in message and "id" not in message
                and message["method"] in {"runtime/updated", "warning"}
            }
        )

    def close(self) -> None:
        server = getattr(self, "server", None)
        if server is not None:
            server.stop()
        self.backend.close()
        shutil.rmtree(self.root, ignore_errors=True)


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def authored_strings(scenario: dict[str, Any]) -> set[str]:
    found: set[str] = set()

    def walk(value: Any) -> None:
        if isinstance(value, str):
            found.add(value)
        elif isinstance(value, dict):
            for key, item in value.items():
                found.add(key)
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)

    walk(scenario)
    return found


class Normalizer:
    def __init__(self, scenario: dict[str, Any], run: Run) -> None:
        self.authored = authored_strings(scenario)
        self.sessions = {session: f"<session-{index + 1}>" for index, session in enumerate(run.sessions)}
        paths = [
            (str(run.world.workspace), "<ws>"),
            (str(run.world.vibe_home), "<vibe-home>"),
            (str(run.world.home), "<home>"),
            (str(run.root), "<root>"),
        ]
        # A path in a TOML document is escaped, so its escaped form is replaced
        # too; the longest spelling goes first so a prefix never wins.
        escaped = [(json.dumps(path)[1:-1], placeholder) for path, placeholder in paths]
        self.paths = sorted({*paths, *escaped}, key=lambda item: -len(item[0]))

    def text(self, value: str) -> str:
        for session, placeholder in self.sessions.items():
            value = value.replace(session, placeholder)
        for path, placeholder in self.paths:
            value = value.replace(path, placeholder)
        return value

    def value(self, value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            return {name: self.value(item, name) for name, item in value.items()}
        if isinstance(value, list):
            return [self.value(item, key) for item in value]
        if not isinstance(value, str):
            return value
        text = self.text(value)
        # The trust file is recorded as written: its bytes are the contract.
        if key == "content":
            return text
        if text in self.authored or value in self.authored:
            return text
        if any(character.isspace() for character in text):
            return digest(text)
        return text


def capture_scenario(scenario: dict[str, Any], command: list[str], dialect: str, quiet: float, raw: bool) -> dict[str, Any]:
    run = Run(scenario, command, dialect, quiet)
    try:
        steps = run.run()
        entry = {"name": scenario["name"], "scenario": scenario, "observed": Normalizer(scenario, run).value(copy.deepcopy(steps))}
        if raw:
            entry["raw"] = steps
        return entry
    finally:
        run.close()


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--server", type=Path, default=None, help="drive this port's stdio fixture instead")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--raw", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.server is not None:
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
            dialect = "port"
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
            dialect = "reference"
        selected = [s for s in scenarios() if not arguments.only or any(name in s["name"] for name in arguments.only)]
        captured = []
        for scenario in selected:
            started = time.monotonic()
            entry = capture_scenario(scenario, command, dialect, arguments.quiet, arguments.raw)
            if dialect == "reference":
                # A corpus holds only what the reference answers every time.
                again = capture_scenario(scenario, command, dialect, arguments.quiet, False)
                if again["observed"] != entry["observed"]:
                    raise OracleError(f"scenario {scenario['name']} is not deterministic across two captures")
            captured.append(entry)
            print(f"{scenario['name']}: {time.monotonic() - started:.1f}s", file=sys.stderr)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "note": (
                "Captured by scripts/parity/workspace_trust.py from the pinned reference's "
                "vibe-app-server. Scenario inputs, paths as placeholders and the trust file as "
                "written; every other string the server authored is a length and a SHA-256."
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
    except (OracleError, acp.OracleError) as error:
        print(f"workspace trust capture failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
