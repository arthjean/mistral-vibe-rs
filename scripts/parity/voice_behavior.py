"""The behavior families of the voice oracle.

`voice.py` captures what an audio session is resolved from; this module
captures what the reference then does with it, with every device, socket and
model endpoint replaced by a scripted stand-in this module authors:

``captureSignal``     the recorder's level meter over authored sample blocks
``audioDevices``      what the recorder and the player ask miniaudio to open
``decodeWav``         what the player decodes out of authored WAV payloads
``audioMetadata``     the request metadata the terminal attaches to audio calls
``transcribeStream``  the realtime client over a scripted websocket: the
                      frames it sends, the headers it opens with and the
                      events it yields
``voiceManager``      the dictation lifecycle over a scripted recorder and a
                      scripted client: listener callbacks, start refusals and
                      telemetry
``narrator``          the read-aloud lifecycle over a scripted summary, speech
                      client and player: states, speech requests and telemetry
``narrationService``  the turn summary request the app server makes, taken at
                      the backend seam
``acpBridge``         the editor's voice extension over the same stand-ins:
                      what each call answers and which notifications it sends

Every text the reference authors (an error, a notice, a prompt) is recorded as
its length and SHA-256 only. Texts this module authors are recorded as they
are, because the replay has to hand the same ones to this port.

The module is imported by `voice.py` after it re-executed itself under the
reference interpreter, so the reference's packages import here.
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import io
import json
import struct
from types import SimpleNamespace
from typing import Any
import wave

#: What a version string is replaced with, so the corpus survives a release.
VERSION_MARK = "<version>"
#: What a credential is replaced with in a recorded header.
CREDENTIAL_MARK = "<credential>"
#: The request identifier every scripted session announces.
REQUEST_ID = "fixture-request"
#: How many loop turns a step is given to settle before it is observed.
SETTLE_TURNS = 50


def prose(text: str) -> dict[str, Any]:
    """A reference-authored text, recorded without its words."""

    return {"prose": len(text), "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()}


async def settle() -> None:
    for _ in range(SETTLE_TURNS):
        await asyncio.sleep(0)


def _version() -> str:
    from vibe import __version__

    return __version__


def _normalize(text: str, sentinel: str) -> str:
    return text.replace(sentinel, CREDENTIAL_MARK).replace(_version(), VERSION_MARK)


# --------------------------------------------------------------------------
# The recorder's level meter
# --------------------------------------------------------------------------

SIGNAL_CASES: list[dict[str, Any]] = [
    {"case": "silence", "blocks": [[0, 0, 0, 0]]},
    {"case": "below-floor", "blocks": [[32, -32]]},
    {"case": "above-floor", "blocks": [[33]]},
    {"case": "full-scale", "blocks": [[32767]]},
    {"case": "negative-full-scale", "blocks": [[-32768, 0]]},
    {"case": "half-scale", "blocks": [[-16384, 100, 200]]},
    {"case": "signal-is-sticky", "blocks": [[20000], [10], [0]]},
    {"case": "empty-block-keeps-the-peak", "blocks": [[5000], []]},
    {"case": "rising", "blocks": [[1], [100], [1000], [10000]]},
]


def capture_signal() -> list[dict[str, Any]]:
    from vibe.cli.audio_recorder.audio_recorder import AudioRecorder
    from vibe.utils.audio import RecordingMode

    records = []
    for case in SIGNAL_CASES:
        recorder = AudioRecorder()
        recorder._mode = RecordingMode.STREAM  # type: ignore[attr-defined]
        readings = []
        for block in case["blocks"]:
            recorder._process_audio(struct.pack(f"<{len(block)}h", *block))  # type: ignore[attr-defined]
            readings.append({"peak": round(recorder.peak, 6), "hasSignal": recorder.has_signal})
        records.append({**case, "readings": readings})
    return records


# --------------------------------------------------------------------------
# What miniaudio is asked to open
# --------------------------------------------------------------------------


class _FakeDevice:
    def __init__(self, opened: list[dict[str, Any]], **keywords: Any) -> None:
        opened.append({key: str(value) for key, value in sorted(keywords.items())})

    def start(self, generator: Any) -> None:
        pass

    def close(self) -> None:
        pass


class _FakeMiniaudio:
    """The three entry points the recorder and the player look up."""

    class SampleFormat:
        SIGNED16 = "SIGNED16"

    def __init__(self) -> None:
        self.opened: list[dict[str, Any]] = []

    def Devices(self) -> Any:  # noqa: N802 - the name the reference calls
        return SimpleNamespace(get_captures=lambda: ["mic"], get_playbacks=lambda: ["speaker"])

    def CaptureDevice(self, **keywords: Any) -> Any:  # noqa: N802
        return _FakeDevice(self.opened, **keywords)

    def PlaybackDevice(self, **keywords: Any) -> Any:  # noqa: N802
        return _FakeDevice(self.opened, **keywords)


def wav(rate: int, channels: int, width: int, frames: bytes) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as writer:
        writer.setnchannels(channels)
        writer.setsampwidth(width)
        writer.setframerate(rate)
        writer.writeframes(frames)
    return buffer.getvalue()


def _pcm(samples: list[int]) -> bytes:
    return struct.pack(f"<{len(samples)}h", *samples)


DEVICE_CASES: list[dict[str, Any]] = [
    {"case": "capture-16k", "kind": "capture", "sampleRate": 16000},
    {"case": "capture-48k", "kind": "capture", "sampleRate": 48000},
    {"case": "playback-mono-24k", "kind": "playback", "rate": 24000, "channels": 1},
    {"case": "playback-stereo-44k", "kind": "playback", "rate": 44100, "channels": 2},
]


def capture_devices() -> list[dict[str, Any]]:
    import vibe.cli.audio_player.audio_player as player_module
    import vibe.cli.audio_recorder.audio_recorder as recorder_module
    from vibe.cli.audio_player.audio_player_port import AudioFormat
    from vibe.utils.audio import RecordingMode

    records = []
    for case in DEVICE_CASES:
        fake = _FakeMiniaudio()
        original = (recorder_module.ma, player_module.ma)
        recorder_module.ma = fake  # type: ignore[assignment]
        player_module.ma = fake  # type: ignore[assignment]
        try:
            if case["kind"] == "capture":
                recorder = recorder_module.AudioRecorder()
                recorder.start(RecordingMode.STREAM, sample_rate=case["sampleRate"], max_duration=0)
                recorder.cancel()
            else:
                payload = wav(case["rate"], case["channels"], 2, _pcm([0] * 8 * case["channels"]))
                player = player_module.AudioPlayer()
                player.play(payload, AudioFormat.WAV)
                player.stop()
        finally:
            recorder_module.ma, player_module.ma = original  # type: ignore[assignment]
        records.append({**case, "opened": fake.opened})
    return records


# --------------------------------------------------------------------------
# WAV decoding
# --------------------------------------------------------------------------


def _wav_cases() -> list[dict[str, Any]]:
    mono = _pcm([0, 1000, -1000, 32767, -32768, 12, -12, 7])
    stereo = _pcm([1, -1, 2, -2, 3, -3, 4, -4])
    with_list = bytearray(wav(16000, 1, 2, mono))
    # A LIST chunk ahead of the data chunk, as an encoder may write one.
    data_at = with_list.index(b"data")
    extra = b"LIST" + struct.pack("<I", 4) + b"INFO"
    with_list[data_at:data_at] = extra
    with_list[4:8] = struct.pack("<I", len(with_list) - 8)
    odd_list = bytearray(wav(16000, 1, 2, mono))
    data_at = odd_list.index(b"data")
    odd = b"note" + struct.pack("<I", 3) + b"abc\x00"
    odd_list[data_at:data_at] = odd
    odd_list[4:8] = struct.pack("<I", len(odd_list) - 8)
    float_format = bytearray(wav(16000, 1, 2, mono))
    float_format[20:22] = struct.pack("<H", 3)
    truncated = wav(16000, 1, 2, mono)[:-3]
    return [
        {"case": "mono-16k", "payload": wav(16000, 1, 2, mono)},
        {"case": "stereo-24k", "payload": wav(24000, 2, 2, stereo)},
        {"case": "list-chunk", "payload": bytes(with_list)},
        {"case": "odd-chunk", "payload": bytes(odd_list)},
        {"case": "empty-data", "payload": wav(22050, 1, 2, b"")},
        {"case": "truncated-data", "payload": truncated},
        {"case": "eight-bit", "payload": wav(8000, 1, 1, bytes([128, 129, 127, 255]))},
        {"case": "float-format", "payload": bytes(float_format)},
        {"case": "not-riff", "payload": b"ID3\x04fixture-mp3-payload"},
        {"case": "short", "payload": b"RIFF"},
    ]


def capture_decode_wav() -> list[dict[str, Any]]:
    from vibe.cli.audio_player.utils import decode_wav

    records = []
    for case in _wav_cases():
        payload = case["payload"]
        record: dict[str, Any] = {
            "case": case["case"],
            "payload": base64.b64encode(payload).decode("ascii"),
        }
        try:
            rate, channels, pcm = decode_wav(payload)
            record["decoded"] = {
                "sampleRate": rate,
                "channels": channels,
                "pcmBytes": len(pcm),
                "pcmSha256": hashlib.sha256(pcm).hexdigest(),
            }
        except Exception as error:
            record["error"] = type(error).__name__
        records.append(record)
    return records


# --------------------------------------------------------------------------
# Request metadata
# --------------------------------------------------------------------------


def capture_audio_metadata() -> list[dict[str, Any]]:
    from vibe.cli.audio_request_metadata import build_audio_request_metadata

    records = []
    for case, session, parent in [
        ("root", "fixture-session", None),
        ("child", "fixture-child", "fixture-parent"),
    ]:
        metadata = build_audio_request_metadata(session_id=session, parent_session_id=parent)
        records.append({
            "case": case,
            "sessionId": session,
            "parentSessionId": parent,
            "keys": list(metadata),
            "values": {
                key: value
                for key, value in metadata.items()
                if key in ("session_id", "parent_session_id", "call_type", "call_source")
            },
        })
    return records


# --------------------------------------------------------------------------
# The realtime client over a scripted websocket
# --------------------------------------------------------------------------


class _ScriptedSocket:
    """The connection the SDK's opener would answer with, playing a script.

    `send` steps deliver a server frame, `await` steps hold the server until
    the client has sent a frame of that type, and `close` ends the connection
    the way a server's normal close ends `websockets`' iteration.
    """

    def __init__(self, script: list[dict[str, Any]]) -> None:
        self._script = list(script)
        self.sent: list[dict[str, Any]] = []
        self._changed = asyncio.Condition()
        self._closed = False

    async def send(self, text: str) -> None:
        async with self._changed:
            self.sent.append(json.loads(text))
            self._changed.notify_all()

    async def _next(self) -> str | None:
        while self._script:
            step = self._script.pop(0)
            if "send" in step:
                return json.dumps(step["send"])
            if "await" in step:
                wanted = step["await"]
                async with self._changed:
                    await self._changed.wait_for(
                        lambda: any(frame.get("type") == wanted for frame in self.sent)
                    )
                continue
            if step.get("close"):
                self._closed = True
                return None
        # The script is spent: the server stays silent until the client
        # closes, as a live endpoint would.
        await asyncio.Event().wait()
        return None

    async def recv(self) -> str:
        frame = await self._next()
        if frame is None:
            raise ConnectionError("the scripted server closed the connection")
        return frame

    def __aiter__(self) -> Any:
        return self._iterate()

    async def _iterate(self) -> Any:
        while not self._closed:
            frame = await self._next()
            if frame is None:
                return
            yield frame

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self._closed = True


#: The session every scripted server announces, complete enough for the SDK's
#: model to accept it: a frame it rejects is replayed as an unknown event.
CREATED = {
    "type": "session.created",
    "session": {
        "request_id": REQUEST_ID,
        "model": "fixture-stream-model",
        "audio_format": {"encoding": "pcm_s16le", "sample_rate": 16000},
    },
}


def done(text: str) -> dict[str, Any]:
    return {
        "type": "transcription.done",
        "model": "fixture-stream-model",
        "text": text,
        "usage": {},
        "language": None,
    }


def failure(message: Any) -> dict[str, Any]:
    return {"type": "error", "error": {"message": message, "code": 4000}}


TRANSCRIBE_CASES: list[dict[str, Any]] = [
    {
        "case": "streamed",
        "chunks": ["AQI=", "AwQ="],
        "script": [
            {"send": CREATED},
            {"await": "input_audio.end"},
            {"send": {"type": "transcription.text.delta", "text": "hello"}},
            {"send": {"type": "transcription.text.delta", "text": " world"}},
            {"send": done("hello world")},
        ],
    },
    {
        "case": "text-before-the-end",
        "chunks": ["AQI="],
        "script": [
            {"send": CREATED},
            {"send": {"type": "transcription.text.delta", "text": "early"}},
            {"await": "input_audio.end"},
            {"send": done("early")},
        ],
    },
    {
        "case": "empty-recording-marker",
        "chunks": [],
        "script": [
            {"send": CREATED},
            {"await": "input_audio.end"},
            {"send": failure("flush requested before sending any audio bytes")},
        ],
    },
    {
        "case": "error-frame-detail",
        "chunks": [],
        "keepOpen": True,
        "script": [
            {"send": CREATED},
            {"send": failure({"detail": "quota exceeded"})},
        ],
    },
    {
        "case": "error-frame-text",
        "chunks": ["AQI="],
        "script": [
            {"send": CREATED},
            {"await": "input_audio.end"},
            {"send": failure("fixture failure")},
        ],
    },
    {
        "case": "unknown-event-ignored",
        "chunks": [],
        "script": [
            {"send": CREATED},
            {"send": {"type": "fixture.unknown", "detail": 1}},
            {"await": "input_audio.end"},
            {"send": done("")},
        ],
    },
    {
        "case": "closed-after-the-recording",
        "chunks": ["AQI="],
        "script": [
            {"send": CREATED},
            {"await": "input_audio.end"},
            {"close": True},
        ],
    },
    {
        "case": "closed-during-the-recording",
        "chunks": [],
        "keepOpen": True,
        "script": [
            {"send": CREATED},
            {"close": True},
        ],
    },
    {
        "case": "done-before-the-recording-ends",
        "chunks": [],
        "keepOpen": True,
        "script": [{"send": CREATED}, {"send": done("quick")}],
    },
    {
        "case": "handshake-refused",
        "chunks": [],
        "keepOpen": True,
        "script": [{"send": failure("invalid model")}],
    },
]

#: The metadata the transcription cases are opened with, authored here so the
#: header's serialization is what is compared.
TRANSCRIBE_METADATA = {"session_id": "fixture-session", "note": "café \"quoted\""}


async def capture_transcribe_stream(sentinel: str) -> list[dict[str, Any]]:
    from mistralai.extra.realtime import transcription as realtime_transcription

    from vibe.app_server.config import AudioProviderView, TranscribeModelConfigView
    from vibe.cli.transcribe.mistral_transcribe_client import MistralTranscribeClient
    from vibe.cli.transcribe.transcribe_client_port import (
        TranscribeDone,
        TranscribeError,
        TranscribeSessionCreated,
        TranscribeTextDelta,
    )

    provider = AudioProviderView(
        api_base="https://gateway.fixture.invalid", api_key_env_var="MISTRAL_API_KEY", client="mistral"
    )
    model = TranscribeModelConfigView(
        name="fixture-stream-model",
        sample_rate=16000,
        encoding="pcm_s16le",
        language="en",
        target_streaming_delay_ms=480,
    )
    records = []
    original = realtime_transcription.connect
    for case in TRANSCRIBE_CASES:
        socket = _ScriptedSocket(case["script"])
        opened: dict[str, Any] = {}

        async def connect(url: str, *arguments: Any, **keywords: Any) -> Any:
            opened["url"] = url
            opened["headers"] = dict(keywords.get("additional_headers") or {})
            return socket

        realtime_transcription.connect = connect  # type: ignore[assignment]
        client = MistralTranscribeClient(
            provider=provider, model=model, metadata_getter=lambda: dict(TRANSCRIBE_METADATA)
        )

        async def audio(chunks: list[str] = case["chunks"], keep_open: bool = bool(case.get("keepOpen"))) -> Any:
            for chunk in chunks:
                yield base64.b64decode(chunk)
            if keep_open:
                await asyncio.Event().wait()

        events: list[dict[str, Any]] = []

        async def run() -> None:
            try:
                async for event in client.transcribe(audio()):
                    match event:
                        case TranscribeSessionCreated(request_id=request_id):
                            events.append({"type": "sessionCreated", "requestId": request_id})
                        case TranscribeTextDelta(text=text):
                            events.append({"type": "textDelta", "text": text})
                        case TranscribeDone():
                            events.append({"type": "done"})
                        case TranscribeError(message=message):
                            events.append({"type": "error", "message": prose(message)})
            except Exception as error:
                # The manager reports an escaping exception by its text, as it
                # reports an error event, so both are recorded the same way.
                events.append({"type": "error", "message": prose(str(error))})

        try:
            await asyncio.wait_for(run(), timeout=5)
        except TimeoutError:
            events.append({"type": "timeout"})
        finally:
            realtime_transcription.connect = original  # type: ignore[assignment]
            await client.close()
        headers = {
            name.lower(): _normalize(value, sentinel)
            for name, value in opened.get("headers", {}).items()
        }
        url = opened.get("url", "")
        records.append({
            "case": case["case"],
            "chunks": case["chunks"],
            "keepOpen": bool(case.get("keepOpen")),
            "script": case["script"],
            "events": events,
            "sent": socket.sent,
            "path": url.split("://", 1)[-1].split("/", 1)[-1],
            "headers": headers,
        })
    return records


# --------------------------------------------------------------------------
# The dictation lifecycle
# --------------------------------------------------------------------------


class _ScriptedRecorder:
    def __init__(self, *, start_error: Exception | None, has_signal: bool, duration: float) -> None:
        from vibe.utils.audio import RecordingMode

        self.mode = RecordingMode.STREAM
        self.peak = 0.0
        self.has_signal = has_signal
        self.is_recording = False
        self._start_error = start_error
        self._duration = duration
        self._queue: asyncio.Queue[bytes | None] | None = None
        self.calls: list[str] = []

    def start(self, mode: Any, *, sample_rate: int = 0, **keywords: Any) -> None:
        self.calls.append(f"start:{sample_rate}")
        if self._start_error is not None:
            raise self._start_error
        self.is_recording = True
        self._queue = asyncio.Queue()
        self._queue.put_nowait(b"\x01\x00")

    def stop(self, *, wait_for_queue_drained: bool = True) -> Any:
        from vibe.cli.audio_recorder.audio_recorder_port import AudioRecording

        self.calls.append("stop")
        self._end()
        return AudioRecording(data=b"", duration=self._duration)

    def cancel(self) -> None:
        self.calls.append("cancel")
        self._end()

    def _end(self) -> None:
        if self.is_recording and self._queue is not None:
            self._queue.put_nowait(None)
        self.is_recording = False

    async def audio_stream(self) -> Any:
        queue = self._queue
        if queue is None:
            return
        while True:
            chunk = await queue.get()
            if chunk is None:
                return
            yield chunk


class _ScriptedTranscribeClient:
    """Yields the scripted events; `awaitEnd` drains the recording first,
    `hang` never returns and `raise` fails with the given text."""

    def __init__(self, script: list[dict[str, Any]]) -> None:
        self._script = script

    async def transcribe(self, audio_stream: Any) -> Any:
        from vibe.cli.transcribe.transcribe_client_port import (
            TranscribeDone,
            TranscribeError,
            TranscribeSessionCreated,
            TranscribeTextDelta,
        )

        for step in self._script:
            kind = step["step"]
            if kind == "created":
                yield TranscribeSessionCreated(request_id=REQUEST_ID)
            elif kind == "text":
                yield TranscribeTextDelta(text=step["text"])
            elif kind == "done":
                yield TranscribeDone()
            elif kind == "error":
                yield TranscribeError(message=step["message"])
            elif kind == "raise":
                raise RuntimeError(step["message"])
            elif kind == "awaitEnd":
                async for _chunk in audio_stream:
                    pass
            elif kind == "hang":
                await asyncio.Event().wait()

    async def close(self) -> None:
        pass


class _Telemetry:
    def __init__(self) -> None:
        self.events: list[Any] = []

    def log(self, event: Any) -> None:
        self.events.append(event)


def _telemetry_record(event: Any) -> dict[str, Any]:
    properties = dict(event.properties)
    record: dict[str, Any] = {"name": event.name, "keys": sorted(properties)}
    for key in ("recording_id", "transcript_length", "status", "trigger"):
        if key in properties:
            record[key] = properties[key]
    for key in ("error_message",):
        if key in properties:
            value = properties[key]
            record[key] = prose(value) if isinstance(value, str) else value
    for key in ("error_type",):
        if key in properties:
            record[key] = properties[key]
    for key in (
        "recording_duration_ms",
        "transcription_duration_ms",
        "elapsed_seconds",
        "time_to_first_read_s",
    ):
        if key in properties:
            record[key] = "null" if properties[key] is None else "number"
    return record


VOICE_CASES: list[dict[str, Any]] = [
    {
        "case": "transcribed",
        "client": [{"step": "created"}, {"step": "text", "text": "hello"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "stop"],
    },
    {
        "case": "no-speech",
        "client": [{"step": "created"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "stop"],
    },
    {
        "case": "no-audio",
        "hasSignal": False,
        "client": [{"step": "created"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "stop"],
    },
    {
        "case": "too-short-for-a-signal",
        "hasSignal": False,
        "duration": 0.2,
        "client": [{"step": "created"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "stop"],
    },
    {
        "case": "silent-with-text",
        "hasSignal": False,
        "client": [{"step": "created"}, {"step": "text", "text": "hi"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "stop"],
    },
    {
        "case": "failed",
        "client": [{"step": "created"}, {"step": "text", "text": "partial"}, {"step": "error", "message": "fixture failure"}],
        "actions": ["start"],
    },
    {
        "case": "raised",
        "client": [{"step": "created"}, {"step": "raise", "message": "fixture exception"}],
        "actions": ["start"],
    },
    {
        "case": "failed-before-the-session",
        "client": [{"step": "error", "message": "fixture refusal"}],
        "actions": ["start"],
    },
    {
        "case": "cancelled",
        "client": [{"step": "created"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "cancel"],
    },
    {
        "case": "cancelled-when-idle",
        "client": [],
        "actions": ["cancel"],
    },
    {
        "case": "stopped-when-idle",
        "client": [],
        "actions": ["stop"],
    },
    {
        "case": "timed-out",
        "client": [{"step": "created"}, {"step": "hang"}],
        "actions": ["start", "stop"],
    },
    {
        "case": "started-twice",
        "client": [{"step": "created"}, {"step": "awaitEnd"}, {"step": "done"}],
        "actions": ["start", "start", "stop"],
    },
    {
        "case": "ended-without-a-stop",
        "client": [{"step": "created"}, {"step": "text", "text": "solo"}, {"step": "done"}],
        "actions": ["start"],
    },
    {
        "case": "missing-key",
        "apiKeyEnvVar": "FIXTURE_UNSET_VOICE_KEY",
        "client": [],
        "actions": ["start"],
    },
    {
        "case": "no-client",
        "client": None,
        "actions": ["start"],
    },
    {
        "case": "already-recording",
        "startError": "AlreadyRecordingError",
        "client": [],
        "actions": ["start"],
    },
    {
        "case": "backend-unavailable",
        "startError": "AudioBackendUnavailableError",
        "client": [],
        "actions": ["start"],
    },
    {
        "case": "no-input-device",
        "startError": "NoAudioInputDeviceError",
        "client": [],
        "actions": ["start"],
    },
]

#: The text an unavailable backend is raised with, authored here.
BACKEND_DETAIL = "fixture backend detail"
#: The drain timeout the timed-out case runs under, so it settles at once.
SHORT_DRAIN_TIMEOUT = 0.05


def _start_error(name: str | None) -> Exception | None:
    from vibe.cli.audio_recorder import audio_recorder_port as port

    if name is None:
        return None
    if name == "AudioBackendUnavailableError":
        return port.AudioBackendUnavailableError(BACKEND_DETAIL)
    return getattr(port, name)()


class _VoiceListener:
    def __init__(self, observed: list[dict[str, Any]]) -> None:
        self._observed = observed

    def on_transcribe_state_change(self, state: Any) -> None:
        self._observed.append({"state": str(state)})

    def on_voice_mode_change(self, enabled: bool) -> None:
        self._observed.append({"enabled": enabled})

    def on_transcribe_text(self, text: str) -> None:
        self._observed.append({"text": text})

    def on_transcribe_error(self, message: str) -> None:
        self._observed.append({"error": prose(message)})

    def on_transcribe_notice(self, message: str) -> None:
        self._observed.append({"notice": prose(message)})


async def capture_voice_manager() -> list[dict[str, Any]]:
    import vibe.cli.voice_manager.voice_manager as manager_module

    records = []
    original_timeout = manager_module.TRANSCRIPTION_DRAIN_TIMEOUT
    manager_module.TRANSCRIPTION_DRAIN_TIMEOUT = SHORT_DRAIN_TIMEOUT  # type: ignore[assignment]
    try:
        for case in VOICE_CASES:
            config = SimpleNamespace(
                voice_mode_enabled=True,
                transcription=SimpleNamespace(
                    model=SimpleNamespace(sample_rate=16000),
                    provider=SimpleNamespace(api_key_env_var=case.get("apiKeyEnvVar", "")),
                ),
            )
            recorder = _ScriptedRecorder(
                start_error=_start_error(case.get("startError")),
                has_signal=case.get("hasSignal", True),
                duration=case.get("duration", 1.0),
            )
            client = None if case["client"] is None else _ScriptedTranscribeClient(case["client"])
            telemetry = _Telemetry()
            manager = manager_module.VoiceManager(
                lambda: config,
                audio_recorder=recorder,
                transcribe_client=client,
                telemetry_client=telemetry,
            )
            observed: list[dict[str, Any]] = []
            manager.add_listener(_VoiceListener(observed))
            results = []
            for action in case["actions"]:
                result: dict[str, Any] = {"action": action}
                if action == "start":
                    try:
                        manager.start_recording()
                    except manager_module.RecordingStartError as error:
                        result["error"] = prose(str(error))
                elif action == "stop":
                    await manager.stop_recording()
                elif action == "cancel":
                    manager.cancel_recording()
                await settle()
                results.append(result)
            await manager.close()
            records.append({
                **case,
                "results": results,
                "listener": observed,
                "telemetry": [_telemetry_record(event) for event in telemetry.events],
                "recorder": recorder.calls,
                "finalState": str(manager.transcribe_state),
            })
    finally:
        manager_module.TRANSCRIPTION_DRAIN_TIMEOUT = original_timeout  # type: ignore[assignment]
    return records


# --------------------------------------------------------------------------
# The read-aloud lifecycle
# --------------------------------------------------------------------------


class _ScriptedSummary:
    def __init__(self, answers: list[Any]) -> None:
        self._answers = list(answers)
        self.calls: list[dict[str, Any]] = []

    async def summarize(self, *, user_message: str, assistant_text: str, error: str | None, message_id: str | None) -> str | None:
        self.calls.append({
            "userMessage": user_message,
            "assistantText": assistant_text,
            "error": error,
            "messageId": message_id,
        })
        answer = self._answers.pop(0) if self._answers else None
        if isinstance(answer, dict) and answer.get("hang"):
            await asyncio.Event().wait()
        if isinstance(answer, dict) and answer.get("raise"):
            raise RuntimeError("fixture summary failure")
        return answer


class FixtureSpeechError(Exception):
    """The class a scripted speech failure is raised as."""


class _ScriptedSpeech:
    def __init__(self, fail: bool) -> None:
        self._fail = fail
        self.spoken: list[str] = []

    async def speak(self, text: str) -> Any:
        from vibe.cli.tts.tts_client_port import TTSResult

        self.spoken.append(text)
        if self._fail:
            raise FixtureSpeechError("fixture speech failure")
        return TTSResult(audio_data=wav(16000, 1, 2, _pcm([0, 1, 2, 3])))

    async def close(self) -> None:
        pass


class _ScriptedPlayer:
    """Plays until told to finish; `fail` names the class `play` raises."""

    def __init__(self, fail: str | None) -> None:
        self._fail = fail
        self.is_playing = False
        self._on_finished: Any = None
        self.played = 0

    def play(self, audio_data: bytes, audio_format: Any, *, on_finished: Any = None) -> None:
        from vibe.cli.audio_player import audio_player_port as port

        if self.is_playing:
            raise port.AlreadyPlayingError("Already playing")
        if self._fail is not None:
            raise getattr(port, self._fail)("fixture device failure")
        self.played += 1
        self.is_playing = True
        self._on_finished = on_finished

    def finish(self) -> None:
        if self.is_playing:
            self.is_playing = False
            callback, self._on_finished = self._on_finished, None
            if callback is not None:
                callback()

    def stop(self) -> None:
        self.is_playing = False
        self._on_finished = None


NARRATOR_CASES: list[dict[str, Any]] = [
    {
        "case": "spoken",
        "summaries": ["Parser written."],
        "steps": [
            {"turnStart": "write the parser"},
            {"userMessage": "message-1"},
            {"assistantText": "Done"},
            {"assistantText": ", parser written."},
            {"turnEnd": True},
            {"finish": True},
        ],
    },
    {
        "case": "empty-summary",
        "summaries": [""],
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}, {"finish": True}],
    },
    {
        "case": "no-summary",
        "summaries": [None],
        "steps": [{"turnStart": "hi"}, {"assistantText": "Hello."}, {"turnEnd": True}],
    },
    {
        "case": "summary-failed",
        "summaries": [{"raise": True}],
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}],
    },
    {
        "case": "turn-error",
        "summaries": ["The turn failed."],
        "steps": [
            {"turnStart": "deploy"},
            {"assistantText": ""},
            {"turnError": "Rate limits exceeded."},
            {"turnEnd": True},
            {"finish": True},
        ],
    },
    {
        "case": "speech-failed",
        "summaries": ["Parser written."],
        "speechFails": True,
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}],
    },
    {
        "case": "no-output-device",
        "summaries": ["Parser written."],
        "playerFails": "NoAudioOutputDeviceError",
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}],
    },
    {
        "case": "cancelled-while-speaking",
        "summaries": ["Parser written."],
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}, {"cancel": True}],
    },
    {
        "case": "cancelled-while-summarizing",
        "summaries": [{"hang": True}],
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}, {"cancel": True}],
    },
    {
        "case": "turn-cancelled",
        "summaries": ["Unused."],
        "steps": [{"turnStart": "hi"}, {"turnCancel": True}, {"turnEnd": True}],
    },
    {
        "case": "turn-end-without-a-start",
        "summaries": ["Unused."],
        "steps": [{"turnEnd": True}],
    },
    {
        "case": "overlapping-turns",
        "summaries": ["First.", "Second."],
        "steps": [
            {"turnStart": "first"},
            {"turnEnd": True},
            {"turnStart": "second"},
            {"turnEnd": True},
            {"finish": True},
        ],
    },
    {
        "case": "no-speech-model",
        "summaries": ["Unused."],
        "speechAvailable": False,
        "steps": [{"turnStart": "hi"}, {"turnEnd": True}],
    },
]


async def capture_narrator() -> list[dict[str, Any]]:
    import vibe.cli.narrator_manager.narrator_manager as narrator_module

    records = []
    original = narrator_module.make_tts_client
    try:
        for case in NARRATOR_CASES:
            speech = _ScriptedSpeech(bool(case.get("speechFails")))
            available = case.get("speechAvailable", True)

            def make(provider: Any, model: Any, metadata_getter: Any = None, speech: Any = speech, available: bool = available) -> Any:
                if not available:
                    raise KeyError("fixture-missing-speech-model")
                return speech

            narrator_module.make_tts_client = make  # type: ignore[assignment]
            config = SimpleNamespace(
                narrator_enabled=True,
                speech=SimpleNamespace(model=None, provider=None),
            )
            player = _ScriptedPlayer(case.get("playerFails"))
            summary = _ScriptedSummary(case["summaries"])
            telemetry = _Telemetry()
            manager = narrator_module.NarratorManager(
                config_getter=lambda: config,
                audio_player=player,
                summary_generator=summary,
                telemetry_client=telemetry,
            )
            states: list[str] = []

            class Listener:
                def on_narrator_state_change(self, state: Any) -> None:
                    states.append(str(state))

            manager.add_listener(Listener())
            for step in case["steps"]:
                if "turnStart" in step:
                    manager.on_turn_start(step["turnStart"])
                elif "userMessage" in step:
                    manager.on_user_message(step["userMessage"])
                elif "assistantText" in step:
                    manager.on_assistant_text(step["assistantText"])
                elif "turnError" in step:
                    manager.on_turn_error(step["turnError"])
                elif "turnCancel" in step:
                    manager.on_turn_cancel()
                elif "turnEnd" in step:
                    manager.on_turn_end()
                elif "cancel" in step:
                    manager.cancel()
                elif "finish" in step:
                    player.finish()
                await settle()
            await manager.close()
            records.append({
                **case,
                "states": states,
                "summaryCalls": summary.calls,
                "spoken": speech.spoken,
                "played": player.played,
                "telemetry": [_telemetry_record(event) for event in telemetry.events],
            })
    finally:
        narrator_module.make_tts_client = original  # type: ignore[assignment]
    return records


# --------------------------------------------------------------------------
# The turn summary request
# --------------------------------------------------------------------------

NARRATION_CASES: list[dict[str, Any]] = [
    {
        "case": "summarized",
        "params": {"userMessage": "write the parser", "assistantText": "Parser written."},
        "answer": {"content": "Parser written and tested."},
    },
    {
        "case": "with-an-error-and-a-message",
        "params": {
            "userMessage": "deploy",
            "assistantText": "",
            "error": "Rate limits exceeded.",
            "messageId": "message-7",
        },
        "answer": {"content": "The deploy hit a rate limit."},
    },
    {
        "case": "no-content",
        "params": {"userMessage": "hi", "assistantText": "Hello."},
        "answer": {"content": None},
    },
    {
        "case": "failed-call",
        "params": {"userMessage": "hi", "assistantText": "Hello."},
        "answer": {"raise": True},
    },
    {
        "case": "no-mistral-provider",
        "params": {"userMessage": "hi", "assistantText": "Hello."},
        "providers": [{"name": "fixture-other", "api_key_env_var": ""}],
        "answer": {"content": "Unused."},
    },
    {
        "case": "provider-key-unset",
        "params": {"userMessage": "hi", "assistantText": "Hello."},
        "providers": [{"name": "mistral", "api_key_env_var": "FIXTURE_UNSET_NARRATION_KEY"}],
        "answer": {"content": "Unused."},
    },
    {
        "case": "provider-without-a-key",
        "params": {"userMessage": "hi", "assistantText": "Hello."},
        "providers": [{"name": "mistral", "api_key_env_var": ""}],
        "answer": {"content": "Keyless summary."},
    },
]


async def capture_narration_service(sentinel: str, build_config: Any) -> list[dict[str, Any]]:
    import vibe.app_server._narration as narration_module
    from vibe.app_server.protocol import NarrationSummarizeParams
    from vibe.core.config import ProviderConfig

    records = []
    original = narration_module.create_backend
    base = await build_config("")
    try:
        for case in NARRATION_CASES:
            requests: list[dict[str, Any]] = []
            answer = case["answer"]

            class Backend:
                async def __aenter__(self) -> Any:
                    return self

                async def __aexit__(self, *exc: Any) -> None:
                    return None

                async def complete(self, **keywords: Any) -> Any:
                    requests.append(keywords)
                    if answer.get("raise"):
                        raise RuntimeError("fixture backend failure")
                    return SimpleNamespace(message=SimpleNamespace(content=answer["content"]))

            def create(*, provider: Any, **keywords: Any) -> Any:
                requests.append({"provider": provider.name})
                return Backend()

            narration_module.create_backend = create  # type: ignore[assignment]
            config = base
            if "providers" in case:
                config = base.model_copy(update={
                    "providers": [
                        ProviderConfig(
                            name=entry["name"],
                            api_base="https://gateway.fixture.invalid/v1",
                            api_key_env_var=entry["api_key_env_var"],
                        )
                        for entry in case["providers"]
                    ]
                })
            service = narration_module.NarrationService(
                lambda config=config: narration_module.NarrationContext(
                    config=config, launch_context=None, parent_session_id=None, user_plan=None
                )
            )
            params = NarrationSummarizeParams(
                session_id="fixture-session",
                user_message=case["params"]["userMessage"],
                assistant_text=case["params"]["assistantText"],
                error=case["params"].get("error"),
                message_id=case["params"].get("messageId"),
            )
            summary = await service.summarize(params)
            request: dict[str, Any] | None = None
            calls = [entry for entry in requests if "model" in entry]
            if calls:
                call = calls[0]
                metadata = call.get("metadata") or {}
                request = {
                    "provider": next(
                        (entry["provider"] for entry in requests if "provider" in entry), None
                    ),
                    "model": call["model"].name,
                    "temperature": call["temperature"],
                    "maxTokens": call["max_tokens"],
                    "messages": [
                        {"role": str(message.role), "content": prose(message.content)}
                        for message in call["messages"]
                    ],
                    "userAgent": _normalize(call["extra_headers"]["user-agent"], sentinel),
                    "metadataKeys": sorted(metadata),
                    "metadata": {
                        key: metadata.get(key)
                        for key in ("call_type", "message_id", "session_id")
                    },
                }
            records.append({
                **case,
                "summary": summary,
                "request": request,
            })
    finally:
        narration_module.create_backend = original  # type: ignore[assignment]
    return records


# --------------------------------------------------------------------------
# The editor's voice extension
# --------------------------------------------------------------------------

BRIDGE_CASES: list[dict[str, Any]] = [
    {
        "case": "no-session",
        "client": [],
        "steps": [
            {"call": "transcribeStart"},
            {"call": "narrate"},
            {"call": "transcribeStop"},
            {"call": "transcribeCancel"},
            {"call": "narrateCancel"},
        ],
        "session": False,
    },
    {
        "case": "dictation",
        "client": [{"step": "created"}, {"step": "text", "text": "hello"}, {"step": "awaitEnd"}, {"step": "done"}],
        "steps": [
            {"call": "transcribeStart"},
            {"call": "transcribeStart"},
            {"call": "transcribeStop"},
            {"call": "transcribeStop"},
        ],
    },
    {
        "case": "dictation-failed",
        "client": [{"step": "created"}, {"step": "error", "message": "fixture failure"}],
        "steps": [{"call": "transcribeStart"}, {"call": "transcribeStop"}],
    },
    {
        "case": "dictation-cancelled",
        "client": [{"step": "created"}, {"step": "awaitEnd"}, {"step": "done"}],
        "steps": [{"call": "transcribeStart"}, {"call": "transcribeCancel"}],
    },
    {
        "case": "dictation-refused",
        "startError": "NoAudioInputDeviceError",
        "client": [],
        "steps": [{"call": "transcribeStart"}],
    },
    {
        "case": "narration",
        "summaries": ["Fixture summary."],
        "steps": [{"call": "narrate"}, {"finish": True}],
    },
    {
        "case": "narration-without-a-summary",
        "summaries": [None],
        "steps": [{"call": "narrate"}],
    },
    {
        "case": "narration-cancelled",
        "summaries": ["Fixture summary."],
        "steps": [{"call": "narrate"}, {"call": "narrateCancel"}],
    },
    {
        "case": "narration-replaced-while-preparing",
        "summaries": [{"hang": True}, "Second."],
        "steps": [{"call": "narrate"}, {"call": "narrate"}, {"finish": True}],
    },
    {
        "case": "narration-replaced-while-speaking",
        "summaries": ["First.", "Second."],
        "steps": [{"call": "narrate"}, {"call": "narrate"}, {"finish": True}],
    },
    {
        "case": "narration-cancelled-twice",
        "summaries": ["Fixture summary."],
        "steps": [{"call": "narrate"}, {"call": "narrateCancel"}, {"call": "narrateCancel"}],
    },
]

#: What the narrate calls send, authored here.
BRIDGE_USER_MESSAGE = "write the parser"
BRIDGE_ASSISTANT_TEXT = "Parser written."


def _result(value: dict[str, Any]) -> dict[str, Any]:
    return {key: prose(item) if key == "error" else item for key, item in value.items()}


async def capture_acp_bridge() -> list[dict[str, Any]]:
    import vibe.acp.voice as bridge_module
    import vibe.cli.narrator_manager.narrator_manager as narrator_module
    from vibe.cli.lazy_audio_managers import LazyNarratorManager, LazyVoiceManager
    from vibe.cli.narrator_manager.narrator_manager import NarratorManager
    from vibe.cli.voice_manager.voice_manager import VoiceManager

    records = []
    originals = (
        bridge_module.create_default_voice_manager,
        bridge_module.create_default_narrator_manager,
        narrator_module.make_tts_client,
    )
    try:
        for case in BRIDGE_CASES:
            recorder = _ScriptedRecorder(
                start_error=_start_error(case.get("startError")),
                has_signal=True,
                duration=1.0,
            )
            client = _ScriptedTranscribeClient(case.get("client", []))
            player = _ScriptedPlayer(None)
            speech = _ScriptedSpeech(False)
            summary = _ScriptedSummary(case.get("summaries", []))

            def voice_factory(config_getter: Any, telemetry_client: Any, request_metadata_getter: Any, recorder: Any = recorder, client: Any = client) -> Any:
                return LazyVoiceManager(
                    config_getter,
                    lambda: VoiceManager(
                        config_getter,
                        audio_recorder=recorder,
                        transcribe_client=client,
                        telemetry_client=telemetry_client,
                    ),
                )

            def narrator_factory(config_getter: Any, summary_generator: Any, telemetry_client: Any, request_metadata_getter: Any, player: Any = player) -> Any:
                return LazyNarratorManager(
                    config_getter,
                    lambda: NarratorManager(
                        config_getter=config_getter,
                        audio_player=player,
                        summary_generator=summary_generator,
                        telemetry_client=telemetry_client,
                    ),
                )

            bridge_module.create_default_voice_manager = voice_factory  # type: ignore[assignment]
            bridge_module.create_default_narrator_manager = narrator_factory  # type: ignore[assignment]
            narrator_module.make_tts_client = lambda provider, model, metadata_getter=None, speech=speech: speech  # type: ignore[assignment]

            notices: list[dict[str, Any]] = []

            class Client:
                async def ext_notification(self, method: str, params: dict[str, Any]) -> None:
                    notices.append({
                        "method": method,
                        "params": _result(params) if "error" in params else params,
                    })

            config = SimpleNamespace(
                narrator_enabled=True,
                voice_mode_enabled=True,
                transcription=SimpleNamespace(
                    model=SimpleNamespace(sample_rate=16000),
                    provider=SimpleNamespace(api_key_env_var=""),
                ),
                speech=SimpleNamespace(model=None, provider=None),
            )
            with_session = case.get("session", True)
            keywords = (
                {"config_getter": lambda: config, "narration_resource": summary, "telemetry": None}
                if with_session
                else {"config_getter": None, "narration_resource": None, "telemetry": None}
            )
            controller = bridge_module.VoiceController(lambda: Client())
            steps = []
            for step in case["steps"]:
                seen = len(notices)
                observed: dict[str, Any] = dict(step)
                if step.get("finish"):
                    player.finish()
                else:
                    call = step["call"]
                    if call == "transcribeStart":
                        result = await controller.transcribe_start(**keywords)
                    elif call == "transcribeStop":
                        result = await controller.transcribe_stop()
                    elif call == "transcribeCancel":
                        result = await controller.transcribe_cancel()
                    elif call == "narrate":
                        result = await controller.narrate(
                            BRIDGE_USER_MESSAGE, BRIDGE_ASSISTANT_TEXT, **keywords
                        )
                    else:
                        result = await controller.narrate_cancel()
                    observed["result"] = _result(result)
                await settle()
                observed["notices"] = notices[seen:]
                steps.append(observed)
            await controller.close()
            await settle()
            records.append({
                "case": case["case"],
                "session": with_session,
                "startError": case.get("startError"),
                "client": case.get("client", []),
                "summaries": case.get("summaries", []),
                "steps": steps,
                "summaryCalls": summary.calls,
                "spoken": speech.spoken,
            })
    finally:
        (
            bridge_module.create_default_voice_manager,
            bridge_module.create_default_narrator_manager,
            narrator_module.make_tts_client,
        ) = originals  # type: ignore[assignment]
    return records


async def capture_behavior(sentinel: str, build_config: Any) -> dict[str, Any]:
    return {
        "captureSignal": capture_signal(),
        "audioDevices": capture_devices(),
        "decodeWav": capture_decode_wav(),
        "audioMetadata": capture_audio_metadata(),
        "transcribeStream": await capture_transcribe_stream(sentinel),
        "voiceManager": await capture_voice_manager(),
        "narrator": await capture_narrator(),
        "narrationService": await capture_narration_service(sentinel, build_config),
        "acpBridge": await capture_acp_bridge(),
    }
