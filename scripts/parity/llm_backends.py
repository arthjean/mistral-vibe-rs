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
arithmetic, ``utilitySelections`` for the utility model choice over the
availability verdicts a probe would have left, and ``vertexEndpoints`` for the
regional Vertex addresses. ``availabilityKeys`` records the key a verdict is
cached under, and ``availabilityProbes`` drives the fast-model probe a session
runs before its first title (``ensure_utility_models_probed``) against the
stand-in, round after round, on a hand-moved clock: the requests it sent, the
selection it left and the cache file byte for byte. ``utilityCompletions``
runs one background completion (``run_utility_completion``) per case against
the stand-in: the request it sent, with the label and attribution its metadata
carries, the ``vibe.request_sent`` payload it reported and the content it
answered.

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

SCHEMA_VERSION = 3
DEFAULT_CORPUS = Path("crates/vibe-core/tests/llm-backends/corpus.json")
DEFAULT_CACHE = Path(".parity")

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
    # Since v2.26.0 a completion renames a tool call id the session already
    # holds (`AgentLoop._unique_tool_call_ids`).
    fake._seen_tool_call_ids = set()
    fake._unique_tool_call_ids = lambda message, renamed: AgentLoop._unique_tool_call_ids(
        fake, message, renamed
    )
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
    for variable in (KEY_VARIABLE, CLIENT_KEY_VARIABLE):
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


# --------------------------------------------------------------------------
# The utility model and the availability probe
# --------------------------------------------------------------------------

#: The two names the fast utility model is asked for under, in preference order
#: (reference ``FAST_MODEL_CANDIDATES``).
FAST_CANDIDATES = ("mistral-vibe-cli-fast", "mistral-small-latest")
#: The wall clock the availability cache reads, frozen so ages are exact.
PROBE_NOW = 1_800_000_000
#: The probe budget every probe case runs with unless it names another.
PROBE_BUDGET = 1.0


class ProbeClock:
    """The wall and monotonic clocks ``model_probe`` reads, moved by hand.

    The probe's own deadline is the event loop's, which stays real, so a slow
    answer still exhausts a budget measured in real seconds.
    """

    def __init__(self) -> None:
        self.wall = float(PROBE_NOW)
        self.mono = 5_000.0

    def time(self) -> float:
        return self.wall

    def monotonic(self) -> float:
        return self.mono

    @staticmethod
    def perf_counter() -> float:
        return time.perf_counter()

    def advance(self, seconds: float) -> None:
        self.wall += seconds
        self.mono += seconds


def isolate_availability(home: Path) -> ProbeClock:
    """A fresh vibe home, an empty availability cache, no keyring and a hand clock."""

    import vibe.utils.api_keys as api_keys
    from vibe.core.llm import model_probe

    os.environ["VIBE_HOME"] = str(home)
    for variable in (KEY_VARIABLE, CLIENT_KEY_VARIABLE, "VIBE_TEST_DISABLE_MODEL_PROBE"):
        os.environ.pop(variable, None)
    api_keys.get_api_key_from_keyring = lambda _variable: None  # type: ignore[assignment]
    clock = ProbeClock()
    model_probe.time = clock  # type: ignore[attr-defined]
    model_probe.MODEL_AVAILABILITY.reset()
    return clock


