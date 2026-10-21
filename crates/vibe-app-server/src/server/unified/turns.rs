//! The turns of a unified session, and what a client does to one: start
//! it, steer it, interrupt it, compact between turns, inject context and
//! answer the callbacks a turn opens.
//!
//! Reference `start_turn`, `steer_turn`, `interrupt_turn`, `compact`,
//! `_inject_context` and `respond_to_callback` on a unified session
//! (`vibe/app_server/_unified_harness_backend_adapter.py`), and the
//! background processes a turn leaves running.

use super::params::{CallbackResultParams, InjectParams, ProcessOutputParams, ProcessStopParams};
use super::*;

impl ServerConnection {
    /// Reference `start_turn` on a unified session.
    pub(in crate::server) fn unified_turn_start(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TurnStartParams>(&request.params)?;
        let content: Vec<Value> = params
            .input
            .iter()
            .filter_map(|block| serde_json::to_value(block).ok())
            .collect();
        let (content, warnings) = self.server.describe_unified_images(&unified, content)?;
        let (content, display) = self.server.prepare_unified_input(
            &unified,
            content,
            params.user_display_content.clone(),
            true,
        )?;
        let turn_request = TurnRequest {
            content,
            client_message_id: params.client_user_message_id.clone(),
            user_display_content: display,
            injected: params.injected,
        };
        let begun = self
            .server
            .begin_unified_turn(unified, turn_request, TurnOrigin::Client)?;
        let mut outbound = warnings;
        outbound.push(success_bytes(
            request.id,
            result_map([
                (
                    "turn",
                    json!({
                        "id": begun.turn_id,
                        "sessionId": begun.session_id,
                        "status": "in_progress",
                        "startedAt": begun.started_at,
                        "completedAt": null,
                        "error": null,
                        "stopReason": null,
                        "inputEntryId": null,
                        "queueItemId": null,
                    }),
                ),
                ("lastEventId", json!(begun.event_id)),
            ]),
        ));
        let announce = self
            .server
            .lock_sessions()
            .get_mut(&begun.session_id)
            .is_some_and(|session| std::mem::take(&mut session.connectors.announce));
        if announce {
            outbound.extend(self.runtime_updated_frame(&begun.session_id));
        }
        outbound.extend(begun.frames);
        Ok(DispatchBatch {
            outbound,
            deferred: vec![DeferredWork::RunUnifiedTurn(begun.work)],
            close_after_flush: false,
        })
    }

