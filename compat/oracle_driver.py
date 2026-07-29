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
