pub mod clipboard;
pub mod commands;
pub mod controls;
pub mod input;
pub mod interaction;
pub mod pickers;
pub mod render;
pub mod setup;
mod shell;
mod shortcuts;
pub mod state;
pub mod terminal;
mod workflow;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use vibe_app_server::client::{
    DriverError, HeadlessService, LiveDriverConfig, LiveTurnDriver, ProgrammaticUpdate,
    PublicCallbackState, PublicContentBlock, PublicDispatch, PublicHistoryEntry, PublicMessageRole,
    PublicTurnOutcome, SessionOptions, TurnDriver, TurnReservation,
};
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_app_server::release4::{Release4Service, VibeCodeCloudConfig};
use vibe_app_server::server::AppServer;

use self::commands::CommandId;
use self::controls::{
    ApprovalScope, CallbackChoice, CallbackKind, CallbackOption, CallbackQuestion, ControlState,
    PendingCallback, UserInputChoice,
};
use self::input::{
    CompletionAction, CompletionEngine, ExternalEditorPort, PromptEditor, SystemExternalEditor,
    normalize_pasted_text,
};
use self::render::{BannerContext, TokenState, UiContext, draw};
use self::setup::{
    CredentialStore, EnvironmentThemeDetector, NativeCredentialStore, NotificationPreference,
    ResolvedTheme, SetupCompletion, SetupFlow, SetupProgress, TerminalThemeDetector, Theme,
    resolve_theme,
};
use self::shell::{ActiveShell, finish_shell, interrupt_shell, start_shell};
use self::shortcuts::{copy_prompt_selection, resume_paused_queue};
use self::state::{
    ApplyResult, EntryStatus, ServerEvent, TranscriptEntry, TranscriptKind, TuiSnapshot, TuiState,
};
use self::terminal::{CrosstermOps, TerminalGuard};
use self::workflow::{
    CommandAction, RuntimeCommand, apply_thinking, cycle_agent, dispatch_command,
    handle_overlay_key, is_user_skill, show_rewind, start_next_queued_prompt, start_prompt,
};
use crate::{
    Arguments, CliError, CliTelemetryObserver, price_per_million_micros, telemetry_event_observer,
    validate_arguments,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const INITIAL_HISTORY_LIMIT: usize = 200;
const DEFAULT_MODEL: &str = "mistral-medium-3.5";
const MAX_CALLBACK_DETAIL_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_QUESTIONS: usize = 16;
const MAX_CALLBACK_OPTIONS: usize = 32;
const MAX_CALLBACK_TEXT_BYTES: usize = 8 * 1024;
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

#[derive(Debug, Deserialize)]
struct PublicHistoryList {
    history: Vec<PersistedMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum PersistedMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
    },
    Tool {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

struct ActiveTurn {
    turn_id: String,
    scheduled_loop_id: Option<String>,
    cancel_requested: bool,
    updates: tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
    task: JoinHandle<(TurnReservation, Result<PublicTurnOutcome, DriverError>)>,
}

#[derive(Debug, Clone)]
struct RuntimeSkill {
    name: String,
    description: String,
    body: String,
    path: Option<String>,
}

struct InteractiveRuntime {
    service: HeadlessService<LiveTurnDriver>,
    session_id: String,
    model: String,
    thinking: String,
    mode: String,
    agent_name: String,
    banner: BannerMetrics,
    context_tokens: u64,
    context_window: u64,
    auto_approve: bool,
    clear_context_after_turn: bool,
    skills: BTreeMap<String, RuntimeSkill>,
    shell: Option<ActiveShell>,
    cloud: CloudWorkflowState,
    telemetry: Option<Arc<CliTelemetryObserver>>,
}

#[derive(Debug, Clone)]
struct BannerMetrics {
    models_count: usize,
    skills_count: usize,
    mcp_servers_enabled: usize,
    mcp_servers_total: usize,
    connectors_connected: usize,
    connectors_total: usize,
    hooks_count: usize,
    plan: Option<String>,
}

impl Default for BannerMetrics {
    fn default() -> Self {
        Self {
            models_count: 1,
            skills_count: 0,
            mcp_servers_enabled: 0,
            mcp_servers_total: 0,
            connectors_connected: 0,
            connectors_total: 0,
            hooks_count: 0,
            plan: None,
        }
    }
}

#[derive(Default)]
struct CloudWorkflowState {
    picker_id: Option<String>,
    project_id: Option<String>,
    teleport_operation_id: Option<String>,
}

pub async fn run_interactive(arguments: Arguments) -> Result<(), CliError> {
    validate_arguments(&arguments)?;
    let working_directory = match &arguments.workdir {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(CliError::CurrentDirectory)?,
    };
    let credential_store = NativeCredentialStore::new("mistral-vibe-rs");
    let (initial_credential, keyring_error) = if arguments.setup {
        (None, None)
    } else {
        let environment_credential = std::env::var(&arguments.credential_environment)
            .ok()
            .filter(|credential| !credential.is_empty());
        match environment_credential {
            Some(credential) => (Some(credential), None),
            None => match credential_store.get(&arguments.credential_environment) {
                Ok(credential) => (credential, None),
                Err(error) => (None, Some(error.to_string())),
            },
        }
    };
    let mut runtime = initial_credential
        .map(|credential| start_runtime(&arguments, &working_directory, credential))
        .transpose()?;
    let fallback_banner = runtime.as_ref().map_or_else(
        || local_banner_metrics(&arguments, &working_directory),
        |runtime| runtime.banner.clone(),
    );
    let session_id = runtime
        .as_ref()
        .map_or_else(|| "setup".to_owned(), |runtime| runtime.session_id.clone());
    let mut state = match runtime.as_mut() {
        Some(runtime) => hydrate_initial_state(runtime, &arguments, &working_directory)?,
        None => {
            let mut state = TuiState::new(session_id.clone());
            state.ready = true;
            state
        }
    };
    if let Some(error) = keyring_error {
        state.push_diagnostic(format!(
            "Native credential lookup failed: {error}. Run /setup after repairing keyring access"
        ));
    }
    if runtime.is_none() {
        push_local_notice(
            &mut state,
            "Setup is required before starting a session. Enter /setup to store an API key in the native keyring.",
            EntryStatus::Completed,
        );
    }
    if arguments.check_upgrade {
        state.push_diagnostic(
            "Upgrade discovery is unavailable in this build; the current session can continue",
        );
    }
    let mut controls = ControlState::new(session_id);
    if let Some(runtime) = runtime.as_mut() {
        sync_active_callbacks(runtime, &mut state, &mut controls);
    }
    let mut editor = PromptEditor::default();
    let mut completion = CompletionEngine::default();
    if let Some(runtime) = runtime.as_ref() {
        completion.set_user_skills(
            runtime
                .skills
                .values()
                .map(|skill| (skill.name.as_str(), skill.description.as_str())),
        );
    }
    let mut setup_flow = arguments.setup.then(|| new_setup_flow(&arguments));
    let mut secret_input = false;
    if let Some(setup) = &setup_flow {
        push_local_notice(&mut state, &setup.prompt(), EntryStatus::Completed);
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let detected_theme = EnvironmentThemeDetector.detect();
    let mut theme = resolve_theme(
        configured_theme(runtime.as_mut()).unwrap_or(Theme::System),
        detected_theme,
        no_color,
    );
    let mut terminal_guard = TerminalGuard::enter(CrosstermOps::stdout())
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|error| CliError::Terminal(error.to_string()))?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut loop_ticker = tokio::time::interval(Duration::from_secs(1));
    loop_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut active: Option<ActiveTurn> = None;

    let event_loop = async {
        let mut exit = false;
        while !exit {
            drain_updates(&mut state, runtime.as_mut(), active.as_mut(), &mut controls);
            drain_callback_requests(runtime.as_mut(), &mut state, &mut controls);
            finish_active(&mut state, &mut controls, &mut runtime, &mut active).await?;
            finish_shell(runtime.as_mut(), &mut state).await;
            start_next_queued_prompt(
                &working_directory,
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
            )
            .await?;
            if let Err(error) = completion.poll() {
                completion.cancel();
                state.push_diagnostic(error.to_string());
            }
            let runtime_view = runtime.as_ref();
            let agent_name =
                runtime_view.map_or("default", |runtime| runtime.agent_name.as_str());
            let border_title = format!(" {} ", agent_name.to_lowercase());
            let banner = runtime_view.map_or(&fallback_banner, |runtime| &runtime.banner);
            let model =
                runtime_view.map_or(arguments.model.as_str(), |runtime| runtime.model.as_str());
            let thinking = runtime_view.map_or("off", |runtime| runtime.thinking.as_str());
            let tokens = runtime_view.map_or(
                TokenState {
                    max_tokens: arguments.max_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW),
                    current_tokens: 0,
                },
                |runtime| TokenState {
                    max_tokens: runtime.context_window,
                    current_tokens: runtime.context_tokens,
                },
            );
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &mut state,
                        &editor,
                        &completion,
                        theme,
                        UiContext {
                            cwd: &working_directory,
                            agent_name: &border_title,
                            secret_input,
                            banner: BannerContext {
                                version: env!("CARGO_PKG_VERSION"),
                                model,
                                thinking,
                                models_count: banner.models_count,
                                skills_count: banner.skills_count,
                                mcp_servers_enabled: banner.mcp_servers_enabled,
                                mcp_servers_total: banner.mcp_servers_total,
                                connectors_connected: banner.connectors_connected,
                                connectors_total: banner.connectors_total,
                                hooks_count: banner.hooks_count,
                                plan: banner.plan.as_deref(),
                            },
                            tokens,
                        },
                    );
                })
                .map_err(|error| CliError::Terminal(error.to_string()))?;
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal.map_err(|error| CliError::Terminal(error.to_string()))?;
                    if let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_mut()) {
                        if active.cancel_requested {
                            exit = true;
                        } else {
                            let _ = controls.interrupt();
                            runtime.service.interrupt(&runtime.session_id, &active.turn_id)?;
                            active.cancel_requested = true;
                            state.push_diagnostic("SIGINT requested turn cancellation");
                        }
                    } else if let Some(runtime) = runtime.as_mut()
                        && runtime.shell.is_some()
                    {
                        interrupt_shell(runtime, &mut state).await;
                    } else {
                        exit = true;
                    }
                }
                event = events.next() => {
                    match event {
                        Some(Ok(event)) => {
                            exit = handle_terminal_event(
                                event,
                                &arguments,
                                &working_directory,
                                &credential_store,
                                &mut runtime,
                                &mut active,
                                &mut state,
                                &mut controls,
                                &mut editor,
                                &mut completion,
                                &mut setup_flow,
                                &mut secret_input,
                                &mut theme,
                                &mut terminal_guard,
                                &mut terminal,
                            ).await?;
                        }
                        Some(Err(error)) => {
                            state.apply(ServerEvent::TransportLost(error.to_string()))
                                .map_err(|error| CliError::Terminal(error.to_string()))?;
                            exit = true;
                        }
                        None => {
                            state.apply(ServerEvent::TransportLost(
                                "Terminal input ended; recoverable session state was preserved".to_owned(),
                            )).map_err(|error| CliError::Terminal(error.to_string()))?;
                            exit = true;
                        }
                    }
                }
                _ = ticker.tick() => {}
                _ = loop_ticker.tick() => {
                    if active.is_none()
                        && runtime.as_ref().is_none_or(|runtime| runtime.shell.is_none())
                        && let Some(runtime) = runtime.as_mut()
                        && let Some(scheduled) = runtime
                            .service
                            .reserve_due_loop(&runtime.session_id, unix_seconds())
                            .await?
                    {
                        let message = scheduled
                            .notice
                            .params
                            .get("entry")
                            .and_then(|entry| entry.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("Scheduled loop fired");
                        push_local_notice(&mut state, message, EntryStatus::Completed);
                        controls
                            .begin_turn(&scheduled.reservation.turn_id)
                            .map_err(|error| CliError::Terminal(error.to_string()))?;
                        active = Some(start_active_turn(
                            &runtime.service,
                            scheduled.reservation,
                            Some(scheduled.loop_id),
                            state.watermark,
                        )?);
                        state.waiting = true;
                    }
                }
            }
        }
        Ok::<(), CliError>(())
    }
    .await;

    drop(terminal);
    let restoration = terminal_guard
        .restore()
        .map_err(|error| CliError::Terminal(error.to_string()));
    let interrupt_result =
        if let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_ref()) {
            runtime
                .service
                .interrupt(&runtime.session_id, &active.turn_id)
                .map_err(CliError::from)
        } else {
            Ok(())
        };
    if let Some(mut active) = active
        && tokio::time::timeout(Duration::from_secs(2), &mut active.task)
            .await
            .is_err()
    {
        active.task.abort();
        let _ = active.task.await;
    }
    if let Some(runtime) = runtime.as_mut() {
        interrupt_shell(runtime, &mut state).await;
        finish_shell(Some(runtime), &mut state).await;
    }
    let mut cleanup_error = None;
    for path in state.take_transient_paths() {
        if let Err(error) = std::fs::remove_file(&path)
            && cleanup_error.is_none()
        {
            cleanup_error = Some(CliError::Terminal(format!(
                "clipboard image cleanup failed for `{}`: {error}",
                path.display()
            )));
        }
    }
    let cleanup_result = cleanup_error.map_or(Ok(()), Err);
    let telemetry = runtime
        .as_ref()
        .and_then(|runtime| runtime.telemetry.clone());
    let (close_result, shutdown_result) = if let Some(mut runtime) = runtime {
        let session_id = runtime.session_id.clone();
        let close = runtime
            .service
            .close_session(&session_id)
            .await
            .map_err(CliError::from);
        let shutdown = runtime.service.shutdown().map_err(CliError::from);
        (close, shutdown)
    } else {
        (Ok(()), Ok(()))
    };
    let result = event_loop
        .and(restoration)
        .and(interrupt_result)
        .and(cleanup_result)
        .and(close_result)
        .and(shutdown_result);
    if let Some(telemetry) = telemetry {
        telemetry.flush().await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn handle_terminal_event(
    event: Event,
    arguments: &Arguments,
    working_directory: &Path,
    credential_store: &dyn CredentialStore,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    editor: &mut PromptEditor,
    completion: &mut CompletionEngine,
    setup_flow: &mut Option<SetupFlow>,
    secret_input: &mut bool,
    theme: &mut ResolvedTheme,
    terminal_guard: &mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<bool, CliError> {
    match event {
        Event::Resize(width, height) => state.resize(width, height),
        Event::Paste(text) => {
            let text = normalize_pasted_text(working_directory, &text);
            if let Err(error) = editor.paste(&text) {
                state.push_diagnostic(error.to_string());
            }
            refresh_completion(completion, editor, working_directory, *secret_input, state);
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                state.scroll_up(3);
                page_older_history(runtime.as_mut(), state);
            }
            MouseEventKind::ScrollDown => {
                state.scroll_down(3);
            }
            _ => {}
        },
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            return handle_key(
                key,
                arguments,
                working_directory,
                credential_store,
                runtime,
                active,
                state,
                controls,
                editor,
                completion,
                setup_flow,
                secret_input,
                theme,
                terminal_guard,
                terminal,
            )
            .await;
        }
        Event::FocusGained | Event::FocusLost | Event::Key(_) => {}
    }
    Ok(false)
}

