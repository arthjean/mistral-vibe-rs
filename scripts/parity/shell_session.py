#!/usr/bin/env python3
"""Capture what the pinned reference's five managed shell handlers actually do.

Row 6 of the scorecard reads the managed shell, and every number in it was so
far an assertion about declarations: the tool surface knows the five names, the
tool-config census knows their fields, and nothing measured what a session does.
This oracle drives ``ExperimentalBash``, ``BashOutput``, ``BashStdin``,
``BashSessions`` and ``BashLogFile`` over scripted commands and records, per
case, the typed result, the model-facing text the agent loop renders from it,
the outcome kind, and where a case leaves a session behind, the manifest on disk
and the ``SessionInfo`` the manager reports.

Two artifacts come out of a run:

``.parity/shell-session-corpus.json``
    The full capture, gitignored, because a shell result carries
    reference-authored prose (error sentences, kill and reset messages) and
    ``NOTICE`` forbids shipping that.

``crates/vibe-core/tests/shell-session/corpus.json``
    The committed projection, which the Rust replay reads unconditionally. A
    captured string survives verbatim only when it is a value a case supplied, a
    normalized path, a marker, or an identifier-shaped token; everything else is
    committed as ``{"described": "sha256:...", "length": n}``, so the corpus
    carries names, pointers, counts and digests and no reference sentence, while
    a digest still fails the replay on any change.

Every session runs under a ``VIBE_HOME`` inside this run's own temporary
directory: the capture refuses to start when the resolved session directory is
not there, so the user's real ``~/.vibe/shell-tool`` is never read or written,
and it sweeps every terminal it started before it exits, however it exits.

Usage::

    scripts/parity/shell_session.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter when the current one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, RESTORE_COMMAND

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/shell-session-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/shell-session/corpus.json")

#: Where the extracted pinned tree is cached between runs. Gitignored, and keyed
#: by commit so a re-pin extracts a new one instead of reusing the old.
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Stands in for the scenario's temporary root, which is a fresh directory on
#: every run and on every machine. Every absolute path a record carries is
#: relativized against it.
ROOT_PLACEHOLDER = "{root}"

#: Stands in for the throwaway ``HOME`` the capture exports, so a login shell
#: reads no user profile and no home path reaches the corpus.
HOME_PLACEHOLDER = "{home}"

#: The shape a session identifier has, and the marker that replaces it: the
#: prefix the manager mints under, the stamp format it renders, and the length of
#: the uuid4 suffix. The value is volatile; the shape is the contract.
SESSION_PATTERN = re.compile(r"\b(bash)_(\d{8}_\d{6})_([0-9a-f]{8})\b")
SESSION_MARKER = "bash_<stamp:%Y%m%d_%H%M%S>_<hex:8>"

#: The shape ``_now_iso`` renders, and the marker that replaces it. The offset is
#: part of the contract: the manager stamps in UTC and a local-time stamp would
#: be a divergence, so the marker names it rather than hiding it.
TIMESTAMP_PATTERN = re.compile(
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?\+00:00"
)
#: The marker carries no space, so it stays a token and is committed as it
#: stands rather than being read as prose.
TIMESTAMP_MARKER = "<timestamp:%Y-%m-%dT%H:%M:%S[.%f]+00:00>"

#: What a leftover volatile value looks like after normalization. A raw stamp or
#: a raw identifier reaching the corpus would make two runs differ, so the
#: capture fails on one instead of committing it.
LEFTOVER_PATTERNS = (
    re.compile(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}"),
    re.compile(r"\bbash_\d{8}_\d{6}_[0-9a-f]{8}\b"),
)

#: An identifier-shaped token: a status word, an action name, an exception class
#: name. The bright line is that it carries no whitespace and no punctuation
#: beyond the hyphen, so no sentence can pass for one.
_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,31}")

#: The shell every case runs under. The reference resolves zsh first and starts
#: it as a login shell, which would read whichever profile the capturing machine
#: happens to hold; naming bash and exporting a throwaway ``HOME`` is what makes
#: two machines record the same output.
CAPTURE_SHELL = "/bin/bash"

#: The text this repository's own commands emit. A result that only reads one of
#: them back is a value this corpus supplied, not a reference sentence, so it is
#: committed as it stands and stays readable in a divergence report.
AUTHORED_TEXT = frozenset({
    "hello\n",
    "oops\n",
    "late\n",
    "timed\n",
    "clamped\n",
    "shell-override\n",
    "parity-token\n",
    "a\nb\nc\n",
    "héllo ✓\n",
    "log line\n",
    "notes fixture\n",
    "inner fixture\n",
    "annotated\n",
    "appended\n",
    "annotated\nappended\n",
    CAPTURE_SHELL,
    "/bin/sh",
    "posix",
})


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def _git(reference: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git {' '.join(arguments)} failed in {reference}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def resolve_reference(reference: Path, expected: str) -> dict[str, str]:
    """The pinned commit, refusing a checkout sitting anywhere else.

    The corpus is only an oracle for the commit it was captured from, so a
    checkout at another revision is refused by name rather than measured, and
    the restore command is printed with the mismatch.
    """

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE to the checkout "
            "path or pass --reference"
        )
    commit = _git(reference, "rev-parse", "HEAD")
    if commit != expected:
        raise OracleError(
            f"reference commit mismatch: expected {expected}, found {commit}; "
            f"restore it with: {RESTORE_COMMAND}"
        )
    return {"commit": commit, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs.

    ``git archive`` writes the commit's contents without moving HEAD, creating a
    branch or adding a worktree, so the reference checkout is observed and never
    modified.
    """

    import tarfile

    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = tree / "vibe" / "__init__.py"
    if marker.is_file():
        return tree
    tree.mkdir(parents=True, exist_ok=True)
    archive = tree.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(tree, filter="data")
    archive.unlink(missing_ok=True)
    if not marker.is_file():
        raise OracleError(f"the extracted tree at {tree} carries no `vibe` package")
    return tree


