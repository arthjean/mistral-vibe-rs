#!/usr/bin/env python3
"""Capture how the pinned Python reference composes configuration layers.

The reference checkout is a read-only behavioural oracle. This script drives the
real ``ConfigBuilder`` merge over synthetic in-memory layers and records, for
each scenario, the merged document the builder hands to validation. The Rust
differential runner replays that corpus against ``LayeredConfig::load``.

It also records the field census: every field the reference schema declares,
with its merge strategy, merge key, editor kind and popular flag. Those are
observations, not authored prose, so unlike the tool-surface corpus this one is
committed. Field *descriptions* are never captured: ``NOTICE`` forbids shipping
reference-authored text.

Usage::

    scripts/parity/config_surface.py --reference /path/to/reference
    scripts/parity/config_surface.py --interpreter /path/to/python

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with an interpreter that can import ``vibe`` when
the current one cannot.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib
from typing import Any

SCHEMA_VERSION = 1
#: Where the read-only reference checkout lives. ``VIBE_REFERENCE`` overrides the
#: default for machines that hold it elsewhere, and ``--reference`` wins over both.
DEFAULT_REFERENCE = Path(
    os.environ.get("VIBE_REFERENCE") or "/home/arthur/dev/mistral-vibe"
)
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/config-surface/corpus.json")
EXPECTED_COMMIT = "68ff32e6a92e80a874c8153312f0aa8ae4955477"
#: The strategies the reference vocabulary declares but no field adopts, so the
#: Rust port implements neither. The census asserts this stays true.
UNREACHABLE_STRATEGIES = ("merge", "conflict")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"


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
    return {"commit": commit}


def reexecute_with_reference_interpreter(
    reference: Path, interpreter: Path | None
) -> None:
    """Re-runs this script under an interpreter that can import ``vibe``.

    ``subprocess`` rather than ``os.execv`` because this script is also run on
    Windows, where an exec'd process loses the parent's console exit code.
    """
    # The reference source tree supplies the package; an interpreter only has to
    # supply its dependencies.
    if str(reference) not in sys.path:
        sys.path.insert(0, str(reference))
    try:
        import vibe.core.config.vibe_schema  # noqa: F401

        return
    except ImportError:
        pass
    candidates = [
        path
        for path in (
            interpreter,
            Path(os.environ[INTERPRETER_VARIABLE])
            if os.environ.get(INTERPRETER_VARIABLE)
            else None,
            reference / ".venv/bin/python",
            reference / ".venv/Scripts/python.exe",
        )
        if path is not None
    ]
    for candidate in candidates:
        if not candidate.is_file():
            continue
        if Path(sys.executable).resolve() == candidate.resolve():
            raise OracleError(f"{candidate} cannot import `vibe`")
        result = subprocess.run(
            [str(candidate), str(Path(__file__).resolve()), *sys.argv[1:]],
            check=False,
        )
        sys.exit(result.returncode)
    raise OracleError(
        "cannot import `vibe` and no usable interpreter among: "
        + ", ".join(str(path) for path in candidates)
    )


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------

#: Layer stacks replayed against the reference merge. Every value here is
#: authored for this corpus; none is read from the reference.
#:
#: ``models`` is deliberately absent: the reference runs a normalisation
#: validator over it before merging, which US-065 restores. Deep merge is
#: therefore exercised through ``tools``, the other ``deep_merge`` field.
SCENARIOS: list[dict[str, Any]] = [
    {
        "name": "defaults-only",
        "layers": [
            (
                "defaults",
                """
