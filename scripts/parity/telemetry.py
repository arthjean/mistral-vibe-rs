#!/usr/bin/env python3
"""Capture what the pinned Python reference's telemetry and observability answer.

This part of the reference splits across four subtrees that are never read in
isolation: the datalake client under ``vibe/core/telemetry/``, all of OpenTelemetry
in ``vibe/core/tracing.py``, the log formatter and the crash reporter under
``vibe/observability/``, and the backward-paginated reader in
``vibe/core/log_reader.py``. This capture drives every one of them directly, over
inputs this script authors, and records fourteen families the Rust replay in
``crates/vibe-core/src/telemetry/telemetry_parity_tests.rs`` compares this build
against:

``constants``        endpoints, timeouts, tracer names, attribute keys, vocabularies
``envelope``         the datalake request a scripted configuration produces
``baseMetadata``     the 12 base fields, the 3 request fields and their omissions
``eventVocabulary``  the 26 event names and where each is raised
``eventPayloads``    the property key set each event carries
``exporterConfig``   the exporter endpoint and headers per configuration
``spans``            the four span families, their attributes and their statuses
``providerNames``    the provider-name normalization and its aliases
``redaction``        which attribute keys survive each redaction mode
``logFormat``        the log line the structured formatter writes
``logEncoding``      the backslash and newline round-trip
``logParse``         the parse verdict per line
``logPagination``    the backward-paginated page per limit and offset
``logConfig``        the resolved level and rotation size per environment

Two artifacts come out of a run::

    .parity/telemetry-corpus.json                   the full capture, gitignored
    crates/vibe-core/tests/telemetry/corpus.json    the committed corpus

The capture asserts its own isolation. Every environment variable a document
names is set to a sentinel before anything runs, a resolved credential is
recorded as the variable it came from and never as a value, and a socket guard
fails the run on any connection or DNS attempt, so a scenario that accidentally
reached a real backend could not write a corpus. Reference-authored sentences,
which here are the status description of an unhandled model-call exception and
the log-line pattern, are recorded as a byte length and a SHA-256: a digest
still fails the replay on any change while no reference sentence ships.

Usage::

    scripts/parity/telemetry.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter, reading the pinned commit through ``git archive`` so
the checkout is never moved.
"""

from __future__ import annotations

import argparse
import ast
import asyncio
import contextlib
import hashlib
import json
import logging
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import tarfile
import tempfile
from typing import Any, get_args

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/telemetry-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/telemetry/corpus.json")
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Stand-ins for values that differ per machine or per release, so the corpus is
#: identical on every workstation and the replay compares a rule rather than a
#: workstation.
VERSION_PLACEHOLDER = "{version}"
PLATFORM_ID_PLACEHOLDER = "{platformId}"
PLATFORM_VERSION_PLACEHOLDER = "{platformVersion}"
PPID_PLACEHOLDER = "{ppid}"
PID_PLACEHOLDER = "{pid}"
TIMESTAMP_PLACEHOLDER = "{timestamp}"

#: Every environment variable this capture names, with the sentinel it is set to
#: first. A value here never reaches the corpus: what is recorded is the variable
#: a credential came from.
SENTINELS: dict[str, str] = {
    "MISTRAL_API_KEY": "oracle-default-sentinel",
    "ORACLE_MISTRAL_KEY": "oracle-mistral-sentinel",
    "ORACLE_PROXY_KEY": "oracle-proxy-sentinel",
    "ORACLE_THIRD_PARTY_KEY": "oracle-third-party-sentinel",
}

#: Named but deliberately left unset, so the "no credential" branch is reached
#: through a variable the corpus can name.
UNSET_VARIABLES = ("ORACLE_ABSENT_KEY",)

#: The variables ``init_file_logging`` reads, managed per case rather than once.
LOG_VARIABLES = ("LOG_LEVEL", "DEBUG_MODE", "LOG_MAX_BYTES")


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def _git(reference: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git {' '.join(arguments)} failed in {reference}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def resolve_reference(reference: Path, expected: str) -> dict[str, str]:
    """The pinned commit, read out of the checkout without depending on its HEAD."""

    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs."""

    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = tree / "vibe" / "__init__.py"
    if marker.is_file():
        return tree
    tree.mkdir(parents=True, exist_ok=True)
    archive = tree.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(tree, filter="data")
    archive.unlink(missing_ok=True)
    if not marker.is_file():
        raise OracleError(f"the extracted tree at {tree} carries no `vibe` package")
    return tree


def reexecute_with_reference_interpreter(
    reference: Path, override: Path | None, tree: Path
) -> None:
    """Re-runs this script under an interpreter importing the *pinned* tree."""

    if os.environ.get(_REEXEC_MARKER) == str(tree):
        if not _imports_pinned_vibe(tree):
            raise OracleError(
                f"the reference interpreter did not import `vibe` from {tree}"
            )
        return
    candidates = [override] if override else []
    candidates += [
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    interpreter = next((c for c in candidates if c and c.is_file()), None)
    if interpreter is None:
        raise OracleError(
            f"no interpreter can import `vibe`; looked for a virtual environment in {reference}"
        )
    environment = dict(os.environ)
    environment[_REEXEC_MARKER] = str(tree)
    environment["PYTHONPATH"] = os.pathsep.join(
        [
            str(tree),
            *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else []),
        ]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


class _SocketGuard:
    """Fails the capture the moment anything tries to reach a network.

    ``connect``, ``connect_ex``, ``create_connection`` and ``getaddrinfo`` are
    replaced with raisers; ``socketpair`` stays, because asyncio's self-pipe
    uses it without connecting anywhere.
    """

    def __init__(self) -> None:
        self.attempts: list[str] = []

    def install(self) -> None:
        guard = self

        def refuse(name: str) -> Any:
            def raiser(*arguments: Any, **keywords: Any) -> Any:
                guard.attempts.append(name)
                raise OracleError(
                    f"the capture attempted network access through {name}"
                )

            return raiser

        socket.socket.connect = refuse("socket.connect")  # type: ignore[method-assign]
        socket.socket.connect_ex = refuse("socket.connect_ex")  # type: ignore[method-assign]
        socket.create_connection = refuse("socket.create_connection")  # type: ignore[assignment]
        socket.getaddrinfo = refuse("socket.getaddrinfo")  # type: ignore[assignment]


GUARD = _SocketGuard()


def install_sentinels() -> None:
    """Every named variable set to a sentinel, before any scenario runs."""

    for name, value in SENTINELS.items():
        os.environ[name] = value
    for name in (*UNSET_VARIABLES, *LOG_VARIABLES):
        os.environ.pop(name, None)


@contextlib.contextmanager
def patched_environ(values: dict[str, str | None]):
    originals = {name: os.environ.get(name) for name in values}
    for name, value in values.items():
        if value is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = value
    try:
        yield
    finally:
        for name, original in originals.items():
            if original is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = original


def digest(value: str) -> dict[str, Any]:
    """A string recorded by its length and its SHA-256, never by its content."""

    return {
        "length": len(value),
        "digest": hashlib.sha256(value.encode("utf-8")).hexdigest(),
    }


def type_name(value: Any) -> str:
    """The wire type of a property value, which is what the corpus compares."""

    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, int):
        return "int"
    if isinstance(value, float):
        return "float"
    if isinstance(value, str):
        return "string"
    if isinstance(value, dict):
        return "object"
    if isinstance(value, (list, tuple)):
        return "array"
    return type(value).__name__


def scrub(value: Any) -> Any:
    """Machine-dependent and release-dependent values replaced by placeholders."""

    from vibe import __version__
    from vibe.utils.platform import get_platform_id, get_platform_version

    replacements = [
        (__version__, VERSION_PLACEHOLDER),
        (get_platform_version() or "", PLATFORM_VERSION_PLACEHOLDER),
        (get_platform_id(), PLATFORM_ID_PLACEHOLDER),
    ]
    if isinstance(value, str):
        for original, placeholder in replacements:
            if original:
                value = value.replace(original, placeholder)
        return value
    if isinstance(value, dict):
        return {key: scrub(item) for key, item in value.items()}
    if isinstance(value, list):
        return [scrub(item) for item in value]
    return value


# --------------------------------------------------------------------------
# Configurations the scenarios are driven over
# --------------------------------------------------------------------------

#: Provider stacks every telemetry scenario resolves a credential from. Names,
#: bases and variables are authored here; the corpus records the variable a key
#: came from and never the key.
#: Provider stacks every telemetry scenario resolves a credential from, authored
#: as the documents both sides load: the reference validates the parsed table,
#: and the Rust replay writes the same TOML to a scratch configuration home.
#: Names, bases and variables are authored here; the corpus records the variable
#: a key came from and never the key.
CONFIGURATIONS: list[dict[str, str]] = [
    {
        "id": "mistral-active",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "telemetry-disabled",
        "toml": """
enable_telemetry = false
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "third-party-only",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
""",
    },
    {
        "id": "third-party-active-mistral-configured",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
""",
    },
    {
        "id": "mistral-behind-a-proxy",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://proxy.example.test/gateway/v1"
api_key_env_var = "ORACLE_PROXY_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "mistral-base-without-a-version-segment",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://bare.example.test"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "mistral-without-a-resolvable-key",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_ABSENT_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "mistral-with-the-default-key-variable",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
]


