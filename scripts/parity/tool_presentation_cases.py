"""The scenarios ``tool_presentation.py`` projects through the pinned reference.

A scenario is one effect a harness reports generically: a tool name, the
arguments the call carried, the state it reached (running, completed with a
result envelope, failed) and the display roots a session hands the projection.
A callback scenario carries the same call inside an approval request instead.
``expect`` says whether the reference rewrites the entry or leaves it as it
came, which the capture checks so a scenario cannot silently stop measuring
what its name claims.

Every value here is authored for the corpus: no tool runs, so a path names
nothing on any machine, and every result is a shape the harness could answer.
The table holds data only and imports neither the driver nor the reference.
"""

from __future__ import annotations

from typing import Any

#: The error every failed scenario reports. This repository's own text.
ERROR_MESSAGE = "the capture authored this failure"

#: The text block every result envelope carries beside its structured content.
RESULT_TEXT = "the tool answered"

#: The session directory the path family binds, as a display root.
ROOT = "/workspace"

#: The annotation keys the harness stamps edit and write previews under.
EDIT_ANNOTATION = "mistralai.vibe.sdk.search_replace"
WRITE_ANNOTATION = "mistralai.vibe.sdk.write_file"


def running() -> dict[str, Any]:
    return {"status": "running"}


def completed(
    tool: str,
    structured: Any,
    *,
    content: list[dict[str, Any]] | None = None,
    meta: dict[str, Any] | None = None,
    message: str | None = None,
) -> dict[str, Any]:
    """A completed state whose result display still names the raw tool key,
    which is what the harness reports before any projection."""

    envelope: dict[str, Any] = {
        "content": content if content is not None else [{"type": "text", "text": RESULT_TEXT}],
        "structured_content": structured,
    }
    if meta is not None:
        envelope["_meta"] = meta
    return {
        "status": "completed",
        "output": envelope,
        "display": {"success": True, "verb": "Ran", "message": message or tool},
    }


def failed() -> dict[str, Any]:
    return {
        "status": "failed",
        "error": {"message": ERROR_MESSAGE, "code": "tool_failed"},
        "display": {"success": False, "message": ERROR_MESSAGE},
    }


def effect(
    name: str,
    tool: str,
    arguments: Any,
    state: dict[str, Any],
    *,
    expect: str = "projected",
    roots: tuple[str, ...] = (),
) -> dict[str, Any]:
    return {
        "name": name,
        "kind": "effect",
        "toolName": tool,
        "input": arguments,
        "state": state,
        "displayRoots": list(roots),
        "expect": expect,
    }


def callback(
    name: str, tool: str, arguments: Any, *, expect: str = "projected"
) -> dict[str, Any]:
    return {
        "name": name,
        "kind": "callback",
        "toolName": tool,
        "input": arguments,
        "displayRoots": [],
        "expect": expect,
    }


# --------------------------------------------------------------------------
# Authored arguments and results
# --------------------------------------------------------------------------

READ_ARGUMENTS = {"path": "/workspace/src/alpha.txt", "offset": 0, "limit": 20}
READ_RESULT = {
    "path": "/workspace/src/alpha.txt",
    "content": "alpha\nbeta\n",
    "offset": 0,
    "lines_read": 2,
    "was_truncated": False,
}
WRITE_ARGUMENTS = {"path": "/workspace/src/alpha.txt", "content": "alpha\n"}
EDIT_ARGUMENTS = {
    "file_path": "/workspace/src/alpha.txt",
    "old_string": "one",
    "new_string": "two",
    "replace_all": False,
}
EDIT_RESULT = {"file": "/workspace/src/alpha.txt", "warnings": []}
EDIT_META = {
    EDIT_ANNOTATION: {
        "blocks": [
            {
                "old_start_line": 3,
                "new_start_line": 3,
                "old_lines": ["one\n"],
                "new_lines": ["two\n"],
            }
        ],
        "previous_content": "zero\nhalf\none\n",
    }
}
GREP_ARGUMENTS = {
    "pattern": "alpha",
    "path": "/workspace/src",
    "max_matches": 20,
    "use_default_ignore": False,
}
GREP_RESULT = {
    "matches": "src/alpha.txt:1:alpha",
    "match_count": 1,
    "was_truncated": False,
    "parsed_matches": [{"path": "src/alpha.txt", "line": 1}],
}
QUESTION = {
    "question": "Which target?",
    "header": "Target",
    "options": [{"label": "alpha"}, {"label": "beta", "description": "the second target"}],
}
TODO_ITEM = {"id": "t1", "content": "capture the presentation", "status": "in_progress"}
PROCESS_ID = "proc-0123456789abcdef"


