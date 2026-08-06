"""Pinned Python oracle for EP-010: semantic transcript and observability.

Every observation is composed from the reference itself: the tool presentation
adapters build the call and result displays, the pinned transcript widgets turn
them into header parts and bodies, and the error, activity, context, debug, and
link helpers are called directly. Nothing here reimplements Rust behavior.
"""

from __future__ import annotations

import asyncio
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import sys
import time

# The reference renders debug timestamps in the local zone; pin it so the
# checked-in corpus is machine independent.
os.environ["TZ"] = "UTC"
time.tzset()

from pydantic import BaseModel, TypeAdapter
from textual.app import App, ComposeResult
from textual.widget import Widget
from textual.widgets import Markdown

from vibe.app_server.models import (
    DebugLogEntry,
    HookSeverity,
    EffectDetail,
    EffectState,
    PublicEffectEntry,
    PublicError,
)
from vibe.core.tools.builtins.ask_user_question import (
    AskUserQuestion,
    AskUserQuestionArgs,
)
from vibe.core.tools.builtins.bash import Bash, BashArgs, BashResult
from vibe.core.tools.builtins.edit import Edit, EditArgs, EditResult
from vibe.core.tools.builtins.grep import Grep, GrepArgs, GrepResult
from vibe.core.tools.builtins.read_file import ReadFile, ReadFileArgs, ReadFileResult
from vibe.core.tools.builtins.todo import Todo, TodoArgs, TodoResult
from vibe.core.tools.builtins.web_fetch import WebFetch, WebFetchArgs, WebFetchResult
from vibe.core.tools.builtins.web_search import (
    WebSearch,
    WebSearchArgs,
    WebSearchResult,
)
from vibe.core.tools.builtins.write_file import (
    WriteFile,
    WriteFileArgs,
    WriteFileResult,
)
from vibe.app_server._tool_projection import project_effect_output_value
from vibe.core.tools.ui import ToolUIDataAdapter
from vibe.core.types import ToolCallEvent, ToolResultEvent
from vibe.questions import UserAnswer, UserQuestion
from vibe.cli.textual_ui.app import VibeApp
from vibe.cli.textual_ui.handlers.event_handler import _NON_GROUPED_EFFECT_KINDS
from vibe.cli.textual_ui.widgets.context_progress import ContextProgress, TokenState
from vibe.cli.textual_ui.widgets.debug_console import LOG_LEVEL_COLORS, DebugConsole
from vibe.cli.textual_ui.widgets.links import _is_safe_url, linkify_urls_in_text
from vibe.cli.textual_ui.widgets.loading import LoadingWidget, _format_elapsed
from vibe.cli.textual_ui.widgets.messages import _HOOK_SEVERITY_ICONS
from vibe.cli.textual_ui.widgets.status_message import IndicatorState
from vibe.cli.textual_ui.widgets.tool_widgets import (
    effect_result_is_collapsible,
    get_result_widget,
)
from vibe.cli.textual_ui.widgets.tools import ToolCallMessage, ToolResultMessage

DETAIL_ADAPTER = TypeAdapter(EffectDetail)
STATE_ADAPTER = TypeAdapter(EffectState)

TOOLS: dict[str, tuple[type, type[BaseModel], type[BaseModel] | None]] = {
    "bash": (Bash, BashArgs, BashResult),
    "read_file": (ReadFile, ReadFileArgs, ReadFileResult),
    "write_file": (WriteFile, WriteFileArgs, WriteFileResult),
    "edit": (Edit, EditArgs, EditResult),
    "grep": (Grep, GrepArgs, GrepResult),
    "todo": (Todo, TodoArgs, TodoResult),
    "ask_user_question": (AskUserQuestion, AskUserQuestionArgs, None),
    "web_search": (WebSearch, WebSearchArgs, WebSearchResult),
    "web_fetch": (WebFetch, WebFetchArgs, WebFetchResult),
}

