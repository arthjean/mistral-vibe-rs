#!/usr/bin/env python3
"""Black-box capture of the Unified Harness, the backend a session runs on by default.

Since v2.26.0 the reference opens every session on its Unified Harness unless
``--legacy-harness`` asks for the legacy backend (``vibe/_experimental_harness.py``).
Rows 1 to 35 of ``docs/parity.md`` measure that legacy backend through the
flag; this capture measures the default one, row 36.

Every scenario starts an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in of ``loop_capture.py``, and drives it the
way ``agent_loop.py`` does: sessions, turns, callbacks, restarts and any
method. What is recorded is what a client, the model and the disk see of the
backend: the notifications each turn raised, every answer, each model request
with its parameters, its tool catalog and its conversation, and the session
store the backend keeps under ``logs/session/unified/``. A scenario may add
flags to the server's command line, which is how the selection itself is
measured.

Nothing is imported from the reference: its own ``vibe-app-server`` is the
oracle, which is what makes the same scenarios replayable against this port by
``crates/vibe-app-server/tests/unified_harness_parity_tests.rs``.
Normalization maps identifiers, paths, times and durations to placeholders and
reduces every string a server authored to its length and SHA-256, which is
what ``NOTICE`` requires of a committed corpus.

Usage::

    python3 scripts/parity/unified_harness.py                  # capture the reference
    python3 scripts/parity/unified_harness.py --check          # recapture and compare
    python3 scripts/parity/unified_harness.py --server target/debug/vibe-app-server-stdio-fixture \\
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

import app_server_client  # noqa: E402
import blackbox  # noqa: E402
import loop_capture  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
from reference import OracleError, require_pin  # noqa: E402
from unified_harness_scenarios import scenarios  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/unified-harness-parity/corpus.json"
SCHEMA_VERSION = 1

#: Unified sessions answer a little slower than legacy ones: a turn commits a
#: store generation before it settles.
QUIET_SECONDS = 0.8


# --------------------------------------------------------------------------
# What a request carried
# --------------------------------------------------------------------------


#: Request metadata that names the machine the capture ran on.
HOST_METADATA = {"os", "arch", "os_version", "terminal_emulator"}
#: Request metadata that names the build, each side's own release.
VERSION_METADATA = {"version", "agent_version"}


def request_view(body: dict[str, Any], utility: bool) -> dict[str, Any]:
    """A model request: its parameters, its tools and its conversation.

    A utility request (a title, a classification, a summary) keeps the count
    of what it carried and its parameters, since its prompt is the server's
    own prose.
    """

    parameters = {
        key: value for key, value in body.items() if key not in {"messages", "tools"}
    }
    metadata = parameters.get("metadata")
    if isinstance(metadata, dict):
        parameters["metadata"] = {
            key: "<host>" if key in HOST_METADATA else "<version>" if key in VERSION_METADATA
            else value
            for key, value in metadata.items()
        }
    messages = body.get("messages", [])
    view: dict[str, Any] = {"parameters": parameters}
    if utility:
        view["messages"] = len(messages)
        return view
    view["system"] = [
        message.get("content") for message in messages if message.get("role") == "system"
    ]
    view["conversation"] = [
        message for message in messages if message.get("role") != "system"
    ]
    view["tools"] = [
        {
            "type": tool.get("type"),
            "name": (tool.get("function") or {}).get("name"),
            "description": (tool.get("function") or {}).get("description"),
            "parameters": (tool.get("function") or {}).get("parameters"),
        }
        for tool in body.get("tools") or []
        if isinstance(tool, dict)
    ]
    return view


# --------------------------------------------------------------------------
# What the store holds
# --------------------------------------------------------------------------


GENERATION = re.compile(r"^\d{16}$")
DIGEST = re.compile(r"^[0-9a-f]{64}(\.json)?$")
#: A background process's directory, named after its session-derived ID.
PROCESS_DIRECTORY = re.compile(r"^process-[0-9a-f]{12}-[0-9a-f]{24}$")


def layout(directory: Path) -> list[str]:
    """The files a session directory holds, generations, journal segments and
    chunks reduced to their shape."""

    shapes: set[str] = set()
    for path in sorted(directory.rglob("*")):
        if not path.is_file():
            continue
        parts = list(path.relative_to(directory).parts)
        for index, part in enumerate(parts):
            stem = part.split(".", 1)[0]
            if GENERATION.match(stem):
                parts[index] = part.replace(stem, "<sequence>")
            elif DIGEST.match(part):
                parts[index] = part.replace(stem, "<sha256>")
            elif PROCESS_DIRECTORY.match(part):
                parts[index] = "<process>"
        shapes.add("/".join(parts))
    return sorted(shapes)


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def keyed(document: Any) -> Any:
    """A JSON object with the order its keys were written in."""

    if isinstance(document, dict):
        return {"keys": list(document), "value": document}
    return document


def session_store(directory: Path) -> dict[str, Any]:
    current = read_json(directory / "CURRENT") or {}
    generation = current.get("generation")
    manifest = (
        read_json(directory / "generations" / str(generation) / "manifest.json")
        if generation
        else None
    )
    projection = (
        read_json(directory / "generations" / str(generation) / "projection-state.json")
        if generation
        else None
    )
    return {
        "layout": layout(directory),
        "current": {
            "keys": list(current),
            "storeFormat": current.get("store_format"),
            "storeFormatMinor": current.get("store_format_minor"),
        },
        "manifest": None if manifest is None else {
            "keys": list(manifest),
            "executionState": manifest.get("execution_state"),
            "manifestVersion": manifest.get("manifest_version"),
            "interopExport": manifest.get("interop_export") is not None,
        },
        "meta": keyed(read_json(directory / "meta.json")),
        "loops": read_json(directory / "scheduled-loops.json"),
        "projection": None if projection is None else {
            "keys": list(projection),
            "version": projection.get("projection_state_version"),
            "snapshot": projection.get("snapshot"),
        },
    }


def persisted(world: blackbox.World) -> dict[str, Any]:
    """Every unified session the store holds, in the order they started, and
    any legacy session directory beside them."""

    root = world.vibe_home / "logs/session"
    unified = root / "unified"
    sessions = []
    if unified.is_dir():
        for directory in unified.iterdir():
            if directory.is_dir():
                meta = read_json(directory / "meta.json") or {}
                sessions.append((meta.get("start_time") or "", directory))
    sessions.sort(key=lambda item: item[0])
    legacy = sorted(
        path.parent.name
        for path in root.glob("*/meta.json")
        if path.parent.name not in {"unified", "active", "plugins"}
    )
    return {
        "unified": [session_store(directory) for _, directory in sessions],
        "legacy": len(legacy),
        "top": sorted(path.name for path in root.iterdir()) if root.is_dir() else [],
    }


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


#: A loop's ID and its clock, as the `cron` tool writes them into its answer:
#: eight random hex digits and seconds since the epoch.
LOOP_ID_TEXT = re.compile(
    r"""(?<=["']id["']: ["'])[0-9a-f]{8}(?=["'])|(?<=loop )[0-9a-f]{8}\b|(?<=ID: )[0-9a-f]{8}\b""")
#: A loop pydantic reports, its repr cut around the clock it was stamped
#: with: where the cut falls depends on how many digits that float printed.
ELIDED_LOOP = re.compile(r"\{'id': '[0-9a-f]{8}', 'inter\.\.\.[^}]*\}")
#: A loop's next run as the `cron` tool and `/loop` print it, in local time.
LOCAL_TIME = re.compile(r"\b\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} [A-Z][A-Za-z0-9+-]{1,5}\b")
EPOCH_SECONDS = re.compile(r"""\b1[7-9]\d{8}\.\d+\b|(?<=["']next_fire_at["']: )\d+\.\d+""")
#: The day a system prompt says it is, whose weekday changes its length.
TODAYS_DATE = re.compile(
    r"\b\d{4}-\d{2}-\d{2} \((?:Mon|Tues|Wednes|Thurs|Fri|Satur|Sun)day\)")


#: Keys whose string values are identities, beyond the loop's.
ID_KEYS = loop_capture.ID_KEYS | {"message_id", "action_id", "lease_id", "inputEntryId",
                                "queueItemId", "latestTurnId", "processId"}
#: Keys whose values are wall-clock readings: a unified session's archive mark
#: and its retry are times like its pin.
TIME_KEYS = app_server_client.TIME_KEYS | {"archivedAt", "retryAt"}


class Normalizer(loop_capture.Normalizer):
    id_keys = ID_KEYS
    time_keys = TIME_KEYS

    def text(self, value: str) -> str:
        value = ELIDED_LOOP.sub("{<loop>}", value)
        value = LOOP_ID_TEXT.sub(lambda match: self.identity(match.group(0)), value)
        value = EPOCH_SECONDS.sub("<time>", value)
        value = LOCAL_TIME.sub("<time>", value)
        value = TODAYS_DATE.sub("<date>", value)
        return super().text(value)

    def value(self, value: Any, key: str | None = None) -> Any:
        if key in {"start_time", "end_time", "bumped_at", "pinned_at", "archived_at",
                   "unseen_at", "seen_at", "created_at"} and isinstance(value, str):
            return "<time>"
        if key in {"firedAt", "next_fire_at", "last_fired_at", "nextFireAt", "lastFiredAt"} \
                and isinstance(value, int | float):
            return "<time>"
        if key in {"username"} and isinstance(value, str):
            return "<user>"
        return super().value(value, key)


def reduce_turn(scenario: dict[str, Any], messages: list[dict[str, Any]]) -> list[Any]:
    return loop_capture.reduce_patches(loop_capture.reduce_turn(scenario, messages, TIME_KEYS))


def reduce_code_mode(requests: list[dict[str, Any]], steps: list[Any]) -> None:
    """Applies the two views above to what the model was sent, and drops the
    ranking-derived count a ``best_match`` discovery entry shows."""

    for request in requests:
        for message in request.get("conversation") or []:
            if message.get("role") != "tool" or not isinstance(message.get("content"), str):
                continue
            if message.get("name") == "search_tool_functions":
                message["content"] = loop_capture.search_view(message["content"])
            elif message.get("name") == "run_typescript":
                message["content"] = loop_capture.program_failure_view(message["content"])

    def walk(value: Any) -> None:
        if isinstance(value, dict):
            detail = value.get("detail")
            if (isinstance(detail, dict) and detail.get("toolName") == "search_tool_functions"
                    and (detail.get("input") or {}).get("mode", "best_match") == "best_match"):
                output = (value.get("state") or {}).get("output")
                if isinstance(output, dict) and output.get("toolCount"):
                    output["toolCount"] = "<ranked>"
            for item in value.values():
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)

    walk(steps)