def build_config(document: str) -> Any:
    """The reference configuration one authored document validates to."""

    import tomllib

    from vibe.core.config.vibe_schema import VibeConfigSchema

    return VibeConfigSchema.model_validate(
        tomllib.loads(document), context={"require_api_key": False}
    )


class RecordingHTTPClient:
    """Stands where the reference's HTTP client stands, one call before the
    connection: it records the request the client would have issued and never
    opens a socket."""

    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []
        self.closed = False

    async def post(
        self,
        url: str,
        *,
        json: dict[str, Any] | None = None,
        headers: dict[str, str] | None = None,
        **_: Any,
    ) -> None:
        self.requests.append({"url": url, "json": json, "headers": headers or {}})

    async def aclose(self) -> None:
        self.closed = True


def variable_for(value: str | None) -> str | None:
    """The environment variable a captured credential came from.

    A credential is recorded by the variable that supplied it, so no value ever
    reaches the corpus even when a scenario resolves one.
    """

    if value is None:
        return None
    for name, sentinel in SENTINELS.items():
        if sentinel == value:
            return name
    return "unrecognized"


def authorization_variable(header: str | None) -> str | None:
    if not header:
        return None
    scheme, _, credential = header.partition(" ")
    if scheme != "Bearer":
        return "unrecognized"
    return variable_for(credential)


# --------------------------------------------------------------------------
# constants
# --------------------------------------------------------------------------


def capture_constants(reference: Path) -> dict[str, Any]:
    from vibe.core.config import DEFAULT_MISTRAL_API_ENV_KEY, DEFAULT_MISTRAL_SERVER_URL
    from vibe.core.config.models import OtelRedactionMode
    from vibe.core.log_reader import DEFAULT_LOG_PATTERN, LOG_POLL_INTERVAL
    from vibe.core.paths import LOG_FILE, VIBE_HOME
    from vibe.core.telemetry import send as send_module
    from vibe.core.telemetry.types import (
        AttachmentKind,
        ProjectSelectionSource,
        RemoteProjectOutcome,
        TelemetryBaseMetadata,
        TelemetryCallType,
        TelemetryRequestMetadata,
        TeleportContextSummaryStatus,
        TeleportFailureStage,
    )
    from vibe.utils import AgentEntrypoint
    from vibe.core import tracing
    from vibe.utils.http import get_user_agent
    from vibe.utils.terminal import TerminalEmulator
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import (
        DEFAULT_TRACES_EXPORT_PATH,
    )
    from opentelemetry.semconv._incubating.attributes import (
        gen_ai_attributes,
        http_attributes,
    )

    client = build_client(build_config(CONFIGURATIONS[0]["toml"]))
    http = client.client
    pool = getattr(getattr(http, "_transport", None), "_pool", None)

    log_file = Path(LOG_FILE.path)
    home = Path(VIBE_HOME.path)

    return {
        "endpoint": {
            "defaultBaseUrl": send_module._DEFAULT_TELEMETRY_BASE_URL,
            "eventsPath": send_module._DATALAKE_EVENTS_PATH,
            "defaultMistralServerUrl": DEFAULT_MISTRAL_SERVER_URL,
            "defaultApiKeyVariable": DEFAULT_MISTRAL_API_ENV_KEY,
        },
        "transport": {
            "timeoutSeconds": http.timeout.read,
            "connectTimeoutSeconds": http.timeout.connect,
            "maxConnections": getattr(pool, "_max_connections", None),
            "maxKeepaliveConnections": getattr(pool, "_max_keepalive_connections", None),
            "headerNames": ["Content-Type", "Authorization", "User-Agent"],
            "contentType": "application/json",
            "authorizationScheme": "Bearer",
            "userAgentMistral": scrub(get_user_agent("mistral")),
            "userAgentGeneric": scrub(get_user_agent("generic")),
            "userAgentUnknown": scrub(get_user_agent(None)),
        },
        "tracing": {
            "tracerName": tracing.VIBE_TRACER_NAME,
            "agentName": tracing.VIBE_AGENT_NAME,
            "mistralOtelPath": tracing.MISTRAL_OTEL_PATH,
            "tracesExportPath": DEFAULT_TRACES_EXPORT_PATH,
            "providerApiStyleKey": tracing.VIBE_PROVIDER_API_STYLE,
            "requestCallTypeKey": tracing.VIBE_REQUEST_CALL_TYPE,
            "requestMessageIdKey": tracing.VIBE_REQUEST_MESSAGE_ID,
            "requestStreamingKey": tracing.VIBE_REQUEST_STREAMING,
            "hookNameKey": "vibe.hook.name",
            "hookTypeKey": "vibe.hook.type",
            "semanticKeys": {
                "operationName": gen_ai_attributes.GEN_AI_OPERATION_NAME,
                "providerName": gen_ai_attributes.GEN_AI_PROVIDER_NAME,
                "agentName": gen_ai_attributes.GEN_AI_AGENT_NAME,
                "conversationId": gen_ai_attributes.GEN_AI_CONVERSATION_ID,
                "requestModel": gen_ai_attributes.GEN_AI_REQUEST_MODEL,
                "requestTemperature": gen_ai_attributes.GEN_AI_REQUEST_TEMPERATURE,
                "requestMaxTokens": gen_ai_attributes.GEN_AI_REQUEST_MAX_TOKENS,
                "toolName": gen_ai_attributes.GEN_AI_TOOL_NAME,
                "toolCallId": gen_ai_attributes.GEN_AI_TOOL_CALL_ID,
                "toolCallArguments": gen_ai_attributes.GEN_AI_TOOL_CALL_ARGUMENTS,
                "toolCallResult": gen_ai_attributes.GEN_AI_TOOL_CALL_RESULT,
                "toolType": gen_ai_attributes.GEN_AI_TOOL_TYPE,
                "usageInputTokens": gen_ai_attributes.GEN_AI_USAGE_INPUT_TOKENS,
                "usageOutputTokens": gen_ai_attributes.GEN_AI_USAGE_OUTPUT_TOKENS,
                "responseFinishReasons": gen_ai_attributes.GEN_AI_RESPONSE_FINISH_REASONS,
                "responseModel": gen_ai_attributes.GEN_AI_RESPONSE_MODEL,
                "responseId": gen_ai_attributes.GEN_AI_RESPONSE_ID,
                "httpRequestMethod": http_attributes.HTTP_REQUEST_METHOD,
                "httpUrl": http_attributes.HTTP_URL,
                "httpResponseStatusCode": http_attributes.HTTP_RESPONSE_STATUS_CODE,
            },
            "operationNames": {
                "invokeAgent": gen_ai_attributes.GenAiOperationNameValues.INVOKE_AGENT.value,
                "executeTool": gen_ai_attributes.GenAiOperationNameValues.EXECUTE_TOOL.value,
                "chat": gen_ai_attributes.GenAiOperationNameValues.CHAT.value,
            },
            "mistralProviderValue": gen_ai_attributes.GenAiProviderNameValues.MISTRAL_AI.value,
        },
        "vocabularies": {
            "agentEntrypoints": list(get_args(AgentEntrypoint.__value__)),
            "terminalEmulators": [value.value for value in TerminalEmulator],
            "otelRedactionModes": [value.value for value in OtelRedactionMode],
            "callTypes": list(get_args(TelemetryCallType)),
            "callSourceDefault": "vibe_code",
            "attachmentKinds": [value.value for value in AttachmentKind],
            "baseMetadataFields": list(TelemetryBaseMetadata.model_fields),
            "requestMetadataFields": [
                name
                for name in TelemetryRequestMetadata.model_fields
                if name not in TelemetryBaseMetadata.model_fields
            ],
            "teleportFailureStages": list(get_args(TeleportFailureStage)),
            "teleportContextSummaryStatuses": list(
                get_args(TeleportContextSummaryStatus)
            ),
            "projectSelectionSources": list(get_args(ProjectSelectionSource)),
            "remoteProjectOutcomes": list(get_args(RemoteProjectOutcome)),
        },
        "logging": {
            "relativeLogFile": log_file.relative_to(home).as_posix(),
            "defaultMaxBytes": 10 * 1024 * 1024,
            "backupCount": 0,
            "defaultLevel": "WARNING",
            "levels": ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"],
            "loggerName": "vibe",
            "pollIntervalSeconds": LOG_POLL_INTERVAL,
            "fieldOrder": ["timestamp", "ppid", "pid", "level", "message"],
            "fieldSeparator": " ",
            "readChunkSize": 8192,
            # The pattern is reference-authored source; its shape and its digest
            # are what the replay compares, never the expression itself.
            "patternGroups": DEFAULT_LOG_PATTERN.groups,
            "patternDigest": digest(DEFAULT_LOG_PATTERN.pattern),
        },
    }


