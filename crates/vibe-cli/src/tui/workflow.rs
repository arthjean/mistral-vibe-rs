use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use vibe_core::telemetry::records::TelemetryCommandKind;
use vibe_core::workspace::WARNING_TAG;

mod config;
mod mcp;
mod overlay;
mod runtime;

#[cfg(test)]
#[path = "workflow/audio_surface_tests.rs"]
mod audio_surface_tests;

use super::chat_input::ChatInputState;
use super::clipboard::{SystemClipboard, SystemClipboardPort};
use super::commands::{CommandId, parse_command_in};
use super::controls::ControlState;
use super::debug_console::{DebugConsole, PAGE_SIZE as DEBUG_PAGE_SIZE};
use super::interaction::{Overlay, OverlayKind, RemoteProjectAction, TeleportPushAction};
use super::pickers::{
    VOICE_MODEL_FIELDS, audio_model_aliases, config_overlay, help_overlay, model_overlay,
    proxy_overlay, rewind_targets, sessions_overlay, status_overlay, theme_overlay,
    thinking_overlay, voice_model_overlay, voice_overlay,
};
use super::rewind::{RewindEffect, RewindState, reduce_key as reduce_rewind_key};
use super::session_picker::{
    SessionDeleteState, SessionPickerEffect, reduce_key as reduce_session_picker_key,
};
use super::setup::ResolvedTheme;
use super::state::{EntryStatus, TranscriptKind, TuiState};
use super::switching::{self, SwitchRequest};
use super::{
    Arguments, InteractiveRuntime, adopt_hydrated_session, call_runtime, metadata_session_id,
    parse_runtime_skills, push_local_notice, refresh_server_banner_metrics, sync_runtime_intent,
    unix_millis,
};
pub(in crate::tui) use config::apply_render_preferences;
use config::{
    configured_value, reset_config_value, reset_config_value_at, selected_config_target,
    set_config_value, update_proxy_value,
};
pub(in crate::tui) use mcp::{McpEffect, McpPendingOperation, apply_pending_operation};
pub(super) use mcp::{SystemUrlOpener, UrlOpenerPort, execute_mcp_effect};
use mcp::{handle_mcp, refresh_selected_mcp, set_selected_mcp};
#[cfg(test)]
pub(in crate::tui) use mcp::{reduce_auth_action, valid_auth_url};
use overlay::select_overlay_item;
pub(in crate::tui) use runtime::handle_runtime_command;

/// How much of the saved transcript the rewind picker lists.
///
/// The store caps a page at 500, and a rewind point past that is one the
/// operator would have to scroll a conversation of that length to reach.
const REWIND_HISTORY_LIMIT: usize = 500;

const DATA_RETENTION_MESSAGE: &str = "\
## Your Data Helps Improve Mistral AI

At Mistral AI, we're committed to delivering the best possible experience. When you use Mistral models on our API, your interactions may be collected to improve our models, ensuring they stay cutting-edge, accurate, and helpful.

Manage your data settings [here](https://chat.mistral.ai/work?profile_dialog=privacy)";

pub(super) enum CommandAction {
    Unhandled,
    Handled,
    RejectedBusy,
    ClipboardImageRequested,
    Exit,
    Runtime(RuntimeCommand),
    /// A command that resolves to a model turn rather than to local state. The
    /// caller submits the text the way it submits a typed line, so queueing,
    /// image preparation and the busy path stay in one place.
    Prompt(String),
}

/// A parsed command whose execution needs the session runtime rather than the
/// overlay layer. Arguments are carried by the variant that consumes them, so
/// no handler re-parses a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RuntimeCommand {
    Clear,
    Compact(String),
    Rename(String),
    Resume(String),
    Loop(String),
    Teleport(String),
    RemoteProject(String),
    Model(String),
    Thinking(String),
    Theme(String),
}

impl RuntimeCommand {
    pub(super) const fn changes_session_projection(&self) -> bool {
        matches!(self, Self::Clear | Self::Compact(_) | Self::Resume(_))
    }
}

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

