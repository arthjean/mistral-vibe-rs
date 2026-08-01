"""Textual pilot harness that observes the reference chat input.

Every scenario runs against the real ``ChatInputContainer`` from the pinned
reference checkout. The harness only observes: it patches message dispatch on
its own widget instances to record effects and never modifies the reference
source tree.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from types import SimpleNamespace
from typing import Any

from textual import events
from textual.app import App, ComposeResult

from vibe.app_server.models import AgentSafety
from vibe.cli.commands import CommandRegistry
from vibe.cli.textual_ui.widgets.chat_input.body import ChatInputBody
from vibe.cli.textual_ui.widgets.chat_input.container import ChatInputContainer
from vibe.cli.textual_ui.widgets.chat_input.text_area import ChatTextArea
from vibe.cli.voice_manager.voice_manager_port import (
    RecordingStartError,
    TranscribeState,
    VoiceManagerListener,
)

SETTLE_ROUNDS = 6
WORKSPACE_PLACEHOLDER = "__WORKSPACE__"
NAMED_KEYS = {
    "enter": "enter",
    "shift_enter": "shift+enter",
    "backspace": "backspace",
    "delete": "delete",
    "tab": "tab",
    "backtab": "shift+tab",
    "escape": "escape",
    "up": "up",
    "down": "down",
    "left": "left",
    "right": "right",
    "home": "home",
    "end": "end",
    "pageup": "pageup",
    "pagedown": "pagedown",
}
SAFETY_VALUES = {
    "neutral": AgentSafety.NEUTRAL,
    "safe": AgentSafety.SAFE,
    "destructive": AgentSafety.DESTRUCTIVE,
    "yolo": AgentSafety.YOLO,
}


class FakeVoiceManager:
    """Deterministic stand-in for the reference voice port."""

    def __init__(self, *, enabled: bool, start_error: str | None = None) -> None:
        self._enabled = enabled
        self._state = TranscribeState.IDLE
        self._listeners: list[VoiceManagerListener] = []
        self._start_error = start_error
        self.calls: list[str] = []

    @property
    def is_enabled(self) -> bool:
        return self._enabled

    @property
    def transcribe_state(self) -> TranscribeState:
        return self._state

    @property
    def peak(self) -> float:
        return 0.0

    def apply_enabled(self, enabled: bool) -> None:
        self._enabled = enabled

    def start_recording(self, mode: Any = None) -> None:
        self.calls.append("start")
        if self._start_error is not None:
            raise RecordingStartError(self._start_error)
        self._set_state(TranscribeState.RECORDING)

    async def stop_recording(self) -> None:
        self.calls.append("stop")
        self._set_state(TranscribeState.FLUSHING)

    def cancel_recording(self) -> None:
        self.calls.append("cancel")
        self._set_state(TranscribeState.IDLE)

    def add_listener(self, listener: VoiceManagerListener) -> None:
        self._listeners.append(listener)

    def remove_listener(self, listener: VoiceManagerListener) -> None:
        if listener in self._listeners:
            self._listeners.remove(listener)

    async def close(self) -> None:
        return None

    def emit_transcript(self, text: str) -> None:
        for listener in list(self._listeners):
            listener.on_transcribe_text(text)
        self._set_state(TranscribeState.IDLE)

    def _set_state(self, state: TranscribeState) -> None:
        self._state = state
        for listener in list(self._listeners):
            listener.on_transcribe_state_change(state)


class HarnessApp(App[None]):
    CSS = """
    ChatInputContainer { height: auto; }
    #input-box { height: auto; border: round $primary; }
    #input { height: auto; max-height: 10; }
    #prompt { width: 2; }
    """

    def __init__(self, scenario: dict[str, Any], workspace: Path) -> None:
        super().__init__()
        self.scenario = scenario
        self.workspace = workspace
        self.effects: list[dict[str, Any]] = []
        self.voice: FakeVoiceManager | None = None
        registry_options = scenario.get("commands", {})
        self.registry = CommandRegistry(
            excluded_commands=registry_options.get("excluded"),
            vibe_code_enabled=registry_options.get("vibeCodeEnabled", False),
        )
        if scenario.get("voice") is not None:
            self.voice = FakeVoiceManager(
                enabled=scenario["voice"].get("enabled", True),
                start_error=scenario["voice"].get("startError"),
            )
        self.history_file: Path | None = None
        if scenario.get("history") is not None:
            self.history_file = workspace / "vibehistory"
            raw = scenario["history"].get("raw")
            entries = scenario["history"].get("entries", [])
            if raw is not None:
                self.history_file.write_text(raw, encoding="utf-8")
            elif entries:
                self.history_file.write_text(
                    "".join(
                        json.dumps(entry, ensure_ascii=False) + "\n" for entry in entries
                    ),
                    encoding="utf-8",
                )

    def compose(self) -> ComposeResult:
        yield ChatInputContainer(
            command_registry=self.registry,
            history_file=self.history_file,
            safety=SAFETY_VALUES[self.scenario.get("safety", "neutral")],
            agent_name=self.scenario.get("agentName", ""),
            skill_entries_getter=lambda: [
                (entry["command"], entry["description"])
                for entry in self.scenario.get("skills", [])
            ],
            file_watcher_for_autocomplete_getter=lambda: False,
            voice_manager=self.voice,
        )

    def notify(self, message: str, *, title: str = "", severity: str = "information", **_: Any) -> None:  # type: ignore[override]
        self.effects.append(
            {"type": "notify", "message": message, "severity": severity}
        )


class ScenarioRunner:
    def __init__(self, scenario: dict[str, Any], workspace: Path) -> None:
        self.scenario = scenario
        self.workspace = workspace
        self.app = HarnessApp(scenario, workspace)

    async def run(self) -> dict[str, Any]:
        viewport = self.scenario.get("viewport", {"width": 80, "height": 24})
        observations: list[dict[str, Any]] = []
        recorded_events: list[dict[str, Any]] = []
        async with self.app.run_test(
            size=(viewport["width"], viewport["height"])
        ) as pilot:
            container = self.app.query_one(ChatInputContainer)
            widget = container.input_widget
            if widget is None:
                raise RuntimeError("reference composer did not mount an input widget")
            self._instrument(container, widget)
            self._apply_initial_state(container, widget)
            await self._settle(pilot)
            self.app.effects.clear()
            initial = self._capture(container, widget)
            for event in expand_events(self.scenario["events"]):
                await self._dispatch(pilot, container, widget, event)
                await self._settle(pilot)
                recorded_events.append(event)
                observation = self._capture(container, widget)
                observation["index"] = len(recorded_events) - 1
                observation["event"] = event
                observations.append(observation)
        return {
            "id": self.scenario["id"],
            "gap": self.scenario["gap"],
            "story": self.scenario["story"],
            "title": self.scenario["title"],
            "capabilities": self.scenario.get("capabilities", ["core"]),
            "setup": {
                "workspace": self.scenario.get("workspace", "empty"),
                "viewport": viewport,
                "safety": self.scenario.get("safety", "neutral"),
                "agentName": self.scenario.get("agentName", ""),
                "commands": self.scenario.get("commands", {}),
                "skills": self.scenario.get("skills", []),
                "voice": self.scenario.get("voice"),
                "history": self.scenario.get("history"),
                "initial": self.scenario.get("initial", {}),
            },
            "initial": initial,
            "events": recorded_events,
            "observations": observations,
        }

    # -- instrumentation ---------------------------------------------------

    def _instrument(self, container: ChatInputContainer, widget: ChatTextArea) -> None:
        effects = self.app.effects

        def trace(source: Any, translate: Any) -> None:
            original = source.post_message

            def patched(message: Any) -> bool:
                effect = translate(message)
                if effect is not None:
                    effects.append(effect)
                return original(message)

            source.post_message = patched  # type: ignore[method-assign]

        trace(widget, translate_widget_message)
        body = container.query_one(ChatInputBody)
        trace(body, translate_body_message)
        trace(container, translate_container_message)

        if self.app.voice is not None:
            voice = self.app.voice
            original_start = voice.start_recording
            original_stop = voice.stop_recording
            original_cancel = voice.cancel_recording

            def start(mode: Any = None) -> None:
                effects.append({"type": "recordingStartRequested"})
                original_start(mode)

            async def stop() -> None:
                effects.append({"type": "recordingStopRequested"})
                await original_stop()

            def cancel() -> None:
                effects.append({"type": "recordingCancelRequested"})
                original_cancel()

            voice.start_recording = start  # type: ignore[method-assign]
            voice.stop_recording = stop  # type: ignore[method-assign]
            voice.cancel_recording = cancel  # type: ignore[method-assign]

    def _apply_initial_state(
        self, container: ChatInputContainer, widget: ChatTextArea
    ) -> None:
        initial = self.scenario.get("initial", {})
        text = initial.get("text")
        if text:
            container.value = text
        cursor = initial.get("cursor")
        if cursor is not None:
            widget.set_cursor_offset(cursor - widget._get_mode_prefix_length())
        if initial.get("feedbackActive"):
            widget.feedback_active = True
        if initial.get("switching"):
            container.set_switching_mode(True)

    # -- dispatch ----------------------------------------------------------

    async def _dispatch(
        self,
        pilot: Any,
        container: ChatInputContainer,
        widget: ChatTextArea,
        event: dict[str, Any],
    ) -> None:
        kind = event["type"]
        if kind == "key":
            await pilot.press(textual_key(event))
        elif kind == "paste":
            widget.post_message(events.Paste(self._expand(event["text"])))
        elif kind == "resize":
            await pilot.resize_terminal(event["width"], event["height"])
        elif kind == "mouse":
            content_x = event["x"]
            content_y = event["y"]
            x = content_x + widget.gutter_width + widget.gutter.left
            y = content_y + widget.gutter.top
            if 0 <= x < widget.region.width and 0 <= y < widget.region.height:
                mouse_event = (
                    events.MouseMove
                    if event.get("extendSelection", False)
                    else events.MouseDown
                )(
                    widget,
                    x=x,
                    y=y,
                    delta_x=0,
                    delta_y=0,
                    button=1,
                    shift=False,
                    meta=False,
                    ctrl=False,
                    screen_x=widget.region.x + x,
                    screen_y=widget.region.y + y,
                )
                if event.get("extendSelection", False):
                    await widget._on_mouse_move(mouse_event)
                    await widget._on_mouse_up(
                        events.MouseUp(
                            widget,
                            x=x,
                            y=y,
                            delta_x=0,
                            delta_y=0,
                            button=1,
                            shift=False,
                            meta=False,
                            ctrl=False,
                            screen_x=widget.region.x + x,
                            screen_y=widget.region.y + y,
                        )
                    )
                else:
                    await widget._on_mouse_down(mouse_event)
        elif kind == "transcript":
            voice = self.app.voice
            if voice is None:
                raise RuntimeError("transcript event requires a voice scenario")
            voice.emit_transcript(event["text"])
        elif kind == "switching":
            container.set_switching_mode(event["active"])
        elif kind == "feedback":
            widget.feedback_active = event["active"]
        elif kind == "safety":
            container.set_safety(SAFETY_VALUES[event["value"]])
        elif kind == "externalEditor":
            widget.clear()
            widget.insert(event["text"])
        else:
            raise RuntimeError(f"unsupported event type: {kind}")

    def _expand(self, value: str) -> str:
        return value.replace(WORKSPACE_PLACEHOLDER, str(self.workspace))

    async def _settle(self, pilot: Any) -> None:
        for _ in range(SETTLE_ROUNDS):
            await pilot.pause()
            await asyncio.sleep(0)

    # -- observation -------------------------------------------------------

    def _capture(
        self, container: ChatInputContainer, widget: ChatTextArea
    ) -> dict[str, Any]:
        effects = list(self.app.effects)
        self.app.effects.clear()
        observation = {
            "state": capture_state(container, widget),
            "effects": effects,
            "render": capture_render(container, widget),
        }
        submitted = next(
            (
                effect["text"]
                for effect in effects
                if effect.get("type") == "submit"
            ),
            None,
        )
        if submitted is not None and self.scenario.get("captureSubmission", False):
            observation["submission"] = capture_submission(submitted, self.workspace)
        if self.app.history_file is not None:
            observation["history"] = read_history(self.app.history_file)
        return normalise(observation, str(self.workspace))


# --------------------------------------------------------------------------
# Event normalisation
# --------------------------------------------------------------------------


def expand_events(events_in: list[dict[str, Any]]) -> list[dict[str, Any]]:
    expanded: list[dict[str, Any]] = []
    for event in events_in:
        if event["type"] == "text":
            for character in event["text"]:
                expanded.append({"type": "key", "key": "char", "char": character, "mods": []})
            continue
        if event["type"] == "key":
            expanded.append({
                "type": "key",
                "key": event["key"],
                "char": event.get("char"),
                "mods": event.get("mods", []),
            })
            continue
        expanded.append(event)
    return expanded


def textual_key(event: dict[str, Any]) -> str:
    modifiers = set(event.get("mods", []))
    if event["key"] == "char":
        character = event["char"]
        prefix = ""
        if "ctrl" in modifiers:
            prefix += "ctrl+"
        if "alt" in modifiers:
            prefix += "alt+"
        if character == " " and not prefix:
            return "space"
        return f"{prefix}{character}"
    name = NAMED_KEYS.get(event["key"])
    if name is None:
        raise RuntimeError(f"unsupported key: {event['key']}")
    prefix = ""
    if "ctrl" in modifiers:
        prefix += "ctrl+"
    if "shift" in modifiers and not name.startswith("shift+"):
        prefix += "shift+"
    if "alt" in modifiers:
        prefix += "alt+"
    return f"{prefix}{name}"


# --------------------------------------------------------------------------
# Effect normalisation
# --------------------------------------------------------------------------


def translate_widget_message(message: Any) -> dict[str, Any] | None:
    match message:
        case ChatTextArea.Submitted():
            return {"type": "submitRequested", "text": message.value}
        case ChatTextArea.HistoryPrevious():
            return {"type": "historyPrevious"}
        case ChatTextArea.HistoryNext():
            return {"type": "historyNext"}
        case ChatTextArea.HistoryReset():
            return {"type": "historyReset"}
        case ChatTextArea.ModeChanged():
            return {"type": "modeChanged", "mode": message.mode}
        case ChatTextArea.ClipboardImagePasted():
            return {
                "type": "clipboardImageRequested",
                "notifyWhenEmpty": message.notify_when_empty,
            }
        case ChatTextArea.FeedbackKeyPressed():
            return {"type": "feedbackRating", "rating": message.rating}
        case ChatTextArea.SnoozeKeyPressed():
            return {"type": "feedbackSnooze"}
        case ChatTextArea.NonFeedbackKeyPressed():
            return {"type": "feedbackDismissed"}
        case _:
            return None


def translate_body_message(message: Any) -> dict[str, Any] | None:
    match message:
        case ChatInputBody.CompletionResetRequested():
            return {"type": "completionReset"}
        case _:
            return None


def translate_container_message(message: Any) -> dict[str, Any] | None:
    match message:
        case ChatInputContainer.Submitted():
            return {"type": "submit", "text": message.value}
        case _:
            return None


# --------------------------------------------------------------------------
# State and render capture
# --------------------------------------------------------------------------


def capture_state(
    container: ChatInputContainer, widget: ChatTextArea
) -> dict[str, Any]:
    prefix = widget._get_mode_prefix_length()
    selection = widget.selection
    selection_offsets = None
    if selection.start != selection.end:
        start = location_to_offset(widget, selection.start) + prefix
        end = location_to_offset(widget, selection.end) + prefix
        selection_offsets = [min(start, end), max(start, end)]
    manager = container._completion_manager
    active = manager._active
    completion: dict[str, Any] = {"open": False, "kind": None, "selected": 0, "items": []}
    if active is not None and getattr(active, "_suggestions", None):
        kind = "slash" if type(active).__name__.startswith("SlashCommand") else "path"
        completion = {
            "open": True,
            "kind": kind,
            "selected": active._selected_index,
            "items": [
                {"label": label, "description": description}
                for label, description in active._suggestions
            ],
        }
    return {
        "text": widget.get_full_text(),
        "mode": widget.input_mode,
        "cursor": widget._get_full_cursor_offset(),
        "selection": selection_offsets,
        "completion": completion,
        "history": {
            "navigating": widget._navigating_history,
            "loadedEntry": widget._cursor_pos_after_load is not None,
            "cursorMovedSinceLoad": widget._cursor_moved_since_load,
        },
        "feedbackActive": widget.feedback_active,
        "switching": container.switching_mode,
    }


def location_to_offset(widget: ChatTextArea, location: tuple[int, int]) -> int:
    row, column = location
    lines = widget.text.split("\n")
    row = max(0, min(row, len(lines) - 1))
    offset = sum(len(lines[index]) + 1 for index in range(row))
    return offset + max(0, min(column, len(lines[row])))


def capture_render(
    container: ChatInputContainer, widget: ChatTextArea
) -> dict[str, Any]:
    wrapped = widget.wrapped_document
    lines: list[str] = []
    for sections in wrapped.lines:
        lines.extend(sections)
    prompt_widget = container.query("#prompt")
    prompt = None
    if prompt_widget:
        node = prompt_widget.first()
        prompt = str(node.content) if node.display else None
    popup_rows: list[str] = []
    popup_visible = False
    for popup in container.query("CompletionPopup"):
        popup_visible = popup.display
        rows = getattr(popup, "_suggestions", None)
        if rows:
            popup_rows = [label for label, _ in rows]
    return {
        "visualLines": lines,
        "wrapWidth": widget.wrap_width,
        "cursorCell": list(cursor_cell(widget)),
        "prompt": prompt,
        "popupVisible": popup_visible,
        "popupRows": popup_rows,
        "borderTitle": str(container.get_widget_by_id("input-box").border_title or ""),
        "borderClasses": sorted(
            class_name
            for class_name in container.get_widget_by_id("input-box").classes
            if class_name.startswith("border-")
        ),
    }


def cursor_cell(widget: ChatTextArea) -> tuple[int, int]:
    wrapped = widget.wrapped_document
    try:
        offset = wrapped.location_to_offset(widget.cursor_location)
    except Exception:  # pragma: no cover - defensive, mirrors reference guards
        return (0, 0)
    return (offset.y, offset.x)


def normalise(value: Any, workspace: str) -> Any:
    """Replace nondeterministic absolute paths with a stable placeholder."""

    if isinstance(value, str):
        return value.replace(workspace, WORKSPACE_PLACEHOLDER)
    if isinstance(value, list):
        return [normalise(item, workspace) for item in value]
    if isinstance(value, dict):
        return {key: normalise(item, workspace) for key, item in value.items()}
    return value


def read_history(path: Path) -> list[str]:
    if not path.is_file():
        return []
    entries: list[str] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line:
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            entry = line
        entries.append(entry if isinstance(entry, str) else str(entry))
    return entries


def capture_submission(message: str, workspace: Path) -> dict[str, Any]:
    """Run the pinned reference prompt preparation and turn/start wire model."""

    from vibe.app_server._workspace import prepare_prompt
    from vibe.app_server.protocol import TurnStartParams
    from vibe.app_server.session import _content_blocks

    agent_loop = SimpleNamespace(
        cwd=workspace,
        config=SimpleNamespace(
            get_active_model=lambda: SimpleNamespace(
                alias="oracle-model", supports_images=True
            )
        ),
        session_logger=SimpleNamespace(
            session_dir=None, needs_initial_auto_title=lambda: False
        ),
    )
    prepared = prepare_prompt(agent_loop, message)
    request = TurnStartParams(
        session_id="oracle-session",
        input=_content_blocks(prepared.prompt_text, prepared.images),
        client_user_message_id=None,
        auto_title=prepared.auto_title,
        user_display_content=None,
        mention_stats=prepared.mentions,
    )
    wire = request.model_dump(mode="json")
    wire.pop("sessionId")
    return {"prompt": prepared.prompt_text, **wire}


__all__ = ["ScenarioRunner"]
