//! US-215: the four audio events the reference emits around a transcription.
//!
//! `vibe/cli/voice_manager/voice_manager.py:202-251` names each event and its
//! exact property set, and `vibe/cli/voice_manager/telemetry.py:8-30` supplies
//! the values from a tracking record. These tests drive the manager the way the
//! event loop drives it, then follow one recorded event to the telemetry
//! client the loop hands it to, which is where `enable_telemetry` decides
//! whether anything is sent.

use std::fs;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_app_server::server::AppServer;

use crate::tui::chat_input::{InputEffect, InputEvent};
use crate::tui::runtime::interactive_test_runtime_with_server;

use vibe_core::telemetry::TelemetryRecord;

use super::{VoiceControl, VoiceManager, VoiceSessionFactory, VoiceSignal};

const TRANSCRIPTION_START: &str = "vibe.audio.transcription.start";
const TRANSCRIPTION_CANCEL: &str = "vibe.audio.transcription.cancel_recording";
const TRANSCRIPTION_DONE: &str = "vibe.audio.transcription.done";
const TRANSCRIPTION_ERROR: &str = "vibe.audio.transcription.error";

const RECORDING_ID: &str = "req-00000000";

/// A session that reports its identity and then does exactly what the test
/// tells it to, so every event below is produced by the manager rather than by
/// the fixture.
struct SignallingFactory;

impl VoiceSessionFactory for SignallingFactory {
    fn spawn(
        &self,
        generation: u64,
        _updates: mpsc::Sender<InputEvent>,
        signals: mpsc::Sender<VoiceSignal>,
        mut control: watch::Receiver<VoiceControl>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let _ = signals
                .send(VoiceSignal::SessionCreated {
                    generation,
                    recording_id: RECORDING_ID.to_owned(),
                })
                .await;
            while control.changed().await.is_ok() {
                if *control.borrow() != VoiceControl::Running {
                    return;
                }
            }
        })
    }
}

fn manager() -> VoiceManager {
    VoiceManager::new(Arc::new(SignallingFactory), true)
}

/// Starts a recording through the effect the composer emits, then settles the
/// session-created signal the endpoint answers with.
async fn start(manager: &mut VoiceManager, generation: u64) -> Vec<TelemetryRecord> {
    manager.apply_effects(&[InputEffect::RecordingStartRequested], generation);
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let events = manager.take_telemetry();
        if !events.is_empty() {
            return events;
        }
    }
    panic!("the session-created signal never arrived");
}

/// Hands the manager an event the session would have sent and drains it
/// through the same reader the event loop calls.
fn feed(manager: &mut VoiceManager, event: InputEvent) -> Vec<TelemetryRecord> {
    manager.inject_for_test(event);
    while manager.try_next_event().is_some() {}
    manager.take_telemetry()
}

fn names(events: &[TelemetryRecord]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| event.event().event_name())
        .collect()
}

/// The properties one record puts on the wire, which is what the reference's
/// call site decides.
fn properties(record: &TelemetryRecord) -> serde_json::Map<String, Value> {
    record
        .attributes(None)
        .expect("an audio payload carries no unsafe label")
        .into_properties()
}

fn keys(record: &TelemetryRecord) -> Vec<String> {
    properties(record).keys().cloned().collect()
}

/// A completed transcription reports its identity, its accumulated length and
/// both durations, in the order the reference emits them.
#[tokio::test(flavor = "multi_thread")]
async fn a_completed_transcription_reports_start_then_done() {
    let mut manager = manager();
    let started = start(&mut manager, 1).await;
    assert_eq!(names(&started), [TRANSCRIPTION_START]);
    assert_eq!(
        properties(&started[0]).get("recording_id"),
        Some(&json!(RECORDING_ID))
    );
    assert_eq!(keys(&started[0]), ["recording_id"]);

    feed(
        &mut manager,
        InputEvent::VoiceTranscriptDelta {
            text: "hello ".to_owned(),
            generation: 1,
        },
    );
    feed(
        &mut manager,
        InputEvent::VoiceTranscriptDelta {
            text: "world".to_owned(),
            generation: 1,
        },
    );
    assert!(
        feed(
            &mut manager,
            InputEvent::VoiceStopResolved {
                generation: 1,
                error: None,
            }
        )
        .is_empty(),
        "a clean stop takes the recording duration and emits nothing of its own"
    );

    let done = feed(&mut manager, InputEvent::VoiceDone { generation: 1 });
    assert_eq!(names(&done), [TRANSCRIPTION_DONE]);
    let mut keys = keys(&done[0]);
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "recording_duration_ms",
            "recording_id",
            "transcript_length",
            "transcription_duration_ms"
        ]
    );
    let done = properties(&done[0]);
    assert_eq!(
        done.get("transcript_length"),
        Some(&json!(11)),
        "every delta accumulates, as `record_text` does"
    );
    assert!(
        done["recording_duration_ms"].is_number() && done["transcription_duration_ms"].is_number(),
        "both durations are reported: {done:?}"
    );
}