def utility_config(providers: list[Any], active: Any, allowed: list[str],
                   models: list[Any] | None = None, utility: dict[str, str] | None = None) -> Any:
    """The slice of ``VibeConfigSchema`` utility selection reads.

    ``get_mistral_provider``, ``available_models`` and ``get_utility_model``
    are the reference's own methods bound to this namespace, over the active
    model and ``models`` keyed by alias, and the ``[utility_models]`` table
    ``utility``; the allowlist is never an administrator's.
    """

    import types

    from vibe.core.config import VibeConfigSchema
    from vibe.core.config.models import UtilityModelsConfig

    def get_active_model() -> Any:
        if active is None:
            raise ValueError("Active model is not configured.")
        return active

    def get_provider_for_model(model_config: Any) -> Any:
        found = next((p for p in providers if p.name == model_config.provider), None)
        if found is None:
            raise ValueError(f"Provider '{model_config.provider}' is not configured.")
        return found

    config = SimpleNamespace(
        providers=providers,
        allowed_models=allowed,
        models={m.alias: m for m in ([active] if active else []) + list(models or [])},
        utility_models=UtilityModelsConfig(**(utility or {})),
        origin_of=lambda _key: "user",
        get_active_model=get_active_model,
        get_provider_for_model=get_provider_for_model,
        get_active_provider=lambda: get_provider_for_model(get_active_model()),
    )
    for method in ("get_mistral_provider", "available_models", "get_utility_model"):
        setattr(config, method, types.MethodType(getattr(VibeConfigSchema, method), config))
    return config


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


def selection_record(config: Any, feature: Any = None) -> dict[str, Any]:
    from vibe.core.llm import utility_completion

    try:
        chosen, chosen_provider = utility_completion.select_utility_model(config, feature=feature)
    except ValueError:
        return {"error": True}
    return {
        "model": chosen.name,
        "alias": chosen.alias,
        "provider": chosen_provider.name,
        "fast": utility_completion.is_fast_utility_model(config, feature=feature),
        "temperature": chosen.temperature,
    }


