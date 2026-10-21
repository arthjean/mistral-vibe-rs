"""The cases ``tool_execution.py`` drives the pinned reference's tools over.

A case names a tool, the arguments it is called with, whether it writes, and
the script its collaborators follow: the skills a manager holds, a user's
answers, a subagent's run, what the loopback server responds. Every value here
is authored for the corpus.
"""

from __future__ import annotations

from typing import Any

#: Stands in for the loopback server's ``host:port``, which is an ephemeral port
#: chosen by the kernel on every run. A case writes it into its URL and the
#: capture substitutes the live authority in, so no port number reaches the
#: corpus and a re-run on another port is byte-identical.
SERVER_PLACEHOLDER = "{server}"


def cases() -> list[dict[str, Any]]:
    """Every execution case, ordered by tool then by name.

    ``mutates`` marks a case that writes, so the tree is materialized fresh for
    it and one case can never observe another's side effect.
    """

    return [
        # -- read_file ---------------------------------------------------
        {"tool": "read_file", "case": "whole-file", "args": {"file_path": "alpha.txt"}},
        {"tool": "read_file", "case": "offset", "args": {"file_path": "alpha.txt", "offset": 3}},
        {"tool": "read_file", "case": "limit", "args": {"file_path": "alpha.txt", "limit": 2}},
        {
            "tool": "read_file",
            "case": "offset-and-limit",
            "args": {"file_path": "alpha.txt", "offset": 2, "limit": 2},
        },
        {
            "tool": "read_file",
            "case": "offset-past-the-end",
            "args": {"file_path": "alpha.txt", "offset": 99},
        },
        {
            "tool": "read_file",
            "case": "offset-one-past-the-last-line",
            "args": {"file_path": "alpha.txt", "offset": 6},
        },
        {"tool": "read_file", "case": "empty-file", "args": {"file_path": "empty.txt"}},
        {"tool": "read_file", "case": "crlf-file", "args": {"file_path": "crlf.txt"}},
        {"tool": "read_file", "case": "nested-file", "args": {"file_path": "nested/beta.py"}},
        {"tool": "read_file", "case": "missing-file", "args": {"file_path": "nowhere.txt"}},
        {"tool": "read_file", "case": "a-directory", "args": {"file_path": "nested"}},
        {"tool": "read_file", "case": "empty-path", "args": {"file_path": ""}},
        # -- grep --------------------------------------------------------
        {"tool": "grep", "case": "lowercase-is-case-insensitive", "args": {"pattern": "gather"}},
        {"tool": "grep", "case": "uppercase-is-case-sensitive", "args": {"pattern": "Gather"}},
        {"tool": "grep", "case": "no-match", "args": {"pattern": "zzz-absent-zzz"}},
        {
            "tool": "grep",
            "case": "scoped-to-a-subdirectory",
            "args": {"pattern": "gather", "path": "nested"},
        },
        {
            "tool": "grep",
            "case": "scoped-to-one-file",
            "args": {"pattern": "value", "path": "nested/beta.py"},
        },
        {"tool": "grep", "case": "max-matches", "args": {"pattern": "value", "max_matches": 2}},
        {
            "tool": "grep",
            "case": "ignore-honored",
            "args": {"pattern": "gather", "use_default_ignore": True},
        },
        {
            "tool": "grep",
            "case": "ignore-disabled",
            "args": {"pattern": "gather", "use_default_ignore": False},
        },
        {"tool": "grep", "case": "invalid-regex", "args": {"pattern": "gather("}},
        {
            "tool": "grep",
            "case": "missing-path",
            "args": {"pattern": "gather", "path": "nowhere"},
        },
        {"tool": "grep", "case": "regex-alternation", "args": {"pattern": "gather|scatter"}},
        {"tool": "grep", "case": "anchored", "args": {"pattern": "^def "}},
        # -- write_file --------------------------------------------------
        {
            "tool": "write_file",
            "case": "new-file",
            "mutates": True,
            "args": {"file_path": "written.txt", "content": "written body\n"},
        },
        {
            "tool": "write_file",
            "case": "existing-file-is-refused",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "content": "overwrite\n"},
        },
        {
            "tool": "write_file",
            "case": "missing-parent",
            "mutates": True,
            "args": {"file_path": "made/up/deep.txt", "content": "deep body\n"},
        },
        {
            "tool": "write_file",
            "case": "empty-content",
            "mutates": True,
            "args": {"file_path": "blank.txt", "content": ""},
        },
        # -- edit --------------------------------------------------------
        {
            "tool": "edit",
            "case": "single-replacement",
            "mutates": True,
            "args": {
                "file_path": "alpha.txt",
                "old_string": "alpha two",
                "new_string": "alpha II",
            },
        },
        {
            "tool": "edit",
            "case": "replace-all",
            "mutates": True,
            "args": {
                "file_path": "alpha.txt",
                "old_string": "alpha",
                "new_string": "ALPHA",
                "replace_all": True,
            },
        },
        {
            "tool": "edit",
            "case": "ambiguous-without-replace-all",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "alpha", "new_string": "ALPHA"},
        },
        {
            "tool": "edit",
            "case": "absent-old-string",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "absent", "new_string": "x"},
        },
        {
            "tool": "edit",
            "case": "identical-strings",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "alpha one", "new_string": "alpha one"},
        },
        {
            "tool": "edit",
            "case": "empty-old-string",
            "mutates": True,
            "args": {"file_path": "alpha.txt", "old_string": "", "new_string": "x"},
        },
        {
            "tool": "edit",
            "case": "crlf-is-preserved",
            "mutates": True,
            "args": {
                "file_path": "crlf.txt",
                "old_string": "second line",
                "new_string": "changed line",
            },
        },
        {
            "tool": "edit",
            "case": "missing-file",
            "mutates": True,
            "args": {"file_path": "nowhere.txt", "old_string": "a", "new_string": "b"},
        },
        # -- todo --------------------------------------------------------
        {
            "tool": "todo",
            "case": "write-two",
            "args": {
                "action": "write",
                "todos": [
                    {"id": "one", "content": "first task", "status": "pending"},
                    {"id": "two", "content": "second task", "status": "in_progress"},
                ],
            },
        },
        {"tool": "todo", "case": "read-empty", "args": {"action": "read"}},
        {
            "tool": "todo",
            "case": "duplicate-identifiers",
            "args": {
                "action": "write",
                "todos": [
                    {"id": "same", "content": "first", "status": "pending"},
                    {"id": "same", "content": "second", "status": "pending"},
                ],
            },
        },
        {
            "tool": "todo",
            "case": "write-empty-list",
            "args": {"action": "write", "todos": []},
        },
        {"tool": "todo", "case": "unknown-action", "args": {"action": "delete"}},
        *_context_cases(),
        *_network_cases(),
    ]