fn start_active_turn(
    service: &HeadlessService<LiveTurnDriver>,
    reservation: TurnReservation,
    scheduled_loop_id: Option<String>,
    event_id: u64,
) -> Result<ActiveTurn, CliError> {
    let (observer, updates) = service.interactive_update_channel_after(
        &reservation.session_id,
        &reservation.turn_id,
        event_id,
    )?;
    let driver = service.driver();
    let turn_id = reservation.turn_id.clone();
    let task = tokio::spawn(async move {
        let outcome = driver.run_observed(&reservation, observer).await;
        (reservation, outcome)
    });
    Ok(ActiveTurn {
        turn_id,
        scheduled_loop_id,
        cancel_requested: false,
        updates,
        task,
    })
}

fn drain_callback_requests(
    runtime: Option<&mut InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let entries = match runtime.service.drain_callbacks() {
        Ok(entries) => entries,
        Err(error) => {
            state.push_diagnostic(format!("Interactive callbacks are unavailable: {error}"));
            return;
        }
    };
    if entries.is_empty() {
        return;
    }
    resync_current_projection(runtime, state);
    for entry in entries {
        let pending = match pending_callback_from_entry(&entry) {
            Ok(Some(pending)) => pending,
            Ok(None) => continue,
            Err(error) => {
                state.push_diagnostic(format!("Interactive callback is invalid: {error}"));
                continue;
            }
        };
        if controls.contains_callback(&pending.callback_id) {
            continue;
        }
        if controls.active_turn_id.is_none()
            && let Err(error) = controls.begin_turn(&pending.turn_id)
        {
            state.push_diagnostic(error.to_string());
            continue;
        }
        if let Err(error) = controls.present_callback(pending.clone()) {
            state.push_diagnostic(error.to_string());
            continue;
        }
        let suffix = if pending.kind == CallbackKind::Approval {
            " Use /approve [once|always|permanent] or /deny."
        } else {
            ""
        };
        push_local_notice(
            state,
            &format!("{}{suffix}", pending.prompt),
            EntryStatus::Streaming,
        );
        if pending.kind == CallbackKind::PlanReview && runtime.mode != "plan" {
            state.push_diagnostic("Rejected exit-plan callback outside plan mode");
            respond_to_pending_callback(runtime, controls, &pending, CallbackChoice::Cancel, state);
        }
    }
}

fn respond_to_pending_callback(
    runtime: &mut InteractiveRuntime,
    controls: &mut ControlState,
    pending: &PendingCallback,
    choice: CallbackChoice,
    state: &mut TuiState,
) {
    if pending.kind == CallbackKind::PlanReview
        && runtime.mode != "plan"
        && choice != CallbackChoice::Cancel
    {
        state.push_diagnostic("Exit plan mode is only valid while the session is in plan mode");
        return;
    }
    let plan_transition = (pending.kind == CallbackKind::PlanReview)
        .then(|| plan_transition(&choice))
        .flatten();
    let dispatch = match controls.prepare_answer(&pending.turn_id, &pending.callback_id, &choice) {
        Ok(dispatch) => dispatch,
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return;
        }
    };
    match runtime.service.respond_callback(dispatch.params.clone()) {
        Ok(_) => {
            if let Err(error) =
                controls.accept_answer(&pending.turn_id, &pending.callback_id, choice, &dispatch)
            {
                state.push_diagnostic(format!(
                    "Server accepted the callback, but the local control state diverged: {error}"
                ));
                resync_current_projection(runtime, state);
                sync_active_callbacks(runtime, state, controls);
                return;
            }
            if let Some((clear_context, auto_approve)) = plan_transition {
                let settings = runtime.service.public_call(
                    "session/settings/update",
                    json!({
                        "sessionId": runtime.session_id,
                        "mode": "code",
                        "autoApprove": auto_approve,
                    }),
                );
                if let Err(error) = settings {
                    state.push_diagnostic(format!(
                        "Plan response was accepted, but the session transition failed: {error}"
                    ));
                } else {
                    runtime.mode = "code".to_owned();
                    runtime.auto_approve = auto_approve;
                    runtime.clear_context_after_turn |= clear_context;
                }
            }
            push_local_notice(state, "Callback response accepted", EntryStatus::Completed);
            resync_current_projection(runtime, state);
            sync_active_callbacks(runtime, state, controls);
        }
        Err(error) => {
            recover_from_callback_response_error(runtime, controls, state, error);
        }
    }
}

fn recover_from_callback_response_error(
    runtime: &mut InteractiveRuntime,
    controls: &mut ControlState,
    state: &mut TuiState,
    error: impl std::fmt::Display,
) {
    state.push_diagnostic(format!("Callback response was rejected: {error}"));
    // The driver can fail after the server commits the response. Canonical state
    // decides whether the callback is still actionable.
    resync_current_projection(runtime, state);
    sync_active_callbacks(runtime, state, controls);
}

