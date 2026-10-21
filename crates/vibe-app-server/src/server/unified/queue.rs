//! A unified session's queue of turns to come.
//!
//! Reference `SessionTurnQueue` (`harness/runtimes/python/.../vibe/_turn_queue.py`)
//! and the queue methods of the harness session (`vibe/_session.py`): a
//! process-local FIFO the session drains whenever it is idle, paused when a
//! turn fails or is interrupted, whose items a client can replace, remove, or
//! fold into the running turn as steering.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_harness::engine::TurnRequest;
use vibe_harness::invocation;

use super::params::{QueueInputParams, QueueItemParams, QueueSteerParams};
use super::{
    AppServer, BegunTurn, DispatchBatch, ProtocolErrorCode, ProtocolFault, ServerConnection,
    ServerRequest, TurnOrigin, UnifiedSession, from_params, now_millis, result_map, success_batch,
    success_bytes,
};

/// Reference `TURN_QUEUE_MAX_ITEMS`.
const MAX_ITEMS: usize = 32;

/// One queued turn: what the client sent, as prepared, and what it runs.
#[derive(Debug, Clone)]
struct Record {
    id: String,
    created_at: u64,
    /// The request's parameters, which an idempotency key answers for.
    params: Value,
    /// The entries as prepared, in their public shape.
    entries: Vec<Value>,
}

/// The queue a session keeps in memory.
#[derive(Debug, Default)]
pub(crate) struct TurnQueue {
    items: Vec<Record>,
    paused: bool,
    idempotency: BTreeMap<String, Record>,
    /// The turn each steered item was folded into.
    steered: BTreeMap<String, String>,
}

impl TurnQueue {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Reference `pause`: only a queue holding items pauses.
    pub(crate) fn pause(&mut self) -> bool {
        if self.items.is_empty() || self.paused {
            return false;
        }
        self.paused = true;
        true
    }

    fn resume(&mut self) -> bool {
        std::mem::replace(&mut self.paused, false)
    }

    fn reset_pause_if_empty(&mut self) {
        if self.items.is_empty() {
            self.paused = false;
        }
    }

    fn pop_next(&mut self) -> Option<Record> {
        if self.paused || self.items.is_empty() {
            return None;
        }
        let record = self.items.remove(0);
        self.reset_pause_if_empty();
        Some(record)
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|record| record.id == id)
    }

    /// The queue as the session state publishes it.
    fn public_items(&self) -> Vec<Value> {
        self.items
            .iter()
            .map(|record| {
                json!({
                    "id": record.id,
                    "createdAt": record.created_at,
                    "entries": record.entries.iter().map(public_entry).collect::<Vec<_>>(),
                })
            })
            .collect()
    }
}

/// Reference `_project_queued_entry` and `project_loop_queue`: a queued
/// user entry without what the host appended to it.
fn public_entry(entry: &Value) -> Value {
    let mut entry = entry.clone();
    if entry["role"] == "user" {
        invocation::project_queued_entry(&mut entry);
        entry["content"] = vibe_harness::loops::without_instructions(&entry["content"]);
        if let Some(annotations) = entry["annotations"].as_object_mut() {
            annotations.retain(|_, value| !value.is_null());
        }
    }
    entry
}

fn not_found(id: &str) -> ProtocolFault {
    ProtocolFault::with_data(
        ProtocolErrorCode::NotFound,
        format!("Queued turn not found: {id}"),
        json!({"queueItemId": id}),
    )
}

