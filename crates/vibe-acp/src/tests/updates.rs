//! Update projection, history replay, and untrusted input validation.

use serde_json::json;
use vibe_app_server::client::PublicHistoryEntry;

use crate::mcp::project_acp_mcp_servers;
use crate::protocol::AcpError;
use crate::updates::{
    MAX_PROMPT_BLOCKS, history_entry_updates, public_entry_updates, queue_update, turn_request,
};

#[test]
fn bounded_update_queue_fails_closed_on_saturation_and_disconnect() {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let update = || json!({"sessionUpdate": "agent_message_chunk"});
    queue_update(&sender, "session", update()).expect("first update fits");
    assert!(matches!(
        queue_update(&sender, "session", update()),
        Err(AcpError::Backpressure)
    ));
    drop(receiver);
    assert!(matches!(
        queue_update(&sender, "session", update()),
        Err(AcpError::Disconnected)
    ));
}

#[test]
fn cumulative_public_entries_emit_only_new_text_and_keep_tool_identity() {
    let message = |text: &str, status: &str| {
        serde_json::from_value::<PublicHistoryEntry>(json!({
            "type": "message",
            "id": "message-1",
            "sessionId": "session-1",
            "turnId": "turn-1",
            "createdAt": 1,
            "updatedAt": 2,
            "generationStatus": status,
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
        }))
        .expect("public message")
    };
    let first = message("Hel", "in_progress");
    let second = message("Hello", "in_progress");
    let completed = message("Hello", "completed");
    let first_updates = public_entry_updates(None, &first).expect("initial update");
    let second_updates = public_entry_updates(Some(&first), &second).expect("delta update");
    let completion_updates =
        public_entry_updates(Some(&second), &completed).expect("completion update");
    assert_eq!(first_updates[0]["content"]["text"], "Hel");
    assert_eq!(second_updates[0]["content"]["text"], "lo");
    assert_eq!(second_updates[0]["messageId"], "message-1");
    assert!(completion_updates.is_empty());

    let reasoning = |text: &str| {
        serde_json::from_value::<PublicHistoryEntry>(json!({
            "type": "reasoning",
            "id": "reasoning-1",
            "sessionId": "session-1",
            "turnId": "turn-1",
            "createdAt": 1,
            "updatedAt": 2,
            "generationStatus": "in_progress",
            "text": text,
        }))
        .expect("public reasoning")
    };
    let first_reasoning = reasoning("think");
    let second_reasoning = reasoning("thinking");
    let reasoning_delta =
        public_entry_updates(Some(&first_reasoning), &second_reasoning).expect("reasoning delta");
    assert_eq!(reasoning_delta[0]["content"]["text"], "ing");
    assert_eq!(reasoning_delta[0]["messageId"], "reasoning-1");

    let effect = |status: &str, generation_status: &str, output_text: &str| {
        serde_json::from_value::<PublicHistoryEntry>(json!({
            "type": "effect",
            "id": "tool-1",
            "sessionId": "session-1",
            "turnId": "turn-1",
            "createdAt": 1,
            "updatedAt": 2,
            "generationStatus": generation_status,
            "title": "Shell",
            "detail": {"command": "cargo check"},
            "state": {
                "status": status,
                "outputText": output_text,
                "output": {"ok": true},
                "durationMs": 1,
                "display": {},
            },
        }))
        .expect("public effect")
    };
    let running = effect("running", "in_progress", "a");
    let completed_effect = effect("completed", "completed", "ab");
    let started = public_entry_updates(None, &running).expect("tool starts");
    let progressed =
        public_entry_updates(Some(&running), &completed_effect).expect("tool progresses");
    assert_eq!(started[0]["sessionUpdate"], "tool_call");
    assert_eq!(progressed[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(progressed[0]["toolCallId"], "tool-1");
    assert_eq!(progressed[0]["content"][0]["content"]["text"], "b");
    assert_eq!(progressed[0]["status"], "completed");
}

#[test]
fn structured_history_replay_preserves_roles_content_and_effects() {
    assert!(
        history_entry_updates(
            &json!({"role": "system", "content": "private system prompt"}),
            0,
        )
        .expect("system replay")
        .is_empty()
    );

    let assistant = history_entry_updates(
        &json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "answer"},
                {"type": "image", "data": "AA==", "mimeType": "image/png"},
                {
                    "type": "resource_link",
                    "name": "docs",
                    "uri": "file:///workspace/docs.md"
                }
            ],
            "reasoning": "reason",
            "tool_calls": [{
                "id": "call-1",
                "name": "read_file",
                "arguments": "{\"path\":\"src/lib.rs\"}"
            }]
        }),
        1,
    )
    .expect("assistant replay");
    assert_eq!(
        assistant
            .iter()
            .map(|update| update["sessionUpdate"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        [
            "agent_thought_chunk",
            "agent_message_chunk",
            "agent_message_chunk",
            "agent_message_chunk",
            "tool_call",
        ]
    );
    assert_eq!(assistant[2]["content"]["type"], "image");
    assert_eq!(assistant[3]["content"]["type"], "resource_link");
    assert_eq!(assistant[4]["toolCallId"], "call-1");

    let tool = history_entry_updates(
        &json!({
            "role": "tool",
            "call_id": "call-1",
            "content": "file body",
            "is_error": false,
        }),
        2,
    )
    .expect("tool replay");
    assert_eq!(tool[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(tool[0]["toolCallId"], "call-1");
    assert_eq!(tool[0]["status"], "completed");
    assert!(
        assistant
            .iter()
            .chain(&tool)
            .all(|update| update["content"]["text"] != "private system prompt")
    );
}

#[test]
fn prompt_and_mcp_projection_reject_oversized_or_malformed_untrusted_inputs() {
    assert!(matches!(
        turn_request(vec![json!({
            "type": "image",
            "mimeType": "image/png",
            "data": "====",
        })]),
        Err(AcpError::InvalidParams(_))
    ));
    assert!(matches!(
        turn_request(vec![json!({"type": "resource", "text": "missing URI"})]),
        Err(AcpError::InvalidParams(_))
    ));
    assert!(matches!(
        turn_request(
            (0..=MAX_PROMPT_BLOCKS)
                .map(|_| json!({"type": "text", "text": "x"}))
                .collect()
        ),
        Err(AcpError::InvalidParams(_))
    ));

    let projected = project_acp_mcp_servers(&[json!({
        "type": "http",
        "name": "remote",
        "url": "https://mcp.example.test",
        "headers": [{"name": "Authorization", "value": "token"}],
    })])
    .expect("HTTP MCP projects");
    assert_eq!(projected[0]["transport"], "streamable-http");
    assert_eq!(projected[0]["headers"]["Authorization"], "token");
    assert!(matches!(
        project_acp_mcp_servers(&[json!({
            "type": "sse",
            "name": "legacy",
            "url": "https://mcp.example.test",
        })]),
        Err(AcpError::UnsupportedClientFlow(_))
    ));
}

#[test]
fn session_updates_carry_the_projected_message_identity() {
    let entry = serde_json::from_value::<PublicHistoryEntry>(json!({
        "type": "message",
        "id": "message-9",
        "sessionId": "session-1",
        "turnId": "turn-1",
        "createdAt": 1,
        "updatedAt": 2,
        "generationStatus": "in_progress",
        "role": "user",
        "content": [{"type": "text", "text": "hello"}],
    }))
    .expect("public message");
    let updates = public_entry_updates(None, &entry).expect("user message projects");
    assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
    assert_eq!(updates[0]["messageId"], "message-9");
    assert_eq!(
        updates[0]["content"],
        json!({"type": "text", "text": "hello"})
    );
}
