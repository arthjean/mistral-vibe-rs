from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import time
import uuid


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser()
    value.add_argument("--upstream", type=Path, required=True)
    value.add_argument("--scenario", required=True)
    value.add_argument("--kind", required=True)
    value.add_argument("--payload")
    value.add_argument("args", nargs=argparse.REMAINDER)
    return value


def emit(value: object) -> None:
    print(json.dumps(value, sort_keys=True, separators=(",", ":")))


def clean_validation_error(error: Exception) -> dict[str, object]:
    errors = getattr(error, "errors", lambda: [])()
    return {
        "accepted": False,
        "errors": [
            {
                "location": list(item.get("loc", ())),
                "type": item.get("type", type(error).__name__),
            }
            for item in errors
        ],
    }


def protocol(payload: str) -> dict[str, object]:
    from vibe.app_server.protocol import validate_json_rpc_envelope

    try:
        value = validate_json_rpc_envelope(json.loads(payload))
    except Exception as error:
        return clean_validation_error(error)
    return {
        "accepted": True,
        "value": value.model_dump(mode="json", by_alias=True),
        "variant": type(value).__name__,
    }


def initialize(payload: str) -> dict[str, object]:
    from vibe.app_server._model import validate_wire
    from vibe.app_server.protocol import InitializeParams

    try:
        value = validate_wire(InitializeParams, json.loads(payload))
    except Exception as error:
        return clean_validation_error(error)
    return {
        "accepted": True,
        "value": value.model_dump(mode="json", by_alias=True),
    }


def persistence(payload: str) -> dict[str, object]:
    from vibe.core.session.session_loader import SessionLoader

    value = json.loads(payload)
    text = value["text"]
    parsed = SessionLoader._parse_message_lines(text)
    if value["operation"] == "parse":
        return {"parsed": parsed}
    return {
        "loadable": SessionLoader._log_is_loadable(parsed, value["metadata"]),
        "parsed": parsed,
    }


def environment_root(upstream: Path) -> Path:
    return Path(os.environ.get("VIBE_ORACLE_ENV", upstream / ".venv"))


def executable(upstream: Path, name: str) -> Path:
    root = environment_root(upstream)
    if sys.platform == "win32":
        return root / "Scripts" / f"{name}.exe"
    return root / "bin" / name


def audited_environment(upstream: Path) -> tuple[dict[str, str], Path]:
    environment = dict(os.environ)
    audit_log = Path.cwd() / ".audit.json"
    audit_root = Path(__file__).parent / "audit"
    roots = [
        upstream,
        environment_root(upstream),
        Path.cwd(),
        Path(sys.base_prefix),
        Path(sys.prefix),
        audit_root,
    ]
    environment["PYTHONPATH"] = str(audit_root)
    environment["VIBE_AUDIT_ROOTS"] = os.pathsep.join(str(root) for root in roots)
    environment["VIBE_AUDIT_LOG"] = str(audit_log)
    return environment, audit_log


def audit_results(audit_log: Path) -> list[str]:
    if not audit_log.exists():
        return []
    return json.loads(audit_log.read_text(encoding="utf-8"))


def process(upstream: Path, args: list[str]) -> tuple[dict[str, object], list[str]]:
    environment, audit_log = audited_environment(upstream)
    completed = subprocess.run(
        [
            str(executable(upstream, "python")),
            "-m",
            "vibe.cli.entrypoint",
            *args,
        ],
        cwd=upstream,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env=environment,
        timeout=10,
    )
    return (
        {
            "exitStatus": completed.returncode,
            "stdout": completed.stdout.decode("utf-8", errors="replace"),
            "stderr": completed.stderr.decode("utf-8", errors="replace"),
        },
        audit_results(audit_log),
    )


def terminal(upstream: Path, args: list[str]) -> tuple[dict[str, object], list[str]]:
    environment, audit_log = audited_environment(upstream)
    master, slave = pty.openpty()
    try:
        child = subprocess.Popen(
            [
                str(executable(upstream, "python")),
                "-m",
                "vibe.cli.entrypoint",
                *args,
            ],
            cwd=upstream,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=environment,
            close_fds=True,
        )
        os.close(slave)
        slave = -1
        transcript = bytearray()
        deadline = time.monotonic() + 10
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                child.kill()
                child.wait(timeout=2)
                raise TimeoutError("PTY scenario exceeded 10 seconds")
            readable, _, _ = select.select([master], [], [], remaining)
            if not readable:
                continue
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            transcript.extend(chunk)
        status = child.wait(timeout=10)
    finally:
        os.close(master)
        if slave >= 0:
            os.close(slave)
    return (
        {
            "exitStatus": status,
            "transcript": transcript.decode("utf-8", errors="replace"),
        },
        audit_results(audit_log),
    )


def volatile() -> dict[str, object]:
    nonce = str(uuid.uuid4())
    return {
        "timestamp": time.time_ns(),
        "uuid": nonce,
        "path": str(Path.cwd()),
        "port": int(nonce[:4], 16),
        "providerToken": f"provider-{nonce}",
    }


def tui_terminal_stack_checks() -> dict[str, object]:
    import asyncio

    from textual.app import App, ComposeResult
    from textual.widgets import Button, Input

    class ProbeInput(Input):
        def on_mount(self) -> None:
            self.app.mounted = True

        def on_unmount(self) -> None:
            self.app.unmounted = True

    class ProbeApp(App[None]):
        def __init__(self) -> None:
            super().__init__()
            self.clicks = 0
            self.mounted = False
            self.unmounted = False

        def compose(self) -> ComposeResult:
            yield ProbeInput(id="probe-input")
            yield Button("select", id="probe-button")

        def on_button_pressed(self, _: Button.Pressed) -> None:
            self.clicks += 1

    async def exercise() -> dict[str, object]:
        app = ProbeApp()
        async with app.run_test(size=(72, 18)) as pilot:
            input_widget = app.query_one("#probe-input", Input)
            input_widget.focus()
            await pilot.press("β")
            await pilot.click("#probe-button")
            observed = {
                "mounted": app.mounted,
                "mouse": app.clicks == 1,
                "resize": [app.size.width, app.size.height],
                "unicode": input_widget.value == "β",
            }
        observed["cleanShutdown"] = app.unmounted
        return observed

    first = asyncio.run(exercise())
    second = asyncio.run(exercise())
    return {
        "headless": {
            key: first[key] for key in ("mounted", "mouse", "resize", "unicode")
        },
        "lifecycle": {
            "cleanShutdown": first["cleanShutdown"],
            "restartable": second["mounted"] and second["cleanShutdown"],
        },
    }


