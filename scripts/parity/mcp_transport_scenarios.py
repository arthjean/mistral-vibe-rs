"""The scenarios ``mcp_transport.py`` runs against the pinned reference's MCP
client.

A scenario names the servers, the script each answers from, and the steps the
client takes. Every value here is authored for the corpus; none is read from
the reference.
"""

from __future__ import annotations

from typing import Any

#: The variable a static-auth scenario reads its token from, and the token.
TOKEN_VARIABLE = "PARITY_MCP_TOKEN"
TOKEN = "token-oracle"


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------

ECHO = {
    "name": "echo",
    "description": "Echo a message",
    "inputSchema": {
        "type": "object",
        "properties": {"message": {"type": "string"}},
        "required": ["message"],
    },
}
BARE = {"name": "bare", "inputSchema": {"type": "object"}}
EMPTY_DESCRIPTION = {"name": "blank", "description": "", "inputSchema": {"type": "object"}}
TYPED = {
    "name": "typed",
    "description": "Return a count",
    "inputSchema": {"type": "object", "properties": {}},
    "outputSchema": {
        "type": "object",
        "properties": {"count": {"type": "integer"}},
        "required": ["count"],
    },
}
LATER = {"name": "later", "description": "On the second page", "inputSchema": {"type": "object"}}


def listed(*tools: dict[str, Any], cursor: str | None = None) -> dict[str, Any]:
    result: dict[str, Any] = {"tools": list(tools)}
    if cursor is not None:
        result["nextCursor"] = cursor
    return {"result": result}


def text_result(*texts: str, **extra: Any) -> dict[str, Any]:
    return {
        "result": {
            "content": [{"type": "text", "text": text} for text in texts],
            **extra,
        }
    }


def stdio(name: str = "demo", **fields: Any) -> dict[str, Any]:
    return {"transport": "stdio", "name": name, **fields}


def http(name: str = "demo", transport: str = "streamable-http", **fields: Any) -> dict[str, Any]:
    return {"transport": transport, "name": name, **fields}


def discover() -> dict[str, Any]:
    return {"do": "discover"}


def call(tool: str, **arguments: Any) -> dict[str, Any]:
    return {"do": "call", "tool": tool, "arguments": arguments}


def both(name: str, script: dict[str, Any], steps: list[dict[str, Any]], **server: Any) -> list[dict[str, Any]]:
    """The same scenario over stdio and over streamable HTTP."""

    return [
        {"name": f"stdio/{name}", "servers": [stdio(**server)], "scripts": {"demo": script}, "steps": steps},
        {"name": f"http/{name}", "servers": [http(**server)], "scripts": {"demo": script}, "steps": steps},
    ]


