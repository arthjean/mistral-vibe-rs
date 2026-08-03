//! The lifecycle of one agent turn: reserving it, streaming its updates,
//! interrupting it, and settling it.
//!
//! Everything that owns an [`ActiveTurn`] lives here, so the rules about which
//! cancellation phase may still accept a driver result are stated once.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use vibe_app_server::client::{
    DriverError, HeadlessService, InterruptOutcome, LiveTurnDriver, ProgrammaticUpdate,
    PublicTurnOutcome, TurnDriver, TurnReservation,
};

use super::callback::{
    cancel_open_callback_notices, fail_open_callback_notices, sync_active_callbacks,
};
use super::chat_input::ChatInputState;
use super::controls::ControlState;
use super::state::{
    ApplyResult, EntryStatus, ServerEvent, TranscriptEntry, TranscriptKind, TuiState,
};
use super::{
    CliError, InteractiveRuntime, apply_narrator_effect, attention, diagnostics, feedback,
    history_entry, notify_attention, push_local_notice, resync_current_projection, unix_millis,
    unix_seconds,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancellationPhase {
    Active,
    DriverOnly,
    Complete,
}

pub(super) struct ActiveTurn {
    pub(super) turn_id: String,
    scheduled_loop_id: Option<String>,
    pub(super) cancellation: CancellationPhase,
    updates: tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
    task: JoinHandle<(TurnReservation, Result<PublicTurnOutcome, DriverError>)>,
}

impl ActiveTurn {
    /// Builds a turn around a driver task the test drives itself, bypassing the
    /// service so a test can pin one exact driver outcome.
    #[cfg(test)]
    pub(super) fn for_test(
        turn_id: String,
        cancellation: CancellationPhase,
        updates: tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
        task: JoinHandle<(TurnReservation, Result<PublicTurnOutcome, DriverError>)>,
    ) -> Self {
        Self {
            turn_id,
            scheduled_loop_id: None,
            cancellation,
            updates,
            task,
        }
    }

    #[cfg(test)]
    pub(super) fn driver_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Waits out the driver task on the way out, then aborts it. The session is
    /// already closing, so an unresponsive driver must not hold the terminal.
    pub(super) async fn join_before_exit(mut self, grace: Duration) {
        if tokio::time::timeout(grace, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

pub(super) fn start_active_turn(
    service: &HeadlessService<LiveTurnDriver>,
    reservation: TurnReservation,
    scheduled_loop_id: Option<String>,
    event_id: u64,
    controls: &mut ControlState,
) -> Result<ActiveTurn, Box<(TurnReservation, CliError)>> {
    if let Err(error) = controls.begin_turn(&reservation.turn_id) {
        return Err(Box::new((
            reservation,
            CliError::Terminal(error.to_string()),
        )));
    }
    let (observer, updates) = match service.interactive_update_channel_after(
        &reservation.session_id,
        &reservation.turn_id,
        event_id,
    ) {
        Ok(channel) => channel,
        Err(error) => {
            let turn_id = reservation.turn_id.clone();
            controls.complete_turn(&turn_id, "Reserved turn failed before local execution");
            return Err(Box::new((reservation, error.into())));
        }
    };
    let driver = service.driver();
    let turn_id = reservation.turn_id.clone();
    let task = tokio::spawn(async move {
        let outcome = driver.run_observed(&reservation, observer).await;
        (reservation, outcome)
    });
    Ok(ActiveTurn {
        turn_id,
        scheduled_loop_id,
        cancellation: CancellationPhase::Active,
        updates,
        task,
    })
}

pub(super) fn settle_unstarted_reservation(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    reservation: &TurnReservation,
    failure: &str,
) -> bool {
    match runtime.service.fail_reserved(reservation, failure) {
        Ok(()) => {
            state.push_diagnostic(failure);
            true
        }
        Err(server_error) => match runtime
            .service
            .interrupt(&reservation.session_id, &reservation.turn_id)
        {
            Ok(InterruptOutcome::Complete) => {
                state.push_diagnostic(format!(
                    "{failure}; failure settlement failed ({server_error}), so the reserved turn \
                     was interrupted"
                ));
                true
            }
            Ok(InterruptOutcome::DriverOnly { canonical_error }) => {
                state.push_diagnostic(format!(
                    "{failure}; failure settlement failed: {server_error}; the driver stopped but \
                     canonical interruption failed: {canonical_error}"
                ));
                false
            }
            Err(interrupt_error) => {
                state.push_diagnostic(format!(
                    "{failure}; failure settlement failed: {server_error}; interruption fallback \
                     failed: {interrupt_error}"
                ));
                false
            }
        },
    }
}

pub(super) fn request_active_turn_interrupt(
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    controls: &mut ControlState,
    state: &mut TuiState,
) -> bool {
    let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_mut()) else {
        return false;
    };
    if active.cancellation == CancellationPhase::Active {
        let interrupt = match runtime
            .service
            .interrupt(&runtime.session_id, &active.turn_id)
        {
            Ok(interrupt) => interrupt,
            Err(error) => {
                state.push_diagnostic(format!("Turn cancellation was rejected: {error}"));
                resync_current_projection(runtime, state);
                sync_active_callbacks(runtime, state, controls);
                return true;
            }
        };
        let _ = controls.interrupt();
        cancel_open_callback_notices(state);
        let diagnostic = match interrupt {
            InterruptOutcome::Complete => {
                active.cancellation = CancellationPhase::Complete;
                "Turn cancellation requested; queued prompts paused".to_owned()
            }
            InterruptOutcome::DriverOnly { canonical_error } => {
                active.cancellation = CancellationPhase::DriverOnly;
                format!(
                    "The driver is cancelling, but canonical interruption must be retried: \
                     {canonical_error}"
                )
            }
        };
        state.prompt_queue.pause();
        push_local_notice(state, "Interrupted", EntryStatus::Cancelled);
        state.push_diagnostic(diagnostic);
    }
    true
}

pub(super) fn drain_updates(
    state: &mut TuiState,
    runtime: Option<&mut InteractiveRuntime>,
    active: Option<&mut ActiveTurn>,
    controls: &mut ControlState,
) {
    let Some(active) = active else {
        return;
    };
    let mut live_context_tokens = None;
    let resync = drain_update_receiver(
        state,
        &active.turn_id,
        &mut active.updates,
        &mut live_context_tokens,
    );
    if let Some(runtime) = runtime {
        if let Some(context_tokens) = live_context_tokens {
            runtime.context_tokens = context_tokens;
        }
        if resync {
            resync_current_projection(runtime, state);
            sync_active_callbacks(runtime, state, controls);
        }
    } else if resync {
        state.push_diagnostic("Canonical resync is unavailable until setup completes");
    }
}

pub(super) fn drain_update_receiver(
    state: &mut TuiState,
    turn_id: &str,
    updates: &mut tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
    live_context_tokens: &mut Option<u64>,
) -> bool {
    while let Ok(update) = updates.try_recv() {
        let event = match update {
            ProgrammaticUpdate::Stats { context_tokens, .. } => {
                *live_context_tokens = Some(context_tokens);
                continue;
            }
            ProgrammaticUpdate::HistoryEntry {
                event_id, entry, ..
            } => {
                let mut transcript = history_entry(*entry);
                transcript.id = format!("{turn_id}:{}", transcript.id);
                let previous_text = state
                    .entries
                    .iter()
                    .find(|current| current.id == transcript.id)
                    .map(|current| current.text.clone());
                track_narrated_entry(state, &transcript, previous_text.as_deref());
                let existing = state
                    .entries
                    .iter()
                    .find(|current| current.id == transcript.id);
                if let Some(current) = existing {
                    transcript.revision = current.revision.saturating_add(1);
                    ServerEvent::EntryUpdated {
                        event_id,
                        entry: transcript,
                    }
                } else {
                    ServerEvent::EntryAdded {
                        event_id,
                        entry: transcript,
                    }
                }
            }
            ProgrammaticUpdate::Watermark { event_id, .. } => ServerEvent::Watermark { event_id },
        };
        match state.apply(event) {
            Ok(ApplyResult::ResyncRequired) => {
                state.push_diagnostic(
                    "Live update continuity was lost; reloading canonical session state",
                );
                return true;
            }
            Ok(ApplyResult::Applied | ApplyResult::Duplicate) => {}
            Err(error) => state.push_diagnostic(error.to_string()),
        }
    }
    false
}

/// Reference `_track_narrator_event`: the narrator follows the canonical stream,
/// and a streamed assistant entry contributes only its new text.
fn track_narrated_entry(
    state: &mut TuiState,
    entry: &TranscriptEntry,
    previous_text: Option<&str>,
) {
    match entry.kind {
        TranscriptKind::UserMessage if previous_text.is_none() => {
            state.narrator.on_user_message(&entry.id);
        }
        TranscriptKind::AssistantMessage => {
            let delta = match previous_text {
                Some(previous) if entry.text.starts_with(previous) => &entry.text[previous.len()..],
                _ => entry.text.as_str(),
            };
            state.narrator.on_assistant_text(delta);
        }
        _ => {}
    }
}

pub(super) async fn finish_active(
    state: &mut TuiState,
    controls: &mut ControlState,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    input: &mut ChatInputState,
) -> Result<(), CliError> {
    if !active
        .as_ref()
        .is_some_and(|active| active.task.is_finished())
    {
        return Ok(());
    }
    let Some(active_turn) = active.take() else {
        return Ok(());
    };
    let ActiveTurn {
        turn_id,
        scheduled_loop_id,
        cancellation,
        mut updates,
        task,
    } = active_turn;
    let (reservation, outcome) = task
        .await
        .map_err(|error| CliError::Terminal(format!("turn task failed: {error}")))?;
    let mut live_context_tokens = None;
    let _ = drain_update_receiver(state, &turn_id, &mut updates, &mut live_context_tokens);
    let runtime = runtime
        .as_mut()
        .ok_or_else(|| CliError::Terminal("interactive runtime disappeared".to_owned()))?;
    let turn_completed = if cancellation != CancellationPhase::Active {
        cancel_open_callback_notices(state);
        settle_cancelled_reservation(runtime, state, &reservation, cancellation)?;
        controls.complete_turn(&turn_id, "Turn cancelled");
        state.waiting = false;
        // Reference `_complete_unsolicited_turn`: a cancelled turn is not narrated.
        state.narrator.on_turn_cancel();
        false
    } else {
        match outcome {
            Ok(outcome) => {
                fail_open_callback_notices(state);
                runtime.context_tokens = outcome.context_tokens;
                runtime.service.finish_reserved(&reservation, outcome)?;
                controls.complete_turn(&turn_id, "Turn complete");
                state.waiting = false;
                true
            }
            Err(error) => {
                fail_open_callback_notices(state);
                runtime
                    .service
                    .fail_reserved(&reservation, &error.to_string())?;
                controls.complete_turn(&turn_id, "Turn failed");
                state.waiting = false;
                state.narrator.on_turn_error(&error.to_string());
                report_turn_failure(state, &error);
                false
            }
        }
    };
    // Reference `_finalize_turn_ui`: narration settles and attention is
    // requested for every turn outcome.
    if let Some(effect) = state.narrator.on_turn_end() {
        apply_narrator_effect(effect, runtime, state);
    }
    notify_attention(
        state,
        attention::NotificationContext::Complete,
        unix_millis(),
    );
    resync_current_projection(runtime, state);
    sync_active_callbacks(runtime, state, controls);
    if let Some(loop_id) = scheduled_loop_id {
        runtime
            .service
            .finish_scheduled_loop(&loop_id, unix_seconds())?;
    }
    if runtime.clear_context_after_turn {
        runtime.clear_context_after_turn = false;
        match runtime.service.public_call(
            "session/history/clear",
            json!({"sessionId": runtime.session_id}),
        ) {
            Ok(_) => {
                runtime.context_tokens = 0;
                resync_current_projection(runtime, state);
                sync_active_callbacks(runtime, state, controls);
                push_local_notice(
                    state,
                    "Planning context cleared after switching to code mode",
                    EntryStatus::Completed,
                );
            }
            Err(error) => state.push_diagnostic(format!(
                "Code mode is active, but planning context could not be cleared: {error}"
            )),
        }
    }
    if turn_completed {
        feedback::maybe_activate(runtime, input, state).await;
    }
    Ok(())
}

/// Renders a failed turn the way the reference does: one classified message in
/// the transcript, never the raw driver payload, and never twice in a row.
pub(super) fn report_turn_failure(state: &mut TuiState, error: &DriverError) {
    let classified = diagnostics::classify(
        diagnostics::driver_error_code(error),
        &error.to_string(),
        &Value::Null,
        false,
    );
    if !state.errors.record(&classified) {
        return;
    }
    let status = match classified.severity {
        diagnostics::Severity::Notice => EntryStatus::Cancelled,
        diagnostics::Severity::Warning | diagnostics::Severity::Error => EntryStatus::Failed,
    };
    let level = match classified.severity {
        diagnostics::Severity::Notice => "info",
        diagnostics::Severity::Warning => "warning",
        diagnostics::Severity::Error => "error",
    };
    let entry = TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: classified.message,
        status,
        details: json!({
            "type": "notice",
            "level": level,
            "detail": {"kind": "turn_failed", "class": classified.class.label()},
        }),
    };
    state.append_local(entry);
}

fn settle_cancelled_reservation(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    reservation: &TurnReservation,
    cancellation: CancellationPhase,
) -> Result<(), CliError> {
    match cancellation {
        CancellationPhase::Complete => return Ok(()),
        CancellationPhase::DriverOnly => {}
        CancellationPhase::Active => {
            debug_assert!(false, "active turn cannot require cancellation settlement");
            return Ok(());
        }
    }
    match runtime
        .service
        .interrupt(&reservation.session_id, &reservation.turn_id)
    {
        Ok(InterruptOutcome::Complete) => Ok(()),
        Ok(InterruptOutcome::DriverOnly { canonical_error }) => {
            state.push_diagnostic(format!(
                "Canonical cancellation retry failed: {canonical_error}"
            ));
            runtime
                .service
                .fail_reserved(reservation, "turn cancelled before canonical settlement")?;
            Ok(())
        }
        Err(interrupt_error) => {
            state.push_diagnostic(format!(
                "Cancellation retry was rejected: {interrupt_error}"
            ));
            runtime
                .service
                .fail_reserved(reservation, "turn cancellation could not reach the driver")?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use vibe_app_server::client::{PublicTurnOutcome, TurnRequest};

    use super::super::hydration::canonical_session_projection;
    use super::super::runtime::interactive_test_runtime;
    use super::*;

    #[tokio::test]
    async fn usage_reaches_the_context_gauge_before_the_turn_settles() {
        let (sender, mut updates) = tokio::sync::mpsc::channel(8);
        sender
            .send(ProgrammaticUpdate::Stats {
                context_tokens: 4_096,
                input_tokens: 3_000,
                output_tokens: 1_096,
            })
            .await
            .expect("usage update queues");
        sender
            .send(ProgrammaticUpdate::Watermark {
                event_id: 1,
                emitted_at: 0,
            })
            .await
            .expect("watermark queues");
        let mut state = TuiState::new("session");
        let mut live = None;
        assert!(!drain_update_receiver(
            &mut state,
            "turn",
            &mut updates,
            &mut live
        ));
        assert_eq!(live, Some(4_096));
        assert_eq!(state.watermark, 1, "usage must not consume the sequence");
    }

    #[test]
    fn a_failed_turn_reports_one_classified_message_without_the_driver_payload() {
        let mut state = TuiState::new("session");
        let refusal = DriverError::Provider(vibe_core::provider::ProviderError::Refusal(
            "internal policy token 42".to_owned(),
        ));
        report_turn_failure(&mut state, &refusal);
        report_turn_failure(&mut state, &refusal);
        assert_eq!(
            state.entries.len(),
            1,
            "one turn must not report the same failure twice"
        );
        let entry = &state.entries[0];
        assert_eq!(entry.kind, TranscriptKind::Notice);
        assert_eq!(entry.status, EntryStatus::Failed);
        assert!(entry.text.starts_with("The model declined to respond"));
        assert!(!entry.text.contains("internal policy token 42"));

        report_turn_failure(
            &mut state,
            &DriverError::Transport(vibe_core::provider::TransportError::Connection(
                "reset".to_owned(),
            )),
        );
        assert_eq!(state.entries.len(), 2, "a distinct failure stays visible");
        assert_eq!(state.entries[1].details["detail"]["class"], "transport");

        // The next turn starts from a clean slate, so a repeat is real news.
        state.waiting = true;
        state.sync_activity(0);
        report_turn_failure(&mut state, &refusal);
        assert_eq!(state.entries.len(), 3, "a later turn reports again");
    }

    #[tokio::test]
    async fn terminal_commits_refresh_the_watermark_between_consecutive_turns() {
        let mut runtime = Some(interactive_test_runtime("watermark-session"));
        let session_id = runtime.as_ref().expect("runtime").session_id.clone();
        let mut state =
            canonical_session_projection(runtime.as_mut().expect("runtime"), &session_id, false)
                .expect("initial projection");
        let mut controls = ControlState::new(&session_id);

        for prompt in ["first", "second"] {
            let reservation = runtime
                .as_mut()
                .expect("runtime")
                .service
                .reserve_prompt(&session_id, &TurnRequest::text(prompt))
                .await
                .expect("turn reserves");
            let turn_id = reservation.turn_id.clone();
            controls.begin_turn(&turn_id).expect("turn begins");
            let (updates_sender, updates) = tokio::sync::mpsc::channel(1);
            drop(updates_sender);
            let task = tokio::spawn(async move {
                (
                    reservation,
                    Err::<PublicTurnOutcome, _>(DriverError::UnsupportedControl("test turn")),
                )
            });
            let mut active = Some(ActiveTurn::for_test(
                turn_id,
                CancellationPhase::Active,
                updates,
                task,
            ));
            while !active
                .as_ref()
                .is_some_and(|active| active.driver_finished())
            {
                tokio::task::yield_now().await;
            }

            let mut input = ChatInputState::default();
            finish_active(
                &mut state,
                &mut controls,
                &mut runtime,
                &mut active,
                &mut input,
            )
            .await
            .expect("terminal commit finishes");
            let canonical_watermark = runtime
                .as_mut()
                .expect("runtime")
                .service
                .public_call("session/read", json!({"sessionId": session_id}))
                .expect("canonical projection")["state"]["eventId"]
                .as_u64()
                .expect("canonical watermark");
            assert_eq!(state.watermark, canonical_watermark);
        }

        assert!(
            state
                .diagnostics()
                .all(|diagnostic| { !diagnostic.contains("Live update continuity was lost") })
        );
    }

    #[tokio::test]
    async fn completed_cancellation_dominates_a_late_successful_driver_result() {
        let mut runtime = Some(interactive_test_runtime("cancelled-late-success"));
        let session_id = runtime.as_ref().expect("runtime").session_id.clone();
        let mut state =
            canonical_session_projection(runtime.as_mut().expect("runtime"), &session_id, false)
                .expect("initial projection");
        let mut controls = ControlState::new(&session_id);
        let reservation = runtime
            .as_mut()
            .expect("runtime")
            .service
            .reserve_prompt(&session_id, &TurnRequest::text("cancel me"))
            .await
            .expect("turn reserves");
        let turn_id = reservation.turn_id.clone();
        controls.begin_turn(&turn_id).expect("turn begins");
        let cancellation = match runtime
            .as_mut()
            .expect("runtime")
            .service
            .interrupt(&session_id, &turn_id)
            .expect("interrupt is accepted")
        {
            InterruptOutcome::Complete => CancellationPhase::Complete,
            InterruptOutcome::DriverOnly { canonical_error } => {
                panic!("canonical cancellation failed: {canonical_error}")
            }
        };
        let outcome = PublicTurnOutcome {
            session_id: session_id.clone(),
            events: Vec::new(),
            snapshot: vibe_core::events::ProjectionReducer::for_turn(&session_id, &turn_id)
                .state()
                .clone(),
            messages: Vec::new(),
            usage: vibe_core::provider::Usage::default(),
            context_tokens: 0,
            price_micros: 0,
            steps: 0,
            checkpoints: 0,
            stop_reason: vibe_core::engine::TurnStopReason::Complete,
        };
        let (updates_sender, updates) = tokio::sync::mpsc::channel(1);
        drop(updates_sender);
        let task = tokio::spawn(async move { (reservation, Ok(outcome)) });
        let mut active = Some(ActiveTurn::for_test(turn_id, cancellation, updates, task));
        while !active
            .as_ref()
            .is_some_and(|active| active.driver_finished())
        {
            tokio::task::yield_now().await;
        }

        finish_active(
            &mut state,
            &mut controls,
            &mut runtime,
            &mut active,
            &mut ChatInputState::default(),
        )
        .await
        .expect("cancelled turn ignores late success");

        assert_eq!(
            controls.notifications.last().map(String::as_str),
            Some("Turn cancelled")
        );
        let canonical = runtime
            .as_mut()
            .expect("runtime")
            .service
            .public_call("session/read", json!({"sessionId": session_id}))
            .expect("canonical session remains readable");
        assert_eq!(
            canonical["state"]
                .pointer("/session/status/type")
                .and_then(Value::as_str),
            Some("idle")
        );
        assert_eq!(
            canonical["state"]
                .pointer("/latestTurn/status")
                .and_then(Value::as_str),
            Some("interrupted")
        );
    }
}
