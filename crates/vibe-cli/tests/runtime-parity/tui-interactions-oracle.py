"""Pinned Python oracle for transcript and composer interactions.

Four families, each observed from the reference itself:

* ``groupHeader``: the summary line a ``ToolGroupHeader`` shows for a set of
  effect kinds, with and without reasoning, while running and once settled.
* ``composer``: presses, drags and releases fed to a mounted ``ChatTextArea``
  through its own mouse handlers. Only the geometry is replaced: the probe
  subclass reads the document location straight off the event, so the chain,
  word and line logic under test is the reference's.
* ``transcript``: the word and line spans ``WordSelectScreen._boundary_around``
  computes over a mounted ``Static``.
* ``loadingSweep``: the color each cell of ``LoadingWidget`` takes, frame by
  frame, for a status no easter egg may replace. The snake's turns are random
  in the reference, so only the sweep is observed.
* ``streamDelta``: what a running call shows under its header as its output
  grows, from the JSON patch the reference server builds between two
  snapshots (``make_json_patch`` over its streaming paths) and the delta the
  event handler reads back out of it (``_appended_text``). Only a non-empty
  delta replaces the line, and a settled call drops it, as
  ``set_stream_message`` and ``settle`` do.
* ``queue``: keys pressed into a mounted ``ChatInputBody`` whose queue getters
  and message handlers stand in for the app's, the way ``app.py`` wires them.
  Notices are observed by the timeout they are raised with, never by their
  text.

The output is the corpus itself; the Rust replay compares the port against the
``reference`` observations and the live probe compares a fresh capture with
the committed one.

Usage::

    .venv/bin/python tui-interactions-oracle.py --reference /path/to/reference
"""

from __future__ import annotations

import argparse
import asyncio
import json
from pathlib import Path
import subprocess
import sys
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts" / "parity"))

from pin import EXPECTED_COMMIT  # noqa: E402  the path insert above enables it

SOURCE_FILES = [
    "vibe/app_server/_patch.py",
    "vibe/app_server/events.py",
    "vibe/cli/textual_ui/handlers/event_handler.py",
    "vibe/cli/textual_ui/widgets/chat_input/body.py",
    "vibe/cli/textual_ui/widgets/chat_input/text_area.py",
    "vibe/cli/textual_ui/widgets/loading.py",
    "vibe/cli/textual_ui/widgets/tools.py",
    "vibe/cli/textual_ui/word_selection.py",
]

GROUP_HEADERS = [
    {"kinds": ["shell", "file_edit"], "reasoning": True},
    {"kinds": ["file_read"], "reasoning": False},
    {"kinds": [], "reasoning": True},
    {"kinds": ["tool", "process"], "reasoning": False},
    {
        "kinds": [
            "web_search",
            "web_fetch",
            "todo",
            "user_question",
            "skill",
            "subagent",
            "worktree",
            "file_write",
            "file_search",
        ],
        "reasoning": False,
    },
]

COMPOSER_TEXT = "alpha beta_2 gamma\nsecond line here"

