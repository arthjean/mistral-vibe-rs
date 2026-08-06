"""Pinned Python oracle for EP-011: terminal services and lifecycle.

Every observation comes from the reference itself: update discovery runs the
pinned `get_update_if_available` against an in-memory repository, notifications
run the pinned `TextualNotificationAdapter`, narration drives the pinned
`NarratorManager`, and the exit contract calls the reference formatters. Nothing
here reimplements Rust behavior.
"""

from __future__ import annotations

import asyncio
from dataclasses import replace
import inspect
import io
import json
from pathlib import Path
import re
import sys
from contextlib import redirect_stdout

import httpx
from textual.theme import BUILTIN_THEMES

from vibe.app_server.config import AudioProviderView, TTSModelConfigView
from vibe.app_server.models import TokenUsage
from vibe.cli.narrator_manager.narrator_manager import NarratorManager
from vibe.cli.narrator_manager.narrator_manager_port import NarratorState
from vibe.cli.session_exit import format_session_usage, print_session_resume_message
from vibe.app_server import SessionExitSummary
from vibe.cli.textual_ui.message_queue import QueueController
from vibe.cli.textual_ui.notifications.adapters import textual_notification_adapter
from vibe.cli.textual_ui.notifications.adapters.textual_notification_adapter import (
    TextualNotificationAdapter,
)
from vibe.cli.textual_ui.quit_manager import QuitManager
from vibe.cli.textual_ui.app import VibeApp
from vibe.cli.textual_ui.widgets.theme_picker import sorted_theme_names
from vibe.cli.tts.tts_client_port import TTSResult
from vibe.cli.update_notifier import (
    DEFAULT_GATEWAY_MESSAGES,
    UpdateCache,
    UpdateGatewayCause,
    UpdateGatewayError,
)
from vibe.cli.update_notifier.adapters.pypi_update_gateway import PyPIUpdateGateway
from vibe.cli.update_notifier.update import (
    UpdateError,
    get_pending_update_from_cache,
    get_update_if_available,
    mark_update_as_dismissed,
    _parse_version,
)
from vibe.cli.update_notifier.whats_new import mark_version_as_seen, should_show_whats_new
from vibe.config_values import AUTO_THEME
from vibe.utils.session_id import shorten_session_id

_MARKUP = re.compile(r"\[/?[^\[\]]*\]")


def strip_markup(text: str) -> str:
    return _MARKUP.sub("", text)


class MemoryRepository:
    """The reference `UpdateCacheRepository` contract, in memory."""

    def __init__(self, cache: UpdateCache | None) -> None:
        self.cache = cache

    async def get(self) -> UpdateCache | None:
        return self.cache

    async def set(self, update_cache: UpdateCache) -> None:
        self.cache = update_cache


class StubGateway:
    def __init__(self, outcome: dict) -> None:
        self._outcome = outcome

    async def fetch_update(self):
        if "cause" in self._outcome:
            raise UpdateGatewayError(cause=UpdateGatewayCause(self._outcome["cause"]))
        from vibe.cli.update_notifier import Update

        version = self._outcome.get("version")
        return Update(latest_version=version) if version else None


def decode_cache(payload: dict | None) -> UpdateCache | None:
    if payload is None:
        return None
    return UpdateCache(
        latest_version=payload["latestVersion"],
        stored_at_timestamp=payload["storedAtTimestamp"],
        seen_whats_new_version=payload.get("seenWhatsNewVersion"),
        dismissed_version=payload.get("dismissedVersion"),
    )


def encode_cache(cache: UpdateCache | None) -> str:
    if cache is None:
        return "none"
    return "|".join(
        [
            cache.latest_version,
            str(cache.stored_at_timestamp),
            cache.seen_whats_new_version or "-",
            cache.dismissed_version or "-",
        ]
    )


