#!/usr/bin/env python3
"""Capture what the pinned Python reference resolves an audio session from.

Voice input and read-aloud both start from a configuration document. The
reference walks it in three steps, and every one of them is observable without
a microphone, a speaker or a socket: the schema resolves an active alias into a
model entry and that entry's provider, the app-server projects the pair into
the two views a client renders, and the per-direction client reads five values
off those views before it ever connects. This script drives all three over
documents it authors itself.

Four resolution families come out of this script, and nine behavior families
out of `voice_behavior.py`, which drives the same reference past resolution
with every device, socket and endpoint scripted. All thirteen are what the
Rust replays compare:

``constants``                the audio defaults, the per-entry field defaults
                             and the closed vocabularies
``transcriptionResolution``  the model, the provider and the failure cause per
                             document, for voice input
``speechResolution``         the same, for read-aloud
``wireFrames``               the request each direction would put on the wire,
                             intercepted one call before the connection

The wire frames are taken at the last seam before the network. The realtime
transcription URL is the one the vendored SDK hands its websocket opener, and
the speech request is the one it hands its HTTP sender; both are intercepted
and neither is sent, which is why the socket guard below stays silent for a
successful run. Reading the URL rather than reconstructing it is what makes the
corpus an oracle for the endpoint this port builds itself.

Two artifacts come out of a run::

    .parity/voice-corpus.json                the full capture, gitignored
    crates/vibe-voice/tests/voice/corpus.json  the committed corpus

No credential is recorded. Every environment variable a document names is set
to a sentinel before the capture, so a resolved credential is recorded as the
variable it came from and never as a value, and ``VIBE_TEST_DISABLE_KEYRING``
keeps the secret store out of the run entirely. Every other name in the corpus
is supplied by the documents in ``voice_cases.py``, which are authored for this
capture; the reference contributes verdicts, defaults and exception class names,
never prose.

Usage::

    scripts/parity/voice.py --reference /path/to/reference --corpus

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
import socket
import sys
import tomllib
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from inprocess import SocketGuard
from voice_cases import CREDENTIAL_VARIABLES, DOCUMENTS

SCHEMA_VERSION = 3
DEFAULT_OUTPUT = Path(".parity/voice-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-voice/tests/voice/corpus.json")
#: What every credential variable is set to before the capture. A resolved
#: credential is recognized by identity with this string and recorded as the
#: variable that carried it, so no value reaches the corpus.
CREDENTIAL_SENTINEL = "parity-sentinel-credential"

#: The reference's own switch for keeping the OS secret store out of a run.
KEYRING_DISABLE_VARIABLE = "VIBE_TEST_DISABLE_KEYRING"

#: The text the speech frame is captured for. Authored here, so the corpus
#: carries this repository's vocabulary and nothing the reference wrote.
SPEECH_INPUT = "fixture summary"


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


class _Guard(SocketGuard):
    """Fails the capture the moment it reaches a network or an audio device.

    Every value this script records is resolved before a connection is opened
    and before a device is queried, which is exactly why an attempt at either
    must fail the run rather than pass unnoticed: a corpus written from a live
    backend, or from whatever hardware the capturing machine happens to hold,
    is not a corpus of the pinned reference.
    """

    def install(self) -> None:
        super().install()
        self._guard_audio()

    def _guard_audio(self) -> None:
        """Refuses every device call the audio surface can make.

        The module is optional on the reference's own terms: it imports
        `miniaudio` inside a `try` and degrades when no audio backend loads
        (`vibe/cli/audio_player/audio_player.py:17-25`), so an absent module
        here is a guard that has nothing to arm rather than a failure. The
        three names are the device entry points the player and the recorder
        look up on the module at call time.
        """

        try:
            import miniaudio
        except Exception:
            return
        for name in ("Devices", "PlaybackDevice", "CaptureDevice"):
            if hasattr(miniaudio, name):
                setattr(miniaudio, name, self.refuse(f"miniaudio.{name}"))

    def verify(self) -> None:
        """Proves the guard is armed, on every run rather than once.

        A guard installed onto the wrong symbol is indistinguishable from a
        capture that never reached the network, and the second reading is the
        one a silent run invites. Resolving a name and querying a device both
        have to fail here, and the attempts they record are discarded so a
        verified guard still reports an empty list.
        """

        self._refused("the network guard is not armed", socket.getaddrinfo, "fixture.invalid", 443)
        try:
            import miniaudio
        except Exception:
            self.attempts.clear()
            return
        self._refused("the audio device guard is not armed", miniaudio.Devices)
        self.attempts.clear()

    @staticmethod
    def _refused(complaint: str, call: Any, *arguments: Any) -> None:
        try:
            call(*arguments)
        except OracleError:
            return
        except Exception as error:
            raise OracleError(f"{complaint}: {type(error).__name__}") from error
        raise OracleError(complaint)


GUARD = _Guard()


# --------------------------------------------------------------------------
# The reference under test
# --------------------------------------------------------------------------


class _Intercepted(Exception):
    """Carries a request captured one call before it would have been sent."""

    def __init__(self, payload: dict[str, Any]) -> None:
        super().__init__("intercepted")
        self.payload = payload


def _initialize_harness() -> None:
    """Arms the singleton the prompt-id validators reach during a build.

    It resolves the prompts shipped inside the checkout, so every document is
    validated against the pinned tree rather than against whatever the
    capturing machine holds.
    """

    from vibe.core.config.harness_files import init_harness_files_manager

    init_harness_files_manager()


async def _build_config(document: str) -> Any:
    from vibe.core.config.builder import ConfigBuilder
    from vibe.core.config.layers.default import DefaultConfigLayer
    from vibe.core.config.layers.overrides import OverridesLayer
    from vibe.core.config.vibe_schema import VibeConfigSchema

    # The builder takes no validation context: the API-key check is an explicit
    # method a plain build never calls (`vibe/core/config/builder.py:46`,
    # `vibe/core/config/vibe_schema.py:724`), so no document is refused for a
    # missing key.
    builder = ConfigBuilder(VibeConfigSchema)
    builder.add_layer(DefaultConfigLayer(schema=VibeConfigSchema))
    builder.add_layer(OverridesLayer(data=tomllib.loads(document), name="user"))
    return await builder.build()


def _config_view_outcome(config: Any) -> str:
    """Whether the whole ``ConfigView`` projects, which is what a client reads.

    Both audio surfaces reach a client through the projected view, so a view
    that raises leaves no session to configure at all, whatever the rest of the
    document says.
    """

    from vibe.app_server._projection import project_config_view

    try:
        project_config_view(config)
    except Exception as error:
        return type(error).__name__
    return "resolved"


def _transcription_resolution(config: Any) -> dict[str, Any]:
    from vibe.app_server._projection import (
        _project_transcribe_model,
        _project_transcribe_provider,
    )

    try:
        model = config.get_active_transcribe_model()
    except Exception as error:
        return {"outcome": "error", "error": {"stage": "model", "type": type(error).__name__}}
    try:
        provider = config.get_transcribe_provider_for_model(model)
    except Exception as error:
        return {
            "outcome": "error",
            "error": {"stage": "provider", "type": type(error).__name__},
        }
    view = _project_transcribe_model(model)
    provider_view = _project_transcribe_provider(provider)
    return {
        "outcome": "resolved",
        "model": {
            "name": view.name,
            "sampleRate": view.sample_rate,
            "encoding": view.encoding,
            "language": view.language,
            "targetStreamingDelayMs": view.target_streaming_delay_ms,
        },
        "provider": {
            "apiBase": provider_view.api_base,
            "apiKeyEnvVar": provider_view.api_key_env_var,
            "client": provider_view.client,
        },
    }


def _speech_resolution(config: Any) -> dict[str, Any]:
    from vibe.app_server._projection import _project_tts_model, _project_tts_provider

    try:
        model = config.get_active_tts_model()
    except Exception as error:
        return {"outcome": "error", "error": {"stage": "model", "type": type(error).__name__}}
    try:
        provider = config.get_tts_provider_for_model(model)
    except Exception as error:
        return {
            "outcome": "error",
            "error": {"stage": "provider", "type": type(error).__name__},
        }
    view = _project_tts_model(model)
    provider_view = _project_tts_provider(provider)
    return {
        "outcome": "resolved",
        "model": {
            "name": view.name,
            "voice": view.voice,
            "responseFormat": view.response_format,
        },
        "provider": {
            "apiBase": provider_view.api_base,
            "apiKeyEnvVar": provider_view.api_key_env_var,
            "client": provider_view.client,
        },
    }


def _credential(client: Any, env_var: str) -> dict[str, Any]:
    """Where the credential came from, never what it is."""

    resolved = getattr(client, "_api_key", "")
    if resolved == CREDENTIAL_SENTINEL:
        source = "environment"
    elif resolved == "":
        source = "unset"
    else:
        raise OracleError(
            f"the credential for {env_var!r} came from neither the environment nor an "
            "empty variable, so the capture cannot record its origin without reading it"
        )
    return {"envVar": env_var, "source": source}


async def _transcription_frame(config: Any) -> dict[str, Any]:
    """The websocket request the reference would open, taken before it opens.

    The SDK builds the URL, merges the model into its query and swaps the
    scheme, then hands the result to its websocket opener. Replacing that
    opener records the finished URL and its headers without a connection.
    """

    from vibe.app_server._projection import (
        _project_transcribe_model,
        _project_transcribe_provider,
    )
    from vibe.cli.transcribe.factory import make_transcribe_client
    from mistralai.extra.realtime import transcription as realtime_transcription

    model = config.get_active_transcribe_model()
    provider = config.get_transcribe_provider_for_model(model)
    model_view = _project_transcribe_model(model)
    provider_view = _project_transcribe_provider(provider)
    client = make_transcribe_client(provider_view, model_view)

    original = realtime_transcription.connect
    captured: dict[str, Any] = {}

    def intercept(url: str, *arguments: Any, **keywords: Any) -> Any:
        headers = keywords.get("additional_headers") or {}
        captured.update({"url": url, "headerNames": sorted(headers)})
        raise _Intercepted(captured)

    async def empty() -> Any:
        return
        yield b""  # pragma: no cover - the stream is never drawn from

    realtime_transcription.connect = intercept  # type: ignore[assignment]
    try:
        async for _event in client.transcribe(empty()):
            break
    except Exception:
        # The opener sits inside the SDK's own error handling, so the abort
        # arrives wrapped. What was captured before it is the observation.
        pass
    finally:
        realtime_transcription.connect = original  # type: ignore[assignment]
        await client.close()
    if not captured:
        raise OracleError("the transcription stream never reached its opener")
    frame = captured

    audio_format = getattr(client, "_audio_format")
    return {
        "endpointUrl": frame["url"],
        "headerNames": frame["headerNames"],
        "serverUrl": getattr(client, "_server_url"),
        "model": getattr(client, "_model_name"),
        "audioFormat": {
            "encoding": audio_format.encoding,
            "sampleRate": audio_format.sample_rate,
        },
        "targetStreamingDelayMs": getattr(client, "_target_streaming_delay_ms"),
        "credential": _credential(client, provider_view.api_key_env_var),
    }


async def _speech_frame(config: Any) -> dict[str, Any]:
    """The HTTP request the reference would post, taken before it posts it.

    The SDK builds a request object and hands it to one sender. Replacing that
    sender records the method, the URL and the body without a connection.
    """

    from vibe.app_server._projection import _project_tts_model, _project_tts_provider
    from vibe.cli.tts.factory import make_tts_client
    from mistralai.client.basesdk import BaseSDK

    model = config.get_active_tts_model()
    provider = config.get_tts_provider_for_model(model)
    model_view = _project_tts_model(model)
    provider_view = _project_tts_provider(provider)
    client = make_tts_client(provider_view, model_view)

    original = BaseSDK.do_request_async
    captured: dict[str, Any] = {}

    async def intercept(self: Any, *arguments: Any, **keywords: Any) -> Any:
        request = keywords.get("request") or arguments[1]
        body = request.content or b""
        captured.update({
            "method": request.method,
            "url": str(request.url),
            "body": json.loads(body) if body else {},
            "headerNames": sorted(name.lower() for name in request.headers),
        })
        raise _Intercepted(captured)

    BaseSDK.do_request_async = intercept  # type: ignore[assignment]
    try:
        await client.speak(SPEECH_INPUT)
    except Exception:
        # The sender sits inside the SDK's own error handling, so the abort may
        # arrive wrapped. What was captured before it is the observation.
        pass
    finally:
        BaseSDK.do_request_async = original  # type: ignore[assignment]
        await client.close()
    if not captured:
        raise OracleError("the speech call never reached its sender")
    frame = captured

    return {
        "method": frame["method"],
        "endpointUrl": frame["url"],
        "body": frame["body"],
        "serverUrl": getattr(client, "_server_url"),
        "model": getattr(client, "_model_name"),
        "voice": getattr(client, "_voice"),
        "responseFormat": getattr(client, "_response_format"),
        "credential": _credential(client, provider_view.api_key_env_var),
    }


# --------------------------------------------------------------------------
# The families
# --------------------------------------------------------------------------


def capture_constants() -> dict[str, Any]:
    from vibe.cli.audio_player.audio_player import (
        DEFAULT_BUFFER_MS,
        DEFAULT_SAMPLE_WIDTH,
    )
    from vibe.cli.voice_manager.voice_manager import TRANSCRIPTION_DRAIN_TIMEOUT
    from vibe.core.config.models import (
        TranscribeModelConfig,
        TranscribeProviderConfig,
        TTSModelConfig,
        TTSProviderConfig,
    )
    from vibe.core.config.vibe_schema import (
        DEFAULT_ACTIVE_TRANSCRIBE_MODEL_CONFIG,
        DEFAULT_ACTIVE_TTS_MODEL_CONFIG,
        DEFAULT_TRANSCRIBE_PROVIDERS,
        DEFAULT_TTS_PROVIDERS,
    )
    from vibe.utils.audio import RecordingMode

    def field_defaults(model: Any, names: tuple[str, ...]) -> dict[str, Any]:
        return {
            name: model.model_fields[name].default
            for name in names
            if name in model.model_fields
        }

    transcribe_provider = DEFAULT_TRANSCRIBE_PROVIDERS[0]
    tts_provider = DEFAULT_TTS_PROVIDERS[0]
    return {
        "transcribe": {
            "activeAlias": DEFAULT_ACTIVE_TRANSCRIBE_MODEL_CONFIG.alias,
            "modelName": DEFAULT_ACTIVE_TRANSCRIBE_MODEL_CONFIG.name,
            "providerName": transcribe_provider.name,
            "apiBase": transcribe_provider.api_base,
            "apiKeyEnvVar": transcribe_provider.api_key_env_var,
            "modelFieldDefaults": field_defaults(
                TranscribeModelConfig,
                ("sample_rate", "encoding", "language", "target_streaming_delay_ms"),
            ),
            "providerFieldDefaults": field_defaults(
                TranscribeProviderConfig, ("api_base", "api_key_env_var", "client")
            ),
        },
        "speech": {
            "activeAlias": DEFAULT_ACTIVE_TTS_MODEL_CONFIG.alias,
            "modelName": DEFAULT_ACTIVE_TTS_MODEL_CONFIG.name,
            "providerName": tts_provider.name,
            "apiBase": tts_provider.api_base,
            "apiKeyEnvVar": tts_provider.api_key_env_var,
            "modelFieldDefaults": field_defaults(
                TTSModelConfig, ("voice", "response_format")
            ),
            "providerFieldDefaults": field_defaults(
                TTSProviderConfig, ("api_base", "api_key_env_var", "client")
            ),
        },
        "vocabularies": {
            "audioClient": _literals("AudioClient"),
            "transcriptionEncoding": _literals("TranscriptionEncoding"),
            "speechOutputFormat": _literals("SpeechOutputFormat"),
            "recordingMode": [mode.value for mode in RecordingMode],
        },
        "transcriptionDrainTimeoutSeconds": TRANSCRIPTION_DRAIN_TIMEOUT,
        # The output device is sized in milliseconds since the reference moved
        # playback onto miniaudio; its sample format is an enum member written
        # at the call site rather than a constant, so it has nothing to capture.
        "playback": {
            "bufferMs": DEFAULT_BUFFER_MS,
            "sampleWidth": DEFAULT_SAMPLE_WIDTH,
        },
    }


def _literals(name: str) -> list[str]:
    """The values a `Literal` alias in the reference's value module declares."""

    import typing

    import vibe.config_values as values

    alias = getattr(values, name)
    arguments = typing.get_args(alias.__value__)
    return [str(argument) for argument in arguments]


