#!/usr/bin/env python3
"""Capture what the pinned Python reference's plugins answer, at both ends.

Row 35 of ``docs/parity.md`` spans the plugin resolver under
``vibe/core/plugins/`` and the session surfaces the unified harness backend
publishes over it in ``vibe/app_server/``: ``plugin_catalog/read`` and its
``plugins/read`` alias, ``plugin/info``, ``plugin/reload``, the plugin rows of
``mcp_catalog/read`` and the plugin parts of ``runtime/read``. Two families
measure them, and each writes its own corpus because the two replays live in
two crates.

``resolve`` drives ``PluginResolver`` and ``PluginMaterializer`` in process
over the trees ``crates/vibe-core/tests/plugins-parity/fixtures.json`` holds:
native Agent Plugins packages with and without the Vibe extension, and the
Claude Code, Codex, Kimi Code and OpenCode formats the adapters translate. The
summary per case keeps every plugin's identity and digests, its skills, MCP
servers, hooks, knowledge, agents, libraries and connectors, what
materialization stages, and each diagnostic's code, severity, fatality,
component and file. Diagnostic messages are left out: the port writes its own
prose for them. ``crates/vibe-core/tests/plugins_parity_tests.rs`` replays it.

``server`` serves the reference's own ``vibe-app-server`` over stdio in a fresh
home per scenario, with ``--experimental-harness``, ``--legacy-harness``, or
neither, which since v2.26.0 starts the Unified Harness whatever the rollout in
the eval cache says, and records what the plugin methods answer around a
session start, a reload and the MCP mutations a plugin server refuses. One
scenario declares MCP servers a plugin ships: one found on ``PATH``, which is
the ``vibe-mcp-stdio-fixture`` binary this repository builds, one script the
plugin carries, and one command that does not exist. Driving the
``vibe-app-server-stdio-fixture`` binary through the same script with
``--server`` yields observations normalized the same way, and
``crates/vibe-app-server/tests/plugins_parity_tests.rs`` compares the two.

Normalization replaces every absolute path with a placeholder for the root it
sits under, reduces a server-authored diagnostic message to ``<prose>``, and
records a skill prompt as a byte length and a SHA-256. The shipped ``vibe``
plugin is this port's own text, so its content digest, its checkout and its
skills' descriptions and prompts are folded to placeholders on both sides.

Usage::

    python3 scripts/parity/plugins.py                     # capture the reference
    python3 scripts/parity/plugins.py --check             # recapture and compare
    python3 scripts/parity/plugins.py --family server \\
        --server target/debug/vibe-app-server-stdio-fixture --output /tmp/port.json

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The resolve family re-executes this
script under the reference interpreter.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
FIXTURES = REPOSITORY / "crates/vibe-core/tests/plugins-parity/fixtures.json"
RESOLVE_OUTPUT = REPOSITORY / "crates/vibe-core/tests/plugins-parity/corpus.json"
SERVER_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/plugins-parity/corpus.json"
MCP_FIXTURE = "vibe-mcp-stdio-fixture"
SCHEMA_VERSION = 1
SKILL_PATH_METADATA = "unified-harness.runtime-skill-path"
BUILTIN_PLUGIN = "vibe"
#: What a scenario waits for one answer before it gives up.
ANSWER_SECONDS = 120.0
#: How long notifications that trail an answer are collected.
QUIET_SECONDS = 0.5


class OracleError(RuntimeError):
    """Raised when a corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def resolve_reference(reference: Path, expected: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"reference checkout is missing: {reference}")
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=reference, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise OracleError(f"git rev-parse failed in {reference}: {result.stderr.strip()}")
    commit = result.stdout.strip()
    if expected and commit != expected:
        raise OracleError(f"reference checkout is at {commit}, not the pinned {expected}")
    return {"commit": commit}


def reexecute_with_reference_interpreter(reference: Path) -> None:
    try:
        import vibe  # noqa: F401

        return
    except ImportError:
        pass
    candidate = reference / ".venv/bin/python"
    if not candidate.is_file():
        raise OracleError(f"cannot import `vibe` and no reference interpreter at {candidate}")
    if Path(sys.executable).resolve() == candidate.resolve():
        raise OracleError(f"{candidate} cannot import `vibe`")
    os.execv(str(candidate), [str(candidate), str(Path(__file__).resolve()), *sys.argv[1:]])


