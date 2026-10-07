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
    PublicHistoryEntry, SessionHandoffCause,
};

use crate::server::{AppServer, HandoffNotice, ServerError, notification_method};

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
    /// A session rotated under its running turn. The cause names the
    /// notification: a compaction summarized the transcript, a clearing
    /// dropped it.
    SessionHandoff {
        old_session_id: String,
        new_session_id: String,
        turn_id: String,
        emitted_at: u64,
        snapshot: ProjectionSnapshot,
        notice: HandoffNotice,
    },
    /// A provider request the backend is retrying, so a client can say why the
    /// turn is stalling instead of showing it as merely slow.
    Retrying {
        session_id: String,
        category: String,
        detail: String,
    },
    /// Usage the engine reported mid-turn, recorded without a frame.
    Stats {
        session_id: String,
        turn_id: String,
        context_tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    /// A steer joined the transcript, which bumps the session.
    SteerApplied { session_id: String },
    /// A model step ran to completion, which may make a background title
    /// due. The transport that runs the turn schedules it; no frame follows
    /// from the update itself.
    StepCompleted {
        session_id: String,
        turn_completing: bool,
    },
    /// Tool calls the turn settled, counted without a frame. Calls settled
    /// `after_turn` are counted once the turn's closing accounting is out.
    ToolCalls {
        session_id: String,
        turn_id: String,
        settled: vibe_core::engine::ToolCallTally,
        after_turn: bool,
    },
    /// The point where the reference looks at the context size again: after
    /// the first event its loop yields once a round trip's usage landed
    /// (`_run_turn`), which publishes the accounting when the size moved.
    StatsCheck { session_id: String, turn_id: String },
    /// The operator's message of a session that had no preview yet, which
    /// the reference publishes as the session's preview (`EventProjector.
    /// _project_user_message`).
    Preview {
        session_id: String,
        turn_id: String,
        text: String,
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
    /// Usage landed since the reference last looked at the context size.
    stats_pending: bool,
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
                stats_pending: false,
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

        // Either shape of a finished compaction answers the same question the
        // handoff notification asks next: how long the summary was. The first
        // is what a transcript written before the boundary pair carries.
        if let EngineEvent::Compaction { summary } = &event.event {
            projection.summary_length = summary.chars().count();
        }
        if let EngineEvent::CompactionCompleted { summary_length, .. } = &event.event {
            projection.summary_length = usize::try_from(*summary_length).unwrap_or(usize::MAX);
        }
        if let EngineEvent::Retrying { category, detail } = &event.event {
            self.sender
                .send(AppServerUpdate::Retrying {
                    session_id: projection.reducer.state().session_id.clone(),
                    category: category.clone(),
                    detail: detail.clone(),
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            return Ok(());
        }
        // A settled call is counted in two parts, around the accounting the
        // reference may publish while its result event is read.
        let (settled, counted_after) = vibe_core::engine::ToolCallTally::settled_by(&event.event);
        let count_calls = |settled: vibe_core::engine::ToolCallTally,
                           after_turn: bool,
                           state: &ProjectionSnapshot|
         -> Result<(), String> {
            let Some(turn_id) = state.turn_id.clone() else {
                return Ok(());
            };
            if settled == vibe_core::engine::ToolCallTally::default() {
                return Ok(());
            }
            self.sender
                .send(AppServerUpdate::ToolCalls {
                    session_id: state.session_id.clone(),
                    turn_id,
                    settled,
                    after_turn,
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())
        };
        count_calls(settled, false, projection.reducer.state())?;
        count_calls(
            vibe_core::engine::ToolCallTally::abandoned_by(&event.event),
            true,
            projection.reducer.state(),
        )?;
        if let EngineEvent::StepCompleted { turn_completing } = event.event {
            self.sender
                .send(AppServerUpdate::StepCompleted {
                    session_id: projection.reducer.state().session_id.clone(),
                    turn_completing,
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            return Ok(());
        }
        if matches!(event.event, EngineEvent::SteerApplied) {
            self.sender
                .send(AppServerUpdate::SteerApplied {
                    session_id: projection.reducer.state().session_id.clone(),
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            return Ok(());
        }
        if let EngineEvent::Stats {
            context_tokens,
            input_tokens,
            output_tokens,
            cached_tokens,
        } = event.event
        {
            let state = projection.reducer.state();
            let Some(turn_id) = state.turn_id.clone() else {
                return Ok(());
            };
            self.sender
                .send(AppServerUpdate::Stats {
                    session_id: state.session_id.clone(),
                    turn_id,
                    context_tokens,
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            projection.stats_pending = true;
            return Ok(());
        }
        if let EngineEvent::SessionHandoff {
            from_session_id,
            to_session_id,
            cause,
        } = &event.event
        {
            let snapshot = projection.reducer.state().clone();
            let turn_id = snapshot
                .turn_id
                .clone()
                .ok_or_else(|| "session handoff has no active turn".to_owned())?;
            // What the rotation itself wrote, the notice a clearing leaves, is
            // published under the new identifier once the handoff is out.
            let written = snapshot
                .history
                .iter()
                .filter(|entry| !projection.entries.contains_key(&entry.metadata().id))
                .cloned()
                .collect::<Vec<_>>();
            projection.entries = snapshot
                .history
                .iter()
                .map(|entry| (entry.metadata().id.clone(), entry.clone()))
                .collect();
            let notice = match cause {
                SessionHandoffCause::Compaction => HandoffNotice::Compacted {
                    summary_length: projection.summary_length,
                },
                SessionHandoffCause::ContextCleared { plan_file_path } => {
                    HandoffNotice::ContextCleared {
                        plan_file_path: plan_file_path.clone(),
                    }
                }
            };
            self.sender
                .send(AppServerUpdate::SessionHandoff {
                    old_session_id: from_session_id.clone(),
                    new_session_id: to_session_id.clone(),
                    turn_id: turn_id.clone(),
                    emitted_at: event.emitted_at,
                    snapshot: snapshot.clone(),
                    notice,
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            for entry in written {
                self.sender
                    .send(AppServerUpdate::HistoryAdded {
                        session_id: snapshot.session_id.clone(),
                        turn_id: turn_id.clone(),
                        emitted_at: event.emitted_at,
                        entry: Box::new(entry),
                        snapshot: snapshot.clone(),
                    })
                    .map_err(|_| "app-server update receiver is closed".to_owned())?;
            }
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
                    let patch = history_entry_patch(previous, entry, event.emitted_at)?;
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
        if let EngineEvent::UserMessage { content, .. } = &event.event
            && !content.is_empty()
        {
            self.sender
                .send(AppServerUpdate::Preview {
                    session_id: snapshot.session_id.clone(),
                    turn_id: turn_id.clone(),
                    text: content.clone(),
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
        }
        // The reference announces a call and settles its own turn outside the
        // loop that looks at the context size, and announces calls while the
        // reply streams, before its usage lands.
        let checks_stats = !matches!(
            event.event,
            EngineEvent::ToolCallAnnounced { .. }
                | EngineEvent::ToolCallUnresolved { .. }
                | EngineEvent::Lifecycle { .. }
                | EngineEvent::RequestSent { .. }
                | EngineEvent::TurnOpened { .. }
                | EngineEvent::CompactionOutcome { .. }
        );
        if checks_stats && std::mem::take(&mut projection.stats_pending) {
            self.sender
                .send(AppServerUpdate::StatsCheck {
                    session_id: snapshot.session_id.clone(),
                    turn_id: turn_id.clone(),
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
        }
        count_calls(counted_after, false, &snapshot)?;
        projection.entries = snapshot
            .history
            .into_iter()
            .map(|entry| (entry.metadata().id.clone(), entry))
            .collect();
        Ok(())
    }
}

/// The patch the reference sends for an entry that changed.
///
/// The reference projector names its operations by the event that caused
/// them (`vibe/app_server/_projector.py`): streamed text and tool output are
/// appended, anything else replaces the top-level field it changed whole, and
/// every patch ends by stamping `/updatedAt`. Diffing the two shapes by
/// top-level field reproduces those operations in the order the projector
/// writes them.
fn history_entry_patch(
    previous: &PublicHistoryEntry,
    current: &PublicHistoryEntry,
    emitted_at: u64,
) -> Result<Vec<JsonPatchOperation>, String> {
    let previous = serde_json::to_value(previous).map_err(|error| error.to_string())?;
    let current = serde_json::to_value(current).map_err(|error| error.to_string())?;
    let (Value::Object(previous), Value::Object(current)) = (previous, current) else {
        return Err("a history entry is not an object".to_owned());
    };
    let mut keys = FIELD_ORDER
        .iter()
        .map(|key| (*key).to_owned())
        .collect::<Vec<_>>();
    keys.extend(
        previous
            .keys()
            .chain(current.keys())
            .filter(|key| !FIELD_ORDER.contains(&key.as_str()) && key.as_str() != "updatedAt")
            .cloned()
            .collect::<BTreeSet<_>>(),
    );
    let mut patch = Vec::new();
    for key in keys {
        let (before, after) = (previous.get(&key), current.get(&key));
        if before == after {
            continue;
        }
        let path = format!("/{}", escape_json_pointer(&key));
        let Some(after) = after else {
            patch.push(JsonPatchOperation {
                op: "remove",
                path,
                value: Value::Null,
            });
            continue;
        };
        let appended = before.and_then(|before| match key.as_str() {
            "content" => appended_at(before, after, &["0", "text"]),
            "text" => appended_at(before, after, &[]),
            "state" => appended_at(before, after, &["outputText"]),
            _ => None,
        });
        // Reference `link_subagent` names the child a delegation opened by
        // replacing that one field of the effect's detail.
        let linked = before.filter(|_| key == "detail").and_then(|before| {
            let mut before = before.clone();
            let mut after = after.clone();
            let field = "childSessionId";
            let (Some(old), Some(new)) = (
                before
                    .as_object_mut()
                    .and_then(|fields| fields.remove(field)),
                after
                    .as_object_mut()
                    .and_then(|fields| fields.remove(field)),
            ) else {
                return None;
            };
            (old != new && before == after).then_some(new)
        });
        if let Some(child_session_id) = linked {
            patch.push(JsonPatchOperation {
                op: "replace",
                path: format!("{path}/childSessionId"),
                value: child_session_id,
            });
            continue;
        }
        patch.push(match appended {
            Some((suffix, text)) => JsonPatchOperation {
                op: "append",
                path: format!("{path}{suffix}"),
                value: Value::String(text),
            },
            None => JsonPatchOperation {
                op: "replace",
                path,
                value: after.clone(),
            },
        });
    }
    if !patch.is_empty() {
        let stamp = current
            .get("updatedAt")
            .filter(|stamp| previous.get("updatedAt") != Some(*stamp))
            .cloned()
            .unwrap_or_else(|| Value::from(emitted_at));
        patch.push(JsonPatchOperation {
            op: "replace",
            path: "/updatedAt".to_owned(),
            value: stamp,
        });
    }
    Ok(patch)
}

/// The top-level fields in the order the reference projector patches them.
const FIELD_ORDER: [&str; 7] = [
    "content",
    "text",
    "detail",
    "message",
    "details",
    "state",
    "generationStatus",
];

/// The text appended at `pointer` below a field, when that string grew and
/// nothing else in the field changed.
fn appended_at(before: &Value, after: &Value, pointer: &[&str]) -> Option<(String, String)> {
    let mut before = before.clone();
    let mut after = after.clone();
    let mut previous = &mut before;
    let mut current = &mut after;
    for segment in pointer {
        previous = match previous {
            Value::Object(fields) => fields.get_mut(*segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
        current = match current {
            Value::Object(fields) => fields.get_mut(*segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    let (Value::String(old), Value::String(new)) = (&*previous, &*current) else {
        return None;
    };
    let text = new.strip_prefix(old.as_str())?.to_owned();
    if text.is_empty() {
        return None;
    }
    // Everything but the grown string has to be unchanged.
    *previous = Value::Null;
    *current = Value::Null;
    (before == after).then(|| {
        let suffix = pointer
            .iter()
            .map(|segment| format!("/{}", escape_json_pointer(segment)))
            .collect::<String>();
        (suffix, text)
    })
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// The frame one update publishes, or `None` for an update the reference
/// keeps silent.
pub(crate) fn app_server_notification(
    server: &AppServer,
    update: AppServerUpdate,
) -> Result<Option<Vec<u8>>, ServerError> {
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
        AppServerUpdate::Retrying {
            session_id,
            category,
            detail,
        } => serde_json::json!({
            "jsonrpc": "2.0",
            "method": notification_method("turn/retrying"),
            // The reference does not sequence this one: it reports a wait, not
            // a state change the client's projection has to order.
            "params": {"sessionId": session_id, "category": category, "detail": detail},
        }),
        AppServerUpdate::Stats {
            session_id,
            turn_id,
            context_tokens,
            input_tokens,
            output_tokens,
            cached_tokens,
        } => {
            server.record_turn_stats(
                &session_id,
                &turn_id,
                context_tokens,
                input_tokens,
                output_tokens,
                cached_tokens,
            )?;
            return Ok(None);
        }
        AppServerUpdate::StepCompleted { .. } => return Ok(None),
        AppServerUpdate::SteerApplied { session_id } => {
            return server.record_steer_activity(&session_id);
        }
        AppServerUpdate::ToolCalls {
            session_id,
            turn_id,
            settled,
            after_turn,
        } => {
            server.record_turn_tool_calls(&session_id, &turn_id, settled, after_turn)?;
            return Ok(None);
        }
        AppServerUpdate::StatsCheck {
            session_id,
            turn_id,
        } => return server.check_turn_stats(&session_id, &turn_id),
        AppServerUpdate::Preview {
            session_id,
            turn_id,
            text,
        } => return server.publish_turn_preview(&session_id, &turn_id, &text),
        AppServerUpdate::SessionHandoff {
            old_session_id,
            new_session_id,
            turn_id,
            emitted_at,
            snapshot,
            notice,
        } => {
            return server
                .handoff_active_turn(
                    &old_session_id,
                    &new_session_id,
                    &turn_id,
                    snapshot,
                    &notice,
                    emitted_at,
                )
                .map(Some);
        }
    };
    serde_json::to_vec(&value)
        .map(Some)
        .map_err(ServerError::Json)
}

#[cfg(test)]
mod live_projection_tests;