def reexecute_with_reference_interpreter(
    reference: Path, override: Path | None, tree: Path
) -> None:
    """Re-runs this script under an interpreter importing the *pinned* tree."""

    if os.environ.get(_REEXEC_MARKER) == str(tree):
        if not _imports_pinned_vibe(tree):
            raise OracleError(
                f"the reference interpreter did not import `vibe` from {tree}"
            )
        return
    candidates = [override] if override else []
    candidates += [
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    interpreter = next((c for c in candidates if c and c.is_file()), None)
    if interpreter is None:
        raise OracleError(
            f"no interpreter can import `vibe`; looked for a virtual environment in {reference}"
        )
    environment = dict(os.environ)
    environment[_REEXEC_MARKER] = str(tree)
    environment["PYTHONPATH"] = os.pathsep.join(
        [str(tree), *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else [])]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


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


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------

#: The five handlers this oracle drives, by the name the reference publishes.
TOOL_NAMES = ("bash", "bash_output", "bash_stdin", "bash_sessions", "bash_log_file")

#: The fixture files every scenario's working directory holds, so a command can
#: read something this repository authored instead of whatever the machine has.
FIXTURES = {
    "notes.txt": "notes fixture\n",
    "nested/inner.txt": "inner fixture\n",
}

_SESSION_REFERENCE = re.compile(r"\{(s|log)(\d+)\}")


def tool_classes() -> dict[str, Any]:
    from vibe.core.tools.builtins.experimental_bash import (
        BashLogFile,
        BashOutput,
        BashSessions,
        BashStdin,
        ExperimentalBash,
    )

    classes = {
        "bash": ExperimentalBash,
        "bash_output": BashOutput,
        "bash_stdin": BashStdin,
        "bash_sessions": BashSessions,
        "bash_log_file": BashLogFile,
    }
    missing = [name for name in TOOL_NAMES if name not in classes]
    if missing:
        raise OracleError(f"the pinned reference publishes no {', '.join(missing)}")
    return classes


def build_tool(cls: Any, cwd: Path, scratchpad: Path, runtime: Any) -> Any:
    """A tool instance bound to the scenario, with declared defaults.

    Only the shell is named: the reference resolves zsh before bash and starts it
    as a login shell, so leaving it to resolution would record whichever profile
    the capturing machine happens to hold. Every other field is the tool's own
    default, because per-tool configuration is EP-031's subject.
    """

    from vibe.core.config.harness_files import HarnessFilesManager

    config_class = cls._get_tool_config_class()
    fields = {"shell": CAPTURE_SHELL} if "shell" in config_class.model_fields else {}
    config = config_class(**fields)
    harness = HarnessFilesManager(sources=("project",)).for_session(cwd)
    return cls.from_config(
        lambda: config,
        cwd=cwd,
        harness_files=harness,
        scratchpad_dir=scratchpad,
        terminal_runtime=runtime,
    )


def assert_hermetic(manager: Any, scenario_root: Path, real_shell_tool: Path) -> None:
    """Refuses to run unless the session directory is this run's own.

    The tools write manifests and logs under ``VIBE_HOME``, and a capture that
    resolved the user's real home would read sessions it did not start and leave
    its own behind. The check runs before the scenario's first case, so a
    misconfigured run starts nothing at all.
    """

    base = Path(manager.base_dir).resolve()
    if not base.is_relative_to(scenario_root.resolve()):
        raise OracleError(
            f"the session directory {base} is not inside this run's root {scenario_root}"
        )
    if base == real_shell_tool.resolve():
        raise OracleError(f"the capture resolved the real session directory {base}")


def resolve_arguments(value: Any, sessions: list[str], scenario_root: Path) -> Any:
    """The authored arguments with this run's identifiers and paths substituted."""

    if isinstance(value, dict):
        return {key: resolve_arguments(item, sessions, scenario_root) for key, item in value.items()}
    if isinstance(value, list):
        return [resolve_arguments(item, sessions, scenario_root) for item in value]
    if not isinstance(value, str):
        return value

    def substitute(match: re.Match[str]) -> str:
        kind, index = match.group(1), int(match.group(2))
        if index >= len(sessions):
            raise OracleError(
                f"a step refers to {match.group(0)} before that session was started"
            )
        session_id = sessions[index]
        return session_id if kind == "s" else f"sessions/{session_id}.log"

    return _SESSION_REFERENCE.sub(substitute, value).replace(
        ROOT_PLACEHOLDER, str(scenario_root)
    )


def referenced_sessions(value: Any, into: list[int]) -> None:
    """Every session index a step's authored arguments name, in order."""

    if isinstance(value, dict):
        for item in value.values():
            referenced_sessions(item, into)
    elif isinstance(value, list):
        for item in value:
            referenced_sessions(item, into)
    elif isinstance(value, str):
        for match in _SESSION_REFERENCE.finditer(value):
            index = int(match.group(2))
            if index not in into:
                into.append(index)


def session_state(manager: Any, session_id: str) -> dict[str, Any]:
    """The two persisted views of one session: the manifest and the reported info.

    Both carry their full key sets, including the null ones, because a field the
    reference writes as ``null`` and this port omits is a divergence a corpus
    that dropped nulls could not see.
    """

    record: dict[str, Any] = {"sessionId": session_id}
    manifest_path = Path(manager.sessions_dir) / f"{session_id}.json"
    try:
        record["manifest"] = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        record["manifest"] = None
    try:
        record["sessionInfo"] = manager.info(session_id).model_dump(mode="json")
    except Exception as error:  # noqa: BLE001 - the refusal is the measurement
        record["sessionInfo"] = None
        record["sessionInfoError"] = type(error).__name__
    return record


async def run_case(
    step: dict[str, Any], tool: Any, arguments: dict[str, Any]
) -> dict[str, Any]:
    """Drives one case and records what an agent loop would have observed."""

    from vibe.core.tools.base import ToolError
    from vibe.core.types import ToolStreamEvent

    outcome: dict[str, Any] = {}
    try:
        result_model = None
        async for item in tool.invoke(None, **arguments):
            if not isinstance(item, ToolStreamEvent):
                result_model = item
        if result_model is None:
            raise ToolError("Tool did not yield a result")
    except Exception as error:  # noqa: BLE001 - the outcome is the measurement
        outcome["outcome"] = "raised"
        outcome["error"] = {"type": type(error).__name__, "message": str(error)}
    else:
        typed = result_model.model_dump(mode="json")
        # Exactly what `_loop.py` sends to the model: the field-per-line
        # rendering plus whatever the tool appends through `get_result_extra`.
        text = "\n".join(f"{key}: {value}" for key, value in typed.items())
        extra = tool.get_result_extra(result_model)
        if extra:
            text += "\n\n" + extra
        outcome["outcome"] = "returned"
        outcome["typedResult"] = typed
        outcome["modelText"] = text
    return outcome


class Sweep:
    """Every terminal the capture started, so none of them outlives the run.

    A managed session is a process group started from this process, and a case
    that times out or a scenario that raises would otherwise leave one running
    against a temporary directory nobody will clean. The terminals are held
    rather than the pids because a reaped child answers ``poll`` and a bare pid
    cannot tell a survivor from a zombie.
    """

    def __init__(self) -> None:
        self._terminals: list[tuple[str, Any]] = []

    def watch(self, session_id: str, terminal: Any) -> None:
        self._terminals.append((session_id, terminal))

    def finish(self) -> list[str]:
        survivors: list[str] = []
        for session_id, terminal in self._terminals:
            if terminal.poll() is None:
                survivors.append(session_id)
                self._force(terminal)
        return survivors

    @staticmethod
    def _force(terminal: Any) -> None:
        pid = getattr(terminal, "pid", None)
        if pid is None:
            return
        try:
            os.killpg(os.getpgid(pid), signal.SIGKILL)
        except OSError:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                return


def session_marker(index: int) -> str:
    """The marker one scenario's session identifier collapses to.

    The prefix and the two shapes are the contract; the tail says which of the
    scenario's sessions this is, so a replay that mints its own identifiers can
    still tell the first session's log from the second's.
    """

    return f"{SESSION_MARKER}#s{index}"


async def run_scenario(
    spec: dict[str, Any],
    root: Path,
    scratchpad: Path,
    real_shell_tool: Path,
    sweep: Sweep,
) -> list[dict[str, Any]]:
    """One scenario: its own session home, its own manager, its own sessions."""

    from vibe.core.tools.terminal_runtime import TerminalRuntime

    name = spec["scenario"]
    scenario_root = (root / name).resolve()
    work = scenario_root / "work"
    for relative, content in FIXTURES.items():
        target = work / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    (scenario_root / "vibe-home").mkdir(parents=True, exist_ok=True)
    os.environ["VIBE_HOME"] = str(scenario_root / "vibe-home")

    classes = tool_classes()
    runtime = TerminalRuntime()
    records: list[dict[str, Any]] = []
    sessions: list[str] = []
    try:
        manager = runtime.get(shell_family="posix", session_prefix="bash")
        assert_hermetic(manager, scenario_root, real_shell_tool)
        tools = {
            tool_name: build_tool(classes[tool_name], work, scratchpad, runtime)
            for tool_name in TOOL_NAMES
        }
        pending: list[dict[str, Any]] = []
        for step in spec["steps"]:
            if "do" in step:
                _perform(step, manager, sessions)
                # The action arranged state this case measures, so it is
                # recorded on the case rather than dropped: a replay that did
                # not wait for the same exit or delete the same log would be
                # measuring a different moment.
                pending.append(step)
                continue
            arguments = resolve_arguments(step["args"], sessions, scenario_root)
            targets: list[int] = []
            referenced_sessions(step["args"], targets)
            outcome = await run_case(step, tools[step["tool"]], arguments)
            for session_id in _started(manager, sessions):
                targets.append(len(sessions))
                sessions.append(session_id)
                live = manager._sessions.get(session_id)  # noqa: SLF001 - the sweep needs the terminal
                if live is not None:
                    sweep.watch(session_id, live.terminal)
            record: dict[str, Any] = {
                "scenario": name,
                "tool": step["tool"],
                "case": step["case"],
                "arguments": step["args"],
                **outcome,
            }
            if pending:
                record["before"] = pending
                pending = []
            states = [session_state(manager, sessions[index]) for index in sorted(set(targets))]
            if states:
                record["sessions"] = states
            records.append(record)
    finally:
        runtime.close()
    return [normalize(record, scenario_root, root, sessions) for record in records]


def _perform(step: dict[str, Any], manager: Any, sessions: list[str]) -> None:
    """A capture action: it arranges state and records nothing."""

    session_id = sessions[step["session"]]
    match step["do"]:
        case "wait":
            # The manager no longer publishes a blocking wait (the reference's
            # `wait_for_exit` is gone at 2.25.7), so the action polls the status
            # `info` refreshes, which leaves `running` at the moment the reader
            # thread finishes, and gives up at the deadline the way the old
            # wait did. The replay's own `wait` polls on the same 25 ms beat.
            deadline = time.monotonic() + float(step["seconds"])
            while (
                manager.info(session_id).status == "running"
                and time.monotonic() < deadline
            ):
                time.sleep(0.025)
        case "delete-log":
            Path(manager.info(session_id).output_path).unlink(missing_ok=True)
        case unsupported:
            raise OracleError(f"unsupported capture action: {unsupported}")


def _started(manager: Any, sessions: list[str]) -> list[str]:
    """The sessions this step added, in the order the manager created them."""

    known = set(sessions)
    return [info.session_id for info in manager.list_sessions() if info.session_id not in known]


# --------------------------------------------------------------------------
# Normalization and the committed projection
# --------------------------------------------------------------------------


def normalize(record: dict[str, Any], scenario_root: Path, root: Path, sessions: list[str]) -> Any:
    """Replaces every volatile value by a marker naming its shape.

    Nothing is dropped: an identifier keeps its prefix, its stamp format and its
    hex length, a timestamp keeps its format and its offset, and a path keeps
    everything below the scenario's temporary root. What is left is the part two
    runs on two machines agree on, which is what a corpus can assert.
    """

    replacements = [(session, session_marker(index)) for index, session in enumerate(sessions)]
    replacements += [(str(scenario_root), ROOT_PLACEHOLDER), (str(root), "{capture}")]
    home = os.environ.get("HOME")
    if home:
        replacements.append((home, HOME_PLACEHOLDER))

    def rewrite(value: Any) -> Any:
        if isinstance(value, dict):
            return {key: rewrite(item) for key, item in value.items()}
        if isinstance(value, list):
            return [rewrite(item) for item in value]
        if not isinstance(value, str):
            return value
        replaced = value
        for original, marker in replacements:
            replaced = replaced.replace(original, marker)
        replaced = TIMESTAMP_PATTERN.sub(TIMESTAMP_MARKER, replaced)
        # A session this scenario did not start: an orphan read off disk, or an
        # identifier a message quoted back. It keeps its shape and loses its
        # value like every other.
        replaced = SESSION_PATTERN.sub(SESSION_MARKER, replaced)
        return replaced.replace("\\", "/") if ROOT_PLACEHOLDER in replaced else replaced

    normalized = rewrite(record)
    _assert_nothing_volatile_survived(normalized, record)
    return normalized


def _assert_nothing_volatile_survived(normalized: Any, record: dict[str, Any]) -> None:
    """Fails the run rather than committing a value the next run will not repeat."""

    def walk(value: Any) -> None:
        if isinstance(value, dict):
            for item in value.values():
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)
        elif isinstance(value, str):
            for pattern in LEFTOVER_PATTERNS:
                found = pattern.search(value)
                if found:
                    raise OracleError(
                        f"case {record['scenario']}/{record['case']} recorded the volatile "
                        f"value {found.group(0)!r}, which no marker covers"
                    )

    walk(normalized)


