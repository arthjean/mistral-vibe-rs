//! Realtime transcription.
//!
//! Reference `MistralTranscribeClient.transcribe`
//! (`vibe/cli/transcribe/mistral_transcribe_client.py`) hands the recorder's
//! stream to the Mistral SDK's realtime transcription, which opens a websocket
//! on `{api_base}/v1/audio/transcriptions/realtime?model=...`, waits for the
//! session to be created, sends the session's audio format and streaming
//! delay, then streams the audio while it reads events back
//! (`mistralai/extra/realtime/transcription.py`). The client maps those events
//! onto four: the session's identifier, a piece of text, the end of the
//! transcription, and an error. An end-of-stream flush refused because no
//! audio was captured is read as the benign end it is, and a frame the SDK's
//! models would refuse is skipped as the SDK skips an unknown event.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::{WebSocketStream, connect_async};
use url::Url;

use crate::capture::AudioStream;
use crate::identity::{MetadataGetter, metadata_header, user_agent};
use crate::settings::{CredentialLookup, TranscriptionSettings, resolve_credential};

/// The websocket client's opening deadline, which is the `websockets`
/// library's own default.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The path the realtime transcription endpoint is served under, appended to
/// whatever path the configured `api_base` already carries.
const REALTIME_PATH: &str = "/v1/audio/transcriptions/realtime";
/// Reference `_EMPTY_RECORDING_MARKER`: what the endpoint says when it refuses
/// to flush a recording that never sent a byte.
const EMPTY_RECORDING_MARKER: &str = "before sending any audio bytes";
/// What `_extract_error_message` reads from an error frame it cannot read a
/// message from.
const GENERIC_ERROR: &str = "Realtime transcription error";

/// Reference `TranscribeEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscribeEvent {
    SessionCreated { request_id: String },
    TextDelta(String),
    Done,
    Error(String),
}

pub type TranscribeFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Reference `TranscribeClientPort`: the recorder's stream in, events out, the
/// returned future settling once the transcription is over.
pub trait TranscribeClient: Send + Sync + 'static {
    fn transcribe(
        &self,
        audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture;
}

/// Where and how a realtime session is opened, resolved from the configured
/// model and provider before anything connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealtimeEndpoint {
    pub url: Url,
    pub encoding: String,
    pub sample_rate: u32,
    pub target_streaming_delay_ms: u32,
}

impl RealtimeEndpoint {
    /// Reference `RealtimeTranscription._build_url` and the scheme swap in
    /// `connect`: the realtime path under the provider's `api_base`, the
    /// model merged into its query, `https` read as `wss` and `http` as `ws`.
    ///
    /// # Errors
    ///
    /// An `api_base` that is not a URL, or names a scheme no websocket runs on.
    pub fn resolve(settings: &TranscriptionSettings) -> Result<Self, String> {
        let mut url = Url::parse(&settings.api_base)
            .map_err(|error| format!("Voice API URL is invalid: {error}"))?;
        let scheme = match url.scheme() {
            "https" | "wss" => "wss",
            "http" | "ws" => "ws",
            scheme => return Err(format!("Voice API URL scheme `{scheme}` is unsupported")),
        };
        url.set_scheme(scheme)
            .map_err(|()| "Voice API URL scheme could not be set".to_owned())?;
        let prefix = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{prefix}{REALTIME_PATH}"));
        let mut pairs = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        match pairs.iter_mut().find(|(key, _)| key == "model") {
            Some(pair) => pair.1.clone_from(&settings.model),
            None => pairs.push(("model".to_owned(), settings.model.clone())),
        }
        url.set_query(None);
        url.query_pairs_mut().extend_pairs(pairs);
        Ok(Self {
            url,
            encoding: settings.encoding.clone(),
            sample_rate: settings.sample_rate,
            target_streaming_delay_ms: settings.target_streaming_delay_ms,
        })
    }

    /// Reference `RealtimeConnection.update_session`.
    #[must_use]
    pub fn session_update(&self) -> String {
        session_update(
            &self.encoding,
            self.sample_rate,
            self.target_streaming_delay_ms,
        )
    }
}

