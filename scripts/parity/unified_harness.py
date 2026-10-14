#!/usr/bin/env python3
"""Black-box capture of the Unified Harness, the backend a session runs on by default.

Since v2.26.0 the reference opens every session on its Unified Harness unless
``--legacy-harness`` asks for the legacy backend (``vibe/_experimental_harness.py``).
Rows 1 to 35 of ``docs/parity.md`` measure that legacy backend through the
flag; this capture measures the default one, row 36.

Every scenario starts an app server over stdio in a fresh home, behind the
scripted chat-completions stand-in ``agent_loop.py`` serves, and drives it the
way that capture does: sessions, turns, callbacks, restarts and any method.
What is recorded is what a client, the model and the disk see of the backend:
the notifications each turn raised, every answer, each model request with its
parameters, its tool catalog and its conversation, and the session store the
backend keeps under ``logs/session/unified/``. A scenario may add flags to the
server's command line, which is how the selection itself is measured.

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

import acp  # noqa: E402
import agent_loop  # noqa: E402
import rewind  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/unified-harness-parity/corpus.json"
SCHEMA_VERSION = 1

#: Unified sessions answer a little slower than legacy ones: a turn commits a
#: store generation before it settles.
QUIET_SECONDS = 0.8

call = agent_loop.call
send = agent_loop.send
START = agent_loop.START
PERSISTED = agent_loop.PERSISTED


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


def persisted(world: acp.World) -> dict[str, Any]:
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


class Normalizer(agent_loop.Normalizer):
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


#: Keys whose string values are identities, beyond the loop's.
ID_KEYS = agent_loop.ID_KEYS | {"message_id", "action_id", "lease_id", "inputEntryId",
                                "queueItemId", "latestTurnId", "processId"}
agent_loop.ID_KEYS.update(ID_KEYS)
#: A unified session's archive mark is a time like its pin, in this capture's
#: process only.
rewind.TIME_KEYS.update({"archivedAt", "retryAt"})


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


def reduce_turn(scenario: dict[str, Any], messages: list[dict[str, Any]]) -> list[Any]:
    return reduce_patches(agent_loop.reduce_turn(scenario, messages))


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


def reduce_code_mode(requests: list[dict[str, Any]], steps: list[Any]) -> None:
    """Applies the two views above to what the model was sent, and drops the
    ranking-derived count a ``best_match`` discovery entry shows."""

    for request in requests:
        for message in request.get("conversation") or []:
            if message.get("role") != "tool" or not isinstance(message.get("content"), str):
                continue
            if message.get("name") == "search_tool_functions":
                message["content"] = search_view(message["content"])
            elif message.get("name") == "run_typescript":
                message["content"] = program_failure_view(message["content"])

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
                response["result"] = agent_loop.reduce_result(response["result"], step.get("pick"))
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


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    return agent_loop.run_scenario(scenario, [*command, *scenario.get("flags", [])], quiet)


# A scheduled loop's ID is random: a step names the loops a response
# created as `$L1`, `$L2`, ... in the order they appeared, the turns that
# started as `$R1`, `$R2`, ... and the queued turns as `$Q1`, `$Q2`, ...
_learn = agent_loop.Session.learn
_substitute = agent_loop.Session.substitute


def _learn_loops(self: Any, message: dict[str, Any]) -> None:
    _learn(self, message)
    loops = getattr(self, "loops", None)
    if loops is None:
        loops = self.loops = []
    items = getattr(self, "queued", None)
    if items is None:
        items = self.queued = []
    result = message.get("result") or {}
    found = [result.get("loop")] if isinstance(result, dict) else []
    for loop in found:
        if isinstance(loop, dict) and isinstance(loop.get("id"), str) and loop["id"] not in loops:
            loops.append(loop["id"])
    item = result.get("queueItemId") if isinstance(result, dict) else None
    if isinstance(item, str) and item not in items:
        items.append(item)
    children = getattr(self, "children", None)
    if children is None:
        children = self.children = []
    if message.get("method") == "session/childSessionUpdated":
        child = ((message.get("params") or {}).get("childSession") or {}).get("id")
        if isinstance(child, str) and child not in children:
            children.append(child)


def _substitute_loops(self: Any, value: Any) -> Any:
    value = _substitute(self, value)
    loops = getattr(self, "loops", [])
    turns = getattr(self, "turns", [])
    if isinstance(value, str):
        for index in range(len(loops), 0, -1):
            value = value.replace(f"$L{index}", loops[index - 1])
        for index in range(len(turns), 0, -1):
            value = value.replace(f"$R{index}", turns[index - 1])
        queued = getattr(self, "queued", [])
        for index in range(len(queued), 0, -1):
            value = value.replace(f"$Q{index}", queued[index - 1])
        children = getattr(self, "children", [])
        for index in range(len(children), 0, -1):
            value = value.replace(f"$K{index}", children[index - 1])
    elif isinstance(value, dict):
        return {key: _substitute_loops(self, item) for key, item in value.items()}
    elif isinstance(value, list):
        return [_substitute_loops(self, item) for item in value]
    return value


agent_loop.Session.learn = _learn_loops
agent_loop.Session.substitute = _substitute_loops

# The loop capture reads these module globals at call time, so the unified
# views replace the legacy ones for every scenario this script runs.
agent_loop.persisted = persisted
agent_loop.request_view = request_view


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def config_read() -> dict[str, Any]:
    return send("config/read", pick=["startupIssue", "harnessSelectionSource"])


def runtime_read() -> dict[str, Any]:
    return send("runtime/read", sessionId="$S1")


#: A one-pixel PNG, inline and as the bytes `WRITE_PNG` writes to `shot.png`.
PNG = ("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAA"
       "AABJRU5ErkJggg==")
WRITE_PNG = f"import base64; open('shot.png', 'wb').write(base64.b64decode('{PNG}'))"
#: The default Mistral model, blind, beside a local one.
BLIND_MODELS = (
    '\n[[models]]\nname = "mistral-vibe-cli-latest"\nprovider = "mistral"\n'
    'alias = "mistral-medium-3.5"\ntemperature = 1.0\nthinking = "high"\n'
    'supports_images = false\n'
)
#: The default model, compacting once its context passes 20 tokens.
AUTO_COMPACT_MODEL = (
    '\n[[models]]\nname = "mistral-vibe-cli-latest"\nprovider = "mistral"\n'
    'alias = "mistral-medium-3.5"\ntemperature = 1.0\nthinking = "high"\n'
    'auto_compact_threshold = 50000\n'
)
#: A model of the same provider that reads images.
VISION_MODEL = (
    '\n[[models]]\nname = "pixtral-large-latest"\nprovider = "mistral"\n'
    'alias = "pixtral"\nsupports_images = true\n'
)


def inline_image() -> dict[str, Any]:
    return {"type": "image", "attachment": {"source": {"kind": "inline", "data": PNG},
                                            "alias": "pasted.png", "mimeType": "image/png"}}


def file_image(path: str) -> dict[str, Any]:
    return {"type": "image", "attachment": {"source": {"kind": "file", "path": path},
                                            "alias": "shot.png", "mimeType": "image/png"}}


#: Two turns of file writes: the first writes `a.txt`, the second rewrites it
#: and creates `b.txt`, so a rewind to either has files to restore.
REWIND_WRITES = [
    {"toolCalls": [call("write_file", {"path": "a.txt", "content": "one\n"}, "call_w1")]},
    {"text": "Wrote one."},
    {"toolCalls": [call("write_file", {"path": "a.txt", "content": "two\n"}, "call_w2")]},
    {"toolCalls": [call("write_file", {"path": "b.txt", "content": "new\n"}, "call_w3")]},
    {"text": "Wrote two."},
    {"text": "Again."},
]
#: The two turns `REWIND_WRITES` answers, each naming its own message, which
#: is the identifier the harness core rewinds to.
REWIND_TURNS = [
    {"turn": "Write one", "params": {"clientUserMessageId": "msg-one"}},
    {"turn": "Write two", "params": {"clientUserMessageId": "msg-two"}},
]
#: Lets a session run under the smart-approve profile.
SMART_APPROVE = "smart_approve_available = true\n"


def verdict(safe: bool, reason: str, risk: str, authorization: str) -> dict[str, Any]:
    """A classifier answer, as the smart-approve gate reads one."""
    return {"text": json.dumps({"safe": safe, "reason": reason, "risk_level": risk,
                                "user_authorization": authorization})}


def user_entry(text: str) -> dict[str, Any]:
    """A queued user entry holding `text`."""
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def session_start(agent: str = "auto-approve") -> dict[str, Any]:
    """A `session/start` whose answer, the state the session opened with, is
    recorded."""

    return send("session/start", agentConfig={"cwd": "$WS", "agent": agent})


ECHO = [{"toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")]}, {"text": "Finished."}]
#: A command no allowlist covers, so a session that asks before running one
#: raises an approval for it.
TOUCH = [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_b2")]},
         {"text": "Finished."}]
#: The same command under another call: the reference derives an action's
#: identifier from its call's, so a later turn reusing a call identifier is a
#: redelivery it refuses, which no model produces.
TOUCH_AGAIN = [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_b3")]},
               {"text": "Finished."}]
#: One question in the shape the unified catalog declares: its keys are the
#: camel-cased ones `ui.ask_user_question` takes.
QUESTION = {"questions": [{
    "question": "Which color?",
    "header": "Color",
    "options": [{"label": "Red", "description": "Warm"},
                {"label": "Blue", "description": "Cold"}],
}]}


#: A program that drives every process operation once. Process IDs and
#: output paths name the session, so the program reports neither.
#: A stdio MCP server small enough to read at a glance: three tools, one
#: whose name needs normalizing and one that reports an error.
MCP_SERVER = """\
import json
import sys