/// Cancelling reports the recording it cancelled and how long it had run.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_recording_reports_the_elapsed_time() {
    let mut manager = manager();
    start(&mut manager, 7).await;

    manager.apply_effects(&[InputEffect::RecordingCancelRequested], 7);
    let cancelled = manager.take_telemetry();

    assert_eq!(names(&cancelled), [TRANSCRIPTION_CANCEL]);
    let mut keys = keys(&cancelled[0]);
    keys.sort_unstable();
    assert_eq!(keys, ["recording_duration_ms", "recording_id"]);
    let cancelled = properties(&cancelled[0]);
    assert_eq!(cancelled.get("recording_id"), Some(&json!(RECORDING_ID)));
    assert!(cancelled["recording_duration_ms"].is_number());

    manager.apply_effects(&[InputEffect::RecordingCancelRequested], 7);
    assert!(
        manager.take_telemetry().is_empty(),
        "a second cancellation with nothing running emits nothing, as the \
         reference's early return does"
    );
}

/// A transcription that fails while the microphone is still open reports no
/// recording duration, which is the `None` the reference carries.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_transcription_reports_the_message_and_a_null_recording_duration() {
    let mut manager = manager();
    start(&mut manager, 3).await;

    let failed = feed(
        &mut manager,
        InputEvent::VoiceStopResolved {
            generation: 3,
            error: Some("Transcription timed out".to_owned()),
        },
    );

    assert_eq!(names(&failed), [TRANSCRIPTION_ERROR]);
    let mut keys = keys(&failed[0]);
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "error_message",
            "recording_duration_ms",
            "recording_id",
            "transcription_duration_ms"
        ]
    );
    let failed = properties(&failed[0]);
    assert_eq!(
        failed.get("error_message"),
        Some(&json!("Transcription timed out"))
    );
    assert_eq!(
        failed.get("recording_duration_ms"),
        Some(&Value::Null),
        "the recorder never stopped cleanly, so no recording duration was taken"
    );
}

/// A start that never resolved emits nothing: the reference raises its
/// `RecordingStartError` to the caller instead of recording an event.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_start_records_nothing() {
    let mut manager = VoiceManager::new(Arc::new(SignallingFactory), false);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(manager.take_telemetry().is_empty());
}

/// The recording id is the one the endpoint named, read off the same frame the
/// reference reads `event.session.request_id` from.
#[tokio::test(flavor = "multi_thread")]
async fn the_recording_id_comes_from_the_session_created_frame() {
    for (frame, expected) in [
        (
            json!({"type": "session.created", "session": {"request_id": "req-live"}}),
            "req-live",
        ),
        (json!({"type": "session.created"}), ""),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a fixture endpoint");
        let address = listener.local_addr().expect("the fixture address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("client connection");
            let mut socket = accept_async(stream).await.expect("websocket handshake");
            socket
                .send(Message::Text(frame.to_string().into()))
                .await
                .expect("session created");
            let _ = socket.next().await.expect("session update");
        });
        let config =
            super::realtime::VoiceConfig::resolve(&super::settings::TranscriptionSettings {
                model: "fixture-transcribe-model".to_owned(),
                sample_rate: 16_000,
                encoding: "pcm_s16le".to_owned(),
                target_streaming_delay_ms: 500,
                api_base: format!("http://{address}"),
                api_key_env_var: String::new(),
            })
            .expect("the fixture endpoint resolves");
        let prepared = super::realtime::prepare_transcription(
            &secrecy::SecretString::from("test-key".to_owned()),
            &config,
        )
        .await
        .expect("the session opens");
        assert_eq!(prepared.recording_id(), expected);
        server.await.expect("the fixture endpoint closes");
    }
}

/// US-012: the loop's recorder drains every queued event and leaves nothing in
/// the transcript, whatever the telemetry client does with it. Where the events
/// go, and the `enable_telemetry` gate that decides whether they travel at all,
/// are the client's own and are held by `telemetry_tests` one layer down.
#[tokio::test(flavor = "multi_thread")]
async fn the_recorder_drains_every_event_without_touching_the_transcript() {
    let temporary = tempfile::tempdir().expect("a temporary vibe home");
    let vibe_home = temporary.path().join("vibe-home");
    fs::create_dir_all(&vibe_home).expect("the vibe home is created");
    let service = Release3Service::new(
        Release3Paths {
            session_root: vibe_home.join("sessions"),
            working_directory: temporary.path().join("workspace"),
            vibe_home,
        },
        true,
    )
    .expect("the configuration service builds");
    let mut runtime = interactive_test_runtime_with_server(
        "audio-telemetry",
        AppServer::with_release3_service(service),
    );
    runtime.voice = manager();

    // The events are left queued rather than drained here, so the loop's own
    // recorder is what carries them.
    runtime
        .voice
        .apply_effects(&[InputEffect::RecordingStartRequested], 1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    crate::tui::narration::record_audio_telemetry(&mut runtime);

    assert!(
        runtime.voice.take_telemetry().is_empty(),
        "the recorder takes every queued event"
    );
    let logs = runtime
        .service
        .public_call(
            "diagnostics/logs/read",
            json!({"sessionId": runtime.session_id}),
        )
        .expect("the log page reads");
    let entries = logs["logs"]["entries"].as_array().expect("a log page");
    assert!(
        !entries.iter().any(|entry| entry["message"]
            .as_str()
            .is_some_and(|message| message.contains(TRANSCRIPTION_START))),
        "an audio event is telemetry, not a diagnostic the operator reads"
    );
}