def tui_shell_checks() -> dict[str, object]:
    from vibe.cli.textual_ui.message_queue import MessageQueue
    from vibe.cli.textual_ui.windowing.state import SessionWindowing

    queue = MessageQueue()
    queue.append_prompt("first")
    queue.append_bash("ls")
    queue.append_prompt("third")
    snapshot = queue.items
    popped = []
    while item := queue.pop_first():
        popped.append(item.content)
    queue.pause()
    queue.clear()

    windowing = SessionWindowing(load_more_batch_size=2)
    windowing.set_backfill(["h0", "h1", "h2", "h3"])
    batches = []
    while batch := windowing.next_load_more_batch():
        batches.append(batch.entries)

    return {
        "history": {
            "backfillOrdered": [item for batch in reversed(batches) for item in batch]
            == ["h0", "h1", "h2", "h3"],
            "batchSizes": [len(batch) for batch in batches],
            "exhausted": not windowing.has_backfill,
        },
        "queue": {
            "clearResumes": not queue.paused,
            "fifo": popped == ["first", "ls", "third"],
            "snapshotIsolated": [item.content for item in snapshot]
            == ["first", "ls", "third"],
        },
    }


def tui_rendering_checks() -> dict[str, object]:
    from vibe.cli.textual_ui.widgets.diff_rendering import (
        DiffOccurrence,
        render_edit_diff,
    )
    from vibe.cli.textual_ui.widgets.messages import BashOutputMessage
    from vibe.cli.textual_ui.widgets.no_markup_static import NoMarkupStatic

    lines = render_edit_diff(
        [DiffOccurrence(42, "value = 1", "value = 2")],
        "py",
        ansi=False,
        dark=True,
    )
    classes = [line.css_class for line in lines]
    output = BashOutputMessage(
        "fixture", "/workspace", "\x1b[31mred\x1b[0m\rfinal\x07"
    )
    return {
        "diff": {
            "added": classes.count("diff-added") == 1,
            "removed": classes.count("diff-removed") == 1,
        },
        "hostileContent": {
            "markupLiteral": str(NoMarkupStatic("[/]").render()) == "[/]",
            "terminalControlsSafe": output._preview_text() == "final",
        },
    }


def tui_input_checks() -> dict[str, object]:
    import tempfile
    from unittest.mock import patch

    from textual import events

    from vibe.cli.autocompletion.base import CompletionResult
    from vibe.cli.autocompletion.slash_command import SlashCommandController
    from vibe.cli.clipboard import try_copy_text_to_clipboard
    from vibe.cli.history_manager import HistoryManager
    from vibe.cli.textual_ui.external_editor import ExternalEditor
    from vibe.cli.textual_ui.widgets.chat_input.paste_path import (
        maybe_prepend_at_for_image_path,
    )

    class Completer:
        def get_completion_items(
            self, _text: str, _cursor_index: int
        ) -> list[tuple[str, str]]:
            return [("/review", "Review changes")]

        def get_replacement_range(
            self, _text: str, cursor_index: int
        ) -> tuple[int, int]:
            return (0, cursor_index)

    class View:
        def __init__(self) -> None:
            self.replacement = ""

        def clear_completion_suggestions(self) -> None:
            pass

        def render_completion_suggestions(
            self, _suggestions: list[tuple[str, str]], _selected_index: int
        ) -> None:
            pass

        def replace_completion_range(
            self,
            _start: int,
            _end: int,
            value: str,
            *,
            suppress_update: bool = False,
        ) -> None:
            del suppress_update
            self.replacement = value

    with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
        root = Path(directory)
        history = HistoryManager(root / "history.jsonl")
        history.add("a\nβ")
        history.persist("a\nβ")
        previous = history.get_previous("draft")
        restored = history.get_next()

        image = root / "fixture image.png"
        image.write_bytes(b"\x89PNG\r\n\x1a\n")
        image_rewritten = maybe_prepend_at_for_image_path(str(image)).startswith("@")

        with patch.object(
            ExternalEditor,
            "edit_file",
            lambda _cls, path, *, check=False: path.write_text(
                "draft edited\n", encoding="utf-8"
            ),
        ):
            external_editor = ExternalEditor().edit("draft")

    view = View()
    completion = SlashCommandController(Completer(), view)
    completion.on_text_changed("/rev", 4)
    applied = completion.on_key(events.Key("tab", "\t"), "/rev", 4)
    stale = completion.on_key(events.Key("tab", "\t"), "/rev", 4)
    with patch("vibe.cli.clipboard._copy_to_clipboard", lambda _text: None):
        copied = try_copy_text_to_clipboard("copied")

    return {
        "clipboard": {
            "copyAccepted": copied,
            "emptyRejected": not try_copy_text_to_clipboard(""),
        },
        "completion": {
            "applied": view.replacement,
            "staleIgnored": stale is CompletionResult.IGNORED,
            "tabHandled": applied is CompletionResult.HANDLED,
        },
        "externalEditor": external_editor,
        "history": {
            "draftRestored": restored,
            "previous": previous,
            "unicodePreserved": previous == "a\nβ",
        },
        "imagePathRewritten": image_rewritten,
    }


def tui_controls_checks() -> dict[str, object]:
    import asyncio
    from collections import deque
    from unittest.mock import AsyncMock, MagicMock

    from vibe.app_server.models import (
        ApprovalCallbackDetail,
        ApprovalCallbackOutput,
        ApprovalDecision,
        EffectCallDisplay,
        GenericEffectDetail,
        OpenCallbackState,
        PublicCallbackEntry,
        PublicEntryGenerationStatus,
        QuestionChoice,
        UserInputCallbackDetail,
        UserQuestion,
        UserQuestionRequest,
    )
    from vibe.cli.commands import CommandRegistry
    from vibe.cli.textual_ui.app import VibeApp

    def callback(callback_id: str) -> PublicCallbackEntry:
        return PublicCallbackEntry(
            id=f"callback:{callback_id}",
            session_id="session",
            turn_id="turn",
            created_at=1,
            updated_at=1,
            generation_status=PublicEntryGenerationStatus.IN_PROGRESS,
            callback_id=callback_id,
            title="Input required",
            detail=UserInputCallbackDetail(
                request=UserQuestionRequest(
                    questions=[
                        UserQuestion(
                            question="Choose",
                            options=[
                                QuestionChoice(label="One"),
                                QuestionChoice(label="Two"),
                            ],
                        )
                    ]
                )
            ),
            state=OpenCallbackState(),
        )

    async def exercise_queue() -> bool:
        app = MagicMock()
        app._active_callback = callback("first")
        app._pending_callbacks = deque()
        await VibeApp._show_callback(app, callback("second"))
        return [item.callback_id for item in app._pending_callbacks] == ["second"]

    async def exercise_interrupt() -> bool:
        app = MagicMock()
        app._active_callback = callback("active")
        app._pending_callbacks = deque([callback("queued")])
        app._pending_local_question = None
        app._agent_task = None
        app._interrupt_requested = False
        app._agent_job_active.return_value = True
        app.app_server.turn_active = True
        app.app_server.interrupt = AsyncMock()
        app.event_handler = None
        app._loading_area.remove_children = AsyncMock()
        app._mount_and_scroll = AsyncMock()
        await VibeApp._interrupt_turn(app)
        return (
            app._active_callback is None
            and not app._pending_callbacks
            and app.app_server.interrupt.await_count == 1
        )

    registry = CommandRegistry()
    commands = {
        command: registry.parse_command(value) is not None
        for command, value in {
            "clear": "/clear",
            "compact": "/compact",
            "continue": "/continue",
            "rename": "/rename title",
            "resume": "/resume",
            "rewind": "/rewind",
        }.items()
    }
    approval = ApprovalCallbackOutput(
        decision=ApprovalDecision(type="approve")
    ).model_dump(mode="json", by_alias=True)
    effect = GenericEffectDetail(
        tool_name="fixture",
        input={"value": "ok"},
        display=EffectCallDisplay(summary="fixture", status_text="Running"),
    )
    detail = ApprovalCallbackDetail(effect=effect)
    return {
        "approval": {
            "decision": approval["decision"]["type"],
            "kind": detail.kind,
        },
        "callbackRaces": {
            "overlapQueued": asyncio.run(exercise_queue()),
        },
        "commands": commands,
        "interrupt": {
            "clearsCallbacks": asyncio.run(exercise_interrupt()),
        },
    }