OBJECT = {"type": "object", "properties": {}, "additionalProperties": False}
TOOLS = [
    {"name": "echo", "description": "Send a message back.",
     "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}},
                     "required": ["message"], "additionalProperties": False}},
    {"name": "create-issue", "description": "Open an issue with a title.",
     "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}},
                     "required": ["title"], "additionalProperties": False}},
    {"name": "fail", "description": "Report an error every time.", "inputSchema": OBJECT},
]


def answer(name, arguments):
    if name == "echo":
        text = arguments.get("message", "")
        return {"content": [{"type": "text", "text": "echo " + text}],
                "structuredContent": {"echo": text}}
    if name == "create-issue":
        return {"content": [{"type": "text", "text": "opened " + arguments.get("title", "")}]}
    return {"content": [{"type": "text", "text": "it failed"}], "isError": True}


for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        result = {"protocolVersion": message["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "oracle-mcp", "version": "1.0.0"}}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        params = message["params"]
        result = answer(params["name"], params.get("arguments") or {})
    elif method == "ping":
        result = {}
    else:
        reply = {"jsonrpc": "2.0", "id": message["id"],
                 "error": {"code": -32601, "message": "unknown method"}}
        print(json.dumps(reply), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"""

#: The server above, under a name the group normalizes, with a hint.
MCP_TABLES = """
[[mcp_servers]]
name = "data-api"
transport = "stdio"
command = "python3"
args = ["$WS/mcp_server.py"]
prompt = "Reach the issue tracker through these tools."
"""

#: A program that calls each tool of the server, catching the failure.
MCP_PROGRAM = (
    "async function main() {\n"
    "  const echoed = await tools.mcp_data_api.echo({message: 'hi'});\n"
    "  const opened = await tools.mcp_data_api.create_issue({title: 'Bug'});\n"
    "  let failure = null;\n"
    "  try { await tools.mcp_data_api.fail({}); } catch (error) { failure = error; }\n"
    "  return {echoed, opened, failure};\n}\n"
)

#: A project plugin that declares the same server, launched from its own
#: checkout.
MCP_PLUGIN = {
    ".vibe/plugins/tracker/plugin.json": json.dumps({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
        "name": "tracker", "version": "1.0.0"}) + "\n",
    ".vibe/plugins/tracker/mcp.json": json.dumps({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
        "mcpServers": {"issues": {"type": "stdio", "command": "./server.py"}}}) + "\n",
    ".vibe/plugins/tracker/server.py": "#!/usr/bin/env python3\n" + MCP_SERVER,
}

#: Discovery runs once the session is open; a turn waits for it.
MCP_START = [START, {"sleep": 2.0}]

PROCESS_PROGRAM = """
const strip = (value) => {
  if (!value || typeof value !== "object") return value;
  const { processId, outputPath, ...rest } = value;
  if (outputPath !== undefined) rest.hasOutputPath = outputPath !== null;
  return rest;
};
const attempt = async (run) => {
  try { return strip(await run()); } catch (error) {
    const { processId, ...details } = (error.cause && error.cause.details) || {};
    return { name: error.name, message: error.message, details };
  }
};
async function main() {
  const a = await tools.process.start({ command: "printf 'one\\ntwo\\n'" });
  await tools.self.sleep({ seconds: 0.8 });
  const out = await tools.process.output({ processId: a.processId });
  const tail = await tools.process.output({ processId: a.processId, from: "end", maxBytes: 4 });
  const page = await tools.process.output({ processId: a.processId, cursor: 2, maxBytes: 3 });
  const b = await tools.process.start({ command: "sleep 30", env: { MARK: "x" } });
  const listed = await tools.process.list({});
  const stopped = await tools.process.stop({ processId: b.processId });
  const again = await attempt(() => tools.process.stop({ processId: b.processId }));
  const written = await attempt(() => tools.process.write({ processId: b.processId, text: "x" }));
  const missing = await attempt(() => tools.process.output({ processId: "process-missing" }));
  const badCwd = await attempt(() => tools.process.start({ command: "true", cwd: "/nonexistent/dir" }));
  const failed = await tools.process.start({ command: "exit 3" });
  await tools.self.sleep({ seconds: 0.8 });
  const failedOut = await tools.process.output({ processId: failed.processId });
  return {
    a: strip(a), out: strip(out), tail: strip(tail), page: strip(page), b: strip(b),
    listed: listed.processes.map(strip), stopped: strip(stopped), again, written, missing,
    badCwd, failedOut: strip(failedOut),
  };
}
"""

#: A program that talks to a process through its terminal.
INTERACTIVE_PROGRAM = """
async function main() {
  const cat = await tools.process.start({ command: "cat" });
  const sent = await tools.process.write({ processId: cat.processId, text: "hello\\n" });
  await tools.self.sleep({ seconds: 0.5 });
  const echoed = await tools.process.output({ processId: cat.processId, waitMs: 2000 });
  const control = await tools.process.write({ processId: cat.processId, control: ["ctrl_d"] });
  await tools.self.sleep({ seconds: 0.5 });
  const done = await tools.process.output({ processId: cat.processId });
  const bytes = await tools.process.start({ command: "head -c 3" });
  const raw = await tools.process.write({ processId: bytes.processId, bytesBase64: "YWJj" });
  await tools.self.sleep({ seconds: 0.5 });
  const rawOut = await tools.process.output({ processId: bytes.processId });
  const drop = ({ processId, ...rest }) => rest;
  return { sent: drop(sent), echoed: drop(echoed), control: drop(control), done: drop(done),
           raw: drop(raw), rawOut: drop(rawOut) };
}
"""


#: A program that drives every subagent operation once, waiting on each
#: turn so the scripted answers reach the child in order.
SUBAGENT_PROGRAM = """
const attempt = async (run) => {
  try { return await run(); } catch (error) { return { name: error.name, message: error.message }; }
};
async function main() {
  const spawned = await tools.subagent.spawn({ agentName: "helper", message: "Do the first part" });
  const first = await tools.subagent.wait({ agentName: "helper", timeoutMs: 20000 });
  const listed = await tools.subagent.list({});
  const again = await tools.subagent.spawn({ agentName: "helper", message: "Again" });
  const sent = await tools.subagent.sendMessage({ agentName: "helper", message: "Do the second part" });
  const second = await tools.subagent.wait({ agentName: "helper", timeoutMs: 20000 });
  const interrupted = await tools.subagent.interrupt({ agentName: "helper" });
  const closed = await tools.subagent.close({ agentName: "helper" });
  const after = await tools.subagent.list({});
  const missing = await tools.subagent.wait({ agentName: "nobody", timeoutMs: 10 });
  const badType = await attempt(() => tools.subagent.spawn({ agentName: "other", message: "x", agentType: "nope" }));
  return { spawned, first, listed, again, sent, second, interrupted, closed, after, missing, badType };
}
"""


#: A child interrupted inside a long command, then waited on and closed.
SUBAGENT_INTERRUPT_PROGRAM = """
async function main() {
  const spawned = await tools.subagent.spawn({ agentName: "worker", message: "Work slowly" });
  await tools.self.sleep({ seconds: 1 });
  const listed = await tools.subagent.list({});
  const interrupted = await tools.subagent.interrupt({ agentName: "worker" });
  const waited = await tools.subagent.wait({ agentName: "worker", timeoutMs: 20000 });
  const sent = await tools.subagent.sendMessage({ agentName: "worker", message: "Resume" });
  const resumed = await tools.subagent.wait({ agentName: "worker", timeoutMs: 20000 });
  const closed = await tools.subagent.close({ agentName: "worker" });
  const again = await tools.subagent.close({ agentName: "worker" });
  const message = await tools.subagent.sendMessage({ agentName: "worker", message: "x" });
  const late = await tools.subagent.wait({ agentName: "worker", timeoutMs: 10 });
  return { spawned, listed, interrupted, waited, sent, resumed, closed, again, message, late };
}
"""

#: A child whose command asks the user, in a session that asks.
SUBAGENT_APPROVAL_PROGRAM = """
async function main() {
  await tools.subagent.spawn({ agentName: "asker", message: "Touch the file" });
  return await tools.subagent.wait({ agentName: "asker", timeoutMs: 20000 });
}
"""

SUBAGENT_KEEP_PROGRAM = """
async function main() {
  await tools.subagent.spawn({ agentName: "keeper", message: "Remember this" });
  return await tools.subagent.wait({ agentName: "keeper", timeoutMs: 20000 });
}
"""

#: A subagent type a user wrote, with its own instructions and no shell.
REVIEWER = {"write": {"$ROOT/vibe-home/agents/reviewer.toml": (
    'agent_type = "subagent"\n'
    'description = "Reviews changes without running commands"\n'
    'instructions = "You review. Never run commands."\n'
    'disabled_tools = ["bash"]\n')}}

SUBAGENT_TYPED_PROGRAM = """
const attempt = async (run) => {
  try { return await run(); } catch (error) { return { name: error.name, message: error.message }; }
};
async function main() {
  const unknown = await attempt(() => tools.subagent.spawn({ agentName: "ghost", agentType: "nobody", message: "Hi" }));
  await tools.subagent.spawn({ agentName: "critic", agentType: "reviewer", message: "Review the change" });
  const answer = await tools.subagent.wait({ agentName: "critic", timeoutMs: 20000 });
  const listed = await tools.subagent.list({});
  return { unknown, answer, listed };
}
"""

#: A session an older Vibe saved, which the unified host lists, previews and
#: imports on resume.
LEGACY_ID = "5eed0000-0000-4000-8000-00000000abcd"
LEGACY_DIRECTORY = "$ROOT/vibe-home/logs/session/session_20260102_030405_5eed0000"
LEGACY = {"write": {
    f"{LEGACY_DIRECTORY}/meta.json": json.dumps({
        "session_id": LEGACY_ID, "parent_session_id": None,
        "start_time": "2026-01-02T03:04:05.000000+00:00",
        "end_time": "2026-01-02T03:05:06.000000+00:00",
        "git_commit": None, "git_branch": None,
        "environment": {"working_directory": "$WS"}, "origin_directory": "$WS",
        "username": "oracle", "child_sessions": [], "loops": [],
        "title": "Seeded session", "title_source": "manual",
        "bumped_at": "2026-01-02T03:04:05.500000+00:00", "pinned_at": None,
        "experiments": None, "config": {"active_model": "mistral-medium-3.5"},
        "import_provenance": None, "created_worktree": None, "stats": {},
        "total_messages": 4, "last_message_fingerprint": None, "tools_available": [],
        "agent_profile": {"name": "default", "overrides": {}}, "system_prompt": None,
    }, indent=2),
    f"{LEGACY_DIRECTORY}/messages.jsonl": "".join(json.dumps(message) + "\n" for message in [
        {"role": "user", "content": "Seeded question", "message_id": "5eed0000-0000-4000-8000-000000000001"},
        {"role": "assistant", "content": "Seeded answer", "message_id": "5eed0000-0000-4000-8000-000000000002"},
        {"role": "user", "content": "Seeded follow-up", "message_id": "5eed0000-0000-4000-8000-000000000003"},
        {"role": "assistant", "content": "Seeded closing", "message_id": "5eed0000-0000-4000-8000-000000000004"},
    ]),
}}


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- which backend a launch selects ----------------------------------
        {
            "name": "selection/default",
            "steps": [config_read(), session_start(), runtime_read()],
        },
        {
            "name": "selection/experimental-flag",
            "flags": ["--experimental-harness"],
            "steps": [config_read(), session_start(), runtime_read()],
        },
        {
            "name": "selection/legacy-flag",
            "flags": ["--legacy-harness"],
            "steps": [config_read(), session_start(), runtime_read()],
        },
        # -- what a turn sends the model and raises --------------------------
        {
            "name": "turn/text",
            "backend": [{"text": "Hello there."}],
            "steps": [START, {"turn": "Say hello"}, PERSISTED],
        },
        {
            "name": "turn/reasoning",
            "backend": [{"reasoning": "Thinking it over.", "text": "Answer."}],
            "steps": [START, {"turn": "Think first"}],
        },
        {
            "name": "turn/two-turns",
            "backend": [{"text": "First."}, {"text": "Second."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"}, PERSISTED],
        },
        {
            "name": "tools/bash",
            "backend": ECHO,
            "steps": [START, {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "tools/files",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [
                {"toolCalls": [call("read_file", {"path": "notes.txt"}, "call_r1")]},
                {"toolCalls": [call("write_file", {"path": "made.txt", "content": "new\n"},
                                    "call_w1")]},
                {"toolCalls": [call("edit", {"file_path": "notes.txt", "old_string": "beta",
                                             "new_string": "gamma"}, "call_e1")]},
                {"text": "Edited."},
            ],
            "steps": [START, {"turn": "Work on the files"}, {"file": "$WS/notes.txt"},
                      {"file": "$WS/made.txt"}],
        },
        {
            "name": "tools/parallel",
            "files": {"a.txt": "one\n", "b.txt": "two\n"},
            "backend": [
                {"toolCalls": [call("read_file", {"path": "a.txt"}, "call_p1"),
                               call("read_file", {"path": "b.txt"}, "call_p2")]},
                {"text": "Both read."},
            ],
            "steps": [START, {"turn": "Read both"}],
        },
        {
            "name": "tools/failures",
            "backend": [
                {"toolCalls": [call("read_file", {"path": "missing.txt"}, "call_f1")]},
                {"toolCalls": [call("no_such_tool", {}, "call_f2")]},
                {"toolCalls": [{"id": "call_f3", "name": "bash", "raw": "{not json"}]},
                {"toolCalls": [call("edit", {"path": "x.txt"}, "call_f4")]},
                {"text": "Gave up."},
            ],
            "steps": [START, {"turn": "Try things"}],
        },
        {
            "name": "tools/todo",
            "backend": [
                {"toolCalls": [call("todo", {"action": "write", "todos": [
                    {"id": "1", "content": "First task", "status": "in_progress"},
                    {"id": "2", "content": "Second task", "priority": "high"},
                ]}, "call_t1")]},
                {"toolCalls": [call("todo", {"action": "read"}, "call_t2")]},
                {"text": "Planned."},
            ],
            "steps": [START, {"turn": "Plan the work"}],
        },
        {
            "name": "tools/scratchpad",
            "backend": [
                {"toolCalls": [call("unified_harness_scratchpad",
                                    {"action": "write", "path": "notes.md", "content": "kept\n"},
                                    "call_s1")]},
                {"toolCalls": [call("unified_harness_scratchpad", {"action": "list"}, "call_s2")]},
                {"toolCalls": [call("unified_harness_scratchpad",
                                    {"action": "read", "path": "notes.md"}, "call_s3")]},
                {"text": "Noted."},
            ],
            "steps": [START, {"turn": "Take notes"}],
        },
        # -- approvals and questions ----------------------------------------
        {
            "name": "approval/approve",
            "backend": TOUCH,
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "approval/deny",
            "backend": TOUCH,
            "callbacks": [{"approve": False}],
            "steps": [session_start("ask"), {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "approval/for-session",
            "backend": [*TOUCH, *TOUCH_AGAIN],
            "callbacks": [{"output": {"type": "approval",
                                      "decision": {"type": "approve_for_session"}}}],
            "steps": [session_start("ask"), {"turn": "Run touch"}, {"turn": "Again"},
                      PERSISTED],
        },
        {
            "name": "approval/accept-edits",
            "backend": [
                {"toolCalls": [call("write_file", {"path": "made.txt", "content": "x\n"},
                                    "call_a1")]},
                {"toolCalls": [call("bash", {"command": "touch other.txt"}, "call_a2")]},
                {"text": "Done."},
            ],
            "callbacks": [{"approve": True}],
            "steps": [session_start("accept-edits"), {"turn": "Write then run"}],
        },
        {
            "name": "approval/interrupted",
            "backend": TOUCH,
            "callbacks": [{"ignore": True}],
            "steps": [session_start("ask"),
                      {"turn": "Run echo", "background": True, "settle": 1.5},
                      {"interrupt": True}, PERSISTED],
        },
        # -- rewind ----------------------------------------------------------
        {
            "name": "rewind/inplace",
            "backend": REWIND_WRITES,
            "steps": [START, *REWIND_TURNS,
                      send("session/rewind/read", sessionId="$S1", entryId="$U2"),
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=True,
                           restoreFiles=True),
                      {"file": "$WS/a.txt"}, {"file": "$WS/b.txt"},
                      send("session/rewind/read", sessionId="$S1", entryId="$U1"),
                      {"turn": "Again"}, send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "rewind/keep-files",
            "backend": REWIND_WRITES,
            "steps": [START, *REWIND_TURNS,
                      send("session/rewind", sessionId="$S1", entryId="$U1", inplace=True,
                           restoreFiles=False),
                      {"file": "$WS/a.txt"}, send("session/turns/list", sessionId="$S1"),
                      PERSISTED],
        },
        {
            "name": "rewind/fork",
            "backend": REWIND_WRITES,
            "steps": [START, *REWIND_TURNS,
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=False,
                           restoreFiles=True),
                      {"file": "$WS/a.txt"}, {"file": "$WS/b.txt"},
                      send("session/read", sessionId="$S2"),
                      send("session/rewind/read", sessionId="$S2", entryId="$U1"),
                      {"turn": "Again", "session": "$S2"}, send("session/list"), PERSISTED],
        },
        {
            "name": "rewind/projected-ids",
            "backend": REWIND_WRITES,
            "steps": [START, {"turn": "Write one"}, {"turn": "Write two"},
                      send("session/rewind/read", sessionId="$S1", entryId="$U2"),
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=True,
                           restoreFiles=True),
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=False,
                           restoreFiles=True),
                      {"file": "$WS/a.txt"}, PERSISTED],
        },
        {
            "name": "rewind/unknown",
            "backend": [{"text": "Hi."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/rewind", sessionId="$S1", entryId="user-missing", inplace=True,
                           restoreFiles=True),
                      send("session/rewind/read", sessionId="$S1", entryId="$A1"),
                      send("session/rewind", sessionId="$S1", entryId="$U1", inplace=True,
                           restoreFiles=True),
                      send("session/read", sessionId="$S1")],
        },
        # -- background titles -----------------------------------------------
        {
            **agent_loop.TITLES,
            "name": "titles/first-answer",
            "backend": [{"text": "Answer one."}],
            "utility": [{"text": "Parser plan"}],
            "steps": [START, {"turn": "Plan the parser", "linger": 1.5}, PERSISTED],
        },
        {
            **agent_loop.TITLES,
            "name": "titles/tool-heavy-turn",
            "backend": [{**step, "delay": 0.6} if index in (2, 9) else step
                        for index, step in enumerate(agent_loop.echo_steps(8))],
            "utility": [{"text": "Echo marathon"}, {"text": "Echo marathon two"}],
            "steps": [START, {"turn": "Echo many times", "linger": 1.5}, PERSISTED],
        },
        {
            **agent_loop.TITLES,
            "name": "titles/after-compaction",
            "backend": [{"text": "Answer one."}, {"text": "A summary of the session."},
                        {"text": "Answer two."}],
            "utility": [{"text": "First title"}, {"text": "Second title"}],
            "steps": [START, {"turn": "First question", "linger": 1.5},
                      {**send("session/compact", sessionId="$S1"), "linger": 1.0},
                      {"turn": "Second question", "linger": 1.5}, PERSISTED],
        },
        {
            **agent_loop.TITLES,
            "name": "titles/manual-title-wins",
            "backend": [{"text": "Answer one."}],
            "steps": [START, send("session/rename", sessionId="$S1", title="Mine"),
                      {"turn": "First question", "linger": 1.5}, PERSISTED],
        },
        {
            **agent_loop.TITLES,
            "name": "titles/generic-answer",
            "backend": [{"text": "Answer one."}, {"text": "Answer two."}],
            "utility": [{"text": "\"New session\""}, {"text": "  Second   try\nignored"}],
            "steps": [START, {"turn": "First question", "linger": 1.5},
                      {"turn": "Second question", "linger": 1.5}, PERSISTED],
        },
        # -- smart approve ---------------------------------------------------
        {
            "name": "smart/allow",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [verdict(True, "Creates a file in the workspace", "low", "high")],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/deny-continue",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [verdict(False, "Touches files nobody asked for", "medium", "unknown")],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/escalate",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [verdict(False, "Writes into the workspace", "high", "high")],
            "callbacks": [{"approve": True}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/unparsable",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [{"text": "not a verdict"}],
            "callbacks": [{"approve": False}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/repeat",
            "config": SMART_APPROVE,
            "backend": [TOUCH[0], TOUCH_AGAIN[0], {"text": "Finished."}],
            "utility": [verdict(False, "Touches files nobody asked for", "medium", "unknown"),
                        verdict(False, "Touches files nobody asked for", "medium", "unknown")],
            "callbacks": [{"approve": False}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/session-grant",
            "config": SMART_APPROVE,
            "backend": [*TOUCH, *TOUCH_AGAIN],
            "utility": [verdict(False, "Writes into the workspace", "high", "high")],
            "callbacks": [{"output": {"type": "approval",
                                      "decision": {"type": "approve_for_session"}}}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, {"turn": "Again"},
                      PERSISTED],
        },
        {
            "name": "smart/precleared",
            "config": SMART_APPROVE,
            "backend": ECHO,
            "steps": [session_start("smart-approve"), {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "question/answer",
            "backend": [{"toolCalls": [call("ask_user_question", QUESTION, "call_q1")]},
                        {"text": "Thanks."}],
            "callbacks": [{"choose": "Blue"}],
            "steps": [START, {"turn": "Ask me"}],
        },
        {
            "name": "tools/skill",
            "backend": [{"toolCalls": [call("skill", {"name": "vibe"}, "call_k1")]},
                        {"toolCalls": [call("skill", {"name": "vibe:worktree"}, "call_k2")]},
                        {"text": "Loaded."}],
            "steps": [START, {"turn": "Load the skill"}],
        },
        {
            "name": "approval/cancel-turn",
            "backend": TOUCH,
            "callbacks": [{"output": {"type": "approval", "decision": {"type": "cancel_turn"}}}],
            "steps": [session_start("ask"), {"turn": "Run touch"}, PERSISTED],
        },
        # -- steering a turn -------------------------------------------------
        {
            "name": "turn/interrupt-stream",
            "backend": [{"text": "Slow.", "delay": 3}],
            "steps": [START, {"turn": "Wait", "background": True, "settle": 1.0},
                      {"interrupt": True}, PERSISTED],
        },
        {
            "name": "turn/steer",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 1"}, "call_s1")]},
                        {"text": "Steered."}],
            "steps": [START, {"turn": "Work", "background": True, "settle": 0.5},
                      {"steer": "Also this"}, {"wait": True}, PERSISTED],
        },
        {
            "name": "turn/steer-applied",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 3"}, "call_s1")]},
                        {"text": "Steered."}],
            "steps": [START, {"turn": "Work", "background": True, "settle": 0.5},
                      send("turn/steer", sessionId="$S1", expectedTurnId="turn-other",
                           message=[{"type": "text", "text": "Wrong turn"}]),
                      send("turn/steer", sessionId="$S1", expectedTurnId="$R1",
                           message=[{"type": "text", "text": "Also this"}]),
                      {"wait": True},
                      send("turn/steer", sessionId="$S1", expectedTurnId="$R1",
                           message=[{"type": "text", "text": "Too late"}]),
                      PERSISTED],
        },
        # -- preparing a turn's input ----------------------------------------
        {
            "name": "prepare/skills",
            "backend": [{"text": "Reviewed."}, {"text": "Again."}, {"text": "Plain."}],
            "steps": [{"write": {
                          "$ROOT/vibe-home/skills/review/SKILL.md":
                              "---\nname: review\ndescription: Review the change\n---\n\nRead the diff.\n",
                          "$ROOT/vibe-home/skills/hidden/SKILL.md":
                              "---\nname: hidden\ndescription: Not for users\nuser-invocable: false\n---\n\nSecret.\n",
                      }},
                      START, {"turn": "/review the diff, then /hidden and `/review`"},
                      {"turn": "Please /review again."},
                      {"turn": "/unknown skill"},
                      send("session/read", sessionId="$S1"),
                      send("session/list"),
                      PERSISTED],
        },
        {
            "name": "prepare/mentions",
            "backend": [{"text": "Read it."}],
            "steps": [{"write": {"notes.txt": "first line\nsecond line\n",
                                 "fence.md": "````\ncode\n````\n"}},
                      START, {"turn": "Look at @notes.txt and @fence.md and @missing.txt"},
                      PERSISTED],
        },
        {
            "name": "prepare/scratchpad",
            "backend": [{"toolCalls": [call("unified_harness_scratchpad",
                                            {"action": "write", "path": "plan.md",
                                             "content": "Step one"}, "call_p1")]},
                        {"text": "Saved."}, {"text": "Next."}, {"text": "Same."},
                        {"toolCalls": [call("unified_harness_scratchpad",
                                            {"action": "write", "path": "plan.md",
                                             "content": ""}, "call_p2")]},
                        {"text": "Removed."}, {"text": "Empty."}],
            "steps": [START, {"turn": "Save a note"}, {"turn": "Continue"}, {"turn": "Again"},
                      {"turn": "Empty it"}, {"turn": "Now"}, PERSISTED],
        },
        # -- images ----------------------------------------------------------
        {
            "name": "images/native",
            "backend": [{"text": "Inline seen."}, {"text": "File seen."}],
            "steps": [{"run": ["python3", "-c", WRITE_PNG]},
                      START,
                      {"turn": "What is this?", "params": {"message": [
                          {"type": "text", "text": "What is this?"}, inline_image()]}},
                      {"turn": "And this file?", "params": {"message": [
                          {"type": "text", "text": "And this file?"}, file_image("$WS/shot.png")]}},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "images/described",
            "tables": BLIND_MODELS + VISION_MODEL,
            "utility": [{"text": "A single red pixel."},
                        {"status": 500, "body": {"message": "vision down"}}],
            "backend": [{"text": "Described."}, {"text": "Unreadable."}],
            "steps": [{"run": ["python3", "-c", WRITE_PNG]},
                      START,
                      {"turn": "What is this?", "params": {"message": [
                          {"type": "text", "text": "What is this?"}, inline_image()]}},
                      {"turn": "And this file?", "params": {"message": [
                          {"type": "text", "text": "And this file?"}, file_image("$WS/shot.png")]}},
                      PERSISTED],
        },
        {
            "name": "images/blind",
            "tables": BLIND_MODELS,
            "backend": [{"text": "Linked."}, {"text": "Inline."}],
            "steps": [{"run": ["python3", "-c", WRITE_PNG]},
                      START,
                      {"turn": "And this file?", "params": {"message": [
                          {"type": "text", "text": "And this file?"}, file_image("$WS/shot.png")]}},
                      {"turn": "What is this?", "params": {"message": [
                          {"type": "text", "text": "What is this?"}, inline_image()]}},
                      PERSISTED],
        },
        {
            "name": "errors/bad-request",
            "backend": [{"status": 400, "body": {"message": "bad request"}}],
            "steps": [START, {"turn": "Fail"}, PERSISTED],
        },
        {
            "name": "errors/unauthorized",
            "backend": [{"status": 401, "body": {"message": "no key"}}],
            "steps": [START, {"turn": "Fail"}],
        },
        {
            "name": "errors/retried",
            "backend": [{"status": 503, "body": {"message": "busy"}}, {"text": "Recovered."}],
            "steps": [START, {"turn": "Retry"}],
        },
        {
            "name": "session/reads",
            "backend": [{"text": "Hi."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/turns/list", sessionId="$S1"),
                      send("session/history/list", sessionId="$S1"),
                      send("session/history/get", sessionId="$S1"),
                      send("session/title/update", sessionId="$S1", title="Titled")],
        },
        # -- code mode -------------------------------------------------------
        {
            "name": "code/inventory",
            "backend": [{"toolCalls": [call("search_tool_functions",
                                            {"mode": "all_connector_capabilities"}, "call_s1")]},
                        {"toolCalls": [call("search_tool_functions",
                                            {"query": "run a background process"}, "call_s2")]},
                        {"toolCalls": [call("search_tool_functions",
                                            {"functions": ["process.start", "subagent.spawn"]}, "call_s3")]},
                        {"text": "Listed."}],
            "steps": [START, {"turn": "List"}],
        },
        {
            "name": "code/details",
            "backend": [{"toolCalls": [call("search_tool_functions", {"functions": [
                "file_system.bash", "file_system.read_file", "file_system.write_file",
                "file_system.edit", "self.sleep", "process.start", "process.output",
                "process.write", "process.list", "process.stop"]}, "call_d1")]},
                        {"toolCalls": [call("search_tool_functions", {"functions": [
                "subagent.list", "subagent.spawn", "subagent.wait", "subagent.sendMessage",
                "subagent.interrupt", "subagent.close", "vibe.todo", "tools.vibe.todo",
                "nowhere.missing"]}, "call_d2")]},
                        {"toolCalls": [call("search_tool_functions", {"mode": "details"}, "call_d3")]},
                        {"toolCalls": [call("search_tool_functions",
                                            {"mode": "all_connector_capabilities",
                                             "connectors": ["process", "nowhere"]}, "call_d4")]},
                        {"toolCalls": [call("search_tool_functions", {"mode": "best_match"}, "call_d5")]},
                        {"toolCalls": [call("search_tool_functions", {"query": "zzzz"}, "call_d6")]},
                        {"toolCalls": [call("search_tool_functions", {"query": "stop",
                                                                      "connectors": ["nowhere"]}, "call_d7")]},
                        {"toolCalls": [call("search_tool_functions", {"functions": [""]}, "call_d8")]},
                        {"text": "Detailed."}],
            "steps": [START, {"turn": "Details"}],
        },
        {
            "name": "code/basic",
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n  console.log('hi', {a: 1});\n"
                "  console.error('warn');\n  return {value: 42, list: [1, 'two']};\n}\n")}, "call_t1")]},
                        {"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n  throw new Error('boom');\n}\n")}, "call_t2")]},
                        {"toolCalls": [call("run_typescript", {"code": "const x: = 1;"}, "call_t3")]},
                        {"toolCalls": [call("run_typescript", {"code": "const y = 1;"}, "call_t4")]},
                        {"text": "Ran."}],
            "steps": [START, {"turn": "Run"}],
        },
        {
            "name": "code/tools",
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  const read = await tools.file_system.read_file({path: 'notes.txt'});\n"
                "  return read;\n}\n")}, "call_t5")]},
                        {"text": "Read."}],
            "steps": [START, {"write": {"notes.txt": "alpha\nbeta\n"}}, {"turn": "Read"}],
        },
        # -- an MCP server's tools ------------------------------------------
        {
            "name": "mcp/calls",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES,
            "backend": [
                {"toolCalls": [call("search_tool_functions",
                                    {"mode": "all_connector_capabilities",
                                     "connectors": ["mcp_data_api"]}, "call_m1")]},
                {"toolCalls": [call("search_tool_functions",
                                    {"functions": ["mcp_data_api.echo",
                                                   "mcp_data_api.create_issue"]}, "call_m2")]},
                {"toolCalls": [call("run_typescript", {"code": MCP_PROGRAM}, "call_m3")]},
                {"text": "Called."},
            ],
            "steps": [*MCP_START, {"turn": "Use the tracker"}, PERSISTED],
        },
        {
            "name": "mcp/approve",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES,
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  return await tools.mcp_data_api.echo({message: 'asked'});\n}\n")},
                "call_m4")]}, {"text": "Approved."}],
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Echo"}, PERSISTED],
        },
        {
            "name": "mcp/deny",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES,
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  return await tools.mcp_data_api.echo({message: 'refused'});\n}\n")},
                "call_m5")]}, {"text": "Denied."}],
            "callbacks": [{"approve": False}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Echo"}],
        },
        {
            "name": "mcp/never",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES + '\n[tools.data-api_echo]\npermission = "never"\n',
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  return await tools.mcp_data_api.echo({message: 'never'});\n}\n")},
                "call_m6")]}, {"text": "Refused."}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Echo"}],
        },
        {
            "name": "mcp/runtime",
            "files": {"mcp_server.py": MCP_SERVER, **MCP_PLUGIN},
            "tables": MCP_TABLES,
            "steps": [*MCP_START, runtime_read()],
        },
        {
            "name": "mcp/plugin",
            "files": MCP_PLUGIN,
            "backend": [
                {"toolCalls": [call("search_tool_functions",
                                    {"mode": "all_connector_capabilities",
                                     "connectors": ["mcp_issues"]}, "call_m7")]},
                {"toolCalls": [call("run_typescript", {"code": (
                    "async function main() {\n"
                    "  return await tools.mcp_issues.create_issue({title: 'From a plugin'});\n}\n")},
                    "call_m8")]},
                {"text": "Opened."},
            ],
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Open one"}, PERSISTED],
        },
        # -- background processes -------------------------------------------
        {
            "name": "process/lifecycle",
            "backend": [{"toolCalls": [call("run_typescript", {"code": PROCESS_PROGRAM}, "call_p1")]},
                        {"text": "Processes."}],
            "steps": [START, {"turn": "Processes"},
                      send("session/backgroundProcess/output", sessionId="$S1",
                           processId="process-missing"),
                      send("session/backgroundProcess/output", sessionId="$S1",
                           processId="process-missing", fromEnd=False, cursor=0, waitMs=0,
                           maxBytes=100),
                      send("session/backgroundProcess/stop", sessionId="$S1",
                           processId="process-missing"),
                      PERSISTED],
        },
        {
            "name": "process/interactive",
            "backend": [{"toolCalls": [call("run_typescript", {"code": INTERACTIVE_PROGRAM}, "call_p2")]},
                        {"text": "Interacted."}],
            "steps": [START, {"turn": "Interact"}],
        },
        {
            "name": "process/idle-exit",
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  const late = await tools.process.start({command: 'sleep 1; echo late'});\n"
                "  return late.status;\n}\n")}, "call_p3")]},
                        {"text": "Started."}, {"text": "Again."}],
            "steps": [START, {"turn": "Start"}, {"sleep": 2.0},
                      send("session/resume", sessionId="$S1"), {"turn": "Again"}],
        },
        # -- subagents -------------------------------------------------------
        {
            "name": "subagent/lifecycle",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_PROGRAM}, "call_a1")]},
                        {"text": "Child answer."},
                        {"text": "Child second answer."},
                        {"text": "Delegated."}],
            "steps": [START, {"turn": "Delegate"},
                      send("session/list"), PERSISTED],
        },
        {
            "name": "subagent/interrupt",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_INTERRUPT_PROGRAM}, "call_i1")]},
                        {"toolCalls": [call("bash", {"command": "sleep 30"}, "call_i2")]},
                        {"text": "Resumed."},
                        {"text": "Interrupted."}],
            "steps": [START, {"turn": "Delegate"}],
        },
        {
            "name": "subagent/approval",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_APPROVAL_PROGRAM}, "call_q1")]},
                        {"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_q2")]},
                        {"text": "Touched."},
                        {"text": "Approved."}],
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"turn": "Delegate"}, PERSISTED],
        },
        {
            "name": "subagent/workspace-type",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_TYPED_PROGRAM}, "call_r1")]},
                        {"toolCalls": [call("bash", {"command": "echo hi"}, "call_r2")]},
                        {"text": "Reviewed."},
                        {"text": "Done."}],
            "steps": [REVIEWER, START, {"turn": "Review"}, PERSISTED],
        },
        {
            "name": "subagent/workspace-type-auto",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_TYPED_PROGRAM}, "call_r1")]},
                        {"toolCalls": [call("read_file", {"path": "notes.txt"}, "call_r2")]},
                        {"text": "Reviewed."},
                        {"text": "Done."}],
            "files": {"notes.txt": "note\n"},
            "steps": [REVIEWER, session_start("auto-approve"), {"turn": "Review"}],
        },
        {
            "name": "subagent/workspace-type-write",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_TYPED_PROGRAM}, "call_r1")]},
                        {"toolCalls": [call("write_file", {"path": "review.md", "content": "ok\n"}, "call_r2")]},
                        {"text": "Reviewed."},
                        {"text": "Done."}],
            "callbacks": [{"approve": True}],
            "steps": [REVIEWER, session_start("auto-approve"), {"turn": "Review"}, {"file": "review.md"}],
        },
        {
            "name": "subagent/resume-children",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_KEEP_PROGRAM}, "call_k1")]},
                        {"text": "Child answer."},
                        {"text": "Kept."},
                        {"text": "After."}],
            "steps": [START, {"turn": "Delegate"},
                      send("session/read", sessionId="$K1"),
                      send("session/history/list", sessionId="$K1"),
                      {"restart": True},
                      send("session/resume", sessionId="$S1"),
                      send("session/read", sessionId="$K1"),
                      {"turn": "Again"}, PERSISTED],
        },
        # -- sessions an older Vibe saved ------------------------------------------
        {
            "name": "legacy/list-read",
            "steps": [LEGACY, send("session/list"),
                      send("session/read", sessionId=LEGACY_ID),
                      send("session/read", sessionId=LEGACY_ID, history={"limit": 1})],
        },
        {
            "name": "legacy/resume",
            "backend": [{"text": "Continued."}],
            "steps": [LEGACY, send("session/resume", sessionId=LEGACY_ID),
                      {"turn": "Go on"}, send("session/list"), PERSISTED],
        },
        # -- scheduled loops ----------------------------------------------------
        {
            "name": "loops/methods",
            "steps": [START,
                      send("loops/list", sessionId="$S1"),
                      send("loops/create", sessionId="$S1", interval="5m", prompt="  Check the build  "),
                      send("loops/create", sessionId="$S1", interval="5x", prompt="Bad"),
                      send("loops/create", sessionId="$S1", interval="10s", prompt="Too fast"),
                      send("loops/create", sessionId="$S1", interval="1h", prompt="/compact"),
                      send("loops/create", sessionId="$S1", interval="1h", prompt="   "),
                      send("loops/list", sessionId="$S1"),
                      send("loops/delete", sessionId="$S1", loopId="nope"),
                      send("loops/delete", sessionId="$S1", loopId="$L1"),
                      send("loops/create", sessionId="$S1", interval="2d", prompt="Daily"),
                      send("loops/clear", sessionId="$S1"),
                      send("loops/clear", sessionId="$S1")],
        },
        {
            "name": "loops/cron-tool",
            "backend": [{"toolCalls": [
                            call("cron", {"action": "schedule", "interval_seconds": 120,
                                          "prompt": "Check the build"}, "call_c1"),
                            call("cron", {"action": "schedule_cron", "cron": "0 9 * * 1",
                                          "prompt": "Weekly report"}, "call_c2"),
                            call("cron", {"action": "schedule", "interval_seconds": 5,
                                          "prompt": "Too fast"}, "call_c3"),
                            call("cron", {"action": "schedule_cron", "cron": "bad",
                                          "prompt": "Broken"}, "call_c4"),
                            call("cron", {"action": "cancel", "id": "nope"}, "call_c5")]},
                        {"toolCalls": [call("cron", {"action": "list"}, "call_c6")]},
                        {"toolCalls": [call("cron", {"action": "clear"}, "call_c7")]},
                        {"text": "Scheduled."}],
            "steps": [START, {"turn": "Schedule"},
                      send("loops/list", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "loops/fire",
            "backend": [{"text": "Hello."}, {"text": "Looped."}],
            "steps": [START, {"turn": "Hi"},
                      send("loops/create", sessionId="$S1", interval="30s", prompt="Check the build"),
                      {"wait": True},
                      send("loops/list", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "loops/slash-loop",
            "backend": [{"toolCalls": [call("cron", {"action": "schedule", "interval_seconds": 60,
                                                     "prompt": "Check the build"}, "call_l1")]},
                        {"text": "Scheduled."}],
            "steps": [START, {"turn": "/loop every minute check the build"},
                      send("session/read", sessionId="$S1"),
                      send("session/list")],
        },
        # -- session operations ---------------------------------------------
        {
            "name": "session/compact",
            "backend": [{"text": "First."}],
            "utility": [{"text": "A summary of the work."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/compact", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-midturn",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"toolCalls": [call("bash", {"command": "echo hi"}, "call_c1")],
                         "promptTokens": 60000},
                        {"text": "Done."}],
            "utility": [{"text": "A summary of the work."}],
            "steps": [START, {"turn": "Work"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-next-turn",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"text": "First.", "promptTokens": 60000}, {"text": "Second."}],
            "utility": [{"text": "A summary of the work."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-empty",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"text": "First.", "promptTokens": 60000},
                        {"text": " "}, {"text": " "}, {"text": " "}, {"text": "Second."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-failed",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"text": "First.", "promptTokens": 60000},
                        {"toolCalls": [call("bash", {"command": "echo a"}, "call_f1")]},
                        {"toolCalls": [call("bash", {"command": "echo b"}, "call_f2")]},
                        {"toolCalls": [call("bash", {"command": "echo c"}, "call_f3")]},
                        {"text": "Second."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "session/inject",
            "backend": [{"text": "Seen."}],
            "steps": [START,
                      send("session/context/inject", sessionId="$S1",
                           input=[{"type": "text", "text": "Background fact."}]),
                      {"turn": "Hello"}],
        },
        {
            "name": "session/clear",
            "backend": [{"text": "First."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/history/clear", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "session/fork-delete",
            "backend": [{"text": "First."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/fork", sourceSessionId="$S1"),
                      send("session/list"),
                      send("session/delete", sessionId="$S1"),
                      send("session/list")],
        },
        {
            "name": "session/settings",
            "backend": [{"text": "First."}],
            "steps": [START,
                      send("session/settings/update", sessionId="$S1",
                           settings={"reasoningEffort": "low"}),
                      send("session/agent/update", sessionId="$S1", agent="plan"),
                      {"turn": "Hello"}],
        },
        {
            "name": "queue/enqueue",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 8"}, "call_q0")]},
                        {"text": "First answer."}, {"text": "Second answer."},
                        {"text": "Third answer."}],
            "steps": [START, {"turn": "First", "background": True, "settle": 0.5},
                      send("session/turn/enqueue", sessionId="$S1", entries=[]),
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[user_entry("Second")]),
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[{"role": "context",
                                     "content": [{"type": "text", "text": "Context note"}]},
                                    user_entry("Third")]),
                      send("session/turn/queue/read", sessionId="$S1"),
                      send("session/turn/queue/replace", sessionId="$S1", queueItemId="$Q1",
                           entries=[user_entry("Second, edited")]),
                      {"wait": True}, {"sleep": 3},
                      send("session/turn/queue/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "queue/pause",
            "backend": [{"text": "Slow.", "delay": 3}, {"text": "Resumed."}],
            "steps": [START, {"turn": "First", "background": True, "settle": 0.5},
                      send("session/turn/enqueue", sessionId="$S1", entries=[user_entry("Later")]),
                      {"interrupt": True}, {"sleep": 1},
                      send("session/turn/queue/read", sessionId="$S1"),
                      send("session/turn/queue/resume", sessionId="$S1"),
                      {"sleep": 2}, PERSISTED],
        },
        {
            "name": "queue/steer",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 7"}, "call_q1")]},
                        {"text": "Steered."}, {"text": "Idle answer."}],
            "steps": [START, {"turn": "Work", "background": True, "settle": 0.5},
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[user_entry("Steer me")]),
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[user_entry("Remove me")]),
                      send("session/turn/queue/steer", sessionId="$S1", queueItemId="$Q1",
                           expectedTurnId="$R1"),
                      send("session/turn/queue/remove", sessionId="$S1", queueItemId="$Q2"),
                      send("session/turn/queue/steer", sessionId="$S1", queueItemId="$Q2",
                           expectedTurnId="$R1"),
                      {"wait": True},
                      send("session/turn/enqueue", sessionId="$S1", entries=[user_entry("Idle")]),
                      {"sleep": 2}, PERSISTED],
        },
        {
            "name": "session/shell",
            "steps": [START,
                      send("session/shellCommand", sessionId="$S1", command="echo shell"),
                      {"sleep": 1}],
        },
        # -- session marks ---------------------------------------------------
        {
            "name": "session/marks",
            "backend": [{"text": "Hi."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/rename", sessionId="$S1", title="Renamed"),
                      send("session/pin", sessionId="$S1", pinned=True),
                      send("session/markAsSeen", sessionId="$S1"),
                      send("session/archive", sessionId="$S1", archived=True),
                      send("session/list"),
                      send("session/archive", sessionId="$S1", archived=False),
                      send("session/pin", sessionId="$S1", pinned=False),
                      PERSISTED],
        },
        # -- the store -------------------------------------------------------
        {
            "name": "store/restart-and-resume",
            "backend": [{"text": "Before."}, {"text": "After."}],
            "steps": [START, {"turn": "Before the restart"}, {"restart": True},
                      send("session/list"),
                      send("session/resume", sessionId="$S1"),
                      {"turn": "After the restart"}, PERSISTED],
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
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise rewind.OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
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
                raise rewind.OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except (rewind.OracleError, acp.OracleError) as error:
        print(f"unified harness oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