def build_client(config: Any, **keywords: Any) -> Any:
    from vibe.core.telemetry.send import TelemetryClient

    return TelemetryClient(lambda: config, **keywords)


# --------------------------------------------------------------------------
# envelope
# --------------------------------------------------------------------------


def capture_envelope() -> list[dict[str, Any]]:
    """The datalake request each configuration produces, intercepted one call
    before the connection."""

    records: list[dict[str, Any]] = []
    for scenario in CONFIGURATIONS:
        for correlation_id in (None, "oracle-correlation"):
            records.append(
                asyncio.run(
                    _send_one(
                        scenario["id"],
                        scenario["toml"],
                        correlation_id=correlation_id,
                    )
                )
            )
    records.append(
        asyncio.run(
            _send_one(
                "mistral-active",
                CONFIGURATIONS[0]["toml"],
                correlation_id=None,
                properties={"version": "authored-by-the-caller", "extra": 1},
                case="caller-properties-win-over-metadata",
            )
        )
    )
    records.append(
        asyncio.run(
            _send_one(
                "mistral-active",
                CONFIGURATIONS[0]["toml"],
                correlation_id="",
                case="empty-correlation-id",
            )
        )
    )
    return records


async def _send_one(
    configuration: str,
    document: str,
    *,
    correlation_id: str | None,
    properties: dict[str, Any] | None = None,
    case: str | None = None,
) -> dict[str, Any]:
    from vibe.core.telemetry.types import LaunchContext
    from vibe.utils.terminal import TerminalEmulator

    config = build_config(document)
    launch_context = LaunchContext(
        agent_entrypoint="cli",
        agent_version="oracle-agent-version",
        client_name="oracle-client",
        client_version="oracle-client-version",
        terminal_emulator=TerminalEmulator.GHOSTTY,
    )
    client = build_client(
        config,
        session_id_getter=lambda: "oracle-session",
        parent_session_id_getter=lambda: "oracle-parent-session",
        launch_context=launch_context,
        user_plan_getter=lambda: "oracle-plan",
    )
    recorder = RecordingHTTPClient()
    client._client = recorder
    client.send_telemetry_event(
        "vibe.ready",
        {"init_duration_ms": 1234} if properties is None else dict(properties),
        correlation_id=correlation_id,
    )
    await client.aclose()

    record: dict[str, Any] = {
        "case": case or f"{configuration}-correlation-{'yes' if correlation_id else 'no'}",
        "configuration": configuration,
        "correlationIdRequested": correlation_id is not None,
        "enabled": client._is_enabled(),
        "credentialResolved": client._get_mistral_api_key() is not None,
        "active": client.is_active(),
        "sent": bool(recorder.requests),
        "flushed": recorder.closed,
        "url": None,
        "headerNames": [],
        "contentType": None,
        "userAgent": None,
        "credentialVariable": None,
        "bodyKeys": [],
        "event": None,
        "correlationId": None,
        "propertyKeys": [],
        "propertyTypes": {},
        "properties": {},
    }
    if not recorder.requests:
        return record
    request = recorder.requests[0]
    body = request["json"] or {}
    headers = request["headers"]
    properties_sent = body.get("properties", {})
    record |= {
        "url": request["url"],
        "headerNames": sorted(headers),
        "contentType": headers.get("Content-Type"),
        "userAgent": scrub(headers.get("User-Agent")),
        "credentialVariable": authorization_variable(headers.get("Authorization")),
        "bodyKeys": sorted(body),
        "event": body.get("event"),
        "correlationId": body.get("correlation_id"),
        "propertyKeys": sorted(properties_sent),
        "propertyTypes": {
            key: type_name(value) for key, value in sorted(properties_sent.items())
        },
        "properties": scrub(properties_sent),
    }
    return record


# --------------------------------------------------------------------------
# baseMetadata
# --------------------------------------------------------------------------

#: Every combination the metadata builders are driven over. Each entry names
#: what it varies, so a divergence reports a rule rather than an index.
METADATA_CASES: list[dict[str, Any]] = [
    {"case": "full-launch-context", "launchContext": True, "session": True,
     "parentSession": True, "experiments": {"ab": "on"}, "userPlan": "oracle-plan"},
    {"case": "no-launch-context", "launchContext": False, "session": True,
     "parentSession": True, "experiments": None, "userPlan": "oracle-plan"},
    {"case": "no-session", "launchContext": True, "session": False,
     "parentSession": False, "experiments": None, "userPlan": None},
    {"case": "empty-experiments", "launchContext": True, "session": True,
     "parentSession": False, "experiments": {}, "userPlan": None},
    {"case": "no-terminal-emulator", "launchContext": True, "session": True,
     "parentSession": True, "experiments": None, "userPlan": None,
     "terminalEmulator": None},
    {"case": "no-user-plan", "launchContext": True, "session": True,
     "parentSession": True, "experiments": {"ab": "off"}, "userPlan": None},
]


def capture_base_metadata() -> list[dict[str, Any]]:
    from vibe.core.telemetry.build_metadata import (
        build_base_metadata,
        build_launch_context,
        build_request_metadata,
    )
    from vibe.utils.terminal import TerminalEmulator

    records: list[dict[str, Any]] = []
    for entry in METADATA_CASES:
        terminal = entry.get("terminalEmulator", "ghostty")
        launch_context = (
            build_launch_context(
                agent_entrypoint="cli",
                agent_version="oracle-agent-version",
                client_name="oracle-client",
                client_version="oracle-client-version",
                terminal_emulator=(
                    TerminalEmulator(terminal) if terminal is not None else None
                ),
            )
            if entry["launchContext"]
            else None
        )
        base = build_base_metadata(
            launch_context=launch_context,
            session_id="oracle-session" if entry["session"] else None,
            parent_session_id=(
                "oracle-parent-session" if entry["parentSession"] else None
            ),
            experiments=entry["experiments"],
            user_plan=entry["userPlan"],
        )
        for call_type in ("main_call", "secondary_call"):
            request = build_request_metadata(
                launch_context=launch_context,
                session_id="oracle-session" if entry["session"] else None,
                parent_session_id=(
                    "oracle-parent-session" if entry["parentSession"] else None
                ),
                call_type=call_type,
                message_id="oracle-message" if call_type == "main_call" else None,
                user_plan=entry["userPlan"],
            )
            records.append({
                "case": f"{entry['case']}-{call_type}",
                "baseKeys": sorted(base),
                "base": scrub(base),
                "requestKeys": sorted(request.model_dump(exclude_none=True)),
                "request": scrub(request.model_dump(exclude_none=True)),
                "requestKeysWithNulls": sorted(request.model_dump()),
                "launchFields": (
                    scrub(launch_context.telemetry_fields())
                    if launch_context is not None
                    else None
                ),
                "sentryTags": (
                    scrub(launch_context.sentry_tags())
                    if launch_context is not None
                    else None
                ),
            })
    return records


def capture_attachment_counts() -> list[dict[str, Any]]:
    from vibe.core.telemetry.build_metadata import build_attachment_counts
    from vibe.core.types import LLMMessage

    def message(count: int) -> Any:
        images = [
            {
                "source": {"kind": "file", "path": f"/oracle/workspace/image-{index}.png"},
                "alias": f"image-{index}",
                "mime_type": "image/png",
            }
            for index in range(count)
        ]
        return LLMMessage(role="user", content="oracle prompt", images=images)

    cases = [
        ("no-message", None, True),
        ("no-images", message(0), True),
        ("images-with-support", message(2), True),
        ("images-without-support", message(2), False),
    ]
    records = []
    for case, value, supports_images in cases:
        counts = build_attachment_counts(value, supports_images=supports_images)
        records.append({
            "case": case,
            "supportsImages": supports_images,
            "counts": {kind.value: count for kind, count in counts.items()},
        })
    return records


# --------------------------------------------------------------------------
# eventVocabulary and eventPayloads
# --------------------------------------------------------------------------

#: Where each event is raised, read off the reference tree. A file listed here
#: is parsed for the literal payloads its call sites pass.
EMISSION_SITES = (
    "vibe/cli/textual_ui/app.py",
    "vibe/cli/voice_manager/voice_manager.py",
    "vibe/cli/narrator_manager/narrator_manager.py",
    "vibe/acp/agent.py",
    "vibe/acp/commands/controller.py",
    "vibe/core/telemetry/send.py",
)

#: The methods a call site raises an event through.
EMISSION_CALLS = ("send_telemetry_event", "record")


