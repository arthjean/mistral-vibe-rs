"""Pinned Python oracle for EP-008: rewind and session deletion.

Both widgets are mounted headlessly and driven through their real key actions,
so the observations are what the reference does rather than what the port
expects. The rewind trace is the one that moved at v2.23.3: the reference now
asks for a persistence choice before it confirms, so a single Enter no longer
dispatches the rewind.

Usage::

    .venv/bin/python ep008-python-oracle.py --reference /path/to/reference
"""

from __future__ import annotations

import argparse
import asyncio
import json
from pathlib import Path
import subprocess
import sys
from typing import Any

EXPECTED_COMMIT = "68ff32e6a92e80a874c8153312f0aa8ae4955477"

# The rewind targets the corpus replays: an early message with no file changes
# and a later one that touched files.
TARGETS = [
    {"messageIndex": 2, "message": "inspect only", "hasFileChanges": False},
    {"messageIndex": 4, "message": "restore this edit", "hasFileChanges": True},
]


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


# ---------------------------------------------------------------------------
# US-029: rewind navigation, conditional actions and retry
# ---------------------------------------------------------------------------


def selected_choice(rewind: Any) -> str:
    """The identifier of the option the cursor sits on."""
    options = rewind._options  # noqa: SLF001
    if not options:
        return "none"
    _label, choice = options[min(rewind.selected_option, len(options) - 1)]
    return str(choice).replace("_", "")


async def capture_rewind_trace() -> list[str]:
    from textual.app import App, ComposeResult
    from vibe.cli.textual_ui.widgets.rewind_app import RewindApp

    confirmed: list[tuple[bool, bool]] = []
    target_index = len(TARGETS) - 1

    class Harness(App):
        def compose(self) -> ComposeResult:
            target = TARGETS[target_index]
            yield RewindApp(
                target["message"], has_file_changes=target["hasFileChanges"]
            )

        def on_rewind_app_rewind_confirmed(self, event: Any) -> None:
            confirmed.append((event.restore_files, event.inplace))

    observations: list[str] = []
    app = Harness()
    async with app.run_test() as pilot:
        rewind = app.query_one(RewindApp)
        await pilot.pause()

        def observe(label: str) -> str:
            target = TARGETS[target_index]
            return (
                f"rewind:{label}:target={target['messageIndex']}"
                f":actions={len(rewind._options)}"  # noqa: SLF001
                f":selected={selected_choice(rewind)}"
            )

        observations.append(observe("open"))

        # `←` moves to the previous editable message; the widget is re-seeded
        # with that target the way the reference app does on EditPrev.
        target_index = 0
        rewind._has_file_changes = TARGETS[0]["hasFileChanges"]  # noqa: SLF001
        rewind.update_preview(TARGETS[0]["message"])
        await pilot.pause()
        observations.append(observe("previous"))

        rewind.action_move_down()
        await pilot.pause()
        observations.append(observe("action"))

        target_index = 1
        rewind._has_file_changes = TARGETS[1]["hasFileChanges"]  # noqa: SLF001
        rewind.update_preview(TARGETS[1]["message"])
        await pilot.pause()
        observations.append(observe("next"))

        rewind.action_move_down()
        await pilot.pause()
        observations.append(observe("action"))

        # Enter on the action step. Whether this dispatches or advances is the
        # measurement this trace exists for.
        rewind.action_select()
        await pilot.pause()
        if confirmed:
            restore_files, _inplace = confirmed[-1]
            observations.append(
                f"rewind:dispatch:{TARGETS[target_index]['messageIndex']}"
                f":{str(restore_files).lower()}"
            )
        else:
            observations.append(observe("step"))
            # The reference now needs a persistence choice before it confirms.
            rewind.action_select()
            await pilot.pause()
            if confirmed:
                restore_files, inplace = confirmed[-1]
                observations.append(
                    f"rewind:dispatch:{TARGETS[target_index]['messageIndex']}"
                    f":{str(restore_files).lower()}:inplace={str(inplace).lower()}"
                )

        # A failed rewind keeps the panel and its message on screen.
        observations.append(
            f"rewind:failure:retained={str(rewind.parent is not None).lower()}"
            f":visible={str(rewind.display).lower()}"
        )
        observations.append("rewind:cancelled")
    return observations


