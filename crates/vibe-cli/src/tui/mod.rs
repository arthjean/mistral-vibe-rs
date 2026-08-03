pub mod attachments;
mod callback;
pub mod chat_input;
pub mod clipboard;
mod clipboard_images;
mod cloud_workflow;
pub mod commands;
pub mod completion;
mod composer;
mod composer_layout;
pub mod controls;
mod external_action;
mod feedback;
pub mod history;
pub mod input;
pub mod interaction;
mod path_mentions;
mod path_normalization;
mod path_resources;
pub mod pickers;
mod plan_review;
mod prompt;
mod queue;
mod remote_project_workflow;
pub mod render;
pub mod rewind;
#[cfg(test)]
mod runtime_parity_tests;
mod session_picker;
pub mod setup;
mod shell;
mod shortcuts;
pub mod startup;
pub mod state;
mod switching;
pub mod terminal;
mod voice;
mod workflow;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures_util::{FutureExt, Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use vibe_app_server::client::{
    DriverError, HeadlessService, InterruptOutcome, LiveDriverConfig, LiveTurnDriver,
    ProgrammaticUpdate, PublicContentBlock, PublicDispatch, PublicHistoryEntry, PublicMessageRole,
    PublicTurnOutcome, SessionOptions, TurnDriver, TurnReservation,
};
use vibe_app_server::release3::Release3Service;
use vibe_app_server::release4::{Release4Service, VibeCodeCloudConfig};
use vibe_app_server::resources::{
    CoreResourceBackend, MistralConnectorClient, production_mcp_adapters,
};
use vibe_app_server::server::AppServer;

use self::callback::{
    cancel_open_callback_notices, drain_callback_requests, fail_open_callback_notices,
    respond_to_pending_callback, sync_active_callbacks, sync_callback_presentation,
};
#[cfg(test)]
use self::callback::{
    fail_inactive_callback_notices, pending_callback_from_entry, plan_approval_error,
    recover_from_callback_response_error,
};
use self::chat_input::{ChatInputState, InputEffect, InputEvent, Safety};
use self::clipboard_images::{ClipboardImageManager, ImageModel, ImageModels};
use self::cloud_workflow::{
    CloudWorkflowState, format_cancelled_loop, format_created_loop, format_loop_list,
};
use self::commands::{CommandContext, CommandId};
use self::composer::{
    apply_effects as apply_composer_effects, apply_event as apply_composer_event,
    normalized_key_event as normalized_input_event,
};
use self::controls::{ApprovalScope, CallbackChoice, CallbackRequest, ControlState};
#[cfg(test)]
use self::controls::{CallbackEffect, PendingCallback};
use self::history::PromptHistory;
use self::input::{ExternalEditorPort, SystemExternalEditor};
use self::interaction::Overlay;
use self::path_normalization::PathNormalizationManager;
use self::plan_review::PlanReviewMonitor;
use self::prompt::{PromptContext, enqueue_prompt, is_user_skill, start_prompt};
use self::queue::start_next_queued_prompt;
use self::remote_project_workflow::{
    handle_project_action, handle_project_command, handle_teleport_command, start_teleport,
};
use self::render::{BannerContext, TokenState, UiContext, draw};
use self::setup::{
    CredentialStore, EnvironmentThemeDetector, NativeCredentialStore, NotificationPreference,
    ResolvedTheme, SetupCompletion, SetupFlow, SetupProgress, TerminalThemeDetector, Theme,
    resolve_theme,
};
use self::shell::{ActiveShell, finish_shell, interrupt_shell, start_shell};
use self::shortcuts::{copy_prompt_selection, resume_paused_queue};
#[cfg(test)]
use self::state::PlanReviewState;
use self::state::{
    ApplyResult, EntryStatus, ServerEvent, TranscriptEntry, TranscriptKind, TuiSnapshot, TuiState,
};
use self::terminal::{CrosstermOps, TerminalGuard};
use self::voice::VoiceManager;
use self::workflow::{
    CommandAction, OverlayEffect, OverlayKeyResult, RuntimeCommand, SystemUrlOpener,
    apply_thinking, cycle_agent, dispatch_command, execute_mcp_effect, handle_overlay_key,
    show_rewind,
};
use crate::{
    Arguments, CliError, CliTelemetryObserver, price_per_million_micros, telemetry_event_observer,
    validate_arguments,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const INITIAL_HISTORY_LIMIT: usize = 200;
const DEFAULT_MODEL: &str = "mistral-medium-3.5";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationPhase {
    Active,
    DriverOnly,
    Complete,
}

struct ActiveTurn {
    turn_id: String,
    scheduled_loop_id: Option<String>,
    cancellation: CancellationPhase,
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
    image_models: ImageModels,
    thinking: String,
    mode: String,
    agent_name: String,
    safety: Safety,
    banner: BannerMetrics,
    context_tokens: u64,
    context_window: u64,
    auto_approve: bool,
    vibe_code_enabled: bool,
    clear_context_after_turn: bool,
    config_target: Option<interaction::ConfigLayerTarget>,
    remote_project_overlay: Option<Overlay>,
    remote_project_draft: Option<interaction::RemoteProjectDraft>,
    ui_operation_sender: Option<tokio::sync::mpsc::UnboundedSender<UiOperationCompletion>>,
    ui_operation_generation: u64,
    active_ui_operation: Option<u64>,
    skills: BTreeMap<String, RuntimeSkill>,
    shell: Option<ActiveShell>,
    cloud: CloudWorkflowState,
    pending_switch: Option<switching::SwitchRequest>,
    telemetry: Option<Arc<CliTelemetryObserver>>,
    voice: VoiceManager,
}

#[derive(Debug, Clone)]
enum UiOperation {
    Mcp(workflow::McpPendingOperation),
    RemoteProject(remote_project_workflow::ProjectPendingOperation),
}

struct UiOperationCompletion {
    generation: u64,
    operation: UiOperation,
    result: Result<PublicDispatch, String>,
}

fn schedule_ui_call(
    runtime: &mut InteractiveRuntime,
    method: &str,
    mut params: Value,
    operation: UiOperation,
    state: &mut TuiState,
) -> bool {
    if runtime.active_ui_operation.is_some() {
        state.push_diagnostic("An interactive operation is already in progress");
        return false;
    }
    let Some(sender) = runtime.ui_operation_sender.clone() else {
        state.push_diagnostic("Interactive operation channel is unavailable");
        return false;
    };
    let Some(params) = params.as_object_mut() else {
        state.push_diagnostic("Interactive operation parameters must be an object");
        return false;
    };
    params
        .entry("sessionId")
        .or_insert_with(|| json!(runtime.session_id));
    let pending = match runtime
        .service
        .begin_public_call(method, Value::Object(params.clone()))
    {
        Ok(pending) => pending,
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return false;
        }
    };
    runtime.ui_operation_generation = runtime.ui_operation_generation.saturating_add(1);
    let generation = runtime.ui_operation_generation;
    runtime.active_ui_operation = Some(generation);
    tokio::spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(30), pending.complete())
            .await
            .map_err(|_| "Interactive operation timed out".to_owned())
            .and_then(|result| result.map_err(|error| error.to_string()));
        let _ = sender.send(UiOperationCompletion {
            generation,
            operation,
            result,
        });
    });
    true
}

fn schedule_ui_external<F>(
    runtime: &mut InteractiveRuntime,
    operation: UiOperation,
    work: F,
    state: &mut TuiState,
) -> bool
where
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    if runtime.active_ui_operation.is_some() {
        state.push_diagnostic("An interactive operation is already in progress");
        return false;
    }
    let Some(sender) = runtime.ui_operation_sender.clone() else {
        state.push_diagnostic("Interactive operation channel is unavailable");
        return false;
    };
    runtime.ui_operation_generation = runtime.ui_operation_generation.saturating_add(1);
    let generation = runtime.ui_operation_generation;
    runtime.active_ui_operation = Some(generation);
    tokio::spawn(async move {
        let result = work.await.map(|()| PublicDispatch {
            result: BTreeMap::new(),
            notifications: Vec::new(),
        });
        let _ = sender.send(UiOperationCompletion {
            generation,
            operation,
            result,
        });
    });
    true
}

fn apply_ui_operation_completion(
    completion: UiOperationCompletion,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) {
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if runtime.active_ui_operation != Some(completion.generation) {
        return;
    }
    runtime.active_ui_operation = None;
    if let Ok(dispatch) = &completion.result {
        apply_public_notifications(dispatch, state);
    }
    match completion.operation {
        UiOperation::Mcp(operation) => {
            workflow::apply_pending_operation(operation, completion.result, runtime, state);
        }
        UiOperation::RemoteProject(operation) => {
            remote_project_workflow::apply_pending_operation(
                operation,
                completion.result,
                runtime,
                state,
            );
        }
    }
}

impl InteractiveRuntime {
    fn image_model(&self) -> ImageModel<'_> {
        self.image_models.get(&self.model)
    }

    fn supports_images(&self) -> bool {
        self.image_model().supports_images
    }
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

#[derive(Debug)]
pub struct InteractiveExit {
    pub session_started: bool,
    pub initialization_error: Option<CliError>,
}

const MAX_FATAL_INPUT_DRAIN: usize = 256;
const MAX_READY_INTERRUPTS_TO_DRAIN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadyInputDrain {
    Empty,
    Saturated,
    Closed,
}

