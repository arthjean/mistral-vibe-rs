use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

mod config;
mod mcp;
mod overlay;

use super::chat_input::ChatInputState;
use super::clipboard::{SystemClipboard, SystemClipboardPort};
use super::commands::{CommandId, parse_command_in};
use super::controls::ControlState;
use super::debug_console::{DebugConsole, PAGE_SIZE as DEBUG_PAGE_SIZE};
use super::interaction::{Overlay, OverlayKind, RemoteProjectAction, TeleportPushAction};
use super::pickers::{
    config_overlay, help_overlay, model_overlay, proxy_overlay, rewind_state, sessions_overlay,
    status_overlay, theme_overlay, thinking_overlay, voice_overlay,
};
use super::rewind::{RewindEffect, reduce_key as reduce_rewind_key};
use super::session_picker::{
    SessionDeleteState, SessionPickerEffect, reduce_key as reduce_session_picker_key,
};
use super::setup::ResolvedTheme;
use super::state::{EntryStatus, TranscriptKind, TuiState};
use super::switching::{self, SwitchRequest};
use super::{
    Arguments, CliError, InteractiveRuntime, adopt_hydrated_session, call_runtime,
    metadata_session_id, parse_runtime_skills, push_local_notice, refresh_server_banner_metrics,
    sync_runtime_intent, unix_millis,
};
pub(in crate::tui) use config::apply_render_preferences;
pub(super) use config::apply_thinking;
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
    Setup,
    Runtime(RuntimeCommand),
}

pub(super) struct RuntimeCommand {
    pub id: CommandId,
    pub arguments: String,
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

impl RuntimeCommand {
    fn new(id: CommandId, arguments: String) -> Self {
        Self { id, arguments }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_command(
    command_line: &str,
    arguments: &Arguments,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    _controls: &mut ControlState,
    composer: &mut ChatInputState,
    _theme: &mut ResolvedTheme,
    runtime_busy: bool,
) -> Result<CommandAction, CliError> {
    let command_context = composer.command_context().clone();
    let Some(parsed) = parse_command_in(command_line, &command_context) else {
        return Ok(CommandAction::Unhandled);
    };
    let command_id = parsed.id;
    let command_arguments = parsed.arguments.to_owned();
    if runtime_busy {
        state.push_diagnostic("Slash commands cannot be queued while the runtime is busy");
        return Ok(CommandAction::RejectedBusy);
    }
    match command_id {
        CommandId::Exit => return Ok(CommandAction::Exit),
        CommandId::Setup => return Ok(CommandAction::Setup),
        CommandId::Help => {
            state.overlay = Some(help_overlay(&command_context));
            return Ok(CommandAction::Handled);
        }
        CommandId::Copy => {
            copy_last_agent_message(state);
            return Ok(CommandAction::Handled);
        }
        CommandId::PasteImage => {
            return Ok(CommandAction::ClipboardImageRequested);
        }
        CommandId::DataRetention => {
            push_local_notice(state, DATA_RETENTION_MESSAGE, EntryStatus::Completed);
            return Ok(CommandAction::Handled);
        }
        _ => {}
    }

    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before using this command");
        return Ok(CommandAction::Handled);
    };
    match command_id {
        CommandId::Config => {
            apply_config_command(&command_arguments, runtime, state, composer);
            Ok(CommandAction::Handled)
        }
        CommandId::Model => {
            if command_arguments.is_empty() {
                show_model(runtime, state);
                Ok(CommandAction::Handled)
            } else {
                Ok(CommandAction::Runtime(RuntimeCommand::new(
                    CommandId::Settings,
                    format!("model {command_arguments}"),
                )))
            }
        }
        CommandId::Thinking => {
            if command_arguments.is_empty() {
                state.overlay = Some(thinking_overlay(&runtime.thinking));
                Ok(CommandAction::Handled)
            } else {
                Ok(CommandAction::Runtime(RuntimeCommand::new(
                    CommandId::Settings,
                    format!("thinking {command_arguments}"),
                )))
            }
        }
        CommandId::Theme => {
            if command_arguments.is_empty() {
                let current = configured_value(runtime, "theme")
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "system".to_owned());
                state.overlay = Some(theme_overlay(&current));
                Ok(CommandAction::Handled)
            } else {
                Ok(CommandAction::Runtime(RuntimeCommand::new(
                    CommandId::Theme,
                    command_arguments,
                )))
            }
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
            Ok(CommandAction::Handled)
        }
        CommandId::Log => {
            show_log_path(arguments, runtime, working_directory, state);
            Ok(CommandAction::Handled)
        }
        CommandId::Debug => {
            show_debug(runtime, state);
            Ok(CommandAction::Handled)
        }
        CommandId::Status => {
            show_status(runtime, state);
            Ok(CommandAction::Handled)
        }
        CommandId::ProxySetup => {
            if command_arguments.is_empty() {
                show_proxy(runtime, state);
            } else {
                update_proxy_value(&command_arguments, runtime, state);
            }
            Ok(CommandAction::Handled)
        }
        CommandId::Resume => {
            if command_arguments.is_empty() {
                show_sessions(runtime, working_directory, state);
                Ok(CommandAction::Handled)
            } else {
                Ok(CommandAction::Runtime(RuntimeCommand::new(
                    CommandId::Resume,
                    command_arguments,
                )))
            }
        }
        CommandId::Continue => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Continue,
            command_arguments,
        ))),
        CommandId::Rename => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Rename,
            command_arguments,
        ))),
        CommandId::Mcp => {
            handle_mcp(&command_arguments, runtime, state);
            Ok(CommandAction::Handled)
        }
        CommandId::Voice => {
            show_voice(runtime, state);
            Ok(CommandAction::Handled)
        }
        CommandId::InstallLean => {
            mutate_lean_agent(runtime, state, true);
            Ok(CommandAction::Handled)
        }
        CommandId::UninstallLean => {
            mutate_lean_agent(runtime, state, false);
            Ok(CommandAction::Handled)
        }
        CommandId::Settings if command_arguments.starts_with("set ") => {
            set_config_value(&command_arguments[4..], runtime, state);
            sync_voice_preference(runtime, composer);
            Ok(CommandAction::Handled)
        }
        CommandId::Settings if command_arguments.starts_with("reset ") => {
            reset_config_value(&command_arguments[6..], runtime, state);
            sync_voice_preference(runtime, composer);
            Ok(CommandAction::Handled)
        }
        CommandId::Clear => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Clear,
            command_arguments,
        ))),
        CommandId::Compact => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Compact,
            command_arguments,
        ))),
        CommandId::Rewind if command_arguments.is_empty() => {
            show_rewind(runtime, state);
            Ok(CommandAction::Handled)
        }
        CommandId::Rewind => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Rewind,
            command_arguments,
        ))),
        CommandId::Loop => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Loop,
            command_arguments,
        ))),
        CommandId::Teleport => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Teleport,
            command_arguments,
        ))),
        CommandId::RemoteProject => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::RemoteProject,
            command_arguments,
        ))),
        CommandId::Approve => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Approve,
            command_arguments,
        ))),
        CommandId::Deny => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Deny,
            command_arguments,
        ))),
        CommandId::Fork => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Fork,
            command_arguments,
        ))),
        CommandId::History => {
            show_sessions(runtime, working_directory, state);
            Ok(CommandAction::Handled)
        }
        CommandId::Settings => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Settings,
            command_arguments,
        ))),
        CommandId::Trust => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Trust,
            command_arguments,
        ))),
        CommandId::Update => Ok(CommandAction::Runtime(RuntimeCommand::new(
            CommandId::Update,
            command_arguments,
        ))),
        CommandId::Exit
        | CommandId::Setup
        | CommandId::Help
        | CommandId::Copy
        | CommandId::PasteImage
        | CommandId::DataRetention => Ok(CommandAction::Handled),
    }
}

