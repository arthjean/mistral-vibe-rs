//! Realtime transcription websocket protocol.

use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::{WebSocketStream, connect_async};
use url::Url;

use crate::tui::chat_input::InputEvent;

use super::settings::TranscriptionSettings;

pub(super) const START_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// The path the realtime transcription endpoint is served under, appended to
/// whatever path the configured `api_base` already carries so a gateway served
/// below a prefix is addressed under it.
const REALTIME_PATH: &str = "/v1/audio/transcriptions/realtime";
const USER_AGENT_VALUE: &str = concat!("mistral-vibe-rs/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug)]
pub(super) struct VoiceConfig {
    pub(super) endpoint: Url,
    pub(super) requested_sample_rate: u32,
    pub(super) encoding: String,
    pub(super) target_streaming_delay_ms: u32,
}

impl VoiceConfig {
    /// The session the configured provider and model describe: the provider's
    /// endpoint carries the model in its query, and the model's own wire values
    /// are what the session frame is built from.
    pub(super) fn resolve(settings: &TranscriptionSettings) -> Result<Self, String> {
        let mut endpoint = Url::parse(&settings.api_base)
            .map_err(|error| format!("Voice API URL is invalid: {error}"))?;
        let websocket_scheme = match endpoint.scheme() {
            "https" | "wss" => "wss",
            "http" | "ws" => "ws",
            scheme => return Err(format!("Voice API URL scheme `{scheme}` is unsupported")),
        };
        endpoint
            .set_scheme(websocket_scheme)
            .map_err(|()| "Voice API URL scheme could not be set".to_owned())?;
        let prefix = endpoint.path().trim_end_matches('/').to_owned();
        endpoint.set_path(&format!("{prefix}{REALTIME_PATH}"));
        endpoint.set_query(None);
        endpoint
            .query_pairs_mut()
            .append_pair("model", &settings.model);
        Ok(Self {
            endpoint,
            requested_sample_rate: settings.sample_rate,
            encoding: settings.encoding.clone(),
            target_streaming_delay_ms: settings.target_streaming_delay_ms,
        })
    }

    pub(super) fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.requested_sample_rate = sample_rate;
        self
    }
}

type RealtimeSocket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) struct PreparedTranscription {
    websocket: RealtimeSocket,
}

pub(super) async fn prepare_transcription(
    credential: &SecretString,
    config: &VoiceConfig,
) -> Result<PreparedTranscription, String> {
    let mut request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|error| format!("Transcription request is invalid: {error}"))?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", credential.expose_secret()))
        .map_err(|_| "Transcription credential is invalid".to_owned())?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    request
        .headers_mut()
        .insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    let (mut websocket, _) = tokio::time::timeout(START_TIMEOUT, connect_async(request))
        .await
        .map_err(|_| "Transcription connection timed out".to_owned())?
        .map_err(|error| format!("Transcription connection failed: {error}"))?;
    wait_for_session(&mut websocket).await?;
    websocket
        .send(Message::Text(
            session_update(
                &config.encoding,
                config.requested_sample_rate,
                config.target_streaming_delay_ms,
            )
            .into(),
        ))
        .await
        .map_err(|error| format!("Transcription session setup failed: {error}"))?;
    Ok(PreparedTranscription { websocket })
}

