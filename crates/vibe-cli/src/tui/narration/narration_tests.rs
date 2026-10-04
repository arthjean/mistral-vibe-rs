//! The narrator's effects reach the session and the speech transport, and
//! their answers drive the same state machine back to idle.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::watch;
use vibe_app_server::client::PublicDispatch;
use vibe_core::telemetry::TelemetryRecord;
use vibe_voice::playback::{AudioOutput, DecodedAudio, Playback, PlaybackError};
use vibe_voice::speech::{SpeechClient, SpeechFailure, SpeechFuture};
use vibe_voice::{NarratorEffect, NarratorManager, NarratorState, SpeechEvent, SpeechManager};

use crate::tui::runtime::interactive_test_runtime;
use crate::tui::state::TuiState;

const FIXTURE_SUMMARY: &str = "wrote the parser";

/// A short mono WAV, which is what a speech response carries.
fn fixture_wav() -> Vec<u8> {
    let data: Vec<u8> = (0..64_i16)
        .flat_map(|index| (index * 100).to_le_bytes())
        .collect();
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16_u32.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body.extend_from_slice(&16_000_u32.to_le_bytes());
    body.extend_from_slice(&32_000_u32.to_le_bytes());
    body.extend_from_slice(&2_u16.to_le_bytes());
    body.extend_from_slice(&16_u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&u32::try_from(data.len()).unwrap_or_default().to_le_bytes());
    body.extend_from_slice(&data);
    let mut container = b"RIFF".to_vec();
    container.extend_from_slice(&u32::try_from(body.len()).unwrap_or_default().to_le_bytes());
    container.extend_from_slice(&body);
    container
}

/// A device that plays every buffer to its end at once.
struct InstantOutput;

impl AudioOutput for InstantOutput {
    fn start(&self, _audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        let (sender, receiver) = watch::channel(true);
        Ok(Playback::new(receiver, Box::new(sender)))
    }
}

/// An endpoint that answers every summary with the fixture and records it.
#[derive(Default)]
struct RecordingClient {
    spoken: Mutex<Vec<String>>,
}

impl SpeechClient for RecordingClient {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a> {
        self.spoken
            .lock()
            .expect("the speech log")
            .push(text.to_owned());
        Box::pin(async { Ok(fixture_wav()) })
    }
}

fn summarizing_state(name: &str) -> (TuiState, u64) {
    let mut state = TuiState::new(name);
    state.narrator = NarratorManager::new(true, true);
    state.narrator.on_turn_start("write the parser");
    state.narrator.on_assistant_text("done");
    let Some(NarratorEffect::Summarize { generation, .. }) = state.narrator.on_turn_end() else {
        panic!("an enabled narrator summarizes");
    };
    (state, generation)
}

fn summary(text: &str) -> Result<PublicDispatch, String> {
    Ok(PublicDispatch {
        result: BTreeMap::from([("summary".to_owned(), json!(text))]),
        notifications: Vec::new(),
    })
}

#[tokio::test]
async fn a_summary_is_spoken_and_its_playback_drives_the_state_machine() {
    let mut runtime = interactive_test_runtime("speech-wiring");
    let client = Arc::new(RecordingClient::default());
    runtime.speech = SpeechManager::scripted(client.clone(), Arc::new(InstantOutput));
    let (mut state, generation) = summarizing_state("speech-wiring");

    super::apply_summary(
        generation,
        summary(FIXTURE_SUMMARY),
        &mut runtime,
        &mut state,
    );
    let started = runtime.speech.next_event().await.expect("an event");
    assert_eq!(started, SpeechEvent::PlaybackStarted { generation });
    super::apply_speech_event(started, &mut state);
    assert_eq!(state.narrator.state(), NarratorState::Speaking);
    assert_eq!(
        *client.spoken.lock().expect("the speech log"),
        [FIXTURE_SUMMARY]
    );

    let finished = runtime.speech.next_event().await.expect("an event");
    assert_eq!(
        finished,
        SpeechEvent::Finished {
            generation,
            error: None
        }
    );
    super::apply_speech_event(finished, &mut state);
    assert_eq!(state.narrator.state(), NarratorState::Idle);
    assert_eq!(
        state.diagnostics().count(),
        0,
        "a spoken summary reports nothing"
    );
}

/// Reference `_on_turn_summary`: only `None` returns to idle, so an empty
/// summary is still spoken, and a failed call is no summary.
#[tokio::test]
async fn an_empty_summary_is_spoken_and_a_failed_one_is_not() {
    let mut runtime = interactive_test_runtime("speech-empty");
    let client = Arc::new(RecordingClient::default());
    runtime.speech = SpeechManager::scripted(client.clone(), Arc::new(InstantOutput));

    let (mut state, generation) = summarizing_state("speech-empty");
    super::apply_summary(generation, summary(""), &mut runtime, &mut state);
    assert_eq!(
        runtime.speech.next_event().await,
        Some(SpeechEvent::PlaybackStarted { generation })
    );
    assert_eq!(*client.spoken.lock().expect("the speech log"), [""]);

    let (mut failed, generation) = summarizing_state("speech-failed");
    super::apply_summary(
        generation,
        Err("the server is gone".to_owned()),
        &mut runtime,
        &mut failed,
    );
    assert_eq!(failed.narrator.state(), NarratorState::Idle);
    let null = Ok(PublicDispatch {
        result: BTreeMap::from([("summary".to_owned(), json!(null))]),
        notifications: Vec::new(),
    });
    let (mut absent, generation) = summarizing_state("speech-null");
    super::apply_summary(generation, null, &mut runtime, &mut absent);
    assert_eq!(absent.narrator.state(), NarratorState::Idle);
}

/// Reference `_speak_summary`'s failure path: the turn settles, the class of
/// the failure is what `vibe.read_aloud.ended` reports, and the operator is
/// told nothing.
#[test]
fn a_speech_failure_settles_the_turn_silently() {
    let (mut state, generation) = summarizing_state("speech-failure");
    super::apply_speech_event(
        SpeechEvent::Finished {
            generation,
            error: Some(SpeechFailure::from(PlaybackError::NoOutputDevice(
                "the host names none".to_owned(),
            ))),
        },
        &mut state,
    );
    assert_eq!(state.narrator.state(), NarratorState::Idle);
    assert_eq!(state.diagnostics().count(), 0);
    let ended = state
        .narrator
        .take_telemetry()
        .into_iter()
        .find_map(|record| match record {
            TelemetryRecord::ReadAloudEnded { error_type, .. } => Some(error_type),
            _ => None,
        });
    assert_eq!(ended, Some(Some("NoAudioOutputDeviceError".to_owned())));
}

/// A late answer whose generation has been superseded settles nothing.
#[test]
fn a_late_answer_from_a_superseded_generation_is_discarded() {
    let mut state = TuiState::new("speech-late");
    state.narrator = NarratorManager::new(true, true);
    state.narrator.on_turn_start("first");
    state.narrator.on_turn_end().expect("a first summary");
    state.narrator.cancel();
    state.narrator.on_turn_start("second");
    state.narrator.on_turn_end().expect("a second summary");

    super::apply_speech_event(SpeechEvent::PlaybackStarted { generation: 1 }, &mut state);
    assert_eq!(
        state.narrator.state(),
        NarratorState::Summarizing,
        "a stale playback never enters the speaking state"
    );
    super::apply_speech_event(
        SpeechEvent::Finished {
            generation: 1,
            error: None,
        },
        &mut state,
    );
    assert_eq!(
        state.narrator.state(),
        NarratorState::Summarizing,
        "a stale completion settles nothing"
    );
}