def capture_call_sites(tree: Path) -> dict[str, dict[str, Any]]:
    """Every event name a call site raises, with the literal payload keys it
    passes.

    Read with :mod:`ast` rather than driven, because these sites sit inside a
    running terminal application or an editor connection; only names and key
    names are recorded, never source.
    """

    found: dict[str, dict[str, Any]] = {}
    for relative in EMISSION_SITES:
        path = tree / relative
        module = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        for node in ast.walk(module):
            if not isinstance(node, ast.Call):
                continue
            name = _called_name(node.func)
            if name not in EMISSION_CALLS or not node.args:
                continue
            event = node.args[0]
            if not isinstance(event, ast.Constant) or not isinstance(event.value, str):
                continue
            if not event.value.startswith("vibe."):
                continue
            entry = found.setdefault(
                event.value, {"sites": [], "payloadKeys": [], "keywords": []}
            )
            if relative not in entry["sites"]:
                entry["sites"].append(relative)
            for key in _literal_keys(node.args[1] if len(node.args) > 1 else None):
                if key not in entry["payloadKeys"]:
                    entry["payloadKeys"].append(key)
            for keyword in node.keywords:
                if keyword.arg and keyword.arg not in entry["keywords"]:
                    entry["keywords"].append(keyword.arg)
    for entry in found.values():
        entry["sites"].sort()
        entry["payloadKeys"].sort()
        entry["keywords"].sort()
    return found


def _called_name(func: ast.expr) -> str | None:
    if isinstance(func, ast.Attribute):
        return func.attr
    if isinstance(func, ast.Name):
        return func.id
    return None


def _literal_keys(node: ast.expr | None) -> list[str]:
    if not isinstance(node, ast.Dict):
        return []
    keys = []
    for key in node.keys:
        if isinstance(key, ast.Constant) and isinstance(key.value, str):
            keys.append(key.value)
    return keys


