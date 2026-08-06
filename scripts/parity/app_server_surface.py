#!/usr/bin/env python3
"""Capture the app-server wire surface the pinned Python reference declares.

The reference checkout is a read-only behavioral oracle. This script imports
its ``protocol`` and ``_connection_protocol`` modules and records what crosses
the wire: the method inventory, the client-tool and notification vocabularies,
the protocol error codes, the enum value sets, the discriminated unions and a
per-model field census carrying each field's camelCase alias, required flag and
declared type kind.

Everything captured is a name, an alias, an enum value, a discriminator value or
a boolean. No description, docstring or other reference-authored prose is read
or written, which is what lets the corpus be committed under ``NOTICE``, exactly
as ``scripts/parity/config_surface.py`` already does for configuration.

Usage::

    scripts/parity/app_server_surface.py --reference /path/to/reference
    scripts/parity/app_server_surface.py --interpreter /path/to/python

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The checkout must sit exactly on the
pinned commit: a corpus captured from any other revision is not an oracle for
the replay that reads it, so the script refuses to write one.

The wrapper re-executes itself with an interpreter that can import ``vibe`` when
the current one cannot.
"""

from __future__ import annotations

import argparse
import enum
import json
import os
from pathlib import Path
import subprocess
import sys
import types
import typing
from typing import Annotated, Any, Literal, Union, get_args, get_origin

SCHEMA_VERSION = 1
#: Where the read-only reference checkout lives. ``VIBE_REFERENCE`` overrides the
#: default for machines that hold it elsewhere, and ``--reference`` wins over both.
DEFAULT_REFERENCE = Path(
    os.environ.get("VIBE_REFERENCE") or "/home/arthur/dev/mistral-vibe"
)
DEFAULT_OUTPUT = Path("crates/vibe-app-server/tests/app-server-surface/corpus.json")
EXPECTED_COMMIT = "68ff32e6a92e80a874c8153312f0aa8ae4955477"
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"
#: Longest string the corpus may carry. A name, alias, pointer or enum value is
#: far shorter; anything longer would be prose, which ``NOTICE`` forbids.
MAX_CAPTURED_STRING = 200

NOTE = (
    "Captured by scripts/parity/app_server_surface.py from the pinned reference. "
    "Names, aliases, required flags, type kinds, enum values and discriminator "
    "values only: no reference-authored prose."
)


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def resolve_reference(reference: Path, expected_commit: str) -> dict[str, str]:
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
    if commit != expected_commit:
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
        import vibe.app_server.protocol  # noqa: F401

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
# The surface this capture asserts
# --------------------------------------------------------------------------

