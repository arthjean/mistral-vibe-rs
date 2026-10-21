//! Reading a saved session: its state, a page of its history or of its
//! turns, and the turns of a subagent the root delegated to.
//!
//! Reference `session/read`, `_history_list` and `_session_turns_list`
//! (`vibe/app_server/_host.py` and `vibe/app_server/_handler.py`).

use super::*;

impl ServerConnection {
    /// Reference `session/read`: the live session answers for itself, and the
    /// store answers for any other.
    pub(super) fn read_session(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let ReadParams {
            session_id,
            history,
        } = wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        // Reference `SessionReadParams.history_limit`.
        let history_limit = history.map_or(1, |page| usize::from(page.limit));
        if let Some(state) = self.live_state(&session_id)? {
            let event_id = state["eventId"].clone();
            return Ok(success_batch(
                request.id.clone(),
                result_map([("state", state), ("lastEventId", event_id)]),
            ));
        }
        let hydrated = self.load_saved(&session_id)?;
        let state = stored_state(&session_id, &hydrated, history_limit);
        Ok(success_batch(
            request.id.clone(),
            result_map([("state", state), ("lastEventId", json!(0))]),
        ))
    }

    /// The state of a session this connection holds, or `None` for one it
    /// does not.
    pub(super) fn live_state(&self, session_id: &str) -> Result<Option<Value>, ProtocolFault> {
        let sessions = self.server.lock_sessions();
        Ok(sessions
            .get(session_id)
            .filter(|session| {
                session.id == session_id
                    && session.status != SessionStatus::Closed
                    && sessions
                        .key(session_id)
                        .is_some_and(|key| self.attached_sessions.contains(key))
            })
            .map(public_session_state))
    }

