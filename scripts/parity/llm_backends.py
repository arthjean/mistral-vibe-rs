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
import copy
import hashlib
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

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned  # noqa: E402
from stand_in import Handler, Loopback  # noqa: E402
from llm_backends_scenarios import (  # noqa: E402
    BIG,
    CLIENT_KEY_VARIABLE,
    FAST_CANDIDATES,
    KEY,
    KEY_VARIABLE,
    LOCAL,
    MEDIUM,
    OPENAI,
    PROBE_NOW,
    PUBLIC,
    SESSION_ID,
    UTILITY_SYSTEM,
    UTILITY_USER,
    all_scenarios,
    completion_cases,
    probe_cases,
    provider_entry,
)

SCHEMA_VERSION = 3
DEFAULT_CORPUS = Path("crates/vibe-core/tests/llm-backends/corpus.json")

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


class StandIn(Loopback):
    """Serves scripted responses in order and records every request."""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = copy.deepcopy(responses)
        #: Everything the scenario scripted, which is what may be recorded verbatim.
        self.served = copy.deepcopy(responses)
        self.requests: list[dict[str, Any]] = []
        self.lock = threading.Lock()
        stand_in = self

        class Scripted(Handler):
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

        super().__init__(Scripted)


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
            UTILITY_MODEL_CACHE_FILE.path.write_text(entry["rawFile"], encoding="utf-8", newline="")
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
        commit, _ = reexecute_pinned(
            arguments.reference,
            arguments.expected_commit,
            cache=arguments.cache,
            override=arguments.python,
        )
        reference = {"commit": commit, "path": str(arguments.reference)}
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
    target.write_text(rendered, encoding="utf-8", newline="")
    print(f"captured {len(corpus['scenarios'])} scenarios from {reference['commit'][:12]} into {target}")
    print(f"python {platform.python_version()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