def digest(value: str) -> str:
    """A string's identity without its content."""

    return "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:32]


def describe(value: str) -> dict[str, Any]:
    """The committable form of a string that may carry reference-authored prose."""

    return {"described": digest(value), "length": len(value)}


def literal_values(value: Any, into: set[str]) -> None:
    """Every string a case's own arguments supplied, at any depth."""

    if isinstance(value, dict):
        for item in value.values():
            literal_values(item, into)
    elif isinstance(value, list):
        for item in value:
            literal_values(item, into)
    elif isinstance(value, str):
        into.add(value)


def keeps_literal(value: str, authored: set[str]) -> bool:
    """Whether a captured string may be committed as it stands.

    Four shapes may: a value this corpus supplied or a slice of one, a
    normalized path, a marker, and an identifier-shaped token such as a status
    word or an exception class name. Every one of those is a name or a pointer,
    which is what ``NOTICE`` allows. Anything else is treated as
    reference-authored prose, including a short error message, and is committed
    as a digest.

    A slice counts because a windowed read answers part of a line this corpus
    wrote: ``max_bytes`` turning ``log line`` into ``log`` says nothing the
    whole string did not already say.
    """

    if not value or value in authored:
        return True
    if " " not in value and "\n" not in value:
        if value.startswith((ROOT_PLACEHOLDER, HOME_PLACEHOLDER, "{capture}")):
            return True
        if "<stamp:" in value or "<timestamp:" in value:
            return True
    if any(value in supplied for supplied in authored):
        return True
    return bool(_IDENTIFIER.fullmatch(value))


