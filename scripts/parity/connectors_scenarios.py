"""The scenarios ``connectors.py`` serves an app server over, the reference's or
this port's.

A scenario names the configuration its session starts with, the connector
bootstrap payloads, authorization URLs and gateway answers the stand-in plays,
what the model answers, and the steps the client sends. Every value here is
authored for the corpus.
"""

from __future__ import annotations

from typing import Any


API_KEY = "oracle-key"


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def send(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    step: dict[str, Any] = {"send": {"method": method, "params": params}}
    if pick:
        step["pick"] = pick
    return step


def on(method: str, pick: list[str] | None = None, **params: Any) -> dict[str, Any]:
    """A method addressed to the first session."""

    return send(method, pick, sessionId="$S1", **params)


def runtime() -> dict[str, Any]:
    return on("runtime/read")


def call(name: str, arguments: dict[str, Any], identifier: str) -> dict[str, Any]:
    return {"id": identifier, "name": name, "arguments": arguments}


def tool(name: str, description: str | None = None, schema: Any = None) -> dict[str, Any]:
    entry: dict[str, Any] = {"name": name}
    if description is not None:
        entry["description"] = description
    if schema is not None:
        entry["inputSchema"] = schema
    return entry


SEARCH_SCHEMA = {
    "type": "object",
    "properties": {"query": {"type": "string", "description": "What to look for"}},
    "required": ["query"],
}

DRIVE = {
    "id": "c-drive", "name": "Drive", "display_name": "Google Drive", "protocol": "mcp",
    "status": {"is_ready": True},
    "tools": [tool("search", "Search files", SEARCH_SCHEMA), tool("list")],
}
MAIL = {
    "id": "c-mail", "name": "Mail box", "protocol": "mcp", "status": {"is_ready": False},
    "auth_action": {"type": "oauth"}, "tools": [tool("send", "Send a mail")],
}
SHEETS = {
    "id": "c-sheets", "name": "Sheets", "protocol": "mcp", "status": {"is_ready": False},
    "auth_action": {"type": "credentials_setup"},
    "bootstrap_errors": ["missing_credentials: set up the sheet access"],
    "tools": [tool("read", "Read a sheet")],
}
DOCS = {
    "id": "c-docs", "name": "Docs", "protocol": "mcp", "status": {"is_ready": True},
    "tools": [tool("fetch", "Fetch a document", SEARCH_SCHEMA)],
}
WEB = {
    "id": "web_search", "name": "web_search", "display_name": "Web search", "protocol": "mcp",
    "status": {"is_ready": True}, "tools": [tool("web_search", "Search the web", SEARCH_SCHEMA)],
}
CATALOG = {"connectors": [DRIVE, MAIL]}
WIDE = {"connectors": [DRIVE, MAIL, SHEETS, DOCS, WEB]}

#: The configuration that opts the two catalog connectors in.
OPT_IN = '[[connectors]]\nname = "Drive"\n\n[[connectors]]\nname = "Mail_box"\n'
#: Every connector of the wide catalog opted in.
OPT_IN_WIDE = OPT_IN + (
    '\n[[connectors]]\nname = "Sheets"\n\n[[connectors]]\nname = "Docs"\n'
    '\n[[connectors]]\nname = "web_search"\n'
)

#: Every shape the reference resolves, isolates or drops, in one payload.
SHAPES = {"connectors": [
    DRIVE,
    WEB,
    # Two names normalizing to one alias: the second by id takes a suffix.
    {"id": "c-my-2", "name": "My_Drive", "status": {"is_ready": True}, "tools": [tool("a")]},
    {"id": "c-my-1", "name": "My Drive", "status": {"is_ready": True}, "tools": [tool("b")]},
    # A name that normalizes to nothing, and one with no name at all.
    {"id": "c-bang", "name": "!!!", "status": {"is_ready": True}},
    {"id": "c-nameless", "display_name": "Nameless", "status": {"is_ready": True}},
    {"id": "c-accents", "name": "Été / Calendar!", "status": {"is_ready": True}},
    # No id, a blank id, and an id two connectors share: all dropped.
    {"name": "Orphan", "status": {"is_ready": True}},
    {"id": "   ", "name": "Blank", "status": {"is_ready": True}},
    {"id": "c-dup", "name": "Dup one"},
    {"id": "c-dup", "name": "Dup two"},
    # Every auth action, an unknown one, and a status that is missing.
    {"id": "c-oauth", "name": "OAuth", "status": {"is_ready": False}, "auth_action": {"type": "oauth"}},
    {"id": "c-setup", "name": "Setup", "status": {"is_ready": False},
     "auth_action": {"type": "credentials_setup"}},
    {"id": "c-weird", "name": "Weird", "status": {"is_ready": False}, "auth_action": {"type": "magic"}},
    {"id": "c-ready-weird", "name": "Ready weird", "status": {"is_ready": True},
     "auth_action": {"type": "magic"}},
    {"id": "c-nostatus", "name": "No status"},
    # Diagnostics: codes, prose, non-strings, blanks and more than three.
    {"id": "c-diag", "name": "Diag", "status": {"is_ready": True},
     "bootstrap_errors": ["token_expired: gone", "Plain words here", 42, "   ", "rate_limited",
                          "Connector bootstrap issue: quota", "Connector failed to bootstrap."]},
    {"id": "c-diag-text", "name": "Diag text", "status": {"is_ready": True},
     "bootstrap_errors": "upstream_down: try later"},
    # Tools: unnamed, blank, malformed, oversized, duplicated.
    {"id": "c-tools", "name": "Tools", "status": {"is_ready": True}, "tools": [
        tool("zeta", "Last"), tool("alpha"), {"description": "no name"}, tool("   "),
        tool("big", "Too big", {"type": "object", "properties": {"x": {"description": "y" * 70000}}}),
        "not a tool", tool("nulls", None, None),
    ]},
    {"id": "c-dupe-tools", "name": "Dupe tools", "status": {"is_ready": True},
     "tools": [tool("same"), tool("same")]},
    {"id": "c-bad-tools", "name": "Bad tools", "tools": "nope"},
    {"id": "c-many", "name": "Many", "status": {"is_ready": True},
     "tools": [tool(f"t{index:03}") for index in range(129)]},
    "not a connector",
]}

#: The configuration of a session whose model runs on a provider that is not
#: Mistral's, while the Mistral provider has no key to reach connectors with.
NO_MISTRAL_KEY_BASE = (
    'active_model = "standin"\n'
    "enable_telemetry = false\n"
    "enable_update_checks = false\n"
    "\n[[models]]\n"
    'name = "standin-model"\n'
    'provider = "standin"\n'
    'alias = "standin"\n'
    "\n[[providers]]\n"
    'name = "mistral"\n'
    'api_base = "$BACKEND/v1"\n'
    'api_key_env_var = "ORACLE_MISSING_KEY"\n'
    'backend = "mistral"\n'
    "\n[[providers]]\n"
    'name = "standin"\n'
    'api_base = "$BACKEND/v1"\n'
    'api_key_env_var = "ORACLE_STANDIN_KEY"\n'
    'backend = "generic"\n'
)

IDENTITY = {
    "id": "user-1", "email": "user@example.com",
    "workspace": {"id": "ws-1", "name": "Workspace"},
    "organization": {"id": "org-1", "name": "Org"},
}


def fresh_entry(payload: dict[str, Any], age: int = 60) -> dict[str, Any]:
    return {"format": 2, "stored_at": f"$NOW-{age}", "payload": payload}


def search(identifier: str, name: str = "connector_Drive_search") -> dict[str, Any]:
    return {"toolCalls": [call(name, {"query": "report"}, identifier)]}


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- catalog ---------------------------------------------------------
        {
            "name": "catalog/sessionless-refresh",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                send("connector_catalog/read"),
                send("connector_catalog/refresh"),
                send("connector_catalog/read"),
                {"cacheFile": True},
            ],
        },
        {
            "name": "catalog/shapes",
            "connectors": {"bootstrap": [SHAPES]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "catalog/selections",
            "config": OPT_IN + '\n[[connectors]]\nname = "Ghost"\ndisabled = true\ndisabled_tools = ["x"]\n',
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"),
                      send("connector_catalog/read")],
        },
        {
            "name": "catalog/empty",
            "connectors": {"bootstrap": [{"connectors": []}, {"connectors": None}, {}]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/read"),
                      send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "catalog/malformed",
            "connectors": {"bootstrap": [[1, 2], {"connectors": "nope"}, "text", {"raw": "not json", "status": 200}]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "catalog/http-errors",
            "connectors": {"bootstrap": [
                {"status": 500, "body": {"detail": "boom"}},
                {"status": 401, "body": {"detail": "who"}},
                {"status": 404, "body": {"detail": "none"}},
                CATALOG,
            ]},
            "steps": [send("connector_catalog/refresh"), send("connector_catalog/refresh"),
                      send("connector_catalog/refresh"), send("connector_catalog/read"),
                      send("connector_catalog/refresh"), send("connector_catalog/read")],
        },
        {
            "name": "catalog/disabled",
            "config": "enable_connectors = false\n" + OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"),
                      {"start": {}}, on("connector_catalog/read"), on("connectors/read"),
                      runtime(), {"turn": "Hello"}],
        },
        {
            "name": "catalog/no-mistral-key",
            "base": NO_MISTRAL_KEY_BASE,
            "env": {"ORACLE_STANDIN_KEY": API_KEY, "MISTRAL_API_KEY": None,
                    # The port's fixture authenticates its turns with this
                    # variable; the reference never reads it.
                    "VIBE_ORACLE_CREDENTIAL": "ORACLE_STANDIN_KEY"},
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"),
                      {"start": {}}, on("connector_catalog/read"), on("connector_catalog/refresh"),
                      on("connectors/read"), runtime()],
        },
        {
            "name": "catalog/manage-url",
            "connectors": {"bootstrap": [CATALOG], "identity": IDENTITY},
            "steps": [send("connector_catalog/read"), {"start": {}}, on("connector_catalog/read")],
        },
        {
            "name": "catalog/manage-url-partial",
            "connectors": {"bootstrap": [CATALOG], "identity": {"id": "user-1"}},
            "steps": [send("connector_catalog/read")],
        },
        # -- cache -----------------------------------------------------------
        {
            "name": "cache/fresh",
            "cache": {"$FP": fresh_entry(CATALOG)},
            "connectors": {"bootstrap": [{"connectors": [DOCS]}]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/read"), {"start": {}},
                      on("connector_catalog/read"), send("connector_catalog/refresh"),
                      send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "cache/legacy-entry",
            "cache": {"$FP": {"stored_at_timestamp": "$NOW-30", "payload": CATALOG}},
            "connectors": {"bootstrap": [{"connectors": [DOCS]}]},
            "steps": [send("connector_catalog/read"), {"cacheFile": True}],
        },
        {
            "name": "cache/stale-and-future",
            "cache": {
                "$FP": fresh_entry(CATALOG, age=601),
                "other-stale": fresh_entry(CATALOG, age=900),
                "other-future": {"format": 2, "stored_at": "$NOW+600", "payload": CATALOG},
                "other-fresh": fresh_entry({"connectors": [DOCS]}),
                "other-legacy": {"stored_at_timestamp": "$NOW-10", "payload": {"connectors": [DOCS]}},
                "other-format": {"format": 3, "stored_at": "$NOW-10", "payload": CATALOG},
                "$RK": {"stored_at_timestamp": "$NOW-10", "payload": CATALOG},
            },
            "connectors": {"bootstrap": [{"connectors": [DOCS]}]},
            "steps": [send("connector_catalog/read"), send("connector_catalog/refresh"), {"cacheFile": True}],
        },
        {
            "name": "cache/diagnostics-roundtrip",
            "cache": {"$FP": fresh_entry({"connectors": [
                {**DOCS, "diagnostics": ["quota_exceeded: slow down"]},
                {**DRIVE, "bootstrap_errors": ["token_expired: gone"]},
            ]})},
            "steps": [send("connector_catalog/read")],
        },
        {
            "name": "cache/corrupt",
            "cache": {"$FP": {"format": 2, "stored_at": "$NOW-10", "payload": {"connectors": "nope"}}},
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [send("connector_catalog/read"), {"start": {}}, runtime(), {"cacheFile": True}],
        },
        # -- publish ---------------------------------------------------------
        {
            "name": "publish/opt-in",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connector_catalog/read"),
                      on("connectors/read"), {"cacheFile": True}],
        },
        {
            "name": "publish/no-entries",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read")],
        },
        {
            "name": "publish/wide",
            "config": OPT_IN_WIDE,
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read"),
                      on("connector_catalog/read")],
        },
        {
            "name": "publish/entry-filters",
            "config": (
                '[[connectors]]\nname = "Drive"\ndisabled_tools = ["list"]\n'
                '\n[[connectors]]\nname = "Docs"\ndisabled = true\n'
                '\n[[connectors]]\nname = "web_search"\ndisabled_tools = ["web_search", "unknown"]\n'
                '\n[[connectors]]\nname = "Absent"\n'
            ),
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read"),
                      on("connector_catalog/read")],
        },
        {
            "name": "publish/global-filters",
            "config": 'disabled_tools = ["connector_Drive_l*"]\n' + OPT_IN_WIDE,
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connector_catalog/read")],
        },
        {
            "name": "publish/enabled-tools",
            "config": 'enabled_tools = ["connector_Docs_*", "grep"]\n' + OPT_IN_WIDE,
            "connectors": {"bootstrap": [WIDE]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connector_catalog/read")],
        },
        {
            "name": "publish/bootstrap-failure",
            "config": OPT_IN,
            "connectors": {"bootstrap": [{"status": 503, "body": {"detail": "down"}}]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, runtime(),
                      on("connector_catalog/read"), on("connectors/read")],
        },
        {
            "name": "publish/bootstrap-recovers",
            "config": OPT_IN,
            "connectors": {"bootstrap": [{"status": 500, "body": {}}, CATALOG]},
            "steps": [{"start": {}}, on("connector_catalog/refresh"), runtime(), {"turn": "Hello"}],
        },
        {
            "name": "publish/shapes",
            "config": "".join(
                f'[[connectors]]\nname = "{alias}"\n\n'
                for alias in ("Drive", "web_search", "My_Drive", "My_Drive_2", "unnamed",
                              "c-nameless", "t____Calendar", "Ready_weird", "Diag", "Diag_text",
                              "Tools", "OAuth", "Setup", "Weird", "No_status")
            ),
            "connectors": {"bootstrap": [SHAPES]},
            "steps": [{"start": {}}, runtime(), {"turn": "Hello"}, on("connectors/read")],
        },
        # -- call ------------------------------------------------------------
        {
            "name": "call/success",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "found report.pdf"}]}},
            },
            "backend": [search("call_c1"), {"text": "Found it."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
        {
            "name": "call/tool-error",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "quota exceeded"}],
                                               "isError": True}},
            },
            "backend": [search("call_c1"), {"text": "It failed."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/structured",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {
                    "content": [{"type": "text", "text": "two files"},
                                {"type": "text", "text": "and more"}],
                    "structuredContent": {"files": ["a.pdf", "b.pdf"]},
                }},
            },
            "backend": [search("call_c1"), {"text": "Two files."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/gateway-401",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 401, "body": {"detail": "bad key"}}}},
            "backend": [search("call_c1"), {"text": "Denied."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
        {
            "name": "call/gateway-404",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 404, "body": {"detail": "gone"}}}},
            "backend": [search("call_c1"), {"text": "Gone."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/gateway-500",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 500, "raw": "upstream exploded"}}},
            "backend": [search("call_c1"), {"text": "Broken."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/ask-approval",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "found report.pdf"}]}},
            },
            "backend": [search("call_c1"), {"text": "Found it."}],
            "callbacks": [{"approve": True}],
            "steps": [{"start": {"agent": "ask"}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/disabled-tool",
            "config": '[[connectors]]\nname = "Drive"\ndisabled_tools = ["search"]\n',
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [search("call_c1"), {"text": "Could not."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "call/not-ready",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [{"toolCalls": [call("connector_Mail_box_send", {}, "call_m1")]}, {"text": "No."}],
            "steps": [{"start": {}}, {"turn": "Send a mail"}],
        },
        # -- lifecycle -------------------------------------------------------
        {
            "name": "lifecycle/toggle-session",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="list"),
                {"configFile": True},
                {"turn": "Hello"},
                on("connector_catalog/toggle", alias="Drive", disabled=True),
                on("connectors/read"),
                {"turn": "Hello again"},
                on("connector_catalog/toggle", alias="Drive", disabled=False),
                on("connector_catalog/toggle", alias="Drive", disabled=False, toolName="list"),
                {"configFile": True},
                on("connector_catalog/read"),
                {"turn": "And again"},
            ],
        },
        {
            "name": "lifecycle/toggle-new-entry",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                on("connector_catalog/toggle", alias="Drive", disabled=False),
                {"configFile": True},
                runtime(),
                {"turn": "Hello"},
                on("connector_catalog/toggle", alias="Mail_box", disabled=True, toolName="send"),
                {"configFile": True},
            ],
        },
        {
            "name": "lifecycle/toggle-refusals",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                send("connector_catalog/toggle", alias="Drive", disabled=True),
                {"configFile": True},
                {"start": {}},
                on("connector_catalog/toggle", alias="Nope", disabled=True),
                on("connector_catalog/toggle", alias="Bad name", disabled=True),
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="   "),
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="x" * 257),
                send("connector_catalog/toggle", alias="Drive", disabled=False),
                send("connector_catalog/toggle", alias="Other", disabled=True),
                send("connector_catalog/toggle", alias="Drive", disabled=False, toolName="list"),
                {"configFile": True},
                send("connector_catalog/toggle", sessionId="nobody", alias="Drive", disabled=True),
            ],
        },
        {
            "name": "lifecycle/stale-session-catalog",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                {"bootstrap": [{"connectors": [DRIVE, MAIL, DOCS]}]},
                send("connector_catalog/refresh"),
                on("connector_catalog/toggle", alias="Drive", disabled=True),
                on("connector_catalog/read"),
                on("connector_catalog/refresh"),
                on("connector_catalog/toggle", alias="Drive", disabled=True),
            ],
        },
        {
            "name": "lifecycle/refresh-changed-catalog",
            "config": OPT_IN_WIDE,
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                {"start": {}},
                {"turn": "Hello"},
                {"bootstrap": [{"connectors": [
                    {**DRIVE, "tools": [tool("search", "Search everything", SEARCH_SCHEMA)]},
                    {**MAIL, "status": {"is_ready": True}},
                    DOCS,
                ]}]},
                on("connector_catalog/refresh"),
                {"turn": "Hello again"},
                on("connectors/refresh", name="Mail_box"),
                on("connectors/refresh", name="Nope"),
                {"bootstrap": [{"status": 500, "body": {}}]},
                on("connector_catalog/refresh"),
                on("connectors/refresh", name="Drive"),
                runtime(),
            ],
        },
        {
            "name": "lifecycle/toggle-during-turn",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [{"text": "Thinking slowly.", "delay": 2.5}, {"text": "Done."}],
            "steps": [
                {"start": {}},
                {"turn": "Take your time", "background": True, "settle": 0.8},
                on("connector_catalog/toggle", alias="Drive", disabled=True, toolName="list"),
                on("connector_catalog/refresh"),
                {"configFile": True},
                {"await": True},
                on("connector_catalog/read"),
                {"turn": "Quick one"},
            ],
        },
        {
            "name": "lifecycle/auth",
            "config": OPT_IN_WIDE,
            "connectors": {
                "bootstrap": [WIDE],
                "authUrls": {"c-mail": {"auth_url": "https://auth.example/mail", "ttl": 600},
                             "c-sheets": {"auth_url": "https://auth.example/sheets", "ttl": 60}},
            },
            "steps": [
                {"start": {}},
                on("connector_catalog/auth/request", alias="Mail_box"),
                {"sleep": 0.5},
                on("connector_catalog/auth/request", alias="Sheets"),
                {"sleep": 0.5},
                on("connector_catalog/auth/request", alias="Drive"),
                on("connector_catalog/auth/request", alias="Nope"),
                on("connector_catalog/auth/request", alias="Bad name"),
                send("connector_catalog/auth/request", sessionId="nobody", alias="Mail_box"),
                on("connectors/auth/read", name="Mail_box"),
                on("connectors/auth/read", name="Sheets"),
                on("connectors/auth/read", name="Drive"),
                on("connectors/auth/read", name="Nope"),
            ],
        },
        {
            # The SDK answers no page for a failing status or for a body its
            # response model refuses, here one without its `ttl`.
            "name": "lifecycle/auth-failures",
            "config": OPT_IN_WIDE,
            "connectors": {
                "bootstrap": [WIDE],
                "authUrls": {"c-mail": {"status": 500, "body": {"detail": "nope"}},
                             "c-sheets": {"auth_url": "https://auth.example/sheets"}},
            },
            "steps": [
                {"start": {}},
                on("connector_catalog/auth/request", alias="Mail_box"),
                {"sleep": 0.5},
                on("connector_catalog/auth/request", alias="Sheets"),
                {"sleep": 0.5},
                on("connectors/auth/read", name="Mail_box"),
                on("connectors/auth/read", name="Sheets"),
            ],
        },
        {
            "name": "lifecycle/auth-disabled-source",
            "config": '[[connectors]]\nname = "Mail_box"\ndisabled = true\n',
            "connectors": {"bootstrap": [CATALOG],
                           "authUrls": {"c-mail": {"auth_url": "https://auth.example/mail", "ttl": 600}}},
            "steps": [{"start": {}}, on("connector_catalog/auth/request", alias="Mail_box"),
                      on("connectors/auth/read", name="Mail_box")],
        },
        {
            "name": "lifecycle/sessionless-methods",
            "connectors": {"bootstrap": [CATALOG]},
            "steps": [
                send("connectors/read", sessionId="nobody"),
                send("connectors/refresh", sessionId="nobody", name="Drive"),
                send("connectors/auth/read", sessionId="nobody", name="Drive"),
                send("connector_catalog/read", sessionId="nobody"),
                send("connector_catalog/refresh", sessionId="nobody"),
            ],
        },
    ]