# Presses on one spot 100 ms apart chain; a gap past half a second does not.
COMPOSER_TRACES = [
    {
        "id": "word-then-line-then-over",
        "steps": [
            {"kind": "down", "x": 8, "y": 0, "atMs": 0},
            {"kind": "up", "x": 8, "y": 0, "atMs": 0},
            {"kind": "down", "x": 8, "y": 0, "atMs": 100},
            {"kind": "up", "x": 8, "y": 0, "atMs": 100},
            {"kind": "down", "x": 8, "y": 0, "atMs": 200},
            {"kind": "up", "x": 8, "y": 0, "atMs": 200},
            {"kind": "down", "x": 8, "y": 0, "atMs": 300},
            {"kind": "up", "x": 8, "y": 0, "atMs": 300},
        ],
    },
    {
        "id": "word-drag-snaps-and-ends-the-chain",
        "steps": [
            {"kind": "down", "x": 8, "y": 0, "atMs": 0},
            {"kind": "up", "x": 8, "y": 0, "atMs": 0},
            {"kind": "down", "x": 8, "y": 0, "atMs": 100},
            {"kind": "move", "x": 15, "y": 0, "atMs": 150},
            {"kind": "move", "x": 3, "y": 1, "atMs": 200},
            {"kind": "up", "x": 3, "y": 1, "atMs": 200},
            {"kind": "down", "x": 3, "y": 1, "atMs": 250},
            {"kind": "up", "x": 3, "y": 1, "atMs": 250},
        ],
    },
    {
        "id": "line-drag-backward",
        "steps": [
            {"kind": "down", "x": 3, "y": 1, "atMs": 0},
            {"kind": "up", "x": 3, "y": 1, "atMs": 0},
            {"kind": "down", "x": 3, "y": 1, "atMs": 100},
            {"kind": "up", "x": 3, "y": 1, "atMs": 100},
            {"kind": "down", "x": 3, "y": 1, "atMs": 200},
            {"kind": "move", "x": 2, "y": 0, "atMs": 250},
            {"kind": "up", "x": 2, "y": 0, "atMs": 250},
        ],
    },
    {
        "id": "a-pause-or-a-blank-breaks-the-word",
        "steps": [
            {"kind": "down", "x": 3, "y": 1, "atMs": 0},
            {"kind": "up", "x": 3, "y": 1, "atMs": 0},
            {"kind": "down", "x": 3, "y": 1, "atMs": 600},
            {"kind": "up", "x": 3, "y": 1, "atMs": 600},
            {"kind": "down", "x": 13, "y": 0, "atMs": 1000},
            {"kind": "up", "x": 13, "y": 0, "atMs": 1000},
            {"kind": "down", "x": 12, "y": 0, "atMs": 1100},
            {"kind": "up", "x": 12, "y": 0, "atMs": 1100},
        ],
    },
    {
        "id": "a-character-drag-selects-across-lines",
        "steps": [
            {"kind": "down", "x": 2, "y": 0, "atMs": 0},
            {"kind": "move", "x": 9, "y": 1, "atMs": 50},
            {"kind": "up", "x": 9, "y": 1, "atMs": 50},
        ],
    },
]

TRANSCRIPT_LINES = ["See https://ratatui.rs for details", "second_line here, ok"]

TRANSCRIPT_SPANS = [
    {"x": 5, "y": 0, "granularity": "word"},
    {"x": 3, "y": 0, "granularity": "word"},
    {"x": 2, "y": 1, "granularity": "word"},
    {"x": 16, "y": 1, "granularity": "word"},
    {"x": 5, "y": 0, "granularity": "paragraph"},
    {"x": 0, "y": 1, "granularity": "paragraph"},
]

QUEUE_TRACES = [
    {
        "id": "walk-edit-save-remove-exit",
        "queue": ["first", "second", "third"],
        "keys": [
            "type:draft",
            "up",
            "up",
            "up",
            "up",
            "up",
            "x",
            "down",
            "enter",
            "home",
            "type:x",
            "enter",
            "backspace",
            "escape",
        ],
    },
    {
        "id": "down-past-the-newest-leaves",
        "queue": ["only"],
        "keys": ["up", "down", "up", "delete"],
    },
    {
        "id": "an-edit-dropped-by-escape",
        "queue": ["first", "second"],
        "keys": ["up", "enter", "type:!", "escape", "escape"],
    },
    {
        "id": "a-started-prompt-asks-before-requeueing",
        "queue": ["first", "second"],
        "keys": ["up", "up", "enter", "promote", "type: again", "enter", "enter", "escape"],
    },
]

NOTICE_KINDS = {3.0: "selection", None: "edit", 8.0: "consumed"}


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


def group_headers() -> list[dict[str, Any]]:
    from vibe.cli.textual_ui.widgets.tools import ToolGroupHeader
    from vibe.utils.tool_presentation import ToolEffectKind

    cases = []
    for case in GROUP_HEADERS:
        for running in (True, False):
            header = ToolGroupHeader()
            for kind in case["kinds"]:
                header.add_category(ToolEffectKind(kind))
            if case["reasoning"]:
                header.mark_reasoning()
            if not running:
                header.stop_spinning()
            cases.append({**case, "running": running, "label": header.get_content()})
    return cases


LOADING_FRAMES = 70

# Output snapshots of one call, the first as it is added; `None` settles it.
STREAM_TRACES = [
    {"id": "appends-show-their-delta", "outputs": ["", "a\n", "a\nb\n", "a\nb\nc\n", None]},
    {"id": "a-rewrite-keeps-the-last-delta", "outputs": ["one", "one two", "zero", "zero", None]},
    {"id": "an-unchanged-update-keeps-the-line", "outputs": ["x", "x", "xy", "xy"]},
]


