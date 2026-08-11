//! Voice input lifecycle and deterministic state boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::chat_input::{InputEffect, InputEvent};

mod realtime;
mod recorder;
mod session;
mod settings;
mod state;

pub use state::VoicePhase;
pub(crate) use state::{VoiceCommand, VoiceState, VoiceUpdate, VoiceUpdateOutcome};

use realtime::VoiceConfig;
use session::ProductionVoiceSessionFactory;
use settings::TranscriptionSettings;

const UPDATE_QUEUE_CAPACITY: usize = 128;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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
    active: Option<ActiveVoice>,
    retiring: Vec<JoinHandle<()>>,
    pending_start: Option<u64>,
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
    fn new(factory: Arc<dyn VoiceSessionFactory>, enabled: bool) -> Self {
        Self::with_factory(Ok(factory), enabled, String::new(), PathBuf::new())
    }

    fn with_factory(
        factory: Result<Arc<dyn VoiceSessionFactory>, String>,
        enabled: bool,
        fallback_credential: String,
        vibe_home: PathBuf,
    ) -> Self {
        let (updates_tx, updates_rx) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
        Self {
            enabled,
            factory,
            fallback_credential,
            vibe_home,
            updates_tx,
            updates_rx,
            active: None,
            retiring: Vec::new(),
            pending_start: None,
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
        let event = self.updates_rx.try_recv().ok();
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
        let task = factory.spawn(generation, self.updates_tx.clone(), receiver);
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