#: The fixture skills the committed tree carries, described here rather than
#: read out of a manifest so the corpus states what the capture offered.
_SMALL_SKILL = {
    "name": "small",
    "directory": "skills/small",
    "description": "A fixture skill carrying one extra file.",
}
_LARGE_SKILL = {
    "name": "large",
    "directory": "skills/large",
    "description": "A fixture skill carrying more files than the listing cap.",
}
_DETACHED_SKILL = {
    "name": "detached",
    "description": "A fixture skill with no directory on disk.",
    "prompt": "Fixture instructions with no base directory.",
}
_BOTH_SKILLS = [_SMALL_SKILL, _LARGE_SKILL]


def _context_cases() -> list[dict[str, Any]]:
    """The four tools that answer from a collaborator rather than from disk.

    Each one is driven from a scripted collaborator: a skill manager holding
    fixture skills, a user who answers by option index, an agent manager on a
    named builtin profile, and a subagent runner that returns a declared run.
    No model is called and no terminal is attached.
    """

    return [
        # -- skill -------------------------------------------------------
        {
            "tool": "skill",
            "case": "fewer-files-than-the-cap",
            "args": {"name": "small"},
            "script": {"skills": _BOTH_SKILLS},
        },
        {
            "tool": "skill",
            "case": "more-files-than-the-cap",
            "args": {"name": "large"},
            "script": {"skills": _BOTH_SKILLS},
        },
        {
            "tool": "skill",
            "case": "already-loaded-earlier",
            "args": {"name": "small"},
            "script": {"skills": _BOTH_SKILLS, "loaded": ["small"]},
        },
        {
            "tool": "skill",
            "case": "no-directory-on-disk",
            "args": {"name": "detached"},
            "script": {"skills": [_DETACHED_SKILL]},
        },
        {
            "tool": "skill",
            "case": "unknown-name",
            "args": {"name": "absent"},
            "script": {"skills": _BOTH_SKILLS},
        },
        {
            "tool": "skill",
            "case": "unknown-name-with-none-available",
            "args": {"name": "absent"},
            "script": {"skills": []},
        },
        {"tool": "skill", "case": "no-skill-manager", "args": {"name": "small"}, "script": {}},
        # -- ask_user_question -------------------------------------------
        # The argument keys are the published ones. `vibe/questions.py`
        # configures `alias_generator=to_camel` with `populate_by_name`, so
        # the reference accepts both spellings but only the camel one
        # appears in the schema a model reads. Driving the snake spelling
        # would measure an argument no caller can send.
        {
            "tool": "ask_user_question",
            "case": "single-select",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"answers": [{"option": 0}]},
        },
        {
            "tool": "ask_user_question",
            "case": "multi-select",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture parts?",
                        "header": "Parts",
                        "multiSelect": True,
                        "options": [
                            {"label": "First"},
                            {"label": "Second"},
                            {"label": "Third"},
                        ],
                    }
                ]
            },
            "script": {"answers": [{"options": [0, 2]}]},
        },
        {
            "tool": "ask_user_question",
            "case": "other-free-text",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"answers": [{"other": "A typed fixture answer."}]},
        },
        {
            "tool": "ask_user_question",
            "case": "cancelled",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"cancelled": True},
        },
        {
            "tool": "ask_user_question",
            "case": "two-questions",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    },
                    {
                        "question": "Which fixture depth?",
                        "header": "Depth",
                        "options": [{"label": "Shallow"}, {"label": "Deep"}],
                    },
                ],
                "footerNote": "A fixture footer.",
            },
            "script": {"answers": [{"option": 1}, {"option": 0}]},
        },
        {
            "tool": "ask_user_question",
            "case": "other-hidden",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "hideOther": True,
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {"answers": [{"option": 1}]},
        },
        {
            "tool": "ask_user_question",
            "case": "options-with-descriptions",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [
                            {"label": "First", "description": "The first fixture branch."},
                            {"label": "Second", "description": "The second fixture branch."},
                        ],
                    }
                ]
            },
            "script": {"answers": [{"option": 0}]},
        },
        {
            "tool": "ask_user_question",
            "case": "no-interaction-source",
            "args": {
                "questions": [
                    {
                        "question": "Which fixture branch?",
                        "header": "Branch",
                        "options": [{"label": "First"}, {"label": "Second"}],
                    }
                ]
            },
            "script": {},
        },
        # -- exit_plan_mode ----------------------------------------------
        {
            "tool": "exit_plan_mode",
            "case": "clear-context-and-auto-approve",
            "args": {},
            "script": {
                "agent": "plan",
                "planFile": "plan.md",
                "clearContext": True,
                "answers": [{"option": 0}],
            },
        },
        {
            "tool": "exit_plan_mode",
            "case": "clear-context-without-a-callback",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 0}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "auto-approve",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 1}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "manual-approval",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 2}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "declined",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "answers": [{"option": 3}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "other-feedback",
            "args": {},
            "script": {
                "agent": "plan",
                "planFile": "plan.md",
                "answers": [{"other": "A typed fixture objection."}],
            },
        },
        {
            "tool": "exit_plan_mode",
            "case": "cancelled",
            "args": {},
            "script": {"agent": "plan", "planFile": "plan.md", "cancelled": True},
        },
        {
            "tool": "exit_plan_mode",
            "case": "no-plan-file",
            "args": {},
            "script": {"agent": "plan", "answers": [{"option": 1}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "not-in-plan-mode",
            "args": {},
            "script": {"agent": "ask", "answers": [{"option": 0}]},
        },
        {
            "tool": "exit_plan_mode",
            "case": "no-interaction-source",
            "args": {},
            "script": {"agent": "plan"},
        },
        {"tool": "exit_plan_mode", "case": "no-agent-manager", "args": {}, "script": {}},
        # -- task --------------------------------------------------------
        {
            "tool": "task",
            "case": "completed-in-one-turn",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "ask",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "completed-in-several-turns",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "ask",
                "runner": {"response": "A fixture finding.", "turns": 3, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "ended-incomplete",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "ask",
                "runner": {"response": "A partial finding.", "turns": 2, "completed": False},
            },
        },
        {
            "tool": "task",
            "case": "empty-response",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "ask",
                "runner": {"response": "", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "streamed-progress",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "ask",
                "runner": {
                    "response": "A fixture finding.",
                    "turns": 2,
                    "completed": True,
                    "stream": ["read_file: a fixture progress line"],
                },
            },
        },
        {
            "tool": "task",
            "case": "default-agent-name",
            "args": {"task": "Inspect the fixture tree."},
            "script": {
                "agent": "ask",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "unknown-agent",
            "args": {"task": "Inspect the fixture tree.", "agent": "absent"},
            "script": {
                "agent": "ask",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "not-a-subagent",
            "args": {"task": "Inspect the fixture tree.", "agent": "plan"},
            "script": {
                "agent": "ask",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {
            "tool": "task",
            "case": "no-subagent-runner",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {"agent": "ask"},
        },
        {
            "tool": "task",
            "case": "already-inside-a-subagent",
            "args": {"task": "Inspect the fixture tree.", "agent": "explore"},
            "script": {
                "agent": "explore",
                "runner": {"response": "A fixture finding.", "turns": 1, "completed": True},
            },
        },
        {"tool": "task", "case": "no-agent-manager", "args": {"task": "Inspect."}, "script": {}},
    ]


#: The one response body the oversized case needs, declared rather than spelled
#: out: 3_200 repetitions of a 40-byte line exceed the 120_000-byte cap.
_OVERSIZED_BODY = {"unit": "A fixture line of exactly forty bytes.\n\n", "count": 3200}

#: One search response, parameterized by the entry content the case exercises.
def _conversation(content: Any, entries: int = 1) -> dict[str, Any]:
    return {
        "conversation_id": "conversation-1",
        "object": "conversation.response",
        "outputs": [
            {
                "object": "entry",
                "type": "message.output",
                "role": "assistant",
                "content": content,
            }
            for _ in range(entries)
        ],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


def _citation(title: str, url: str | None) -> dict[str, Any]:
    chunk: dict[str, Any] = {"type": "tool_reference", "tool": "web_search", "title": title}
    if url is not None:
        chunk["url"] = url
    return chunk


def _network_cases() -> list[dict[str, Any]]:
    """The two tools that speak HTTP, driven against the loopback server.

    Every URL is written with ``{server}`` and the capture substitutes the
    ephemeral authority in, so the corpus carries no port number and a re-run on
    another port is byte-identical. The socket guard refuses every other
    destination, so nothing here can silently become a live request.
    """

    return [
        # -- web_fetch ---------------------------------------------------
        {
            "tool": "web_fetch",
            "case": "an-html-page",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.html"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/html; charset=utf-8"},
                        "body": "<html><body><h1>Fixture</h1><p>A fixture paragraph.</p></body></html>",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-plain-text-page",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "A fixture paragraph.",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-json-body",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.json"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": {"fixture": True, "count": 2},
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "larger-than-max-content-bytes",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/large.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "bodyRepeat": _OVERSIZED_BODY,
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-redirect-chain",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/first"},
            "script": {
                "responses": [
                    {"status": 302, "reason": "Found", "headers": {"Location": "/second"}},
                    {"status": 302, "reason": "Found", "headers": {"Location": "/third"}},
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "The fixture destination.",
                    },
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "not-found",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/absent"},
            "script": {
                "responses": [
                    {
                        "status": 404,
                        "reason": "Not Found",
                        "headers": {"Content-Type": "text/plain"},
                        "body": "absent",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-challenge-then-success",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/guarded"},
            "script": {
                "responses": [
                    {
                        "status": 403,
                        "reason": "Forbidden",
                        "headers": {"cf-mitigated": "challenge"},
                        "body": "",
                    },
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "The fixture behind the challenge.",
                    },
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-challenge-that-persists",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/guarded"},
            "script": {
                "responses": [
                    {
                        "status": 403,
                        "reason": "Forbidden",
                        "headers": {"cf-mitigated": "challenge"},
                        "body": "",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "forbidden-without-the-challenge-header",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/guarded"},
            "script": {
                "responses": [{"status": 403, "reason": "Forbidden", "body": ""}]
            },
        },
        {
            "tool": "web_fetch",
            "case": "a-server-error",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/broken"},
            "script": {
                "responses": [
                    {"status": 500, "reason": "Internal Server Error", "body": ""}
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "an-empty-body",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/empty.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "",
                    }
                ]
            },
        },
        {
            "tool": "web_fetch",
            "case": "no-content-type",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/bare"},
            "script": {"responses": [{"status": 200, "body": "A bare fixture body."}]},
        },
        {
            "tool": "web_fetch",
            "case": "a-url-with-no-scheme",
            "args": {"url": f"{SERVER_PLACEHOLDER}/page.txt"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "text/plain; charset=utf-8"},
                        "body": "A fixture paragraph.",
                    }
                ]
            },
        },
        {"tool": "web_fetch", "case": "an-empty-url", "args": {"url": ""}, "script": {}},
        {
            "tool": "web_fetch",
            "case": "an-unsupported-scheme",
            "args": {"url": "ftp://fixture.invalid/page.txt"},
            "script": {},
        },
        {
            "tool": "web_fetch",
            "case": "a-timeout-above-the-cap",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.txt", "timeout": 300},
            "script": {},
        },
        {
            "tool": "web_fetch",
            "case": "a-non-positive-timeout",
            "args": {"url": f"http://{SERVER_PLACEHOLDER}/page.txt", "timeout": 0},
            "script": {},
        },
        # -- web_search --------------------------------------------------
        {
            "tool": "web_search",
            "case": "a-string-content-answer",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation("A fixture answer."),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "chunked-content-with-citations",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation(
                            [
                                {"type": "text", "text": "A fixture answer."},
                                _citation("The first source", "https://fixture.invalid/one"),
                                _citation("The second source", "https://fixture.invalid/two"),
                            ]
                        ),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "duplicate-citation-urls",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation(
                            [
                                {"type": "text", "text": "A fixture answer."},
                                _citation("The first source", "https://fixture.invalid/one"),
                                _citation("The same source again", "https://fixture.invalid/one"),
                            ]
                        ),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "a-citation-with-no-url",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation(
                            [
                                {"type": "text", "text": "A fixture answer."},
                                _citation("The unlinked source", None),
                            ]
                        ),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "several-message-outputs",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation("A fixture answer.", entries=2),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "no-text-in-the-response",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 200,
                        "headers": {"Content-Type": "application/json"},
                        "json": _conversation([]),
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "a-non-2xx-status",
            "args": {"query": "the fixture question"},
            "script": {
                "responses": [
                    {
                        "status": 500,
                        "reason": "Internal Server Error",
                        "headers": {"Content-Type": "application/json"},
                        "json": {"message": "a fixture failure"},
                    }
                ]
            },
        },
        {
            "tool": "web_search",
            "case": "no-api-key",
            "args": {"query": "the fixture question"},
            "script": {"apiKeyVariable": "VIBE_PARITY_ABSENT_KEY", "responses": []},
        },
    ]