def project(value: Any, authored: set[str]) -> Any:
    """The committable form: names and pointers verbatim, prose as a digest."""

    if isinstance(value, dict):
        return {key: project(item, authored) for key, item in value.items()}
    if isinstance(value, list):
        return [project(item, authored) for item in value]
    if isinstance(value, str) and not keeps_literal(value, authored):
        return describe(value)
    return value


def authored_vocabulary() -> set[str]:
    """Every string the scenarios supply, whichever case reads it back.

    A session carries the command that started it into every later result, so
    the vocabulary is the whole capture's, not one case's: ``bash_sessions``
    reporting ``printf 'hello\\n'`` is quoting this file, not the reference.
    """

    supplied: set[str] = set()
    for spec in scenarios():
        for step in spec["steps"]:
            literal_values(step.get("args"), supplied)
    return supplied


def project_case(record: dict[str, Any]) -> dict[str, Any]:
    """One case projected, with its own arguments as the authored vocabulary.

    An error *message* is described unconditionally, never committed: the PRD
    lists byte-identical error text as a non-goal for the same licensing reason
    as tool descriptions, so a message must name the same cause and value rather
    than reproduce the reference's wording. The digest still makes a re-pin that
    reworded one visible in this file's diff, and the full text stays in the
    gitignored artifact for a human to read.
    """

    authored = set(AUTHORED_TEXT) | authored_vocabulary()
    authored |= {ROOT_PLACEHOLDER, HOME_PLACEHOLDER, "{capture}"}
    literal_values(record.get("arguments"), authored)
    verbatim = ("scenario", "tool", "case", "outcome", "arguments", "before")
    projected = {
        key: (value if key in verbatim else project(value, authored))
        for key, value in record.items()
    }
    if isinstance(record.get("error"), dict):
        projected["error"] = {
            "type": record["error"]["type"],
            "message": describe(record["error"].get("message") or ""),
        }
    return projected


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": reference["commit"],
        "platform": platform.system().lower(),
        "note": (
            "Managed shell session corpus: what the pinned reference's five shell handlers "
            "return for each scripted case, with the manifest and the session info each case "
            "leaves behind. A captured string is committed as it stands only when it is a value "
            "these arguments supplied, a normalized path, a shape marker, or an identifier; "
            "everything else, including every error message, is committed as a {described, "
            "length} pair, so no reference prose ships while any change still fails the replay. "
            "Regenerate with scripts/parity/shell_session.py --corpus when the pinned reference "
            "moves."
        ),
        "cases": [project_case(record) for record in records],
    }