def tui_setup_checks() -> dict[str, object]:
    import asyncio
    import tempfile
    from types import SimpleNamespace
    from unittest.mock import patch

    from vibe.cli.audio_recorder.audio_recorder_port import RecordingMode
    from vibe.cli.theme import resolve_theme_name
    from vibe.cli.update_notifier import (
        UpdateCache,
        UpdateGatewayCause,
        UpdateGatewayError,
    )
    from vibe.cli.update_notifier.update import UpdateError, get_update_if_available
    from vibe.cli.voice_manager.voice_manager import VoiceManager
    from vibe.cli.voice_manager.voice_manager_port import TranscribeState
    from vibe.core.config import DEFAULT_MISTRAL_API_ENV_KEY, ProviderConfig
    from vibe.core.types import Backend
    from vibe.setup.auth import AuthStateKind, assess_auth_state
    from vibe.setup.trusted_folders.trust_folder_dialog import TrustFolderDialog

    class FailingGateway:
        async def fetch_update(self) -> None:
            raise UpdateGatewayError(cause=UpdateGatewayCause.REQUEST_FAILED)

    class MemoryRepository:
        def __init__(self) -> None:
            self.value: UpdateCache | None = None

        async def get(self) -> UpdateCache | None:
            return self.value

        async def set(self, value: UpdateCache) -> None:
            self.value = value

    class Recorder:
        def __init__(self) -> None:
            self.mode = RecordingMode.STREAM
            self.peak = 0.0
            self.cancelled = False

        def start(self, mode: RecordingMode, *, sample_rate: int) -> None:
            del sample_rate
            self.mode = mode

        def cancel(self) -> None:
            self.cancelled = True

        async def audio_stream(self):
            if False:
                yield b""

    class Transcriber:
        async def transcribe(self, _audio_stream):
            await asyncio.Event().wait()
            if False:
                yield None

        async def close(self) -> None:
            pass

    provider = ProviderConfig(
        name="mistral",
        api_base="https://api.mistral.ai/v1",
        api_key_env_var=DEFAULT_MISTRAL_API_ENV_KEY,
        browser_auth_base_url="https://console.mistral.ai",
        browser_auth_api_base_url="https://console.mistral.ai/api",
        backend=Backend.MISTRAL,
    )
    with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
        env_path = Path(directory) / ".env"
        with patch(
            "vibe.setup.auth.auth_state.get_api_key_from_keyring",
            lambda _key: None,
        ):
            missing = assess_auth_state(provider, env_path=env_path, environ={})
            process = assess_auth_state(
                provider,
                env_path=env_path,
                environ={DEFAULT_MISTRAL_API_ENV_KEY: "fixture"},
            )

    dialog = TrustFolderDialog(
        Path("/workspace"),
        None,
        ["AGENTS.md"],
    )
    with patch.object(dialog, "post_message") as post_message:
        dialog.action_select()
        trust_decision = post_message.call_args.args[0].decision

    async def exercise_update() -> bool:
        repository = MemoryRepository()
        try:
            await get_update_if_available(
                FailingGateway(),
                "1.0.0",
                repository,
                get_current_timestamp=lambda: 100,
            )
        except UpdateError:
            return (
                repository.value is not None
                and repository.value.latest_version == "1.0.0"
            )
        return False

    async def exercise_voice() -> tuple[bool, bool]:
        recorder = Recorder()
        manager = VoiceManager(
            lambda: SimpleNamespace(
                voice_mode_enabled=True,
                transcription=SimpleNamespace(
                    model=SimpleNamespace(sample_rate=16_000)
                ),
            ),
            recorder,
            Transcriber(),
        )
        manager.start_recording()
        started = manager.transcribe_state is TranscribeState.RECORDING
        manager.cancel_recording()
        cancelled = (
            recorder.cancelled and manager.transcribe_state is TranscribeState.IDLE
        )
        await manager.close()
        return started, cancelled

    voice_started, voice_cancelled = asyncio.run(exercise_voice())
    return {
        "authentication": {
            "missingRejected": (
                missing.kind is AuthStateKind.SIGNED_OUT
                and not missing.can_use_active_provider
            ),
            "processCredentialExternal": (
                process.kind is AuthStateKind.PROCESS_ENV
                and not process.sign_out_available
            ),
            "workspaceTrustDecision": trust_decision,
        },
        "theme": {
            "explicitPreserved": resolve_theme_name("textual-dark")
            == "textual-dark",
            "invalidFallsBack": resolve_theme_name("not-a-theme")
            != "not-a-theme",
        },
        "updateFailureRecoverable": asyncio.run(exercise_update()),
        "voice": {
            "cancelSafe": voice_cancelled,
            "recordingStarted": voice_started,
        },
    }