def utility_selections(home: Path) -> list[dict[str, Any]]:
    """Which model and provider a background nicety runs on.

    ``verdicts`` are remembered in the availability cache first, under the
    Mistral provider the configuration resolves, as a probe would have left them.
    """

    from vibe.core.config import ModelConfig, ProviderConfig
    from vibe.core.llm.model_probe import MODEL_AVAILABILITY

    key = {KEY_VARIABLE: KEY}
    second_eu = provider_entry("mistral-eu", "http://127.0.0.1:2/v1", "")
    eu_model = {"name": "small-eu", "provider": "mistral-eu", "alias": "small-eu"}

    def public(api_base: str) -> dict[str, Any]:
        return {**PUBLIC, "api_base": api_base}

    first, second = FAST_CANDIDATES
    cases: list[tuple[str, list[dict[str, Any]], Any, list[str], dict[str, str], dict[str, bool]]] = [
        ("mistral-with-key", [OPENAI, PUBLIC], BIG, [], key, {}),
        ("mistral-without-key", [OPENAI, PUBLIC], BIG, [], {}, {}),
        ("no-mistral-provider", [OPENAI], BIG, [], key, {}),
        ("keyless-mistral", [OPENAI, LOCAL], BIG, [], {}, {}),
        ("allowlist-excludes-fast", [OPENAI, PUBLIC], BIG, ["big"], key, {}),
        ("allowlist-admits-fast", [OPENAI, PUBLIC], BIG, ["mistral-*"], key, {}),
        ("allowlist-alias-only", [OPENAI, PUBLIC], BIG, ["mistral-small"], key, {}),
        ("allowlist-fast-name", [OPENAI, PUBLIC], BIG, [first], key, {}),
        ("allowlist-case-insensitive", [OPENAI, PUBLIC], BIG, [first.upper()], key, {}),
        ("allowlist-regex", [OPENAI, PUBLIC], BIG, ["re:mistral-vibe-.*"], key, {}),
        ("allowlist-second-unprobed", [OPENAI, PUBLIC], BIG, [second], key, {}),
        ("allowlist-second-served", [OPENAI, PUBLIC], BIG, [second], key, {second: True}),
        ("public-first-refused", [OPENAI, PUBLIC], BIG, [], key, {first: False}),
        ("public-first-refused-second-served", [OPENAI, PUBLIC], BIG, [], key,
         {first: False, second: True}),
        ("public-second-served-first-unknown", [OPENAI, PUBLIC], BIG, [], key, {second: True}),
        ("public-both-refused", [OPENAI, PUBLIC], BIG, [], key, {first: False, second: False}),
        ("keyless-first-served", [OPENAI, LOCAL], BIG, [], {}, {first: True}),
        ("keyless-second-served", [OPENAI, LOCAL], BIG, [], {}, {second: True}),
        ("keyless-first-refused-second-served", [OPENAI, LOCAL], BIG, [], {},
         {first: False, second: True}),
        ("keyless-both-refused", [OPENAI, LOCAL], BIG, [], {}, {first: False, second: False}),
        ("public-explicit-port", [OPENAI, public("https://api.mistral.ai:443/v1")], BIG, [], key, {}),
        ("public-uppercase", [OPENAI, public("HTTPS://API.Mistral.AI/v1")], BIG, [], key, {}),
        ("public-other-port", [OPENAI, public("https://api.mistral.ai:8443/v1")], BIG, [], key, {}),
        ("public-plain-http", [OPENAI, public("http://api.mistral.ai/v1")], BIG, [], key, {}),
        ("public-http-port-443", [OPENAI, public("http://api.mistral.ai:443/v1")], BIG, [], key, {}),
        ("public-invalid-port", [OPENAI, public("https://api.mistral.ai:99999/v1")], BIG, [], key, {}),
        ("public-empty-port", [OPENAI, public("https://api.mistral.ai:/v1")], BIG, [], key, {}),
        ("public-bare-origin", [OPENAI, public("https://api.mistral.ai")], BIG, [], key, {}),
        ("public-other-host", [OPENAI, public("https://eu.api.mistral.ai/v1")], BIG, [], key, {}),
        ("active-on-mistral", [OPENAI, PUBLIC], MEDIUM, [], key, {}),
        ("active-on-second-mistral", [OPENAI, PUBLIC, second_eu], eu_model, [], key, {}),
        ("active-on-second-mistral-served", [OPENAI, PUBLIC, second_eu], eu_model, [], key,
         {first: True}),
        ("unknown-active-with-fast", [OPENAI, PUBLIC], None, [], key, {}),
        ("unknown-active-without-fast", [OPENAI, PUBLIC], None, [], {}, {}),
        ("unknown-provider-with-fast", [PUBLIC], BIG, [], key, {}),
    ]
    small = {"name": "small-model", "provider": "openai", "alias": "small"}
    fast_named = {"name": second, "provider": "mistral", "alias": "fast-latest"}
    aliased_active = {"name": "aliased-active", "provider": "openai", "alias": "active"}
    orphan = {"name": "orphan-model", "provider": "gone", "alias": "orphan"}
    overrides: list[tuple[str, list[dict[str, Any]], Any, list[str], dict[str, str], list[Any],
                          dict[str, str], str | None]] = [
        ("override-title", [OPENAI, PUBLIC], BIG, [], key, [small], {"title": "small"}, "title"),
        ("override-title-padded", [OPENAI, PUBLIC], BIG, [], key, [small], {"title": "  small "},
         "title"),
        ("override-title-fast-name", [OPENAI, PUBLIC], BIG, [], key, [fast_named],
         {"title": "fast-latest"}, "title"),
        ("override-title-active-selector", [OPENAI, PUBLIC], BIG, [], key, [small],
         {"title": "active"}, "title"),
        ("override-title-aliased-active", [OPENAI, PUBLIC], BIG, [], key, [aliased_active],
         {"title": "active"}, "title"),
        ("override-title-unknown", [OPENAI, PUBLIC], BIG, [], key, [small], {"title": "nope"}, "title"),
        ("override-title-excluded", [OPENAI, PUBLIC], BIG, ["big"], key, [small], {"title": "small"},
         "title"),
        ("override-title-allowlist-admits-nothing", [OPENAI, PUBLIC], BIG, ["zzz"], key, [small],
         {"title": "small"}, "title"),
        ("override-smart-approve-only", [OPENAI, PUBLIC], BIG, [], key, [small],
         {"smart_approve": "small"}, "title"),
        ("override-smart-approve", [OPENAI, PUBLIC], BIG, [], key, [small],
         {"smart_approve": "small"}, "smart_approve"),
        ("override-without-feature", [OPENAI, PUBLIC], BIG, [], key, [small], {"title": "small"}, None),
        ("override-unknown-provider", [OPENAI, PUBLIC], BIG, [], key, [orphan], {"title": "orphan"},
         "title"),
        ("override-active-selector-without-active", [OPENAI, PUBLIC], None, [], key, [small],
         {"title": "active"}, "title"),
        ("no-override-for-title", [OPENAI, PUBLIC], BIG, [], key, [small], {}, "title"),
    ]
    every = [(n, e, a, al, env, v, [], {}, None) for n, e, a, al, env, v in cases] + [
        (n, e, a, al, env, {}, m, u, f) for n, e, a, al, env, m, u, f in overrides
    ]
    out: list[dict[str, Any]] = []
    for index, (name, entries, active_entry, allowed, env, verdicts, extra, utility, feature) in (
        enumerate(every)
    ):
        from vibe.core.config import UtilityFeature

        isolate_availability(home / f"selection-{index}")
        os.environ.pop(KEY_VARIABLE, None)
        os.environ.update(env)
        providers = [ProviderConfig.model_validate(entry) for entry in entries]
        active = ModelConfig.model_validate(active_entry) if active_entry else None
        config = utility_config(providers, active, allowed,
                                [ModelConfig.model_validate(m) for m in extra], utility)
        candidates = {c.name: c for c in __import__(
            "vibe.core.llm.utility_completion", fromlist=["FAST_MODEL_CANDIDATES"]
        ).FAST_MODEL_CANDIDATES}
        mistral = config.get_mistral_provider()
        for model_name, available in verdicts.items():
            MODEL_AVAILABILITY.remember(provider=mistral, model=candidates[model_name],
                                        available=available)
        out.append({
            "name": name,
            "active": active_entry,
            "providers": entries,
            "allowedModels": allowed,
            "env": sorted(env),
            "verdicts": verdicts,
            "models": extra,
            "utilityModels": utility,
            "feature": feature,
            **selection_record(config, UtilityFeature(feature) if feature else None),
        })
    os.environ.pop(KEY_VARIABLE, None)
    return out