fn drain_ready_terminal_events<S, E>(events: &mut S) -> Result<ReadyInputDrain, CliError>
where
    S: Stream<Item = Result<Event, E>> + Unpin,
    E: std::fmt::Display,
{
    for _ in 0..MAX_FATAL_INPUT_DRAIN {
        match events.next().now_or_never() {
            Some(Some(Ok(_))) => {}
            Some(Some(Err(error))) => return Err(CliError::Terminal(error.to_string())),
            Some(None) => return Ok(ReadyInputDrain::Closed),
            None => return Ok(ReadyInputDrain::Empty),
        }
    }
    Ok(ReadyInputDrain::Saturated)
}

fn drain_ready_interrupts<F, E, R>(
    interrupt: &mut Pin<Box<F>>,
    mut recreate: R,
) -> Result<usize, CliError>
where
    F: Future<Output = Result<(), E>>,
    E: std::fmt::Display,
    R: FnMut() -> F,
{
    let mut drained = 0;
    while drained < MAX_READY_INTERRUPTS_TO_DRAIN {
        let Some(signal) = interrupt.as_mut().now_or_never() else {
            return Ok(drained);
        };
        signal.map_err(|error| CliError::Terminal(error.to_string()))?;
        drained += 1;
        interrupt.set(recreate());
    }
    Ok(drained)
}

