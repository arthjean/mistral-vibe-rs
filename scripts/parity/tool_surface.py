#!/usr/bin/env python3
"""Capture the tool surface the pinned Python reference publishes.

The reference checkout is a read-only behavioural oracle. This script reads the
tool names it publishes for the running platform, the ``parameters`` object each
one sends to the model, and a set of argument fixtures with the verdict Pydantic
gives them. The Rust differential runner replays that corpus.

The corpus is a local artifact only: it holds reference-authored description
text, which ``NOTICE`` forbids shipping, so it is written to a gitignored path
and never committed.

Usage::

    scripts/parity/tool_surface.py --reference /path/to/reference
    scripts/parity/tool_surface.py --probe-endpoint   # needs MISTRAL_API_KEY

The wrapper re-executes itself with the reference interpreter when the current
one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
from typing import Any

SCHEMA_VERSION = 3
DIGEST_SCHEMA_VERSION = 1
DEFAULT_REFERENCE = Path("/home/arthur/dev/mistral-vibe")
DEFAULT_OUTPUT = Path(".parity/tool-surface-corpus.json")
DEFAULT_DIGEST = Path("crates/vibe-app-server/tests/tool-surface/digest.json")
#: Stands in for every description string, so the digest records that a
#: description exists without carrying reference prose into the repository.
DESCRIBED = "<described>"
EXPECTED_COMMIT = "68ff32e6a92e80a874c8153312f0aa8ae4955477"
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
        raise OracleError(f"reference checkout is missing: {reference}")
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
    from vibe.core.tools.manager import ShellToolPolicy, ToolManager

    init_harness_files_manager()
    config = VibeConfigSchema()
    with tempfile.TemporaryDirectory() as workdir:
        # An empty working directory keeps project-local tool and prompt
        # overrides out of the captured surface.
        def surface(policy: Any) -> dict[str, Any]:
            manager = ToolManager(
                lambda: config,
                defer_mcp=True,
                shell_policy=policy,
                cwd=Path(workdir),
            )
            return manager.available_tools

        available = surface(ShellToolPolicy())
        tools = [
            {"name": name, "parameters": available[name].get_parameters()}
            for name in sorted(available)
        ]
        fixtures = [
            fixture
            for name in sorted(available)
            for fixture in argument_fixtures(name, available[name])
        ]
        # The managed rollout is a second surface, not a second corpus: the
        # variant it selects for `bash` and the four session tools it adds are
        # only reachable with the experiment variant resolved to `managed`.
        managed_available = surface(
            ShellToolPolicy(
                managed_tools_enabled=lambda: True,
                local_managed_tools_enabled=True,
            )
        )
        managed_tools = [
            {"name": name, "parameters": managed_available[name].get_parameters()}
            for name in sorted(managed_available)
        ]
    conditions = {
        "managedShellRollout": False,
        "managedShellRolloutCaptured": True,
        "enabledTools": list(config.enabled_tools),
        "disabledTools": list(config.disabled_tools),
    }
    return tools, {
        "conditions": conditions,
        "fixtures": fixtures,
        "managedTools": managed_tools,
        "windowsTools": capture_windows_tools(),
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


def argument_fixtures(name: str, tool_class: Any) -> list[dict[str, Any]]:
    """Payloads spanning the reference's accept and reject envelope.

    The verdict is Pydantic's, taken from the reference model itself, so the
    Rust replay compares against measured behaviour rather than an assumption
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
                    "rejection": type(error).__name__,
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
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument(
        "--probe-endpoint",
        action="store_true",
        help="ask the live Mistral endpoint whether it accepts a reference-shaped schema",
    )
    return parser.parse_args()


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
        }
        if arguments.probe_endpoint:
            corpus["endpointProbe"] = probe_endpoint(tools)
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
                json.dumps(build_digest(corpus), indent=2, sort_keys=True, ensure_ascii=False)
                + "\n",
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
    if probe := corpus.get("endpointProbe"):
        print(f"endpoint probe: {json.dumps(probe, sort_keys=True)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