def digest(text: str) -> dict[str, Any]:
    encoded = text.encode("utf-8")
    return {"len": len(encoded), "sha256": hashlib.sha256(encoded).hexdigest()}


# --------------------------------------------------------------------------
# Resolve family
# --------------------------------------------------------------------------


def write_case(home: Path, files: dict[str, str]) -> None:
    for relative, content in files.items():
        target = home / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")


def resolve_case(case: dict[str, Any], home: Path) -> dict[str, Any]:
    from vibe.core.plugins import PluginMaterializer, PluginResolver

    roots = {name: home / name for name in ("project", "user", "builtin")}
    orchestrator = None
    if case.get("configuredMcp"):
        orchestrator = SimpleNamespace(
            config=SimpleNamespace(
                mcp_servers=[SimpleNamespace(name=name) for name in case["configuredMcp"]]
            )
        )
    resolution = PluginResolver(
        project_roots=[roots["project"]] if roots["project"].is_dir() else [],
        user_roots=[roots["user"]] if roots["user"].is_dir() else [],
        builtin_roots=[roots["builtin"]] if roots["builtin"].is_dir() else [],
        data_root_base=home / "plugin-data",
        config_orchestrator=orchestrator,
    ).resolve()
    materialized = asyncio.run(PluginMaterializer().materialize(resolution))
    return summarize_resolution(home, resolution, materialized)


def summarize_resolution(home: Path, resolution: Any, materialized: Any) -> dict[str, Any]:
    def rel(path: Any) -> str | None:
        if path is None:
            return None
        return os.path.relpath(str(path), str(home))

    def masked(text: str) -> str:
        return text.replace(str(home), "<home>")

    plugins = [
        {
            "name": plugin.name,
            "version": plugin.version,
            "namespace": plugin.namespace,
            "description": plugin.description,
            "author": plugin.author,
            "scope": plugin.scope.value,
            "source_format": plugin.source_format.value,
            "manifest_digest": plugin.manifest_digest,
            "content_digest": plugin.content_digest,
            "manifest_path": rel(plugin.manifest_path),
            "root": rel(plugin.root),
            "data_root": rel(plugin.data_root),
        }
        for plugin in resolution.plugins
    ]
    skills = [
        {
            "alias": alias,
            "description": skill.description,
            "model_invocable": skill.model_invocable,
            "user_invocable": skill.user_invocable,
            "allowed_tools": list(skill.allowed_tools),
            "metadata": {
                key: value for key, value in skill.metadata.items() if key != SKILL_PATH_METADATA
            },
            "path": rel(skill.skill_path),
            # A skill declaring tools carries the port's own guidance preamble.
            "prompt": skill.prompt if not skill.allowed_tools else None,
        }
        for alias, skill in resolution.skills.items()
    ]
    hooks = [
        {
            "name": hook.config.name,
            "type": hook.config.type.value,
            "match": hook.config.match,
            "timeout": hook.config.timeout,
            "protocol": str(hook.protocol),
            "env": sorted(hook.environment),
            "cwd": rel(hook.cwd),
            "config_file": rel(hook.config_file),
            "order": hook.order,
        }
        for hook in resolution.runtime_hooks
    ]
    mcp = []
    for definition in resolution.mcp_servers:
        server = definition.server
        row: dict[str, Any] = {
            "plugin": definition.plugin_name,
            "source_id": definition.source_id,
            "alias": definition.private_alias,
            "transport": server.transport,
            "config_file": rel(definition.config_file),
        }
        if server.transport == "stdio":
            row.update(
                command=[masked(part) for part in server.command],
                args=[masked(part) for part in server.args],
                env={key: masked(value) for key, value in server.env.items()},
                cwd=masked(server.cwd) if server.cwd is not None else None,
            )
        else:
            row.update(url=server.url, headers=dict(server.http_headers()))
        mcp.append(row)
    knowledge = [
        {
            "plugin": item.plugin_name,
            "name": item.name,
            "source_name": item.source_name,
            "description": item.description,
            "display_name": item.display_name,
            "icon": item.icon,
            "source_root": rel(item.source_root),
            "source_entrypoint": rel(item.source_entrypoint),
            "runtime_root": rel(item.runtime_root),
            "runtime_entrypoint": rel(item.runtime_entrypoint),
        }
        for item in resolution.knowledge
    ]
    agents = [
        {
            "plugin": item.plugin_name,
            "name": item.name,
            "source_name": item.source_name,
            "source_file": rel(item.source_file),
            "display_name": item.profile.display_name,
            "description": item.profile.description,
            "safety": item.profile.safety.value,
            "agent_type": item.profile.agent_type.value,
            "instructions": item.profile.instructions,
            "overrides": json.loads(json.dumps(item.profile.overrides, sort_keys=True)),
        }
        for item in resolution.agents
    ]
    libraries = [
        {
            "plugin": item.plugin_name,
            "language": item.language,
            "alias": item.alias,
            "source_path": rel(item.source_path),
            "runtime_path": rel(item.runtime_path),
            "config_file": rel(item.config_file),
        }
        for item in resolution.libraries
    ]
    connectors = [
        {
            "plugin": item.plugin_name,
            "source_id": item.source_id,
            "tools": list(item.tools),
            "config_file": rel(item.config_file),
        }
        for item in resolution.connectors
    ]
    unsupported = [
        [item.plugin_name, item.kind, item.reason, rel(item.path)]
        for item in resolution.unsupported_components
    ]
    return {
        "plugins": plugins,
        "skills": skills,
        "hooks": hooks,
        "mcp": mcp,
        "knowledge": knowledge,
        "agents": agents,
        "libraries": libraries,
        "connectors": connectors,
        "issues": issue_rows(home, resolution.issues),
        "unsupported": unsupported,
        "materialized": {
            "knowledge": [
                [item.name, rel(item.runtime_root), rel(item.runtime_entrypoint)]
                for item in materialized.knowledge
            ],
            "libraries": [
                [item.language, item.alias, rel(item.runtime_path)]
                for item in materialized.libraries
            ],
            "environment": {
                key: masked(value) for key, value in sorted(materialized.process_environment.items())
            },
            "staged": sorted(
                os.path.relpath(str(path), str(home))
                for path in (home / "plugin-data").rglob("*")
                if path.is_file()
            )
            if (home / "plugin-data").is_dir()
            else [],
            "issues": issue_rows(home, materialized.issues),
        },
    }