impl PreparedTranscription {
    pub(super) async fn run(
        self,
        mut audio: mpsc::Receiver<Vec<u8>>,
        mut failures: watch::Receiver<Option<String>>,
        ready: oneshot::Sender<()>,
        generation: u64,
        updates: &mpsc::Sender<InputEvent>,
    ) -> Result<(), String> {
        if let Some(error) = failures.borrow().clone() {
            return Err(error);
        }
        let (mut sink, mut source) = self.websocket.split();
        let mut ended = false;
        let mut failures_open = true;
        let mut drain = Box::pin(tokio::time::sleep(DRAIN_TIMEOUT));
        ready
            .send(())
            .map_err(|()| "Transcription startup was cancelled".to_owned())?;

        loop {
            tokio::select! {
                frame = audio.recv(), if !ended => match frame {
                Some(chunk) => {
                    let payload = json!({
                        "type": "input_audio.append",
                        "audio": base64::engine::general_purpose::STANDARD.encode(chunk),
                    });
                    sink.send(Message::Text(payload.to_string().into()))
                        .await
                        .map_err(|error| format!("Transcription audio send failed: {error}"))?;
                }
                None => {
                    sink.send(Message::Text(json!({"type": "input_audio.flush"}).to_string().into()))
                        .await
                        .map_err(|error| format!("Transcription flush failed: {error}"))?;
                    sink.send(Message::Text(json!({"type": "input_audio.end"}).to_string().into()))
                        .await
                        .map_err(|error| format!("Transcription end failed: {error}"))?;
                    ended = true;
                    drain.as_mut().reset(tokio::time::Instant::now() + DRAIN_TIMEOUT);
                }
                },
                changed = failures.changed(), if failures_open => match changed {
                    Ok(()) => {
                        if let Some(error) = failures.borrow().clone() {
                            return Err(error);
                        }
                    }
                    Err(_) => failures_open = false,
                },
                message = source.next() => {
                    let Some(message) = message else {
                        return Err("Transcription connection closed before completion".to_owned());
                    };
                    let message = message.map_err(|error| format!("Transcription stream failed: {error}"))?;
                    match realtime_event(&message)? {
                        RealtimeEvent::TextDelta(text) => {
                            let _ = updates.send(InputEvent::VoiceTranscriptDelta { text, generation }).await;
                        }
                        RealtimeEvent::Done => return Ok(()),
                        RealtimeEvent::Error(error) => return Err(error),
                        RealtimeEvent::Ping(payload) => {
                            sink.send(Message::Pong(payload))
                                .await
                                .map_err(|error| format!("Transcription keepalive failed: {error}"))?;
                        }
                        RealtimeEvent::Other => {}
                    }
                }
                _ = &mut drain, if ended => return Err("Transcription timed out".to_owned()),
            }
        }
    }
}

async fn wait_for_session(websocket: &mut RealtimeSocket) -> Result<(), String> {
    tokio::time::timeout(START_TIMEOUT, async {
        loop {
            let message = websocket
                .next()
                .await
                .ok_or_else(|| "Transcription connection closed during setup".to_owned())?
                .map_err(|error| format!("Transcription setup failed: {error}"))?;
            let value = message_json(&message)?;
            match value.get("type").and_then(Value::as_str) {
                Some("session.created") => return Ok(()),
                Some("error") => return Err(realtime_error(&value)),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "Transcription session setup timed out".to_owned())?
}

pub(super) fn session_update(
    encoding: &str,
    sample_rate: u32,
    target_streaming_delay_ms: u32,
) -> String {
    json!({
        "type": "session.update",
        "session": {
            "audio_format": {
                "encoding": encoding,
                "sample_rate": sample_rate,
            },
            "target_streaming_delay_ms": target_streaming_delay_ms,
        },
    })
    .to_string()
}

enum RealtimeEvent {
    TextDelta(String),
    Done,
    Error(String),
    Ping(tokio_tungstenite::tungstenite::Bytes),
    Other,
}

fn realtime_event(message: &Message) -> Result<RealtimeEvent, String> {
    if let Message::Ping(payload) = message {
        return Ok(RealtimeEvent::Ping(payload.clone()));
    }
    let value = message_json(message)?;
    Ok(match value.get("type").and_then(Value::as_str) {
        Some("transcription.text.delta") => RealtimeEvent::TextDelta(
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        Some("transcription.done") => RealtimeEvent::Done,
        Some("error") => RealtimeEvent::Error(realtime_error(&value)),
        _ => RealtimeEvent::Other,
    })
}

pub(super) fn message_json(message: &Message) -> Result<Value, String> {
    match message {
        Message::Text(text) => serde_json::from_str(text.as_ref()),
        Message::Binary(bytes) => serde_json::from_slice(bytes),
        Message::Close(_) => return Err("Transcription connection closed".to_owned()),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(Value::Null),
    }
    .map_err(|error| format!("Transcription event is invalid: {error}"))
}

fn realtime_error(value: &Value) -> String {
    value
        .pointer("/error/message/detail")
        .or_else(|| value.pointer("/error/message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Realtime transcription failed")
        .to_owned()
}