#: A program that calls the Drive connector's search from ``run_typescript``,
#: the only way the Unified Harness offers a connector tool.
UNIFIED_SEARCH = (
    "async function main() {\n"
    "  return await tools.connector_Drive.search({query: 'report'});\n}\n"
)
FOUND = {"c-drive/search": {"content": [{"type": "text", "text": "found report.pdf"}]}}


def program(identifier: str) -> dict[str, Any]:
    return {"toolCalls": [call("run_typescript", {"code": UNIFIED_SEARCH}, identifier)]}


def unified_scenarios() -> list[dict[str, Any]]:
    """The same catalog and gateway under the Unified Harness, where each
    connector is a ``connector_<alias>`` group a program reaches."""

    return [
        {
            "name": "unified/inventory",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [
                {"toolCalls": [call("search_tool_functions",
                                    {"mode": "all_connector_capabilities"}, "call_s1")]},
                {"toolCalls": [call("search_tool_functions",
                                    {"functions": ["connector_Drive.search"]}, "call_s2")]},
                {"text": "Listed."},
            ],
            "steps": [{"start": {}}, {"turn": "List"}, runtime()],
        },
        {
            "name": "unified/success",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG], "gateway": FOUND},
            "backend": [program("call_c1"), {"text": "Found it."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
        {
            "name": "unified/implicit",
            "connectors": {"bootstrap": [CATALOG], "gateway": FOUND},
            "backend": [program("call_c1"), {"text": "Found it."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
        {
            "name": "unified/tool-error",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {"content": [{"type": "text", "text": "quota exceeded"}],
                                               "isError": True}},
            },
            "backend": [program("call_c1"), {"text": "It failed."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "unified/structured",
            "config": OPT_IN,
            "connectors": {
                "bootstrap": [CATALOG],
                "gateway": {"c-drive/search": {
                    "content": [{"type": "text", "text": "two files"}],
                    "structuredContent": {"files": ["a.pdf", "b.pdf"]},
                }},
            },
            "backend": [program("call_c1"), {"text": "Two files."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "unified/gateway-401",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 401, "body": {"detail": "bad key"}}}},
            "backend": [program("call_c1"), {"text": "Denied."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "unified/gateway-500",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG],
                           "gateway": {"c-drive": {"status": 500, "raw": "upstream exploded"}}},
            "backend": [program("call_c1"), {"text": "Broken."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}],
        },
        {
            "name": "unified/ask-approval",
            "config": OPT_IN,
            "connectors": {"bootstrap": [CATALOG], "gateway": FOUND},
            "backend": [program("call_c1"), {"text": "Found it."}],
            "callbacks": [{"approve": True}],
            "steps": [{"start": {"agent": "ask"}}, {"turn": "Find the report"}],
        },
        {
            "name": "unified/disabled-tool",
            "config": '[[connectors]]\nname = "Drive"\ndisabled_tools = ["search"]\n',
            "connectors": {"bootstrap": [CATALOG]},
            "backend": [program("call_c1"), {"text": "Could not."}],
            "steps": [{"start": {}}, {"turn": "Find the report"}, runtime()],
        },
    ]
