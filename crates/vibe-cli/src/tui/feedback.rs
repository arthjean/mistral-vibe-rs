//! Feedback prompt persistence and rating telemetry.

use serde_json::{Value, json};

use super::InteractiveRuntime;
use super::chat_input::{ChatInputState, InputEffect, InputEvent};
use super::state::TuiState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedbackAction {
    Rating(u8),
    Snooze,
    Dismiss,
}

pub(super) async fn maybe_activate(
    runtime: &mut InteractiveRuntime,
    input: &mut ChatInputState,
    state: &mut TuiState,
) {
    let should_show = match runtime
        .service
        .public_call_async(
            "feedback/shouldShow",
            json!({"sessionId": runtime.session_id}),
        )
        .await
    {
        Ok(dispatch) => dispatch
            .result
            .get("show")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        Err(error) => {
            state.push_diagnostic(format!(
                "Feedback availability could not be checked: {error}"
            ));
            return;
        }
    };
    if !should_show {
        return;
    }

    if let Err(error) = runtime
        .service
        .public_call_async(
            "feedback/record",
            json!({"sessionId": runtime.session_id, "action": "asked"}),
        )
        .await
    {
        state.push_diagnostic(format!("Feedback prompt could not be recorded: {error}"));
    }
    let _ = input.apply(InputEvent::Feedback { active: true });
}

pub(super) async fn handle_effects(
    effects: &[InputEffect],
    runtime: &mut Option<InteractiveRuntime>,
    input: &mut ChatInputState,
    state: &mut TuiState,
) {
    let Some(action) = feedback_action(effects) else {
        return;
    };
    let _ = input.apply(InputEvent::Feedback { active: false });
    if action == FeedbackAction::Dismiss {
        return;
    }

    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Feedback could not be saved: interactive runtime is unavailable");
        return;
    };
    let persistence_action = match action {
        FeedbackAction::Rating(_) => "given",
        FeedbackAction::Snooze => "snoozed",
        FeedbackAction::Dismiss => return,
    };
    if let Err(error) = runtime
        .service
        .public_call_async(
            "feedback/record",
            json!({"sessionId": runtime.session_id, "action": persistence_action}),
        )
        .await
    {
        state.push_diagnostic(format!("Feedback could not be saved: {error}"));
        return;
    }
    if let FeedbackAction::Rating(rating) = action
        && let Some(telemetry) = runtime.telemetry.as_ref()
        && let Err(error) = telemetry.enqueue_feedback(rating, &runtime.model)
    {
        state.push_diagnostic(format!("Feedback telemetry could not be queued: {error}"));
    }
}

fn feedback_action(effects: &[InputEffect]) -> Option<FeedbackAction> {
    effects.iter().find_map(|effect| match effect {
        InputEffect::FeedbackRating { rating } => Some(FeedbackAction::Rating(*rating)),
        InputEffect::FeedbackSnooze => Some(FeedbackAction::Snooze),
        InputEffect::FeedbackDismissed => Some(FeedbackAction::Dismiss),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_is_preserved_until_the_telemetry_boundary() {
        assert_eq!(
            feedback_action(&[InputEffect::FeedbackRating { rating: 3 }]),
            Some(FeedbackAction::Rating(3))
        );
    }
}
