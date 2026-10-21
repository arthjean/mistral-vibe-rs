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
import concurrent.futures
import copy
import hashlib
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

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402
import host_env  # noqa: E402
from blackbox import (  # noqa: E402
    Agent,
    Backend,
    Normalizer,
    RESPONSE_TIMEOUT,
    World,
    base_config,
    rendered,
    write_tree,
)
from acp_scenarios import scenarios  # noqa: E402
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