fn plan_transition(choice: &CallbackChoice) -> Option<(bool, bool)> {
    match choice {
        CallbackChoice::Option { id } if id == "clear_auto" => Some((true, true)),
        CallbackChoice::Option { id } if id == "auto" => Some((false, true)),
        CallbackChoice::Option { id } if id == "manual" => Some((false, false)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    arguments: &Arguments,
    working_directory: &Path,
    credential_store: &dyn CredentialStore,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    editor: &mut PromptEditor,
    completion: &mut CompletionEngine,
    setup_flow: &mut Option<SetupFlow>,
    secret_input: &mut bool,
    theme: &mut ResolvedTheme,
    terminal_guard: &mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<bool, CliError> {
    if handle_overlay_key(key, runtime, state, controls, editor, theme) {
        completion.cancel();
        return Ok(false);
    }
    if key.code != KeyCode::Esc {
        state.rewind_confirmation.cancel();
    }
    if !*secret_input && completion.view().is_some() {
        match key.code {
            KeyCode::Esc => {
                completion.cancel();
                return Ok(false);
            }
            KeyCode::Up if key.modifiers.is_empty() => {
                completion.move_selection(-1);
                return Ok(false);
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                completion.move_selection(1);
                return Ok(false);
            }
            KeyCode::Tab if key.modifiers.is_empty() => {
                if let Err(error) = completion.accept(editor) {
                    state.push_diagnostic(error.to_string());
                }
                return Ok(false);
            }
            KeyCode::Enter if key.modifiers.is_empty() => match completion.accept(editor) {
                Ok(CompletionAction::Applied) => return Ok(false),
                Ok(CompletionAction::Submit) => {}
                Err(error) => {
                    state.push_diagnostic(error.to_string());
                    return Ok(false);
                }
            },
            _ => {}
        }
    }
    if key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
    {
        completion.cancel();
        cycle_agent(runtime, state);
        return Ok(false);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        completion.cancel();
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                copy_prompt_selection(editor, state);
                return Ok(false);
            }
            KeyCode::Char('c') => {
                if !editor.text().is_empty() {
                    editor.set_text("");
                    state.quit_confirmation.cancel();
                    return Ok(false);
                }
                if let Some(cancelled) = state.prompt_queue.cancel_last() {
                    state.push_diagnostic(format!(
                        "Removed queued prompt: {}",
                        cancelled.lines().next().unwrap_or_default()
                    ));
                    return Ok(false);
                }
                if let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_mut()) {
                    if !active.cancel_requested {
                        let _ = controls.interrupt();
                        runtime
                            .service
                            .interrupt(&runtime.session_id, &active.turn_id)?;
                        active.cancel_requested = true;
                        state.prompt_queue.pause();
                        state.push_diagnostic("Turn cancellation requested; queued prompts paused");
                    }
                    return Ok(false);
                }
                if let Some(runtime) = runtime.as_mut()
                    && runtime.shell.is_some()
                {
                    interrupt_shell(runtime, state).await;
                    state.prompt_queue.pause();
                    return Ok(false);
                }
                if state.quit_confirmation.request("Ctrl+C", unix_millis()) {
                    return Ok(true);
                }
                state.push_diagnostic("Press Ctrl+C again within one second to quit");
                return Ok(false);
            }
            KeyCode::Char('d') => {
                if !editor.text().is_empty() {
                    editor.delete_forward();
                    refresh_completion(completion, editor, working_directory, *secret_input, state);
                    return Ok(false);
                }
                if state.quit_confirmation.request("Ctrl+D", unix_millis()) {
                    return Ok(true);
                }
                state.push_diagnostic("Press Ctrl+D again within one second to quit");
                return Ok(false);
            }
            KeyCode::Char('o') => {
                state.tools_collapsed = !state.tools_collapsed;
                state.push_diagnostic(if state.tools_collapsed {
                    "Tool output collapsed"
                } else {
                    "Tool output expanded"
                });
                return Ok(false);
            }
            KeyCode::Char('y') => {
                copy_prompt_selection(editor, state);
                return Ok(false);
            }
            KeyCode::Char('g') => {
                if *secret_input {
                    state.push_diagnostic("External editing is disabled while entering a secret");
                    return Ok(false);
                }
                terminal_guard
                    .restore()
                    .map_err(|error| CliError::Terminal(error.to_string()))?;
                let mut external = SystemExternalEditor::from_environment();
                let edited = ExternalEditorPort::edit(&mut external, editor.text());
                terminal_guard
                    .resume()
                    .map_err(|error| CliError::Terminal(error.to_string()))?;
                terminal
                    .clear()
                    .map_err(|error| CliError::Terminal(error.to_string()))?;
                match edited {
                    Ok(edited) => editor.set_text(edited),
                    Err(error) => state.push_diagnostic(error),
                }
                refresh_completion(completion, editor, working_directory, *secret_input, state);
                return Ok(false);
            }
            KeyCode::Char('a') => editor.move_home(false),
            KeyCode::Char('e') => editor.move_end(false),
            KeyCode::Char('j') => editor.insert("\n"),
            _ => {}
        }
        refresh_completion(completion, editor, working_directory, *secret_input, state);
        return Ok(false);
    }
    match key.code {
        KeyCode::Esc => {
            if let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_mut()) {
                let _ = controls.interrupt();
                runtime
                    .service
                    .interrupt(&runtime.session_id, &active.turn_id)?;
                active.cancel_requested = true;
                state.prompt_queue.pause();
                state.push_diagnostic("Turn cancellation requested");
            } else if let Some(runtime) = runtime.as_mut()
                && runtime.shell.is_some()
            {
                interrupt_shell(runtime, state).await;
                state.prompt_queue.pause();
            } else if !state.prompt_queue.is_empty() {
                state.prompt_queue.pause();
                state.push_diagnostic(
                    "Queued prompts paused; press Enter on an empty prompt to resume",
                );
            } else if !editor.text().is_empty() {
                editor.set_text("");
                state.rewind_confirmation.cancel();
            } else if state.rewind_confirmation.request("Esc", unix_millis())
                && let Some(runtime) = runtime.as_mut()
            {
                show_rewind(runtime, state);
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => editor.insert("\n"),
        KeyCode::Enter => {
            if resume_paused_queue(editor, state) {
                return Ok(false);
            }
            let submitted = if setup_flow.is_some() || *secret_input {
                editor.take_unrecorded()
            } else {
                editor.submit()
            };
            let Some(submitted) = submitted else {
                return Ok(false);
            };
            if let Some(setup) = setup_flow.as_mut() {
                match setup
                    .submit(submitted.as_str(), credential_store)
                    .map_err(|error| CliError::Terminal(error.to_string()))
                {
                    Ok(SetupProgress::Continue {
                        prompt,
                        secret_input: next_secret_input,
                    }) => {
                        *secret_input = next_secret_input;
                        push_local_notice(state, &prompt, EntryStatus::Completed);
                    }
                    Ok(SetupProgress::Complete(setup_completion)) => {
                        *secret_input = false;
                        persist_setup(arguments, working_directory, &setup_completion)?;
                        if arguments.setup {
                            push_local_notice(
                                state,
                                "Setup complete. Credentials and preferences were saved.",
                                EntryStatus::Completed,
                            );
                            return Ok(true);
                        }
                        if let Some(mut previous) = runtime.take() {
                            let session_id = previous.session_id.clone();
                            previous.service.close_session(&session_id).await?;
                            previous.service.shutdown()?;
                        }
                        let credential = credential_store
                            .get(&setup_completion.resources.credential_handle)
                            .map_err(|error| CliError::Terminal(error.to_string()))?
                            .ok_or_else(|| {
                                CliError::Terminal(
                                    "Setup completed without a retrievable credential".to_owned(),
                                )
                            })?;
                        let mut configured = arguments.clone();
                        configured.provider_style = setup_completion.resources.provider.clone();
                        configured.model = setup_completion.resources.model.clone();
                        configured.trust = setup_completion.resources.workspace_trusted;
                        *runtime = Some(start_runtime(&configured, working_directory, credential)?);
                        if let Some(runtime) = runtime.as_ref() {
                            completion.set_user_skills(
                                runtime
                                    .skills
                                    .values()
                                    .map(|skill| (skill.name.as_str(), skill.description.as_str())),
                            );
                        }
                        let mut hydrated = runtime
                            .as_mut()
                            .map(|runtime| {
                                hydrate_initial_state(runtime, &configured, working_directory)
                            })
                            .transpose()?
                            .unwrap_or_else(|| TuiState::new(""));
                        for path in state.take_transient_paths() {
                            hydrated.track_transient_path(path);
                        }
                        let session_id = hydrated.session_id.clone();
                        *state = hydrated;
                        *controls = ControlState::new(session_id);
                        if let Some(runtime) = runtime.as_mut() {
                            sync_active_callbacks(runtime, state, controls);
                        }
                        *theme = resolve_theme(
                            setup_completion.preferences.theme,
                            EnvironmentThemeDetector.detect(),
                            std::env::var_os("NO_COLOR").is_some(),
                        );
                        *setup_flow = None;
                        push_local_notice(
                            state,
                            "Setup complete. Credentials and preferences were saved.",
                            EntryStatus::Completed,
                        );
                    }
                    Err(error) => {
                        state.push_diagnostic(error.to_string());
                        push_local_notice(state, &setup.prompt(), EntryStatus::Completed);
                    }
                }
                return Ok(false);
            }
            if !submitted.starts_with('/')
                && let Some(pending) = controls.pending_callback().cloned()
            {
                match callback_choice_from_input(&pending, &submitted) {
                    Ok(choice) => {
                        let Some(runtime) = runtime.as_mut() else {
                            state.push_diagnostic(
                                "The callback is no longer attached to an interactive session",
                            );
                            return Ok(false);
                        };
                        respond_to_pending_callback(runtime, controls, &pending, choice, state);
                    }
                    Err(error) => {
                        editor.set_text(submitted);
                        state.push_diagnostic(error);
                    }
                }
                return Ok(false);
            }
            if is_exit_command(&submitted) {
                return Ok(true);
            }
            match dispatch_command(
                &submitted,
                arguments,
                working_directory,
                runtime,
                state,
                controls,
                editor,
                completion,
                theme,
                active.is_some(),
            )
            .await?
            {
                CommandAction::Exit => return Ok(true),
                CommandAction::Setup => {
                    if active.is_some() {
                        state.push_diagnostic(
                            "Finish or interrupt the active turn before starting setup",
                        );
                        return Ok(false);
                    }
                    let setup = new_setup_flow(arguments);
                    push_local_notice(state, &setup.prompt(), EntryStatus::Completed);
                    *setup_flow = Some(setup);
                    *secret_input = false;
                }
                CommandAction::Handled => {}
                CommandAction::Runtime(command) => {
                    if handle_runtime_command(
                        &command,
                        working_directory,
                        runtime,
                        state,
                        controls,
                        theme,
                        active.is_some(),
                    )
                    .await
                    {
                        return Ok(false);
                    }
                    state.push_diagnostic("The command is not available in this runtime");
                }
                CommandAction::Unhandled if submitted.starts_with('/') => {
                    if is_user_skill(runtime.as_ref(), &submitted) {
                        if active.is_some()
                            || runtime
                                .as_ref()
                                .is_some_and(|runtime| runtime.shell.is_some())
                        {
                            state.prompt_queue.push(submitted);
                            state.prompt_queue.resume();
                            state.push_diagnostic(format!(
                                "Skill queued ({} pending)",
                                state.prompt_queue.len()
                            ));
                        } else if !start_prompt(
                            working_directory,
                            submitted.clone(),
                            runtime,
                            active,
                            state,
                            controls,
                        )
                        .await?
                        {
                            editor.set_text(submitted);
                        }
                    } else {
                        editor.set_text(&submitted);
                        state.push_diagnostic(format!("Unknown command `{submitted}`"));
                    }
                }
                CommandAction::Unhandled if submitted.trim() == "!" => {
                    state.push_diagnostic("No command provided after '!'");
                }
                CommandAction::Unhandled
                    if active.is_some()
                        || runtime
                            .as_ref()
                            .is_some_and(|runtime| runtime.shell.is_some()) =>
                {
                    state.prompt_queue.push(submitted);
                    state.prompt_queue.resume();
                    state.push_diagnostic(format!(
                        "Input queued ({} pending)",
                        state.prompt_queue.len()
                    ));
                }
                CommandAction::Unhandled if submitted.trim_start().starts_with('!') => {
                    if !start_shell(&submitted, runtime, state).await? {
                        editor.set_text(submitted);
                    }
                }
                CommandAction::Unhandled => {
                    if !start_prompt(
                        working_directory,
                        submitted.clone(),
                        runtime,
                        active,
                        state,
                        controls,
                    )
                    .await?
                    {
                        editor.set_text(submitted);
                    }
                }
            }
        }
        KeyCode::Backspace => editor.delete_backward(),
        KeyCode::Delete => editor.delete_forward(),
        KeyCode::Left => editor.move_left(key.modifiers.contains(KeyModifiers::SHIFT)),
        KeyCode::Right => editor.move_right(key.modifiers.contains(KeyModifiers::SHIFT)),
        KeyCode::Home => editor.move_home(key.modifiers.contains(KeyModifiers::SHIFT)),
        KeyCode::End => editor.move_end(key.modifiers.contains(KeyModifiers::SHIFT)),
        KeyCode::Up => editor.history_previous(),
        KeyCode::Down => editor.history_next(),
        KeyCode::PageUp => {
            state.scroll_up(10);
            page_older_history(runtime.as_mut(), state);
        }
        KeyCode::PageDown => {
            state.scroll_down(10);
        }
        KeyCode::Tab if !*secret_input => {
            refresh_completion(completion, editor, working_directory, false, state);
            if completion.view().is_some()
                && let Err(error) = completion.accept(editor)
            {
                completion.cancel();
                state.push_diagnostic(error.to_string());
            }
        }
        KeyCode::Tab => {
            state.push_diagnostic("Completion is disabled while entering a secret");
        }
        KeyCode::Char(character) => editor.insert(&character.to_string()),
        _ => {}
    }
    refresh_completion(completion, editor, working_directory, *secret_input, state);
    Ok(false)
}

fn refresh_completion(
    completion: &mut CompletionEngine,
    editor: &PromptEditor,
    working_directory: &Path,
    secret_input: bool,
    state: &mut TuiState,
) {
    if secret_input {
        completion.cancel();
        return;
    }
    if let Err(error) = completion.refresh(editor, working_directory) {
        completion.cancel();
        state.push_diagnostic(error.to_string());
    }
}

async fn handle_runtime_command(
    command: &RuntimeCommand,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    theme: &mut ResolvedTheme,
    turn_active: bool,
) -> bool {
    let command_id = command.id;
    let command_arguments = command.arguments.as_str();
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before using session commands");
        return true;
    };
    if turn_active && command_id.changes_session_projection() {
        state.push_diagnostic(
            "Finish or interrupt the active turn before changing the session projection",
        );
        return true;
    }

    match command_id {
        CommandId::Approve => {
            let scope = match command_arguments.split_whitespace().next() {
                None | Some("once") => ApprovalScope::Once,
                Some("always" | "session") => ApprovalScope::Session,
                Some("permanent") => ApprovalScope::Permanent,
                Some(other) => {
                    state.push_diagnostic(format!(
                        "Unknown approval scope `{other}`; use once, always, or permanent"
                    ));
                    return true;
                }
            };
            let choice = CallbackChoice::Approve { scope };
            if let Some(pending) = controls.pending_callback().cloned() {
                respond_to_pending_callback(runtime, controls, &pending, choice, state);
            } else {
                state.push_diagnostic("No approval is pending");
            }
        }
        CommandId::Deny => {
            let choice = CallbackChoice::Deny {
                scope: ApprovalScope::Once,
            };
            if let Some(pending) = controls.pending_callback().cloned() {
                respond_to_pending_callback(runtime, controls, &pending, choice, state);
            } else {
                state.push_diagnostic("No approval is pending");
            }
        }
        CommandId::Clear => {
            if call_runtime(
                runtime,
                "session/history/clear",
                json!({"sessionId": runtime.session_id}),
                state,
            )
            .is_some()
            {
                let session_id = runtime.session_id.clone();
                if adopt_hydrated_session(runtime, state, controls, session_id) {
                    push_local_notice(
                        state,
                        "Conversation history cleared",
                        EntryStatus::Completed,
                    );
                }
            }
        }
        CommandId::Compact => {
            let instructions = command_arguments.trim();
            let session_id = runtime.session_id.clone();
            match runtime.service.compact(&session_id, instructions).await {
                Ok(result) => {
                    let new_session_id = result
                        .get("state")
                        .and_then(|state| state.pointer("/session/id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let summary_length = result
                        .get("summary")
                        .and_then(Value::as_str)
                        .map_or(0, str::len);
                    match new_session_id {
                        Some(ref new_session_id)
                            if adopt_hydrated_session(
                                runtime,
                                state,
                                controls,
                                new_session_id.clone(),
                            ) =>
                        {
                            push_local_notice(
                                state,
                                &format!("Context compacted ({summary_length} summary bytes)"),
                                EntryStatus::Completed,
                            );
                        }
                        Some(_) => {}
                        None => state
                            .push_diagnostic("Compaction completed without a canonical session id"),
                    }
                }
                Err(error) => state.push_diagnostic(error.to_string()),
            }
        }
        CommandId::Rewind => {
            let keep_messages = command_arguments
                .split_whitespace()
                .next()
                .map(str::parse::<usize>)
                .transpose();
            match keep_messages {
                Ok(keep_messages) => {
                    let keep_messages = keep_messages.unwrap_or_default();
                    if call_runtime(
                        runtime,
                        "session/rewind",
                        json!({
                            "sessionId": runtime.session_id,
                            "keepMessages": keep_messages,
                        }),
                        state,
                    )
                    .is_some()
                    {
                        let session_id = runtime.session_id.clone();
                        if adopt_hydrated_session(runtime, state, controls, session_id) {
                            push_local_notice(
                                state,
                                &format!("Rewound to {keep_messages} stored messages"),
                                EntryStatus::Completed,
                            );
                        }
                    }
                }
                Err(_) => state.push_diagnostic("Usage: /rewind [message-count]"),
            }
        }
        CommandId::Fork => {
            if let Some(result) = call_runtime(
                runtime,
                "session/fork",
                json!({"sessionId": runtime.session_id}),
                state,
            ) && let Some(session_id) = metadata_session_id(&result)
                && adopt_hydrated_session(runtime, state, controls, session_id)
            {
                push_local_notice(state, "Forked session", EntryStatus::Completed);
            }
        }
        CommandId::Resume => {
            let Some(session_id) = command_arguments.split_whitespace().next() else {
                state.push_diagnostic("Usage: /resume <session-id>");
                return true;
            };
            if let Some(result) = call_runtime(
                runtime,
                "session/resume",
                json!({"sessionId": session_id}),
                state,
            ) && let Some(session_id) = metadata_session_id(&result)
                && adopt_hydrated_session(runtime, state, controls, session_id)
            {
                push_local_notice(state, "Resumed session", EntryStatus::Completed);
            }
        }
        CommandId::Continue => {
            if let Some(result) = call_runtime(
                runtime,
                "session/continue",
                json!({"cwd": working_directory.to_string_lossy()}),
                state,
            ) && let Some(session_id) = metadata_session_id(&result)
                && adopt_hydrated_session(runtime, state, controls, session_id)
            {
                push_local_notice(state, "Continued latest session", EntryStatus::Completed);
            }
        }
        CommandId::Rename => {
            let title = command_arguments.trim();
            if title.is_empty() {
                state.push_diagnostic("Usage: /title <new title>");
            } else if call_runtime(
                runtime,
                "session/title/update",
                json!({"sessionId": runtime.session_id, "title": title}),
                state,
            )
            .is_some()
            {
                push_local_notice(state, "Session title updated", EntryStatus::Completed);
            }
        }
        CommandId::History => {
            if let Some(result) = call_runtime(
                runtime,
                "session/list",
                json!({"offset": 0, "limit": 50}),
                state,
            ) {
                push_json_notice(state, "Saved sessions", result.get("sessions"));
            }
        }
        CommandId::Settings => {
            let arguments = command_arguments.split_whitespace().collect::<Vec<_>>();
            match arguments.as_slice() {
                [] => {
                    if let Some(result) =
                        call_runtime(runtime, "config/read", json!({}), state)
                    {
                        push_json_notice(state, "Configuration", result.get("snapshot"));
                    }
                }
                [key @ ("turns" | "tokens"), value] => {
                    update_session_limit(runtime, key, value, state);
                }
                ["mode", value @ ("code" | "plan")] => {
                    if call_runtime(
                        runtime,
                        "session/settings/update",
                        json!({"sessionId": runtime.session_id, "mode": value}),
                        state,
                    )
                    .is_some()
                    {
                        runtime.mode = (*value).to_owned();
                        push_local_notice(state, "Session mode updated", EntryStatus::Completed);
                    }
                }
                ["thinking", value @ ("off" | "low" | "medium" | "high" | "max")] => {
                    apply_thinking(runtime, value, state);
                }
                ["model", value] if !value.trim().is_empty() => {
                    if persist_user_setting(
                        runtime,
                        &["active_model"],
                        json!(value),
                        false,
                        state,
                    ) && call_runtime(
                        runtime,
                        "session/settings/update",
                        json!({"sessionId": runtime.session_id, "model": value}),
                        state,
                    )
                    .is_some()
                    {
                        runtime.model = (*value).to_owned();
                        push_local_notice(
                            state,
                            "Model updated for this session and future sessions",
                            EntryStatus::Completed,
                        );
                    }
                }
                ["agent", value] if !value.trim().is_empty() => {
                    if call_runtime(
                        runtime,
                        "session/agent/update",
                        json!({"sessionId": runtime.session_id, "name": value}),
                        state,
                    )
                    .is_some()
                    {
                        sync_runtime_intent(runtime, Some(value));
                        push_local_notice(state, "Session agent updated", EntryStatus::Completed);
                    }
                }
                ["proxy", "off"] => {
                    if persist_user_setting(runtime, &["proxy"], Value::Null, true, state) {
                        push_local_notice(state, "Proxy preference removed", EntryStatus::Completed);
                    }
                }
                ["proxy", value] => {
                    if persist_user_setting(runtime, &["proxy"], json!(value), false, state) {
                        push_local_notice(
                            state,
                            "Proxy preference saved with secrets redacted from public state",
                            EntryStatus::Completed,
                        );
                    }
                }
                ["certificate", "off"] => {
                    if persist_user_setting(
                        runtime,
                        &["tls_ca_path"],
                        Value::Null,
                        true,
                        state,
                    ) {
                        push_local_notice(
                            state,
                            "TLS certificate preference removed",
                            EntryStatus::Completed,
                        );
                    }
                }
                ["certificate", value] if Path::new(value).is_file() => {
                    if persist_user_setting(
                        runtime,
                        &["tls_ca_path"],
                        json!(value),
                        false,
                        state,
                    ) {
                        push_local_notice(
                            state,
                            "TLS certificate preference saved",
                            EntryStatus::Completed,
                        );
                    }
                }
                ["certificate", _] => {
                    state.push_diagnostic("TLS certificate path is unavailable");
                }
                ["notifications", value @ ("off" | "unfocused" | "always")] => {
                    if persist_user_setting(
                        runtime,
                        &["notifications"],
                        json!(value),
                        false,
                        state,
                    ) {
                        push_local_notice(
                            state,
                            "Notification preference saved",
                            EntryStatus::Completed,
                        );
                    }
                }
                ["updates", value @ ("on" | "off")] => {
                    if persist_user_setting(
                        runtime,
                        &["enable_update_checks"],
                        json!(*value == "on"),
                        false,
                        state,
                    ) {
                        push_local_notice(
                            state,
                            "Update-check preference saved",
                            EntryStatus::Completed,
                        );
                    }
                }
                ["theme", value] => update_theme(runtime, value, state, theme),
                _ => state.push_diagnostic(
                    "Usage: /settings [turns|tokens|mode|thinking|model|agent|proxy|certificate|notifications|updates|theme] [value]",
                ),
            }
        }
        CommandId::Theme => {
            let value = command_arguments
                .split_whitespace()
                .next()
                .unwrap_or_default();
            update_theme(runtime, value, state, theme);
        }
        CommandId::Update => push_local_notice(
            state,
            &format!(
                "Mistral Vibe {} is installed. Update discovery is unavailable in this build; the current session remains usable.",
                env!("CARGO_PKG_VERSION")
            ),
            EntryStatus::Completed,
        ),
        CommandId::Trust => {
            if call_runtime_async(
                runtime,
                "workspace/trust/decision",
                json!({
                    "sessionId": runtime.session_id,
                    "cwd": working_directory,
                    "decision": "trust_cwd",
                }),
                state,
            )
            .await
            .is_some()
            {
                push_local_notice(
                    state,
                    "Workspace trusted for this session",
                    EntryStatus::Completed,
                );
            }
        }
        CommandId::Loop => handle_loop_command(command_arguments, runtime, state),
        CommandId::RemoteProject => {
            handle_project_command(command_arguments, working_directory, runtime, state).await
        }
        CommandId::Teleport => {
            handle_teleport_command(command_arguments, working_directory, runtime, state).await
        }
        _ => return false,
    }
    true
}

fn call_runtime(
    runtime: &mut InteractiveRuntime,
    method: &str,
    params: Value,
    state: &mut TuiState,
) -> Option<BTreeMap<String, Value>> {
    match runtime.service.public_call(method, params) {
        Ok(result) => Some(result),
        Err(error) => {
            state.push_diagnostic(error.to_string());
            None
        }
    }
}

fn configured_theme(runtime: Option<&mut InteractiveRuntime>) -> Option<Theme> {
    let result = runtime?
        .service
        .public_call("config/read", json!({}))
        .ok()?;
    result
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("config"))
        .and_then(|config| config.get("theme"))
        .and_then(Value::as_str)
        .and_then(parse_theme)
}

fn parse_theme(value: &str) -> Option<Theme> {
    match value {
        "system" | "default" => Some(Theme::System),
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

fn update_theme(
    runtime: &mut InteractiveRuntime,
    value: &str,
    state: &mut TuiState,
    theme: &mut ResolvedTheme,
) {
    let Some(preference) = parse_theme(value) else {
        state.push_diagnostic("Usage: /theme <system|light|dark>");
        return;
    };
    if persist_user_setting(runtime, &["theme"], json!(value), false, state) {
        let no_color = !theme.colors_enabled;
        *theme = resolve_theme(preference, EnvironmentThemeDetector.detect(), no_color);
        push_local_notice(state, "Theme preference saved", EntryStatus::Completed);
    }
}

fn update_session_limit(
    runtime: &mut InteractiveRuntime,
    key: &str,
    value: &str,
    state: &mut TuiState,
) {
    let protocol_key = if key == "turns" {
        "maxTurns"
    } else {
        "maxTokens"
    };
    match value.parse::<u64>() {
        Ok(value) => {
            let mut params =
                serde_json::Map::from_iter([("sessionId".to_owned(), json!(runtime.session_id))]);
            params.insert(protocol_key.to_owned(), json!(value));
            if call_runtime(
                runtime,
                "session/settings/update",
                Value::Object(params),
                state,
            )
            .is_some()
            {
                if key == "tokens" {
                    runtime.context_window = value;
                }
                push_local_notice(state, "Session limits updated", EntryStatus::Completed);
            }
        }
        Err(_) => state.push_diagnostic(
            "Session setting must be a non-negative integer in the supported range",
        ),
    }
}

fn persist_user_setting(
    runtime: &mut InteractiveRuntime,
    path: &[&str],
    value: Value,
    remove: bool,
    state: &mut TuiState,
) -> bool {
    let expected_fingerprint = match runtime.service.public_call("config/read", json!({})) {
        Ok(result) => result
            .get("snapshot")
            .and_then(|snapshot| snapshot.pointer("/fingerprints/user"))
            .cloned()
            .unwrap_or(Value::Null),
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return false;
        }
    };
    let mutation = if remove {
        json!({"path": path, "remove": true})
    } else {
        json!({"path": path, "value": value})
    };
    call_runtime(
        runtime,
        "config/batchWrite",
        json!({
            "writes": [{
                "target": "user",
                "expectedFingerprint": expected_fingerprint,
                "mutations": [mutation],
            }],
        }),
        state,
    )
    .is_some()
}

async fn call_runtime_async(
    runtime: &mut InteractiveRuntime,
    method: &str,
    params: Value,
    state: &mut TuiState,
) -> Option<PublicDispatch> {
    match runtime.service.public_call_async(method, params).await {
        Ok(dispatch) => {
            for notification in &dispatch.notifications {
                push_local_notice(
                    state,
                    &format!(
                        "{}: {}",
                        notification.method,
                        compact_json(&json!(notification.params))
                    ),
                    EntryStatus::Completed,
                );
            }
            Some(dispatch)
        }
        Err(error) => {
            state.push_diagnostic(error.to_string());
            None
        }
    }
}

fn adopt_hydrated_session(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    controls: &mut ControlState,
    session_id: String,
) -> bool {
    let mut replacement = match canonical_session_projection(runtime, &session_id, true) {
        Ok(replacement) => replacement,
        Err(error) => {
            state.push_diagnostic(format!(
                "Canonical session `{session_id}` is unavailable: {error}"
            ));
            return false;
        }
    };
    for path in state.take_transient_paths() {
        replacement.track_transient_path(path);
    }
    replacement.resize(state.viewport.0, state.viewport.1);
    runtime.session_id.clone_from(&session_id);
    sync_runtime_intent(runtime, None);
    *state = replacement;
    *controls = ControlState::new(session_id);
    sync_active_callbacks(runtime, state, controls);
    true
}

fn sync_runtime_intent(runtime: &mut InteractiveRuntime, agent_name: Option<&str>) {
    if let Some(agent_name) = agent_name {
        runtime.agent_name = agent_name.to_owned();
    }
    if let Ok(session) = runtime.service.session(&runtime.session_id) {
        if agent_name.is_none()
            && let Some(agent_name) = session.intent.agent
        {
            runtime.agent_name = agent_name;
        }
        if let Some(model) = session.intent.model {
            runtime.model = model;
        }
        runtime.thinking = session
            .intent
            .reasoning_effort
            .unwrap_or_else(|| "off".to_owned());
        if let Some(mode) = session.intent.mode {
            runtime.mode = mode;
        }
        runtime.auto_approve = session.intent.auto_approve;
    }
}

fn metadata_session_id(result: &BTreeMap<String, Value>) -> Option<String> {
    result
        .get("metadata")
        .and_then(|metadata| {
            metadata
                .get("session_id")
                .or_else(|| metadata.get("sessionId"))
                .or_else(|| metadata.get("id"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn handle_loop_command(arguments: &str, runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let arguments = arguments.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] | ["ls"] => {
            if let Some(result) = call_runtime(
                runtime,
                "loops/list",
                json!({"sessionId": runtime.session_id}),
                state,
            ) {
                push_json_notice(state, "Scheduled loops", result.get("loops"));
            }
        }
        [action, target] if matches!(*action, "cancel" | "rm" | "stop" | "delete") => {
            let (method, params) = if *target == "all" {
                ("loops/clear", json!({"sessionId": runtime.session_id}))
            } else {
                (
                    "loops/delete",
                    json!({"sessionId": runtime.session_id, "loopId": target}),
                )
            };
            if call_runtime(runtime, method, params, state).is_some() {
                push_local_notice(state, "Scheduled loop removed", EntryStatus::Completed);
            }
        }
        [interval, prompt @ ..] if !prompt.is_empty() => {
            if let Some(result) = call_runtime(
                runtime,
                "loops/create",
                json!({
                    "sessionId": runtime.session_id,
                    "interval": interval,
                    "prompt": prompt.join(" "),
                }),
                state,
            ) {
                push_json_notice(state, "Scheduled loop created", result.get("loop"));
            }
        }
        _ => state.push_diagnostic(
            "Usage: /loop [list|ls] | /loop <interval> <prompt> | /loop delete <id|all>",
        ),
    }
}

async fn handle_project_command(
    command_arguments: &str,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let arguments = command_arguments.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["open"] => {
            if let Some(result) = call_runtime_async(
                runtime,
                "vibeCode/projects/open",
                json!({
                    "sessionId": runtime.session_id,
                    "workingDirectory": working_directory,
                    "purpose": "configure",
                }),
                state,
            )
            .await
            {
                runtime.cloud.picker_id = result
                    .result
                    .get("pickerId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                push_json_notice(
                    state,
                    "Remote projects",
                    result
                        .result
                        .get("view")
                        .and_then(|view| view.pointer("/state/projects")),
                );
            }
        }
        ["select", project_id] => {
            let Some(picker_id) = runtime.cloud.picker_id.clone() else {
                state.push_diagnostic("Open the remote project picker first");
                return;
            };
            if call_runtime_async(
                runtime,
                "vibeCode/projects/select",
                json!({
                    "sessionId": runtime.session_id,
                    "pickerId": picker_id,
                    "projectId": project_id,
                }),
                state,
            )
            .await
            .is_some()
            {
                runtime.cloud.project_id = Some((*project_id).to_owned());
                push_local_notice(
                    state,
                    &format!("Selected remote project `{project_id}`"),
                    EntryStatus::Completed,
                );
            }
        }
        ["more"] => {
            let Some(picker_id) = runtime.cloud.picker_id.clone() else {
                state.push_diagnostic("Open the remote project picker first");
                return;
            };
            if let Some(result) = call_runtime_async(
                runtime,
                "vibeCode/projects/loadMore",
                json!({"sessionId": runtime.session_id, "pickerId": picker_id}),
                state,
            )
            .await
            {
                push_json_notice(
                    state,
                    "Remote projects",
                    result
                        .result
                        .get("view")
                        .and_then(|view| view.pointer("/state/projects")),
                );
            }
        }
        ["create", name] | ["create", name, _] => {
            let Some(picker_id) = runtime.cloud.picker_id.clone() else {
                state.push_diagnostic("Open the remote project picker first");
                return;
            };
            let default_branch = arguments.get(2).copied().unwrap_or("main");
            if let Some(result) = call_runtime_async(
                runtime,
                "vibeCode/projects/create",
                json!({
                    "sessionId": runtime.session_id,
                    "pickerId": picker_id,
                    "name": name,
                    "defaultBranch": default_branch,
                }),
                state,
            )
            .await
            {
                runtime.cloud.project_id = result
                    .result
                    .get("project")
                    .and_then(|project| project.get("projectId"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                push_json_notice(
                    state,
                    "Remote project created",
                    result.result.get("project"),
                );
            }
        }
        ["unlink"] | ["cancel"] => {
            let Some(picker_id) = runtime.cloud.picker_id.clone() else {
                state.push_diagnostic("No remote project picker is active");
                return;
            };
            let method = if arguments[0] == "unlink" {
                "vibeCode/projects/unlink"
            } else {
                "vibeCode/projects/cancel"
            };
            if call_runtime_async(
                runtime,
                method,
                json!({"sessionId": runtime.session_id, "pickerId": picker_id}),
                state,
            )
            .await
            .is_some()
            {
                runtime.cloud.project_id = None;
                if method.ends_with("cancel") {
                    runtime.cloud.picker_id = None;
                }
                push_local_notice(
                    state,
                    "Remote project selection cleared",
                    EntryStatus::Completed,
                );
            }
        }
        _ => state.push_diagnostic(
            "Usage: /remote-project [open|more|select <id>|create <name> [branch]|unlink|cancel]",
        ),
    }
}

async fn handle_teleport_command(
    command_arguments: &str,
    working_directory: &Path,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let arguments = command_arguments.split_whitespace().collect::<Vec<_>>();
    if let Some(action) = arguments.first()
        && matches!(*action, "approve" | "deny" | "cancel")
    {
        let Some(operation_id) = runtime.cloud.teleport_operation_id.clone() else {
            state.push_diagnostic("No Teleport operation is active");
            return;
        };
        let (method, params) = if *action == "cancel" {
            (
                "vibeCode/teleport/cancel",
                json!({
                    "sessionId": runtime.session_id,
                    "operationId": operation_id,
                }),
            )
        } else {
            (
                "vibeCode/teleport/push/respond",
                json!({
                    "sessionId": runtime.session_id,
                    "operationId": operation_id,
                    "approved": *action == "approve",
                }),
            )
        };
        let _ = call_runtime_async(runtime, method, params, state).await;
        return;
    }
    let (Some(picker_id), Some(project_id)) = (
        runtime.cloud.picker_id.clone(),
        runtime.cloud.project_id.clone(),
    ) else {
        state.push_diagnostic("Select a remote project before starting Teleport");
        return;
    };
    let operation_id = format!(
        "teleport-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis())
    );
    let prompt = arguments.join(" ");
    if call_runtime_async(
        runtime,
        "vibeCode/teleport/start",
        json!({
            "sessionId": runtime.session_id,
            "operationId": operation_id,
            "pickerId": picker_id,
            "projectId": project_id,
            "workingDirectory": working_directory,
            "prompt": prompt,
        }),
        state,
    )
    .await
    .is_some()
    {
        runtime.cloud.teleport_operation_id = Some(operation_id);
    }
}

fn push_json_notice(state: &mut TuiState, label: &str, value: Option<&Value>) {
    push_local_notice(
        state,
        &format!("{label}: {}", compact_json(value.unwrap_or(&Value::Null))),
        EntryStatus::Completed,
    );
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn is_exit_command(command: &str) -> bool {
    matches!(command.trim(), "/close" | "/exit" | "/quit")
}

fn new_setup_flow(arguments: &Arguments) -> SetupFlow {
    SetupFlow::new(
        arguments.provider_style.clone(),
        arguments.credential_environment.clone(),
        arguments.trust,
        arguments.model.clone(),
    )
}

fn persist_setup(
    arguments: &Arguments,
    working_directory: &Path,
    completion: &SetupCompletion,
) -> Result<(), CliError> {
    let release3 = release3_service(arguments, working_directory)?;
    let snapshot = release3
        .dispatch("config/read", &BTreeMap::new())
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    let expected_fingerprint = snapshot
        .result
        .get("snapshot")
        .and_then(|snapshot| snapshot.pointer("/fingerprints/user"))
        .cloned()
        .unwrap_or(Value::Null);
    let notifications = match completion.preferences.notifications {
        NotificationPreference::Off => "off",
        NotificationPreference::WhenUnfocused => "unfocused",
        NotificationPreference::Always => "always",
    };
    let mut mutations = vec![
        json!({"path": ["provider"], "value": completion.resources.provider}),
        json!({"path": ["active_model"], "value": completion.resources.model}),
        json!({"path": ["thinking"], "value": completion.resources.thinking}),
        json!({"path": ["theme"], "value": completion.preferences.theme}),
        json!({"path": ["notifications"], "value": notifications}),
        json!({
            "path": ["enable_update_checks"],
            "value": completion.preferences.update_checks,
        }),
    ];
    match &completion.resources.proxy {
        Some(proxy) => mutations.push(json!({"path": ["proxy"], "value": proxy})),
        None => mutations.push(json!({"path": ["proxy"], "remove": true})),
    }
    match &completion.resources.certificate_path {
        Some(path) => mutations.push(json!({
            "path": ["tls_ca_path"],
            "value": path.to_string_lossy(),
        })),
        None => mutations.push(json!({"path": ["tls_ca_path"], "remove": true})),
    }
    let params = BTreeMap::from([(
        "writes".to_owned(),
        json!([{
            "target": "user",
            "expectedFingerprint": expected_fingerprint,
            "mutations": mutations,
        }]),
    )]);
    release3
        .dispatch("config/batchWrite", &params)
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    Ok(())
}

fn hydrate_initial_state(
    runtime: &mut InteractiveRuntime,
    _arguments: &Arguments,
    _working_directory: &Path,
) -> Result<TuiState, CliError> {
    let session_id = runtime.session_id.clone();
    match canonical_session_projection(runtime, &session_id, true) {
        Ok(state) => Ok(state),
        Err(error) => Ok(recoverable_initial_state(
            &session_id,
            format!("Initial session state is unavailable: {error}"),
        )),
    }
}

fn canonical_session_projection(
    runtime: &mut InteractiveRuntime,
    session_id: &str,
    include_saved_history: bool,
) -> Result<TuiState, CliError> {
    let result = runtime
        .service
        .public_call("session/read", json!({"sessionId": session_id}))?;
    let Some(public_state) = result.get("state") else {
        return Err(CliError::Terminal(
            "session/read omitted public state".to_owned(),
        ));
    };
    let mut state = tui_state_from_public_session(session_id, public_state)?;
    if include_saved_history {
        overlay_latest_saved_history(runtime, &mut state)?;
    }
    Ok(state)
}

fn overlay_latest_saved_history(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) -> Result<(), CliError> {
    let count = match runtime.service.public_call(
        "session/rewind/read",
        json!({"sessionId": state.session_id}),
    ) {
        Ok(result) => {
            runtime.context_tokens = result
                .get("statistics")
                .and_then(|statistics| statistics.get("context_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_default();
            result.get("messageCount").and_then(Value::as_u64)
        }
        Err(_) => None,
    };
    let Some(message_count) = count.and_then(|count| usize::try_from(count).ok()) else {
        return Ok(());
    };
    let offset = message_count.saturating_sub(INITIAL_HISTORY_LIMIT);
    let result = runtime.service.public_call(
        "history/list",
        json!({
            "sessionId": state.session_id,
            "offset": offset,
            "limit": INITIAL_HISTORY_LIMIT,
        }),
    )?;
    let history = decode_public_result::<PublicHistoryList>(result)?;
    let mut entries = transcript_entries_from_history(&state.session_id, offset, &history.history);
    for entry in state
        .entries
        .iter()
        .filter(|entry| {
            entry.status == EntryStatus::Streaming || entry.kind == TranscriptKind::Callback
        })
        .cloned()
    {
        if !entries.iter().any(|current| current.id == entry.id) {
            entries.push(entry);
        }
    }
    state
        .apply(ServerEvent::Snapshot(TuiSnapshot {
            session_id: state.session_id.clone(),
            event_id: state.watermark,
            entries,
            cursor_before: (offset > 0).then(|| offset.to_string()),
            cursor_after: None,
            waiting: state.waiting,
        }))
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    Ok(())
}

fn page_older_history(runtime: Option<&mut InteractiveRuntime>, state: &mut TuiState) {
    if !state.needs_older_history() {
        return;
    }
    let Some(runtime) = runtime else {
        state.push_diagnostic("Saved history is unavailable until setup completes");
        return;
    };
    let Some(before) = state.cursor_before.clone() else {
        return;
    };
    let Ok(before) = before.parse::<usize>() else {
        state.cursor_before = None;
        state.push_diagnostic("Saved-history cursor is invalid");
        return;
    };
    let limit = before.min(INITIAL_HISTORY_LIMIT);
    let offset = before.saturating_sub(limit);
    let result = runtime.service.public_call(
        "history/list",
        json!({
            "sessionId": state.session_id,
            "offset": offset,
            "limit": limit,
        }),
    );
    let history = match result
        .map_err(CliError::from)
        .and_then(decode_public_result::<PublicHistoryList>)
    {
        Ok(history) => history,
        Err(error) => {
            state.push_diagnostic(format!("Older history is unavailable: {error}"));
            return;
        }
    };
    let entries = transcript_entries_from_history(&state.session_id, offset, &history.history);
    if let Err(error) = state.prepend_history(entries, (offset > 0).then(|| offset.to_string())) {
        state.push_diagnostic(format!("Older history is invalid: {error}"));
        return;
    }
    state.scroll_to_oldest();
}

fn transcript_entries_from_history(
    session_id: &str,
    offset: usize,
    history: &[PersistedMessage],
) -> Vec<TranscriptEntry> {
    history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let (kind, text, status) = match message {
                PersistedMessage::User { content } => (
                    TranscriptKind::UserMessage,
                    content.clone(),
                    EntryStatus::Completed,
                ),
                PersistedMessage::Assistant { content } => (
                    TranscriptKind::AssistantMessage,
                    content.clone(),
                    EntryStatus::Completed,
                ),
                PersistedMessage::Tool {
                    call_id,
                    content,
                    is_error,
                } => (
                    TranscriptKind::Effect,
                    format!("Tool {call_id}\n{content}"),
                    if *is_error {
                        EntryStatus::Failed
                    } else {
                        EntryStatus::Completed
                    },
                ),
                PersistedMessage::System { content } => {
                    let _ = content;
                    return None;
                }
            };
            Some(TranscriptEntry {
                id: format!("persisted:{session_id}:{}", offset.saturating_add(index)),
                revision: 1,
                kind,
                text,
                status,
                details: json!({"source": "history/list"}),
            })
        })
        .collect()
}

fn decode_public_result<T: DeserializeOwned>(
    result: BTreeMap<String, Value>,
) -> Result<T, CliError> {
    serde_json::from_value(Value::Object(result.into_iter().collect())).map_err(CliError::Json)
}

fn recoverable_initial_state(session_id: &str, diagnostic: impl Into<String>) -> TuiState {
    let mut state = TuiState::new(session_id);
    state.ready = true;
    state.push_diagnostic(diagnostic);
    state
}

fn tui_state_from_public_session(
    session_id: &str,
    public_state: &Value,
) -> Result<TuiState, CliError> {
    let reported_session_id = public_state
        .pointer("/session/id")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Terminal("session/read omitted session id".to_owned()))?;
    if reported_session_id != session_id {
        return Err(CliError::Terminal(format!(
            "session/read returned foreign session `{reported_session_id}`"
        )));
    }
    let mut entries = serde_json::from_value::<Vec<PublicHistoryEntry>>(
        public_state
            .pointer("/history/entries")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(CliError::Json)?
    .into_iter()
    .map(history_entry)
    .collect::<Vec<_>>();
    let active_callbacks = serde_json::from_value::<Vec<PublicHistoryEntry>>(
        public_state
            .get("activeCallbacks")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(CliError::Json)?;
    if active_callbacks.len() > 1 {
        return Err(CliError::Terminal(
            "session/read projected more than one active callback".to_owned(),
        ));
    }
    for callback in active_callbacks {
        let callback = history_entry(callback);
        if !entries.iter().any(|entry| entry.id == callback.id) {
            entries.push(callback);
        }
    }
    let cursor_before = public_state
        .pointer("/history/cursor/before")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cursor_after = public_state
        .pointer("/history/cursor/after")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let waiting = matches!(
        public_state
            .pointer("/session/status/type")
            .and_then(Value::as_str),
        Some("running" | "blocked")
    );
    let mut state = TuiState::new(session_id);
    state
        .apply(ServerEvent::Snapshot(TuiSnapshot {
            session_id: session_id.to_owned(),
            event_id: public_state
                .get("eventId")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            entries,
            cursor_before,
            cursor_after,
            waiting,
        }))
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    state
        .apply(ServerEvent::Ready)
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    Ok(state)
}

fn start_runtime(
    arguments: &Arguments,
    working_directory: &Path,
    credential: String,
) -> Result<InteractiveRuntime, CliError> {
    let release3 = release3_service(arguments, working_directory)?;
    let mut banner = banner_metrics_from_release3(&release3, arguments, working_directory);
    let skills = runtime_skills(&release3);
    let preferences = startup_preferences(arguments, &release3)?;
    let release4 = Release4Service::production(
        VibeCodeCloudConfig::from_credential(credential.clone())
            .map_err(|error| CliError::Terminal(error.to_string()))?,
    )
    .map_err(|error| CliError::Terminal(error.to_string()))?;
    let telemetry = telemetry_event_observer(arguments, &credential, "tui")?;
    let mut driver = LiveTurnDriver::from_credential(
        live_driver_config(arguments, &preferences.model)?,
        credential,
    )?;
    if let Some(observer) = telemetry.as_ref() {
        driver = driver.with_event_observer(observer.clone());
    }
    let driver = Arc::new(driver);
    let server = AppServer::default()
        .using_release3_service(release3)
        .using_release4_service(release4);
    let mut service = HeadlessService::new_interactive_shared_with_server(driver, server)?;
    let session_id = service.start_session(&SessionOptions {
        working_directory: working_directory.to_string_lossy().into_owned(),
        session_id: arguments.resume.clone(),
        add_directories: arguments
            .add_directories
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        trusted: arguments.trust,
        agent: arguments.agent.clone(),
        tool_filters: arguments.tool_filters.clone(),
        enabled_tools: arguments.enabled_tools.clone(),
        disabled_tools: arguments.disabled_tools.clone(),
        mcp_servers: Vec::new(),
        model: Some(preferences.model.clone()),
        max_turns: arguments.max_turns,
        max_tokens: arguments.max_tokens,
        max_price_micros: arguments
            .max_price
            .map(|price| (price * 1_000_000.0).round() as u64),
        mode: Some(preferences.mode.clone()),
        thinking: preferences.thinking,
        reasoning_effort: preferences.reasoning_effort.clone(),
        auto_approve: arguments.auto_approve,
        resume: arguments.resume.clone(),
        continue_session: arguments.continue_session,
    })?;
    refresh_server_banner_metrics(&mut service, &mut banner);
    let session = service.session(&session_id)?;
    let agent_name = session
        .intent
        .agent
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    Ok(InteractiveRuntime {
        service,
        session_id,
        model: session.intent.model.unwrap_or(preferences.model),
        thinking: session
            .intent
            .reasoning_effort
            .unwrap_or_else(|| "off".to_owned()),
        mode: session
            .intent
            .mode
            .unwrap_or_else(|| preferences.mode.clone()),
        agent_name,
        banner,
        context_tokens: 0,
        context_window: arguments.max_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW),
        auto_approve: session.intent.auto_approve,
        clear_context_after_turn: false,
        skills,
        shell: None,
        cloud: CloudWorkflowState::default(),
        telemetry,
    })
}

fn runtime_skills(release3: &Release3Service) -> BTreeMap<String, RuntimeSkill> {
    release3
        .dispatch("skills/list", &BTreeMap::new())
        .ok()
        .and_then(|dispatch| dispatch.result.get("skills").cloned())
        .map_or_else(BTreeMap::new, |skills| parse_runtime_skills(Some(&skills)))
}

fn parse_runtime_skills(skills: Option<&Value>) -> BTreeMap<String, RuntimeSkill> {
    skills
        .and_then(Value::as_array)
        .cloned()
        .into_iter()
        .flatten()
        .filter(|skill| {
            skill
                .get("userInvocable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|skill| {
            let name = skill.get("name").and_then(Value::as_str)?.to_owned();
            Some((
                name.clone(),
                RuntimeSkill {
                    name,
                    description: skill
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    body: skill
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    path: skill
                        .get("path")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                },
            ))
        })
        .collect()
}

fn local_banner_metrics(arguments: &Arguments, working_directory: &Path) -> BannerMetrics {
    release3_service(arguments, working_directory).map_or_else(
        |_| BannerMetrics::default(),
        |release3| banner_metrics_from_release3(&release3, arguments, working_directory),
    )
}

fn banner_metrics_from_release3(
    release3: &Release3Service,
    arguments: &Arguments,
    working_directory: &Path,
) -> BannerMetrics {
    let mut banner = BannerMetrics::default();
    if let Ok(dispatch) = release3.dispatch("skills/list", &BTreeMap::new()) {
        banner.skills_count = dispatch
            .result
            .get("skills")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
    }
    if let Ok(servers) = release3.mcp_servers_for_session(working_directory, arguments.trust, &[]) {
        banner.mcp_servers_total = servers.len();
        banner.mcp_servers_enabled = servers.iter().filter(|server| server.enabled).count();
    }
    banner
}

fn refresh_server_banner_metrics(
    service: &mut HeadlessService<LiveTurnDriver>,
    banner: &mut BannerMetrics,
) {
    if let Ok(result) = service.public_call("connectors/read", json!({}))
        && let Some(counts) = result.get("counts")
    {
        banner.connectors_connected = json_usize(counts.get("connected"));
        banner.connectors_total = json_usize(counts.get("total"));
    }
    if let Ok(result) = service.public_call("mcp/read", json!({}))
        && let Some(sources) = result
            .get("mcp")
            .and_then(|mcp| mcp.get("sources"))
            .and_then(Value::as_array)
    {
        banner.mcp_servers_total = sources.len();
        banner.mcp_servers_enabled = sources
            .iter()
            .filter(|source| {
                source
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status != "disabled")
            })
            .count();
    }
    if let Ok(result) = service.public_call("diagnostics/list", json!({})) {
        banner.hooks_count = json_usize(result.get("hooksCount"));
    }
    if let Ok(result) = service.public_call("account/read", json!({})) {
        banner.plan = result
            .get("account")
            .and_then(|account| account.get("plan"))
            .and_then(|plan| plan.get("title"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

fn json_usize(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default()
}

struct StartupPreferences {
    model: String,
    mode: String,
    thinking: bool,
    reasoning_effort: Option<String>,
}

fn release3_service(
    arguments: &Arguments,
    working_directory: &Path,
) -> Result<Release3Service, CliError> {
    let vibe_home = arguments
        .session_root
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("VIBE_HOME").map(Into::into))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|path| path.join(".vibe"))
        })
        .unwrap_or_else(|| working_directory.join(".vibe"));
    let session_root = arguments
        .session_root
        .clone()
        .unwrap_or_else(|| vibe_home.join("sessions"));
    Release3Service::new(
        Release3Paths {
            vibe_home,
            working_directory: working_directory.to_path_buf(),
            session_root,
        },
        toml::Table::new(),
        arguments.trust,
    )
    .map(Release3Service::with_runtime_session_persistence)
    .map_err(|error| CliError::Terminal(error.to_string()))
}

fn startup_preferences(
    arguments: &Arguments,
    release3: &Release3Service,
) -> Result<StartupPreferences, CliError> {
    let dispatch = release3
        .dispatch("config/read", &BTreeMap::new())
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    let config = dispatch
        .result
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("config"));
    let configured_model = config
        .and_then(|config| config.get("active_model"))
        .and_then(Value::as_str);
    let model = if arguments.model == DEFAULT_MODEL {
        configured_model.unwrap_or(&arguments.model).to_owned()
    } else {
        arguments.model.clone()
    };
    let reasoning_effort = config
        .and_then(|config| config.get("thinking"))
        .and_then(Value::as_str)
        .filter(|value| *value != "off")
        .map(ToOwned::to_owned);
    let mode = if arguments.agent.as_deref() == Some("plan") {
        "plan".to_owned()
    } else {
        config
            .and_then(|config| config.get("mode"))
            .and_then(Value::as_str)
            .filter(|mode| matches!(*mode, "code" | "plan"))
            .unwrap_or("code")
            .to_owned()
    };
    Ok(StartupPreferences {
        model,
        mode,
        thinking: reasoning_effort.is_some(),
        reasoning_effort,
    })
}

fn live_driver_config(arguments: &Arguments, model: &str) -> Result<LiveDriverConfig, CliError> {
    Ok(LiveDriverConfig {
        style: arguments.provider_style.clone(),
        endpoint: arguments.api_base.clone(),
        model: model.to_owned(),
        credential_environment: arguments.credential_environment.clone(),
        system_prompt: "You are Mistral Vibe.".to_owned(),
        session_root: arguments.session_root.clone(),
        input_price_per_million_micros: price_per_million_micros(arguments.input_price)?,
        output_price_per_million_micros: price_per_million_micros(arguments.output_price)?,
    })
}

fn drain_updates(
    state: &mut TuiState,
    runtime: Option<&mut InteractiveRuntime>,
    active: Option<&mut ActiveTurn>,
    controls: &mut ControlState,
) {
    let Some(active) = active else {
        return;
    };
    if drain_update_receiver(state, &active.turn_id, &mut active.updates) {
        if let Some(runtime) = runtime {
            resync_current_projection(runtime, state);
            sync_active_callbacks(runtime, state, controls);
        } else {
            state.push_diagnostic("Canonical resync is unavailable until setup completes");
        }
    }
}

fn drain_update_receiver(
    state: &mut TuiState,
    turn_id: &str,
    updates: &mut tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
) -> bool {
    while let Ok(update) = updates.try_recv() {
        let event = match update {
            ProgrammaticUpdate::HistoryEntry {
                event_id, entry, ..
            } => {
                let mut transcript = history_entry(*entry);
                transcript.id = format!("{turn_id}:{}", transcript.id);
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

fn resync_current_projection(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    match canonical_session_projection(runtime, &runtime.session_id.clone(), false) {
        Ok(replacement) => {
            if let Err(error) = state.replace_projection_preserving_diagnostics(replacement) {
                state.push_diagnostic(format!("Canonical resync was rejected: {error}"));
            }
        }
        Err(error) => {
            state.push_diagnostic(format!("Canonical resync failed: {error}"));
        }
    }
}

fn sync_active_callbacks(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    controls: &mut ControlState,
) {
    let result = match runtime
        .service
        .public_call("session/read", json!({"sessionId": runtime.session_id}))
    {
        Ok(result) => result,
        Err(error) => {
            state.push_diagnostic(format!("Active callbacks are unavailable: {error}"));
            return;
        }
    };
    let active = result
        .get("state")
        .and_then(|state| state.get("activeCallbacks"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let entries = match serde_json::from_value::<Vec<PublicHistoryEntry>>(active) {
        Ok(entries) if entries.len() <= 1 => entries,
        Ok(_) => {
            state.push_diagnostic("Server projected more than one active callback");
            return;
        }
        Err(error) => {
            state.push_diagnostic(format!("Active callback projection is invalid: {error}"));
            return;
        }
    };
    let pending_callbacks = entries
        .into_iter()
        .filter_map(|entry| match pending_callback_from_entry(&entry) {
            Ok(pending) => pending,
            Err(error) => {
                state.push_diagnostic(format!("Active callback is invalid: {error}"));
                None
            }
        })
        .collect::<Vec<_>>();
    let active_callback_ids = pending_callbacks
        .iter()
        .map(|pending| pending.callback_id.as_str())
        .collect::<Vec<_>>();
    controls.reconcile_active_callbacks(&active_callback_ids);

    for pending in pending_callbacks {
        if controls.contains_callback(&pending.callback_id) {
            continue;
        }
        if controls.active_turn_id.is_none()
            && let Err(error) = controls.begin_turn(&pending.turn_id)
        {
            state.push_diagnostic(error.to_string());
            continue;
        }
        if let Err(error) = controls.present_callback(pending.clone()) {
            state.push_diagnostic(error.to_string());
            continue;
        }
        push_local_notice(state, &pending.prompt, EntryStatus::Streaming);
        if pending.kind == CallbackKind::PlanReview && runtime.mode != "plan" {
            state.push_diagnostic("Rejected exit-plan callback outside plan mode");
            respond_to_pending_callback(runtime, controls, &pending, CallbackChoice::Cancel, state);
        }
    }
}

async fn finish_active(
    state: &mut TuiState,
    controls: &mut ControlState,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
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
        cancel_requested: _,
        mut updates,
        task,
    } = active_turn;
    let (reservation, outcome) = task
        .await
        .map_err(|error| CliError::Terminal(format!("turn task failed: {error}")))?;
    let _ = drain_update_receiver(state, &turn_id, &mut updates);
    let runtime = runtime
        .as_mut()
        .ok_or_else(|| CliError::Terminal("interactive runtime disappeared".to_owned()))?;
    match outcome {
        Ok(outcome) => {
            runtime.context_tokens = outcome.context_tokens;
            runtime.service.finish_reserved(&reservation, outcome)?;
            controls.complete_turn(&turn_id, "Turn complete");
            state.waiting = false;
        }
        Err(error) => {
            runtime
                .service
                .fail_reserved(&reservation, &error.to_string())?;
            controls.complete_turn(&turn_id, "Turn failed");
            state.waiting = false;
            state.push_diagnostic(error.to_string());
        }
    }
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
    Ok(())
}

fn pending_callback_from_entry(
    entry: &PublicHistoryEntry,
) -> Result<Option<PendingCallback>, String> {
    let PublicHistoryEntry::Callback {
        metadata,
        callback_id,
        title,
        detail,
        state: PublicCallbackState::Open,
    } = entry
    else {
        return Ok(None);
    };
    if serde_json::to_vec(detail)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_CALLBACK_DETAIL_BYTES
    {
        return Err("callback detail exceeds the interactive safety limit".to_owned());
    }
    required_callback_text(title, "callback title")?;
    let turn_id = metadata
        .turn_id
        .clone()
        .ok_or_else(|| "active callback omitted its turn ID".to_owned())?;
    match detail.get("kind").and_then(Value::as_str) {
        Some("approval") => Ok(Some(PendingCallback {
            callback_id: callback_id.clone(),
            session_id: metadata.session_id.clone(),
            turn_id,
            kind: CallbackKind::Approval,
            prompt: title.clone(),
            options: approval_options(detail)?,
            allows_free_text: false,
            multi_select: false,
            questions: Vec::new(),
        })),
        Some("user_input") => {
            let questions = callback_questions(detail)?;
            let plan_review = detail
                .get("planReview")
                .or_else(|| detail.pointer("/request/planReview"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let options = if questions.len() == 1 {
                questions[0].options.clone()
            } else {
                Vec::new()
            };
            let allows_free_text = questions.len() == 1 && questions[0].allows_free_text;
            let multi_select = questions.len() == 1 && questions[0].multi_select;
            Ok(Some(PendingCallback {
                callback_id: callback_id.clone(),
                session_id: metadata.session_id.clone(),
                turn_id,
                kind: if plan_review {
                    CallbackKind::PlanReview
                } else {
                    CallbackKind::UserInput
                },
                prompt: callback_prompt(title, &questions, detail),
                options,
                allows_free_text,
                multi_select,
                questions,
            }))
        }
        Some(other) => Err(format!("unsupported callback kind `{other}`")),
        None => Err("callback detail omitted its kind".to_owned()),
    }
}

fn approval_options(detail: &Value) -> Result<Vec<CallbackOption>, String> {
    let choices = detail
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| "approval callback omitted its choices".to_owned())?;
    if choices.is_empty() || choices.len() > MAX_CALLBACK_OPTIONS {
        return Err("approval callback has an invalid choice count".to_owned());
    }
    choices
        .iter()
        .map(|choice| {
            let id = choice
                .as_str()
                .ok_or_else(|| "approval choice must be a string".to_owned())?;
            let (label, description) = match id {
                "approve" => ("Allow once", "Approve this invocation"),
                "approve_for_session" => (
                    "Allow for session",
                    "Approve matching requests for this session",
                ),
                "approve_permanently" => ("Always allow", "Persist approval for matching requests"),
                "deny" => ("Deny", "Reject this invocation"),
                "cancel_turn" => ("Cancel turn", "Reject and interrupt the turn"),
                _ => return Err(format!("unsupported approval decision `{id}`")),
            };
            Ok(CallbackOption {
                id: id.to_owned(),
                label: label.to_owned(),
                description: description.to_owned(),
            })
        })
        .collect()
}

fn callback_questions(detail: &Value) -> Result<Vec<CallbackQuestion>, String> {
    let questions = detail
        .pointer("/request/questions")
        .and_then(Value::as_array)
        .ok_or_else(|| "user-input callback omitted its questions".to_owned())?;
    if questions.is_empty() || questions.len() > MAX_CALLBACK_QUESTIONS {
        return Err("user-input callback has an invalid question count".to_owned());
    }
    questions
        .iter()
        .enumerate()
        .map(|(question_index, question)| {
            let question_text = question
                .get("question")
                .and_then(Value::as_str)
                .ok_or_else(|| "callback question omitted its text".to_owned())?;
            required_callback_text(question_text, "callback question")?;
            let header = question
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or_default();
            bounded_callback_text(header, "callback question header")?;
            let raw_options = question
                .get("options")
                .and_then(Value::as_array)
                .ok_or_else(|| "callback question omitted its options".to_owned())?;
            if raw_options.len() < 2 || raw_options.len() > MAX_CALLBACK_OPTIONS {
                return Err("callback question has an invalid option count".to_owned());
            }
            let options = raw_options
                .iter()
                .enumerate()
                .map(|(option_index, option)| {
                    let label = option
                        .get("label")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "callback option omitted its label".to_owned())?;
                    let description = option
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    required_callback_text(label, "callback option label")?;
                    bounded_callback_text(description, "callback option description")?;
                    let id = option
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| {
                            plan_option_id(label).map_or_else(
                                || format!("q{question_index}-o{option_index}"),
                                ToOwned::to_owned,
                            )
                        });
                    Ok(CallbackOption {
                        id,
                        label: label.to_owned(),
                        description: description.to_owned(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if options.iter().enumerate().any(|(index, option)| {
                options[..index]
                    .iter()
                    .any(|previous| previous.id == option.id)
            }) {
                return Err("callback question contains duplicate option IDs".to_owned());
            }
            Ok(CallbackQuestion {
                header: header.to_owned(),
                question: question_text.to_owned(),
                options,
                allows_free_text: !question
                    .get("hideOther")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn callback_prompt(title: &str, questions: &[CallbackQuestion], detail: &Value) -> String {
    let mut prompt = title.to_owned();
    for (index, question) in questions.iter().enumerate() {
        prompt.push_str(&format!(
            "\n{}. {}{}",
            index + 1,
            if question.header.is_empty() {
                String::new()
            } else {
                format!("{}: ", question.header)
            },
            question.question
        ));
        for (option_index, option) in question.options.iter().enumerate() {
            prompt.push_str(&format!(
                "\n   {}. {}{}",
                option_index + 1,
                option.label,
                if option.description.is_empty() {
                    String::new()
                } else {
                    format!(": {}", option.description)
                }
            ));
        }
        if question.allows_free_text {
            prompt.push_str("\n   Other: enter free text");
        }
    }
    if let Some(footer) = detail
        .pointer("/request/footerNote")
        .and_then(Value::as_str)
        .filter(|footer| !footer.is_empty())
    {
        prompt.push('\n');
        prompt.push_str(footer);
    }
    prompt
}

fn bounded_callback_text(value: &str, label: &str) -> Result<(), String> {
    if value.len() > MAX_CALLBACK_TEXT_BYTES {
        return Err(format!("{label} exceeds the safety limit"));
    }
    Ok(())
}

fn required_callback_text(value: &str, label: &str) -> Result<(), String> {
    bounded_callback_text(value, label)?;
    if value.trim().is_empty() {
        return Err(format!("{label} is empty"));
    }
    Ok(())
}

fn plan_option_id(label: &str) -> Option<&'static str> {
    match label {
        "Yes, clear context and auto approve edits" => Some("clear_auto"),
        "Yes, and auto approve edits" => Some("auto"),
        "Yes, and request approval for edits" => Some("manual"),
        "No" => Some("no"),
        _ => None,
    }
}

fn callback_choice_from_input(
    pending: &PendingCallback,
    input: &str,
) -> Result<CallbackChoice, String> {
    if pending.kind == CallbackKind::Approval {
        return Err("Use /approve [once|always|permanent] or /deny".to_owned());
    }
    let answer_texts = if pending.questions.len() == 1 {
        vec![input.trim().to_owned()]
    } else if let Ok(values) = serde_json::from_str::<Vec<String>>(input) {
        values
    } else {
        input
            .split('|')
            .map(str::trim)
            .map(ToOwned::to_owned)
            .collect()
    };
    if answer_texts.len() != pending.questions.len() {
        return Err(format!(
            "This callback requires {} atomic answers. Enter a JSON string array or separate answers with `|`",
            pending.questions.len()
        ));
    }
    let answers = pending
        .questions
        .iter()
        .zip(answer_texts)
        .map(|(question, answer)| question_choice(question, &answer))
        .collect::<Result<Vec<_>, _>>()?;
    if answers.len() == 1 {
        let answer = match answers.into_iter().next() {
            Some(answer) => answer,
            None => return Err("Callback answer disappeared during validation".to_owned()),
        };
        return Ok(match answer {
            UserInputChoice::Option { id } => CallbackChoice::Option { id },
            UserInputChoice::Options { ids } => CallbackChoice::Options { ids },
            UserInputChoice::FreeText { value } => CallbackChoice::FreeText { value },
        });
    }
    Ok(CallbackChoice::UserInput { answers })
}

fn question_choice(question: &CallbackQuestion, input: &str) -> Result<UserInputChoice, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Callback answers cannot be empty".to_owned());
    }
    if question.multi_select {
        let selected = input
            .split(',')
            .map(str::trim)
            .map(|value| callback_option_id(&question.options, value))
            .collect::<Option<Vec<_>>>();
        if let Some(ids) = selected.filter(|ids| !ids.is_empty()) {
            return Ok(UserInputChoice::Options { ids });
        }
    } else if let Some(id) = callback_option_id(&question.options, input) {
        return Ok(UserInputChoice::Option { id });
    }
    if question.allows_free_text {
        bounded_callback_text(input, "callback answer")?;
        return Ok(UserInputChoice::FreeText {
            value: input.to_owned(),
        });
    }
    Err("Answer with an option number or label shown in the callback".to_owned())
}

fn callback_option_id(options: &[CallbackOption], input: &str) -> Option<String> {
    input
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| options.get(index))
        .or_else(|| {
            options.iter().find(|option| {
                option.id.eq_ignore_ascii_case(input) || option.label.eq_ignore_ascii_case(input)
            })
        })
        .map(|option| option.id.clone())
}

fn history_entry(entry: PublicHistoryEntry) -> TranscriptEntry {
    let metadata = entry.metadata().clone();
    let completed = entry.is_completed();
    let mut details = serde_json::to_value(&entry).unwrap_or(Value::Null);
    let (kind, text) = match entry {
        PublicHistoryEntry::Message { role, content, .. } => {
            let kind = match role {
                PublicMessageRole::User => TranscriptKind::UserMessage,
                PublicMessageRole::Assistant => TranscriptKind::AssistantMessage,
                PublicMessageRole::System => TranscriptKind::Notice,
            };
            (kind, content_text(&content))
        }
        PublicHistoryEntry::Reasoning { text, summary, .. } => {
            let text = if text.is_empty() {
                summary.join("\n")
            } else {
                text
            };
            (TranscriptKind::Reasoning, text)
        }
        PublicHistoryEntry::Effect { title, state, .. } => {
            let encoded = serde_json::to_value(state).unwrap_or(Value::Null);
            let output = encoded
                .get("outputText")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(object) = details.as_object_mut() {
                object.insert(
                    "durationMs".to_owned(),
                    encoded.get("durationMs").cloned().unwrap_or(Value::Null),
                );
                object.insert(
                    "error".to_owned(),
                    encoded
                        .pointer("/error/message")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                object.insert(
                    "presentationKind".to_owned(),
                    encoded
                        .pointer("/display/kind")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                object.insert(
                    "diff".to_owned(),
                    encoded
                        .pointer("/display/lines")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
            }
            (
                TranscriptKind::Effect,
                if output.is_empty() {
                    title
                } else {
                    format!("{title}\n{output}")
                },
            )
        }
        PublicHistoryEntry::Callback { title, detail, .. } => {
            (TranscriptKind::Callback, format!("{title}\n{detail}"))
        }
        PublicHistoryEntry::Checkpoint { kind, message, .. } => (
            TranscriptKind::Checkpoint,
            message.unwrap_or_else(|| format!("Checkpoint: {kind}")),
        ),
        PublicHistoryEntry::Notice { message, .. } => (TranscriptKind::Notice, message),
    };
    let status_name = details.get("status").and_then(Value::as_str);
    let status = match status_name {
        Some("failed") => EntryStatus::Failed,
        Some("cancelled") | Some("expired") => EntryStatus::Cancelled,
        _ if completed => EntryStatus::Completed,
        _ => EntryStatus::Streaming,
    };
    TranscriptEntry {
        id: metadata.id,
        revision: metadata.updated_at.max(1),
        kind,
        text,
        status,
        details,
    }
}

fn content_text(content: &[PublicContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            PublicContentBlock::Text { text } => text.clone(),
            PublicContentBlock::Image { attachment } => {
                format!(
                    "[image: {}]",
                    attachment
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unsupported terminal image")
                )
            }
            PublicContentBlock::Resource { resource } => format!(
                "[resource: {}]",
                resource
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("resource")
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_local_notice(state: &mut TuiState, message: &str, status: EntryStatus) {
    let entry = TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: message.to_owned(),
        status,
        details: json!({"source": "tui"}),
    };
    state.append_local(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_app_server::client::TurnRequest;

    fn interactive_test_runtime(session_id: &str) -> InteractiveRuntime {
        let driver = Arc::new(
            LiveTurnDriver::from_credential(
                LiveDriverConfig {
                    style: "mistral".to_owned(),
                    endpoint: "http://127.0.0.1:1/v1".to_owned(),
                    model: "test-model".to_owned(),
                    credential_environment: "TEST_CREDENTIAL".to_owned(),
                    system_prompt: "test".to_owned(),
                    session_root: None,
                    input_price_per_million_micros: 0,
                    output_price_per_million_micros: 0,
                },
                "test-credential".to_owned(),
            )
            .expect("test driver"),
        );
        let mut service =
            HeadlessService::new_interactive_shared_with_server(driver, AppServer::default())
                .expect("test service");
        let session_id = service
            .start_session(&SessionOptions {
                working_directory: "/workspace".to_owned(),
                session_id: Some(session_id.to_owned()),
                add_directories: Vec::new(),
                trusted: true,
                agent: None,
                tool_filters: Vec::new(),
                enabled_tools: Vec::new(),
                disabled_tools: Vec::new(),
                mcp_servers: Vec::new(),
                model: Some("test-model".to_owned()),
                max_turns: None,
                max_tokens: None,
                max_price_micros: None,
                mode: None,
                thinking: false,
                reasoning_effort: None,
                auto_approve: true,
                resume: None,
                continue_session: false,
            })
            .expect("session starts");
        InteractiveRuntime {
            service,
            session_id,
            model: "test-model".to_owned(),
            thinking: "off".to_owned(),
            mode: "code".to_owned(),
            agent_name: "default".to_owned(),
            banner: BannerMetrics::default(),
            context_tokens: 0,
            context_window: DEFAULT_CONTEXT_WINDOW,
            auto_approve: true,
            clear_context_after_turn: false,
            skills: BTreeMap::new(),
            shell: None,
            cloud: CloudWorkflowState::default(),
            telemetry: None,
        }
    }

    #[test]
    fn rich_content_has_safe_terminal_fallbacks() {
        let content = vec![
            PublicContentBlock::Text {
                text: "hello".to_owned(),
            },
            PublicContentBlock::Image {
                attachment: json!({"name": "diagram.png"}),
            },
            PublicContentBlock::Resource {
                resource: json!({"name": "README.md"}),
            },
        ];
        assert_eq!(
            content_text(&content),
            "hello\n[image: diagram.png]\n[resource: README.md]"
        );
    }

    #[test]
    fn local_notices_do_not_advance_the_server_watermark() {
        let mut state = TuiState::new("session");
        push_local_notice(&mut state, "ready", EntryStatus::Completed);
        push_local_notice(&mut state, "done", EntryStatus::Completed);
        assert_eq!(state.watermark, 0);
        assert_eq!(state.entries.len(), 2);
    }

    #[test]
    fn close_is_a_canonical_exit_alias() {
        assert!(is_exit_command("/close"));
        assert!(is_exit_command("/exit"));
        assert!(is_exit_command("/quit"));
        assert!(!is_exit_command("/close now"));
    }

    #[test]
    fn public_session_history_hydrates_the_initial_projection() {
        let state = tui_state_from_public_session(
            "session",
            &json!({
                "eventId": 9,
                "session": {
                    "id": "session",
                    "status": {"type": "running"}
                },
                "history": {
                    "entries": [{
                        "type": "message",
                        "id": "restored-user",
                        "sessionId": "session",
                        "createdAt": 1,
                        "updatedAt": 2,
                        "generationStatus": "completed",
                        "role": "user",
                        "content": [{"type": "text", "text": "restored prompt"}]
                    }],
                    "cursor": {"before": "older", "after": null}
                }
            }),
        )
        .expect("public session state hydrates");

        assert_eq!(state.watermark, 9);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].text, "restored prompt");
        assert_eq!(state.cursor_before.as_deref(), Some("older"));
        assert!(state.waiting);
        assert!(state.ready);
    }

    #[test]
    fn persisted_history_fallback_is_bounded_and_semantic() {
        let messages = json!([
            {"role": "system", "content": "internal"},
            {"role": "user", "content": "restored question"},
            {
                "role": "assistant",
                "content": "restored answer",
                "reasoning": null,
                "reasoning_state": [],
                "tool_calls": []
            },
            {"role": "tool", "call_id": "shell-1", "content": "failed", "is_error": true}
        ]);
        let messages =
            serde_json::from_value::<Vec<PersistedMessage>>(messages).expect("typed history");
        let entries = transcript_entries_from_history("saved", 40, &messages);

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "persisted:saved:41");
        assert_eq!(entries[0].kind, TranscriptKind::UserMessage);
        assert_eq!(entries[1].kind, TranscriptKind::AssistantMessage);
        assert_eq!(entries[2].kind, TranscriptKind::Effect);
        assert_eq!(entries[2].status, EntryStatus::Failed);
    }

    #[test]
    fn active_callbacks_are_included_when_the_history_window_omits_them() {
        let state = tui_state_from_public_session(
            "session",
            &json!({
                "eventId": 4,
                "session": {
                    "id": "session",
                    "status": {"type": "blocked"}
                },
                "history": {"entries": [], "cursor": {"before": null, "after": null}},
                "activeCallbacks": [{
                    "type": "callback",
                    "id": "callback-entry",
                    "sessionId": "session",
                    "turnId": "turn",
                    "createdAt": 1,
                    "updatedAt": 1,
                    "generationStatus": "in_progress",
                    "callbackId": "callback-1",
                    "title": "Approve?",
                    "detail": {
                        "kind": "approval",
                        "choices": ["approve", "deny", "cancel_turn"]
                    },
                    "state": {"status": "open"}
                }]
            }),
        )
        .expect("active callback hydrates");
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].kind, TranscriptKind::Callback);
        assert_eq!(state.entries[0].status, EntryStatus::Streaming);
    }

    #[test]
    fn malformed_public_history_is_not_mistaken_for_empty_history() {
        let malformed = BTreeMap::from([("history".to_owned(), json!({"role": "user"}))]);
        assert!(decode_public_result::<PublicHistoryList>(malformed).is_err());
    }

    #[test]
    fn canonical_multi_question_callbacks_preserve_structure_and_atomic_answers() {
        let entry = serde_json::from_value::<PublicHistoryEntry>(json!({
            "type": "callback",
            "id": "callback-entry",
            "sessionId": "session",
            "turnId": "turn",
            "createdAt": 1,
            "updatedAt": 1,
            "generationStatus": "in_progress",
            "callbackId": "callback-1",
            "title": "Need input\u{1b}[31m",
            "detail": {
                "kind": "user_input",
                "request": {
                    "questions": [
                        {
                            "header": "Runtime",
                            "question": "Choose runtimes",
                            "options": [
                                {"label": "Rust", "description": "Native"},
                                {"label": "Python", "description": "Portable"}
                            ],
                            "multiSelect": true,
                            "hideOther": true
                        },
                        {
                            "header": "Constraint",
                            "question": "Any constraints? \u{4e16}\u{754c}",
                            "options": [
                                {"label": "Fast", "description": ""},
                                {"label": "Small", "description": ""}
                            ],
                            "multiSelect": false,
                            "hideOther": false
                        }
                    ],
                    "footerNote": "All answers are submitted together."
                }
            },
            "state": {"status": "open"}
        }))
        .expect("callback fixture");
        let pending = pending_callback_from_entry(&entry)
            .expect("callback is valid")
            .expect("callback is active");
        assert_eq!(pending.questions.len(), 2);
        assert!(pending.questions[0].multi_select);
        assert!(
            pending
                .prompt
                .contains("All answers are submitted together.")
        );

        let choice = callback_choice_from_input(&pending, r#"["1, 2", "No network \u2713"]"#)
            .expect("atomic answers parse");
        let CallbackChoice::UserInput { answers } = choice else {
            panic!("multi-question callback must stay atomic");
        };
        assert_eq!(answers.len(), 2);
        assert!(matches!(
            &answers[0],
            UserInputChoice::Options { ids } if ids.len() == 2
        ));
        assert!(matches!(
            &answers[1],
            UserInputChoice::FreeText { value } if value == "No network \u{2713}"
        ));
    }

    #[test]
    fn plan_review_choices_and_callback_bounds_are_exact() {
        assert_eq!(
            plan_option_id("Yes, clear context and auto approve edits"),
            Some("clear_auto")
        );
        assert_eq!(
            plan_transition(&CallbackChoice::Option {
                id: "manual".to_owned()
            }),
            Some((false, false))
        );
        assert_eq!(
            plan_transition(&CallbackChoice::FreeText {
                value: "Revise it".to_owned()
            }),
            None
        );
        assert!(
            required_callback_text(&"x".repeat(MAX_CALLBACK_TEXT_BYTES + 1), "callback").is_err()
        );
    }

    #[test]
    fn committed_callback_driver_error_reconciles_stale_controls() {
        let mut runtime = interactive_test_runtime("callback-race-session");
        let session_id = runtime.session_id.clone();
        let mut state = canonical_session_projection(&mut runtime, &session_id, false)
            .expect("initial projection");
        let mut controls = ControlState::new(&session_id);
        controls.begin_turn("turn").expect("turn begins");
        controls
            .present_callback(PendingCallback {
                callback_id: "committed-callback".to_owned(),
                session_id,
                turn_id: "turn".to_owned(),
                kind: CallbackKind::Approval,
                prompt: "Approve?".to_owned(),
                options: Vec::new(),
                allows_free_text: false,
                multi_select: false,
                questions: Vec::new(),
            })
            .expect("local callback presents");
        assert_eq!(controls.focus, controls::ControlFocus::Callback);

        recover_from_callback_response_error(
            &mut runtime,
            &mut controls,
            &mut state,
            "driver failed after commit",
        );

        assert!(controls.pending_callback().is_none());
        assert_eq!(controls.focus, controls::ControlFocus::Prompt);
        assert!(
            state
                .diagnostics()
                .any(|diagnostic| diagnostic.contains("driver failed after commit"))
        );
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
            let mut active = Some(ActiveTurn {
                turn_id,
                scheduled_loop_id: None,
                cancel_requested: false,
                updates,
                task,
            });
            while !active
                .as_ref()
                .is_some_and(|active| active.task.is_finished())
            {
                tokio::task::yield_now().await;
            }

            finish_active(&mut state, &mut controls, &mut runtime, &mut active)
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
}
