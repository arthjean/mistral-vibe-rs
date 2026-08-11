//! The speech transport: a turn summary posted to the configured speech model,
//! and the decoded audio played through the default output device.
//!
//! The reference builds one client per configuration from the active `tts_models`
//! entry and its provider, posts the summary to that provider's `api_base`, and
//! base64-decodes the audio the response carries before handing it to the player
//! (`vibe/cli/tts/mistral_tts_client.py:23-27,44-56`). The narrator holds no
//! client at all when the configuration resolves to nothing, which is the gate
//! that keeps a turn silent rather than failing it
//! (`vibe/cli/narrator_manager/narrator_manager.py:107-117,146-165`).
//!
//! This module owns that transport. The state machine in
//! [`crate::tui::narrator`] stays pure: it emits `Speak`, the manager here runs
//! the request and the playback off the event loop, and the two events it
//! answers with drive the same machine back to idle.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use url::Url;

use super::player::{AudioOutput, CpalAudioOutput, PlaybackError, decode_wav};
use super::settings::SpeechSettings;

/// The path the speech endpoint is served under, appended to whatever path the
/// configured `api_base` already carries so a gateway served below a prefix is
/// addressed under it.
const SPEECH_PATH: &str = "/v1/audio/speech";
const USER_AGENT_VALUE: &str = concat!("mistral-vibe-rs/", env!("CARGO_PKG_VERSION"));
/// This port's own bound on one speech request. The reference leaves the SDK's
/// default in place; a summary that never answers must not hold the narrator in
/// its speaking state for the rest of the session.
const SPEECH_TIMEOUT: Duration = Duration::from_secs(60);
const EVENT_QUEUE_CAPACITY: usize = 16;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// What the narrator state machine is driven by once a summary is spoken.
///
/// Every answer carries the generation that asked for it, so a result that
/// outlived its turn settles nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpeechEvent {
    /// Reference `_speak_summary` once the player accepted the buffer.
    PlaybackStarted { generation: u64 },
    /// Reference `_on_playback_finished` and the failure path that settles the
    /// same generation without playing anything.
    Finished {
        generation: u64,
        error: Option<String>,
    },
}

/// The request the configured speech surface describes, built before anything is
/// opened so it can be compared against the reference's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SpeechRequest {
    pub(super) endpoint: Url,
    pub(super) body: Value,
}

impl SpeechRequest {
    pub(super) fn resolve(settings: &SpeechSettings, text: &str) -> Result<Self, String> {
        let mut endpoint = Url::parse(&settings.api_base)
            .map_err(|error| format!("Speech API URL is invalid: {error}"))?;
        match endpoint.scheme() {
            "https" | "http" => {}
            scheme => return Err(format!("Speech API URL scheme `{scheme}` is unsupported")),
        }
        let prefix = endpoint.path().trim_end_matches('/').to_owned();
        endpoint.set_path(&format!("{prefix}{SPEECH_PATH}"));
        endpoint.set_query(None);
        Ok(Self {
            endpoint,
            body: json!({
                "model": settings.model,
                "input": text,
                "voice_id": settings.voice,
                "response_format": settings.response_format,
                "stream": false,
            }),
        })
    }
}

type SpeechFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// The network boundary: a summary in, decoded audio bytes out.
pub(super) trait SpeechClient: Send + Sync + 'static {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a>;
}

/// Reference `MistralTTSClient`.
pub(super) struct HttpSpeechClient {
    client: reqwest::Client,
    settings: SpeechSettings,
    credential: SecretString,
}

impl HttpSpeechClient {
    pub(super) fn new(settings: SpeechSettings, credential: String) -> Result<Self, String> {
        // The endpoint is resolved once here so an unusable `api_base` is
        // reported as an unresolved configuration rather than at the first turn.
        SpeechRequest::resolve(&settings, "")?;
        let client = reqwest::Client::builder()
            .timeout(SPEECH_TIMEOUT)
            .build()
            .map_err(|error| format!("Speech transport could not start: {error}"))?;
        Ok(Self {
            client,
            settings,
            credential: SecretString::from(credential),
        })
    }
}