def issue_rows(home: Path, issues: Any) -> list[list[Any]]:
    rows = [
        [
            issue.code,
            issue.severity,
            issue.fatal,
            issue.component,
            os.path.relpath(str(issue.file), str(home)),
        ]
        for issue in issues
    ]
    return sorted(rows, key=lambda row: json.dumps(row))


def capture_resolve(fixtures: dict[str, Any]) -> dict[str, Any]:
    captured = {}
    for name, case in sorted(fixtures["cases"].items()):
        with tempfile.TemporaryDirectory(prefix="vibe-plugins-oracle-") as directory:
            home = Path(directory).resolve()
            write_case(home, case["files"])
            captured[name] = resolve_case(case, home)
    return captured


# --------------------------------------------------------------------------
# Server family
# --------------------------------------------------------------------------

SCHEMA_URL = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
MCP_SCHEMA_URL = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json"


def manifest(**fields: Any) -> str:
    return json.dumps({"$schema": SCHEMA_URL, **fields}) + "\n"


def skill(name: str, description: str, body: str) -> str:
    return f"---\nname: {name}\ndescription: {description}\n---\n{body}\n"


DEMO = {
    "plugins/demo/plugin.json": manifest(name="demo", version="0.1.0", description="Probe plugin"),
    "plugins/demo/skills/hello/SKILL.md": skill("hello", "Says hello for the probe", "Say hello."),
}
PROJECT = {
    "plugins/local/plugin.json": manifest(name="local", version="0.0.1"),
    "plugins/local/skills/review/SKILL.md": skill("review", "Reviews the project", "Review it."),
}
SERVERS = {
    "plugins/servers/plugin.json": manifest(name="servers", version="0.2.0"),
    "plugins/servers/mcp.json": json.dumps(
        {
            "$schema": MCP_SCHEMA_URL,
            "mcpServers": {
                "fixture": {"type": "stdio", "command": MCP_FIXTURE, "env": {"LOG_LEVEL": "info"}},
                "script": {"type": "stdio", "command": "./bin/serve"},
                "missing": {"type": "stdio", "command": "./bin/missing"},
            },
        }
    )
    + "\n",
    "plugins/servers/bin/serve": f"#!/bin/sh\nexec {MCP_FIXTURE}\n",
}
BROKEN = {
    "plugins/broken/plugin.json": manifest(name="broken", flavor="unknown"),
    "plugins/skillless/plugin.json": manifest(name="skillless"),
}
ROLLOUT_CACHE = {
    "experiment_eval_cache.json": json.dumps(
        {
            "oracle": {
                "stored_at_timestamp": "<now>",
                "payload": {
                    "features": {"vibe_cli_unified_harness_rollout": {"defaultValue": "unified"}}
                },
            }
        }
    ).replace('"<now>"', "<now>")
}


