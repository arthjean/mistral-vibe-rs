//! The speech transport: a turn summary posted to the configured speech model,
//! and the decoded audio played through the default output device.
//!
//! The reference builds one client per configuration from the active `tts_models`
//! entry and its provider, posts the summary to that provider's `api_base` with
//! the request metadata its getter returns, and base64-decodes the audio the
//! response carries before handing it to the player
//! (`vibe/cli/tts/mistral_tts_client.py`). The narrator holds no client at all
//! when the configuration resolves no speech entry, which is the gate that keeps
//! a turn silent rather than failing it
//! (`vibe/cli/narrator_manager/narrator_manager.py`, `_make_tts_client`).
//!
//! This module owns that transport. The state machine in [`crate::narrator`]
//! stays pure: it emits `Speak`, the manager here runs the request and the
//! playback off the caller's loop, and the two events it answers with drive the
//! same machine back to idle.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use url::Url;

use crate::identity::{MetadataGetter, metadata_object, no_metadata, user_agent};
use crate::playback::{AudioOutput, CpalAudioOutput, PlaybackError, decode_wav};
use crate::settings::{CredentialLookup, SpeechSettings, resolve_credential};

/// The path the speech endpoint is served under, appended to whatever path the
/// configured `api_base` already carries so a gateway served below a prefix is
/// addressed under it.
const SPEECH_PATH: &str = "/v1/audio/speech";
/// This port's own bound on one speech request. The reference leaves the SDK's
/// default in place; a summary that never answers must not hold the narrator in
/// its speaking state for the rest of the session.
const SPEECH_TIMEOUT: Duration = Duration::from_secs(60);
const EVENT_QUEUE_CAPACITY: usize = 16;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a summary was not spoken: the message an operator reads and the class
/// name the reference's exception carries, which is what the read-aloud
/// telemetry reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechFailure {
    pub message: String,
    pub class: String,
}

impl SpeechFailure {
    #[must_use]
    pub fn new(class: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class: class.to_owned(),
        }
    }
}

impl From<PlaybackError> for SpeechFailure {
    fn from(error: PlaybackError) -> Self {
        Self::new(error.class_name(), error.to_string())
    }
}

/// What the narrator state machine is driven by once a summary is spoken.
///
/// Every answer carries the generation that asked for it, so a result that
/// outlived its turn settles nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeechEvent {
    /// Reference `_speak_summary` once the player accepted the buffer.
    PlaybackStarted { generation: u64 },
    /// Reference `_on_playback_finished` and the failure path that settles the
    /// same generation without playing anything.
    Finished {
        generation: u64,
        error: Option<SpeechFailure>,
    },
}

/// The request the configured speech surface describes, built before anything is
/// opened so it can be compared against the reference's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechRequest {
    pub endpoint: Url,
    pub body: Value,
}

impl SpeechRequest {
    /// Reference `MistralTTSClient.speak`'s request: the configured model,
    /// voice and output format, the text, and the metadata the getter
    /// returned.
    ///
    /// # Errors
    ///
    /// An `api_base` that is not an HTTP URL.
    pub fn resolve(
        settings: &SpeechSettings,
        text: &str,
        metadata: &crate::identity::RequestMetadata,
    ) -> Result<Self, String> {
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
                "metadata": metadata_object(metadata),
            }),
        })
    }
}

pub type SpeechFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, SpeechFailure>> + Send + 'a>>;

/// The network boundary: a summary in, decoded audio bytes out.
pub trait SpeechClient: Send + Sync + 'static {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a>;
}

/// Reference `MistralTTSClient`.
pub struct HttpSpeechClient {
    client: reqwest::Client,
    settings: SpeechSettings,
    credential: SecretString,
    metadata: MetadataGetter,
}

impl HttpSpeechClient {
    /// # Errors
    ///
    /// An endpoint [`SpeechRequest::resolve`] refuses, or an HTTP client that
    /// cannot start.
    pub fn new(
        settings: SpeechSettings,
        credential: String,
        metadata: MetadataGetter,
    ) -> Result<Self, String> {
        // The endpoint is resolved once here so an unusable `api_base` is
        // reported as an unresolved configuration rather than at the first turn.
        SpeechRequest::resolve(&settings, "", &Vec::new())?;
        let client = reqwest::Client::builder()
            .timeout(SPEECH_TIMEOUT)
            .build()
            .map_err(|error| format!("Speech transport could not start: {error}"))?;
        Ok(Self {
            client,
            settings,
            credential: SecretString::from(credential),
            metadata,
        })
    }
}

