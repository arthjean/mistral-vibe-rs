//! Session lifecycle: creation, listing, forking, loading, and shutdown.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::client::EchoTurnDriver;

use super::{RecordingTurnDriver, prompt, start_session};
use crate::agent::state::MAX_CLOSED_SESSIONS;
use crate::agent::{AcpAgent, SESSION_LIST_PAGE_SIZE};
use crate::protocol::{
    AcpError, AcpForkSession, AcpInitializeRequest, AcpLoadSession, AcpNewSession,
};
use crate::session::{SessionSettings, SessionSlot, Thinking};

#[test]
fn persisted_thinking_boolean_and_effort_round_trip_without_losing_off() {
    let disabled = SessionSettings::from_release3_result(&BTreeMap::from([(
        "metadata".to_owned(),
        json!({"config": {"mode": "plan", "thinking": false, "reasoningEffort": "high"}}),
    )]));
    assert_eq!(disabled.mode.as_str(), "plan");
    assert_eq!(disabled.thinking, Thinking::Off);
    assert_eq!(
        disabled.as_config(),
        json!({"mode": "plan", "thinking": false})
    );

    let enabled = SessionSettings::from_release3_result(&BTreeMap::from([(
        "metadata".to_owned(),
        json!({"config": {"thinking": true, "reasoningEffort": "high"}}),
    )]));
    assert_eq!(enabled.thinking, Thinking::High);
    assert_eq!(
        enabled.as_config(),
        json!({"mode": "code", "thinking": true, "reasoningEffort": "high"})
    );
}

#[test]
fn malformed_lifecycle_requests_are_rejected_without_runtime_mutation() {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
    assert!(matches!(
        agent.new_session(AcpNewSession {
            cwd: "/workspace".to_owned(),
            additional_directories: None,
            mcp_servers: Vec::new(),
            meta: None,
        }),
        Err(AcpError::NotInitialized)
    ));
    assert!(matches!(
        agent.initialize_with(AcpInitializeRequest {
            protocol_version: 99,
            ..AcpInitializeRequest::default()
        }),
        Err(AcpError::UnsupportedProtocol(99))
    ));
}

#[test]
fn closed_session_tombstones_stay_bounded() {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
    let mut state = agent.lock_state().expect("agent state");
    for index in 0..MAX_CLOSED_SESSIONS.saturating_add(10) {
        state.tombstone(format!("session-{index:05}"));
    }
    assert_eq!(state.sessions.len(), MAX_CLOSED_SESSIONS);
    assert!(!state.sessions.contains_key("session-00000"));
    assert!(state.sessions.contains_key(&format!(
        "session-{:05}",
        MAX_CLOSED_SESSIONS.saturating_add(9)
    )));
}

#[test]
fn session_load_reservation_is_atomic() {
    let agent = Arc::new(AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts"));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let handles = (0..2)
            .map(|_| {
                let agent = agent.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    agent
                        .lock_state()
                        .expect("agent state")
                        .begin_load("durable-session")
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation thread"))
            .collect::<Vec<_>>()
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(AcpError::SessionConflict(_))))
            .count(),
        1
    );
}

