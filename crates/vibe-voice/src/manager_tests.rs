//! The voice manager over a scripted recorder and a scripted transcribe
//! client, which is everything a recording does short of the hardware and the
//! network.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use vibe_core::telemetry::TelemetryRecord;

use super::*;
use crate::capture::{AudioRecorder, AudioStream, RecorderError};
use crate::transcribe::{TranscribeClient, TranscribeEvent, TranscribeFuture};

/// A recorder whose outcome every test sets.
#[derive(Default)]
pub(crate) struct ScriptedRecorder {
    pub(crate) refusal: Mutex<Option<RecorderError>>,
    pub(crate) signal: AtomicBool,
    pub(crate) duration: Mutex<Duration>,
    pub(crate) chunks: Mutex<Vec<Vec<u8>>>,
    sender: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    pub(crate) starts: AtomicUsize,
    pub(crate) cancels: AtomicUsize,
    pub(crate) rates: Mutex<Vec<u32>>,
}

impl ScriptedRecorder {
    pub(crate) fn with_signal(signal: bool, duration: Duration) -> Arc<Self> {
        let recorder = Self::default();
        recorder.signal.store(signal, Ordering::Relaxed);
        *recorder.duration.lock().unwrap() = duration;
        Arc::new(recorder)
    }
}

impl AudioRecorder for ScriptedRecorder {
    fn start(&self, sample_rate: u32) -> Result<AudioStream, RecorderError> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        self.rates.lock().unwrap().push(sample_rate);
        if let Some(refusal) = self.refusal.lock().unwrap().clone() {
            return Err(refusal);
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        for chunk in self.chunks.lock().unwrap().iter() {
            let _ = sender.send(chunk.clone());
        }
        *self.sender.lock().unwrap() = Some(sender);
        Ok(receiver)
    }

    fn stop(&self) -> Duration {
        match self.sender.lock().unwrap().take() {
            Some(_) => *self.duration.lock().unwrap(),
            None => Duration::ZERO,
        }
    }

