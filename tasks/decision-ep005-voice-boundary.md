# EP-005 Voice Boundary Decision

## Status

Approved and implemented on 2026-08-02. Production voice remains owned by `vibe-cli`; no change to `vibe-core`, `vibe-app-server`, or `vibe-protocol` was required for US-016.

## Finding

The first spike proved only a single final-transcript trace. The pinned Python oracle is stricter: microphone PCM is streamed while recording, text deltas are inserted immediately, stop enters a flushing state for at most 10 seconds, and only the terminal transcription event returns the composer to idle.

At the time of the spike, the Rust reducer was incomplete for production parity:

- `InputEvent::Transcript` returned to `Idle` on the first delta, so every later realtime delta was rejected as stale.
- No distinct terminal transcription event existed.
- The renderer exposed a fixed recording glyph but no microphone peak or flushing animation.
- The effect runner had no production owner for start, stop, cancel, timeout, WebSocket events, or audio-device errors.

These are local `vibe-cli` gaps. They do not justify a new protocol or app-server boundary.

## Smallest production design

Keep three responsibilities under a `vibe-cli` voice module:

1. `CpalAudioRecorder`: capture the default input with `cpal`, convert or downmix supported device samples to mono PCM signed 16-bit little-endian, publish peak level, and forward chunks through a bounded non-blocking Tokio channel.
2. `MistralRealtimeTranscriber`: connect to `wss://api.mistral.ai/v1/audio/transcriptions/realtime?model=...` with the existing credential, send `session.update`, base64 JSON `input_audio.append` messages, then `input_audio.flush` and `input_audio.end`; parse `session.created`, `transcription.text.delta`, `transcription.done`, and `error`.
3. `VoiceManager`: own generation, start, stop, cancel, the 10-second drain timeout, task cleanup, and typed updates back to the reducer.

Extend the reducer boundary with a delta event that preserves `Recording` or `Transcribing`, a terminal done event that returns to `Idle`, and a peak observation used only by rendering. Generation invalidation remains the protection against late results.

The minimum new production dependencies are:

- `cpal` for cross-platform microphone input.
- `tokio-tungstenite` with a rustls feature for the secure realtime WebSocket.

Existing `base64`, `futures-util`, `serde`, `serde_json`, `tokio`, and `url` dependencies cover the remaining wire and task work. No WAV or batch-transcription dependency is needed. Workspace convention means both the root dependency table and `vibe-cli/Cargo.toml` must change.

## Configuration and platform constraints

Use the Python defaults when the generic Rust config has no explicit transcription section: model `voxtral-mini-transcribe-realtime-2602`, 16 kHz, mono `pcm_s16le`, and 500 ms target delay. The existing `config/read` boundary can supply `voice_mode_enabled` and any future explicit overrides. The runtime must give the voice manager a secret credential without exposing it through the public config snapshot.

`cpal` supports ALSA on Linux, CoreAudio on macOS, and WASAPI on Windows. Linux builds require ALSA development files. This packaging consequence belongs to the dependency approval; native cross-platform certification remains deferred with US-019.

## Decision

The two dependencies and root manifest change were approved. US-016 implements the realtime design above in `vibe-cli`; the batch REST fallback was rejected because it cannot reproduce live deltas or flushing semantics.

## Primary sources

- Pinned Python oracle: `vibe/cli/audio_recorder/audio_recorder.py`, `vibe/cli/voice_manager/voice_manager.py`, and `vibe/cli/transcribe/mistral_transcribe_client.py` at `99a6efa9ca1fb48671adebe0f6f5d931945bd8c9`.
- Mistral realtime transcription: https://docs.mistral.ai/studio-api/audio/speech_to_text/realtime_transcription
- Mistral Python SDK v2.6.0 wire implementation: https://github.com/mistralai/client-python/tree/v2.6.0/src/mistralai/extra/realtime
- CPAL platform and build requirements: https://docs.rs/crate/cpal/latest
- Tokio WebSocket transport and TLS features: https://github.com/snapview/tokio-tungstenite