def capture_event_payloads(tree: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """The property key set every event carries.

    The client's named senders and the two audio managers are driven, so their
    keys are what the reference actually put on the wire. The sites that only
    run inside a mounted terminal application or an editor connection are read
    from their call sites instead, and say so.
    """

    driven = asyncio.run(_drive_named_senders())
    driven |= _drive_audio_managers()
    call_sites = capture_call_sites(tree)

    payloads: list[dict[str, Any]] = []
    vocabulary: list[dict[str, Any]] = []
    for event in sorted(set(driven) | set(call_sites)):
        recorded = driven.get(event)
        site = call_sites.get(event, {})
        source = "driven" if recorded is not None else "callSite"
        keys = (
            sorted(recorded["properties"])
            if recorded is not None
            else site.get("payloadKeys", [])
        )
        payloads.append({
            "event": event,
            "source": source,
            "propertyKeys": keys,
            "propertyTypes": (
                {key: type_name(value) for key, value in sorted(recorded["properties"].items())}
                if recorded is not None
                else None
            ),
            "properties": scrub(recorded["properties"]) if recorded is not None else None,
            "correlated": recorded["correlated"] if recorded is not None else None,
        })
        vocabulary.append({
            "event": event,
            "source": source,
            "sites": site.get("sites", []),
            "sender": recorded["sender"] if recorded is not None else None,
        })
    return vocabulary, payloads


async def _drive_named_senders() -> dict[str, dict[str, Any]]:
    """Every ``send_*`` method on the client, called with scripted arguments."""

    from types import SimpleNamespace

    from vibe.core.config.admin_config import AdminConfigOutcome
    from vibe.core.telemetry.types import LaunchContext
    from vibe.utils.terminal import TerminalEmulator

    config = build_config(CONFIGURATIONS[0]["toml"])
    client = build_client(
        config,
        session_id_getter=lambda: "oracle-session",
        parent_session_id_getter=lambda: "oracle-parent-session",
        launch_context=LaunchContext(
            agent_entrypoint="cli",
            agent_version="oracle-agent-version",
            client_name="oracle-client",
            client_version="oracle-client-version",
            terminal_emulator=TerminalEmulator.GHOSTTY,
        ),
        user_plan_getter=lambda: "oracle-plan",
    )
    recorder = RecordingHTTPClient()
    client._client = recorder
    client.last_correlation_id = "oracle-last-request"

    tool_call = SimpleNamespace(
        tool_name="write_file",
        args_dict={"file_path": "/oracle/workspace/file.rs", "background": False},
    )
    decision = SimpleNamespace(
        verdict=SimpleNamespace(value="approved"),
        approval_type=SimpleNamespace(value="session"),
    )

    senders: list[tuple[str, Any]] = [
        (
            "send_tool_call_finished",
            lambda: client.send_tool_call_finished(
                tool_call=tool_call,
                status="success",
                decision=decision,
                agent_profile_name="oracle-profile",
                model="oracle-model",
                result={"background": False},
                message_id="oracle-message",
            ),
        ),
        ("send_user_copied_text", lambda: client.send_user_copied_text("oracle text")),
        (
            "send_user_cancelled_action",
            lambda: client.send_user_cancelled_action("interrupt_agent"),
        ),
        (
            "send_auto_compact_triggered",
            lambda: client.send_auto_compact_triggered(
                nb_context_tokens_before=150_000,
                auto_compact_threshold=120_000,
                status="success",
                session_id="oracle-session",
                parent_session_id="oracle-parent-session",
            ),
        ),
        (
            "send_compaction_failed",
            lambda: client.send_compaction_failed(
                reason="tool_call",
                session_id="oracle-session",
                parent_session_id="oracle-parent-session",
            ),
        ),
        (
            "send_slash_command_used",
            lambda: client.send_slash_command_used("/oracle", "builtin"),
        ),
        (
            "send_new_session",
            lambda: client.send_new_session(
                has_agents_md=True, nb_skills=3, nb_mcp_servers=2, nb_models=4
            ),
        ),
        ("send_session_closed", client.send_session_closed),
        (
            "send_admin_config_applied",
            lambda: client.send_admin_config_applied(
                outcome=next(iter(AdminConfigOutcome)),
                enforced_keys=["theme", "enable_telemetry"],
                error="oracle failure",
            ),
        ),
        (
            "send_onboarding_api_key_added",
            lambda: client.send_onboarding_api_key_added(custom_domain=True),
        ),
        (
            "send_request_sent",
            lambda: client.send_request_sent(
                model="oracle-model",
                nb_context_chars=2048,
                nb_context_messages=6,
                nb_prompt_chars=512,
                call_type="main_call",
                message_id="oracle-message",
                attachment_counts=None,
            ),
        ),
        ("send_ready", lambda: client.send_ready(init_duration_ms=1234)),
        (
            "send_at_mention_inserted",
            lambda: client.send_at_mention_inserted(
                nb_mentions=2,
                context_types={"file": 2},
                file_extensions={".rs": 2},
                message_id="oracle-message",
            ),
        ),
        (
            "send_user_rating_feedback",
            lambda: client.send_user_rating_feedback(5, "oracle-model"),
        ),
        (
            "send_teleport_completed",
            lambda: client.send_teleport_completed(
                push_required=True,
                nb_session_messages=12,
                context_summary="generated",
                context_summary_chars=480,
                project_picker={
                    "project_picker_shown": True,
                    "project_selection_source": "selected_existing",
                    "project_candidate_count_loaded": 3,
                    "project_multi_repo_match_count": 2,
                    "saved_project_link_cleared": False,
                    "project_repo_remote_changed": False,
                },
            ),
        ),
        (
            "send_teleport_failed",
            lambda: client.send_teleport_failed(
                stage="push",
                error_class="OracleError",
                push_required=True,
                nb_session_messages=12,
                context_summary="failed",
                context_summary_chars=None,
                error_details={"failure_kind": "http", "http_status_code": 403},
                project_picker={"project_picker_shown": False},
            ),
        ),
        (
            "send_remote_project_configured",
            lambda: client.send_remote_project_configured(
                outcome="configured",
                project_picker={
                    "project_picker_shown": True,
                    "project_selection_source": "created_project",
                },
            ),
        ),
    ]

    recorded: dict[str, dict[str, Any]] = {}
    for sender, invoke in senders:
        before = len(recorder.requests)
        invoke()
        await asyncio.sleep(0)
        for request in recorder.requests[before:]:
            body = request["json"] or {}
            recorded[body["event"]] = {
                "sender": sender,
                "properties": _client_properties(client, body.get("properties", {})),
                "correlated": "correlation_id" in body,
            }
    await client.aclose()
    return recorded


def _client_properties(client: Any, properties: dict[str, Any]) -> dict[str, Any]:
    """The event's own properties, with the base metadata the client merges in
    removed, so the payload family measures what the sender decided."""

    metadata = client.build_client_event_metadata()
    return {
        key: value
        for key, value in properties.items()
        if key not in metadata or metadata[key] != value
    }


def _drive_audio_managers() -> dict[str, dict[str, Any]]:
    """The seven audio emitters, driven on their own manager instances."""

    from vibe.cli.narrator_manager.narrator_manager import NarratorManager
    from vibe.cli.narrator_manager.telemetry import ReadAloudTrackingState
    from vibe.cli.voice_manager.telemetry import TranscriptionTrackingState
    from vibe.cli.voice_manager.voice_manager import VoiceManager

    class Sink:
        def __init__(self) -> None:
            self.events: list[tuple[str, dict[str, Any]]] = []

        def send_telemetry_event(
            self,
            event_name: str,
            properties: dict[str, Any],
            *,
            correlation_id: str | None = None,
        ) -> None:
            self.events.append((event_name, properties))

        def __bool__(self) -> bool:
            return True

    sink = Sink()

    voice = object.__new__(VoiceManager)
    voice._telemetry_client = sink
    voice._tracking = TranscriptionTrackingState()
    voice._tracking.set_recording_id("oracle-recording")
    voice._tracking.record_text("oracle transcript")
    voice._tracking.set_recording_duration(1.5)
    voice._on_audio_transcription_start()
    voice._on_audio_transcription_cancel()
    voice._on_audio_transcription_done()
    voice._on_audio_transcription_error("oracle failure")

    narrator = object.__new__(NarratorManager)
    narrator._telemetry_client = sink
    narrator._tracking = ReadAloudTrackingState()
    narrator._tracking.reset()
    narrator._on_read_aloud_requested()
    narrator._tracking.mark_play_started()
    narrator._on_read_aloud_play_started()
    narrator._on_read_aloud_ended("completed")

    recorded: dict[str, dict[str, Any]] = {}
    for event, properties in sink.events:
        recorded[event] = {
            "sender": "driven",
            # Durations and identifiers are wall-clock or random here, so the
            # family records the key set and the value types, which is what the
            # payload contract is.
            "properties": {key: _stable(key, value) for key, value in properties.items()},
            "correlated": False,
        }
    return recorded


#: Payload keys whose value is a clock reading or a generated identifier. The
#: key and the type are the contract; the value is not reproducible.
VOLATILE_KEYS = frozenset({
    "recording_duration_ms",
    "transcription_duration_ms",
    "time_to_first_read_s",
    "elapsed_seconds",
    "read_aloud_session_id",
})


def _stable(key: str, value: Any) -> Any:
    if key not in VOLATILE_KEYS:
        return value
    return f"{{{type_name(value)}}}"


# --------------------------------------------------------------------------
# exporterConfig
# --------------------------------------------------------------------------

EXPORTER_CASES: list[dict[str, Any]] = [
    {"case": "explicit-endpoint", "endpoint": "https://collector.example.test",
     "configuration": "mistral-active"},
    {"case": "explicit-endpoint-with-a-trailing-slash",
     "endpoint": "https://collector.example.test/", "configuration": "mistral-active"},
    {"case": "explicit-endpoint-with-a-path",
     "endpoint": "https://collector.example.test/ingest", "configuration": "mistral-active"},
    {"case": "derived-from-mistral", "endpoint": "", "configuration": "mistral-active"},
    {"case": "derived-from-a-proxy", "endpoint": "",
     "configuration": "mistral-behind-a-proxy"},
    {"case": "derived-from-a-base-without-a-version",
     "endpoint": "", "configuration": "mistral-base-without-a-version-segment"},
    {"case": "derived-without-a-resolvable-key", "endpoint": "",
     "configuration": "mistral-without-a-resolvable-key"},
    {"case": "no-mistral-provider", "endpoint": "", "configuration": "third-party-only"},
    {"case": "no-provider-at-all", "endpoint": "", "configuration": None},
]


def configuration_document(identifier: str) -> str:
    for scenario in CONFIGURATIONS:
        if scenario["id"] == identifier:
            return scenario["toml"]
    raise OracleError(f"no configuration named {identifier}")


def capture_exporter_config() -> list[dict[str, Any]]:
    from vibe.core import tracing

    records: list[dict[str, Any]] = []
    for entry in EXPORTER_CASES:
        provider = None
        if entry["configuration"] is not None:
            config = build_config(configuration_document(entry["configuration"]))
            provider = config.get_mistral_provider()
        resolved = tracing.build_otel_span_exporter_config(entry["endpoint"], provider)
        records.append({
            "case": entry["case"],
            "endpointRequested": entry["endpoint"],
            "configuration": entry["configuration"],
            "resolved": resolved is not None,
            "endpoint": resolved.endpoint if resolved else None,
            "headerNames": sorted(resolved.headers or {}) if resolved else [],
            "credentialVariable": (
                authorization_variable((resolved.headers or {}).get("Authorization"))
                if resolved
                else None
            ),
            "enableTelemetry": None,
            "enableOtel": None,
            "providerInstalled": None,
        })

    for entry in SETUP_CASES:
        records.append(_capture_setup_tracing(entry))
    return records


SETUP_CASES: list[dict[str, Any]] = [
    {"case": "setup-telemetry-off", "configuration": "mistral-active",
     "enableTelemetry": False, "enableOtel": True, "endpoint": ""},
    {"case": "setup-otel-off", "configuration": "mistral-active",
     "enableTelemetry": True, "enableOtel": False, "endpoint": ""},
    {"case": "setup-both-on", "configuration": "mistral-active",
     "enableTelemetry": True, "enableOtel": True, "endpoint": ""},
    {"case": "setup-both-on-explicit-endpoint", "configuration": "mistral-active",
     "enableTelemetry": True, "enableOtel": True,
     "endpoint": "https://collector.example.test"},
    {"case": "setup-both-on-without-a-key", "configuration":
     "mistral-without-a-resolvable-key", "enableTelemetry": True, "enableOtel": True,
     "endpoint": ""},
]


def _reset_tracer_provider() -> None:
    """Returns the global provider to unset, so each case observes its own."""

    from opentelemetry import trace

    trace._TRACER_PROVIDER = None
    trace._TRACER_PROVIDER_SET_ONCE._done = False


def _with_otel_keys(
    document: str, *, enable_telemetry: bool, enable_otel: bool, endpoint: str
) -> str:
    """The document a setup case loads: the same authored providers with the
    three tracing keys the case varies, written where a user would write them."""

    body = "\n".join(
        line for line in document.splitlines() if not line.startswith("enable_telemetry")
    )
    header = (
        f"enable_telemetry = {str(enable_telemetry).lower()}\n"
        f"enable_otel = {str(enable_otel).lower()}\n"
        f'otel_endpoint = "{endpoint}"\n'
    )
    return header + body


def _capture_setup_tracing(entry: dict[str, Any]) -> dict[str, Any]:
    from opentelemetry import trace
    from opentelemetry.sdk.trace import TracerProvider

    from vibe.core import tracing

    document = _with_otel_keys(
        configuration_document(entry["configuration"]),
        enable_telemetry=entry["enableTelemetry"],
        enable_otel=entry["enableOtel"],
        endpoint=entry["endpoint"],
    )
    _reset_tracer_provider()
    tracing.setup_tracing(build_config(document))
    provider = trace.get_tracer_provider()
    installed = isinstance(provider, TracerProvider)
    if installed:
        provider.shutdown()
    _reset_tracer_provider()
    return {
        "case": entry["case"],
        "endpointRequested": entry["endpoint"],
        "configuration": entry["configuration"],
        "resolved": None,
        "endpoint": None,
        "headerNames": [],
        "credentialVariable": None,
        "enableTelemetry": entry["enableTelemetry"],
        "enableOtel": entry["enableOtel"],
        "providerInstalled": installed,
    }


# --------------------------------------------------------------------------
# spans
# --------------------------------------------------------------------------


def _install_in_memory_tracing() -> Any:
    from opentelemetry import trace
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import SimpleSpanProcessor
    from opentelemetry.sdk.trace.export.in_memory_span_exporter import (
        InMemorySpanExporter,
    )

    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    _reset_tracer_provider()
    trace.set_tracer_provider(provider)
    return exporter


def capture_spans() -> list[dict[str, Any]]:
    exporter = _install_in_memory_tracing()
    records = asyncio.run(_run_spans(exporter))
    _reset_tracer_provider()
    return records


async def _run_spans(exporter: Any) -> list[dict[str, Any]]:
    from vibe.core import tracing
    from vibe.core.llm.exceptions import BackendError

    records: list[dict[str, Any]] = []

    def drain(case: str, extra: dict[str, Any] | None = None) -> None:
        for span in exporter.get_finished_spans():
            attributes = dict(span.attributes or {})
            records.append({
                "case": case,
                "name": span.name,
                "attributeKeys": sorted(attributes),
                "attributes": scrub({
                    key: _attribute_value(value) for key, value in sorted(attributes.items())
                }),
                "statusCode": span.status.status_code.name,
                "statusDescription": (
                    digest(span.status.description)
                    if span.status.description is not None
                    else None
                ),
                "recordedExceptions": len(span.events),
                "recording": True,
                "bodyRan": True,
                "raised": None,
                "backendProvider": None,
                "backendStatus": None,
                "recordUnhandledException": None,
                **(extra or {}),
            })
        exporter.clear()

    async with tracing.agent_span(model="oracle-model", session_id="oracle-session"):
        pass
    drain("agent-span")

    async with tracing.agent_span():
        pass
    drain("agent-span-without-model-or-session")

    async with tracing.agent_span(model="oracle-model", session_id="oracle-session"):
        async with tracing.tool_span(
            tool_name="read_file", call_id="oracle-call", arguments='{"path":"a.rs"}'
        ) as span:
            tracing.set_tool_result(span, "oracle result")
    drain("tool-span-inside-an-agent-span")

    async with tracing.tool_span(
        tool_name="read_file", call_id="oracle-call", arguments='{"path":"a.rs"}'
    ):
        pass
    drain("tool-span-without-a-parent")

    async with tracing.agent_span(model="oracle-model", session_id="oracle-session"):
        async with tracing.model_call_span(
            provider_name="mistral",
            provider_api_style="openai",
            model="oracle-model",
            streaming=True,
            temperature=0.3,
            max_tokens=512,
            metadata={"call_type": "main_call", "message_id": "oracle-message"},
            http_url="https://api.mistral.ai/v1/chat/completions",
        ) as span:
            tracing.set_model_call_http_status(span, 200)
            tracing.set_model_call_usage(span, prompt_tokens=120, completion_tokens=34)
            tracing.set_model_call_response_metadata(
                span,
                {
                    "id": "oracle-response",
                    "model": "oracle-response-model",
                    "choices": [{"finish_reason": "stop"}],
                },
            )
    drain("model-call-span-inside-an-agent-span")

    async with tracing.model_call_span(
        provider_name="third-party",
        provider_api_style="openai",
        model="oracle-model",
        streaming=False,
        metadata={"session_id": "oracle-metadata-session"},
    ):
        pass
    drain("model-call-span-from-metadata-session")

    async with tracing.hook_span(
        hook_name="oracle-hook",
        hook_type="pre_tool_use",
        tool_name="read_file",
        tool_call_id="oracle-call",
    ):
        pass
    drain("hook-span")

    async with tracing.agent_span(model="oracle-model", session_id="oracle-session"):
        async with tracing.hook_span(hook_name="oracle-hook", hook_type="stop"):
            pass
    drain("hook-span-inside-an-agent-span")

    for case, status in (
        ("model-call-span-raising-a-backend-error", 503),
        ("model-call-span-raising-a-backend-error-without-a-status", None),
    ):
        with contextlib.suppress(BackendError):
            async with tracing.model_call_span(
                provider_name="mistral",
                provider_api_style="openai",
                model="oracle-model",
                streaming=False,
            ):
                raise _backend_error(status)
        drain(
            case,
            {"raised": "BackendError", "backendProvider": "mistral",
             "backendStatus": status},
        )

    with contextlib.suppress(RuntimeError):
        async with tracing.model_call_span(
            provider_name="mistral",
            provider_api_style="openai",
            model="oracle-model",
            streaming=False,
        ):
            raise RuntimeError("oracle wrapper") from _backend_error(429)
    drain(
        "model-call-span-wrapping-a-backend-error",
        {"raised": "RuntimeError", "backendProvider": "mistral", "backendStatus": 429},
    )

    with contextlib.suppress(RuntimeError):
        async with tracing.model_call_span(
            provider_name="mistral",
            provider_api_style="openai",
            model="oracle-model",
            streaming=False,
        ):
            raise RuntimeError("oracle unhandled failure")
    drain(
        "model-call-span-raising-an-unhandled-exception",
        {"raised": "RuntimeError", "recordUnhandledException": False},
    )

    with contextlib.suppress(RuntimeError):
        async with tracing.tool_span(
            tool_name="read_file", call_id="oracle-call", arguments="{}"
        ):
            raise RuntimeError("oracle unhandled failure")
    drain(
        "tool-span-raising-an-unhandled-exception",
        {"raised": "RuntimeError", "recordUnhandledException": True},
    )

    return records


def _backend_error(status: int | None) -> Exception:
    """A backend failure built from script-authored values, so the status
    description the span carries is decided here rather than by a live call."""

    from vibe.core.llm.exceptions import BackendError, PayloadSummary

    return BackendError(
        provider="mistral",
        endpoint="https://api.mistral.ai/v1/chat/completions",
        status=status,
        reason="oracle reason",
        headers={},
        body_text="oracle body",
        parsed_error=None,
        model="oracle-model",
        payload_summary=PayloadSummary(
            model="oracle-model",
            message_count=3,
            approx_chars=512,
            temperature=0.3,
            has_tools=False,
            tool_choice=None,
        ),
    )


def _attribute_value(value: Any) -> Any:
    if isinstance(value, tuple):
        return list(value)
    return value


def capture_span_without_a_provider() -> list[dict[str, Any]]:
    """The never-raise policy: with no provider installed the body still runs
    and the span is not recording."""

    from vibe.core import tracing

    _reset_tracer_provider()

    async def run() -> list[dict[str, Any]]:
        records = []
        async with tracing.agent_span(model="oracle-model") as span:
            records.append({
                "case": "agent-span-without-a-provider",
                "name": None,
                "attributeKeys": [],
                "attributes": {},
                "statusCode": None,
                "statusDescription": None,
                "recordedExceptions": 0,
                "recording": span.is_recording(),
                "bodyRan": True,
                "raised": None,
                "backendProvider": None,
                "backendStatus": None,
                "recordUnhandledException": None,
            })
        return records

    return asyncio.run(run())


# --------------------------------------------------------------------------
# providerNames
# --------------------------------------------------------------------------

PROVIDER_NAMES = (
    "mistral",
    "mistral_ai",
    "mistral-ai",
    "MISTRAL",
    "  mistral  ",
    "google_vertex",
    "google-vertex",
    "vertex",
    "google",
    "azure_openai",
    "bedrock",
    "xai",
    "openai",
    "anthropic",
    "unknown-provider",
    "",
    None,
)


def capture_provider_names() -> list[dict[str, Any]]:
    from vibe.core.tracing import _provider_attribute_value

    return [
        {"input": name, "normalized": _provider_attribute_value(name)}
        for name in PROVIDER_NAMES
    ]


# --------------------------------------------------------------------------
# redaction
# --------------------------------------------------------------------------

#: Attribute sets a span carries into the redaction policy. Every value is
#: authored here, so what the corpus records is which key survived and which
#: value was replaced, never a secret. The second set carries a credential-shaped
#: string, which is what separates the key-oriented mode from the pattern-oriented
#: one: the first set has nothing for a pattern to match.
REDACTION_SETS: dict[str, dict[str, Any]] = {
    "content-attributes": {
        "gen_ai.operation.name": "chat",
        "gen_ai.provider.name": "mistral_ai",
        "gen_ai.request.model": "oracle-model",
        "gen_ai.conversation.id": "oracle-session",
        "gen_ai.tool.name": "read_file",
        "gen_ai.tool.call.arguments": '{"file_path": "/oracle/workspace/file.rs"}',
        "gen_ai.tool.call.result": "oracle tool output",
        "gen_ai.usage.input_tokens": 120,
        "gen_ai.usage.output_tokens": 34,
        "gen_ai.response.finish_reasons": ("stop",),
        "http.request.method": "POST",
        "http.response.status_code": 200,
        "vibe.provider.api_style": "openai",
        "vibe.request.call_type": "main_call",
    },
    "credential-shaped-attributes": {
        "gen_ai.request.model": "oracle-model",
        "gen_ai.tool.call.arguments": "sk-oracleoracleoracleoracle",
        "vibe.provider.api_style": "Bearer oracleoracleoracleoracle",
        "http.url": "https://api.mistral.ai/v1/chat/completions",
    },
}


def capture_redaction() -> list[dict[str, Any]]:
    from opentelemetry import trace
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import SimpleSpanProcessor
    from opentelemetry.sdk.trace.export.in_memory_span_exporter import (
        InMemorySpanExporter,
    )

    from mistralai.extra.observability import (
        AttributeRedactionPolicy,
        RedactingSpanExporter,
        default_redaction_policy,
    )

    from vibe.core.config.models import OtelRedactionMode

    records: list[dict[str, Any]] = []
    for name, declared in REDACTION_SETS.items():
        for mode in OtelRedactionMode:
            collector = InMemorySpanExporter()
            if mode is OtelRedactionMode.NONE:
                exporter: Any = collector
            else:
                policy = (
                    AttributeRedactionPolicy()
                    if mode is OtelRedactionMode.STRICT
                    else default_redaction_policy()
                )
                exporter = RedactingSpanExporter(collector, policy)
            provider = TracerProvider()
            provider.add_span_processor(SimpleSpanProcessor(exporter))
            _reset_tracer_provider()
            trace.set_tracer_provider(provider)
            tracer = trace.get_tracer("oracle")
            with tracer.start_as_current_span("chat oracle-model") as span:
                for key, value in declared.items():
                    span.set_attribute(key, value)
            provider.shutdown()
            for exported in collector.get_finished_spans():
                attributes = dict(exported.attributes or {})
                unchanged = sorted(
                    key
                    for key, value in attributes.items()
                    if key in declared
                    and _attribute_value(value) == _attribute_value(declared[key])
                )
                records.append({
                    "case": f"{name}-{mode.value}",
                    "attributeSet": name,
                    "mode": mode.value,
                    "spanName": exported.name,
                    "survivingKeys": sorted(attributes),
                    "unchangedKeys": unchanged,
                    "replacedKeys": sorted(set(attributes) - set(unchanged)),
                    "addedKeys": sorted(set(attributes) - set(declared)),
                    "removedKeys": sorted(set(declared) - set(attributes)),
                })
            collector.clear()
    _reset_tracer_provider()
    return records


# --------------------------------------------------------------------------
# logFormat, logEncoding, logParse
# --------------------------------------------------------------------------

LOG_MESSAGES = (
    "oracle plain message",
    "oracle message with a backslash \\ inside",
    "oracle message\nwith a newline",
    "oracle message with \\n already written out",
    "oracle message with a trailing backslash \\",
    "",
)


def capture_log_format() -> list[dict[str, Any]]:
    from vibe.observability.logging import StructuredLogFormatter

    formatter = StructuredLogFormatter()
    records: list[dict[str, Any]] = []
    for level in ("DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"):
        for index, message in enumerate(LOG_MESSAGES):
            record = logging.LogRecord(
                name="vibe",
                level=getattr(logging, level),
                pathname=__file__,
                lineno=1,
                msg=message,
                args=(),
                exc_info=None,
            )
            records.append({
                "case": f"{level.lower()}-message-{index}",
                "level": level,
                "message": message,
                "line": _normalize_line(formatter.format(record)),
                "hasException": False,
                "exceptionSeparator": None,
                "exceptionEncodedNewlines": None,
            })
    try:
        raise RuntimeError("oracle failure")
    except RuntimeError:
        record = logging.LogRecord(
            name="vibe",
            level=logging.ERROR,
            pathname=__file__,
            lineno=1,
            msg="oracle message with an exception",
            args=(),
            exc_info=sys.exc_info(),
        )
        line = formatter.format(record)
    prefix, _, remainder = line.partition("oracle message with an exception")
    records.append({
        "case": "error-with-exception-info",
        "level": "ERROR",
        "message": "oracle message with an exception",
        "line": _normalize_line(prefix + "oracle message with an exception"),
        "hasException": True,
        # The traceback is machine-dependent, so only its shape is recorded: it
        # follows the message after one separator and carries no newline.
        "exceptionSeparator": remainder[:1],
        "exceptionEncodedNewlines": "\n" not in remainder,
    })
    return records


def _normalize_line(line: str) -> str:
    """The formatted line with its clock and its process identifiers replaced,
    so what the corpus holds is the format rather than one run of it."""

    parts = line.split(" ", 4)
    if len(parts) < 4:
        return line
    timestamp, ppid, pid = parts[0], parts[1], parts[2]
    if ppid != str(os.getppid()) or pid != str(os.getpid()):
        raise OracleError("the formatted line does not carry this process")
    from datetime import datetime

    datetime.fromisoformat(timestamp)
    rest = parts[4] if len(parts) > 4 else ""
    return " ".join([
        TIMESTAMP_PLACEHOLDER,
        PPID_PLACEHOLDER,
        PID_PLACEHOLDER,
        parts[3],
        rest,
    ])


def capture_log_encoding() -> list[dict[str, Any]]:
    from vibe.observability.logging import decode_log_message, encode_log_message

    records = []
    for message in LOG_MESSAGES:
        encoded = encode_log_message(message)
        records.append({
            "input": message,
            "encoded": encoded,
            "decoded": decode_log_message(encoded),
            "roundTrips": decode_log_message(encoded) == message,
        })
    for encoded in ("already\\\\escaped", "trailing\\", "\\n", "\\t", "plain"):
        records.append({
            "input": None,
            "encoded": encoded,
            "decoded": decode_log_message(encoded),
            "roundTrips": None,
        })
    return records


PARSE_LINES = (
    ("well-formed", "2026-02-21T10:28:51.529718+00:00 12 34 WARNING oracle message"),
    ("without-an-offset", "2026-02-21T10:28:51.529718 12 34 INFO oracle message"),
    ("with-an-encoded-newline",
     "2026-02-21T10:28:51.529718+00:00 12 34 ERROR oracle\\nmessage"),
    ("malformed-timestamp", "21-02-2026 12 34 WARNING oracle message"),
    ("unknown-level", "2026-02-21T10:28:51.529718+00:00 12 34 TRACE oracle message"),
    ("missing-message", "2026-02-21T10:28:51.529718+00:00 12 34 WARNING"),
    ("empty-line", ""),
    ("not-a-log-line", "oracle wrote this by hand"),
    ("negative-identifiers", "2026-02-21T10:28:51.529718+00:00 -1 34 WARNING oracle"),
    ("extra-whitespace",
     "2026-02-21T10:28:51.529718+00:00   12   34   DEBUG   oracle message"),
    ("no-fractional-seconds", "2026-02-21T10:28:51+00:00 12 34 WARNING oracle message"),
    ("critical-level", "2026-02-21T10:28:51.529718+00:00 12 34 CRITICAL oracle message"),
)


def capture_log_parse() -> list[dict[str, Any]]:
    from vibe.core.log_reader import LogReader

    reader = LogReader(log_file=Path("/oracle/does-not-exist.log"))
    records = []
    for case, line in PARSE_LINES:
        entry = reader._parse_line(line, 7)
        records.append({
            "case": case,
            "line": line,
            "parsed": entry is not None,
            "timestamp": entry.timestamp.isoformat() if entry else None,
            "ppid": entry.ppid if entry else None,
            "pid": entry.pid if entry else None,
            "level": entry.level if entry else None,
            "message": entry.message if entry else None,
            "lineNumber": entry.line_number if entry else None,
        })
    return records


# --------------------------------------------------------------------------
# logPagination
# --------------------------------------------------------------------------

#: The file every pagination case is read from, authored here line by line.
PAGINATION_LINES = [
    f"2026-02-21T10:28:{index:02d}.100000+00:00 12 34 WARNING oracle entry {index}"
    for index in range(12)
]
#: Interleaved lines no pattern matches, so a page still returns.
PAGINATION_NOISE = {3: "oracle wrote this by hand", 8: ""}

PAGINATION_CASES = (
    (100, 0),
    (5, 0),
    (5, 5),
    (5, 10),
    (5, 20),
    (1, 0),
    (1, 11),
    (12, 0),
    (100, 3),
)


def capture_log_pagination() -> list[dict[str, Any]]:
    from vibe.core.log_reader import LogReader

    with tempfile.TemporaryDirectory(prefix="telemetry-oracle-") as scratch:
        path = Path(scratch) / "vibe.log"
        lines: list[str] = []
        for index, line in enumerate(PAGINATION_LINES):
            if index in PAGINATION_NOISE:
                lines.append(PAGINATION_NOISE[index])
            lines.append(line)
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")

        records: list[dict[str, Any]] = []
        for limit, offset in PAGINATION_CASES:
            reader = LogReader(log_file=path)
            page = reader.get_logs(limit=limit, offset=offset)
            records.append({
                "case": f"limit-{limit}-offset-{offset}",
                "limit": limit,
                "offset": offset,
                "messages": [entry.message for entry in page.entries],
                "lineNumbers": [entry.line_number for entry in page.entries],
                "hasMore": page.has_more,
                "cursor": page.cursor,
                "newLinesCountReset": None,
            })

        missing = LogReader(log_file=Path(scratch) / "absent.log")
        page = missing.get_logs()
        records.append({
            "case": "file-absent",
            "limit": 100,
            "offset": 0,
            "messages": [],
            "lineNumbers": [],
            "hasMore": page.has_more,
            "cursor": page.cursor,
            "newLinesCountReset": None,
        })

        empty = Path(scratch) / "empty.log"
        empty.write_text("", encoding="utf-8")
        page = LogReader(log_file=empty).get_logs()
        records.append({
            "case": "file-empty",
            "limit": 100,
            "offset": 0,
            "messages": [],
            "lineNumbers": [],
            "hasMore": page.has_more,
            "cursor": page.cursor,
            "newLinesCountReset": None,
        })

        records.append(_capture_shrink(Path(scratch)))
    return records


def _capture_shrink(scratch: Path) -> dict[str, Any]:
    """A file that shrinks between polls resets the reader rather than reading
    past its end."""

    from vibe.core.log_reader import LogReader

    path = scratch / "rotating.log"
    path.write_text("\n".join(PAGINATION_LINES) + "\n", encoding="utf-8")
    consumed: list[str] = []
    reader = LogReader(log_file=path, consumer=lambda entry: consumed.append(entry.message))
    reader._last_position = path.stat().st_size
    reader._new_lines_count = len(PAGINATION_LINES)
    path.write_text(PAGINATION_LINES[0] + "\n", encoding="utf-8")
    reader._process_new_content()
    return {
        "case": "file-shrank-between-polls",
        "limit": 0,
        "offset": 0,
        "messages": consumed,
        "lineNumbers": [],
        "hasMore": False,
        "cursor": reader._last_position,
        "newLinesCountReset": reader._new_lines_count,
    }


# --------------------------------------------------------------------------
# logConfig
# --------------------------------------------------------------------------

LOG_CONFIG_CASES: list[dict[str, str | None]] = [
    {"case": "defaults", "LOG_LEVEL": None, "DEBUG_MODE": None, "LOG_MAX_BYTES": None},
    {"case": "level-debug", "LOG_LEVEL": "DEBUG", "DEBUG_MODE": None, "LOG_MAX_BYTES": None},
    {"case": "level-lowercase", "LOG_LEVEL": "info", "DEBUG_MODE": None, "LOG_MAX_BYTES": None},
    {"case": "level-unknown", "LOG_LEVEL": "TRACE", "DEBUG_MODE": None, "LOG_MAX_BYTES": None},
    {"case": "level-empty", "LOG_LEVEL": "", "DEBUG_MODE": None, "LOG_MAX_BYTES": None},
    {"case": "debug-mode-true", "LOG_LEVEL": "ERROR", "DEBUG_MODE": "true", "LOG_MAX_BYTES": None},
    {"case": "debug-mode-other", "LOG_LEVEL": "ERROR", "DEBUG_MODE": "1", "LOG_MAX_BYTES": None},
    {"case": "max-bytes", "LOG_LEVEL": None, "DEBUG_MODE": None, "LOG_MAX_BYTES": "4096"},
    {"case": "max-bytes-zero", "LOG_LEVEL": None, "DEBUG_MODE": None, "LOG_MAX_BYTES": "0"},
]


def capture_log_config() -> list[dict[str, Any]]:
    from vibe.observability.logging import _VibeFileHandler, init_file_logging

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="telemetry-oracle-") as scratch:
        for index, entry in enumerate(LOG_CONFIG_CASES):
            target = logging.getLogger(f"oracle-log-{index}")
            path = Path(scratch) / f"{index}" / "logs" / "vibe.log"
            variables = {
                name: entry[name]  # type: ignore[index]
                for name in LOG_VARIABLES
            }
            with patched_environ(variables):
                init_file_logging(path, target_logger=target)
                before = len(target.handlers)
                init_file_logging(path, target_logger=target)
            handler = next(
                handler
                for handler in target.handlers
                if isinstance(handler, _VibeFileHandler)
            )
            records.append({
                "case": entry["case"],
                "level": logging.getLevelName(handler.level),
                "maxBytes": handler.maxBytes,
                "backupCount": handler.backupCount,
                "encoding": handler.encoding,
                "loggerLevel": logging.getLevelName(target.level),
                "directoryCreated": path.parent.is_dir(),
                "handlerCount": before,
                "duplicateGuarded": len(target.handlers) == before,
                "raised": None,
            })
            for handler in list(target.handlers):
                handler.close()
                target.removeHandler(handler)
    with_invalid = _capture_invalid_max_bytes()
    if with_invalid is not None:
        records.append(with_invalid)
    return records