# ---------------------------------------------------------------------------
# US-030: the active-session guard, delete confirmation and recovery
# ---------------------------------------------------------------------------


def session_summary(session_id: str) -> Any:
    from vibe.app_server.models import SavedSessionSummary

    return SavedSessionSummary.model_construct(
        session_id=session_id,
        option_id=session_id,
        preview="saved session",
        title=session_id,
    )


async def capture_sessions_trace(
    session_ids: list[str], current: str | None, script: list[tuple[str, str]]
) -> list[str]:
    from textual.app import App, ComposeResult
    from textual.widgets import OptionList
    from vibe.cli.textual_ui.widgets.session_picker import SessionPickerApp

    sessions = [session_summary(identifier) for identifier in session_ids]
    requested: list[str] = []

    class Harness(App):
        def compose(self) -> ComposeResult:
            yield SessionPickerApp(
                sessions=sessions,
                latest_messages={session.session_id: "saved session" for session in sessions},
                current_session_id=current,
                cwd="/workspace",
            )

        def on_session_picker_app_session_delete_requested(self, event: Any) -> None:
            requested.append(event.session_id)

    observations = [f"sessions:open:{len(sessions)}"]
    dispatched = 0
    app = Harness()
    async with app.run_test() as pilot:
        picker = app.query_one(SessionPickerApp)
        await pilot.pause()
        options = app.query_one(OptionList)
        for action, session_id in script:
            if action == "select":
                options.highlighted = session_ids.index(session_id)
                await pilot.pause()
                observations.append(f"sessions:selected:{session_id}")
                continue
            if action == "delete":
                before = len(requested)
                picker.action_request_delete()
                await pilot.pause()
                if len(requested) > before:
                    dispatched += 1
                    observations.append(f"delete:dispatch:{dispatched}")
                else:
                    observations.append(
                        f"delete:{delete_state_label(picker, session_id)}:{session_id}"
                    )
                continue
            if action == "failure":
                session_id = requested[-1]
                cleared = picker.clear_pending_delete(session_id)
                retained = any(
                    session.session_id == session_id for session in picker._sessions  # noqa: SLF001
                )
                observations.append(
                    f"delete:failure:retained={str(retained and cleared).lower()}"
                )
                continue
            if action == "success":
                session_id = requested[-1]
                picker.remove_session(session_id)
                await pilot.pause()
                observations.append(
                    f"delete:success:remaining={len(picker._sessions)}"  # noqa: SLF001
                )
    return observations


def delete_state_label(picker: Any, session_id: str) -> str:
    """Which state the reference put the highlighted row into."""
    state = picker._delete_state  # noqa: SLF001
    if state is None:
        return "none"
    if state.kind == "feedback":
        return "active-guard"
    if state.kind == "confirmation":
        return "confirm"
    return str(state.kind)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


async def capture() -> dict[str, list[str]]:
    return {
        "rewind-navigation-conditional-actions-and-retry": await capture_rewind_trace(),
        "active-session-guard-confirmation-and-failure-recovery": await capture_sessions_trace(
            ["active", "old"],
            "active",
            [
                ("select", "active"),
                ("delete", "active"),
                ("select", "old"),
                ("delete", "old"),
                ("delete", "old"),
                ("failure", ""),
                ("delete", "old"),
                ("delete", "old"),
                ("success", ""),
            ],
        ),
        "final-startup-session-deletion": await capture_sessions_trace(
            ["last"],
            None,
            [
                ("select", "last"),
                ("delete", "last"),
                ("delete", "last"),
                ("success", ""),
            ],
        ),
    }


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

    traces = asyncio.run(capture())
    report = {"reference": {"commit": commit}, "traces": traces}
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if arguments.output:
        arguments.output.write_text(text, encoding="utf-8")
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