theme = "system"
enable_telemetry = true
api_timeout = 30.0
auto_compact_threshold = 120000
disabled_tools = []
""",
            )
        ],
    },
    {
        "name": "single-layer-concat",
        "layers": [("user", 'disabled_tools = ["bash", "edit"]\n')],
    },
    {
        "name": "concat-two-layers",
        "layers": [
            ("user", 'disabled_tools = ["bash"]\n'),
            ("project", 'disabled_tools = ["edit", "web_search"]\n'),
        ],
    },
    {
        "name": "concat-preserves-duplicates",
        "layers": [
            ("user", 'disabled_tools = ["bash", "edit"]\n'),
            ("project", 'disabled_tools = ["bash"]\n'),
        ],
    },
    {
        "name": "concat-lower-layer-only",
        "layers": [
            ("user", 'disabled_agents = ["plan"]\n'),
            ("project", 'theme = "nord"\n'),
        ],
    },
    {
        "name": "concat-higher-layer-only",
        "layers": [
            ("user", 'theme = "nord"\n'),
            ("project", 'disabled_agents = ["plan"]\n'),
        ],
    },
    {
        "name": "concat-empty-table-is-absent",
        "layers": [
            ("user", 'installed_agents = ["plan"]\n'),
            ("project", "[installed_agents]\n"),
        ],
    },
    {
        "name": "concat-empty-table-in-lower-layer",
        "layers": [
            ("user", "[installed_agents]\n"),
            ("project", 'installed_agents = ["plan"]\n'),
        ],
    },
    {
        "name": "concat-every-path-list",
        "layers": [
            (
                "user",
                'tool_paths = ["~/tools"]\nagent_paths = ["~/agents"]\nskill_paths = ["~/skills"]\n',
            ),
            (
                "project",
                'tool_paths = [".vibe/tools"]\nagent_paths = [".vibe/agents"]\nskill_paths = [".vibe/skills"]\n',
            ),
        ],
    },
    {
        "name": "concat-applied-migrations",
        "layers": [
            ("user", 'applied_migrations = ["read_only_commands"]\n'),
            ("project", 'applied_migrations = ["tool_rename"]\n'),
        ],
    },
    {
        "name": "concat-agent-and-skill-lists",
        "layers": [
            (
                "user",
                'enabled_agents = ["custom-*"]\nenabled_skills = ["search-*"]\ndisabled_skills = ["legacy"]\n',
            ),
            (
                "project",
                'enabled_agents = ["review"]\nenabled_skills = ["deploy"]\ndisabled_skills = ["legacy"]\n',
            ),
        ],
    },
    {
        "name": "replace-scalars-across-four-layers",
        "layers": [
            ("defaults", 'theme = "system"\nenable_telemetry = true\napi_timeout = 30.0\n'),
            ("user", 'theme = "nord"\n'),
            ("project", 'theme = "dracula"\napi_timeout = 45.5\n'),
            ("overrides", 'theme = "gruvbox"\n'),
        ],
    },
    {
        "name": "replace-enabled-tools-is-not-concat",
        "layers": [
            ("user", 'enabled_tools = ["bash", "edit"]\n'),
            ("project", 'enabled_tools = ["read_file"]\n'),
        ],
    },
    {
        "name": "replace-nested-table-wholesale",
        "layers": [
            ("user", "[project_context]\nenabled = true\nmax_files = 40\n"),
            ("project", "[project_context]\nenabled = false\n"),
        ],
    },
    {
        "name": "replace-booleans-and-numbers",
        "layers": [
            (
                "defaults",
                "enable_otel = false\nauto_compact_threshold = 120000\napi_retry_max_elapsed_time = 60.0\n",
            ),
            (
                "user",
                "enable_otel = true\nauto_compact_threshold = 90000\napi_retry_max_elapsed_time = 12.5\n",
            ),
        ],
    },
    {
        "name": "union-providers-same-name-replaces-entry",
        "layers": [
            (
                "defaults",
                '[[providers]]\nname = "mistral"\napi_base = "https://api.mistral.ai/v1"\napi_key_env_var = "MISTRAL_API_KEY"\n',
            ),
            (
                "user",
                '[[providers]]\nname = "mistral"\napi_base = "https://proxy.example.test/v1"\n',
            ),
        ],
    },
    {
        "name": "union-providers-distinct-names",
        "layers": [
            (
                "defaults",
                '[[providers]]\nname = "mistral"\napi_base = "https://api.mistral.ai/v1"\n',
            ),
            (
                "user",
                '[[providers]]\nname = "llamacpp"\napi_base = "http://127.0.0.1:8080/v1"\n',
            ),
        ],
    },
    {
        "name": "union-preserves-first-seen-order",
        "layers": [
            (
                "defaults",
                '[[providers]]\nname = "alpha"\napi_base = "https://alpha.example.test"\n\n[[providers]]\nname = "beta"\napi_base = "https://beta.example.test"\n',
            ),
            (
                "user",
                '[[providers]]\nname = "gamma"\napi_base = "https://gamma.example.test"\n\n[[providers]]\nname = "alpha"\napi_base = "https://alpha.override.test"\n',
            ),
        ],
    },
    {
        "name": "union-transcribe-models-by-alias",
        "layers": [
            (
                "defaults",
                '[[transcribe_models]]\nname = "voxtral-mini"\nprovider = "mistral"\nalias = "voxtral-realtime"\n',
            ),
            (
                "user",
                '[[transcribe_models]]\nname = "voxtral-large"\nprovider = "mistral"\nalias = "voxtral-realtime"\n\n[[transcribe_models]]\nname = "voxtral-tiny"\nprovider = "mistral"\nalias = "tiny"\n',
            ),
        ],
    },
    {
        "name": "union-tts-providers-and-models",
        "layers": [
            (
                "defaults",
                '[[tts_providers]]\nname = "mistral"\napi_base = "https://api.mistral.ai"\n\n[[tts_models]]\nname = "voxtral-mini-tts"\nprovider = "mistral"\nalias = "voxtral-tts"\n',
            ),
            (
                "user",
                '[[tts_providers]]\nname = "local"\napi_base = "http://127.0.0.1:9000"\n\n[[tts_models]]\nname = "voxtral-mini-tts"\nprovider = "mistral"\nalias = "voxtral-tts"\n',
            ),
        ],
    },
    {
        "name": "union-empty-table-is-absent",
        "layers": [
            (
                "user",
                '[[transcribe_providers]]\nname = "mistral"\napi_base = "wss://api.mistral.ai"\n',
            ),
            ("project", "[transcribe_providers]\n"),
        ],
    },
    {
        "name": "union-lower-layer-only",
        "layers": [
            (
                "user",
                '[[tts_providers]]\nname = "mistral"\napi_base = "https://api.mistral.ai"\n',
            ),
            ("project", 'theme = "nord"\n'),
        ],
    },
    {
        "name": "union-mcp-servers-distinct-names",
        "layers": [
            (
                "user",
                '[[mcp_servers]]\nname = "docs"\ntransport = "streamable-http"\nurl = "https://mcp.example.test/rpc"\n',
            ),
            (
                "project",
                '[[mcp_servers]]\nname = "local"\ntransport = "stdio"\ncommand = "/usr/bin/local-mcp"\n',
            ),
        ],
    },
    {
        "name": "deep-merge-tools-two-layers",
        "layers": [
            (
                "user",
                '[tools.bash]\nallowlist = ["git status"]\ntimeout = 30\n\n[tools.edit]\nconfirm = true\n',
            ),
            ("project", '[tools.bash]\nallowlist = ["cargo test"]\n'),
        ],
    },
    {
        "name": "deep-merge-tools-three-layers",
        "layers": [
            ("defaults", "[tools.bash]\ntimeout = 30\n"),
            ("user", '[tools.bash]\nallowlist = ["git status"]\n'),
            ("project", "[tools.bash]\ntimeout = 60\n\n[tools.web_search]\nenabled = false\n"),
        ],
    },
    {
        "name": "deep-merge-preserves-absent-keys",
        "layers": [
            ("user", "[tools.read_file]\nmax_bytes = 100000\nline_numbers = true\n"),
            ("project", "[tools.read_file]\nline_numbers = false\n"),
        ],
    },
    {
        "name": "unregistered-keys-are-dropped",
        "layers": [
            ("user", 'theme = "nord"\nfuture_key = "kept-by-rust"\n\n[future_table]\nnested = 1\n'),
        ],
    },
    {
        "name": "empty-layers-are-skipped",
        "layers": [
            ("defaults", 'theme = "system"\n'),
            ("empty", ""),
            ("user", 'disabled_tools = ["bash"]\n'),
            ("also-empty", ""),
            ("project", 'disabled_tools = ["edit"]\n'),
        ],
    },
    {
        "name": "four-layer-every-strategy",
        "layers": [
            (
                "defaults",
                'theme = "system"\ndisabled_tools = ["dangerous"]\n\n[[providers]]\nname = "mistral"\napi_base = "https://api.mistral.ai/v1"\n\n[tools.bash]\ntimeout = 30\n',
            ),
            (
                "user",
                'theme = "nord"\ndisabled_tools = ["bash"]\n\n[[providers]]\nname = "llamacpp"\napi_base = "http://127.0.0.1:8080/v1"\n\n[tools.bash]\nallowlist = ["git status"]\n',
            ),
            (
                "project",
                'disabled_tools = ["edit"]\n\n[[providers]]\nname = "mistral"\napi_base = "https://proxy.example.test/v1"\n\n[tools.edit]\nconfirm = true\n',
            ),
            ("overrides", 'theme = "gruvbox"\n'),
        ],
    },
]


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture_fields(reference: Path) -> list[dict[str, Any]]:
    sys.path.insert(0, str(reference))
    from vibe.app_server._config_introspect import POPULAR_SETTINGS, classify_annotation
    from vibe.core.config.schema import MergeFieldMetadata
    from vibe.core.config.vibe_schema import VibeConfigSchema

    fields: list[dict[str, Any]] = []
    for name, info in VibeConfigSchema.model_fields.items():
        metadata = MergeFieldMetadata.from_field(info)
        if metadata is None:
            raise OracleError(f"field {name} declares no merge metadata")
        kind, choices = classify_annotation(info.annotation)
        fields.append({
            "name": name,
            "strategy": str(metadata.merge_strategy.value),
            "mergeKey": metadata.merge_key,
            "kind": str(kind.value),
            "choices": list(choices),
            "popular": name in POPULAR_SETTINGS,
        })
    return fields


def capture_strategy_vocabulary(reference: Path) -> dict[str, list[str]]:
    sys.path.insert(0, str(reference))
    from vibe.core.config.vibe_schema import VibeConfigSchema
    from vibe.core.config.schema import MergeFieldMetadata
    from vibe.core.utils.merge import MergeStrategy

    declared = sorted(strategy.value for strategy in MergeStrategy)
    used = {
        MergeFieldMetadata.from_field(info).merge_strategy.value
        for info in VibeConfigSchema.model_fields.values()
        if MergeFieldMetadata.from_field(info) is not None
    }
    return {
        "declared": declared,
        "used": sorted(used),
        "unused": sorted(set(declared) - used),
    }


async def merge_scenario(
    reference: Path, layers: list[tuple[str, str]]
) -> tuple[dict[str, Any], list[str]]:
    sys.path.insert(0, str(reference))
    from vibe.core.config.builder import ConfigBuilder, _LayerData
    from vibe.core.config.layers.overrides import OverridesLayer
    from vibe.core.config.vibe_schema import VibeConfigSchema

    builder = ConfigBuilder(VibeConfigSchema)
    layer_data: list[Any] = []
    supplied: set[str] = set()
    for name, document in layers:
        parsed = tomllib.loads(document)
        supplied.update(parsed)
        layer = OverridesLayer(data=parsed, name=name)
        builder.add_layer(layer)
        raw = (await layer.load()).model_dump()
        # `ConfigBuilder.build` skips a layer that loads to nothing.
        if raw:
            layer_data.append(_LayerData(name=name, data=raw))

    merged, _origins = builder._merge_fields(VibeConfigSchema, layer_data)
    dropped = sorted(supplied - set(merged))
    return dict(merged), dropped


def capture_scenarios(reference: Path) -> list[dict[str, Any]]:
    async def run() -> list[dict[str, Any]]:
        captured: list[dict[str, Any]] = []
        for scenario in SCENARIOS:
            merged, dropped = await merge_scenario(reference, scenario["layers"])
            captured.append({
                "name": scenario["name"],
                "layers": [
                    {"name": name, "toml": document}
                    for name, document in scenario["layers"]
                ],
                "merged": merged,
                "droppedKeys": dropped,
            })
        return captured

    return asyncio.run(run())


def build_corpus(reference: Path, expected_commit: str | None) -> dict[str, Any]:
    pin = resolve_reference(reference, expected_commit)
    vocabulary = capture_strategy_vocabulary(reference)
    if tuple(vocabulary["unused"]) != tuple(sorted(UNREACHABLE_STRATEGIES)):
        raise OracleError(
            "unused merge strategies changed: expected "
            f"{sorted(UNREACHABLE_STRATEGIES)}, got {vocabulary['unused']}"
        )
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": (
            "Captured from the pinned reference by scripts/parity/config_surface.py. "
            "Field names, merge strategies, merge keys, editor kinds and merged "
            "documents are observations; no reference-authored description text is "
            "recorded here."
        ),
        "strategies": vocabulary,
        "fields": capture_fields(reference),
        "scenarios": capture_scenarios(reference),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--interpreter",
        type=Path,
        default=None,
        help="Python that can import `vibe`; also read from " + INTERPRETER_VARIABLE,
    )
    parser.add_argument(
        "--allow-unpinned",
        action="store_true",
        help="capture from a checkout at another revision, for a re-pin",
    )
    arguments = parser.parse_args()

    try:
        reexecute_with_reference_interpreter(arguments.reference, arguments.interpreter)
        corpus = build_corpus(
            arguments.reference,
            None if arguments.allow_unpinned else EXPECTED_COMMIT,
        )
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        f"wrote {arguments.output} "
        f"({len(corpus['fields'])} fields, {len(corpus['scenarios'])} scenarios)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
