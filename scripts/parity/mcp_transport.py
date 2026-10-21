#!/usr/bin/env python3
"""Capture what the pinned reference's MCP client sends and publishes.

Row 11 of ``docs/parity.md`` is MCP. This capture drives the reference's own
MCP client in the composition its app server builds for a legacy session: an
``MCPAuthenticationService`` bound to the configured servers, the resolved
catalog ``MCPCatalogService`` derives from it, and an ``MCPRegistry`` configured
through ``configure_legacy_mcp_registry`` with a descriptor cache. Every server
it meets is ``vibe-mcp-script-fixture``, a scripted MCP server built from this
repository, so the replay in ``crates/vibe-app-server/tests`` drives this port's
client against the very same server and the two differ only in what they send
and how they read the answers.

A scenario is a list of servers, the script each answers from, and a list of
steps: discover, call a published tool, start a fresh process (a new registry
and pool over the same descriptor cache), age the cache, or close the pool.
What the capture records per scenario:

``published``      each discovery's tools: published name, the description the
                   model reads and the parameter schema
``calls``          each call's outcome: the text the agent loop hands the model,
                   or the class of failure and a digest of its message
``state``          after each discovery: the status per server, which ones need
                   authentication, the descriptor revision, discovery failures
``authRequired``   every authorization requirement the registry published
``wire``           every message the fixture received, in order, normalized
``cache``          the descriptor cache records on disk, timestamps removed
``fingerprints``   the canonical identity each server's fingerprint hashes

The committed corpus carries only values the scenarios supplied, names, codes,
counts and digests: every message the reference authors is reduced to its
length and SHA-256, which is what ``NOTICE`` requires.

Usage::

    scripts/parity/mcp_transport.py --corpus          # capture and write the corpus
    scripts/parity/mcp_transport.py --check           # recapture and compare

``--fixture`` names a built ``vibe-mcp-script-fixture``; without it the script
builds one with cargo. ``--reference`` names the checkout; the script
re-executes itself under the reference interpreter against a ``git archive`` of
the pinned commit.
"""

from __future__ import annotations

import argparse
import asyncio
import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned  # noqa: E402
import host_env  # noqa: E402
from mcp_transport_scenarios import TOKEN, TOKEN_VARIABLE, all_scenarios  # noqa: E402

SCHEMA_VERSION = 1
DEFAULT_CORPUS = Path("crates/vibe-app-server/tests/mcp-transport/corpus.json")
FIXTURE_VARIABLE = "VIBE_MCP_PARITY_FIXTURE"
REPOSITORY = Path(__file__).resolve().parents[2]

#: What the SDK lets a stdio server inherit on POSIX. A name here is present or
#: absent depending on who runs the capture, so the normalized log drops it and
#: keeps every other name, which is the part a client decides.
INHERITED_NAMES = {"HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"}

#: What the SDK lets a stdio server inherit on win32 instead. The log drops
#: these too, while the corpus keeps recording the POSIX set above.
WINDOWS_INHERITED_NAMES = {
    "APPDATA", "HOMEDRIVE", "HOMEPATH", "LOCALAPPDATA", "PATH", "PATHEXT",
    "PROCESSOR_ARCHITECTURE", "SYSTEMDRIVE", "SYSTEMROOT", "TEMP", "USERNAME", "USERPROFILE",
}

#: Placeholders for what differs between two runs of the same scenario.
FIXTURE_PLACEHOLDER = "__FIXTURE__"
PORT_PLACEHOLDER = "__PORT__"
FINGERPRINT_PLACEHOLDER = "__FINGERPRINT__"


def reference_version() -> str:
    from vibe import __version__

    return __version__


def digest(text: str) -> dict[str, Any]:
    return {"length": len(text), "sha256": hashlib.sha256(text.encode()).hexdigest()}


def _posts(entry: dict[str, Any], method: str) -> bool:
    body = entry.get("body")
    return entry.get("kind") == "http" and isinstance(body, dict) and body.get("method") == method