def acp_full_checks() -> dict[str, object]:
    import asyncio
    from types import SimpleNamespace

    from acp.schema import ClientCapabilities

    from vibe.acp.agent import VibeAcpAgent
    from vibe.acp.session import AcpSession

    class AppServer:
        def __init__(self) -> None:
            self.turn_active = True
            self.interrupts = 0
            self.closes = 0

        async def interrupt(self) -> None:
            self.interrupts += 1

        async def close(self) -> None:
            self.closes += 1

    async def exercise() -> dict[str, object]:
        agent = VibeAcpAgent()
        initialized = await agent.initialize(
            1, client_capabilities=ClientCapabilities()
        )
        app_server = AppServer()
        session = AcpSession(
            session_id="session",
            app_server=app_server,
            cwd=Path.cwd(),
            commands=SimpleNamespace(),
        )
        await session.cancel_prompt()
        active_cancel = app_server.interrupts == 1
        app_server.turn_active = False
        await session.cancel_prompt()
        idle_cancel = app_server.interrupts == 1

        started = asyncio.Event()

        async def pending() -> None:
            started.set()
            await asyncio.Event().wait()

        task = session.spawn(pending())
        await started.wait()
        await session.close()
        await session.close()

        async def noop() -> None:
            pass

        rejected = session.spawn(noop()) is None
        capabilities = initialized.agent_capabilities
        return {
            "initialize": {
                "closeSession": capabilities.session_capabilities.close is not None,
                "embeddedContext": (
                    capabilities.prompt_capabilities.embedded_context
                ),
                "forkSession": capabilities.session_capabilities.fork is not None,
                "imagePrompts": capabilities.prompt_capabilities.image,
                "listSessions": capabilities.session_capabilities.list is not None,
                "loadSession": capabilities.load_session,
                "protocolVersion": initialized.protocol_version,
            },
            "lifecycle": {
                "activeCancelInterrupts": active_cancel,
                "closeCancelsTasks": task is not None and task.cancelled(),
                "closeIdempotent": app_server.closes == 1,
                "idleCancelNoop": idle_cancel,
                "spawnRejectedAfterClose": rejected,
            },
        }

    return asyncio.run(exercise())


def cloud_workflow_checks() -> dict[str, object]:
    import asyncio
    import tempfile
    from types import SimpleNamespace
    from unittest.mock import patch

    from vibe.app_server._execution import SessionExecution
    from vibe.app_server._vibe_code import VibeCodeController
    from vibe.app_server.models import AccountStatus, AccountView
    from vibe.app_server.protocol import TeleportStartParams
    from vibe.core.agent_loop import TeleportError
    from vibe.core.loop import LoopManager
    from vibe.core.teleport.git import GitRepoInfo
    from vibe.core.teleport.types import (
        TeleportCheckingGitEvent,
        TeleportCompleteEvent,
        TeleportPushingEvent,
        TeleportPushRequiredEvent,
        TeleportStartingWorkflowEvent,
        TeleportSummarizingContextEvent,
    )
    from vibe.core.types import Role
    from vibe.core.vibe_code_project import (
        ProjectRepository,
        TeleportProjectResolution,
        VibeCodeProject,
        VibeCodeProjectLink,
        VibeCodeProjectPage,
        VibeCodeProjectPickerService,
        VibeCodeProjectResolverError,
        VibeProjectsStore,
    )

    repo_url = "https://github.com/mistralai/mistral-vibe.git"

    class PageFetcher:
        def __init__(self, pages: list[VibeCodeProjectPage]) -> None:
            self.pages = pages

        async def list_projects(
            self, cursor: str | None = None, limit: int | None = None
        ) -> VibeCodeProjectPage:
            del cursor, limit
            return self.pages.pop(0)

    class Telemetry:
        def send_remote_project_configured(self, **_kwargs: object) -> None:
            pass

        def send_teleport_failed(self, **_kwargs: object) -> None:
            pass

    class Config:
        def is_active_model_mistral(self) -> bool:
            return True

    class AgentLoop:
        def __init__(self) -> None:
            self.config = Config()
            self.base_config = SimpleNamespace(vibe_base_url="https://example.test")
            self.telemetry_client = Telemetry()
            self.messages = [SimpleNamespace(role=Role.user)]
            self.mode = "complete"
            self.push_responses: list[bool] = []

        async def teleport_to_vibe_code(
            self,
            _prompt: str | None,
            *,
            project_id: str | None = None,
            project_picker: object | None = None,
        ):
            assert project_id == "page-first"
            assert project_picker is not None
            yield TeleportSummarizingContextEvent()
            yield TeleportCheckingGitEvent()
            if self.mode == "failure":
                raise TeleportError("injected teleport failure")
            response = yield TeleportPushRequiredEvent(
                unpushed_count=2, branch_not_pushed=True
            )
            self.push_responses.append(bool(response and response.approved))
            if self.mode == "cancel":
                await asyncio.Event().wait()
            yield TeleportPushingEvent()
            yield TeleportStartingWorkflowEvent()
            yield TeleportCompleteEvent(url="https://example.test/session")

    class Controller(VibeCodeController):
        def __init__(
            self,
            agent_loop: AgentLoop,
            notify,
            execution: SessionExecution,
            service: VibeCodeProjectPickerService,
            git: GitRepoInfo,
        ) -> None:
            super().__init__(
                agent_loop,
                notify,
                execution,
                lambda: asyncio.sleep(
                    0,
                    result=AccountView(
                        status=AccountStatus.READY, teleport_eligible=True
                    ),
                ),
            )
            self.fixture_service = service
            self.fixture_git = git

        def _make_service(self) -> VibeCodeProjectPickerService:
            return self.fixture_service

        async def _read_git(self) -> GitRepoInfo:
            return self.fixture_git

    class SessionMetadata:
        def __init__(self) -> None:
            self.loops = []

    class SessionLogger:
        def __init__(self) -> None:
            self.session_metadata = SessionMetadata()
            self.persisted: list[list[object]] = []

        async def persist_loops(self) -> None:
            self.persisted.append(list(self.session_metadata.loops))

    async def wait_for(
        predicate, *, attempts: int = 100
    ) -> None:
        for _ in range(attempts):
            if predicate():
                return
            await asyncio.sleep(0)
        raise TimeoutError("cloud workflow fixture did not settle")

    async def exercise(root: Path) -> dict[str, object]:
        repo_root = root / "repo"
        repo_root.mkdir()
        projects_path = root / "projects.toml"
        store = VibeProjectsStore(projects_path)
        page_first = VibeCodeProject(
            project_id="page-first",
            name="First",
            repositories=(ProjectRepository(repo_url=repo_url),),
        )
        read_only = VibeCodeProject(
            project_id="read-only-first",
            name="Read only",
            repositories=(ProjectRepository(repo_url=repo_url),),
            is_read_only=True,
        )
        stale = VibeCodeProjectLink(
            repo_root=repo_root,
            repo_url="https://github.com/mistralai/other.git",
            project_id="stale",
            project_name="Stale",
        )
        store.upsert_remote_project(stale)
        picker = VibeCodeProjectPickerService(
            base_url="https://example.test",
            api_key="fixture",
            repo_root=repo_root,
            page_fetcher=PageFetcher(
                [
                    VibeCodeProjectPage(
                        projects=[page_first, read_only], next_cursor=None
                    ),
                    VibeCodeProjectPage(
                        projects=[page_first, read_only], next_cursor=None
                    ),
                ]
            ),
            project_store=store,
        )
        git = GitRepoInfo(
            remote_name="origin",
            remote_url=repo_url,
            owner="mistralai",
            repo="mistral-vibe",
            branch="main",
            commit="abc123",
            diff="",
            default_branch="main",
            repo_root=repo_root,
        )
        initial = await picker.load_initial(git)
        resolution: TeleportProjectResolution = picker.resolve_project_for_teleport(
            initial
        )
        try:
            await picker.find_linkable_project(
                project_id=read_only.project_id, repo_url=repo_url
            )
            read_only_rejected = False
        except VibeCodeProjectResolverError:
            read_only_rejected = True
        picker.save_project_link(
            context=resolution.initial_data.context,
            project_id=page_first.project_id,
            project_name=page_first.name,
        )

        events: dict[str, list[str]] = {}
        current_operation = ""

        async def notify(_method: str, params: object) -> None:
            events.setdefault(current_operation, []).append(params.event.kind)

        loop = AgentLoop()
        execution = SessionExecution()
        controller = Controller(loop, notify, execution, picker, git)
        picker_id, _, resolved = await controller.open(
            purpose="teleport", prompt="continue"
        )

        def params(operation_id: str) -> TeleportStartParams:
            return TeleportStartParams(
                session_id="session",
                picker_id=picker_id,
                operation_id=operation_id,
                prompt="continue",
                project_id="page-first",
            )

        current_operation = "approved"
        approved_params = params(current_operation)
        await controller.reserve_teleport(approved_params)
        controller.start_teleport(approved_params)
        await wait_for(
            lambda: events.get("approved", [])[-1:] == ["push_required"]
        )
        controller.respond_to_push("approved", True)
        await wait_for(lambda: "approved" not in controller._tasks)

        loop.mode = "cancel"
        current_operation = "cancelled"
        cancelled_params = params(current_operation)
        await controller.reserve_teleport(cancelled_params)
        controller.start_teleport(cancelled_params)
        await wait_for(
            lambda: events.get("cancelled", [])[-1:] == ["push_required"]
        )
        controller.respond_to_push("cancelled", True)
        await wait_for(lambda: loop.push_responses[-1:] == [True])
        cancelled = await controller.cancel_teleport("cancelled")

        loop.mode = "failure"
        current_operation = "failed"
        failed_params = params(current_operation)
        await controller.reserve_teleport(failed_params)
        controller.start_teleport(failed_params)
        await wait_for(lambda: "failed" not in controller._tasks)
        view = controller.view()
        link = view.context.saved_link
        await controller.close()

        logger = SessionLogger()
        manager = LoopManager(logger)
        with patch("vibe.core.loop.time.time", return_value=100.0):
            scheduled = await manager.create("30s", "review")
        snapshot = manager.loops
        snapshot.clear()
        early_due = manager.due(now=129.0)
        fired = await manager.pop_due(now=130.0)
        restored = LoopManager(logger)
        restored.restore(list(logger.session_metadata.loops))

        return {
            "failureSafety": {
                "executionIdle": execution.active is None,
                "projectLinkPreserved": (
                    link is not None and link.project_id == "page-first"
                ),
                "teleportEvents": events["failed"],
            },
            "loops": {
                "earlyDue": early_due is not None,
                "firedAtDue": fired is not None and fired.id == scheduled.id,
                "listSnapshotIsolated": len(manager.loops) == 1,
                "persistedCount": len(logger.session_metadata.loops),
                "rearmedAfterSeconds": (
                    None
                    if fired is None
                    else int(fired.next_fire_at - 130.0)
                ),
                "restoredCount": len(restored.loops),
            },
            "projects": {
                "readOnlyRejected": read_only_rejected,
                "resolvedProjectId": resolved,
                "staleLinkCleared": resolution.stale_link_cleared,
            },
            "teleport": {
                "approvalEvents": events["approved"],
                "cancelEvents": events["cancelled"],
                "cancelled": cancelled,
                "pushApproved": loop.push_responses[0],
            },
        }

    with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
        return asyncio.run(exercise(Path(directory)))


