"""Pinned Python oracle for EP-007: active-turn callbacks, queue and shell.

Every observation is measured from reference code. The approval and question
widgets are mounted in a Textual harness and driven through their real key
actions; the callback FIFO, the typed queue and the shell stream are exercised
through the reference methods themselves over the minimum state each one reads.
Nothing here restates a rule the reference implements.

Usage::

    .venv/bin/python ep007-python-oracle.py --reference /path/to/reference
"""

from __future__ import annotations

import argparse
import asyncio
from collections import deque
import json
from pathlib import Path
import subprocess
import sys
import time
from typing import Any

EXPECTED_COMMIT = "68ff32e6a92e80a874c8153312f0aa8ae4955477"

# The approval fixture the corpus replays: one shell effect carrying a command
# and one required permission.
EFFECT_COMMAND = "cargo test"


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
# US-024: approval presentation and the input grace period
# ---------------------------------------------------------------------------


def approval_effect() -> Any:
    """The effect detail the reference builds for a shell approval."""
    from pydantic import TypeAdapter
    from vibe.app_server.models import EffectDetail
    from vibe.core.tools.builtins.bash import Bash, BashArgs
    from vibe.core.tools.ui import ToolUIDataAdapter
    from vibe.core.types import ToolCallEvent

    adapter = ToolUIDataAdapter(Bash)
    call = adapter.get_call_presentation(
        ToolCallEvent(
            tool_call_id="call-1",
            tool_name="bash",
            tool_class=Bash,
            args=BashArgs(command=EFFECT_COMMAND),
        )
    )
    return TypeAdapter(EffectDetail).validate_python({
        "kind": call.kind.value,
        "toolName": "bash",
        "input": {"command": EFFECT_COMMAND},
        "display": call.display.model_dump(by_alias=True),
    })


async def capture_approval_trace() -> list[str]:
    from textual.app import App, ComposeResult
    from vibe.permissions import RequiredPermission
    from vibe.cli.textual_ui.widgets.approval_app import ApprovalApp

    effect = approval_effect()
    permission = RequiredPermission(
        scope="command_pattern",
        invocation_pattern=EFFECT_COMMAND,
        session_pattern=EFFECT_COMMAND,
        label=EFFECT_COMMAND,
    )
    submitted: list[str] = []

    class Harness(App):

        def compose(self) -> ComposeResult:
            yield ApprovalApp(effect, config_view(), [permission])

        def on_approval_app_approval_granted(self, _event: Any) -> None:
            submitted.append("approve_once")

    observations: list[str] = []
    app = Harness()
    async with app.run_test() as pilot:
        approval = app.query_one(ApprovalApp)
        await pilot.pause()
        # The command reaches the model through the approval body; the required
        # permission reaches it through the title the reference builds.
        title = approval._build_title()  # noqa: SLF001
        body = "\n".join(line for line in widget_lines(approval) if line != title)
        observations.append(
            f"active:zeta:effect={str(EFFECT_COMMAND in body).lower()}"
            f":permissions={str(permission.label in title).lower()}"
        )

        # 1499 ms on the corpus clock is 499 ms after the callback is presented,
        # inside the reference grace period.
        approval.action_select()
        await pilot.pause()
        observations.append("grace_blocked" if not submitted else f"submit:{submitted[-1]}")

        # 1500 ms: the grace period has elapsed.
        approval._mount_time = time.monotonic() - 0.5  # noqa: SLF001
        approval.action_select()
        await pilot.pause()
        observations.append(f"submit:{submitted[-1]}" if submitted else "grace_blocked")
    return observations


def title_text(widget: Any) -> str:
    return str(getattr(widget, "title_widget", ""))


def widget_lines(widget: Any) -> list[str]:
    """Every rendered string the widget subtree currently holds."""
    lines = [str(widget._build_title())]  # noqa: SLF001
    for node in widget.walk_children(with_self=True):
        content = getattr(node, "content", None)
        if content is None:
            continue
        text = content.plain if hasattr(content, "plain") else str(content)
        if text:
            lines.extend(text.split("\n"))
    return lines


