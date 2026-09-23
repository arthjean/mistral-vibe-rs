use crossterm::event::KeyEvent;
use serde_json::{Value, json};

mod config;
mod keys;
mod live_commands;
mod mcp;
mod overlay;

#[cfg(test)]
#[path = "workflow/audio_surface_tests.rs"]
mod audio_surface_tests;

#[cfg(test)]
#[path = "workflow/command_line_tests.rs"]
mod command_line_tests;

use super::chat_input::ChatInputState;
use super::controls::ControlState;
use super::debug_console::{DebugConsole, PAGE_SIZE as DEBUG_PAGE_SIZE};
use super::interaction::{
    Overlay, OverlayKind, RemoteProjectAction, TeleportPushAction, ValueEdit,
};
use super::pickers::{
    VOICE_MODEL_FIELDS, audio_model_aliases, config_overlay, model_overlay, proxy_overlay,
    rewind_targets, voice_model_overlay, voice_overlay,
};
use super::rewind::{RewindEffect, RewindState, reduce_key as reduce_rewind_key};
use super::session_picker::SessionDeleteState;
use super::state::{EntryStatus, TuiState};
use super::switching::{self, SwitchRequest};
use super::{
    InteractiveRuntime, adopt_hydrated_session, call_runtime, metadata_session_id,
    push_local_notice, unix_millis,
};
pub(in crate::tui) use config::apply_render_preferences;
use config::{
    configured_value, reset_config_value_at, selected_config_target, set_config_value,
    update_proxy_value,
};
pub(in crate::tui) use keys::handle_overlay_key;
// The form reducer runs through `handle_overlay_key` in production; its own
// tests drive it directly.
#[cfg(test)]
pub(in crate::tui) use keys::handle_remote_project_create_key;
#[cfg(test)]
pub(in crate::tui) use live_commands::scheduled_loop;
pub(in crate::tui) use live_commands::{
    FollowUp, LOGIN_POLL_ATTEMPTS, LOGIN_POLL_INTERVAL, LiveBackend, run_command,
};
pub(in crate::tui) use mcp::{McpEffect, McpPendingOperation, apply_pending_operation};
pub(super) use mcp::{SystemUrlOpener, UrlOpenerPort, execute_mcp_effect};
#[cfg(test)]
pub(in crate::tui) use mcp::{reduce_auth_action, valid_auth_url};

