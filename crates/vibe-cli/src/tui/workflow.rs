use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

mod config;
mod mcp;

use super::chat_input::ChatInputState;
use super::clipboard::{SystemClipboard, SystemClipboardPort};
use super::commands::{CommandId, parse_command_in};
use super::controls::ControlState;
use super::input::prepare_submission;
use super::interaction::{Overlay, OverlayKind};
use super::pickers::{
    config_overlay, debug_overlay, help_overlay, model_overlay, proxy_overlay, rewind_overlay,
    sessions_overlay, status_overlay, theme_overlay, thinking_overlay, voice_overlay,
};
use super::setup::ResolvedTheme;
use super::state::{EntryStatus, TranscriptKind, TuiState};
use super::{
    ActiveTurn, Arguments, CliError, InteractiveRuntime, RuntimeSkill, adopt_hydrated_session,
    call_runtime, metadata_session_id, parse_runtime_skills, persist_user_setting,
    push_local_notice, refresh_server_banner_metrics, start_active_turn, start_shell,
    sync_runtime_intent, update_theme,
};
pub(super) use config::apply_thinking;
use config::{
    apply_render_preferences, configured_value, persist_config_setting, reset_config_value,
    reset_config_value_at, selected_config_target, set_config_value,
};
use mcp::{handle_mcp, refresh_selected_mcp, show_mcp, source_enabled as mcp_source_enabled};

const DATA_RETENTION_MESSAGE: &str = "\
## Your Data Helps Improve Mistral AI

At Mistral AI, we're committed to delivering the best possible experience. When you use Mistral models on our API, your interactions may be collected to improve our models, ensuring they stay cutting-edge, accurate, and helpful.

Manage your data settings [here](https://chat.mistral.ai/work?profile_dialog=privacy)";

pub(super) enum CommandAction {
    Unhandled,
    Handled,
    Exit,
    Setup,
    Runtime(RuntimeCommand),
}

pub(super) struct RuntimeCommand {
    pub id: CommandId,
    pub arguments: String,
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
    turn_active: bool,
) -> Result<CommandAction, CliError> {
    let command_context = composer.command_context().clone();
    let Some(parsed) = parse_command_in(command_line, &command_context) else {
        return Ok(CommandAction::Unhandled);
    };
    let command_id = parsed.id;
    let command_arguments = parsed.arguments.to_owned();
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
            paste_clipboard_image(working_directory, composer, state);
            return Ok(CommandAction::Handled);
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
    if turn_active && command_id.changes_session_projection() {
        state.push_diagnostic(
            "Finish or interrupt the active turn before changing the session projection",
        );
        return Ok(CommandAction::Handled);
    }

    match command_id {
        CommandId::Config => {
            show_config(runtime, state);
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
                refresh_server_banner_metrics(&mut runtime.service, &mut runtime.banner);
                apply_render_preferences(runtime, state);
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
            show_proxy(runtime, state);
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
            Ok(CommandAction::Handled)
        }
        CommandId::Settings if command_arguments.starts_with("reset ") => {
            reset_config_value(&command_arguments[6..], runtime, state);
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
pub(super) fn handle_overlay_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    theme: &mut ResolvedTheme,
) -> bool {
    let Some(kind) = state.overlay.as_ref().map(|overlay| overlay.kind) else {
        return false;
    };
    match key.code {
        KeyCode::Esc => {
            state.overlay = None;
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(-1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.move_selection(1);
            }
        }
        KeyCode::Backspace if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.pop_query();
            }
        }
        KeyCode::Delete if kind == OverlayKind::Sessions => {
            delete_selected_session(runtime, state);
        }
        KeyCode::Char('r')
            if kind == OverlayKind::Mcp && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            refresh_selected_mcp(runtime, state);
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
            select_overlay_item(runtime, state, controls, composer, theme);
        }
        KeyCode::Char(character) if key.modifiers.is_empty() => {
            if let Some(overlay) = state.overlay.as_mut() {
                overlay.push_query(character);
            }
        }
        _ => {}
    }
    true
}

pub(super) async fn start_next_queued_prompt(
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
) -> Result<(), CliError> {
    if active.is_some()
        || runtime
            .as_ref()
            .is_some_and(|runtime| runtime.shell.is_some())
        || controls.pending_callback().is_some()
    {
        return Ok(());
    }
    if let Some(prompt) = state.prompt_queue.pop_next() {
        if prompt.trim_start().starts_with('!') {
            if start_shell(&prompt, runtime, state).await? {
                return Ok(());
            }
            state.prompt_queue.push_front(prompt);
            state.prompt_queue.pause();
            return Ok(());
        }
        if start_prompt(
            working_directory,
            prompt.clone(),
            runtime,
            active,
            state,
            controls,
        )
        .await?
        {
            return Ok(());
        }
        state.prompt_queue.push_front(prompt);
        state.prompt_queue.pause();
    }
    Ok(())
}

