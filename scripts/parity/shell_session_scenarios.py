"""The scenarios ``shell_session.py`` drives the pinned reference's managed
shell handlers through, and the fixture files their working directory holds.

Every command, argument and fixture here is authored for the corpus; none is
read from the reference.
"""

from __future__ import annotations

from typing import Any


# --------------------------------------------------------------------------
# The scenarios
# --------------------------------------------------------------------------
#
# A scenario is an ordered list of steps sharing one ``TerminalRuntime``, one
# ``VIBE_HOME`` and one working directory, because a session tool only says
# anything against the sessions its own manager holds: `list` has to be able to
# answer "none" in one scenario and "two" in another. A step is either a case,
# which invokes one tool and produces one record, or a capture action, which
# arranges the state the next case measures and produces none.
#
# ``{s0}``, ``{s1}`` name the sessions the scenario started, in creation order;
# ``{log0}`` names the first session's log as a path relative to the session
# directory; ``{root}`` names the scenario's temporary root. A case's arguments
# are committed in this authored form, not the resolved one, so a replay that
# mints its own identifiers can resolve them the same way.


def scenarios() -> list[dict[str, Any]]:
    return [
        {
            "scenario": "foreground",
            "steps": _foreground_steps(),
        },
        {
            "scenario": "polling",
            "steps": _polling_steps(),
        },
        {
            "scenario": "stdin",
            "steps": _stdin_steps(),
        },
        {
            "scenario": "sessions-empty",
            "steps": _sessions_empty_steps(),
        },
        {
            "scenario": "sessions-many",
            "steps": _sessions_many_steps(),
        },
        {
            "scenario": "log-file",
            "steps": _log_file_steps(),
        },
    ]


def _foreground_steps() -> list[dict[str, Any]]:
    return [
        {"tool": "bash", "case": "foreground-exit-zero", "args": {"command": "printf 'hello\n'"}},
        {"tool": "bash", "case": "foreground-reads-fixture", "args": {"command": "cat notes.txt"}},
        {"tool": "bash", "case": "foreground-no-output", "args": {"command": "true"}},
        {
            "tool": "bash",
            "case": "foreground-stderr-only",
            "args": {"command": "printf 'oops\n' >&2"},
        },
        {
            "tool": "bash",
            "case": "foreground-multiline-output",
            "args": {"command": "printf 'a\nb\nc\n'"},
        },
        {
            "tool": "bash",
            "case": "foreground-multibyte-output",
            "args": {"command": "printf 'héllo ✓\n'"},
        },
        {
            "tool": "bash",
            "case": "foreground-output-exceeds-window",
            "args": {"command": "seq 1 4000"},
        },
        {"tool": "bash", "case": "foreground-exit-non-zero", "args": {"command": "exit 3"}},
        {
            "tool": "bash",
            "case": "foreground-cwd-override",
            "args": {"command": "cat inner.txt", "cwd": "{root}/work/nested"},
        },
        {
            "tool": "bash",
            "case": "foreground-cwd-is-not-a-directory",
            "args": {"command": "true", "cwd": "{root}/work/absent"},
        },
        {
            "tool": "bash",
            "case": "foreground-env-override",
            "args": {
                "command": "printf '%s\n' \"$VIBE_PARITY_TOKEN\"",
                "env": {"VIBE_PARITY_TOKEN": "parity-token"},
            },
        },
        {
            "tool": "bash",
            "case": "foreground-shell-override",
            "args": {"command": "printf 'shell-override\n'", "shell": "/bin/sh"},
        },
        {
            "tool": "bash",
            "case": "foreground-shell-override-not-executable",
            "args": {"command": "true", "shell": "/bin/no-such-shell"},
        },
        {"tool": "bash", "case": "foreground-empty-command", "args": {"command": "   "}},
        {
            "tool": "bash",
            "case": "foreground-legacy-timeout-field",
            "args": {"command": "printf 'timed\n'", "timeout": 5},
        },
        {
            "tool": "bash",
            "case": "foreground-timeout-clamped-to-maximum",
            "args": {"command": "printf 'clamped\n'", "timeout_seconds": 900},
        },
        {
            "tool": "bash",
            "case": "background-returns-immediately",
            "args": {"command": "sleep 1", "background": True},
        },
        {
            "tool": "bash",
            "case": "soft-timeout-backgrounds-the-session",
            "args": {"command": "sleep 5", "timeout_seconds": 0.2},
        },
        {
            "tool": "bash",
            "case": "hard-timeout-kills-the-session",
            "args": {"command": "sleep 5", "timeout_seconds": 0.2, "hard_timeout": True},
        },
    ]