/// The `session.update` frame, in the SDK's field order.
#[must_use]
pub fn session_update(encoding: &str, sample_rate: u32, target_streaming_delay_ms: u32) -> String {
    json!({
        "session": {
            "audio_format": {
                "encoding": encoding,
                "sample_rate": sample_rate,
            },
            "target_streaming_delay_ms": target_streaming_delay_ms,
        },
        "type": "session.update",
    })
    .to_string()
}

/// [`TranscribeClient`] over the realtime websocket.
pub struct RealtimeTranscribeClient {
    endpoint: RealtimeEndpoint,
    credential: SecretString,
    metadata: MetadataGetter,
}

/// Reference `_create_real_voice_manager`: the client the active transcription
/// model and its provider resolve to, with the key the provider names read
/// once, or `None` for a configuration that resolves no model.
#[must_use]
pub fn client_from_view(
    view: &Value,
    credentials: &CredentialLookup,
    metadata: MetadataGetter,
) -> Option<Arc<dyn TranscribeClient>> {
    let settings = TranscriptionSettings::from_config_view(view).ok()?;
    let credential = resolve_credential(&settings.api_key_env_var, credentials);
    let client = RealtimeTranscribeClient::new(&settings, credential, metadata).ok()?;
    Some(Arc::new(client))
}

impl RealtimeTranscribeClient {
    /// Reference `MistralTranscribeClient.__init__`.
    ///
    /// # Errors
    ///
    /// An endpoint [`RealtimeEndpoint::resolve`] refuses.
    pub fn new(
        settings: &TranscriptionSettings,
        credential: String,
        metadata: MetadataGetter,
    ) -> Result<Self, String> {
        Ok(Self {
            endpoint: RealtimeEndpoint::resolve(settings)?,
            credential: SecretString::from(credential),
            metadata,
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &RealtimeEndpoint {
        &self.endpoint
    }

    /// The headers the opening request carries: the credential, the client's
    /// identity and the request metadata.
    ///
    /// # Errors
    ///
    /// A credential or metadata value no header can carry.
    pub fn request(
        &self,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
        let mut request = self
            .endpoint
            .url
            .as_str()
            .into_client_request()
            .map_err(|error| format!("Failed to connect: {error}"))?;
        let headers = request.headers_mut();
        let authorization =
            HeaderValue::from_str(&format!("Bearer {}", self.credential.expose_secret())).map_err(
                |_| "Failed to connect: the credential is not a header value".to_owned(),
            )?;
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&user_agent())
                .map_err(|_| "Failed to connect: invalid user agent".to_owned())?,
        );
        headers.insert(
            "x-metadata",
            HeaderValue::from_str(&metadata_header(&(self.metadata)()))
                .map_err(|_| "Failed to connect: invalid request metadata".to_owned())?,
        );
        Ok(request)
    }
}

type RealtimeSocket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl TranscribeClient for RealtimeTranscribeClient {
    fn transcribe(
        &self,
        audio: AudioStream,
        events: mpsc::UnboundedSender<TranscribeEvent>,
    ) -> TranscribeFuture {
        let request = self.request();
        let update = self.endpoint.session_update();
        Box::pin(async move {
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    let _ = events.send(TranscribeEvent::Error(error));
                    return;
                }
            };
            let socket = match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request)).await {
                Ok(Ok((socket, _))) => socket,
                Ok(Err(error)) => {
                    let _ = events.send(TranscribeEvent::Error(format!(
                        "Failed to connect: {error}"
                    )));
                    return;
                }
                Err(_) => {
                    let _ = events.send(TranscribeEvent::Error(
                        "Failed to connect: timed out during opening handshake".to_owned(),
                    ));
                    return;
                }
            };
            run_session(socket, &update, audio, &events).await;
        })
    }
}

