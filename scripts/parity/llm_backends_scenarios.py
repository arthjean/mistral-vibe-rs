"""The scenarios and cases ``llm_backends.py`` drives through the pinned reference.

A scenario is a provider entry, a model entry, the API budgets, the calls made
through the agent loop and what the scripted HTTP stand-in answers each request.
A probe case and a completion case drive the utility model's availability probe
and one background completion against the same stand-in. Every provider, model,
message, response and key here is authored for the corpus; none is read from
the reference.
"""

from __future__ import annotations

import base64
import copy
from typing import Any


KEY_VARIABLE = "ORACLE_API_KEY"
#: The variable the Mistral client reads a key from when it is handed none.
CLIENT_KEY_VARIABLE = "MISTRAL_API_KEY"
KEY = "oracle-key"
SESSION_ID = "session-oracle"
#: One pixel, so an image scenario carries real base64 without bulk.
PIXEL = base64.b64encode(
    bytes.fromhex(
        "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
        "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082"
    )
).decode()


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------


def mistral_provider(**extra: Any) -> dict[str, Any]:
    entry = {
        "name": "mistral",
        "api_base": "$BASE/v1",
        "api_key_env_var": KEY_VARIABLE,
        "backend": "mistral",
    }
    entry.update(extra)
    return entry


def generic_provider(style: str, **extra: Any) -> dict[str, Any]:
    entry = {
        "name": f"{style}-provider",
        "api_base": "$BASE" if style == "anthropic" else "$BASE/v1",
        "api_key_env_var": KEY_VARIABLE,
        "api_style": style,
        "backend": "generic",
    }
    entry.update(extra)
    return entry


def vertex_provider(**extra: Any) -> dict[str, Any]:
    entry = {
        "name": "vertex",
        "api_base": "",
        "api_key_env_var": "",
        "api_style": "vertex-anthropic",
        "backend": "generic",
        "project_id": "oracle-project",
        "region": "europe-west1",
    }
    entry.update(extra)
    return entry


def model(provider: str, **extra: Any) -> dict[str, Any]:
    entry = {
        "name": "oracle-model",
        "provider": provider,
        "alias": "oracle",
        "temperature": 0.2,
        "thinking": "off",
    }
    entry.update(extra)
    return entry


def system(text: str) -> dict[str, Any]:
    return {"role": "system", "content": text}


def user(text: str, images: int = 0) -> dict[str, Any]:
    entry: dict[str, Any] = {"role": "user", "content": text}
    if images:
        entry["images"] = [
            {"source": {"kind": "inline", "data": PIXEL}, "alias": f"img{i}", "mime_type": "image/png"}
            for i in range(images)
        ]
    return entry


def assistant(
    content: str = "",
    reasoning: str | None = None,
    payloads: list[dict[str, Any]] | None = None,
    calls: list[tuple[str, str, str]] | None = None,
) -> dict[str, Any]:
    entry: dict[str, Any] = {"role": "assistant", "content": content}
    if reasoning is not None:
        entry["reasoning_content"] = reasoning
    if payloads is not None:
        entry["reasoning_payloads"] = payloads
    if calls:
        entry["tool_calls"] = [
            {"id": call_id, "index": index, "type": "function",
             "function": {"name": name, "arguments": arguments}}
            for index, (call_id, name, arguments) in enumerate(calls)
        ]
    return entry


def tool(call_id: str, name: str, content: str) -> dict[str, Any]:
    return {"role": "tool", "tool_call_id": call_id, "name": name, "content": content}


TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file.",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "grep",
            "description": "Search files.",
            "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}},
        },
    },
]

BASIC = [system("You are terse."), user("Say hi.")]

HISTORY = [
    system("You are terse."),
    user("Read a.txt"),
    assistant("Reading it.", calls=[("call_1", "read_file", '{"path": "a.txt"}')]),
    tool("call_1", "read_file", "alpha"),
    user("Thanks, now b.txt and c.txt"),
    assistant(
        "",
        calls=[
            ("call_2", "read_file", '{"path": "b.txt"}'),
            ("call_3", "read_file", '{"path": "c.txt"}'),
        ],
    ),
    tool("call_2", "read_file", "beta"),
    tool("call_3", "read_file", "gamma"),
    user("Summarize."),
]


def call(
    messages: list[dict[str, Any]] | None = None,
    streaming: bool = True,
    **extra: Any,
) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "messages": messages if messages is not None else BASIC,
        "streaming": streaming,
        "tools": [],
        "toolChoice": "auto",
        "maxTokens": None,
        "metadata": {"session_id": SESSION_ID, "call_type": "main_call"},
    }
    entry.update(extra)
    return entry


def scenario(
    name: str,
    provider: dict[str, Any],
    model_entry: dict[str, Any],
    calls: list[dict[str, Any]],
    responses: list[dict[str, Any]],
    **extra: Any,
) -> dict[str, Any]:
    entry = {
        "name": name,
        "provider": provider,
        "model": model_entry,
        "calls": calls,
        "responses": responses,
        "api": {
            "timeout": 5.0,
            "retryMaxElapsedTime": 3.0,
            "connectTimeout": 5.0,
            "writeTimeout": 5.0,
            "poolTimeout": 5.0,
        },
        "env": {KEY_VARIABLE: KEY},
    }
    entry.update(extra)
    return entry


# --------------------------------------------------------------------------
# Response builders, one per dialect
# --------------------------------------------------------------------------


def chat_chunk(delta: dict[str, Any], finish: str | None = None, usage: dict[str, Any] | None = None,
               with_choice: bool = True) -> dict[str, Any]:
    body: dict[str, Any] = {
        "id": "cmpl-oracle",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": "oracle-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}] if with_choice else [],
    }
    if usage is not None:
        body["usage"] = usage
    return body


USAGE = {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}
CACHED_USAGE = {
    "prompt_tokens": 40,
    "completion_tokens": 6,
    "total_tokens": 46,
    "prompt_tokens_details": {"cached_tokens": 32},
}


def chat_stream(
    pieces: list[str] | None = None,
    finish: str | None = "stop",
    usage: dict[str, Any] | None = None,
    reasoning: list[Any] | None = None,
    calls: list[dict[str, Any]] | None = None,
    reasoning_field: str = "reasoning_content",
    done: bool = True,
    usage_chunk: bool = False,
) -> dict[str, Any]:
    events: list[dict[str, Any]] = [{"data": chat_chunk({"role": "assistant", "content": ""})}]
    for piece in reasoning or []:
        if isinstance(piece, str):
            events.append({"data": chat_chunk({reasoning_field: piece})})
        else:
            events.append({"data": chat_chunk({"content": piece})})
    for piece in pieces if pieces is not None else ["Hi", " there."]:
        events.append({"data": chat_chunk({"content": piece})})
    for delta in calls or []:
        events.append({"data": chat_chunk({"tool_calls": [delta]})})
    if usage_chunk:
        events.append({"data": chat_chunk({"content": ""}, finish=finish)})
        events.append({"data": chat_chunk({}, usage=usage or USAGE, with_choice=False)})
    else:
        events.append({"data": chat_chunk({"content": ""}, finish=finish, usage=usage or USAGE)})
    if done:
        events.append({"data": "[DONE]"})
    return {"status": 200, "sse": events}