#: Which wire model each routed method takes and answers with. The names are
#: read from the reference dispatchers; the capture resolves every one of them
#: against the pinned module and fails when a name no longer exists, so a rename
#: upstream surfaces here rather than in a silently thinner corpus.
METHOD_MODELS: dict[str, tuple[str, str]] = {
    "account/read": ("AccountReadParams", "AccountReadResponse"),
    "agents/install": ("AgentInstallParams", "AgentsListResponse"),
    "agents/list": ("AgentsListParams", "AgentsListResponse"),
    "agents/uninstall": ("AgentInstallParams", "AgentsListResponse"),
    "callback/respond": ("CallbackRespondParams", "CallbackRespondResponse"),
    "config/fields/read": ("ConfigFieldsReadParams", "ConfigFieldsReadResponse"),
    "config/patch": ("ConfigPatchParams", "ConfigPatchResponse"),
    "config/proxy/read": ("ConfigProxyReadParams", "ConfigProxyReadResponse"),
    "config/proxy/write": ("ConfigProxyWriteParams", "EmptyResponse"),
    "config/read": ("ConfigReadParams", "ConfigReadResponse"),
    "config/reload": ("ConfigReloadParams", "ConfigMutationResponse"),
    "config/schema": ("ConfigSchemaReadParams", "ConfigSchemaReadResponse"),
    "config/thinking/write": ("ConfigThinkingWriteParams", "ConfigMutationResponse"),
    "connectors/auth/read": ("ConnectorAuthReadParams", "ConnectorAuthReadResponse"),
    "connectors/read": ("ConnectorsReadParams", "ConnectorsReadResponse"),
    "connectors/refresh": ("ConnectorRefreshParams", "ConnectorRefreshResponse"),
    "diagnostics/list": ("DiagnosticsListParams", "DiagnosticsListResponse"),
    "diagnostics/logs/read": (
        "DiagnosticsLogsReadParams",
        "DiagnosticsLogsReadResponse",
    ),
    "feedback/record": ("FeedbackRecordParams", "EmptyResponse"),
    "feedback/shouldShow": ("FeedbackShouldShowParams", "FeedbackShouldShowResponse"),
    "history/list": ("HistoryListParams", "HistoryListResponse"),
    "loops/clear": ("LoopsClearParams", "LoopsClearResponse"),
    "loops/create": ("LoopsCreateParams", "LoopsCreateResponse"),
    "loops/delete": ("LoopsDeleteParams", "LoopsDeleteResponse"),
    "loops/list": ("LoopsListParams", "LoopsListResponse"),
    "mcp/add": ("MCPAddParams", "MCPAddResponse"),
    "mcp/login": ("MCPLoginParams", "RuntimeMutationResponse"),
    "mcp/logout": ("MCPLogoutParams", "RuntimeMutationResponse"),
    "mcp/read": ("MCPReadParams", "MCPReadResponse"),
    "mcp/refresh": ("MCPRefreshParams", "RuntimeMutationResponse"),
    "mcp/toggle": ("MCPToggleParams", "RuntimeMutationResponse"),
    "narration/summarize": ("NarrationSummarizeParams", "NarrationSummarizeResponse"),
    "projectLinks/create": ("ProjectLinksCreateParams", "ProjectLinkMutationResponse"),
    "projectLinks/inspectRoot": (
        "ProjectLinksInspectRootParams",
        "ProjectLinksInspectRootResponse",
    ),
    "projectLinks/link": ("ProjectLinksLinkParams", "ProjectLinkMutationResponse"),
    "projectLinks/list": ("ProjectLinksListParams", "ProjectLinksListResponse"),
    "projectLinks/picker/load": (
        "ProjectLinksPickerLoadParams",
        "ProjectLinksPickerLoadResponse",
    ),
    "projectLinks/picker/loadMore": (
        "ProjectLinksPickerLoadMoreParams",
        "ProjectLinksPickerLoadMoreResponse",
    ),
    "projectLinks/resolveRoot": (
        "ProjectLinksResolveRootParams",
        "ProjectLinksResolveRootResponse",
    ),
    "projectLinks/save": ("ProjectLinksSaveParams", "ProjectLinkMutationResponse"),
    "projectLinks/unlink": ("ProjectLinksUnlinkParams", "ProjectLinksUnlinkResponse"),
    "review/approve": ("ReviewMutationParams", "EmptyResponse"),
    "review/baseline": ("ReviewBaselineParams", "ReviewBaselineResponse"),
    "review/hunks": ("ReviewHunksParams", "ReviewHunksResponse"),
    "review/revert": ("ReviewMutationParams", "EmptyResponse"),
    "review/state": ("ReviewStateParams", "ReviewStateResponse"),
    "review/turnDiff": ("ReviewTurnDiffParams", "ReviewTurnDiffResponse"),
    "runtime/read": ("RuntimeReadParams", "RuntimeReadResponse"),
    "session/agent/update": ("AgentSwitchParams", "RuntimeMutationResponse"),
    "session/close": ("SessionCloseParams", "SessionCloseResponse"),
    "session/compact/start": ("SessionCompactParams", "SessionCompactResponse"),
    "session/context/inject": ("ContextInjectParams", "ContextInjectResponse"),
    "session/continue": ("SessionContinueParams", "SessionContinueResponse"),
    "session/delete": ("SessionDeleteParams", "EmptyResponse"),
    "session/fork": ("SessionForkParams", "SessionForkResponse"),
    "session/history/clear": (
        "SessionHistoryClearParams",
        "SessionHistoryClearResponse",
    ),
    "session/list": ("SessionListParams", "SessionListResponse"),
    "session/log/read": ("SessionLogReadParams", "SessionLogReadResponse"),
    "session/read": ("SessionReadParams", "SessionReadResponse"),
    "session/ready/read": ("SessionReadyReadParams", "SessionReadyReadResponse"),
    "session/ready/wait": ("SessionReadyWaitParams", "SessionReadyWaitResponse"),
    "session/resume": ("SessionResumeParams", "SessionResumeResponse"),
    "session/rewind": ("SessionRewindParams", "SessionRewindResponse"),
    "session/rewind/read": ("SessionRewindReadParams", "SessionRewindReadResponse"),
    "session/settings/update": ("SessionSettingsUpdateParams", "EmptyResponse"),
    "session/start": ("SessionStartParams", "SessionStartResponse"),
    "session/title/update": ("SessionTitleUpdateParams", "SessionTitleUpdateResponse"),
    "shell/interrupt": ("ShellInterruptParams", "ShellInterruptResponse"),
    "shell/run": ("ShellRunParams", "ShellRunResponse"),
    "skills/list": ("SkillsListParams", "SkillsListResponse"),
    "stats/read": ("StatsReadParams", "StatsReadResponse"),
    "telemetry/record": ("TelemetryRecordParams", "EmptyResponse"),
    "tools/list": ("ToolsListParams", "ToolsListResponse"),
    "turn/interrupt": ("TurnInterruptParams", "TurnInterruptResponse"),
    "turn/start": ("TurnStartParams", "TurnStartResponse"),
    "turn/steer": ("TurnSteerParams", "TurnSteerResponse"),
    "vibeCode/projects/cancel": ("VibeCodeProjectCancelParams", "EmptyResponse"),
    "vibeCode/projects/create": (
        "VibeCodeProjectCreateParams",
        "VibeCodeProjectCreateResponse",
    ),
    "vibeCode/projects/loadMore": (
        "VibeCodeProjectsLoadMoreParams",
        "VibeCodeProjectsLoadMoreResponse",
    ),
    "vibeCode/projects/open": (
        "VibeCodeProjectsOpenParams",
        "VibeCodeProjectsOpenResponse",
    ),
    "vibeCode/projects/recover": (
        "VibeCodeProjectRecoverParams",
        "VibeCodeProjectRecoverResponse",
    ),
    "vibeCode/projects/select": (
        "VibeCodeProjectSelectParams",
        "VibeCodeProjectSelectResponse",
    ),
    "vibeCode/projects/unlink": (
        "VibeCodeProjectUnlinkParams",
        "VibeCodeProjectUnlinkResponse",
    ),
    "vibeCode/teleport/cancel": ("TeleportCancelParams", "TeleportCancelResponse"),
    "vibeCode/teleport/push/respond": ("TeleportPushRespondParams", "EmptyResponse"),
    "vibeCode/teleport/start": ("TeleportStartParams", "TeleportStartResponse"),
    "workspace/prompt/prepare": (
        "WorkspacePromptPrepareParams",
        "WorkspacePromptPrepareResponse",
    ),
    "workspace/trust/decision": (
        "WorkspaceTrustDecisionParams",
        "WorkspaceTrustStatusResponse",
    ),
    "workspace/trust/status": (
        "WorkspaceTrustStatusParams",
        "WorkspaceTrustStatusResponse",
    ),
}

