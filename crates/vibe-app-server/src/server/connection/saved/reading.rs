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
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let history_limit = check.bounded_int(&object, "historyLimit", &[], 200, PAGE_BOUNDS);
        let _turns_limit = check.bounded_int(&object, "turnsLimit", &[], 200, PAGE_BOUNDS);
        let _include_history = check.optional_bool(&object, "includeHistory", &[]);
        let _include_turns = check.optional_bool(&object, "includeTurns", &[]);
        check.extras(
            &object,
            &[
                "sessionId",
                "historyLimit",
                "turnsLimit",
                "includeHistory",
                "includeTurns",
            ],
            &[],
        );
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let history_limit = usize::try_from(history_limit.unwrap_or(200)).unwrap_or(200);
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
        let sessions = self.server.lock_sessions()?;
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
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let limit = check.bounded_int(&object, "historyLimit", &[], 200, PAGE_BOUNDS);
        check.extras(&object, &["sessionId", "historyLimit"], &[]);
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let limit = usize::try_from(limit.unwrap_or(200)).unwrap_or(200);
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
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let turn_id = check.optional_string(&object, "turnId", &[]);
        let page = page_request(&mut check, &object);
        check.extras(&object, &["sessionId", "turnId", "page"], &[]);
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let page = page.unwrap_or_default();
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
                self.server.lock_sessions().ok().and_then(|sessions| {
                    sessions.get(&session_id).map(|session| {
                        session
                            .snapshot
                            .as_ref()
                            .map(|snapshot| snapshot.history.clone())
                            .unwrap_or_default()
                    })
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
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let page = page_request(&mut check, &object);
        check.extras(&object, &["sessionId", "page"], &[]);
        if self.root_key().is_none() {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        }
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let page = page.unwrap_or_default();
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
                let sessions = self.server.lock_sessions()?;
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
        let sessions = self.server.lock_sessions()?;
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

/// Reference `PageRequest`.
#[derive(Default)]
struct Page {
    cursor: Option<String>,
    limit: Option<usize>,
    forward: bool,
}

fn page_request(check: &mut WireCheck, object: &Map<String, Value>) -> Option<Page> {
    let parent = [PathSegment::Field("page".to_owned())];
    let page = check.optional_object(object, "page", &[])?;
    let cursor = check.optional_string(page, "cursor", &parent);
    let limit = check.bounded_int(page, "limit", &parent, 200, PAGE_BOUNDS);
    let forward = match page.get("direction") {
        None | Some(Value::Null) => false,
        Some(Value::String(direction)) if direction == "forward" => true,
        Some(Value::String(direction)) if direction == "backward" => false,
        Some(_) => {
            check.report(
                crate::params::child_path(&parent, "direction"),
                "Input should be 'forward' or 'backward'",
            );
            false
        }
    };
    check.extras(page, &["cursor", "limit", "direction"], &parent);
    Some(Page {
        cursor,
        limit: limit.and_then(|limit| usize::try_from(limit).ok()),
        forward,
    })
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
    let limit = page.limit.unwrap_or(200);
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