def request(method: str, **params: Any) -> dict[str, Any]:
    return {"call": method, "params": params}


def edit(files: dict[str, str | None]) -> dict[str, Any]:
    return {"edit": files}


SESSION_READS = [
    request("plugin_catalog/read", sessionId="$S"),
    request("plugins/read", sessionId="$S"),
    request("plugin/info", sessionId="$S"),
    request("runtime/read", sessionId="$S"),
    request("mcp_catalog/read", sessionId="$S"),
]


def server_scenarios() -> list[dict[str, Any]]:
    return [
        {
            "name": "unified/user-and-project",
            "flags": ["--experimental-harness"],
            "home": DEMO,
            "workspace": {".vibe/" + path: text for path, text in PROJECT.items()},
            "trusted": True,
            "steps": SESSION_READS,
        },
        {
            "name": "unified/untrusted-project",
            "flags": ["--experimental-harness"],
            "home": DEMO,
            "workspace": {".vibe/" + path: text for path, text in PROJECT.items()},
            "trusted": False,
            "steps": [request("plugin_catalog/read", sessionId="$S")],
        },
        {
            "name": "unified/only-builtin",
            "flags": ["--experimental-harness"],
            "home": {},
            "steps": SESSION_READS,
        },
        {
            "name": "unified/dropped",
            "flags": ["--experimental-harness"],
            "home": BROKEN,
            "steps": [
                request("plugin_catalog/read", sessionId="$S"),
                request("runtime/read", sessionId="$S"),
            ],
        },
        {
            "name": "unified/mcp-servers",
            "flags": ["--experimental-harness"],
            "home": SERVERS,
            "executable": ["plugins/servers/bin/serve"],
            "steps": [
                *SESSION_READS,
                request("mcp_catalog/toggle", sessionId="$S", name="fixture", disabled=True),
                request("mcp_catalog/remove", sessionId="$S", name="fixture"),
                request("plugin/reload", sessionId="$S"),
            ],
        },
        {
            "name": "unified/reload",
            "flags": ["--experimental-harness"],
            "home": DEMO,
            "steps": [
                request("plugin/reload", sessionId="$S"),
                edit(
                    {
                        "plugins/demo/skills/hello/SKILL.md": skill(
                            "hello", "Says hello again", "Say hello twice."
                        ),
                        "plugins/added/plugin.json": manifest(name="added", version="1.0.0"),
                        "plugins/added/skills/new/SKILL.md": skill("new", "A new skill", "New."),
                    }
                ),
                request("plugin/reload", sessionId="$S"),
                request("plugin_catalog/read", sessionId="$S"),
                request("runtime/read", sessionId="$S"),
                edit({"plugins/added": None}),
                request("plugin/reload", sessionId="$S"),
                request("plugin_catalog/read", sessionId="$S"),
            ],
        },
        {
            "name": "unified/other-session",
            "flags": ["--experimental-harness"],
            "home": DEMO,
            "steps": [
                request("plugin_catalog/read", sessionId="not-this-session"),
                request("plugin/info", sessionId="not-this-session"),
                request("plugin/reload", sessionId="not-this-session"),
            ],
        },
        {
            "name": "unified/no-session",
            "flags": ["--experimental-harness"],
            "home": DEMO,
            "start": False,
            "steps": [
                request("plugin_catalog/read", sessionId="nobody"),
                request("plugin/info", sessionId="nobody"),
                request("plugin/reload", sessionId="nobody"),
            ],
        },
        {
            "name": "default/unified",
            "flags": [],
            "home": {**DEMO, **ROLLOUT_CACHE},
            "steps": [
                request("plugin_catalog/read", sessionId="$S"),
                request("config/read", sessionId="$S"),
            ],
        },
        {
            "name": "rollout/legacy-flag-wins",
            "flags": ["--legacy-harness"],
            "home": {**DEMO, **ROLLOUT_CACHE},
            "steps": [request("plugin_catalog/read", sessionId="$S")],
        },
        {
            "name": "legacy/declines",
            "flags": list(HARNESS_FLAGS),
            "home": DEMO,
            "steps": [
                request("plugin_catalog/read", sessionId="$S"),
                request("plugins/read", sessionId="$S"),
                request("plugin/info", sessionId="$S"),
                request("plugin/reload", sessionId="$S"),
                request("runtime/read", sessionId="$S"),
            ],
        },
    ]