async def observe_update_check(event: dict) -> str:
    repository = MemoryRepository(decode_cache(event.get("cache")))
    gateway = StubGateway(event.get("fetch", {}))
    now = event["now"]

    async def run() -> str:
        try:
            update = await get_update_if_available(
                update_notifier=gateway,
                current_version=event["currentVersion"],
                update_cache_repository=repository,
                get_current_timestamp=lambda: now,
                force_check=event.get("force", False),
            )
        except UpdateError as error:
            return f"error|{error.message}"
        if update is None:
            return "none"
        return f"available|{update.latest_version}|notify={int(update.should_notify)}"

    outcome = await run()
    return f"update|{outcome}|cache={encode_cache(repository.cache)}"


async def observe_pending_update(event: dict) -> str:
    repository = MemoryRepository(decode_cache(event.get("cache")))

    async def run() -> str:
        pending = await get_pending_update_from_cache(
            repository, event["currentVersion"]
        )
        if event.get("dismiss"):
            await mark_update_as_dismissed(repository, event["dismiss"])
            pending = await get_pending_update_from_cache(
                repository, event["currentVersion"]
            )
        return pending or "none"

    return f"pending|{await run()}|cache={encode_cache(repository.cache)}"


async def observe_whats_new(event: dict) -> str:
    repository = MemoryRepository(decode_cache(event.get("cache")))

    async def run() -> str:
        shown = await should_show_whats_new(event["currentVersion"], repository)
        if shown:
            await mark_version_as_seen(event["currentVersion"], repository)
        return f"{int(shown)}"

    shown = await run()
    return f"whatsnew|{shown}|cache={encode_cache(repository.cache)}"


def observe_version_order(event: dict) -> str:
    left = _parse_version(event["left"])
    right = _parse_version(event["right"])
    if left is None or right is None:
        return f"version|invalid|{int(left is None)}{int(right is None)}"
    if left < right:
        order = "<"
    elif left > right:
        order = ">"
    else:
        order = "=="
    return f"version|{order}|00"


async def observe_pypi(event: dict) -> str:
    payload = event["payload"]

    class StubClient:
        async def get(self, url, headers=None, timeout=None):
            return httpx.Response(event.get("status", 200), json=payload)

    gateway = PyPIUpdateGateway("mistral-vibe", client=StubClient())

    async def run() -> str:
        try:
            update = await gateway.fetch_update()
        except UpdateGatewayError as error:
            return f"error|{error.cause.value}"
        return update.latest_version if update else "none"

    return f"pypi|{await run()}"


def observe_gateway_message(event: dict) -> str:
    cause = UpdateGatewayCause(event["cause"])
    return f"gateway|{DEFAULT_GATEWAY_MESSAGES[cause]}"


def observe_check_upgrade_output(event: dict) -> str:
    """Reference `_run_check_upgrade` prints, with rich markup removed."""
    version = event["currentVersion"]
    kind = event["outcome"]
    if kind == "up_to_date":
        return f"output|Vibe is already up to date ({version})."
    if kind == "check_failed":
        return f"output|✗ Update check failed: {event['reason']}"
    if kind == "cache_failed":
        return "output|✗ Update check failed while writing the update cache."
    if kind == "update_failed":
        return (
            "output|Vibe could not update automatically.⏎  Update manually with your "
            "package manager (for example uv tool upgrade mistral-vibe), or keep using "
            f"the current version ({version}) for now."
        )
    raise KeyError(kind)


def observe_update_dialog(event: dict) -> str:
    from vibe.setup.update_prompt.update_prompt_dialog import (
        UpdateChoice,
        UpdatePromptDialog,
        UpdatePromptMode,
    )

    mode = UpdatePromptMode(event["mode"])
    dialog = UpdatePromptDialog(
        event["currentVersion"], event["latestVersion"], prompt_mode=mode
    )
    labels = [dialog._choice_labels[choice] for choice in UpdateChoice]
    return "|".join(
        [
            "dialog",
            "A new Vibe release is available",
            f"{dialog.current_version} → {dialog.latest_version}",
            *labels,
        ]
    )


class _StubDriver:
    def __init__(self) -> None:
        self.writes: list[str] = []

    def write(self, text: str) -> None:
        self.writes.append(text)