def _capture_invalid_max_bytes() -> dict[str, Any] | None:
    """``LOG_MAX_BYTES`` that is not a number: the reference lets ``int`` raise,
    which is the behavior the replay compares."""

    from vibe.observability.logging import init_file_logging

    with tempfile.TemporaryDirectory(prefix="telemetry-oracle-") as scratch:
        target = logging.getLogger("oracle-log-invalid")
        path = Path(scratch) / "logs" / "vibe.log"
        with patched_environ({"LOG_MAX_BYTES": "not-a-number", "LOG_LEVEL": None,
                              "DEBUG_MODE": None}):
            try:
                init_file_logging(path, target_logger=target)
            except ValueError:
                return {
                    "case": "max-bytes-invalid",
                    "level": None,
                    "maxBytes": None,
                    "backupCount": None,
                    "encoding": None,
                    "loggerLevel": None,
                    "directoryCreated": path.parent.is_dir(),
                    "handlerCount": 0,
                    "duplicateGuarded": None,
                    "raised": "ValueError",
                }
        for handler in list(target.handlers):
            handler.close()
            target.removeHandler(handler)
    return None


# --------------------------------------------------------------------------
# Corpus
# --------------------------------------------------------------------------


def build_families(tree: Path, reference: Path) -> dict[str, Any]:
    vocabulary, payloads = capture_event_payloads(tree)
    return {
        "constants": capture_constants(reference),
        "envelope": capture_envelope(),
        "baseMetadata": capture_base_metadata(),
        "attachmentCounts": capture_attachment_counts(),
        "eventVocabulary": vocabulary,
        "eventPayloads": payloads,
        "exporterConfig": capture_exporter_config(),
        "spans": capture_spans() + capture_span_without_a_provider(),
        "providerNames": capture_provider_names(),
        "redaction": capture_redaction(),
        "logFormat": capture_log_format(),
        "logEncoding": capture_log_encoding(),
        "logParse": capture_log_parse(),
        "logPagination": capture_log_pagination(),
        "logConfig": capture_log_config(),
    }