#: Server-to-client requests. ``callback/call`` is the only one the port already
#: issues; the ``clientTool/*`` family is recorded so a field rename fails the
#: replay before the delegation is implemented.
SERVER_REQUEST_MODELS: dict[str, tuple[str, str]] = {
    "callback/call": ("CallbackCallParams", "CallbackCallResponse"),
}
CLIENT_TOOL_MODELS: dict[str, tuple[str, str]] = {
    "clientTool/readTextFile": (
        "ClientToolReadTextFileParams",
        "ClientToolReadTextFileResponse",
    ),
    "clientTool/writeTextFile": ("ClientToolWriteTextFileParams", "EmptyResponse"),
    "clientTool/terminal/create": (
        "ClientToolTerminalCreateParams",
        "ClientToolTerminalCreateResponse",
    ),
    "clientTool/terminal/wait": (
        "ClientToolTerminalParams",
        "ClientToolTerminalWaitResponse",
    ),
    "clientTool/terminal/output": (
        "ClientToolTerminalParams",
        "ClientToolTerminalOutputResponse",
    ),
    "clientTool/terminal/kill": ("ClientToolTerminalParams", "EmptyResponse"),
    "clientTool/terminal/release": ("ClientToolTerminalParams", "EmptyResponse"),
}

#: Server-to-client notifications, with the params model each carries. The
#: sequenced ones derive from ``EventNotificationParams`` and are proved to be
#: dispatched by the reference client projection; the rest are proved by their
#: params model resolving.
NOTIFICATION_MODELS: dict[str, str] = {
    "session/snapshot": "SessionSnapshotParams",
    "session/compacted": "SessionCompactedParams",
    "session/contextCleared": "SessionContextClearedParams",
    "session/updated": "SessionUpdatedParams",
    "session/statsUpdated": "StatsUpdatedParams",
    "history/entryAdded": "HistoryEntryAddedParams",
    "history/entryUpdated": "HistoryEntryUpdatedParams",
    "turn/started": "TurnStartedParams",
    "turn/completed": "TurnCompletedParams",
    "turn/retrying": "TurnRetryingParams",
    "warning": "ServerWarningParams",
    "error": "ServerErrorParams",
    "runtime/updated": "RuntimeUpdatedParams",
    "mcp/authUrl": "MCPAuthUrlParams",
    "vibeCode/teleport/event": "TeleportEventParams",
}