def availability_keys(home: Path) -> list[dict[str, Any]]:
    """The cache key a verdict is stored under, per endpoint, backend, model and key."""

    from vibe.core.config import ModelConfig, ProviderConfig
    from vibe.core.llm.model_probe import _cache_key

    isolate_availability(home / "keys")
    cases = [
        ("https://api.mistral.ai/v1", "mistral", FAST_CANDIDATES[0], KEY),
        ("https://api.mistral.ai/v1/", "mistral", FAST_CANDIDATES[0], KEY),
        ("https://api.mistral.ai/v1///", "mistral", FAST_CANDIDATES[0], KEY),
        ("https://api.mistral.ai/v1", "mistral", FAST_CANDIDATES[1], KEY),
        ("https://api.mistral.ai/v1", "mistral", FAST_CANDIDATES[0], "another-key"),
        ("http://127.0.0.1:8080/v1", "mistral", FAST_CANDIDATES[0], None),
        ("https://example.test/v1", "generic", FAST_CANDIDATES[1], KEY),
        ("https://exämple.test/v1", "mistral", FAST_CANDIDATES[0], "kéy"),
    ]
    out = []
    for api_base, backend, model_name, credential in cases:
        os.environ.pop(KEY_VARIABLE, None)
        if credential is not None:
            os.environ[KEY_VARIABLE] = credential
        provider = ProviderConfig.model_validate(provider_entry(
            "p", api_base, KEY_VARIABLE if credential is not None else "", backend))
        model_config = ModelConfig(name=model_name, provider="p", alias=model_name)
        out.append({
            "apiBase": api_base,
            "backend": backend,
            "model": model_name,
            "credential": credential,
            "key": _cache_key(provider=provider, model=model_config),
        })
    os.environ.pop(KEY_VARIABLE, None)
    return out


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


