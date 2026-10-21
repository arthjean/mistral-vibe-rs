"""The scenarios ``telemetry_session.py`` runs through a session on each
surface, the reference's or this port's.

A scenario names the surface it runs on, the files and configuration its
workspace holds, what the stand-in platform answers, and the runs, editor steps
or typed lines it sends. Every value here is authored for the corpus.
"""

from __future__ import annotations

from typing import Any

import blackbox


MISTRAL_PROVIDER = (
    "\n[[providers]]\n"
    'name = "mistral"\n'
    'api_base = "$BASE/v1"\n'
    'api_key_env_var = "MISTRAL_API_KEY"\n'
    'backend = "mistral"\n'
)
#: A third-party provider and a model on it, for the scenarios that run off
#: Mistral.
GENERIC_PROVIDER = (
    "\n[[providers]]\n"
    'name = "oracle-provider"\n'
    'api_base = "$BASE/v1"\n'
    'api_key_env_var = "ORACLE_PROVIDER_KEY"\n'
    'api_style = "openai"\n'
    'backend = "generic"\n'
    "\n[[models]]\n"
    'name = "oracle-model-v1"\n'
    'provider = "oracle-provider"\n'
    'alias = "oracle-model"\n'
    "input_price = 0.0\n"
    "output_price = 0.0\n"
)


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------


def text(value: str) -> dict[str, Any]:
    return {"text": value}


def call(identifier: str, name: str, **arguments: Any) -> dict[str, Any]:
    return {"toolCalls": [{"id": identifier, "name": name, "arguments": arguments}]}


def run(*args: str, **extra: Any) -> dict[str, Any]:
    return {"args": list(args), **extra}


READ_ME = {"notes.txt": "remember the milk\n"}
READ = call("call_read", "read_file", file_path="notes.txt")
#: The summary a compaction asks for, in the shape both sides extract.
SUMMARY = text("<summary>The notes say milk.</summary>")
#: The active model with a threshold the next answer's usage crosses.
COMPACT_AT = (
    "\n[[models]]\n"
    'name = "mistral-vibe-cli-latest"\n'
    'provider = "mistral"\n'
    'alias = "mistral-medium-3.5"\n'
    "auto_compact_threshold = 1000\n"
)


def crossing(response: dict[str, Any]) -> dict[str, Any]:
    return {**response, "promptTokens": 5000}