def build_corpus(commit: str, families: dict[str, Any]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": commit},
        # Not a family: the documents every family is measured over, written the
        # way a user writes them so both sides load the same bytes.
        "documents": [
            {"id": scenario["id"], "toml": scenario["toml"]}
            for scenario in CONFIGURATIONS
        ],
        "note": (
            "Telemetry and observability corpus: what the pinned reference's datalake client, "
            "metadata builders, event senders, tracing module, redaction policy, log formatter "
            "and log reader answer for each scripted input. Values, key sets, vocabularies and "
            "verdicts are committed as they stand because the scenarios supplied them; a "
            "credential is recorded as the environment variable it came from and never as a "
            "value, and every reference-authored sentence is committed as a SHA-256 digest and "
            "a length. Regenerate with scripts/parity/telemetry.py --corpus when the pinned "
            "reference moves."
        ),
        **families,
    }


def assert_no_credential_leaked(corpus: dict[str, Any]) -> None:
    encoded = json.dumps(corpus)
    for name, sentinel in SENTINELS.items():
        if sentinel in encoded:
            raise OracleError(
                f"the corpus carries the value of {name}; record the variable instead"
            )


def count_comparisons(families: dict[str, Any]) -> dict[str, int]:
    counted = {}
    for family, value in families.items():
        counted[family] = len(value) if isinstance(value, list) else 1
    return counted


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="?",
        const=DEFAULT_CORPUS,
        default=None,
        help=(
            "also write the committed corpus, which the Rust replay reads unconditionally "
            f"(default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def write_json(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(staged, path)


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(
            arguments.reference, arguments.python, pinned
        )
        install_sentinels()
        GUARD.install()
        with tempfile.TemporaryDirectory(prefix="telemetry-oracle-home-") as home:
            with patched_environ({"VIBE_HOME": home}):
                from vibe.core.config.harness_files import init_harness_files_manager
                from vibe.utils import api_keys

                # The prompt validators reach the harness-files singleton, and a
                # keyring lookup would leave the process, so the credential
                # sources are the sentinels and nothing else.
                init_harness_files_manager()
                api_keys.get_api_key_from_keyring = lambda _: None
                families = build_families(pinned, arguments.reference)
    except OracleError as error:
        print(f"telemetry capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(
            f"telemetry capture failed: network attempts {GUARD.attempts}",
            file=sys.stderr,
        )
        return 1

    corpus = build_corpus(reference["commit"], families)
    try:
        assert_no_credential_leaked(corpus)
    except OracleError as error:
        print(f"telemetry capture failed: {error}", file=sys.stderr)
        return 1

    write_json(
        arguments.output,
        {
            **corpus,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
        },
    )
    if arguments.corpus is not None:
        write_json(arguments.corpus, corpus)

    counted = count_comparisons(families)
    total = sum(counted.values())
    print(
        f"captured {total} records across {len(counted)} families "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family, count in counted.items():
        print(f"  {family}: {count}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