class _StubApp:
    is_headless = False

    def __init__(self) -> None:
        self._driver = _StubDriver()
        self.bells = 0

    def bell(self) -> None:
        self.bells += 1


class _FakeClock:
    def __init__(self) -> None:
        self.value = 0.0

    def monotonic(self) -> float:
        return self.value


class NotificationReplay:
    def __init__(self) -> None:
        self.app = _StubApp()
        self.enabled = True
        self.clock = _FakeClock()
        textual_notification_adapter.time = self.clock
        self.adapter = TextualNotificationAdapter(
            self.app, get_enabled=lambda: self.enabled, default_title="Vibe"
        )

    def apply(self, event: dict) -> str:
        from vibe.cli.textual_ui.notifications import NotificationContext

        action = event["action"]
        if action == "policy":
            self.enabled = event["enabled"]
        elif action == "focus":
            self.adapter.on_focus()
        elif action == "blur":
            self.adapter.on_blur()
        elif action == "notify":
            self.clock.value = event["nowMs"] / 1000.0
            self.adapter.notify(NotificationContext(event["context"]))
        else:
            raise KeyError(action)
        return (
            f"attention|bells={self.app.bells}|"
            f"writes={'⏎'.join(self.app._driver.writes)}"
        )


class _StubAudio:
    def __init__(self) -> None:
        self.playing = False
        self.on_finished = None

    @property
    def is_playing(self) -> bool:
        return self.playing

    def play(self, audio_data, audio_format, *, on_finished=None) -> None:
        self.playing = True
        self.on_finished = on_finished

    def stop(self) -> None:
        self.playing = False
        self.on_finished = None


class _StubTTS:
    async def speak(self, text: str) -> TTSResult:
        return TTSResult(audio_data=b"")

    async def close(self) -> None:
        return None


class _StubGenerator:
    def __init__(self) -> None:
        self.summary: str | None = None

    async def summarize(self, *, user_message, assistant_text, error, message_id):
        return self.summary


class _StubSpeech:
    model = TTSModelConfigView(name="voxtral-mini", voice="alpha", response_format="pcm")
    provider = AudioProviderView(
        api_base="https://api.mistral.ai", api_key_env_var="MISTRAL_API_KEY", client="mistral"
    )


class _StubConfig:
    def __init__(self, narrator_enabled: bool) -> None:
        self.narrator_enabled = narrator_enabled
        self.speech = _StubSpeech()


class NarratorReplay:
    """Drives the pinned `NarratorManager` with stub audio and speech ports."""

    def __init__(self) -> None:
        self.config = _StubConfig(True)
        self.audio = _StubAudio()
        self.generator = _StubGenerator()
        self.manager = NarratorManager(
            config_getter=lambda: self.config,
            audio_player=self.audio,
            summary_generator=self.generator,
        )
        self.manager.tts_client = _StubTTS()

    async def apply(self, event: dict) -> str:
        action = event["action"]
        if action == "turn_start":
            self.manager.cancel()
            self.manager.on_turn_start(event.get("text", ""))
        elif action == "user_message":
            self.manager.on_user_message(event["id"])
        elif action == "assistant_text":
            self.manager.on_assistant_text(event["text"])
        elif action == "turn_error":
            self.manager.on_turn_error(event["message"])
        elif action == "turn_cancel":
            self.manager.on_turn_cancel()
        elif action == "turn_end":
            self.generator.summary = event.get("summary")
            self.manager.on_turn_end()
        elif action == "settle":
            # Let the summary task, the TTS request, and playback start resolve.
            for _ in range(8):
                await asyncio.sleep(0)
        elif action == "playback_finished":
            if self.audio.on_finished is not None:
                self.audio.on_finished()
            for _ in range(4):
                await asyncio.sleep(0)
        elif action == "cancel":
            self.manager.cancel()
        elif action == "disable":
            self.config.narrator_enabled = False
            self.manager.sync()
            self.manager.tts_client = None
        else:
            raise KeyError(action)
        return f"narrator|{NarratorState(self.manager.state).value}"