pub(super) async fn dispatch_command(
    command_line: &str,
    arguments: &Arguments,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    composer: &mut ChatInputState,
    runtime_busy: bool,
) -> CommandAction {
    let command_context = composer.command_context().clone();
    let Some(parsed) = parse_command_in(command_line, &command_context) else {
        return CommandAction::Unhandled;
    };
    // Reference `_handle_command`: the event is recorded where the command is
    // resolved, before it runs, and reports the name the operator typed.
    if let Some(runtime) = runtime.as_ref() {
        super::report_slash_command(runtime, command_line, TelemetryCommandKind::Builtin);
    }
    let command_id = parsed.id;
    let command_arguments = parsed.arguments.to_owned();
    if runtime_busy {
        state.push_diagnostic("Slash commands cannot be queued while the runtime is busy");
        return CommandAction::RejectedBusy;
    }
    match command_id {
        CommandId::Exit => return CommandAction::Exit,
        CommandId::Help => {
            state.overlay = Some(help_overlay(&command_context));
            return CommandAction::Handled;
        }
        CommandId::Copy => {
            copy_last_agent_message(state);
            return CommandAction::Handled;
        }
        CommandId::PasteImage => return CommandAction::ClipboardImageRequested,
        CommandId::DataRetention => {
            push_local_notice(state, DATA_RETENTION_MESSAGE, EntryStatus::Completed);
            return CommandAction::Handled;
        }
        _ => {}
    }

    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before using this command");
        return CommandAction::Handled;
    };
    // Commands whose bare form opens a picker and whose argument form is
    // executed by the runtime layer.
    match command_id {
        CommandId::Model if !command_arguments.is_empty() => {
            return CommandAction::Runtime(RuntimeCommand::Model(command_arguments));
        }
        CommandId::Thinking if !command_arguments.is_empty() => {
            return CommandAction::Runtime(RuntimeCommand::Thinking(command_arguments));
        }
        CommandId::Theme if !command_arguments.is_empty() => {
            return CommandAction::Runtime(RuntimeCommand::Theme(command_arguments));
        }
        CommandId::Resume if !command_arguments.is_empty() => {
            return CommandAction::Runtime(RuntimeCommand::Resume(command_arguments));
        }
        _ => {}
    }
    match command_id {
        CommandId::Clear => CommandAction::Runtime(RuntimeCommand::Clear),
        CommandId::Compact => CommandAction::Runtime(RuntimeCommand::Compact(command_arguments)),
        CommandId::Rename => CommandAction::Runtime(RuntimeCommand::Rename(command_arguments)),
        CommandId::Loop => CommandAction::Runtime(RuntimeCommand::Loop(command_arguments)),
        CommandId::Teleport => CommandAction::Runtime(RuntimeCommand::Teleport(command_arguments)),
        CommandId::RemoteProject => {
            CommandAction::Runtime(RuntimeCommand::RemoteProject(command_arguments))
        }
        CommandId::Model => {
            show_model(runtime, state);
            CommandAction::Handled
        }
        CommandId::Thinking => {
            state.overlay = Some(thinking_overlay(&runtime.thinking));
            CommandAction::Handled
        }
        CommandId::Theme => {
            let current = configured_value(runtime, "theme")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| "system".to_owned());
            state.overlay = Some(theme_overlay(&current));
            CommandAction::Handled
        }
        CommandId::Resume => {
            show_sessions(runtime, working_directory, state);
            CommandAction::Handled
        }
        CommandId::Rewind => {
            show_rewind(runtime, state);
            CommandAction::Handled
        }
        CommandId::Config => {
            apply_config_command(&command_arguments, runtime, state, composer);
            CommandAction::Handled
        }
        CommandId::Reload => {
            if call_runtime(runtime, "config/reload", json!({}), state).is_some() {
                if let Some(result) = call_runtime(runtime, "skills/list", json!({}), state) {
                    runtime.skills = parse_runtime_skills(result.get("skills"));
                    composer.set_user_skills(
                        runtime
                            .skills
                            .values()
                            .map(|skill| (skill.name.as_str(), skill.description.as_str())),
                    );
                }
                let session_id = runtime.session_id.clone();
                refresh_server_banner_metrics(
                    &mut runtime.service,
                    &session_id,
                    &mut runtime.banner,
                )
                .await;
                apply_render_preferences(runtime, state);
                sync_voice_preference(runtime, composer);
                push_local_notice(
                    state,
                    "Configuration reloaded (includes agent instructions and skills).",
                    EntryStatus::Completed,
                );
            }
            CommandAction::Handled
        }
        CommandId::Log => {
            show_log_path(arguments, runtime, working_directory, state);
            CommandAction::Handled
        }
        CommandId::Debug => {
            show_debug(runtime, state);
            CommandAction::Handled
        }
        CommandId::Status => {
            show_status(runtime, state);
            CommandAction::Handled
        }
        CommandId::Whoami => {
            show_whoami(runtime, state);
            CommandAction::Handled
        }
        CommandId::Retry => CommandAction::Prompt(retry_prompt(&command_arguments)),
        CommandId::ProxySetup => {
            if command_arguments.is_empty() {
                show_proxy(runtime, state);
            } else {
                update_proxy_value(&command_arguments, runtime, state);
            }
            CommandAction::Handled
        }
        CommandId::Mcp => {
            handle_mcp(&command_arguments, runtime, state);
            CommandAction::Handled
        }
        CommandId::Voice => {
            show_voice(runtime, state);
            CommandAction::Handled
        }
        CommandId::InstallLean => {
            mutate_lean_agent(runtime, state, true);
            CommandAction::Handled
        }
        CommandId::UninstallLean => {
            mutate_lean_agent(runtime, state, false);
            CommandAction::Handled
        }
        CommandId::Exit
        | CommandId::Help
        | CommandId::Copy
        | CommandId::PasteImage
        | CommandId::DataRetention => CommandAction::Handled,
    }
}

