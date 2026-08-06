"""Capture the EP-006 startup routing the pinned Python reference performs.

The corpus records, for one set of command-line arguments, which mode the
reference enters and what it does once the UI is mounted. Both are measured by
running reference code rather than by restating the rule it implements:

- the route comes from ``vibe.cli.cli.run_cli``, executed with its I/O
  boundaries replaced so the branch it takes is observable;
- the startup options come from ``_run_interactive_mode``, which really runs and
  hands them to the patched ``run_textual_ui``;
- the post-mount action comes from ``VibeApp._process_startup_prompt``, the
  unbound reference method driven over a state double carrying only the
  attributes it reads.

Usage::

    .venv/bin/python ep006-python-oracle.py --reference /path/to/reference
"""

from __future__ import annotations

import argparse
import contextlib
import json
from pathlib import Path
import subprocess
import sys
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts" / "parity"))

from pin import EXPECTED_COMMIT  # noqa: E402  the path insert above enables it

# The argument shapes the corpus replays, keyed by trace id.
CASES: dict[str, dict[str, Any]] = {
    "trust-before-session-initialization": {},
    "worktree-before-trust-and-session": {
        "initial_prompt": "inspect the checkout",
        "worktree": "review",
    },
    "positional-prompt-after-mount": {"initial_prompt": "explain this repository"},
    "stdin-prompt-after-mount": {"stdin_prompt": "piped request"},
    "explicit-programmatic-prompt": {"prompt": "headless request"},
    "teleport-after-mount-without-prompt": {"teleport": True},
    "teleport-after-mount-without-agent-turn": {
        "initial_prompt": "deployment context",
        "teleport": True,
    },
    "bare-resume-picker-before-session-initialization": {"resume": True},
    "direct-resume-without-picker": {"resume": "session-123"},
    "continue-without-picker": {"continue_session": True},
}


def resolve_reference(reference: Path, expected_commit: str) -> str:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"git rev-parse failed in {reference}: {result.stderr.strip()}")
    commit = result.stdout.strip()
    if commit != expected_commit:
        raise RuntimeError(f"reference is at {commit}, not the pinned {expected_commit}")
    return commit


def command_line(case: dict[str, Any]) -> list[str]:
    """The invocation a user types to produce this case."""
    argv: list[str] = []
    if prompt := case.get("prompt"):
        argv += ["--prompt", prompt]
    if worktree := case.get("worktree"):
        argv += ["--worktree", worktree]
    if case.get("teleport"):
        argv.append("--teleport")
    if case.get("continue_session"):
        argv.append("--continue")
    resume = case.get("resume")
    if resume is True:
        argv.append("--resume")
    elif isinstance(resume, str):
        argv += ["--resume", resume]
    if initial := case.get("initial_prompt"):
        argv.append(initial)
    return argv


def capture_route_and_startup(case: dict[str, Any]) -> tuple[str, Any]:
    """Runs the reference `run_cli` and reports the branch and startup options."""
    from vibe.cli import cli
    from vibe.cli.entrypoint import parse_arguments
    import vibe.app_server.local as local
    import vibe.cli.textual_ui.app as textual_app

    argv = command_line(case)
    observed: dict[str, Any] = {}

    def record_textual_ui(**kwargs: Any) -> Any:
        observed["route"] = "interactive"
        observed["startup"] = kwargs["startup"]
        return mock.MagicMock()

    def record_programmatic(**kwargs: Any) -> None:
        del kwargs
        observed["route"] = "programmatic"

    orchestrator = mock.MagicMock()
    orchestrator.config.enable_telemetry = False
    with contextlib.ExitStack() as stack:
        patch = stack.enter_context
        patch(mock.patch.object(sys, "argv", ["vibe", *argv]))
        patch(mock.patch.object(cli, "load_dotenv_values", lambda: None))
        patch(mock.patch.object(cli, "bootstrap_config_files", lambda: None))
        patch(
            mock.patch.object(
                cli, "load_config_orchestrator_or_exit", lambda **_: orchestrator
            )
        )
        patch(mock.patch.object(cli, "_maybe_run_startup_update_prompt", lambda *_: None))
        patch(mock.patch.object(cli, "init_sentry", lambda **_: False))
        patch(
            mock.patch.object(
                cli, "get_prompt_from_stdin", lambda: case.get("stdin_prompt")
            )
        )
        patch(mock.patch.object(cli, "FileSystemUpdateCacheRepository", mock.MagicMock()))
        patch(mock.patch.object(cli, "print_session_resume_message", lambda _: None))
        patch(mock.patch.object(cli, "_run_programmatic_mode", record_programmatic))
        # `_run_interactive_mode` itself is not patched: it is the code under
        # measurement, and it imports these two names when it runs.
        patch(mock.patch.object(local, "LocalHarness", mock.MagicMock()))
        patch(mock.patch.object(textual_app, "run_textual_ui", record_textual_ui))
        cli.run_cli(parse_arguments())

    return observed["route"], observed.get("startup")


def capture_post_mount_action(startup: Any) -> dict[str, Any] | None:
    """Drives the reference post-mount branch over a minimal state double."""
    from vibe.cli.textual_ui.app import VibeApp

    dispatched: list[dict[str, Any]] = []

    class Coroutine:
        """Stands in for the handler coroutine so nothing is awaited."""

        def __init__(self, kind: str, prompt: str | None) -> None:
            self.kind = kind
            self.prompt = prompt

    class Commands:
        def has_command(self, name: str) -> bool:
            # The corpus captures a session where teleport is available; the
            # reference also gates it on `vibe_code_enabled`, recorded below.
            return name == "teleport"

    class StateDouble:
        # Both reference methods run unmodified against this state.
        _process_startup_prompt = VibeApp._process_startup_prompt  # noqa: SLF001
        _process_initial_prompt = VibeApp._process_initial_prompt  # noqa: SLF001

        def __init__(self) -> None:
            self._startup_prompt_processed = False
            self._initial_prompt = startup.initial_prompt
            self._teleport_on_start = startup.teleport_on_start
            self.commands = Commands()

        def _handle_teleport_command(self, prompt: str | None) -> Coroutine:
            return Coroutine("teleport", prompt)

        def _handle_user_message(self, prompt: str) -> Coroutine:
            return Coroutine("prompt", prompt)

        def run_worker(self, coroutine: Coroutine, **_: Any) -> None:
            dispatched.append({"type": coroutine.kind, "prompt": coroutine.prompt})

    state = StateDouble()
    state._process_startup_prompt()  # noqa: SLF001 - the reference method is the oracle
    if not dispatched:
        return None
    return dispatched[0]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=Path("/home/arthur/dev/mistral-vibe"))
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--output", type=Path, default=None)
    arguments = parser.parse_args()

    commit = resolve_reference(arguments.reference, arguments.expected_commit)
    sys.path.insert(0, str(arguments.reference))
    from vibe.core.config.harness_files import init_harness_files_manager

    init_harness_files_manager()

    measured = {}
    for trace_id, case in CASES.items():
        route, startup = capture_route_and_startup(case)
        action = capture_post_mount_action(startup) if startup is not None else None
        measured[trace_id] = {"route": route, "postMountAction": action}

    report = {"reference": {"commit": commit}, "traces": measured}
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if arguments.output:
        arguments.output.write_text(text, encoding="utf-8")
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
