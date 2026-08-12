//! Voice input lifecycle and deterministic state boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::chat_input::{InputEffect, InputEvent};

mod player;
mod realtime;
mod recorder;
mod session;
mod settings;
mod speech;
mod state;
mod telemetry;

pub(crate) use speech::{SpeechEvent, SpeechManager};
pub use state::VoicePhase;
pub(crate) use state::{VoiceCommand, VoiceState, VoiceUpdate, VoiceUpdateOutcome};

use vibe_core::telemetry::TelemetryRecord;

use realtime::VoiceConfig;
use session::ProductionVoiceSessionFactory;
use settings::TranscriptionSettings;
use telemetry::TranscriptionTracking;

const UPDATE_QUEUE_CAPACITY: usize = 128;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// What a running session tells its manager that the event stream the composer
/// reads does not carry.
///
/// The composer's `InputEvent` vocabulary is an observable protocol of its own,
/// so the recording identity travels beside it rather than inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum VoiceSignal {
    /// The endpoint accepted the session and named it, which is where the
    /// reference sets the recording id and emits its start event.
    SessionCreated {
        generation: u64,
        recording_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VoiceControl {
    Running,
    Stop,
    Cancel,
}

struct ActiveVoice {
    generation: u64,
    control: watch::Sender<VoiceControl>,
    task: JoinHandle<()>,
}

pub(super) trait VoiceSessionFactory: Send + Sync {
    fn spawn(
        &self,
        generation: u64,
        updates: mpsc::Sender<InputEvent>,
        signals: mpsc::Sender<VoiceSignal>,
        control: watch::Receiver<VoiceControl>,
    ) -> JoinHandle<()>;
}

/// Owns generation-aware voice sessions without exposing device or network
/// work to the deterministic composer reducer.
pub(super) struct VoiceManager {
    enabled: bool,
    /// The session factory the configuration resolved to, or why it resolved to
    /// none. A configuration this build cannot address is answered when a
    /// recording is asked for, the way the reference answers one with a null
    /// transcribe client, rather than taken as a startup failure.
    factory: Result<Arc<dyn VoiceSessionFactory>, String>,
    /// What a resolution needs beyond the configuration itself, kept so the
    /// surface can be resolved again when the configuration changes.
    fallback_credential: String,
    vibe_home: PathBuf,
    updates_tx: mpsc::Sender<InputEvent>,
    updates_rx: mpsc::Receiver<InputEvent>,
    signals_tx: mpsc::Sender<VoiceSignal>,
    signals_rx: mpsc::Receiver<VoiceSignal>,
    active: Option<ActiveVoice>,
    retiring: Vec<JoinHandle<()>>,
    pending_start: Option<u64>,
    /// Reference `VoiceManager._tracking`: what the four audio events are built
    /// from, reset where a recording starts.
    tracking: TranscriptionTracking,
    /// When the microphone actually opened, which is what the reference reads
    /// off `AudioRecording.duration` when the recorder stops. This port never
    /// holds the captured buffer, so the recording is measured from the moment
    /// the session reported it running to the moment it stopped.
    recording_started: Option<std::time::Instant>,
    /// The events produced but not yet handed to the telemetry client.
    telemetry: Vec<TelemetryRecord>,
}

impl VoiceManager {
    /// Resolves the transcription session from the published configuration:
    /// the endpoint, the model, the wire values and the credential the provider
    /// entry names, with the session's own credential standing in only where the
    /// provider names no variable.
    pub(super) fn production(
        config_view: &Value,
        fallback_credential: &str,
        vibe_home: &Path,
        enabled: bool,
    ) -> Self {
        let mut manager = Self::with_factory(
            Err("Voice mode is not configured".to_owned()),
            enabled,
            fallback_credential.to_owned(),
            vibe_home.to_path_buf(),
        );
        manager.resync(config_view);
        manager
    }

    /// Resolves the transcription surface again from the configuration as it
    /// stands now.
    ///
    /// Reference `LazyVoiceManager`, which materializes its manager, and with it
    /// its transcribe client, from the current configuration rather than from
    /// the one the process started on: an operator who changes the active model
    /// or its provider is recording against the new one on the next start. A
    /// session already running keeps the endpoint it opened.
    pub(super) fn resync(&mut self, config_view: &Value) {
        self.factory = TranscriptionSettings::from_config_view(config_view).and_then(|settings| {
            let credential = settings.credential(&self.fallback_credential, &self.vibe_home)?;
            let config = VoiceConfig::resolve(&settings)?;
            let factory: Arc<dyn VoiceSessionFactory> =
                Arc::new(ProductionVoiceSessionFactory::new(credential, config));
            Ok(factory)
        });
    }

    #[cfg(test)]
    pub(in crate::tui) fn new(factory: Arc<dyn VoiceSessionFactory>, enabled: bool) -> Self {
        Self::with_factory(Ok(factory), enabled, String::new(), PathBuf::new())
    }

    fn with_factory(
        factory: Result<Arc<dyn VoiceSessionFactory>, String>,
        enabled: bool,
        fallback_credential: String,
        vibe_home: PathBuf,
    ) -> Self {
        let (updates_tx, updates_rx) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
        let (signals_tx, signals_rx) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
        Self {
            enabled,
            factory,
            fallback_credential,
            vibe_home,
            updates_tx,
            updates_rx,
            signals_tx,
            signals_rx,
            active: None,
            retiring: Vec::new(),
            pending_start: None,
            tracking: TranscriptionTracking::default(),
            recording_started: None,
            telemetry: Vec::new(),
        }
    }

    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.pending_start = None;
            self.cancel_active();
        }
    }

    pub(super) fn apply_effects(&mut self, effects: &[InputEffect], generation: u64) {
        self.reap_finished();
        for effect in effects {
            match effect {
                InputEffect::RecordingStartRequested => self.start(generation),
                InputEffect::RecordingStopRequested => self.control(VoiceControl::Stop),
                InputEffect::RecordingCancelRequested => self.cancel_active(),
                _ => {}
            }
        }
    }

    pub(super) fn try_next_event(&mut self) -> Option<InputEvent> {
        self.drain_signals();
        let event = self.updates_rx.try_recv().ok();
        if let Some(event) = event.as_ref() {
            self.observe(event);
        }
        if event
            .as_ref()
            .and_then(terminal_generation)
            .is_some_and(|generation| {
                self.active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
            })
        {
            self.retire_active();
        }
        self.reap_finished();
        event
    }

    /// Queues an event a session would have sent, so a test drives the same
    /// reader the event loop drives rather than the observer behind it.
    #[cfg(test)]
    pub(in crate::tui) fn inject_for_test(&mut self, event: InputEvent) {
        let _ = self.updates_tx.try_send(event);
    }

    /// The audio events produced since the last drain, in the order they fired.
    ///
    /// The caller hands them to the session's telemetry client, which is where
    /// `enable_telemetry` decides whether anything is sent.
    pub(crate) fn take_telemetry(&mut self) -> Vec<TelemetryRecord> {
        self.drain_signals();
        std::mem::take(&mut self.telemetry)
    }

    fn drain_signals(&mut self) {
        while let Ok(signal) = self.signals_rx.try_recv() {
            let VoiceSignal::SessionCreated {
                generation,
                recording_id,
            } = signal;
            // A signal from a session already retired belongs to a recording
            // whose tracking has been reset, so it is dropped rather than
            // renaming the running one.
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                self.tracking.set_recording_id(recording_id);
                self.telemetry.push(self.tracking.start_event());
            }
        }
    }

    /// What the reference reads off its own transcribe stream: text lengths
    /// accumulate, a clean stop takes the recording's duration, and the
    /// terminal answer emits `done` or `error`.
    ///
    /// A start that failed emits nothing, matching the reference, whose
    /// `RecordingStartError` is raised to the caller rather than reported: only
    /// a transcription that ran and then failed reaches the error event.
    fn observe(&mut self, event: &InputEvent) {
        match event {
            InputEvent::VoiceStartResolved { error: None, .. } => {
                self.recording_started = Some(std::time::Instant::now());
            }
            InputEvent::VoiceTranscriptDelta { text, .. } => self.tracking.record_text(text),
            // Reference `stop_recording`: the recorder's own duration is taken
            // where it stops cleanly, and a transcription that fails while the
            // microphone is still open reports no recording duration at all.
            InputEvent::VoiceStopResolved { error: None, .. } => {
                if let Some(started) = self.recording_started.take() {
                    self.tracking.set_recording_duration(started.elapsed());
                }
            }
            InputEvent::VoiceStopResolved {
                error: Some(error), ..
            } => self.telemetry.push(self.tracking.error_event(error)),
            InputEvent::VoiceDone { .. } => self.telemetry.push(self.tracking.done_event()),
            _ => {}
        }
    }

    pub(super) async fn shutdown(&mut self) {
        self.pending_start = None;
        self.cancel_active();
        for mut task in self.retiring.drain(..) {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }

    fn start(&mut self, generation: u64) {
        if !self.enabled {
            let _ = self.updates_tx.try_send(InputEvent::VoiceStartResolved {
                generation,
                error: Some("Voice mode is disabled".to_owned()),
            });
            return;
        }
        // A configuration that resolves to no session is reported here, where
        // the operator asked for a recording, and nothing is connected to.
        if let Err(error) = &self.factory {
            let _ = self.updates_tx.try_send(InputEvent::VoiceStartResolved {
                generation,
                error: Some(error.clone()),
            });
            return;
        }
        if self.active.is_some() {
            return;
        }
        if !self.retiring.is_empty() {
            self.pending_start = Some(generation);
            return;
        }
        self.start_now(generation);
    }

    fn start_now(&mut self, generation: u64) {
        let Ok(factory) = self.factory.as_ref() else {
            return;
        };
        let (control, receiver) = watch::channel(VoiceControl::Running);
        let task = factory.spawn(
            generation,
            self.updates_tx.clone(),
            self.signals_tx.clone(),
            receiver,
        );
        // Reference `start_recording`: the tracking record is reset where the
        // recording begins, so every event a session emits belongs to it.
        self.tracking.reset();
        self.recording_started = None;
        self.active = Some(ActiveVoice {
            generation,
            control,
            task,
        });
    }

    fn control(&self, control: VoiceControl) {
        if let Some(active) = self.active.as_ref() {
            let _ = active.control.send(control);
        }
    }

    fn cancel_active(&mut self) {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            let _ = active.control.send(VoiceControl::Cancel);
            self.retiring.push(active.task);
            // Reference `cancel_recording`, which returns before its emitter
            // when nothing is running: the event fires exactly where a session
            // was cancelled.
            self.recording_started = None;
            self.telemetry.push(self.tracking.cancel_event());
        }
    }

    fn retire_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.retiring.push(active.task);
        }
    }

    fn reap_finished(&mut self) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.task.is_finished())
        {
            self.retire_active();
        }
        self.retiring.retain(|task| !task.is_finished());
        if self.active.is_none()
            && self.retiring.is_empty()
            && let Some(generation) = self.pending_start.take()
        {
            self.start_now(generation);
        }
    }
}

fn terminal_generation(event: &InputEvent) -> Option<u64> {
    match event {
        InputEvent::VoiceDone { generation }
        | InputEvent::VoiceStartResolved {
            generation,
            error: Some(_),
        }
        | InputEvent::VoiceStopResolved {
            generation,
            error: Some(_),
        } => Some(*generation),
        _ => None,
    }
}

impl Drop for VoiceManager {
    fn drop(&mut self) {
        self.pending_start = None;
        if let Some(active) = self.active.take() {
            let _ = active.control.send(VoiceControl::Cancel);
            active.task.abort();
        }
        for task in self.retiring.drain(..) {
            task.abort();
        }
    }
}

#[cfg(test)]
#[path = "voice/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "voice/voice_parity_tests.rs"]
mod voice_parity_tests;

#[cfg(test)]
#[path = "voice/audio_telemetry_tests.rs"]
mod audio_telemetry_tests;