def _opens_stream(entry: dict[str, Any]) -> bool:
    return entry.get("kind") == "http" and entry.get("method") == "GET"


def stream_after_initialized(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The wire with each session's event-stream GET right after its
    `notifications/initialized` POST.

    The reference's HTTP client opens its event stream concurrently with that
    notification and with the requests that follow it, so the GET reaches the
    fixture before, between or after them, even after the DELETE that ends the
    session. This records it where the client issues it.
    """

    ordered = list(entries)
    index = 0
    while index < len(ordered):
        if _posts(ordered[index], "notifications/initialized"):
            start = index
            while start > 0 and not _posts(ordered[start - 1], "initialize"):
                start -= 1
            end = index + 1
            while end < len(ordered) and not _posts(ordered[end], "initialize"):
                end += 1
            stream = next((at for at in range(start, end) if _opens_stream(ordered[at])), None)
            if stream is not None:
                moved = ordered.pop(stream)
                if stream < index:
                    index -= 1
                ordered.insert(index + 1, moved)
        index += 1
    return ordered


# --------------------------------------------------------------------------
# Running one scenario against the reference
# --------------------------------------------------------------------------


class Scenario:
    def __init__(self, spec: dict[str, Any], fixture: Path, root: Path) -> None:
        self.spec = spec
        self.fixture = fixture
        self.root = root
        self.processes: list[subprocess.Popen[bytes]] = []
        self.ports: dict[str, str] = {}
        self.files: dict[str, dict[str, Path]] = {}
        for server in spec["servers"]:
            name = server["name"]
            directory = root / name
            directory.mkdir(parents=True)
            script = directory / "script.json"
            script.write_text(json.dumps(spec["scripts"][name]), encoding="utf-8", newline="")
            self.files[name] = {
                "script": script,
                "log": directory / "log.jsonl",
                "state": directory / "state.json",
                "port": directory / "port",
            }

    def environment(self, name: str) -> dict[str, str]:
        files = self.files[name]
        return {
            "VIBE_MCP_SCRIPT": str(files["script"]),
            "VIBE_MCP_LOG": str(files["log"]),
            "VIBE_MCP_STATE": str(files["state"]),
        }

    def start_http(self, name: str) -> str:
        files = self.files[name]
        environment = {**os.environ, **self.environment(name), "VIBE_MCP_PORT_FILE": str(files["port"])}
        process = subprocess.Popen([str(self.fixture), "http"], env=environment)
        self.processes.append(process)
        deadline = time.monotonic() + 10
        while not files["port"].exists():
            if time.monotonic() > deadline:
                raise OracleError(f"the HTTP fixture for {name} never bound a port")
            time.sleep(0.01)
        port = files["port"].read_text(encoding="utf-8").strip()
        self.ports[name] = port
        return port

    def server_models(self) -> list[Any]:
        from vibe.core.config import MCPHttp, MCPStdio, MCPStreamableHttp

        models = []
        for server in self.spec["servers"]:
            fields = {key: value for key, value in server.items() if key != "command_form"}
            name = server["name"]
            if server["transport"] == "stdio":
                if server.get("command_form") == "text":
                    fields["command"] = f"{self.fixture} stdio"
                else:
                    fields["command"] = [str(self.fixture), "stdio"]
                fields["env"] = {**server.get("env", {}), **self.environment(name)}
                models.append(MCPStdio.model_validate(fields))
            else:
                port = self.start_http(name)
                fields["url"] = f"http://127.0.0.1:{port}/mcp"
                model = MCPHttp if server["transport"] == "http" else MCPStreamableHttp
                models.append(model.model_validate(fields))
        return models

    def normalize(self, value: Any) -> Any:
        if isinstance(value, str):
            for spelling in host_env.path_spellings(str(self.fixture)):
                value = value.replace(spelling, FIXTURE_PLACEHOLDER)
            for port in self.ports.values():
                value = value.replace(f"127.0.0.1:{port}", f"127.0.0.1:{PORT_PLACEHOLDER}")
            for spelling in host_env.path_spellings(str(self.root)):
                value = value.replace(spelling, "__SCENARIO__")
            # The user agent carries the package version, which each
            # implementation reports for itself.
            value = value.replace(f"MistralAI-VibeCLI/{reference_version()}", "MistralAI-VibeCLI/__VERSION__")
            return value
        if isinstance(value, list):
            return [self.normalize(item) for item in value]
        if isinstance(value, dict):
            return {self.normalize(key): self.normalize(item) for key, item in value.items()}
        return value

    def wire(self, name: str) -> list[dict[str, Any]]:
        log = self.files[name]["log"]
        if not log.exists():
            return []
        entries = []
        for line in log.read_text(encoding="utf-8").splitlines():
            entry = json.loads(line)
            if entry.get("kind") == "spawn":
                inherited = INHERITED_NAMES | (WINDOWS_INHERITED_NAMES if os.name == "nt" else set())
                entry["envNames"] = sorted(set(entry["envNames"]) - inherited)
                entry["cwd"] = "__CWD__" if entry.get("cwd") == os.getcwd() else self.normalize(entry.get("cwd"))
            entries.append(self.normalize(entry))
        return stream_after_initialized(entries)

    async def settle(self) -> None:
        """Waits until every stdio server spawned has logged how it ended."""

        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if all(self._settled(name) for name in self.files):
                return
            await asyncio.sleep(0.05)

    def _settled(self, name: str) -> bool:
        log = self.files[name]["log"]
        if not log.exists():
            return True
        spawned: set[int] = set()
        ended: set[int] = set()
        for line in log.read_text(encoding="utf-8").splitlines():
            entry = json.loads(line)
            if entry.get("kind") == "spawn":
                spawned.add(entry["process"])
            elif entry.get("kind") in {"eof", "exit"}:
                ended.add(entry["process"])
        return spawned <= ended

    def close(self) -> None:
        for process in self.processes:
            process.kill()
            process.wait()


class SamplingBackend:
    """Answers a sampling completion with a fixed message and records the call."""

    def __init__(self) -> None:
        self.calls: list[dict[str, Any]] = []

    async def complete(self, *, model: Any, messages: list[Any], temperature: Any, tools: Any, max_tokens: Any, tool_choice: Any, extra_headers: Any, metadata: Any) -> Any:
        from types import SimpleNamespace

        self.calls.append(
            {
                "messages": [{"role": str(message.role.value), "content": message.content} for message in messages],
                "maxTokens": max_tokens,
            }
        )
        return SimpleNamespace(message=SimpleNamespace(content="sampled answer"))


class SamplingConfig:
    def get_active_model(self) -> Any:
        from types import SimpleNamespace

        return SimpleNamespace(name="oracle-model", temperature=0.2)


class Session:
    """One process's worth of MCP state: authentication, registry and pool."""

    def __init__(self, scenario: Scenario, servers: list[Any]) -> None:
        from vibe.app_server._legacy_session_backend import configure_legacy_mcp_registry
        from vibe.app_server._mcp_auth import MCPAuthenticationService
        from vibe.app_server._session_backend_port import ResolvedMCPCatalog
        from vibe.app_server.mcp_catalog import MCPCatalogService
        from vibe.core.tools.mcp.pool import MCPConnectionPool
        from vibe.core.tools.mcp.registry import MCPRegistry
        from vibe.core.tools.mcp_sampling import MCPSamplingHandler

        self.scenario = scenario
        self.servers = servers
        self.authentication = MCPAuthenticationService()
        self.catalog = MCPCatalogService(self.authentication)
        self.registry = MCPRegistry()
        self.pool = MCPConnectionPool()
        self.required: list[dict[str, Any]] = []
        self.backend = SamplingBackend()
        self.sampling = MCPSamplingHandler(lambda: self.backend, SamplingConfig)
        self._configure = lambda: configure_legacy_mcp_registry(
            self.registry,
            ResolvedMCPCatalog(
                revision="oracle",
                servers=tuple(self.catalog._resolve_server(server) for server in servers),  # noqa: SLF001
            ),
            self.authentication,
            required_sink=self._record_required,
            descriptor_cache_root=scenario.root / "descriptors",
        )
        self.tools: dict[str, Any] = {}

    def _record_required(self, name: str, required: Any) -> None:
        self.required.append(
            {
                "name": name,
                "reason": required.reason,
                "descriptorRevision": required.descriptor_revision,
                "observedConnectionRevision": required.observed_connection_revision,
            }
        )

    async def start(self) -> None:
        await self.authentication.bind_catalog(self.servers)
        self._configure()

    async def discover(self) -> dict[str, Any]:
        tools = await self.registry.get_tools_async(self.servers)
        self.tools = tools
        published = [
            {
                "name": name,
                # The description carries the reference's own fallback wording
                # when a server declares none, so it is compared by digest.
                "description": digest(self.scenario.normalize(tool.description)),
                "parameters": tool.get_parameters(),
            }
            for name, tool in tools.items()
        ]
        failed = self.registry.pop_failed()
        return {
            "published": published,
            "state": {
                "status": {name: status.value for name, status in sorted(self.registry.status().items())},
                "needsAuth": sorted(self.registry.needs_auth),
                "descriptorRevisions": {
                    server.name: self.registry.descriptor_revision(server.name) for server in self.servers
                },
                "failed": {name: digest(message) for name, message in sorted(failed.items())},
            },
        }

    async def call(self, tool_name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        from vibe.core.tools.base import InvokeContext, ToolError

        tool_class = self.tools.get(tool_name)
        if tool_class is None:
            return {"unpublished": True}
        tool = tool_class.from_config(lambda: tool_class._get_tool_config_class()())  # noqa: SLF001
        context = InvokeContext(tool_call_id="oracle-call", mcp_pool=self.pool, sampling_callback=self.sampling)
        result = None
        try:
            async for item in tool.invoke(context, **arguments):
                result = item
        except ToolError as error:
            return {"error": {"kind": "tool", "message": digest(str(error))}}
        except Exception as error:  # noqa: BLE001 - recorded, never raised
            return {"error": {"kind": type(error).__name__, "message": digest(str(error))}}
        dumped = result.model_dump(mode="json")
        return {"text": "\n".join(f"{key}: {value}" for key, value in dumped.items())}

    async def close(self) -> None:
        await self.pool.aclose()


def age_cache(root: Path, seconds: float) -> None:
    from datetime import UTC, datetime, timedelta

    for path in sorted((root / "descriptors").glob("*.json")):
        record = json.loads(path.read_text(encoding="utf-8"))
        moved = datetime.now(UTC) - timedelta(seconds=seconds)
        stamp = moved.isoformat().replace("+00:00", "Z")
        record["discoveredAt"] = stamp
        record["lastUsedAt"] = stamp
        path.write_text(json.dumps(record, separators=(",", ":")), encoding="utf-8", newline="")


def fingerprint_identity(server: Any) -> dict[str, Any]:
    """The canonical identity ``_server_fingerprint`` hashes, and its hash."""

    from vibe.app_server import _mcp_auth
    from vibe.core.auth.mcp_oauth import Fingerprint
    from vibe.core.config import MCPHttp, MCPOAuth, MCPStreamableHttp

    if isinstance(server, MCPHttp | MCPStreamableHttp):
        auth = server.auth
        if isinstance(auth, MCPOAuth):
            identity: object = Fingerprint.compute(server).model_dump(mode="json")
        else:
            identity = {
                "type": "static",
                "header_names": sorted(auth.headers),
                "api_key_env": auth.api_key_env,
                "api_key_header": auth.api_key_header,
                "api_key_format": auth.api_key_format,
            }
        value: dict[str, Any] = {
            "name": server.name,
            "transport": server.transport,
            "url": server.url,
            "auth": identity,
            "prompt": server.prompt,
            "startup_timeout_sec": server.startup_timeout_sec,
            "tool_timeout_sec": server.tool_timeout_sec,
            "sampling_enabled": server.sampling_enabled,
        }
    else:
        value = server.model_dump(mode="json", exclude={"env"})
        value["env_names"] = sorted(server.env)
    canonical = json.dumps(value, sort_keys=True, separators=(",", ":"))
    fingerprint = hashlib.sha256(canonical.encode()).hexdigest()
    if fingerprint != _mcp_auth._server_fingerprint(server):  # noqa: SLF001
        raise OracleError(f"the fingerprint identity of {server.name} no longer matches the reference")
    return {"canonical": canonical, "sha256": fingerprint}


async def run_scenario(spec: dict[str, Any], fixture: Path) -> dict[str, Any]:
    with host_env.scratch_root("mcp-transport-") as root:
        scenario = Scenario(spec, fixture, root)
        sessions: list[Session] = []
        try:
            servers = scenario.server_models()
            fingerprints = {server.name: fingerprint_identity(server) for server in servers}
            session = Session(scenario, servers)
            sessions.append(session)
            await session.start()
            steps: list[dict[str, Any]] = []
            for step in spec["steps"]:
                match step["do"]:
                    case "discover":
                        steps.append({"do": "discover", **await session.discover()})
                    case "call":
                        steps.append({"do": "call", "tool": step["tool"], **await session.call(step["tool"], step["arguments"])})
                    case "restart":
                        await session.close()
                        session = Session(scenario, servers)
                        sessions.append(session)
                        await session.start()
                        steps.append({"do": "restart"})
                    case "ageCache":
                        age_cache(root, step["seconds"])
                        steps.append({"do": "ageCache"})
                    case other:
                        raise OracleError(f"unknown step {other}")
            await session.close()
            await scenario.settle()
            cache = []
            for path in sorted((root / "descriptors").glob("*.json")):
                record = json.loads(path.read_text(encoding="utf-8"))
                key = record["key"]
                if path.name != f"{hashlib.sha256(key.encode()).hexdigest()}.json":
                    raise OracleError("a descriptor cache file is not named by its key")
                parsed_key = json.loads(key)
                parsed_key["serverFingerprint"] = next(
                    (name for name, identity in fingerprints.items() if identity["sha256"] == parsed_key["serverFingerprint"]),
                    parsed_key["serverFingerprint"],
                )
                record["key"] = parsed_key
                record["discoveredAt"] = "__TIME__"
                record["lastUsedAt"] = "__TIME__"
                cache.append(record)
            # A file is named by a hash over the port and the fixture path, so the
            # directory order is not an order two runs share.
            cache.sort(key=lambda record: (record["sourceName"], json.dumps(record["key"], sort_keys=True)))
            required = [entry for session in sessions for entry in session.required]
            sampled = [call for session in sessions for call in session.backend.calls]
            observed = {
                "steps": steps,
                "authRequired": required,
                "sampling": sampled,
                "wire": {name: scenario.wire(name) for name in scenario.files},
                "cache": cache,
                "fingerprints": {name: {"canonical": identity["canonical"]} for name, identity in fingerprints.items()},
            }
            normalized = scenario.normalize(observed)
            return replace_fingerprints(normalized, {identity["sha256"]: name for name, identity in fingerprints.items()})
        finally:
            for session in sessions:
                try:
                    await session.close()
                except Exception:  # noqa: BLE001 - teardown only
                    pass
            scenario.close()


def replace_fingerprints(value: Any, known: dict[str, str]) -> Any:
    """Replaces every fingerprint hash, whole or as the 16-digit revision prefix."""

    if isinstance(value, str):
        for fingerprint, name in known.items():
            value = value.replace(fingerprint, f"{FINGERPRINT_PLACEHOLDER}{name}")
            value = value.replace(fingerprint[:16], f"{FINGERPRINT_PLACEHOLDER}{name}")
        return value
    if isinstance(value, list):
        return [replace_fingerprints(item, known) for item in value]
    if isinstance(value, dict):
        return {key: replace_fingerprints(item, known) for key, item in value.items()}
    return value


def capture(reference: dict[str, str], fixture: Path) -> dict[str, Any]:
    os.environ[TOKEN_VARIABLE] = TOKEN
    scenarios = []
    for spec in all_scenarios():
        first = asyncio.run(run_scenario(spec, fixture))
        second = asyncio.run(run_scenario(spec, fixture))
        if first != second:
            if os.environ.get("VIBE_MCP_PARITY_DUMP"):
                Path(os.environ["VIBE_MCP_PARITY_DUMP"]).write_text(json.dumps([first, second], indent=1, sort_keys=True), encoding="utf-8", newline="")
            raise OracleError(f"scenario {spec['name']} is not deterministic across two captures")
        scenarios.append({"name": spec["name"], "spec": copy.deepcopy(spec), "observed": first})
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Captured by scripts/parity/mcp_transport.py from the pinned reference against "
            "vibe-mcp-script-fixture. Scenario inputs, wire messages, names and digests only: "
            "every reference-authored message is a length and a SHA-256."
        ),
        "tokenVariable": TOKEN_VARIABLE,
        "token": TOKEN,
        "inheritedNames": sorted(INHERITED_NAMES),
        "scenarios": scenarios,
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def build_fixture() -> Path:
    result = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "vibe-app-server",
            "--features",
            "test-fixtures",
            "--bin",
            "vibe-mcp-script-fixture",
            "--message-format",
            "json",
        ],
        cwd=REPOSITORY,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(f"cannot build the MCP fixture: {result.stderr[-2000:]}")
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            if message["target"]["name"] == "vibe-mcp-script-fixture":
                return Path(message["executable"])
    raise OracleError("cargo built no vibe-mcp-script-fixture executable")


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument("--fixture", type=Path, default=None)
    parser.add_argument("--corpus", type=Path, nargs="?", const=DEFAULT_CORPUS, default=None)
    parser.add_argument("--output", type=Path, default=None,
                        help="write the capture here instead of the committed corpus")
    parser.add_argument("--check", action="store_true",
                        help="recapture and fail when the committed corpus differs")
    parser.add_argument("--only", default=None, help="capture only scenarios whose name starts so")
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if not os.environ.get(FIXTURE_VARIABLE):
            fixture = (arguments.fixture or build_fixture()).resolve()
            os.environ[FIXTURE_VARIABLE] = str(fixture)
        commit, _ = reexecute_pinned(
            arguments.reference,
            arguments.expected_commit,
            cache=arguments.cache,
            override=arguments.python,
        )
        reference = {"commit": commit, "path": str(arguments.reference)}
        fixture = Path(os.environ[FIXTURE_VARIABLE])
        if arguments.only:
            global all_scenarios  # noqa: PLW0603
            everything = all_scenarios
            all_scenarios = lambda: [s for s in everything() if s["name"].startswith(arguments.only)]  # noqa: E731
        corpus = capture(reference, fixture)
    except OracleError as error:
        print(f"MCP transport capture failed: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps(corpus, indent=1, sort_keys=True, ensure_ascii=False) + "\n"
    if arguments.check:
        committed = (REPOSITORY / DEFAULT_CORPUS).read_text(encoding="utf-8")
        if committed != rendered:
            print("the committed corpus differs from a fresh capture", file=sys.stderr)
            return 1
        print(f"corpus matches a fresh capture of {len(corpus['scenarios'])} scenarios")
        return 0
    target = arguments.output or arguments.corpus
    if target is None:
        sys.stdout.write(rendered)
        return 0
    target = target if target.is_absolute() else REPOSITORY / target
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(rendered, encoding="utf-8", newline="")
    print(f"captured {len(corpus['scenarios'])} scenarios from {reference['commit'][:12]} into {target}")
    print(f"python {platform.python_version()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
