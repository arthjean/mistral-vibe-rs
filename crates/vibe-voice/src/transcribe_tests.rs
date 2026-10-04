//! The realtime client against a scripted endpoint.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::identity::no_metadata;
use crate::settings::{CredentialLookup, credential_missing, resolve_credential};
use crate::test_endpoint::{EndpointStep, ScriptedEndpoint};

fn transcription_settings(api_base: &str) -> TranscriptionSettings {
    TranscriptionSettings {
        model: "fixture-transcribe-model".to_owned(),
        sample_rate: 16_000,
        encoding: "pcm_s16le".to_owned(),
        target_streaming_delay_ms: 500,
        api_base: api_base.to_owned(),
        api_key_env_var: String::new(),
    }
}

fn client(endpoint: &ScriptedEndpoint) -> RealtimeTranscribeClient {
    RealtimeTranscribeClient::new(
        &transcription_settings(&endpoint.http_base()),
        "test-key".to_owned(),
        no_metadata(),
    )
    .expect("a client")
}

/// Runs one transcription over `chunks` and answers every event it produced.
async fn transcribe(client: &RealtimeTranscribeClient, chunks: &[&[u8]]) -> Vec<TranscribeEvent> {
    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();
    for chunk in chunks {
        audio_tx.send(chunk.to_vec()).expect("a queued chunk");
    }
    drop(audio_tx);
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.transcribe(audio_rx, events_tx),
    )
    .await
    .expect("the transcription settles");
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    events
}

fn created() -> EndpointStep {
    EndpointStep::Send(json!({"type": "session.created", "session": {
        "request_id": "req-1",
        "model": "fixture-transcribe-model",
        "audio_format": {"encoding": "pcm_s16le", "sample_rate": 16_000},
    }}))
}

fn done() -> EndpointStep {
    EndpointStep::Send(json!({
        "type": "transcription.done",
        "model": "fixture-transcribe-model",
        "text": "hello",
        "usage": {},
        "language": null,
    }))
}

#[tokio::test]
async fn audio_is_streamed_and_the_events_are_mapped() {
    let endpoint = ScriptedEndpoint::start(vec![
        created(),
        EndpointStep::ReadUntil("input_audio.end"),
        EndpointStep::Send(json!({"type": "transcription.text.delta", "text": "hello"})),
        done(),
        EndpointStep::AwaitClose,
    ])
    .await;
    let events = transcribe(&client(&endpoint), &[&[1, 2, 3]]).await;
    assert_eq!(
        events,
        [
            TranscribeEvent::SessionCreated {
                request_id: "req-1".to_owned()
            },
            TranscribeEvent::TextDelta("hello".to_owned()),
            TranscribeEvent::Done,
        ]
    );
    let record = endpoint.finish().await;
    assert_eq!(
        record.path,
        "/v1/audio/transcriptions/realtime?model=fixture-transcribe-model"
    );
    assert_eq!(record.header("authorization"), Some("Bearer test-key"));
    assert_eq!(record.header("user-agent"), Some(user_agent().as_str()));
    assert_eq!(record.header("x-metadata"), Some("{}"));
    assert_eq!(
        record.frame_types(),
        [
            "session.update",
            "input_audio.append",
            "input_audio.flush",
            "input_audio.end"
        ]
    );
    assert_eq!(
        record.frames[0]["session"]["audio_format"]["sample_rate"],
        16_000
    );
    assert_eq!(record.frames[1]["audio"], "AQID");
}

/// Reference `_EMPTY_RECORDING_MARKER`: a flush refused because nothing was
/// captured ends the transcription rather than failing it.
#[tokio::test]
async fn an_empty_recording_refusal_is_the_end() {
    let endpoint = ScriptedEndpoint::start(vec![
        created(),
        EndpointStep::ReadUntil("input_audio.end"),
        EndpointStep::Send(json!({
            "type": "error",
            "error": {"message": "flush requested before sending any audio bytes", "code": 4000},
        })),
        EndpointStep::AwaitClose,
    ])
    .await;
    let events = transcribe(&client(&endpoint), &[]).await;
    assert_eq!(events.last(), Some(&TranscribeEvent::Done));
    endpoint.finish().await;
}

/// Reference `str(event.error.message)`: an object message is reported as
/// Python prints the `dict`, where the handshake would have read its `detail`.
#[tokio::test]
async fn an_error_frame_ends_the_transcription_with_its_message() {
    let endpoint = ScriptedEndpoint::start(vec![
        created(),
        EndpointStep::Send(json!({"type": "error", "error": {"message": {"detail": "quota exceeded"}, "code": 4000}})),
        EndpointStep::AwaitClose,
    ])
    .await;
    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    client(&endpoint).transcribe(audio_rx, events_tx).await;
    drop(audio_tx);
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    assert_eq!(
        events.last(),
        Some(&TranscribeEvent::Error(
            "{'detail': 'quota exceeded'}".to_owned()
        ))
    );
    endpoint.finish().await;
}