def _polling_steps() -> list[dict[str, Any]]:
    return [
        {
            "tool": "bash",
            "case": "background-poll-source",
            "args": {"command": "sleep 1; printf 'late\n'", "background": True},
        },
        {"tool": "bash_output", "case": "poll-before-any-output", "args": {"session_id": "{s0}"}},
        {
            "tool": "bash_output",
            "case": "poll-before-any-output-with-max-bytes",
            "args": {"session_id": "{s0}", "max_bytes": 4},
        },
        {
            "tool": "bash_output",
            "case": "poll-unknown-session",
            "args": {"session_id": "bash_unknown"},
        },
        {
            "tool": "bash_output",
            "case": "poll-negative-cursor",
            "args": {"session_id": "{s0}", "cursor": -1},
        },
        {
            "tool": "bash_output",
            "case": "poll-zero-max-bytes",
            "args": {"session_id": "{s0}", "max_bytes": 0},
        },
        {
            "tool": "bash_output",
            "case": "poll-waits-for-the-session-to-exit",
            "args": {"session_id": "{s0}", "wait_seconds": 5},
        },
        {"do": "wait", "session": 0, "seconds": 10},
        {
            "tool": "bash_output",
            "case": "poll-after-completion",
            "args": {"session_id": "{s0}", "cursor": 0},
        },
        {
            "tool": "bash_output",
            "case": "poll-from-a-cursor",
            "args": {"session_id": "{s0}", "cursor": 3},
        },
        {
            "tool": "bash_output",
            "case": "poll-from-a-cursor-past-the-end",
            "args": {"session_id": "{s0}", "cursor": 10000},
        },
        {
            "tool": "bash_output",
            "case": "poll-with-max-bytes",
            "args": {"session_id": "{s0}", "max_bytes": 2},
        },
        {
            "tool": "bash_output",
            "case": "poll-with-the-max-chars-alias",
            "args": {"session_id": "{s0}", "max_chars": 3},
        },
        {
            "tool": "bash_output",
            "case": "poll-wait-clamped-to-the-maximum",
            "args": {"session_id": "{s0}", "wait_seconds": 900},
        },
    ]


def _stdin_steps() -> list[dict[str, Any]]:
    return [
        {
            "tool": "bash",
            "case": "background-input-target",
            "args": {"command": "sleep 5", "background": True},
        },
        {
            "tool": "bash_stdin",
            "case": "write-text-to-a-running-session",
            "args": {"session_id": "{s0}", "text": "ping\n"},
        },
        {
            "tool": "bash_stdin",
            "case": "write-multibyte-text",
            "args": {"session_id": "{s0}", "text": "héllo\n"},
        },
        {
            "tool": "bash_stdin",
            "case": "write-base64-bytes",
            "args": {"session_id": "{s0}", "bytes_base64": "cG9uZwo="},
        },
        {
            "tool": "bash_stdin",
            "case": "write-a-control-key",
            "args": {"session_id": "{s0}", "control": ["enter"]},
        },
        {
            "tool": "bash_stdin",
            "case": "write-two-control-keys",
            "args": {"session_id": "{s0}", "control": ["ctrl_a", "ctrl_e"]},
        },
        {
            "tool": "bash_stdin",
            "case": "write-an-empty-text",
            "args": {"session_id": "{s0}", "text": ""},
        },
        {"tool": "bash_stdin", "case": "write-nothing", "args": {"session_id": "{s0}"}},
        {
            "tool": "bash_stdin",
            "case": "write-text-and-control-together",
            "args": {"session_id": "{s0}", "text": "x", "control": ["ctrl_c"]},
        },
        {
            "tool": "bash_stdin",
            "case": "write-an-unknown-control-key",
            "args": {"session_id": "{s0}", "control": ["ctrl_zzz"]},
        },
        {
            "tool": "bash_stdin",
            "case": "write-invalid-base64",
            "args": {"session_id": "{s0}", "bytes_base64": "not base64!"},
        },
        {
            "tool": "bash_stdin",
            "case": "write-to-an-unknown-session",
            "args": {"session_id": "bash_unknown", "text": "x"},
        },
        {"tool": "bash", "case": "foreground-input-target-exits", "args": {"command": "true"}},
        {
            "tool": "bash_stdin",
            "case": "write-to-a-session-that-exited",
            "args": {"session_id": "{s1}", "text": "x"},
        },
    ]


def _sessions_empty_steps() -> list[dict[str, Any]]:
    return [
        {"tool": "bash_sessions", "case": "list-with-no-session", "args": {}},
        {"tool": "bash_sessions", "case": "list-explicitly", "args": {"action": "list"}},
        {"tool": "bash_sessions", "case": "inspect-without-a-session-id", "args": {"action": "inspect"}},
        {
            "tool": "bash_sessions",
            "case": "inspect-an-unknown-session",
            "args": {"action": "inspect", "session_id": "bash_unknown"},
        },
        {"tool": "bash_sessions", "case": "kill-without-a-session-id", "args": {"action": "kill"}},
        {
            "tool": "bash_sessions",
            "case": "kill-an-unknown-session",
            "args": {"action": "kill", "session_id": "bash_unknown"},
        },
        {"tool": "bash_sessions", "case": "reset-with-no-session", "args": {"action": "reset"}},
        {
            "tool": "bash_sessions",
            "case": "reset-with-clear-logs-and-no-session",
            "args": {"action": "reset", "clear_logs": True},
        },
        {
            "tool": "bash_sessions",
            "case": "unsupported-action",
            "args": {"action": "purge"},
        },
    ]