impl SpeechClient for HttpSpeechClient {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a> {
        Box::pin(async move {
            let request = SpeechRequest::resolve(&self.settings, text)?;
            let response = self
                .client
                .post(request.endpoint.clone())
                .bearer_auth(self.credential.expose_secret())
                .header(reqwest::header::USER_AGENT, USER_AGENT_VALUE)
                .json(&request.body)
                .send()
                .await
                .map_err(|error| format!("Speech request failed: {}", transport_reason(&error)))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("Speech request was refused with status {status}"));
            }
            let payload: Value = response
                .json()
                .await
                .map_err(|error| format!("Speech response is invalid: {error}"))?;
            let encoded = payload
                .get("audio_data")
                .and_then(Value::as_str)
                .ok_or_else(|| "Speech response carries no audio".to_owned())?;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| format!("Spoken audio could not be decoded: {error}"))
        })
    }
}

/// A transport failure without the URL it carries, so a credential embedded in a
/// configured endpoint never reaches a diagnostic.
fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "the request timed out".to_owned();
    }
    if error.is_connect() {
        return "the speech endpoint could not be reached".to_owned();
    }
    if error.is_decode() {
        return "the response could not be read".to_owned();
    }
    "the request did not complete".to_owned()
}

struct ActiveSpeech {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Owns the speech client the configuration resolves to and the one playback
/// that may be running, so the deterministic narrator never touches a socket or
/// a device.
pub(crate) struct SpeechManager {
    /// The client the configuration resolved to, or why it resolved to none.
    /// Reference `tts_client is not None`: a configuration this build cannot
    /// address keeps the narrator out of its speaking state instead of failing
    /// the turn.
    client: Result<Arc<dyn SpeechClient>, String>,
    output: Arc<dyn AudioOutput>,
    fallback_credential: String,
    vibe_home: PathBuf,
    events_tx: mpsc::Sender<SpeechEvent>,
    events_rx: mpsc::Receiver<SpeechEvent>,
    active: Option<ActiveSpeech>,
    retiring: Vec<JoinHandle<()>>,
}

impl SpeechManager {
    /// Resolves the speech surface from the published configuration: the model,
    /// the voice, the output format, the endpoint and the credential the
    /// provider entry names.
    pub(crate) fn production(
        config_view: &Value,
        fallback_credential: &str,
        vibe_home: &Path,
    ) -> Self {
        let mut manager = Self::with_output(
            Arc::new(CpalAudioOutput),
            fallback_credential.to_owned(),
            vibe_home.to_path_buf(),
        );
        manager.resync(config_view);
        manager
    }

    /// A manager over a scripted transport and a scripted device, so the whole
    /// speech path is driven in a test that has neither.
    #[cfg(test)]
    pub(super) fn scripted(client: Arc<dyn SpeechClient>, output: Arc<dyn AudioOutput>) -> Self {
        let mut manager = Self::with_output(output, String::new(), PathBuf::new());
        manager.client = Ok(client);
        manager
    }

    fn with_output(
        output: Arc<dyn AudioOutput>,
        fallback_credential: String,
        vibe_home: PathBuf,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        Self {
            client: Err("Narration is not configured".to_owned()),
            output,
            fallback_credential,
            vibe_home,
            events_tx,
            events_rx,
            active: None,
            retiring: Vec::new(),
        }
    }

    /// Reference `NarratorManager.sync`: the client is rebuilt from the
    /// configuration as it stands now rather than kept from process start, so an
    /// edited active model, provider or credential variable reaches the next
    /// turn.
    pub(crate) fn resync(&mut self, config_view: &Value) {
        self.client = SpeechSettings::from_config_view(config_view).and_then(|settings| {
            let credential = settings.credential(&self.fallback_credential, &self.vibe_home)?;
            let client: Arc<dyn SpeechClient> =
                Arc::new(HttpSpeechClient::new(settings, credential)?);
            Ok(client)
        });
    }