async def capture_documents() -> tuple[
    list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]
]:
    _initialize_harness()
    transcription: list[dict[str, Any]] = []
    speech: list[dict[str, Any]] = []
    frames: list[dict[str, Any]] = []
    for document in DOCUMENTS:
        case = document["id"]
        try:
            config = await _build_config(document["toml"])
        except Exception as error:
            failure = {
                "case": case,
                "outcome": "error",
                "error": {"stage": "build", "type": type(error).__name__},
                "validationWarnings": 0,
                "configView": "unbuilt",
            }
            transcription.append(dict(failure))
            speech.append(dict(failure))
            continue
        warnings = len(config.validation_warnings)
        view = _config_view_outcome(config)
        transcribed = {
            "case": case,
            **_transcription_resolution(config),
            "validationWarnings": warnings,
            "configView": view,
        }
        spoken = {
            "case": case,
            **_speech_resolution(config),
            "validationWarnings": warnings,
            "configView": view,
        }
        transcription.append(transcribed)
        speech.append(spoken)
        frame: dict[str, Any] = {"case": case}
        if transcribed["outcome"] == "resolved":
            frame["transcription"] = await _transcription_frame(config)
        if spoken["outcome"] == "resolved":
            frame["speech"] = await _speech_frame(config)
        frames.append(frame)
    return transcription, speech, frames


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
            "also write the committed corpus, which the Rust replay reads "
            f"unconditionally (default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def build_corpus(reference: dict[str, str], families: dict[str, Any]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Voice corpus: what the pinned reference resolves an audio session from, per "
            "configuration document. Every document, alias, model name, provider name, host "
            "and variable is authored by scripts/parity/voice.py; the reference contributes "
            "resolved values, defaults and exception class names, and no reference-authored "
            "text, no reference source and no machine path is recorded. A failure is recorded "
            "as a stage and an exception class rather than a message, so the replay compares "
            "causes. The two wire frames are intercepted at the last call before the network, "
            "the websocket opener and the HTTP sender, so nothing is ever sent. No credential "
            "is recorded: every variable a document names carries a sentinel during the "
            "capture and is recorded as a variable name and an origin. The behavior families "
            "drive the reference's recorder, player, realtime client, dictation and read-aloud "
            "managers, turn summary service and editor bridge over stand-ins "
            "scripts/parity/voice_behavior.py authors; a text the reference writes is recorded "
            "as its length and SHA-256 only. Regenerate with "
            "scripts/parity/voice.py --corpus when the pinned reference moves."
        ),
        "documents": DOCUMENTS,
        **families,
    }


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
        os.environ[KEYRING_DISABLE_VARIABLE] = "1"
        for variable in CREDENTIAL_VARIABLES:
            os.environ[variable] = CREDENTIAL_SENTINEL
        GUARD.install()
        GUARD.verify()
        constants = capture_constants()
        transcription, speech, frames = asyncio.run(capture_documents())
        import voice_behavior

        behavior = asyncio.run(
            voice_behavior.capture_behavior(CREDENTIAL_SENTINEL, _build_config)
        )
    except OracleError as error:
        print(f"voice capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(f"voice capture failed: guarded calls {GUARD.attempts}", file=sys.stderr)
        return 1

    shortfalls = []
    if len(DOCUMENTS) < 20:
        shortfalls.append(f"documents holds {len(DOCUMENTS)} entries, under 20")
    if not any(record["outcome"] == "error" for record in transcription + speech):
        shortfalls.append("no document records a reference failure")
    if not any("transcription" in frame and "speech" in frame for frame in frames):
        shortfalls.append("no document records both wire frames")
    if shortfalls:
        print("voice capture failed: " + "; ".join(shortfalls), file=sys.stderr)
        return 1

    families = {
        "constants": constants,
        "transcriptionResolution": transcription,
        "speechResolution": speech,
        "wireFrames": frames,
        **behavior,
    }
    corpus = build_corpus(reference, families)

    # The credential never leaves the process. Recording one would be a leak
    # into a committed file, so the serialized corpus is searched for the
    # sentinel every variable carried before anything is written.
    serialized = json.dumps(corpus)
    if CREDENTIAL_SENTINEL in serialized:
        print(
            "voice capture failed: a resolved credential reached the corpus",
            file=sys.stderr,
        )
        return 1

    full = {
        **corpus,
        "reference": reference,
        "platform": platform.system().lower(),
        "python": platform.python_version(),
    }
    _write(arguments.output, full)
    if arguments.corpus is not None:
        _write(arguments.corpus, corpus)

    counted = {
        "transcriptionResolution": len(transcription),
        "speechResolution": len(speech),
        "wireFrames": sum(
            len([key for key in frame if key != "case"]) for frame in frames
        ),
        **{family: len(records) for family, records in behavior.items()},
    }
    total = sum(counted.values())
    print(
        f"captured {total} scenarios across {len(counted)} families plus the constants block "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family, count in counted.items():
        print(f"  {family}: {count}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


def _write(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="",
    )
    os.replace(staged, path)


if __name__ == "__main__":
    sys.exit(main())