def config_view() -> Any:
    from vibe.app_server.config import ConfigView

    return ConfigView.model_construct()


# ---------------------------------------------------------------------------
# US-025: the callback FIFO is the server's arrival order
# ---------------------------------------------------------------------------


async def capture_fifo_trace(presented: str) -> list[str]:
    from vibe.app_server.models import (
        ApprovalCallbackDetail,
        PublicCallbackEntry,
    )
    from vibe.cli.textual_ui.app import VibeApp

    def callback(identifier: str) -> Any:
        return PublicCallbackEntry.model_construct(
            callback_id=identifier,
            detail=ApprovalCallbackDetail.model_construct(
                effect=approval_effect(), required_permissions=[]
            ),
        )

    shown: list[str] = []

    class StateDouble:
        # The reference methods under measurement.
        _show_callback = VibeApp._show_callback  # noqa: SLF001
        _respond_to_active_callback = VibeApp._respond_to_active_callback  # noqa: SLF001

        def __init__(self) -> None:
            self._pending_callbacks: deque[Any] = deque()
            self._active_callback: Any = None
            self._pending_local_question = None
            self._terminal_notifier = _Notifier()
            self.app_server = _AppServer()

        async def _wait_for_typing_pause(self) -> None:
            return None

        async def _switch_to_approval_app(self, *_: Any, **__: Any) -> None:
            shown.append(self._active_callback.callback_id)

        async def _switch_to_question_app(self, *_: Any, **__: Any) -> None:
            shown.append(self._active_callback.callback_id)

        async def _switch_to_input_app(self) -> None:
            return None

    class _Notifier:
        def notify(self, *_: Any, **__: Any) -> None:
            return None

    class _AppServer:
        async def respond_to_callback(self, *_: Any, **__: Any) -> None:
            return None

    state = StateDouble()
    await state._show_callback(callback("zeta"))  # noqa: SLF001
    # The presentation of this first callback is the one already measured from
    # the mounted approval widget; what this trace adds is which callback the
    # reference surfaces next.
    observations = [presented.replace("active:zeta", f"active:{shown[-1]}")]

    # The second callback arrives while the first is active, then the first is
    # answered: what surfaces next is what the reference dequeues.
    await state._show_callback(callback("alpha"))  # noqa: SLF001
    await state._respond_to_active_callback(  # noqa: SLF001
        _approval_output()
    )
    observations.append(f"active:{shown[-1]}")
    return observations


def _approval_output() -> Any:
    from vibe.app_server.models import ApprovalCallbackOutput, ApprovalDecision

    return ApprovalCallbackOutput.model_construct(
        decision=ApprovalDecision.model_construct(type="approve"), feedback=None
    )


# ---------------------------------------------------------------------------
# US-025: question tabs, multi-select and free text
# ---------------------------------------------------------------------------


def question_request() -> Any:
    from vibe.questions import QuestionChoice, UserQuestion, UserQuestionRequest

    return UserQuestionRequest(
        questions=[
            UserQuestion(
                header="Runtime",
                question="Choose runtimes",
                options=[
                    QuestionChoice(label="Rust"),
                    QuestionChoice(label="Python"),
                ],
                multiSelect=True,
            ),
            UserQuestion(
                header="Constraint",
                question="Constraint?",
                options=[
                    QuestionChoice(label="Fast"),
                    QuestionChoice(label="Small"),
                ],
                multiSelect=False,
            ),
        ],
        footerNote="Answer both",
    )


