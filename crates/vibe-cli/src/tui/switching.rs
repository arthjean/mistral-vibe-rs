//! Deferred model and agent switching with an observable busy frame.

use serde_json::json;

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
                "session/settings/update",
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
                    "session/settings/update",
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