/// The entry's text blocks, joined as the reference joins them.
fn text_of(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl AppServer {
    /// Reference `_prepared_queue_params`: each entry in its public shape,
    /// the user entry carrying the bodies of the skills it invokes, the
    /// files it mentions and the `/loop` instructions.
    fn prepare_queue_entries(
        &self,
        unified: &UnifiedSession,
        entries: &[Value],
    ) -> Result<Vec<Value>, ProtocolFault> {
        let user_index = entries.iter().position(|entry| entry["role"] == "user");
        let mut prepared: Vec<Value> = entries
            .iter()
            .map(|entry| {
                let annotations = entry
                    .get("annotations")
                    .and_then(Value::as_object)
                    .map(|annotations| {
                        annotations
                            .iter()
                            .filter(|(_, value)| !value.is_null())
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect::<serde_json::Map<_, _>>()
                    })
                    .unwrap_or_default();
                json!({
                    "role": entry["role"],
                    "entryId": entry.get("entryId").cloned().unwrap_or(Value::Null),
                    "content": entry["content"],
                    "annotations": annotations,
                })
            })
            .collect();
        let Some(index) = user_index else {
            return Ok(prepared);
        };
        let display_key = "vibe.userDisplayContent";
        let display = prepared[index]["annotations"][display_key].clone();
        if !display.is_null() {
            let stripped = invocation::without_display(&display);
            prepared[index]["annotations"][display_key] = stripped;
        }
        let text = text_of(&prepared[index]["content"]);
        let invoked = self.resolved_invoked_skills(unified, &text);
        let mentioned =
            super::prepare::mentioned_file_blocks(&text, std::path::Path::new(&unified.cwd))
                .map_err(|message| {
                    ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message)
                })?;
        let user = &mut prepared[index];
        if let Some(content) = user["content"].as_array_mut() {
            content.extend(mentioned);
            content.extend(
                invoked
                    .iter()
                    .map(|(_, body)| json!({"type": "text", "text": body})),
            );
        }
        if !invoked.is_empty() {
            let names: Vec<String> = invoked.iter().map(|(name, _)| name.clone()).collect();
            let display =
                Some(user["annotations"][display_key].clone()).filter(|value| !value.is_null());
            if let Some(display) = invocation::with_display(display, &names) {
                user["annotations"][display_key] = display;
            }
        }
        if let Some(content) = user["content"].as_array_mut() {
            vibe_harness::loops::prepare_loop_content(content);
        }
        if let Some(annotations) = user["annotations"].as_object_mut() {
            annotations.retain(|_, value| !value.is_null());
        }
        Ok(prepared)
    }

    /// Reference `_drain_turn_queue`: an idle session starts its next queued
    /// turn.
    pub(crate) fn drain_unified_queue(&self, session: &Arc<UnifiedSession>) -> Option<BegunTurn> {
        let record = {
            let mut host = session.host();
            if host.running.is_some() {
                return None;
            }
            host.queue.pop_next()?
        };
        let mut frames = publish_queue(session);
        let user = record.entries.iter().find(|entry| entry["role"] == "user");
        let context: Vec<Vec<Value>> = record
            .entries
            .iter()
            .filter(|entry| entry["role"] == "context")
            .map(|entry| entry["content"].as_array().cloned().unwrap_or_default())
            .collect();
        let request = TurnRequest {
            injected: false,
            content: user
                .and_then(|entry| entry["content"].as_array().cloned())
                .unwrap_or_default(),
            client_message_id: user
                .and_then(|entry| entry["entryId"].as_str())
                .map(ToOwned::to_owned),
            user_display_content: user
                .map(|entry| entry["annotations"]["vibe.userDisplayContent"].clone())
                .filter(|display| !display.is_null()),
        };
        let mut begun = self
            .begin_unified_turn(
                Arc::clone(session),
                request,
                TurnOrigin::Queued {
                    item_id: record.id,
                    context,
                },
            )
            .ok()?;
        frames.append(&mut begun.frames);
        begun.frames = frames;
        Some(begun)
    }
}

/// Mirrors the queue into the session state and publishes the change.
pub(crate) fn publish_queue(session: &UnifiedSession) -> Vec<Vec<u8>> {
    let (items, paused) = {
        let host = session.host();
        (host.queue.public_items(), host.queue.paused)
    };
    let publication = {
        let mut core = session.core();
        core.projector.state.queue_items = items;
        core.projector.state.queue_paused = paused;
        core.publication(vibe_harness::state::Advance::Provisional, None)
    };
    session.publish(&publication)
}

