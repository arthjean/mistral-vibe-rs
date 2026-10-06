//! Deferred model and agent switching with an observable busy frame.

use serde_json::{Value, json};

use super::chat_input::{ChatInputState, InputEvent};
use super::state::{EntryStatus, TuiState};
use super::{
    InteractiveRuntime, call_runtime, persist_setting, push_local_notice, sync_runtime_intent,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SwitchRequest {
    Model {
        model: String,
        target: Option<String>,
    },
    Agent(String),
}

pub(super) fn request(
    runtime: &mut InteractiveRuntime,
    composer: &mut ChatInputState,
    state: &mut TuiState,
    request: SwitchRequest,
) {
    if runtime.pending_switch.is_some() {
        state.push_diagnostic("A model or agent switch is already pending");
        return;
    }
    runtime.pending_switch = Some(request);
    let _ = composer.apply(InputEvent::Switching { active: true });
}

pub(super) fn apply_pending(
    runtime: &mut InteractiveRuntime,
    composer: &mut ChatInputState,
    state: &mut TuiState,
) {
    let Some(request) = runtime.pending_switch.take() else {
        return;
    };
    match request {
        SwitchRequest::Model { model, target } => {
            let previous = runtime.model.clone();
            if call_runtime(
                runtime,
                "session/overrides/write",
                json!({"sessionId": runtime.session_id, "model": model}),
                state,
            )
            .is_none()
            {
                let _ = composer.apply(InputEvent::Switching { active: false });
                return;
            }
            if !persist_setting(
                runtime,
                target.as_deref().unwrap_or("user"),
                &["active_model"],
                json!(model),
                false,
                state,
            ) {
                if let Err(error) = runtime.service.public_call(
                    "session/overrides/write",
                    json!({"sessionId": runtime.session_id, "model": previous}),
                ) {
                    state.push_diagnostic(format!(
                        "Model preference save failed and the active session rollback also failed: {error}"
                    ));
                }
                let _ = composer.apply(InputEvent::Switching { active: false });
                return;
            }
            runtime.model = model;
            push_local_notice(
                state,
                "Model updated for this session and future sessions",
                EntryStatus::Completed,
            );
        }
        SwitchRequest::Agent(agent) => {
            if call_runtime(
                runtime,
                "session/agent/update",
                json!({"sessionId": runtime.session_id, "name": agent}),
                state,
            )
            .is_some()
            {
                sync_runtime_intent(runtime, Some(&agent));
                push_local_notice(
                    state,
                    &format!("Switched to agent `{agent}`"),
                    EntryStatus::Completed,
                );
            }
        }
    }
    let _ = composer.apply(InputEvent::Switching { active: false });
}

/// Moves a new session onto the model its rollout routed it to, once the
/// lookup it was waiting on settles.
///
/// Reference `initialize_experiments` refreshes the configuration of a session
/// built without a cached rollout, and `_apply_config_to_ui` then shows the
/// active model it resolves: the routed default, unless the operator pinned a
/// model. Nothing is persisted, because the routing is the rollout's choice
/// rather than the operator's.
pub(super) fn sync_routed_model(runtime: &mut InteractiveRuntime) {
    let awaiting = runtime
        .experiments
        .as_ref()
        .is_some_and(|experiments| experiments.awaiting_model());
    if awaiting || !std::mem::take(&mut runtime.awaiting_model) {
        return;
    }
    let Some(view) = runtime.published_config() else {
        return;
    };
    if view
        .get("activeModelPinned")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        return;
    }
    let Some(alias) = view
        .pointer("/activeModel/alias")
        .and_then(Value::as_str)
        .filter(|alias| *alias != runtime.model)
        .map(ToOwned::to_owned)
    else {
        return;
    };
    if runtime
        .service
        .public_call(
            "session/overrides/write",
            json!({"sessionId": runtime.session_id, "model": alias}),
        )
        .is_ok()
    {
        runtime.model = alias;
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime::interactive_test_runtime;
    use super::*;

    #[test]
    fn switching_state_spans_the_scheduler_boundary() {
        let mut runtime = interactive_test_runtime("switch-session");
        let mut input = ChatInputState::default();
        let mut state = TuiState::new("switch-session");

        request(
            &mut runtime,
            &mut input,
            &mut state,
            SwitchRequest::Agent("default".to_owned()),
        );
        assert!(input.switching());
        assert_eq!(
            runtime.pending_switch,
            Some(SwitchRequest::Agent("default".to_owned()))
        );

        apply_pending(&mut runtime, &mut input, &mut state);
        assert!(!input.switching());
        assert!(runtime.pending_switch.is_none());
    }
}
