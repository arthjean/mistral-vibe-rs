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
        && let Err(error) = telemetry.enqueue_feedback(rating, &runtime.model, &runtime.session_id)
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
    use super::super::chat_input::{self, InputEvent};
    use super::super::runtime::interactive_test_runtime;
    use super::super::state::TuiState;
    use super::*;

    #[test]
    fn rating_is_preserved_until_the_telemetry_boundary() {
        assert_eq!(
            feedback_action(&[InputEffect::FeedbackRating { rating: 3 }]),
            Some(FeedbackAction::Rating(3))
        );
    }

    #[tokio::test]
    async fn feedback_activation_and_response_use_the_existing_resource_boundary() {
        let mut runtime = interactive_test_runtime("feedback-session");
        let mut input = ChatInputState::default();
        let mut state = TuiState::new("feedback-session");

        maybe_activate(&mut runtime, &mut input, &mut state).await;
        assert!(
            input.feedback_active(),
            "feedback diagnostics: {:?}",
            state.diagnostics().collect::<Vec<_>>()
        );
        let effects = input.apply(InputEvent::Key {
            key: chat_input::KeyName::Char,
            char: Some('2'),
            mods: Vec::new(),
        });
        let mut runtime = Some(runtime);
        handle_effects(&effects, &mut runtime, &mut input, &mut state).await;
        assert!(!input.feedback_active());
        assert_eq!(state.diagnostics().count(), 0);

        maybe_activate(
            runtime.as_mut().expect("runtime remains available"),
            &mut input,
            &mut state,
        )
        .await;
        assert!(!input.feedback_active(), "feedback is not asked twice");
    }

    #[tokio::test]
    async fn unavailable_feedback_persistence_exits_transient_state_once() {
        let mut runtime = None;
        let mut input = ChatInputState::default();
        let mut state = TuiState::new("feedback-session");
        let _ = input.apply(InputEvent::Feedback { active: true });

        handle_effects(
            &[InputEffect::FeedbackSnooze],
            &mut runtime,
            &mut input,
            &mut state,
        )
        .await;

        assert!(!input.feedback_active());
        assert_eq!(state.diagnostics().count(), 1);
    }
}