/// One realtime session over an open socket.
///
/// Reference `RealtimeTranscription.transcribe_stream`: the frames read while
/// waiting for the session are replayed in order once the session update is
/// sent, then the live frames follow, and the session ends at its first
/// `transcription.done` or `error`. A connection the endpoint closes ends the
/// iteration with nothing more to report, as the `websockets` iterator ends on
/// a normal close.
async fn run_session(
    mut socket: RealtimeSocket,
    update: &str,
    mut audio: AudioStream,
    events: &mpsc::UnboundedSender<TranscribeEvent>,
) {
    let initial = match wait_for_session(&mut socket).await {
        Ok(initial) => initial,
        Err(error) => {
            let _ = events.send(TranscribeEvent::Error(error));
            let _ = socket.close(None).await;
            return;
        }
    };
    if let Err(error) = socket.send(Message::Text(update.to_owned().into())).await {
        let _ = events.send(TranscribeEvent::Error(format!(
            "Failed to connect: {error}"
        )));
        return;
    }
    let (mut sink, mut source) = socket.split();
    for frame in &initial {
        if relay(frame, events) {
            let _ = sink.close().await;
            return;
        }
    }
    let mut stream_ended = false;
    let mut sink_closed = false;
    loop {
        tokio::select! {
            chunk = audio.recv(), if !stream_ended && !sink_closed => match chunk {
                Some(chunk) => {
                    let frame = json!({
                        "audio": base64::engine::general_purpose::STANDARD.encode(chunk),
                        "type": "input_audio.append",
                    });
                    if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                        sink_closed = true;
                    }
                }
                None => {
                    for frame in [json!({"type": "input_audio.flush"}), json!({"type": "input_audio.end"})] {
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            sink_closed = true;
                            break;
                        }
                    }
                    stream_ended = true;
                }
            },
            message = source.next() => {
                let message = match message {
                    None | Some(Ok(Message::Close(_))) => return,
                    Some(Err(error)) => {
                        let _ = events.send(TranscribeEvent::Error(error.to_string()));
                        return;
                    }
                    Some(Ok(message)) => message,
                };
                if let Message::Ping(payload) = &message {
                    let _ = sink.send(Message::Pong(payload.clone())).await;
                    continue;
                }
                let Some(value) = message_json(&message) else {
                    continue;
                };
                if relay(&value, events) {
                    let _ = sink.close().await;
                    return;
                }
            }
        }
    }
}