#: Discriminated unions the projection publishes. Each is resolved from the
#: pinned module: the discriminator and every variant's value are read off the
#: annotation, never authored here.
UNION_ROOTS: tuple[tuple[str, str], ...] = (
    ("vibe.app_server._effect_models", "EffectDetail"),
    ("vibe.app_server.models", "NoticeDetail"),
    ("vibe.app_server.models", "CallbackDetail"),
    ("vibe.app_server.models", "CallbackOutput"),
    ("vibe.app_server.models", "CallbackState"),
    ("vibe.app_server.models", "PublicHistoryEntry"),
    ("vibe.app_server.models", "ContentBlock"),
    ("vibe.app_server.models", "EffectState"),
    ("vibe.app_server.models", "TeleportEvent"),
    ("vibe.app_server.protocol", "SessionMCPServer"),
)

#: Models no routed method reaches but that still cross the wire: the JSON-RPC
#: envelopes, the structured `invalid_params` detail, the notification bases and
#: the per-tool effect output shapes carried inside the untyped `output` value.
#: `CompactionDetails` is declared by the pinned reference and referenced by
#: nothing in it; it is rooted here so the completeness check below stays a
#: statement about the whole declared surface rather than the reachable part.
EXTRA_MODEL_ROOTS: tuple[str, ...] = (
    "CompactionDetails",
    "EventNotificationParams",
    "FileEditEffectOutput",
    "FileReadEffectOutput",
    "FileSearchEffectOutput",
    "FileWriteEffectOutput",
    "InvalidParamsData",
    "InvalidParamsIssue",
    "JsonRpcErrorResponse",
    "JsonRpcSuccessResponse",
    "Notification",
    "ProtocolError",
    "ServerRequest",
    "SessionHandoffParams",
    "SessionOpenParams",
    "SessionOptions",
    "ShellEffectOutput",
    "SkillEffectOutput",
    "SubagentEffectOutput",
    "TodoEffectOutput",
    "WebFetchEffectOutput",
    "WebSearchEffectOutput",
)

#: Enum vocabularies the replay pins by name. One that the pinned reference does
#: not declare is recorded as absent rather than invented, which is how the
#: corpus reports a vocabulary that arrived after the pin.
NAMED_ENUMS: tuple[str, ...] = (
    "MCPSourceStatus",
    "PublicRetryCategory",
    "TurnErrorCode",
    "ToolEffectKind",
    "AccountStatus",
    "PublicTurnStopReason",
    "PublicEntryGenerationStatus",
)

#: Modules searched, in order, when resolving a model or enum by name.
SOURCE_MODULES: tuple[str, ...] = (
    "vibe.app_server.protocol",
    "vibe.app_server._connection_protocol",
    "vibe.app_server.models",
    "vibe.app_server._effect_models",
    "vibe.app_server.config",
    "vibe.app_server.review",
    # Two contracts the app-server publishes are declared outside it: the
    # effect displays every entry detail carries, and the question models the
    # user-input callback and the `ask_user_question` effect take.
    "vibe.utils.tool_presentation",
    "vibe.questions",
)

