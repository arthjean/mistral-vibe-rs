//! The voice input lifecycle.
//!
//! Reference `VoiceManager` (`vibe/cli/voice_manager/voice_manager.py`) owns
//! one recorder and one transcribe client. A start checks that a client
//! exists and that the credential its provider names resolves, opens the
//! microphone and moves to recording before anything connects, so the audio
//! captured while the session opens is queued rather than lost. The
//! transcription then runs beside the recording: text is relayed as it
//! arrives, the session's identifier is recorded, and an error ends both. A
//! stop flushes: the recorder ends its stream and the manager waits up to ten
//! seconds for the transcription to finish. Once it has, a recording that
//! produced no text says why: a long enough recording that never carried a
//! signal points at the microphone, any other one reports that no speech was
//! detected.
//!
//! Listeners are an event channel here. Every event carries the tag the
//! recording was started under, so a caller that starts another recording can
//! tell a late event from a current one.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use vibe_core::telemetry::TelemetryRecord;

use crate::capture::{AudioRecorder, AudioStream, RecorderError};
use crate::settings::{CredentialLookup, credential_missing};
use crate::tracking::TranscriptionTracking;
use crate::transcribe::{TranscribeClient, TranscribeEvent};

/// Reference `TRANSCRIPTION_DRAIN_TIMEOUT`.
pub const TRANSCRIPTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// What the manager reports when a transcription produced no text.
pub const NO_SPEECH_NOTICE: &str = "No speech detected";
/// What a stop reports when the transcription outlived the drain timeout.
pub const TIMED_OUT_ERROR: &str = "Transcription timed out";

/// Reference `TranscribeState`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TranscribeState {
    #[default]
    Idle,
    Recording,
    Flushing,
}

impl TranscribeState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Flushing => "flushing",
        }
    }
}

/// Reference `VoiceManagerListener`'s callbacks, and the telemetry events the
/// manager logs, in the order they happen.
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceEvent {
    State { tag: u64, state: TranscribeState },
    Text { tag: u64, text: String },
    Error { tag: u64, message: String },
    Notice { tag: u64, message: String },
    Telemetry(TelemetryRecord),
}

/// What one start reads from the configuration as it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartRequest {
    /// The caller's name for this recording, carried on every event it
    /// produces.
    pub tag: u64,
    /// Reference `transcription.model.sample_rate`.
    pub sample_rate: u32,
    /// Reference `transcription.provider.api_key_env_var`.
    pub api_key_env_var: String,
}

/// Reference `RecordingStartError`: why a start was refused, which the caller
/// reports as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingStartError(pub String);

impl std::fmt::Display for RecordingStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A stop or a cancellation that arrived while the microphone was opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Stop,
    Cancel,
}

#[derive(Default)]
struct Inner {
    state: TranscribeState,
    tag: u64,
    /// Which recording the running task belongs to, so a task that outlived
    /// its recording settles nothing.
    run: u64,
    starting: bool,
    pending: Option<Pending>,
    task: Option<JoinHandle<()>>,
    abort: Option<AbortHandle>,
    tracking: TranscriptionTracking,
}

struct Shared {
    recorder: Arc<dyn AudioRecorder>,
    client: Option<Arc<dyn TranscribeClient>>,
    credentials: CredentialLookup,
    events: mpsc::UnboundedSender<VoiceEvent>,
    drain_timeout: Duration,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while the lock was held leaves plain data behind; the state
        // it describes is still the best reading there is.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, event: VoiceEvent) {
        let _ = self.events.send(event);
    }

    /// Reference `_set_state`: listeners hear of a change only.
    fn set_state(&self, inner: &mut Inner, state: TranscribeState) {
        if inner.state == state {
            return;
        }
        inner.state = state;
        self.emit(VoiceEvent::State {
            tag: inner.tag,
            state,
        });
    }

    /// Reference `_run_transcription`'s normal end.
    fn finish(&self, run: u64) {
        let mut inner = self.lock();
        if inner.run != run {
            return;
        }
        let tag = inner.tag;
        self.set_state(&mut inner, TranscribeState::Idle);
        if inner.tracking.transcript_length() == 0
            && !self.recorder.has_signal()
            && inner.tracking.recorded_long_enough_for_signal()
        {
            let message = no_audio_detected_message();
            self.emit(VoiceEvent::Telemetry(inner.tracking.error_event(&message)));
            self.emit(VoiceEvent::Error { tag, message });
            return;
        }
        if inner.tracking.transcript_length() == 0 {
            self.emit(VoiceEvent::Notice {
                tag,
                message: NO_SPEECH_NOTICE.to_owned(),
            });
        }
        self.emit(VoiceEvent::Telemetry(inner.tracking.done_event()));
    }

    /// Reference `_run_transcription`'s failure path: the recorder is
    /// cancelled, the manager returns to idle, and the error is reported.
    fn fail(&self, run: u64, message: String) {
        let mut inner = self.lock();
        if inner.run != run {
            return;
        }
        let tag = inner.tag;
        self.recorder.cancel();
        self.set_state(&mut inner, TranscribeState::Idle);
        self.emit(VoiceEvent::Telemetry(inner.tracking.error_event(&message)));
        self.emit(VoiceEvent::Error { tag, message });
    }
}

