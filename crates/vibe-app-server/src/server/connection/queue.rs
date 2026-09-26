//! The turn queue methods and the promotion that runs a queued turn once its
//! session is free: reference `TurnController` (`vibe/app_server/_turns.py`)
//! behind the `session/turn/*` handlers of `vibe/app_server/_handler.py`.
//!
//! Every change the client makes publishes the queue as `turn/queueUpdated`
//! and then tries to promote its head, which is what makes an item queued on
//! an idle session start at once.

use super::*;
use crate::server::turn_queue::{QueueRefusal, QueuedTurn};

impl ServerConnection {
    pub(super) fn turn_queue_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.queue_request(request))
    }

    fn queue_request(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(batch) = self.attachment_error(request.id.clone(), &session_id) {
            return Ok(batch);
        }
        let now = now_millis();
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        let canonical_session_id = session.id.clone();
        let key = request
            .params
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let entries = queued_entries(&request.params);
        let asked = json!({
            "sessionId": session_id,
            "entries": entries,
            "idempotencyKey": key,
        });
        // What the answer carries, whether the queue changed, and the
        // activity the change counts as.
        let (result, changed, activity) = match request.method.as_str() {
            "session/turn/queue/read" => (
                result_map([("queue", session.turn_queue.public())]),
                false,
                None,
            ),
            "session/turn/enqueue" => {
                let (item, created) = session
                    .turn_queue
                    .enqueue(
                        asked,
                        entries,
                        key.as_deref(),
                        vibe_core::session_id::uuid_v4(),
                        now,
                    )
                    .map_err(refused)?;
                // Reference `enqueue_turn` pins the model before it queues,
                // and a repeated key is not new activity.
                let pinned = self.pin_session_model(session)?;
                let bumped = created
                    .then(|| self.bump_session(session, now))
                    .transpose()?
                    .flatten();
                (
                    result_map([("queueItemId", json!(item.id))]),
                    created,
                    Some((pinned, bumped)),
                )
            }
            "session/turn/queue/replace" => {
                let queue_item_id = request
                    .params
                    .get("queueItemId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (item, created) = session
                    .turn_queue
                    .replace(queue_item_id, asked, entries, key.as_deref())
                    .map_err(refused)?;
                let bumped = created
                    .then(|| self.bump_session(session, now))
                    .transpose()?
                    .flatten();
                (
                    result_map([("queueItemId", json!(item.id))]),
                    created,
                    Some((false, bumped)),
                )
            }
            "session/turn/queue/remove" => {
                let queue_item_id = request
                    .params
                    .get("queueItemId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let removed = session.turn_queue.remove(queue_item_id);
                (BTreeMap::new(), removed, None)
            }
            "session/turn/queue/resume" => {
                let resumed = session.turn_queue.resume();
                let bumped = resumed
                    .then(|| self.bump_session(session, now))
                    .transpose()?
                    .flatten();
                (BTreeMap::new(), resumed, Some((false, bumped)))
            }
            method => return Err(route::method_not_found(method)),
        };
        drop(sessions);
        let mut batch = success_batch(request.id, result);
        if let Some(activity) = activity {
            batch
                .outbound
                .extend(self.user_activity_frames(&canonical_session_id, activity));
        }
        if changed {
            let mut sessions = self.server.lock_sessions()?;
            if let Some(session) = sessions.get_mut(&canonical_session_id) {
                batch.outbound.push(queue_updated_frame(session));
            }
            drop(sessions);
            let promoted = self.promote_queued_turn(&canonical_session_id)?;
            batch.outbound.extend(promoted.outbound);
            batch.deferred.extend(promoted.deferred);
        }
        Ok(batch)
    }

    /// What a settled turn does to its session's queue: an interrupted one
    /// pauses it, any other runs its head. Reference `_after_turn_terminal`.
    pub(crate) fn after_turn_settled(&mut self, session_id: &str) -> DispatchBatch {
        let interrupted = match self.server.lock_sessions() {
            Ok(mut sessions) => match sessions.get_mut(session_id) {
                Some(session)
                    if session.latest_turn.as_ref().map(|turn| turn.status)
                        == Some(PublicTurnStatus::Interrupted) =>
                {
                    if session.turn_queue.pause() {
                        return DispatchBatch {
                            outbound: vec![queue_updated_frame(session)],
                            deferred: Vec::new(),
                            close_after_flush: false,
                        };
                    }
                    true
                }
                Some(_) => false,
                None => true,
            },
            Err(_) => true,
        };
        if interrupted {
            return DispatchBatch::default();
        }
        self.promote_queued_turn(session_id).unwrap_or_default()
    }

    /// Reference `_promote_next`: the head of an idle session's queue runs.
    /// An item holding only context is injected and the next one tried.
    fn promote_queued_turn(&mut self, session_id: &str) -> Result<DispatchBatch, ProtocolFault> {
        let mut batch = DispatchBatch::default();
        let mut sessions = self.server.lock_sessions()?;
        let Some(session) = sessions.get_mut(session_id) else {
            return Ok(batch);
        };
        loop {
            if session.active_turn.is_some()
                || session.compaction_pending
                || session.status == SessionStatus::Closed
            {
                return Ok(batch);
            }
            let Some(item) = session.turn_queue.peek_next().cloned() else {
                return Ok(batch);
            };
            let contexts = context_texts(&item);
            let Some(user) = item.user_entry().cloned() else {
                session.turn_queue.pop_next();
                batch.outbound.push(queue_updated_frame(session));
                batch
                    .deferred
                    .extend(self.inject_queued_contexts(session, contexts));
                continue;
            };
            let input = serde_json::from_value::<Vec<PublicContentBlock>>(
                user.get("content").cloned().unwrap_or(Value::Null),
            )
            .map_err(|error| ProtocolFault::invalid_params(error.to_string()))?;
            let turn = self.open_turn(session, false, Some(item.id.clone()))?;
            session.turn_queue.pop_next();
            batch.outbound.push(queue_updated_frame(session));
            batch
                .deferred
                .extend(self.inject_queued_contexts(session, contexts));
            batch.deferred.push(DeferredWork::RunTurn {
                session_id: session.id.clone(),
                turn_id: turn.id,
                prompt: content_text(&input),
                input,
                injected: false,
                client_user_message_id: user
                    .get("entryId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                auto_title: None,
                user_display_content: user
                    .pointer("/annotations/vibeUserDisplayContent")
                    .filter(|content| !content.is_null())
                    .cloned(),
                mention_stats: None,
            });
            return Ok(batch);
        }
    }

    /// Reference `_inject_queued_contexts`: context entries join the
    /// conversation without a message of their own.
    fn inject_queued_contexts(
        &self,
        session: &mut SessionRuntime,
        contexts: Vec<String>,
    ) -> Vec<DeferredWork> {
        contexts
            .into_iter()
            .map(|content| {
                session.context.push(content.clone());
                DeferredWork::InjectContext {
                    session_id: session.id.clone(),
                    content,
                    as_message: false,
                    inject_invoked_skill: false,
                }
            })
            .collect()
    }
}

/// The entries of a queued turn as `PublicQueuedTurn` publishes them, with
/// the defaults the reference model fills in.
fn queued_entries(params: &BTreeMap<String, Value>) -> Vec<Value> {
    params
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    json!({
                        "role": entry.get("role").cloned().unwrap_or(Value::Null),
                        "content": entry.get("content").cloned().unwrap_or_else(|| json!([])),
                        "entryId": entry.get("entryId").cloned().unwrap_or(Value::Null),
                        "annotations": entry
                            .get("annotations")
                            .cloned()
                            .unwrap_or_else(|| json!({})),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn context_texts(item: &QueuedTurn) -> Vec<String> {
    item.context_entries()
        .filter_map(|entry| {
            serde_json::from_value::<Vec<PublicContentBlock>>(entry.get("content")?.clone()).ok()
        })
        .map(|content| content_text(&content))
        .collect()
}

/// Publishes the queue as a sequenced `turn/queueUpdated`.
fn queue_updated_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let event_id = next_event_id(session);
    encode_notification(
        "turn/queueUpdated",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("queue", session.turn_queue.public()),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

/// The refusals reference `_turn_queue.py` raises, as its handler maps them.
fn refused(refusal: QueueRefusal) -> ProtocolFault {
    match refusal {
        QueueRefusal::Full => ProtocolFault::with_data(
            ProtocolErrorCode::Conflict,
            format!(
                "Turn queue is full ({} items)",
                crate::server::turn_queue::MAX_ITEMS
            ),
            json!({"maxItems": crate::server::turn_queue::MAX_ITEMS}),
        ),
        QueueRefusal::IdempotencyConflict(key) => ProtocolFault::with_data(
            ProtocolErrorCode::Conflict,
            format!("Idempotency key was already used with different input: {key}"),
            json!({"idempotencyKey": key}),
        ),
        QueueRefusal::ItemNotFound(id) => ProtocolFault::with_data(
            ProtocolErrorCode::NotFound,
            format!("Queued turn not found: {id}"),
            json!({"queueItemId": id}),
        ),
    }
}