# --------------------------------------------------------------------------
# The run
# --------------------------------------------------------------------------


def real_shell_tool_directory() -> Path:
    """The session directory the capture must never read or write.

    Resolved before the run redirects ``VIBE_HOME``, because that is the value
    an ordinary invocation of the reference would use on this machine.
    """

    configured = os.environ.get("VIBE_HOME")
    base = Path(configured) if configured else Path.home() / ".vibe"
    return (base / "shell-tool").resolve()


def snapshot(directory: Path) -> list[str]:
    """Every entry under a directory with its modification time.

    Compared before and after the run: an equal snapshot is the evidence that
    the capture stayed inside its own temporary session home.
    """

    if not directory.is_dir():
        return []
    entries = []
    for path in sorted(directory.rglob("*")):
        try:
            entries.append(f"{path.relative_to(directory)}:{path.stat().st_mtime_ns}")
        except OSError:
            entries.append(f"{path.relative_to(directory)}:unreadable")
    return entries


async def capture(root: Path, real_shell_tool: Path, sweep: Sweep) -> list[dict[str, Any]]:
    """Every scenario, in order, into one flat list of normalized records."""

    scratchpad = root / "scratchpad"
    scratchpad.mkdir(parents=True, exist_ok=True)
    records: list[dict[str, Any]] = []
    for spec in scenarios():
        records.extend(await run_scenario(spec, root, scratchpad, real_shell_tool, sweep))
    return records