_MARKUP = re.compile(r"\[/?[^\[\]]*\]")


class Harness(App):
    """Mounts a pinned widget headlessly so its own composition is the oracle."""

    def __init__(self, widget: Widget) -> None:
        super().__init__()
        self._widget = widget

    def compose(self) -> ComposeResult:
        yield self._widget


def widget_text(widget: Widget) -> list[str]:
    async def run() -> list[str]:
        app = Harness(widget)
        async with app.run_test() as pilot:
            await pilot.pause()
            lines: list[str] = []
            # A Markdown node owns its rendered block, so its own children are
            # the same content a second time.
            inside_markdown: set[int] = set()
            for node in widget.walk_children(with_self=True):
                if id(node) in inside_markdown:
                    continue
                if isinstance(node, Markdown):
                    lines.extend(unfence(node._markdown or ""))
                    inside_markdown.update(
                        id(child) for child in node.walk_children(with_self=False)
                    )
                    continue
                content = getattr(node, "content", None)
                if content is None:
                    continue
                text = content.plain if hasattr(content, "plain") else str(content)
                if text:
                    lines.extend(text.split("\n"))
            return lines

    return asyncio.run(run())


def unfence(markdown: str) -> list[str]:
    """Strips the code fence the reference wraps file and command output in."""
    lines = markdown.split("\n")
    if lines and lines[0].startswith("`"):
        lines = lines[1:]
    if lines and lines[-1].startswith("`"):
        lines = lines[:-1]
    return [line for line in lines if line]


def strip_markup(text: str) -> str:
    return _MARKUP.sub("", text)


def build_entry(tool: str, arguments: dict, outcome: dict) -> tuple[PublicEffectEntry, object]:
    tool_class, args_model, result_model = TOOLS.get(tool, (None, None, None))
    adapter = ToolUIDataAdapter(tool_class)
    args = None
    if args_model is not None:
        args = args_model.model_validate(decode_arguments(tool, arguments))
    call = adapter.get_call_presentation(
        ToolCallEvent(
            tool_call_id="call-1", tool_name=tool, tool_class=tool_class, args=args
        )
    )
    detail = DETAIL_ADAPTER.validate_python(
        {
            "kind": call.kind.value,
            "toolName": tool,
            "input": arguments or None,
            "display": call.display.model_dump(by_alias=True),
        }
    )
    state = build_state(tool, call.kind, adapter, result_model, outcome)
    entry = PublicEffectEntry.model_validate(
        {
            "type": "effect",
            "id": "entry-1",
            "sessionId": "session",
            "turnId": "turn",
            "createdAt": 0,
            "updatedAt": 0,
            "generationStatus": "completed"
            if outcome["status"] not in {"pending", "running", "blocked"}
            else "in_progress",
            "title": tool,
            "detail": detail.model_dump(by_alias=True),
            "state": state,
        }
    )
    return entry, detail


def decode_arguments(tool: str, arguments: dict) -> dict:
    if tool == "ask_user_question":
        return {
            "questions": [UserQuestion(**question) for question in arguments["questions"]]
        }
    return arguments


