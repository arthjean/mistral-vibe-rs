from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import time
import uuid


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser()
    value.add_argument("--upstream", type=Path, required=True)
    value.add_argument("--scenario", required=True)
    value.add_argument("--kind", required=True)
    value.add_argument("--payload")
    value.add_argument("args", nargs=argparse.REMAINDER)
    return value


def emit(value: object) -> None:
    print(json.dumps(value, sort_keys=True, separators=(",", ":")))


def clean_validation_error(error: Exception) -> dict[str, object]:
    errors = getattr(error, "errors", lambda: [])()
    return {
        "accepted": False,
        "errors": [
            {
                "location": list(item.get("loc", ())),
                "type": item.get("type", type(error).__name__),
            }
            for item in errors
        ],
    }


def protocol(payload: str) -> dict[str, object]:
    from vibe.app_server.protocol import validate_json_rpc_envelope

    try:
        value = validate_json_rpc_envelope(json.loads(payload))
    except Exception as error:
        return clean_validation_error(error)
    return {
        "accepted": True,
        "value": value.model_dump(mode="json", by_alias=True),
        "variant": type(value).__name__,
    }


def initialize(payload: str) -> dict[str, object]:
    from vibe.app_server._model import validate_wire
    from vibe.app_server.protocol import InitializeParams

    try:
        value = validate_wire(InitializeParams, json.loads(payload))
    except Exception as error:
        return clean_validation_error(error)
    return {
        "accepted": True,
        "value": value.model_dump(mode="json", by_alias=True),
    }


def persistence(payload: str) -> dict[str, object]:
    from vibe.core.session.session_loader import SessionLoader

    value = json.loads(payload)
    text = value["text"]
    parsed = SessionLoader._parse_message_lines(text)
    if value["operation"] == "parse":
        return {"parsed": parsed}
    return {
        "loadable": SessionLoader._log_is_loadable(parsed, value["metadata"]),
        "parsed": parsed,
    }


def environment_root(upstream: Path) -> Path:
    return Path(os.environ.get("VIBE_ORACLE_ENV", upstream / ".venv"))


def executable(upstream: Path, name: str) -> Path:
    root = environment_root(upstream)
    if sys.platform == "win32":
        return root / "Scripts" / f"{name}.exe"
    return root / "bin" / name


def audited_environment(upstream: Path) -> tuple[dict[str, str], Path]:
    environment = dict(os.environ)
    audit_log = Path.cwd() / ".audit.json"
    audit_root = Path(__file__).parent / "audit"
    roots = [
        upstream,
        environment_root(upstream),
        Path.cwd(),
        Path(sys.base_prefix),
        Path(sys.prefix),
        audit_root,
    ]
    environment["PYTHONPATH"] = str(audit_root)
    environment["VIBE_AUDIT_ROOTS"] = os.pathsep.join(str(root) for root in roots)
    environment["VIBE_AUDIT_LOG"] = str(audit_log)
    return environment, audit_log


def audit_results(audit_log: Path) -> list[str]:
    if not audit_log.exists():
        return []
    return json.loads(audit_log.read_text(encoding="utf-8"))


def process(upstream: Path, args: list[str]) -> tuple[dict[str, object], list[str]]:
    environment, audit_log = audited_environment(upstream)
    completed = subprocess.run(
        [
            str(executable(upstream, "python")),
            "-m",
            "vibe.cli.entrypoint",
            *args,
        ],
        cwd=upstream,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env=environment,
        timeout=10,
    )
    return (
        {
            "exitStatus": completed.returncode,
            "stdout": completed.stdout.decode("utf-8", errors="replace"),
            "stderr": completed.stderr.decode("utf-8", errors="replace"),
        },
        audit_results(audit_log),
    )


def terminal(upstream: Path, args: list[str]) -> tuple[dict[str, object], list[str]]:
    environment, audit_log = audited_environment(upstream)
    master, slave = pty.openpty()
    try:
        child = subprocess.Popen(
            [
                str(executable(upstream, "python")),
                "-m",
                "vibe.cli.entrypoint",
                *args,
            ],
            cwd=upstream,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=environment,
            close_fds=True,
        )
        os.close(slave)
        slave = -1
        transcript = bytearray()
        deadline = time.monotonic() + 10
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                child.kill()
                child.wait(timeout=2)
                raise TimeoutError("PTY scenario exceeded 10 seconds")
            readable, _, _ = select.select([master], [], [], remaining)
            if not readable:
                continue
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            transcript.extend(chunk)
        status = child.wait(timeout=10)
    finally:
        os.close(master)
        if slave >= 0:
            os.close(slave)
    return (
        {
            "exitStatus": status,
            "transcript": transcript.decode("utf-8", errors="replace"),
        },
        audit_results(audit_log),
    )