def _sessions_many_steps() -> list[dict[str, Any]]:
    return [
        {
            "tool": "bash",
            "case": "background-first-session",
            "args": {"command": "sleep 30", "background": True},
        },
        {
            "tool": "bash",
            "case": "background-second-session",
            "args": {"command": "sleep 30", "background": True},
        },
        {"tool": "bash_sessions", "case": "list-two-sessions", "args": {"action": "list"}},
        {
            "tool": "bash_sessions",
            "case": "inspect-a-running-session",
            "args": {"action": "inspect", "session_id": "{s0}"},
        },
        {
            "tool": "bash_sessions",
            "case": "inspect-with-max-bytes",
            "args": {"action": "inspect", "session_id": "{s0}", "max_bytes": 4},
        },
        {
            "tool": "bash_sessions",
            "case": "inspect-with-the-max-chars-alias",
            "args": {"action": "inspect", "session_id": "{s0}", "max_chars": 4},
        },
        {
            "tool": "bash_sessions",
            "case": "kill-a-running-session",
            "args": {"action": "kill", "session_id": "{s0}"},
        },
        {
            "tool": "bash_sessions",
            "case": "inspect-a-killed-session",
            "args": {"action": "inspect", "session_id": "{s0}"},
        },
        {
            "tool": "bash_sessions",
            "case": "kill-an-already-killed-session",
            "args": {"action": "kill", "session_id": "{s0}"},
        },
        {"tool": "bash_sessions", "case": "list-after-a-kill", "args": {"action": "list"}},
        {"tool": "bash_sessions", "case": "reset-the-remaining-session", "args": {"action": "reset"}},
        {"tool": "bash_sessions", "case": "list-after-a-reset", "args": {"action": "list"}},
        {
            "tool": "bash_sessions",
            "case": "reset-and-clear-the-logs",
            "args": {"action": "reset", "clear_logs": True},
        },
        {"tool": "bash_sessions", "case": "list-after-clearing-the-logs", "args": {"action": "list"}},
    ]


def _log_file_steps() -> list[dict[str, Any]]:
    return [
        {
            "tool": "bash",
            "case": "foreground-log-source",
            "args": {"command": "printf 'log line\n'"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-a-session-log-from-the-start",
            "args": {"action": "read", "session_id": "{s0}"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-with-max-bytes",
            "args": {"action": "read", "session_id": "{s0}", "max_bytes": 3},
        },
        {
            "tool": "bash_log_file",
            "case": "read-with-the-max-chars-alias",
            "args": {"action": "read", "session_id": "{s0}", "max_chars": 4},
        },
        {
            "tool": "bash_log_file",
            "case": "read-from-an-offset",
            "args": {"action": "read", "session_id": "{s0}", "offset": 4},
        },
        {
            "tool": "bash_log_file",
            "case": "read-from-an-offset-past-the-end",
            "args": {"action": "read", "session_id": "{s0}", "offset": 9999},
        },
        {
            "tool": "bash_log_file",
            "case": "read-a-negative-offset",
            "args": {"action": "read", "session_id": "{s0}", "offset": -1},
        },
        {
            "tool": "bash_log_file",
            "case": "read-by-relative-path",
            "args": {"action": "read", "relative_path": "{log0}"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-outside-the-session-directory",
            "args": {"action": "read", "relative_path": "../../etc/hostname"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-another-family-session-log",
            "args": {"action": "read", "relative_path": "sessions/pwsh_20200101_000000_aaaaaaaa.log"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-without-a-session-or-a-path",
            "args": {"action": "read"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-an-unknown-session-log",
            "args": {"action": "read", "session_id": "bash_unknown"},
        },
        {
            "tool": "bash_log_file",
            "case": "write-an-annotation",
            "args": {"action": "write", "relative_path": "notes/annotation.txt", "content": "annotated\n"},
        },
        {
            "tool": "bash_log_file",
            "case": "append-to-an-annotation",
            "args": {"action": "append", "relative_path": "notes/annotation.txt", "content": "appended\n"},
        },
        {
            "tool": "bash_log_file",
            "case": "read-the-annotation-back",
            "args": {"action": "read", "relative_path": "notes/annotation.txt"},
        },
        {
            "tool": "bash_log_file",
            "case": "write-without-content",
            "args": {"action": "write", "relative_path": "notes/annotation.txt"},
        },
        {
            "tool": "bash",
            "case": "background-live-log-source",
            "args": {"command": "sleep 30", "background": True},
        },
        {
            "tool": "bash_log_file",
            "case": "write-to-a-live-session-log",
            "args": {"action": "write", "relative_path": "{log1}", "content": "x"},
        },
        {"do": "delete-log", "session": 0},
        {
            "tool": "bash_log_file",
            "case": "read-a-log-that-was-deleted",
            "args": {"action": "read", "session_id": "{s0}"},
        },
    ]


#: The fixture files every scenario's working directory holds, so a command can
#: read something this repository authored instead of whatever the machine has.
FIXTURES = {
    "notes.txt": "notes fixture\n",
    "nested/inner.txt": "inner fixture\n",
}