def stream_delta() -> list[dict[str, Any]]:
    from vibe.app_server._patch import make_json_patch
    from vibe.app_server.events import _STREAMING_TEXT_PATHS
    from vibe.cli.textual_ui.handlers.event_handler import _appended_text

    traces = []
    for trace in STREAM_TRACES:
        # The first snapshot is the call being added, which streams nothing.
        previous = {"state": {"type": "running", "outputText": trace["outputs"][0]}}
        shown = ""
        observed = [shown]
        for output in trace["outputs"][1:]:
            if output is None:
                shown = ""
                observed.append(shown)
                continue
            current = {"state": {"type": "running", "outputText": output}}
            patch = make_json_patch(previous, current, append_paths=_STREAMING_TEXT_PATHS)
            if delta := _appended_text(patch, "/state/outputText"):
                shown = delta
            observed.append(shown)
            previous = current
        traces.append({**trace, "reference": observed})
    return traces


def loading_sweep() -> list[dict[str, Any]]:
    from vibe.cli.textual_ui.widgets.loading import LoadingWidget

    status = "Retrying"
    widget = LoadingWidget(status)
    frames = []
    for _ in range(LOADING_FRAMES):
        cells = 1 + len(widget.status) + 1
        frames.append(
            "".join(
                str(LoadingWidget.TARGET_COLORS.index(widget._get_color_for_position(cell)))  # noqa: SLF001
                for cell in range(cells)
            )
        )
        widget._update_animation()  # noqa: SLF001
    return [{"status": status, "frames": frames}]


def selection_text(selection: Any) -> str:
    start, end = sorted([tuple(selection.start), tuple(selection.end)])
    if start == end:
        return f"caret {start[0]}:{start[1]}"
    return f"{start[0]}:{start[1]}-{end[0]}:{end[1]}"


async def composer() -> list[dict[str, Any]]:
    from textual import events
    from textual.app import App, ComposeResult

    from vibe.cli.commands import CommandRegistry
    from vibe.cli.textual_ui.widgets.chat_input.text_area import ChatTextArea

    class Probe(ChatTextArea):
        def get_target_document_location(self, event: events.MouseEvent) -> tuple[int, int]:
            return (event.y, event.x)

    class Host(App[None]):
        def compose(self) -> ComposeResult:
            self.probe = Probe(CommandRegistry(), id="input")
            yield self.probe

    kinds = {"down": events.MouseDown, "move": events.MouseMove, "up": events.MouseUp}
    traces = []
    for trace in COMPOSER_TRACES:
        app = Host()
        async with app.run_test(size=(80, 10)):
            probe = app.probe
            probe.load_text(COMPOSER_TEXT)
            observed = []
            for step in trace["steps"]:
                event = kinds[step["kind"]](
                    probe, step["x"], step["y"], 0, 0, 1, False, False, False,
                    screen_x=step["x"], screen_y=step["y"],
                )
                event.time = step["atMs"] / 1000
                handler = {
                    "down": probe._on_mouse_down,  # noqa: SLF001
                    "move": probe._on_mouse_move,  # noqa: SLF001
                    "up": probe._on_mouse_up,  # noqa: SLF001
                }[step["kind"]]
                await handler(event)
                observed.append(selection_text(probe.selection))
        traces.append({**trace, "text": COMPOSER_TEXT, "reference": observed})
    return traces


async def transcript() -> list[dict[str, Any]]:
    from textual.app import App, ComposeResult
    from textual.geometry import Offset
    from textual.widgets import Static

    from vibe.cli.textual_ui.word_selection import SelectGranularity, WordSelectScreen

    class Host(App[None]):
        def compose(self) -> ComposeResult:
            yield Static("\n".join(TRANSCRIPT_LINES), id="lines")

    observed = []
    app = Host()
    async with app.run_test(size=(80, 10)):
        widget = app.query_one("#lines")
        for case in TRANSCRIPT_SPANS:
            span = WordSelectScreen._boundary_around(  # noqa: SLF001
                widget,
                Offset(case["x"], case["y"]),
                SelectGranularity(case["granularity"]),
            )
            text = (
                "none"
                if span is None
                else f"{span[0].y}:{span[0].x}-{span[1].y}:{span[1].x}"
            )
            observed.append({**case, "reference": text})
    return [{"lines": TRANSCRIPT_LINES, "spans": observed}]