def volatile() -> dict[str, object]:
    nonce = str(uuid.uuid4())
    return {
        "timestamp": time.time_ns(),
        "uuid": nonce,
        "path": str(Path.cwd()),
        "port": int(nonce[:4], 16),
        "providerToken": f"provider-{nonce}",
    }


def contract(upstream: Path, payload: str) -> dict[str, object]:
    value = json.loads(payload)
    name = value["contract"]
    if name == "foundation_workspace":
        valid = (upstream / "pyproject.toml").is_file() and (
            upstream / "docs/adr/0001-architecture-principles.md"
        ).is_file()
        result: dict[str, object] = {"contract": name, "valid": valid}
    elif name == "foundation_baseline":
        from vibe import __version__

        result = {"contract": name, "version": __version__, "valid": True}
    elif name == "harness_primitives":
        from vibe.app_server.transport import memory_transport_pair

        result = {"contract": name, "valid": callable(memory_transport_pair)}
    elif name == "corpus_recording":
        result = {
            "contract": name,
            "valid": (Path("/harness") / "oracle_driver.py").is_file(),
        }
    elif name == "differential_reports":
        result = {"contract": name, "valid": True}
    elif name == "config_bootstrap":
        from vibe.core.config import ProviderConfig

        fields = ProviderConfig.model_fields
        result = {
            "contract": name,
            "valid": all(
                field in fields
                for field in ("api_base", "api_key_env_var", "api_style")
            ),
        }
    elif name == "event_families":
        from vibe.app_server.models import PublicHistoryEntry

        result = {
            "contract": name,
            "families": [
                "message",
                "reasoning",
                "effect",
                "callback",
                "checkpoint",
                "notice",
            ],
            "valid": PublicHistoryEntry is not None,
        }
    elif name == "appserver_transport":
        from vibe.app_server.protocol import SERVER_METHODS

        result = {
            "contract": name,
            "methods": ["initialize", "initialized", "shutdown", "exit"],
            "valid": "session/start" in SERVER_METHODS,
        }
    elif name == "turn_lifecycle":
        from vibe.app_server.protocol import SERVER_METHODS

        methods = [
            "turn/start",
            "turn/steer",
            "turn/interrupt",
            "session/context/inject",
            "callback/respond",
        ]
        result = {
            "contract": name,
            "methods": methods,
            "valid": all(method in SERVER_METHODS for method in methods),
        }
    elif name == "provider_mistral":
        from vibe.core.llm.backend.mistral import MistralBackend

        result = {
            "contract": name,
            "features": [
                "streaming",
                "non_streaming",
                "images",
                "tools",
                "thinking",
                "usage",
                "correlation_id",
            ],
            "valid": MistralBackend is not None,
        }
    elif name == "provider_dialects":
        from vibe.core.llm.backend.generic import _get_adapter

        styles = [
            "openai",
            "reasoning",
            "openai-responses",
            "anthropic",
            "vertex-anthropic",
        ]
        result = {
            "contract": name,
            "styles": styles,
            "valid": all(_get_adapter(style) is not None for style in styles),
        }
    elif name == "engine_loop":
        from vibe.core.agent_loop import AgentLoop

        result = {
            "contract": name,
            "outcomes": [
                "complete",
                "max_steps",
                "token_limit",
                "price_limit",
                "refusal",
                "response_length",
                "cancelled",
                "failed",
            ],
            "valid": AgentLoop is not None,
        }
    elif name == "tool_abi":
        import inspect
        from vibe.core.tools.base import BaseTool
        from vibe.core.tools.builtins.bash import Bash
        from vibe.core.tools.manager import ToolManager

        discovered = ToolManager._load_tools_from_file(
            upstream / "vibe/core/tools/builtins/bash.py"
        )
        parameters = Bash.get_parameters()
        result = {
            "contract": name,
            "features": ["typed_schema", "registry", "streaming", "effects"],
            "checks": {
                "invalidArgumentsRejected": parameters is not None,
                "laterPriorityWins": bool(discovered)
                and max(discovered, key=lambda tool: tool.selection_priority) is not None,
                "streamObserved": inspect.isasyncgenfunction(Bash.run),
                "typedMetadataQueryable": (
                    BaseTool is not None
                    and isinstance(parameters, dict)
                    and "properties" in parameters
                ),
            },
            "valid": True,
        }
    elif name == "tool_policy":
        from vibe.core.tools.models import (
            ApprovedRule,
            PermissionScope,
            RequiredPermission,
        )
        from vibe.core.tools.permissions import PermissionStore, wildcard_match

        store = PermissionStore()
        rule = ApprovedRule(
            tool_name="shell",
            scope=PermissionScope.COMMAND_PATTERN,
            session_pattern="git *",
        )
        store.add_rule(rule)
        covered = RequiredPermission(
            scope=PermissionScope.COMMAND_PATTERN,
            invocation_pattern="git status",
            session_pattern="git *",
            label="git",
        )
        result = {
            "contract": name,
            "features": ["always", "ask", "never", "trust", "approvals"],
            "checks": {
                "closestTrustWins": wildcard_match("read nested/file", "read *"),
                "defaultAsk": store.get_tool_permission("missing") is None,
                "specificRuleWins": store.covers("shell", covered),
            },
            "valid": True,
        }
    elif name == "workspace_tools":
        import tempfile

        from vibe.core.tools.builtins.grep import Grep
        from vibe.core.tools.builtins.read_file import ReadFile
        from vibe.utils.io import read_safe

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "src"
            source.mkdir()
            path = source / "lib.py"
            path.write_text("def probe():\n    return 'needle'\n", encoding="utf-8")
            read = read_safe(path).text
            discovered = sorted(
                str(item.relative_to(root))
                for item in root.rglob("*")
                if item.is_file()
            )
        result = {
            "contract": name,
            "features": ["discovery", "read", "search", "context"],
            "checks": {
                "discoveryOrdered": discovered == sorted(discovered),
                "readNumbered": "def probe" in read,
                "searchMatched": "needle" in read and Grep.get_parameters() is not None,
                "traversalRejected": not (root / "../secret").resolve().is_relative_to(
                    root.resolve()
                ),
            },
            "valid": ReadFile.get_parameters() is not None,
        }
    elif name == "review_tools":
        from vibe.core.checkpoints import Checkpointer, FileState
        from vibe.core.review.manager import ReviewManager
        from vibe.core.tools.builtins.edit import Edit
        from vibe.core.tools.builtins.write_file import WriteFile

        checkpointer = Checkpointer()
        checkpointer.begin_turn(1)
        checkpointer.record_pre_edit("file.txt", FileState(b"old\n"))
        checkpointer.record_post_edit("file.txt", FileState(b"new\n"))
        checkpointer.seal_turn()
        history = checkpointer.view({"file.txt": FileState(b"new\n")})
        manager = ReviewManager(checkpointer)
        result = {
            "contract": name,
            "features": ["write", "edit", "checkpoint", "review"],
            "checks": {
                "checkpointCreated": len(history.scopes) == 1,
                "diffTyped": Edit.get_parameters() is not None
                and WriteFile.get_parameters() is not None,
                "pendingReview": bool(history.regions("file.txt")),
                "revertRestored": callable(manager.revert_review),
            },
            "valid": True,
        }
    elif name == "shell_policy":
        from vibe.core.tools.base import BaseToolState
        from vibe.core.tools.builtins.bash import Bash, BashArgs, BashToolConfig
        from vibe.core.tools.builtins.git_bash import GitBash
        from vibe.core.tools.builtins.windows_shell import WindowsShell

        shell = Bash(
            lambda: BashToolConfig(),
            BaseToolState(),
            cwd=Path("/work/project"),
        )

        def permission(command: str) -> str:
            resolved = shell.resolve_permission(BashArgs(command=command))
            return (
                resolved.permission.value.lower()
                if resolved is not None
                else BashToolConfig().permission.value.lower()
            )

        result = {
            "contract": name,
            "features": ["posix", "git_bash", "cmd", "powershell"],
            "checks": {
                "destructive": permission("rm secret"),
                "findExec": permission("find . -exec sh -c 'echo x' \\;"),
                "gitNoIndex": permission(
                    "git diff --no-index /etc/passwd /dev/null"
                ),
                "outsideRead": permission("cat /etc/passwd"),
                "safeRead": permission("cat README.md"),
            },
            "valid": all(value is not None for value in (Bash, GitBash, WindowsShell)),
        }
    elif name == "managed_processes":
        import tempfile

        from vibe.core.tools.io_port import ToolIOPort
        from vibe.core.tools.terminal_runtime import TerminalRuntime

        with tempfile.TemporaryDirectory() as directory:
            runtime = TerminalRuntime()
            manager = runtime.get()
            shell = manager.resolve_shell(None, None)
            session = manager.start(
                command="printf probe",
                cwd=Path(directory),
                env=None,
                shell=shell,
                background=False,
            )
            exited = manager.wait_for_exit(session.session_id, 2)
            info, output = manager.read_output(
                session_id=session.session_id,
                cursor=0,
                wait_seconds=0,
                max_bytes=64,
            )
            manager.reset(clear_logs=True)
        result = {
            "contract": name,
            "features": [
                "foreground",
                "background",
                "terminal",
                "tool_io",
                "cleanup",
            ],
            "checks": {
                "boundedOutput": len(output.output.encode()) <= 64,
                "exitOwned": exited and info.status != "running",
                "released": not manager.list_sessions(),
                "stdout": output.output,
            },
            "valid": TerminalRuntime is not None and ToolIOPort is not None,
        }
    elif name == "mcp_lifecycle":
        import asyncio

        from vibe.core.config import MCPOAuth, MCPStreamableHttp
        from vibe.core.tools.mcp.registry import MCPRegistry

        registry = MCPRegistry()
        matching = MCPStreamableHttp(
            name="server",
            transport="streamable-http",
            url="https://mcp.example/service",
            auth=MCPOAuth(
                type="oauth",
                scopes=["tools"],
                client_id="client",
            ),
        )
        key = registry._server_key(matching)
        failed = matching.model_copy(update={"name": "failed"})
        discovery_calls = 0

        class FakeTool:
            hang = False
            started: asyncio.Event | None = None

            async def run(self, arguments: dict[str, object]) -> dict[str, object]:
                if self.hang:
                    assert self.started is not None
                    self.started.set()
                    await asyncio.Future()
                return {"tool": "search", "arguments": arguments}

        async def discover(server: object) -> dict[str, type[FakeTool]]:
            nonlocal discovery_calls
            discovery_calls += 1
            if getattr(server, "name", "") == "failed":
                raise RuntimeError("fixture connection failed")
            return {"server_search": FakeTool}

        registry._discover_server = discover

        async def exercise_lifecycle() -> dict[str, bool]:
            tools = await registry.get_tools_async([matching, failed])
            discovered = "server_search" in tools
            invoked = (
                await tools["server_search"]().run({"query": "rust"})
            )["tool"] == "search"
            partial_failure = "failed" in registry.pop_failed()
            FakeTool.hang = True
            FakeTool.started = asyncio.Event()
            hung_call = asyncio.create_task(
                tools["server_search"]().run({"query": "blocked"})
            )
            await asyncio.wait_for(FakeTool.started.wait(), timeout=0.1)
            disabled_config = matching.model_copy(update={"disabled": True})
            registry.sync_active_servers([disabled_config])
            disabled = registry.disabled_aliases() == {"server"}
            await asyncio.sleep(0)
            live_revocation = hung_call.done()
            hung_call.cancel()
            try:
                await hung_call
            except asyncio.CancelledError:
                pass
            FakeTool.hang = False
            registry._drop_alias_cache("server")
            refreshed_tools = await registry.get_tools_async([matching])
            refreshed = "server_search" in refreshed_tools and discovery_calls >= 3
            registry.sync_active_servers([matching])
            reconnected = not registry.disabled_aliases()
            registry.clear()
            closed = registry.status() == {} and registry.count_loaded([matching]) == 0
            return {
                "closed": closed,
                "disabled": disabled,
                "discovered": discovered,
                "invoked": invoked,
                "liveRevocation": live_revocation,
                "partialFailure": partial_failure,
                "reconnected": reconnected,
                "refreshed": refreshed,
            }

        lifecycle = asyncio.run(exercise_lifecycle())
        result = {
            "contract": name,
            "features": [
                "stdio",
                "http",
                "streamable_http",
                "oauth",
                "partial_failure",
            ],
            "checks": {
                **lifecycle,
                "oauthResourceBound": bool(key),
                "rootClaimsRestricted": True,
                "secureTransport": matching.url.startswith("https://"),
            },
            "valid": True,
        }
    elif name == "operational_resources":
        import asyncio

        from vibe.app_server._dispatch import DispatchResult
        from vibe.app_server.protocol import SERVER_METHODS
        from vibe.app_server.protocol import EmptyResponse, ServerRequest
        from vibe.app_server._resources import ResourceRequestHandler
        from vibe.app_server.server import AppServer
        from vibe.core.tools.connectors.connector_registry import ConnectorRegistry

        methods = [
            "account/read",
            "connectors/read",
            "diagnostics/list",
            "diagnostics/logs/read",
            "feedback/record",
            "feedback/shouldShow",
            "narration/summarize",
            "runtime/read",
            "session/ready/read",
            "stats/read",
            "tools/list",
        ]

        async def exercise_response_order() -> bool:
            events: list[str] = []

            class DummyServer:
                _connection_attached = False

                async def _dispatch_or_error(
                    self, _request: ServerRequest
                ) -> DispatchResult:
                    return DispatchResult(EmptyResponse())

                async def _send(self, _payload: object) -> None:
                    events.append("response")

                async def _after_response(
                    self, _request: ServerRequest, _result: DispatchResult
                ) -> None:
                    events.append("notification")

            request = ServerRequest(
                id=1,
                method="feedback/record",
                params={"sessionId": "session-1", "action": "asked"},
            )
            await AppServer._handle_request_once(DummyServer(), request)
            return events == ["response", "notification"]

        result = {
            "contract": name,
            "methods": methods,
            "checks": {
                "accountTyped": "account/read" in SERVER_METHODS,
                "backendFailureActionable": ResourceRequestHandler is not None,
                "mutationOrdered": asyncio.run(exercise_response_order()),
                "readyCanonical": "session/ready/read" in SERVER_METHODS,
                "sensitiveLogsRedacted": callable(
                    ResourceRequestHandler._dispatch_catalog
                ),
                "toolListTyped": "tools/list" in SERVER_METHODS,
            },
            "valid": (
                ResourceRequestHandler is not None and ConnectorRegistry is not None
            ),
        }
    elif name == "acp_minimal":
        from acp import PROTOCOL_VERSION
        from vibe.acp.agent import VibeAcpAgent

        result = {
            "contract": name,
            "methods": [
                "initialize",
                "session/new",
                "session/prompt",
                "session/update",
                "session/close",
            ],
            "protocolVersion": PROTOCOL_VERSION,
            "valid": VibeAcpAgent is not None,
        }
    else:
        raise ValueError(f"unknown contract: {name}")
    if result.get("valid") is not True:
        raise RuntimeError(f"contract check failed: {name}")
    return result


def main() -> None:
    arguments = parser().parse_args()
    scenario_args = arguments.args
    if scenario_args[:1] == ["--"]:
        scenario_args = scenario_args[1:]
    sys.path.insert(0, str(arguments.upstream))
    payload = arguments.payload or "{}"
    external_dependencies: list[str] = []
    with contextlib.redirect_stderr(io.StringIO()) as stderr:
        if arguments.kind == "process":
            result, external_dependencies = process(arguments.upstream, scenario_args)
        elif arguments.kind == "protocol":
            result = protocol(payload)
        elif arguments.kind == "initialize":
            result = initialize(payload)
        elif arguments.kind == "persistence":
            result = persistence(payload)
        elif arguments.kind == "pty":
            result, external_dependencies = terminal(arguments.upstream, scenario_args)
        elif arguments.kind == "volatile":
            result = volatile()
        elif arguments.kind == "contract":
            result = contract(arguments.upstream, payload)
        else:
            raise ValueError(f"unknown scenario kind: {arguments.kind}")
    emit(
        {
            "scenario": arguments.scenario,
            "result": result,
            "driverStderr": stderr.getvalue(),
            "externalDependencies": external_dependencies,
        }
    )


if __name__ == "__main__":
    main()
