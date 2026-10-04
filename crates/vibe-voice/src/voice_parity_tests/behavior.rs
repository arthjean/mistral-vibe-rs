//! The behavior families: what the reference's audio stack does past
//! resolution, replayed against this crate over the same scripted stand-ins.
//!
//! `scripts/parity/voice_behavior.py` drives the reference's recorder meter,
//! player, WAV decoder, realtime client, dictation and read-aloud managers and
//! turn summary service with every device, socket and endpoint scripted, and
//! records what each one did. Each reader below plays the same script against
//! this crate's counterpart and compares the record field by field. A text the
//! reference authors is held in the corpus as its length and SHA-256 only, so
//! this port's text is reduced the same way before it is compared. The
//! `acpBridge` family is read by the editor adapter, which owns the bridge, in
//! `crates/vibe-acp/src/agent/voice/voice_parity_tests.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use vibe_core::llm::{BackendContext, MapCredentials};
use vibe_core::narration::{TurnSummaryInput, summarize};
use vibe_core::provider::config::{ApiSettings, BackendKind, ProviderConfig};
use vibe_core::telemetry::{TelemetryCallType, TelemetryContext, TelemetryRecord};

use super::Report;
use crate::capture::{
    AudioRecorder, AudioStream, CAPTURE_BUFFER_MS, CAPTURE_CHANNELS, CAPTURE_SAMPLE_FORMAT,
    RecorderError, SignalMeter,
};
use crate::identity::{MetadataGetter, audio_request_metadata};
use crate::manager::{StartRequest, VoiceEvent, VoiceManager};
use crate::narrator::{NarratorEffect, NarratorManager};
use crate::playback::{
    AudioOutput, DecodedAudio, PLAYBACK_BUFFER_MS, PLAYBACK_SAMPLE_FORMAT, Playback, PlaybackError,
    decode_wav,
};
use crate::settings::{CredentialLookup, TranscriptionSettings};
use crate::speech::{SpeechClient, SpeechEvent, SpeechFailure, SpeechFuture, SpeechManager};
use crate::test_endpoint::{EndpointStep, ScriptedEndpoint};
use crate::transcribe::{
    RealtimeTranscribeClient, TranscribeClient, TranscribeEvent, TranscribeFuture,
};