#: A background process ID, which hashes the session and the call that
#: started it.
PROCESS_ID = re.compile(r"process-[0-9a-f]{12}-[0-9a-f]{24}")
CHILD_ID = re.compile(r"child-[0-9a-f]{24}")


def rename_processes(run: dict[str, Any]) -> dict[str, Any]:
    """The run with every background process ID, and the eight-character
    short form a display shows, replaced by its order of first appearance."""

    text = json.dumps(run)
    names: dict[str, str] = {}
    for found in PROCESS_ID.findall(text):
        names.setdefault(found, f"process-{len(names) + 1}")
    for process_id, name in names.items():
        text = text.replace(process_id, name)
        text = re.sub(rf"\bprocess {process_id[-8:]}\b", f"process {name}", text)
    # A child session's ID is random, and its name defaults to the ID's head.
    children: dict[str, str] = {}
    for found in CHILD_ID.findall(text):
        children.setdefault(found, f"child-session-{len(children) + 1}")
    for child_id, name in children.items():
        text = text.replace(child_id, name)
        text = text.replace(f'"{child_id[:8]}"', f'"{name}:short"')
    return json.loads(text)


def split_children(messages: list[dict[str, Any]]) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """A step's notifications without its child summaries, and what those
    summaries showed of each child.

    A child's turn runs beside its parent's, so where a summary lands among
    the parent's events, and which of them a summary already reflects, is a
    race the two processes settle by speed. What each child went through is
    not: the statuses it passed through, repeats folded, and its last
    summary.
    """

    parent: list[dict[str, Any]] = []
    children: dict[str, Any] = {}
    for message in messages:
        if message.get("method") != "session/childSessionUpdated":
            parent.append(message)
            continue
        child = copy.deepcopy((message.get("params") or {}).get("childSession") or {})
        view = children.setdefault(str(child.get("id")), {"statuses": [], "final": None})
        status = (child.get("status") or {}).get("type")
        if not view["statuses"] or view["statuses"][-1] != status:
            view["statuses"].append(status)
        view["final"] = child
    return parent, children


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    run = rename_processes(run)
    normalizer = Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    reduced: list[dict[str, Any]] = []
    for step in run["steps"]:
        if "failure" in step:
            reduced.append({"failure": step["failure"]})
        elif "turn" in step or "idle" in step:
            key = "turn" if "turn" in step else "idle"
            parent, children = split_children(step[key])
            view: dict[str, Any] = {key: reduce_turn(scenario, parent)}
            if children:
                view["children"] = children
            reduced.append(view)
        elif "persisted" in step or "file" in step:
            reduced.append(copy.deepcopy(step))
        else:
            response = copy.deepcopy(step["response"])
            if "result" in response:
                response["result"] = loop_capture.reduce_result(response["result"], step.get("pick"))
                # The reference lists a runtime's tools in the order of a
                # Python set.
                runtime = response["result"].get("runtime") if isinstance(response["result"], dict) else None
                if isinstance(runtime, dict) and isinstance(runtime.get("tools"), list):
                    runtime["tools"].sort(key=lambda tool: str((tool or {}).get("name")))
            reduced.append({
                "method": step["method"],
                "response": response,
                "notifications": reduce_turn(scenario, step["notifications"]),
            })
    # A utility request races the model steps it runs beside, so the model
    # requests keep their order and the utility ones follow in theirs.
    requests = sorted(copy.deepcopy(run["requests"]), key=lambda request: request["utility"])
    reduce_code_mode(requests, reduced)
    callbacks = [
        {
            "kind": ((c.get("callback") or {}).get("detail") or {}).get("kind"),
            "detail": (c.get("callback") or {}).get("detail"),
        }
        for c in run["callbacks"]
    ]
    for part in (reduced, requests, callbacks):
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
        "requests": shared_tools(normalizer.value(requests)),
        "callbacks": normalizer.value(callbacks),
    }