#: The base classes whose subclasses cross the wire. They differ only in which
#: package declares them; each one forbids surplus fields and serializes under a
#: camelCase alias, which is the contract the census records.
WIRE_MODEL_BASES: tuple[tuple[str, str], ...] = (
    ("vibe.app_server._model", "ProtocolModel"),
    ("vibe.utils.tool_presentation", "PresentationModel"),
    ("vibe.questions", "QuestionModel"),
)


# --------------------------------------------------------------------------
# Census
# --------------------------------------------------------------------------


class Census:
    """Walks the reference wire models and records their shape."""

    def __init__(self) -> None:
        from vibe.app_server._model import ProtocolModel

        self.protocol_model = ProtocolModel
        #: Every base a wire model may inherit from. `describe` and `record`
        #: walk all of them; the completeness check below stays scoped to
        #: `ProtocolModel`, which is the only base the app-server package owns.
        self.wire_models = tuple(
            getattr(__import__(module, fromlist=["_"]), name)
            for module, name in WIRE_MODEL_BASES
        )
        self.modules = [__import__(name, fromlist=["_"]) for name in SOURCE_MODULES]
        self.models: dict[str, Any] = {}
        self.enums: dict[str, Any] = {}

    # -- resolution -------------------------------------------------------

    def lookup(self, name: str) -> Any:
        for module in self.modules:
            found = getattr(module, name, None)
            if found is not None:
                return found
        return None

    def model(self, name: str) -> Any:
        found = self.lookup(name)
        if not isinstance(found, type) or not issubclass(found, self.wire_models):
            raise OracleError(f"reference declares no wire model named {name}")
        return found

    # -- annotation classification ---------------------------------------

    def describe(self, annotation: Any) -> dict[str, Any]:
        """Classifies a field annotation into wire-shaped observations."""
        models: set[str] = set()
        enums: set[str] = set()
        literals: list[Any] = []
        optional = False
        scalars = {bool: "bool", int: "int", float: "float", str: "str"}

        def visit(node: Any) -> str | None:
            """Returns the wire kind of `node`, recording what it references."""
            nonlocal optional
            node = self._unwrap(node)
            origin = get_origin(node)
            if node is type(None):
                optional = True
                return None
            if origin is Literal:
                for value in get_args(node):
                    if value not in literals:
                        literals.append(value)
                return "literal"
            if origin in (Union, types.UnionType):
                member_kinds = [visit(member) for member in get_args(node)]
                distinct = {kind for kind in member_kinds if kind is not None}
                if len(distinct) == 1:
                    return distinct.pop()
                return "union" if distinct else None
            if origin in (list, tuple, set, frozenset):
                for argument in get_args(node):
                    visit(argument)
                return "list"
            if origin is dict:
                arguments = get_args(node)
                if len(arguments) == 2:
                    visit(arguments[1])
                return "dict"
            if isinstance(node, type):
                if issubclass(node, self.wire_models):
                    models.add(node.__name__)
                    self.record(node)
                    return "model"
                if issubclass(node, enum.Enum):
                    enums.add(node.__name__)
                    self.enums.setdefault(node.__name__, node)
                    return "enum"
                for scalar, name in scalars.items():
                    if node is scalar:
                        return name
                return "scalar"
            return "any"

        kind = visit(annotation) or "any"
        return {
            "kind": kind,
            # An untyped JSON value includes null, which the walk above never
            # reaches because it stops at the alias rather than expanding its
            # recursive union.
            "optional": optional or kind == "any",
            "models": sorted(models),
            "enums": sorted(enums),
            "literals": [_wire_value(value) for value in literals],
        }

    @staticmethod
    def _unwrap(annotation: Any) -> Any:
        while get_origin(annotation) is Annotated:
            annotation = get_args(annotation)[0]
        if isinstance(annotation, typing.TypeAliasType):
            return Census._unwrap(annotation.__value__)
        return annotation

    # -- recording --------------------------------------------------------

    def record(self, model: Any) -> None:
        name = model.__name__
        if name in self.models:
            return
        self.models[name] = None  # guards the cycle before the fields recurse
        fields = []
        for field_name, info in model.model_fields.items():
            described = self.describe(info.annotation)
            fields.append({
                "name": field_name,
                "alias": info.alias or field_name,
                "required": bool(info.is_required()),
                **described,
            })
        self.models[name] = {
            "name": name,
            "fields": sorted(fields, key=lambda field: field["alias"]),
        }

    def union(self, module_name: str, name: str) -> dict[str, Any]:
        module = __import__(module_name, fromlist=["_"])
        alias = getattr(module, name, None)
        if alias is None:
            raise OracleError(f"reference declares no union named {name}")
        annotation = alias.__value__ if hasattr(alias, "__value__") else alias
        discriminator = None
        while get_origin(annotation) is Annotated:
            args = get_args(annotation)
            for meta in args[1:]:
                found = getattr(meta, "discriminator", None)
                if isinstance(found, str):
                    discriminator = found
            annotation = args[0]
        members = [arg for arg in get_args(annotation) if arg is not type(None)]
        if not members:
            raise OracleError(f"{name} resolves to no union members")
        variants = []
        for member in members:
            self.record(member)
            value = None
            if discriminator is not None:
                info = member.model_fields.get(discriminator)
                if info is not None:
                    value = _discriminator_value(info)
            variants.append({"model": member.__name__, "value": value})
        return {
            "name": name,
            "discriminator": discriminator,
            "variants": variants,
        }