def candidate_labels(config: Any) -> dict[str, str]:
    from vibe.core.llm.model_probe import _cache_key
    from vibe.core.llm.utility_completion import FAST_MODEL_CANDIDATES

    mistral = config.get_mistral_provider()
    if mistral is None:
        return {}
    return {_cache_key(provider=mistral, model=c): c.name for c in FAST_MODEL_CANDIDATES}


def write_seed(config: Any, seed: list[dict[str, Any]], now: float) -> None:
    from vibe.core.llm.model_probe import _read_entries, _write_entries

    labels = {name: key for key, name in candidate_labels(config).items()}
    entries = _read_entries()
    for item in seed:
        if "raw" in item:
            key = labels.get(FAST_CANDIDATES[0], "") if item["raw"] == "$FIRST" else item["raw"]
            # In the order the committed corpus keeps it, which sorts keys, so
            # a replay seeds the same bytes.
            entries[key] = json.loads(json.dumps(item["value"], sort_keys=True))
        else:
            entries[labels[item["model"]]] = {
                "available": item["available"],
                "stored_at_timestamp": int(now) - item["age"],
            }
    _write_entries(entries)


def file_record(config: Any) -> Any:
    """The cache file byte for byte, with the keys the case's own provider gives
    each candidate written as the candidate's name: they hash the stand-in's
    address, which changes every run."""

    from vibe.core.paths import UTILITY_MODEL_CACHE_FILE

    path = UTILITY_MODEL_CACHE_FILE.path
    if not path.exists():
        return None
    text = path.read_text(encoding="utf-8")
    for key, name in candidate_labels(config).items():
        text = text.replace(key, f"<{name}>")
    return text