    /// Reference `tts_client is not None`.
    pub(crate) fn available(&self) -> bool {
        self.client.is_ok()
    }

    /// Reference `_speak_summary`: the summary is posted, and the audio it
    /// answers with is played. A configuration that resolves to no client is
    /// reported through the same event path rather than as a start failure, so
    /// the narrator always settles.
    pub(crate) fn speak(&mut self, generation: u64, text: String) {
        let client = match self.client.as_ref() {
            Ok(client) => Arc::clone(client),
            Err(reason) => {
                self.settle_now(generation, reason.clone());
                return;
            }
        };
        // Reference `AlreadyPlayingError`: a second playback is refused and the
        // running one is left alone.
        if self.active.is_some() {
            self.settle_now(generation, PlaybackError::AlreadyPlaying.to_string());
            return;
        }
        let (stop, stop_rx) = watch::channel(false);
        let events = self.events_tx.clone();
        let output = Arc::clone(&self.output);
        let task = tokio::spawn(async move {
            run_speech(generation, text, client, output, events, stop_rx).await;
        });
        self.active = Some(ActiveSpeech { stop, task });
    }

    /// Reference `cancel`: the summary task is dropped and playback stops before
    /// the state machine returns to idle.
    pub(crate) fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active.stop.send(true);
            self.retiring.push(active.task);
        }
    }

    pub(crate) fn try_next_event(&mut self) -> Option<SpeechEvent> {
        let event = self.events_rx.try_recv().ok();
        if matches!(event, Some(SpeechEvent::Finished { .. })) {
            self.retire_active();
        }
        self.reap_finished();
        event
    }

    pub(crate) async fn shutdown(&mut self) {
        self.stop();
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

    /// Answers a request that never reached the transport, so the caller learns
    /// the reason through the one event path it already drains.
    fn settle_now(&mut self, generation: u64, failure: String) {
        let _ = self.events_tx.try_send(SpeechEvent::Finished {
            generation,
            error: Some(failure),
        });
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
    }
}

impl Drop for SpeechManager {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active.stop.send(true);
            active.task.abort();
        }
        for task in self.retiring.drain(..) {
            task.abort();
        }
    }
}

/// One spoken summary: the request, the decode, the playback and the completion.
/// A stop at any point returns without answering, because the state machine that
/// asked for the stop has already returned to idle.
async fn run_speech(
    generation: u64,
    text: String,
    client: Arc<dyn SpeechClient>,
    output: Arc<dyn AudioOutput>,
    events: mpsc::Sender<SpeechEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let settle = |failure: Option<String>| {
        let events = events.clone();
        async move {
            let _ = events
                .send(SpeechEvent::Finished {
                    generation,
                    error: failure,
                })
                .await;
        }
    };
    let spoken = tokio::select! {
        result = client.speak(&text) => result,
        _ = stop.changed() => return,
    };
    let payload = match spoken {
        Ok(payload) => payload,
        Err(error) => return settle(Some(error)).await,
    };
    // The container is decoded before any device is opened, so a payload this
    // build cannot read never reaches the audio layer.
    let decoded = match decode_wav(&payload) {
        Ok(decoded) => decoded,
        Err(error) => return settle(Some(error.to_string())).await,
    };
    if *stop.borrow() {
        return;
    }
    let started = tokio::task::spawn_blocking(move || output.start(decoded)).await;
    let mut playback = match started {
        Ok(Ok(playback)) => playback,
        Ok(Err(error)) => return settle(Some(error.to_string())).await,
        Err(error) => return settle(Some(format!("Audio output worker failed: {error}"))).await,
    };
    let _ = events
        .send(SpeechEvent::PlaybackStarted { generation })
        .await;
    let stopped = tokio::select! {
        () = playback.finished() => false,
        _ = stop.changed() => true,
    };
    // The stream is closed off the event loop, exactly as the recorder disposes
    // of its own.
    let _ = tokio::task::spawn_blocking(move || drop(playback)).await;
    if !stopped {
        settle(None).await;
    }
}

#[cfg(test)]
#[path = "speech_tests.rs"]
mod speech_tests;