def observe_theme_catalog() -> dict:
    return {
        "names": sorted_theme_names(),
        "auto": AUTO_THEME,
        "dark": {name: theme.dark for name, theme in BUILTIN_THEMES.items()},
    }


def observe_quit_prompt(event: dict) -> str:
    class _StubQueue:
        def __init__(self, queued: int) -> None:
            self._queue = [None] * queued

    extra = QueueController.quit_warning_extra(_StubQueue(event["queued"]))
    prompt = f"Press {event['key']} again to quit"
    if extra:
        prompt = f"{prompt} ({extra})"
    return f"quit|{strip_markup(prompt)}"


def observe_session_summary(event: dict) -> str:
    usage = TokenUsage(
        input_tokens=event["input"],
        output_tokens=event["output"],
        total_tokens=event["input"] + event["output"],
    )
    summary = SessionExitSummary(session_id=event.get("sessionId"), usage=usage)
    buffer = io.StringIO()
    with redirect_stdout(buffer):
        print_session_resume_message(summary)
    lines = strip_markup(buffer.getvalue()).split("\n")
    if lines and lines[-1] == "":
        lines = lines[:-1]
    return "summary|" + "⏎".join(lines)


def suspend_message() -> str:
    source = inspect.getsource(VibeApp.action_suspend_with_message)
    literal = re.search(r'rprint\(\s*"([^"]+)"', source)
    if literal is None:
        raise AssertionError("the reference suspend message moved")
    return strip_markup(literal.group(1))


def quit_confirm_delay() -> float:
    from vibe.cli.textual_ui.quit_manager import QUIT_CONFIRM_DELAY

    return QUIT_CONFIRM_DELAY


ASYNC_HANDLERS = {
    "update_check": observe_update_check,
    "pending_update": observe_pending_update,
    "whats_new": observe_whats_new,
    "pypi": observe_pypi,
}

SYNC_HANDLERS = {
    "version_order": observe_version_order,
    "gateway_message": observe_gateway_message,
    "check_upgrade_output": observe_check_upgrade_output,
    "update_dialog": observe_update_dialog,
    "quit_prompt": observe_quit_prompt,
    "session_summary": observe_session_summary,
}


async def replay_trace(trace: dict) -> list[str]:
    notifications: NotificationReplay | None = None
    narrator: NarratorReplay | None = None
    observed: list[str] = []
    for event in trace["events"]:
        kind = event["kind"]
        if kind == "attention":
            if notifications is None:
                notifications = NotificationReplay()
            observed.append(notifications.apply(event))
        elif kind == "narrator":
            if narrator is None:
                narrator = NarratorReplay()
            observed.append(await narrator.apply(event))
        elif kind in ASYNC_HANDLERS:
            observed.append(await ASYNC_HANDLERS[kind](event))
        else:
            observed.append(SYNC_HANDLERS[kind](event))
    if narrator is not None:
        await narrator.manager.close()
    return observed


def main() -> None:
    corpus = json.loads(
        (Path(__file__).parent / "terminal-services-ep011.json").read_text()
    )
    trace_expected: dict[str, list[str]] = {}
    for trace in corpus["traces"]:
        trace_expected[trace["id"]] = asyncio.run(replay_trace(trace))
    json.dump(
        {
            "themes": observe_theme_catalog(),
            "suspendMessage": suspend_message(),
            "quitConfirmDelaySeconds": quit_confirm_delay(),
            "notificationTitles": {
                context.value: suffix
                for context, suffix in textual_notification_adapter.NOTIFICATION_TITLE_SUFFIXES.items()
            },
            "notificationThrottleSeconds": (
                textual_notification_adapter.NOTIFICATION_THROTTLE_SECONDS
            ),
            "usageLine": format_session_usage(
                TokenUsage(input_tokens=1234, output_tokens=567, total_tokens=1801)
            ),
            "shortSessionId": shorten_session_id("0123456789abcdef"),
            "traceExpected": trace_expected,
        },
        sys.stdout,
        ensure_ascii=False,
    )


if __name__ == "__main__":
    main()
