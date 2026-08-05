//! Live projection of engine events into app-server notifications.
//!
//! The reducer keeps the last emitted shape of every history entry so a change
//! is published as a JSON patch instead of a full replacement.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use vibe_core::engine::EventObserver;
use vibe_core::events::{
    ApplyOutcome, EngineEvent, EventEnvelope, ProjectionReducer, ProjectionSnapshot,
    PublicHistoryEntry,
};

use crate::server::{AppServer, ServerError, notification_method};

#[derive(Debug)]
pub(crate) enum AppServerUpdate {
    HistoryAdded {
        session_id: String,
        turn_id: String,
        emitted_at: u64,
        entry: Box<PublicHistoryEntry>,
        snapshot: ProjectionSnapshot,
    },
    HistoryUpdated {
        session_id: String,
        turn_id: String,
        emitted_at: u64,
        entry_id: String,
        patch: Vec<JsonPatchOperation>,
        snapshot: ProjectionSnapshot,
    },
    SessionCompacted {
        old_session_id: String,
        new_session_id: String,
        turn_id: String,
        emitted_at: u64,
        snapshot: ProjectionSnapshot,
        summary_length: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct JsonPatchOperation {
    op: &'static str,
    path: String,
    value: Value,
}

struct AppServerProjection {
    reducer: ProjectionReducer,
    entries: BTreeMap<String, PublicHistoryEntry>,
    summary_length: usize,
}

struct AppServerEventObserver {
    projection: Mutex<AppServerProjection>,
    sender: mpsc::UnboundedSender<AppServerUpdate>,
}

pub(crate) fn app_server_update_channel_for_turn(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    mpsc::UnboundedReceiver<AppServerUpdate>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(AppServerEventObserver {
            projection: Mutex::new(AppServerProjection {
                reducer: ProjectionReducer::for_turn(session_id, turn_id),
                entries: BTreeMap::new(),
                summary_length: 0,
            }),
            sender,
        }),
        receiver,
    )
}

impl EventObserver for AppServerEventObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let mut projection = self
            .projection
            .lock()
            .map_err(|_| "app-server projection lock is poisoned".to_owned())?;
        if projection
            .reducer
            .apply(event)
            .map_err(|error| error.to_string())?
            == ApplyOutcome::Duplicate
        {
            return Ok(());
        }