    fn cancel(&self) {
        if self.sender.lock().unwrap().take().is_some() {
            self.cancels.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn peak(&self) -> f32 {
        0.5
    }

    fn has_signal(&self) -> bool {
        self.signal.load(Ordering::Relaxed)
    }
}

/// What the scripted client does, in order.
#[derive(Clone, Debug)]
pub(crate) enum ClientStep {
    Emit(TranscribeEvent),
    /// Waits for the recorder's stream to end.
    AwaitAudioEnd,
    /// Never settles.
    Hang,
}

pub(crate) struct ScriptedClient {
    pub(crate) steps: Vec<ClientStep>,
}

impl TranscribeClient for ScriptedClient {
    fn transcribe(
        &self,
        mut audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture {
        let steps = self.steps.clone();
        Box::pin(async move {
            for step in steps {
                match step {
                    ClientStep::Emit(event) => {
                        let _ = events.send(event);
                    }
                    ClientStep::AwaitAudioEnd => while audio.recv().await.is_some() {},
                    ClientStep::Hang => std::future::pending::<()>().await,
                }
            }
        })
    }
}

fn credentials() -> CredentialLookup {
    Arc::new(|name: &str| (name == "FIXTURE_KEY").then(|| "secret".to_owned()))
}

fn request() -> StartRequest {
    StartRequest {
        tag: 7,
        sample_rate: 16_000,
        api_key_env_var: "FIXTURE_KEY".to_owned(),
    }
}

pub(crate) fn completed_steps(text: &[&str]) -> Vec<ClientStep> {
    let mut steps = vec![ClientStep::Emit(TranscribeEvent::SessionCreated {
        request_id: "recording-1".to_owned(),
    })];
    steps.extend(
        text.iter()
            .map(|text| ClientStep::Emit(TranscribeEvent::TextDelta((*text).to_owned()))),
    );
    steps.push(ClientStep::AwaitAudioEnd);
    steps.push(ClientStep::Emit(TranscribeEvent::Done));
    steps
}

fn manager(
    recorder: Arc<ScriptedRecorder>,
    steps: Option<Vec<ClientStep>>,
) -> (VoiceManager, mpsc::UnboundedReceiver<VoiceEvent>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let client = steps.map(|steps| Arc::new(ScriptedClient { steps }) as Arc<dyn TranscribeClient>);
    (
        VoiceManager::new(recorder, client, credentials(), sender),
        receiver,
    )
}

fn drain(receiver: &mut mpsc::UnboundedReceiver<VoiceEvent>) -> Vec<VoiceEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

/// The listener calls a run produced, with telemetry reduced to its name.
fn outline(events: &[VoiceEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            VoiceEvent::State { state, .. } => format!("state:{}", state.label()),
            VoiceEvent::Text { text, .. } => format!("text:{text}"),
            VoiceEvent::Error { message, .. } => format!("error:{message}"),
            VoiceEvent::Notice { message, .. } => format!("notice:{message}"),
            VoiceEvent::Telemetry(record) => format!("telemetry:{}", record.event().event_name()),
        })
        .collect()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn a_recording_relays_its_text_and_ends_idle() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::from_secs(2));
    let (manager, mut events) = manager(
        recorder.clone(),
        Some(completed_steps(&["hello ", "world"])),
    );
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    assert_eq!(manager.state(), TranscribeState::Recording);
    settle().await;
    manager.stop_recording().await;
    assert_eq!(manager.state(), TranscribeState::Idle);
    assert_eq!(
        outline(&drain(&mut events)),
        [
            "state:recording",
            "telemetry:vibe.audio.transcription.start",
            "text:hello ",
            "text:world",
            "state:flushing",
            "state:idle",
            "telemetry:vibe.audio.transcription.done",
        ]
    );
    assert_eq!(*recorder.rates.lock().unwrap(), [16_000]);
}

#[tokio::test]
async fn a_recording_without_text_reports_that_no_speech_was_detected() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::from_secs(2));
    let (manager, mut events) = manager(recorder, Some(completed_steps(&[])));
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    manager.stop_recording().await;
    let outline = outline(&drain(&mut events));
    assert_eq!(
        &outline[outline.len() - 3..],
        [
            "state:idle",
            "notice:No speech detected",
            "telemetry:vibe.audio.transcription.done",
        ]
    );
}

/// Reference `_no_audio_detected_message`: a long enough recording that never
/// carried a signal points at the microphone, and is reported as an error.
#[tokio::test]
async fn a_silent_recording_points_at_the_microphone() {
    let recorder = ScriptedRecorder::with_signal(false, Duration::from_millis(500));
    let (manager, mut events) = manager(recorder, Some(completed_steps(&[])));
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    manager.stop_recording().await;
    let events = drain(&mut events);
    let outline = outline(&events);
    assert_eq!(
        &outline[outline.len() - 3..],
        [
            "state:idle".to_owned(),
            "telemetry:vibe.audio.transcription.error".to_owned(),
            format!("error:{}", no_audio_detected_message()),
        ]
    );
    let Some(VoiceEvent::Telemetry(TelemetryRecord::TranscriptionFailed {
        recording_duration,
        ..
    })) = events.iter().rev().nth(1)
    else {
        panic!("the error event: {events:?}");
    };
    assert_eq!(*recording_duration, Some(Duration::from_millis(500)));
}

/// A recording stopped before it could deliver a block says nothing about the
/// microphone.
#[tokio::test]
async fn a_short_silent_recording_only_reports_no_speech() {
    let recorder = ScriptedRecorder::with_signal(false, Duration::from_millis(499));
    let (manager, mut events) = manager(recorder, Some(completed_steps(&[])));
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    manager.stop_recording().await;
    let outline = outline(&drain(&mut events));
    assert!(
        outline.contains(&"notice:No speech detected".to_owned()),
        "{outline:?}"
    );
    assert!(
        !outline.iter().any(|line| line.starts_with("error:")),
        "{outline:?}"
    );
}

