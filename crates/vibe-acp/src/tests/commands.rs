//! Built-in commands: dispatch, streamed output, and Teleport.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_app_server::client::EchoTurnDriver;
use vibe_app_server::release4::Release4Service;

use super::{
    CleanFixtureGit, FixtureProjectCloud, FixtureTeleportCloud, RecordingTurnDriver, prompt,
    start_session,
};
use crate::agent::AcpAgent;
use crate::commands::command_response;
use crate::updates::MAX_ACP_UPDATE_QUEUE;

#[tokio::test]
async fn built_in_help_is_advertised_and_does_not_start_a_model_turn() {
    let reservations = Arc::new(Mutex::new(Vec::new()));
    let agent =
        AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");
    let (sender, mut updates) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);

    let response = agent
        .prompt_streaming(&session.session_id, "/help", sender)
        .await
        .expect("help command");

    assert_eq!(response, command_response());
    assert!(reservations.lock().expect("reservations").is_empty());
    let updates = std::iter::from_fn(|| updates.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].update["sessionUpdate"], "user_message_chunk");
    assert_eq!(updates[1].update["sessionUpdate"], "agent_message_chunk");
    let help = updates[1].update["content"]["text"]
        .as_str()
        .expect("help text");
    assert!(help.contains("/compact"));
    assert!(help.contains("/reload"));
    assert!(!help.contains("/teleport"), "{help}");
    assert!(agent.advertised_commands().iter().all(|command| {
        let name = command["name"].as_str().expect("command name");
        help.contains(&format!("/{name}"))
    }));
}

#[tokio::test]
async fn unknown_slash_input_reaches_the_model_instead_of_the_command_dispatcher() {
    let reservations = Arc::new(Mutex::new(Vec::new()));
    let agent =
        AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");

    prompt(&agent, &session.session_id, "/not-a-command")
        .await
        .expect("unknown command is an ordinary prompt");

    assert_eq!(reservations.lock().expect("reservations").len(), 1);
}

#[tokio::test]
async fn teleport_command_streams_public_cloud_events_and_completion_url() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let cwd = temporary.path().to_string_lossy().into_owned();
    let session_root = temporary.path().join("sessions");
    let release4 = Release4Service::with_backends(
        Arc::new(FixtureProjectCloud),
        Arc::new(FixtureTeleportCloud),
        Arc::new(CleanFixtureGit),
    );
    let open = release4
        .dispatch(
            "vibeCode/projects/open",
            &serde_json::from_value(json!({
                "sessionId": "link-seed",
                "workingDirectory": cwd,
                "purpose": "configure",
            }))
            .expect("project open params"),
        )
        .expect("project picker");
    let picker_id = open.result["pickerId"]
        .as_str()
        .expect("picker ID")
        .to_owned();
    release4
        .dispatch(
            "vibeCode/projects/select",
            &serde_json::from_value(json!({
                "sessionId": "link-seed",
                "pickerId": picker_id,
                "projectId": "project-1",
            }))
            .expect("project selection params"),
        )
        .expect("project link");

    let agent =
        AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
            .expect("agent starts")
            .with_session_root(session_root)
            .with_release4_service(release4);
    agent.initialize().expect("initialize");
    let session = start_session(&agent, &cwd);
    prompt(&agent, &session.session_id, "context to continue")
        .await
        .expect("history prompt");

    let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);
    let response = agent
        .prompt_streaming(&session.session_id, "/teleport", sender)
        .await
        .expect("Teleport command");
    assert_eq!(response, command_response());
    let updates = std::iter::from_fn(|| receiver.try_recv().ok())
        .map(|update| update.update)
        .collect::<Vec<_>>();
    let statuses = updates
        .iter()
        .filter_map(|update| update.pointer("/_meta/teleport/status"))
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(statuses.contains(&"summarizing_context"), "{statuses:?}");
    assert!(statuses.contains(&"preparing_workspace"), "{statuses:?}");
    assert!(statuses.contains(&"starting_workflow"), "{statuses:?}");
    assert!(statuses.contains(&"completed"), "{statuses:?}");
    assert!(updates.iter().any(|update| {
        update["status"] == "completed"
            && update
                .pointer("/rawOutput/url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.starts_with("https://cloud.example.test/teleport/"))
    }));
    agent.disconnect().await.expect("disconnect");
}
