//! Prompt turns: reservation, streaming, callbacks, and settlement.
//!
//! Every failure after a turn is reserved funnels through `fail_turn`, so the
//! canonical turn is settled in exactly one place.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::Receiver;
use vibe_app_server::client::{
    ClientError, EventObserver, ProgrammaticTurn, ProgrammaticUpdate, PublicCallbackState,
    PublicHistoryEntry, PublicTurnOutcome, TurnDriver, TurnRequest, TurnReservation,
};

use crate::agent::AcpAgent;
use crate::commands::{self, builtin_command};
use crate::history::all_history_entries;
use crate::protocol::{AcpError, AcpPromptResponse, AcpSessionUpdate, AcpUsage};
use crate::session::{AcpHarness, ActivePhase};
use crate::updates::{
    AcpUpdateProjection, MAX_ACP_UPDATE_QUEUE, UpdateSender, acp_stop_reason, send_acp_updates,
    send_usage, turn_request,
};

/// Interval at which the canonical callback queue is drained while a turn runs.
const CALLBACK_POLL: Duration = Duration::from_millis(5);

struct ReservedTurn<D>
where
    D: TurnDriver,
{
    reservation: TurnReservation,
    driver: Arc<D>,
    observer: Arc<dyn EventObserver>,
    updates: Receiver<ProgrammaticUpdate>,
    /// History length before the prompt, when the session has a store.
    history_before: Option<usize>,
    user_message_id: String,
    prompt: String,
}

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    /// Runs a prompt, handing every session update to `emit` as it is produced.
    /// A failing `emit` cancels the turn and is reported once the turn settles.
    pub async fn prompt_content(
        &self,
        session_id: &str,
        content: Vec<Value>,
        mut emit: impl FnMut(&AcpSessionUpdate) -> Result<(), AcpError>,
    ) -> Result<AcpPromptResponse, AcpError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ACP_UPDATE_QUEUE);
        let prompt = self.prompt_content_streaming(session_id, content, sender);
        tokio::pin!(prompt);
        let mut emit_error = None;
        let mut drained = false;
        let response = loop {
            if drained {
                break prompt.as_mut().await;
            }
            tokio::select! {
                response = prompt.as_mut() => break response,
                update = receiver.recv() => match update {
                    None => drained = true,
                    Some(update) => {
                        if emit_error.is_none()
                            && let Err(error) = emit(&update)
                        {
                            emit_error = Some(error);
                            let _ = self.cancel(session_id).await;
                        }
                    }
                },
            }
        };
        while let Ok(update) = receiver.try_recv() {
            if emit_error.is_none()
                && let Err(error) = emit(&update)
            {
                emit_error = Some(error);
            }
        }
        emit_error.map_or(response, Err)
    }

    pub async fn prompt_streaming(
        &self,
        session_id: &str,
        prompt: &str,
        sender: UpdateSender,
    ) -> Result<AcpPromptResponse, AcpError> {
        self.prompt_content_streaming(
            session_id,
            vec![json!({"type": "text", "text": prompt})],
            sender,
        )
        .await
    }

    pub async fn prompt_content_streaming(
        &self,
        session_id: &str,
        content: Vec<Value>,
        sender: UpdateSender,
    ) -> Result<AcpPromptResponse, AcpError> {
        self.require_initialized()?;
        let harness = self.session_harness(session_id)?;
        if let Some(command) = builtin_command(&content, self.vibe_code_enabled()) {
            return commands::execute(self, &harness, &sender, &command).await;
        }
        let request = turn_request(content)?;
        harness.begin(ActivePhase::Reserving)?;
        let response = self.run_turn(&harness, request, &sender).await;
        harness.release()?;
        response
    }

    async fn run_turn(
        &self,
        harness: &Arc<AcpHarness<D>>,
        request: TurnRequest,
        sender: &UpdateSender,
    ) -> Result<AcpPromptResponse, AcpError> {
        let mut reserved = self.reserve_turn(harness, request).await?;
        harness.set_phase(ActivePhase::Running(reserved.reservation.turn_id.clone()))?;
        // A cancel that arrived while reserving has no turn to interrupt yet,
        // so it is applied as soon as the turn exists.
        if harness.take_cancel_request()
            && let Err(error) = reserved
                .driver
                .interrupt(&harness.session_id, &reserved.reservation.turn_id)
        {
            let error = AcpError::Driver(error.to_string());
            return Err(self.fail_turn(harness, &reserved, error).await);
        }
        let turn = match self.complete_turn(harness, &mut reserved, sender).await {
            Ok(turn) => turn,
            Err(error) => return Err(self.fail_turn(harness, &reserved, error).await),
        };
        send_usage(&harness.session_id, &turn, sender)?;
        Ok(AcpPromptResponse {
            stop_reason: acp_stop_reason(&turn).to_owned(),
            usage: AcpUsage {
                total_tokens: turn
                    .usage
                    .input_tokens
                    .saturating_add(turn.usage.output_tokens),
                input_tokens: turn.usage.input_tokens,
                output_tokens: turn.usage.output_tokens,
            },
        })
    }

    async fn reserve_turn(
        &self,
        harness: &Arc<AcpHarness<D>>,
        mut request: TurnRequest,
    ) -> Result<ReservedTurn<D>, AcpError> {
        let session_id = harness.session_id.clone();
        let mut service = harness.service.lock().await;
        // Sessions without a store cannot anchor forks on a history index, so
        // they fall back to an ephemeral message ID.
        let history_before = match all_history_entries(&mut service, &session_id) {
            Ok(history) => Some(history.len()),
            Err(AcpError::Client(ClientError::Protocol(
                vibe_protocol::ProtocolErrorCode::NotFound,
                _,
            ))) => None,
            Err(error) => return Err(error),
        };
        let user_message_id = history_before.map_or_else(
            || harness.next_ephemeral_user_message_id(),
            |index| format!("history:{index}:user"),
        );
        request.client_user_message_id = Some(user_message_id.clone());
        let reservation = service.reserve_prompt(&session_id, &request).await?;
        let driver = service.driver();
        let (observer, updates) = match service.interactive_update_channel_after(
            &reservation.session_id,
            &reservation.turn_id,
            0,
        ) {
            Ok(channel) => channel,
            Err(error) => {
                let _ = service.fail_reserved(&reservation, &error.to_string());
                return Err(error.into());
            }
        };
        Ok(ReservedTurn {
            reservation,
            driver,
            observer,
            updates,
            history_before,
            user_message_id,
            prompt: request.prompt,
        })
    }

    async fn complete_turn(
        &self,
        harness: &Arc<AcpHarness<D>>,
        reserved: &mut ReservedTurn<D>,
        sender: &UpdateSender,
    ) -> Result<ProgrammaticTurn, AcpError> {
        let outcome = self.stream_turn(harness, reserved, sender).await?;
        let mut service = harness.service.lock().await;
        let turn = service.finish_reserved(&reserved.reservation, outcome)?;
        if let Some(history_before) = reserved.history_before
            && let Ok(history) = all_history_entries(&mut service, &harness.session_id)
        {
            harness.record_user_message_anchor(
                reserved.user_message_id.clone(),
                history_before,
                &history,
                &reserved.prompt,
            )?;
        }
        Ok(turn)
    }

    /// Drives the turn while forwarding canonical updates and answering
    /// callbacks. The driver keeps running until it yields an outcome.
    async fn stream_turn(
        &self,
        harness: &Arc<AcpHarness<D>>,
        reserved: &mut ReservedTurn<D>,
        sender: &UpdateSender,
    ) -> Result<PublicTurnOutcome, AcpError> {
        let session_id = harness.session_id.as_str();
        let mut projection =
            AcpUpdateProjection::with_user_message_id(reserved.user_message_id.clone());
        let mut callback_poll = tokio::time::interval(CALLBACK_POLL);
        callback_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let run = reserved
            .driver
            .run_observed(&reserved.reservation, reserved.observer.clone());
        tokio::pin!(run);
        // Failing while the driver is still running interrupts it; an error the
        // driver itself returned needs no interruption.
        let interrupt = |error| {
            let _ = reserved
                .driver
                .interrupt(session_id, &reserved.reservation.turn_id);
            Err(error)
        };
        let updates = &mut reserved.updates;
        let outcome = loop {
            tokio::select! {
                biased;
                outcome = &mut run => {
                    break outcome.map_err(|error| AcpError::Driver(error.to_string()))?;
                }
                update = updates.recv() => {
                    let result = match update {
                        None => Err(AcpError::Disconnected),
                        Some(update) => {
                            send_acp_updates(session_id, update, &mut projection, sender)
                        }
                    };
                    if let Err(error) = result {
                        return interrupt(error);
                    }
                }
                _ = callback_poll.tick() => {
                    if let Err(error) = self.route_pending_callbacks(harness).await {
                        return interrupt(error);
                    }
                }
            }
        };
        while let Ok(update) = updates.try_recv() {
            send_acp_updates(session_id, update, &mut projection, sender)?;
        }
        Ok(outcome)
    }

    /// Settles a turn that cannot complete. The original error is returned
    /// unchanged so a settlement failure never masks the cause.
    async fn fail_turn(
        &self,
        harness: &Arc<AcpHarness<D>>,
        reserved: &ReservedTurn<D>,
        error: AcpError,
    ) -> AcpError {
        let _ = harness
            .service
            .lock()
            .await
            .fail_reserved(&reserved.reservation, &error.to_string());
        error
    }

    async fn route_pending_callbacks(&self, harness: &Arc<AcpHarness<D>>) -> Result<(), AcpError> {
        let callbacks = harness.service.lock().await.drain_callbacks()?;
        for callback in callbacks {
            let PublicHistoryEntry::Callback {
                metadata,
                callback_id,
                detail,
                state: PublicCallbackState::Open,
                title,
            } = callback
            else {
                return Err(AcpError::InvalidResponse(
                    "interactive callback drain returned a non-open callback".to_owned(),
                ));
            };
            if metadata.session_id != harness.session_id {
                return Err(AcpError::InvalidResponse(format!(
                    "callback `{callback_id}` crossed from session `{}` into `{}`",
                    metadata.session_id, harness.session_id
                )));
            }
            let output = match detail.get("kind").and_then(Value::as_str) {
                Some("approval") => {
                    self.resolve_approval_callback(harness, &callback_id, &title, &detail)
                        .await
                }
                // ACP v1 has no interactive question flow, so the canonical
                // request is declined instead of being surfaced.
                Some("user_input") => json!({
                    "type": "user_input",
                    "result": {
                        "answers": [],
                        "cancelled": true,
                    },
                }),
                Some(kind) => {
                    return Err(AcpError::InvalidResponse(format!(
                        "unsupported canonical callback kind `{kind}`"
                    )));
                }
                None => {
                    return Err(AcpError::InvalidResponse(format!(
                        "callback `{callback_id}` omitted its kind"
                    )));
                }
            };
            harness.service.lock().await.respond_callback(json!({
                "sessionId": harness.session_id,
                "callbackId": callback_id,
                "output": output,
            }))?;
        }
        Ok(())
    }

    async fn resolve_approval_callback(
        &self,
        harness: &Arc<AcpHarness<D>>,
        callback_id: &str,
        title: &str,
        detail: &Value,
    ) -> Value {
        let options = acp_approval_options(detail);
        if options.is_empty() {
            return cancelled_approval_output();
        }
        let permission = self.request_permission(
            &harness.session_id,
            json!({
                "toolCallId": callback_id,
                "title": title,
                "kind": "execute",
                "rawInput": detail,
                "_meta": {
                    "callbackId": callback_id,
                    "toolName": detail.pointer("/effect/toolName"),
                },
            }),
            options.clone(),
        );
        let response = tokio::select! {
            response = permission => response.ok(),
            () = harness.cancelled() => None,
        };
        approval_output_from_acp(response.as_ref(), &options)
    }
}