def all_scenarios() -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []
    basic = {
        "methods": {
            "tools/list": [listed(ECHO, BARE, EMPTY_DESCRIPTION, TYPED)],
            "tools/call:echo": [
                text_result("first", "second"),
                {"result": {"content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}},
            ],
            "tools/call:typed": [{"result": {"content": [{"type": "text", "text": "{\"count\": 3}"}], "structuredContent": {"count": 3}}}],
            "tools/call:bare": [{"result": {"content": [], "structuredContent": {"nested": {"flag": True, "none": None, "list": [1, 2.5, "x'y"]}}}}],
            "tools/call:blank": [text_result("done", isError=True)],
        }
    }
    scenarios += both(
        "basic",
        basic,
        [
            discover(),
            call("demo_echo", message="hi"),
            call("demo_typed"),
            call("demo_echo", message="again"),
            call("demo_bare"),
            call("demo_blank"),
        ],
    )
    scenarios += both(
        "hint",
        {"methods": {"tools/list": [listed(ECHO, BARE)]}},
        [discover()],
        prompt="Prefer the echo tool.\nIt is cheap.",
    )
    scenarios += both(
        "first-page-only",
        {
            "methods": {
                "tools/list": [listed(ECHO, cursor="page-2"), listed(LATER)],
                "tools/call:echo": [text_result("ok")],
                "tools/call:later": [text_result("late")],
            }
        },
        [discover(), call("demo_echo", message="x"), call("demo_echo", message="y")],
    )
    scenarios += both(
        "output-schema",
        {
            "methods": {
                "tools/list": [listed(TYPED)],
                "tools/call:typed": [
                    {"result": {"content": [{"type": "text", "text": "no structure"}]}},
                    {"result": {"content": [], "structuredContent": {"count": "three"}}},
                    {"result": {"content": [], "structuredContent": {"count": 4}}},
                ],
            }
        },
        [discover(), call("demo_typed"), call("demo_typed"), call("demo_typed")],
    )
    scenarios += both(
        "rpc-error",
        {
            "methods": {
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [{"error": {"code": -32000, "message": "the server refused"}}, text_result("fine")],
            }
        },
        [discover(), call("demo_echo", message="x"), call("demo_echo", message="y")],
    )
    scenarios += both(
        "older-protocol",
        {
            "methods": {
                "initialize": [{"result": {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "old", "version": "0"}}}],
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [text_result("ok")],
            }
        },
        [discover(), call("demo_echo", message="x")],
    )
    scenarios += both(
        "unsupported-protocol",
        {
            "methods": {
                "initialize": [{"result": {"protocolVersion": "1999-01-01", "capabilities": {"tools": {}}, "serverInfo": {"name": "odd", "version": "0"}}}],
                "tools/list": [listed(ECHO)],
            }
        },
        [discover()],
    )
    scenarios += both(
        "no-tools-capability",
        {
            "methods": {
                "initialize": [{"result": {"protocolVersion": "$requested", "capabilities": {}, "serverInfo": {"name": "bare", "version": "0"}}}],
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [text_result("ok")],
            }
        },
        [discover(), call("demo_echo", message="x")],
    )
    scenarios += both(
        "initialize-error",
        {"methods": {"initialize": [{"error": {"code": -32603, "message": "cannot start"}}]}},
        [discover()],
    )
    scenarios += both(
        "invalid-tool",
        {"methods": {"tools/list": [listed(ECHO, {"name": "schemaless"})]}},
        [discover()],
    )
    scenarios += both(
        "spaced-name",
        {"methods": {"tools/list": [listed({"name": "a tool", "description": "Spaced", "inputSchema": {"type": "object"}})]}},
        [discover()],
    )
    scenarios += both(
        "progress",
        {
            "methods": {
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [
                    {
                        "notify": [{"method": "notifications/message", "params": {"level": "info", "data": "working"}}],
                        **text_result("finished"),
                    }
                ],
            }
        },
        [discover(), call("demo_echo", message="x")],
    )
    scenarios += both(
        "slow-tool",
        {
            "methods": {
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [{"delayMs": 1500, **text_result("late")}, text_result("prompt")],
            }
        },
        [discover(), call("demo_echo", message="x"), call("demo_echo", message="y")],
        tool_timeout_sec=0.5,
    )
    scenarios += both(
        "slow-start",
        {"methods": {"initialize": [{"delayMs": 1500, "result": {"protocolVersion": "$requested", "capabilities": {"tools": {}}, "serverInfo": {"name": "slow", "version": "0"}}}]}},
        [discover()],
        startup_timeout_sec=0.5,
    )
    scenarios += both(
        "disabled",
        {"methods": {"tools/list": [listed(ECHO)]}},
        [discover()],
        disabled=True,
    )
    scenarios += both(
        "cache",
        {"methods": {"tools/list": [listed(ECHO), listed(ECHO, BARE)], "tools/call:echo": [text_result("cached")]}},
        [
            discover(),
            {"do": "restart"},
            discover(),
            call("demo_echo", message="x"),
            {"do": "restart"},
            {"do": "ageCache", "seconds": 90000},
            discover(),
        ],
    )
    scenarios += both(
        "sampling",
        {
            "methods": {
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [
                    {
                        "serverRequest": {
                            "method": "sampling/createMessage",
                            "params": {
                                "messages": [
                                    {"role": "user", "content": {"type": "text", "text": "Summarize"}},
                                    {"role": "assistant", "content": {"type": "image", "data": "aGk=", "mimeType": "image/png"}},
                                ],
                                "systemPrompt": "Be brief",
                                "maxTokens": 32,
                            },
                        },
                        **text_result("sampled"),
                    }
                ],
            }
        },
        [discover(), call("demo_echo", message="x")],
    )
    scenarios += both(
        "sampling-disabled",
        {
            "methods": {
                "tools/list": [listed(ECHO)],
                "tools/call:echo": [
                    {
                        "serverRequest": {
                            "method": "sampling/createMessage",
                            "params": {"messages": [{"role": "user", "content": {"type": "text", "text": "Summarize"}}], "maxTokens": 8},
                        },
                        **text_result("unsampled"),
                    }
                ],
            }
        },
        [discover(), call("demo_echo", message="x")],
        sampling_enabled=False,
    )
    # stdio only: the pool respawns a dead server once and retries the call.
    scenarios.append(
        {
            "name": "stdio/respawn",
            "servers": [stdio()],
            "scripts": {"demo": {"methods": {"tools/list": [listed(ECHO)], "tools/call:echo": [{"exit": True}, text_result("revived")]}}},
            "steps": [discover(), call("demo_echo", message="x"), call("demo_echo", message="y")],
        }
    )
    scenarios.append(
        {
            "name": "stdio/respawn-twice",
            "servers": [stdio()],
            "scripts": {"demo": {"methods": {"tools/list": [listed(ECHO)], "tools/call:echo": [{"exit": True}, {"exit": True}, text_result("third")]}}},
            "steps": [discover(), call("demo_echo", message="x"), call("demo_echo", message="y")],
        }
    )
    scenarios.append(
        {
            "name": "stdio/argv",
            "servers": [stdio(command_form="text", args=["--flag", "two words"], env={"PARITY_DECLARED": "1"})],
            "scripts": {"demo": {"methods": {"tools/list": [listed(BARE)]}}},
            "steps": [discover()],
        }
    )
    # HTTP only: headers, static credentials and the answers only HTTP can give.
    scenarios.append(
        {
            "name": "http/static-auth",
            "servers": [
                http(
                    auth={
                        "type": "static",
                        "headers": {"X-Workspace": "alpha"},
                        "api_key_env": TOKEN_VARIABLE,
                        "api_key_header": "X-Api-Key",
                        "api_key_format": "Token {token}",
                    }
                )
            ],
            "scripts": {"demo": {"methods": {"tools/list": [listed(ECHO)], "tools/call:echo": [text_result("ok")]}}},
            "steps": [discover(), call("demo_echo", message="x")],
        }
    )
    scenarios.append(
        {
            "name": "http/legacy-transport",
            "servers": [http(transport="http")],
            "scripts": {"demo": {"methods": {"tools/list": [listed(ECHO)], "tools/call:echo": [text_result("ok")]}}},
            "steps": [discover(), call("demo_echo", message="x")],
        }
    )
    scenarios.append(
        {
            "name": "http/sse",
            "servers": [http()],
            "scripts": {"demo": {"methods": {"tools/list": [{"sse": True, **listed(ECHO)}], "tools/call:echo": [{"sse": True, **text_result("streamed")}]}}},
            "steps": [discover(), call("demo_echo", message="x")],
        }
    )
    scenarios.append(
        {
            "name": "http/unauthorized-discovery",
            "servers": [http(auth={"type": "static", "headers": {"Authorization": "Bearer stale"}})],
            "scripts": {"demo": {"methods": {"initialize": [{"status": 401, "headers": {"www-authenticate": "Bearer"}}]}}},
            "steps": [discover(), discover()],
        }
    )
    scenarios.append(
        {
            "name": "http/unauthorized-call",
            "servers": [http(auth={"type": "static", "headers": {"Authorization": "Bearer stale"}})],
            "scripts": {
                "demo": {
                    "methods": {
                        "initialize": [
                            {"result": {"protocolVersion": "$requested", "capabilities": {"tools": {}}, "serverInfo": {"name": "s", "version": "0"}}},
                            {"status": 401, "headers": {"www-authenticate": "Bearer"}},
                        ],
                        "tools/list": [listed(ECHO)],
                    }
                }
            },
            "steps": [discover(), call("demo_echo", message="x"), discover()],
        }
    )
    scenarios.append(
        {
            "name": "http/server-error",
            "servers": [http()],
            "scripts": {"demo": {"methods": {"tools/list": [listed(ECHO)], "tools/call:echo": [{"status": 500, "body": "down"}]}}},
            "steps": [discover(), call("demo_echo", message="x")],
        }
    )
    scenarios.append(
        {
            "name": "http/oauth-without-login",
            "servers": [http(auth={"type": "oauth", "scopes": ["read"]})],
            "scripts": {"demo": {"methods": {"tools/list": [listed(ECHO)]}}},
            "steps": [discover()],
        }
    )
    scenarios.append(
        {
            "name": "mixed/two-servers",
            "servers": [stdio(name="local"), http(name="remote")],
            "scripts": {
                "local": {"methods": {"tools/list": [listed(ECHO)], "tools/call:echo": [text_result("local")]}},
                "remote": {"methods": {"tools/list": [listed(ECHO, BARE)], "tools/call:echo": [text_result("remote")]}},
            },
            "steps": [discover(), call("local_echo", message="x"), call("remote_echo", message="y")],
        }
    )
    return scenarios