pub(super) async fn start_prompt(
    working_directory: &Path,
    prompt: String,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
) -> Result<bool, CliError> {
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before starting a session. Restart with --setup.");
        return Ok(false);
    };
    let mut prepared = match prepare_submission(working_directory, &prompt) {
        Ok(prepared) => prepared,
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return Ok(false);
        }
    };
    for path in prepared.cleanup_paths.drain(..) {
        match std::fs::remove_file(&path) {
            Ok(()) => state.release_transient_path(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                state.release_transient_path(&path);
            }
            Err(error) => state.push_diagnostic(format!(
                "Clipboard image cleanup failed for `{}`: {error}",
                path.display()
            )),
        }
    }
    if let Some(skill) = invoked_skill(runtime, &prompt) {
        prepared
            .turn
            .input
            .push(vibe_app_server::client::PublicContentBlock::Resource {
                resource: json!({
                    "resource": {
                        "uri": skill.path.as_deref().unwrap_or("skill://invoked"),
                        "name": skill.name,
                        "text": skill.body,
                    }
                }),
            });
    }
    let reservation = runtime
        .service
        .reserve_prompt(&runtime.session_id, &prepared.turn)
        .await?;
    controls
        .begin_turn(&reservation.turn_id)
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    *active = Some(start_active_turn(
        &runtime.service,
        reservation,
        None,
        state.watermark,
    )?);
    state.waiting = true;
    Ok(true)
}

pub(super) fn is_user_skill(runtime: Option<&InteractiveRuntime>, input: &str) -> bool {
    runtime
        .and_then(|runtime| invoked_skill(runtime, input))
        .is_some()
}

fn invoked_skill<'a>(runtime: &'a InteractiveRuntime, input: &str) -> Option<&'a RuntimeSkill> {
    let name = input
        .trim()
        .strip_prefix('/')?
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();
    runtime.skills.get(&name)
}

pub(super) fn cycle_agent(runtime: &mut Option<InteractiveRuntime>, state: &mut TuiState) {
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
    let next = agents[(current + 1) % agents.len()];
    if call_runtime(
        runtime,
        "session/agent/update",
        json!({"sessionId": runtime.session_id, "name": next}),
        state,
    )
    .is_some()
    {
        sync_runtime_intent(runtime, Some(next));
        push_local_notice(
            state,
            &format!("Switched to agent `{next}`"),
            EntryStatus::Completed,
        );
    }
}

