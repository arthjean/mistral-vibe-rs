#!/usr/bin/env python3
"""Capture the tool surface the pinned Python reference publishes.

The reference checkout is a read-only behavioral oracle. This script reads the
tool names it publishes for the running platform, the ``parameters`` object each
one sends to the model, and a set of argument fixtures with the verdict Pydantic
gives them. The Rust differential runner replays that corpus.

The corpus is a local artifact only: it holds reference-authored description
text, which ``NOTICE`` forbids shipping, so it is written to a gitignored path
and never committed.

Usage::

    scripts/parity/tool_surface.py --reference /path/to/reference
    scripts/parity/tool_surface.py --probe-endpoint   # needs MISTRAL_API_KEY

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter when the current
one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 5
DIGEST_SCHEMA_VERSION = 1
FIXTURES_SCHEMA_VERSION = 2
GATES_SCHEMA_VERSION = 1
QUADRANTS_SCHEMA_VERSION = 1

#: The `enabled_tools` and `disabled_tools` pairs whose published names are
#: recorded. They span the four combinations of the two lists being written and
#: left out, and within the written ones they separate the list the operator
#: typed from the patterns it compiles to: a blank entry and an uncompilable
#: expression both narrow the surface without matching anything, which is the
#: state a filter reading its compiled patterns gets wrong in the widening
#: direction.
GATE_CASES: tuple[tuple[str, list[str], list[str]], ...] = (
    ("neither-list", [], []),
    ("blank-enabled", ["  "], []),
    ("uncompilable-enabled", ["re:("], []),
    ("blank-and-glob-enabled", ["  ", "read_*"], []),
    ("regex-enabled", ["re:read_.*"], []),
    ("blank-disabled", [], ["  "]),
    ("uncompilable-disabled", [], ["re:("]),
    ("glob-disabled", [], ["read_*"]),
    ("both-lists-match", ["read_*"], ["read_file"]),
    ("both-lists-written", ["read_*", "write_file"], ["  "]),
)
#: The four combinations of the managed-shell rollout and the runtime gate.
#: The rollout is the `managed_shell_tools_enabled` configuration field and the
#: gate is the `local_managed_shell_runtime_enabled` argument the manager reads
#: in `_is_tool_available`, which the reference runtime resolves from the tools
#: the client declares: a client hosting its own terminal turns it off, and the
#: manager then withholds every class carrying `local_managed_shell_only`.
#: Passing it explicitly is what makes the fourth combination visible at all;
#: inheriting its `True` default captures only the three where the gate
#: withholds nothing, which is the one quadrant where this port diverges.
QUADRANT_CASES: tuple[tuple[str, bool, bool], ...] = (
    ("rollout-off-gate-on", False, True),
    ("rollout-off-gate-off", False, False),
    ("rollout-on-gate-on", True, True),
    ("rollout-on-gate-off", True, False),
)
DEFAULT_OUTPUT = Path(".parity/tool-surface-corpus.json")
DEFAULT_DIGEST = Path("crates/vibe-app-server/tests/tool-surface/digest.json")
DEFAULT_FIXTURES = Path("crates/vibe-app-server/tests/tool-surface/fixtures.json")
DEFAULT_GATES = Path("crates/vibe-app-server/tests/tool-surface/gates.json")
DEFAULT_QUADRANTS = Path("crates/vibe-app-server/tests/tool-surface/quadrants.json")
#: Stands in for every description string, so the digest records that a
#: description exists without carrying reference prose into the repository.
DESCRIBED = "<described>"
PROBE_ENDPOINT = "https://api.mistral.ai/v1/chat/completions"
PROBE_MODEL = "mistral-medium-3.5"
UNEXPECTED_KEY = "__unexpected__"


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def resolve_reference(reference: Path, expected_commit: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE to the checkout "
            "path or pass --reference"
        )
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git rev-parse failed in {reference}: {result.stderr.strip()}"
        )
    commit = result.stdout.strip()
    if expected_commit and commit != expected_commit:
        raise OracleError(
            f"reference checkout is at {commit}, not the pinned {expected_commit}"
        )
    return {"path": str(reference), "commit": commit}


def reexecute_with_reference_interpreter(reference: Path, interpreter: Path | None) -> None:
    """Re-runs this script under the reference virtualenv when ``vibe`` is absent."""
    try:
        import vibe  # noqa: F401

        return
    except ImportError:
        pass
    candidate = interpreter or reference / ".venv/bin/python"
    if not candidate.is_file():
        raise OracleError(
            f"cannot import `vibe` and no reference interpreter at {candidate}"
        )
    if Path(sys.executable).resolve() == candidate.resolve():
        raise OracleError(f"{candidate} cannot import `vibe`")
    os.execv(str(candidate), [str(candidate), str(Path(__file__).resolve()), *sys.argv[1:]])


# --------------------------------------------------------------------------
# Surface capture
# --------------------------------------------------------------------------


def capture_tools(reference: Path) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    sys.path.insert(0, str(reference))
    from vibe.core.config import VibeConfigSchema
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.core.tools.manager import ToolManager

    init_harness_files_manager()
    config = VibeConfigSchema()
    with tempfile.TemporaryDirectory() as workdir:
        # An empty working directory keeps project-local tool and prompt
        # overrides out of the captured surface.
        #
        # v2.24.0 replaced the injected `ShellToolPolicy` with the
        # `managed_shell_tools_enabled` configuration field, so the rollout is
        # now selected by the configuration the manager reads rather than by a
        # constructor argument. The two surfaces below are the same two the
        # policy object used to select.
        # `runtime_enabled` has no default here on purpose: the manager's own
        # default is `True`, and inheriting it silently is what kept the gate
        # out of this census. Every call below states which quadrant it asks
        # for.
        def surface(rollout_config: Any, *, runtime_enabled: bool) -> dict[str, Any]:
            manager = ToolManager(
                lambda: rollout_config,
                defer_mcp=True,
                cwd=Path(workdir),
                local_managed_shell_runtime_enabled=runtime_enabled,
            )
            return manager.available_tools

        available = surface(config, runtime_enabled=True)
        tools = [
            {"name": name, "parameters": available[name].get_parameters()}
            for name in sorted(available)
        ]
        fixtures = [
            fixture
            for name in sorted(available)
            for fixture in argument_fixtures(name, available[name])
        ]
        multi_rejections = [
            probe
            for name in sorted(available)
            for probe in multi_argument_probes(name, available[name])
        ]
        # The managed rollout is a second surface, not a second corpus: the
        # variant it selects for `bash` and the four session tools it adds are
        # only reachable with the experiment variant resolved to `managed`.
        managed_available = surface(
            VibeConfigSchema(managed_shell_tools_enabled=True), runtime_enabled=True
        )
        managed_tools = [
            {"name": name, "parameters": managed_available[name].get_parameters()}
            for name in sorted(managed_available)
        ]
        # The two configured filters narrow the same surface, so the gate cases
        # are captured from the manager rather than from a list comprehension
        # over the default surface: what is measured is which names survive
        # `available_tools`, filters included.
        gates = [
            {
                "case": case,
                "enabledTools": list(enabled),
                "disabledTools": list(disabled),
                "names": sorted(
                    surface(
                        VibeConfigSchema(
                            enabled_tools=list(enabled),
                            disabled_tools=list(disabled),
                        ),
                        runtime_enabled=True,
                    )
                ),
            }
            for case, enabled, disabled in GATE_CASES
        ]
        # The census the three surfaces above could not take: the rollout and
        # the gate are independent, and only their fourth combination collapses
        # the shell surface. Names and serving classes only, because a quadrant
        # answers which tools exist and which class won, not what they say.
        quadrants = [
            quadrant_record(
                case,
                rollout,
                runtime_enabled,
                surface(
                    VibeConfigSchema(managed_shell_tools_enabled=rollout),
                    runtime_enabled=runtime_enabled,
                ),
            )
            for case, rollout, runtime_enabled in QUADRANT_CASES
        ]
    conditions = {
        "managedShellRollout": False,
        "managedShellRolloutCaptured": True,
        "localManagedShellRuntimeEnabled": True,
        "quadrantsCaptured": len(QUADRANT_CASES),
        "enabledTools": list(config.enabled_tools),
        "disabledTools": list(config.disabled_tools),
    }
    return tools, {
        "conditions": conditions,
        "fixtures": fixtures,
        "gates": gates,
        "quadrants": quadrants,
        "multiRejections": multi_rejections,
        "managedTools": managed_tools,
        "windowsTools": capture_windows_tools(),
    }


def quadrant_record(
    case: str, rollout: bool, runtime_enabled: bool, available: dict[str, Any]
) -> dict[str, Any]:
    """One quadrant, labeled by the two flags that produced it.

    A shell name is one whose serving class declares `shell_rollout`, which is
    the marker the manager itself filters on, so the recorded shell surface is
    the set the rollout and the gate can move rather than a list written here.
    The class per name is recorded beside it because the fourth quadrant is not
    only a shorter list: the name that survives is served by another class.
    """

    shell = {
        name: tool_class.__name__
        for name, tool_class in available.items()
        if getattr(tool_class, "shell_rollout", None) is not None
    }
    return {
        "case": case,
        "managedShellRollout": rollout,
        "localManagedShellRuntimeEnabled": runtime_enabled,
        "names": sorted(available),
        "shellNames": sorted(shell),
        "shellClasses": dict(sorted(shell.items())),
    }


# --------------------------------------------------------------------------
# Windows-only families
# --------------------------------------------------------------------------


def capture_windows_tools() -> list[dict[str, Any]]:
    """The names and schemas the two Windows-only families publish.

    ``available_tools`` cannot answer for them off Windows: their
    ``is_available`` reads the running platform and the shell it finds, so a
    Linux capture would record nothing at all. The declarations are read from
    the reference classes instead, and the variant per name is resolved the way
    ``ToolManager._select_available_variant`` resolves it — highest
    ``selection_priority`` first, discovery order breaking a tie — under the
    host the classes are written for: Windows, with the family's shell present
    and the managed backend supported, which is the only state where all five
    names of a family are available at once.
    """
    from vibe.core.tools.base import BaseTool
    from vibe.core.tools.builtins import git_bash, windows_shell

    captured: list[dict[str, Any]] = []
    for family, module in (("git_bash", git_bash), ("powershell", windows_shell)):
        ranked: dict[str, tuple[tuple[int, int], Any]] = {}
        for index, value in enumerate(vars(module).values()):
            if (
                not isinstance(value, type)
                or not issubclass(value, BaseTool)
                or value.__module__ != module.__name__
            ):
                continue
            name = value.get_name()
            rank = (value.selection_priority, index)
            if name not in ranked or rank > ranked[name][0]:
                ranked[name] = (rank, value)
        captured.extend(
            {
                "family": family,
                "name": name,
                "parameters": tool_class.get_parameters(),
            }
            for name, (_rank, tool_class) in sorted(ranked.items())
        )
    return captured


# --------------------------------------------------------------------------
# Argument fixtures
# --------------------------------------------------------------------------


def resolve_ref(schema: dict[str, Any], root: dict[str, Any]) -> dict[str, Any]:
    seen = 0
    while "$ref" in schema and seen < 16:
        name = str(schema["$ref"]).removeprefix("#/$defs/")
        schema = root.get("$defs", {}).get(name, {})
        seen += 1
    return schema


def concrete_branch(schema: dict[str, Any], root: dict[str, Any]) -> dict[str, Any]:
    """The non-null branch of a nullable property, or the property itself."""
    schema = resolve_ref(schema, root)
    for branch in schema.get("anyOf", []):
        resolved = resolve_ref(branch, root)
        if resolved.get("type") != "null":
            return resolved
    return schema


def sample_value(schema: dict[str, Any], root: dict[str, Any], depth: int = 0) -> Any:
    schema = concrete_branch(schema, root)
    if depth > 8:
        return None
    if enum := schema.get("enum"):
        return enum[0]
    declared = schema.get("type")
    if isinstance(declared, list):
        declared = next((entry for entry in declared if entry != "null"), None)
    match declared:
        case "string":
            return "sample"
        case "integer":
            return max(1, int(schema.get("minimum", 1)))
        case "number":
            return 1.5
        case "boolean":
            return True
        case "array":
            item = schema.get("items", {"type": "string"})
            count = max(1, int(schema.get("minItems", 1)))
            return [sample_value(item, root, depth + 1) for _ in range(count)]
        case "object":
            return {
                name: sample_value(subschema, root, depth + 1)
                for name, subschema in schema.get("properties", {}).items()
                if name in schema.get("required", [])
            }
        case _:
            return "sample"


def mismatched_value(schema: dict[str, Any], root: dict[str, Any]) -> Any:
    schema = concrete_branch(schema, root)
    declared = schema.get("type")
    if isinstance(declared, list):
        declared = next((entry for entry in declared if entry != "null"), None)
    match declared:
        case "string":
            return 17
        case "integer" | "number":
            return "seventeen"
        case "boolean":
            return "yes"
        case "array":
            return {"not": "an array"}
        case _:
            return []


def render_pointer(location: tuple[Any, ...]) -> str:
    """A validation location in the pointer spelling this repository writes.

    Pydantic reports a location as a tuple of field names and indices. The
    rendering is authored here, in the `$.field[0].sub` form this port's own
    violations carry, so the corpus records where the reference objected
    without recording a single character it wrote.
    """
    rendered = "$"
    for step in location:
        if isinstance(step, int):
            rendered += f"[{step}]"
        else:
            rendered += f".{step}"
    return rendered


def rejection_summary(
    name: str, tool_class: Any, payload: dict[str, Any], error: Exception
) -> dict[str, Any]:
    """What the reference answers a rejected call, recorded structurally.

    The message itself is reference-authored prose and `NOTICE` forbids
    committing it, so what is stored is its shape: the exception type the model
    sees, whether the text names the tool that rejected the call, which
    arguments it objected to, and a digest of the full string so a change
    upstream is still caught. The wrapper is driven rather than reconstructed:
    ``invoke`` validates before it reaches ``run``, so advancing the generator
    once over an already-rejected payload measures the error a model would read
    without executing anything.
    """

    locations = [
        tuple(entry.get("loc", ())) for entry in getattr(error, "errors", lambda: [])()
    ]
    text = ""
    kind = type(error).__name__
    try:
        config_class = tool_class._get_tool_config_class()
        instance = tool_class.from_config(config_class)
        stream = instance.invoke(**payload)

        async def advance() -> None:
            async for _ in stream:
                break

        asyncio.run(advance())
    except Exception as wrapped:  # noqa: BLE001 - the wrapper is the measurement
        kind = type(wrapped).__name__
        text = str(wrapped)
    return {
        "error": kind,
        "namesTool": name in text,
        "arguments": sorted(
            {str(location[0]) for location in locations if location and isinstance(location[0], str)}
        ),
        "pointers": sorted({render_pointer(location) for location in locations}),
        "digest": "sha256:" + hashlib.sha256(text.encode("utf-8")).hexdigest(),
    }


def multi_argument_probes(name: str, tool_class: Any) -> list[dict[str, Any]]:
    """Payloads breaking more than one argument at once, and the answer they get.

    Every fixture in :func:`argument_fixtures` breaks a single argument, so the
    fixture set cannot say whether a rejection names every argument it objected
    to or stops at the first. These probes ask that question directly: the
    payload is the valid one with every declared property overwritten by a value
    of the wrong type, so more than one property is wrong at once, and what is
    recorded is the pointer set the reference reports back.

    They are kept out of the fixture list on purpose. A fixture is an
    accept-or-reject verdict and the committed set is a fixed conformance count;
    a probe is a second measurement over the same surface, so it is recorded
    beside them rather than mixed in.
    """
    root = tool_class.get_parameters()
    properties: dict[str, Any] = root.get("properties", {})
    if len(properties) < 2:
        return []
    required: list[str] = list(root.get("required", []))
    payload = {field: sample_value(properties[field], root) for field in required} | {
        field: mismatched_value(subschema, root)
        for field, subschema in properties.items()
    }
    try:
        tool_class.validate_arguments(payload)
    except Exception as error:  # noqa: BLE001 - the rejection is the measurement
        rejection = rejection_summary(name, tool_class, payload, error)
    else:
        return []
    if len(rejection["pointers"]) < 2:
        return []
    return [
        {
            "tool": name,
            "case": "mismatched-every-property",
            "arguments": payload,
            "rejection": rejection,
        }
    ]


def argument_fixtures(name: str, tool_class: Any) -> list[dict[str, Any]]:
    """Payloads spanning the reference's accept and reject envelope.

    The verdict is Pydantic's, taken from the reference model itself, so the
    Rust replay compares against measured behavior rather than an assumption
    about what a schema means.
    """
    root = tool_class.get_parameters()
    properties: dict[str, Any] = root.get("properties", {})
    required: list[str] = list(root.get("required", []))
    valid = {field: sample_value(properties[field], root) for field in required}

    candidates: list[tuple[str, Any]] = [("required-only", valid)]
    for field in required:
        candidates.append((f"missing-{field}", {k: v for k, v in valid.items() if k != field}))
    for field, subschema in properties.items():
        candidates.append((f"mismatched-{field}", valid | {field: mismatched_value(subschema, root)}))
        candidates.append((f"null-{field}", valid | {field: None}))
        concrete = concrete_branch(subschema, root)
        if concrete.get("enum"):
            candidates.append((f"unknown-enum-{field}", valid | {field: "__not_a_member__"}))
        if concrete.get("type") == "array":
            candidates.append((f"empty-{field}", valid | {field: []}))
            item = concrete_branch(concrete.get("items", {}), root)
            if item.get("type") == "object" and item.get("required"):
                incomplete = sample_value(item, root)
                incomplete.pop(item["required"][0], None)
                candidates.append((f"incomplete-item-{field}", valid | {field: [incomplete]}))
    candidates.append(("unexpected-key", valid | {UNEXPECTED_KEY: 1}))

    fixtures = []
    for case, payload in candidates:
        try:
            tool_class.validate_arguments(payload)
        except Exception as error:  # noqa: BLE001 - the verdict is what matters
            fixtures.append(
                {
                    "tool": name,
                    "case": case,
                    "arguments": payload,
                    "accepted": False,
                    "rejection": rejection_summary(name, tool_class, payload, error),
                }
            )
        else:
            fixtures.append(
                {"tool": name, "case": case, "arguments": payload, "accepted": True}
            )
    return fixtures


# --------------------------------------------------------------------------
# Committed digest
# --------------------------------------------------------------------------


def canonicalize(value: Any) -> Any:
    """The schema with every description replaced by a sentinel.

    ``NOTICE`` forbids shipping reference prose, and the Rust runner already
    compares descriptions by presence only. Applying the same rule here is what
    makes the digest committable: it carries names and schema structure, and the
    only thing it says about a description is that there is one.
    """
    if isinstance(value, dict):
        return {
            key: DESCRIBED if key == "description" and isinstance(item, str) else canonicalize(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [canonicalize(item) for item in value]
    return value


def build_digest(corpus: dict[str, Any]) -> dict[str, Any]:
    """The committed conformance target: every published name and its structure.

    CI has no pinned Python checkout, so the corpus cannot be recaptured there.
    The digest is what the Rust runner diffs the published surface against when
    no oracle is reachable, which is what makes a schema change fail the
    pipeline instead of skipping it.
    """
    windows: dict[str, dict[str, Any]] = {}
    for tool in corpus["windowsTools"]:
        windows.setdefault(tool["family"], {})[tool["name"]] = canonicalize(tool["parameters"])
    return {
        "schemaVersion": DIGEST_SCHEMA_VERSION,
        "referenceCommit": corpus["reference"]["commit"],
        "platform": corpus["platform"],
        "note": (
            "Canonical tool-surface digest: published names and schema structure only, with "
            "every description replaced by a sentinel so no reference prose is committed. "
            "Regenerate with scripts/parity/tool_surface.py --digest when the pinned reference "
            "moves."
        ),
        "tools": {tool["name"]: canonicalize(tool["parameters"]) for tool in corpus["tools"]},
        "managedTools": {
            tool["name"]: canonicalize(tool["parameters"]) for tool in corpus["managedTools"]
        },
        "windowsTools": windows,
    }


def build_fixtures(corpus: dict[str, Any]) -> dict[str, Any]:
    """The committed argument fixtures: payloads and the verdict Pydantic gave.

    Committable on the same terms as the digest, and for a stricter reason: no
    part of a fixture is reference-authored. The payloads are built by
    :func:`argument_fixtures` from the schema alone, the case names are written
    here, and the verdict is a boolean. What the fixture set carries about the
    reference is the accept-or-reject decision, which is the measurement.

    A rejected fixture carries a second measurement: the shape of the error the
    model reads back, recorded by :func:`rejection_summary` as an exception
    type, a flag, a list of argument names and a digest. None of it is the
    reference's own text.

    The replay needs a schema, which it reads from the committed digest, so a
    fixture and the schema it is validated against always describe the same
    pinned surface.
    """
    return {
        "schemaVersion": FIXTURES_SCHEMA_VERSION,
        "referenceCommit": corpus["reference"]["commit"],
        "platform": corpus["platform"],
        "note": (
            "Argument fixtures with the verdict the reference Pydantic gave them. Payloads and "
            "case names are authored by scripts/parity/tool_surface.py; the captured values are "
            "the accepted flag and, for a rejection, the shape of the error the model reads: its "
            "type, whether it names the tool, which arguments it objected to, and a digest of "
            "the text. multiArgument holds the same measurement for payloads breaking more "
            "than one argument at once, which is what says whether a rejection names every "
            "argument or only the first. Schemas come from digest.json. Regenerate with "
            "scripts/parity/tool_surface.py --fixtures when the pinned reference moves."
        ),
        "fixtures": sorted(
            (
                {
                    "tool": fixture["tool"],
                    "case": fixture["case"],
                    "arguments": fixture["arguments"],
                    "accepted": fixture["accepted"],
                }
                | ({} if fixture["accepted"] else {"rejection": fixture["rejection"]})
                for fixture in corpus["fixtures"]
            ),
            key=lambda fixture: (fixture["tool"], fixture["case"]),
        ),
        "multiArgument": sorted(
            corpus["multiRejections"],
            key=lambda probe: (probe["tool"], probe["case"]),
        ),
    }


def build_gates(corpus: dict[str, Any]) -> dict[str, Any]:
    """The names the reference publishes under each configured filter pair.

    Every value here is an observation: the two lists are written by
    :data:`GATE_CASES` and the names are the reference's answer to them. The
    corpus is what holds this port's gate to the reference's, which reads the
    written list rather than the patterns it compiles to.
    """
    return {
        "schemaVersion": GATES_SCHEMA_VERSION,
        "referenceCommit": corpus["reference"]["commit"],
        "platform": corpus["platform"],
        "note": (
            "Published tool names per `enabled_tools` and `disabled_tools` pair, captured from "
            "the pinned reference by scripts/parity/tool_surface.py. The lists are authored "
            "here; the names are the reference's answer. Regenerate with "
            "scripts/parity/tool_surface.py --gates when the pinned reference moves."
        ),
        "gates": corpus["gates"],
    }


def build_quadrants(corpus: dict[str, Any]) -> dict[str, Any]:
    """The published surface under each rollout and runtime-gate combination.

    Committable on the same terms as the gate corpus: every value is a name or
    a class name the reference answered, and no description reaches it. It is
    what holds this port's publication gate to the reference's, including the
    combination where the gate collapses five shell names to one.
    """

    return {
        "schemaVersion": QUADRANTS_SCHEMA_VERSION,
        "referenceCommit": corpus["reference"]["commit"],
        "platform": corpus["platform"],
        "note": (
            "Published tool names per managed-shell rollout and runtime gate, captured from the "
            "pinned reference by scripts/parity/tool_surface.py. The two flags are authored "
            "here; the names and the class serving each shell name are the reference's answer. "
            "Regenerate with scripts/parity/tool_surface.py --quadrants when the pinned "
            "reference moves."
        ),
        "quadrants": corpus["quadrants"],
    }


# --------------------------------------------------------------------------
# Live endpoint probe
# --------------------------------------------------------------------------


def probe_endpoint(tools: list[dict[str, Any]]) -> dict[str, Any]:
    """Sends one reference-shaped schema to the live endpoint.

    Answers the PRD's open question on whether ``$defs``, ``$ref``, ``anyOf``
    and ``default`` survive the wire, which the API documentation does not say.
    """
    import urllib.error
    import urllib.request

    key = os.environ.get("MISTRAL_API_KEY")
    if not key:
        return {"ran": False, "reason": "MISTRAL_API_KEY is not set"}
    probed = next(
        (
            tool
            for tool in tools
            if "$defs" in tool["parameters"]
            and any(
                "anyOf" in value or "default" in value
                for value in tool["parameters"].get("properties", {}).values()
            )
        ),
        None,
    )
    if probed is None:
        return {"ran": False, "reason": "no captured schema carries $defs, anyOf and default"}
    body = json.dumps(
        {
            "model": PROBE_MODEL,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "reply with ok"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": probed["name"],
                        "description": "schema acceptance probe",
                        "parameters": probed["parameters"],
                    },
                }
            ],
        }
    ).encode()
    request = urllib.request.Request(
        PROBE_ENDPOINT,
        data=body,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return {"ran": True, "accepted": True, "status": response.status, "tool": probed["name"]}
    except urllib.error.HTTPError as error:
        return {
            "ran": True,
            "accepted": False,
            "status": error.code,
            "tool": probed["name"],
            "detail": error.read().decode(errors="replace")[:500],
        }
    except urllib.error.URLError as error:
        return {"ran": False, "reason": f"endpoint unreachable: {error.reason}"}


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--digest",
        type=Path,
        nargs="?",
        const=DEFAULT_DIGEST,
        default=None,
        help=(
            "also write the committed canonical digest, which CI diffs against when no "
            f"reference checkout is reachable (default {DEFAULT_DIGEST})"
        ),
    )
    parser.add_argument(
        "--fixtures",
        type=Path,
        nargs="?",
        const=DEFAULT_FIXTURES,
        default=None,
        help=(
            "also write the committed argument fixtures, which the Rust replay reads "
            f"unconditionally (default {DEFAULT_FIXTURES})"
        ),
    )
    parser.add_argument(
        "--gates",
        type=Path,
        nargs="?",
        const=DEFAULT_GATES,
        default=None,
        help=(
            "also write the committed filter-gate corpus, which the Rust replay reads "
            f"unconditionally (default {DEFAULT_GATES})"
        ),
    )
    parser.add_argument(
        "--quadrants",
        type=Path,
        nargs="?",
        const=DEFAULT_QUADRANTS,
        default=None,
        help=(
            "also write the committed rollout and runtime-gate quadrants, which the Rust "
            f"replay reads unconditionally (default {DEFAULT_QUADRANTS})"
        ),
    )
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "capture and compare against the committed artifacts instead of rewriting them; "
            "a difference exits non-zero, which is what proves a re-run with no change in "
            "between is byte-identical"
        ),
    )
    parser.add_argument(
        "--probe-endpoint",
        action="store_true",
        help="ask the live Mistral endpoint whether it accepts a reference-shaped schema",
    )
    return parser.parse_args()


def rendered(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def check_committed(corpus: dict[str, Any], arguments: argparse.Namespace) -> int:
    """Compares a fresh capture against every committed artifact, writing none.

    This is the byte-identity proof: the four files below are what the Rust
    replay reads, so a capture whose output moved without the reference moving
    is a failure here rather than a diff someone notices later.
    """

    targets = (
        ("digest", arguments.digest or DEFAULT_DIGEST, build_digest),
        ("fixtures", arguments.fixtures or DEFAULT_FIXTURES, build_fixtures),
        ("gates", arguments.gates or DEFAULT_GATES, build_gates),
        ("quadrants", arguments.quadrants or DEFAULT_QUADRANTS, build_quadrants),
    )
    differing = []
    for label, target, builder in targets:
        if not target.is_file():
            raise OracleError(f"no committed {label} at {target} to check against")
        if target.read_text(encoding="utf-8") != rendered(builder(corpus)):
            differing.append(f"{label} at {target}")
    if differing:
        raise OracleError("a fresh capture differs from " + ", ".join(differing))
    print(
        "the committed digest, fixtures, gates and quadrants match a fresh capture of "
        f"{len(corpus['tools'])} tools and {len(corpus['quadrants'])} quadrants"
    )
    return 0


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        reexecute_with_reference_interpreter(arguments.reference, arguments.python)
        tools, extra = capture_tools(arguments.reference)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
            "conditions": extra["conditions"],
            "tools": tools,
            "managedTools": extra["managedTools"],
            "windowsTools": extra["windowsTools"],
            "fixtures": extra["fixtures"],
            "gates": extra["gates"],
            "quadrants": extra["quadrants"],
            "multiRejections": extra["multiRejections"],
        }
        if arguments.probe_endpoint:
            corpus["endpointProbe"] = probe_endpoint(tools)
        if arguments.check:
            return check_committed(corpus, arguments)
        output = arguments.output
        output.parent.mkdir(parents=True, exist_ok=True)
        # The Rust runner captures once per test and its tests run concurrently,
        # so a truncating write would let one test read what another is still
        # writing. Rename over the target instead: it is atomic on POSIX.
        staged = output.with_name(f"{output.name}.{os.getpid()}.tmp")
        staged.write_text(
            json.dumps(corpus, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        os.replace(staged, output)
        # Written only on request: the Rust runner recaptures the corpus on
        # every run, and regenerating the committed digest with it would let a
        # schema change rewrite its own conformance target.
        if arguments.digest is not None:
            arguments.digest.parent.mkdir(parents=True, exist_ok=True)
            arguments.digest.write_text(
                rendered(build_digest(corpus)),
                encoding="utf-8",
            )
        # Written on request for the same reason as the digest: the fixtures are
        # a conformance target, and recapturing them on every run would let a
        # validation change rewrite the verdict it is supposed to be held to.
        if arguments.fixtures is not None:
            arguments.fixtures.parent.mkdir(parents=True, exist_ok=True)
            arguments.fixtures.write_text(
                rendered(build_fixtures(corpus)),
                encoding="utf-8",
            )
        # Written on request for the same reason: the gate corpus is what the
        # filter is held to, so it is regenerated deliberately rather than by
        # every run of the replay that reads it.
        if arguments.gates is not None:
            arguments.gates.parent.mkdir(parents=True, exist_ok=True)
            arguments.gates.write_text(
                rendered(build_gates(corpus)),
                encoding="utf-8",
            )
        # Written on request for the same reason as the gates: the quadrants
        # are what the publication gate is held to, and the fourth one is a
        # divergence this port has yet to close, so it is regenerated
        # deliberately rather than by the replay that reads it.
        if arguments.quadrants is not None:
            arguments.quadrants.parent.mkdir(parents=True, exist_ok=True)
            arguments.quadrants.write_text(
                rendered(build_quadrants(corpus)),
                encoding="utf-8",
            )
    except OracleError as error:
        print(f"tool-surface capture failed: {error}", file=sys.stderr)
        return 1
    print(
        f"captured {len(tools)} tools, {len(extra['managedTools'])} managed tools, "
        f"{len(extra['windowsTools'])} Windows tools and {len(extra['fixtures'])} fixtures "
        f"from {reference['commit'][:12]} into {output}"
    )
    if arguments.digest is not None:
        print(f"wrote the canonical digest to {arguments.digest}")
    if arguments.fixtures is not None:
        print(f"wrote the argument fixtures to {arguments.fixtures}")
    if arguments.gates is not None:
        print(f"wrote {len(extra['gates'])} filter gates to {arguments.gates}")
    if arguments.quadrants is not None:
        print(f"wrote {len(extra['quadrants'])} surface quadrants to {arguments.quadrants}")
    if probe := corpus.get("endpointProbe"):
        print(f"endpoint probe: {json.dumps(probe, sort_keys=True)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
