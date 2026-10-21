"""The cases ``compaction.py`` runs through the pinned reference's compaction
functions and manager.

A case names the text, the messages or the scripted model answers one call is
fed. Every value here is authored for the corpus; none is read from the
reference.
"""

from __future__ import annotations

from typing import Any

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