impl SpeechClient for HttpSpeechClient {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a> {
        Box::pin(async move {
            let request = SpeechRequest::resolve(&self.settings, text, &(self.metadata)())
                .map_err(|error| SpeechFailure::new("SDKError", error))?;
            let response = self
                .client
                .post(request.endpoint.clone())
                .bearer_auth(self.credential.expose_secret())
                .header(reqwest::header::USER_AGENT, user_agent())
                .json(&request.body)
                .send()
                .await
                .map_err(|error| {
                    SpeechFailure::new(
                        transport_class(&error),
                        format!("Speech request failed: {}", transport_reason(&error)),
                    )
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(SpeechFailure::new(
                    "SDKError",
                    format!("Speech request was refused with status {status}"),
                ));
            }
            let payload: Value = response.json().await.map_err(|error| {
                SpeechFailure::new(
                    "ResponseValidationError",
                    format!("Speech response is invalid: {error}"),
                )
            })?;
            let encoded = payload
                .get("audio_data")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SpeechFailure::new(
                        "ResponseValidationError",
                        "Speech response carries no audio",
                    )
                })?;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| {
                    SpeechFailure::new(
                        "Error",
                        format!("Spoken audio could not be decoded: {error}"),
                    )
                })
        })
    }
}

/// The `httpx` exception a transport failure is raised as.
fn transport_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "ReadTimeout"
    } else if error.is_connect() {
        "ConnectError"
    } else {
        "TransportError"
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
pub struct SpeechManager {
    /// The client the configuration resolved to, or why it resolved to none.
    /// Reference `tts_client is not None`: a configuration this build cannot
    /// address keeps the narrator out of its speaking state instead of failing
    /// the turn.
    client: Result<Arc<dyn SpeechClient>, String>,
    output: Arc<dyn AudioOutput>,
    credentials: CredentialLookup,
    metadata: MetadataGetter,
    events_tx: mpsc::Sender<SpeechEvent>,
    events_rx: mpsc::Receiver<SpeechEvent>,
    /// Every spoken summary still running, the latest last. An earlier one
    /// keeps playing when a later one is asked for, as the reference's player
    /// does.
    running: Vec<ActiveSpeech>,
    /// Reference `AudioPlayer.is_playing`: one playback at a time.
    playing: Arc<AtomicBool>,
}

impl SpeechManager {
    /// Resolves the speech surface from the published configuration: the model,
    /// the voice, the output format, the endpoint and the credential the
    /// provider entry names.
    #[must_use]
    pub fn production(
        config_view: &Value,
        credentials: CredentialLookup,
        metadata: MetadataGetter,
    ) -> Self {
        Self::with_output(
            config_view,
            Arc::new(CpalAudioOutput),
            credentials,
            metadata,
        )
    }

    /// A manager resolved from `config_view` that plays through `output`.
    #[must_use]
    pub fn with_output(
        config_view: &Value,
        output: Arc<dyn AudioOutput>,
        credentials: CredentialLookup,
        metadata: MetadataGetter,
    ) -> Self {
        let mut manager = Self::empty(output, credentials, metadata);
        manager.resync(config_view);
        manager
    }

    /// A manager over a scripted transport and a scripted device, so the whole
    /// speech path is driven in a test that has neither.
    #[must_use]
    pub fn scripted(client: Arc<dyn SpeechClient>, output: Arc<dyn AudioOutput>) -> Self {
        let mut manager = Self::empty(output, Arc::new(|_: &str| None), no_metadata());
        manager.client = Ok(client);
        manager
    }

    fn empty(
        output: Arc<dyn AudioOutput>,
        credentials: CredentialLookup,
        metadata: MetadataGetter,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        Self {
            client: Err("Narration is not configured".to_owned()),
            output,
            credentials,
            metadata,
            events_tx,
            events_rx,
            running: Vec::new(),
            playing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Reference `NarratorManager.sync`: the client is rebuilt from the
    /// configuration as it stands now rather than kept from process start, so an
    /// edited active model, provider or credential variable reaches the next
    /// turn.
    pub fn resync(&mut self, config_view: &Value) {
        self.client = SpeechSettings::from_config_view(config_view).and_then(|settings| {
            let credential = resolve_credential(&settings.api_key_env_var, &self.credentials);
            let client: Arc<dyn SpeechClient> = Arc::new(HttpSpeechClient::new(
                settings,
                credential,
                Arc::clone(&self.metadata),
            )?);
            Ok(client)
        });
    }

    /// Reference `tts_client is not None`.
    #[must_use]
    pub fn available(&self) -> bool {
        self.client.is_ok()
    }

    /// Reference `_speak_summary`: the summary is posted, and the audio it
    /// answers with is played. A configuration that resolves to no client is
    /// reported through the same event path rather than as a start failure, so
    /// the narrator always settles.
    pub fn speak(&mut self, generation: u64, text: String) {
        let client = match self.client.as_ref() {
            Ok(client) => Arc::clone(client),
            Err(reason) => {
                self.settle_now(generation, SpeechFailure::new("KeyError", reason.clone()));
                return;
            }
        };
        let (stop, stop_rx) = watch::channel(false);
        let speech = Speech {
            generation,
            client,
            output: Arc::clone(&self.output),
            events: self.events_tx.clone(),
            playing: Arc::clone(&self.playing),
        };
        let task = tokio::spawn(speech.run(text, stop_rx));
        self.reap_finished();
        self.running.push(ActiveSpeech { stop, task });
    }

    /// Reference `cancel`: the summary task is dropped and playback stops before
    /// the state machine returns to idle.
    pub fn stop(&mut self) {
        for active in &self.running {
            let _ = active.stop.send(true);
        }
    }

    #[must_use]
    pub fn try_next_event(&mut self) -> Option<SpeechEvent> {
        let event = self.events_rx.try_recv().ok();
        self.reap_finished();
        event
    }

    /// Waits for the transport's next answer.
    pub async fn next_event(&mut self) -> Option<SpeechEvent> {
        let event = self.events_rx.recv().await;
        self.reap_finished();
        event
    }

    pub async fn shutdown(&mut self) {
        self.stop();
        for mut active in self.running.drain(..) {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut active.task)
                .await
                .is_err()
            {
                active.task.abort();
                let _ = active.task.await;
            }
        }
    }

    /// Answers a request that never reached the transport, so the caller learns
    /// the reason through the one event path it already drains.
    fn settle_now(&mut self, generation: u64, failure: SpeechFailure) {
        let _ = self.events_tx.try_send(SpeechEvent::Finished {
            generation,
            error: Some(failure),
        });
    }

    fn reap_finished(&mut self) {
        self.running.retain(|active| !active.task.is_finished());
    }
}

impl Drop for SpeechManager {
    fn drop(&mut self) {
        for active in self.running.drain(..) {
            let _ = active.stop.send(true);
            active.task.abort();
        }
    }
}

/// One spoken summary: the request, the decode, the playback and the completion.
/// A stop at any point returns without answering, because the state machine that
/// asked for the stop has already returned to idle.
struct Speech {
    generation: u64,
    client: Arc<dyn SpeechClient>,
    output: Arc<dyn AudioOutput>,
    events: mpsc::Sender<SpeechEvent>,
    playing: Arc<AtomicBool>,
}

impl Speech {
    async fn settle(&self, error: Option<SpeechFailure>) {
        let _ = self
            .events
            .send(SpeechEvent::Finished {
                generation: self.generation,
                error,
            })
            .await;
    }

    async fn run(self, text: String, mut stop: watch::Receiver<bool>) {
        let spoken = tokio::select! {
            result = self.client.speak(&text) => result,
            _ = stop.changed() => return,
        };
        let payload = match spoken {
            Ok(payload) => payload,
            Err(error) => return self.settle(Some(error)).await,
        };
        // Reference `_speak_summary` enters its speaking state as soon as the
        // speech endpoint answers, before the player is asked to play, so a
        // player that refuses ends a narration that spoke.
        let _ = self
            .events
            .send(SpeechEvent::PlaybackStarted {
                generation: self.generation,
            })
            .await;
        // Reference `AudioPlayer.play`: a playback still running refuses the
        // next one first, and leaves the running one alone.
        if self
            .playing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self
                .settle(Some(PlaybackError::AlreadyPlaying.into()))
                .await;
        }
        let stopped = self.play(&payload, &mut stop).await;
        self.playing.store(false, Ordering::Release);
        match stopped {
            Ok(true) => {}
            Ok(false) => self.settle(None).await,
            Err(failure) => self.settle(Some(failure)).await,
        }
    }

    /// Plays `payload` to its end, answering whether a stop cut it short.
    async fn play(
        &self,
        payload: &[u8],
        stop: &mut watch::Receiver<bool>,
    ) -> Result<bool, SpeechFailure> {
        // The container is decoded before any device is opened, so a payload
        // this build cannot read never reaches the audio layer.
        let decoded = decode_wav(payload)
            .map_err(|error| SpeechFailure::new(error.class, error.to_string()))?;
        if *stop.borrow() {
            return Ok(true);
        }
        let output = Arc::clone(&self.output);
        let mut playback = match tokio::task::spawn_blocking(move || output.start(decoded)).await {
            Ok(Ok(playback)) => playback,
            Ok(Err(error)) => return Err(error.into()),
            Err(error) => {
                return Err(SpeechFailure::new(
                    "AudioBackendUnavailableError",
                    format!("Audio output worker failed: {error}"),
                ));
            }
        };
        let stopped = tokio::select! {
            () = playback.finished() => false,
            _ = stop.changed() => true,
        };
        // The stream is closed off the event loop, exactly as the recorder
        // disposes of its own.
        let _ = tokio::task::spawn_blocking(move || drop(playback)).await;
        Ok(stopped)
    }
}

#[cfg(test)]
#[path = "speech_tests.rs"]
mod speech_tests;