class Server:
    """One app server over stdio, read line by line."""

    def __init__(self, command: list[str], env: dict[str, str], cwd: Path) -> None:
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            cwd=cwd,
        )
        self.next_id = 0
        self.buffer = b""

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write((json.dumps(message) + "\n").encode())
        self.process.stdin.flush()

    def _lines(self, timeout: float) -> list[dict[str, Any]]:
        assert self.process.stdout is not None
        ready, _, _ = select.select([self.process.stdout], [], [], timeout)
        if not ready:
            return []
        chunk = os.read(self.process.stdout.fileno(), 1 << 16)
        if not chunk:
            raise OracleError("the server closed its output")
        self.buffer += chunk
        messages = []
        while b"\n" in self.buffer:
            line, self.buffer = self.buffer.split(b"\n", 1)
            if line.strip():
                messages.append(json.loads(line))
        return messages

    def call(self, method: str, params: dict[str, Any]) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        self.next_id += 1
        identifier = self.next_id
        self.send({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
        notes: list[dict[str, Any]] = []
        deadline = time.monotonic() + ANSWER_SECONDS
        answer = None
        while answer is None:
            if time.monotonic() > deadline:
                raise OracleError(f"{method} was not answered")
            for message in self._lines(0.2):
                if message.get("id") == identifier and "method" not in message:
                    answer = message
                elif "method" in message and "id" not in message:
                    notes.append(message)
        quiet_until = time.monotonic() + QUIET_SECONDS
        while time.monotonic() < quiet_until:
            for message in self._lines(0.1):
                if "method" in message and "id" not in message:
                    notes.append(message)
        return answer, notes

    def stop(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()


def relax(root: Path) -> None:
    """Makes a hardened package store removable again."""
    for directory, names, files in os.walk(root):
        os.chmod(directory, 0o700)
        for name in names:
            os.chmod(os.path.join(directory, name), 0o700)
        for name in files:
            path = os.path.join(directory, name)
            if not os.path.islink(path):
                os.chmod(path, 0o600)


def write_tree(root: Path, files: dict[str, str | None]) -> None:
    for relative, content in files.items():
        target = root / relative
        if content is None:
            relax(target)
            shutil.rmtree(target)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content.replace("<now>", str(int(time.time()))), encoding="utf-8")


def run_scenario(
    scenario: dict[str, Any], command: list[str], fixture_dir: Path
) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="vibe-plugins-oracle-")).resolve()
    home = root / "home"
    vibe_home = home / ".vibe"
    workspace = root / "workspace"
    for directory in (vibe_home, workspace):
        directory.mkdir(parents=True)
    write_tree(vibe_home, scenario.get("home", {}))
    write_tree(workspace, scenario.get("workspace", {}))
    for relative in scenario.get("executable", []):
        os.chmod(vibe_home / relative, 0o755)
    trusted = [json.dumps(str(workspace))] if scenario.get("trusted") else []
    (vibe_home / "trusted_folders.toml").write_text(
        f"trusted = [{', '.join(trusted)}]\nuntrusted = []\n", encoding="utf-8"
    )
    env = {
        "PATH": f"{fixture_dir}:{os.environ.get('PATH', '/usr/bin:/bin')}",
        "HOME": str(home),
        "VIBE_HOME": str(vibe_home),
        "MISTRAL_API_KEY": "oracle-key",
        "VIBE_API_BASE": "http://127.0.0.1:9/v1/chat/completions",
        "LANG": "C.UTF-8",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "CI": "true",
        "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
    }
    server = Server([*command, *scenario["flags"]], env, workspace)
    steps: list[Any] = []
    session_id = None
    try:
        server.call("initialize", {"clientInfo": {"name": "plugins-oracle", "version": "0"}})
        server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        if scenario.get("start", True):
            answer, _ = server.call("session/start", {"agentConfig": {"cwd": str(workspace)}})
            session_id = answer["result"]["state"]["session"]["id"]
        for step in scenario["steps"]:
            if "edit" in step:
                write_tree(vibe_home, step["edit"])
                steps.append({"edit": sorted(step["edit"])})
                continue
            params = {
                key: session_id if value == "$S" else value for key, value in step["params"].items()
            }
            answer, notes = server.call(step["call"], params)
            steps.append(
                {
                    "call": step["call"],
                    "answer": answer.get("result", answer.get("error")),
                    "ok": "result" in answer,
                    "warnings": [
                        note["params"]["warning"]
                        for note in notes
                        if note.get("method") == "warning"
                    ],
                }
            )
    finally:
        server.stop()
    paths = {
        "<workspace>": str(workspace),
        "<vibe-home>": str(vibe_home),
        "<home>": str(home),
        "<fixtures>": str(fixture_dir),
    }
    relax(root)
    shutil.rmtree(root, ignore_errors=True)
    return {"steps": steps, "paths": paths}