def run_capture(root: Path, real_shell_tool: Path) -> list[dict[str, Any]]:
    """The capture with its environment, its sweep and its hermeticity check.

    The sweep runs in ``finally`` so a scenario that raises still takes its
    terminals with it, and a survivor fails the run rather than being reported
    after the fact: a corpus captured while a stray session was writing to a
    session directory is not evidence of anything.
    """

    saved = {name: os.environ.get(name) for name in ("VIBE_HOME", "HOME", "FORCE_COLOR")}
    before = snapshot(real_shell_tool)
    sweep = Sweep()
    try:
        # A login shell reads the invoking user's profile, so the run gets a
        # throwaway one: what `/etc/profile` prints is the machine's, what
        # `~/.bash_profile` prints would be the user's.
        (root / "home").mkdir(parents=True, exist_ok=True)
        os.environ["HOME"] = str(root / "home")
        os.environ.pop("FORCE_COLOR", None)
        return asyncio.run(capture(root, real_shell_tool, sweep))
    finally:
        survivors = sweep.finish()
        for name, value in saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        if survivors:
            raise OracleError(
                "the capture left "
                + ", ".join(survivors)
                + " running; every one was killed, and the run is not evidence"
            )
        after = snapshot(real_shell_tool)
        if after != before:
            raise OracleError(
                f"the capture touched the real session directory {real_shell_tool}; "
                "the corpus is discarded"
            )