#[allow(clippy::too_many_arguments)]
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

/// `/config` opens the browser; `/config set` and `/config reset` write, so the
/// value editor the overlay prefills is reachable through a parseable alias.
///
/// A bare subcommand still reaches its own usage message rather than silently
/// opening the browser.
fn apply_config_command(
    command_arguments: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    composer: &mut ChatInputState,
) {
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
                .get("kind")
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
    runtime.voice.set_enabled(enabled);
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
    let Some(mut snapshot) = call_runtime(runtime, "config/read", json!({}), state)
        .and_then(|result| result.get("snapshot").cloned())
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
    let Some(snapshot) = call_runtime(runtime, "config/read", json!({}), state)
        .and_then(|result| result.get("snapshot").cloned())
    else {
        return;
    };
    state.overlay = Some(model_overlay(&snapshot, &runtime.model));
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

pub(super) fn show_rewind(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(result) = call_runtime(
        runtime,
        "session/rewind/read",
        json!({"sessionId": runtime.session_id}),
        state,
    ) else {
        return;
    };
    let rewind = rewind_state(&map_value(result));
    if let Some(rewind) = rewind {
        state.overlay = None;
        state.rewind = Some(rewind);
    } else {
        push_local_notice(
            state,
            "There are no user messages to rewind.",
            EntryStatus::Completed,
        );
    }
}

fn show_voice(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    if let Some(snapshot) = call_runtime(runtime, "config/read", json!({}), state)
        .and_then(|result| result.get("snapshot").cloned())
    {
        state.overlay = Some(voice_overlay(&snapshot));
    }
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
        if let Some(agent) = result
            .get("agent")
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
    let session_root = arguments
        .session_root
        .clone()
        .or_else(|| {
            std::env::var_os("VIBE_HOME")
                .map(PathBuf::from)
                .map(|p| p.join("sessions"))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibe/sessions"))
        })
        .unwrap_or_else(|| working_directory.join(".vibe/sessions"));
    let path = session_root.join(&runtime.session_id);
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
    match reduce_rewind_key(rewind, key) {
        RewindEffect::None => {}
        RewindEffect::Cancel => state.rewind = None,
        RewindEffect::Scroll(delta) if delta.is_negative() => {
            state.scroll_up(delta.unsigned_abs());
        }
        RewindEffect::Scroll(delta) => {
            state.scroll_down(delta.unsigned_abs());
        }
        RewindEffect::Accept {
            message_index,
            restore_files,
        } => accept_rewind(
            runtime,
            state,
            controls,
            composer,
            message_index,
            restore_files,
        ),
    }
}

fn accept_rewind(
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    message_index: usize,
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
            "messageIndex": message_index,
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
    if let Some(session_id) = metadata_session_id(&result)
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
