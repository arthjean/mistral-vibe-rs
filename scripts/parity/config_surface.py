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

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 3
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/config-surface/corpus.json")
#: The strategies the reference vocabulary declares but no field adopts, so the
#: Rust port implements neither. The census asserts this stays true.
UNREACHABLE_STRATEGIES = ("merge", "conflict")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"
#: Stands in for the machine-dependent vibe home in the captured default
#: document, so the corpus stays identical on every workstation.
VIBE_HOME_PLACEHOLDER = "{vibe_home}"


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
#: ``models`` is deliberately absent: the reference runs a normalization
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
        # One layer alone is coalesced through, so the merge key is never read
        # and an entry without it still reaches the document.
        "name": "union-single-layer-entry-without-its-merge-key",
        "layers": [
            (
                "user",
                '[[mcp_servers]]\ntransport = "stdio"\ncommand = "/usr/bin/nameless-mcp"\n',
            ),
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
# MCP scenarios
# --------------------------------------------------------------------------

#: The environment variable the captured ``http_headers`` reads its token from.
#: Both the name and the value are authored here, so nothing secret is recorded.
MCP_TOKEN_VARIABLE = "VIBE_PARITY_MCP_TOKEN"
MCP_TOKEN_VALUE = "parity-token"

#: URLs run through ``normalize_mcp_server_url``, its comparison key and the
#: name suggested from it. Rejections record only that the URL was refused: the
#: message is reference-authored prose.
MCP_URLS: list[str] = [
    "https://mcp.example.com/tools",
    "HTTPS://MCP.Example.COM:443/Tools",
    "https://mcp.example.com/tools/",
    "https://mcp.example.com",
    "https://mcp.example.com:8443/rpc?version=1",
    "https://[2001:DB8::1]:8443/rpc",
    "http://localhost:3000/mcp",
    "http://127.0.0.1:3000/mcp",
    "http://[::1]:3000/mcp",
    "http://mcp.example.com/rpc",
    "https://user:secret@mcp.example.com/rpc",
    "https://mcp.example.com/rpc#section",
    "mcp.example.com/rpc",
    "ftp://mcp.example.com/rpc",
    "https://",
    "   ",
    "https://www.example.com/rpc",
    "https://api.example.com/v1",
    "https://api.example/api/mcp",
    "https://mcp.example/rpc",
    "https://mcp.github.com/api",
    "https://server.example.com/",
]

#: Names run through ``normalize_mcp_server_name``.
MCP_NAMES: list[str] = [
    "docs",
    "My Server!",
    "__docs--",
    "héllo",
    "!!!",
    "a" * 300,
    "UPPER_case-9",
]

#: ``(requested name, URL, already configured names)`` triples run through the
#: name resolution an add performs.
MCP_NAME_RESOLUTIONS: list[tuple[str | None, str, list[str]]] = [
    (None, "https://mcp.github.com/api", []),
    (None, "https://mcp.github.com/api", ["github"]),
    (None, "https://mcp.github.com/api", ["github", "github_2"]),
    (None, "https://api.example/api/mcp", ["mcp"]),
    ("docs", "https://mcp.github.com/api", []),
    ("docs", "https://mcp.github.com/api", ["docs"]),
    ("My Server!", "https://mcp.github.com/api", []),
    ("!!!", "https://mcp.github.com/api", []),
]

#: Persisted entries run through the transport and auth unions. Every document
#: is authored for this corpus.
MCP_ENTRIES: list[tuple[str, str]] = [
    (
        "streamable-http-bare",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
""",
    ),
    (
        "legacy-http-transport",
        """
[[mcp_servers]]
name = "docs"
transport = "http"
url = "https://docs.example/mcp"
""",
    ),
    (
        "unknown-transport",
        """
[[mcp_servers]]
name = "docs"
transport = "sse"
url = "https://docs.example/mcp"
""",
    ),
    (
        "static-auth-block",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_env = "VIBE_PARITY_MCP_TOKEN"
api_key_header = "X-Api-Key"
api_key_format = "Token {token}"
headers = { X-Trace = "on" }
""",
    ),
    (
        "static-auth-token-format-spec",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_env = "VIBE_PARITY_MCP_TOKEN"
api_key_format = "Bearer {token:>8}"
""",
    ),
    (
        "static-auth-escaped-placeholder",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_format = "Bearer {{token}}"
""",
    ),
    (
        "static-auth-foreign-placeholder",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_format = "Bearer {secret}"
""",
    ),
    (
        "static-auth-invalid-header",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
headers = { "Bad Header" = "1" }
""",
    ),
    (
        "static-auth-duplicate-header",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
headers = { Authorization = "a", authorization = "b" }
""",
    ),
    (
        "static-auth-invalid-variable",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_env = "2BAD"
""",
    ),
    (
        "explicit-header-wins-over-token",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "static"
api_key_env = "VIBE_PARITY_MCP_TOKEN"
headers = { authorization = "Bearer explicit" }
""",
    ),
    (
        "legacy-auth-promotion",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
api_key_env = "VIBE_PARITY_MCP_TOKEN"
headers = { X-Trace = "on" }
""",
    ),
    (
        "legacy-auth-mixed-with-block",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
api_key_env = "VIBE_PARITY_MCP_TOKEN"

[mcp_servers.auth]
type = "static"
""",
    ),
    (
        "oauth-block",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "oauth"
scopes = ["repo", "read"]
client_id = "vibe"
redirect_port = 51000
""",
    ),
    (
        "oauth-without-scopes",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "oauth"
client_id = "vibe"
""",
    ),
    (
        "oauth-conflicting-identity",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "oauth"
scopes = []
client_id = "vibe"
client_metadata_url = "https://docs.example/client.json"
""",
    ),
    (
        "oauth-privileged-redirect-port",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"

[mcp_servers.auth]
type = "oauth"
scopes = []
redirect_port = 80
""",
    ),
    (
        "stdio-string-command",
        """
[[mcp_servers]]
name = "local"
transport = "stdio"
command = "npx -y @scope/server"
args = ["--stdio"]
""",
    ),
    (
        "stdio-list-command",
        """
[[mcp_servers]]
name = "local"
transport = "stdio"
command = ["npx", "-y", "@scope/server"]
args = ["--stdio"]
""",
    ),
    (
        "entry-flags",
        """
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://docs.example/mcp"
prompt = "Search the handbook first"
sampling_enabled = false
disabled = true
disabled_tools = ["search"]
""",
    ),
    (
        "name-normalized-on-read",
        """
[[mcp_servers]]
name = "My Server!"
transport = "streamable-http"
url = "https://docs.example/mcp"
""",
    ),
    (
        "name-without-letters-or-numbers",
        """
[[mcp_servers]]
name = "!!!"
transport = "streamable-http"
url = "https://docs.example/mcp"
""",
    ),
]


def capture_mcp(reference: Path) -> dict[str, Any]:
    """How the reference names, addresses and decodes an MCP entry."""
    sys.path.insert(0, str(reference))
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


#: Layer stacks replayed on top of the real default layer, recording what the
#: reference *validates* rather than what it merges. They exist because the
#: model rules — sparse completion, the global compaction threshold, the unknown
#: active-model fallback — only run once the merged document is validated.
MODEL_SCENARIOS: list[dict[str, Any]] = [
    {
        "name": "models-defaults-only",
        "layers": [],
    },
    {
        "name": "models-sparse-override-of-a-default-model",
        "layers": [("user", '[[models]]\nalias = "local"\ntemperature = 0.9\n')],
    },
    {
        "name": "models-alias-map-form",
        "layers": [("user", "[models.local]\ntemperature = 0.55\n")],
    },
    {
        "name": "models-added-entry-inherits-the-global-threshold",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 50000\n\n[[models]]\nname = "scratch"\nprovider = "llamacpp"\n',
            )
        ],
    },
    {
        "name": "models-entry-keeps-its-own-threshold",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 50000\n\n[[models]]\nname = "scratch"\nprovider = "llamacpp"\nauto_compact_threshold = 4096\n',
            )
        ],
    },
    {
        "name": "models-unknown-active-model-falls-back",
        "layers": [("user", 'active_model = "not-configured"\n')],
    },
    {
        "name": "models-active-model-selects-an-added-entry",
        "layers": [
            (
                "user",
                'active_model = "scratch"\n\n[[models]]\nname = "scratch-latest"\nprovider = "llamacpp"\nalias = "scratch"\nsupports_images = true\n',
            )
        ],
    },
    {
        "name": "models-two-layers-deep-merge-one-entry",
        "layers": [
            ("user", '[[models]]\nalias = "local"\ntemperature = 0.4\nthinking = "low"\n'),
            ("project", '[[models]]\nalias = "local"\ntemperature = 0.8\n'),
        ],
    },
]


async def validated_models(
    reference: Path, layers: list[tuple[str, str]]
) -> dict[str, Any]:
    sys.path.insert(0, str(reference))
    from vibe.core.config.builder import ConfigBuilder
    from vibe.core.config.layers.default import DefaultConfigLayer
    from vibe.core.config.layers.overrides import OverridesLayer
    from vibe.core.config.vibe_schema import VibeConfigSchema

    builder = ConfigBuilder(
        VibeConfigSchema, validation_context={"require_api_key": False}
    )
    builder.add_layer(DefaultConfigLayer(schema=VibeConfigSchema))
    for name, document in layers:
        builder.add_layer(OverridesLayer(data=tomllib.loads(document), name=name))
    config = await builder.build()
    return {
        "activeModel": config.active_model,
        "models": {
            alias: model.model_dump(mode="json")
            for alias, model in config.models.items()
        },
        # Only the count: the warning text is reference-authored prose and
        # ``NOTICE`` forbids committing it.
        "validationWarnings": len(config.validation_warnings),
    }


def capture_model_scenarios(reference: Path) -> list[dict[str, Any]]:
    sys.path.insert(0, str(reference))
    from vibe.core.config.harness_files import init_harness_files_manager

    # The prompt-id validators reach the harness-files singleton; it resolves
    # the builtin prompts shipped inside the checkout, so the capture stays
    # machine-independent.
    init_harness_files_manager()

    async def run() -> list[dict[str, Any]]:
        captured: list[dict[str, Any]] = []
        for scenario in MODEL_SCENARIOS:
            result = await validated_models(reference, scenario["layers"])
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


def capture_defaults(reference: Path) -> dict[str, Any]:
    """The document ``create_default_config`` ships, minus discovered tools."""
    sys.path.insert(0, str(reference))
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
        "defaults": capture_defaults(reference),
        "scenarios": capture_scenarios(reference),
        "modelScenarios": capture_model_scenarios(reference),
        "mcp": capture_mcp(reference),
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
        f"({len(corpus['fields'])} fields, "
        f"{len(corpus['defaults']['document'])} defaults, "
        f"{len(corpus['scenarios'])} scenarios, "
        f"{len(corpus['modelScenarios'])} model scenarios, "
        f"{len(corpus['mcp']['entries'])} MCP entries)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