/// Reference `MistralTranscribeClient.transcribe`'s mapping of one parsed
/// event, answering whether it ends the session. A frame the SDK's models
/// refuse is the SDK's `UnknownRealtimeEvent`, which the client skips.
fn relay(frame: &Value, events: &mpsc::UnboundedSender<TranscribeEvent>) -> bool {
    match frame.get("type").and_then(Value::as_str) {
        Some("session.created") if valid_session(frame) => {
            let _ = events.send(TranscribeEvent::SessionCreated {
                request_id: session_request_id(frame),
            });
            false
        }
        Some("transcription.text.delta") => match frame.get("text").and_then(Value::as_str) {
            Some(text) => {
                let _ = events.send(TranscribeEvent::TextDelta(text.to_owned()));
                false
            }
            None => false,
        },
        Some("transcription.done") if valid_done(frame) => {
            let _ = events.send(TranscribeEvent::Done);
            true
        }
        Some("error") => match valid_error_message(frame) {
            Some(message) => {
                let message = python_str(message);
                let event = if message.contains(EMPTY_RECORDING_MARKER) {
                    TranscribeEvent::Done
                } else {
                    TranscribeEvent::Error(message)
                };
                let _ = events.send(event);
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Reference `_recv_handshake`: frames are read until the session is created
/// or the endpoint reports an error, and every frame read on the way is kept
/// for the session to replay. An error during the handshake is reported by
/// `_extract_error_message`, whatever shape the rest of the frame has.
async fn wait_for_session(socket: &mut RealtimeSocket) -> Result<Vec<Value>, String> {
    let mut initial = Vec::new();
    loop {
        let message = match socket.next().await {
            Some(Ok(message)) => message,
            Some(Err(error)) => {
                return Err(format!("Unexpected websocket handshake failure: {error}"));
            }
            None => {
                return Err(
                    "Unexpected websocket handshake failure: the connection closed".to_owned(),
                );
            }
        };
        let Some(value) = message_json(&message) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("error") => return Err(handshake_error_message(&value)),
            Some("session.created") if valid_session(&value) => {
                initial.push(value);
                return Ok(initial);
            }
            _ => initial.push(value),
        }
    }
}

#[must_use]
pub fn session_request_id(value: &Value) -> String {
    value
        .pointer("/session/request_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A text or binary frame read as JSON; anything else, and a frame that is
/// not JSON, is skipped as the SDK skips an unknown event.
#[must_use]
pub fn message_json(message: &Message) -> Option<Value> {
    match message {
        Message::Text(text) => serde_json::from_str(text.as_ref()).ok(),
        Message::Binary(bytes) => serde_json::from_slice(bytes).ok(),
        _ => None,
    }
}

/// Reference `_extract_error_message`: the message when it is text, the
/// `detail` it carries when it is an object, and a fixed reading otherwise.
fn handshake_error_message(value: &Value) -> String {
    match value.pointer("/error/message") {
        Some(Value::String(message)) => message.clone(),
        Some(Value::Object(message)) => match message.get("detail") {
            Some(Value::String(detail)) => detail.clone(),
            _ => GENERIC_ERROR.to_owned(),
        },
        _ => GENERIC_ERROR.to_owned(),
    }
}

/// The SDK's `RealtimeTranscriptionError` model: a message that is text or an
/// object, and an integer code.
fn valid_error_message(frame: &Value) -> Option<&Value> {
    let error = frame.get("error")?;
    let message = error.get("message")?;
    (matches!(message, Value::String(_) | Value::Object(_)) && is_integer(error.get("code")))
        .then_some(message)
}

/// The SDK's `RealtimeTranscriptionSessionCreated` model.
fn valid_session(frame: &Value) -> bool {
    let Some(session) = frame.get("session") else {
        return false;
    };
    session.get("request_id").is_some_and(Value::is_string)
        && session.get("model").is_some_and(Value::is_string)
        && session
            .pointer("/audio_format/encoding")
            .is_some_and(Value::is_string)
        && is_integer(session.pointer("/audio_format/sample_rate"))
        && session
            .get("target_streaming_delay_ms")
            .is_none_or(|delay| delay.is_null() || is_integer(Some(delay)))
}

/// The SDK's `TranscriptionStreamDone` model: `language` is required and
/// nullable, and `usage` takes any object since every field it names is
/// optional.
fn valid_done(frame: &Value) -> bool {
    frame.get("model").is_some_and(Value::is_string)
        && frame.get("text").is_some_and(Value::is_string)
        && frame.get("usage").is_some_and(Value::is_object)
        && frame
            .get("language")
            .is_some_and(|language| language.is_null() || language.is_string())
}

fn is_integer(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.is_i64() || value.is_u64())
}

/// Python's `str()` of the message the error event carries: text as it is,
/// an object as Python prints a `dict`.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

/// Python's `repr()` of a parsed JSON value.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => python_string_repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(key, item)| format!("{}: {}", python_string_repr(key), python_repr(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python's `repr()` of a string: single quotes unless the text holds one
/// and no double quote, and the control characters escaped.
fn python_string_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut repr = String::with_capacity(text.len() + 2);
    repr.push(quote);
    for character in text.chars() {
        match character {
            '\\' => repr.push_str("\\\\"),
            '\n' => repr.push_str("\\n"),
            '\r' => repr.push_str("\\r"),
            '\t' => repr.push_str("\\t"),
            character if character == quote => {
                repr.push('\\');
                repr.push(character);
            }
            character if u32::from(character) < 0x20 || character == '\u{7f}' => {
                repr.push_str(&format!("\\x{:02x}", u32::from(character)));
            }
            character => repr.push(character),
        }
    }
    repr.push(quote);
    repr
}

#[cfg(test)]
#[path = "transcribe_tests.rs"]
mod transcribe_tests;
