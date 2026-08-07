//! Prompt turns, streaming updates, and approval callbacks.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::{EchoTurnDriver, PublicContentBlock};

use super::{
    ApprovalInvokingDriver, BlockingPermissionClient, PermissionClient, RecordingTurnDriver,
    UserInputOnceDriver, prompt, start_session, text_prompt,
};
use crate::agent::AcpAgent;
use crate::agent::turn::{acp_approval_options, approval_output_from_acp};
use crate::protocol::AcpError;
use crate::updates::MAX_ACP_UPDATE_QUEUE;

#[tokio::test]
async fn lifecycle_rich_content_and_updates_stay_on_public_app_server_contracts() {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
    let initialized = agent.initialize().expect("ACP initializes");
    assert_eq!(initialized.protocol_version, 1);
    assert!(initialized.agent_capabilities.load_session);
    assert_eq!(initialized.auth_methods[0]["id"], "environment");
    assert_eq!(
        initialized.auth_methods[0]["vars"][0]["name"],
        "MISTRAL_API_KEY"
    );
    agent.authenticate("environment").expect("auth");
    let session = start_session(&agent, "/workspace");
    let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);
    let response = agent
        .prompt_content_streaming(
            &session.session_id,
            vec![
                json!({"type": "text", "text": "question"}),
                json!({"type": "image", "mimeType": "image/png", "data": "AA=="}),
                json!({"type": "resource", "uri": "file:///workspace/context.txt", "text": "ctx"}),
            ],
            sender,
        )
        .await
        .expect("ACP prompt completes");
    assert_eq!(response.stop_reason, "end_turn");
    let updates = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        updates
            .iter()
            .any(|update| { update.update["sessionUpdate"] == json!("agent_message_chunk") })
    );
    assert_eq!(
        updates.last().map(|update| &update.update["sessionUpdate"]),
        Some(&json!("usage_update"))
    );
    agent
        .close_session(&session.session_id)
        .await
        .expect("ACP session closes");
    agent.disconnect().await.expect("ACP disconnects");
}

#[tokio::test]
async fn malformed_input_and_update_backpressure_release_the_session_for_later_prompts() {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");

    let (sender, _updates) = tokio::sync::mpsc::channel(1);
    assert!(matches!(
        agent
            .prompt_content_streaming(
                &session.session_id,
                vec![json!({"type": "image", "mimeType": "image/png", "data": "===="})],
                sender,
            )
            .await,
        Err(AcpError::InvalidParams(_))
    ));
    assert!(
        prompt(&agent, &session.session_id, "after invalid")
            .await
            .is_ok()
    );

    let (sender, _updates) = tokio::sync::mpsc::channel(1);
    assert!(matches!(
        agent
            .prompt_streaming(&session.session_id, "overflow", sender)
            .await,
        Err(AcpError::Backpressure)
    ));
    assert!(
        prompt(&agent, &session.session_id, "after overflow")
            .await
            .is_ok()
    );
    agent.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn embedded_resource_context_reaches_the_turn_driver() {
    let reservations = Arc::new(Mutex::new(Vec::new()));
    let agent =
        AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");
    let resource = json!({
        "type": "resource",
        "uri": "file:///workspace/context.txt",
        "text": "embedded context",
    });
    let (sender, _updates) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);

    agent
        .prompt_content_streaming(
            &session.session_id,
            vec![
                json!({"type": "text", "text": "use the context"}),
                resource.clone(),
            ],
            sender,
        )
        .await
        .expect("prompt");

    let reservations = reservations.lock().expect("reservations");
    let reservation = reservations.last().expect("recorded reservation");
    assert_eq!(reservation.prompt, "use the context");
    assert_eq!(
        reservation.input,
        vec![
            PublicContentBlock::Text {
                text: "use the context".to_owned(),
            },
            PublicContentBlock::Resource { resource },
        ]
    );
}

