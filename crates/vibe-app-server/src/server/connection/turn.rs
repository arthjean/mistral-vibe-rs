//! The turn methods a connection answers: starting one, steering it, injecting
//! context between them, interrupting one and settling a callback.
//!
//! Every mutation of a running turn goes through the same three checks, which
//! [`ServerConnection::mutate_active_turn`] states once: the connection is
//! attached, the session exists, and the turn the client expects is the one that
//! is running.

use super::*;

impl ServerConnection {
    pub(super) fn turn_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.start_turn(request))
    }

    fn start_turn(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let mut params = from_params::<TurnStartParams>(&request.params)?;
        let scheduled = scheduled_loop_turn(&params.user_display_content)
            .map_err(ProtocolFault::invalid_params)?;
        let mut prompt = content_text(&params.input);
        if scheduled.is_none() && prompt.trim().is_empty() {
            return Err(ProtocolFault::invalid_params("Prompt must not be empty"));
        }
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session.active_turn.is_some()
            || session.compaction_pending
            || session.status == SessionStatus::Closed
        {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Session cannot start another turn",
            ));
        }
        let mut loop_notice = None;
        if let Some((loop_id, fired_at)) = scheduled {
            let fire = self.server.release4.fire_loop_for_session(
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
        let turn_sequence = self.server.next_turn.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!("turn-{turn_sequence}");
        if let Some(review) = &session.review {
            let message_index = review_message_index(&self.server.release3, session)?;
            review
                .begin_turn_at(&turn_id, message_index)
                .map_err(|error| ProtocolFault::from(ServerError::Resource(error.to_string())))?;
        }
        session.active_turn = Some(turn_id.clone());
        let started_at = now_millis();
        session.active_turn_started_at = Some(started_at);
        session.status = SessionStatus::Running;
        let canonical_session_id = session.id.clone();
        let turn = PublicTurn {
            id: turn_id.clone(),
            session_id: canonical_session_id.clone(),
            status: PublicTurnStatus::InProgress,
            started_at,
            completed_at: None,
            error: None,
            stop_reason: None,
        };
        session.latest_turn = Some(turn.clone());
        session.updated_at = started_at;
        let mut outbound = vec![success_bytes(
            request.id,
            result_map([("turn", json!(turn))]),
        )];
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
                client_user_message_id: params.client_user_message_id,
                auto_title: params.auto_title,
                user_display_content: params.user_display_content,
                mention_stats: params.mention_stats,
            }],
            close_after_flush: false,
        })
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
        let sequence = self.server.next_entry.fetch_add(1, Ordering::Relaxed);
        let timestamp = now_millis();
        session.context.push(content.clone());
        session.updated_at = timestamp;
        let metadata = PublicEntryMetadata {
            id: params
                .client_user_message_id
                .unwrap_or_else(|| format!("entry-{sequence}")),
            session_id: params.session_id.clone(),
            turn_id: Some(format!("injection:{sequence}")),
            created_at: timestamp,
            updated_at: timestamp,
            generation_status: PublicEntryGenerationStatus::Completed,
            related_entry_id: None,
        };
        let entry = if params.as_message {
            PublicHistoryEntry::Message {
                metadata,
                role: PublicMessageRole::User,
                content: params.input,
                source: Some(PublicMessageSource::Harness),
                user_display_content: None,
            }
        } else {
            PublicHistoryEntry::Checkpoint {
                metadata,
                kind: "context_injected".to_owned(),
                message: None,
                details: json!({"content": content}),
            }
        };
        Ok(DispatchBatch {
            outbound: vec![success_bytes(
                request.id,
                result_map([("entries", json!([entry]))]),
            )],
            deferred: vec![DeferredWork::InjectContext {
                session_id: params.session_id,
                content,
                as_message: params.as_message,
                inject_invoked_skill: params.inject_invoked_skill,
            }],
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
                .release4
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
        session.latest_turn = Some(PublicTurn {
            id: params.expected_turn_id.clone(),
            session_id: canonical_session_id,
            status: PublicTurnStatus::Interrupted,
            started_at,
            completed_at: Some(completed_at),
            error: None,
            stop_reason: None,
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
