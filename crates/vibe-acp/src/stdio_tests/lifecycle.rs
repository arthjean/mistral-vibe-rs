//! Session lifecycle over the wire: opening, listing, forking, loading, and
//! keeping two sessions apart.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_app_server::client::{
    DriverFuture, EchoTurnDriver, LiveDriverConfig, LiveTurnDriver, TurnDriver, TurnReservation,
};
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;

use super::{
    close_session, initialize, new_session, prompt, request, spawn_stdio, spawn_stdio_with_root,
};
use crate::stdio::driver::DeferredTurnDriver;

#[tokio::test]
async fn initialize_and_session_lifecycle_do_not_require_provider_credentials() {
    const MISSING_CREDENTIAL: &str = "VIBE_ACP_TEST_CREDENTIAL_MUST_REMAIN_UNSET_9F4C";
    assert!(
        std::env::var_os(MISSING_CREDENTIAL).is_none(),
        "{MISSING_CREDENTIAL} must remain unset for this test"
    );
    let resolutions = Arc::new(AtomicUsize::new(0));
    let driver = DeferredTurnDriver::<LiveTurnDriver>::new({
        let resolutions = resolutions.clone();
        move || {
            resolutions.fetch_add(1, Ordering::SeqCst);
            LiveTurnDriver::from_environment(
                LiveDriverConfig {
                    style: "mistral".to_owned(),
                    endpoint: "http://127.0.0.1:1".to_owned(),
                    model: "test-model".to_owned(),
                    credential_environment: MISSING_CREDENTIAL.to_owned(),
                    system_prompt: "test".to_owned(),
                    session_root: None,
                    compaction_prompts: CompactionPromptResolution::default(),
                    input_price_per_million_micros: 0,
                    output_price_per_million_micros: 0,
                },
                &DotenvValues::default(),
            )
        }
    });
    let mut peer = spawn_stdio(driver);

    let initialized = initialize(&mut peer, 1).await;
    assert_eq!(
        initialized["result"]["authMethods"][0]["id"],
        "browser-auth"
    );
    let session_id = new_session(&mut peer, 2, "/workspace").await;
    let closed = close_session(&mut peer, 3, &session_id).await;
    assert_eq!(closed["result"], json!({}));
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);

    let session_id = new_session(&mut peer, 4, "/workspace").await;
    let (_, response) = prompt(&mut peer, 5, &session_id, "requires provider").await;
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(MISSING_CREDENTIAL))
    );
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    peer.shutdown(6).await;
}

#[tokio::test]
async fn initialize_precedes_provider_use_and_prompt_drains_the_final_update() {
    let resolutions = Arc::new(AtomicUsize::new(0));
    let driver = DeferredTurnDriver::new({
        let resolutions = resolutions.clone();
        move || {
            resolutions.fetch_add(1, Ordering::SeqCst);
            Ok(EchoTurnDriver::new("answer"))
        }
    });
    let mut peer = spawn_stdio(driver);

    initialize(&mut peer, 1).await;
    let session_id = new_session(&mut peer, 2, "/workspace").await;
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);

    let (preceding, response) = prompt(&mut peer, 3, &session_id, "question").await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(
        preceding
            .last()
            .and_then(|message| message.pointer("/params/update/sessionUpdate"))
            .and_then(Value::as_str),
        Some("usage_update")
    );
    peer.shutdown(4).await;
}