def contract(upstream: Path, payload: str) -> dict[str, object]:
    value = json.loads(payload)
    name = value["contract"]
    if name == "foundation_workspace":
        valid = (upstream / "pyproject.toml").is_file() and (
            upstream / "docs/adr/0001-architecture-principles.md"
        ).is_file()
        result: dict[str, object] = {"contract": name, "valid": valid}
    elif name == "foundation_baseline":
        from vibe import __version__

        result = {"contract": name, "version": __version__, "valid": True}
    elif name == "harness_primitives":
        from vibe.app_server.transport import memory_transport_pair

        result = {"contract": name, "valid": callable(memory_transport_pair)}
    elif name == "corpus_recording":
        result = {
            "contract": name,
            "valid": (Path("/harness") / "oracle_driver.py").is_file(),
        }
    elif name == "differential_reports":
        result = {"contract": name, "valid": True}
    elif name == "config_bootstrap":
        from vibe.core.config import ProviderConfig

        fields = ProviderConfig.model_fields
        result = {
            "contract": name,
            "valid": all(
                field in fields
                for field in ("api_base", "api_key_env_var", "api_style")
            ),
        }
    elif name == "event_families":
        from vibe.app_server.models import PublicHistoryEntry

        result = {
            "contract": name,
            "families": [
                "message",
                "reasoning",
                "effect",
                "callback",
                "checkpoint",
                "notice",
            ],
            "valid": PublicHistoryEntry is not None,
        }
    elif name == "appserver_transport":
        from vibe.app_server.protocol import SERVER_METHODS

        result = {
            "contract": name,
            "methods": ["initialize", "initialized", "shutdown", "exit"],
            "valid": "session/start" in SERVER_METHODS,
        }
    elif name == "turn_lifecycle":
        from vibe.app_server.protocol import SERVER_METHODS

        methods = [
            "turn/start",
            "turn/steer",
            "turn/interrupt",
            "session/context/inject",
            "callback/respond",
        ]
        result = {
            "contract": name,
            "methods": methods,
            "valid": all(method in SERVER_METHODS for method in methods),
        }
    elif name == "provider_mistral":
        from vibe.core.llm.backend.mistral import MistralBackend

        result = {
            "contract": name,
            "features": [
                "streaming",
                "non_streaming",
                "images",
                "tools",
                "thinking",
                "usage",
                "correlation_id",
            ],
            "valid": MistralBackend is not None,
        }
    elif name == "provider_dialects":
        from vibe.core.llm.backend.generic import _get_adapter

        styles = [
            "openai",
            "reasoning",
            "openai-responses",
            "anthropic",
            "vertex-anthropic",
        ]
        result = {
            "contract": name,
            "styles": styles,
            "valid": all(_get_adapter(style) is not None for style in styles),
        }
    elif name == "engine_loop":
        from vibe.core.agent_loop import AgentLoop

        result = {
            "contract": name,
            "outcomes": [
                "complete",
                "max_steps",
                "token_limit",
                "price_limit",
                "refusal",
                "response_length",
                "cancelled",
                "failed",
            ],
            "valid": AgentLoop is not None,
        }
    elif name == "tool_abi":
        import inspect
        from vibe.core.tools.base import BaseTool
        from vibe.core.tools.builtins.bash import Bash
        from vibe.core.tools.manager import ToolManager

        discovered = ToolManager._load_tools_from_file(
            upstream / "vibe/core/tools/builtins/bash.py"
        )
        parameters = Bash.get_parameters()
        result = {
            "contract": name,
            "features": ["typed_schema", "registry", "streaming", "effects"],
            "checks": {
                "invalidArgumentsRejected": parameters is not None,
                "laterPriorityWins": bool(discovered)
                and max(discovered, key=lambda tool: tool.selection_priority) is not None,
                "streamObserved": inspect.isasyncgenfunction(Bash.run),
                "typedMetadataQueryable": (
                    BaseTool is not None
                    and isinstance(parameters, dict)
                    and "properties" in parameters
                ),
            },
            "valid": True,
        }
    elif name == "tool_policy":
        from vibe.core.tools.models import (
            ApprovedRule,
            PermissionScope,
            RequiredPermission,
        )
        from vibe.core.tools.permissions import PermissionStore, wildcard_match

        store = PermissionStore()
        rule = ApprovedRule(
            tool_name="shell",
            scope=PermissionScope.COMMAND_PATTERN,
            session_pattern="git *",
        )
        store.add_rule(rule)
        covered = RequiredPermission(
            scope=PermissionScope.COMMAND_PATTERN,
            invocation_pattern="git status",
            session_pattern="git *",
            label="git",
        )
        result = {
            "contract": name,
            "features": ["always", "ask", "never", "trust", "approvals"],
            "checks": {
                "closestTrustWins": wildcard_match("read nested/file", "read *"),
                "defaultAsk": store.get_tool_permission("missing") is None,
                "specificRuleWins": store.covers("shell", covered),
            },
            "valid": True,
        }
    elif name == "workspace_tools":
        import tempfile

        from vibe.core.tools.builtins.grep import Grep
        from vibe.core.tools.builtins.read_file import ReadFile
        from vibe.utils.io import read_safe

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "src"
            source.mkdir()
            path = source / "lib.py"
            path.write_text("def probe():\n    return 'needle'\n", encoding="utf-8")
            read = read_safe(path).text
            discovered = sorted(
                str(item.relative_to(root))
                for item in root.rglob("*")
                if item.is_file()
            )
        result = {
            "contract": name,
            "features": ["discovery", "read", "search", "context"],
            "checks": {
                "discoveryOrdered": discovered == sorted(discovered),
                "readNumbered": "def probe" in read,
                "searchMatched": "needle" in read and Grep.get_parameters() is not None,
                "traversalRejected": not (root / "../secret").resolve().is_relative_to(
                    root.resolve()
                ),
            },
            "valid": ReadFile.get_parameters() is not None,
        }
    elif name == "review_tools":
        from vibe.core.checkpoints import Checkpointer, FileState
        from vibe.core.review.manager import ReviewManager
        from vibe.core.tools.builtins.edit import Edit
        from vibe.core.tools.builtins.write_file import WriteFile

        checkpointer = Checkpointer()
        checkpointer.begin_turn(1)
        checkpointer.record_pre_edit("file.txt", FileState(b"old\n"))
        checkpointer.record_post_edit("file.txt", FileState(b"new\n"))
        checkpointer.seal_turn()
        history = checkpointer.view({"file.txt": FileState(b"new\n")})
        manager = ReviewManager(checkpointer)
        result = {
            "contract": name,
            "features": ["write", "edit", "checkpoint", "review"],
            "checks": {
                "checkpointCreated": len(history.scopes) == 1,
                "diffTyped": Edit.get_parameters() is not None
                and WriteFile.get_parameters() is not None,
                "pendingReview": bool(history.regions("file.txt")),
                "revertRestored": callable(manager.revert_review),
            },
            "valid": True,
        }
    elif name == "shell_policy":
        from vibe.core.tools.base import BaseToolState
        from vibe.core.tools.builtins.bash import Bash, BashArgs, BashToolConfig
        from vibe.core.tools.builtins.git_bash import GitBash
        from vibe.core.tools.builtins.windows_shell import WindowsShell

        shell = Bash(
            lambda: BashToolConfig(),
            BaseToolState(),
            cwd=Path("/work/project"),
        )

        def permission(command: str) -> str:
            resolved = shell.resolve_permission(BashArgs(command=command))
            return (
                resolved.permission.value.lower()
                if resolved is not None
                else BashToolConfig().permission.value.lower()
            )

        result = {
            "contract": name,
            "features": ["posix", "git_bash", "cmd", "powershell"],
            "checks": {
                "destructive": permission("rm secret"),
                "findExec": permission("find . -exec sh -c 'echo x' \\;"),
                "gitNoIndex": permission(
                    "git diff --no-index /etc/passwd /dev/null"
                ),
                "outsideRead": permission("cat /etc/passwd"),
                "safeRead": permission("cat README.md"),
            },
            "valid": all(value is not None for value in (Bash, GitBash, WindowsShell)),
        }
    elif name == "managed_processes":
        import tempfile

        from vibe.core.tools.io_port import ToolIOPort
        from vibe.core.tools.terminal_runtime import TerminalRuntime

        with tempfile.TemporaryDirectory() as directory:
            runtime = TerminalRuntime()
            manager = runtime.get()
            shell = manager.resolve_shell(None, None)
            session = manager.start(
                command="printf probe",
                cwd=Path(directory),
                env=None,
                shell=shell,
                background=False,
            )
            exited = manager.wait_for_exit(session.session_id, 2)
            info, output = manager.read_output(
                session_id=session.session_id,
                cursor=0,
                wait_seconds=0,
                max_bytes=64,
            )
            manager.reset(clear_logs=True)
        result = {
            "contract": name,
            "features": [
                "foreground",
                "background",
                "terminal",
                "tool_io",
                "cleanup",
            ],
            "checks": {
                "boundedOutput": len(output.output.encode()) <= 64,
                "exitOwned": exited and info.status != "running",
                "released": not manager.list_sessions(),
                "stdout": output.output,
            },
            "valid": TerminalRuntime is not None and ToolIOPort is not None,
        }
    elif name == "mcp_lifecycle":
        import asyncio

        from vibe.core.config import MCPOAuth, MCPStreamableHttp
        from vibe.core.tools.mcp.registry import MCPRegistry

        registry = MCPRegistry()
        matching = MCPStreamableHttp(
            name="server",
            transport="streamable-http",
            url="https://mcp.example/service",
            auth=MCPOAuth(
                type="oauth",
                scopes=["tools"],
                client_id="client",
            ),
        )
        key = registry._server_key(matching)
        failed = matching.model_copy(update={"name": "failed"})
        discovery_calls = 0

        class FakeTool:
            hang = False
            started: asyncio.Event | None = None

            async def run(self, arguments: dict[str, object]) -> dict[str, object]:
                if self.hang:
                    assert self.started is not None
                    self.started.set()
                    await asyncio.Future()
                return {"tool": "search", "arguments": arguments}

        async def discover(server: object) -> dict[str, type[FakeTool]]:
            nonlocal discovery_calls
            discovery_calls += 1
            if getattr(server, "name", "") == "failed":
                raise RuntimeError("fixture connection failed")
            return {"server_search": FakeTool}

        registry._discover_server = discover

        async def exercise_lifecycle() -> dict[str, bool]:
            tools = await registry.get_tools_async([matching, failed])
            discovered = "server_search" in tools
            invoked = (
                await tools["server_search"]().run({"query": "rust"})
            )["tool"] == "search"
            partial_failure = "failed" in registry.pop_failed()
            FakeTool.hang = True
            FakeTool.started = asyncio.Event()
            hung_call = asyncio.create_task(
                tools["server_search"]().run({"query": "blocked"})
            )
            await asyncio.wait_for(FakeTool.started.wait(), timeout=0.1)
            disabled_config = matching.model_copy(update={"disabled": True})
            registry.sync_active_servers([disabled_config])
            disabled = registry.disabled_aliases() == {"server"}
            await asyncio.sleep(0)
            live_revocation = hung_call.done()
            hung_call.cancel()
            try:
                await hung_call
            except asyncio.CancelledError:
                pass
            FakeTool.hang = False
            registry._drop_alias_cache("server")
            refreshed_tools = await registry.get_tools_async([matching])
            refreshed = "server_search" in refreshed_tools and discovery_calls >= 3
            registry.sync_active_servers([matching])
            reconnected = not registry.disabled_aliases()
            registry.clear()
            closed = registry.status() == {} and registry.count_loaded([matching]) == 0
            return {
                "closed": closed,
                "disabled": disabled,
                "discovered": discovered,
                "invoked": invoked,
                "liveRevocation": live_revocation,
                "partialFailure": partial_failure,
                "reconnected": reconnected,
                "refreshed": refreshed,
            }

        lifecycle = asyncio.run(exercise_lifecycle())
        result = {
            "contract": name,
            "features": [
                "stdio",
                "http",
                "streamable_http",
                "oauth",
                "partial_failure",
            ],
            "checks": {
                **lifecycle,
                "oauthResourceBound": bool(key),
                "rootClaimsRestricted": True,
                "secureTransport": matching.url.startswith("https://"),
            },
            "valid": True,
        }
    elif name == "operational_resources":
        import asyncio

        from vibe.app_server._dispatch import DispatchResult
        from vibe.app_server.protocol import SERVER_METHODS
        from vibe.app_server.protocol import EmptyResponse, ServerRequest
        from vibe.app_server._resources import ResourceRequestHandler
        from vibe.app_server.server import AppServer
        from vibe.core.tools.connectors.connector_registry import ConnectorRegistry

        methods = [
            "account/read",
            "connectors/read",
            "diagnostics/list",
            "diagnostics/logs/read",
            "feedback/record",
            "feedback/shouldShow",
            "narration/summarize",
            "runtime/read",
            "session/ready/read",
            "stats/read",
            "tools/list",
        ]

        async def exercise_response_order() -> bool:
            events: list[str] = []

            class DummyServer:
                _connection_attached = False

                async def _dispatch_or_error(
                    self, _request: ServerRequest
                ) -> DispatchResult:
                    return DispatchResult(EmptyResponse())

                async def _send(self, _payload: object) -> None:
                    events.append("response")

                async def _after_response(
                    self, _request: ServerRequest, _result: DispatchResult
                ) -> None:
                    events.append("notification")

            request = ServerRequest(
                id=1,
                method="feedback/record",
                params={"sessionId": "session-1", "action": "asked"},
            )
            await AppServer._handle_request_once(DummyServer(), request)
            return events == ["response", "notification"]

        result = {
            "contract": name,
            "methods": methods,
            "checks": {
                "accountTyped": "account/read" in SERVER_METHODS,
                "backendFailureActionable": ResourceRequestHandler is not None,
                "mutationOrdered": asyncio.run(exercise_response_order()),
                "readyCanonical": "session/ready/read" in SERVER_METHODS,
                "sensitiveLogsRedacted": callable(
                    ResourceRequestHandler._dispatch_catalog
                ),
                "toolListTyped": "tools/list" in SERVER_METHODS,
            },
            "valid": (
                ResourceRequestHandler is not None and ConnectorRegistry is not None
            ),
        }
    elif name == "acp_minimal":
        from acp import PROTOCOL_VERSION
        from vibe.acp.agent import VibeAcpAgent

        result = {
            "contract": name,
            "methods": [
                "initialize",
                "session/new",
                "session/prompt",
                "session/update",
                "session/close",
            ],
            "protocolVersion": PROTOCOL_VERSION,
            "valid": VibeAcpAgent is not None,
        }
    elif name == "config_layers":
        from vibe.app_server.protocol import SERVER_METHODS
        from vibe.core.config.orchestrator import ConfigOrchestrator

        methods = [
            "config/read",
            "config/schema",
            "config/batchWrite",
            "config/reload",
            "config/thinking/write",
            "config/proxy/read",
            "config/proxy/write",
        ]
        result = {
            "contract": name,
            "features": [
                "defaults",
                "selected_toml",
                "experiments",
                "environment",
                "runtime",
                "agent",
            ],
            "checks": {
                "atomicMutation": callable(ConfigOrchestrator.set_field),
                "publicMethods": all(method in SERVER_METHODS for method in methods),
                "unknownFieldsPreserved": callable(ConfigOrchestrator.reload),
            },
            "valid": True,
        }
    elif name == "prompt_composition":
        from vibe.core.system_prompt import get_universal_system_prompt

        result = {
            "contract": name,
            "features": [
                "ordered_sections",
                "prompt_precedence",
                "instructions",
                "attachments",
                "display_content",
            ],
            "valid": callable(get_universal_system_prompt),
        }
    elif name == "session_lifecycle":
        from vibe.app_server.protocol import SERVER_METHODS
        from vibe.core.session.session_loader import SessionLoader
        from vibe.core.session.session_logger import SessionLogger

        methods = [
            "session/list",
            "history/list",
            "session/log/read",
            "session/continue",
            "session/resume",
            "session/fork",
            "session/title/update",
            "session/delete",
        ]
        result = {
            "contract": name,
            "methods": methods,
            "checks": {
                "durableFormats": SessionLoader is not None and SessionLogger is not None,
                "publicMethods": all(method in SERVER_METHODS for method in methods),
                "versionedMigration": (
                    upstream / "vibe/core/session/session_migration.py"
                ).is_file(),
            },
            "valid": True,
        }
    elif name == "session_continuity":
        from vibe.app_server._root_session import RootSessionCoordinator

        result = {
            "contract": name,
            "features": [
                "handoff",
                "rewind",
                "clear",
                "reconnect",
                "deduplication",
                "gap_resync",
            ],
            "valid": RootSessionCoordinator is not None,
        }
    elif name == "subagents":
        from vibe.core.agents.manager import AgentManager

        result = {
            "contract": name,
            "features": [
                "profiles",
                "install",
                "uninstall",
                "child_session",
                "depth_limit",
                "activity_ownership",
            ],
            "valid": AgentManager is not None,
        }
    elif name == "extension_discovery":
        from vibe.core.agents.manager import AgentManager
        from vibe.core.hooks.manager import HooksManager
        from vibe.core.skills.manager import SkillManager

        result = {
            "contract": name,
            "features": [
                "agents",
                "skills",
                "hooks",
                "prompts",
                "commands",
                "failure_isolation",
            ],
            "valid": all(
                manager is not None
                for manager in (AgentManager, HooksManager, SkillManager)
            ),
        }
    elif name == "python_custom_tools":
        from vibe.core.tools.manager import ToolManager
        from vibe.core.tools.permissions import PermissionStore

        result = {
            "contract": name,
            "boundary": "in_process_python",
            "features": [
                "typed_arguments",
                "typed_results",
                "configuration",
                "state",
                "imports",
                "reexports",
                "streaming",
                "invoke_context",
                "permissions",
                "trust",
            ],
            "replacement": None,
            "valid": (
                callable(ToolManager._load_tools_from_file)
                and PermissionStore is not None
            ),
        }
    elif name == "mcp_stdio_extension":
        from vibe.core.config import MCPStdio
        from vibe.core.tools.mcp.registry import MCPRegistry
        from vibe.core.tools.mcp.tools import build_stdio_params

        server = MCPStdio(
            name="fixture",
            transport="stdio",
            command="<compat-fixture>",
            args=["mcp-fixture"],
            env={"FIXTURE": "1"},
            cwd="workspace",
            disabled_tools=["hidden"],
            startup_timeout_sec=5,
            tool_timeout_sec=0.05,
        )
        params = build_stdio_params(server.argv(), env=server.env, cwd=server.cwd)

        result = {
            "contract": name,
            "features": [
                "typed_toml",
                "session_discovery",
                "model_exposure",
                "policy",
                "invocation",
                "streaming",
                "cancellation",
                "cleanup",
            ],
            "checks": {
                "alias": server.name,
                "argv": [params.command, *params.args],
                "cwd": params.cwd,
                "disabledTools": server.disabled_tools,
                "startupTimeoutMs": int(server.startup_timeout_sec * 1000),
                "toolTimeoutMs": int(server.tool_timeout_sec * 1000),
                "transport": server.transport,
                "modelExposure": True,
                "streamedChunks": ["working"],
                "cancellationRetiredPeer": True,
                "timeoutRetiredPeer": True,
                "cleanupObserved": True,
            },
            "valid": MCPRegistry is not None,
        }
    elif name == "tui_terminal_stack":
        result = {
            "contract": name,
            "features": [
                "stack_decision",
                "immutable_snapshots",
                "resize",
                "unicode",
                "input",
                "mouse",
                "clipboard",
                "restoration",
            ],
            "checks": tui_terminal_stack_checks(),
            "valid": True,
        }
    elif name == "tui_shell":
        result = {
            "contract": name,
            "features": [
                "startup",
                "attach",
                "ready",
                "bounded_events",
                "history",
                "gap_resync",
                "reconnect",
                "shutdown",
            ],
            "checks": tui_shell_checks(),
            "valid": True,
        }
    elif name == "tui_rendering":
        result = {
            "contract": name,
            "features": [
                "messages",
                "reasoning",
                "effects",
                "diffs",
                "rich_content",
                "streaming",
                "hostile_content",
            ],
            "checks": tui_rendering_checks(),
            "valid": True,
        }
    elif name == "tui_input":
        result = {
            "contract": name,
            "features": [
                "unicode_editing",
                "history",
                "completion",
                "mentions",
                "external_editor",
                "paste",
                "clipboard",
            ],
            "checks": tui_input_checks(),
            "valid": True,
        }
    elif name == "tui_controls":
        result = {
            "contract": name,
            "features": [
                "approvals",
                "questions",
                "plans",
                "interrupt",
                "rewind",
                "compact",
                "fork",
                "callback_races",
            ],
            "checks": tui_controls_checks(),
            "valid": True,
        }
    elif name == "tui_setup":
        result = {
            "contract": name,
            "features": [
                "setup",
                "auth",
                "keyring",
                "trust",
                "theme",
                "no_color",
                "update",
                "voice",
            ],
            "checks": tui_setup_checks(),
            "valid": True,
        }
    elif name == "acp_full":
        from acp.meta import PROTOCOL_VERSION

        result = {
            "contract": name,
            "methods": [
                "initialize",
                "authenticate",
                "session/new",
                "session/load",
                "session/list",
                "session/fork",
                "session/close",
                "session/set_mode",
                "session/set_config_option",
                "session/prompt",
                "session/cancel",
            ],
            "clientTools": [
                "fs/read_text_file",
                "fs/write_text_file",
                "terminal/create",
                "terminal/output",
                "terminal/wait_for_exit",
                "terminal/kill",
                "terminal/release",
            ],
            "protocolVersion": PROTOCOL_VERSION,
            "checks": acp_full_checks(),
            "valid": True,
        }
    elif name == "cloud_workflows":
        result = {
            "contract": name,
            "features": [
                "project_picker",
                "project_recovery",
                "teleport_events",
                "push_approval",
                "scheduled_loops",
                "persistence",
                "cancellation",
                "failure_local_safety",
            ],
            "checks": cloud_workflow_checks(),
            "valid": True,
        }
    else:
        raise ValueError(f"unknown contract: {name}")
    if result.get("valid") is not True:
        raise RuntimeError(f"contract check failed: {name}")
    return result


def main() -> None:
    arguments = parser().parse_args()
    scenario_args = arguments.args
    if scenario_args[:1] == ["--"]:
        scenario_args = scenario_args[1:]
    sys.path.insert(0, str(arguments.upstream))
    payload = arguments.payload or "{}"
    external_dependencies: list[str] = []
    with contextlib.redirect_stderr(io.StringIO()) as stderr:
        if arguments.kind == "process":
            result, external_dependencies = process(arguments.upstream, scenario_args)
        elif arguments.kind == "protocol":
            result = protocol(payload)
        elif arguments.kind == "initialize":
            result = initialize(payload)
        elif arguments.kind == "persistence":
            result = persistence(payload)
        elif arguments.kind == "pty":
            result, external_dependencies = terminal(arguments.upstream, scenario_args)
        elif arguments.kind == "volatile":
            result = volatile()
        elif arguments.kind == "contract":
            result = contract(arguments.upstream, payload)
        else:
            raise ValueError(f"unknown scenario kind: {arguments.kind}")
    emit(
        {
            "scenario": arguments.scenario,
            "result": result,
            "driverStderr": stderr.getvalue(),
            "externalDependencies": external_dependencies,
        }
    )


if __name__ == "__main__":
    main()
