"""The layer stacks and documents ``config_surface.py`` composes through the
pinned reference.

Every document, variable, URL and name here is authored for the corpus; none is
read from the reference. ``config_surface.py`` records what the reference
merges, validates and decodes for each.
"""

from __future__ import annotations

from typing import Any

# --------------------------------------------------------------------------
# Merge scenarios
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
        "name": "merge-session-logging-keeps-the-keys-a-higher-table-omits",
        "layers": [
            ("user", '[session_logging]\nenabled = false\nsession_prefix = "lower"\n'),
            ("project", '[session_logging]\nsession_prefix = "higher"\n'),
        ],
    },
    {
        "name": "merge-compaction-model-key-by-key",
        "layers": [
            ("defaults", '[[providers]]\nname = "mistral"\napi_base = "https://api.example.test/v1"\n'),
            (
                "user",
                '[compaction_model]\nname = "devstral-small-latest"\nprovider = "mistral"\ntemperature = 0.3\n',
            ),
            ("project", "[compaction_model]\ntemperature = 0.9\n"),
        ],
    },
    {
        "name": "merge-vision-model-key-by-key",
        "layers": [
            ("defaults", '[[providers]]\nname = "mistral"\napi_base = "https://api.example.test/v1"\n'),
            (
                "user",
                '[vision_model]\nname = "pixtral-large-latest"\nprovider = "mistral"\nsupports_images = true\n',
            ),
            ("project", '[vision_model]\nalias = "seer"\n'),
        ],
    },
    {
        "name": "merge-experiments-key-by-key",
        "layers": [
            ("defaults", '[experiments]\nenable = true\napi_host = "https://lower.example.test/"\n'),
            ("user", "[experiments]\nenable = false\n"),
        ],
    },
    {
        "name": "merge-replaces-a-nested-value-whole",
        "layers": [
            ("user", '[experiments]\nenable = true\nclient_key = "lower"\n'),
            ("project", '[experiments]\nclient_key = "higher"\n'),
            ("overrides", "[experiments]\nenable = false\n"),
        ],
    },
    {
        "name": "replace-allowed-models",
        "layers": [
            ("user", 'allowed_models = ["mistral-*", "local"]\n'),
            ("project", 'allowed_models = ["re:^devstral.*"]\n'),
        ],
    },
    {
        "name": "replace-show-greeting",
        "layers": [
            ("defaults", "show_greeting = true\n"),
            ("user", "show_greeting = false\n"),
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


# --------------------------------------------------------------------------
# Layer stack scenarios
# --------------------------------------------------------------------------

#: Whole homes and checkouts composed through the reference's own
#: `build_default_orchestrator`: the user file under the vibe home, project
#: files under directories of the root, the folders the trust store trusts, the
#: working directory, and implicit writes made afterwards. Every document is
#: authored for this corpus.
STACK_SCENARIOS: list[dict[str, Any]] = [
    {
        "name": "stack-user-file-alone",
        "user": 'theme = "nord"\ndisabled_tools = ["bash"]\n',
        "projects": {},
        "trusted": [],
        "cwd": "work",
    },
    {
        "name": "stack-trusted-project-inherits-the-user-file",
        "user": 'theme = "nord"\ndefault_agent = "plan"\ndisabled_tools = ["bash"]\n\n[session_logging]\nenabled = false\nsession_prefix = "user"\n',
        "projects": {
            "work": 'theme = "dracula"\ndisabled_tools = ["edit"]\n\n[session_logging]\nsession_prefix = "project"\n'
        },
        "trusted": ["work"],
        "cwd": "work",
    },
    {
        "name": "stack-untrusted-project-is-ignored",
        "user": 'theme = "nord"\n',
        "projects": {"work": 'theme = "dracula"\n'},
        "trusted": [],
        "cwd": "work",
    },
    {
        "name": "stack-project-found-above-the-working-directory",
        "user": 'theme = "nord"\n',
        "projects": {"work": 'theme = "gruvbox"\n'},
        "trusted": ["work"],
        "cwd": "work/a/b",
    },
    {
        "name": "stack-nearest-project-file-wins",
        "user": 'theme = "nord"\n',
        "projects": {"work": 'theme = "gruvbox"\n', "work/a": 'theme = "monokai"\n'},
        "trusted": ["work"],
        "cwd": "work/a/b",
    },
    {
        "name": "stack-project-without-a-user-file",
        "user": None,
        "projects": {"work": 'theme = "dracula"\ndisabled_tools = ["edit"]\n'},
        "trusted": ["work"],
        "cwd": "work",
    },
]

#: Implicit writes made through the reference orchestrator after the stack
#: loads, each recording every file afterwards. A trusted project file sits
#: beside the user file, so the routing is observable.
WRITE_SCENARIOS: list[dict[str, Any]] = [
    {
        "name": "write-implicit-goes-to-the-user-file",
        "user": 'theme = "nord"\n',
        "projects": {"work": 'theme = "dracula"\n'},
        "trusted": ["work"],
        "cwd": "work",
        "writes": [{"path": "default_agent", "value": "plan"}],
    },
    {
        "name": "write-creates-the-user-file",
        "user": None,
        "projects": {"work": 'theme = "dracula"\n'},
        "trusted": ["work"],
        "cwd": "work",
        "writes": [{"path": "theme", "value": "nord"}],
    },
    {
        "name": "write-keeps-the-file-order-and-appends-new-keys",
        "user": 'theme = "nord"\nauto_compact_threshold = 90000\ndisabled_tools = ["bash", "edit"]\n\n[session_logging]\nenabled = false\n\n[tools.bash]\ntimeout = 30\n',
        "projects": {},
        "trusted": [],
        "cwd": "work",
        "writes": [
            {"path": "active_model", "value": "local"},
            {"path": "theme", "value": "dracula"},
        ],
    },
]


# --------------------------------------------------------------------------
# Environment, agent profile, dotenv and sign-in rewrite scenarios
# --------------------------------------------------------------------------

#: `VIBE_*` variables read by the reference's own `EnvironmentLayer`, one set
#: per case. Every name and value is authored for this corpus.
ENVIRONMENT_SCENARIOS: list[dict[str, Any]] = [
    {"name": "env-typed-scalars", "variables": {
        "VIBE_ENABLE_TELEMETRY": "false",
        "VIBE_THEME": "1",
        "VIBE_AUTO_COMPACT_THRESHOLD": "90000",
        "VIBE_API_TIMEOUT": "12.5",
        "VIBE_DISPLAYED_WORKDIR": "",
    }},
    {"name": "env-json-list", "variables": {"VIBE_DISABLED_TOOLS": '["bash", "edit"]'}},
    {"name": "env-prefix-ignores-case", "variables": {
        "vibe_theme": "nord",
        "Vibe_Default_Agent": "plan",
    }},
    {"name": "env-undeclared-names-are-ignored", "variables": {
        "VIBE_FUTURE_KEY": "1",
        "VIBE_HOME_ELSEWHERE": "/nowhere",
        "VIBE_NESTED__WINNER": "environment",
    }},
    {"name": "env-nested-declared-keys", "variables": {
        "VIBE_SESSION_LOGGING__ENABLED": "false",
        "VIBE_SESSION_LOGGING__SESSION_PREFIX": "123",
        "VIBE_TOOLS__BASH__TIMEOUT": "9",
    }},
    {"name": "env-nested-undeclared-key-is-dropped", "variables": {
        "VIBE_PROJECT_CONTEXT__MAX_FILES": "7",
    }},
    {"name": "env-boolean-vocabulary", "variables": {
        "VIBE_ENABLE_OTEL": "yes",
        "VIBE_ENABLE_NOTIFICATIONS": "0",
        "VIBE_AUTOCOPY_TO_CLIPBOARD": "Off",
    }},
]

#: Overrides an agent profile carries, read by the reference's own
#: `AgentProfileLayer`.
AGENT_PROFILE_SCENARIOS: list[dict[str, Any]] = [
    {"name": "profile-plain-overrides", "overrides": {"active_model": "local", "disabled_tools": ["bash"]}},
    {"name": "profile-protected-fields-are-stripped", "overrides": {
        "theme": "nord",
        "vibe_base_url": "https://attacker.example.test",
        "console_base_url": "https://attacker.example.test",
        "vibe_code_sessions_base_url": "https://attacker.example.test",
    }},
]

#: Global dotenv files read by the reference's own `load_dotenv_values` into a
#: seeded environment. Names are unique to this corpus so the capturing
#: process environment cannot answer an expansion.
DOTENV_SCENARIOS: list[dict[str, Any]] = [
    {
        "name": "dotenv-python-dotenv-grammar",
        "file": (
            "# a comment\n"
            "VIBE_PARITY_DOTENV_A=one # trailing\n"
            'VIBE_PARITY_DOTENV_B="two\nlines"\n'
            "VIBE_PARITY_DOTENV_C=${VIBE_PARITY_DOTENV_A}-x\n"
            "export VIBE_PARITY_DOTENV_D = spaced\n"
            "VIBE_PARITY_DOTENV_E\n"
            "VIBE_PARITY_DOTENV_F='lit ${VIBE_PARITY_DOTENV_A}'\n"
            'VIBE_PARITY_DOTENV_G="esc \\t tab \\" quote"\n'
            "VIBE_PARITY_DOTENV_H=${VIBE_PARITY_DOTENV_UNSET:-fallback}\n"
            "VIBE_PARITY_DOTENV_I=a#not-a-comment\n"
            "'VIBE_PARITY_DOTENV_J'=quoted key\n"
            "malformed line without a separator\n"
            "VIBE_PARITY_DOTENV_K=\n"
            "VIBE_PARITY_DOTENV_A=redefined\n"
        ),
        "environ": {},
    },
    {
        "name": "dotenv-a-non-empty-process-value-wins",
        "file": (
            "VIBE_PARITY_DOTENV_SET=from-file\n"
            "VIBE_PARITY_DOTENV_EMPTY=from-file\n"
            "VIBE_PARITY_DOTENV_NEW=from-file\n"
        ),
        "environ": {"VIBE_PARITY_DOTENV_SET": "from-process", "VIBE_PARITY_DOTENV_EMPTY": ""},
    },
]

#: Server-supplied URLs checked against a configured base with the origin
#: rewrite on, as `browser_auth_allow_origin_rewrite` asks.
ORIGIN_REWRITE_CASES: list[tuple[str, str]] = [
    ("https://console.example/api/vibe/sign-in/1?state=x#frag", "https://connector.internal:8443/api"),
    ("https://connector.internal:8443/api/poll", "https://connector.internal:8443/api"),
    ("http://console.example:9000/api/poll", "https://connector.internal/api"),
    ("https://console.example/elsewhere", "https://connector.internal:8443/api"),
    ("https://console.example/api/../elsewhere", "https://connector.internal/api"),
    ("https://user:secret@console.example/api/x", "https://connector.internal/api"),
    ("not a url", "https://connector.internal/api"),
]

#: Documents the reference writes back through `tomli_w`, as
#: `_write_toml_snapshot` does with its default options. Each is authored for
#: this corpus to reach one layout rule.
ENCODING_CASES: list[str] = [
    "",
    'theme = "nord"\nauto_compact_threshold = 90000\nenable_telemetry = false\n',
    'disabled_tools = ["bash", "edit"]\nenabled_tools = []\n',
    "[session_logging]\nenabled = false\n",
    "[tools.bash]\ntimeout = 30\n\n[tools.edit]\nmax = 2\n",
    "[project_context]\n",
    'nested = [[1, 2], ["a"]]\n',
    'mixed = [1, "two", { three = 3 }]\n',
    '[[models]]\nname = "local"\nprovider = "llamacpp"\nalias = "local"\n\n[[models]]\nname = "other"\nprovider = "mistral"\n',
    '[[models]]\nname = "a-model-with-a-name-long-enough"\nprovider = "a-provider-long-enough"\nalias = "an-alias-long-enough-to-overflow"\n',
    '[[mcp_servers]]\nname = "server"\ntransport = "stdio"\nargs = ["--flag"]\n',
    'empty_table = {}\nlist_of_empty = [{}]\n',
    '"key with space" = 1\n"dotted.key" = 2\n"" = 3\n"é" = 4\n',
    'text = "quote \\" backslash \\\\ tab \\t newline \\n bell \\u0007 del \\u007f é"\n',
    "floats = [0.7, 1.0, 200000.0, 1e16, 1.5e-5, 0.0001, -0.0, 123456789.125, 1e300, inf, -inf]\n",
    "big = 9223372036854775807\nnegative = -12\n",
    "stamp = 1979-05-27T07:32:00Z\nlocal = 1979-05-27T07:32:00.5\nday = 1979-05-27\nclock = 07:32:00\nshifted = 1979-05-27T00:32:00.999999-07:00\n",
    "[a]\nx = 1\n\n[a.b]\ny = 2\n\n[a.b.c]\nz = 3\n\n[d.e]\nw = 4\n",
    'top = 1\n\n[[servers]]\nname = "one"\n\n[servers.env]\nKEY = "value"\n',
]


# --------------------------------------------------------------------------
# Model scenarios
# --------------------------------------------------------------------------

#: Layer stacks replayed on top of the real default layer, recording what the
#: reference *validates* rather than what it merges. They exist because the
#: model rules (sparse completion, the global compaction threshold, the unknown
#: active-model fallback) only run once the merged document is validated.
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
    # The alias rule the reference binds to every model class. An entry that
    # declares none borrows its name, which is what keys it and what the active
    # alias then has to name.
    {
        "name": "models-entry-without-an-alias-borrows-its-name",
        "layers": [
            (
                "user",
                'active_model = "scratch"\n\n[[models]]\nname = "scratch"\nprovider = "mistral"\n',
            )
        ],
    },
    {
        "name": "models-compaction-model-without-an-alias-borrows-its-name",
        "layers": [
            (
                "user",
                '[compaction_model]\nname = "devstral-small-latest"\nprovider = "mistral"\n',
            )
        ],
    },
    # The allowlist narrows what may be selected without removing anything
    # from `models`; a pattern admitting nothing warns and admits everything.
    {
        "name": "models-allowed-models-glob-narrows-the-available-set",
        "layers": [("user", 'allowed_models = ["mistral-*"]\n')],
    },
    {
        "name": "models-allowed-models-regex-is-case-insensitive",
        "layers": [("user", 'allowed_models = ["re:LOC.L"]\n')],
    },
    {
        "name": "models-allowed-models-matching-nothing-warns-and-admits-all",
        "layers": [("user", 'allowed_models = ["nothing-*", "  ", "local"]\n')],
    },
    {
        "name": "models-allowed-models-excluding-the-pin-falls-back",
        "layers": [
            ("user", 'active_model = "local"\nallowed_models = ["mistral-*"]\n')
        ],
    },
    {
        "name": "models-allowed-models-excluding-the-default",
        "layers": [("user", 'allowed_models = ["local"]\n')],
    },
    # The routed fields an experiment writes, in the JSON text form the
    # experiments layer carries and in the list form a file carries.
    {
        "name": "models-routed-extra-models-from-json-text",
        "layers": [
            (
                "user",
                "routed_extra_models = '"
                '[{"name": "extra-latest", "provider": "mistral", "alias": "extra", '
                '"input_price": "0.4"}, {"name": "broken"}, '
                '{"name": "nameless", "provider": "mistral", "alias": ""}]'
                "'\n",
            )
        ],
    },
    {
        "name": "models-routed-extra-models-keep-what-the-operator-wrote",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 64000\n\n[[models]]\nalias = "local"\ntemperature = 0.65\n\n'
                '[[routed_extra_models]]\nname = "local-routed"\nprovider = "llamacpp"\nalias = "local"\n'
                'supports_images = true\n\n[[routed_extra_models]]\nname = "fresh"\nprovider = "mistral"\n',
            )
        ],
    },
    {
        "name": "models-routed-extra-models-not-a-list",
        "layers": [("user", 'routed_extra_models = "not json"\n')],
    },
    {
        "name": "models-routed-default-reaches-a-pinned-installation",
        "layers": [
            (
                "user",
                'active_model = "local"\nrouted_default_model = "routed"\n'
                "routed_model_config = '"
                '{"name": "vibe-routed", "provider": "mistral", "alias": "routed"}'
                "'\n",
            )
        ],
    },
    {
        "name": "models-routed-definition-keeps-the-operator-overrides",
        "layers": [
            (
                "user",
                'routed_default_model = "local"\n'
                "routed_model_config = '"
                '{"name": "local-routed", "provider": "llamacpp", "alias": "local", '
                '"supports_images": true, "temperature": 0.1}'
                "'\n\n"
                '[[models]]\nalias = "local"\ntemperature = 0.75\n',
            )
        ],
    },
    {
        "name": "models-unknown-active-model-resolves-to-the-routed-default",
        "layers": [
            (
                "user",
                'active_model = "gone"\nrouted_default_model = "routed"\n'
                "routed_model_config = '"
                '{"name": "vibe-routed", "provider": "mistral", "alias": "routed"}'
                "'\n",
            )
        ],
    },
    # The vision model is a `ModelConfig` validated on its own terms.
    {
        "name": "models-vision-model-is-completed-like-a-model",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 70000\n\n[vision_model]\nname = "pixtral-large-latest"\n'
                'provider = "llamacpp"\nsupports_images = true\n',
            )
        ],
    },
    {
        "name": "models-vision-model-that-cannot-see-is-refused",
        "layers": [
            ("user", '[vision_model]\nname = "blind"\nprovider = "mistral"\n')
        ],
    },
    {
        "name": "models-vision-model-on-an-unknown-provider-is-refused",
        "layers": [
            (
                "user",
                '[vision_model]\nname = "pixtral"\nprovider = "nowhere"\nsupports_images = true\n',
            )
        ],
    },
    {
        "name": "models-compaction-model-ignores-the-global-threshold",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 70000\n\n[compaction_model]\nname = "devstral-small-latest"\n'
                'provider = "mistral"\n',
            )
        ],
    },
    # A threshold the organization's managed layer sets reaches every model,
    # a model's own threshold included.
    {
        "name": "models-admin-threshold-overrides-every-model",
        "layers": [
            (
                "user",
                '[[models]]\nname = "scratch"\nprovider = "llamacpp"\nauto_compact_threshold = 4096\n',
            ),
            ("admin", "auto_compact_threshold = 32000\n"),
        ],
    },
    {
        "name": "models-user-threshold-keeps-a-model-threshold",
        "layers": [
            (
                "user",
                '[[models]]\nname = "scratch"\nprovider = "llamacpp"\nauto_compact_threshold = 4096\n',
            ),
            ("project", "auto_compact_threshold = 32000\n"),
        ],
    },
    {
        "name": "models-compaction-model-keeps-the-alias-it-declares",
        "layers": [
            (
                "user",
                '[compaction_model]\nname = "devstral-small-latest"\nprovider = "mistral"\n'
                'alias = "compactor"\ntemperature = 0.7\n',
            )
        ],
    },
    # A model's threshold resolves against its context window: no layer
    # setting one leaves 80% of the window, a cap the window cannot reach is
    # ignored, and 0 stays a cap meaning "never compact".
    {
        "name": "models-window-derives-the-threshold",
        "layers": [
            (
                "user",
                '[[models]]\nname = "windowed"\nprovider = "mistral"\nmax_context_length = 128000\n\n'
                '[[models]]\nname = "odd-window"\nprovider = "mistral"\nmax_context_length = 1001\n',
            )
        ],
    },
    {
        "name": "models-cap-the-window-cannot-reach-is-ignored",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 150000\n\n'
                '[[models]]\nname = "capped-above"\nprovider = "mistral"\nmax_context_length = 128000\n'
                'auto_compact_threshold = 300000\n\n'
                '[[models]]\nname = "capped-at"\nprovider = "mistral"\nmax_context_length = 128000\n'
                'auto_compact_threshold = 128000\n\n'
                '[[models]]\nname = "capped-below"\nprovider = "mistral"\nmax_context_length = 128000\n'
                'auto_compact_threshold = 90000\n\n'
                '[[models]]\nname = "wide"\nprovider = "mistral"\nmax_context_length = 400000\n\n'
                '[[models]]\nname = "never"\nprovider = "mistral"\nmax_context_length = 128000\n'
                'auto_compact_threshold = 0\n',
            )
        ],
    },
    {
        "name": "models-global-cap-at-the-window-falls-back-to-the-ratio",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 128000\n\n'
                '[[models]]\nname = "windowed"\nprovider = "mistral"\nmax_context_length = 128000\n',
            )
        ],
    },
    {
        "name": "models-a-hand-written-unset-threshold-is-unset",
        "layers": [
            (
                "user",
                'auto_compact_threshold = -1\n\n'
                '[[models]]\nname = "windowed"\nprovider = "mistral"\nmax_context_length = 64000\n'
                'auto_compact_threshold = -1\n\n'
                '[[models]]\nname = "plain"\nprovider = "mistral"\nauto_compact_threshold = -1\n',
            )
        ],
    },
    {
        "name": "models-global-threshold-of-zero-disables-every-model",
        "layers": [
            (
                "project",
                'auto_compact_threshold = 0\n\n'
                '[[models]]\nname = "windowed"\nprovider = "mistral"\nmax_context_length = 64000\n',
            )
        ],
    },
    {
        "name": "models-admin-threshold-overrides-a-window",
        "layers": [
            (
                "user",
                '[[models]]\nname = "windowed"\nprovider = "mistral"\nmax_context_length = 64000\n',
            ),
            ("admin", "auto_compact_threshold = 96000\n"),
        ],
    },
    # The settings screen edits the active model's own threshold and window
    # where a layer set them, and the global threshold where an administrator
    # enforces it.
    {
        "name": "models-active-model-threshold-and-window-are-its-own-rows",
        "layers": [
            (
                "user",
                'active_model = "scratch"\nauto_compact_threshold = 50000\n\n'
                '[[models]]\nname = "scratch"\nprovider = "llamacpp"\nmax_context_length = 32000\n'
                'auto_compact_threshold = 4096\n',
            ),
            ("project", '[[models]]\nalias = "scratch"\nmax_context_length = 48000\n'),
        ],
    },
    {
        "name": "models-admin-threshold-keeps-the-global-row",
        "layers": [
            (
                "user",
                'active_model = "local"\n\n[[models]]\nalias = "local"\nauto_compact_threshold = 4096\n',
            ),
            ("admin", "auto_compact_threshold = 24000\n"),
        ],
    },
    # The routed definition is a field rather than an entry of `models`, so it
    # keeps the unset threshold while its injected copy resolves.
    {
        "name": "models-routed-definition-keeps-the-unset-threshold",
        "layers": [
            (
                "user",
                'auto_compact_threshold = 50000\nrouted_default_model = "routed"\n'
                "routed_model_config = '"
                '{"name": "vibe-routed", "provider": "mistral", "alias": "routed", '
                '"max_context_length": 32000}'
                "'\n",
            )
        ],
    },
    # The allowlist matches model names; an administrator's allowlist fails
    # closed where the operator's degrades to every model.
    {
        "name": "models-allowed-models-match-the-model-name",
        "layers": [
            ("user", 'active_model = "local"\nallowed_models = ["devstral"]\n')
        ],
    },
    {
        "name": "models-admin-allowlist-narrows-by-name",
        "layers": [
            ("user", 'active_model = "mistral-medium-3.5"\n'),
            ("admin", 'allowed_models = ["DEVSTRAL"]\n'),
        ],
    },
    {
        "name": "models-admin-allowlist-matching-nothing-is-refused",
        "layers": [("admin", 'allowed_models = ["nothing-*"]\n')],
    },
    {
        "name": "models-admin-allowlist-of-blanks-is-refused",
        "layers": [("admin", 'allowed_models = ["  "]\n')],
    },
    # Compaction rides the active model's connection, so a compaction model on
    # another provider warns and the active model compacts instead.
    {
        "name": "models-compaction-model-on-another-provider-warns",
        "layers": [
            (
                "user",
                '[compaction_model]\nname = "devstral-small"\nprovider = "llamacpp"\n',
            )
        ],
    },
    {
        "name": "models-compaction-model-follows-the-active-provider",
        "layers": [
            (
                "user",
                'active_model = "local"\n\n[compaction_model]\nname = "devstral-small"\n'
                'provider = "llamacpp"\n',
            )
        ],
    },
    {
        "name": "models-compaction-model-on-an-unknown-provider-is-refused",
        "layers": [
            (
                "user",
                '[compaction_model]\nname = "devstral-small"\nprovider = "nowhere"\n',
            )
        ],
    },
    {
        "name": "models-compaction-prompt-naming-nothing-is-refused",
        "layers": [("user", 'compaction_prompt_id = "names-no-prompt"\n')],
    },
    # `[utility_models]` names a model alias, or `active` for the session's
    # model; an alias the allowlist excludes or no model carries warns and
    # leaves the feature on automatic selection.
    {
        "name": "models-utility-models-resolve-an-alias-and-the-active-selector",
        "layers": [
            (
                "user",
                '[utility_models]\ntitle = " local "\nsmart_approve = "active"\n',
            )
        ],
    },
    {
        "name": "models-utility-model-naming-nothing-warns",
        "layers": [("user", '[utility_models]\ntitle = "ghost"\n')],
    },
    {
        "name": "models-utility-model-the-allowlist-excludes-warns",
        "layers": [
            (
                "user",
                'allowed_models = ["mistral-vibe-cli-latest"]\n\n[utility_models]\n'
                'title = "local"\nsmart_approve = "mistral-medium-3.5"\n',
            )
        ],
    },
    {
        "name": "models-a-model-aliased-active-wins-over-the-selector",
        "layers": [
            (
                "user",
                '[[models]]\nname = "devstral"\nprovider = "llamacpp"\nalias = "active"\n\n'
                '[utility_models]\nsmart_approve = "active"\n',
            )
        ],
    },
]