/// `/config` opens the browser; `/config set` and `/config reset` write, so the
/// value editor the overlay prefills is reachable through a parseable alias.
fn apply_config_command(
    command_arguments: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    composer: &mut ChatInputState,
) {
    // A bare subcommand still reaches its own usage message rather than silently
    // opening the browser.
    let (subcommand, rest) = command_arguments
        .split_once(char::is_whitespace)
        .unwrap_or((command_arguments, ""));
    match subcommand {
        "set" => set_config_value(rest, runtime, state),
        "reset" => reset_config_value(rest, runtime, state),
        _ => {
            show_config(runtime, state);
            return;
        }
    }
    sync_voice_preference(runtime, composer);
}

pub(super) async fn handle_overlay_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    theme: &mut ResolvedTheme,
) -> OverlayKeyResult {
    if state.rewind.is_some() {
        handle_rewind_key(key, runtime, state, controls, composer);
        return OverlayKeyResult::Handled;
    }
    let Some(kind) = state.overlay.as_ref().map(|overlay| overlay.kind) else {
        return OverlayKeyResult::Unhandled;
    };
    if kind == OverlayKind::RemoteProjectCreate {
        return handle_remote_project_create_key(key, runtime, state);
    }
    if kind == OverlayKind::Sessions {
        let current_session_id = runtime
            .as_ref()
            .map_or("", |runtime| runtime.session_id.as_str());
        let Some(overlay) = state.overlay.as_mut() else {
            return OverlayKeyResult::Unhandled;
        };
        let effect =
            reduce_session_picker_key(overlay, &mut state.session_delete, current_session_id, key);
        match effect {
            SessionPickerEffect::None => {}
            SessionPickerEffect::Close => state.overlay = None,
            SessionPickerEffect::Resume(session_id) => {
                if let Some(runtime) = runtime.as_mut() {
                    resume_selected_session(runtime, state, controls, &session_id);
                }
            }
            SessionPickerEffect::Delete(session_id) => {
                delete_selected_session(runtime, state, &session_id);
            }
        }
        return OverlayKeyResult::Handled;
    }
    match key.code {
        KeyCode::Esc => {
            if kind == OverlayKind::TeleportApproval {
                let action = state.overlay.as_ref().and_then(|overlay| {
                    overlay.items.iter().find_map(|item| match &item.action {
                        super::interaction::OverlayAction::TeleportPush(action) => {
                            Some(TeleportPushAction {
                                operation_id: action.operation_id.clone(),
                                approved: false,
                            })
                        }
                        _ => None,
                    })
                });
                return action.map_or(OverlayKeyResult::Handled, |action| {
                    OverlayKeyResult::Effect(OverlayEffect::TeleportPush(action))
                });
            }
            if let Some(effect) = remote_project_escape_effect(kind) {
                return OverlayKeyResult::Effect(effect);
            }
            // Reference `ThemePickerApp.Cancelled`: cancelling restores the
            // persisted theme, discarding every preview.
            if kind == OverlayKind::Theme
                && let Some(runtime) = runtime.as_mut()
            {
                let persisted = configured_value(runtime, "theme")
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| crate::tui::themes::AUTO_THEME.to_owned());
                crate::tui::preview_theme(&persisted, theme);
            }
            state.overlay = None;
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(-1);
            }
            preview_selected_theme(kind, state, theme);
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(1);
            }
            preview_selected_theme(kind, state, theme);
        }
        KeyCode::Backspace if kind == OverlayKind::McpDetail && key.modifiers.is_empty() => {
            return OverlayKeyResult::Effect(OverlayEffect::Mcp(McpEffect::Show { filter: None }));
        }
        KeyCode::Backspace if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.pop_query();
            }
            preview_selected_theme(kind, state, theme);
        }
        KeyCode::Char('r')
            if matches!(kind, OverlayKind::Mcp | OverlayKind::McpDetail)
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            if let Some(effect) = refresh_selected_mcp(state) {
                return OverlayKeyResult::Effect(OverlayEffect::Mcp(effect));
            }
        }
        KeyCode::Char('d')
            if matches!(kind, OverlayKind::Mcp | OverlayKind::McpDetail)
                && key.modifiers.is_empty() =>
        {
            if let Some(effect) = set_selected_mcp(state, false) {
                return OverlayKeyResult::Effect(OverlayEffect::Mcp(effect));
            }
        }
        KeyCode::Char('e')
            if matches!(kind, OverlayKind::Mcp | OverlayKind::McpDetail)
                && key.modifiers.is_empty() =>
        {
            if let Some(effect) = set_selected_mcp(state, true) {
                return OverlayKeyResult::Effect(OverlayEffect::Mcp(effect));
            }
        }
        KeyCode::Char('r')
            if kind == OverlayKind::Config && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            reset_selected_config(runtime, state);
        }
        KeyCode::Enter | KeyCode::Char(' ')
            if key.modifiers.is_empty()
                && !matches!(
                    kind,
                    OverlayKind::Help
                        | OverlayKind::Debug
                        | OverlayKind::Status
                        | OverlayKind::DataRetention
                ) =>
        {
            if let Some(effect) =
                select_overlay_item(runtime, state, controls, composer, theme).await
            {
                return OverlayKeyResult::Effect(effect);
            }
        }
        KeyCode::Char(character) if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.push_query(character);
            }
            preview_selected_theme(kind, state, theme);
        }
        _ => {}
    }
    OverlayKeyResult::Handled
}