PACKAGE = re.compile(r"/plugins/packages/[0-9a-f]{2}/[0-9a-f]{64}")


def normalize_server(run: dict[str, Any], reference_root: str | None) -> list[Any]:
    """Placeholders for every root, and the shipped plugin folded on both sides."""
    builtin_digests: set[str] = set()
    for step in run["steps"]:
        answer = step.get("answer")
        if not isinstance(answer, dict):
            continue
        catalog = answer.get("plugins")
        if isinstance(catalog, dict):
            for entry in catalog.get("plugins", []):
                if entry.get("name") == BUILTIN_PLUGIN and entry.get("contentSha256"):
                    builtin_digests.add(entry["contentSha256"])
        info = answer.get("info")
        if isinstance(info, dict):
            pinned = info.get("raw", {}).get("plugins", {}).get(BUILTIN_PLUGIN)
            if pinned:
                builtin_digests.add(pinned["contentSha256"])
    text = json.dumps(run["steps"])
    for digest_value in builtin_digests:
        text = text.replace(digest_value, "<builtin-digest>")
        text = text.replace(f"/{digest_value[:2]}/<builtin-digest>", "/<builtin-shard>/<builtin-digest>")
    for placeholder, path in sorted(run["paths"].items(), key=lambda item: -len(item[1])):
        text = text.replace(path, placeholder)
    if reference_root:
        text = text.replace(f"{reference_root}/vibe/plugins/builtins/vibe", "<builtin-root>")
    text = text.replace("<vibe-home>/builtin-plugins/vibe", "<builtin-root>")
    steps = json.loads(text)
    for step in steps:
        fold(step)
    return steps


def fold(value: Any) -> Any:
    """Prose to placeholders, prompts to digests, the shipped skills folded."""
    if isinstance(value, list):
        for item in value:
            fold(item)
        return value
    if not isinstance(value, dict):
        return value
    for key in ("dropped", "issues", "warnings"):
        rows = value.get(key)
        if isinstance(rows, list):
            for row in rows:
                if isinstance(row, dict) and "message" in row:
                    row["message"] = "<prose>"
    if value.get("source") == "plugin" and isinstance(value.get("name"), str):
        if value["name"].startswith(f"{BUILTIN_PLUGIN}:"):
            value["description"] = "<builtin-prose>"
            value["prompt"] = "<builtin-prose>"
        elif isinstance(value.get("prompt"), str):
            value["prompt"] = digest(value["prompt"])
    for child in value.values():
        fold(child)
    return value