def build_state(
    tool: str,
    kind: object,
    adapter: ToolUIDataAdapter,
    result_model: type[BaseModel] | None,
    outcome: dict,
) -> dict:
    status = outcome["status"]
    if status == "pending":
        return {"status": "pending"}
    if status == "running":
        return {"status": "running", "outputText": outcome.get("outputText", "")}
    if status == "blocked":
        return {
            "status": "blocked",
            "callbackId": outcome.get("callbackId", "callback-1"),
            "outputText": outcome.get("outputText", ""),
        }
    if status == "skipped":
        display = adapter.get_result_display(
            ToolResultEvent(
                tool_call_id="call-1",
                tool_name=tool,
                tool_class=None,
                skipped=True,
                skip_reason=outcome["reason"],
            )
        )
        return {
            "status": "skipped",
            "reason": outcome["reason"],
            "display": display.model_dump(by_alias=True),
        }
    if status == "cancelled":
        display = adapter.get_result_display(
            ToolResultEvent(
                tool_call_id="call-1",
                tool_name=tool,
                tool_class=None,
                skipped=True,
                skip_reason=outcome["reason"],
            )
        )
        return {
            "status": "cancelled",
            "reason": outcome["reason"],
            "outputText": "",
            "durationMs": 0,
            "display": display.model_dump(by_alias=True),
        }
    if status == "failed":
        display = adapter.get_result_display(
            ToolResultEvent(
                tool_call_id="call-1",
                tool_name=tool,
                tool_class=None,
                error=outcome["error"],
            )
        )
        return {
            "status": "failed",
            "error": PublicError(message=outcome["error"]).model_dump(by_alias=True),
            "outputText": outcome["error"],
            "durationMs": 0,
            "display": display.model_dump(by_alias=True),
        }
    result = decode_result(tool, result_model, outcome.get("result"))
    display = adapter.get_result_display(
        ToolResultEvent(
            tool_call_id="call-1", tool_name=tool, tool_class=None, result=result
        )
    )
    presentation = adapter.get_result_presentation(
        ToolResultEvent(
            tool_call_id="call-1", tool_name=tool, tool_class=None, result=result
        )
    )
    projected = project_effect_output_value(
        kind,
        presentation.projected_output
        if presentation.projected_output is not None
        else result,
    )
    return {
        "status": "completed",
        "output": projected,
        "outputText": outcome.get("outputText", ""),
        "durationMs": 0,
        "display": display.model_dump(by_alias=True),
    }


def decode_result(
    tool: str, result_model: type[BaseModel] | None, payload: dict | None
) -> BaseModel | None:
    if payload is None:
        return None
    if tool == "ask_user_question":
        from vibe.core.tools.builtins.ask_user_question import AskUserQuestionResult

        return AskUserQuestionResult(
            answers=[UserAnswer(**answer) for answer in payload.get("answers", [])],
            cancelled=payload.get("cancelled", False),
        )
    if result_model is None:
        return None
    return result_model.model_validate(payload)


def indicator_of(entry: PublicEffectEntry) -> str:
    call = ToolCallMessage(entry)
    status = entry.state.status
    if status in {"pending", "running", "blocked"}:
        return "running"
    if status in {"skipped", "cancelled"}:
        return IndicatorState.MUTED.value
    if status == "failed":
        return IndicatorState.ERROR.value
    return call._state.value


def observe_effect(event: dict) -> str:
    entry, detail = build_entry(event["tool"], event.get("arguments", {}), event["outcome"])
    call = ToolCallMessage(entry)
    status = entry.state.status
    if status in {"pending", "running", "blocked"}:
        verb, message, suffix = call._header_parts()
        body: list[str] = []
    else:
        result = ToolResultMessage(entry, call)
        verb, message, suffix = result._get_result_parts()
        body = result_body(entry, detail)
    return "|".join(
        [
            "effect",
            detail.kind.value,
            indicator_of(entry),
            verb,
            message,
            suffix,
            f"collapsible={int(effect_result_is_collapsible(detail))}",
            f"grouped={int(detail.kind not in _NON_GROUPED_EFFECT_KINDS)}",
            "body=" + "⏎".join(body),
        ]
    )


def result_body(entry: PublicEffectEntry, detail: EffectDetail) -> list[str]:
    state = entry.state
    status = state.status
    if status == "failed":
        return [f"Error: {state.error.message}"]
    if status in {"skipped", "cancelled"}:
        return [f"Skipped: {state.reason}"]
    display = state.display
    lines = [f"⚠ {warning}" for warning in display.warnings]
    widget = get_result_widget(
        detail,
        state.output,
        success=display.success,
        message=display.message,
        warnings=display.warnings,
    )
    lines.extend(widget_text(widget))
    return lines