fn select_overlay_item(
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    composer: &mut ChatInputState,
    theme: &mut ResolvedTheme,
) {
    let Some((kind, item)) = state.overlay.as_ref().and_then(|overlay| {
        overlay
            .selected_item()
            .cloned()
            .map(|item| (overlay.kind, item))
    }) else {
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        state.overlay = None;
        return;
    };
    match kind {
        OverlayKind::Config => match item.id.as_str() {
            "active_model" => show_model(runtime, state),
            "thinking" => state.overlay = Some(thinking_overlay(&runtime.thinking)),
            "theme" => {
                let current = configured_value(runtime, "theme")
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "system".to_owned());
                state.overlay = Some(theme_overlay(&current));
            }
            key if item.description.contains("boolean") => {
                let current = configured_value(runtime, key)
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let path = key.split('.').collect::<Vec<_>>();
                let target = selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
                if persist_config_setting(runtime, &target, &path, json!(!current), false, state) {
                    show_config(runtime, state);
                }
            }
            key => {
                let target = selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
                composer.replace_text(format!("/settings set --target {target} {key} "));
                state.overlay = None;
            }
        },
        OverlayKind::Model => {
            if persist_user_setting(runtime, &["active_model"], json!(item.id), false, state) {
                let model = item.id;
                if call_runtime(
                    runtime,
                    "session/settings/update",
                    json!({"sessionId": runtime.session_id, "model": model}),
                    state,
                )
                .is_some()
                {
                    runtime.model = model;
                    state.overlay = None;
                    push_local_notice(
                        state,
                        "Model updated for this session and future sessions",
                        EntryStatus::Completed,
                    );
                }
            }
        }
        OverlayKind::Thinking => {
            apply_thinking(runtime, &item.id, state);
            state.overlay = None;
        }
        OverlayKind::Theme => {
            update_theme(runtime, &item.id, state, theme);
            state.overlay = None;
        }
        OverlayKind::Sessions => {
            if let Some(result) = call_runtime(
                runtime,
                "session/resume",
                json!({"sessionId": item.id}),
                state,
            ) && let Some(session_id) = metadata_session_id(&result)
                && adopt_hydrated_session(runtime, state, controls, session_id)
            {
                state.overlay = None;
                push_local_notice(state, "Resumed session", EntryStatus::Completed);
            }
        }
        OverlayKind::Mcp | OverlayKind::Connectors => {
            let enabled = mcp_source_enabled(runtime, &item.id, state).unwrap_or(true);
            if call_runtime(
                runtime,
                "mcp/toggle",
                json!({
                    "sessionId": runtime.session_id,
                    "name": item.id,
                    "disabled": enabled,
                }),
                state,
            )
            .is_some()
            {
                show_mcp(runtime, state, None);
            }
        }
        OverlayKind::Voice => {
            let current = configured_value(runtime, &item.id)
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if persist_user_setting(runtime, &[&item.id], json!(!current), false, state) {
                show_voice(runtime, state);
            }
        }
        OverlayKind::Proxy => {
            let command = if item.id == "tls_ca_path" {
                "/settings certificate "
            } else {
                "/settings proxy "
            };
            composer.replace_text(command);
            state.overlay = None;
        }
        OverlayKind::Rewind => {
            let Ok(message_index) = item.id.parse::<usize>() else {
                state.push_diagnostic("The selected rewind point is invalid");
                return;
            };
            if let Some(result) = call_runtime(
                runtime,
                "session/rewind",
                json!({
                    "sessionId": runtime.session_id,
                    "messageIndex": message_index,
                    "restoreFiles": false,
                }),
                state,
            ) {
                let message = result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(session_id) = metadata_session_id(&result)
                    && adopt_hydrated_session(runtime, state, controls, session_id)
                {
                    composer.replace_text(message);
                    state.overlay = None;
                    push_local_notice(
                        state,
                        "Rewound into a new branch; the original session was preserved",
                        EntryStatus::Completed,
                    );
                }
            }
        }
        OverlayKind::Help
        | OverlayKind::Debug
        | OverlayKind::Status
        | OverlayKind::DataRetention => {}
    }
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
    let Some(snapshot) = call_runtime(runtime, "config/read", json!({}), state)
        .and_then(|result| result.get("snapshot").cloned())
    else {
        return;
    };
    let schema = call_runtime(runtime, "config/schema", json!({}), state)
        .and_then(|result| result.get("schema").cloned())
        .unwrap_or(Value::Null);
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
    let overlay = rewind_overlay(&map_value(result));
    if overlay.items.is_empty() {
        push_local_notice(
            state,
            "There are no user messages to rewind.",
            EntryStatus::Completed,
        );
    } else {
        state.overlay = Some(overlay);
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
    if let Some(snapshot) = call_runtime(runtime, "config/read", json!({}), state)
        .and_then(|result| result.get("snapshot").cloned())
    {
        state.overlay = Some(proxy_overlay(&snapshot));
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
        return;
    }
    if let Some(result) = call_runtime(
        runtime,
        "diagnostics/logs/read",
        json!({"offset": 0, "limit": 100}),
        state,
    ) {
        state.overlay = Some(debug_overlay(&map_value(result)));
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

fn paste_clipboard_image(
    working_directory: &Path,
    composer: &mut ChatInputState,
    state: &mut TuiState,
) {
    match SystemClipboard.paste_image(working_directory) {
        Ok(Some(path)) => {
            let prefix = if composer.editor().text().is_empty()
                || composer
                    .editor()
                    .text()
                    .chars()
                    .last()
                    .is_some_and(char::is_whitespace)
            {
                ""
            } else {
                " "
            };
            state.track_transient_path(path.clone());
            let canonical_workspace = std::fs::canonicalize(working_directory)
                .unwrap_or_else(|_| working_directory.to_path_buf());
            let path = path
                .strip_prefix(&canonical_workspace)
                .unwrap_or(&path)
                .to_string_lossy();
            let token = format!("{prefix}@'{path}' ");
            composer.insert_adapter_text(&token);
            push_local_notice(
                state,
                "Clipboard image added to prompt",
                EntryStatus::Completed,
            );
        }
        Ok(None) => state.push_diagnostic("No image found on the clipboard"),
        Err(error) => state.push_diagnostic(error.to_string()),
    }
}

fn delete_selected_session(runtime: &mut Option<InteractiveRuntime>, state: &mut TuiState) {
    let Some(session_id) = state
        .overlay
        .as_ref()
        .and_then(Overlay::selected_item)
        .map(|item| item.id.clone())
    else {
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if session_id == runtime.session_id {
        state.push_diagnostic("Deleting the current session is not supported.");
        return;
    }
    if call_runtime(
        runtime,
        "session/delete",
        json!({"sessionId": session_id}),
        state,
    )
    .is_some()
        && let Some(overlay) = state.overlay.as_mut()
    {
        overlay.items.retain(|item| item.id != session_id);
        overlay.set_query(overlay.query.clone());
    }
}

fn map_value(map: std::collections::BTreeMap<String, Value>) -> Value {
    Value::Object(map.into_iter().collect())
}
