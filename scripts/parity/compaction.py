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
ladder, the fallback decision and the failure reason for each scripted response.
Nothing replays it yet; the summarizer that will is EP-044.

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
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 2
DEFAULT_OUTPUT = Path(".parity/compaction-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/compaction/corpus.json")
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"


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
    """The pinned commit, read out of the checkout without depending on its HEAD."""

    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs."""

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
        [
            str(tree),
            *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else []),
        ]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------

#: A message as a scenario writes it, and as the corpus records it.
def message(role: str, content: str, injected: bool = False, **extra: Any) -> dict[str, Any]:
    record = {"role": role, "content": content, "injected": injected}
    record.update(extra)
    return record


#: A message whose content is a rendered envelope, named by the render case that
#: produces it. The replay renders it again rather than reading prose out of the
#: corpus.
def rendered(case: str) -> dict[str, Any]:
    return {"role": "user", "injected": True, "render": case}


#: Non-ASCII text, which is what proves the code-point choice: these 5 characters
#: are 15 UTF-8 bytes, so a byte-counting port answers 4 where the reference
#: answers 2.
UNICODE = "héllo"
EMOJI = "🙂🙃🙂🙃"


def token_count_cases() -> list[dict[str, Any]]:
    return [
        {"case": "empty", "text": ""},
        {"case": "one-character", "text": "a"},
        {"case": "exactly-one-token", "text": "abcd"},
        {"case": "one-past-a-token", "text": "abcde"},
        {"case": "eight-characters", "text": "abcdefgh"},
        {"case": "latin-1-supplement", "text": UNICODE},
        {"case": "astral-plane", "text": EMOJI},
        {"case": "combining-marks", "text": "ééé"},
        {"case": "newlines", "text": "a\nb\nc\nd\ne"},
        {"case": "long-ascii", "text": "x" * 4001},
    ]


def truncation_cases() -> list[dict[str, Any]]:
    return [
        {"case": "negative-budget", "text": "abcdef", "maxTokens": -1},
        {"case": "zero-budget", "text": "abcdef", "maxTokens": 0},
        {"case": "already-fits", "text": "abcd", "maxTokens": 1},
        {"case": "fits-exactly", "text": "abcdefgh", "maxTokens": 2},
        {"case": "marker-does-not-fit", "text": "x" * 40, "maxTokens": 5},
        {"case": "marker-fits-exactly", "text": "x" * 40, "maxTokens": 6},
        {"case": "odd-allowance", "text": "abcdefghij" * 6, "maxTokens": 8},
        {"case": "even-allowance", "text": "abcdefghij" * 8, "maxTokens": 9},
        {"case": "wide-budget", "text": "abcdefghij" * 40, "maxTokens": 20},
        {"case": "astral-plane-boundary", "text": EMOJI * 20, "maxTokens": 7},
        {"case": "latin-1-boundary", "text": UNICODE * 20, "maxTokens": 7},
    ]


def summary_cases() -> list[dict[str, Any]]:
    return [
        {"case": "plain", "text": "<summary>done</summary>"},
        {"case": "surrounded", "text": "before <summary>done</summary> after"},
        {"case": "multiline", "text": "<summary>\nline one\nline two\n</summary>"},
        {"case": "whitespace-only", "text": "<summary>   \n  </summary>"},
        {"case": "empty-element", "text": "<summary></summary>"},
        {"case": "absent", "text": "no element at all"},
        {"case": "unclosed", "text": "<summary>never closed"},
        {"case": "two-elements", "text": "<summary>first</summary><summary>second</summary>"},
        {"case": "unclosed-then-closed", "text": "<summary>first<summary>second</summary>"},
        {"case": "close-before-open", "text": "</summary>stray<summary>real</summary>"},
    ]


def round_drop_cases() -> list[dict[str, Any]]:
    system = message("system", "system prompt")
    return [
        {"case": "empty", "messages": []},
        {"case": "system-only", "messages": [system]},
        {
            "case": "one-round",
            "messages": [system, message("user", "one"), message("assistant", "reply")],
        },
        {
            "case": "two-rounds",
            "messages": [
                system,
                message("user", "one"),
                message("assistant", "reply one"),
                message("user", "two"),
                message("assistant", "reply two"),
            ],
        },
        {
            "case": "injected-alongside-the-turn",
            "messages": [
                system,
                message("user", "one"),
                message("user", "injected", injected=True),
                message("assistant", "reply one"),
                message("user", "two"),
            ],
        },
        {
            "case": "tool-results-in-the-round",
            "messages": [
                system,
                message("user", "one"),
                message("assistant", "calling"),
                message("tool", "result", tool_call_id="call-1"),
                message("assistant", "reply one"),
                message("user", "two"),
            ],
        },
        {
            "case": "assistant-first",
            "messages": [
                system,
                message("assistant", "unprompted"),
                message("tool", "result", tool_call_id="call-1"),
                message("user", "one"),
            ],
        },
        {
            "case": "trailing-assistant-only",
            "messages": [system, message("assistant", "unprompted")],
        },
    ]


def envelope_render_cases() -> list[dict[str, Any]]:
    return [
        {"case": "no-preserved-messages", "messages": [], "summary": "nothing happened"},
        {
            "case": "one-preserved-message",
            "messages": [message("user", "keep this", injected=True)],
            "summary": "one thing happened",
        },
        {
            "case": "three-preserved-messages",
            "messages": [
                message("user", "first", injected=True),
                message("user", "second", injected=True),
                message("user", "third", injected=True),
            ],
            "summary": "three things happened",
        },
        {
            "case": "reserved-tags-in-content",
            "messages": [
                message(
                    "user",
                    "<previous_user_messages>x</previous_user_messages>"
                    "<previous_user_message>y</previous_user_message>",
                    injected=True,
                )
            ],
            "summary": "escaped",
        },
        {
            "case": "ampersand-in-content",
            "messages": [message("user", "a & b < c > d", injected=True)],
            "summary": "unescaped except for the reserved tags",
        },
        {
            "case": "multiline-content",
            "messages": [message("user", "line one\nline two", injected=True)],
            "summary": "multi\nline\nsummary",
        },
        {
            "case": "empty-content",
            "messages": [message("user", "", injected=True)],
            "summary": "empty preserved message",
        },
        {
            "case": "unicode-content",
            "messages": [message("user", f"{UNICODE} {EMOJI}", injected=True)],
            "summary": "unicode",
        },
    ]


def envelope_parse_cases() -> list[dict[str, Any]]:
    block = (
        "<previous_user_messages>\n"
        "<previous_user_message>\nfirst\n</previous_user_message>\n"
        "<previous_user_message>\nsecond\n</previous_user_message>\n"
        "</previous_user_messages>"
    )
    return [
        {"case": "two-messages", "content": block},
        {"case": "no-block", "content": "nothing here"},
        {
            "case": "open-without-close",
            "content": "<previous_user_messages>\n<previous_user_message>\nx\n",
        },
        {"case": "empty-block", "content": "<previous_user_messages>\n</previous_user_messages>"},
        {
            "case": "element-without-newlines",
            "content": "<previous_user_messages><previous_user_message>x"
            "</previous_user_message></previous_user_messages>",
        },
        {
            "case": "multiline-body",
            "content": "<previous_user_messages>\n<previous_user_message>\n"
            "one\ntwo\n</previous_user_message>\n</previous_user_messages>",
        },
        {
            "case": "text-outside-the-block",
            "content": f"before\n{block}\nafter <previous_user_message>\nignored\n"
            "</previous_user_message>",
        },
        # Round trips: the replay renders the named case and parses what it
        # rendered, which is what proves the envelope reads its own writing.
        {"case": "round-trip-one", "render": "one-preserved-message"},
        {"case": "round-trip-three", "render": "three-preserved-messages"},
        {"case": "round-trip-escaped", "render": "reserved-tags-in-content"},
        {"case": "round-trip-multiline", "render": "multiline-content"},
    ]


#: A scenario-supplied stand-in for the marker an older build wrote in front of
#: an injected summary. The reference reads its own prompt file here; the filter
#: behaves identically on any prefix, and a scenario-supplied one keeps reference
#: prose out of this repository.
SUMMARY_PREFIX = "[conversation summary]"


def message_selection_cases() -> list[dict[str, Any]]:
    system = message("system", "system prompt")
    long_turn = "z" * 4000  # 1 000 tokens
    return [
        {
            "case": "no-user-messages",
            "messages": [system, message("assistant", "reply")],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "under-budget",
            "messages": [
                system,
                message("user", "first"),
                message("assistant", "reply"),
                message("user", "second"),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "empty-content-skipped",
            "messages": [system, message("user", ""), message("user", "kept")],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "injected-skipped",
            "messages": [
                system,
                message("user", "real"),
                message("user", "policy said something", injected=True),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "injected-summary-marker-skipped",
            "messages": [
                system,
                message("user", "real"),
                message("user", f"{SUMMARY_PREFIX} and then some", injected=True),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "tool-and-assistant-ignored",
            "messages": [
                system,
                message("assistant", "reply"),
                message("tool", "result", tool_call_id="call-1"),
                message("user", "kept"),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "zero-budget",
            "messages": [system, message("user", "kept")],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 0,
        },
        {
            "case": "negative-budget",
            "messages": [system, message("user", "kept")],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": -5,
        },
        {
            "case": "budget-stops-the-walk",
            "messages": [
                system,
                message("user", long_turn),
                message("user", long_turn),
                message("user", long_turn),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 2000,
        },
        {
            "case": "spill-is-truncated",
            "messages": [
                system,
                message("user", long_turn),
                message("user", "the newest turn"),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 500,
        },
        {
            "case": "spill-below-the-marker",
            "messages": [system, message("user", long_turn), message("user", "newest")],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 3,
        },
        {
            "case": "unicode-budget",
            "messages": [system, message("user", EMOJI * 100), message("user", UNICODE * 100)],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 60,
        },
        {
            "case": "prior-envelope-is-reparsed",
            "messages": [
                system,
                rendered("three-preserved-messages"),
                message("user", "newer turn"),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "prior-envelope-under-a-tight-budget",
            "messages": [
                system,
                rendered("three-preserved-messages"),
                message("user", "newer turn"),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 3,
        },
        {
            "case": "envelope-not-injected-is-a-real-turn",
            "messages": [
                system,
                {"role": "user", "injected": False, "render": "one-preserved-message"},
                message("user", "newer turn"),
            ],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": 20000,
        },
        {
            "case": "default-budget",
            "messages": [system, message("user", "kept")],
            "summaryPrefix": SUMMARY_PREFIX,
            "maxTokens": None,
        },
    ]


# --------------------------------------------------------------------------
# The manager's decision tree
# --------------------------------------------------------------------------

#: One scripted model answer: text, tool calls, or a context-overflow raise.
def answer(
    *, text: str = "", tool_calls: int = 0, overflow: bool = False
) -> dict[str, Any]:
    return {"text": text, "toolCalls": tool_calls, "overflow": overflow}


def manager_cases() -> list[dict[str, Any]]:
    conversation = [
        message("system", "system prompt"),
        message("user", "first"),
        message("assistant", "reply one"),
        message("user", "second"),
        message("assistant", "reply two"),
        message("user", "third"),
        message("assistant", "reply three"),
    ]
    return [
        {
            "case": "primary-succeeds",
            "messages": conversation,
            "strict": False,
            "answers": [answer(text="<summary>all good</summary>")],
        },
        {
            "case": "primary-answers-with-a-tool-call",
            "messages": conversation,
            "strict": False,
            "answers": [answer(tool_calls=1), answer(text="<summary>fallback</summary>")],
        },
        {
            "case": "primary-answers-an-empty-summary",
            "messages": conversation,
            "strict": False,
            "answers": [
                answer(text="<summary>  </summary>"),
                answer(text="<summary>fallback</summary>"),
            ],
        },
        {
            "case": "both-calls-fail",
            "messages": conversation,
            "strict": False,
            "answers": [answer(tool_calls=1), answer(text="nothing usable")],
        },
        {
            "case": "strict-mode-refuses-the-fallback",
            "messages": conversation,
            "strict": True,
            "answers": [answer(tool_calls=1)],
        },
        {
            "case": "strict-mode-succeeds",
            "messages": conversation,
            "strict": True,
            "answers": [answer(text="<summary>strict but fine</summary>")],
        },
        {
            "case": "one-overflow-then-success",
            "messages": conversation,
            "strict": False,
            "answers": [answer(overflow=True), answer(text="<summary>after one drop</summary>")],
        },
        {
            "case": "three-overflows-then-success",
            "messages": [
                message("system", "system prompt"),
                *[
                    entry
                    for index in range(5)
                    for entry in (
                        message("user", f"turn {index}"),
                        message("assistant", f"reply {index}"),
                    )
                ],
            ],
            "strict": False,
            "answers": [
                answer(overflow=True),
                answer(overflow=True),
                answer(overflow=True),
                answer(text="<summary>after three drops</summary>"),
            ],
        },
        {
            "case": "four-overflows-propagate",
            "messages": [
                message("system", "system prompt"),
                *[
                    entry
                    for index in range(8)
                    for entry in (
                        message("user", f"turn {index}"),
                        message("assistant", f"reply {index}"),
                    )
                ],
            ],
            "strict": False,
            "answers": [answer(overflow=True)] * 5,
        },
        {
            "case": "overflow-with-nothing-to-drop",
            "messages": [
                message("system", "system prompt"),
                message("user", "only round"),
                message("assistant", "reply"),
            ],
            "strict": False,
            "answers": [answer(overflow=True)],
        },
        {
            "case": "extra-instructions",
            "messages": conversation,
            "strict": False,
            "extraInstructions": "keep the file names",
            "answers": [answer(text="<summary>with instructions</summary>")],
        },
    ]


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
            reset_session=nothing,
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


def digest(value: str) -> dict[str, Any]:
    """A string recorded by its length and its SHA-256, never by its content.

    Used for anything reference-authored: the two prose runs the envelope
    carries and the compaction request the prompt files hold. A digest still
    fails the replay on any change, while no reference sentence ships.
    """

    return {
        "length": len(value),
        "digest": hashlib.sha256(value.encode("utf-8")).hexdigest(),
    }


_MESSAGES_OPEN = "<previous_user_messages>"
_MESSAGES_CLOSE = "</previous_user_messages>"
_SUMMARY_BLOCK_OPEN = "<compaction_summary>"

#: What stands in the committed corpus where the reference wrote prose. The Rust
#: replay masks its own prose with the same two markers, so what is compared is
#: every byte the envelope's readers depend on and nothing either side authored.
_PREAMBLE_MARK = "{preamble}"
_SUMMARY_LEAD_MARK = "{summaryLead}"


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
    managers = []
    for record in manager:
        projected = dict(record)
        # The manager's own messages carry the envelope; nothing replays this
        # section yet, so it stays a whole-content digest until EP-044 gives it
        # a consumer and decides the shape it needs.
        projected["messagesAfter"] = [
            {**item, "content": digest(item["content"])} if item["injected"] else item
            for item in record["messagesAfter"]
        ]
        # A scenario supplies every summary the manager returns, except the
        # placeholder the reference substitutes when summarization failed.
        if record["failures"] and record.get("outcome") == "returned":
            projected["summary"] = digest(record["summary"])
        managers.append(projected)
    return {
        **capture,
        "envelopeProse": prose,
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
            "length, because those are the reference's own sentences. Regenerate with "
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
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(
            arguments.reference, arguments.python, pinned
        )
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
