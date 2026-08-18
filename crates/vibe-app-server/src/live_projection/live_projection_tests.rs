use vibe_protocol::{Envelope, Notification, decode_frame};

use super::*;

/// A retry crosses under the reference name and carries the wait's reason,
/// so a client can explain a stalling turn instead of only showing it slow.
#[test]
fn a_retried_request_is_published_as_turn_retrying() {
    let server = AppServer::default();
    let (observer, mut updates) = app_server_update_channel_for_turn("session-1", "turn-1");
    observer
        .observe(&EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            event_id: 1,
            emitted_at: 10,
            event: EngineEvent::Retrying {
                reason: "provider answered HTTP 503".to_owned(),
            },
        })
        .expect("the retry projects");
    let update = updates.try_recv().expect("a retry update is queued");
    let frame = app_server_notification(&server, update).expect("the retry publishes");
    assert!(matches!(
        decode_frame(&frame).expect("retry notification"),
        Envelope::Notification(Notification { method, params, .. })
            if method == "turn/retrying"
                && params["sessionId"] == "session-1"
                && params["reason"] == "provider answered HTTP 503"
                // The reference does not sequence this one, so it carries
                // no event id a client would count.
                && !params.contains_key("eventId")
    ));
}

/// The clearing handoff crosses under its own reference name and carries
/// the body a reference projection validates: two different identifiers, an
/// embedded state whose session and event id match the notification, and
/// the plan the acceptance came from.
#[test]
fn a_cleared_context_publishes_session_context_cleared() {
    let server = AppServer::default();
    let mut connection = server.connect(vibe_protocol::TransportKind::InProcess);
    for frame in [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "test",
                    "version": "1",
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                },
                "capabilities": {}
            }
        }),
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/start",
            "params": {"sessionId": "session-1", "workingDirectory": "/workspace"}
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "sessionId": "session-1",
                "input": [{"type": "text", "text": "plan"}]
            }
        }),
    ] {
        connection.dispatch(&serde_json::to_vec(&frame).expect("request frame"));
    }
    server
        .turn_started("session-1", "turn-1")
        .expect("turn starts");

    let (observer, mut updates) = app_server_update_channel_for_turn("session-1", "turn-1");
    for event in [
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 10,
            event_id: 1,
            event: EngineEvent::UserMessage {
                content: "plan".to_owned(),
            },
        },
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 11,
            event_id: 2,
            event: EngineEvent::SessionHandoff {
                from_session_id: "session-1".to_owned(),
                to_session_id: "session-1-cleared".to_owned(),
                cause: SessionHandoffCause::ContextCleared {
                    plan_file_path: Some("/plans/session-1.md".to_owned()),
                },
            },
        },
    ] {
        observer.observe(&event).expect("event projects");
    }
    let notifications = std::iter::from_fn(|| updates.try_recv().ok())
        .map(|update| {
            app_server_notification(&server, update)
                .and_then(|bytes| decode_frame(&bytes).map_err(ServerError::from))
                .expect("notification projects")
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        notifications.last(),
        Some(Envelope::Notification(_))
    ));
    let Some(Envelope::Notification(Notification { method, params, .. })) = notifications.last()
    else {
        return;
    };
    assert_eq!(method, "session/contextCleared");
    assert_eq!(params["sessionId"], "session-1-cleared");
    assert_eq!(params["oldSessionId"], "session-1");
    assert_eq!(params["planFilePath"], "/plans/session-1.md");
    assert_eq!(params["emittedAt"], 11);
    assert_eq!(params["state"]["eventId"], params["eventId"]);
    assert_eq!(params["state"]["session"]["id"], "session-1-cleared");
    assert!(params.contains_key("sessionLog"));
    assert!(
        !params.contains_key("summaryLength"),
        "a clearing summarizes nothing: {params:?}"
    );
}

#[test]
fn compaction_rebinds_history_and_resets_the_new_session_watermark() {
    let server = AppServer::default();
    let mut connection = server.connect(vibe_protocol::TransportKind::InProcess);
    for frame in [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "test",
                    "version": "1",
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                },
                "capabilities": {}
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/start",
            "params": {"sessionId": "session-1", "workingDirectory": "/workspace"}
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "sessionId": "session-1",
                "input": [{"type": "text", "text": "compact"}]
            }
        }),
    ] {
        connection.dispatch(&serde_json::to_vec(&frame).expect("request frame"));
    }
    let started = server
        .turn_started("session-1", "turn-1")
        .expect("turn starts");
    let started = decode_frame(&started[0]).expect("started notification");
    assert!(matches!(
        started,
        // The snapshot the attachment published opened the sequence, so
        // the first turn event is the second.
        Envelope::Notification(Notification { params, .. })
            if params["eventId"] == 2
    ));

    let (observer, mut updates) = app_server_update_channel_for_turn("session-1", "turn-1");
    for event in [
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 10,
            event_id: 1,
            event: EngineEvent::UserMessage {
                content: "compact".to_owned(),
            },
        },
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 11,
            event_id: 2,
            event: EngineEvent::CompactionStarted {
                compaction_id: "compaction-1".to_owned(),
                current_context_tokens: 150_000,
                threshold: 120_000,
            },
        },
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 12,
            event_id: 3,
            event: EngineEvent::CompactionCompleted {
                compaction_id: "compaction-1".to_owned(),
                summary_length: 5,
                old_session_id: "session-1".to_owned(),
                new_session_id: "session-2".to_owned(),
            },
        },
        EventEnvelope {
            session_id: "session-1".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 13,
            event_id: 4,
            event: EngineEvent::SessionHandoff {
                from_session_id: "session-1".to_owned(),
                to_session_id: "session-2".to_owned(),
                cause: SessionHandoffCause::Compaction,
            },
        },
    ] {
        observer.observe(&event).expect("event projects");
    }
    let notifications = std::iter::from_fn(|| updates.try_recv().ok())
        .map(|update| {
            app_server_notification(&server, update)
                .and_then(|bytes| decode_frame(&bytes).map_err(ServerError::from))
                .expect("notification projects")
        })
        .collect::<Vec<_>>();
    assert_eq!(notifications.len(), 4);
    assert!(matches!(
        &notifications[0],
        Envelope::Notification(Notification { method, params, .. })
            if method == "history/entryAdded" && params["eventId"] == 4
    ));
    assert!(matches!(
        &notifications[1],
        Envelope::Notification(Notification { method, params, .. })
            if method == "history/entryAdded" && params["eventId"] == 5
    ));
    // The end event patches the entry the start added rather than adding a
    // second one, which is what a client renders in place.
    assert!(matches!(
        &notifications[2],
        Envelope::Notification(Notification { method, params, .. })
            if method == "history/entryUpdated" && params["eventId"] == 6
    ));
    assert!(matches!(
        &notifications[3],
        Envelope::Notification(Notification { method, params, .. })
            if method == "session/compacted"
                && params["eventId"] == 1
                && params["sessionId"] == "session-2"
                && params["oldSessionId"] == "session-1"
                && params["summaryLength"] == 5
    ));
    let rebound = server.session("session-2").expect("rebound session");
    assert!(
        rebound
            .snapshot
            .expect("handoff snapshot")
            .history
            .iter()
            .all(|entry| entry.metadata().session_id == "session-2")
    );
}
