//! The `telemetry/send` extension notification an editor records through.
//!
//! Reference `AcpAgent.ext_notification` (`vibe/acp/agent.py:1002-1031`): one
//! method name is served, two event names are routed, an unknown session drops
//! the event and an unknown name is ignored with a warning.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use vibe_app_server::client::EchoTurnDriver;
use vibe_core::telemetry::ClientTelemetry;

use super::start_session;
use crate::agent::AcpAgent;
use crate::protocol::{AcpError, AcpInitializeRequest};

/// One event as it reached the telemetry client.
#[derive(Debug, Clone, PartialEq)]
struct Recorded {
    name: String,
    properties: Map<String, Value>,
    session_id: Option<String>,
    correlate_last_request: bool,
}

#[derive(Default)]
struct RecordingTelemetry {
    events: Mutex<Vec<Recorded>>,
}

impl RecordingTelemetry {
    fn drain(&self) -> Vec<Recorded> {
        std::mem::take(&mut *self.events.lock().expect("recorded events"))
    }
}

impl ClientTelemetry for RecordingTelemetry {
    fn record_client_event(
        &self,
        name: &str,
        properties: Map<String, Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    ) {
        if let Ok(mut events) = self.events.lock() {
            events.push(Recorded {
                name: name.to_owned(),
                properties,
                session_id: session_id.map(ToOwned::to_owned),
                correlate_last_request,
            });
        }
    }
}

/// A home whose configuration keeps telemetry on and publishes one model, so
/// the rating's `model` has an alias to report.
fn workspace(root: &Path) -> String {
    let working_directory = root.join("workspace");
    fs::create_dir_all(working_directory.join(".vibe")).expect("project configuration directory");
    fs::create_dir_all(root.join("home")).expect("home directory");
    fs::write(
        working_directory.join(".vibe/config.toml"),
        r#"
enable_telemetry = true
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    )
    .expect("telemetry configuration");
    working_directory.to_string_lossy().into_owned()
}

fn agent(root: &Path, telemetry: Arc<RecordingTelemetry>) -> AcpAgent<EchoTurnDriver> {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
        .expect("agent starts")
        .with_session_root(root.join("home").join("sessions"))
        .with_client_telemetry(telemetry);
    agent
        .initialize_with(AcpInitializeRequest::default())
        .expect("the agent initializes");
    agent
}

#[tokio::test]
async fn the_two_routed_events_reach_the_telemetry_client() {
    let temporary = tempfile::tempdir().expect("temporary home");
    let working_directory = workspace(temporary.path());
    let telemetry = Arc::new(RecordingTelemetry::default());
    let agent = agent(temporary.path(), telemetry.clone());
    let session = start_session(&agent, &working_directory);

    agent
        .telemetry_notification(&json!({
            "event": "vibe.at_mention_inserted",
            "properties": {"mention_type": "file", "source": "editor"},
            "sessionId": session.session_id,
        }))
        .await
        .expect("the mention is accepted");
    let mention = telemetry.drain();
    assert_eq!(
        mention,
        vec![Recorded {
            name: "vibe.at_mention_inserted".to_owned(),
            properties: json!({"mention_type": "file", "source": "editor"})
                .as_object()
                .expect("an object")
                .clone(),
            session_id: Some(session.session_id.clone()),
            correlate_last_request: false,
        }],
        "the editor's own properties are recorded unchanged"
    );

    // The alias the session itself publishes is what the reference reads off
    // its configuration, so the expectation is taken from the same surface.
    let alias = published_model_alias(&agent, &session.session_id).await;
    agent
        .telemetry_notification(&json!({
            "event": "vibe.user_rating_feedback",
            "properties": {"rating": 5, "surplus": "dropped"},
            "sessionId": session.session_id,
        }))
        .await
        .expect("the rating is accepted");
    assert_eq!(
        telemetry.drain(),
        vec![Recorded {
            name: "vibe.user_rating_feedback".to_owned(),
            properties: json!({"rating": 5, "model": alias})
                .as_object()
                .expect("an object")
                .clone(),
            session_id: Some(session.session_id.clone()),
            correlate_last_request: true,
        }],
        "the rating carries the active model alias and correlates"
    );

    // Reference `properties.get("rating", 0)`: a rating the editor omitted is
    // reported as zero rather than dropped.
    agent
        .telemetry_notification(&json!({
            "event": "vibe.user_rating_feedback",
            "sessionId": session.session_id,
        }))
        .await
        .expect("the rating without a value is accepted");
    assert_eq!(
        telemetry.drain().first().expect("one event").properties,
        json!({"rating": 0, "model": alias})
            .as_object()
            .expect("an object")
            .clone()
    );
}

/// The alias `config/read` publishes for the model a turn would run on.
async fn published_model_alias(agent: &AcpAgent<EchoTurnDriver>, session_id: &str) -> String {
    let harness = agent.session_harness(session_id).expect("a live session");
    let config = harness
        .service
        .lock()
        .await
        .public_call("config/read", json!({"sessionId": session_id}))
        .expect("the session publishes its configuration");
    config["config"]["activeModel"]["alias"]
        .as_str()
        .expect("an alias")
        .to_owned()
}

#[tokio::test]
async fn an_unsupported_event_an_unknown_session_and_an_invalid_payload_ship_nothing() {
    let temporary = tempfile::tempdir().expect("temporary home");
    let working_directory = workspace(temporary.path());
    let telemetry = Arc::new(RecordingTelemetry::default());
    let agent = agent(temporary.path(), telemetry.clone());
    let session = start_session(&agent, &working_directory);

    agent
        .telemetry_notification(&json!({
            "event": "vibe.invented_by_the_editor",
            "properties": {},
            "sessionId": session.session_id,
        }))
        .await
        .expect("an unsupported event is ignored rather than refused");
    agent
        .telemetry_notification(&json!({
            "event": "vibe.at_mention_inserted",
            "sessionId": "acp-session-absent",
        }))
        .await
        .expect("an unknown session drops the event without error");
    assert!(
        telemetry.drain().is_empty(),
        "neither an unsupported event nor an unknown session ships"
    );

    for invalid in [
        json!({"properties": {}, "sessionId": session.session_id}),
        json!({"event": "vibe.at_mention_inserted"}),
        json!({"event": 7, "sessionId": session.session_id}),
    ] {
        assert!(
            matches!(
                agent.telemetry_notification(&invalid).await,
                Err(AcpError::InvalidParams(_))
            ),
            "an invalid payload is refused: {invalid}"
        );
    }
    assert!(telemetry.drain().is_empty());
}