#[tokio::test]
async fn concurrent_close_and_disconnect_finish_the_single_owned_service() {
    let agent = Arc::new(AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts"));
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");

    let harness = agent
        .session_harness(&session.session_id)
        .expect("session harness");
    let service_guard = harness.service.lock().await;
    let closing_agent = agent.clone();
    let closing_session_id = session.session_id.clone();
    let close_task =
        tokio::spawn(async move { closing_agent.close_session(&closing_session_id).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                agent
                    .lock_state()
                    .expect("agent state")
                    .sessions
                    .get(&session.session_id),
                Some(SessionSlot::Closing(_))
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("close reserves the session");

    let disconnecting_agent = agent.clone();
    let disconnect_task = tokio::spawn(async move { disconnecting_agent.disconnect().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !agent.lock_state().expect("agent state").initialized {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect takes ownership");
    drop(service_guard);

    tokio::time::timeout(Duration::from_secs(1), close_task)
        .await
        .expect("close finishes")
        .expect("close task")
        .expect("session closes");
    tokio::time::timeout(Duration::from_secs(1), disconnect_task)
        .await
        .expect("disconnect finishes")
        .expect("disconnect task")
        .expect("agent disconnects");

    let state = agent.lock_state().expect("agent state");
    assert!(matches!(
        state.sessions.get(&session.session_id),
        Some(SessionSlot::Closed(_))
    ));
    assert!(!state.initialized);
}

#[tokio::test]
async fn idle_cancel_is_a_noop_and_close_is_idempotent_for_owned_sessions() {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");
    agent
        .cancel(&session.session_id)
        .await
        .expect("idle cancellation is a no-op");
    agent
        .close_session(&session.session_id)
        .await
        .expect("first close");
    agent
        .close_session(&session.session_id)
        .await
        .expect("repeated close");
    assert!(matches!(
        prompt(&agent, &session.session_id, "after close").await,
        Err(AcpError::SessionNotFound(_))
    ));
    assert!(matches!(
        agent.close_session("unknown").await,
        Err(AcpError::SessionNotFound(_))
    ));
    agent.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn simultaneous_sessions_are_isolated_and_missing_sessions_fail_closed() {
    let agent = AcpAgent::new(EchoTurnDriver::new("answer")).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let first = start_session(&agent, "/first");
    let second = start_session(&agent, "/second");
    assert_ne!(first.session_id, second.session_id);
    let (first_result, second_result) = tokio::join!(
        prompt(&agent, &first.session_id, "first"),
        prompt(&agent, &second.session_id, "second")
    );
    assert!(first_result.is_ok());
    assert!(second_result.is_ok());
    assert!(matches!(
        prompt(&agent, "missing", "question").await,
        Err(AcpError::SessionNotFound(_))
    ));
    agent.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn mode_and_thinking_updates_change_the_next_turn_reservation_intent() {
    let reservations = Arc::new(Mutex::new(Vec::new()));
    let agent =
        AcpAgent::new(RecordingTurnDriver::new(reservations.clone())).expect("agent starts");
    agent.initialize().expect("ACP initializes");
    let session = start_session(&agent, "/workspace");

    prompt(&agent, &session.session_id, "before settings")
        .await
        .expect("initial prompt");
    agent
        .set_mode(&session.session_id, "plan")
        .await
        .expect("mode changes");
    agent
        .set_config_option(&session.session_id, "thinking", "high")
        .await
        .expect("thinking changes");
    prompt(&agent, &session.session_id, "after settings")
        .await
        .expect("updated prompt");

    let reservations = reservations.lock().expect("reservations");
    assert_eq!(reservations.len(), 2);
    assert_eq!(reservations[0].intent.mode.as_deref(), Some("code"));
    assert!(reservations[0].intent.thinking);
    assert_eq!(
        reservations[0].intent.reasoning_effort.as_deref(),
        Some("medium")
    );
    assert_eq!(reservations[1].intent.mode.as_deref(), Some("plan"));
    assert!(reservations[1].intent.thinking);
    assert_eq!(
        reservations[1].intent.reasoning_effort.as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn shared_session_root_supports_history_fork_and_reload() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let session_root = temporary.path().join("sessions");
    let agent =
        AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
            .expect("agent starts")
            .with_session_root(session_root);
    agent.initialize().expect("ACP initializes");
    let cwd = temporary.path().display().to_string();
    let session = start_session(&agent, &cwd);

    prompt(&agent, &session.session_id, "question")
        .await
        .expect("prompt");
    let history = agent
        .history(&session.session_id, 0, 50)
        .await
        .expect("history");
    assert_eq!(history.entries.len(), 2);
    let forked = agent
        .fork_session(AcpForkSession {
            session_id: session.session_id.clone(),
            cwd: cwd.clone(),
            new_session_id: Some("acp-fork".to_owned()),
            message_id: None,
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            meta: None,
        })
        .expect("fork");
    agent
        .close_session(&forked.session_id)
        .await
        .expect("close fork");
    agent
        .close_session(&session.session_id)
        .await
        .expect("close source");
    let loaded = agent
        .load_session(AcpLoadSession {
            session_id: session.session_id,
            cwd,
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            meta: None,
        })
        .expect("reload");
    assert_eq!(
        agent
            .history(&loaded.session_id, 0, 50)
            .await
            .expect("reloaded history")
            .entries
            .len(),
        2
    );
}

#[tokio::test]
async fn restart_stable_user_message_ids_fork_through_the_selected_turn() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let session_root = temporary.path().join("sessions");
    let cwd = temporary.path().to_string_lossy().into_owned();
    let source_id = {
        let agent =
            AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
                .expect("agent starts")
                .with_session_root(session_root.clone());
        agent.initialize().expect("initialize");
        let session = start_session(&agent, &cwd);
        prompt(&agent, &session.session_id, "first")
            .await
            .expect("first turn");
        prompt(&agent, &session.session_id, "second")
            .await
            .expect("second turn");
        agent
            .close_session(&session.session_id)
            .await
            .expect("close source");
        session.session_id
    };

    let agent =
        AcpAgent::new(EchoTurnDriver::new("answer").with_session_root(session_root.clone()))
            .expect("agent restarts")
            .with_session_root(session_root);
    agent.initialize().expect("initialize restarted agent");
    let fork = agent
        .fork_session(AcpForkSession {
            session_id: source_id,
            cwd,
            new_session_id: Some("turn-prefix-fork".to_owned()),
            message_id: Some("history:0:user".to_owned()),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            meta: None,
        })
        .expect("fork from replayed user ID");
    let history = agent
        .history(&fork.session_id, 0, 50)
        .await
        .expect("fork history");
    assert_eq!(
        history
            .entries
            .iter()
            .filter_map(|entry| entry.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        ["first", "answer"]
    );
    assert_eq!(
        crate::updates::history_entry_updates(&history.entries[0], 0).expect("replay first user")
            [0]["messageId"],
        "history:0:user"
    );
}

#[tokio::test]
async fn list_pagination_is_stable_filtered_and_never_repeats_active_sessions() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let session_root = temporary.path().join("sessions");
    let first_cwd = temporary.path().join("first");
    let second_cwd = temporary.path().join("second");
    std::fs::create_dir_all(&first_cwd).expect("first cwd");
    std::fs::create_dir_all(&second_cwd).expect("second cwd");
    let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
        .expect("agent")
        .with_session_root(session_root);
    agent.initialize().expect("initialize");

    for _ in 0..SESSION_LIST_PAGE_SIZE.saturating_add(1) {
        start_session(&agent, &first_cwd.to_string_lossy());
    }
    start_session(&agent, &second_cwd.to_string_lossy());

    let first = agent
        .list_sessions(Some(&first_cwd.to_string_lossy()), None)
        .expect("first page");
    assert_eq!(first.sessions.len(), SESSION_LIST_PAGE_SIZE);
    let cursor = first.next_cursor.as_deref().expect("next cursor");
    let second = agent
        .list_sessions(Some(&first_cwd.to_string_lossy()), Some(cursor))
        .expect("second page");
    assert_eq!(second.sessions.len(), 1);
    assert!(second.next_cursor.is_none());
    assert!(
        first
            .sessions
            .iter()
            .chain(&second.sessions)
            .all(|session| {
                session.cwd == first_cwd.to_string_lossy()
                    && session.meta.get("active") == Some(&json!(true))
            })
    );
    let ids = first
        .sessions
        .iter()
        .chain(&second.sessions)
        .map(|session| &session.session_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), SESSION_LIST_PAGE_SIZE.saturating_add(1));

    let other = agent
        .list_sessions(Some(&second_cwd.to_string_lossy()), None)
        .expect("other cwd");
    assert_eq!(other.sessions.len(), 1);
    assert_eq!(other.sessions[0].cwd, second_cwd.to_string_lossy());
    assert!(matches!(
        agent.list_sessions(None, Some("not-a-cursor")),
        Err(AcpError::InvalidParams(_))
    ));
    agent.disconnect().await.expect("disconnect");
}

#[test]
fn production_cloud_is_lazy_when_the_configured_credential_is_absent() {
    const MISSING: &str = "VIBE_ACP_CLOUD_CREDENTIAL_MUST_REMAIN_UNSET_582F1";
    assert!(std::env::var_os(MISSING).is_none());
    let temporary = tempfile::tempdir().expect("temporary session root");
    let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
        .expect("agent starts")
        .with_session_root(temporary.path().join("sessions"))
        .with_credential_environment(MISSING)
        .with_production_cloud();
    agent.initialize().expect("initialize needs no credential");
    let session = start_session(&agent, &temporary.path().to_string_lossy());
    assert_eq!(
        agent
            .list_sessions(Some(&temporary.path().to_string_lossy()), None)
            .expect("list needs no cloud credential")
            .sessions[0]
            .session_id,
        session.session_id
    );
}

#[tokio::test]
async fn load_and_fork_reconstruct_settings_and_attach_roots_and_stdio_mcp() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let session_root = temporary.path().join("sessions");
    let cwd = temporary.path().join("workspace");
    let additional = temporary.path().join("shared");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&additional).expect("additional root");
    let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
        .expect("agent")
        .with_session_root(session_root);
    agent.initialize().expect("initialize");
    let source = start_session(&agent, &cwd.to_string_lossy());
    agent
        .set_mode(&source.session_id, "plan")
        .await
        .expect("mode");
    agent
        .set_config_option(&source.session_id, "thinking", "high")
        .await
        .expect("thinking");
    let mcp_server = json!({
        "type": "stdio",
        "name": "fixture",
        "command": "/bin/false",
        "args": [],
        "env": [{"name": "FIXTURE", "value": "1"}],
    });
    let forked = agent
        .fork_session(AcpForkSession {
            session_id: source.session_id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            new_session_id: Some("settings-fork".to_owned()),
            message_id: None,
            additional_directories: vec![additional.to_string_lossy().into_owned()],
            mcp_servers: vec![mcp_server.clone()],
            meta: None,
        })
        .expect("fork");
    assert_eq!(
        forked
            .modes
            .as_ref()
            .and_then(|modes| modes.get("currentModeId")),
        Some(&json!("plan"))
    );
    assert_eq!(
        forked.config_options.as_ref().and_then(|options| {
            options
                .iter()
                .find(|option| option["id"] == "thinking")
                .map(|option| option["currentValue"].clone())
        }),
        Some(json!("high"))
    );
    let harness = agent
        .session_harness(&forked.session_id)
        .expect("fork harness");
    let mut service = harness.service.lock().await;
    let view = service.session(&forked.session_id).expect("fork view");
    assert_eq!(
        view.intent.add_directories,
        [additional.to_string_lossy().into_owned()]
    );
    assert_eq!(view.intent.mcp_servers[0]["transport"], "stdio");
    assert_eq!(view.intent.mcp_servers[0]["env"]["FIXTURE"], "1");
    drop(service);

    assert!(matches!(
        agent.fork_session(AcpForkSession {
            session_id: source.session_id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            new_session_id: None,
            message_id: Some("message-1".to_owned()),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            meta: None,
        }),
        Err(AcpError::InvalidParams(_))
    ));

    agent
        .close_session(&forked.session_id)
        .await
        .expect("close fork");
    let loaded = agent
        .load_session(AcpLoadSession {
            session_id: forked.session_id,
            cwd: cwd.to_string_lossy().into_owned(),
            additional_directories: vec![additional.to_string_lossy().into_owned()],
            mcp_servers: vec![mcp_server],
            meta: None,
        })
        .expect("load");
    assert_eq!(
        loaded
            .modes
            .as_ref()
            .and_then(|modes| modes.get("currentModeId")),
        Some(&json!("plan"))
    );
    assert_eq!(
        loaded.config_options.as_ref().and_then(|options| {
            options
                .iter()
                .find(|option| option["id"] == "thinking")
                .map(|option| option["currentValue"].clone())
        }),
        Some(json!("high"))
    );
    agent.disconnect().await.expect("disconnect");
}
