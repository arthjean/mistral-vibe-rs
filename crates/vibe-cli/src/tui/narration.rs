//! Executing the effects the narrator produces, and settling what the speech
//! transport answers.
//!
//! [`super::narrator`] owns the state machine; this module is the only place
//! that turns its effects into calls on the session and on the audio transport.

use serde_json::{Value, json};

use super::narrator;
use super::runtime::InteractiveRuntime;
use super::state::TuiState;
use super::voice::SpeechEvent;

pub(super) fn apply_narrator_effect(
    effect: narrator::NarratorEffect,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    match effect {
        // Reference `cancel`: playback stops before the machine returns to idle.
        narrator::NarratorEffect::Stop => runtime.speech.stop(),
        narrator::NarratorEffect::Summarize {
            generation,
            user_message,
            assistant_text,
            ..
        } => {
            let summary = runtime
                .service
                .public_call(
                    "narration/summarize",
                    json!({
                        "sessionId": runtime.session_id,
                        "userMessage": user_message,
                        "assistantText": assistant_text,
                    }),
                )
                .ok()
                .and_then(|result| {
                    result
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                });
            if let Some(narrator::NarratorEffect::Speak { generation, text }) =
                state.narrator.apply_summary(generation, summary)
            {
                runtime.speech.speak(generation, text);
            }
        }
        narrator::NarratorEffect::Speak { generation, text } => {
            runtime.speech.speak(generation, text);
        }
    }
}

/// Applies one answer from the speech transport. The generation each answer
/// carries is what the state machine discards a superseded turn by, so a result
/// that outlived its turn settles nothing and plays nothing.
pub(super) fn apply_speech_event(event: SpeechEvent, state: &mut TuiState) {
    match event {
        SpeechEvent::PlaybackStarted { generation } => state.narrator.playback_started(generation),
        SpeechEvent::Finished { generation, error } => {
            match error {
                // Reference `_speak_summary` reports the exception's class
                // name; this port's transport answers a message rather than an
                // exception, so the class it names is the failure itself.
                Some(failure) => {
                    state.narrator.fail(generation, SPEECH_ERROR_CLASS);
                    report_speech_failure(state, failure);
                }
                None => state.narrator.settle(generation),
            }
        }
    }
}

/// What a read-aloud failure reports as its error type.
const SPEECH_ERROR_CLASS: &str = "SpeechError";

/// Sends the read-aloud events the narrator produced, on the same terms as the
/// transcription ones.
pub(super) fn record_narrator_telemetry(runtime: &InteractiveRuntime, state: &mut TuiState) {
    for record in &state.narrator.take_telemetry() {
        runtime.report(record);
    }
}

/// Reports a speech failure once per session. An unconfigured model, an absent
/// output device and an endpoint that refuses the request are all the same fact
/// on every following turn, so the operator is told once rather than once per
/// turn, and the turn itself stays successful.
pub(super) fn report_speech_failure(state: &mut TuiState, failure: String) {
    if state.speech_notice_shown {
        return;
    }
    state.speech_notice_shown = true;
    state.push_diagnostic(failure);
}

/// Sends the audio lifecycle events the voice manager produced.
///
/// The reference hands each one to the agent loop's telemetry client
/// (`vibe/cli/voice_manager/voice_manager.py:202-251`), and so does this port:
/// the same client, the same census and the same `enable_telemetry` gate as
/// every other event. A delivery failure is never surfaced to the operator:
/// telemetry is best effort on both sides, and a diagnostic here would put an
/// audio event in the transcript.
pub(super) fn record_audio_telemetry(runtime: &mut InteractiveRuntime) {
    for record in &runtime.voice.take_telemetry() {
        runtime.report(record);
    }
}