def _call_cases() -> list[dict[str, Any]]:
    """Every projected tool as it starts, with arguments it accepts."""

    return [
        effect("call-read_file", "read_file", READ_ARGUMENTS, running()),
        effect(
            "call-read_file-offset",
            "read_file",
            {"path": "/workspace/src/alpha.txt", "offset": 4, "limit": 2},
            running(),
        ),
        effect("call-file_system.read_file", "file_system.read_file", READ_ARGUMENTS, running()),
        effect("call-write_file", "write_file", WRITE_ARGUMENTS, running()),
        effect(
            "call-file_system.write_file", "file_system.write_file", WRITE_ARGUMENTS, running()
        ),
        effect("call-edit", "edit", EDIT_ARGUMENTS, running()),
        effect("call-file_system.edit", "file_system.edit", EDIT_ARGUMENTS, running()),
        effect("call-grep", "grep", GREP_ARGUMENTS, running()),
        effect("call-grep-defaults", "grep", {"pattern": "alpha"}, running()),
        effect("call-bash", "bash", {"command": "echo alpha"}, running()),
        effect("call-file_system.bash", "file_system.bash", {"command": "ls src"}, running()),
        effect("call-skill.read", "skill.read", {"name": "reviewer"}, running()),
        effect("call-process.start", "process.start", {"command": "sleep 5"}, running()),
        effect("call-process.stop", "process.stop", {"processId": PROCESS_ID}, running()),
        effect("call-process.output", "process.output", {"processId": PROCESS_ID}, running()),
        effect("call-process.list", "process.list", {}, running()),
        effect(
            "call-process.write",
            "process.write",
            {"processId": "proc-1", "text": "y\n"},
            running(),
        ),
        effect(
            "call-ui.ask_user_question",
            "ui.ask_user_question",
            {"questions": [QUESTION]},
            running(),
        ),
        effect(
            "call-ui.ask_user_question-two",
            "ui.ask_user_question",
            {"questions": [QUESTION, {**QUESTION, "question": "Which mode?"}]},
            running(),
        ),
        effect("call-web_search", "web_search", {"query": "rust parity oracle"}, running()),
        effect(
            "call-web_fetch",
            "web_fetch",
            {"url": "https://example.invalid/doc", "timeout": 15},
            running(),
        ),
        effect(
            "call-web_fetch-no-host",
            "web_fetch",
            {"url": "example-invalid-without-a-scheme"},
            running(),
        ),
        effect(
            "call-connector_web_search.web_search",
            "connector_web_search.web_search",
            {"query": "rust parity oracle"},
            running(),
        ),
        effect(
            "call-connector_web_search.open_url",
            "connector_web_search.open_url",
            {"url": "https://example.invalid/doc"},
            running(),
        ),
        effect("call-todo-read", "todo", {"action": "read"}, running()),
        effect("call-todo-write", "todo", {"action": "write", "todos": [TODO_ITEM]}, running()),
        effect("call-todo-unknown", "todo", {"action": "archive"}, running()),
        effect("call-vibe.todo", "vibe.todo", {"action": "read"}, running()),
        effect(
            "call-vibe.cron-schedule",
            "vibe.cron",
            {"action": "schedule", "interval_seconds": 5430, "prompt": "check   the\nbuild"},
            running(),
        ),
        effect(
            "call-vibe.cron-schedule_cron",
            "vibe.cron",
            {"action": "schedule_cron", "cron": "0 9 * * 1", "prompt": "weekly report"},
            running(),
        ),
        effect("call-vibe.cron-list", "vibe.cron", {"action": "list"}, running()),
        effect("call-vibe.cron-cancel", "vibe.cron", {"action": "cancel", "id": "loop-1"}, running()),
        effect("call-vibe.cron-clear", "vibe.cron", {"action": "clear"}, running()),
        effect(
            "call-vibe.unified_harness_scratchpad-list",
            "vibe.unified_harness_scratchpad",
            {"action": "list"},
            running(),
        ),
        effect(
            "call-vibe.unified_harness_scratchpad-read",
            "vibe.unified_harness_scratchpad",
            {"action": "read", "path": "notes.md"},
            running(),
        ),
        effect(
            "call-vibe.unified_harness_scratchpad-write",
            "vibe.unified_harness_scratchpad",
            {"action": "write", "path": "notes.md", "content": "remember this"},
            running(),
        ),
    ]


