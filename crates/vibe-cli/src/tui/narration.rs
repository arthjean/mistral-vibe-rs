//! Executing the effects the narrator produces, and settling what the speech
//! transport answers.
//!
//! [`narrator`] owns the state machine; this module is the only place that
//! turns its effects into calls on the session and on the audio transport.

use serde_json::{Value, json};
use vibe_app_server::client::PublicDispatch;
use vibe_voice::narrator;

use super::runtime::{InteractiveRuntime, UiOperation, UiOperationCompletion};
use super::state::TuiState;
use super::voice::SpeechEvent;

pub(super) fn apply_narrator_effect(
    effect: narrator::NarratorEffect,
    runtime: &mut InteractiveRuntime,
) {
    match effect {
        // Reference `cancel`: the summary task is cancelled and playback
        // stops before the machine returns to idle.
        narrator::NarratorEffect::Stop => {
            if let Some(summary) = runtime.narration_summary.take() {
                summary.abort();
            }
            runtime.speech.stop();
        }
        // Reference `TurnSummaryTracker._generate_summary`: the summary is
        // generated beside the event loop, and a failure is no summary.
        narrator::NarratorEffect::Summarize {
            generation,
            user_message,
            assistant_text,
            error,
            message_id,
        } => {
            if let Some(previous) = runtime.narration_summary.take() {
                previous.abort();
            }
            let pending = runtime.service.begin_public_call(
                "narration/summarize",
                json!({
                    "sessionId": runtime.session_id,
                    "userMessage": user_message,
                    "assistantText": assistant_text,
                    "error": error,
                    "messageId": message_id,
                }),
            );
            let sender = runtime.ui_operation_sender.clone();
            let task = tokio::spawn(async move {
                let result = match pending {
                    Ok(pending) => pending.complete().await.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let _ = sender.send(UiOperationCompletion {
                    generation: None,
                    operation: UiOperation::NarrationSummary(generation),
                    result,
                });
            });
            runtime.narration_summary = Some(task.abort_handle());
        }
        narrator::NarratorEffect::Speak { generation, text } => {
            runtime.speech.speak(generation, text);
        }
    }
}

/// Reference `_on_turn_summary`: the summary the server answered, or none when
/// the call failed or the server could not make one.
pub(super) fn apply_summary(
    generation: u64,
    result: Result<PublicDispatch, String>,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    runtime.narration_summary = None;
    let summary = result.ok().and_then(|dispatch| {
        dispatch
            .result
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

/// Applies one answer from the speech transport. The generation each answer
/// carries is what the state machine discards a superseded turn by, so a result
/// that outlived its turn settles nothing and plays nothing.
pub(super) fn apply_speech_event(event: SpeechEvent, state: &mut TuiState) {
    match event {
        SpeechEvent::PlaybackStarted { generation } => state.narrator.playback_started(generation),
        SpeechEvent::Finished { generation, error } => {
            match error {
                // Reference `_speak_summary` logs the failure and reports the
                // exception's class name in `vibe.read_aloud.ended`; the
                // operator is told nothing.
                Some(failure) => state.narrator.fail(generation, failure.class),
                None => state.narrator.settle(generation),
            }
        }
    }
}

/// Sends the read-aloud events the narrator produced, on the same terms as the
/// transcription ones.
pub(super) fn record_narrator_telemetry(runtime: &InteractiveRuntime, state: &mut TuiState) {
    for record in &state.narrator.take_telemetry() {
        runtime.report(record);
    }
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

#[cfg(test)]
mod narration_tests;