/// Maps the canonical approval choices onto ACP permission options, dropping
/// any choice the canonical layer did not offer.
pub(crate) fn acp_approval_options(detail: &Value) -> Vec<Value> {
    const CHOICES: [(&str, &str, &str, &str); 4] = [
        ("approve", "allow_once", "Allow once", "allow_once"),
        (
            "approve_for_session",
            "allow_always",
            "Allow for this session",
            "allow_always",
        ),
        (
            "approve_permanently",
            "allow_always_permanent",
            "Always allow",
            "allow_always",
        ),
        ("deny", "reject_once", "Reject", "reject_once"),
    ];
    let offered = detail.get("choices").and_then(Value::as_array);
    CHOICES
        .into_iter()
        .filter(|(choice, ..)| {
            offered.is_none_or(|offered| {
                offered
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|value| value == *choice)
            })
        })
        .map(
            |(_, option_id, name, kind)| json!({"optionId": option_id, "name": name, "kind": kind}),
        )
        .collect()
}

/// Anything other than an explicit selection of an offered option cancels the
/// turn, so a malformed client response can never approve work.
pub(crate) fn approval_output_from_acp(response: Option<&Value>, options: &[Value]) -> Value {
    let selected = response
        .filter(|response| {
            response.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected")
        })
        .and_then(|response| response.pointer("/outcome/optionId"))
        .and_then(Value::as_str)
        .filter(|option_id| {
            options
                .iter()
                .any(|option| option.get("optionId").and_then(Value::as_str) == Some(*option_id))
        });
    let decision = match selected {
        Some("allow_once") => "approve",
        Some("allow_always") => "approve_for_session",
        Some("allow_always_permanent") => "approve_permanently",
        Some("reject_once" | "reject_always") => "deny",
        _ => "cancel_turn",
    };
    json!({
        "type": "approval",
        "decision": {"type": decision},
    })
}

fn cancelled_approval_output() -> Value {
    json!({
        "type": "approval",
        "decision": {"type": "cancel_turn"},
    })
}