def _invalid_cases() -> list[dict[str, Any]]:
    """Arguments a projector refuses, so the entry stays as the harness sent it."""

    return [
        effect("invalid-read_file-no-path", "read_file", {"offset": 2}, running(), expect="unchanged"),
        effect(
            "invalid-edit-same-strings",
            "edit",
            {**EDIT_ARGUMENTS, "new_string": "one"},
            running(),
            expect="unchanged",
        ),
        effect("invalid-bash-empty", "bash", {"command": ""}, running(), expect="unchanged"),
        effect("invalid-grep-absent-input", "grep", None, running(), expect="unchanged"),
        effect(
            "invalid-vibe.cron-no-schedule",
            "vibe.cron",
            {"action": "schedule", "prompt": "check the build"},
            running(),
            expect="unchanged",
        ),
        effect(
            "invalid-vibe.unified_harness_scratchpad-unknown",
            "vibe.unified_harness_scratchpad",
            {"action": "delete", "path": "notes.md"},
            running(),
            expect="unchanged",
        ),
        effect(
            "unknown-mcp-tool",
            "mcp_acme.create_issue",
            {"owner": "acme", "repo": "api"},
            running(),
            expect="unchanged",
        ),
    ]


def _result_cases() -> list[dict[str, Any]]:
    """Every projected tool settling with a result envelope."""

    return [
        effect("result-read_file", "read_file", READ_ARGUMENTS, completed("read_file", READ_RESULT)),
        effect(
            "result-read_file-one-line-truncated",
            "read_file",
            READ_ARGUMENTS,
            completed("read_file", {**READ_RESULT, "lines_read": 1, "was_truncated": True}),
        ),
        effect(
            "result-read_file-image",
            "read_file",
            {"path": "/workspace/logo.png"},
            completed(
                "read_file",
                {"path": "/workspace/logo.png", "content": "", "offset": 0, "lines_read": 0},
                content=[{"type": "image", "data": "aW1hZ2U=", "mimeType": "image/png"}],
            ),
        ),
        effect(
            "result-read_file-receipt",
            "read_file",
            READ_ARGUMENTS,
            {
                "status": "completed",
                "output": {
                    "content": [
                        {"type": "text", "text": "<truncated-output>kept</truncated-output>"}
                    ]
                },
                "display": {"success": True, "verb": "Ran", "message": "read_file"},
            },
        ),
        effect(
            "result-read_file-unreadable",
            "read_file",
            READ_ARGUMENTS,
            completed("read_file", {"content": "no path"}),
            expect="unchanged",
        ),
        effect(
            "result-write_file-created",
            "write_file",
            WRITE_ARGUMENTS,
            completed("write_file", {"path": "/workspace/src/alpha.txt", "file_existed": False}),
        ),
        effect(
            "result-write_file-replaced",
            "write_file",
            WRITE_ARGUMENTS,
            completed(
                "write_file",
                {"path": "/workspace/src/alpha.txt", "file_existed": True},
                meta={WRITE_ANNOTATION: {"previous_content": "older\n"}},
            ),
        ),
        effect(
            "result-edit",
            "edit",
            EDIT_ARGUMENTS,
            completed(
                "edit",
                {**EDIT_RESULT, "warnings": ["the capture authored this warning"]},
                meta=EDIT_META,
            ),
        ),
        effect(
            "result-edit-no-annotations",
            "edit",
            EDIT_ARGUMENTS,
            completed("edit", EDIT_RESULT),
            expect="unchanged",
        ),
        effect("result-grep", "grep", GREP_ARGUMENTS, completed("grep", GREP_RESULT)),
        effect(
            "result-grep-truncated",
            "grep",
            {"pattern": "beta"},
            completed(
                "grep",
                {**GREP_RESULT, "match_count": 3, "was_truncated": True},
            ),
        ),
        effect(
            "result-bash",
            "bash",
            {"command": "echo alpha"},
            completed("bash", {"stdout": "alpha\n", "stderr": "", "returncode": 0}),
        ),
        effect(
            "result-bash-failing",
            "bash",
            {"command": "false"},
            completed("bash", {"stdout": "", "stderr": "nope\n", "returncode": 1}),
        ),
        effect("result-skill.read", "skill.read", {"name": "reviewer"}, completed("skill.read", {"body": "x"})),
        effect(
            "result-process.start",
            "process.start",
            {"command": "sleep 5"},
            completed("process.start", {"processId": PROCESS_ID, "status": "running"}),
        ),
        effect(
            "result-process.stop",
            "process.stop",
            {"processId": PROCESS_ID},
            completed("process.stop", {"processId": PROCESS_ID, "status": "exited", "exitCode": 0}),
        ),
        effect(
            "result-process.output",
            "process.output",
            {"processId": PROCESS_ID},
            completed(
                "process.output",
                {
                    "processId": PROCESS_ID,
                    "status": "running",
                    "output": "tick\n",
                    "bytesAvailable": 2048,
                    "hasMore": True,
                    "nextCursor": 5,
                },
            ),
        ),
        effect(
            "result-process.list",
            "process.list",
            {},
            completed(
                "process.list",
                {"processes": [{"processId": "proc-1", "command": "sleep 5", "status": "running"}]},
            ),
        ),
        effect(
            "result-process.write",
            "process.write",
            {"processId": "proc-1", "text": "y\n"},
            completed(
                "process.write",
                {"processId": "proc-1", "status": "running", "bytesWritten": 3145728},
            ),
        ),
        effect(
            "result-ui.ask_user_question",
            "ui.ask_user_question",
            {"questions": [QUESTION]},
            completed(
                "ui.ask_user_question",
                {"answers": [{"question": "Which target?", "answer": "alpha", "isOther": False}]},
            ),
        ),
        effect(
            "result-ui.ask_user_question-other",
            "ui.ask_user_question",
            {"questions": [QUESTION]},
            completed(
                "ui.ask_user_question",
                {"answers": [{"question": "Which target?", "answer": "gamma", "isOther": True}]},
            ),
        ),
        effect(
            "result-ui.ask_user_question-cancelled",
            "ui.ask_user_question",
            {"questions": [QUESTION]},
            completed("ui.ask_user_question", {"answers": [], "cancelled": True}),
        ),
        effect(
            "result-web_search",
            "web_search",
            {"query": "rust parity oracle"},
            completed(
                "web_search",
                {
                    "query": "rust parity oracle",
                    "answer": "the search answer",
                    "sources": [{"title": "a source", "url": "https://example.invalid/doc"}],
                },
            ),
        ),
        effect(
            "result-web_fetch",
            "web_fetch",
            {"url": "https://example.invalid/doc"},
            completed(
                "web_fetch",
                {
                    "url": "https://example.invalid/doc",
                    "content": "x" * 1500,
                    "content_type": "text/html; charset=utf-8",
                    "was_truncated": True,
                },
            ),
        ),
        effect(
            "result-connector_web_search.web_search",
            "connector_web_search.web_search",
            {"query": "rust parity oracle"},
            completed(
                "connector_web_search.web_search",
                {
                    "ref-a": {"url": "https://example.invalid/b", "title": "second", "rank": 2},
                    "ref-b": {"url": "https://example.invalid/a", "title": "first", "rank": 1},
                },
            ),
        ),
        effect(
            "result-connector_web_search.open_url",
            "connector_web_search.open_url",
            {"url": "https://example.invalid/doc"},
            completed(
                "connector_web_search.open_url",
                {"url": "https://example.invalid/doc", "content": "the page"},
            ),
        ),
        effect(
            "result-todo",
            "todo",
            {"action": "write", "todos": [TODO_ITEM]},
            completed("todo", {"verb": "Updated", "todos": [TODO_ITEM], "total_count": 1}),
        ),
        effect(
            "result-vibe.cron-cancel",
            "vibe.cron",
            {"action": "cancel", "id": "loop-1"},
            completed(
                "vibe.cron",
                {
                    "message": "the capture cancelled it",
                    "loops": [
                        {
                            "id": "loop-1",
                            "prompt": "check the\nbuild",
                            "interval_seconds": 3600,
                            "next_fire_at": 0,
                        }
                    ],
                },
            ),
        ),
        effect(
            "result-vibe.cron-clear",
            "vibe.cron",
            {"action": "clear"},
            completed(
                "vibe.cron", {"message": "the capture cleared them", "loops": [], "cleared_count": 1}
            ),
        ),
        effect(
            "result-vibe.cron-list-empty",
            "vibe.cron",
            {"action": "list"},
            completed("vibe.cron", {"message": "none", "loops": []}),
        ),
        effect(
            "result-vibe.unified_harness_scratchpad-list",
            "vibe.unified_harness_scratchpad",
            {"action": "list"},
            completed(
                "vibe.unified_harness_scratchpad", {"verb": "Listed", "files": ["a.md", "b.md"]}
            ),
        ),
        effect(
            "result-vibe.unified_harness_scratchpad-write",
            "vibe.unified_harness_scratchpad",
            {"action": "write", "path": "notes.md", "content": "remember this"},
            completed(
                "vibe.unified_harness_scratchpad", {"verb": "Saved", "path": "notes.md"}
            ),
        ),
        effect("failed-read_file", "read_file", READ_ARGUMENTS, failed()),
        effect("failed-bash", "bash", {"command": "false"}, failed()),
    ]