def chat_json(content: Any = "Hi there.", finish: str = "stop", usage: dict[str, Any] | None = None,
              calls: list[dict[str, Any]] | None = None, extra: dict[str, Any] | None = None) -> dict[str, Any]:
    message: dict[str, Any] = {"role": "assistant", "content": content}
    if calls:
        message["tool_calls"] = calls
    if extra:
        message.update(extra)
    return {
        "status": 200,
        "json": {
            "id": "cmpl-oracle",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": "oracle-model",
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": usage or USAGE,
        },
    }


def anthropic_events(
    blocks: list[dict[str, Any]],
    stop: str | None = "end_turn",
    usage: dict[str, Any] | None = None,
    stop_details: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """A Messages stream: each block is opened, filled by deltas, closed."""

    start_usage = usage or {"input_tokens": 20, "output_tokens": 1}
    events: list[dict[str, Any]] = [
        {"event": "message_start", "data": {
            "type": "message_start",
            "message": {"id": "msg_oracle", "type": "message", "role": "assistant",
                        "model": "oracle-model", "content": [], "stop_reason": None,
                        "usage": start_usage},
        }},
        {"event": "ping", "data": {"type": "ping"}},
    ]
    for index, block in enumerate(blocks):
        kind = block["type"]
        if kind == "text":
            events.append({"event": "content_block_start", "data": {
                "type": "content_block_start", "index": index,
                "content_block": {"type": "text", "text": ""}}})
            for piece in block["pieces"]:
                events.append({"event": "content_block_delta", "data": {
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "text_delta", "text": piece}}})
        elif kind == "thinking":
            events.append({"event": "content_block_start", "data": {
                "type": "content_block_start", "index": index,
                "content_block": {"type": "thinking", "thinking": ""}}})
            for piece in block["pieces"]:
                events.append({"event": "content_block_delta", "data": {
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "thinking_delta", "thinking": piece}}})
            for piece in block.get("signature", []):
                events.append({"event": "content_block_delta", "data": {
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "signature_delta", "signature": piece}}})
        elif kind == "redacted_thinking":
            events.append({"event": "content_block_start", "data": {
                "type": "content_block_start", "index": index,
                "content_block": {"type": "redacted_thinking", "data": block["data"]}}})
        elif kind == "tool_use":
            events.append({"event": "content_block_start", "data": {
                "type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": block["id"], "name": block["name"],
                                  "input": {}}}})
            for piece in block["pieces"]:
                events.append({"event": "content_block_delta", "data": {
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": piece}}})
        elif kind == "unknown":
            events.append({"event": "content_block_start", "data": {
                "type": "content_block_start", "index": index,
                "content_block": {"type": "server_tool_use", "id": "srv", "name": "web", "input": {}}}})
            events.append({"event": "content_block_delta", "data": {
                "type": "content_block_delta", "index": index,
                "delta": {"type": "citations_delta", "citation": {"type": "x"}}}})
        events.append({"event": "content_block_stop", "data": {"type": "content_block_stop", "index": index}})
    delta: dict[str, Any] = {"stop_reason": stop, "stop_sequence": None}
    if stop_details is not None:
        delta["stop_details"] = stop_details
    events.append({"event": "message_delta", "data": {
        "type": "message_delta", "delta": delta, "usage": {"output_tokens": 9}}})
    events.append({"event": "message_stop", "data": {"type": "message_stop"}})
    return {"status": 200, "sse": events}


def anthropic_json(content: list[dict[str, Any]], stop: str = "end_turn",
                   usage: dict[str, Any] | None = None,
                   stop_details: dict[str, Any] | None = None) -> dict[str, Any]:
    body: dict[str, Any] = {
        "id": "msg_oracle", "type": "message", "role": "assistant", "model": "oracle-model",
        "content": content, "stop_reason": stop, "stop_sequence": None,
        "usage": usage or {"input_tokens": 20, "output_tokens": 9},
    }
    if stop_details is not None:
        body["stop_details"] = stop_details
    return {"status": 200, "json": body}


def responses_stream(events: list[dict[str, Any]], named: bool = True) -> dict[str, Any]:
    return {
        "status": 200,
        "sse": [
            ({"event": event["type"], "data": event} if named else {"data": event})
            for event in events
        ],
    }


RESPONSES_USAGE = {
    "input_tokens": 30,
    "output_tokens": 7,
    "total_tokens": 37,
    "input_tokens_details": {"cached_tokens": 8},
}


def responses_completed(output: list[dict[str, Any]], kind: str = "response.completed",
                        usage: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "type": kind,
        "response": {"id": "resp_oracle", "status": kind.removeprefix("response."),
                     "output": output, "usage": usage or RESPONSES_USAGE},
    }


REASONING_ITEM = {
    "type": "reasoning",
    "id": "rs_oracle",
    "summary": [{"type": "summary_text", "text": "Considered it."}],
    "encrypted_content": "enc-oracle",
}


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def wire_scenarios() -> list[dict[str, Any]]:
    """What each dialect sends, over every message shape the loop replays."""

    out: list[dict[str, Any]] = []
    styles: list[tuple[str, dict[str, Any], dict[str, Any], dict[str, Any]]] = [
        ("mistral", mistral_provider(), chat_stream(), chat_json()),
        ("openai", generic_provider("openai"), chat_stream(), chat_json()),
        ("mistral-generic", generic_provider("openai", name="mistral"), chat_stream(), chat_json()),
        ("reasoning", generic_provider("reasoning"), chat_stream(), chat_json()),
        ("anthropic", generic_provider("anthropic"),
         anthropic_events([{"type": "text", "pieces": ["Hi", " there."]}]),
         anthropic_json([{"type": "text", "text": "Hi there."}])),
        ("responses", generic_provider("openai-responses"),
         responses_stream([
             {"type": "response.created", "response": {"id": "resp_oracle"}},
             {"type": "response.output_item.added", "output_index": 0,
              "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}},
             {"type": "response.output_text.delta", "output_index": 0, "delta": "Hi there."},
             responses_completed([{"type": "message", "id": "msg_1", "role": "assistant",
                                   "content": [{"type": "output_text", "text": "Hi there."}]}]),
         ]),
         {"status": 200, "json": {"id": "resp_oracle", "object": "response", "status": "completed",
                                  "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                                              "content": [{"type": "output_text", "text": "Hi there."}]}],
                                  "usage": RESPONSES_USAGE}}),
        ("vertex", vertex_provider(),
         anthropic_events([{"type": "text", "pieces": ["Hi", " there."]}]),
         anthropic_json([{"type": "text", "text": "Hi there."}])),
    ]
    history_payloads = {
        "anthropic": [{"type": "thinking", "thinking": "Earlier thought.", "signature": "sig-1"},
                      {"type": "redacted_thinking", "data": "opaque"}],
        "vertex": [{"type": "thinking", "thinking": "Earlier thought.", "signature": "sig-1"}],
        "responses": [REASONING_ITEM, {"type": "message", "id": "ignored"}],
    }
    for label, provider, stream_ok, json_ok in styles:
        name = provider["name"]
        for thinking in ("off", "low", "medium", "high", "max"):
            out.append(scenario(
                f"wire/{label}/basic-thinking-{thinking}", provider,
                model(name, thinking=thinking),
                [call()], [copy.deepcopy(stream_ok)],
            ))
        out.append(scenario(
            f"wire/{label}/tools-and-budget", provider,
            model(name, temperature=0.7),
            [call(tools=TOOLS, maxTokens=321)], [copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"wire/{label}/forced-tool", provider, model(name),
            [call(tools=TOOLS, toolChoice={"type": "function", "function": {"name": "grep"}})],
            [copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"wire/{label}/no-tool-choice", provider, model(name),
            [call(tools=TOOLS, toolChoice=None)], [copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"wire/{label}/history", provider, model(name, thinking="off"),
            [call(HISTORY, tools=TOOLS)], [copy.deepcopy(stream_ok)],
        ))
        payloads = history_payloads.get(label)
        reasoning_history = [
            system("You are terse."),
            user("Think first."),
            assistant("Thought done.", reasoning="Earlier thought.", payloads=payloads,
                      calls=[("call_9", "grep", '{"pattern": "x"}')]),
            tool("call_9", "grep", "no match"),
            assistant("", reasoning="Only thought.", payloads=payloads),
            user("Go on."),
        ]
        for thinking in ("off", "high"):
            out.append(scenario(
                f"wire/{label}/reasoning-history-{thinking}", provider,
                model(name, thinking=thinking),
                [call(reasoning_history, tools=TOOLS)], [copy.deepcopy(stream_ok)],
            ))
        for supports in (True, False):
            out.append(scenario(
                f"wire/{label}/images-{'supported' if supports else 'stripped'}", provider,
                model(name, supports_images=supports),
                [call([system("Look."), user("What is this?", images=2), assistant("A pixel."),
                       user("", images=1)])],
                [copy.deepcopy(stream_ok)],
            ))
        out.append(scenario(
            f"wire/{label}/non-streaming", provider, model(name, thinking="high"),
            [call(HISTORY, streaming=False, tools=TOOLS, maxTokens=64)], [copy.deepcopy(json_ok)],
        ))
        out.append(scenario(
            f"wire/{label}/extra-headers", {**provider, "extra_headers": {"x-team": "oracle"}},
            model(name),
            [call()], [copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"wire/{label}/two-systems", provider, model(name),
            [call([system("First."), system("Second."), user("Hi.")])], [copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"wire/{label}/unicode", provider, model(name),
            [call([system("Réponds."), user("Ça va ?   🙂")])], [copy.deepcopy(stream_ok)],
        ))
    # The key a request carries, and where it may be missing.
    out.append(scenario(
        "wire/openai/keyless", generic_provider("openai", api_key_env_var=""), model("openai-provider"),
        [call()], [chat_stream()], env={},
    ))
    out.append(scenario(
        "wire/openai/key-unset", generic_provider("openai"), model("openai-provider"),
        [call()], [chat_stream()], env={},
    ))
    out.append(scenario(
        "wire/anthropic/key-unset", generic_provider("anthropic"), model("anthropic-provider"),
        [call()], [anthropic_events([{"type": "text", "pieces": ["ok"]}])], env={},
    ))
    out.append(scenario(
        "wire/openai/custom-reasoning-field",
        generic_provider("openai", reasoning_field_name="reasoning"),
        model("openai-provider", thinking="medium"),
        [call([system("s"), user("u"), assistant("a", reasoning="r"), user("again")])],
        [chat_stream(reasoning=["thinking ", "hard"], reasoning_field="reasoning")],
    ))
    out.append(scenario(
        "wire/mistral/api-base-with-path", mistral_provider(api_base="$BASE/v1/extra/"),
        model("mistral"), [call()], [chat_stream()],
    ))
    out.append(scenario(
        "wire/openai/api-base-trailing-slash", generic_provider("openai", api_base="$BASE/v1/"),
        model("openai-provider"), [call()], [chat_stream()],
    ))
    out.append(scenario(
        "wire/mistral/no-metadata", mistral_provider(), model("mistral"),
        [call(metadata=None)], [chat_stream()],
    ))
    out.append(scenario(
        "wire/responses/temperature-model", generic_provider("openai-responses"),
        model("openai-responses-provider", name="gpt-4.1", temperature=0.4),
        [call(tools=TOOLS)],
        [responses_stream([responses_completed([])])],
    ))
    out.append(scenario(
        "wire/responses/tool-choice-without-tools", generic_provider("openai-responses"),
        model("openai-responses-provider"),
        [call(toolChoice="auto")],
        [responses_stream([responses_completed([])])],
    ))
    return out


def parse_scenarios() -> list[dict[str, Any]]:
    """What each dialect reads back out of a response."""

    out: list[dict[str, Any]] = []
    tool_deltas = [
        {"index": 0, "id": "call_a", "type": "function",
         "function": {"name": "read_file", "arguments": ""}},
        {"index": 0, "function": {"arguments": '{"path": '}},
        {"index": 0, "function": {"arguments": '"a.txt"}'}},
        {"index": 1, "id": "call_b", "type": "function",
         "function": {"name": "grep", "arguments": '{"pattern": "x"}'}},
    ]
    # Mistral streams each call whole; the chat dialects also split one.
    whole_calls = [
        {"index": 0, "id": "call_a", "type": "function",
         "function": {"name": "read_file", "arguments": '{"path": "a.txt"}'}},
        {"index": 1, "id": "call_b", "type": "function",
         "function": {"name": "grep", "arguments": '{"pattern": "x"}'}},
    ]
    for label, provider in (("mistral", mistral_provider()), ("openai", generic_provider("openai")),
                            ("reasoning", generic_provider("reasoning"))):
        name = provider["name"]
        out.append(scenario(f"parse/{label}/tool-calls", provider, model(name),
                            [call(tools=TOOLS)],
                            [chat_stream(pieces=["Let me look."], finish="tool_calls",
                                         calls=whole_calls if label == "mistral" else tool_deltas)]))
        out.append(scenario(f"parse/{label}/cached-usage", provider, model(name), [call()],
                            [chat_stream(usage=CACHED_USAGE)]))
        out.append(scenario(f"parse/{label}/usage-only-final-chunk", provider, model(name), [call()],
                            [chat_stream(usage_chunk=True)]))
        out.append(scenario(f"parse/{label}/length", provider, model(name), [call()],
                            [chat_stream(finish="length")]))
        out.append(scenario(f"parse/{label}/refusal-finish", provider, model(name), [call()],
                            [chat_stream(finish="refusal")]))
        out.append(scenario(f"parse/{label}/no-done-marker", provider, model(name), [call()],
                            [chat_stream(done=False)]))
        # Typed content chunks are a Mistral and reasoning dialect; the plain
        # chat adapter would stringify the list, which no server sends it.
        if label != "openai":
            out.append(scenario(f"parse/{label}/thinking-chunks", provider, model(name, thinking="high"), [call()],
                                [chat_stream(reasoning=[[{"type": "thinking", "thinking": [
                                    {"type": "text", "text": "Mull"}]}], [{"type": "thinking", "thinking": [
                                        {"type": "text", "text": "ing."}]}, {"type": "text", "text": "Answer: "}]])]))
        out.append(scenario(f"parse/{label}/reasoning-field", provider, model(name, thinking="high"),
                            [call()], [chat_stream(reasoning=["Mull", "ing."])]))
        out.append(scenario(f"parse/{label}/non-streaming-tools", provider, model(name),
                            [call(streaming=False, tools=TOOLS)],
                            [chat_json(content="", finish="tool_calls", calls=[
                                {"id": "call_z", "type": "function", "index": 0,
                                 "function": {"name": "grep", "arguments": '{"pattern": "q"}'}}])]))
        if label != "openai":
            out.append(scenario(f"parse/{label}/non-streaming-blocks", provider, model(name, thinking="high"),
                                [call(streaming=False)],
                                [chat_json(content=[
                                    {"type": "thinking", "thinking": [{"type": "text", "text": "Hmm."}]},
                                    {"type": "text", "text": "Yes."}])]))
        out.append(scenario(f"parse/{label}/crlf-and-comments", provider, model(name), [call()],
                            [{**chat_stream(), "crlf": True,
                              "sse": [{"comment": "keep-alive", **event} if i == 1 else event
                                      for i, event in enumerate(chat_stream()["sse"])]}]))
        out.append(scenario(f"parse/{label}/line-separator-in-text", provider, model(name), [call()],
                            [chat_stream(pieces=["one two", "\u0085three"])]))
    # SSE framing the generic reader handles by name.
    out.append(scenario("parse/openai/foreign-keys", generic_provider("openai"), model("openai-provider"),
                        [call()], [{"status": 200, "sse": [
                            {"raw": "retry: 100\n"}, {"raw": "id: 7\n"},
                            *chat_stream()["sse"]]}]))
    out.append(scenario("parse/openai/data-without-space", generic_provider("openai"),
                        model("openai-provider"), [call()],
                        [{"status": 200, "sse": [{"raw": 'data:{"choices":[]}\n\n'}]}]))
    out.append(scenario("parse/openai/malformed-json", generic_provider("openai"), model("openai-provider"),
                        [call()], [{"status": 200, "sse": [{"raw": "data: {not json}\n\n"}]}]))
    out.append(scenario("parse/openai/empty-stream", generic_provider("openai"), model("openai-provider"),
                        [call()], [{"status": 200, "sse": [{"data": "[DONE]"}]}]))
    out.append(scenario("parse/openai/no-finish-reason", generic_provider("openai"), model("openai-provider"),
                        [call()], [chat_stream(finish=None)]))
    out.append(scenario("parse/openai/no-finish-reason-tolerated",
                        generic_provider("openai", emits_finish_reason=False), model("openai-provider"),
                        [call()], [chat_stream(finish=None)]))
    out.append(scenario("parse/mistral/no-finish-reason", mistral_provider(), model("mistral"),
                        [call()], [chat_stream(finish=None)]))
    out.append(scenario("parse/openai/truncated", generic_provider("openai"), model("openai-provider"),
                        [call()], [{**chat_stream(finish=None, done=False), "truncate": True}]))
    out.append(scenario("parse/openai/no-usage", generic_provider("openai"), model("openai-provider"),
                        [call()], [{"status": 200, "sse": [
                            {"data": chat_chunk({"content": "Hi"})},
                            {"data": chat_chunk({}, finish="stop")}, {"data": "[DONE]"}]}]))
    out.append(scenario("parse/openai/non-streaming-refusal-field", generic_provider("openai"),
                        model("openai-provider"), [call(streaming=False)],
                        [chat_json(content=None, extra={"refusal": "No."})]))
    # Anthropic.
    anth = generic_provider("anthropic")
    out.append(scenario("parse/anthropic/thinking-signature-tools", anth, model("anthropic-provider", thinking="high"),
                        [call(tools=TOOLS)],
                        [anthropic_events([
                            {"type": "thinking", "pieces": ["Let me ", "check."], "signature": ["sig-", "abc"]},
                            {"type": "text", "pieces": ["Checking."]},
                            {"type": "tool_use", "id": "toolu_1", "name": "read_file",
                             "pieces": ['{"path"', ': "a.txt"}']},
                            {"type": "redacted_thinking", "data": "opaque-1"},
                            {"type": "tool_use", "id": "toolu_2", "name": "grep", "pieces": []},
                        ], stop="tool_use", usage={"input_tokens": 20, "output_tokens": 1,
                                                   "cache_creation_input_tokens": 5,
                                                   "cache_read_input_tokens": 7})]))
    out.append(scenario("parse/anthropic/refusal-details", anth, model("anthropic-provider"), [call()],
                        [anthropic_events([{"type": "text", "pieces": ["I can"]}], stop="refusal",
                                          stop_details={"category": "cyber", "explanation": "Declined."})]))
    out.append(scenario("parse/anthropic/max-tokens", anth, model("anthropic-provider"), [call()],
                        [anthropic_events([{"type": "text", "pieces": ["cut"]}], stop="max_tokens")]))
    out.append(scenario("parse/anthropic/unknown-blocks", anth, model("anthropic-provider"), [call()],
                        [anthropic_events([{"type": "unknown"}, {"type": "text", "pieces": ["ok"]}])]))
    out.append(scenario("parse/anthropic/unnamed-events", anth, model("anthropic-provider"), [call()],
                        [{"status": 200, "sse": [{"data": event["data"]} for event in
                                                 anthropic_events([{"type": "text", "pieces": ["x"]}])["sse"]]}]))
    out.append(scenario("parse/anthropic/error-event", anth, model("anthropic-provider"), [call()],
                        [{"status": 200, "sse": [
                            anthropic_events([])["sse"][0],
                            {"event": "error", "data": {"type": "error", "error": {
                                "type": "overloaded_error", "message": "Overloaded"}}}]}]))
    out.append(scenario("parse/anthropic/no-stop", anth, model("anthropic-provider"), [call()],
                        [{"status": 200, "sse": anthropic_events([{"type": "text", "pieces": ["x"]}],
                                                                 stop=None)["sse"]}]))
    out.append(scenario("parse/anthropic/no-usage", anth, model("anthropic-provider"), [call()],
                        [{"status": 200, "sse": [
                            {"event": "content_block_start", "data": {"type": "content_block_start", "index": 0,
                                                                      "content_block": {"type": "text", "text": ""}}},
                            {"event": "content_block_delta", "data": {"type": "content_block_delta", "index": 0,
                                                                      "delta": {"type": "text_delta", "text": "x"}}},
                            {"event": "message_delta", "data": {"type": "message_delta",
                                                                "delta": {"stop_reason": "end_turn"}}}]}]))
    out.append(scenario("parse/anthropic/non-streaming-full", anth, model("anthropic-provider", thinking="high"),
                        [call(streaming=False, tools=TOOLS)],
                        [anthropic_json([
                            {"type": "thinking", "thinking": "Plan.", "signature": "sig-9"},
                            {"type": "redacted_thinking", "data": "opaque-9"},
                            {"type": "text", "text": "Doing."},
                            {"type": "tool_use", "id": "toolu_9", "name": "grep", "input": {"pattern": "z"}},
                        ], stop="tool_use", usage={"input_tokens": 3, "output_tokens": 4,
                                                   "cache_read_input_tokens": 2})]))
    out.append(scenario("parse/anthropic/non-streaming-refusal", anth, model("anthropic-provider"),
                        [call(streaming=False)],
                        [anthropic_json([{"type": "text", "text": "No."}], stop="refusal",
                                        stop_details={"category": "bio", "explanation": "Nope."})]))
    # OpenAI Responses.
    resp = generic_provider("openai-responses")
    rm = model("openai-responses-provider", thinking="high")
    out.append(scenario("parse/responses/full-stream", resp, rm, [call(tools=TOOLS)],
                        [responses_stream([
                            {"type": "response.created", "response": {"id": "resp_oracle"}},
                            {"type": "response.output_item.added", "output_index": 0, "item": {
                                "type": "reasoning", "id": "rs_1", "summary": []}},
                            {"type": "response.reasoning_summary_text.delta", "output_index": 0,
                             "delta": "Think"},
                            {"type": "response.summary_text.delta", "output_index": 0, "delta": "ing."},
                            {"type": "response.output_item.added", "output_index": 1, "item": {
                                "type": "message", "id": "m_c", "role": "assistant", "phase": "commentary",
                                "content": []}},
                            {"type": "response.output_text.delta", "output_index": 1, "delta": "aside"},
                            {"type": "response.output_item.added", "output_index": 2, "item": {
                                "type": "message", "id": "m_1", "role": "assistant", "content": []}},
                            {"type": "response.output_text.delta", "output_index": 2, "delta": "Answer."},
                            {"type": "response.output_item.added", "output_index": 3, "item": {
                                "type": "function_call", "id": "fc_1", "call_id": "call_r1",
                                "name": "read_file", "arguments": ""}},
                            {"type": "response.function_call_arguments.delta", "output_index": 3,
                             "item_id": "fc_1", "delta": '{"path": '},
                            {"type": "response.function_call_arguments.delta", "output_index": 3,
                             "item_id": "fc_1", "delta": '"a.txt"}'},
                            {"type": "response.function_call_arguments.done", "output_index": 3,
                             "item_id": "fc_1", "arguments": '{"path": "a.txt"}'},
                            {"type": "response.output_item.done", "output_index": 3, "item": {
                                "type": "function_call", "id": "fc_1", "call_id": "call_r1",
                                "name": "read_file", "arguments": '{"path": "a.txt"}'}},
                            {"type": "response.output_item.added", "output_index": 4, "item": {
                                "type": "function_call", "id": "fc_2", "call_id": "call_r2",
                                "name": "grep", "arguments": ""}},
                            {"type": "response.output_item.done", "output_index": 4, "item": {
                                "type": "function_call", "id": "fc_2", "call_id": "call_r2",
                                "name": "grep", "arguments": '{"pattern": "y"}'}},
                            {"type": "response.some_future_event", "output_index": 4},
                            responses_completed([REASONING_ITEM,
                                                 {"type": "reasoning", "id": "rs_2", "summary": []}]),
                        ])]))
    out.append(scenario("parse/responses/incomplete", resp, rm, [call()],
                        [responses_stream([
                            {"type": "response.output_text.delta", "output_index": 0, "delta": "Part"},
                            responses_completed([], kind="response.incomplete")])]))
    out.append(scenario("parse/responses/unnamed-events", resp, rm, [call()],
                        [responses_stream([
                            {"type": "response.output_text.delta", "output_index": 0, "delta": "x"},
                            responses_completed([])], named=False)]))
    for code, label in (("rate_limit_exceeded", "rate-limit"), ("server_error", "server"),
                        ("invalid_prompt", "invalid-prompt")):
        out.append(scenario(f"parse/responses/failed-{label}", resp, rm, [call()],
                            [responses_stream([
                                {"type": "response.created", "response": {"id": "r"}},
                                {"type": "response.failed", "response": {
                                    "id": "r", "status": "failed",
                                    "error": {"code": code, "message": f"Failure {label}"}}}]),
                             responses_stream([
                                 {"type": "response.output_text.delta", "output_index": 0, "delta": "ok"},
                                 responses_completed([])])]))
    out.append(scenario("parse/responses/error-event", resp, rm, [call()],
                        [responses_stream([
                            {"type": "response.output_text.delta", "output_index": 0, "delta": "par"},
                            {"type": "error", "code": "server_error", "message": "Boom"}])]))
    out.append(scenario("parse/responses/non-streaming", resp, rm, [call(streaming=False, tools=TOOLS)],
                        [{"status": 200, "json": {"id": "r", "object": "response", "status": "completed",
                                                  "output": [
                                                      REASONING_ITEM,
                                                      {"type": "message", "id": "m", "role": "assistant",
                                                       "phase": "commentary",
                                                       "content": [{"type": "output_text", "text": "aside"}]},
                                                      {"type": "message", "id": "m2", "role": "assistant",
                                                       "content": [{"type": "output_text", "text": "Done."},
                                                                   {"type": "refusal", "refusal": "x"}]},
                                                      {"type": "function_call", "id": "fc", "call_id": "c1",
                                                       "name": "grep", "arguments": '{"pattern": "q"}'}],
                                                  "usage": RESPONSES_USAGE}}]))
    return out


def error_scenarios() -> list[dict[str, Any]]:
    """What a refused or broken call is classified into."""

    out: list[dict[str, Any]] = []
    bodies = {
        "context-400": (400, {"error": {"message": "Prompt is too long for this model"}}),
        "context-422": (422, {"detail": "model_context_exceeded"}),
        "response-422": (422, {"error": {"message": "max_tokens_exceeded"}}),
        "invalid-model-400": (400, {"error": {"type": "invalid_model", "message": "Invalid model: x"}}),
        "invalid-model-404": (404, {"error": {"message": "invalid_model"}}),
        "bad-request": (400, {"message": "Something else"}),
        "unauthorized": (401, {"error": {"message": "Unauthorized"}}),
        "forbidden": (403, {"detail": "Forbidden"}),
        "not-found": (404, {"error": {"type": "not_found"}}),
        "too-large": (413, {"error": {"message": "Payload too large"}}),
        "plain-text": (400, None),
    }
    providers = (
        ("mistral", mistral_provider()),
        ("openai", generic_provider("openai")),
        ("anthropic", generic_provider("anthropic")),
        ("responses", generic_provider("openai-responses")),
    )
    for label, provider in providers:
        for case, (status, body) in bodies.items():
            response = ({"status": status, "json": body} if body is not None
                        else {"status": status, "text": "plain failure text"})
            for streaming in (True, False):
                out.append(scenario(
                    f"errors/{label}/{case}{'' if streaming else '-non-streaming'}",
                    provider, model(provider["name"]),
                    [call(streaming=streaming)], [response],
                ))
        # A rate limit that outlasts the budget, with a request id to report.
        out.append(scenario(
            f"errors/{label}/rate-limit-exhausted", provider, model(provider["name"]),
            [call()], [{"status": 429, "json": {"error": {"message": "slow down"}},
                        "headers": {"x-request-id": "req-1"}}] * 8,
        ))
        out.append(scenario(
            f"errors/{label}/server-error-exhausted", provider, model(provider["name"]),
            [call()], [{"status": 503, "json": {"message": "unavailable"}}] * 8,
        ))
        out.append(scenario(
            f"errors/{label}/connection-refused",
            {**provider, "api_base": provider["api_base"].replace("$BASE", "$CLOSED")},
            model(provider["name"]), [call()], [],
        ))
        out.append(scenario(
            f"errors/{label}/dropped-connection", provider, model(provider["name"]),
            [call()], [{"drop": True}] * 8,
        ))
    # A partial answer before a broken stream is kept in the history.
    out.append(scenario(
        "errors/openai/interrupted-after-text", generic_provider("openai"), model("openai-provider"),
        [call()], [{**chat_stream(pieces=["Partial ", "answer"], finish=None, done=False), "truncate": True}],
    ))
    out.append(scenario(
        "errors/mistral/interrupted-after-text", mistral_provider(), model("mistral"),
        [call()], [{**chat_stream(pieces=["Partial ", "answer"], finish=None, done=False), "truncate": True}],
    ))
    out.append(scenario(
        "errors/mistral/validation-422", mistral_provider(), model("mistral"),
        [call()], [{"status": 422, "json": {"detail": [{"loc": ["body"], "msg": "bad", "type": "x"}]}}],
    ))
    out.append(scenario(
        "errors/mistral/wrong-content-type", mistral_provider(), model("mistral"),
        [call()], [{"status": 200, "json": {"unexpected": True}}],
    ))
    out.append(scenario(
        "errors/openai/success-body-not-json", generic_provider("openai"), model("openai-provider"),
        [call(streaming=False)], [{"status": 200, "text": "not json"}],
    ))
    out.append(scenario(
        "errors/vertex/unauthorized", vertex_provider(), model("vertex"),
        [call()], [{"status": 401, "json": {"error": {"message": "bad token"}}}],
    ))
    return out


def retry_scenarios() -> list[dict[str, Any]]:
    """How long each backend waits, and what it reports while it does."""

    out: list[dict[str, Any]] = []
    providers = (
        ("mistral", mistral_provider(), chat_stream(), chat_json()),
        ("openai", generic_provider("openai"), chat_stream(), chat_json()),
        ("anthropic", generic_provider("anthropic"),
         anthropic_events([{"type": "text", "pieces": ["ok"]}]),
         anthropic_json([{"type": "text", "text": "ok"}])),
    )
    for label, provider, stream_ok, json_ok in providers:
        name = provider["name"]
        for status in (408, 409, 425, 429, 500, 502, 503, 504, 529):
            out.append(scenario(
                f"retry/{label}/status-{status}", provider, model(name),
                [call()], [{"status": status, "json": {"error": {"message": "wait"}}},
                           copy.deepcopy(stream_ok)],
            ))
        out.append(scenario(
            f"retry/{label}/retry-after-seconds", provider, model(name), [call()],
            [{"status": 429, "json": {}, "headers": {"retry-after": "2"}}, copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"retry/{label}/retry-after-over-cap", provider, model(name), [call()],
            [{"status": 503, "json": {}, "headers": {"retry-after": "90"}}, copy.deepcopy(stream_ok)],
            api={"timeout": 5.0, "retryMaxElapsedTime": 400.0, "connectTimeout": 5.0,
                 "writeTimeout": 5.0, "poolTimeout": 5.0},
        ))
        out.append(scenario(
            f"retry/{label}/retry-after-past-date", provider, model(name), [call()],
            [{"status": 429, "json": {}, "headers": {"retry-after": "Wed, 21 Oct 2015 07:28:00 GMT"}},
             copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"retry/{label}/retry-after-garbage", provider, model(name), [call()],
            [{"status": 429, "json": {}, "headers": {"retry-after": "soon"}}, copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"retry/{label}/long-backoff", provider, model(name), [call()],
            [{"status": 500, "json": {}}] * 12 + [copy.deepcopy(stream_ok)],
            api={"timeout": 5.0, "retryMaxElapsedTime": 200.0, "connectTimeout": 5.0,
                 "writeTimeout": 5.0, "poolTimeout": 5.0},
        ))
        out.append(scenario(
            f"retry/{label}/zero-budget", provider, model(name), [call()],
            [{"status": 503, "json": {}}, copy.deepcopy(stream_ok)],
            api={"timeout": 5.0, "retryMaxElapsedTime": 0.0, "connectTimeout": 5.0,
                 "writeTimeout": 5.0, "poolTimeout": 5.0},
        ))
        out.append(scenario(
            f"retry/{label}/non-streaming", provider, model(name), [call(streaming=False)],
            [{"status": 502, "json": {}}, copy.deepcopy(json_ok)],
        ))
        out.append(scenario(
            f"retry/{label}/connection-drop-then-success", provider, model(name), [call()],
            [{"drop": True}, copy.deepcopy(stream_ok)],
        ))
        out.append(scenario(
            f"retry/{label}/read-timeout", provider, model(name), [call()],
            [{"delay": 1.2, "status": 200, "json": {}}, copy.deepcopy(stream_ok)],
            api={"timeout": 0.4, "retryMaxElapsedTime": 3.0, "connectTimeout": 5.0,
                 "writeTimeout": 5.0, "poolTimeout": 5.0},
        ))
        # The pacer spaces the next call after a rate limit.
        out.append(scenario(
            f"retry/{label}/pacing", provider, model(name), [call(), call(), call()],
            [{"status": 429, "json": {}}, copy.deepcopy(stream_ok), copy.deepcopy(stream_ok),
             copy.deepcopy(stream_ok)],
        ))
    # A mid-stream failure after output is not retried.
    out.append(scenario(
        "retry/openai/after-first-chunk", generic_provider("openai"), model("openai-provider"), [call()],
        [{**chat_stream(pieces=["x"], finish=None, done=False), "truncate": True}, chat_stream()],
    ))
    return out


#: The two names the fast utility model is asked for under, in preference order
#: (reference ``FAST_MODEL_CANDIDATES``).
FAST_CANDIDATES = ("mistral-vibe-cli-fast", "mistral-small-latest")
#: The wall clock the availability cache reads, frozen so ages are exact.
PROBE_NOW = 1_800_000_000
#: The probe budget every probe case runs with unless it names another.
PROBE_BUDGET = 1.0


def provider_entry(name: str, api_base: str, key_variable: str, backend: str = "mistral") -> dict[str, Any]:
    entry: dict[str, Any] = {"name": name, "api_base": api_base, "api_key_env_var": key_variable,
                             "backend": backend}
    if backend == "generic":
        entry["api_style"] = "openai"
    return entry


OPENAI = provider_entry("openai", "https://api.openai.com/v1", "OPENAI_KEY", "generic")
PUBLIC = provider_entry("mistral", "https://api.mistral.ai/v1", KEY_VARIABLE)
LOCAL = provider_entry("mistral", "http://127.0.0.1:1/v1", "")
BIG = {"name": "big", "provider": "openai", "alias": "big"}
MEDIUM = {"name": "mistral-medium-latest", "provider": "mistral", "alias": "medium"}


def probe_case(name: str, *, rounds: list[dict[str, Any]], responses: list[dict[str, Any]] | None = None,
               providers: list[dict[str, Any]] | None = None, active: Any = BIG,
               allowed: list[str] | None = None, env: dict[str, str] | None = None,
               seed: list[dict[str, Any]] | None = None, raw_file: str | None = None,
               models: list[dict[str, Any]] | None = None,
               utility: dict[str, str] | None = None) -> dict[str, Any]:
    """One availability case: a cache seeded before the first round, then rounds.

    A seed entry is ``{"model", "available", "age"}``, stored under the key the
    case's Mistral provider and credential give that model, or ``{"raw",
    "value"}``, stored under a key of its own. A round may ``advance`` both
    clocks first, ``seed`` the file again (another process writing it), turn
    probing off with ``disabled``, pick its ``features`` and its ``budget``.
    """

    return {
        "name": name,
        "providers": providers if providers is not None
        else [OPENAI, provider_entry("mistral", "$BASE/v1", KEY_VARIABLE)],
        "active": active,
        "allowedModels": allowed or [],
        "models": models or [],
        "utilityModels": utility or {},
        "env": env if env is not None else {KEY_VARIABLE: KEY},
        "seed": seed or [],
        "rawFile": raw_file,
        "responses": responses or [],
        "rounds": [{"advance": 0, "features": ["title"], "disabled": False,
                    "budget": PROBE_BUDGET, "seed": [], **r} for r in rounds],
    }


def probe_cases() -> list[dict[str, Any]]:
    first, second = FAST_CANDIDATES
    served = chat_json("ok")
    once = [{}]

    def refusal(status: int) -> dict[str, Any]:
        return {"status": status, "json": {"message": f"status {status}"}}

    keyless = [OPENAI, provider_entry("mistral", "$BASE/v1", "")]
    return [
        probe_case("served-first", rounds=once, responses=[served]),
        probe_case("refused-first-served-second", rounds=once, responses=[refusal(404), served]),
        probe_case("refused-both", rounds=once, responses=[refusal(400), refusal(403)]),
        probe_case("unauthorized", rounds=once, responses=[refusal(401), refusal(401)]),
        probe_case("unprocessable", rounds=once, responses=[refusal(422), served]),
        probe_case("request-timeout-status", rounds=once, responses=[refusal(408), served]),
        probe_case("rate-limited", rounds=once, responses=[refusal(429), served]),
        probe_case("server-errors", rounds=[{}, {}, {"advance": 601}],
                   responses=[refusal(500), refusal(503), served]),
        probe_case("slow-first", rounds=[{}, {"advance": 599}, {"advance": 2}],
                   responses=[{**served, "delay": 3.0}, served]),
        probe_case("connection-refused", rounds=once,
                   providers=[OPENAI, provider_entry("mistral", "$CLOSED/v1", KEY_VARIABLE)]),
        probe_case("keyed-without-key", rounds=once, env={}),
        probe_case("keyless", rounds=once, providers=keyless, env={}, responses=[served]),
        probe_case("keyless-client-environment-key", rounds=once, providers=keyless,
                   env={CLIENT_KEY_VARIABLE: "oracle-client-key"}, responses=[served]),
        probe_case("keyless-refused", rounds=once, providers=keyless, env={},
                   responses=[refusal(404), refusal(404)]),
        probe_case("no-features", rounds=[{"features": []}]),
        probe_case("disabled", rounds=[{"disabled": True}, {}], responses=[served]),
        probe_case("no-mistral-provider", rounds=once, providers=[OPENAI]),
        probe_case("allowlist-second-only", rounds=once, allowed=[second], responses=[served]),
        probe_case("allowlist-excludes-both", rounds=once, allowed=["big"]),
        probe_case("allowlist-alias-only", rounds=once, allowed=["mistral-small"]),
        probe_case("cached-available", rounds=once,
                   seed=[{"model": first, "available": True, "age": 10}]),
        probe_case("cached-unavailable", rounds=once, responses=[served],
                   seed=[{"model": first, "available": False, "age": 10}]),
        probe_case("cached-both-unavailable", rounds=once,
                   seed=[{"model": first, "available": False, "age": 10},
                         {"model": second, "available": False, "age": 3_599}]),
        probe_case("stale-unavailable", rounds=once, responses=[served],
                   seed=[{"model": first, "available": False, "age": 3_600}]),
        probe_case("stale-available", rounds=once, responses=[refusal(404), served],
                   seed=[{"model": first, "available": True, "age": 604_800}]),
        probe_case("fresh-available-boundary", rounds=once,
                   seed=[{"model": first, "available": True, "age": 604_799}]),
        probe_case("second-cached-available", rounds=once, responses=[served],
                   seed=[{"model": second, "available": True, "age": 10}]),
        probe_case("prune-on-write", rounds=once, responses=[served], seed=[
            {"raw": "foreign-fresh", "value": {"available": True, "stored_at_timestamp": PROBE_NOW - 5}},
            {"raw": "foreign-stale", "value": {"available": False, "stored_at_timestamp": PROBE_NOW - 4_000}},
            {"raw": "foreign-extra", "value": {"available": True, "stored_at_timestamp": PROBE_NOW,
                                               "note": "kept as written"}},
            {"raw": "foreign-flag", "value": {"available": False, "stored_at_timestamp": True}},
            {"raw": "foreign-float", "value": {"available": True, "stored_at_timestamp": 1.5e9}},
            {"raw": "foreign-text", "value": "not an entry"},
            {"raw": "fördern", "value": {"available": True, "stored_at_timestamp": PROBE_NOW}},
        ]),
        probe_case("malformed-file", rounds=once, responses=[served], raw_file="{not json"),
        probe_case("array-file", rounds=once, responses=[served], raw_file="[1, 2]"),
        probe_case("malformed-entry", rounds=once, responses=[served], seed=[
            {"raw": "$FIRST", "value": {"available": "yes", "stored_at_timestamp": PROBE_NOW}},
        ]),
        probe_case("remembered-in-process", rounds=[{}, {}], responses=[served]),
        probe_case("external-write-within-a-minute", rounds=[
            {"features": []},
            {"seed": [{"model": first, "available": True, "age": 0}]},
        ], responses=[refusal(404), refusal(404)]),
        probe_case("external-write-after-a-minute", rounds=[
            {"features": []},
            {"advance": 61, "seed": [{"model": first, "available": True, "age": 0}]},
        ]),
        probe_case("unknown-active-model", rounds=once, active=None, responses=[served]),
        probe_case("active-on-mistral", rounds=once, active=MEDIUM, responses=[served]),
        probe_case("title-overridden", rounds=once, utility={"title": "big"}),
        probe_case("title-overridden-to-missing", rounds=once, utility={"title": "nope"},
                   responses=[served]),
        probe_case("smart-approve-overridden", rounds=once, utility={"smart_approve": "big"},
                   responses=[served]),
        probe_case("both-features-overridden", rounds=[{"features": ["title", "smart_approve"]}],
                   utility={"title": "big", "smart_approve": "big"}),
        probe_case("one-of-two-features-overridden", rounds=[{"features": ["smart_approve", "title"]}],
                   utility={"smart_approve": "big"}, responses=[served]),
        probe_case("override-cannot-resolve", rounds=once, active=None, utility={"title": "active"}),
    ]


#: The launch an attributed utility completion reports.
LAUNCH = {"agent_entrypoint": "cli", "agent_version": "9.9.9", "client_name": "vibe_cli",
          "client_version": "9.9.9", "terminal_emulator": "kitty"}
#: Authored here, so they may be recorded verbatim; the user content measures
#: characters, not bytes.
UTILITY_SYSTEM = "Answer with a short label."
UTILITY_USER = "Résumé of the café session 🚀"


def completion_case(name: str, *, feature: str | None = None, call_type: str | None = None,
                    launch: dict[str, Any] | None = None, session: str | None = None,
                    telemetry: bool = True, skip_if_no_key: bool = False,
                    env: dict[str, str] | None = None, providers: list[dict[str, Any]] | None = None,
                    models: list[dict[str, Any]] | None = None,
                    utility: dict[str, str] | None = None,
                    responses: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    """One ``run_utility_completion`` call, its arguments and the stand-in's answers."""

    return {
        "name": name,
        "providers": providers if providers is not None
        else [OPENAI, provider_entry("mistral", "$BASE/v1", KEY_VARIABLE)],
        "active": {"name": "mistral-large-latest", "provider": "mistral", "alias": "large"},
        "allowedModels": [],
        "models": models or [],
        "utilityModels": utility or {},
        "env": env if env is not None else {KEY_VARIABLE: KEY},
        "feature": feature,
        "callType": call_type,
        "launch": launch,
        "session": session,
        "telemetry": telemetry,
        "skipIfNoKey": skip_if_no_key,
        "responses": responses if responses is not None else [chat_json("Cafe notes")],
    }


def completion_cases() -> list[dict[str, Any]]:
    keyless = [OPENAI, provider_entry("mistral", "$BASE/v1", "")]
    plain = {**LAUNCH, "terminal_emulator": None}
    return [
        completion_case("title-attributed", feature="title", launch=LAUNCH,
                        session=SESSION_ID),
        completion_case("title-unattributed", feature="title", telemetry=False),
        completion_case("title-without-terminal", feature="title", launch=plain,
                        session=SESSION_ID),
        completion_case("smart-approve", feature="smart_approve", session=SESSION_ID),
        completion_case("worktree-title", call_type="worktree_title", launch=LAUNCH,
                        skip_if_no_key=True, telemetry=False),
        completion_case("worktree-title-reported", call_type="worktree_title",
                        launch=LAUNCH, skip_if_no_key=True),
        completion_case("unlabeled", session=SESSION_ID),
        completion_case("skipped-without-key", call_type="worktree_title",
                        skip_if_no_key=True, env={}, responses=[]),
        completion_case("keyless-not-skipped", call_type="worktree_title",
                        skip_if_no_key=True, env={}, providers=keyless),
        completion_case("title-override", feature="title", session=SESSION_ID,
                        models=[MEDIUM], utility={"title": "medium"}),
        completion_case("empty-answer", feature="title", session=SESSION_ID,
                        responses=[chat_json("")]),
    ]


def all_scenarios() -> list[dict[str, Any]]:
    return wire_scenarios() + parse_scenarios() + error_scenarios() + retry_scenarios()
