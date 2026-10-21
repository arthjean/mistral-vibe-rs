"""The scenarios ``acp.py`` runs against a ``vibe-acp`` binary, the reference's or
this port's.

A scenario names the files, the vibe home, the home directory and the
environment its agent starts with, what the chat-completions stand-in answers,
how the client answers what the agent asks of it, and the messages the client
sends. Every value here is authored for the corpus.
"""

from __future__ import annotations

import base64
from typing import Any

from blackbox import (
    APPROVE_FILE_PERMANENT,
    APPROVE_FOLDER_SESSION,
    APPROVE_ONCE,
    APPROVE_SESSION,
    CLIENT_CAPABILITIES,
    DISMISS,
    ELICITING,
    INITIALIZE,
    NEW_SESSION,
    REJECT,
    ext,
    initialize,
    new_session,
    notify,
    prompt,
    request,
)


def call(name: str, arguments: dict[str, Any], identifier: str = "call_1") -> dict[str, Any]:
    return {"toolCalls": [{"id": identifier, "name": name, "arguments": arguments}]}


#: Files outside the workdir, under the scenario's home directory.
OUTSIDE_FILES = {"outside/data/a.txt": "alpha\n", "outside/data/nested/b.txt": "beta\n"}
BASE = [INITIALIZE, NEW_SESSION]
SKILL = (
    "---\nname: greet\ndescription: Greets the user politely\n---\n\n"
    "Say hello to the user.\n"
)
QUESTION = {
    "questions": [
        {
            "question": "Which color?",
            "header": "Color",
            "options": [
                {"label": "Red", "description": "Warm"},
                {"label": "Blue", "description": "Cold"},
            ],
        },
        {
            "question": "Which sizes?",
            "header": "Size",
            "multi_select": True,
            "options": [
                {"label": "Small", "description": "S"},
                {"label": "Large", "description": "L"},
            ],
        },
    ]
}
TODOS = {
    "action": "write",
    "todos": [
        {"id": "1", "content": "First task", "status": "completed", "priority": "high"},
        {"id": "2", "content": "Second task", "status": "in_progress", "priority": "medium"},
        {"id": "3", "content": "Dropped task", "status": "cancelled", "priority": "low"},
        {"id": "4", "content": "Last task", "status": "pending", "priority": "low"},
    ],
}
PNG = base64.b64encode(
    bytes.fromhex(
        "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
        "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082"
    )
).decode()


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- handshake ----------------------------------------------------
        {"name": "initialize/plain", "steps": [INITIALIZE]},
        {
            "name": "initialize/terminal-auth",
            "steps": [initialize({**CLIENT_CAPABILITIES, "_meta": {"terminal-auth": True}})],
        },
        {
            "name": "initialize/browser-delegated",
            "steps": [
                initialize({**CLIENT_CAPABILITIES, "_meta": {"browser-auth-delegated": True}})
            ],
        },
        {
            "name": "initialize/jetbrains",
            "steps": [initialize(info={"name": "JetBrains.Rider", "version": "2026.1"})],
        },
        {
            "name": "initialize/jetbrains-without-key",
            "env": {"MISTRAL_API_KEY": None},
            "steps": [initialize(info={"name": "JetBrains.Rider", "version": "2026.1"})],
        },
        {"name": "initialize/other-protocol", "steps": [initialize(version=7)]},
        {"name": "initialize/minimal", "steps": [request(0, "initialize", {"protocolVersion": 1})]},
        {"name": "initialize/twice", "steps": [INITIALIZE, initialize(identifier=1)]},
        {"name": "initialize/skipped", "steps": [NEW_SESSION]},
        {
            "name": "auth/status",
            "steps": [INITIALIZE, ext(1, "auth/status"), ext(2, "auth/signOut"), ext(3, "auth/status")],
        },
        {
            "name": "auth/status-without-key",
            "env": {"MISTRAL_API_KEY": None},
            "steps": [INITIALIZE, ext(1, "auth/status"), ext(2, "auth/signOut")],
        },
        {
            "name": "authenticate/unknown-method",
            "steps": [INITIALIZE, request(1, "authenticate", {"methodId": "nope"})],
        },
        {
            "name": "protocol/unknown-methods",
            "steps": [
                INITIALIZE,
                request(1, "foo/bar"),
                ext(2, "foo/bar"),
                request(3, "session/delete", {"sessionId": "x"}),
                request(4, "logout"),
                notify("_unknown/notice", {"a": 1}),
                notify("unknown/notice", {"a": 1}),
            ],
        },
        {
            "name": "protocol/invalid-params",
            "steps": [
                INITIALIZE,
                request(1, "session/new", {"mcpServers": []}),
                request(2, "session/prompt", {"sessionId": "missing", "prompt": []}),
                request(3, "session/set_mode", {"sessionId": "missing", "modeId": "plan"}),
                request(4, "session/load", {"sessionId": "missing", "cwd": "$WS", "mcpServers": []}),
                request(5, "session/close", {"sessionId": "missing"}),
                request(6, "session/fork", {"sessionId": "missing", "cwd": "$WS"}),
                request(7, "session/resume", {"sessionId": "missing", "cwd": "$WS"}),
                request(8, "session/set_config_option", {"sessionId": "missing", "configId": "mode", "value": "plan"}),
                request(9, "session/new", {"cwd": "$WS"}),
                request(10, "session/new", {"cwd": 7, "mcpServers": "none"}),
                request(11, "session/set_mode", {}),
                request(12, "session/prompt", {"sessionId": "missing", "prompt": "hi"}),
                request(13, "session/prompt", {"sessionId": "missing", "prompt": [{"type": "text"}]}),
                request(14, "session/new", {"cwd": "$WS", "mcpServers": [{"name": "a"}]}),
                request(15, "initialize", {}),
                request(16, "session/list", {"cwd": 3}),
                request(17, "authenticate", {}),
                request(18, "session/cancel", {"sessionId": "missing"}),
            ],
        },
        # -- session lifecycle ---------------------------------------------
        {"name": "session/new", "steps": BASE},
        {
            "name": "session/new-additional-directories",
            "files": {"extra/keep.txt": "keep\n"},
            "steps": [INITIALIZE, new_session(additionalDirectories=["$WS/extra"])],
        },
        {
            "name": "session/new-mcp-sse",
            "steps": [
                INITIALIZE,
                request(1, "session/new", {
                    "cwd": "$WS",
                    "mcpServers": [{"type": "sse", "name": "events", "url": "http://127.0.0.1:9/sse", "headers": []}],
                }),
            ],
        },
        {
            "name": "session/new-mcp-unreachable",
            "steps": [
                INITIALIZE,
                {**request(1, "session/new", {
                    "cwd": "$WS",
                    "mcpServers": [
                        {"type": "http", "name": "remote", "url": "http://127.0.0.1:9/mcp", "headers": [{"name": "X-Token", "value": "t"}]},
                        {"name": "local", "command": "/nonexistent/mcp-server", "args": ["--stdio"], "env": [{"name": "A", "value": "1"}]},
                    ],
                }), "quiet": 3.0},
            ],
        },
        {
            "name": "session/list",
            "backend": [{"text": "First answer"}],
            "steps": [
                *BASE,
                prompt(2, "List me"),
                request(3, "session/list", {}),
                request(4, "session/list", {"cwd": "$WS"}),
                request(5, "session/list", {"cwd": "/elsewhere"}),
            ],
        },
        {
            "name": "session/load",
            "backend": [{"text": "Remembered"}],
            "files": {"notes.txt": "alpha\nbeta\n"},
            "steps": [
                *BASE,
                prompt(2, "Remember this"),
                {"restart": True},
                INITIALIZE,
                request(1, "session/load", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}),
                prompt(2, "Again"),
            ],
        },
        {
            "name": "session/load-live",
            "backend": [{"text": "Remembered"}],
            "steps": [
                *BASE,
                prompt(2, "Remember this"),
                request(3, "session/load", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}),
            ],
        },
        {
            "name": "session/resume",
            "backend": [{"text": "Remembered"}],
            "steps": [
                *BASE,
                prompt(2, "Remember this"),
                request(3, "session/resume", {"sessionId": "$S1", "cwd": "$WS"}),
                {"restart": True},
                INITIALIZE,
                request(1, "session/resume", {"sessionId": "$S1", "cwd": "$WS"}),
                prompt(2, "Again"),
            ],
        },
        {
            "name": "session/fork",
            "backend": [{"text": "One"}, {"text": "Two"}],
            "steps": [
                *BASE,
                prompt(2, "First"),
                prompt(3, "Second"),
                request(4, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": []}),
                request(5, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": [], "messageId": "$U2"}),
                request(6, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "messageId": "unknown-message"}),
            ],
        },
        {
            # The router merges `_meta` into the handler's arguments, which is
            # where a fork point travels.
            "name": "session/fork-at-message",
            "backend": [{"text": "One"}, {"text": "Two"}],
            "steps": [
                *BASE,
                prompt(2, "First"),
                prompt(3, "Second"),
                request(4, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": [], "_meta": {"messageId": "$U2"}}),
                request(5, "session/close", {"sessionId": "$S2"}),
                request(6, "session/load", {"sessionId": "$S2", "cwd": "$WS", "mcpServers": []}),
                request(7, "session/fork", {"sessionId": "$S1", "cwd": "$WS", "mcpServers": [], "_meta": {"messageId": "unknown-message"}}),
            ],
        },
        {
            "name": "session/close",
            "steps": [
                *BASE,
                request(2, "session/close", {"sessionId": "$S1"}),
                request(3, "session/close", {"sessionId": "$S1"}),
                prompt(4, "After close"),
            ],
        },
        {
            "name": "session/set-mode",
            "steps": [
                *BASE,
                request(2, "session/set_mode", {"sessionId": "$S1", "modeId": "plan"}),
                request(3, "session/set_mode", {"sessionId": "$S1", "modeId": "no-such-mode"}),
                request(4, "session/set_mode", {"sessionId": "$S1", "modeId": "ask"}),
            ],
        },
        {
            "name": "session/set-config-option",
            "steps": [
                *BASE,
                request(2, "session/set_config_option", {"sessionId": "$S1", "configId": "mode", "value": "plan"}),
                request(3, "session/set_config_option", {"sessionId": "$S1", "configId": "model", "value": "local"}),
                request(4, "session/set_config_option", {"sessionId": "$S1", "configId": "thinking", "value": "low"}),
                request(5, "session/set_config_option", {"sessionId": "$S1", "configId": "max_turns", "value": "5"}),
                request(6, "session/set_config_option", {"sessionId": "$S1", "configId": "max_tokens", "value": "abc"}),
                request(7, "session/set_config_option", {"sessionId": "$S1", "configId": "model", "value": "missing"}),
                request(8, "session/set_config_option", {"sessionId": "$S1", "configId": "thinking", "value": "extreme"}),
                request(9, "session/set_config_option", {"sessionId": "$S1", "configId": "colour", "value": "red"}),
                request(10, "session/set_config_option", {"sessionId": "$S1", "configId": "mode", "value": True}),
            ],
        },
        # -- prompt turns --------------------------------------------------
        {
            "name": "prompt/text",
            "backend": [{"text": ["Hello", " there"]}],
            "steps": [*BASE, prompt(2, "Say hello")],
        },
        {
            "name": "prompt/content-blocks",
            "backend": [{"text": "Seen"}],
            "steps": [
                *BASE,
                request(2, "session/prompt", {
                    "sessionId": "$S1",
                    "prompt": [
                        {"type": "text", "text": "Look at these"},
                        {"type": "resource", "resource": {"uri": "file://$WS/a.txt", "mimeType": "text/plain", "text": "embedded text"}},
                        {"type": "resource_link", "uri": "file://$WS/b.txt", "name": "b.txt", "title": "B file", "mimeType": "text/plain", "size": 3},
                        {"type": "image", "data": PNG, "mimeType": "image/png", "uri": "file://$WS/shot.png"},
                        {"type": "text", "text": "automatic context", "_meta": {"automatic": True}},
                    ],
                }),
            ],
        },
        {
            "name": "prompt/rejected-blocks",
            "steps": [
                *BASE,
                request(2, "session/prompt", {"sessionId": "$S1", "prompt": [{"type": "audio", "data": "AAAA", "mimeType": "audio/wav"}]}),
                request(3, "session/prompt", {"sessionId": "$S1", "prompt": [{"type": "image", "data": "not base64!", "mimeType": "image/png"}]}),
                request(4, "session/prompt", {"sessionId": "$S1", "prompt": [{"type": "image", "data": PNG, "mimeType": "image/tiff"}]}),
            ],
        },
        {
            "name": "prompt/user-display-content",
            "backend": [{"text": "Shown"}],
            "steps": [
                *BASE,
                request(2, "session/prompt", {
                    "sessionId": "$S1",
                    "prompt": [{"type": "text", "text": "Expanded prompt"}],
                    "user_display_content": {"text": "Short prompt"},
                }),
                request(3, "session/prompt", {
                    "sessionId": "$S1",
                    "prompt": [{"type": "text", "text": "Bad display"}],
                    "user_display_content": {"unknown": 1},
                }),
            ],
        },
        {
            "name": "prompt/mention",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [{"text": "Read it"}],
            "steps": [*BASE, prompt(2, "Summarize @notes.txt please")],
        },
        {
            "name": "prompt/read-file",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [call("read_file", {"file_path": "$WS/notes.txt"}), {"text": "It says alpha"}],
            "steps": [*BASE, prompt(2, "Read notes")],
        },
        {
            "name": "prompt/read-file-range",
            "files": {"notes.txt": "alpha\nbeta\ngamma\n"},
            "backend": [call("read_file", {"file_path": "$WS/notes.txt", "offset": 2, "limit": 1}), {"text": "Done"}],
            "steps": [*BASE, prompt(2, "Read one line")],
        },
        {
            "name": "prompt/read-file-without-fs",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [call("read_file", {"file_path": "$WS/notes.txt"}), {"text": "It says alpha"}],
            "steps": [initialize({}), NEW_SESSION, prompt(2, "Read notes")],
        },
        {
            "name": "prompt/write-file",
            "backend": [call("write_file", {"file_path": "$WS/out.txt", "content": "written\n"}), {"text": "Wrote it"}],
            "steps": [*BASE, prompt(2, "Write a file")],
        },
        {
            "name": "prompt/edit-file",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [call("edit", {"file_path": "$WS/notes.txt", "old_string": "beta", "new_string": "delta"}), {"text": "Edited"}],
            "steps": [*BASE, prompt(2, "Edit a file")],
        },
        {
            "name": "prompt/grep",
            "files": {"notes.txt": "alpha\nbeta\n", "more.txt": "beta again\n"},
            "backend": [call("grep", {"pattern": "beta", "path": "$WS"}), {"text": "Found"}],
            "steps": [*BASE, prompt(2, "Search")],
        },
        {
            "name": "prompt/bash-approved",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), {"text": "Ran"}],
            "client": {"session/request_permission": APPROVE_ONCE},
            "steps": [*BASE, prompt(2, "Run a command")],
        },
        {
            "name": "prompt/bash-approved-for-session",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), call("bash", {"command": "echo hi > made.txt"}, "call_2"), {"text": "Ran twice"}],
            "client": {"session/request_permission": APPROVE_SESSION},
            "steps": [*BASE, prompt(2, "Run twice")],
        },
        {
            # A folder outside the workdir offers its recursive grant, which
            # covers the nested search that follows.
            "name": "prompt/grep-outside-folder",
            "homeFiles": OUTSIDE_FILES,
            "backend": [call("grep", {"pattern": "alpha", "path": "$HOME/outside/data"}),
                        call("grep", {"pattern": "beta", "path": "$HOME/outside/data/nested/b.txt"},
                             "call_2"),
                        {"text": "Found"}],
            "client": {"session/request_permission": APPROVE_FOLDER_SESSION},
            "steps": [*BASE, prompt(2, "Search outside")],
        },
        {
            # A file outside the workdir offers its exact grant, kept in the
            # configuration, which covers the same file again.
            "name": "prompt/read-outside-file",
            "homeFiles": OUTSIDE_FILES,
            "backend": [call("read_file", {"file_path": "$HOME/outside/data/a.txt"}),
                        call("read_file", {"file_path": "$HOME/outside/data/a.txt"}, "call_2"),
                        {"text": "Read"}],
            "client": {"session/request_permission": APPROVE_FILE_PERMANENT},
            "steps": [*BASE, prompt(2, "Read outside")],
        },
        {
            "name": "prompt/bash-rejected",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), {"text": "Understood"}],
            "client": {"session/request_permission": REJECT},
            "steps": [*BASE, prompt(2, "Run a command")],
        },
        {
            "name": "prompt/bash-dismissed",
            "backend": [call("bash", {"command": "echo hi > made.txt"}), {"text": "Understood"}],
            "client": {"session/request_permission": DISMISS},
            "steps": [*BASE, prompt(2, "Run a command")],
        },
        {
            "name": "prompt/bash-ask-mode",
            "backend": [call("read_file", {"file_path": "$WS/notes.txt"}), {"text": "Read"}],
            "files": {"notes.txt": "alpha\n"},
            "client": {"session/request_permission": APPROVE_ONCE},
            "steps": [
                *BASE,
                request(2, "session/set_mode", {"sessionId": "$S1", "modeId": "ask"}),
                prompt(3, "Read under ask"),
            ],
        },
        {
            "name": "prompt/todo",
            "backend": [call("todo", TODOS), {"text": "Planned"}],
            "steps": [*BASE, prompt(2, "Plan the work")],
        },
        {
            "name": "prompt/question-elicited",
            "backend": [call("ask_user_question", QUESTION), {"text": "Thanks"}],
            "client": {
                "elicitation/create": {"action": "accept", "content": {"q0": "Red", "q1": ["Small", "Huge"]}},
            },
            "steps": [initialize(ELICITING), NEW_SESSION, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/question-declined",
            "backend": [call("ask_user_question", QUESTION), {"text": "Ok"}],
            "client": {"elicitation/create": {"action": "decline"}},
            "steps": [initialize(ELICITING), NEW_SESSION, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/question-invalid-answer",
            "backend": [call("ask_user_question", QUESTION), {"text": "Ok"}],
            "client": {"elicitation/create": {"action": "accept", "content": {"q0": "", "q1": ["Small"]}}},
            "steps": [initialize(ELICITING), NEW_SESSION, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/question-without-elicitation",
            "backend": [call("ask_user_question", QUESTION), {"text": "Ok"}],
            "steps": [*BASE, prompt(2, "Ask me")],
        },
        {
            "name": "prompt/backend-error",
            "backend": [{"status": 400, "body": {"message": "bad request", "type": "invalid_request_error"}}],
            "steps": [*BASE, prompt(2, "Fail please")],
        },
        {
            "name": "prompt/backend-unauthorized",
            "backend": [{"status": 401, "body": {"message": "Unauthorized"}}],
            "steps": [*BASE, prompt(2, "Fail please")],
        },
        {
            "name": "prompt/cancel",
            "backend": [{"text": "Too late", "delay": 3.0}],
            "steps": [
                *BASE,
                {**prompt(2, "Slow answer"), "defer": True, "quiet": 0.8},
                {**notify("session/cancel", {"sessionId": "$S1"}), "quiet": 0.0},
                {"awaitId": 2},
            ],
        },
        {
            "name": "prompt/busy",
            "backend": [{"text": "Slow", "delay": 2.0}, {"text": "Second"}],
            "steps": [
                *BASE,
                {**prompt(2, "First"), "defer": True, "quiet": 0.5},
                prompt(3, "Second"),
                {"awaitId": 2},
            ],
        },
        {
            "name": "prompt/session-title",
            "backend": [{"text": "Titled"}],
            "steps": [*BASE, prompt(2, "A fairly long first message that becomes the title of the session")],
        },
        # -- commands --------------------------------------------------------
        {
            "name": "command/help",
            "vibeHomeFiles": {"skills/greet/SKILL.md": SKILL},
            "steps": [*BASE, prompt(2, "/help")],
        },
        {"name": "command/log", "steps": [*BASE, prompt(2, "/log")]},
        {"name": "command/data-retention", "steps": [*BASE, prompt(2, "/data-retention")]},
        {
            "name": "command/reload",
            "steps": [
                *BASE,
                {"write": {}},
                prompt(2, "/reload"),
            ],
        },
        {
            "name": "command/mcp",
            "steps": [
                *BASE,
                prompt(2, "/mcp"),
                prompt(3, "/mcp status extra"),
                prompt(4, "/mcp login"),
                prompt(5, "/mcp login nothing"),
                prompt(6, "/mcp logout nothing"),
                prompt(7, "/mcp logout"),
                prompt(8, "/mcp frobnicate"),
            ],
        },
        {
            "name": "command/proxy-setup",
            "steps": [
                *BASE,
                prompt(2, "/proxy-setup"),
                prompt(3, "/proxy-setup HTTPS_PROXY http://proxy.local:8080"),
                prompt(4, "/proxy-setup"),
                prompt(5, "/proxy-setup https_proxy"),
                prompt(6, "/proxy-setup NOT_A_PROXY value"),
            ],
        },
        {
            "name": "command/compact-empty",
            "steps": [*BASE, prompt(2, "/compact")],
        },
        {
            "name": "command/compact",
            "backend": [{"text": "Answer"}, {"text": "Summary of the conversation"}],
            "steps": [*BASE, prompt(2, "Hello"), prompt(3, "/compact keep it short")],
        },
        {
            "name": "command/retry",
            "backend": [{"text": "Answer"}, {"text": "Continued"}],
            "steps": [*BASE, prompt(2, "/retry"), prompt(3, "Hello"), prompt(4, "/retry be brief")],
        },
        {
            "name": "command/lean",
            "steps": [*BASE, prompt(2, "/leanstall"), prompt(3, "/unleanstall")],
        },
        {"name": "command/teleport", "steps": [*BASE, prompt(2, "/teleport")]},
        {
            "name": "command/unknown-and-case",
            "backend": [{"text": "Plain"}],
            "steps": [*BASE, prompt(2, "/HELP"), prompt(3, "/nonexistent thing")],
        },
        {
            "name": "command/skill",
            "vibeHomeFiles": {"skills/greet/SKILL.md": SKILL},
            "backend": [{"text": "Hello friend"}],
            "steps": [*BASE, prompt(2, "/greet warmly")],
        },
        # -- extensions ------------------------------------------------------
        {"name": "ext/config-schema", "steps": [INITIALIZE, ext(1, "config/schema")]},
        {
            "name": "ext/session-title",
            "backend": [{"text": "Answer"}],
            "steps": [
                *BASE,
                prompt(2, "Hello"),
                ext(3, "session/set_title", {"sessionId": "$S1", "title": "  Renamed  "}),
                ext(4, "session/set_title", {"sessionId": "$S1", "title": "   "}),
                ext(5, "session/set_title", {"sessionId": "unknown", "title": "Nope"}),
                ext(6, "session/set_title", {}),
                request(7, "session/list", {}),
            ],
        },
        {
            "name": "ext/session-delete",
            "backend": [{"text": "Answer"}],
            "steps": [
                *BASE,
                prompt(2, "Hello"),
                ext(3, "session/delete", {"sessionId": "$S1"}),
                request(4, "session/list", {}),
                ext(5, "session/delete", {"sessionId": "unknown"}),
                ext(6, "session/delete", {}),
            ],
        },
        {
            "name": "ext/loops",
            "steps": [
                *BASE,
                ext(2, "loops/list", {"sessionId": "$S1"}),
                ext(3, "loops/create", {"sessionId": "$S1", "interval": "5m", "prompt": "check status"}),
                ext(4, "loops/create", {"sessionId": "$S1", "interval": "never", "prompt": "bad"}),
                ext(5, "loops/list", {"sessionId": "$S1"}),
                ext(6, "loops/delete", {"sessionId": "$S1", "loopId": "unknown"}),
                ext(7, "loops/clear", {"sessionId": "$S1"}),
                ext(8, "loops/list", {"sessionId": "unknown"}),
                ext(9, "loops/create", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/trust",
            "steps": [
                *BASE,
                ext(2, "trust/status", {}),
                ext(3, "trust/status", {"sessionId": "$S1"}),
                ext(4, "trust/status", {"cwd": "$WS"}),
                ext(5, "trust/decision", {"decision": "trust"}),
                ext(6, "trust/decision", {"sessionId": "$S1", "decision": "maybe"}),
                ext(7, "trust/decision", {"sessionId": "$S1", "decision": "trust"}),
                ext(8, "trust/status", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/rewind",
            "files": {"notes.txt": "alpha\n"},
            "backend": [call("write_file", {"file_path": "$WS/notes.txt", "content": "changed\n"}), {"text": "Changed"}],
            "steps": [
                *BASE,
                prompt(2, "Change notes"),
                ext(3, "rewind/preview", {"sessionId": "$S1", "messageId": "$U1"}),
                ext(4, "rewind/preview", {"sessionId": "$S1"}),
                ext(5, "rewind/preview", {"sessionId": "$S1", "messageId": "unknown"}),
                ext(6, "rewind/to", {"sessionId": "$S1", "messageId": "$U1"}),
            ],
        },
        {
            "name": "ext/review",
            "files": {"notes.txt": "alpha\n"},
            "backend": [call("write_file", {"file_path": "$WS/notes.txt", "content": "changed\n"}), {"text": "Changed"}],
            "steps": [
                *BASE,
                ext(2, "review/state", {"sessionId": "$S1"}),
                prompt(3, "Change notes"),
                ext(4, "review/state", {"sessionId": "$S1"}),
                ext(5, "review/baseline", {"sessionId": "$S1", "path": "$WS/notes.txt"}),
                ext(6, "review/turnDiff", {"sessionId": "$S1", "path": "$WS/notes.txt", "owner": "agent"}),
                ext(7, "review/hunks", {"sessionId": "$S1", "path": "$WS/notes.txt", "owner": "agent"}),
                ext(8, "review/approve", {"sessionId": "$S1", "target": {"kind": "all"}}),
                ext(9, "review/revert", {"sessionId": "$S1", "target": {"kind": "all"}}),
                ext(10, "review/state", {}),
            ],
        },
        {
            "name": "ext/connectors",
            "steps": [
                *BASE,
                ext(2, "connectors/list", {"sessionId": "$S1"}),
                ext(3, "connectors/authUrl", {"sessionId": "$S1", "name": "gmail"}),
                ext(4, "connectors/refresh", {"sessionId": "$S1", "names": ["gmail"]}),
                ext(5, "connectors/toggle", {"sessionId": "$S1", "name": "gmail", "disabled": True}),
                ext(6, "connectors/list", {}),
                ext(7, "connectors/unknown", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/project-links",
            "steps": [
                INITIALIZE,
                ext(1, "projectLinks/list", {}),
                ext(2, "projectLinks/resolveRoot", {"rootPath": "$WS"}),
                ext(3, "projectLinks/picker/load", {"rootPath": "$WS"}),
                ext(4, "projectLinks/picker/loadMore", {"rootPath": "$WS", "cursor": "c"}),
                ext(5, "projectLinks/create", {"rootPath": "$WS", "name": "demo", "defaultBranch": "main"}),
                ext(6, "projectLinks/link", {"rootPath": "$WS", "projectId": "p", "projectName": "n"}),
                ext(7, "projectLinks/unlink", {"rootPath": "$WS"}),
                ext(8, "projectLinks/resolveRoot", {}),
                ext(9, "projectLinks/other", {}),
            ],
        },
        {
            "name": "ext/whoami",
            "steps": [
                *BASE,
                ext(2, "identity/read", {"sessionId": "$S1"}),
                ext(3, "account/read", {"sessionId": "$S1"}),
                ext(4, "identity/read", {}),
                ext(5, "account/other", {"sessionId": "$S1"}),
            ],
        },
        {
            "name": "ext/log-level",
            "steps": [
                *BASE,
                ext(2, "logLevel/read", {}),
                ext(3, "logLevel/write", {"sessionId": "$S1", "sessionOverride": "DEBUG"}),
                ext(4, "logLevel/write", {"sessionId": "$S1", "configLevel": "WARNING"}),
                ext(5, "logLevel/write", {"sessionId": "$S1", "configLevel": None, "sessionOverride": None}),
                ext(6, "logLevel/write", {}),
                ext(7, "logLevel/other", {}),
            ],
        },
        {
            "name": "ext/voice",
            "steps": [
                INITIALIZE,
                ext(1, "voice/transcribeStart", {}),
                ext(2, "voice/narrate", {"userMessage": "hi", "assistantText": "hello"}),
                ext(3, "voice/transcribeStop", {}),
                ext(4, "voice/transcribeCancel", {}),
                ext(5, "voice/narrateCancel", {}),
                ext(6, "voice/other", {}),
            ],
        },
        {
            "name": "ext/telemetry",
            "steps": [
                *BASE,
                notify("_telemetry/send", {"sessionId": "$S1", "event": "vibe.at_mention_inserted", "properties": {"kind": "file"}}),
                notify("_telemetry/send", {"sessionId": "$S1", "event": "vibe.unknown_event"}),
                notify("_telemetry/send", {"event": "missing session"}),
                request(2, "session/list", {}),
            ],
        },
    ]
