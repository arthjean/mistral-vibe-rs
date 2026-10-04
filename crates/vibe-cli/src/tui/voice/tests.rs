//! The terminal's side of voice input: the shared manager's callbacks reach
//! the composer as its own events, tagged with the generation the recording
//! was started under, and its telemetry reaches the session's client.
//!
//! The lifecycle itself, its refusals and its four audio events are the
//! manager's, held by `vibe-voice`; these tests hold what this adapter adds.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use vibe_app_server::server::AppServer;
use vibe_app_server::workspace::{WorkspacePaths, WorkspaceService};
use vibe_voice::capture::{AudioRecorder, AudioStream, RecorderError};
use vibe_voice::transcribe::{TranscribeClient, TranscribeEvent, TranscribeFuture};

use super::{ClientFactory, VoiceManager};
use crate::tui::chat_input::{InputEffect, InputEvent};
use crate::tui::runtime::{interactive_test_runtime_with_server, no_credentials};

/// A microphone that streams nothing and reports the level it was given.
struct FakeRecorder {
    level: f32,
    stream: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl FakeRecorder {
    fn new(level: f32) -> Arc<Self> {
        Arc::new(Self {
            level,
            stream: Mutex::new(None),
        })
    }
}

impl AudioRecorder for FakeRecorder {
    fn start(&self, _sample_rate: u32) -> Result<AudioStream, RecorderError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.stream.lock().expect("the stream lock") = Some(sender);
        Ok(receiver)
    }

    fn stop(&self) -> Duration {
        self.cancel();
        Duration::from_secs(1)
    }

    fn cancel(&self) {
        self.stream.lock().expect("the stream lock").take();
    }

    fn peak(&self) -> f32 {
        self.level
    }

    fn has_signal(&self) -> bool {
        true
    }
}

/// An endpoint that names the session, says one word, and finishes once the
/// recording has.
struct FakeClient;

impl TranscribeClient for FakeClient {
    fn transcribe(
        &self,
        mut audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture {
        Box::pin(async move {
            let _ = events.send(TranscribeEvent::SessionCreated {
                request_id: "req-1".to_owned(),
            });
            let _ = events.send(TranscribeEvent::TextDelta("hello".to_owned()));
            while audio.recv().await.is_some() {}
            let _ = events.send(TranscribeEvent::Done);
        })
    }
}

/// A factory that counts the clients it built.
fn counting_factory() -> (ClientFactory, Arc<AtomicUsize>) {
    let built = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&built);
    let factory: ClientFactory = Arc::new(move |_: &Value| {
        counter.fetch_add(1, Ordering::SeqCst);
        Some(Arc::new(FakeClient) as Arc<dyn TranscribeClient>)
    });
    (factory, built)
}

fn view(api_key_env_var: &str) -> Value {
    json!({"transcription": {
        "model": {"name": "fixture-model", "sampleRate": 16_000},
        "provider": {"apiBase": "wss://gateway.fixture.invalid", "apiKeyEnvVar": api_key_env_var},
    }})
}

fn manager(enabled: bool) -> (VoiceManager, Arc<AtomicUsize>) {
    let (factory, built) = counting_factory();
    let manager = VoiceManager::new(
        &view(""),
        FakeRecorder::new(0.5),
        factory,
        no_credentials(),
        enabled,
    );
    (manager, built)
}