/// Reference `VoiceManager`.
#[derive(Clone)]
pub struct VoiceManager {
    shared: Arc<Shared>,
}

impl VoiceManager {
    /// A manager over `recorder` and `client`; `client` is `None` when the
    /// configuration resolved no transcription surface, which every start then
    /// reports. Events are sent on `events`.
    #[must_use]
    pub fn new(
        recorder: Arc<dyn AudioRecorder>,
        client: Option<Arc<dyn TranscribeClient>>,
        credentials: CredentialLookup,
        events: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Self {
        Self::with_drain_timeout(
            recorder,
            client,
            credentials,
            events,
            TRANSCRIPTION_DRAIN_TIMEOUT,
        )
    }

    /// The same manager waiting `drain_timeout` for a stopped transcription,
    /// so a test can reach the timeout without waiting out the reference's.
    #[must_use]
    pub(crate) fn with_drain_timeout(
        recorder: Arc<dyn AudioRecorder>,
        client: Option<Arc<dyn TranscribeClient>>,
        credentials: CredentialLookup,
        events: mpsc::UnboundedSender<VoiceEvent>,
        drain_timeout: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                recorder,
                client,
                credentials,
                events,
                drain_timeout,
                inner: Mutex::new(Inner::default()),
            }),
        }
    }

    #[must_use]
    pub fn state(&self) -> TranscribeState {
        self.shared.lock().state
    }

    /// Whether a start is opening the microphone.
    #[must_use]
    pub fn is_starting(&self) -> bool {
        self.shared.lock().starting
    }

    /// Reference `peak`.
    #[must_use]
    pub fn peak(&self) -> f32 {
        self.shared.recorder.peak()
    }

    /// Reference `start_recording`. A manager already recording, or already
    /// opening the microphone, ignores the start.
    ///
    /// # Errors
    ///
    /// No transcribe client, a credential its provider names that does not
    /// resolve, or a recorder that cannot open.
    pub async fn start_recording(&self, request: StartRequest) -> Result<(), RecordingStartError> {
        let client = {
            let mut inner = self.shared.lock();
            if inner.state != TranscribeState::Idle || inner.starting {
                return Ok(());
            }
            let Some(client) = self.shared.client.clone() else {
                return Err(RecordingStartError(
                    "Transcribe client is not available".to_owned(),
                ));
            };
            if credential_missing(&request.api_key_env_var, &self.shared.credentials) {
                return Err(RecordingStartError(format!(
                    "Voice transcription needs an API key: set {}",
                    request.api_key_env_var
                )));
            }
            inner.starting = true;
            inner.pending = None;
            client
        };
        let recorder = Arc::clone(&self.shared.recorder);
        let sample_rate = request.sample_rate;
        let opened = tokio::task::spawn_blocking(move || recorder.start(sample_rate)).await;
        let pending = {
            let mut inner = self.shared.lock();
            inner.starting = false;
            let audio = match opened {
                Ok(Ok(audio)) => audio,
                Ok(Err(error)) => return Err(start_error(error)),
                Err(error) => {
                    return Err(RecordingStartError(format!(
                        "Audio backend is unavailable: {error}"
                    )));
                }
            };
            if inner.pending == Some(Pending::Cancel) {
                inner.pending = None;
                drop(inner);
                self.shared.recorder.cancel();
                return Ok(());
            }
            inner.tracking.reset();
            inner.tag = request.tag;
            inner.run = inner.run.wrapping_add(1);
            self.shared
                .set_state(&mut inner, TranscribeState::Recording);
            let task = tokio::spawn(run_transcription(
                Arc::clone(&self.shared),
                inner.run,
                request.tag,
                client,
                audio,
            ));
            inner.abort = Some(task.abort_handle());
            inner.task = Some(task);
            inner.pending.take()
        };
        if pending == Some(Pending::Stop) {
            self.stop_recording().await;
        }
        Ok(())
    }

    /// Reference `stop_recording`: the stream is flushed, the transcription
    /// is given [`TRANSCRIPTION_DRAIN_TIMEOUT`] to finish, and the manager
    /// returns to idle.
    pub async fn stop_recording(&self) {
        let (run, task) = {
            let mut inner = self.shared.lock();
            if inner.starting {
                inner.pending = Some(Pending::Stop);
                return;
            }
            if inner.state != TranscribeState::Recording {
                return;
            }
            self.shared.set_state(&mut inner, TranscribeState::Flushing);
            (inner.run, inner.task.take())
        };
        // The recorder stops and the duration is recorded under one lock, as
        // the reference does both before its transcription task can run
        // again: the task's ending reads that duration.
        let shared = Arc::clone(&self.shared);
        let stopped = tokio::task::spawn_blocking(move || {
            let mut inner = shared.lock();
            if inner.run != run {
                return false;
            }
            let duration = shared.recorder.stop();
            inner.tracking.set_recording_duration(duration);
            true
        })
        .await
        .unwrap_or(false);
        if !stopped {
            return;
        }
        if let Some(task) = task {
            let abort = task.abort_handle();
            if tokio::time::timeout(self.shared.drain_timeout, task)
                .await
                .is_err()
            {
                abort.abort();
                let inner = self.shared.lock();
                if inner.run != run {
                    return;
                }
                self.shared.emit(VoiceEvent::Telemetry(
                    inner.tracking.error_event(TIMED_OUT_ERROR),
                ));
                self.shared.emit(VoiceEvent::Error {
                    tag: inner.tag,
                    message: TIMED_OUT_ERROR.to_owned(),
                });
            }
        }
        let mut inner = self.shared.lock();
        if inner.run != run {
            return;
        }
        inner.abort = None;
        self.shared.set_state(&mut inner, TranscribeState::Idle);
    }

    /// Reference `cancel_recording`.
    pub fn cancel_recording(&self) {
        let mut inner = self.shared.lock();
        if inner.starting {
            inner.pending = Some(Pending::Cancel);
            return;
        }
        if inner.state == TranscribeState::Idle {
            return;
        }
        self.shared.recorder.cancel();
        if let Some(abort) = inner.abort.take() {
            abort.abort();
        }
        inner.task = None;
        self.shared.set_state(&mut inner, TranscribeState::Idle);
        let event = inner.tracking.cancel_event();
        self.shared.emit(VoiceEvent::Telemetry(event));
    }

    /// Reference `close`: a running recording is cancelled and its
    /// transcription awaited.
    pub async fn close(&self) {
        let task = self.shared.lock().task.take();
        self.cancel_recording();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

async fn run_transcription(
    shared: Arc<Shared>,
    run_id: u64,
    tag: u64,
    client: Arc<dyn TranscribeClient>,
    audio: AudioStream,
) {
    let (sender, mut events) = mpsc::unbounded_channel();
    let mut run = client.transcribe(audio, sender);
    let mut finished = false;
    let failure = loop {
        tokio::select! {
            event = events.recv() => match event {
                None => break None,
                Some(TranscribeEvent::TextDelta(text)) => {
                    shared.lock().tracking.record_text(&text);
                    shared.emit(VoiceEvent::Text { tag, text });
                }
                Some(TranscribeEvent::Error(message)) => break Some(message),
                Some(TranscribeEvent::SessionCreated { request_id }) => {
                    let mut inner = shared.lock();
                    inner.tracking.set_recording_id(request_id);
                    let event = inner.tracking.start_event();
                    drop(inner);
                    shared.emit(VoiceEvent::Telemetry(event));
                }
                Some(TranscribeEvent::Done) => {}
            },
            () = &mut run, if !finished => finished = true,
        }
    };
    // The session ends with the error that ended it, before the recorder is
    // touched.
    drop(run);
    match failure {
        Some(message) => shared.fail(run_id, message),
        None => shared.finish(run_id),
    }
}

fn start_error(error: RecorderError) -> RecordingStartError {
    RecordingStartError(match error {
        RecorderError::AlreadyRecording => "Recording is already in progress".to_owned(),
        RecorderError::BackendUnavailable(detail) => {
            format!("Audio backend is unavailable: {detail}")
        }
        RecorderError::NoInputDevice => {
            format!("No audio input device found.{}", mic_access_hint())
        }
    })
}

/// Reference `_mic_access_hint`: where the operating system grants microphone
/// access, on the two platforms that gate it.
#[must_use]
pub fn mic_access_hint() -> &'static str {
    match vibe_core::telemetry::platform_id().as_str() {
        "darwin" => " Grant access in System Settings → Privacy & Security → Microphone.",
        "windows" => " Grant access in Settings → Privacy & security → Microphone.",
        _ => "",
    }
}

/// Reference `_no_audio_detected_message`.
#[must_use]
pub fn no_audio_detected_message() -> String {
    format!(
        "No audio detected from microphone; check your terminal has mic access.{}",
        mic_access_hint()
    )
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod manager_tests;
