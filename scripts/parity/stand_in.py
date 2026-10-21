"""The loopback HTTP stand-ins a capture scripts its answers on.

A capture points the program it measures at a server of its own on the
loopback interface. This module holds what those servers share: the quiet
handler every stand-in answers with (:class:`Handler`), a server on a free port
running on a thread of its own (:class:`Loopback`), the chat-completions
stand-in most captures put behind a session (:class:`Backend`), and the
completion and the stream of chunks a scripted response is served as
(:func:`completion`, :func:`chunks`). Nothing here imports the reference.
"""

from __future__ import annotations

from collections.abc import Callable
import http.server
import json
import sys
import threading
import time
from typing import Any


#: What the stand-in answers a fast-model probe with. Since v2.26.0 a root
#: session opened with titles on first asks the provider whether it serves a
#: fast model, with a one-token completion that offers no tools (reference
#: ``vibe/core/llm/model_probe.py:189-211``).
PROBE_ANSWER = {"text": "Available."}


def is_model_probe(body: dict[str, Any]) -> bool:
    return body.get("max_tokens") == 1 and not body.get("tools")


def is_utility(body: dict[str, Any]) -> bool:
    """Whether a request is a utility call (a title, a summary, a probe) rather
    than a model step of the conversation, which always offers tools."""

    return not body.get("tools")


def requested(body: dict[str, Any]) -> bool:
    """Streams an answer when the request asks for one."""

    return bool(body.get("stream"))


def always(_body: dict[str, Any]) -> bool:
    """Streams every answer, whatever the request asked for."""

    return True


# --------------------------------------------------------------------------
# The server
# --------------------------------------------------------------------------


class Handler(http.server.BaseHTTPRequestHandler):
    """Answers over HTTP/1.1 without logging, in JSON or as a chunk stream."""

    protocol_version = "HTTP/1.1"

    def log_message(self, *_: Any) -> None:
        return

    def read_json(self) -> Any:
        """The request's JSON body, an empty object when it has none or a
        malformed one."""

        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            return json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return {}

    def reply(self, status: int, body: Any) -> None:
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def stream(self, events: list[dict[str, Any]]) -> None:
        payload = b"".join(
            b"data: " + json.dumps(event).encode() + b"\n\n" for event in events
        ) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class Server(http.server.ThreadingHTTPServer):
    def handle_error(self, request: Any, client_address: Any) -> None:
        # A client interrupted mid-answer, or one that gave up on a delayed
        # answer, hangs up; that is the scenario, not a failure of the stand-in.
        if isinstance(sys.exc_info()[1], OSError):
            return
        super().handle_error(request, client_address)