    /// The saved session `session_id` names, as the host reads it
    /// (reference `HostRequestHandler._load_session`).
    pub(super) fn load_saved(&self, session_id: &str) -> Result<HydratedSession, ProtocolFault> {
        match self.store().load(session_id) {
            Ok(hydrated) => Ok(hydrated),
            Err(StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_)) => {
                Err(not_found(session_id))
            }
            Err(error) => Err(storage_fault(error)),
        }
    }

    /// Reference `_get_session_history`: always the saved transcript, the
    /// latest `historyLimit` entries of it.
    pub(super) fn history_get(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let HistoryGetParams {
            session_id,
            history_limit,
        } = wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        let limit = usize::from(history_limit);
        let hydrated = self.load_saved(&session_id)?;
        let history = stored_history(&session_id, &hydrated);
        let tail = history.len().saturating_sub(limit);
        Ok(success_batch(
            request.id.clone(),
            result_map([("history", json!(history.get(tail..).unwrap_or_default()))]),
        ))
    }

    /// Reference `_history_list`: the live history for the root, the saved
    /// one for any other session.
    pub(super) fn history_list(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let PagedParams {
            session_id,
            turn_id,
            page,
        } = wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        if self.server.harness_selection().use_unified {
            let state = self.unified_state(&session_id, None)?;
            let history = state["history"].as_array().cloned().unwrap_or_default();
            let entries: Vec<&Value> = history
                .iter()
                .filter(|entry| {
                    turn_id
                        .as_ref()
                        .is_none_or(|turn_id| entry["turnId"].as_str() == Some(turn_id))
                })
                .collect();
            let (items, next, previous) =
                page_window(&entries, &page, ForwardStart::Latest, |entry| {
                    entry["id"].as_str().unwrap_or_default().to_owned()
                });
            return Ok(success_batch(
                request.id.clone(),
                result_map([
                    ("items", json!(items)),
                    ("nextCursor", json!(next)),
                    ("previousCursor", json!(previous)),
                ]),
            ));
        }
        let live = (self.root_id().as_deref() == Some(session_id.as_str()))
            .then(|| {
                self.server.lock_sessions().get(&session_id).map(|session| {
                    session
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.history.clone())
                        .unwrap_or_default()
                })
            })
            .flatten();
        let history = match live {
            Some(history) => history,
            None => stored_history(&session_id, &self.load_saved(&session_id)?),
        };
        let entries: Vec<&PublicHistoryEntry> = history
            .iter()
            .filter(|entry| {
                turn_id
                    .as_ref()
                    .is_none_or(|turn_id| entry.metadata().turn_id.as_ref() == Some(turn_id))
            })
            .collect();
        let (items, next, previous) = page_window(&entries, &page, ForwardStart::Latest, |entry| {
            entry.metadata().id.clone()
        });
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", json!(next)),
                ("previousCursor", json!(previous)),
            ]),
        ))
    }

    /// Reference `_session_turns_list`: only the root has turns to page.
    pub(super) fn turns_list(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = wire_validation::validate_params(&request.method, &request.params);
        if self.root_key().is_none() {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        }
        let PagedParams {
            session_id, page, ..
        } = params.map_err(rejected)?;
        if self.server.harness_selection().use_unified {
            let state = self.unified_state(&session_id, None)?;
            let turns = turns_from_history(
                state["history"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                &session_id,
            );
            let turns: Vec<&Value> = turns.iter().collect();
            let (items, next, previous) = page_window(&turns, &page, ForwardStart::First, |turn| {
                turn["id"].as_str().unwrap_or_default().to_owned()
            });
            return Ok(success_batch(
                request.id.clone(),
                result_map([
                    ("items", json!(items)),
                    ("nextCursor", json!(next)),
                    ("previousCursor", json!(previous)),
                ]),
            ));
        }
        let turns = match self.require_root(&session_id) {
            Ok(_) => {
                let sessions = self.server.lock_sessions();
                sessions
                    .get(&session_id)
                    .map(|session| session.turns.clone())
                    .unwrap_or_default()
            }
            // Reference `SessionCoordinator.turns` also answers for a subagent
            // the root session started, while `session/history/get` and
            // `runtime/read` stay with the root alone.
            Err(fault) => self.child_turns(&session_id)?.ok_or(fault)?,
        };
        let turns: Vec<&PublicTurn> = turns.iter().collect();
        let (items, next, previous) =
            page_window(&turns, &page, ForwardStart::First, |turn| turn.id.clone());
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", json!(next)),
                ("previousCursor", json!(previous)),
            ]),
        ))
    }

    /// Whether `id` names a subagent of the root's unified session.
    pub(in crate::server::connection) fn unified_child(&self, id: &str) -> bool {
        self.root_id()
            .is_some_and(|root| self.server.unified_child_of(&root, id))
    }

    /// The turns of a subagent the root session delegated to, read off the
    /// `task` effect that started it: the child runs one turn, in the driver
    /// rather than in this server, and the effect is where its end is recorded.
    /// `None` when no delegation of the root names `child_session_id`.
    pub(in crate::server::connection) fn child_turns(
        &self,
        child_session_id: &str,
    ) -> Result<Option<Vec<PublicTurn>>, ProtocolFault> {
        let Some(root) = self.root_key() else {
            return Ok(None);
        };
        let sessions = self.server.lock_sessions();
        let Some(snapshot) = sessions
            .get(&root)
            .and_then(|session| session.snapshot.as_ref())
        else {
            return Ok(None);
        };
        Ok(snapshot.history.iter().find_map(|entry| {
            let PublicHistoryEntry::Effect {
                metadata,
                detail,
                state,
                ..
            } = entry
            else {
                return None;
            };
            if detail.kind != vibe_core::events::ToolEffectKind::Subagent
                || detail.child_session_id.as_deref() != Some(child_session_id)
            {
                return None;
            }
            let (status, error) = match state {
                PublicEffectState::Completed { output, .. } => {
                    let response = output
                        .get("response")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match response.rfind(SUBAGENT_ERROR_MARKER) {
                        Some(start) => (
                            PublicTurnStatus::Failed,
                            Some(PublicError {
                                message: response[start + SUBAGENT_ERROR_MARKER.len()..]
                                    .trim_end_matches(']')
                                    .to_owned(),
                                code: None,
                                details: Value::Null,
                            }),
                        ),
                        None => (PublicTurnStatus::Completed, None),
                    }
                }
                PublicEffectState::Failed { .. } | PublicEffectState::Cancelled { .. } => {
                    (PublicTurnStatus::Interrupted, None)
                }
                _ => (PublicTurnStatus::InProgress, None),
            };
            let settled = status != PublicTurnStatus::InProgress;
            Some(vec![PublicTurn {
                input_entry_id: None,
                id: format!("{child_session_id}-turn"),
                session_id: child_session_id.to_owned(),
                status,
                started_at: metadata.created_at,
                completed_at: settled.then_some(metadata.updated_at),
                error,
                stop_reason: None,
                queue_item_id: None,
            }])
        }))
    }
}