def shared_tools(requests: list[Any]) -> list[Any]:
    """Requests whose tool list repeats an earlier request's name that one
    instead of carrying the list again."""

    for index, request in enumerate(requests):
        tools = request.get("tools")
        if not isinstance(tools, list):
            continue
        for earlier in range(index):
            if requests[earlier].get("tools") == tools:
                request["tools"] = f"<as request {earlier}>"
                break
    return requests


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


class Session(loop_capture.Session):
    """What a scenario learned, plus what a unified session names by a random ID.

    A step names the loops a response created as ``$L1``, ``$L2``, ... in the
    order they appeared, the turns that started as ``$R1``, ``$R2``, ..., the
    queued turns as ``$Q1``, ``$Q2``, ... and the child sessions as ``$K1``,
    ``$K2``, ...
    """

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.loops: list[str] = []
        self.queued: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        result = message.get("result") or {}
        found = [result.get("loop")] if isinstance(result, dict) else []
        for loop in found:
            if isinstance(loop, dict) and isinstance(loop.get("id"), str) and loop["id"] not in self.loops:
                self.loops.append(loop["id"])
        item = result.get("queueItemId") if isinstance(result, dict) else None
        if isinstance(item, str) and item not in self.queued:
            self.queued.append(item)
        if message.get("method") == "session/childSessionUpdated":
            child = ((message.get("params") or {}).get("childSession") or {}).get("id")
            if isinstance(child, str) and child not in self.children:
                self.children.append(child)

    def substitute(self, value: Any) -> Any:
        value = super().substitute(value)
        if isinstance(value, str):
            for index in range(len(self.loops), 0, -1):
                value = value.replace(f"$L{index}", self.loops[index - 1])
            for index in range(len(self.turns), 0, -1):
                value = value.replace(f"$R{index}", self.turns[index - 1])
            for index in range(len(self.queued), 0, -1):
                value = value.replace(f"$Q{index}", self.queued[index - 1])
            for index in range(len(self.children), 0, -1):
                value = value.replace(f"$K{index}", self.children[index - 1])
        elif isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        elif isinstance(value, list):
            return [self.substitute(item) for item in value]
        return value


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    return loop_capture.run_scenario(
        scenario,
        [*command, *scenario.get("flags", [])],
        quiet,
        session_class=Session,
        persisted=persisted,
        request_view=request_view,
    )


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
            reference = {"commit": require_pin(arguments.reference, arguments.expected_commit)}
            binary = arguments.reference / ".venv/bin/vibe-app-server"
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
        print(f"unified harness oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