#[tokio::test]
async fn a_transcription_error_cancels_the_recorder_and_is_reported() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::from_secs(1));
    let (manager, mut events) = manager(
        recorder.clone(),
        Some(vec![ClientStep::Emit(TranscribeEvent::Error(
            "quota exceeded".to_owned(),
        ))]),
    );
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    settle().await;
    assert_eq!(manager.state(), TranscribeState::Idle);
    assert_eq!(recorder.cancels.load(Ordering::Relaxed), 1);
    assert_eq!(
        outline(&drain(&mut events)),
        [
            "state:recording",
            "state:idle",
            "telemetry:vibe.audio.transcription.error",
            "error:quota exceeded",
        ]
    );
}

#[tokio::test]
async fn a_start_is_refused_for_each_reference_cause() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::ZERO);
    let (manager_without_client, _events) = manager(recorder.clone(), None);
    assert_eq!(
        manager_without_client.start_recording(request()).await,
        Err(RecordingStartError(
            "Transcribe client is not available".to_owned()
        ))
    );

    let (manager, mut events) = manager(recorder.clone(), Some(completed_steps(&[])));
    let mut unresolved = request();
    unresolved.api_key_env_var = "FIXTURE_UNSET".to_owned();
    assert_eq!(
        manager.start_recording(unresolved).await,
        Err(RecordingStartError(
            "Voice transcription needs an API key: set FIXTURE_UNSET".to_owned()
        ))
    );
    assert_eq!(
        recorder.starts.load(Ordering::Relaxed),
        0,
        "nothing was opened"
    );

    for (refusal, message) in [
        (
            RecorderError::AlreadyRecording,
            "Recording is already in progress".to_owned(),
        ),
        (
            RecorderError::BackendUnavailable("no driver".to_owned()),
            "Audio backend is unavailable: no driver".to_owned(),
        ),
        (
            RecorderError::NoInputDevice,
            format!("No audio input device found.{}", mic_access_hint()),
        ),
    ] {
        *recorder.refusal.lock().unwrap() = Some(refusal);
        assert_eq!(
            manager.start_recording(request()).await,
            Err(RecordingStartError(message))
        );
    }
    assert!(
        drain(&mut events).is_empty(),
        "a refused start reports nothing"
    );
    assert_eq!(manager.state(), TranscribeState::Idle);
}

#[tokio::test]
async fn cancelling_returns_to_idle_and_reports_the_cancellation() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::from_secs(1));
    let (manager, mut events) = manager(recorder.clone(), Some(vec![ClientStep::Hang]));
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    manager.cancel_recording();
    manager.cancel_recording();
    assert_eq!(manager.state(), TranscribeState::Idle);
    assert_eq!(recorder.cancels.load(Ordering::Relaxed), 1);
    assert_eq!(
        outline(&drain(&mut events)),
        [
            "state:recording",
            "state:idle",
            "telemetry:vibe.audio.transcription.cancel_recording",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_transcription_that_outlives_the_drain_timeout_is_abandoned() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::from_secs(1));
    let (manager, mut events) = manager(recorder, Some(vec![ClientStep::Hang]));
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    manager.stop_recording().await;
    assert_eq!(
        outline(&drain(&mut events)),
        [
            "state:recording",
            "state:flushing",
            "telemetry:vibe.audio.transcription.error",
            "error:Transcription timed out",
            "state:idle",
        ]
    );
}

/// Every event carries the tag its recording was started under.
#[tokio::test]
async fn events_carry_the_recording_tag() {
    let recorder = ScriptedRecorder::with_signal(true, Duration::from_secs(1));
    let (manager, mut events) = manager(recorder, Some(completed_steps(&["a"])));
    manager
        .start_recording(request())
        .await
        .expect("a recording");
    manager.stop_recording().await;
    for event in drain(&mut events) {
        match event {
            VoiceEvent::State { tag, .. }
            | VoiceEvent::Text { tag, .. }
            | VoiceEvent::Error { tag, .. }
            | VoiceEvent::Notice { tag, .. } => assert_eq!(tag, 7),
            VoiceEvent::Telemetry(_) => {}
        }
    }
}