#[tokio::test]
async fn stdio_session_list_honors_cwd_and_cursor_with_acp_session_shapes() {
    const PAGE_SIZE: usize = 50;
    let temporary = tempfile::tempdir().expect("temporary root");
    let first_cwd = temporary.path().join("first");
    let second_cwd = temporary.path().join("second");
    std::fs::create_dir_all(&first_cwd).expect("first cwd");
    std::fs::create_dir_all(&second_cwd).expect("second cwd");
    let mut peer = spawn_stdio_with_root(
        EchoTurnDriver::new("answer"),
        Some(temporary.path().join("sessions")),
    );
    initialize(&mut peer, 1).await;

    for id in 0..=PAGE_SIZE {
        new_session(
            &mut peer,
            i64::try_from(id).unwrap_or_default().saturating_add(10),
            &first_cwd.to_string_lossy(),
        )
        .await;
    }
    new_session(&mut peer, 100, &second_cwd.to_string_lossy()).await;

    peer.send(request(
        101,
        "session/list",
        json!({"cwd": first_cwd.to_string_lossy()}),
    ))
    .await;
    let (_, first) = peer.response(101).await;
    let first_sessions = first["result"]["sessions"].as_array().expect("sessions");
    assert_eq!(first_sessions.len(), PAGE_SIZE);
    assert!(first_sessions.iter().all(|session| {
        session.get("sessionId").is_some_and(Value::is_string)
            && session["cwd"] == first_cwd.to_string_lossy().as_ref()
            && session.get("id").is_none()
            && session.get("workingDirectory").is_none()
    }));
    let cursor = first["result"]["nextCursor"].as_str().expect("next cursor");

    peer.send(request(
        102,
        "session/list",
        json!({"cwd": first_cwd.to_string_lossy(), "cursor": cursor}),
    ))
    .await;
    let (_, second) = peer.response(102).await;
    assert_eq!(
        second["result"]["sessions"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(second["result"].get("nextCursor").is_none());
    let first_ids = first_sessions
        .iter()
        .filter_map(|session| session["sessionId"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let second_id = second["result"]["sessions"][0]["sessionId"]
        .as_str()
        .expect("second-page ID");
    assert!(!first_ids.contains(second_id));

    peer.send(request(
        103,
        "session/list",
        json!({"cwd": second_cwd.to_string_lossy()}),
    ))
    .await;
    let (_, filtered) = peer.response(103).await;
    assert_eq!(
        filtered["result"]["sessions"][0]["cwd"],
        second_cwd.to_string_lossy().as_ref()
    );
    peer.shutdown(104).await;
}

#[tokio::test]
async fn stdio_load_and_fork_preserve_structured_replay_and_lifecycle_options() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let cwd = temporary.path().join("workspace");
    let additional = temporary.path().join("shared");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&additional).expect("additional root");
    let session_root = temporary.path().join("sessions");
    let mut peer = spawn_stdio_with_root(
        EchoTurnDriver::new("answer").with_session_root(session_root.clone()),
        Some(session_root),
    );
    initialize(&mut peer, 1).await;
    let source = new_session(&mut peer, 2, &cwd.to_string_lossy()).await;
    let (_, prompted) = prompt(&mut peer, 3, &source, "question").await;
    assert_eq!(prompted["result"]["stopReason"], "end_turn");

    peer.send(request(
        4,
        "session/set_mode",
        json!({"sessionId": source, "modeId": "plan"}),
    ))
    .await;
    assert!(peer.response(4).await.1.get("error").is_none());
    peer.send(request(
        5,
        "session/set_config_option",
        json!({"sessionId": source, "configId": "thinking", "value": "high"}),
    ))
    .await;
    assert!(peer.response(5).await.1.get("error").is_none());

    let mcp_server = json!({
        "type": "stdio",
        "name": "fixture",
        "command": "/bin/false",
        "args": [],
        "env": [],
    });
    peer.send(request(
        6,
        "session/fork",
        json!({
            "sessionId": source,
            "cwd": cwd.to_string_lossy(),
            "additionalDirectories": [additional.to_string_lossy()],
            "mcpServers": [mcp_server],
            "newSessionId": "stdio-fork",
        }),
    ))
    .await;
    let (_, forked) = peer.response(6).await;
    assert_eq!(forked["result"]["sessionId"], "stdio-fork");
    assert_eq!(forked["result"]["modes"]["currentModeId"], "plan");
    assert_eq!(forked["result"]["configOptions"][0]["currentValue"], "high");
    peer.expect_commands_update("stdio-fork").await;

    peer.send(request(
        7,
        "session/fork",
        json!({
            "sessionId": source,
            "cwd": cwd.to_string_lossy(),
            "messageId": "message-1",
        }),
    ))
    .await;
    let (_, unsupported_fork) = peer.response(7).await;
    assert_eq!(unsupported_fork["error"]["code"], -32602);

    assert_eq!(
        close_session(&mut peer, 8, "stdio-fork").await["result"],
        json!({})
    );
    assert_eq!(
        close_session(&mut peer, 9, &source).await["result"],
        json!({})
    );
    peer.send(request(
        10,
        "session/load",
        json!({
            "sessionId": source,
            "cwd": cwd.to_string_lossy(),
            "additionalDirectories": [additional.to_string_lossy()],
            "mcpServers": [{
                "type": "stdio",
                "name": "fixture",
                "command": "/bin/false",
                "args": [],
                "env": [],
            }],
        }),
    ))
    .await;
    let (replay, loaded) = peer.response(10).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    assert!(loaded["result"].get("sessionId").is_none());
    // The transcript is replayed before the response and the command catalog
    // after it, which is the order ACP and the reference both publish.
    assert!(replay.iter().all(|message| {
        message.pointer("/params/update/sessionUpdate") != Some(&json!("available_commands_update"))
    }));
    peer.expect_commands_update(&source).await;
    let replay_updates = replay
        .iter()
        .filter_map(|message| message.pointer("/params/update"))
        .collect::<Vec<_>>();
    assert!(
        replay_updates.iter().any(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"] == json!({"type": "text", "text": "question"})
                && update.get("messageId").is_some()
        }),
        "{replay_updates:#?}"
    );
    assert!(replay_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"] == json!({"type": "text", "text": "answer"})
            && update.get("messageId").is_some()
    }));
    assert!(replay_updates.iter().all(|update| {
        update["sessionUpdate"] != "agent_message_chunk" || update["content"]["text"].is_string()
    }));
    assert_eq!(
        close_session(&mut peer, 11, &source).await["result"],
        json!({})
    );
    peer.shutdown(12).await;
}