def observe_hook(event: dict) -> str:
    severity = HookSeverity(event.get("severity", "warning"))
    icon = _HOOK_SEVERITY_ICONS.get(severity, _HOOK_SEVERITY_ICONS[HookSeverity.WARNING])
    return f"hook|{icon}|[{event['hookName']}] {event['content']}"


class _AccountStub:
    def __init__(self, upgrade: bool) -> None:
        self.rate_limit_action = "upgrade" if upgrade else None


class _ResourcesStub:
    def __init__(self, upgrade: bool) -> None:
        self.account = type("Account", (), {"current": _AccountStub(upgrade)})()


class _AppStub:
    def __init__(self, upgrade: bool) -> None:
        self.app_server = type("Server", (), {"resources": _ResourcesStub(upgrade)})()


def observe_turn_error(event: dict) -> str:
    stub = _AppStub(event.get("upgradeAvailable", False))
    code = event.get("code")
    if code == "rate_limit":
        message = VibeApp._rate_limit_message(stub)
    elif code == "context_too_long":
        message = VibeApp._context_too_long_message(stub)
    elif code == "refusal":
        message = VibeApp._refusal_message(
            stub, PublicError(message=event["message"], code=code, details=event.get("details", {}))
        )
    else:
        message = event["message"]
    return "error|" + message.replace("\n", "⏎")


def observe_context(event: dict) -> str:
    widget = ContextProgress()

    async def run() -> str:
        app = Harness(widget)
        async with app.run_test() as pilot:
            widget.tokens = TokenState(
                max_tokens=event["maxTokens"], current_tokens=event["currentTokens"]
            )
            await pilot.pause()
            content = widget.content
            return content.plain if hasattr(content, "plain") else str(content)

    return "context|" + asyncio.run(run())


def observe_activity(event: dict) -> str:
    widget = LoadingWidget(status=event.get("status"))
    widget.set_queue_count(event.get("queued", 0))
    hint = strip_markup(widget._format_hint(event["elapsedSeconds"]))
    return f"activity|{widget._base_status}|{_format_elapsed(event['elapsedSeconds'])}|{hint}"


def observe_debug_log(event: dict) -> str:
    entry = DebugLogEntry(
        id=event["id"],
        timestamp=datetime.fromtimestamp(event["timestamp"], tz=timezone.utc),
        level=event["level"],
        message=event["message"],
        ppid=0,
        pid=0,
        raw_line=event["message"],
    )
    formatted = DebugConsole._format_entry(entry)
    color = LOG_LEVEL_COLORS.get(event["level"], "dim")
    return f"log|{color}|{strip_markup(formatted)}"


def observe_link(event: dict) -> str:
    url = event["url"]
    content = linkify_urls_in_text(event["text"])
    spans = sum(1 for span in content.spans if span.style is not None)
    return f"link|safe={int(_is_safe_url(url))}|spans={spans}"


HANDLERS = {
    "effect": observe_effect,
    "hook": observe_hook,
    "turn_error": observe_turn_error,
    "context": observe_context,
    "activity": observe_activity,
    "debug_log": observe_debug_log,
    "link": observe_link,
}


def main() -> None:
    corpus = json.loads(
        (Path(__file__).parent / "semantic-transcript-ep010.json").read_text()
    )
    trace_expected: dict[str, list[str]] = {}
    for trace in corpus["traces"]:
        trace_expected[trace["id"]] = [
            HANDLERS[event["kind"]](event) for event in trace["events"]
        ]
    json.dump(
        {
            "indicatorGlyphs": {
                state.value: state.glyph for state in IndicatorState
            },
            "hookIcons": {
                severity.value: icon for severity, icon in _HOOK_SEVERITY_ICONS.items()
            },
            "nonGroupedKinds": sorted(kind.value for kind in _NON_GROUPED_EFFECT_KINDS),
            "traceExpected": trace_expected,
        },
        sys.stdout,
        ensure_ascii=False,
    )


if __name__ == "__main__":
    main()