impl ServerConnection {
    /// Reference `enqueue_turn`, `replace_queued_turn`, `read_turn_queue`,
    /// `remove_queued_turn`, `resume_turn_queue` and `steer_queued_turn`.
    pub(in crate::server) fn unified_queue(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        match request.method.as_str() {
            "session/turn/queue/read" => {
                let (items, paused) = {
                    let host = unified.host();
                    (host.queue.public_items(), host.queue.paused)
                };
                Ok(success_batch(
                    request.id,
                    result_map([(
                        "queue",
                        json!({"items": items, "maxItems": MAX_ITEMS, "paused": paused}),
                    )]),
                ))
            }
            "session/turn/enqueue" | "session/turn/queue/replace" => {
                let QueueInputParams {
                    entries,
                    idempotency_key: key,
                    queue_item_id,
                } = from_params(&request.params)?;
                let target = (request.method == "session/turn/queue/replace")
                    .then(|| queue_item_id.unwrap_or_default());
                let prepared = self.server.prepare_queue_entries(&unified, &entries)?;
                let mut params = Value::Object(request.params.clone().into_iter().collect());
                if let Some(object) = params.as_object_mut() {
                    object.remove("queueItemId");
                }
                let (id, duplicate) = {
                    let mut host = unified.host();
                    let index = target
                        .as_deref()
                        .map(|target| host.queue.position(target).ok_or_else(|| not_found(target)))
                        .transpose()?;
                    if let Some(key) = &key
                        && let Some(existing) = host.queue.idempotency.get(key)
                    {
                        if existing.params != params
                            || target.as_ref().is_some_and(|target| existing.id != *target)
                        {
                            return Err(ProtocolFault::with_data(
                                ProtocolErrorCode::Conflict,
                                format!(
                                    "Idempotency key was already used with different input: {key}"
                                ),
                                json!({"idempotencyKey": key}),
                            ));
                        }
                        (existing.id.clone(), true)
                    } else {
                        let record = match index {
                            Some(index) => Record {
                                params,
                                entries: prepared,
                                ..host.queue.items[index].clone()
                            },
                            None => {
                                if host.queue.items.len() >= MAX_ITEMS {
                                    return Err(ProtocolFault::with_data(
                                        ProtocolErrorCode::Conflict,
                                        format!("Turn queue is full ({MAX_ITEMS} items)"),
                                        json!({"maxItems": MAX_ITEMS}),
                                    ));
                                }
                                Record {
                                    id: vibe_core::session_id::uuid_v4(),
                                    created_at: now_millis(),
                                    params,
                                    entries: prepared,
                                }
                            }
                        };
                        match index {
                            Some(index) => host.queue.items[index] = record.clone(),
                            None => host.queue.items.push(record.clone()),
                        }
                        if let Some(key) = &key {
                            host.queue.idempotency.insert(key.clone(), record.clone());
                        }
                        (record.id, false)
                    }
                };
                let mut outbound = vec![success_bytes(
                    request.id,
                    result_map([("queueItemId", json!(id))]),
                )];
                if !duplicate {
                    outbound.extend(publish_queue(&unified));
                    unified.wake();
                }
                Ok(DispatchBatch {
                    outbound,
                    deferred: Vec::new(),
                    close_after_flush: false,
                })
            }
            "session/turn/queue/remove" | "session/turn/queue/resume" => {
                let removed = (request.method == "session/turn/queue/remove")
                    .then(|| from_params::<QueueItemParams>(&request.params))
                    .transpose()?;
                let changed = {
                    let mut host = unified.host();
                    match removed {
                        None => host.queue.resume(),
                        Some(removed) => match host.queue.position(&removed.queue_item_id) {
                            Some(index) => {
                                host.queue.items.remove(index);
                                host.queue.reset_pause_if_empty();
                                true
                            }
                            None => false,
                        },
                    }
                };
                let mut outbound = vec![success_bytes(request.id, result_map::<0>([]))];
                if changed {
                    outbound.extend(publish_queue(&unified));
                    unified.wake();
                }
                Ok(DispatchBatch {
                    outbound,
                    deferred: Vec::new(),
                    close_after_flush: false,
                })
            }
            "session/turn/queue/steer" => self.unified_queue_steer(&request, &unified),
            other => Err(ProtocolFault::plain(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {other}"),
            )),
        }
    }

    /// Reference `steer_queued_turn`: a queued item's user entry joins the
    /// running turn as steering, and leaves the queue.
    fn unified_queue_steer(
        &mut self,
        request: &ServerRequest,
        unified: &Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let QueueSteerParams {
            queue_item_id: item_id,
            expected_turn_id: expected,
        } = from_params(&request.params)?;
        let record = {
            let host = unified.host();
            if let Some(turn) = host.queue.steered.get(&item_id) {
                if *turn != expected {
                    return Err(ProtocolFault::with_data(
                        ProtocolErrorCode::Conflict,
                        format!("Client command was already used: queue-steer:{item_id}"),
                        json!({"harnessCode": "client_command_conflict"}),
                    ));
                }
                return Ok(success_batch(
                    request.id.clone(),
                    result_map([
                        ("queueItemId", json!(item_id)),
                        ("turnId", json!(turn)),
                        ("lastEventId", json!(host.translator.event_id)),
                    ]),
                ));
            }
            host.require_turn(&expected, "No matching active turn")?;
            let index = host
                .queue
                .position(&item_id)
                .ok_or_else(|| not_found(&item_id))?;
            host.queue.items[index].clone()
        };
        if record
            .entries
            .iter()
            .any(|entry| entry["role"] == "context")
        {
            return Err(ProtocolFault::invalid_params(
                "Unified Harness queued steering does not support context entries",
            ));
        }
        let Some(user) = record.entries.iter().find(|entry| entry["role"] == "user") else {
            return Err(ProtocolFault::invalid_params(
                "Unified Harness queued steering requires user content",
            ));
        };
        let publication = {
            let mut core = unified.core();
            let advance = core.steer(
                &expected,
                &TurnRequest {
                    injected: false,
                    content: user["content"].as_array().cloned().unwrap_or_default(),
                    client_message_id: user["entryId"].as_str().map(ToOwned::to_owned),
                    user_display_content: Some(
                        user["annotations"]["vibe.userDisplayContent"].clone(),
                    )
                    .filter(|display| !display.is_null()),
                },
            );
            core.publication(advance, None)
        };
        let mut outbound = unified.publish(&publication);
        {
            let mut host = unified.host();
            if let Some(index) = host.queue.position(&item_id) {
                host.queue.items.remove(index);
                host.queue.reset_pause_if_empty();
                host.queue.steered.insert(item_id.clone(), expected.clone());
            }
        }
        outbound.extend(publish_queue(unified));
        let last_event_id = unified.host().translator.event_id;
        outbound.push(success_bytes(
            request.id.clone(),
            result_map([
                ("queueItemId", json!(item_id)),
                ("turnId", json!(expected)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }
}