/// Reference `on_option_list_option_highlighted`: the highlighted theme previews
/// immediately, without touching the configuration.
fn preview_selected_theme(kind: OverlayKind, state: &TuiState, theme: &mut ResolvedTheme) {
    if kind != OverlayKind::Theme {
        return;
    }
    if let Some(selected) = state
        .overlay
        .as_ref()
        .and_then(super::interaction::Overlay::selected_item)
    {
        crate::tui::preview_theme(&selected.id, theme);
    }
}

pub(super) fn handle_remote_project_create_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) -> OverlayKeyResult {
    let Some(runtime) = runtime.as_mut() else {
        state.overlay = None;
        return OverlayKeyResult::Handled;
    };
    let Some(mut draft) = runtime.remote_project_draft.clone() else {
        state.push_diagnostic("Remote project draft is unavailable");
        state.overlay.clone_from(&runtime.remote_project_overlay);
        return OverlayKeyResult::Handled;
    };
    let selected = state
        .overlay
        .as_ref()
        .and_then(|overlay| overlay.selected_item())
        .map(|item| item.id.as_str())
        .unwrap_or_default();
    match key.code {
        KeyCode::Esc => {
            runtime.remote_project_draft = None;
            state.overlay.clone_from(&runtime.remote_project_overlay);
            return OverlayKeyResult::Handled;
        }
        KeyCode::Up | KeyCode::BackTab => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(-1);
            }
            return OverlayKeyResult::Handled;
        }
        KeyCode::Down | KeyCode::Tab => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(1);
            }
            return OverlayKeyResult::Handled;
        }
        KeyCode::Enter if selected == "remote-project:create:submit" => {
            let action = RemoteProjectAction::Create {
                name: draft.name.trim().to_owned(),
                default_branch: draft.default_branch.trim().to_owned(),
            };
            return OverlayKeyResult::Effect(OverlayEffect::RemoteProject(action));
        }
        KeyCode::Enter => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(1);
            }
            return OverlayKeyResult::Handled;
        }
        KeyCode::Backspace if key.modifiers.is_empty() => match selected {
            "remote-project:create:name" => {
                draft.name.pop();
            }
            "remote-project:create:branch" => {
                draft.default_branch.pop();
            }
            _ => return OverlayKeyResult::Handled,
        },
        KeyCode::Char(character) if key.modifiers.is_empty() => match selected {
            "remote-project:create:name" => draft.name.push(character),
            "remote-project:create:branch" => draft.default_branch.push(character),
            _ => return OverlayKeyResult::Handled,
        },
        _ => return OverlayKeyResult::Handled,
    }
    let selected_id = selected.to_owned();
    let mut overlay = super::pickers::remote_project_create_overlay(&draft);
    overlay.select_by_id(&selected_id);
    runtime.remote_project_draft = Some(draft);
    state.overlay = Some(overlay);
    OverlayKeyResult::Handled
}