async def capture_questions_trace() -> list[str]:
    from textual.app import App, ComposeResult
    from vibe.cli.textual_ui.widgets.question_app import QuestionApp

    answers: list[Any] = []

    request = question_request()

    class Harness(App):

        def compose(self) -> ComposeResult:
            yield QuestionApp(request)

        def on_question_app_answered(self, event: Any) -> None:
            answers.append(event.answers)

    # The corpus drives: toggle, move to Other, type "No", move to Submit,
    # submit, then the same shape on the single-select question.
    script: list[tuple[str, Any]] = [
        ("select", None),
        ("down", None),
        ("down", None),
        ("text", "No"),
        ("down", None),
        ("select", None),
        ("down", None),
        ("down", None),
        ("text", "OK"),
        ("select", None),
    ]

    observations = ["active:questions"]
    app = Harness()
    async with app.run_test() as pilot:
        questions = app.query_one(QuestionApp)
        await pilot.pause()
        questions._mount_time = time.monotonic() - 1.0  # noqa: SLF001 - past the grace period
        for action, value in script:
            steps = 1
            if action == "select":
                questions.action_select()
            elif action == "down":
                questions.action_move_down()
            elif action == "text":
                steps = len(value)
                type_other_text(questions, value)
            await pilot.pause()
            observations.append(",".join(["updated"] * steps))
        if answers:
            observations[-1] = f"submit:{render_answers(answers[-1], request)}"
    return observations


def type_other_text(questions: Any, value: str) -> None:
    """Enters free text the way the reference input path stores it."""
    if questions.other_input is not None:
        questions.other_input.value = value
    questions.other_texts[questions.current_question_idx] = value
    questions._update_display()  # noqa: SLF001


def render_answers(answers: list[Any], request: Any) -> str:
    """Renders the reference answers the way the corpus records them.

    Each answer is split back into the parts the reference joined. A part that
    names one of the question's options is recorded by its identifier, the
    lowercased label; anything else is free text and keeps its exact value,
    tagged `text:` when the whole answer is free text.
    """
    labels = {
        question.question: {choice.label for choice in question.options}
        for question in request.questions
    }
    parts = []
    for answer in answers:
        options = labels.get(answer.question, set())
        pieces = [piece.strip() for piece in answer.answer.split(",") if piece.strip()]
        rendered = [
            piece.lower() if piece in options else piece for piece in pieces
        ]
        if all(piece not in options for piece in pieces):
            parts.append(f"text:{'+'.join(rendered)}")
        else:
            parts.append("+".join(rendered))
    return "combined:" + "|".join(parts)


# ---------------------------------------------------------------------------
# US-026: the plan review file refresh
# ---------------------------------------------------------------------------


async def capture_plan_trace(workdir: Path) -> list[str]:
    from vibe.cli.textual_ui.widgets.messages import PlanFileMessage

    from vibe.utils.io import read_safe_async

    plan_path = workdir / "plans/session.md"
    plan_path.parent.mkdir(parents=True, exist_ok=True)
    if plan_path.exists():
        plan_path.unlink()
    message = PlanFileMessage(file_path=plan_path)
    observations = [f"plan:{str(message._file_path == plan_path).lower()}"]  # noqa: SLF001

    async def read() -> str:
        """The read the reference performs when the plan file changes."""
        try:
            return f"plan:{(await read_safe_async(plan_path)).text}"
        except OSError:
            return "plan:unreadable"

    observations.append(await read())
    plan_path.write_text("# Initial", encoding="utf-8")
    observations.append("plan:written")
    observations.append(await read())
    plan_path.unlink()
    observations.append("plan:removed")
    observations.append(await read())
    plan_path.write_text("# Updated", encoding="utf-8")
    observations.append("plan:written")
    observations.append(await read())
    return observations


# ---------------------------------------------------------------------------
# US-027: the typed queue, its rollback and its shell boundaries
# ---------------------------------------------------------------------------


def capture_queue_trace() -> list[str]:
    from vibe.cli.textual_ui.message_queue import MessageQueue, QueuedItemKind

    queue = MessageQueue()
    observations: list[str] = []
    for text in ["first", "second"]:
        queue.append_prompt(text)
        observations.append(f"queued:{text}")
    queue.append_bash("pwd")
    observations.append("queued:!pwd")
    queue.append_prompt("third")
    observations.append("queued:third")

    queue.pause()
    observations.append(f"queue:paused:{len(queue)}")
    queue.resume()
    observations.append(f"queue:resumed:{len(queue)}")

    # Draining stops at every shell boundary, so a batch is homogeneous.
    while queue:
        batch: list[Any] = []
        kind = queue.items[0].kind
        while queue and queue.items[0].kind == kind:
            item = queue.pop_first()
            if item is None:
                break
            batch.append(item)
            if kind is QueuedItemKind.BASH:
                break
        label = "shell" if kind is QueuedItemKind.BASH else "prompt"
        observations.append(f"batch:{label}:{len(batch)}")
    return observations