def _discriminator_value(info: Any) -> Any:
    annotation = Census._unwrap(info.annotation)
    if get_origin(annotation) is Literal:
        args = get_args(annotation)
        if len(args) == 1:
            return _wire_value(args[0])
    default = info.default
    if default is not None and not isinstance(default, type(Ellipsis)):
        return _wire_value(default)
    return None


def _wire_value(value: Any) -> Any:
    if isinstance(value, enum.Enum):
        return value.value
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    return str(value)


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture(commit: str) -> dict[str, Any]:
    import vibe.app_server.protocol as protocol
    from vibe.app_server._connection_protocol import ClientToolMethod
    from vibe.app_server.protocol import EventNotificationParams, ProtocolErrorCode

    census = Census()

    declared = tuple(protocol.SERVER_METHODS)
    unmapped = sorted(set(declared) - set(METHOD_MODELS))
    surplus = sorted(set(METHOD_MODELS) - set(declared))
    if unmapped or surplus:
        raise OracleError(
            "the method model table no longer matches the reference inventory: "
            f"unmapped {unmapped}, surplus {surplus}"
        )

    methods = []
    for name in declared:
        params_name, response_name = METHOD_MODELS[name]
        census.record(census.model(params_name))
        census.record(census.model(response_name))
        methods.append({
            "name": name,
            "params": params_name,
            "response": response_name,
        })

    client_tool_names = [member.value for member in ClientToolMethod]
    if sorted(client_tool_names) != sorted(CLIENT_TOOL_MODELS):
        raise OracleError(
            "ClientToolMethod no longer matches the captured client-tool table: "
            f"{sorted(client_tool_names)}"
        )
    client_tools = []
    for name in client_tool_names:
        params_name, response_name = CLIENT_TOOL_MODELS[name]
        census.record(census.model(params_name))
        census.record(census.model(response_name))
        client_tools.append({
            "name": name,
            "params": params_name,
            "response": response_name,
        })

    server_requests = []
    for name, (params_name, response_name) in SERVER_REQUEST_MODELS.items():
        census.record(census.model(params_name))
        census.record(census.model(response_name))
        server_requests.append({
            "name": name,
            "params": params_name,
            "response": response_name,
        })

    notifications = []
    for name, params_name in NOTIFICATION_MODELS.items():
        model = census.model(params_name)
        census.record(model)
        sequenced = issubclass(model, EventNotificationParams)
        if sequenced and not _reference_dispatches_event(name):
            raise OracleError(
                f"the reference client projection does not dispatch {name}"
            )
        notifications.append({
            "name": name,
            "params": params_name,
            "sequenced": sequenced,
        })

    # The handshake models are not reachable from any routed method.
    for name in ("InitializeParams", "InitializeResponse", "ClientCapabilities"):
        census.record(census.model(name))
    for name in EXTRA_MODEL_ROOTS:
        census.record(census.model(name))

    unions = [census.union(module, name) for module, name in UNION_ROOTS]
    _assert_census_is_complete(census)

    enums = []
    absent_enums = []
    for name in NAMED_ENUMS:
        found = census.lookup(name)
        if found is None:
            absent_enums.append(name)
            continue
        if not isinstance(found, type) or not issubclass(found, enum.Enum):
            raise OracleError(f"{name} is not an enum in the pinned reference")
        census.enums.setdefault(name, found)
    for name, declared_enum in sorted(census.enums.items()):
        enums.append({
            "name": name,
            "values": [_wire_value(member) for member in declared_enum],
        })

    corpus = {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": commit},
        "note": NOTE,
        "methods": methods,
        "clientToolMethods": client_tools,
        "serverRequests": server_requests,
        "notifications": notifications,
        "errorCodes": [_wire_value(code) for code in ProtocolErrorCode],
        "enums": enums,
        "absentEnums": absent_enums,
        "unions": unions,
        "models": [census.models[name] for name in sorted(census.models)],
    }
    _assert_no_prose(corpus)
    return corpus