def rendered(document: dict[str, Any]) -> str:
    return json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def write_atomically(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_suffix(path.suffix + ".tmp")
    staged.write_text(content, encoding="utf-8")
    os.replace(staged, path)


def parse_arguments(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Capture the pinned reference's managed shell session contract."
    )
    parser.add_argument(
        "--reference",
        type=Path,
        default=Path(os.environ.get("VIBE_REFERENCE", str(DEFAULT_REFERENCE))),
        help="the reference checkout; VIBE_REFERENCE sets it, this wins over both",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="the gitignored full capture",
    )
    parser.add_argument(
        "--corpus",
        type=Path,
        default=DEFAULT_CORPUS,
        help="the committed projection",
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=DEFAULT_CACHE,
        help="where the pinned tree is extracted",
    )
    parser.add_argument(
        "--python",
        type=Path,
        default=None,
        help="an interpreter that can import the pinned tree",
    )
    parser.add_argument(
        "--expected-commit",
        default=EXPECTED_COMMIT,
        help="the pinned commit the checkout must sit on",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="compare the projection against the committed file instead of writing it",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    arguments = parse_arguments(argv)
    try:
        reference = resolve_reference(arguments.reference.resolve(), arguments.expected_commit)
        tree = extract_pinned_tree(
            Path(reference["path"]), reference["commit"], arguments.cache.resolve()
        )
        reexecute_with_reference_interpreter(Path(reference["path"]), arguments.python, tree)
        real_shell_tool = real_shell_tool_directory()
        with tempfile.TemporaryDirectory(prefix="vibe-shell-parity-") as workspace:
            records = run_capture(Path(workspace).resolve(), real_shell_tool)
        document = build_corpus(records, reference)
    except OracleError as failure:
        print(f"shell session oracle: {failure}", file=sys.stderr)
        return 1

    # The gitignored artifact keeps every captured string, so a human can read
    # the message behind a digest without opening the reference.
    full = {**document, "cases": records}
    write_atomically(arguments.output, rendered(full))
    projection = rendered(document)
    if arguments.check:
        if not arguments.corpus.is_file():
            print(f"shell session oracle: no corpus at {arguments.corpus}", file=sys.stderr)
            return 1
        committed = arguments.corpus.read_text(encoding="utf-8")
        if committed != projection:
            print(
                f"shell session oracle: {arguments.corpus} is stale; regenerate it with "
                "scripts/parity/shell_session.py",
                file=sys.stderr,
            )
            return 1
        print(f"corpus matches: {arguments.corpus} ({len(records)} cases)")
        return 0

    write_atomically(arguments.corpus, projection)
    per_tool = {name: sum(1 for r in records if r["tool"] == name) for name in TOOL_NAMES}
    counts = ", ".join(f"{name} {per_tool[name]}" for name in TOOL_NAMES)
    print(
        f"captured {len(records)} cases over {len(scenarios())} scenarios "
        f"from {reference['commit'][:12]} ({counts})\n"
        f"  full capture: {arguments.output}\n"
        f"  committed projection: {arguments.corpus}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