    /// Reference `compact` on a unified session: the context is summarized
    /// between turns, and the reply waits for the summary.
    pub(in crate::server) fn unified_compact(
        &mut self,
        request: &ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<SessionCompactParams>(&request.params)?;
        let (interrupt, interrupted) = watch::channel(None);
        {
            let mut host = unified.host();
            host.require_idle()?;
            host.running = Some(String::new());
            host.interrupt = Some(interrupt);
        }
        let environment = self
            .server
            .unified_environment(&unified, self.launch.as_ref())?;
        let session_log = self.server.session_log_summary(&unified.id);
        Ok(DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::RunUnifiedTurn(UnifiedTurnWork {
                session: unified,
                turn_id: String::new(),
                environment: Box::new(environment),
                interrupted,
                delivery: self.callback_delivery(),
                compaction: Some(CompactionRequest {
                    request_id: request.id.clone(),
                    extra_instructions: params.extra_instructions,
                    session_log,
                }),
            })],
            close_after_flush: false,
        })
    }

    /// Reference `_inject_context`: context the model reads before the next
    /// turn, kept out of the public history unless asked for as a message.
    pub(in crate::server) fn unified_context_inject(
        &mut self,
        request: &ServerRequest,
        unified: &Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let InjectParams {
            input: content,
            as_message,
        } = from_params(&request.params)?;
        unified.host().require_idle()?;
        if content.is_empty() {
            return Err(ProtocolFault::invalid_params(
                "Unified Harness context injection requires content",
            ));
        }
        unified.promote();
        let (entries, publication) = {
            let mut core = unified.core();
            core.conversation
                .push(vibe_harness::engine::ConversationMessage::User {
                    content: content.clone(),
                    injected: true,
                });
            if as_message {
                let now = now_millis();
                let entry = vibe_harness::state::public_message_entry(
                    &unified.id,
                    None,
                    &format!("context-{}", vibe_core::session_id::random_hex::<16>()),
                    "user",
                    &content,
                    now,
                    "harness",
                );
                let advance = core.projector.append_entries(vec![entry.clone()], now);
                (vec![entry], Some(core.publication(advance, None)))
            } else {
                (Vec::new(), None)
            }
        };
        let mut outbound = vec![success_bytes(
            request.id.clone(),
            result_map([(
                "entries",
                Value::Array(entries.iter().map(wire_entry).collect()),
            )]),
        )];
        if let Some(publication) = publication {
            outbound.extend(unified.publish(&publication));
        }
        unified.settle();
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    /// Reference `respond_to_callback` on a unified session: the callback
    /// settles, the session is bumped, and the call waiting on it resumes once
    /// both are on the wire.
    pub(in crate::server) fn unified_callback_result(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let CallbackResultParams {
            sent: result,
            callback_id,
            output,
            error,
        } = from_params(&request.params)?;
        let answer = if let Some(error) = error {
            CallbackAnswer::Closed(if error.message.is_empty() {
                "Callback closed".to_owned()
            } else {
                error.message
            })
        } else if let Some(output) = output {
            CallbackAnswer::Output(
                crate::wire_validation::validate_nested_field("callback/result", "output", &output)
                    .map_err(|issues| self.rejected("callback/result", &issues))?,
            )
        } else {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::InvalidParams,
                "Callback result must include output or error",
            ));
        };
        let cancels_turn = matches!(
            &answer,
            CallbackAnswer::Output(output) if output.pointer("/decision/type") == Some(&json!("cancel_turn"))
        );
        // A callback a subagent opened is answered on the subagent's session.
        let child = {
            let core = unified.core();
            let held = core.callbacks.is_open(&callback_id)
                || core.callbacks.answered.contains_key(&callback_id)
                || core.callbacks.closed.contains(&callback_id);
            if held {
                None
            } else {
                core.subagents.child_with_callback(&callback_id)
            }
        };
        let target = child
            .as_ref()
            .map_or_else(|| Arc::clone(&unified.core), |(_, core)| Arc::clone(core));
        let publication = {
            let mut core = lock(&target);
            if let Some(previous) = core.callbacks.answered.get(&callback_id) {
                if *previous != result {
                    return Err(ProtocolFault::plain(
                        ProtocolErrorCode::Conflict,
                        format!("Callback already answered differently: {callback_id}"),
                    ));
                }
                None
            } else {
                if !core.callbacks.is_open(&callback_id) {
                    let message = if core.callbacks.closed.contains(&callback_id) {
                        format!("Callback closed: {callback_id}")
                    } else {
                        format!("Callback not found: {callback_id}")
                    };
                    return Err(ProtocolFault::plain(ProtocolErrorCode::NotFound, message));
                }
                let entry = core
                    .callbacks
                    .settle(&callback_id, answer, now_millis())
                    .ok_or_else(|| ProtocolFault::internal("the callback is gone"))?;
                core.callbacks.answered.insert(callback_id.clone(), result);
                Some(core.publication(Advance::Provisional, Some(CallbackEvent::Resolved(entry))))
            }
        };
        let mut outbound = Vec::new();
        let mut deferred = Vec::new();
        if let Some(publication) = publication {
            match &child {
                Some((child_id, _)) => {
                    let manager = Arc::clone(&unified.core().subagents);
                    manager.announce(child_id, publication);
                }
                None => outbound.extend(unified.publish(&publication)),
            }
            outbound.extend(unified.bump());
            deferred.push(DeferredWork::ReleaseUnifiedCallback {
                session_id: child
                    .as_ref()
                    .map_or_else(|| unified.id.clone(), |(child_id, _)| child_id.clone()),
                callback_id,
                cancel_turn: cancels_turn,
            });
        }
        let last_event_id = unified.host().translator.event_id;
        outbound.push(success_bytes(
            request.id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred,
            close_after_flush: false,
        })
    }

    /// Reference `steer_turn`: the message joins the running turn's history
    /// at once, and the model reads it on its next call.
    pub(in crate::server) fn unified_turn_steer(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TurnSteerParams>(&request.params)?;
        unified
            .host()
            .require_turn(&params.expected_turn_id, "No matching active turn")?;
        let content: Vec<Value> = params
            .input
            .iter()
            .filter_map(|block| serde_json::to_value(block).ok())
            .collect();
        let (content, warnings) = self.server.describe_unified_images(&unified, content)?;
        let (content, display) = self.server.prepare_unified_input(
            &unified,
            content,
            params.user_display_content.clone(),
            params.inject_invoked_skill,
        )?;
        if content.is_empty() {
            return Err(ProtocolFault::invalid_params(
                "Unified Harness turn/steer currently supports text input only",
            ));
        }
        let publication = {
            let mut core = unified.core();
            let active = core
                .projector
                .state
                .latest_turn
                .as_ref()
                .is_some_and(|turn| {
                    turn.id == params.expected_turn_id
                        && turn.status == vibe_harness::state::TurnStatus::InProgress
                });
            if !active {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "No active turn",
                ));
            }
            let advance = core.steer(
                &params.expected_turn_id,
                &TurnRequest {
                    injected: false,
                    content,
                    client_message_id: params.client_user_message_id.clone(),
                    user_display_content: display,
                },
            );
            core.publication(advance, None)
        };
        let mut outbound = warnings;
        outbound.extend(unified.publish(&publication));
        let last_event_id = unified.host().translator.event_id;
        outbound.push(success_bytes(
            request.id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    /// Reference `interrupt_turn` on a unified session.
    pub(in crate::server) fn unified_turn_interrupt(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TurnParams>(&request.params)?;
        let host = unified.host();
        host.require_turn(
            &params.expected_turn_id,
            "Active turn does not match expectedTurnId",
        )?;
        if let Some(interrupt) = &host.interrupt {
            interrupt.send_replace(Some("client interrupt".to_owned()));
        }
        let last_event_id = host.translator.event_id;
        Ok(success_batch(
            request.id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ))
    }

    /// Reference `_read_background_process_output` and
    /// `_stop_background_process`, on the named root's processes.
    pub(in crate::server) fn unified_background_process(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let processes = |session_id: &str| {
            self.server
                .unified_session(session_id)
                .map(|session| Arc::clone(&session.core().processes))
                .ok_or_else(|| ProtocolFault::plain(ProtocolErrorCode::NotFound, "Unknown session"))
        };
        let answered = if request.method == "session/backgroundProcess/stop" {
            let params = from_params::<ProcessStopParams>(&request.params)?;
            processes(&params.session_id)?.stop_raw(&params.process_id)
        } else {
            let params = from_params::<ProcessOutputParams>(&request.params)?;
            processes(&params.session_id)?.read_raw(
                &params.process_id,
                params.from_end,
                params.cursor,
                params.wait_ms,
                usize::try_from(params.max_bytes).unwrap_or(usize::MAX),
            )
        };
        match answered {
            Ok(Value::Object(result)) => Ok(success_batch(
                request.id.clone(),
                result.into_iter().collect(),
            )),
            Ok(_) => Err(ProtocolFault::internal("no result")),
            Err((code, message)) => Err(ProtocolFault::plain(
                match code.as_str() {
                    "not_found" => ProtocolErrorCode::NotFound,
                    "invalid_params" => ProtocolErrorCode::InvalidParams,
                    _ => ProtocolErrorCode::InternalError,
                },
                message,
            )),
        }
    }
}

/// Who starts a turn.
pub(crate) enum TurnOrigin {
    /// A client's `turn/start`, which bumps the session.
    Client,
    /// A scheduled loop that fired.
    Scheduled { loop_id: String, fired_at: u64 },
    /// The queue, with the context entries the turn's user entry follows.
    Queued {
        item_id: String,
        context: Vec<Vec<Value>>,
    },
}

/// A turn committed and published, waiting to run.
pub(crate) struct BegunTurn {
    pub(crate) turn_id: String,
    pub(crate) session_id: String,
    pub(crate) started_at: u64,
    /// The last event before the turn's own.
    pub(crate) event_id: u64,
    pub(crate) frames: Vec<Vec<u8>>,
    pub(crate) work: UnifiedTurnWork,
}

impl AppServer {
    /// Reference `start_turn`: commits `request` as the session's next turn,
    /// publishes it, and hands back the work that runs it. A client's turn
    /// bumps the session; a scheduled loop's, named by `scheduled`, leaves a
    /// notice of its firing instead.
    pub(crate) fn begin_unified_turn(
        &self,
        unified: Arc<UnifiedSession>,
        request: TurnRequest,
        origin: TurnOrigin,
    ) -> Result<BegunTurn, ProtocolFault> {
        {
            let host = unified.host();
            host.require_idle()?;
            if !matches!(origin, TurnOrigin::Queued { .. }) && !host.queue.is_empty() {
                return Err(ProtocolFault::with_data(
                    ProtocolErrorCode::Conflict,
                    "Resume queued turns before starting a direct turn",
                    json!({"harnessCode": "turn_queue_pending"}),
                ));
            }
        }
        let pins = self.unified_pins(&unified.id);
        unified.promote();
        let (turn_id, state) = {
            let mut core = unified.core();
            core.pins.clear();
            if let Some(model) = &pins.model {
                core.pins.insert("model".to_owned(), model.clone());
            }
            if let Some(effort) = &pins.reasoning_effort {
                core.pins
                    .insert("reasoningEffort".to_owned(), effort.clone());
            }
            match &origin {
                TurnOrigin::Queued { item_id, context } => {
                    core.begin_queued_turn(&request, item_id, context)
                }
                _ => core.begin_turn(&request),
            }
        };
        let (interrupt, interrupted) = watch::channel(None);
        let event_id = {
            let mut host = unified.host();
            host.running = Some(turn_id.clone());
            host.interrupt = Some(interrupt);
            host.pins = pins;
            host.translator.event_id
        };
        let started_at = state
            .latest_turn
            .as_ref()
            .map_or_else(now_millis, |turn| turn.started_at);
        let mut frames = Vec::new();
        if matches!(origin, TurnOrigin::Client) {
            frames.extend(unified.bump());
        }
        frames.extend(unified.publish(&Publication {
            state,
            advance: Advance::Journaled,
            callbacks: Vec::new(),
            event: None,
        }));
        if let TurnOrigin::Scheduled { loop_id, fired_at } = &origin {
            let publication = {
                let mut core = unified.core();
                let notice =
                    super::loops::scheduled_loop_notice(&unified.id, &turn_id, loop_id, *fired_at);
                let advance = core.projector.append_entries(vec![notice], now_millis());
                core.publication(advance, None)
            };
            frames.extend(unified.publish(&publication));
        }
        let driver = lock(&unified.driver).clone();
        let environment = self.unified_environment(
            &unified,
            driver.as_ref().and_then(|driver| driver.launch.as_ref()),
        )?;
        let delivery = driver.map_or_else(
            || CallbackDelivery {
                server: self.clone(),
                kinds: Vec::new(),
            },
            |driver| driver.delivery,
        );
        Ok(BegunTurn {
            session_id: unified.id.clone(),
            turn_id: turn_id.clone(),
            started_at,
            event_id,
            frames,
            work: UnifiedTurnWork {
                session: unified,
                turn_id,
                environment: Box::new(environment),
                interrupted,
                delivery,
                compaction: None,
            },
        })
    }
}

impl AppServer {
    /// Hands a settled callback's answer to the call waiting on it, and
    /// interrupts the turn when the answer asked to.
    pub(crate) fn release_unified_callback(
        &self,
        session_id: &str,
        callback_id: &str,
        cancel_turn: bool,
    ) {
        let Some(unified) = self.unified_session(session_id) else {
            self.release_child_callback(session_id, callback_id, cancel_turn);
            return;
        };
        unified.core().callbacks.release(callback_id);
        if cancel_turn && let Some(interrupt) = &unified.host().interrupt {
            interrupt.send_replace(Some("approval cancel_turn".to_owned()));
        }
    }
}

impl AppServer {
    /// Whether `id` names a subagent of the session `root` runs.
    pub(crate) fn unified_child_of(&self, root: &str, id: &str) -> bool {
        self.unified_session(root)
            .map(|session| Arc::clone(&session.core().subagents))
            .is_some_and(|manager| manager.child_core(id).is_some())
    }

    /// [`Self::release_unified_callback`] for a callback a subagent opened.
    fn release_child_callback(&self, child_id: &str, callback_id: &str, cancel_turn: bool) {
        let managers: Vec<_> = lock(&self.unified)
            .values()
            .map(|session| Arc::clone(&session.core().subagents))
            .collect();
        for manager in managers {
            if let Some(core) = manager.child_core(child_id) {
                lock(&core).callbacks.release(callback_id);
                if cancel_turn {
                    manager.interrupt_child(child_id, "approval cancel_turn");
                }
                return;
            }
        }
    }
}