#[test]
fn approval_choices_and_invalid_client_outcomes_fail_closed() {
    let options = acp_approval_options(&json!({
        "choices": ["approve", "deny", "cancel_turn"],
    }));
    assert_eq!(
        options
            .iter()
            .filter_map(|option| option.get("optionId").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        ["allow_once", "reject_once"]
    );
    assert_eq!(
        approval_output_from_acp(
            Some(&json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once",
                }
            })),
            &options,
        )["decision"]["type"],
        "approve"
    );
    for response in [
        None,
        Some(json!({"outcome": {"outcome": "cancelled"}})),
        Some(json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_always",
            }
        })),
        Some(json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "unknown",
            }
        })),
    ] {
        assert_eq!(
            approval_output_from_acp(response.as_ref(), &options)["decision"]["type"],
            "cancel_turn"
        );
    }
}

#[tokio::test]
async fn canonical_approval_callback_routes_through_the_acp_client() {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(directory.path().join("approval.txt"), "approved\n").expect("workspace file");
    let client = Arc::new(PermissionClient {
        params: Mutex::new(None),
    });
    let agent = AcpAgent::new(ApprovalInvokingDriver {
        inner: EchoTurnDriver::new("answer"),
    })
    .expect("agent starts")
    .with_client_port(client.clone(), Duration::from_secs(1));
    agent.initialize().expect("initialize");
    let session = start_session(&agent, &directory.path().to_string_lossy());
    prompt(&agent, &session.session_id, "read the file")
        .await
        .expect("approved prompt");
    let params = client
        .params
        .lock()
        .expect("permission params")
        .clone()
        .expect("permission request");
    assert_eq!(params["sessionId"], session.session_id);
    assert_eq!(
        params["options"][0]["kind"],
        Value::String("allow_once".to_owned())
    );
    assert_eq!(
        params["toolCall"]["rawInput"]["effect"]["toolName"],
        "read_file"
    );
    // US-105: the requirement crosses to the editor as the reference model, so
    // the client can render what an approval for the session would cover.
    let requirement = &params["toolCall"]["rawInput"]["requiredPermissions"][0];
    assert_eq!(requirement["scope"], "outside_directory");
    assert_eq!(
        requirement["invocationPattern"],
        requirement["sessionPattern"]
    );
    assert!(
        requirement["label"]
            .as_str()
            .is_some_and(|label| label.starts_with("outside workdir (")),
        "{requirement}"
    );
    agent.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn unsupported_user_input_is_denied_without_reaching_the_acp_client() {
    let directory = tempfile::tempdir().expect("workspace");
    let client = Arc::new(PermissionClient {
        params: Mutex::new(None),
    });
    let agent = AcpAgent::new(UserInputOnceDriver {
        inner: EchoTurnDriver::new("answer"),
        attempts: AtomicU64::new(0),
    })
    .expect("agent starts")
    .with_client_port(client.clone(), Duration::from_secs(1));
    agent.initialize().expect("initialize");
    let session = start_session(&agent, &directory.path().to_string_lossy());

    assert!(matches!(
        prompt(&agent, &session.session_id, "ask").await,
        Err(AcpError::Driver(_))
    ));
    assert!(client.params.lock().expect("permission params").is_none());
    prompt(&agent, &session.session_id, "continue")
        .await
        .expect("unsupported callback released the session");
    agent.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn disconnect_cancels_a_pending_canonical_approval() {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(directory.path().join("approval.txt"), "approval\n").expect("workspace file");
    let client = Arc::new(BlockingPermissionClient {
        started: tokio::sync::Notify::new(),
    });
    let agent = Arc::new(
        AcpAgent::new(ApprovalInvokingDriver {
            inner: EchoTurnDriver::new("answer"),
        })
        .expect("agent starts")
        .with_client_port(client.clone(), Duration::from_secs(30)),
    );
    agent.initialize().expect("initialize");
    let session = start_session(&agent, &directory.path().to_string_lossy());
    let prompt_agent = agent.clone();
    let session_id = session.session_id;
    let prompt = tokio::spawn(async move {
        prompt_agent
            .prompt_content(&session_id, text_prompt("read"), |_| Ok(()))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), client.started.notified())
        .await
        .expect("permission request started");
    tokio::time::timeout(Duration::from_secs(1), agent.disconnect())
        .await
        .expect("disconnect did not hang")
        .expect("disconnect");
    let result = tokio::time::timeout(Duration::from_secs(1), prompt)
        .await
        .expect("prompt did not stop")
        .expect("prompt task");
    assert!(result.is_err(), "disconnect must not approve pending work");
}