def _shell_grep_cases() -> list[dict[str, Any]]:
    """Shell calls the projection may render as a search, and ones it may not."""

    matched = completed(
        "bash", {"stdout": "./src/alpha.txt:1:alpha\nBinary file logo.png matches\n", "returncode": 0}
    )
    return [
        effect("shell-grep-recursive", "bash", {"command": "grep -rn alpha src"}, running()),
        effect("shell-grep-recursive-result", "bash", {"command": "grep -rn alpha src"}, matched),
        effect(
            "shell-grep-no-match-result",
            "bash",
            {"command": "grep -rn zeta src"},
            completed("bash", {"stdout": "", "returncode": 1}),
        ),
        effect("shell-grep-rg-directory", "bash", {"command": "rg -n alpha ."}, running()),
        effect("shell-grep-rg-file", "bash", {"command": "rg -n alpha src/alpha.txt"}, running()),
        effect("shell-grep-rg-file-prefixed", "bash", {"command": "rg -nH alpha src/alpha.txt"}, running()),
        effect("shell-grep-no-line-numbers", "bash", {"command": "grep -r alpha src"}, running()),
        effect("shell-grep-piped", "bash", {"command": "grep -rn alpha src | head"}, running()),
        effect("shell-grep-context", "bash", {"command": "grep -rn -A2 alpha src"}, running()),
        effect(
            "shell-grep-pattern-flag",
            "bash",
            {"command": "grep -rn --include=*.txt -e alpha src"},
            running(),
        ),
    ]