fn remote_project_escape_effect(kind: OverlayKind) -> Option<OverlayEffect> {
    (kind == OverlayKind::RemoteProjects)
        .then_some(OverlayEffect::RemoteProject(RemoteProjectAction::Cancel))
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
    if let Some(view) = published_config_view(runtime) {
        runtime.voice.resync(&view);
    }
    composer.set_voice_enabled(enabled);
}

/// The `ConfigView` this session publishes, which is what the audio surface is
/// resolved from.
pub(super) fn published_config_view(runtime: &mut InteractiveRuntime) -> Option<Value> {
    runtime
        .service
        .public_call("config/read", json!({"sessionId": runtime.session_id}))
        .ok()?
        .get("config")
        .cloned()
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
        .release3
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

fn show_sessions(runtime: &mut InteractiveRuntime, working_directory: &Path, state: &mut TuiState) {
    let Some(result) = call_runtime(
        runtime,
        "session/list",
        json!({
            "cwd": working_directory.to_string_lossy(),
            "offset": 0,
            "limit": 100,
        }),
        state,
    ) else {
        return;
    };
    let value = map_value(result);
    let overlay = sessions_overlay(&value, &runtime.session_id);
    if overlay.items.is_empty() {
        push_local_notice(
            state,
            "No sessions found for this directory.",
            EntryStatus::Completed,
        );
    } else {
        state.overlay = Some(overlay);
    }
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
    if let Some(rewind) = RewindState::new(targets) {
        state.overlay = None;
        state.rewind = Some(rewind);
        probe_rewind_target(runtime, state);
    } else {
        push_local_notice(
            state,
            "There are no user messages to rewind.",
            EntryStatus::Completed,
        );
    }
}

fn show_voice(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    if let Some(fields) = config::published_fields(runtime, state) {
        let view = published_config_view(runtime).unwrap_or(Value::Null);
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
    let view = published_config_view(runtime).unwrap_or(Value::Null);
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

fn show_status(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    if let Some(result) = call_runtime(
        runtime,
        "stats/read",
        json!({"sessionId": runtime.session_id}),
        state,
    ) {
        state.overlay = Some(status_overlay(&map_value(result)));
    }
}

/// `/whoami`: what this build can answer about the signed-in account.
///
/// The reference reads `identity/read` and `account/read` together and prints
/// the name, email, workspace, organization and plan. This port declares
/// `identity/read` and does not route it yet, the divergence `docs/parity.md`
/// records, so the identity half says so plainly instead of being invented and
/// the account half answers from `account/read`.
fn show_whoami(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(result) = call_runtime(runtime, "account/read", json!({}), state) else {
        return;
    };
    let account = result.get("account").cloned().unwrap_or(Value::Null);
    let status = account
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let mut lines = vec!["Who am I".to_owned(), format!("Account: {status}")];
    if let Some(plan) = account.get("plan").and_then(Value::as_str) {
        lines.push(format!("Plan: {plan}"));
    }
    lines.push(
        "Signed-in name, email, workspace and organization are unavailable: this build declares \
         `identity/read` and does not route it yet."
            .to_owned(),
    );
    push_local_notice(state, &lines.join("\n"), EntryStatus::Completed);
}

/// The continuation a `/retry` submits, in this port's own words.
///
/// The reference wraps the same three directives in the same warning tag
/// (`vibe/cli/commands.py:18`): resume where the stream broke, do not restate
/// what was already produced, and answer the pending request from the start when
/// nothing was. `NOTICE` forbids shipping its sentences, so these are original.
fn retry_prompt(additional_instructions: &str) -> String {
    let mut message = "The previous model stream stopped before it finished. Pick the response up \
                       where it broke off, without restating anything already written. If nothing \
                       was written yet, answer the pending request from the start."
        .to_owned();
    let instructions = additional_instructions.trim();
    if !instructions.is_empty() {
        message.push_str(&format!(
            "\n\nApply these further instructions from the operator while continuing:\n\
             {instructions}"
        ));
    }
    format!("<{WARNING_TAG}>{message}</{WARNING_TAG}>")
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

fn mutate_lean_agent(runtime: &mut InteractiveRuntime, state: &mut TuiState, install: bool) {
    let method = if install {
        "agents/install"
    } else {
        "agents/uninstall"
    };
    if let Some(result) = call_runtime(
        runtime,
        method,
        json!({"sessionId": runtime.session_id, "agentName": "lean"}),
        state,
    ) {
        // The catalog answer names the agent the session runs now, which is the
        // default when the one it was running has just been uninstalled.
        if let Some(agent) = result
            .get("active")
            .and_then(|agent| agent.get("name"))
            .and_then(Value::as_str)
        {
            sync_runtime_intent(runtime, Some(agent));
        }
        push_local_notice(
            state,
            if install {
                "Lean agent installed."
            } else {
                "Lean agent uninstalled."
            },
            EntryStatus::Completed,
        );
    }
}

fn show_log_path(
    arguments: &Arguments,
    runtime: &InteractiveRuntime,
    working_directory: &Path,
    state: &mut TuiState,
) {
    // The same roots the session itself was opened under, so the directory
    // named here is the one the session is actually writing to.
    let path = super::startup::release3_paths(arguments, working_directory)
        .session_root
        .join(&runtime.session_id);
    if path.is_dir() {
        push_local_notice(
            state,
            &format!(
                "## Current Log Directory\n\n`{}`\n\nYou can send this directory to share your interaction.",
                path.display()
            ),
            EntryStatus::Completed,
        );
    } else {
        state.push_diagnostic("The current session has not been persisted yet.");
    }
}

fn copy_last_agent_message(state: &mut TuiState) {
    let content = state
        .entries
        .iter()
        .rev()
        .find(|entry| {
            entry.kind == TranscriptKind::AssistantMessage && !entry.text.trim().is_empty()
        })
        .map(|entry| entry.text.clone());
    let Some(content) = content else {
        state.push_diagnostic("No agent message available to copy");
        return;
    };
    match SystemClipboard.copy_text(&content) {
        Ok(()) => push_local_notice(
            state,
            "Last agent message copied to clipboard",
            EntryStatus::Completed,
        ),
        Err(_) => state.push_diagnostic("Failed to copy: clipboard not available"),
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
        Some(_) => push_local_notice(
            state,
            &format!(
                "Deleted session `{}`.",
                session_id.chars().take(8).collect::<String>()
            ),
            EntryStatus::Completed,
        ),
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

#[cfg(test)]
mod overlay_effect_tests {
    use super::*;

    #[test]
    fn remote_project_escape_emits_cancellation() {
        assert_eq!(
            remote_project_escape_effect(OverlayKind::RemoteProjects),
            Some(OverlayEffect::RemoteProject(RemoteProjectAction::Cancel))
        );
        assert_eq!(remote_project_escape_effect(OverlayKind::Mcp), None);
    }
}