async def queue() -> list[dict[str, Any]]:
    from textual.app import App, ComposeResult

    from vibe.cli.commands import CommandRegistry
    from vibe.cli.textual_ui.widgets.chat_input.body import ChatInputBody

    class Host(App[None]):
        """The app's side of queue mode (`app.py` queue handlers), by identity."""

        def __init__(self, texts: list[str]) -> None:
            super().__init__()
            self.items = [{"id": index, "text": text} for index, text in enumerate(texts)]
            self.next_id = len(texts)
            self.selected: int | None = None
            self.notice: str | None = None

        def selected_index(self) -> int | None:
            for index, item in enumerate(self.items):
                if item["id"] == self.selected:
                    return index
            return None

        def compose(self) -> ComposeResult:
            self.body = ChatInputBody(
                CommandRegistry(),
                queue_edit_active_getter=lambda: True,
                queue_items_getter=lambda: [
                    (index, item["text"]) for index, item in enumerate(self.items)
                ],
                queue_selected_index_getter=self.selected_index,
            )
            yield self.body

        def append(self, text: str) -> None:
            self.items.append({"id": self.next_id, "text": text})
            self.next_id += 1

        def on_chat_input_body_queue_selection_scroll(self, event: Any) -> None:
            if event.queue_index < len(self.items):
                self.selected = self.items[event.queue_index]["id"]

        def on_chat_input_body_queue_remove_requested(self, _event: Any) -> None:
            index = self.selected_index()
            if index is not None:
                self.items.pop(index)
            self.selected = None

        def on_chat_input_body_queue_edit_submitted(self, event: Any) -> None:
            index = self.selected_index()
            if index is None:
                self.append(event.value)
            else:
                self.items[index]["text"] = event.value

        def on_chat_input_body_queue_edit_consumed(self, event: Any) -> None:
            self.append(event.value)

        def on_chat_input_body_queue_mode_exited(self, _event: Any) -> None:
            self.selected = None

        def on_chat_input_body_inline_notice_requested(self, event: Any) -> None:
            self.notice = NOTICE_KINDS.get(event.timeout, "other")

        def on_chat_input_body_inline_notice_cleared(self, _event: Any) -> None:
            self.notice = None

    traces = []
    for trace in QUEUE_TRACES:
        app = Host(list(trace["queue"]))
        async with app.run_test(size=(80, 10)) as pilot:
            body = app.body
            observed = []
            for key in trace["keys"]:
                if key == "promote":
                    app.items.pop(0)
                elif key.startswith("type:"):
                    for character in key.removeprefix("type:"):
                        await pilot.press(character)
                else:
                    await pilot.press(key)
                await pilot.pause()
                observed.append(
                    json.dumps(
                        {
                            "cursor": body._queue_cursor,  # noqa: SLF001
                            "editing": body._queue_in_edit_mode,  # noqa: SLF001
                            "text": body.input_widget.text,
                            "notice": app.notice,
                            "queue": [item["text"] for item in app.items],
                        },
                        sort_keys=True,
                    )
                )
        traces.append({**trace, "reference": observed})
    return traces


async def capture() -> dict[str, Any]:
    return {
        "groupHeader": group_headers(),
        "loadingSweep": loading_sweep(),
        "streamDelta": stream_delta(),
        "composer": await composer(),
        "transcript": await transcript(),
        "queue": await queue(),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=Path("/home/arthur/dev/mistral-vibe"))
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--output", type=Path, default=None)
    arguments = parser.parse_args()

    commit = resolve_reference(arguments.reference, arguments.expected_commit)
    sys.path.insert(0, str(arguments.reference))
    from vibe import __version__
    from vibe.core.config.harness_files import init_harness_files_manager

    init_harness_files_manager()

    report = {
        "schemaVersion": 1,
        "oracle": "tui-interactions-oracle.py",
        "reference": {"commit": commit, "version": __version__, "sourceFiles": SOURCE_FILES},
        **asyncio.run(capture()),
    }
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if arguments.output:
        arguments.output.write_text(text, encoding="utf-8")
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
