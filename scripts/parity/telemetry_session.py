#!/usr/bin/env python3
"""Black-box capture of what a running session reports to the datalake.

Row 27 of ``docs/parity.md`` is telemetry. ``scripts/parity/telemetry.py``
measures the reference's builders, senders and formatters one call at a time;
this script measures what they add up to once a real session runs them. Every
scenario builds a fresh home and workspace behind a local stand-in for the
whole Mistral platform a session reaches: the chat-completions API, the
caller's identity (``/v1/users/me``), the console's account answer
(``/api/vibe/whoami``), the GrowthBook remote evaluation (``/api/eval/...``) and
the datalake itself (``/v1/datalake/events``). It then runs one entry point
against it and records, run by run:

- every event the datalake received, its census and its own payload;
- the ``metadata`` every chat-completions request carried, which is the request
  census the reference hands its backend (``_build_backend_metadata``);
- the attributes every evaluation request posted;
- how many times the identity and the account endpoints were asked.

Two surfaces are driven, each through its own entry point: ``vibe -p``
(programmatic mode) and ``vibe-acp`` over a scripted editor connection. Both
embed the app server, whose own entry point this port does not ship (row 1 of
``docs/parity.md``). Nothing is imported from the reference: its installed
entry points are the oracle, which is what makes the same scenarios replayable
against this port's binaries.

Deliveries are fire-and-forget on both sides, so the events of one run are
compared as a set: each run's events are sorted by name and then by their
normalized payload. Identities become numbered placeholders in order of first
appearance, versions, platform readings, durations and the system prompt's
size become typed placeholders, and nothing the reference authored as prose
travels in an event, so no digest is needed.

Usage::

    python3 scripts/parity/telemetry_session.py                    # capture the reference
    python3 scripts/parity/telemetry_session.py --check            # recapture and compare
    python3 scripts/parity/telemetry_session.py --vibe target/debug/vibe \\
        --vibe-acp target/debug/vibe-acp --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import blackbox  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
from reference import OracleError, reference_command  # noqa: E402
import host_env  # noqa: E402
import stand_in  # noqa: E402
from telemetry_session_scenarios import MISTRAL_PROVIDER, scenarios  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-cli/tests/telemetry-session/corpus.json"
SCHEMA_VERSION = 1
RUN_TIMEOUT = 120.0
#: How long a finished process is given for a delivery still in flight to land.
SETTLE_SECONDS = 0.5
#: How long an editor connection stays quiet before a step ends.
QUIET_SECONDS = 1.0

#: How long the terminal client stays quiet after a typed line before the
#: next one is typed.
TUI_QUIET_SECONDS = 2.0
#: The terminal the interactive client is drawn into.
TUI_SIZE = (40, 120)

SURFACES = ("programmatic", "acp", "tui")


# --------------------------------------------------------------------------
# The stand-in platform
# --------------------------------------------------------------------------

#: The identity `/v1/users/me` answers by default.
IDENTITY = {
    "id": "oracle-user",
    "workspace": {"id": "oracle-workspace", "name": "Oracle Workspace"},
    "organization": {"id": "oracle-organization", "name": "Oracle"},
}
#: The account `/api/vibe/whoami` answers by default.
WHOAMI = {
    "plan_type": "api",
    "plan_name": "FREE_TRIAL",
    "organization_kind": "personal",
    "customer_id": "oracle-customer",
}


def track(experiment: str, value: Any, *, confirmed: bool = True, variation: int = 1) -> dict[str, Any]:
    return {
        "experiment": {"key": experiment},
        "result": {
            "key": str(variation),
            "variationId": variation,
            "value": value,
            "inExperiment": confirmed,
            "hashAttribute": "userId",
            "hashValue": "oracle-user",
            "featureId": "oracle-feature",
        },
    }


#: An evaluation with one confirmed exposure, on the default variant so the
#: session runs the prompt it would run anyway, and one forced feature that is
#: not an exposure and must stay out of every census.
EVAL = {
    "features": {
        "vibe_cli_system_prompt": {
            "defaultValue": "cli",
            "rules": [{"force": "cli", "tracks": [track("oracle-prompt-experiment", "cli")]}],
        },
        "vibe_cli_managed_shell_tools": {
            "defaultValue": "legacy",
            "rules": [
                {
                    "force": "legacy",
                    "tracks": [track("oracle-shell-experiment", "legacy", confirmed=False)],
                }
            ],
        },
    }
}


class Stand(stand_in.Backend):
    """Every endpoint a session reaches, scripted, with what each was asked.

    A chat-completions request that does not say whether it streams, as the
    generic backend's non-streaming one does not (reference
    ``vibe/core/llm/backend/base.py:65-66``), is served a stream.
    """

    def __init__(self) -> None:
        super().__init__(stream=lambda body: body.get("stream", True), scripted_usage=True)
        self.identity: dict[str, Any] = {"status": 200, "body": IDENTITY}
        self.whoami: dict[str, Any] = {"status": 200, "body": WHOAMI}
        self.evaluation: dict[str, Any] = {"status": 200, "body": EVAL}
        self.managed: dict[str, Any] = {"status": 404, "body": {"error": "not found"}}
        self.log: list[dict[str, Any]] = []

    def get(self, handler: stand_in.Handler, path: str) -> None:
        if path.endswith("/users/me"):
            self.record({"kind": "identity"})
            handler.reply(self.identity["status"], self.identity.get("body", {}))
            return
        if path.endswith("/api/vibe/whoami"):
            self.record({"kind": "whoami"})
            handler.reply(self.whoami["status"], self.whoami.get("body", {}))
            return
        if path.endswith("/api/v1/code/managed-config"):
            self.record({"kind": "managed"})
            handler.reply(self.managed["status"], self.managed.get("body", {}))
            return
        super().get(handler, path)

    def post(self, handler: stand_in.Handler, path: str, body: Any) -> None:
        if "/api/eval/" in path:
            self.record({"kind": "evaluation", "attributes": body.get("attributes")})
            handler.reply(self.evaluation["status"], self.evaluation.get("body", {}))
            return
        if path.endswith("/datalake/events"):
            self.record({"kind": "event", "envelope": body})
            handler.reply(200, {})
            return
        self.record({"kind": "unserved", "path": path})
        super().post(handler, path, body)

    def keep(self, body: Any) -> None:
        self.log.append(
            {"kind": "chat", "metadata": body.get("metadata"), "messages": len(body.get("messages") or [])}
        )

    def record(self, entry: dict[str, Any]) -> None:
        with self.lock:
            self.log.append(entry)

    def take(self) -> list[dict[str, Any]]:
        with self.lock:
            taken, self.log = self.log, []
        return taken


# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------


def compose_config(scenario: dict[str, Any], base: str) -> str:
    top = {
        "active_model": '"mistral-medium-3.5"',
        "enable_telemetry": "true",
        "enable_update_checks": "false",
        "console_base_url": '"$BASE"',
        "vibe_base_url": '"$BASE"',
        "vibe_code_sessions_base_url": '"$BASE"',
    }
    top.update(scenario.get("top", {}))
    experiments = {"api_host": '"$BASE"', "client_key": '"sdk-oracle"'}
    experiments.update(scenario.get("experiments", {}))
    lines = [f"{key} = {value}" for key, value in top.items()]
    lines.append("\n[experiments]")
    lines.extend(f"{key} = {value}" for key, value in experiments.items())
    text = "\n".join(lines) + "\n" + scenario.get("providers", MISTRAL_PROVIDER)
    text += scenario.get("tables", "")
    return text.replace("$BASE", base)


# --------------------------------------------------------------------------
# Running
# --------------------------------------------------------------------------


class World:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.vibe_home = root / "vibe-home"
        self.workspace = root / "workspace"
        for directory in (self.home, self.vibe_home, self.workspace):
            directory.mkdir(parents=True, exist_ok=True)


def environment(world: World, scenario: dict[str, Any], stand: Stand) -> dict[str, str]:
    env = host_env.isolated(
        world.home,
        world.vibe_home,
        MISTRAL_API_KEY="oracle-key",
        VIBE_API_BASE=f"{stand.base}/v1/chat/completions",
        VIBE_ORACLE_TELEMETRY="1",
    )
    for key, value in scenario.get("env", {}).items():
        if value is None:
            env.pop(key, None)
        else:
            env[key] = value
    return env


def learned_session(stdout: str) -> str | None:
    try:
        document = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    entries = document.get("history") if isinstance(document, dict) else document
    for entry in entries or []:
        if isinstance(entry, dict) and isinstance(entry.get("sessionId"), str):
            return entry["sessionId"]
    return None


def run_programmatic(
    scenario: dict[str, Any], command: list[str], env: dict[str, str], world: World, stand: Stand
) -> list[dict[str, Any]]:
    runs = []
    session: str | None = None
    for step in scenario["runs"]:
        args = [arg.replace("$SESSION", session or "") for arg in step["args"]]
        stand.responses = copy.deepcopy(step.get("backend", []))
        completed = subprocess.run(
            [*command, *args],
            input=b"",
            capture_output=True,
            env=env,
            cwd=world.workspace,
            timeout=RUN_TIMEOUT,
            check=False,
        )
        time.sleep(SETTLE_SECONDS)
        if step["args"] and "--output" in step["args"]:
            session = learned_session(completed.stdout.decode("utf-8", errors="replace")) or session
        runs.append({"exit": completed.returncode, "traffic": stand.take()})
    return runs


def run_acp(
    scenario: dict[str, Any], command: list[str], env: dict[str, str], world: Any, stand: Stand
) -> list[dict[str, Any]]:
    """One run per agent process: a restart closes the run it ends."""

    runs = []
    policy = copy.deepcopy(scenario.get("client", {}))
    agent = blackbox.Agent(command, env, world.workspace, policy, world)

    def close() -> None:
        code = agent.stop()
        time.sleep(SETTLE_SECONDS)
        runs.append({"exit": code, "traffic": stand.take()})

    try:
        for step in scenario["steps"]:
            if "restart" in step:
                close()
                agent = blackbox.Agent(command, env, world.workspace, policy, world)
                continue
            if "backend" in step:
                stand.responses.extend(copy.deepcopy(step["backend"]))
                continue
            message = world.substitute(step["send"])
            message.setdefault("jsonrpc", "2.0")
            agent.send(message)
            for item in agent.collect(message.get("id"), QUIET_SECONDS):
                world.learn(item)
    finally:
        close()
    return runs


class Terminal:
    """The interactive client on a pseudo-terminal, read continuously so it
    never blocks on a full screen."""

    def __init__(self, command: list[str], env: dict[str, str], cwd: Path) -> None:
        # POSIX-only, so imported here: the driver itself must import on
        # Windows, where only the scenarios that need a terminal are lost.
        import fcntl
        import pty
        import termios

        self.master, slave = pty.openpty()
        rows, columns = TUI_SIZE
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        self.process = subprocess.Popen(
            command, stdin=slave, stdout=slave, stderr=slave, env=env, cwd=cwd,
            start_new_session=True,
        )
        os.close(slave)

    def pump(self, seconds: float) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.1)
            if ready:
                try:
                    os.read(self.master, 65536)
                except OSError:
                    time.sleep(0.1)

    def press(self, keys: bytes) -> None:
        os.write(self.master, keys)

    def type(self, line: str) -> None:
        os.write(self.master, line.encode())
        self.pump(0.5)
        os.write(self.master, b"\r")

    def close(self) -> int:
        try:
            code = self.process.wait(timeout=RUN_TIMEOUT)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = self.process.wait()
        os.close(self.master)
        return code


def run_tui(
    scenario: dict[str, Any], command: list[str], env: dict[str, str], world: World, stand: Stand
) -> list[dict[str, Any]]:
    """One run: the client starts trusted, waits for its startup report, then
    types each line and waits for the platform to fall quiet."""

    env = {**env, "TERM": "xterm-256color"}
    env.pop("CI", None)
    env.pop("NO_COLOR", None)
    terminal = Terminal([*command, "--trust"], env, world.workspace)

    def quiet(seconds: float) -> None:
        deadline = time.monotonic() + RUN_TIMEOUT
        seen = -1
        while time.monotonic() < deadline:
            with stand.lock:
                count = len(stand.log)
            if count == seen:
                return
            seen = count
            terminal.pump(seconds)

    def started() -> bool:
        with stand.lock:
            return any(
                entry["kind"] == "event" and entry["envelope"].get("event") == "vibe.startup"
                for entry in stand.log
            )

    try:
        deadline = time.monotonic() + RUN_TIMEOUT
        while not started():
            if time.monotonic() > deadline:
                raise OracleError(f"{scenario['name']}: the client never reported its startup")
            terminal.pump(0.5)
        quiet(TUI_QUIET_SECONDS)
        for line in [*scenario["lines"], {"type": "/exit"}]:
            if "backend" in line:
                stand.responses.extend(copy.deepcopy(line["backend"]))
                continue
            if "press" in line:
                terminal.pump(line.get("after", 0))
                terminal.press(line["press"].encode())
            else:
                terminal.type(line["type"])
            if not line.get("hold"):
                quiet(TUI_QUIET_SECONDS)
    finally:
        code = terminal.close()
    time.sleep(SETTLE_SECONDS)
    return [{"exit": code, "traffic": stand.take()}]


def run_scenario(scenario: dict[str, Any], commands: dict[str, list[str]]) -> dict[str, Any]:
    stand = Stand()
    with host_env.scratch_root("vibe-telemetry-session-oracle-") as root:
        world = blackbox.World(root) if scenario["surface"] == "acp" else World(root)
        try:
            for name, value in scenario.get("stand", {}).items():
                setattr(stand, name, copy.deepcopy(value))
            (world.vibe_home / "config.toml").write_text(compose_config(scenario, stand.base), encoding="utf-8", newline="")
            blackbox.write_tree(world.workspace, scenario.get("files", {}))
            env = environment(world, scenario, stand)
            surface = scenario["surface"]
            if surface == "programmatic":
                runs = run_programmatic(scenario, commands["programmatic"], env, world, stand)
            elif surface == "acp":
                runs = run_acp(scenario, commands["acp"], env, world, stand)
            elif surface == "tui":
                runs = run_tui(scenario, commands["tui"], env, world, stand)
            else:
                raise OracleError(f"unknown surface {surface}")
            return {
                "runs": runs,
                "sessions": list(getattr(world, "sessions", [])),
                "paths": {"root": str(root), "base": stand.base},
            }
        finally:
            stand.close()


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------

#: Keys whose string values are identities minted by the program.
ID_KEYS = {
    "session_id",
    "parent_session_id",
    "message_id",
    "source_session_id",
    "new_session_id",
    "correlation_id",
    "read_aloud_session_id",
    "recording_id",
}
#: The version the scripted editor declares in its `clientInfo`.
ORACLE_CLIENT_VERSION = blackbox.INITIALIZE["send"]["params"]["clientInfo"]["version"]
#: Keys whose values are this build's own version.
VERSION_KEYS = {"agent_version", "client_version", "version"}
#: Keys whose values describe the machine the capture ran on.
PLATFORM_KEYS = {"os": "<os>", "os_version": "<os-version>", "arch": "<arch>"}
#: Keys whose numbers are wall-clock readings.
DURATION_KEYS = {
    "init_duration_ms",
    "first_frame_duration_ms",
    "agent_ready_duration_ms",
    "session_init_duration_ms",
}
#: The census fields the experiments task fills, dropped from the one event
#: that races it.
RACED_CENSUS_KEYS = (
    "experiments",
    "experiment_assignments",
    "experiment_attributes",
    "user_plan",
)
#: Keys whose numbers measure the system prompt, which is authored prose on
#: both sides and differs on purpose.
PROSE_SIZE_KEYS = {"nb_context_chars"}


class Normalizer:
    def __init__(self) -> None:
        self.ids: dict[str, str] = {}

    def identity(self, value: str, numbered: bool) -> str:
        if not numbered:
            return "<id>"
        if value not in self.ids:
            self.ids[value] = f"<id-{len(self.ids) + 1}>"
        return self.ids[value]

    def value(self, value: Any, key: str | None = None, numbered: bool = True) -> Any:
        if isinstance(value, dict):
            # Keys are walked sorted, so identities are numbered the same way
            # whatever order a serializer wrote an object's keys in.
            return {name: self.value(value[name], name, numbered) for name in sorted(value)}
        if isinstance(value, list):
            return [self.value(item, key, numbered) for item in value]
        if key in ID_KEYS and isinstance(value, str) and value:
            return self.identity(value, numbered)
        # The version the oracle's editor declares is the client's own and
        # stays; any other is this build's.
        if key in VERSION_KEYS and isinstance(value, str) and value != ORACLE_CLIENT_VERSION:
            return "<version>"
        if key in PLATFORM_KEYS and isinstance(value, str):
            return PLATFORM_KEYS[key]
        if key in DURATION_KEYS and isinstance(value, (int, float)) and not isinstance(value, bool):
            return "<ms>"
        if key in PROSE_SIZE_KEYS and isinstance(value, int) and not isinstance(value, bool):
            return "<chars>"
        return value


def normalize_run(
    run: dict[str, Any], normalizer: Normalizer, racy_lookups: bool = False
) -> dict[str, Any]:
    traffic = run["traffic"]
    chats = [entry for entry in traffic if entry["kind"] == "chat"]
    evaluations = [entry for entry in traffic if entry["kind"] == "evaluation"]
    events = [entry["envelope"] for entry in traffic if entry["kind"] == "event"]
    # The chat requests of one run are sequential, so their order is the
    # session's; they mint the identities first.
    chat_metadata = [normalizer.value(entry["metadata"], "metadata") for entry in chats]
    attributes = [normalizer.value(entry["attributes"], "attributes") for entry in evaluations]
    # The admin-config fetch and the experiments task run concurrently on both
    # sides, so whether the outcome's census already carries what the lookup
    # resolved is a race, not a contract.
    for envelope in events:
        properties = envelope.get("properties", {})
        if envelope.get("event") == "vibe.admin_config_applied":
            for key in RACED_CENSUS_KEYS:
                properties.pop(key, None)
        # A summarization request's prompt is the compaction request, which
        # is authored prose on both sides, like the system prompt.
        if (
            envelope.get("event") == "vibe.request_sent"
            and properties.get("call_type") == "secondary_call"
            and isinstance(properties.get("nb_prompt_chars"), int)
        ):
            properties["nb_prompt_chars"] = "<chars>"
    unnumbered = Normalizer()
    events.sort(
        key=lambda envelope: json.dumps(
            unnumbered.value(envelope, None, numbered=False), sort_keys=True
        )
    )
    numbered = [normalizer.value(envelope, None) for envelope in events]
    # Two events that differ only by an identity sort by its number, which the
    # protocol's own order of announcement fixed.
    numbered.sort(
        key=lambda envelope: (
            json.dumps(unnumbered.value(envelope, None, numbered=False), sort_keys=True),
            json.dumps(envelope, sort_keys=True),
        )
    )
    lookups = {
        name: sum(1 for entry in traffic if entry["kind"] == kind)
        for name, kind in (("identityRequests", "identity"), ("accountRequests", "whoami"))
    }
    # The terminal client reads the identity and the account again once it is
    # ready, racing the experiments task's own reads of both: whether the
    # second read finds the first one cached is timing, so only whether either
    # was asked at all is compared.
    if racy_lookups:
        lookups = {name: count > 0 for name, count in lookups.items()}
    return {
        "exit": run["exit"],
        "chatRequests": len(chats),
        "chatMetadata": chat_metadata,
        "evaluations": attributes,
        **lookups,
        "managedConfigRequests": sum(1 for entry in traffic if entry["kind"] == "managed"),
        "events": numbered,
    }


def normalize(captured: dict[str, Any], surface: str = "") -> list[dict[str, Any]]:
    normalizer = Normalizer()
    # The sessions an editor connection opened are numbered in the order the
    # protocol answered them, before anything else mints an identity.
    for session in captured.get("sessions", []):
        normalizer.identity(session, True)
    return [normalize_run(run, normalizer, surface == "tui") for run in captured["runs"]]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def capture(scenario: dict[str, Any], commands: dict[str, list[str]], raw: bool) -> dict[str, Any]:
    captured = run_scenario(scenario, commands)
    entry = {"name": scenario["name"], "surface": scenario["surface"], "observed": normalize(captured, scenario["surface"])}
    if raw:
        entry["raw"] = captured["runs"]
    return entry


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--vibe", type=Path, default=None, help="drive this port's `vibe`")
    parser.add_argument("--vibe-acp", type=Path, default=None, help="drive this port's `vibe-acp`")
    parser.add_argument("--surface", action="append", default=[], choices=SURFACES)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--raw", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        port = any(
            path is not None for path in (arguments.vibe, arguments.vibe_acp)
        )
        if port:
            commands = {
                "programmatic": [str(arguments.vibe.resolve())] if arguments.vibe else [],
                "acp": [str(arguments.vibe_acp.resolve())] if arguments.vibe_acp else [],
                "tui": [str(arguments.vibe.resolve())] if arguments.vibe else [],
            }
            reference = {"commit": "binary-override"}
        else:
            commit, vibe = reference_command(arguments.reference, "vibe", arguments.expected_commit)
            _, vibe_acp = reference_command(arguments.reference, "vibe-acp", arguments.expected_commit)
            reference = {"commit": commit}
            commands = {"programmatic": vibe, "acp": vibe_acp, "tui": vibe}
        commands = {name: [*command, *HARNESS_FLAGS] if command else [] for name, command in commands.items()}
        surfaces = arguments.surface or [name for name, command in commands.items() if command]
        selected = [
            scenario
            for scenario in scenarios()
            if scenario["surface"] in surfaces
            and (not arguments.only or any(name in scenario["name"] for name in arguments.only))
        ]
        captured = []
        for scenario in selected:
            started = time.monotonic()
            entry = capture(scenario, commands, arguments.raw)
            if not port and not arguments.check:
                again = capture(scenario, commands, False)
                if again["observed"] != entry["observed"]:
                    raise OracleError(
                        f"scenario {scenario['name']} is not deterministic across two captures"
                    )
            captured.append(entry)
            print(f"{scenario['name']}: {time.monotonic() - started:.1f}s", file=sys.stderr)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "note": (
                "Captured by scripts/parity/telemetry_session.py from the pinned reference's "
                "entry points behind a scripted Mistral platform. Identities, versions, platform "
                "readings, durations and the system prompt's size as placeholders; each run's "
                "events sorted, since deliveries are fire-and-forget."
            ),
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
        print(f"captured {len(captured)} scenarios into {arguments.output}")
    except (OracleError, subprocess.TimeoutExpired) as error:
        print(f"telemetry session capture failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
