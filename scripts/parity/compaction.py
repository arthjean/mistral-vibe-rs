#!/usr/bin/env python3
"""Capture what the pinned Python reference's compaction actually computes.

Compaction splits in two, and so does this capture. The pure half,
``vibe/core/compaction/context.py`` plus ``vibe/core/utils/tokens.py``, is total
functions over strings and message lists: they are driven directly over scripted
inputs and their answers recorded. The orchestrating half, ``CompactionManager``,
takes its one model call as an injected ``CompletionFn`` protocol, so a stub that
returns scripted chunks and raises ``ContextTooLongError`` on demand observes the
whole decision tree with no network, no key and no nondeterminism.

Seven families come out of the pure half, and are what the Rust replay compares:

``tokenCounts``      the code-point approximation
``truncations``      the middle truncation with its marker and its head/tail split
``summaryExtractions`` the summary parser and its empty-summary signal
``roundDrops``       the oldest-round trimmer and its exhaustion signal
``envelopeRenders``  the persisted envelope, recorded by digest
``envelopeParses``   the envelope read back, including a render round trip
``messageSelections`` which user turns survive a budget

An eighth section, ``managerScenarios``, records the call sequence, the retry
ladder, the fallback decision, the failure reason and the transcript left behind
for each scripted response. Since 2.24.1 the reference appends the envelope to the
live history instead of replacing it (``vibe/core/compaction/manager.py``), and no
longer takes a ``reset_session`` hook.

Two artifacts come out of a run::

    .parity/compaction-corpus.json          the full capture, gitignored
    crates/vibe-core/tests/compaction/corpus.json   the committed projection

The committed one carries scenario-supplied values, counts, tag names and
pointers. Anything that would carry reference-authored prose, which is the two
prose runs the envelope holds and the compaction request the prompt files carry,
is committed as a SHA-256 digest and its length: a digest still fails the replay
on any change, which is what a conformance target has to do, while no reference
sentence ships. The envelope is therefore split rather than digested whole, so
its structure stays a byte-for-byte conformance target while its prose stays a
recorded divergence.

Usage::

    scripts/parity/compaction.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter, reading the pinned commit through ``git archive`` so
the checkout is never moved.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import platform
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from inprocess import digest
from compaction_cases import (
    answer,
    envelope_parse_cases,
    envelope_render_cases,
    manager_cases,
    message_selection_cases,
    round_drop_cases,
    summary_cases,
    token_count_cases,
    truncation_cases,
)

SCHEMA_VERSION = 3
DEFAULT_OUTPUT = Path(".parity/compaction-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/compaction/corpus.json")


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def build_messages(entries: list[dict[str, Any]], envelopes: dict[str, str]) -> list[Any]:
    from vibe.core.types import LLMMessage, Role

    built = []
    for entry in entries:
        if "render" in entry:
            content = envelopes[entry["render"]]
        else:
            content = entry["content"]
        extra: dict[str, Any] = {}
        if entry.get("tool_call_id"):
            extra["tool_call_id"] = entry["tool_call_id"]
        built.append(
            LLMMessage(
                role=Role(entry["role"]),
                content=content,
                injected=entry.get("injected", False),
                **extra,
            )
        )
    return built


def capture_pure() -> dict[str, Any]:
    from vibe.core.compaction.context import (
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        collect_prior_user_messages,
        drop_oldest_round,
        extract_summary,
        parse_previous_user_messages,
        render_compaction_context,
    )
    from vibe.core.utils.tokens import approx_token_count, truncate_middle_to_tokens

    token_counts = [
        {**case, "count": approx_token_count(case["text"])} for case in token_count_cases()
    ]
    truncations = [
        {**case, "result": truncate_middle_to_tokens(case["text"], case["maxTokens"])}
        for case in truncation_cases()
    ]
    summaries = [
        {**case, "summary": extract_summary(case["text"])} for case in summary_cases()
    ]

    round_drops = []
    for case in round_drop_cases():
        dropped = drop_oldest_round(build_messages(case["messages"], {}))
        round_drops.append(
            {
                **case,
                "result": None
                if dropped is None
                else [
                    {"role": item.role.value, "content": item.content or ""}
                    for item in dropped
                ],
            }
        )

    envelopes: dict[str, str] = {}
    renders = []
    for case in envelope_render_cases():
        text = render_compaction_context(
            build_messages(case["messages"], {}), case["summary"]
        )
        envelopes[case["case"]] = text
        renders.append({**case, "rendered": text})

    parses = []
    for case in envelope_parse_cases():
        content = envelopes[case["render"]] if "render" in case else case["content"]
        parses.append({**case, "parsed": parse_previous_user_messages(content)})

    selections = []
    for case in message_selection_cases():
        built = build_messages(case["messages"], envelopes)
        budget = (
            COMPACT_USER_MESSAGE_MAX_TOKENS
            if case["maxTokens"] is None
            else case["maxTokens"]
        )
        selected = collect_prior_user_messages(built, case["summaryPrefix"], budget)
        selections.append(
            {
                **case,
                "selected": [
                    {"content": item.content or "", "injected": bool(item.injected)}
                    for item in selected
                ],
            }
        )

    return {
        "constants": {
            "compactUserMessageMaxTokens": COMPACT_USER_MESSAGE_MAX_TOKENS,
        },
        "tokenCounts": token_counts,
        "truncations": truncations,
        "summaryExtractions": summaries,
        "roundDrops": round_drops,
        "envelopeRenders": renders,
        "envelopeParses": parses,
        "messageSelections": selections,
    }


class _Telemetry:
    """Records what the manager would have shipped, and ships nothing."""

    def __init__(self) -> None:
        self.failures: list[str] = []

    def send_compaction_failed(self, *, reason: str, **_: Any) -> None:
        self.failures.append(reason)


async def capture_manager() -> list[dict[str, Any]]:
    from vibe.core.compaction.manager import CompactionFailedError, CompactionManager
    from vibe.core.config import VibeConfigSchema
    from vibe.core.types import (
        AgentStats,
        ContextTooLongError,
        LLMChunk,
        LLMMessage,
        MessageList,
        FunctionCall,
        Role,
        ToolCall,
    )

    records = []
    for case in manager_cases():
        messages = MessageList(build_messages(case["messages"], {}))
        stats = AgentStats(context_tokens=1234)
        config = VibeConfigSchema(raise_on_compaction_failure=case["strict"])
        telemetry = _Telemetry()
        calls: list[dict[str, Any]] = []
        scripted = list(case["answers"])

        async def complete(
            *,
            model: Any,
            messages: Any,
            tools: Any,
            tool_choice: Any,
            call_type: Any,
            _scripted: list[dict[str, Any]] = scripted,
            _calls: list[dict[str, Any]] = calls,
        ) -> Any:
            step = _scripted.pop(0) if _scripted else answer(text="")
            _calls.append(
                {
                    "model": model.alias,
                    "thinking": str(getattr(model, "thinking", "")),
                    "tools": None if tools is None else len(tools),
                    "toolChoice": tool_choice if isinstance(tool_choice, str) else None,
                    "callType": call_type,
                    "messages": len(messages),
                    "systemMessages": sum(
                        1 for item in messages if item.role == Role.system
                    ),
                    "overflow": step["overflow"],
                }
            )
            if step["overflow"]:
                raise ContextTooLongError("provider", model.alias)
            return LLMChunk(
                message=LLMMessage(
                    role=Role.assistant,
                    content=step["text"],
                    tool_calls=[
                        ToolCall(
                            id=f"call-{index}",
                            function=FunctionCall(name="read_file", arguments="{}"),
                        )
                        for index in range(step["toolCalls"])
                    ]
                    or None,
                )
            )

        async def nothing() -> None:
            return None

        manager = CompactionManager(
            messages=messages,
            stats_getter=lambda: stats,
            config_getter=lambda: config,
            complete=complete,
            available_tools=lambda: [],
            tool_choice=lambda: "auto",
            save=nothing,
            telemetry_client=telemetry,
            session_ids=lambda: ("session-1", None),
        )

        outcome: dict[str, Any] = {"outcome": "returned"}
        try:
            summary = await manager.compact(case.get("extraInstructions", ""))
            outcome["summary"] = summary
        except CompactionFailedError as error:
            outcome = {"outcome": "raised", "reason": error.reason}
        except ContextTooLongError:
            outcome = {"outcome": "overflowed"}

        records.append(
            {
                "case": case["case"],
                "strict": case["strict"],
                # The inputs a replay needs to drive the same scenario: both are
                # values this script supplied, so they commit as they stand.
                "conversation": case["messages"],
                "answers": case["answers"],
                "extraInstructions": case.get("extraInstructions", ""),
                "calls": calls,
                "failures": telemetry.failures,
                "contextTokens": stats.context_tokens,
                "messagesAfter": [
                    {
                        "role": item.role.value,
                        "injected": bool(item.injected),
                        "content": item.content or "",
                    }
                    for item in messages
                ],
                **outcome,
            }
        )
    return records


# --------------------------------------------------------------------------
# The committed projection
# --------------------------------------------------------------------------


_MESSAGES_OPEN = "<previous_user_messages>"
_MESSAGES_CLOSE = "</previous_user_messages>"
_SUMMARY_BLOCK_OPEN = "<compaction_summary>"

#: What stands in the committed corpus where the reference wrote prose. The Rust
#: replay masks its own prose with the same two markers, so what is compared is
#: every byte the envelope's readers depend on and nothing either side authored.
_PREAMBLE_MARK = "{preamble}"
_SUMMARY_LEAD_MARK = "{summaryLead}"
#: What stands where the placeholder summary was, for the same reason.
PLACEHOLDER_MARK = "{placeholderSummary}"


def _anchors(text: str) -> tuple[int, int, int] | None:
    """Where the envelope's three structural anchors sit, or None if it is not one.

    The reserved tags are escaped inside every preserved message, so a text that
    carries all three in order is an envelope and the split is unambiguous.
    """

    open_at = text.find(_MESSAGES_OPEN)
    if open_at < 0:
        return None
    close_at = text.find(_MESSAGES_CLOSE, open_at)
    if close_at < 0:
        return None
    close_at += len(_MESSAGES_CLOSE)
    summary_at = text.find(_SUMMARY_BLOCK_OPEN, close_at)
    if summary_at < 0:
        return None
    return open_at, close_at, summary_at


def mask_envelope(text: str) -> str:
    """``text`` with the two prose runs an envelope carries replaced by markers.

    Anything that is not an envelope comes back untouched, so this is safe to
    apply to every recorded message content.
    """

    anchors = _anchors(text)
    if anchors is None:
        return text
    open_at, close_at, summary_at = anchors
    return (
        _PREAMBLE_MARK
        + text[open_at:close_at]
        + _SUMMARY_LEAD_MARK
        + text[summary_at:]
    )


def envelope_prose(text: str) -> dict[str, Any]:
    """The two prose runs ``text`` carries, each by length and SHA-256."""

    anchors = _anchors(text)
    if anchors is None:
        raise OracleError("the rendered envelope carries no structural anchors")
    open_at, close_at, summary_at = anchors
    return {
        "preamble": digest(text[:open_at]),
        "summaryLead": digest(text[close_at:summary_at]),
    }


def project(capture: dict[str, Any], manager: list[dict[str, Any]]) -> dict[str, Any]:
    prose: dict[str, Any] | None = None
    renders = []
    for case in capture["envelopeRenders"]:
        candidate = envelope_prose(case["rendered"])
        if prose is None:
            prose = candidate
        elif prose != candidate:
            raise OracleError(
                f"the envelope prose is not constant across renders: {case['case']}"
            )
        renders.append({
            **{key: value for key, value in case.items() if key != "rendered"},
            "structure": mask_envelope(case["rendered"]),
        })
    if prose is None:
        raise OracleError("no envelope render was captured")
    # A selection can return an envelope verbatim, when the transcript carries
    # one that is not injected and is therefore a real user turn.
    selections = [
        {
            **case,
            "selected": [
                {**item, "content": mask_envelope(item["content"])}
                for item in case["selected"]
            ],
        }
        for case in capture["messageSelections"]
    ]
    # The manager's own transcript carries the envelope, so it is masked the way
    # a render is: the structure commits verbatim and the reference's prose does
    # not. The placeholder summary is the third run of reference-authored prose
    # in this surface, recorded once by digest and masked wherever it appears.
    managers = []
    placeholder: dict[str, Any] | None = None
    for record in manager:
        projected = dict(record)
        placeholder_text = ""
        if record["failures"] and record.get("outcome") == "returned":
            placeholder_text = record["summary"]
            candidate = digest(placeholder_text)
            if placeholder is None:
                placeholder = candidate
            elif placeholder != candidate:
                raise OracleError(
                    f"the placeholder summary is not constant: {record['case']}"
                )
            projected["summary"] = PLACEHOLDER_MARK

        def mask(content: str, _placeholder: str = placeholder_text) -> str:
            masked = mask_envelope(content)
            return masked.replace(_placeholder, PLACEHOLDER_MARK) if _placeholder else masked

        projected["messagesAfter"] = [
            {**item, "content": mask(item["content"])} for item in record["messagesAfter"]
        ]
        managers.append(projected)
    if placeholder is None:
        raise OracleError("no scenario exercised the placeholder summary")
    return {
        **capture,
        "envelopeProse": prose,
        "placeholderSummary": placeholder,
        "envelopeRenders": renders,
        "messageSelections": selections,
        "managerScenarios": managers,
    }


def build_corpus(
    capture: dict[str, Any], manager: list[dict[str, Any]], reference: dict[str, str]
) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Compaction corpus: what the pinned reference's pure compaction functions answer for "
            "each scripted input, plus the decision tree CompactionManager walks for each scripted "
            "model response. Values, tag names and counts are committed as they stand because the "
            "scenarios supplied them; the rendered envelope is split, its structure committed "
            "verbatim and the two prose runs it carries committed as a SHA-256 digest and a "
            "length, because those are the reference's own sentences, as is the placeholder "
            "summary a failed summarization falls back to. Regenerate with "
            "scripts/parity/compaction.py --corpus when the pinned reference moves."
        ),
        **project(capture, manager),
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="?",
        const=DEFAULT_CORPUS,
        default=None,
        help=(
            "also write the committed corpus, which the Rust replay reads unconditionally "
            f"(default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        commit, _ = reexecute_pinned(
            arguments.reference,
            arguments.expected_commit,
            cache=arguments.cache,
            override=arguments.python,
        )
        reference = {"commit": commit, "path": str(arguments.reference)}
        from vibe.core.config.harness_files import init_harness_files_manager

        init_harness_files_manager()
        capture = capture_pure()
        manager = asyncio.run(capture_manager())
    except OracleError as error:
        print(f"compaction capture failed: {error}", file=sys.stderr)
        return 1

    full = {
        "schemaVersion": SCHEMA_VERSION,
        "reference": reference,
        "platform": platform.system().lower(),
        "python": platform.python_version(),
        **capture,
        "managerScenarios": manager,
    }
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(full, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="",
    )
    os.replace(staged, arguments.output)
    families = [
        "tokenCounts",
        "truncations",
        "summaryExtractions",
        "roundDrops",
        "envelopeRenders",
        "envelopeParses",
        "messageSelections",
    ]
    total = sum(len(capture[family]) for family in families)
    if arguments.corpus is not None:
        arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
        arguments.corpus.write_text(
            json.dumps(
                build_corpus(capture, manager, reference),
                indent=2,
                sort_keys=True,
                ensure_ascii=False,
            )
            + "\n",
            encoding="utf-8",
            newline="",
        )
    print(
        f"captured {total} scenarios across {len(families)} families and "
        f"{len(manager)} manager scenarios from {reference['commit'][:12]} into {arguments.output}"
    )
    for family in families:
        print(f"  {family}: {len(capture[family])}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