def reduce_answer(call: str, answer: Any) -> Any:
    """Only the parts of a wide answer this part owns."""
    if not isinstance(answer, dict):
        return answer
    if call == "runtime/read" and "runtime" in answer:
        runtime = answer["runtime"]
        return {
            "experimentalHarness": runtime.get("experimentalHarness"),
            # The legacy builtins a legacy session lists belong to row 15.
            "skills": [
                skill for skill in runtime.get("skills") or [] if skill.get("source") == "plugin"
            ],
            "issues": runtime.get("issues"),
            "mcp": runtime.get("mcp"),
        }
    if call == "config/read" and "config" in answer:
        return {"startupIssue": answer.get("startupIssue")}
    return answer


def capture_server(command: list[str], fixture_dir: Path, reference_root: str | None) -> list[Any]:
    captured = []
    for scenario in server_scenarios():
        started = time.monotonic()
        run = run_scenario(scenario, command, fixture_dir)
        for step in run["steps"]:
            if "call" in step:
                step["answer"] = reduce_answer(step["call"], step["answer"])
        captured.append(
            {
                "name": scenario["name"],
                "scenario": scenario,
                "observed": normalize_server(run, reference_root),
            }
        )
        print(f"{scenario['name']}: {time.monotonic() - started:.1f}s", file=sys.stderr)
    return captured


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--family", choices=("resolve", "server", "all"), default="all")
    parser.add_argument("--server", type=Path, default=None, help="drive this binary instead")
    parser.add_argument(
        "--mcp-fixture",
        type=Path,
        default=REPOSITORY / "target/debug" / MCP_FIXTURE,
        help="the MCP server a plugin finds on PATH",
    )
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument(
        "--check", action="store_true", help="compare with the committed corpus, write nothing"
    )
    return parser.parse_args()


def write_json(path: Path, document: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, indent=1, sort_keys=True) + "\n", encoding="utf-8")


def emit(path: Path, document: Any, key: str, check: bool) -> None:
    """Writes `document`, or with `check` compares its `key` with the committed one."""
    if not check:
        write_json(path, document)
        return
    committed = json.loads(path.read_text(encoding="utf-8"))
    if committed.get(key) != json.loads(json.dumps(document[key])):
        raise OracleError(f"the pinned reference no longer answers what {path} records")
    print(f"{path}: the pinned reference still answers what it records", file=sys.stderr)


def main() -> int:
    arguments = parse_arguments()
    try:
        families = ("resolve", "server") if arguments.family == "all" else (arguments.family,)
        if "resolve" in families and arguments.server is not None:
            raise OracleError("the resolve family runs the reference in process; drop --server")
        if "resolve" in families:
            reexecute_with_reference_interpreter(arguments.reference)
        if arguments.server is None:
            reference = resolve_reference(arguments.reference, arguments.expected_commit)
        else:
            reference = {"commit": "server-override"}
        if "resolve" in families:
            fixtures = json.loads(FIXTURES.read_text(encoding="utf-8"))
            emit(
                arguments.output or RESOLVE_OUTPUT,
                {
                    "schemaVersion": SCHEMA_VERSION,
                    "reference": reference,
                    "note": (
                        "Captured from the pinned reference by scripts/parity/plugins.py over "
                        "fixtures.json. Names, digests, paths and diagnostic codes are "
                        "observations; diagnostic messages are not recorded."
                    ),
                    "cases": capture_resolve(fixtures),
                },
                "cases",
                arguments.check,
            )
        if "server" in families:
            fixture = arguments.mcp_fixture.resolve()
            if not fixture.is_file():
                raise OracleError(f"build {MCP_FIXTURE} first: no binary at {fixture}")
            if arguments.server is not None:
                command = [str(arguments.server.resolve())]
                reference_root = None
            else:
                binary = arguments.reference / ".venv/bin/vibe-app-server"
                if not binary.is_file():
                    raise OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
                command = [str(binary)]
                reference_root = str(arguments.reference.resolve())
            emit(
                arguments.output or SERVER_OUTPUT,
                {
                    "schemaVersion": SCHEMA_VERSION,
                    "reference": reference,
                    "scenarios": capture_server(command, fixture.parent, reference_root),
                },
                "scenarios",
                arguments.check,
            )
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