class Loopback:
    """A server on a free loopback port, answering with ``handler`` on a
    thread of its own."""

    def __init__(self, handler: type[Handler]) -> None:
        self.server = Server(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


# --------------------------------------------------------------------------
# The chat-completions stand-in
# --------------------------------------------------------------------------


class Backend(Loopback):
    """Serves scripted completions in order, then a fixed closing answer, and
    keeps the body of every request it was sent in ``bodies``.

    An entry of ``responses`` may carry ``text`` (a string or its chunks),
    ``reasoning``, ``toolCalls``, ``promptTokens``, ``completionTokens``,
    ``cachedTokens``, a ``finish`` reason, a ``delay`` in seconds, or a
    ``status`` and ``body`` to fail with. An answer is a stream of chunks when
    ``stream`` says so of the request, and one completion otherwise. A stream
    reports the token counts its entry scripted; one completion reports them
    only with ``scripted_usage``, and the default counts otherwise, which is
    what the app-server corpora were captured with.

    With ``utility``, a request that offers no tools (a title, a summary) takes
    the next entry of ``utility`` instead, so a title generated in the
    background never consumes the answer the next step was scripted with. With
    ``probe``, a fast-model probe takes :data:`PROBE_ANSWER` and consumes no
    entry at all.

    :meth:`get` answers a GET, :meth:`post` a POST to any other path and
    :meth:`delete` a DELETE, all with a 404, and :meth:`keep` records a
    chat-completions request; a stand-in that plays more endpoints overrides
    them.
    """

    def __init__(
        self,
        responses: list[dict[str, Any]] | None = None,
        *,
        stream: Callable[[dict[str, Any]], bool] = requested,
        utility: bool = False,
        probe: bool = False,
        scripted_usage: bool = False,
    ) -> None:
        self.responses: list[dict[str, Any]] = list(responses or [])
        self.utility: list[dict[str, Any]] = []
        self.bodies: list[Any] = []
        self.streams = stream
        self.routes_utility = utility
        self.answers_probe = probe
        self.scripted_usage = scripted_usage
        self.lock = threading.Lock()
        backend = self

        class ChatHandler(Handler):
            def do_GET(self) -> None:  # noqa: N802
                backend.get(self, self.path.split("?", 1)[0].rstrip("/"))

            def do_POST(self) -> None:  # noqa: N802
                body = self.read_json()
                path = self.path.split("?", 1)[0].rstrip("/")
                if path.endswith("/chat/completions"):
                    backend.complete(self, body)
                else:
                    backend.post(self, path, body)

            def do_DELETE(self) -> None:  # noqa: N802
                backend.delete(self, self.path.split("?", 1)[0].rstrip("/"))

        super().__init__(ChatHandler)

    def get(self, handler: Handler, path: str) -> None:
        handler.reply(404, {"error": "not found"})

    def post(self, handler: Handler, path: str, body: Any) -> None:
        handler.reply(404, {"error": "not found"})

    def delete(self, handler: Handler, path: str) -> None:
        handler.reply(404, {"error": "not found"})

    def keep(self, body: Any) -> None:
        """Records one chat-completions request; called under the lock."""

        self.bodies.append(body)

    def complete(self, handler: Handler, body: Any) -> None:
        with self.lock:
            self.keep(body)
            if self.answers_probe and is_model_probe(body):
                response = PROBE_ANSWER
            elif self.routes_utility and is_utility(body):
                response = self.utility.pop(0) if self.utility else {"text": "Oracle title"}
            else:
                response = self.responses.pop(0) if self.responses else {"text": "Done."}
        if response.get("delay"):
            time.sleep(float(response["delay"]))
        if response.get("status"):
            handler.reply(response["status"], response.get("body", {}))
            return
        model = str(body.get("model", "model"))
        if self.streams(body):
            handler.stream(chunks(response, model))
            return
        answer = completion(response, model)
        if not self.scripted_usage:
            answer["usage"] = usage({})
        handler.reply(200, answer)


def usage(response: dict[str, Any]) -> dict[str, Any]:
    prompt = response.get("promptTokens", 10)
    completion_tokens = response.get("completionTokens", 5)
    body: dict[str, Any] = {
        "prompt_tokens": prompt,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt + completion_tokens,
    }
    if "cachedTokens" in response:
        body["prompt_tokens_details"] = {"cached_tokens": response["cachedTokens"]}
    return body


def tool_calls(response: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        {
            "id": call.get("id", f"call_{index}"),
            "type": "function",
            "index": index,
            "function": {
                "name": call["name"],
                "arguments": call["raw"] if "raw" in call else json.dumps(call.get("arguments", {})),
            },
        }
        for index, call in enumerate(response.get("toolCalls", []))
    ]


def text_pieces(response: dict[str, Any]) -> list[str]:
    text = response.get("text")
    if isinstance(text, list):
        return text
    return [text] if text else []


def finish_reason(response: dict[str, Any]) -> str:
    return "tool_calls" if response.get("toolCalls") else response.get("finish", "stop")


def chunks(response: dict[str, Any], model: str) -> list[dict[str, Any]]:
    """The stream a scripted response is served as, one chunk per delta."""

    served: list[dict[str, Any]] = []

    def chunk(delta: dict[str, Any], finish: str | None = None, with_usage: bool = False) -> None:
        body: dict[str, Any] = {
            "id": "cmpl-oracle",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }
        if with_usage:
            body["usage"] = usage(response)
        served.append(body)

    chunk({"role": "assistant", "content": ""})
    if response.get("reasoning"):
        # Mistral streams reasoning as a thinking chunk.
        chunk({"content": [{"type": "thinking", "thinking": [
            {"type": "text", "text": response["reasoning"]}]}]})
    for piece in text_pieces(response):
        chunk({"content": piece})
    for call in tool_calls(response):
        chunk({"tool_calls": [call]})
    chunk({"content": ""}, finish=finish_reason(response), with_usage=True)
    return served


def completion(response: dict[str, Any], model: str) -> dict[str, Any]:
    """The one completion a scripted response is served as."""

    message: dict[str, Any] = {"role": "assistant", "content": "".join(text_pieces(response))}
    if response.get("toolCalls"):
        message["tool_calls"] = tool_calls(response)
    return {
        "id": "cmpl-oracle",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason(response)}],
        "usage": usage(response),
    }