/// How much of the saved transcript the rewind picker lists.
///
/// The store caps a page at 500, and a rewind point past that is one the
/// operator would have to scroll a conversation of that length to reach.
const REWIND_HISTORY_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OverlayEffect {
    Mcp(McpEffect),
    RemoteProject(RemoteProjectAction),
    TeleportPush(TeleportPushAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OverlayKeyResult {
    Unhandled,
    Handled,
    Effect(OverlayEffect),
}

/// Starts typing a panel field's value: the panel closes, the composer holds
/// the current value, and the next `Enter` saves what it holds.
fn begin_value_edit(
    edit: ValueEdit,
    current: String,
    composer: &mut ChatInputState,
    state: &mut TuiState,
) {
    state.push_diagnostic(format!(
        "Editing `{}`: Enter saves, Esc cancels",
        edit.field()
    ));
    composer.replace_text(current);
    state.overlay = None;
    state.value_edit = Some(edit);
}

/// Saves the value typed for a panel field and reopens the panel it came from.
pub(in crate::tui) fn apply_value_edit(
    edit: ValueEdit,
    value: &str,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) {
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    let value = value.trim();
    match edit {
        ValueEdit::Config { target, key } => {
            set_config_value(&format!("--target {target} {key} {value}"), runtime, state);
            show_config(runtime, state);
        }
        ValueEdit::Proxy { key } => {
            let arguments = if value.is_empty() {
                key
            } else {
                match shlex::try_quote(value) {
                    Ok(quoted) => format!("{key} {quoted}"),
                    Err(_) => {
                        state.push_diagnostic("Proxy values cannot contain a NUL byte");
                        return;
                    }
                }
            };
            update_proxy_value(&arguments, runtime, state);
            show_proxy(runtime, state);
        }
    }
}

/// Drops the value being typed for a panel field and reopens its panel.
pub(in crate::tui) fn cancel_value_edit(
    edit: ValueEdit,
    runtime: &mut Option<InteractiveRuntime>,
    composer: &mut ChatInputState,
    state: &mut TuiState,
) {
    composer.replace_text("");
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    match edit {
        ValueEdit::Config { .. } => show_config(runtime, state),
        ValueEdit::Proxy { .. } => show_proxy(runtime, state),
    }
}

pub(super) fn cycle_agent(
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    composer: &mut ChatInputState,
) {
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    let Some(result) = call_runtime(
        runtime,
        "agents/list",
        json!({"sessionId": runtime.session_id}),
        state,
    ) else {
        return;
    };
    let agents = result
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|agent| {
            agent
                .get("agentType")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "agent")
        })
        .filter_map(|agent| agent.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if agents.is_empty() {
        state.push_diagnostic("No agent profiles are available");
        return;
    }
    let current = agents
        .iter()
        .position(|agent| *agent == runtime.agent_name)
        .unwrap_or_default();
    let next = agents[(current + 1) % agents.len()].to_owned();
    switching::request(runtime, composer, state, SwitchRequest::Agent(next));
}

fn sync_voice_preference(runtime: &mut InteractiveRuntime, composer: &mut ChatInputState) {
    let enabled = configured_value(runtime, "voice_mode_enabled")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    // Reference `action_toggle_voice_mode`: the event reports a change of the
    // preference, so a resynchronization that moves nothing sends nothing.
    if enabled != runtime.voice.enabled() {
        super::report_voice_mode_toggled(runtime, enabled);
    }
    runtime.voice.set_enabled(enabled);
    // Reference `LazyVoiceManager`: the audio surface is resolved from the
    // configuration as it stands, so an edited active model, provider or
    // credential variable takes effect on the next recording rather than at the
    // next process start.
    if let Some(view) = runtime.published_config() {
        runtime.voice.resync(&view);
    }
    composer.set_voice_enabled(enabled);
}

fn reset_selected_config(runtime: &mut Option<InteractiveRuntime>, state: &mut TuiState) {
    let Some(path) = state
        .overlay
        .as_ref()
        .and_then(Overlay::selected_item)
        .map(|item| item.id.clone())
    else {
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        state.overlay = None;
        return;
    };
    let target = selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
    reset_config_value_at(&path, &target, runtime, state);
    show_config(runtime, state);
}

fn show_config(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    // The effective document with every layer it was composed from, which the
    // published `ConfigView` does not carry and this process already holds.
    let Some(mut snapshot) = runtime
        .workspace
        .config_document()
        .map_err(|error| state.push_diagnostic(error.to_string()))
        .ok()
    else {
        return;
    };
    let schema = call_runtime(runtime, "config/schema", json!({}), state)
        .and_then(|result| result.get("schema").cloned())
        .unwrap_or(Value::Null);
    if let Some(target) = runtime.config_target
        && let Some(snapshot) = snapshot.as_object_mut()
    {
        snapshot.insert("selectedTarget".to_owned(), json!(target.as_str()));
    }
    state.overlay = Some(config_overlay(&snapshot, &schema));
}

fn show_model(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(fields) = config::published_fields(runtime, state) else {
        return;
    };
    state.overlay = Some(model_overlay(&fields, &runtime.model));
}

/// Opens the rewind picker over the session's saved user messages.
///
/// The list is the stored transcript, which is what a rewind cuts, and each
/// point is addressed by its history identity rather than by a position that
/// the next compaction would move. Whether a point would change files is asked
/// for the selected point alone, because only the session's checkpoint log
/// knows and the panel shows one point at a time.
pub(super) fn show_rewind(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(history) = call_runtime(
        runtime,
        "history/list",
        json!({
            "sessionId": runtime.session_id,
            "offset": 0,
            "limit": REWIND_HISTORY_LIMIT,
        }),
        state,
    ) else {
        return;
    };
    let targets = rewind_targets(&map_value(history), 0);
    // Reference `action_rewind_prev`: with no message to rewind to, nothing
    // opens and nothing is said.
    if let Some(rewind) = RewindState::new(targets) {
        state.overlay = None;
        state.rewind = Some(rewind);
        probe_rewind_target(runtime, state);
    }
}

fn show_voice(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    if let Some(fields) = config::published_fields(runtime, state) {
        let view = runtime.published_config().unwrap_or(Value::Null);
        state.overlay = Some(voice_overlay(&fields, &view));
    }
}

/// The choice list one audio family offers, opened from the voice settings.
///
/// Reference `_apply_dynamic_choices`: the two active-model fields are strings
/// on the wire and choice lists on the screen, and the options are the aliases
/// the projection publishes rather than anything the client invents.
fn show_voice_model(runtime: &mut InteractiveRuntime, state: &mut TuiState, field: &str) {
    let Some((_, list, label)) = VOICE_MODEL_FIELDS
        .into_iter()
        .find(|(name, _, _)| *name == field)
    else {
        return;
    };
    let Some(fields) = config::published_fields(runtime, state) else {
        return;
    };
    let current = fields
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let view = runtime.published_config().unwrap_or(Value::Null);
    let aliases = audio_model_aliases(&view, list, &current);
    if aliases.is_empty() {
        state.push_diagnostic(format!(
            "No {} is declared; add one before selecting it",
            label.to_lowercase()
        ));
        return;
    }
    state.overlay = Some(voice_model_overlay(field, label, &aliases, &current));
}

fn show_proxy(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    if let Some(settings) = call_runtime(runtime, "config/proxy/read", json!({}), state)
        .and_then(|result| result.get("settings").cloned())
    {
        state.overlay = Some(proxy_overlay(&settings));
    }
}

fn show_debug(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    if state
        .overlay
        .as_ref()
        .is_some_and(|overlay| overlay.kind == OverlayKind::Debug)
    {
        state.overlay = None;
        state.debug_console = None;
        return;
    }
    state.debug_console = Some(DebugConsole::default());
    refresh_debug_console(runtime, state, unix_millis());
}

/// Reads the next log page and rebuilds the console overlay. A read failure is
/// reported once and leaves the console open on what it already loaded.
pub(in crate::tui) fn refresh_debug_console(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    now_ms: u64,
) {
    let Some(console) = state.debug_console.as_mut() else {
        return;
    };
    let offset = console.next_offset();
    let selected = state
        .overlay
        .as_ref()
        .filter(|overlay| overlay.kind == OverlayKind::Debug)
        .and_then(Overlay::selected_item)
        .map(|item| item.id.clone());
    match runtime.service.public_call(
        "diagnostics/logs/read",
        json!({"offset": offset, "limit": DEBUG_PAGE_SIZE}),
    ) {
        Ok(result) => {
            let page = map_value(result);
            let Some(console) = state.debug_console.as_mut() else {
                return;
            };
            console.absorb(&page, now_ms);
            let overlay = console.overlay(selected.as_deref());
            state.overlay = Some(overlay);
        }
        Err(error) => {
            let console = console.clone();
            state.push_diagnostic(format!("Debug log read failed: {error}"));
            state.overlay = Some(console.overlay(selected.as_deref()));
        }
    }
}

fn resume_selected_session(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    controls: &mut ControlState,
    session_id: &str,
) {
    if session_id == runtime.session_id {
        state.push_diagnostic("This session is already active.");
        return;
    }
    if let Some(result) = call_runtime(
        runtime,
        "session/resume",
        json!({"sessionId": session_id}),
        state,
    ) && let Some(session_id) = metadata_session_id(&result)
        && adopt_hydrated_session(runtime, state, controls, session_id)
    {
        state.overlay = None;
        state.session_delete = None;
        push_local_notice(state, "Resumed session", EntryStatus::Completed);
    }
}

fn delete_selected_session(
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    session_id: &str,
) {
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if let Err(error) = runtime
        .service
        .public_call("session/delete", json!({"sessionId": session_id}))
    {
        state.session_delete = Some(SessionDeleteState::failure(session_id, error.to_string()));
        return;
    }
    state.session_delete = None;
    let remaining = if let Some(overlay) = state.overlay.as_mut() {
        overlay.items.retain(|item| item.id != session_id);
        overlay.set_query(overlay.query.clone());
        Some(overlay.items.len())
    } else {
        None
    };
    match remaining {
        Some(0) => {
            state.overlay = None;
            push_local_notice(
                state,
                "No saved sessions left for this directory.",
                EntryStatus::Completed,
            );
        }
        Some(_) => {
            push_local_notice(
                state,
                &format!(
                    "Deleted session `{}`.",
                    session_id.chars().take(8).collect::<String>()
                ),
                EntryStatus::Completed,
            );
        }
        None => {}
    }
}

fn handle_rewind_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
) {
    let Some(rewind) = state.rewind.as_mut() else {
        return;
    };
    let selected = rewind.target().entry_id.clone();
    match reduce_rewind_key(rewind, key) {
        RewindEffect::None => {
            // Moving to another point changes which actions the panel offers,
            // and only the log can say whether that point would change files.
            let moved = state
                .rewind
                .as_ref()
                .is_some_and(|rewind| rewind.target().entry_id != selected);
            if let Some(runtime) = runtime.as_mut()
                && moved
            {
                probe_rewind_target(runtime, state);
            }
        }
        RewindEffect::Cancel => state.rewind = None,
        RewindEffect::Scroll(delta) if delta.is_negative() => {
            state.scroll_up(delta.unsigned_abs());
        }
        RewindEffect::Scroll(delta) => {
            state.scroll_down(delta.unsigned_abs());
        }
        RewindEffect::Accept {
            entry_id,
            restore_files,
        } => accept_rewind(runtime, state, controls, composer, &entry_id, restore_files),
    }
}