def _path_cases() -> list[dict[str, Any]]:
    """The file tools over the path shapes a header renders differently, with
    the session directory as the display root."""

    shapes = (
        ("inside", "/workspace/src/alpha.txt"),
        ("the-root", "/workspace"),
        ("dotted", "/workspace/src/../alpha.txt"),
        ("sibling-prefix", "/workspace-other/alpha.txt"),
        ("outside", "/elsewhere/alpha.txt"),
        ("relative", "src/alpha.txt"),
    )
    cases: list[dict[str, Any]] = []
    for shape, path in shapes:
        cases.append(
            effect(
                f"path-read_file-{shape}",
                "read_file",
                {"path": path},
                completed("read_file", {**READ_RESULT, "path": path}),
                roots=(ROOT,),
            )
        )
        cases.append(
            effect(
                f"path-write_file-{shape}",
                "write_file",
                {"path": path, "content": "alpha\n"},
                completed("write_file", {"path": path, "file_existed": False}),
                roots=(ROOT,),
            )
        )
        cases.append(
            effect(
                f"path-edit-{shape}",
                "edit",
                {**EDIT_ARGUMENTS, "file_path": path},
                completed("edit", {**EDIT_RESULT, "file": path}, meta=EDIT_META),
                roots=(ROOT,),
            )
        )
        cases.append(
            effect(
                f"path-grep-{shape}",
                "grep",
                {"pattern": "alpha", "path": path},
                running(),
                roots=(ROOT,),
            )
        )
    cases.append(
        effect(
            "path-read_file-second-root",
            "read_file",
            {"path": "/other/alpha.txt"},
            running(),
            roots=(ROOT, "/other"),
        )
    )
    return cases


def _label_cases() -> list[dict[str, Any]]:
    """Namespaced tools that only get a human header."""

    tools = (
        "self.sleep",
        "self.wait",
        "agents.list",
        "agents.send_message",
        "subagent.list",
        "subagent.stop",
        "connector_github_app.create_pull_request",
        "connector_slack.slack_send_message",
    )
    cases: list[dict[str, Any]] = []
    for tool in tools:
        cases.append(effect(f"label-{tool}", tool, {}, running()))
        cases.append(effect(f"label-{tool}-result", tool, {}, completed(tool, {"ok": True})))
    cases.append(
        effect(
            "label-agents.list-result-kept",
            "agents.list",
            {},
            completed("agents.list", {"ok": True}, message="two subagents"),
        )
    )
    return cases


def _callback_cases() -> list[dict[str, Any]]:
    """The call an approval request describes, projected the same way."""

    return [
        callback("callback-read_file", "read_file", READ_ARGUMENTS),
        callback("callback-bash", "bash", {"command": "echo alpha"}),
        callback("callback-agents.list", "agents.list", {}),
        callback("callback-unknown", "mcp_acme.create_issue", {"owner": "acme"}, expect="unchanged"),
    ]


def scenarios() -> list[dict[str, Any]]:
    """Every scenario, by family."""

    return [
        *_call_cases(),
        *_invalid_cases(),
        *_result_cases(),
        *_shell_grep_cases(),
        *_path_cases(),
        *_label_cases(),
        *_callback_cases(),
    ]