/// What the oracle writes in place of a version string and a credential.
const VERSION_MARK: &str = "<version>";
const CREDENTIAL_MARK: &str = "<credential>";
/// The credential the transcription replay opens with, replaced by
/// [`CREDENTIAL_MARK`] before its header is compared.
const CREDENTIAL: &str = "fixture-replay-credential";
/// The request identifier every scripted session announces, authored by the
/// oracle.
const REQUEST_ID: &str = "fixture-request";
/// The detail an unavailable backend is raised with, authored by the oracle.
const BACKEND_DETAIL: &str = "fixture backend detail";
/// The drain timeout the oracle patches in, so a stop that outlives it settles
/// at once.
const SHORT_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
/// How long a step is given to settle before it is observed, standing for the
/// oracle's loop turns.
const SETTLE: Duration = Duration::from_millis(40);

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SignalCase {
    case: String,
    blocks: Vec<Vec<i16>>,
    readings: Vec<Reading>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reading {
    peak: f64,
    has_signal: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct DeviceCase {
    case: String,
    kind: String,
    sample_rate: Option<u32>,
    rate: Option<u32>,
    channels: Option<u16>,
    opened: Vec<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WavCase {
    case: String,
    payload: String,
    decoded: Option<Decoded>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Decoded {
    sample_rate: u32,
    channels: u16,
    pcm_bytes: usize,
    pcm_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MetadataCase {
    case: String,
    session_id: String,
    parent_session_id: Option<String>,
    keys: Vec<String>,
    values: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct StreamCase {
    case: String,
    chunks: Vec<String>,
    keep_open: bool,
    script: Vec<Value>,
    events: Vec<Value>,
    sent: Vec<Value>,
    path: String,
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct VoiceCase {
    case: String,
    client: Option<Vec<Value>>,
    actions: Vec<String>,
    #[serde(default)]
    has_signal: Option<bool>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    api_key_env_var: Option<String>,
    #[serde(default)]
    start_error: Option<String>,
    results: Vec<Value>,
    listener: Vec<Value>,
    telemetry: Vec<Value>,
    recorder: Vec<String>,
    final_state: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NarratorCase {
    case: String,
    summaries: Vec<Value>,
    steps: Vec<Value>,
    #[serde(default)]
    speech_fails: Option<bool>,
    #[serde(default)]
    player_fails: Option<String>,
    #[serde(default)]
    speech_available: Option<bool>,
    states: Vec<String>,
    summary_calls: Vec<Value>,
    spoken: Vec<String>,
    played: usize,
    telemetry: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NarrationCase {
    case: String,
    params: Value,
    answer: Value,
    #[serde(default)]
    providers: Option<Vec<Value>>,
    summary: Option<String>,
    request: Option<Value>,
}

// --------------------------------------------------------------------------
// Shared readings
// --------------------------------------------------------------------------

/// A text reduced the way the oracle reduces one the reference authored: its
/// length in code points and its SHA-256.
pub(super) fn prose(text: &str) -> Value {
    json!({
        "prose": text.chars().count(),
        "sha256": sha256(text.as_bytes()),
    })
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn normalize(text: &str) -> String {
    text.replace(CREDENTIAL, CREDENTIAL_MARK)
        .replace(env!("CARGO_PKG_VERSION"), VERSION_MARK)
}

/// A telemetry record as the oracle records the reference's: the event name,
/// the sorted property keys, the identifying values, an error message as
/// prose, and every duration as whether it is null.
fn telemetry_record(record: &TelemetryRecord) -> Value {
    let attributes = record
        .attributes(None)
        .expect("an audio event carries safe attributes");
    let Value::Object(properties) = serde_json::to_value(attributes).expect("attributes serialize")
    else {
        panic!("attributes serialize to an object");
    };
    let mut keys = properties.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();
    let mut observed = Map::new();
    observed.insert("name".to_owned(), json!(record.event().event_name()));
    observed.insert("keys".to_owned(), json!(keys));
    for key in [
        "recording_id",
        "transcript_length",
        "status",
        "trigger",
        "error_type",
    ] {
        if let Some(value) = properties.get(key) {
            observed.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(value) = properties.get("error_message") {
        let value = value.as_str().map_or_else(|| value.clone(), prose);
        observed.insert("error_message".to_owned(), value);
    }
    for key in [
        "recording_duration_ms",
        "transcription_duration_ms",
        "elapsed_seconds",
        "time_to_first_read_s",
    ] {
        if let Some(value) = properties.get(key) {
            let kind = if value.is_null() { "null" } else { "number" };
            observed.insert(key.to_owned(), json!(kind));
        }
    }
    Value::Object(observed)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a test runtime")
}

fn no_credentials() -> CredentialLookup {
    Arc::new(|_: &str| None)
}

/// A WAV payload a scripted speech client answers with, decodable at once.
fn fixture_wav() -> Vec<u8> {
    let data = [0_i16, 1, 2, 3]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect::<Vec<_>>();
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
    body.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_le_bytes());
    body.extend_from_slice(&data);
    let mut payload = b"RIFF".to_vec();
    payload.extend_from_slice(&u32::try_from(body.len()).unwrap_or(0).to_le_bytes());
    payload.extend_from_slice(&body);
    payload
}

/// The scripted wave payload the oracle builds for a playback case: a
/// container at `rate` on `channels` channels.
fn wav_at(rate: u32, channels: u16) -> Vec<u8> {
    let mut payload = fixture_wav();
    payload[22..24].copy_from_slice(&channels.to_le_bytes());
    payload[24..28].copy_from_slice(&rate.to_le_bytes());
    payload
}

// --------------------------------------------------------------------------
// captureSignal, audioDevices, decodeWav, audioMetadata
// --------------------------------------------------------------------------

/// Reference `AudioRecorder._process_audio` against [`SignalMeter::observe`].
pub(super) fn run_capture_signal(cases: &[SignalCase], report: &mut Report) {
    for case in cases {
        let meter = SignalMeter::default();
        let readings = case
            .blocks
            .iter()
            .map(|block| {
                meter.observe(block);
                Reading {
                    peak: (f64::from(meter.peak()) * 1e6).round() / 1e6,
                    has_signal: meter.has_signal(),
                }
            })
            .collect::<Vec<_>>();
        report.check(
            "captureSignal",
            "readings",
            &case.case,
            &case.readings,
            &readings,
        );
    }
}

/// The miniaudio name of this port's frame format.
fn miniaudio_format(format: &str) -> &'static str {
    match format {
        "int16" => "SIGNED16",
        _ => "unknown",
    }
}

/// What the reference asks miniaudio to open, against what this port's
/// stream contract and decoder answer: the recorder delivers its documented
/// frames at the requested rate, and the player opens the container's own
/// rate and channel count.
pub(super) fn run_audio_devices(cases: &[DeviceCase], report: &mut Report) {
    for case in cases {
        let opened = match case.kind.as_str() {
            "capture" => BTreeMap::from([
                ("buffersize_msec".to_owned(), CAPTURE_BUFFER_MS.to_string()),
                (
                    "input_format".to_owned(),
                    miniaudio_format(CAPTURE_SAMPLE_FORMAT).to_owned(),
                ),
                ("nchannels".to_owned(), CAPTURE_CHANNELS.to_string()),
                (
                    "sample_rate".to_owned(),
                    case.sample_rate.unwrap_or_default().to_string(),
                ),
            ]),
            _ => {
                let decoded = decode_wav(&wav_at(
                    case.rate.unwrap_or_default(),
                    case.channels.unwrap_or_default(),
                ))
                .expect("the scripted container decodes");
                BTreeMap::from([
                    ("buffersize_msec".to_owned(), PLAYBACK_BUFFER_MS.to_string()),
                    ("nchannels".to_owned(), decoded.channels.to_string()),
                    (
                        "output_format".to_owned(),
                        miniaudio_format(PLAYBACK_SAMPLE_FORMAT).to_owned(),
                    ),
                    ("sample_rate".to_owned(), decoded.sample_rate.to_string()),
                ])
            }
        };
        report.check(
            "audioDevices",
            "opened",
            &case.case,
            &case.opened,
            &vec![opened],
        );
    }
}

/// Reference `decode_wav`, which is Python's `wave` module, against
/// [`decode_wav`] over the oracle's authored payloads.
pub(super) fn run_decode_wav(cases: &[WavCase], report: &mut Report) {
    for case in cases {
        let payload = base64::engine::general_purpose::STANDARD
            .decode(&case.payload)
            .expect("the payload is base64");
        let (decoded, error) = match decode_wav(&payload) {
            Ok(audio) => (
                Some(Decoded {
                    sample_rate: audio.sample_rate,
                    channels: audio.channels,
                    pcm_bytes: audio.pcm.len(),
                    pcm_sha256: sha256(&audio.pcm),
                }),
                None,
            ),
            Err(error) => (None, Some(error.class.to_owned())),
        };
        report.check("decodeWav", "decoded", &case.case, &case.decoded, &decoded);
        report.check("decodeWav", "error", &case.case, &case.error, &error);
    }
}

/// Reference `build_audio_request_metadata` against
/// [`audio_request_metadata`].
pub(super) fn run_audio_metadata(cases: &[MetadataCase], report: &mut Report) {
    for case in cases {
        let metadata = audio_request_metadata(&case.session_id, case.parent_session_id.as_deref());
        let keys = metadata
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let values = metadata
            .iter()
            .filter(|(key, _)| {
                [
                    "session_id",
                    "parent_session_id",
                    "call_type",
                    "call_source",
                ]
                .contains(&key.as_str())
            })
            .cloned()
            .collect::<BTreeMap<_, _>>();
        report.check("audioMetadata", "keys", &case.case, &case.keys, &keys);
        report.check("audioMetadata", "values", &case.case, &case.values, &values);
    }
}

// --------------------------------------------------------------------------
// transcribeStream
// --------------------------------------------------------------------------

/// The metadata the oracle's transcription client is opened with, authored by
/// the oracle, so the header's serialization is what is compared.
fn transcribe_metadata() -> MetadataGetter {
    Arc::new(|| {
        vec![
            ("session_id".to_owned(), "fixture-session".to_owned()),
            ("note".to_owned(), "caf\u{e9} \"quoted\"".to_owned()),
        ]
    })
}

fn endpoint_steps(script: &[Value]) -> Vec<EndpointStep> {
    let mut steps = Vec::new();
    let mut closed = false;
    for step in script {
        if let Some(frame) = step.get("send") {
            steps.push(EndpointStep::Send(frame.clone()));
        } else if let Some(kind) = step.get("await").and_then(Value::as_str) {
            let kind = match kind {
                "input_audio.end" => "input_audio.end",
                other => panic!("the replay awaits no `{other}` frame"),
            };
            steps.push(EndpointStep::ReadUntil(kind));
        } else if step.get("close").is_some() {
            steps.push(EndpointStep::Close);
            closed = true;
        }
    }
    if !closed {
        steps.push(EndpointStep::AwaitClose);
    }
    steps
}

fn event_record(event: &TranscribeEvent) -> Value {
    match event {
        TranscribeEvent::SessionCreated { request_id } => {
            json!({"type": "sessionCreated", "requestId": request_id})
        }
        TranscribeEvent::TextDelta(text) => json!({"type": "textDelta", "text": text}),
        TranscribeEvent::Done => json!({"type": "done"}),
        TranscribeEvent::Error(message) => json!({"type": "error", "message": prose(message)}),
    }
}

async fn stream_case(
    case: &StreamCase,
) -> (Vec<Value>, Vec<Value>, String, BTreeMap<String, String>) {
    let endpoint = ScriptedEndpoint::start(endpoint_steps(&case.script)).await;
    let settings = TranscriptionSettings {
        model: "fixture-stream-model".to_owned(),
        sample_rate: 16_000,
        encoding: "pcm_s16le".to_owned(),
        target_streaming_delay_ms: 480,
        api_base: endpoint.http_base(),
        api_key_env_var: "MISTRAL_API_KEY".to_owned(),
    };
    let client =
        RealtimeTranscribeClient::new(&settings, CREDENTIAL.to_owned(), transcribe_metadata())
            .expect("the scripted endpoint resolves");
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    for chunk in &case.chunks {
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(chunk)
            .expect("a chunk is base64");
        audio_tx.send(chunk).expect("a queued chunk");
    }
    let held = case.keep_open.then_some(audio_tx);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let finished = tokio::time::timeout(
        Duration::from_secs(5),
        client.transcribe(audio_rx, events_tx),
    )
    .await;
    drop(held);
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event_record(&event));
    }
    if finished.is_err() {
        events.push(json!({"type": "timeout"}));
    }
    let record = endpoint.finish().await;
    let headers = case
        .headers
        .keys()
        .filter_map(|name| {
            record
                .header(name)
                .map(|value| (name.clone(), normalize(value)))
        })
        .collect();
    let path = record.path.trim_start_matches('/').to_owned();
    (events, record.frames, path, headers)
}

/// Reference `MistralTranscribeClient.transcribe` over the SDK's realtime
/// client, against [`RealtimeTranscribeClient`], both facing the same
/// scripted server.
pub(super) fn run_transcribe_stream(cases: &[StreamCase], report: &mut Report) {
    let runtime = runtime();
    for case in cases {
        let (events, sent, path, headers) = runtime.block_on(stream_case(case));
        report.check(
            "transcribeStream",
            "events",
            &case.case,
            &case.events,
            &events,
        );
        report.check("transcribeStream", "sent", &case.case, &case.sent, &sent);
        report.check("transcribeStream", "path", &case.case, &case.path, &path);
        report.check(
            "transcribeStream",
            "headers",
            &case.case,
            &case.headers,
            &headers,
        );
    }
}

// --------------------------------------------------------------------------
// voiceManager
// --------------------------------------------------------------------------

/// The oracle's scripted recorder: one queued block, a scripted signal and
/// duration, and every call it received.
struct ScriptedRecorder {
    start_error: Option<RecorderError>,
    has_signal: bool,
    duration: Duration,
    calls: Mutex<Vec<String>>,
    stream: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl ScriptedRecorder {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("the call log").clone()
    }

    fn call(&self, name: String) {
        self.calls.lock().expect("the call log").push(name);
    }

    fn end(&self) {
        self.stream.lock().expect("the stream slot").take();
    }
}

impl AudioRecorder for ScriptedRecorder {
    fn start(&self, sample_rate: u32) -> Result<AudioStream, RecorderError> {
        self.call(format!("start:{sample_rate}"));
        if let Some(error) = &self.start_error {
            return Err(error.clone());
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(vec![1, 0]).expect("the queued block");
        *self.stream.lock().expect("the stream slot") = Some(sender);
        Ok(receiver)
    }

    fn stop(&self) -> Duration {
        self.call("stop".to_owned());
        self.end();
        self.duration
    }

    fn cancel(&self) {
        self.call("cancel".to_owned());
        self.end();
    }

    fn peak(&self) -> f32 {
        0.0
    }

    fn has_signal(&self) -> bool {
        self.has_signal
    }
}

/// The oracle's scripted transcribe client. An exception the reference's
/// client raises reaches the manager the way an error event does, which is
/// the only failure this port's client reports.
struct ScriptedTranscription(Vec<Value>);

impl TranscribeClient for ScriptedTranscription {
    fn transcribe(
        &self,
        mut audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture {
        let script = self.0.clone();
        Box::pin(async move {
            for step in script {
                let text = || {
                    step.get("text")
                        .or_else(|| step.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                match step.get("step").and_then(Value::as_str) {
                    Some("created") => {
                        let _ = events.send(TranscribeEvent::SessionCreated {
                            request_id: REQUEST_ID.to_owned(),
                        });
                    }
                    Some("text") => {
                        let _ = events.send(TranscribeEvent::TextDelta(text()));
                    }
                    Some("done") => {
                        let _ = events.send(TranscribeEvent::Done);
                    }
                    Some("error") => {
                        let _ = events.send(TranscribeEvent::Error(text()));
                    }
                    Some("raise") => {
                        let _ = events.send(TranscribeEvent::Error(text()));
                        return;
                    }
                    Some("awaitEnd") => while audio.recv().await.is_some() {},
                    Some("hang") => std::future::pending::<()>().await,
                    other => panic!("the replay scripts no `{other:?}` step"),
                }
            }
        })
    }
}

fn start_error(name: Option<&str>) -> Option<RecorderError> {
    match name? {
        "AlreadyRecordingError" => Some(RecorderError::AlreadyRecording),
        "AudioBackendUnavailableError" => {
            Some(RecorderError::BackendUnavailable(BACKEND_DETAIL.to_owned()))
        }
        "NoAudioInputDeviceError" => Some(RecorderError::NoInputDevice),
        other => panic!("the replay maps no `{other}` recorder error"),
    }
}

struct VoiceObservation {
    results: Vec<Value>,
    listener: Vec<Value>,
    telemetry: Vec<Value>,
    recorder: Vec<String>,
    final_state: String,
}

async fn voice_case(case: &VoiceCase) -> VoiceObservation {
    let recorder = Arc::new(ScriptedRecorder {
        start_error: start_error(case.start_error.as_deref()),
        has_signal: case.has_signal.unwrap_or(true),
        duration: Duration::from_secs_f64(case.duration.unwrap_or(1.0)),
        calls: Mutex::new(Vec::new()),
        stream: Mutex::new(None),
    });
    let client = case
        .client
        .as_ref()
        .map(|script| Arc::new(ScriptedTranscription(script.clone())) as Arc<dyn TranscribeClient>);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let manager = VoiceManager::with_drain_timeout(
        Arc::clone(&recorder) as Arc<dyn AudioRecorder>,
        client,
        no_credentials(),
        events_tx,
        SHORT_DRAIN_TIMEOUT,
    );
    let mut results = Vec::new();
    for action in &case.actions {
        let mut result = Map::new();
        result.insert("action".to_owned(), json!(action));
        match action.as_str() {
            "start" => {
                let request = StartRequest {
                    tag: 0,
                    sample_rate: 16_000,
                    api_key_env_var: case.api_key_env_var.clone().unwrap_or_default(),
                };
                if let Err(error) = manager.start_recording(request).await {
                    result.insert("error".to_owned(), prose(&error.0));
                }
            }
            "stop" => manager.stop_recording().await,
            "cancel" => manager.cancel_recording(),
            other => panic!("the replay performs no `{other}` action"),
        }
        tokio::time::sleep(SETTLE).await;
        results.push(Value::Object(result));
    }
    manager.close().await;
    tokio::time::sleep(SETTLE).await;
    let mut listener = Vec::new();
    let mut telemetry = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        match event {
            VoiceEvent::State { state, .. } => listener.push(json!({"state": state.label()})),
            VoiceEvent::Text { text, .. } => listener.push(json!({"text": text})),
            VoiceEvent::Error { message, .. } => listener.push(json!({"error": prose(&message)})),
            VoiceEvent::Notice { message, .. } => {
                listener.push(json!({"notice": prose(&message)}));
            }
            VoiceEvent::Telemetry(record) => telemetry.push(telemetry_record(&record)),
        }
    }
    VoiceObservation {
        results,
        listener,
        telemetry,
        recorder: recorder.calls(),
        final_state: manager.state().label().to_owned(),
    }
}

/// Reference `VoiceManager` against [`VoiceManager`], over the same scripted
/// recorder and client.
pub(super) fn run_voice_manager(cases: &[VoiceCase], report: &mut Report) {
    let runtime = runtime();
    for case in cases {
        let observed = runtime.block_on(voice_case(case));
        let family = "voiceManager";
        report.check(
            family,
            "results",
            &case.case,
            &case.results,
            &observed.results,
        );
        report.check(
            family,
            "listener",
            &case.case,
            &case.listener,
            &observed.listener,
        );
        report.check(
            family,
            "telemetry",
            &case.case,
            &case.telemetry,
            &observed.telemetry,
        );
        report.check(
            family,
            "recorder",
            &case.case,
            &case.recorder,
            &observed.recorder,
        );
        report.check(
            family,
            "finalState",
            &case.case,
            &case.final_state,
            &observed.final_state,
        );
    }
}

// --------------------------------------------------------------------------
// narrator
// --------------------------------------------------------------------------

/// The oracle's scripted speech client: it records what it was asked to say
/// and answers with a decodable payload, or fails with the class the oracle
/// raises.
struct ScriptedSpeech {
    fail: bool,
    spoken: Mutex<Vec<String>>,
}

impl SpeechClient for ScriptedSpeech {
    fn speak<'a>(&'a self, text: &'a str) -> SpeechFuture<'a> {
        self.spoken
            .lock()
            .expect("the spoken log")
            .push(text.to_owned());
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err(SpeechFailure::new(
                    "FixtureSpeechError",
                    "fixture speech failure",
                ))
            } else {
                Ok(fixture_wav())
            }
        })
    }
}

/// The oracle's scripted player: it plays until told to finish, or refuses
/// with the class the oracle names.
struct ScriptedOutput {
    fail: Option<String>,
    played: AtomicUsize,
    current: Mutex<Option<watch::Sender<bool>>>,
}

impl ScriptedOutput {
    fn finish(&self) {
        if let Some(current) = self.current.lock().expect("the playback slot").take() {
            let _ = current.send(true);
        }
    }
}

impl AudioOutput for ScriptedOutput {
    fn start(&self, _audio: DecodedAudio) -> Result<Playback, PlaybackError> {
        if let Some(class) = &self.fail {
            assert_eq!(
                class, "NoAudioOutputDeviceError",
                "a scripted device failure"
            );
            return Err(PlaybackError::NoOutputDevice(
                "fixture device failure".to_owned(),
            ));
        }
        self.played.fetch_add(1, Ordering::SeqCst);
        let (finished_tx, finished_rx) = watch::channel(false);
        *self.current.lock().expect("the playback slot") = Some(finished_tx);
        Ok(Playback::new(finished_rx, Box::new(())))
    }
}

/// The narrator and its speech transport driven the way both adapters drive
/// them: an effect is executed, a summary answer and a speech event are fed
/// back, and every state the machine passes through is observed.
struct NarratorDriver {
    narrator: NarratorManager,
    speech: SpeechManager,
    answers: VecDeque<Value>,
    pending: VecDeque<(u64, Option<String>)>,
    summary_calls: Vec<Value>,
    states: Vec<&'static str>,
    telemetry: Vec<Value>,
}

impl NarratorDriver {
    fn observe(&mut self) {
        let state = self.narrator.state().label();
        if self.states.last() != Some(&state) {
            self.states.push(state);
        }
        for record in self.narrator.take_telemetry() {
            self.telemetry.push(telemetry_record(&record));
        }
    }

    fn apply(&mut self, effect: Option<NarratorEffect>) {
        self.observe();
        match effect {
            Some(NarratorEffect::Summarize {
                generation,
                user_message,
                assistant_text,
                error,
                message_id,
            }) => {
                self.summary_calls.push(json!({
                    "assistantText": assistant_text,
                    "error": error,
                    "messageId": message_id,
                    "userMessage": user_message,
                }));
                match self.answers.pop_front() {
                    Some(Value::String(summary)) => {
                        self.pending.push_back((generation, Some(summary)));
                    }
                    Some(Value::Object(answer)) if answer.contains_key("hang") => {}
                    // A summary that failed reaches the narrator as no summary
                    // at all, as the reference's tracker reports it.
                    _ => self.pending.push_back((generation, None)),
                }
            }
            Some(NarratorEffect::Speak { generation, text }) => {
                self.speech.speak(generation, text);
            }
            Some(NarratorEffect::Stop) => self.speech.stop(),
            None => {}
        }
    }

    async fn settle(&mut self) {
        loop {
            if let Some((generation, summary)) = self.pending.pop_front() {
                let effect = self.narrator.apply_summary(generation, summary);
                self.apply(effect);
                continue;
            }
            match tokio::time::timeout(SETTLE, self.speech.next_event()).await {
                Ok(Some(SpeechEvent::PlaybackStarted { generation })) => {
                    self.narrator.playback_started(generation);
                }
                Ok(Some(SpeechEvent::Finished { generation, error })) => match error {
                    Some(failure) => self.narrator.fail(generation, failure.class),
                    None => self.narrator.settle(generation),
                },
                _ => break,
            }
            self.observe();
        }
        self.observe();
    }
}

struct NarratorObservation {
    states: Vec<String>,
    summary_calls: Vec<Value>,
    spoken: Vec<String>,
    played: usize,
    telemetry: Vec<Value>,
}

async fn narrator_case(case: &NarratorCase) -> NarratorObservation {
    let client = Arc::new(ScriptedSpeech {
        fail: case.speech_fails.unwrap_or(false),
        spoken: Mutex::new(Vec::new()),
    });
    let output = Arc::new(ScriptedOutput {
        fail: case.player_fails.clone(),
        played: AtomicUsize::new(0),
        current: Mutex::new(None),
    });
    let mut driver = NarratorDriver {
        narrator: NarratorManager::new(true, case.speech_available.unwrap_or(true)),
        speech: SpeechManager::scripted(
            Arc::clone(&client) as Arc<dyn SpeechClient>,
            Arc::clone(&output) as Arc<dyn AudioOutput>,
        ),
        answers: case.summaries.iter().cloned().collect(),
        pending: VecDeque::new(),
        summary_calls: Vec::new(),
        states: vec!["idle"],
        telemetry: Vec::new(),
    };
    for step in &case.steps {
        let text = |key: &str| step.get(key).and_then(Value::as_str).unwrap_or_default();
        if step.get("turnStart").is_some() {
            driver.narrator.on_turn_start(text("turnStart"));
        } else if step.get("userMessage").is_some() {
            driver.narrator.on_user_message(text("userMessage"));
        } else if step.get("assistantText").is_some() {
            driver.narrator.on_assistant_text(text("assistantText"));
        } else if step.get("turnError").is_some() {
            driver.narrator.on_turn_error(text("turnError"));
        } else if step.get("turnCancel").is_some() {
            driver.narrator.on_turn_cancel();
        } else if step.get("turnEnd").is_some() {
            let effect = driver.narrator.on_turn_end();
            driver.apply(effect);
        } else if step.get("cancel").is_some() {
            let effect = driver.narrator.cancel();
            driver.apply(effect);
        } else if step.get("finish").is_some() {
            output.finish();
        }
        driver.settle().await;
    }
    let effect = driver.narrator.shutdown();
    driver.apply(effect);
    driver.speech.shutdown().await;
    driver.observe();
    let spoken = client.spoken.lock().expect("the spoken log").clone();
    NarratorObservation {
        states: driver.states[1..]
            .iter()
            .map(|state| (*state).to_owned())
            .collect(),
        summary_calls: driver.summary_calls,
        spoken,
        played: output.played.load(Ordering::SeqCst),
        telemetry: driver.telemetry,
    }
}

/// The states a listener of the reference heard, as transitions: a listener
/// is told of every `_set_state` call, including one that names the state it
/// is already in, which is no transition at all.
fn transitions(states: &[String]) -> Vec<String> {
    let mut transitions: Vec<String> = Vec::new();
    let mut last = "idle";
    for state in states {
        if state != last {
            transitions.push(state.clone());
            last = state;
        }
    }
    transitions
}

/// Reference `NarratorManager` and `TurnSummaryTracker` against
/// [`NarratorManager`] and [`SpeechManager`], over the same scripted summary,
/// speech client and player.
pub(super) fn run_narrator(cases: &[NarratorCase], report: &mut Report) {
    let runtime = runtime();
    for case in cases {
        let observed = runtime.block_on(narrator_case(case));
        let family = "narrator";
        report.check(
            family,
            "states",
            &case.case,
            &transitions(&case.states),
            &observed.states,
        );
        report.check(
            family,
            "summaryCalls",
            &case.case,
            &case.summary_calls,
            &observed.summary_calls,
        );
        report.check(family, "spoken", &case.case, &case.spoken, &observed.spoken);
        report.check(family, "played", &case.case, &case.played, &observed.played);
        report.check(
            family,
            "telemetry",
            &case.case,
            &case.telemetry,
            &observed.telemetry,
        );
    }
}

// --------------------------------------------------------------------------
// narrationService
// --------------------------------------------------------------------------

/// The credential the default provider's variable carries during the replay.
const NARRATION_CREDENTIAL: &str = "fixture-narration-credential";

type Exchange = (String, Value);

/// Answers one chat completion with `status` and `body`, and hands back the
/// request head and its JSON body; a run that never calls leaves it waiting.
async fn one_shot(status: u16, body: Value) -> (String, tokio::task::JoinHandle<Exchange>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}/v1", listener.local_addr().expect("an address"));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("a connection");
        let mut buffer = Vec::new();
        let mut piece = [0_u8; 8_192];
        let head_end = loop {
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            let read = socket.read(&mut piece).await.expect("a read");
            buffer.extend_from_slice(&piece[..read]);
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buffer.len() < head_end + 4 + length {
            let read = socket.read(&mut piece).await.expect("a read");
            buffer.extend_from_slice(&piece[..read]);
        }
        let request = serde_json::from_slice(&buffer[head_end + 4..]).unwrap_or(Value::Null);
        let payload = body.to_string();
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{payload}",
            payload.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("a write");
        (head, request)
    });
    (base, task)
}

fn completion(content: &Value) -> Value {
    json!({
        "id": "summary-1",
        "object": "chat.completion",
        "model": "mistral-vibe-cli-fast",
        "created": 0,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The request as the oracle records the reference's: the system prompt is
/// this port's own prose, so only its role and presence are compared.
fn request_record(head: &str, body: &Value) -> Value {
    let messages = body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .map(|message| {
                    let content = message["content"].as_str().unwrap_or_default();
                    json!({"role": message["role"], "content": prose(content)})
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let metadata = body["metadata"].as_object().cloned().unwrap_or_default();
    let mut keys = metadata.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();
    json!({
        "provider": "mistral",
        "model": body["model"],
        "temperature": body["temperature"],
        "maxTokens": body["max_tokens"],
        "messages": messages,
        "userAgent": normalize(header(head, "user-agent").unwrap_or_default()),
        "metadataKeys": keys,
        "metadata": {
            "call_type": metadata.get("call_type").cloned().unwrap_or(Value::Null),
            "message_id": metadata.get("message_id").cloned().unwrap_or(Value::Null),
            "session_id": metadata.get("session_id").cloned().unwrap_or(Value::Null),
        },
    })
}

/// Blanks the system prompt's prose, which this port authors itself.
fn without_system_prose(request: &Value) -> Value {
    let mut request = request.clone();
    if let Some(messages) = request["messages"].as_array_mut() {
        for message in messages {
            if message["role"] == "system" {
                let authored = message["content"]["prose"].as_u64().unwrap_or(0) > 0;
                message["content"] = json!({"authored": authored});
            }
        }
    }
    request
}

async fn narration_case(case: &NarrationCase) -> (Option<String>, Option<Value>) {
    let (status, body) = if case.answer.get("raise").is_some() {
        (400, json!({"message": "fixture backend failure"}))
    } else {
        (200, completion(&case.answer["content"]))
    };
    let (base, server) = one_shot(status, body).await;
    let provider = |name: &str, variable: &str| {
        let mut provider = ProviderConfig::new(name, &base);
        provider.backend = BackendKind::Mistral;
        provider.api_key_env_var = variable.to_owned();
        provider
    };
    let providers = case.providers.as_ref().map_or_else(
        || vec![provider("mistral", "MISTRAL_API_KEY")],
        |entries| {
            entries
                .iter()
                .map(|entry| {
                    provider(
                        entry["name"].as_str().unwrap_or_default(),
                        entry["api_key_env_var"].as_str().unwrap_or_default(),
                    )
                })
                .collect()
        },
    );
    let context = BackendContext::ambient(
        ApiSettings::default(),
        Arc::new(MapCredentials(BTreeMap::from([(
            "MISTRAL_API_KEY".to_owned(),
            NARRATION_CREDENTIAL.to_owned(),
        )]))),
    );
    let text = |key: &str| case.params.get(key).and_then(Value::as_str);
    let session_id = "fixture-session";
    // The metadata the app-server builds for `narration/summarize`.
    let metadata = TelemetryContext::default()
        .request_metadata(
            Some(session_id),
            TelemetryCallType::SecondaryCall,
            text("messageId").map(ToOwned::to_owned),
        )
        .properties();
    let input = TurnSummaryInput {
        user_message: text("userMessage").unwrap_or_default(),
        assistant_text: text("assistantText").unwrap_or_default(),
        error: text("error"),
    };
    let summary = summarize(&providers, &context, &input, &metadata).await;
    // A run that never called leaves the server waiting, which is how no
    // request is told from one.
    let request = match tokio::time::timeout(SETTLE * 5, server).await {
        Ok(Ok((head, body))) => Some(request_record(&head, &body)),
        _ => None,
    };
    (summary, request)
}

/// Reference `NarrationService.summarize` against
/// [`vibe_core::narration::summarize`] with the metadata the app-server
/// builds, both answered by a scripted completion.
pub(super) fn run_narration_service(cases: &[NarrationCase], report: &mut Report) {
    let runtime = runtime();
    for case in cases {
        let (summary, request) = runtime.block_on(narration_case(case));
        let family = "narrationService";
        report.check(family, "summary", &case.case, &case.summary, &summary);
        report.check(
            family,
            "request",
            &case.case,
            &case.request.as_ref().map(without_system_prose),
            &request.as_ref().map(without_system_prose),
        );
    }
}