#[derive(Debug, PartialEq, Eq)]
struct RecordedTurn {
    session_id: String,
    prompt: String,
    working_directory: String,
}

struct RecordingDriver {
    inner: EchoTurnDriver,
    turns: Arc<Mutex<Vec<RecordedTurn>>>,
}

impl TurnDriver for RecordingDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        self.turns.lock().expect("turns").push(RecordedTurn {
            session_id: reservation.session_id.clone(),
            prompt: reservation.prompt.clone(),
            working_directory: reservation.working_directory.clone(),
        });
        self.inner.run(reservation)
    }
}

#[tokio::test]
async fn two_stdio_session_lifecycles_remain_isolated() {
    let turns = Arc::new(Mutex::new(Vec::new()));
    let driver = RecordingDriver {
        inner: EchoTurnDriver::new("answer"),
        turns: turns.clone(),
    };
    let mut peer = spawn_stdio(driver);

    initialize(&mut peer, 1).await;
    let first = new_session(&mut peer, 2, "/first").await;
    let second = new_session(&mut peer, 3, "/second").await;
    assert_ne!(first, second);

    let (first_updates, first_response) = prompt(&mut peer, 4, &first, "first").await;
    assert!(first_response.get("error").is_none());
    assert!(first_updates.iter().all(|message| {
        message.get("method").and_then(Value::as_str) != Some("session/update")
            || message.pointer("/params/sessionId").and_then(Value::as_str) == Some(first.as_str())
    }));

    let (second_updates, second_response) = prompt(&mut peer, 5, &second, "second").await;
    assert!(second_response.get("error").is_none());
    assert!(second_updates.iter().all(|message| {
        message.get("method").and_then(Value::as_str) != Some("session/update")
            || message.pointer("/params/sessionId").and_then(Value::as_str) == Some(second.as_str())
    }));

    let closed = close_session(&mut peer, 6, &first).await;
    assert_eq!(closed["result"], json!({}));
    let (_, still_active) = prompt(&mut peer, 7, &second, "still active").await;
    assert!(still_active.get("error").is_none());
    let (_, closed_error) = prompt(&mut peer, 8, &first, "closed").await;
    assert_eq!(closed_error["error"]["code"], -32001);

    assert_eq!(
        turns.lock().expect("turns").as_slice(),
        [
            RecordedTurn {
                session_id: first,
                prompt: "first".to_owned(),
                working_directory: "/first".to_owned(),
            },
            RecordedTurn {
                session_id: second.clone(),
                prompt: "second".to_owned(),
                working_directory: "/second".to_owned(),
            },
            RecordedTurn {
                session_id: second.clone(),
                prompt: "still active".to_owned(),
                working_directory: "/second".to_owned(),
            },
        ]
    );
    let closed = close_session(&mut peer, 9, &second).await;
    assert_eq!(closed["result"], json!({}));
    peer.shutdown(10).await;
}