def _assert_census_is_complete(census: Census) -> None:
    """Fails when the reference declares a wire model the walk never reached.

    The walk starts from the routed methods, the notifications and the union
    roots, so a model that no root reaches is a surface this capture would leave
    unmeasured. Private bases stay out: they carry no `kind` of their own and
    every field they declare is recorded on the variants that inherit them.
    """
    import importlib
    import pkgutil

    import vibe.app_server as package

    declared: dict[str, Any] = {}
    for module_info in pkgutil.iter_modules(package.__path__):
        try:
            module = importlib.import_module(f"vibe.app_server.{module_info.name}")
        except ImportError:
            continue
        for name in dir(module):
            candidate = getattr(module, name)
            if (
                isinstance(candidate, type)
                and issubclass(candidate, census.protocol_model)
                and candidate is not census.protocol_model
                and not candidate.__name__.startswith("_")
            ):
                declared[candidate.__name__] = candidate
    unreached = sorted(set(declared) - set(census.models))
    if unreached:
        raise OracleError(
            "the census walk never reached: " + ", ".join(unreached)
        )


def _reference_dispatches_event(method: str) -> bool:
    """Whether the reference client projection knows this notification name.

    Probed rather than read: the parser is handed a frame with empty params, so
    a known name fails validation and an unknown one raises its own error.
    """
    from vibe.app_server.events import UnknownNotificationError, _parse_event_params
    from vibe.app_server.protocol import Notification

    try:
        _parse_event_params(Notification(method=method, params={}))
    except UnknownNotificationError:
        return False
    except Exception:
        return True
    return True


def _assert_no_prose(node: Any, pointer: str = "") -> None:
    """Fails on any captured string long enough to be reference-authored prose."""
    if isinstance(node, dict):
        for key, value in node.items():
            _assert_no_prose(value, f"{pointer}/{key}")
        return
    if isinstance(node, list):
        for index, value in enumerate(node):
            _assert_no_prose(value, f"{pointer}/{index}")
        return
    if isinstance(node, str) and len(node) > MAX_CAPTURED_STRING and pointer != "/note":
        raise OracleError(
            f"{pointer} carries a {len(node)}-character string, which NOTICE forbids"
        )


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--interpreter", type=Path, default=None)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    arguments = parser.parse_args()

    reference = arguments.reference.expanduser()
    try:
        pin = resolve_reference(reference, EXPECTED_COMMIT)
        reexecute_with_reference_interpreter(reference, arguments.interpreter)
        corpus = capture(pin["commit"])
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    output = arguments.output
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(corpus, indent=2, sort_keys=False) + "\n")
    print(
        f"wrote {output}: {len(corpus['methods'])} methods, "
        f"{len(corpus['clientToolMethods'])} client tools, "
        f"{len(corpus['notifications'])} notifications, "
        f"{len(corpus['models'])} models, {len(corpus['enums'])} enums, "
        f"{len(corpus['unions'])} unions"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