/// Asks the session's checkpoint log whether the selected point would change
/// files, which is what decides the actions the panel offers.
///
/// A point the log carries no turn for answers false, which is the same answer
/// a session with no engine attached gives.
fn probe_rewind_target(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(entry_id) = state
        .rewind
        .as_ref()
        .map(|rewind| rewind.target().entry_id.clone())
    else {
        return;
    };
    let has_file_changes = call_runtime(
        runtime,
        "session/rewind/read",
        json!({"sessionId": runtime.session_id, "entryId": entry_id}),
        state,
    )
    .and_then(|result| result.get("hasFileChanges").and_then(Value::as_bool))
    .unwrap_or(false);
    if let Some(rewind) = state.rewind.as_mut() {
        rewind.set_target_file_changes(has_file_changes);
    }
}

fn accept_rewind(
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    entry_id: &str,
    restore_files: bool,
) {
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("The selected rewind point is unavailable");
        return;
    };
    let result = match runtime.service.public_call(
        "session/rewind",
        json!({
            "sessionId": runtime.session_id,
            "entryId": entry_id,
            "restoreFiles": restore_files,
        }),
    ) {
        Ok(result) => result,
        Err(error) => {
            if let Some(rewind) = state.rewind.as_mut() {
                rewind.set_error(format!("Rewind failed: {error}"));
            }
            return;
        }
    };
    let message = result
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let restore_errors = result
        .get("restoreErrors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    // The answer carries the rewound session's public state rather than its
    // stored metadata, so the session to adopt is named there. A fork lands on
    // a new identifier and an in-place rewind on the same one.
    if let Some(session_id) = result
        .get("state")
        .and_then(|state| state.pointer("/session/id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        && adopt_hydrated_session(runtime, state, controls, session_id)
    {
        composer.replace_text(message);
        for error in restore_errors {
            state.push_diagnostic(format!("File restoration warning: {error}"));
        }
        state.rewind = None;
        push_local_notice(
            state,
            "Rewound into a new branch; the original session was preserved",
            EntryStatus::Completed,
        );
    }
}

fn map_value(map: std::collections::BTreeMap<String, Value>) -> Value {
    Value::Object(map.into_iter().collect())
}