/// Reference: a normal close ends the `websockets` iteration, so the SDK's
/// event loop ends with nothing more to report, before or after the
/// recording finished.
#[tokio::test]
async fn a_closed_connection_ends_without_an_event() {
    let endpoint = ScriptedEndpoint::start(vec![
        created(),
        EndpointStep::ReadUntil("input_audio.end"),
        EndpointStep::Close,
    ])
    .await;
    let events = transcribe(&client(&endpoint), &[&[1, 2]]).await;
    assert_eq!(
        events,
        [TranscribeEvent::SessionCreated {
            request_id: "req-1".to_owned()
        }]
    );
    endpoint.finish().await;
}

#[tokio::test]
async fn a_refused_handshake_is_an_error() {
    let endpoint = ScriptedEndpoint::start(vec![
        EndpointStep::Send(json!({"type": "error", "error": {"message": "invalid model"}})),
        EndpointStep::AwaitClose,
    ])
    .await;
    let events = transcribe(&client(&endpoint), &[]).await;
    assert_eq!(events, [TranscribeEvent::Error("invalid model".to_owned())]);
    endpoint.finish().await;
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_to_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().expect("an address");
    drop(listener);
    let client = RealtimeTranscribeClient::new(
        &transcription_settings(&format!("http://{address}")),
        String::new(),
        no_metadata(),
    )
    .expect("a client");
    let events = transcribe(&client, &[]).await;
    assert!(
        matches!(events.as_slice(), [TranscribeEvent::Error(message)] if message.starts_with("Failed to connect")),
        "{events:?}"
    );
}

#[test]
fn the_endpoint_and_the_session_frame_come_from_the_configured_entry() {
    let settings = TranscriptionSettings::from_config_view(&json!({"transcription": {
        "model": {
            "name": "fixture-gateway-model",
            "sampleRate": 8_000,
            "encoding": "pcm_s16le",
            "language": "en",
            "targetStreamingDelayMs": 750,
        },
        "provider": {"apiBase": "wss://gateway.fixture.invalid:8443", "apiKeyEnvVar": ""},
    }}))
    .expect("the configured surface resolves");
    let endpoint = RealtimeEndpoint::resolve(&settings).expect("an endpoint");
    assert_eq!(
        endpoint.url.as_str(),
        "wss://gateway.fixture.invalid:8443/v1/audio/transcriptions/realtime\
         ?model=fixture-gateway-model"
    );
    let update: Value = serde_json::from_str(&endpoint.session_update()).expect("JSON");
    assert_eq!(update["type"], "session.update");
    assert_eq!(update["session"]["audio_format"]["encoding"], "pcm_s16le");
    assert_eq!(update["session"]["audio_format"]["sample_rate"], 8_000);
    assert_eq!(update["session"]["target_streaming_delay_ms"], 750);
}

/// A gateway served below a path prefix keeps it, and a query it carries is
/// merged with the model rather than replaced.
#[test]
fn a_provider_path_prefix_and_query_are_kept() {
    let mut settings = transcription_settings("https://gateway.fixture.invalid/audio/?tenant=a");
    settings.model = "fixture-suffixed-model".to_owned();
    let endpoint = RealtimeEndpoint::resolve(&settings).expect("an endpoint");
    assert_eq!(
        endpoint.url.as_str(),
        "wss://gateway.fixture.invalid/audio/v1/audio/transcriptions/realtime\
         ?tenant=a&model=fixture-suffixed-model"
    );
}

#[test]
fn an_endpoint_that_is_not_a_url_is_reported_rather_than_opened() {
    let error = RealtimeEndpoint::resolve(&transcription_settings("not a url"))
        .expect_err("an unusable endpoint is reported");
    assert!(error.contains("invalid"), "{error}");
    let error = RealtimeEndpoint::resolve(&transcription_settings("ftp://gateway.fixture.invalid"))
        .expect_err("an unsupported scheme is reported");
    assert!(error.contains("ftp"), "{error}");
}

/// Reference `resolve_api_key(env_var) or ""`: an unnamed variable leaves the
/// key empty, a named one is read, and only a named one that resolves to
/// nothing refuses a recording.
#[test]
fn the_credential_is_read_under_the_variable_the_provider_names() {
    let lookup: CredentialLookup = Arc::new(|name: &str| {
        (name == "FIXTURE_GATEWAY_TOKEN").then(|| "provider-credential".to_owned())
    });
    assert_eq!(resolve_credential("", &lookup), "");
    assert!(!credential_missing("", &lookup));
    assert_eq!(
        resolve_credential("FIXTURE_GATEWAY_TOKEN", &lookup),
        "provider-credential"
    );
    assert_eq!(resolve_credential("FIXTURE_UNSET", &lookup), "");
    assert!(credential_missing("FIXTURE_UNSET", &lookup));
}