pub async fn run_interactive(
    invocation: startup::InteractiveInvocation,
) -> Result<InteractiveExit, CliError> {
    let startup::InteractiveInvocation {
        mut arguments,
        post_mount_action,
        ..
    } = invocation;
    validate_arguments(&arguments)?;
    let working_directory = match &arguments.workdir {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(CliError::CurrentDirectory)?,
    };
    let startup_host = startup::startup_host(&arguments, &working_directory);
    let trust = startup::resolve_workspace_trust(&mut arguments, &startup_host)?;
    if trust.cancelled {
        return Ok(InteractiveExit {
            session_started: false,
            initialization_error: None,
        });
    }
    if !startup::resolve_location_safety(trust.dangerous_warning.as_deref())? {
        return Ok(InteractiveExit {
            session_started: false,
            initialization_error: None,
        });
    }
    match startup::resolve_bare_resume(&arguments, &startup_host)? {
        startup::ResumeResolution::Unchanged => {}
        startup::ResumeResolution::StartNew => arguments.resume = None,
        startup::ResumeResolution::Resume(session_id) => {
            arguments.resume = Some(session_id);
            arguments.continue_session = false;
        }
        startup::ResumeResolution::Abort => {
            return Ok(InteractiveExit {
                session_started: false,
                initialization_error: None,
            });
        }
    }
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
    let release3 = startup_host
        .into_release3(arguments.trust)
        .map_err(startup::StartupError::from)?;
    let fallback_banner = banner_metrics_from_release3(&release3, &arguments, &working_directory);
    let mut runtime = match initial_credential {
        Some(credential) => Some(start_runtime(
            &arguments,
            &working_directory,
            release3,
            credential,
        )?),
        None => None,
    };
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
            "Native credential lookup failed: {error}. Restart with --setup after repairing keyring access"
        ));
    }
    if runtime.is_none() {
        push_local_notice(
            &mut state,
            "Setup is required before starting a session. Restart with --setup to store an API key in the native keyring.",
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
    let (mut prompt_history, history_load) = PromptHistory::open(
        startup::vibe_home_directory(&arguments, &working_directory).join("vibehistory"),
    );
    let mut input = ChatInputState::new();
    input.set_voice_enabled(
        runtime
            .as_ref()
            .is_some_and(|runtime| runtime.voice.enabled()),
    );
    input.replace_history(history_load.entries);
    input.set_viewport_width(state.viewport.0);
    input.set_command_context(CommandContext::new(teleport_available(runtime.as_ref())));
    if let Some(diagnostic) = history_load.diagnostic {
        state.push_diagnostic(diagnostic);
    }
    if let Some(runtime) = runtime.as_ref() {
        input.set_user_skills(
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
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let _ = drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut voice_ticker = tokio::time::interval(Duration::from_millis(100));
    voice_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut loop_ticker = tokio::time::interval(Duration::from_secs(1));
    loop_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut active: Option<ActiveTurn> = None;
    let mut clipboard_images = ClipboardImageManager::default();
    let mut path_normalization = PathNormalizationManager::new().map_err(CliError::Terminal)?;
    let mut deferred_enter = None;
    let (ui_operation_sender, mut ui_operation_receiver) = tokio::sync::mpsc::unbounded_channel();
    if let Some(runtime) = runtime.as_mut() {
        runtime.ui_operation_sender = Some(ui_operation_sender.clone());
    }
    let mut session_started = runtime.is_some();
    let mut mounted_startup = startup::MountedStartup::new(post_mount_action);
    let mut plan_review_monitor = PlanReviewMonitor::default();
    state.waiting |= runtime.is_some();

    let event_loop = async {
        let mut exit = false;
        while !exit {
            session_started |= runtime.is_some();
            if let Some(runtime) = runtime.as_mut() {
                if runtime.ui_operation_sender.is_none() {
                    runtime.ui_operation_sender = Some(ui_operation_sender.clone());
                }
                while let Some(event) = runtime.voice.try_next_event() {
                    apply_composer_event(
                        &mut input,
                        event,
                        &working_directory,
                        &mut state,
                    );
                }
            }
            for diagnostic in prompt_history.drain_ready().await {
                state.push_diagnostic(diagnostic);
            }
            drain_updates(&mut state, runtime.as_mut(), active.as_mut(), &mut controls);
            drain_callback_requests(runtime.as_mut(), &mut state, &mut controls);
            finish_active(
                &mut state,
                &mut controls,
                &mut runtime,
                &mut active,
                &mut input,
            )
            .await?;
            finish_shell(runtime.as_mut(), &mut state).await;
            start_next_queued_prompt(PromptContext::new(
                &working_directory,
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
                &mut clipboard_images,
            ))
            .await?;
            let effects = input.poll_completion();
            apply_composer_effects(
                &mut input,
                effects,
                &working_directory,
                &mut state,
            );
            let now_ms = unix_millis();
            let plan_path = controls
                .pending_callback()
                .and_then(|pending| match &pending.request {
                    CallbackRequest::PlanReview { plan_path, .. } => Some(plan_path.clone()),
                    CallbackRequest::Approval { .. } | CallbackRequest::UserInput { .. } => None,
                });
            plan_review_monitor.sync(plan_path, &mut state).await;
            sync_callback_presentation(&controls, &mut state, now_ms);
            if !path_normalization.has_pending()
                && let Some(key) = deferred_enter.take()
            {
                exit = handle_terminal_event(
                    Event::Key(key),
                    &arguments,
                    &working_directory,
                    &credential_store,
                    &mut runtime,
                    &mut active,
                    &mut state,
                    &mut controls,
                    &mut prompt_history,
                    &mut input,
                    &mut setup_flow,
                    &mut secret_input,
                    &mut theme,
                    &mut terminal_guard,
                    &mut terminal,
                    &mut clipboard_images,
                    &mut path_normalization,
                    &mut deferred_enter,
                )
                .await?;
                continue;
            }
            let runtime_view = runtime.as_ref();
            let agent_name =
                runtime_view.map_or("default", |runtime| runtime.agent_name.as_str());
            let border_title = format!(" {} ", agent_name.to_lowercase());
            input.set_agent_name(agent_name);
            input.set_safety(runtime_view.map_or(Safety::Neutral, |runtime| runtime.safety));
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
                        input.editor(),
                        input.completion(),
                        input.mode(),
                        theme,
                        UiContext {
                            cwd: &working_directory,
                            agent_name: &border_title,
                            secret_input,
                            safety: input.safety(),
                            switching: input.switching(),
                            feedback_active: input.feedback_active(),
                            voice_phase: input.voice_phase(),
                            voice_indicator: input.voice_indicator(),
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
            if mounted_startup.needs_fatal_render() {
                match drain_ready_terminal_events(&mut events)? {
                    ReadyInputDrain::Empty => {
                        let _ =
                            drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
                        mounted_startup.arm_fatal_acknowledgement();
                    }
                    ReadyInputDrain::Saturated => continue,
                    ReadyInputDrain::Closed => {
                        exit = true;
                        continue;
                    }
                }
            }
            startup::complete_mounted_startup(
                &mut mounted_startup,
                &working_directory,
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
                &mut input,
                &mut clipboard_images,
            )
            .await?;
            if mounted_startup.needs_fatal_render() {
                continue;
            }
            tokio::select! {
                signal = interrupt.as_mut() => {
                    signal.map_err(|error| CliError::Terminal(error.to_string()))?;
                    interrupt.set(tokio::signal::ctrl_c());
                    let _ = drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
                    if mounted_startup.is_fatal() {
                        exit = true;
                    } else if active.is_some() && runtime.is_some() {
                        if active
                            .as_ref()
                            .is_some_and(|active| active.cancellation != CancellationPhase::Active)
                        {
                            exit = true;
                        } else {
                            request_active_turn_interrupt(
                                &mut runtime,
                                &mut active,
                                &mut controls,
                                &mut state,
                            );
                        }
                    } else if let Some(runtime) = runtime.as_mut()
                        && runtime.shell.is_some()
                    {
                        interrupt_shell(runtime, &mut state).await;
                    } else {
                        exit = true;
                    }
                }
                event = events.next(), if deferred_enter.is_none() => {
                    match event {
                        Some(Ok(Event::Key(key))) if mounted_startup.is_awaiting_fatal_key()
                            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            exit = true;
                        }
                        Some(Ok(_)) if mounted_startup.is_fatal() => {}
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
                                &mut prompt_history,
                                &mut input,
                                &mut setup_flow,
                                &mut secret_input,
                                &mut theme,
                                &mut terminal_guard,
                                &mut terminal,
                                &mut clipboard_images,
                                &mut path_normalization,
                                &mut deferred_enter,
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
                completion = clipboard_images.next_completion(), if clipboard_images.has_pending_capture() => {
                    if let Some(completion) = completion {
                        clipboard_images.apply_completion(
                            completion,
                            runtime.as_ref().map(InteractiveRuntime::image_model),
                            &mut input,
                            &mut state,
                        ).await;
                    }
                }
                completion = ui_operation_receiver.recv() => {
                    let completion = completion.ok_or_else(|| {
                        CliError::Terminal("interactive operation worker stopped".to_owned())
                    })?;
                    apply_ui_operation_completion(completion, &mut runtime, &mut state);
                }
                event = path_normalization.next_event(), if path_normalization.has_pending() => {
                    let event = event.ok_or_else(|| {
                        CliError::Terminal("path normalization worker stopped".to_owned())
                    })?;
                    apply_path_normalization_event(
                        &mut path_normalization,
                        &mut input,
                        event,
                        &working_directory,
                        &mut state,
                    )?;
                }
                _ = ticker.tick() => {
                    if let Some(runtime) = runtime.as_mut() {
                        switching::apply_pending(runtime, &mut input, &mut state);
                    }
                }
                _ = voice_ticker.tick() => {
                    if input.voice_phase() == self::chat_input::VoicePhase::Transcribing {
                        apply_composer_event(
                            &mut input,
                            InputEvent::VoiceIndicatorTick,
                            &working_directory,
                            &mut state,
                        );
                    }
                }
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
                        match start_active_turn(
                            &runtime.service,
                            scheduled.reservation,
                            Some(scheduled.loop_id),
                            state.watermark,
                            &mut controls,
                        ) {
                            Ok(started) => {
                                active = Some(started);
                                state.waiting = true;
                            }
                            Err(failure) => {
                                let (reservation, error) = *failure;
                                let failure = format!(
                                    "Reserved scheduled turn could not start locally: {error}"
                                );
                                settle_unstarted_reservation(
                                    runtime,
                                    &mut state,
                                    &reservation,
                                    &failure,
                                );
                            }
                        }
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
    for diagnostic in prompt_history.finish().await {
        eprintln!("{diagnostic}");
    }
    let interrupt_result =
        if let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_ref()) {
            runtime
                .service
                .interrupt(&runtime.session_id, &active.turn_id)
                .map(|_| ())
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
        runtime.voice.shutdown().await;
    }
    let cleanup_result = clipboard_images
        .shutdown()
        .await
        .map_err(CliError::Terminal);
    path_normalization.shutdown();
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
    let initialization_error = mounted_startup.into_initialization_error();
    let result = event_loop
        .and(restoration)
        .and(interrupt_result)
        .and(cleanup_result)
        .and(close_result)
        .and(shutdown_result);
    if let Some(telemetry) = telemetry {
        telemetry.flush().await;
    }
    result.map(|()| InteractiveExit {
        session_started,
        initialization_error,
    })
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
    prompt_history: &mut PromptHistory,
    input: &mut ChatInputState,
    setup_flow: &mut Option<SetupFlow>,
    secret_input: &mut bool,
    theme: &mut ResolvedTheme,
    terminal_guard: &mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    clipboard_images: &mut ClipboardImageManager,
    path_normalization: &mut PathNormalizationManager,
    deferred_enter: &mut Option<KeyEvent>,
) -> Result<bool, CliError> {
    match event {
        Event::Resize(width, height) => {
            state.resize(width, height);
            apply_composer_event(
                input,
                InputEvent::Resize { width, height },
                working_directory,
                state,
            );
        }
        Event::Paste(text) => {
            let effects =
                apply_composer_event(input, InputEvent::Paste { text }, working_directory, state);
            clipboard_images.schedule_effects(&effects);
            path_normalization
                .schedule_effects(&effects)
                .map_err(CliError::Terminal)?;
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                state.scroll_up(3);
                page_older_history(runtime.as_mut(), state);
            }
            MouseEventKind::ScrollDown => {
                state.scroll_down(3);
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
                let screen = Rect::new(0, 0, state.viewport.0, state.viewport.1);
                if let Some((row, column)) = self::render::editor_mouse_cell(
                    input.editor(),
                    screen,
                    *secret_input,
                    input.mode(),
                    mouse.column,
                    mouse.row,
                ) {
                    let extend_selection = matches!(mouse.kind, MouseEventKind::Drag(_));
                    apply_composer_event(
                        input,
                        InputEvent::Mouse {
                            x: u16::try_from(column).unwrap_or(u16::MAX),
                            y: u16::try_from(row).unwrap_or(u16::MAX),
                            extend_selection,
                        },
                        working_directory,
                        state,
                    );
                }
            }
            _ => {}
        },
        Event::Key(key) if accepts_key_event(key.kind) => {
            if should_defer_submission(key, path_normalization.has_pending()) {
                *deferred_enter = Some(key);
                return Ok(false);
            }
            return handle_key(
                key,
                arguments,
                working_directory,
                credential_store,
                runtime,
                active,
                state,
                controls,
                prompt_history,
                input,
                setup_flow,
                secret_input,
                theme,
                terminal_guard,
                terminal,
                clipboard_images,
                path_normalization,
            )
            .await;
        }
        Event::FocusGained | Event::FocusLost | Event::Key(_) => {}
    }
    Ok(false)
}

const fn accepts_key_event(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn should_defer_submission(key: KeyEvent, normalization_pending: bool) -> bool {
    normalization_pending && key.code == KeyCode::Enter && key.modifiers.is_empty()
}

fn teleport_available(runtime: Option<&InteractiveRuntime>) -> bool {
    runtime.is_some_and(|runtime| runtime.vibe_code_enabled)
}

fn start_active_turn(
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

fn settle_unstarted_reservation(
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

fn request_active_turn_interrupt(
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
    prompt_history: &mut PromptHistory,
    input: &mut ChatInputState,
    setup_flow: &mut Option<SetupFlow>,
    secret_input: &mut bool,
    theme: &mut ResolvedTheme,
    terminal_guard: &mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    clipboard_images: &mut ClipboardImageManager,
    path_normalization: &mut PathNormalizationManager,
) -> Result<bool, CliError> {
    input.set_secret_input(*secret_input);
    input.set_teleport_available(teleport_available(runtime.as_ref()));
    if callback::handle_key(
        key,
        runtime,
        active,
        state,
        controls,
        terminal_guard,
        terminal,
    )? {
        return Ok(false);
    }
    match handle_overlay_key(key, runtime, state, controls, input, theme).await {
        OverlayKeyResult::Unhandled => {}
        OverlayKeyResult::Handled => {
            let effects = input.refresh_after_adapter_mutation();
            apply_composer_effects(input, effects, working_directory, state);
            return Ok(false);
        }
        OverlayKeyResult::Effect(effect) => {
            let Some(runtime) = runtime.as_mut() else {
                return Ok(false);
            };
            match effect {
                OverlayEffect::Mcp(effect) => {
                    execute_mcp_effect(effect, runtime, state, &SystemUrlOpener);
                }
                OverlayEffect::RemoteProject(action) => {
                    handle_project_action(action, working_directory, runtime, state);
                }
                OverlayEffect::TeleportPush(action) => {
                    remote_project_workflow::handle_teleport_push_response(action, runtime, state);
                }
            }
            let effects = input.refresh_after_adapter_mutation();
            apply_composer_effects(input, effects, working_directory, state);
            return Ok(false);
        }
    }
    let voice_key = input.voice_phase().is_active()
        || (input.voice_phase() == self::chat_input::VoicePhase::Idle
            && key.code == KeyCode::Char('r')
            && key.modifiers.contains(KeyModifiers::CONTROL));
    if voice_key {
        if let Some(event) = normalized_input_event(key) {
            let effects = apply_composer_event(input, event, working_directory, state);
            if let Some(runtime) = runtime.as_mut() {
                runtime
                    .voice
                    .apply_effects(&effects, input.voice_generation());
            }
        }
        return Ok(false);
    }
    if input.feedback_active() && key.code == KeyCode::Esc {
        if let Some(event) = normalized_input_event(key) {
            let effects = apply_composer_event(input, event, working_directory, state);
            feedback::handle_effects(&effects, runtime, input, state).await;
        }
        return Ok(false);
    }
    if key.code != KeyCode::Esc {
        state.rewind_confirmation.cancel();
    }
    if key.code == KeyCode::Char('v')
        && key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
    {
        if let Some(event) = normalized_input_event(key) {
            let effects = apply_composer_event(input, event, working_directory, state);
            clipboard_images.schedule_effects(&effects);
        }
        return Ok(false);
    }
    if key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
    {
        if let Some(event) = normalized_input_event(key) {
            apply_composer_event(input, event, working_directory, state);
        }
        cycle_agent(runtime, state, input);
        return Ok(false);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                copy_prompt_selection(input.editor(), state);
                return Ok(false);
            }
            KeyCode::Char('c') => {
                if request_active_turn_interrupt(runtime, active, controls, state) {
                    return Ok(false);
                }
                if let Some(runtime) = runtime.as_mut()
                    && runtime.shell.is_some()
                {
                    interrupt_shell(runtime, state).await;
                    state.prompt_queue.pause();
                    return Ok(false);
                }
                if !input.editor().text().is_empty() {
                    if let Some(event) = normalized_input_event(key) {
                        apply_composer_event(input, event, working_directory, state);
                    }
                    let protected = state.prompt_queue.transient_images();
                    clipboard_images
                        .discard_unreferenced(&protected, state)
                        .await;
                    state.quit_confirmation.cancel();
                    return Ok(false);
                }
                if let Some(cancelled) = state.prompt_queue.cancel_last() {
                    state.push_diagnostic(format!(
                        "Removed queued prompt: {}",
                        cancelled.text().lines().next().unwrap_or_default()
                    ));
                    let protected = state.prompt_queue.transient_images();
                    clipboard_images
                        .discard_unreferenced(&protected, state)
                        .await;
                    return Ok(false);
                }
                if state.quit_confirmation.request("Ctrl+C", unix_millis()) {
                    return Ok(true);
                }
                state.push_diagnostic("Press Ctrl+C again within one second to quit");
                return Ok(false);
            }
            KeyCode::Char('d') => {
                if !input.editor().text().is_empty() {
                    if let Some(event) = normalized_input_event(key) {
                        apply_composer_event(input, event, working_directory, state);
                    }
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
                copy_prompt_selection(input.editor(), state);
                return Ok(false);
            }
            KeyCode::Char('g') => {
                if *secret_input {
                    state.push_diagnostic("External editing is disabled while entering a secret");
                    return Ok(false);
                }
                let Some(event) = normalized_input_event(key) else {
                    return Ok(false);
                };
                let effects = apply_composer_event(input, event, working_directory, state);
                let Some(text) = effects.into_iter().find_map(|effect| match effect {
                    InputEffect::OpenExternalEditor { text } => Some(text),
                    _ => None,
                }) else {
                    return Ok(false);
                };
                terminal_guard
                    .restore()
                    .map_err(|error| CliError::Terminal(error.to_string()))?;
                let mut external = SystemExternalEditor::from_environment();
                let edited = ExternalEditorPort::edit(&mut external, &text);
                terminal_guard
                    .resume()
                    .map_err(|error| CliError::Terminal(error.to_string()))?;
                terminal
                    .clear()
                    .map_err(|error| CliError::Terminal(error.to_string()))?;
                match edited {
                    Ok(edited) => {
                        apply_composer_event(
                            input,
                            InputEvent::ExternalEditor { text: Some(edited) },
                            working_directory,
                            state,
                        );
                    }
                    Err(error) => state.push_diagnostic(error),
                }
                return Ok(false);
            }
            KeyCode::Char('a' | 'e' | 'j') => {
                if let Some(event) = normalized_input_event(key) {
                    apply_composer_event(input, event, working_directory, state);
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    match key.code {
        KeyCode::Esc => {
            if !request_active_turn_interrupt(runtime, active, controls, state)
                && let Some(runtime) = runtime.as_mut()
                && runtime.shell.is_some()
            {
                interrupt_shell(runtime, state).await;
                state.prompt_queue.pause();
            } else if active.is_none() && !state.prompt_queue.is_empty() {
                state.prompt_queue.pause();
                state.push_diagnostic(
                    "Queued prompts paused; press Enter on an empty prompt to resume",
                );
            } else if active.is_none() && !input.editor().text().is_empty() {
                if let Some(event) = normalized_input_event(key) {
                    apply_composer_event(input, event, working_directory, state);
                }
                let protected = state.prompt_queue.transient_images();
                clipboard_images
                    .discard_unreferenced(&protected, state)
                    .await;
                state.rewind_confirmation.cancel();
            } else if active.is_none()
                && state.rewind_confirmation.request("Esc", unix_millis())
                && let Some(runtime) = runtime.as_mut()
            {
                show_rewind(runtime, state);
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if let Some(event) = normalized_input_event(key) {
                apply_composer_event(input, event, working_directory, state);
            }
        }
        KeyCode::Enter => {
            if resume_paused_queue(input.editor(), state) {
                return Ok(false);
            }
            let record_history = setup_flow.is_none() && !*secret_input;
            let submitted = if setup_flow.is_some() || *secret_input {
                let submitted = input.take_unrecorded();
                let effects = input.refresh_after_adapter_mutation();
                apply_composer_effects(input, effects, working_directory, state);
                submitted
            } else {
                let Some(event) = normalized_input_event(key) else {
                    return Ok(false);
                };
                let effects = apply_composer_event(input, event, working_directory, state);
                for entry in effects.iter().filter_map(|effect| match effect {
                    InputEffect::RecordHistory { entry } => Some(entry),
                    _ => None,
                }) {
                    prompt_history.persist(entry.clone());
                }
                effects.into_iter().find_map(|effect| match effect {
                    InputEffect::Submit { text } if !text.is_empty() => Some(text),
                    _ => None,
                })
            };
            let Some(submitted) = submitted else {
                return Ok(false);
            };
            debug_assert!(!record_history || input.editor().text().is_empty());
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
                        input.set_secret_input(next_secret_input);
                        push_local_notice(state, &prompt, EntryStatus::Completed);
                    }
                    Ok(SetupProgress::Complete(setup_completion)) => {
                        *secret_input = false;
                        input.set_secret_input(false);
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
                            previous.voice.shutdown().await;
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
                        let release3 = release3_service(&configured, working_directory)?;
                        *runtime = Some(start_runtime(
                            &configured,
                            working_directory,
                            release3,
                            credential,
                        )?);
                        if let Some(runtime) = runtime.as_ref() {
                            input.set_voice_enabled(runtime.voice.enabled());
                            input.set_teleport_available(teleport_available(Some(runtime)));
                            input.set_user_skills(
                                runtime
                                    .skills
                                    .values()
                                    .map(|skill| (skill.name.as_str(), skill.description.as_str())),
                            );
                        }
                        let hydrated = runtime
                            .as_mut()
                            .map(|runtime| {
                                hydrate_initial_state(runtime, &configured, working_directory)
                            })
                            .transpose()?
                            .unwrap_or_else(|| TuiState::new(""));
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
            let runtime_busy = active.is_some()
                || runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.shell.is_some())
                || state.prompt_queue.is_paused();
            if !runtime_busy && is_exit_command(&submitted) {
                return Ok(true);
            }
            let command_action = dispatch_command(
                &submitted,
                arguments,
                working_directory,
                runtime,
                state,
                controls,
                input,
                theme,
                runtime_busy,
            )
            .await?;
            let effects = input.refresh_after_adapter_mutation();
            apply_composer_effects(input, effects, working_directory, state);
            match command_action {
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
                    input.set_secret_input(false);
                }
                CommandAction::ClipboardImageRequested => {
                    clipboard_images.schedule(true);
                }
                CommandAction::RejectedBusy => {
                    input.replace_text(&submitted);
                    let effects = input.refresh_after_adapter_mutation();
                    apply_composer_effects(input, effects, working_directory, state);
                }
                CommandAction::Handled => {}
                CommandAction::Runtime(command) => {
                    if handle_runtime_command(
                        &command,
                        working_directory,
                        runtime,
                        state,
                        controls,
                        input,
                        theme,
                        active.is_some(),
                    )
                    .await
                    {
                        return Ok(false);
                    }
                    state.push_diagnostic("The command is not available in this runtime");
                }
                CommandAction::Unhandled
                    if submitted.starts_with('/')
                        && is_user_skill(runtime.as_ref(), &submitted) =>
                {
                    if runtime_busy {
                        let draft = clipboard_images.draft(working_directory, submitted);
                        if enqueue_prompt(working_directory, &draft, runtime, state).await? {
                            state.push_diagnostic(format!(
                                "Skill queued ({} pending)",
                                state.prompt_queue.len()
                            ));
                        } else {
                            input.replace_text(draft.into_text());
                            let effects = input.refresh_after_adapter_mutation();
                            apply_composer_effects(input, effects, working_directory, state);
                        }
                    } else {
                        let draft = clipboard_images.draft(working_directory, submitted);
                        if !start_prompt(
                            PromptContext::new(
                                working_directory,
                                runtime,
                                active,
                                state,
                                controls,
                                clipboard_images,
                            ),
                            &draft,
                        )
                        .await?
                        {
                            input.replace_text(draft.into_text());
                            let effects = input.refresh_after_adapter_mutation();
                            apply_composer_effects(input, effects, working_directory, state);
                        }
                    }
                }
                CommandAction::Unhandled if submitted.trim() == "!" => {
                    state.push_diagnostic("No command provided after '!'");
                    if runtime_busy {
                        state.prompt_queue.resume();
                    }
                }
                CommandAction::Unhandled
                    if runtime_busy
                        && submitted.starts_with('&')
                        && teleport_available(runtime.as_ref()) =>
                {
                    input.replace_text(&submitted);
                    let effects = input.refresh_after_adapter_mutation();
                    apply_composer_effects(input, effects, working_directory, state);
                    state.push_diagnostic("Teleport cannot be queued while the runtime is busy");
                }
                CommandAction::Unhandled
                    if submitted.starts_with('&') && teleport_available(runtime.as_ref()) =>
                {
                    if let Some(runtime) = runtime.as_mut() {
                        handle_teleport_command(
                            submitted.trim_start_matches('&').trim(),
                            working_directory,
                            runtime,
                            state,
                        );
                    }
                }
                CommandAction::Unhandled
                    if runtime_busy && submitted.trim_start().starts_with('!') =>
                {
                    state
                        .prompt_queue
                        .push(clipboard_images.draft(working_directory, submitted));
                    state.prompt_queue.resume();
                    state.push_diagnostic(format!(
                        "Input queued ({} pending)",
                        state.prompt_queue.len()
                    ));
                }
                CommandAction::Unhandled if runtime_busy => {
                    let draft = clipboard_images.draft(working_directory, submitted);
                    if enqueue_prompt(working_directory, &draft, runtime, state).await? {
                        state.push_diagnostic(format!(
                            "Input queued ({} pending)",
                            state.prompt_queue.len()
                        ));
                    } else {
                        input.replace_text(draft.into_text());
                        let effects = input.refresh_after_adapter_mutation();
                        apply_composer_effects(input, effects, working_directory, state);
                    }
                }
                CommandAction::Unhandled if submitted.trim_start().starts_with('!') => {
                    if !start_shell(&submitted, runtime, state).await? {
                        input.replace_text(submitted);
                        let effects = input.refresh_after_adapter_mutation();
                        apply_composer_effects(input, effects, working_directory, state);
                    }
                }
                CommandAction::Unhandled => {
                    let draft = clipboard_images.draft(working_directory, submitted);
                    if !start_prompt(
                        PromptContext::new(
                            working_directory,
                            runtime,
                            active,
                            state,
                            controls,
                            clipboard_images,
                        ),
                        &draft,
                    )
                    .await?
                    {
                        input.replace_text(draft.into_text());
                        let effects = input.refresh_after_adapter_mutation();
                        apply_composer_effects(input, effects, working_directory, state);
                    }
                }
            }
        }
        KeyCode::PageUp if key.modifiers.contains(KeyModifiers::ALT) => {
            state.prompt_queue.scroll(-5);
        }
        KeyCode::PageDown if key.modifiers.contains(KeyModifiers::ALT) => {
            state.prompt_queue.scroll(5);
        }
        KeyCode::PageUp => {
            state.scroll_up(10);
            page_older_history(runtime.as_mut(), state);
        }
        KeyCode::PageDown => {
            state.scroll_down(10);
        }
        KeyCode::Tab if !*secret_input => {
            if let Some(event) = normalized_input_event(key) {
                apply_composer_event(input, event, working_directory, state);
            }
        }
        KeyCode::Tab => {
            state.push_diagnostic("Completion is disabled while entering a secret");
        }
        KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Char(_) => {
            if let Some(event) = normalized_input_event(key) {
                let effects = apply_composer_event(input, event, working_directory, state);
                feedback::handle_effects(&effects, runtime, input, state).await;
                path_normalization
                    .schedule_effects(&effects)
                    .map_err(CliError::Terminal)?;
            }
        }
        _ => {}
    }
    Ok(false)
}

fn apply_path_normalization_event(
    path_normalization: &mut PathNormalizationManager,
    input: &mut ChatInputState,
    event: InputEvent,
    working_directory: &Path,
    state: &mut TuiState,
) -> Result<(), CliError> {
    let effects = apply_composer_event(input, event, working_directory, state);
    path_normalization
        .schedule_effects(&effects)
        .map_err(CliError::Terminal)
}

#[allow(clippy::too_many_arguments)]
async fn handle_runtime_command(
    command: &RuntimeCommand,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    input: &mut ChatInputState,
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
                    apply_thinking(runtime, value, None, state);
                }
                ["model", value] if !value.trim().is_empty() => {
                    switching::request(
                        runtime,
                        input,
                        state,
                        switching::SwitchRequest::Model {
                            model: (*value).to_owned(),
                            target: None,
                        },
                    );
                }
                ["agent", value] if !value.trim().is_empty() => {
                    switching::request(
                        runtime,
                        input,
                        state,
                        switching::SwitchRequest::Agent((*value).to_owned()),
                    );
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
                ["theme", value] => update_theme(runtime, value, None, state, theme),
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
            update_theme(runtime, value, None, state, theme);
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
            handle_project_command(command_arguments, working_directory, runtime, state)
        }
        CommandId::Teleport => {
            handle_teleport_command(command_arguments, working_directory, runtime, state)
        }
        _ => return false,
    }
    true
}

fn call_runtime(
    runtime: &mut InteractiveRuntime,
    method: &str,
    mut params: Value,
    state: &mut TuiState,
) -> Option<BTreeMap<String, Value>> {
    if let Some(params) = params.as_object_mut() {
        params
            .entry("sessionId")
            .or_insert_with(|| json!(runtime.session_id));
    }
    match runtime.service.public_call(method, params) {
        Ok(result) => Some(result),
        Err(error) => {
            state.push_diagnostic(error.to_string());
            None
        }
    }
}

fn configured_theme(runtime: Option<&mut InteractiveRuntime>) -> Option<Theme> {
    let runtime = runtime?;
    let result = runtime
        .service
        .public_call("config/read", json!({"sessionId": runtime.session_id}))
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
    target: Option<&str>,
    state: &mut TuiState,
    theme: &mut ResolvedTheme,
) {
    let Some(preference) = parse_theme(value) else {
        state.push_diagnostic("Usage: /theme <system|light|dark>");
        return;
    };
    if persist_setting(
        runtime,
        target.unwrap_or("user"),
        &["theme"],
        json!(value),
        false,
        state,
    ) {
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
    persist_setting(runtime, "user", path, value, remove, state)
}

pub(in crate::tui) fn persist_setting(
    runtime: &mut InteractiveRuntime,
    target: &str,
    path: &[&str],
    value: Value,
    remove: bool,
    state: &mut TuiState,
) -> bool {
    let expected_fingerprint = match runtime
        .service
        .public_call("config/read", json!({"sessionId": runtime.session_id}))
    {
        Ok(result) => result
            .get("snapshot")
            .and_then(|snapshot| snapshot.pointer(&format!("/fingerprints/{target}")))
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
                "target": target,
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
    mut params: Value,
    state: &mut TuiState,
) -> Option<PublicDispatch> {
    if let Some(params) = params.as_object_mut() {
        params
            .entry("sessionId")
            .or_insert_with(|| json!(runtime.session_id));
    }
    match runtime.service.public_call_async(method, params).await {
        Ok(dispatch) => {
            apply_public_notifications(&dispatch, state);
            Some(dispatch)
        }
        Err(error) => {
            state.push_diagnostic(error.to_string());
            None
        }
    }
}

fn apply_public_notifications(dispatch: &PublicDispatch, state: &mut TuiState) {
    for notification in &dispatch.notifications {
        if notification.method == "vibeCode/teleport/event" {
            if let Some(event) = notification.params.get("event")
                && event.get("kind").and_then(Value::as_str) == Some("push_required")
            {
                match pickers::teleport_push_overlay(event) {
                    Some(overlay) => state.overlay = Some(overlay),
                    None => state.push_diagnostic("Teleport push request omitted its operation ID"),
                }
            }
            match teleport_event_message(notification.params.get("event")) {
                Ok((message, status)) => push_local_notice(state, &message, status),
                Err(message) => state.push_diagnostic(message),
            }
        } else {
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
    }
}

fn teleport_event_message(event: Option<&Value>) -> Result<(String, EntryStatus), &'static str> {
    let event = event.ok_or("Teleport event omitted its payload")?;
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("Teleport event omitted its kind")?;
    let (message, status) = match kind {
        "summarizing_context" => ("Summarizing context...".to_owned(), EntryStatus::Streaming),
        "checking_git" => ("Preparing workspace...".to_owned(), EntryStatus::Streaming),
        "push_required" => {
            let count = event
                .get("unpushedCount")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let branch = event
                .get("branchNotPushed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = if branch {
                "Teleport requires publishing the current branch.".to_owned()
            } else {
                format!(
                    "Teleport requires pushing {count} commit{}.",
                    if count == 1 { "" } else { "s" }
                )
            };
            (message, EntryStatus::Streaming)
        }
        "pushing" => ("Syncing with remote...".to_owned(), EntryStatus::Streaming),
        "starting_workflow" => ("Teleporting...".to_owned(), EntryStatus::Streaming),
        "complete" => {
            let url = event
                .get("url")
                .and_then(Value::as_str)
                .ok_or("Completed Teleport event omitted its URL")?;
            (
                format!("Teleported to Vibe Code Web: {url}"),
                EntryStatus::Completed,
            )
        }
        "failed" => {
            let message = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Teleport failed");
            return Ok((format!("Teleport failed: {message}"), EntryStatus::Failed));
        }
        "cancelled" => ("Teleport cancelled.".to_owned(), EntryStatus::Cancelled),
        _ => return Err("Teleport event kind is unknown"),
    };
    Ok((message, status))
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
    runtime.safety = active_agent_safety(
        &mut runtime.service,
        &runtime.session_id,
        &runtime.agent_name,
    );
}

fn active_agent_safety(
    service: &mut HeadlessService<LiveTurnDriver>,
    session_id: &str,
    agent_name: &str,
) -> Safety {
    service
        .public_call("agents/list", json!({"sessionId": session_id}))
        .ok()
        .and_then(|result| result.get("agents").and_then(Value::as_array).cloned())
        .into_iter()
        .flatten()
        .find(|agent| agent.get("name").and_then(Value::as_str) == Some(agent_name))
        .and_then(|agent| {
            agent
                .get("safety")
                .and_then(Value::as_str)
                .map(parse_safety)
        })
        .unwrap_or(Safety::Neutral)
}

fn parse_safety(value: &str) -> Safety {
    match value {
        "safe" | "read_only" => Safety::Safe,
        "destructive" | "approval_required" => Safety::Destructive,
        "yolo" | "unsafe" => Safety::Yolo,
        _ => Safety::Neutral,
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
                match result
                    .get("loops")
                    .ok_or("Scheduled-loop response omitted its list")
                    .and_then(|loops| format_loop_list(loops, unix_seconds()))
                {
                    Ok(message) => push_local_notice(state, &message, EntryStatus::Completed),
                    Err(message) => state.push_diagnostic(message),
                }
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
            if let Some(result) = call_runtime(runtime, method, params, state) {
                let message = if *target == "all" {
                    result
                        .get("count")
                        .and_then(Value::as_u64)
                        .map(|count| format!("Cancelled {count} scheduled loop(s)."))
                        .ok_or("Scheduled-loop clear response omitted its count")
                } else {
                    result
                        .get("loop")
                        .ok_or("Scheduled-loop delete response omitted the loop")
                        .and_then(format_cancelled_loop)
                };
                match message {
                    Ok(message) => push_local_notice(state, &message, EntryStatus::Completed),
                    Err(message) => state.push_diagnostic(message),
                }
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
                match result
                    .get("loop")
                    .ok_or("Scheduled-loop create response omitted the loop")
                    .and_then(format_created_loop)
                {
                    Ok(message) => push_local_notice(state, &message, EntryStatus::Completed),
                    Err(message) => state.push_diagnostic(message),
                }
            }
        }
        _ => state.push_diagnostic(
            "Usage: /loop [list|ls] | /loop <interval> <prompt> | /loop delete <id|all>",
        ),
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
    command.trim() == "/exit"
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
    release3: Release3Service,
    credential: String,
) -> Result<InteractiveRuntime, CliError> {
    let voice_credential = credential.clone();
    let connector_credential = credential.clone();
    let banner = banner_metrics_from_release3(&release3, arguments, working_directory);
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
    let connector = Arc::new(
        MistralConnectorClient::new(&arguments.api_base, connector_credential)
            .map_err(|error| CliError::Terminal(error.to_string()))?,
    );
    let (mcp_factory, mcp_auth) =
        production_mcp_adapters().map_err(|error| CliError::Terminal(error.to_string()))?;
    let resource_backend = CoreResourceBackend::default()
        .with_config(release3.layered_config())
        .with_mcp_factory(mcp_factory)
        .with_mcp_auth(mcp_auth)
        .with_connector_catalog(
            connector.clone(),
            connector.clone(),
            arguments.credential_environment.clone(),
            connector.base_url(),
        )
        .with_connector_auth(connector);
    let server = AppServer::with_resource_backend(Arc::new(resource_backend))
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
    let voice_enabled = service
        .public_call("config/read", json!({"sessionId": session_id}))
        .ok()
        .and_then(|result| {
            result
                .get("snapshot")
                .and_then(|snapshot| snapshot.pointer("/config/voice_mode_enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    let voice = VoiceManager::production(voice_credential, &arguments.api_base, voice_enabled)
        .map_err(CliError::Terminal)?;
    let session = service.session(&session_id)?;
    let agent_name = session
        .intent
        .agent
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    let safety = active_agent_safety(&mut service, &session_id, &agent_name);
    Ok(InteractiveRuntime {
        service,
        session_id,
        model: session.intent.model.unwrap_or(preferences.model),
        image_models: preferences.image_models,
        thinking: session
            .intent
            .reasoning_effort
            .unwrap_or_else(|| "off".to_owned()),
        mode: session
            .intent
            .mode
            .unwrap_or_else(|| preferences.mode.clone()),
        agent_name,
        safety,
        banner,
        context_tokens: 0,
        context_window: arguments.max_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW),
        auto_approve: session.intent.auto_approve,
        vibe_code_enabled: preferences.vibe_code_enabled,
        clear_context_after_turn: false,
        config_target: None,
        remote_project_overlay: None,
        remote_project_draft: None,
        ui_operation_sender: None,
        ui_operation_generation: 0,
        active_ui_operation: None,
        skills,
        shell: None,
        cloud: CloudWorkflowState::default(),
        pending_switch: None,
        telemetry,
        voice,
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

async fn refresh_server_banner_metrics(
    service: &mut HeadlessService<LiveTurnDriver>,
    session_id: &str,
    banner: &mut BannerMetrics,
) {
    if let Ok(dispatch) = service
        .public_call_async("connectors/read", json!({"sessionId": session_id}))
        .await
        && let Some(counts) = dispatch.result.get("counts")
    {
        banner.connectors_connected = json_usize(counts.get("connected"));
        banner.connectors_total = json_usize(counts.get("total"));
    }
    if let Ok(dispatch) = service
        .public_call_async("mcp/read", json!({"sessionId": session_id}))
        .await
        && let Some(sources) = dispatch
            .result
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
    if let Ok(result) = service.public_call("diagnostics/list", json!({"sessionId": session_id})) {
        banner.hooks_count = json_usize(result.get("hooksCount"));
    }
    if let Ok(result) = service.public_call("account/read", json!({"sessionId": session_id})) {
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
    image_models: ImageModels,
    mode: String,
    thinking: bool,
    reasoning_effort: Option<String>,
    vibe_code_enabled: bool,
}

fn release3_service(
    arguments: &Arguments,
    working_directory: &Path,
) -> Result<Release3Service, CliError> {
    startup::startup_host(arguments, working_directory)
        .into_release3(arguments.trust)
        .map_err(startup::StartupError::from)
        .map_err(CliError::from)
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
    let mut image_models = ImageModels::default();
    image_models.insert(DEFAULT_MODEL, true);
    if let Some(models) = config
        .and_then(|config| config.get("models"))
        .and_then(Value::as_array)
    {
        for configured in models {
            let supports_images = configured
                .get("supports_images")
                .or_else(|| configured.get("supportsImages"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            for key in ["name", "alias"] {
                if let Some(value) = configured.get(key).and_then(Value::as_str) {
                    image_models.insert(value, supports_images);
                }
            }
        }
    }
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
        image_models,
        mode,
        thinking: reasoning_effort.is_some(),
        reasoning_effort,
        vibe_code_enabled: config
            .and_then(|config| {
                config
                    .get("vibe_code_enabled")
                    .or_else(|| config.get("vibeCodeEnabled"))
            })
            .and_then(Value::as_bool)
            .unwrap_or(true),
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

fn drain_update_receiver(
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

async fn finish_active(
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
                state.push_diagnostic(error.to_string());
                false
            }
        }
    };
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

fn append_local_notice(state: &mut TuiState, message: &str, status: EntryStatus) -> String {
    let entry = TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: message.to_owned(),
        status,
        details: json!({"source": "tui"}),
    };
    state.append_local(entry)
}

fn push_local_notice(state: &mut TuiState, message: &str, status: EntryStatus) {
    append_local_notice(state, message, status);
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    use super::*;
    use vibe_app_server::client::TurnRequest;
    use vibe_app_server::release4::{
        CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectGitSnapshot, ProjectPage,
        ProjectRepository, TeleportCloud, TeleportStartRequest,
    };

    struct PanicAfterCompletionInterrupt {
        ready: bool,
        completed: bool,
    }

    impl Future for PanicAfterCompletionInterrupt {
        type Output = Result<(), std::io::Error>;

        fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            assert!(!self.completed, "completed interrupt was polled again");
            if self.ready {
                self.completed = true;
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }

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
    fn stale_inputs_are_drained_before_fatal_acknowledgement_arms() {
        let queued = Arc::new(Mutex::new(VecDeque::from([Ok::<_, std::io::Error>(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        )])));
        let polled = Arc::clone(&queued);
        let mut events = futures_util::stream::poll_fn(move |_| {
            polled
                .lock()
                .expect("event queue lock")
                .pop_front()
                .map_or(Poll::Pending, |event| Poll::Ready(Some(event)))
        });
        let mut startup = startup::MountedStartup::FatalPendingRender(CliError::Terminal(
            "initialization failed".to_owned(),
        ));

        assert_eq!(
            drain_ready_terminal_events(&mut events).expect("stale input drains"),
            ReadyInputDrain::Empty
        );
        let mut stale_interrupt = Box::pin(PanicAfterCompletionInterrupt {
            ready: true,
            completed: false,
        });
        let mut replacements = VecDeque::from([true, false]);
        assert_eq!(
            drain_ready_interrupts(&mut stale_interrupt, || {
                PanicAfterCompletionInterrupt {
                    ready: replacements.pop_front().expect("bounded replacement"),
                    completed: false,
                }
            })
            .expect("stale interrupt drains"),
            2
        );
        assert!(stale_interrupt.as_mut().now_or_never().is_none());
        assert!(!startup.is_awaiting_fatal_key());
        startup.arm_fatal_acknowledgement();
        assert!(startup.is_awaiting_fatal_key());
        assert!(events.next().now_or_never().is_none());

        queued
            .lock()
            .expect("event queue lock")
            .push_back(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            ))));
        assert!(matches!(
            events.next().now_or_never(),
            Some(Some(Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('n'),
                ..
            }))))
        ));
    }

    fn interactive_test_runtime(session_id: &str) -> InteractiveRuntime {
        interactive_test_runtime_with_server(session_id, AppServer::default())
    }

    fn interactive_test_runtime_with_server(
        session_id: &str,
        server: AppServer,
    ) -> InteractiveRuntime {
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
        let mut service = HeadlessService::new_interactive_shared_with_server(driver, server)
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
            image_models: {
                let mut models = ImageModels::default();
                models.insert("test-model", true);
                models
            },
            thinking: "off".to_owned(),
            mode: "code".to_owned(),
            agent_name: "default".to_owned(),
            safety: Safety::Neutral,
            banner: BannerMetrics::default(),
            context_tokens: 0,
            context_window: DEFAULT_CONTEXT_WINDOW,
            auto_approve: true,
            vibe_code_enabled: true,
            clear_context_after_turn: false,
            config_target: None,
            remote_project_overlay: None,
            remote_project_draft: None,
            ui_operation_sender: None,
            ui_operation_generation: 0,
            active_ui_operation: None,
            skills: BTreeMap::new(),
            shell: None,
            cloud: CloudWorkflowState::default(),
            pending_switch: None,
            telemetry: None,
            voice: VoiceManager::production(
                "test-credential".to_owned(),
                "https://provider.invalid",
                false,
            )
            .expect("test voice manager"),
        }
    }

    #[tokio::test]
    async fn ui_operations_are_mutually_exclusive_and_session_scoped() {
        let mut runtime = interactive_test_runtime("ui-operation-session");
        let (operation_sender, mut operation_receiver) = tokio::sync::mpsc::unbounded_channel();
        runtime.ui_operation_sender = Some(operation_sender);
        let mut state = TuiState::new("ui-operation-session");
        let rejected_work_ran = Arc::new(AtomicBool::new(false));

        assert!(schedule_ui_call(
            &mut runtime,
            "config/read",
            json!({}),
            UiOperation::Mcp(workflow::McpPendingOperation::CopyUrl),
            &mut state,
        ));
        let rejected_work = Arc::clone(&rejected_work_ran);
        assert!(!schedule_ui_external(
            &mut runtime,
            UiOperation::Mcp(workflow::McpPendingOperation::CopyUrl),
            async move {
                rejected_work.store(true, Ordering::SeqCst);
                Ok(())
            },
            &mut state,
        ));
        assert_eq!(runtime.ui_operation_generation, 1);
        assert!(
            state
                .diagnostics()
                .any(|message| message.contains("already in progress"))
        );

        let completion = tokio::time::timeout(Duration::from_secs(1), operation_receiver.recv())
            .await
            .expect("config read completes")
            .expect("operation channel remains open");
        assert!(completion.result.is_ok(), "sessionId was injected");
        let mut runtime = Some(runtime);
        apply_ui_operation_completion(completion, &mut runtime, &mut state);
        assert_eq!(
            runtime
                .as_ref()
                .expect("runtime remains mounted")
                .active_ui_operation,
            None
        );
        assert!(!rejected_work_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn remote_project_create_draft_survives_failure_and_clears_on_success_or_cancel() {
        let draft = interaction::RemoteProjectDraft {
            name: "vibe-rs".to_owned(),
            default_branch: "main".to_owned(),
        };
        let picker = Overlay::new(
            interaction::OverlayKind::RemoteProjects,
            "Projects",
            Vec::new(),
        );
        let mut runtime = interactive_test_runtime("remote-project-create");
        runtime
            .cloud
            .configure_project("picker".to_owned())
            .expect("picker starts");
        runtime.remote_project_overlay = Some(picker.clone());
        runtime.remote_project_draft = Some(draft.clone());
        let mut state = TuiState::new("remote-project-create");
        let mut create_overlay = pickers::remote_project_create_overlay(&draft);
        create_overlay.select_by_id("remote-project:create:submit");
        state.overlay = Some(create_overlay);
        let mut submitting_runtime = Some(runtime);

        assert!(matches!(
            workflow::handle_remote_project_create_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut submitting_runtime,
                &mut state,
            ),
            workflow::OverlayKeyResult::Effect(workflow::OverlayEffect::RemoteProject(
                interaction::RemoteProjectAction::Create { .. }
            ))
        ));
        assert_eq!(
            submitting_runtime
                .as_ref()
                .expect("runtime remains mounted")
                .remote_project_draft,
            Some(draft.clone())
        );

        let mut runtime = interactive_test_runtime("remote-project-create-failure");
        runtime.remote_project_overlay = Some(picker.clone());
        runtime.remote_project_draft = Some(draft.clone());
        state.overlay = Some(pickers::remote_project_create_overlay(&draft));
        remote_project_workflow::apply_pending_operation(
            remote_project_workflow::ProjectPendingOperation::Create {
                working_directory: PathBuf::from("/workspace"),
                requested_name: draft.name.clone(),
            },
            Err("creation failed".to_owned()),
            &mut runtime,
            &mut state,
        );
        assert_eq!(runtime.remote_project_draft, Some(draft.clone()));
        assert_eq!(
            state.overlay.as_ref().map(|overlay| overlay.kind),
            Some(interaction::OverlayKind::RemoteProjectCreate)
        );

        runtime
            .cloud
            .configure_project("picker".to_owned())
            .expect("picker restarts");
        remote_project_workflow::apply_pending_operation(
            remote_project_workflow::ProjectPendingOperation::Create {
                working_directory: PathBuf::from("/workspace"),
                requested_name: draft.name.clone(),
            },
            Ok(PublicDispatch {
                result: BTreeMap::from([(
                    "project".to_owned(),
                    json!({"projectId": "project-1", "name": "vibe-rs"}),
                )]),
                notifications: Vec::new(),
            }),
            &mut runtime,
            &mut state,
        );
        assert!(runtime.remote_project_draft.is_none());
        assert!(state.overlay.is_none());

        runtime.remote_project_overlay = Some(picker);
        runtime.remote_project_draft = Some(draft.clone());
        state.overlay = Some(pickers::remote_project_create_overlay(&draft));
        let mut runtime = Some(runtime);
        assert_eq!(
            workflow::handle_remote_project_create_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &mut runtime,
                &mut state,
            ),
            workflow::OverlayKeyResult::Handled
        );
        assert!(
            runtime
                .as_ref()
                .expect("runtime remains mounted")
                .remote_project_draft
                .is_none()
        );
        assert_eq!(
            state.overlay.as_ref().map(|overlay| overlay.kind),
            Some(interaction::OverlayKind::RemoteProjects)
        );
    }

    struct StartupProjects;

    impl ProjectCloud for StartupProjects {
        fn create(
            &self,
            name: &str,
            repo_url: &str,
            default_branch: &str,
        ) -> Result<Project, CloudError> {
            Ok(Project {
                project_id: "startup-project".to_owned(),
                name: name.to_owned(),
                repositories: vec![ProjectRepository {
                    repo_url: repo_url.to_owned(),
                    default_branch: Some(default_branch.to_owned()),
                }],
                is_read_only: false,
            })
        }

        fn list(&self, _cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
            Ok(ProjectPage {
                projects: vec![Project {
                    project_id: "startup-project".to_owned(),
                    name: "startup".to_owned(),
                    repositories: vec![ProjectRepository {
                        repo_url: "https://github.com/example/startup.git".to_owned(),
                        default_branch: Some("main".to_owned()),
                    }],
                    is_read_only: false,
                }],
                next_cursor: None,
            })
        }
    }

    struct StartupGit;

    impl GitProbe for StartupGit {
        fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
            Ok(GitSnapshot {
                repository: "https://github.com/example/startup.git".to_owned(),
                dirty: false,
                unpushed: false,
            })
        }

        fn inspect_project(
            &self,
            working_directory: &Path,
        ) -> Result<ProjectGitSnapshot, CloudError> {
            Ok(ProjectGitSnapshot {
                snapshot: self.inspect(working_directory)?,
                repo_root: working_directory.to_string_lossy().into_owned(),
                remote_name: "origin".to_owned(),
                branch: Some("main".to_owned()),
            })
        }

        fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
            Ok(())
        }
    }

    struct CapturingStartupTeleport {
        requests: Arc<Mutex<Vec<TeleportStartRequest>>>,
    }

    impl TeleportCloud for CapturingStartupTeleport {
        fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
            self.requests
                .lock()
                .map_err(|_| CloudError::Unavailable("fixture lock was poisoned".to_owned()))?
                .push(request.clone());
            Ok("https://cloud.example/teleport/startup".to_owned())
        }
    }

    #[tokio::test]
    async fn mounted_startup_teleport_executes_once_without_an_agent_turn() {
        for prompt in [None, Some("deployment context".to_owned())] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let release4 = Release4Service::with_backends(
                Arc::new(StartupProjects),
                Arc::new(CapturingStartupTeleport {
                    requests: requests.clone(),
                }),
                Arc::new(StartupGit),
            );
            let server = AppServer::with_release4_service(release4);
            let mut runtime = Some(interactive_test_runtime_with_server(
                "startup-teleport",
                server,
            ));
            let (operation_sender, mut operation_receiver) = tokio::sync::mpsc::unbounded_channel();
            runtime.as_mut().expect("runtime").ui_operation_sender = Some(operation_sender);
            let mut mounted = startup::MountedStartup::new(Some(
                startup::PostMountAction::Teleport(prompt.clone()),
            ));
            let mut active = None;
            let mut state = TuiState::new("startup-teleport");
            let mut controls = ControlState::new("startup-teleport");
            let mut input = ChatInputState::default();
            let mut clipboard_images = ClipboardImageManager::default();

            startup::complete_mounted_startup(
                &mut mounted,
                Path::new("/workspace"),
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
                &mut input,
                &mut clipboard_images,
            )
            .await
            .expect("mounted Teleport startup");
            for _ in 0..2 {
                let completion =
                    tokio::time::timeout(Duration::from_secs(1), operation_receiver.recv())
                        .await
                        .expect("startup operation completes")
                        .expect("startup operation channel");
                apply_ui_operation_completion(completion, &mut runtime, &mut state);
            }
            startup::complete_mounted_startup(
                &mut mounted,
                Path::new("/workspace"),
                &mut runtime,
                &mut active,
                &mut state,
                &mut controls,
                &mut input,
                &mut clipboard_images,
            )
            .await
            .expect("mounted startup is consumed once");

            assert!(
                active.is_none(),
                "Teleport startup must not start an agent turn"
            );
            let requests = requests.lock().expect("captured Teleport requests");
            assert_eq!(
                requests.len(),
                1,
                "workflow: {:?}; diagnostics: {:?}; entries: {:?}",
                runtime.as_ref().map(|runtime| &runtime.cloud),
                state.diagnostics().collect::<Vec<_>>(),
                state
                    .entries
                    .iter()
                    .map(|entry| entry.text.as_str())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                requests[0].summary,
                prompt.unwrap_or_else(|| "Continue this session in Vibe Code".to_owned())
            );
        }
    }

    #[tokio::test]
    async fn unavailable_startup_teleport_stays_visible_and_has_no_remote_effect() {
        let mut runtime = interactive_test_runtime("startup-teleport-unavailable");
        runtime.vibe_code_enabled = false;
        let mut runtime = Some(runtime);
        let mut mounted = startup::MountedStartup::new(Some(startup::PostMountAction::Teleport(
            Some("do not submit".to_owned()),
        )));
        let mut active = None;
        let mut state = TuiState::new("startup-teleport-unavailable");
        let mut controls = ControlState::new("startup-teleport-unavailable");
        let mut input = ChatInputState::default();
        let mut clipboard_images = ClipboardImageManager::default();

        startup::complete_mounted_startup(
            &mut mounted,
            Path::new("/workspace"),
            &mut runtime,
            &mut active,
            &mut state,
            &mut controls,
            &mut input,
            &mut clipboard_images,
        )
        .await
        .expect("unavailable startup Teleport is recoverable");

        assert!(active.is_none());
        assert!(
            state
                .diagnostics()
                .any(|diagnostic| { diagnostic.contains("Startup Teleport is unavailable") })
        );
    }

    #[tokio::test]
    async fn feedback_activation_and_response_use_the_existing_resource_boundary() {
        let mut runtime = interactive_test_runtime("feedback-session");
        let mut input = ChatInputState::default();
        let mut state = TuiState::new("feedback-session");

        feedback::maybe_activate(&mut runtime, &mut input, &mut state).await;
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
        feedback::handle_effects(&effects, &mut runtime, &mut input, &mut state).await;
        assert!(!input.feedback_active());
        assert_eq!(state.diagnostics().count(), 0);

        feedback::maybe_activate(
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

        feedback::handle_effects(
            &[InputEffect::FeedbackSnooze],
            &mut runtime,
            &mut input,
            &mut state,
        )
        .await;

        assert!(!input.feedback_active());
        assert_eq!(state.diagnostics().count(), 1);
    }

    #[test]
    fn switching_state_spans_the_scheduler_boundary() {
        let mut runtime = interactive_test_runtime("switch-session");
        let mut input = ChatInputState::default();
        let mut state = TuiState::new("switch-session");

        switching::request(
            &mut runtime,
            &mut input,
            &mut state,
            switching::SwitchRequest::Agent("default".to_owned()),
        );
        assert!(input.switching());
        assert_eq!(
            runtime.pending_switch,
            Some(switching::SwitchRequest::Agent("default".to_owned()))
        );

        switching::apply_pending(&mut runtime, &mut input, &mut state);
        assert!(!input.switching());
        assert!(runtime.pending_switch.is_none());
    }

    #[test]
    fn production_adapter_routes_keys_completion_and_mouse_through_chat_input_state() {
        let temporary = tempfile::tempdir().expect("workspace");
        let mut state = TuiState::new("session");
        let mut input = ChatInputState::new();

        for character in "select me".chars() {
            let event =
                normalized_input_event(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                    .expect("character is normalised");
            apply_composer_event(&mut input, event, temporary.path(), &mut state);
        }
        apply_composer_event(
            &mut input,
            InputEvent::Mouse {
                x: 2,
                y: 0,
                extend_selection: false,
            },
            temporary.path(),
            &mut state,
        );
        apply_composer_event(
            &mut input,
            InputEvent::Mouse {
                x: 7,
                y: 0,
                extend_selection: true,
            },
            temporary.path(),
            &mut state,
        );
        assert_eq!(input.observe().selection, Some([2, 7]));

        let mut external = ChatInputState::new();
        apply_composer_event(
            &mut external,
            InputEvent::ExternalEditor {
                text: Some("/c".to_owned()),
            },
            temporary.path(),
            &mut state,
        );
        assert_eq!(external.mode(), chat_input::InputMode::Prompt);
        assert!(external.completion().view().is_some());
    }

    #[test]
    fn terminal_adapter_accepts_press_and_repeat_but_not_release() {
        assert!(accepts_key_event(KeyEventKind::Press));
        assert!(accepts_key_event(KeyEventKind::Repeat));
        assert!(!accepts_key_event(KeyEventKind::Release));
    }

    #[test]
    fn only_plain_submission_waits_for_path_normalization() {
        assert!(should_defer_submission(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            true,
        ));
        assert!(!should_defer_submission(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            true,
        ));
        assert!(!should_defer_submission(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            false,
        ));
    }

    #[test]
    fn teleport_mode_uses_runtime_capability_instead_of_runtime_presence() {
        let mut runtime = interactive_test_runtime("teleport-capability-session");
        assert!(teleport_available(Some(&runtime)));
        runtime.vibe_code_enabled = false;
        assert!(!teleport_available(Some(&runtime)));
        assert!(!teleport_available(None));
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
    fn callback_absence_fails_an_unsettled_notice_closed() {
        let mut state = TuiState::new("session");
        state.append_local(TranscriptEntry {
            id: String::new(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: "approval pending".to_owned(),
            status: EntryStatus::Streaming,
            details: json!({"callbackId": "callback"}),
        });

        fail_inactive_callback_notices(&mut state, None);

        assert_eq!(state.entries[0].status, EntryStatus::Failed);
    }

    #[test]
    fn only_the_reference_slash_exit_alias_bypasses_dispatch() {
        assert!(is_exit_command("/exit"));
        assert!(!is_exit_command("/close"));
        assert!(!is_exit_command("/quit"));
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
    fn canonical_multi_question_callbacks_preserve_structure() {
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
        let CallbackRequest::UserInput { questions, .. } = &pending.request else {
            panic!("expected user-input callback");
        };
        assert_eq!(questions.len(), 2);
        assert!(questions[0].multi_select);
        assert!(
            pending
                .prompt
                .contains("All answers are submitted together.")
        );
    }

    #[test]
    fn plan_approval_fails_closed_until_the_live_file_is_readable_and_nonempty() {
        let mut state = TuiState::new("session");
        assert!(plan_approval_error(&state).is_some());
        state.plan_review = Some(PlanReviewState {
            path: PathBuf::from("plan.md"),
            content: String::new(),
            error: Some("missing".to_owned()),
        });
        assert!(plan_approval_error(&state).is_some());
        state.plan_review = Some(PlanReviewState {
            path: PathBuf::from("plan.md"),
            content: "# Ready".to_owned(),
            error: None,
        });
        assert_eq!(plan_approval_error(&state), None);
    }

    #[tokio::test]
    async fn plan_callback_stays_open_when_code_mode_cannot_be_committed() {
        let mut runtime = interactive_test_runtime("plan-settings-failure");
        runtime.mode = "plan".to_owned();
        let session_id = runtime.session_id.clone();
        runtime
            .service
            .close_session(&session_id)
            .await
            .expect("session closes");
        let mut state = TuiState::new(&session_id);
        state.plan_review = Some(PlanReviewState {
            path: PathBuf::from("plan.md"),
            content: "# Ready".to_owned(),
            error: None,
        });
        let mut controls = ControlState::new(&session_id);
        controls.begin_turn("turn").expect("turn begins");
        let pending = PendingCallback {
            callback_id: "plan-callback".to_owned(),
            session_id,
            turn_id: "turn".to_owned(),
            prompt: "Approve plan?".to_owned(),
            request: CallbackRequest::PlanReview {
                questions: vec![controls::CallbackQuestion {
                    header: "Plan".to_owned(),
                    question: "Approve?".to_owned(),
                    options: vec![controls::CallbackOption {
                        id: "manual".to_owned(),
                        label: "Yes, and request approval for edits".to_owned(),
                        description: String::new(),
                    }],
                    allows_free_text: true,
                    multi_select: false,
                }],
                footer_note: None,
                plan_path: PathBuf::from("plan.md"),
            },
        };
        controls
            .present_callback(pending.clone())
            .expect("plan callback presents");

        respond_to_pending_callback(
            &mut runtime,
            &mut controls,
            &pending,
            CallbackChoice::Option {
                id: "manual".to_owned(),
            },
            &mut state,
        );

        assert_eq!(runtime.mode, "plan");
        assert!(controls.contains_callback("plan-callback"));
        assert!(
            state
                .diagnostics()
                .any(|message| message.contains("Cannot approve the plan"))
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
                prompt: "Approve?".to_owned(),
                request: CallbackRequest::Approval {
                    options: Vec::new(),
                    effect: CallbackEffect {
                        tool_name: "test".to_owned(),
                        summary: String::new(),
                        content: String::new(),
                        permissions: Vec::new(),
                    },
                },
            })
            .expect("local callback presents");
        assert_eq!(controls.focus, controls::ControlFocus::Callback);

        recover_from_callback_response_error(
            &mut runtime,
            &mut controls,
            &mut state,
            "committed-callback",
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
                cancellation: CancellationPhase::Active,
                updates,
                task,
            });
            while !active
                .as_ref()
                .is_some_and(|active| active.task.is_finished())
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
        let mut active = Some(ActiveTurn {
            turn_id,
            scheduled_loop_id: None,
            cancellation,
            updates,
            task,
        });
        while !active
            .as_ref()
            .is_some_and(|active| active.task.is_finished())
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
