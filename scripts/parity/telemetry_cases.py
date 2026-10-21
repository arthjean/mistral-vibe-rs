"""The cases ``telemetry.py`` drives the pinned reference's telemetry and
observability over.

A case names the configuration document a credential is resolved from, the
metadata, exporter and tracing setups, the provider names, the span attribute
sets the redaction policy sees, and the log lines, files and environments the
logging code is driven over. Every value here is authored for the corpus.
"""

from __future__ import annotations

from typing import Any


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
        "id": "mistral-on-a-regional-host",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.eu.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
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
    {"case": "derived-from-a-regional-host", "endpoint": "",
     "configuration": "mistral-on-a-regional-host"},
    {"case": "derived-from-a-base-without-a-version",
     "endpoint": "", "configuration": "mistral-base-without-a-version-segment"},
    {"case": "derived-without-a-resolvable-key", "endpoint": "",
     "configuration": "mistral-without-a-resolvable-key"},
    {"case": "no-mistral-provider", "endpoint": "", "configuration": "third-party-only"},
    {"case": "no-provider-at-all", "endpoint": "", "configuration": None},
]


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


LOG_MESSAGES = (
    "oracle plain message",
    "oracle message with a backslash \\ inside",
    "oracle message\nwith a newline",
    "oracle message with \\n already written out",
    "oracle message with a trailing backslash \\",
    "",
)


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