/// Collects composer events until `last` arrives.
async fn events_until(
    manager: &mut VoiceManager,
    last: impl Fn(&InputEvent) -> bool,
) -> Vec<InputEvent> {
    let mut events = Vec::new();
    for _ in 0..400 {
        while let Some(event) = manager.try_next_event() {
            let done = last(&event);
            events.push(event);
            if done {
                return events;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the expected event never arrived: {events:?}");
}

fn telemetry_names(manager: &mut VoiceManager) -> Vec<&'static str> {
    manager
        .take_telemetry()
        .iter()
        .map(|record| record.event().event_name())
        .collect()
}

#[tokio::test]
async fn a_recording_reaches_the_composer_under_its_generation() {
    let (mut manager, _) = manager(true);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 7);
    let started = events_until(&mut manager, |event| {
        matches!(event, InputEvent::VoiceTranscriptDelta { .. })
    })
    .await;
    assert_eq!(
        started,
        [
            InputEvent::VoiceStartResolved {
                generation: 7,
                error: None
            },
            InputEvent::VoiceTranscriptDelta {
                text: "hello".to_owned(),
                generation: 7
            },
        ]
    );

    manager.apply_effects(&[InputEffect::RecordingStopRequested], 7);
    let stopped = events_until(&mut manager, |event| {
        matches!(event, InputEvent::VoiceDone { .. })
    })
    .await;
    assert_eq!(
        stopped,
        [
            InputEvent::VoiceStopResolved {
                generation: 7,
                error: None
            },
            InputEvent::VoiceDone { generation: 7 },
        ]
    );
    assert_eq!(
        telemetry_names(&mut manager),
        [
            "vibe.audio.transcription.start",
            "vibe.audio.transcription.done"
        ]
    );
}

/// Reference `cancel_recording`: the manager returns to idle at once, under
/// the generation the composer already left.
#[tokio::test]
async fn a_cancelled_recording_settles_under_its_old_generation() {
    let (mut manager, _) = manager(true);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    events_until(&mut manager, |event| {
        matches!(event, InputEvent::VoiceStartResolved { .. })
    })
    .await;
    manager.apply_effects(&[InputEffect::RecordingCancelRequested], 2);
    let events = events_until(&mut manager, |event| {
        matches!(event, InputEvent::VoiceDone { .. })
    })
    .await;
    assert_eq!(
        events.last(),
        Some(&InputEvent::VoiceDone { generation: 1 })
    );
    assert!(telemetry_names(&mut manager).contains(&"vibe.audio.transcription.cancel_recording"));
}

#[tokio::test]
async fn a_refused_start_is_answered_with_the_reason() {
    let (mut disabled, _) = manager(false);
    disabled.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    assert_eq!(
        events_until(&mut disabled, |_| true).await,
        [InputEvent::VoiceStartResolved {
            generation: 1,
            error: Some("Voice mode is disabled".to_owned())
        }]
    );

    // Reference `RecordingStartError("Transcribe client is not available")`
    // for a configuration that resolves no client.
    let mut unconfigured = VoiceManager::new(
        &json!({}),
        FakeRecorder::new(0.0),
        Arc::new(|_: &Value| None),
        no_credentials(),
        true,
    );
    unconfigured.apply_effects(&[InputEffect::RecordingStartRequested], 2);
    assert_eq!(
        events_until(&mut unconfigured, |_| true).await,
        [InputEvent::VoiceStartResolved {
            generation: 2,
            error: Some("Transcribe client is not available".to_owned())
        }]
    );

    // The credential variable is read from the configuration as it stands at
    // the start.
    let (mut keyless, _) = manager(true);
    keyless.resync(&view("FIXTURE_UNSET_KEY"));
    keyless.apply_effects(&[InputEffect::RecordingStartRequested], 3);
    assert_eq!(
        events_until(&mut keyless, |_| true).await,
        [InputEvent::VoiceStartResolved {
            generation: 3,
            error: Some("Voice transcription needs an API key: set FIXTURE_UNSET_KEY".to_owned())
        }]
    );
    assert!(
        keyless.take_telemetry().is_empty(),
        "a refused start reports nothing"
    );
}

/// Reference `LazyVoiceManager`: the client is built the first time voice
/// mode is on and kept afterward, whatever the configuration becomes.
#[tokio::test]
async fn the_client_is_built_once_when_voice_mode_first_turns_on() {
    let (mut manager, built) = manager(false);
    assert_eq!(
        built.load(Ordering::SeqCst),
        0,
        "nothing is built while off"
    );
    manager.set_enabled(false);
    assert_eq!(built.load(Ordering::SeqCst), 0);
    manager.set_enabled(true);
    assert_eq!(built.load(Ordering::SeqCst), 1);
    manager.resync(&view(""));
    manager.set_enabled(true);
    manager.apply_effects(&[InputEffect::RecordingStartRequested], 1);
    events_until(&mut manager, |event| {
        matches!(event, InputEvent::VoiceStartResolved { .. })
    })
    .await;
    assert_eq!(built.load(Ordering::SeqCst), 1);

    let (_, enabled_built) = self::manager(true);
    assert_eq!(
        enabled_built.load(Ordering::SeqCst),
        1,
        "built at once when on"
    );
}

/// Reference `RecordingIndicator._poll_peak`:
/// `min(int(peak * len(PEAK_BLOCKS)), len(PEAK_BLOCKS) - 1)`.
#[tokio::test]
async fn the_peak_is_drawn_on_eight_levels() {
    for (level, expected) in [(0.0, 0), (0.5, 4), (0.99, 7), (1.0, 7)] {
        let (factory, _) = counting_factory();
        let manager = VoiceManager::new(
            &view(""),
            FakeRecorder::new(level),
            factory,
            no_credentials(),
            true,
        );
        assert_eq!(manager.peak_level(), expected, "peak {level}");
    }
    let (manager, _) = manager(false);
    assert_eq!(manager.peak_level(), 0, "no manager reads silence");
}

/// The loop's recorder drains every queued event and leaves nothing in the
/// transcript, whatever the telemetry client does with it. Where the events
/// go, and the `enable_telemetry` gate that decides whether they travel at
/// all, are the client's own and are held by `telemetry_tests` one layer down.
#[tokio::test(flavor = "multi_thread")]
async fn the_recorder_drains_every_event_without_touching_the_transcript() {
    let temporary = tempfile::tempdir().expect("a temporary vibe home");
    let vibe_home = temporary.path().join("vibe-home");
    fs::create_dir_all(&vibe_home).expect("the vibe home is created");
    let service = WorkspaceService::new(
        WorkspacePaths {
            session_root: vibe_home.join("sessions"),
            working_directory: temporary.path().join("workspace"),
            vibe_home,
        },
        true,
    )
    .expect("the configuration service builds");
    let mut runtime = interactive_test_runtime_with_server(
        "audio-telemetry",
        AppServer::with_workspace_service(service),
    );
    runtime.voice = manager(true).0;

    runtime
        .voice
        .apply_effects(&[InputEffect::RecordingStartRequested], 1);
    events_until(&mut runtime.voice, |event| {
        matches!(event, InputEvent::VoiceTranscriptDelta { .. })
    })
    .await;
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
            .is_some_and(|message| message.contains("vibe.audio.transcription.start"))),
        "an audio event is telemetry, not a diagnostic the operator reads"
    );
}