def normalize_platform(requests: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The platform a request names is the capturing machine's, not the backend's."""

    for request in requests:
        metadata = (request.get("body") or {}).get("metadata")
        if isinstance(metadata, dict):
            for field in ("os", "arch", "os_version"):
                if field in metadata:
                    metadata[field] = f"<{field}>"
    return requests


async def run_probe_case(entry: dict[str, Any], home: Path) -> dict[str, Any]:
    from vibe import __version__
    from vibe.core.config import ModelConfig, ProviderConfig, UtilityFeature
    from vibe.core.llm.utility_completion import ensure_utility_models_probed

    clock = isolate_availability(home)
    install_clock(FakeClock())
    stand_in = StandIn(entry["responses"])
    closed = f"http://127.0.0.1:{closed_port()}"
    os.environ.pop(KEY_VARIABLE, None)
    os.environ.update(entry["env"])
    try:
        providers = [ProviderConfig.model_validate(substitute(p, stand_in.base, closed))
                     for p in entry["providers"]]
        active = ModelConfig.model_validate(entry["active"]) if entry["active"] else None
        config = utility_config(providers, active, entry["allowedModels"],
                                [ModelConfig.model_validate(m) for m in entry["models"]],
                                entry["utilityModels"])
        if entry["rawFile"] is not None:
            from vibe.core.paths import UTILITY_MODEL_CACHE_FILE

            UTILITY_MODEL_CACHE_FILE.path.parent.mkdir(parents=True, exist_ok=True)
            UTILITY_MODEL_CACHE_FILE.path.write_text(entry["rawFile"], encoding="utf-8")
        if entry["seed"]:
            write_seed(config, entry["seed"], clock.wall)
        rounds = []
        for round_entry in entry["rounds"]:
            clock.advance(round_entry["advance"])
            if round_entry["seed"]:
                write_seed(config, round_entry["seed"], clock.wall)
            if round_entry["disabled"]:
                os.environ["VIBE_TEST_DISABLE_MODEL_PROBE"] = "1"
            before = len(stand_in.requests)
            features = tuple(UtilityFeature(f) for f in round_entry["features"])
            await ensure_utility_models_probed(config, features=features,
                                               timeout_seconds=round_entry["budget"])
            os.environ.pop("VIBE_TEST_DISABLE_MODEL_PROBE", None)
            rounds.append({
                "requests": normalize_platform(copy.deepcopy(stand_in.requests[before:])),
                "selection": selection_record(config, UtilityFeature.TITLE),
                "file": file_record(config),
            })
    finally:
        stand_in.close()
        for variable in (KEY_VARIABLE, CLIENT_KEY_VARIABLE):
            os.environ.pop(variable, None)
    observed = json.loads(json.dumps(rounds).replace(closed, "$CLOSED"))
    return normalize_text(observed, stand_in.base, __version__)


async def capture_probes(home: Path) -> list[dict[str, Any]]:
    out = []
    for index, entry in enumerate(probe_cases()):
        observed = await run_probe_case(entry, home / f"probe-{index}")
        out.append({**entry, "observed": observed})
    return out


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


class RecordingTelemetry:
    """Records what the reference's ``send_request_sent`` would have sent."""

    def __init__(self) -> None:
        self.events: list[dict[str, Any]] = []

    def send_telemetry_event(self, event_name: str, properties: dict[str, Any], *,
                             correlation_id: str | None = None) -> None:
        self.events.append({"event": event_name, "properties": properties,
                            "correlationId": correlation_id})


async def run_completion_case(entry: dict[str, Any], home: Path) -> dict[str, Any]:
    from vibe import __version__
    from vibe.core.config import ModelConfig, ProviderConfig, UtilityFeature
    from vibe.core.llm.utility_completion import run_utility_completion
    from vibe.core.telemetry.send import TelemetryClient
    from vibe.core.telemetry.types import LaunchContext

    RecordingTelemetry.send_request_sent = TelemetryClient.send_request_sent  # type: ignore[attr-defined]
    isolate_availability(home)
    install_clock(FakeClock())
    stand_in = StandIn(entry["responses"])
    os.environ.update(entry["env"])
    telemetry = RecordingTelemetry() if entry["telemetry"] else None
    try:
        providers = [ProviderConfig.model_validate(substitute(p, stand_in.base, ""))
                     for p in entry["providers"]]
        config = utility_config(providers, ModelConfig.model_validate(entry["active"]),
                                entry["allowedModels"],
                                [ModelConfig.model_validate(m) for m in entry["models"]],
                                entry["utilityModels"])
        try:
            content: Any = await run_utility_completion(
                config=config,
                system_prompt=UTILITY_SYSTEM,
                user_content=UTILITY_USER,
                max_tokens=24,
                request_timeout_seconds=5.0,
                retry_budget_seconds=0,
                feature=UtilityFeature(entry["feature"]) if entry["feature"] else None,
                skip_if_no_key=entry["skipIfNoKey"],
                call_type=entry["callType"],
                launch_context=LaunchContext.model_validate(entry["launch"])
                if entry["launch"] else None,
                session_id=entry["session"],
                telemetry=telemetry,  # type: ignore[arg-type]
            )
        except Exception as error:  # noqa: BLE001 - the class is the observation
            content = {"error": type(error).__name__}
    finally:
        stand_in.close()
        for variable in (KEY_VARIABLE, CLIENT_KEY_VARIABLE):
            os.environ.pop(variable, None)
    observed = {
        "content": content,
        "requests": normalize_platform(copy.deepcopy(stand_in.requests)),
        "telemetry": telemetry.events if telemetry is not None else None,
    }
    return normalize_text(observed, stand_in.base, __version__)


async def capture_completions(home: Path) -> list[dict[str, Any]]:
    out = []
    for index, entry in enumerate(completion_cases()):
        observed = await run_completion_case(entry, home / f"completion-{index}")
        out.append({**entry, "observed": observed})
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
    import tempfile

    scenarios = asyncio.run(capture_scenarios(all_scenarios()))
    with tempfile.TemporaryDirectory(prefix="llm-backends-") as scratch:
        home = Path(scratch)
        selections = utility_selections(home)
        keys = availability_keys(home)
        probes = asyncio.run(capture_probes(home))
        completions = asyncio.run(capture_completions(home))
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "scenarios": scenarios,
        "retryDelays": retry_delays(),
        "utilitySelections": selections,
        "availabilityKeys": keys,
        "availabilityProbes": probes,
        "utilityCompletions": completions,
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
