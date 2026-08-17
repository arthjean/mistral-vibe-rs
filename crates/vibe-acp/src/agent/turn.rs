//! Prompt turns: reservation, streaming, and settlement.
//!
//! Callbacks raised while a turn runs are answered in [`callbacks`].
//!
//! Every failure after a turn is reserved funnels through `fail_turn`, so the
//! canonical turn is settled in exactly one place.

pub(crate) mod callbacks;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::Receiver;
use vibe_app_server::client::{
    ClientError, EventObserver, HeadlessService, ProgrammaticTurn, ProgrammaticUpdate,
    PublicTurnOutcome, TurnDriver, TurnErrorCode, TurnRequest, TurnReservation,
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

/// What a usage update reports as the context window when the canonical
/// session does not publish one, which happens only if `stats/read` fails.
const FALLBACK_CONTEXT_WINDOW: u64 = 200_000;

/// A settled turn, with the context window its usage update is measured
/// against. The window is read while the turn's own service lock is already
/// held, so reporting it costs no extra round of contention.
struct SettledTurn {
    turn: ProgrammaticTurn,
    context_window: u64,
}

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
        if harness.is_cancelled()
            && let Err(error) = reserved
                .driver
                .interrupt(&harness.session_id, &reserved.reservation.turn_id)
        {
            let error = AcpError::Driver(error.to_string());
            return Err(self.fail_turn(harness, &reserved, error).await);
        }
        let settled = match self.complete_turn(harness, &mut reserved, sender).await {
            Ok(settled) => settled,
            Err(error) => return Err(self.fail_turn(harness, &reserved, error).await),
        };
        let SettledTurn {
            turn,
            context_window,
        } = settled;
        send_usage(&harness.session_id, &turn, context_window, sender)?;
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
                // The turn never reached the driver: the update channel this
                // bridge needs is what failed.
                let _ = service.fail_reserved(
                    &reservation,
                    &error.to_string(),
                    TurnErrorCode::InternalError,
                );
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
    ) -> Result<SettledTurn, AcpError> {
        let outcome = self.stream_turn(harness, reserved, sender).await?;
        let mut service = harness.service.lock().await;
        let turn = service.finish_reserved(&reserved.reservation, outcome)?;
        let context_window = session_context_window(&mut service, &harness.session_id);
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
        Ok(SettledTurn {
            turn,
            context_window,
        })
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
    ///
    /// The code is coarser here than on the app-server side: this bridge sees
    /// the driver's failure already rendered to a string, so it can only say
    /// whether the failure came from the driver or from itself.
    async fn fail_turn(
        &self,
        harness: &Arc<AcpHarness<D>>,
        reserved: &ReservedTurn<D>,
        error: AcpError,
    ) -> AcpError {
        let _ = harness.service.lock().await.fail_reserved(
            &reserved.reservation,
            &error.to_string(),
            acp_error_code(&error),
        );
        error
    }
}

/// The context window the canonical session measures its usage against, which
/// is what the reference reports as the usage update's `size`. A session that
/// cannot answer falls back to the shipped default rather than reporting a
/// window of zero, which an editor would render as a full context.
fn session_context_window<D>(service: &mut HeadlessService<D>, session_id: &str) -> u64
where
    D: TurnDriver,
{
    service
        .public_call("stats/read", json!({"sessionId": session_id}))
        .ok()
        .and_then(|result| result.get("contextWindow")?.as_u64())
        .filter(|window| *window > 0)
        .unwrap_or(FALLBACK_CONTEXT_WINDOW)
}

/// Classifies a bridge-side failure into the vocabulary a turn error is drawn
/// from.
///
/// The driver's own failures arrive already rendered to a string, so the split
/// here is coarse on purpose: what the driver reported is the backend failing,
/// and anything else is this bridge failing.
fn acp_error_code(error: &AcpError) -> TurnErrorCode {
    match error {
        AcpError::Driver(_) => TurnErrorCode::BackendError,
        _ => TurnErrorCode::InternalError,
    }
}