def programmatic_scenarios() -> list[dict[str, Any]]:
    return [
        {
            "name": "programmatic/tool-turn",
            "files": READ_ME,
            "runs": [run("-p", "read it", "--output", "json", backend=[READ, text("It says milk.")])],
        },
        {
            "name": "programmatic/text-turn",
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/continue",
            "files": READ_ME,
            "runs": [
                run("-p", "read it", backend=[READ, text("Milk.")]),
                run("-p", "again", "-c", backend=[text("Still milk.")]),
            ],
        },
        {
            "name": "programmatic/resume-by-id",
            "runs": [
                run("-p", "first", "--output", "json", backend=[text("One.")]),
                run("-p", "second", "--resume", "$SESSION", backend=[text("Two.")]),
            ],
        },
        {
            "name": "programmatic/experiments-disabled",
            "experiments": {"enable": "false"},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/evaluation-fails",
            "stand": {"evaluation": {"status": 500, "body": {}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/evaluation-without-exposure",
            "stand": {"evaluation": {"status": 200, "body": {"features": {}}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/account-unavailable",
            "stand": {"whoami": {"status": 500, "body": {}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/account-refused",
            "stand": {"whoami": {"status": 401, "body": {}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/chat-plan",
            "stand": {"whoami": {"status": 200, "body": {"plan_type": "chat", "plan_name": "individual"}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/identity-unavailable",
            "stand": {"identity": {"status": 500, "body": {}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/identity-without-organization",
            "stand": {"identity": {"status": 200, "body": {"id": "oracle-user"}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/telemetry-disabled",
            "top": {"enable_telemetry": "false"},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/third-party-model-beside-mistral",
            "top": {"active_model": '"oracle-model"'},
            "providers": MISTRAL_PROVIDER + GENERIC_PROVIDER,
            "env": {"ORACLE_PROVIDER_KEY": "oracle-provider-key"},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            # The shipped Mistral provider is always configured, so the key is
            # what goes missing: nothing is looked up and nothing is reported.
            "name": "programmatic/mistral-key-missing",
            "top": {"active_model": '"oracle-model"'},
            "providers": MISTRAL_PROVIDER + GENERIC_PROVIDER,
            "env": {"ORACLE_PROVIDER_KEY": "oracle-provider-key", "MISTRAL_API_KEY": None},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/admin-config-applied",
            "stand": {
                "managed": {
                    "status": 200,
                    "body": {
                        "state": "enabled",
                        "toml": 'enable_update_checks = false\nauto_compact_threshold = 150000\n',
                    },
                }
            },
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/admin-config-disabled",
            "stand": {"managed": {"status": 200, "body": {"state": "disabled"}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/admin-config-unparsable",
            "stand": {"managed": {"status": 200, "body": {"state": "enabled", "toml": "= nope"}}},
            "runs": [run("-p", "hello", backend=[text("Hi.")])],
        },
        {
            "name": "programmatic/approval-refused",
            "runs": [
                run(
                    "-p", "write it", "--agent", "ask",
                    backend=[call("call_write", "write_file", file_path="a.txt", content="x"), text("No.")],
                )
            ],
        },
        {
            "name": "programmatic/approval-bypassed",
            "runs": [
                run(
                    "-p", "run it", "--auto-approve",
                    backend=[call("call_shell", "bash", command="true"), text("Ran.")],
                )
            ],
        },
        {
            "name": "programmatic/permission-never",
            "files": READ_ME,
            "tables": '\n[tools.read_file]\npermission = "never"\n',
            "runs": [run("-p", "read it", backend=[READ, text("Refused.")])],
        },
        {
            "name": "programmatic/failed-tool",
            "runs": [
                run(
                    "-p", "read it",
                    backend=[call("call_read", "read_file", file_path="missing.txt"), text("Gone.")],
                )
            ],
        },
        {
            "name": "programmatic/written-file",
            "runs": [
                run(
                    "-p", "write it",
                    backend=[call("call_write", "write_file", file_path="made.rs", content="x"), text("Done.")],
                )
            ],
        },
        {
            "name": "programmatic/mention",
            "files": READ_ME,
            "runs": [run("-p", "read @notes.txt", backend=[text("Milk.")])],
        },
        {
            "name": "programmatic/auto-compaction",
            "tables": COMPACT_AT,
            "files": READ_ME,
            "runs": [run("-p", "read it", backend=[crossing(READ), SUMMARY, text("Milk.")])],
        },
        {
            "name": "programmatic/failed-request",
            # The stand-in the corpus was captured with served no scripted
            # status, so this request was answered with an empty completion and
            # the run succeeds. Serving it a 400 changes the corpus, which
            # waits for a re-pin.
            "runs": [run("-p", "hello", backend=[{}])],
        },
    ]


def acp_prompt(identifier: int, value: str, session: str = "$S1") -> dict[str, Any]:
    return blackbox.prompt(identifier, value, session)


ACP_OPEN = [blackbox.INITIALIZE, blackbox.NEW_SESSION]
BASH = call("call_shell", "bash", command="true")


def acp_scenarios() -> list[dict[str, Any]]:
    return [
        {
            "name": "acp/tool-turn",
            "files": READ_ME,
            "steps": [*ACP_OPEN, {"backend": [READ, text("It says milk.")]}, acp_prompt(2, "read it")],
        },
        {
            "name": "acp/two-prompts",
            "steps": [
                *ACP_OPEN,
                {"backend": [text("One.")]},
                acp_prompt(2, "first"),
                {"backend": [text("Two.")]},
                acp_prompt(3, "second"),
            ],
        },
        {
            "name": "acp/approval-approved",
            "client": {"session/request_permission": blackbox.APPROVE_ONCE},
            "steps": [*ACP_OPEN, {"backend": [BASH, text("Ran.")]}, acp_prompt(2, "run it")],
        },
        {
            "name": "acp/approval-rejected",
            "client": {"session/request_permission": blackbox.REJECT},
            "steps": [*ACP_OPEN, {"backend": [BASH, text("No.")]}, acp_prompt(2, "run it")],
        },
        {
            "name": "acp/second-session",
            "steps": [
                *ACP_OPEN,
                blackbox.new_session(2),
                {"backend": [text("Hi.")]},
                acp_prompt(3, "hello", "$S2"),
            ],
        },
        {
            "name": "acp/load-session",
            "steps": [
                *ACP_OPEN,
                {"backend": [text("One.")]},
                acp_prompt(2, "first"),
                {"restart": True},
                blackbox.INITIALIZE,
                blackbox.request(
                    1, "session/load", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}
                ),
                {"backend": [text("Two.")]},
                acp_prompt(2, "second"),
            ],
        },
        {
            "name": "acp/auto-compaction",
            "tables": COMPACT_AT,
            "steps": [
                *ACP_OPEN,
                {"backend": [crossing(text("One."))]},
                acp_prompt(2, "first"),
                {"backend": [SUMMARY, text("Two.")]},
                acp_prompt(3, "second"),
            ],
        },
        {
            "name": "acp/manual-compaction",
            "steps": [
                *ACP_OPEN,
                {"backend": [text("One.")]},
                acp_prompt(2, "first"),
                {"backend": [SUMMARY]},
                acp_prompt(3, "/compact"),
            ],
        },
        {
            "name": "acp/telemetry-disabled",
            "top": {"enable_telemetry": "false"},
            "steps": [*ACP_OPEN, {"backend": [text("Hi.")]}, acp_prompt(2, "hello")],
        },
        {
            "name": "acp/experiments-disabled",
            "experiments": {"enable": "false"},
            "steps": [*ACP_OPEN, {"backend": [text("Hi.")]}, acp_prompt(2, "hello")],
        },
    ]


def tui_scenarios() -> list[dict[str, Any]]:
    """Lines typed into the interactive client once it reported its startup;
    every scenario ends with ``/exit``."""

    return [
        {"name": "tui/startup", "lines": []},
        {"name": "tui/clear", "lines": [{"type": "/clear"}]},
        {
            "name": "tui/prompt",
            "lines": [{"backend": [text("Hi.")]}, {"type": "hello"}],
        },
        {
            # The answer is held while Ctrl+C interrupts the turn.
            "name": "tui/interrupt",
            "lines": [
                {"backend": [{"text": "Late.", "delay": 6}]},
                {"type": "hello", "hold": True},
                {"press": "\x03", "after": 2.0},
            ],
        },
        {
            "name": "tui/prompt-then-clear",
            "lines": [{"backend": [text("Hi.")]}, {"type": "hello"}, {"type": "/clear"}],
        },
    ]


def scenarios() -> list[dict[str, Any]]:
    return [
        *({"surface": "programmatic", **scenario} for scenario in programmatic_scenarios()),
        *({"surface": "acp", **scenario} for scenario in acp_scenarios()),
        *({"surface": "tui", **scenario} for scenario in tui_scenarios()),
    ]
