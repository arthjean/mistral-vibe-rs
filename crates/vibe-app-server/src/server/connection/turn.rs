//! The turn methods a connection answers: starting one, steering it, injecting
//! context between them, interrupting one and settling a callback.
//!
//! Every mutation of a running turn goes through the same three checks, which
//! [`ServerConnection::mutate_active_turn`] states once: the connection is
//! attached, the session exists, and the turn the client expects is the one that
//! is running.

use super::*;
use crate::server::projection::session_preview;
use crate::server::session_callbacks::{
    callback_effect_id, entry_added_frame, rejection_output, set_effect_state,
    settled_callback_frame,
};
use crate::server::turns::preview_updated_frame;

impl ServerConnection {
    pub(super) fn turn_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.start_turn(request))
    }

    fn start_turn(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let mut params = from_params::<TurnStartParams>(&request.params)?;
        let scheduled = scheduled_loop_turn(&params.user_display_content)
            .map_err(ProtocolFault::invalid_params)?;
        // Reference `TurnStartParams` puts no floor under the message: a blank
        // prompt is a turn like any other (`vibe -p "   "` runs one).
        let mut prompt = content_text(&params.input);
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session.active_turn.is_some() {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "A turn is already running",
            ));
        }
        if !session.turn_queue.is_empty() {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Resume queued turns before starting a new turn",
            ));
        }
        if session.compaction_pending || session.status == SessionStatus::Closed {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Session cannot start another turn",
            ));
        }
        // Reference `SessionExecution.begin`: a manual command holds the
        // session until it settles.
        if let Some(operation) = &session.shell_operation {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is already running shell {}", operation.id),
            ));
        }
        let mut loop_notice = None;
        if let Some((loop_id, fired_at)) = scheduled {
            let fire = self.server.projects.fire_loop_for_session(
                &loop_id,
                &session.id,
                fired_at,
                true,
            )?;
            prompt.clone_from(&fire.prompt);
            params.input = vec![PublicContentBlock::Text { text: fire.prompt }];
            params.user_display_content = Some(json!({
                "kind": "scheduled_loop",
                "loopId": fire.loop_id,
                "firedAt": fired_at,
            }));
            session.active_scheduled_loop = Some(loop_id);
            loop_notice = Some((fire.notice, fired_at));
        }
        let turn = self.open_turn(session, params.injected, None)?;
        let turn_id = turn.id.clone();
        let started_at = turn.started_at;
        let canonical_session_id = session.id.clone();
        let last_event_id = session.event_watermark;
        let activity = self.accept_user_activity(session, started_at)?;
        drop(sessions);
        let mut outbound = vec![success_bytes(
            request.id,
            result_map([("lastEventId", json!(last_event_id)), ("turn", json!(turn))]),
        )];
        outbound.extend(self.user_activity_frames(&canonical_session_id, activity));
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&canonical_session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if let Some((mut notice, fired_at)) = loop_notice {
            let event_id = next_event_id(session);
            notice.params.insert("eventId".to_owned(), json!(event_id));
            notice
                .params
                .insert("sessionId".to_owned(), json!(canonical_session_id.clone()));
            notice
                .params
                .insert("turnId".to_owned(), json!(turn_id.clone()));
            notice.params.insert(
                "emittedAt".to_owned(),
                json!(fired_at.saturating_mul(1_000)),
            );
            if let Some(entry) = notice.params.get_mut("entry") {
                entry["id"] = json!(format!("scheduled-loop:{turn_id}"));
                entry["sessionId"] = json!(canonical_session_id.clone());
                entry["turnId"] = json!(turn_id.clone());
            }
            outbound.push(encode_notification(&notice.method, notice.params));
        }
        Ok(DispatchBatch {
            outbound,
            deferred: vec![DeferredWork::RunTurn {
                session_id: canonical_session_id,
                turn_id,
                prompt,
                input: params.input,
                injected: params.injected,
                client_user_message_id: params.client_user_message_id,
                auto_title: params.auto_title,
                user_display_content: params.user_display_content,
                mention_stats: params.mention_stats,
            }],
            close_after_flush: false,
        })
    }

    /// Makes `session` run a new turn, the one a request or the queue
    /// started: the turn is recorded and the session is running.
    pub(super) fn open_turn(
        &self,
        session: &mut SessionRuntime,
        injected: bool,
        queue_item_id: Option<String>,
    ) -> Result<PublicTurn, ProtocolFault> {
        // A fork a rewind left unwritten is written by its first turn, which
        // reads its transcript back from the store.
        if let Some(published) = self.server.workspace.publish_draft(&session.id)? {
            session.persisted = Some(published);
        }
        let turn_sequence = self.server.next_turn.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!("turn-{turn_sequence}");
        if let Some(review) = &session.review {
            let message_index = review_message_index(&self.server.workspace, session)?;
            review
                .begin_turn_at(&turn_id, message_index)
                .map_err(|error| ProtocolFault::from(ServerError::Resource(error.to_string())))?;
        }
        session.active_turn = Some(turn_id.clone());
        session.stats.injected_turn = injected;
        let started_at = now_millis();
        session.active_turn_started_at = Some(started_at);
        session.status = SessionStatus::Running;
        let turn = PublicTurn {
            id: turn_id,
            session_id: session.id.clone(),
            status: PublicTurnStatus::InProgress,
            started_at,
            completed_at: None,
            error: None,
            stop_reason: None,
            queue_item_id,
        };
        session.record_turn(turn.clone());
        session.updated_at = started_at;
        Ok(turn)
    }

    /// Reference `_accepted_user_activity_result` and, before it,
    /// `_pin_session_active_model`: a turn the client asked for pins the
    /// session's model and bumps the session, both recorded with it.
    ///
    /// Answers whether the pin changed and the `session/updated` that
    /// publishes the bump, which [`Self::user_activity_frames`] puts after the
    /// runtime the pin changed.
    pub(super) fn accept_user_activity(
        &self,
        session: &mut SessionRuntime,
        now: u64,
    ) -> Result<(bool, Option<Vec<u8>>), ProtocolFault> {
        let pinned = self.pin_session_model(session)?;
        Ok((pinned, self.bump_session(session, now)?))
    }

    /// Reference `_pin_session_active_model`: a session that records itself
    /// keeps the model it first took a turn with. Answers whether that
    /// changed.
    pub(super) fn pin_session_model(
        &self,
        session: &mut SessionRuntime,
    ) -> Result<bool, ProtocolFault> {
        if !self.server.workspace.persists_runtime_sessions() {
            return Ok(false);
        }
        let Some(alias) = self.server.workspace.active_model_alias() else {
            return Ok(false);
        };
        if session.pinned_model.as_deref() == Some(alias.as_str()) {
            return Ok(false);
        }
        let store = self.server.workspace.session_store();
        if let Ok(mut hydrated) = store.open(&session.id) {
            store
                .persist_active_model(&mut hydrated.metadata, &alias)
                .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        }
        session.pinned_model = Some(alias);
        Ok(true)
    }

    /// Reference `_persist_accepted_user_activity_metadata`: the session is
    /// bumped, and a session that records itself publishes the bump.
    pub(super) fn bump_session(
        &self,
        session: &mut SessionRuntime,
        now: u64,
    ) -> Result<Option<Vec<u8>>, ProtocolFault> {
        let bumped_at = session.bumped_at.map_or(now, |current| current.max(now));
        session.bumped_at = Some(bumped_at);
        if !self.server.workspace.persists_runtime_sessions() {
            return Ok(None);
        }
        // The accepted turn is recorded with the session, written or not, so
        // a listing orders by it after a restart.
        let store = self.server.workspace.session_store();
        if let Ok(mut hydrated) = store.open(&session.id) {
            store
                .persist_bumped_at(&mut hydrated.metadata, bumped_at)
                .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        }
        let event_id = next_event_id(session);
        Ok(Some(encode_notification(
            "session/updated",
            result_map([
                ("eventId", json!(event_id)),
                ("sessionId", json!(session.id)),
                (
                    "patch",
                    json!([{"op": "replace", "path": "/bumpedAt", "value": bumped_at}]),
                ),
                ("emittedAt", json!(now_millis())),
            ]),
        )))
    }

    /// The frames an accepted turn publishes after its answer: the runtime a
    /// new pin changed, then the bump.
    pub(super) fn user_activity_frames(
        &self,
        session_id: &str,
        (pinned, bumped): (bool, Option<Vec<u8>>),
    ) -> Vec<Vec<u8>> {
        let runtime = pinned
            .then(|| self.runtime_updated_frame(session_id))
            .flatten();
        runtime.into_iter().chain(bumped).collect()
    }

    pub(super) fn turn_steer(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<TurnSteerParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return ProtocolFault::from(rejection).into_batch(request.id);
            }
        };
        let session_id = params.session_id.clone();
        let turn_id = params.expected_turn_id.clone();
        let content = content_text(&params.input);
        let inject_invoked_skill = params.inject_invoked_skill;
        let expected_turn_id = params.expected_turn_id.clone();
        let lookup_turn_id = expected_turn_id.clone();
        self.mutate_active_turn(request.id, &params.session_id, &lookup_turn_id, |session| {
            if session.status != SessionStatus::Running {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::NotSteerable,
                    "Turn is not steerable",
                ));
            }
            session.steering.push(content.clone());
            session.updated_at = now_millis();
            Ok((
                result_map([("turnId", json!(turn_id))]),
                vec![DeferredWork::SteerTurn {
                    session_id,
                    turn_id: expected_turn_id,
                    content,
                    inject_invoked_skill,
                }],
            ))
        })
    }

    pub(super) fn context_inject(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.inject_context(request))
    }

    fn inject_context(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<ContextInjectParams>(&request.params)?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let content = content_text(&params.input);
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session.active_turn.is_some() || session.status != SessionStatus::Idle {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Use turn/steer while a turn is active",
            ));
        }
        let timestamp = now_millis();
        session.context.push(content.clone());
        session.updated_at = timestamp;
        // Reference `TurnController.inject`: context joins the conversation
        // unseen, a message is published as the harness's own entry. Either
        // is recorded with the session at once, as the reference's loop saves
        // it, unless a skill it invokes still has to be read at the next turn.
        let persist = !params.inject_invoked_skill;
        if !params.as_message {
            let persisted = persist
                && self
                    .server
                    .persist_injected_message(session, &content, None, None)?;
            return Ok(DispatchBatch {
                outbound: vec![success_bytes(
                    request.id,
                    result_map([("entries", json!([]))]),
                )],
                deferred: (!persisted)
                    .then_some(DeferredWork::InjectContext {
                        session_id: params.session_id,
                        content,
                        as_message: false,
                        inject_invoked_skill: params.inject_invoked_skill,
                    })
                    .into_iter()
                    .collect(),
                close_after_flush: false,
            });
        }
        let turn_id = format!("injection:{}", vibe_core::session_id::uuid_v4());
        let entry_id = params
            .client_user_message_id
            .clone()
            .unwrap_or_else(vibe_core::session_id::uuid_v4);
        let entry = PublicHistoryEntry::Message {
            metadata: PublicEntryMetadata {
                id: entry_id.clone(),
                session_id: session.id.clone(),
                turn_id: Some(turn_id.clone()),
                created_at: timestamp,
                updated_at: timestamp,
                generation_status: PublicEntryGenerationStatus::Completed,
                related_entry_id: None,
            },
            role: PublicMessageRole::User,
            content: params.input,
            source: Some(PublicMessageSource::Harness),
            user_display_content: None,
        };
        let history = session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.history.clone())
            .unwrap_or_default();
        let had_preview = !session_preview(session, &history).is_empty();
        session
            .snapshot
            .get_or_insert_with(|| ProjectionSnapshot {
                session_id: session.id.clone(),
                turn_id: None,
                handoff_cause: None,
                watermark: 0,
                lifecycle: LifecycleState::Completed,
                title: None,
                history: Vec::new(),
            })
            .history
            .push(entry.clone());
        let mut outbound = vec![entry_added_frame(session, &turn_id, &entry)];
        if !had_preview {
            outbound.push(preview_updated_frame(session, &content));
        }
        let persisted = persist
            && self
                .server
                .persist_injected_message(session, &content, Some(&entry_id), None)?;
        outbound.push(success_bytes(
            request.id,
            result_map([("entries", json!([entry]))]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred: (!persisted)
                .then_some(DeferredWork::InjectContext {
                    session_id: params.session_id,
                    content,
                    as_message: true,
                    inject_invoked_skill: params.inject_invoked_skill,
                })
                .into_iter()
                .collect(),
            close_after_flush: false,
        })
    }

    pub(super) fn turn_interrupt(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.interrupt_turn(request))
    }

    fn interrupt_turn(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TurnParams>(&request.params)?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session.active_turn.as_deref() != Some(&params.expected_turn_id) {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::StaleTurn,
                "Turn is stale",
            ));
        }
        let completed_at = now_millis();
        if let Some(loop_id) = &session.active_scheduled_loop {
            self.server
                .projects
                .finish_loop_fire(loop_id, completed_at / 1_000)?;
        }
        if let Some(review) = &session.review {
            review
                .seal_turn()
                .map_err(|error| ProtocolFault::from(ServerError::Resource(error.to_string())))?;
            self.server.publish_retention_notice(review);
        }
        let started_at = session.active_turn_started_at.unwrap_or_default();
        let canonical_session_id = session.id.clone();
        session.active_turn = None;
        session.active_turn_started_at = None;
        session.active_scheduled_loop = None;
        session.status = SessionStatus::Cancelled;
        session.record_turn(PublicTurn {
            id: params.expected_turn_id.clone(),
            session_id: canonical_session_id,
            status: PublicTurnStatus::Interrupted,
            started_at,
            completed_at: Some(completed_at),
            error: None,
            stop_reason: None,
            queue_item_id: None,
        });
        session.updated_at = completed_at;
        cancel_pending_callback(session, "Turn was interrupted");
        let status = session_updated_frame(session);
        Ok(DispatchBatch {
            outbound: vec![
                success_bytes(request.id, result_map([("interrupted", json!(true))])),
                status,
            ],
            deferred: vec![DeferredWork::InterruptTurn {
                session_id: params.session_id,
                turn_id: params.expected_turn_id,
            }],
            close_after_flush: false,
        })
    }

    /// Reference `callback/result` (`_callback_result` in
    /// `vibe/app_server/_handler.py`): an error rejects the callback, an
    /// output answers it, and the answer carries the session's watermark.
    pub(super) fn reference_callback_result(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = request
            .params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let result = request.params.get("result").cloned().unwrap_or(Value::Null);
        let callback_id = result
            .get("callbackId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut outbound = Vec::new();
        let deferred;
        if let Some(error) = result.get("error").filter(|error| !error.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (status, work) = self.reject_named_callback(&session_id, &callback_id, message)?;
            outbound.extend(status);
            deferred = work;
        } else if let Some(output) = result.get("output").filter(|output| !output.is_null()) {
            let output =
                wire_validation::validate_nested_field("callback/result", "output", output)
                    .map_err(|issues| self.rejected("callback/result", &issues))?;
            let (frames, work) = self.answer_named_callback(&session_id, &callback_id, output)?;
            outbound = frames;
            deferred = work;
        } else {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::InvalidParams,
                "Callback result must include output or error",
            ));
        }
        let last_event_id = self
            .server
            .lock_sessions()?
            .get(&session_id)
            .map_or(0, |session| session.event_watermark);
        // The settlement is published before the answer, which carries its
        // last event, as the reference awaits it inside the handler.
        outbound.push(success_bytes(
            request.id,
            result_map([
                ("lastEventId", json!(last_event_id)),
                ("accepted", json!(true)),
            ]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred,
            close_after_flush: false,
        })
    }

    /// Reference `TurnController.answer_callback`: answers the frames and
    /// work settling the callback produced, none for a repeated answer.
    fn answer_named_callback(
        &mut self,
        session_id: &str,
        callback_id: &str,
        output: Value,
    ) -> Result<(Vec<Vec<u8>>, Vec<DeferredWork>), ProtocolFault> {
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        let pending = session
            .pending_callback
            .as_ref()
            .filter(|callback| callback.id == callback_id)
            .cloned();
        let Some(callback) = pending else {
            let Some(resolved) = session.resolved_callbacks.get(callback_id) else {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::NotFound,
                    format!("Callback not found: {callback_id}"),
                ));
            };
            if resolved.output == output {
                return Ok((Vec::new(), Vec::new()));
            }
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Callback already has a different answer",
            ));
        };
        let kind = match output.get("type").and_then(Value::as_str) {
            Some("approval") => EngineCallbackKind::Approval,
            _ => EngineCallbackKind::UserInput,
        };
        if kind != callback.kind {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Callback answer has the wrong type",
            ));
        }
        let turn_id = session.active_turn.clone().unwrap_or_default();
        let public = serde_json::from_value::<CallbackOutput>(output.clone())
            .map_err(|error| ProtocolFault::invalid_params(error.to_string()))?;
        let cancel_turn = callback_requests_turn_cancel(&output);
        let value = serde_json::to_string(&output).ok();
        settle_pending_callback(
            session,
            callback_id,
            PublicCallbackState::Answered { output: public },
        );
        session.pending_callback = None;
        session
            .resolved_callbacks
            .insert(callback_id.to_owned(), ResolvedCallback { kind, output });
        // Reference `answer_callback`: the callback settles, the effect it
        // gated runs again, and so does the session.
        let mut frames = Vec::new();
        frames.extend(settled_callback_frame(session, &turn_id, callback_id));
        let effect_id = callback_effect_id(session, callback_id);
        frames.extend(set_effect_state(
            session,
            &turn_id,
            effect_id.as_deref(),
            |output_text| PublicEffectState::Running { output_text },
        ));
        session.status = SessionStatus::Running;
        session.updated_at = now_millis();
        frames.push(session_updated_frame(session));
        let mut deferred = vec![DeferredWork::ResolveCallback {
            session_id: session.id.clone(),
            turn_id: turn_id.clone(),
            callback_id: callback_id.to_owned(),
            accepted: true,
            value,
        }];
        if cancel_turn {
            deferred.push(DeferredWork::InterruptTurn {
                session_id: session.id.clone(),
                turn_id,
            });
        }
        drop(sessions);
        self.mark_callback_answered(session_id, callback_id);
        Ok((frames, deferred))
    }

    /// Reference `TurnController.reject_callback`, for a callback the client
    /// names by its identifier.
    fn reject_named_callback(
        &mut self,
        session_id: &str,
        callback_id: &str,
        message: &str,
    ) -> Result<(Option<Vec<u8>>, Vec<DeferredWork>), ProtocolFault> {
        let turn_id = {
            let sessions = self.server.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| session_missing("Session not found"))?;
            let pending = session
                .pending_callback
                .as_ref()
                .is_some_and(|callback| callback.id == callback_id);
            if !pending {
                if let Some(resolved) = session.resolved_callbacks.get(callback_id) {
                    if resolved.output == rejection_output(message) {
                        return Ok((None, Vec::new()));
                    }
                    return Err(ProtocolFault::plain(
                        ProtocolErrorCode::Conflict,
                        "Callback already has a different answer",
                    ));
                }
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::NotFound,
                    format!("Callback not found: {callback_id}"),
                ));
            }
            session.active_turn.clone().unwrap_or_default()
        };
        let route = CallbackRoute {
            session_id: session_id.to_owned(),
            turn_id,
            callback_id: callback_id.to_owned(),
            answered: false,
        };
        let (status, deferred) = self
            .server
            .reject_callback(&route, message)
            .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        self.mark_callback_answered(session_id, callback_id);
        Ok(((!status.is_empty()).then_some(status), deferred))
    }

    /// Reference `_mark_callback_answered`: the delivery's acknowledgement
    /// no longer decides anything once the callback is settled.
    fn mark_callback_answered(&mut self, session_id: &str, callback_id: &str) {
        for route in self.pending_server_requests.values_mut() {
            if route.session_id == session_id && route.callback_id == callback_id {
                route.answered = true;
            }
        }
    }

    pub(super) fn callback_respond(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.respond_to_callback(request))
    }

    fn respond_to_callback(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<CallbackResponseParams>(&request.params)?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        let turn_id = session
            .active_turn
            .clone()
            .ok_or_else(|| ProtocolFault::new(ProtocolErrorCode::StaleTurn, "Turn is stale"))?;
        let Some(callback) = &session.pending_callback else {
            // A client that retries an answer the server already settled reads
            // `duplicate` when it sends the same one, and a conflict when it
            // sends a different one for the same callback.
            let kind =
                validate_callback_output(&params.output).map_err(ProtocolFault::invalid_params)?;
            let Some(resolved) = session.resolved_callbacks.get(&params.callback_id) else {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "Callback is not pending",
                ));
            };
            if resolved.kind == kind && resolved.output == params.output {
                return Ok(success_batch(
                    request.id,
                    result_map([("status", json!("duplicate"))]),
                ));
            }
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Callback already has a different answer",
            ));
        };
        if callback.id != params.callback_id {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Callback ID does not match",
            ));
        }
        let kind = validate_callback_output_against_request(&params.output, callback)
            .map_err(ProtocolFault::invalid_params)?;
        // The answer is published as the union the reference declares, so a
        // body that passed the checks above but is not one of its two forms is
        // rejected rather than echoed back in the answered state.
        let output =
            serde_json::from_value::<CallbackOutput>(params.output.clone()).map_err(|_| {
                ProtocolFault::invalid_params("Callback output does not match the protocol union")
            })?;
        let cancel_turn = callback_requests_turn_cancel(&params.output);
        let value = serde_json::to_string(&params.output).ok();
        settle_pending_callback(
            session,
            &params.callback_id,
            PublicCallbackState::Answered { output },
        );
        session.pending_callback = None;
        session.resolved_callbacks.insert(
            params.callback_id.clone(),
            ResolvedCallback {
                kind,
                output: params.output.clone(),
            },
        );
        session.status = SessionStatus::Running;
        session.updated_at = now_millis();
        let status = session_updated_frame(session);
        let result = result_map([("status", json!("accepted"))]);
        let mut deferred = vec![DeferredWork::ResolveCallback {
            session_id: params.session_id.clone(),
            turn_id: turn_id.clone(),
            callback_id: params.callback_id.clone(),
            accepted: true,
            value,
        }];
        if cancel_turn {
            deferred.push(DeferredWork::InterruptTurn {
                session_id: params.session_id.clone(),
                turn_id,
            });
        }
        drop(sessions);
        for route in self.pending_server_requests.values_mut() {
            if route.session_id == params.session_id && route.callback_id == params.callback_id {
                route.answered = true;
            }
        }
        Ok(DispatchBatch {
            outbound: vec![success_bytes(request.id, result), status],
            deferred,
            close_after_flush: false,
        })
    }

    /// Applies a mutation to the turn a request names, behind the checks every
    /// one of them shares: the connection is attached, the session exists, and
    /// the turn the client expects is the one that is running.
    pub(super) fn mutate_active_turn(
        &mut self,
        request_id: RequestId,
        session_id: &str,
        turn_id: &str,
        mutation: impl FnOnce(
            &mut SessionRuntime,
        )
            -> Result<(BTreeMap<String, Value>, Vec<DeferredWork>), ProtocolFault>,
    ) -> DispatchBatch {
        let id = request_id.clone();
        answered(
            id,
            self.mutate_active_turn_inner(request_id, session_id, turn_id, mutation),
        )
    }

    fn mutate_active_turn_inner(
        &mut self,
        request_id: RequestId,
        session_id: &str,
        turn_id: &str,
        mutation: impl FnOnce(
            &mut SessionRuntime,
        )
            -> Result<(BTreeMap<String, Value>, Vec<DeferredWork>), ProtocolFault>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        if let Some(batch) = self.attachment_error(request_id.clone(), session_id) {
            return Ok(batch);
        }
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::StaleTurn,
                "Turn is stale",
            ));
        }
        let (result, deferred) = mutation(session)?;
        Ok(DispatchBatch {
            outbound: vec![success_bytes(request_id, result)],
            deferred,
            close_after_flush: false,
        })
    }
}