        if let EngineEvent::Compaction { summary } = &event.event {
            projection.summary_length = summary.chars().count();
        }
        if let EngineEvent::SessionHandoff {
            from_session_id,
            to_session_id,
        } = &event.event
        {
            let snapshot = projection.reducer.state().clone();
            let turn_id = snapshot
                .turn_id
                .clone()
                .ok_or_else(|| "session handoff has no active turn".to_owned())?;
            projection.entries = snapshot
                .history
                .iter()
                .map(|entry| (entry.metadata().id.clone(), entry.clone()))
                .collect();
            self.sender
                .send(AppServerUpdate::SessionCompacted {
                    old_session_id: from_session_id.clone(),
                    new_session_id: to_session_id.clone(),
                    turn_id,
                    emitted_at: event.emitted_at,
                    snapshot,
                    summary_length: projection.summary_length,
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            return Ok(());
        }

        let snapshot = projection.reducer.state().clone();
        let turn_id = snapshot
            .turn_id
            .clone()
            .ok_or_else(|| "live history update has no active turn".to_owned())?;
        for entry in &snapshot.history {
            let entry_id = entry.metadata().id.clone();
            match projection.entries.get(&entry_id) {
                None => self
                    .sender
                    .send(AppServerUpdate::HistoryAdded {
                        session_id: snapshot.session_id.clone(),
                        turn_id: turn_id.clone(),
                        emitted_at: event.emitted_at,
                        entry: Box::new(entry.clone()),
                        snapshot: snapshot.clone(),
                    })
                    .map_err(|_| "app-server update receiver is closed".to_owned())?,
                Some(previous) if previous != entry => {
                    let patch = history_entry_patch(previous, entry)?;
                    self.sender
                        .send(AppServerUpdate::HistoryUpdated {
                            session_id: snapshot.session_id.clone(),
                            turn_id: turn_id.clone(),
                            emitted_at: event.emitted_at,
                            entry_id,
                            patch,
                            snapshot: snapshot.clone(),
                        })
                        .map_err(|_| "app-server update receiver is closed".to_owned())?;
                }
                Some(_) => {}
            }
        }
        projection.entries = snapshot
            .history
            .into_iter()
            .map(|entry| (entry.metadata().id.clone(), entry))
            .collect();
        Ok(())
    }
}

fn history_entry_patch(
    previous: &PublicHistoryEntry,
    current: &PublicHistoryEntry,
) -> Result<Vec<JsonPatchOperation>, String> {
    let previous = serde_json::to_value(previous).map_err(|error| error.to_string())?;
    let current = serde_json::to_value(current).map_err(|error| error.to_string())?;
    let mut patch = Vec::new();
    diff_json("", &previous, &current, &mut patch);
    Ok(patch)
}

fn diff_json(path: &str, previous: &Value, current: &Value, patch: &mut Vec<JsonPatchOperation>) {
    if previous == current {
        return;
    }
    match (previous, current) {
        (Value::Object(previous), Value::Object(current)) => {
            let keys = previous
                .keys()
                .chain(current.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for key in keys {
                let child_path = format!("{path}/{}", escape_json_pointer(&key));
                match (previous.get(&key), current.get(&key)) {
                    (Some(previous), Some(current)) => {
                        diff_json(&child_path, previous, current, patch);
                    }
                    (None, Some(value)) => patch.push(JsonPatchOperation {
                        op: "add",
                        path: child_path,
                        value: value.clone(),
                    }),
                    (Some(_), None) => patch.push(JsonPatchOperation {
                        op: "remove",
                        path: child_path,
                        value: Value::Null,
                    }),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(previous), Value::Array(current)) if previous.len() == current.len() => {
            for (index, (previous, current)) in previous.iter().zip(current).enumerate() {
                diff_json(&format!("{path}/{index}"), previous, current, patch);
            }
        }
        (Value::String(previous), Value::String(current))
            if is_append_path(path) && current.starts_with(previous) =>
        {
            patch.push(JsonPatchOperation {
                op: "append",
                path: path.to_owned(),
                value: Value::String(current[previous.len()..].to_owned()),
            });
        }
        _ => patch.push(JsonPatchOperation {
            op: "replace",
            path: path.to_owned(),
            value: current.clone(),
        }),
    }
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn is_append_path(path: &str) -> bool {
    path == "/state/outputText" || path.ends_with("/text")
}

pub(crate) fn app_server_notification(
    server: &AppServer,
    update: AppServerUpdate,
) -> Result<Vec<u8>, ServerError> {
    let value = match update {
        AppServerUpdate::HistoryAdded {
            session_id,
            turn_id,
            emitted_at,
            entry,
            snapshot,
        } => {
            let event_id = server.apply_live_projection(&session_id, &turn_id, snapshot)?;
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": notification_method("history/entryAdded"),
                "params": {
                    "eventId": event_id,
                    "sessionId": session_id,
                    "turnId": turn_id,
                    "entry": entry,
                    "emittedAt": emitted_at,
                }
            })
        }
        AppServerUpdate::HistoryUpdated {
            session_id,
            turn_id,
            emitted_at,
            entry_id,
            patch,
            snapshot,
        } => {
            let event_id = server.apply_live_projection(&session_id, &turn_id, snapshot)?;
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": notification_method("history/entryUpdated"),
                "params": {
                    "eventId": event_id,
                    "sessionId": session_id,
                    "turnId": turn_id,
                    "entryId": entry_id,
                    "patch": patch,
                    "emittedAt": emitted_at,
                }
            })
        }
        AppServerUpdate::SessionCompacted {
            old_session_id,
            new_session_id,
            turn_id,
            emitted_at,
            snapshot,
            summary_length,
        } => {
            return server.handoff_active_turn(
                &old_session_id,
                &new_session_id,
                &turn_id,
                snapshot,
                summary_length,
                emitted_at,
            );
        }
    };
    serde_json::to_vec(&value).map_err(ServerError::Json)
}

#[cfg(test)]
mod tests {
    use vibe_protocol::{Envelope, Notification, decode_frame};

    use super::*;

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
        let started = decode_frame(&started).expect("started notification");
        assert!(matches!(
            started,
            Envelope::Notification(Notification { params, .. })
                if params["eventId"] == 1
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
                event: EngineEvent::Compaction {
                    summary: "short".to_owned(),
                },
            },
            EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 12,
                event_id: 3,
                event: EngineEvent::SessionHandoff {
                    from_session_id: "session-1".to_owned(),
                    to_session_id: "session-2".to_owned(),
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
        assert_eq!(notifications.len(), 3);
        assert!(matches!(
            &notifications[0],
            Envelope::Notification(Notification { method, params, .. })
                if method == "history/entryAdded" && params["eventId"] == 2
        ));
        assert!(matches!(
            &notifications[1],
            Envelope::Notification(Notification { method, params, .. })
                if method == "history/entryAdded" && params["eventId"] == 3
        ));
        assert!(matches!(
            &notifications[2],
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
}