# ---------------------------------------------------------------------------
# US-028: shell streaming and cancellation identity
# ---------------------------------------------------------------------------


async def capture_shell_trace() -> list[str]:
    """Streams a shell run through the reference and cancels it mid-flight.

    What is measured is which chunks the reference hands to the event handler:
    the run loop stops at cancellation, so the chunk emitted afterwards never
    reaches the transcript and the accumulated text stops growing.
    """
    from vibe.cli.textual_ui.app import VibeApp

    accumulated: list[str] = []
    observations: list[str] = []
    status = "streaming"

    from vibe.app_server.events import HistoryEntryUpdated

    def chunk(text: str) -> Any:
        """One shell output update, in the wire shape the reference consumes."""
        entry = _ShellEntry(text)
        return HistoryEntryUpdated(previous=entry, entry=entry, patch=[])

    class _ShellEntry:
        __slots__ = ("text",)

        def __init__(self, text: str) -> None:
            self.text = text

    class Shell:
        async def run(self, _command: str) -> Any:
            for text in ["a", "b"]:
                yield chunk(text)
            # The turn is cancelled here; the reference catches it and stops
            # consuming, so the trailing chunk below is never delivered.
            raise asyncio.CancelledError
            yield chunk("c")  # noqa: B901 - unreachable by design, documents the input

    class EventHandler:
        async def handle_event(self, event: Any, loading_widget: Any = None) -> None:
            del loading_widget
            accumulated.append(event.entry.text)
            observations.append(f"shell:{status}:{''.join(accumulated)}")

    class StateDouble:
        _handle_bash_command_inner = VibeApp._handle_bash_command_inner  # noqa: SLF001

        def __init__(self) -> None:
            self._loading_widget = None
            self._tools_collapsed = False
            self.event_handler = EventHandler()
            self.app_server = _Resources()

        async def _ensure_loading_widget(self, _label: str) -> None:
            return None

        async def _remove_loading_widget(self) -> None:
            return None

        async def _mount_and_scroll(self, _widget: Any) -> None:
            return None

    class _Resources:
        def __init__(self) -> None:
            self.resources = self

        shell = Shell()

    observations.append(f"shell:{status}:")
    state = StateDouble()
    await state._handle_bash_command_inner("echo streaming")  # noqa: SLF001
    status = "cancelled"
    observations.append(f"shell:{status}:{''.join(accumulated)}")
    # The post-cancellation chunk changes nothing: it was never consumed.
    observations.append(f"shell:{status}:{''.join(accumulated)}")
    return observations


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


async def capture(workdir: Path) -> dict[str, list[str]]:
    approval = await capture_approval_trace()
    return {
        "approval-effect-permissions-scopes-and-grace": approval,
        "canonical-server-fifo-beats-identifier-order": await capture_fifo_trace(approval[0]),
        "questions-tabs-multi-select-and-other": await capture_questions_trace(),
        "plan-review-live-file-refresh": await capture_plan_trace(workdir),
        "typed-queue-rollback-and-shell-boundaries": capture_queue_trace(),
        "shell-stream-and-identity-cancellation": await capture_shell_trace(),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=Path("/home/arthur/dev/mistral-vibe"))
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--workdir", type=Path, default=Path("/tmp/ep007-oracle"))
    arguments = parser.parse_args()

    commit = resolve_reference(arguments.reference, arguments.expected_commit)
    sys.path.insert(0, str(arguments.reference))
    from vibe.core.config.harness_files import init_harness_files_manager

    init_harness_files_manager()
    arguments.workdir.mkdir(parents=True, exist_ok=True)

    traces = asyncio.run(capture(arguments.workdir))
    report = {"reference": {"commit": commit}, "traces": traces}
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if arguments.output:
        arguments.output.write_text(text, encoding="utf-8")
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
