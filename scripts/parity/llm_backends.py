#!/usr/bin/env python3
"""Capture what the pinned reference's LLM backends put on the wire and read back.

Row 10 of ``docs/parity.md`` is the model backend layer: the Mistral SDK backend,
the generic backend and its five API adapters, the retry and pacing machinery,
the error taxonomy a failed call is classified into, and the utility model a
background nicety runs on. This capture drives the reference's own objects
in-process against a scripted HTTP stand-in on the loopback interface, so every
request is the one the reference really sends and every answer is read back by
the reference's own parsers.

A scenario is a provider entry, a model entry, the API budgets and a list of
calls. Each call is made through the agent loop's own ``_chat_streaming`` or
``_chat`` bound to a minimal stand-in for the loop, so what a failure turns
into is the loop's classification and ``public_error``'s code, not a copy of
either. What the capture records per call:

``requests``   every request the stand-in received: method, path, the headers a
               client chose (transport defaults removed) and the parsed body
``result``     the assistant message the call produced, aggregated the way the
               loop aggregates it, with usage, stop and correlation identifier
``deltas``     the non-empty text and reasoning pieces a streaming call yielded
``error``      the exception class, the turn error code, its details and a
               digest of the message
``appended``   the messages the loop appended to its history, which is what the
               next request replays
``retries``    each retry reason the backend reported, and ``sleeps`` each wait
               it asked for, on a fake clock so a budget is deterministic

Three further families record pure decisions: ``retryDelays`` for the backoff
arithmetic, ``utilitySelections`` for the utility model choice, and
``vertexEndpoints`` for the regional Vertex addresses.

The committed corpus carries only values the scenarios supplied, names, codes,
counts and digests: every message the reference authors is reduced to its
length and SHA-256, which is what ``NOTICE`` requires.

Usage::

    scripts/parity/llm_backends.py --corpus          # capture and write the corpus
    scripts/parity/llm_backends.py --check           # recapture and compare

``--reference`` names the checkout; the script re-executes itself under the
reference interpreter against a ``git archive`` of the pinned commit.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import copy
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import socket
import sys
import threading
import time
from types import SimpleNamespace
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compaction import (  # noqa: E402
    OracleError,
    extract_pinned_tree,
    reexecute_with_reference_interpreter,
    resolve_reference,
)
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402

SCHEMA_VERSION = 1
DEFAULT_CORPUS = Path("crates/vibe-core/tests/llm-backends/corpus.json")
DEFAULT_CACHE = Path(".parity")

KEY_VARIABLE = "ORACLE_API_KEY"
KEY = "oracle-key"
SESSION_ID = "session-oracle"
#: One pixel, so an image scenario carries real base64 without bulk.
PIXEL = base64.b64encode(
    bytes.fromhex(
        "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
        "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082"
    )
).decode()

#: Headers a transport adds on its own, which say nothing about the backend.
TRANSPORT_HEADERS = {"host", "content-length", "accept-encoding", "connection"}


def digest(text: str) -> dict[str, Any]:
    return {"length": len(text), "sha256": hashlib.sha256(text.encode()).hexdigest()}


# --------------------------------------------------------------------------
# The scripted stand-in
# --------------------------------------------------------------------------


def sse_body(events: list[dict[str, Any]], crlf: bool = False) -> bytes:
    """Server-sent events as the wire carries them.

    An event is ``{"data": value}``, optionally with ``"event"``, ``"id"`` or
    ``"comment"`` lines before it, or ``{"raw": text}`` written verbatim.
    """

    newline = "\r\n" if crlf else "\n"
    out: list[str] = []
    for event in events:
        if "raw" in event:
            out.append(event["raw"])
            continue
        if "comment" in event:
            out.append(f": {event['comment']}{newline}")
        if "event" in event:
            out.append(f"event: {event['event']}{newline}")
        if "id" in event:
            out.append(f"id: {event['id']}{newline}")
        data = event["data"]
        payload = data if isinstance(data, str) else json.dumps(data, ensure_ascii=False)
        out.append(f"data: {payload}{newline}{newline}")
    return "".join(out).encode()


class StandIn:
    """Serves scripted responses in order and records every request."""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = copy.deepcopy(responses)
        #: Everything the scenario scripted, which is what may be recorded verbatim.
        self.served = copy.deepcopy(responses)
        self.requests: list[dict[str, Any]] = []
        self.lock = threading.Lock()
        stand_in = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                return

            def do_GET(self) -> None:  # noqa: N802
                self.handle_request("GET")

            def do_POST(self) -> None:  # noqa: N802
                self.handle_request("POST")

            def handle_request(self, method: str) -> None:
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body: Any = json.loads(raw) if raw else None
                except json.JSONDecodeError:
                    body = {"raw": raw.decode("utf-8", "replace")}
                headers = {
                    name.lower(): value
                    for name, value in self.headers.items()
                    if name.lower() not in TRANSPORT_HEADERS
                }
                with stand_in.lock:
                    stand_in.requests.append(
                        {"method": method, "path": self.path, "headers": headers, "body": body}
                    )
                    response = (
                        stand_in.responses.pop(0)
                        if stand_in.responses
                        else {"status": 599, "json": {"error": "unscripted request"}}
                    )
                if response.get("delay"):
                    time.sleep(float(response["delay"]))
                if response.get("drop"):
                    # The connection closes without an answer.
                    self.close_connection = True
                    try:
                        self.connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                    return
                status = int(response.get("status", 200))
                extra = dict(response.get("headers", {}))
                if "sse" in response:
                    payload = sse_body(response["sse"], response.get("crlf", False))
                    content_type = "text/event-stream"
                elif "json" in response:
                    payload = json.dumps(response["json"], ensure_ascii=False).encode()
                    content_type = "application/json"
                else:
                    payload = str(response.get("text", "")).encode()
                    content_type = "text/plain"
                content_type = extra.pop("content-type", content_type)
                self.send_response(status)
                self.send_header("content-type", content_type)
                for name, value in extra.items():
                    self.send_header(name, value)
                if response.get("truncate"):
                    # The stream stops mid-body: the declared length is never met.
                    self.send_header("content-length", str(len(payload) + 64))
                    self.end_headers()
                    self.wfile.write(payload)
                    self.wfile.flush()
                    self.close_connection = True
                    try:
                        self.connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                    return
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        class Server(http.server.ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request: Any, client_address: Any) -> None:
                # A client that gave up on a delayed answer closes its end;
                # that is the scenario, not a failure of the stand-in.
                return

        self.server = Server(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def closed_port() -> int:
    """A loopback port nothing listens on, for a connection that is refused."""

    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


# --------------------------------------------------------------------------
# The fake clock every wait runs on
# --------------------------------------------------------------------------


class FakeClock:
    """Time that only moves when something sleeps on it."""

    def __init__(self) -> None:
        self.now = 1_000.0
        self.sleeps: list[dict[str, Any]] = []

    def monotonic(self) -> float:
        return self.now

    def time(self) -> float:
        return self.now

    async def retry_sleep(self, seconds: float) -> None:
        self.sleeps.append({"kind": "retry", "seconds": round(float(seconds), 6)})
        self.now += max(float(seconds), 0.0)

    async def pacer_sleep(self, seconds: float) -> None:
        self.sleeps.append({"kind": "pacer", "seconds": round(float(seconds), 6)})
        self.now += max(float(seconds), 0.0)


def install_clock(clock: FakeClock) -> None:
    import mistralai.client.utils.retries as sdk_retries

    import vibe.core.utils.retry as retry

    retry.time = SimpleNamespace(monotonic=clock.monotonic)  # type: ignore[attr-defined]
    retry.asyncio = SimpleNamespace(sleep=clock.retry_sleep)  # type: ignore[attr-defined]
    sdk_retries.time = SimpleNamespace(  # type: ignore[attr-defined]
        time=clock.time, sleep=lambda seconds: None
    )
    sdk_retries.asyncio = SimpleNamespace(sleep=clock.retry_sleep)  # type: ignore[attr-defined]
    # The SDK adds up to a second of jitter; the midpoint is what both sides use.
    sdk_retries.random = SimpleNamespace(uniform=lambda low, high: (low + high) / 2)  # type: ignore[attr-defined]


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


# --------------------------------------------------------------------------
# Running a scenario against the reference
# --------------------------------------------------------------------------


def substitute(value: Any, base: str, closed: str) -> Any:
    if isinstance(value, dict):
        return {key: substitute(item, base, closed) for key, item in value.items()}
    if isinstance(value, list):
        return [substitute(item, base, closed) for item in value]
    if isinstance(value, str):
        return value.replace("$BASE", base).replace("$CLOSED", closed)
    return value


def normalize_text(value: Any, base: str, version: str) -> Any:
    if isinstance(value, dict):
        return {key: normalize_text(item, base, version) for key, item in value.items()}
    if isinstance(value, list):
        return [normalize_text(item, base, version) for item in value]
    if isinstance(value, str):
        return value.replace(base, "$BASE").replace(version, "<version>")
    return value


def message_record(message: Any) -> dict[str, Any]:
    record: dict[str, Any] = {"role": str(message.role.value if hasattr(message.role, "value") else message.role)}
    if message.content:
        record["content"] = message.content
    if message.reasoning_content:
        record["reasoningContent"] = message.reasoning_content
    if message.reasoning_payloads:
        record["reasoningPayloads"] = message.reasoning_payloads
    if message.tool_calls:
        record["toolCalls"] = [
            {"id": tc.id, "index": tc.index, "name": tc.function.name,
             "arguments": tc.function.arguments}
            for tc in message.tool_calls
        ]
    if message.tool_call_id:
        record["toolCallId"] = message.tool_call_id
    return record


def scenario_text(value: str | None, stand_in: StandIn) -> Any:
    """A string the scenario supplied, verbatim; one the reference wrote, digested."""

    if value is None:
        return None
    served = json.dumps(stand_in.served, ensure_ascii=False)
    if value in served or json.dumps(value, ensure_ascii=False)[1:-1] in served:
        return value
    return digest(value)


def chunk_record(chunk: Any) -> dict[str, Any]:
    record: dict[str, Any] = {"message": message_record(chunk.message)}
    if chunk.usage is not None:
        record["usage"] = {
            "prompt": chunk.usage.prompt_tokens,
            "completion": chunk.usage.completion_tokens,
            "cached": chunk.usage.cached_tokens,
        }
    if chunk.stop is not None:
        record["stop"] = {key: value for key, value in chunk.stop.model_dump().items()
                          if value is not None}
    if chunk.correlation_id:
        record["correlationId"] = chunk.correlation_id
    return record


def fake_loop(backend: Any, provider: Any, model_config: Any, call_entry: dict[str, Any],
              messages: list[Any], stats: list[Any]) -> Any:
    from vibe.core.agent_loop._loop import AgentLoop
    from vibe.core.llm.format import APIToolFormatHandler
    from vibe.core.types import AvailableTool

    handler = APIToolFormatHandler()
    tools = [AvailableTool.model_validate(item) for item in call_entry["tools"]] or None
    choice = call_entry["toolChoice"]
    if isinstance(choice, dict):
        choice = AvailableTool.model_validate({
            "type": "function",
            "function": {"name": choice["function"]["name"], "description": "", "parameters": {}},
        })
    fake = SimpleNamespace()
    fake.config = SimpleNamespace(
        get_active_model=lambda: model_config,
        get_active_provider=lambda: provider,
        get_provider_for_model=lambda _model: provider,
    )
    metadata = call_entry["metadata"]
    fake._build_backend_metadata = lambda call_type=None: SimpleNamespace(
        model_dump=lambda exclude_none=True: metadata,
        call_type="main_call",
        message_id=None,
    )
    fake.format_handler = SimpleNamespace(
        get_available_tools=lambda _manager: tools,
        get_tool_choice=lambda: choice,
        process_api_response_message=handler.process_api_response_message,
    )
    fake.tool_manager = None
    fake.session_id = SESSION_ID
    fake.config_provider = provider
    fake._messages_for_backend = lambda msgs, active: AgentLoop._messages_for_backend(fake, msgs, active)
    fake._last_user_message_from = AgentLoop._last_user_message_from
    fake.telemetry_client = SimpleNamespace(send_request_sent=lambda **_: None, last_correlation_id=None)
    fake.backend = backend
    fake._get_extra_headers = lambda provider_override=None: AgentLoop._get_extra_headers(
        fake, provider_override
    )
    fake._max_tokens = call_entry["maxTokens"]
    fake._update_stats = lambda usage, time_seconds: stats.append({
        "prompt": usage.prompt_tokens, "completion": usage.completion_tokens,
        "cached": usage.cached_tokens})
    fake.messages = messages
    fake._record_interrupted_assistant = lambda chunk: AgentLoop._record_interrupted_assistant(fake, chunk)
    fake._complete = lambda **kwargs: AgentLoop._complete(fake, **kwargs)
    return fake


async def run_call(backend: Any, provider: Any, model_config: Any, call_entry: dict[str, Any],
                   stand_in: StandIn, closed: str, clock: FakeClock,
                   retries: list[Any]) -> dict[str, Any]:
    from vibe import __version__
    from vibe.app_server._utils import public_error
    from vibe.core.agent_loop._loop import AgentLoop
    from vibe.core.types import LLMMessage

    history = [LLMMessage.model_validate(copy.deepcopy(m)) for m in call_entry["messages"]]
    stats: list[Any] = []
    fake = fake_loop(backend, provider, model_config, call_entry, history, stats)
    before_requests = len(stand_in.requests)
    before_sleeps = len(clock.sleeps)
    before_retries = len(retries)
    before_messages = len(history)
    record: dict[str, Any] = {}
    chunks: list[Any] = []
    try:
        if call_entry["streaming"]:
            async for chunk in AgentLoop._chat_streaming(fake):
                chunks.append(chunk)
        else:
            chunks.append(await AgentLoop._chat(fake))
    except Exception as error:  # noqa: BLE001 - every failure is an observation
        public = public_error(error)
        record["error"] = {
            "type": type(error).__name__,
            "cause": type(error.__cause__).__name__ if error.__cause__ is not None else None,
            "code": public.code,
            "details": public.details,
            # The message names the stand-in's port, which changes every run.
            "message": digest(
                public.message.replace(stand_in.base, "$BASE")
                .replace(closed, "$CLOSED")
                .replace(__version__, "<version>")
            ),
        }
        backend_error = error if hasattr(error, "status") and hasattr(error, "payload_summary") else (
            error.__cause__ if error.__cause__ is not None and hasattr(error.__cause__, "payload_summary")
            else None)
        if backend_error is not None:
            origin = backend_error.api_key_origin
            record["error"]["backend"] = {
                "status": backend_error.status,
                "parsedError": scenario_text(backend_error.parsed_error, stand_in),
                "contextTooLong": backend_error.is_context_too_long,
                "responseTooLong": backend_error.is_response_too_long,
                "invalidModel": backend_error.is_invalid_model,
                "requestId": backend_error.headers.get("x-request-id")
                or backend_error.headers.get("request-id"),
                "keyOrigin": {"source": str(origin.source), "variable": origin.env_var}
                if origin else None,
            }
    if chunks:
        total = chunks[0]
        for chunk in chunks[1:]:
            total = total + chunk
        record["result"] = chunk_record(total)
        record["deltas"] = {
            "text": [c.message.content for c in chunks if c.message.content],
            "reasoning": [c.message.reasoning_content for c in chunks if c.message.reasoning_content],
        }
    record["appended"] = [message_record(m) for m in history[before_messages:]]
    record["stats"] = stats
    record["requests"] = stand_in.requests[before_requests:]
    record["sleeps"] = clock.sleeps[before_sleeps:]
    record["retries"] = retries[before_retries:]
    return record


async def run_scenario(entry: dict[str, Any]) -> dict[str, Any]:
    from vibe import __version__
    from vibe.core.config import ModelConfig, ProviderConfig
    from vibe.core.llm.backend import vertex
    from vibe.core.llm.backend.factory import create_backend
    from vibe.core.utils import AdaptivePacer

    stand_in = StandIn(entry["responses"])
    closed = f"http://127.0.0.1:{closed_port()}"
    clock = FakeClock()
    install_clock(clock)
    for variable in (KEY_VARIABLE,):
        os.environ.pop(variable, None)
    os.environ.update(entry["env"])
    retries: list[dict[str, Any]] = []

    async def on_retry(reason: Any) -> None:
        retries.append({"category": reason.category.value, "detail": reason.detail})

    original_base = vertex.build_vertex_base_url
    vertex.build_vertex_base_url = lambda region: stand_in.base
    vertex._CREDENTIALS = SimpleNamespace(access_token="vertex-token")
    try:
        provider = ProviderConfig.model_validate(substitute(entry["provider"], stand_in.base, closed))
        model_config = ModelConfig.model_validate(entry["model"])
        api = entry["api"]
        backend = create_backend(
            provider=provider,
            timeout=api["timeout"],
            retry_max_elapsed_time=api["retryMaxElapsedTime"],
            connect_timeout=api["connectTimeout"],
            write_timeout=api["writeTimeout"],
            pool_timeout=api["poolTimeout"],
            on_retry=on_retry,
        )
        if hasattr(backend, "_pacer"):
            backend._pacer = AdaptivePacer(clock=clock.monotonic, sleep=clock.pacer_sleep)
        calls = []
        async with backend:
            for call_entry in entry["calls"]:
                calls.append(await run_call(backend, provider, model_config, call_entry,
                                            stand_in, closed, clock, retries))
        # Let reports scheduled from a worker thread land before reading them.
        await asyncio.sleep(0)
    finally:
        vertex.build_vertex_base_url = original_base
        stand_in.close()
    observed = {"calls": calls, "pendingRetries": retries[sum(len(c["retries"]) for c in calls):]}
    observed = json.loads(json.dumps(observed).replace(closed, "$CLOSED"))
    return normalize_text(observed, stand_in.base, __version__)


# --------------------------------------------------------------------------
# Pure decisions
# --------------------------------------------------------------------------


def retry_delays() -> list[dict[str, Any]]:
    """``_next_delay`` over attempts, factors and caps, without a server."""

    from vibe.core.utils.retry import _next_delay

    out: list[dict[str, Any]] = []
    for delay, factor, cap in ((0.5, 2.0, 60.0), (0.0, 2.0, 60.0), (1.0, 1.0, 5.0), (0.5, 3.0, 10.0)):
        for attempt in (0, 1, 2, 5, 7, 12, 40, 200):
            out.append({
                "delay": delay, "factor": factor, "cap": cap, "attempt": attempt,
                "seconds": round(_next_delay(RuntimeError("x"), attempt, delay, factor, cap), 9),
            })
    return out


def utility_selections() -> list[dict[str, Any]]:
    """Which model and provider a background nicety runs on."""

    from vibe.core.config import ModelConfig, ProviderConfig
    from vibe.core.llm import utility_completion

    active = ModelConfig(name="big", provider="openai", alias="big")
    mistral = ProviderConfig(name="mistral", api_base="https://api.mistral.ai/v1",
                             api_key_env_var=KEY_VARIABLE, backend="mistral")
    keyless = ProviderConfig(name="mistral", api_base="http://127.0.0.1:1/v1",
                             api_key_env_var="", backend="mistral")
    openai = ProviderConfig(name="openai", api_base="https://api.openai.com/v1",
                            api_key_env_var="OPENAI_KEY")
    cases = [
        ("mistral-with-key", [openai, mistral], [], {KEY_VARIABLE: KEY}),
        ("mistral-without-key", [openai, mistral], [], {}),
        ("no-mistral-provider", [openai], [], {KEY_VARIABLE: KEY}),
        ("keyless-mistral", [openai, keyless], [], {}),
        ("allowlist-excludes-fast", [openai, mistral], ["big"], {KEY_VARIABLE: KEY}),
        ("allowlist-admits-fast", [openai, mistral], ["mistral-*"], {KEY_VARIABLE: KEY}),
    ]
    out: list[dict[str, Any]] = []
    for name, providers, allowed, env in cases:
        os.environ.pop(KEY_VARIABLE, None)
        os.environ.update(env)

        def provider_for(model_config: Any, providers: list[Any] = providers) -> Any:
            return next(p for p in providers if p.name == model_config.provider)

        def mistral_provider_for(providers: list[Any] = providers) -> Any:
            return next((p for p in providers if p.backend == "mistral"), None)

        config = SimpleNamespace(
            get_active_model=lambda: active,
            get_provider_for_model=provider_for,
            get_mistral_provider=mistral_provider_for,
            allowed_models=allowed,
        )
        chosen, chosen_provider = utility_completion.select_utility_model(config)
        out.append({
            "name": name,
            "active": {"name": active.name, "provider": active.provider, "alias": active.alias},
            "providers": [
                {"name": p.name, "apiBase": p.api_base, "keyVariable": p.api_key_env_var,
                 "backend": str(p.backend)}
                for p in providers
            ],
            "allowedModels": allowed,
            "env": sorted(env),
            "model": chosen.name,
            "alias": chosen.alias,
            "provider": chosen_provider.name,
            "fast": utility_completion.is_fast_utility_model(config),
            "temperature": chosen.temperature,
        })
    os.environ.pop(KEY_VARIABLE, None)
    return out


def vertex_endpoints() -> list[dict[str, Any]]:
    from vibe.core.llm.backend.vertex import build_vertex_base_url, build_vertex_endpoint

    out = []
    for region in ("global", "europe-west1", "us-east5"):
        for streaming in (True, False):
            out.append({
                "region": region,
                "streaming": streaming,
                "url": build_vertex_base_url(region)
                + build_vertex_endpoint(region, "proj", "claude-x@1", streaming=streaming),
            })
    return out


async def capture_scenarios(scenarios: list[dict[str, Any]]) -> list[dict[str, Any]]:
    out = []
    for entry in scenarios:
        observed = await run_scenario(entry)
        out.append({**entry, "observed": observed})
    return out


def all_scenarios() -> list[dict[str, Any]]:
    return wire_scenarios() + parse_scenarios() + error_scenarios() + retry_scenarios()


def capture(reference: dict[str, str]) -> dict[str, Any]:
    import logging

    logging.disable(logging.CRITICAL)
    scenarios = asyncio.run(capture_scenarios(all_scenarios()))
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "scenarios": scenarios,
        "retryDelays": retry_delays(),
        "utilitySelections": utility_selections(),
        "vertexEndpoints": vertex_endpoints(),
    }


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument("--corpus", type=Path, nargs="?", const=DEFAULT_CORPUS, default=None)
    parser.add_argument("--output", type=Path, default=None,
                        help="write the capture here instead of the committed corpus")
    parser.add_argument("--check", action="store_true",
                        help="recapture and fail when the committed corpus differs")
    parser.add_argument("--only", default=None, help="capture only scenarios whose name starts so")
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(arguments.reference, reference["commit"], arguments.cache)
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        if arguments.only:
            global all_scenarios  # noqa: PLW0603
            everything = all_scenarios
            all_scenarios = lambda: [s for s in everything() if s["name"].startswith(arguments.only)]  # noqa: E731
        corpus = capture(reference)
    except OracleError as error:
        print(f"llm backend capture failed: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps(corpus, indent=1, sort_keys=True, ensure_ascii=False) + "\n"
    if arguments.check:
        committed = DEFAULT_CORPUS.read_text(encoding="utf-8")
        if committed != rendered:
            print("the committed corpus differs from a fresh capture", file=sys.stderr)
            return 1
        print(f"corpus matches a fresh capture of {len(corpus['scenarios'])} scenarios")
        return 0
    target = arguments.output or arguments.corpus
    if target is None:
        sys.stdout.write(rendered)
        return 0
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(rendered, encoding="utf-8")
    print(f"captured {len(corpus['scenarios'])} scenarios from {reference['commit'][:12]} into {target}")
    print(f"python {platform.python_version()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
