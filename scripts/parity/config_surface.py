#!/usr/bin/env python3
"""Capture how the pinned Python reference composes configuration layers.

The reference checkout is a read-only behavioral oracle. This script drives the
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

The wrapper re-executes itself with the reference interpreter, importing the
pinned commit extracted with ``git archive`` (``scripts/parity/reference.py``).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import sys
import tomllib
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import INTERPRETER_VARIABLE, OracleError, reexecute_pinned
from config_surface_cases import (
    AGENT_PROFILE_SCENARIOS,
    DOTENV_SCENARIOS,
    ENCODING_CASES,
    ENVIRONMENT_SCENARIOS,
    MCP_ENTRIES,
    MCP_NAMES,
    MCP_NAME_RESOLUTIONS,
    MCP_TOKEN_VALUE,
    MCP_TOKEN_VARIABLE,
    MCP_URLS,
    MODEL_SCENARIOS,
    ORIGIN_REWRITE_CASES,
    SCENARIOS,
    STACK_SCENARIOS,
    WRITE_SCENARIOS,
)

SCHEMA_VERSION = 6
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/config-surface/corpus.json")
#: The strategies the reference vocabulary declares but no field adopts. The
#: census asserts this stays true. v2.25.5 gave four fields ``WithShallowMerge``
#: (``merge``) and v2.25.7 a fifth, so only ``conflict`` is left
#: (``vibe/core/config/vibe_schema.py:346``, ``:347``, ``:618``, ``:621``,
#: ``:624``).
#:
#: Schema version 5 adds the shallow-merge, allowlist, routed and vision
#: scenarios, the layer stack and its writes, the environment layer, the agent
#: profile layer, the global dotenv file and the browser sign-in origin
#: rewrite.
UNREACHABLE_STRATEGIES = ("conflict",)
#: Stands in for the machine-dependent vibe home in the captured default
#: document, so the corpus stays identical on every workstation.
VIBE_HOME_PLACEHOLDER = "{vibe_home}"


# --------------------------------------------------------------------------
# MCP scenarios
# --------------------------------------------------------------------------


def capture_mcp() -> dict[str, Any]:
    """How the reference names, addresses and decodes an MCP entry."""
    os.environ[MCP_TOKEN_VARIABLE] = MCP_TOKEN_VALUE
    from pydantic import TypeAdapter, ValidationError

    from vibe.core.config.mcp_servers import (
        MCPServerAddError,
        _resolve_new_server_name,
        _suggest_server_name,
        _url_key,
        normalize_mcp_server_url,
    )
    from vibe.core.config.models import MCPServer, MCPStdio, normalize_mcp_server_name

    adapter = TypeAdapter(MCPServer)

    urls: list[dict[str, Any]] = []
    for raw in MCP_URLS:
        try:
            normalized = normalize_mcp_server_url(raw)
        except MCPServerAddError:
            urls.append({"input": raw, "rejected": True})
            continue
        urls.append({
            "input": raw,
            "rejected": False,
            "normalized": normalized,
            "key": _url_key(normalized),
            "suggested": _suggest_server_name(normalized),
        })

    resolutions: list[dict[str, Any]] = []
    for requested, url, existing in MCP_NAME_RESOLUTIONS:
        normalized_request = (
            normalize_mcp_server_name(requested) if requested is not None else None
        )
        entry: dict[str, Any] = {
            "requested": requested,
            "url": url,
            "existing": existing,
        }
        if requested is not None and not normalized_request:
            entry["rejected"] = True
            resolutions.append(entry)
            continue
        try:
            entry["resolved"] = _resolve_new_server_name(
                normalized_request, normalize_mcp_server_url(url), set(existing)
            )
            entry["rejected"] = False
        except MCPServerAddError:
            entry["rejected"] = True
        resolutions.append(entry)

    entries: list[dict[str, Any]] = []
    for name, document in MCP_ENTRIES:
        raw = tomllib.loads(document)["mcp_servers"][0]
        try:
            server = adapter.validate_python(raw)
        except (ValidationError, ValueError):
            entries.append({"name": name, "toml": document, "rejected": True})
            continue
        decoded: dict[str, Any] = {
            "name": server.name,
            "transport": server.transport,
            "prompt": server.prompt,
            "samplingEnabled": server.sampling_enabled,
            "disabled": server.disabled,
            "disabledTools": list(server.disabled_tools),
            "startupTimeoutSec": server.startup_timeout_sec,
            "toolTimeoutSec": server.tool_timeout_sec,
        }
        if isinstance(server, MCPStdio):
            decoded["argv"] = server.argv()
            decoded["env"] = dict(server.env)
            decoded["cwd"] = server.cwd
        else:
            decoded["url"] = server.url
            decoded["authType"] = server.auth.type
            decoded["httpHeaders"] = server.http_headers()
            if server.auth.type == "static":
                decoded["headers"] = dict(server.auth.headers)
                decoded["apiKeyEnv"] = server.auth.api_key_env
                decoded["apiKeyHeader"] = server.auth.api_key_header
                decoded["apiKeyFormat"] = server.auth.api_key_format
            else:
                decoded["scopes"] = list(server.auth.scopes)
                decoded["clientId"] = server.auth.client_id
                decoded["clientMetadataUrl"] = (
                    str(server.auth.client_metadata_url)
                    if server.auth.client_metadata_url
                    else None
                )
                decoded["redirectPort"] = server.auth.redirect_port
        entries.append({
            "name": name,
            "toml": document,
            "rejected": False,
            "decoded": decoded,
        })

    return {
        "tokenVariable": MCP_TOKEN_VARIABLE,
        "tokenValue": MCP_TOKEN_VALUE,
        "urls": urls,
        "names": [
            {"input": name, "normalized": normalize_mcp_server_name(name)}
            for name in MCP_NAMES
        ],
        "resolutions": resolutions,
        "entries": entries,
    }


# --------------------------------------------------------------------------
# Layer stack scenarios
# --------------------------------------------------------------------------

#: Stands in for the per-scenario temporary root in recorded values.
ROOT_PLACEHOLDER = "{root}"


def _stack_root_layout(root: Path, scenario: dict[str, Any]) -> tuple[Path, Path]:
    home = root / ".vibe"
    home.mkdir(parents=True)
    if scenario["user"] is not None:
        (home / "config.toml").write_text(scenario["user"], encoding="utf-8", newline="")
    for directory, document in scenario["projects"].items():
        target = root / directory / ".vibe"
        target.mkdir(parents=True, exist_ok=True)
        (target / "config.toml").write_text(document, encoding="utf-8", newline="")
    cwd = root / scenario["cwd"]
    cwd.mkdir(parents=True, exist_ok=True)
    return home, cwd


def _files_under(root: Path) -> dict[str, str]:
    files: dict[str, str] = {}
    for path in sorted(root.rglob("config.toml")):
        files[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    return files


def _placeholder_root(value: Any, root: Path) -> Any:
    if isinstance(value, str):
        return value.replace(str(root), ROOT_PLACEHOLDER)
    if isinstance(value, dict):
        return {key: _placeholder_root(entry, root) for key, entry in value.items()}
    if isinstance(value, list):
        return [_placeholder_root(entry, root) for entry in value]
    return value


async def _stack_capture(scenario: dict[str, Any]) -> dict[str, Any]:
    import tempfile

    from vibe.core.config.default_orchestrator import build_default_orchestrator
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.trusted_folders import TrustedFoldersManager

    saved = dict(os.environ)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory).resolve()
        try:
            home, cwd = _stack_root_layout(root, scenario)
            os.environ.clear()
            os.environ.update(_scrubbed_environment_from(saved))
            os.environ["VIBE_HOME"] = str(home)
            store = TrustedFoldersManager()
            for trusted in scenario["trusted"]:
                store.add_trusted(root / trusted)
            manager = HarnessFilesManager(
                sources=("user", "project"), cwd=cwd, trust_store=store
            )
            orchestrator = await build_default_orchestrator(harness_files=manager)
            keys = sorted({
                key
                for document in [scenario["user"], *scenario["projects"].values()]
                if document
                for key in tomllib.loads(document)
            } | {write["path"] for write in scenario.get("writes", [])})
            dumped = orchestrator.config.model_dump(mode="json")
            captured: dict[str, Any] = {
                "effective": {key: dumped.get(key) for key in keys},
                "writableLayer": orchestrator.writable_layer_name,
            }
            for write in scenario.get("writes", []):
                failures = await orchestrator.set_field("/" + write["path"], write["value"])
                if failures:
                    raise OracleError(f"{scenario['name']}: a write failed")
            if "writes" in scenario:
                captured["files"] = _files_under(root)
                dumped = orchestrator.config.model_dump(mode="json")
                captured["afterWrites"] = {key: dumped.get(key) for key in keys}
            return _placeholder_root(captured, root)
        finally:
            os.environ.clear()
            os.environ.update(saved)


def _scrubbed_environment_from(environment: dict[str, str]) -> dict[str, str]:
    """`environment` without any `VIBE_` variable, so a variable the capturing
    shell exported cannot reach the environment layer."""
    return {
        key: value
        for key, value in environment.items()
        if not key.upper().startswith("VIBE_")
    }


def capture_stack() -> dict[str, list[dict[str, Any]]]:

    async def run(scenarios: list[dict[str, Any]]) -> list[dict[str, Any]]:
        captured = []
        for scenario in scenarios:
            captured.append({**scenario, **(await _stack_capture(scenario))})
        return captured

    return {
        "stacks": asyncio.run(run(STACK_SCENARIOS)),
        "writes": asyncio.run(run(WRITE_SCENARIOS)),
    }


# --------------------------------------------------------------------------
# Environment, agent profile, dotenv and sign-in rewrite scenarios
# --------------------------------------------------------------------------


def capture_encoding() -> list[dict[str, str]]:
    import tomli_w

    return [
        {"document": document, "written": tomli_w.dumps(tomllib.loads(document))}
        for document in ENCODING_CASES
    ]


def capture_environment() -> list[dict[str, Any]]:
    from vibe.core.config.layers.environment import EnvironmentLayer
    from vibe.core.config.vibe_schema import VibeConfigSchema

    async def run() -> list[dict[str, Any]]:
        captured = []
        saved = dict(os.environ)
        try:
            for scenario in ENVIRONMENT_SCENARIOS:
                os.environ.clear()
                os.environ.update(_scrubbed_environment_from(saved))
                os.environ.update(scenario["variables"])
                layer = EnvironmentLayer(schema=VibeConfigSchema)
                data = (await layer.load()).model_dump(mode="json")
                captured.append({**scenario, "layer": data})
        finally:
            os.environ.clear()
            os.environ.update(saved)
        return captured

    return asyncio.run(run())


def capture_agent_profiles() -> list[dict[str, Any]]:
    from vibe.core.config.layers.agent_profile import AgentProfileLayer

    async def run() -> list[dict[str, Any]]:
        captured = []
        for scenario in AGENT_PROFILE_SCENARIOS:
            layer = AgentProfileLayer(data=scenario["overrides"])
            captured.append({**scenario, "layer": (await layer.load()).model_dump(mode="json")})
        return captured

    return asyncio.run(run())


def capture_dotenv() -> list[dict[str, Any]]:
    import tempfile

    from vibe.core.config.vibe_schema import load_dotenv_values

    captured = []
    for scenario in DOTENV_SCENARIOS:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / ".env"
            path.write_text(scenario["file"], encoding="utf-8", newline="")
            environ = dict(scenario["environ"])
            load_dotenv_values(env_path=path, environ=environ)
        captured.append({**scenario, "result": dict(sorted(environ.items()))})
    return captured


def capture_origin_rewrite() -> list[dict[str, Any]]:
    from vibe.setup.auth.browser_sign_in_gateway import (
        BrowserSignInError,
        BrowserSignInErrorCode,
    )
    from vibe.setup.auth.http_browser_sign_in_gateway import (
        _validate_url_against_base_url,
    )

    captured = []
    for value, base in ORIGIN_REWRITE_CASES:
        try:
            answered: str | None = _validate_url_against_base_url(
                value,
                base_url=base,
                message="",
                code=BrowserSignInErrorCode.START_FAILED,
                allow_origin_rewrite=True,
            )
        except BrowserSignInError:
            answered = None
        captured.append({"value": value, "base": base, "answered": answered})
    return captured


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture_fields() -> list[dict[str, Any]]:
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


def capture_strategy_vocabulary() -> dict[str, list[str]]:
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


async def merge_scenario(layers: list[tuple[str, str]]) -> tuple[dict[str, Any], list[str]]:
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


async def validated_models(layers: list[tuple[str, str]]) -> dict[str, Any]:
    from vibe.core.config.builder import ConfigBuilder
    from vibe.core.config.layers.default import DefaultConfigLayer
    from vibe.core.config.layers.overrides import OverridesLayer
    from vibe.core.config.models import normalize_model_configs
    from vibe.core.config.vibe_schema import VibeConfigSchema

    # No validation context: the builder stopped taking one, and the API-key
    # check it used to turn off is no longer a validator. It is a method that
    # callers invoke after the build (``vibe/core/config/builder.py:46``,
    # ``vibe/core/config/vibe_schema.py:724``, ``vibe/cli/cli.py:102``,
    # ``vibe/core/agent_loop/_loop.py:625``, ``vibe/app_server/_runtime.py:1449``).
    from pydantic import ValidationError

    from vibe.core.config.layers.admin import AdminConfigLayer

    builder = ConfigBuilder(VibeConfigSchema)
    builder.add_layer(DefaultConfigLayer(schema=VibeConfigSchema))
    for name, document in layers:
        data = tomllib.loads(document)
        # The admin layer is named for what `validate_merged` reads: the origin
        # of the global compaction threshold.
        if name == "admin":
            builder.add_layer(AdminConfigLayer(data=data))
        else:
            # Named and read as the file layers the stack stands for, which
            # load `[[models]]` as an alias map (`layers/_base.py:107`), so the
            # settings rows below report the origin a client reads.
            if "models" in data:
                data["models"] = normalize_model_configs(data["models"])
            builder.add_layer(OverridesLayer(data=data, name=FILE_LAYER_NAMES[name]))
    try:
        config = await builder.build()
    except (ValidationError, ValueError):
        # Only the verdict: the message is reference-authored prose.
        return {"rejected": True}
    fields = await settings_rows(config, builder.layers)

    def dump(model: Any) -> Any:
        return None if model is None else model.model_dump(mode="json")

    from vibe.core.config.models import UtilityFeature

    try:
        active_alias: str | None = config.get_active_model().alias
    except ValueError:
        active_alias = None
    try:
        compaction_alias: str | None = config.get_compaction_model().alias
    except ValueError:
        compaction_alias = None

    def utility_alias(feature: Any) -> str | None:
        try:
            model = config.get_utility_model(feature)
        except ValueError:
            return None
        return None if model is None else model.alias

    return {
        "rejected": False,
        "activeModel": config.active_model,
        "models": {
            alias: model.model_dump(mode="json")
            for alias, model in config.models.items()
        },
        # The compaction and vision models are `ModelConfig`s too, so they
        # carry the same alias rule and the same per-entry defaults; `null`
        # where the document declares none.
        "compactionModel": dump(config.compaction_model),
        "visionModel": dump(config.vision_model),
        "routedExtraModels": [dump(model) for model in config.routed_extra_models],
        "availableModels": list(config.available_models()),
        "defaultModelAlias": config.resolve_default_model_alias(),
        "activeModelAlias": active_alias,
        # The routed definition is a field of its own, which keeps the
        # `ModelConfig` defaults its injected copy in `models` resolves.
        "routedModelConfig": dump(config.routed_model_config),
        # The model a compaction runs on: the configured one while it shares
        # the active model's provider, the active model otherwise.
        "compactionModelAlias": compaction_alias,
        "utilityModels": {
            feature.value: utility_alias(feature) for feature in UtilityFeature
        },
        # Only the count: the warning text is reference-authored prose and
        # ``NOTICE`` forbids committing it.
        "validationWarnings": len(config.validation_warnings),
        "fields": fields,
    }


#: The layer each scenario layer name stands for, by its reference name.
FILE_LAYER_NAMES = {"user": "user-toml", "project": "project-toml"}

#: The settings rows whose value the reference reads off the active model
#: rather than off the field, compared by value as well as by origin.
MODEL_SCOPED_ROWS = ("auto_compact_threshold", "max_context_length")


async def settings_rows(config: Any, layers: list[Any]) -> list[dict[str, Any]]:
    """What ``config/fields/read`` answers over the validated stack, without
    the descriptions, which are reference-authored prose: the name, path and
    the layers its value comes from of every row some layer above the defaults
    sets, plus the value and the per-layer values of the two rows read off the
    active model. Which rows exist and in what order is recorded once, by
    ``capture_settings_order``."""
    from vibe.app_server._config_introspect import (
        HIDDEN_SETTINGS,
        POPULAR_SETTINGS,
        build_field_wires,
        collect_layer_values,
    )

    layer_values = await collect_layer_values(layers)
    rows = []
    for wire in build_field_wires(config, layer_values, popular=POPULAR_SETTINGS):
        if wire.name in HIDDEN_SETTINGS:
            continue
        row: dict[str, Any] = {
            "name": wire.name,
            "path": wire.path,
            "layers": [entry.layer for entry in wire.layer_values],
        }
        if wire.name in MODEL_SCOPED_ROWS:
            row["value"] = wire.value
            row["layerValues"] = [entry.value for entry in wire.layer_values]
        elif row["layers"] == ["default"]:
            continue
        rows.append(row)
    return rows


def capture_settings_order() -> list[str]:
    """The rows ``config/fields/read`` answers over the shipped defaults, in
    the order it lists them."""
    from vibe.app_server._config_introspect import (
        HIDDEN_SETTINGS,
        POPULAR_SETTINGS,
        build_field_wires,
        collect_layer_values,
    )
    from vibe.core.config.builder import ConfigBuilder
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.core.config.layers.default import DefaultConfigLayer
    from vibe.core.config.vibe_schema import VibeConfigSchema

    init_harness_files_manager()

    async def run() -> list[str]:
        builder = ConfigBuilder(VibeConfigSchema)
        builder.add_layer(DefaultConfigLayer(schema=VibeConfigSchema))
        config = await builder.build()
        layer_values = await collect_layer_values(builder.layers)
        return [
            wire.name
            for wire in build_field_wires(
                config, layer_values, popular=POPULAR_SETTINGS
            )
            if wire.name not in HIDDEN_SETTINGS
        ]

    return asyncio.run(run())


def capture_model_scenarios() -> list[dict[str, Any]]:
    from vibe.core.config.harness_files import init_harness_files_manager

    # The prompt-id validators reach the harness-files singleton; it resolves
    # the builtin prompts shipped inside the checkout, so the capture stays
    # machine-independent.
    init_harness_files_manager()

    async def run() -> list[dict[str, Any]]:
        captured: list[dict[str, Any]] = []
        for scenario in MODEL_SCENARIOS:
            result = await validated_models(scenario["layers"])
            captured.append({
                "name": scenario["name"],
                "layers": [
                    {"name": name, "toml": document}
                    for name, document in scenario["layers"]
                ],
                **result,
            })
        return captured

    return asyncio.run(run())


def capture_defaults() -> dict[str, Any]:
    """The document ``create_default_config`` ships, minus discovered tools."""
    from vibe.core.paths import SESSION_LOG_DIR
    from vibe.core.config.vibe_schema import create_default_config

    document = create_default_config()
    tools = document.pop("tools", {})
    logging = document.get("session_logging")
    if not isinstance(logging, dict):
        raise OracleError("session_logging is not a table in the default document")
    if logging.get("save_dir") != str(SESSION_LOG_DIR.path):
        raise OracleError("the default session log directory moved")
    logging["save_dir"] = f"{VIBE_HOME_PLACEHOLDER}/logs/session"
    return {
        "document": document,
        # Compared for shape only: both implementations fill `tools` from their
        # own tool discovery, so only the key set is an observation worth
        # recording.
        "toolNames": sorted(tools),
    }


def capture_scenarios() -> list[dict[str, Any]]:
    async def run() -> list[dict[str, Any]]:
        captured: list[dict[str, Any]] = []
        for scenario in SCENARIOS:
            merged, dropped = await merge_scenario(scenario["layers"])
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


def build_corpus(commit: str) -> dict[str, Any]:
    pin = {"commit": commit}
    vocabulary = capture_strategy_vocabulary()
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
        "fields": capture_fields(),
        "defaults": capture_defaults(),
        "scenarios": capture_scenarios(),
        "modelScenarios": capture_model_scenarios(),
        "settingsOrder": capture_settings_order(),
        "mcp": capture_mcp(),
        "stack": capture_stack(),
        "environment": capture_environment(),
        "agentProfiles": capture_agent_profiles(),
        "dotenv": capture_dotenv(),
        "originRewrite": capture_origin_rewrite(),
        "encoding": capture_encoding(),
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
        commit, _ = reexecute_pinned(
            arguments.reference,
            None if arguments.allow_unpinned else EXPECTED_COMMIT,
            override=arguments.interpreter,
        )
        corpus = build_corpus(commit)
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline=""
    )
    print(
        f"wrote {arguments.output} "
        f"({len(corpus['fields'])} fields, "
        f"{len(corpus['defaults']['document'])} defaults, "
        f"{len(corpus['scenarios'])} scenarios, "
        f"{len(corpus['modelScenarios'])} model scenarios, "
        f"{len(corpus['mcp']['entries'])} MCP entries)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