/// Reference `SessionReadParams`: a history left out is read as a default
/// page, and one sent as null as none.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadParams {
    session_id: String,
    #[serde(default = "default_page")]
    history: Option<Page>,
}

fn default_page() -> Option<Page> {
    Some(Page::default())
}

/// Reference `SessionHistoryGetParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryGetParams {
    session_id: String,
    #[serde(default = "default_history_limit")]
    history_limit: u16,
}

/// Reference `SessionHistoryListParams` and `SessionTurnsListParams`, which
/// names no turn.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PagedParams {
    session_id: String,
    turn_id: Option<String>,
    #[serde(default)]
    page: Page,
}

/// Reference `PageRequest`.
#[derive(Deserialize)]
#[serde(default)]
struct Page {
    cursor: Option<String>,
    limit: u16,
    #[serde(rename = "direction", deserialize_with = "is_forward")]
    forward: bool,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: default_history_limit(),
            forward: false,
        }
    }
}

/// Whether a page's `direction` reads forward, the other one being backward.
fn is_forward<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(String::deserialize(deserializer)? == "forward")
}

/// Where a forward page that names no cursor starts.
#[derive(Clone, Copy)]
enum ForwardStart {
    /// At the latest items, as reference `history_page` pages a history
    /// (`_history_list`).
    Latest,
    /// At the first items, as reference `_session_turns_list` pages turns.
    First,
}

/// One page of `items`: going backward, the latest items or the ones before
/// the cursor; going forward, the ones after the cursor, or where `start`
/// says without one. Answers the page and its next and previous cursors.
fn page_window<'a, T>(
    items: &[&'a T],
    page: &Page,
    start: ForwardStart,
    id: impl Fn(&T) -> String,
) -> (Vec<&'a T>, Option<String>, Option<String>) {
    let limit = usize::from(page.limit);
    let position = |cursor: &str| items.iter().position(|item| id(item) == cursor);
    let (first, end) = match (page.cursor.as_deref(), page.forward, start) {
        (Some(cursor), true, _) => {
            let first = position(cursor).map_or(items.len(), |index| index + 1);
            (first, (first + limit).min(items.len()))
        }
        (None, true, ForwardStart::First) => (0, limit.min(items.len())),
        (Some(cursor), false, _) => {
            let end = position(cursor).unwrap_or(0);
            (end.saturating_sub(limit), end)
        }
        (None, _, _) => (items.len().saturating_sub(limit), items.len()),
    };
    let window: Vec<&'a T> = items.get(first..end).unwrap_or_default().to_vec();
    let before = (first > 0)
        .then(|| window.first().map(|item| id(item)))
        .flatten();
    let after = (end < items.len())
        .then(|| window.last().map(|item| id(item)))
        .flatten();
    if page.forward {
        (window, after, before)
    } else {
        (window, before, after)
    }
}

/// Reference `_turns_from_history`: a unified session's turns as its history
/// spells them, each one completed, spanning its first entry to its last.
fn turns_from_history(history: &[Value], session_id: &str) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    for entry in history {
        let Some(turn_id) = entry["turnId"].as_str() else {
            continue;
        };
        let previous = turns.iter().position(|turn| turn["id"] == turn_id);
        let input_entry_id = if entry["inputEntryId"].is_null() {
            previous.map_or(Value::Null, |index| turns[index]["inputEntryId"].clone())
        } else {
            entry["inputEntryId"].clone()
        };
        let started_at = previous.map_or_else(
            || entry["createdAt"].clone(),
            |index| turns[index]["startedAt"].clone(),
        );
        let turn = json!({
            "id": turn_id,
            "sessionId": session_id,
            "status": "completed",
            "startedAt": started_at,
            "completedAt": entry["updatedAt"],
            "error": null,
            "stopReason": null,
            "queueItemId": null,
            "inputEntryId": input_entry_id,
        });
        match previous {
            Some(index) => turns[index] = turn,
            None => turns.push(turn),
        }
    }
    turns
}

/// What a failed subagent's answer carries its error under, as
/// `client::live::delegation` appends it (reference `SubagentRunAccumulator`).
const SUBAGENT_ERROR_MARKER: &str = "\n[Subagent error: ";
