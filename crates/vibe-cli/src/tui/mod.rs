pub mod attachments;
pub mod attention;
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
pub mod debug_console;
pub mod diagnostics;
pub mod exit;
mod external_action;
mod feedback;
pub mod history;
mod hydration;
pub mod input;
pub mod interaction;
pub mod narrator;
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
mod runtime;
#[cfg(test)]
mod runtime_parity_tests;
mod session_picker;
pub mod setup;
mod shell;
mod shortcuts;
pub mod startup;
pub mod state;
mod submission;
mod switching;
pub mod terminal;
pub mod themes;
pub mod transcript;
pub mod transcript_view;
mod turn;
pub mod updates;
mod voice;
mod workflow;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, EventStream, KeyEventKind};
#[cfg(test)]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::{FutureExt, Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, LiveTurnDriver, PublicDispatch};
use vibe_app_server::release3::Release3Service;

use self::callback::{drain_callback_requests, sync_active_callbacks, sync_callback_presentation};
use self::chat_input::{ChatInputState, InputEvent, Safety};
use self::clipboard_images::{ClipboardImageManager, ImageModels};
use self::cloud_workflow::CloudWorkflowState;
use self::commands::CommandContext;
use self::composer::{
    apply_effects as apply_composer_effects, apply_event as apply_composer_event,
};
use self::controls::{CallbackRequest, ControlState};
use self::history::PromptHistory;
use self::hydration::{
    adopt_hydrated_session, history_entry, hydrate_initial_state, metadata_session_id,
    page_older_history, resync_current_projection,
};
use self::path_normalization::PathNormalizationManager;
use self::plan_review::PlanReviewMonitor;
use self::prompt::PromptContext;
use self::queue::start_next_queued_prompt;
use self::remote_project_workflow::start_teleport;
use self::render::{BannerContext, TokenState, UiContext, draw};
use self::runtime::{
    BannerMetrics, InteractiveRuntime, RuntimeSkill, UiOperation, apply_ui_operation_completion,
    teleport_available,
};
use self::setup::{
    CredentialStore, EnvironmentThemeDetector, NativeCredentialStore, NotificationPreference,
    ResolvedTheme, SetupCompletion, SetupFlow, TerminalThemeDetector, Theme, resolve_theme,
};
use self::shell::{finish_shell, interrupt_shell};
use self::state::{EntryStatus, ServerEvent, TranscriptEntry, TranscriptKind, TuiState};
use self::terminal::{CrosstermOps, TerminalGuard};
use self::turn::{
    ActiveTurn, CancellationPhase, drain_updates, finish_active, request_active_turn_interrupt,
    settle_unstarted_reservation, start_active_turn,
};
use self::voice::VoiceManager;
use crate::{
    Arguments, CliError, CliTelemetryObserver, bootstrap, telemetry_event_observer,
    validate_arguments,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const INITIAL_HISTORY_LIMIT: usize = 200;
const DEFAULT_MODEL: &str = "mistral-medium-3.5";
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

#[derive(Debug)]
pub struct InteractiveExit {
    pub session_started: bool,
    pub initialization_error: Option<CliError>,
    /// Reference `SessionExitSummary`, printed after the terminal is restored.
    pub summary: Option<exit::SessionExitSummary>,
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
            summary: None,
        });
    }
    if !startup::resolve_location_safety(trust.dangerous_warning.as_deref())? {
        return Ok(InteractiveExit {
            session_started: false,
            initialization_error: None,
            summary: None,
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
                summary: None,
            });
        }
    }
    let credential_store = NativeCredentialStore::new("mistral-vibe-rs");
    let (initial_credential, keyring_error) = if arguments.setup {
        (None, None)
    } else {
        // The process environment first, then `{vibe_home}/.env`, then the
        // keyring: a key the operator keeps in the dotenv file is as usable
        // here as an exported one.
        let environment_credential = vibe_core::config::DotenvValues::global(
            &startup::vibe_home_directory(&arguments, &working_directory),
        )
        .variable(&arguments.credential_environment)
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
    if !startup::resolve_startup_update_prompt(
        &arguments,
        &working_directory,
        &release3,
        env!("CARGO_PKG_VERSION"),
        &mut std::io::stdout().lock(),
    )? {
        return Ok(InteractiveExit {
            session_started: false,
            initialization_error: None,
            summary: None,
        });
    }
    let update_checks_enabled = startup::update_checks_enabled(&release3);
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
    if let Some(runtime) = runtime.as_mut() {
        // Reference `_on_config_changed`: rendering, notification, and narration
        // preferences apply before the first frame, not only after an edit.
        workflow::apply_render_preferences(runtime, &mut state);
    }
    announce_release_notes(&arguments, &working_directory, &mut state);
    // Reference `_schedule_update_notification`: refresh the cache for the next
    // startup without rendering anything or blocking input.
    let update_check = update_checks_enabled
        .then(startup::production_update_gateway)
        .flatten()
        .map(|gateway| {
            let store = startup::update_cache_store(&arguments, &working_directory);
            tokio::spawn(async move {
                startup::refresh_update_cache(&gateway, &store, env!("CARGO_PKG_VERSION")).await;
            })
        });
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

    // The two dispatch points below borrow the same locals, so the context is
    // rebuilt at each rather than held across the loop.
    macro_rules! key_context {
        () => {
            shortcuts::KeyContext {
                arguments: &arguments,
                working_directory: &working_directory,
                credential_store: &credential_store,
                runtime: &mut runtime,
                active: &mut active,
                state: &mut state,
                controls: &mut controls,
                prompt_history: &mut prompt_history,
                input: &mut input,
                setup_flow: &mut setup_flow,
                secret_input: &mut secret_input,
                theme: &mut theme,
                terminal_guard: &mut terminal_guard,
                terminal: &mut terminal,
                clipboard_images: &mut clipboard_images,
                path_normalization: &mut path_normalization,
                deferred_enter: &mut deferred_enter,
            }
        };
    }

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
            state.sync_activity(now_ms);
            if state
                .debug_console
                .as_ref()
                .is_some_and(|console| console.poll_due(now_ms))
                && let Some(runtime) = runtime.as_mut()
            {
                workflow::refresh_debug_console(runtime, &mut state, now_ms);
            }
            if !path_normalization.has_pending()
                && let Some(key) = deferred_enter.take()
            {
                exit =
                    shortcuts::handle_terminal_event(Event::Key(key), &mut key_context!()).await?;
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
                            exit = shortcuts::handle_terminal_event(event, &mut key_context!())
                                .await?;
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
                        // Reference `_begin_unsolicited_turn`: a turn nobody typed
                        // still restarts narration, with no user message.
                        if let Some(effect) = state.narrator.cancel() {
                            apply_narrator_effect(effect, runtime, &mut state);
                        }
                        state.narrator.on_turn_start("");
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
    if let Some(active) = active {
        active.join_before_exit(Duration::from_secs(2)).await;
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
    // A discovery still in flight never delays the exit the operator asked for.
    if let Some(update_check) = update_check {
        update_check.abort();
    }
    let telemetry = runtime
        .as_ref()
        .and_then(|runtime| runtime.telemetry.clone());
    // Reference `exit_summary`, read before the session closes.
    let summary = runtime.as_mut().map(session_exit_summary);
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
        summary,
    })
}

/// Reference `AppServerSession.exit_summary`: the session identity plus the
/// tokens spent since the session baseline.
fn session_exit_summary(runtime: &mut InteractiveRuntime) -> exit::SessionExitSummary {
    let stats = runtime
        .service
        .public_call("stats/read", json!({"sessionId": runtime.session_id}))
        .ok()
        .and_then(|result| result.get("stats").cloned())
        .unwrap_or(Value::Null);
    let tokens = |key: &str| {
        stats
            .get(key)
            .or_else(|| stats.get(pickers::to_camel_case(key).as_str()))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    exit::SessionExitSummary {
        session_id: Some(runtime.session_id.clone()),
        usage: exit::SessionUsage {
            input_tokens: tokens("session_prompt_tokens"),
            output_tokens: tokens("session_completion_tokens"),
        },
    }
}

/// Writes one attention effect. A terminal that refuses the escape keeps the
/// session usable and reports the scoped failure once.
fn emit_attention(state: &mut TuiState, effect: &attention::AttentionEffect) {
    if let Err(error) = attention::write_attention(&mut std::io::stdout().lock(), effect) {
        state.push_diagnostic(error);
    }
}

/// Reference `_terminal_notifier.notify`.
fn notify_attention(state: &mut TuiState, context: attention::NotificationContext, now_ms: u64) {
    if let Some(effect) = state.notifier.notify(context, now_ms) {
        emit_attention(state, &effect);
    }
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

/// Executes one narrator effect. Summaries come from the existing narration
/// resource; playback has no transport in this port, so a spoken summary settles
/// with one bounded, non-secret notice instead of a silent success.
pub(in crate::tui) fn apply_narrator_effect(
    effect: narrator::NarratorEffect,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    match effect {
        // Nothing is playing, so cancellation has no external effect to stop.
        narrator::NarratorEffect::Stop => {}
        narrator::NarratorEffect::Summarize {
            generation,
            user_message,
            assistant_text,
            ..
        } => {
            let summary = runtime
                .service
                .public_call(
                    "narration/summarize",
                    json!({
                        "sessionId": runtime.session_id,
                        "userMessage": user_message,
                        "assistantText": assistant_text,
                    }),
                )
                .ok()
                .and_then(|result| {
                    result
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                });
            if let Some(narrator::NarratorEffect::Speak { generation, .. }) =
                state.narrator.apply_summary(generation, summary)
            {
                state.narrator.settle(generation);
                report_speech_unavailable(state);
            }
        }
        narrator::NarratorEffect::Speak { generation, .. } => {
            state.narrator.settle(generation);
            report_speech_unavailable(state);
        }
    }
}

/// The narrator preference is honored, but this port has no speech transport.
/// The operator is told once per session, never once per turn.
fn report_speech_unavailable(state: &mut TuiState) {
    if state.speech_notice_shown {
        return;
    }
    state.speech_notice_shown = true;
    state.push_diagnostic(
        "Narrator playback is unavailable in this build; turn summaries are not spoken",
    );
}

/// Reference `_check_and_show_whats_new`: release notes appear once per version,
/// and the version is marked as seen even when no notes ship.
fn announce_release_notes(arguments: &Arguments, working_directory: &Path, state: &mut TuiState) {
    let version = env!("CARGO_PKG_VERSION");
    let store = startup::update_cache_store(arguments, working_directory);
    let cache = store.load();
    if !vibe_core::updates::should_show_whats_new(cache.as_ref(), version) {
        return;
    }
    if let Some(content) = updates::whats_new_content() {
        push_local_notice(state, content, EntryStatus::Completed);
    }
    let seen =
        vibe_core::updates::mark_version_as_seen(cache.as_ref(), version, startup::unix_seconds());
    if store.store(&seen).is_err() {
        state.push_diagnostic("Release notes could not be marked as seen");
    }
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
    themes::theme_polarity(value)
}

/// Reference `ThemePreviewed`: a highlighted theme applies before acceptance and
/// is never persisted.
pub(in crate::tui) fn preview_theme(value: &str, theme: &mut ResolvedTheme) {
    if let Some(preference) = parse_theme(value) {
        *theme = resolve_theme(
            preference,
            EnvironmentThemeDetector.detect(),
            !theme.colors_enabled,
        );
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
        state.push_diagnostic(format!(
            "Usage: /theme <{}>",
            themes::accepted_theme_values().join("|")
        ));
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

/// Releasing the pointer over the transcript either activates the link under a
/// plain click or settles a drag selection, auto-copying it when the reference
/// preference is on. Every external effect is scoped: a failure is reported and
/// the selection survives it.
async fn settle_transcript_pointer(state: &mut TuiState, column: u16, row: u16) {
    let Some(cell) = state.transcript_view.cell_at(column, row) else {
        return;
    };
    if state.transcript_view.is_click() {
        state.transcript_view.clear_selection();
        let Some(url) = state.transcript_view.link_at(cell) else {
            return;
        };
        if let Err(error) =
            workflow::UrlOpenerPort::open(&workflow::SystemUrlOpener, url.clone()).await
        {
            state.push_diagnostic(format!("Could not open {url}: {error}"));
        }
        return;
    }
    state.transcript_view.extend_selection(cell);
    if state.autocopy_to_clipboard {
        copy_transcript_selection(state);
    }
}

/// Reference `action_copy_selection`: copies the transcript selection when one
/// exists, and reports a clipboard refusal without discarding it.
fn copy_transcript_selection(state: &mut TuiState) -> bool {
    let Some(selection) = state.transcript_view.selected_text() else {
        return false;
    };
    match clipboard::SystemClipboardPort::copy_text(&clipboard::SystemClipboard, &selection) {
        Ok(()) => push_local_notice(
            state,
            "Selection copied to clipboard",
            EntryStatus::Completed,
        ),
        Err(_) => state.push_diagnostic("Failed to copy: clipboard not available"),
    }
    true
}

/// Reference `_try_load_previous`: reveals the page above the debug window and
/// leaves it pinned there until the console is reopened.
fn page_older_debug_logs(runtime: Option<&mut InteractiveRuntime>, state: &mut TuiState) {
    let Some(console) = state.debug_console.as_mut() else {
        return;
    };
    if !console.load_older() {
        return;
    }
    let Some(runtime) = runtime else {
        state.push_diagnostic("Debug logs are unavailable until setup completes");
        return;
    };
    workflow::refresh_debug_console(runtime, state, unix_millis());
}

fn start_runtime(
    arguments: &Arguments,
    working_directory: &Path,
    release3: Release3Service,
    credential: String,
) -> Result<InteractiveRuntime, CliError> {
    let voice_credential = credential.clone();
    let banner = banner_metrics_from_release3(&release3, arguments, working_directory);
    let skills = runtime_skills(&release3);
    let preferences = startup_preferences(arguments, &release3)?;
    let telemetry = telemetry_event_observer(arguments, &credential, "tui")?;
    let mut driver = LiveTurnDriver::from_credential(
        bootstrap::live_driver_config(arguments, &preferences.model)?,
        credential.clone(),
    )?;
    if let Some(observer) = telemetry.as_ref() {
        driver = driver.with_event_observer(observer.clone());
    }
    let server = bootstrap::resource_server(arguments, release3, credential.clone())?
        .using_release4_service(bootstrap::cloud_service(credential)?);
    let mut service =
        HeadlessService::new_interactive_shared_with_server(Arc::new(driver), server)?;
    let session_id = service.start_session(&bootstrap::session_options(
        arguments,
        working_directory,
        preferences.model.clone(),
        Some(preferences.mode.clone()),
        preferences.reasoning_effort.clone(),
    ))?;
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
    // Models are keyed by alias in the effective configuration; the entry still
    // carries its own name, which a provider request is sent under.
    if let Some(models) = config
        .and_then(|config| config.get("models"))
        .and_then(Value::as_object)
    {
        for (alias, configured) in models {
            let supports_images = configured
                .get("supports_images")
                .or_else(|| configured.get("supportsImages"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            image_models.insert(alias, supports_images);
            if let Some(name) = configured.get("name").and_then(Value::as_str) {
                image_models.insert(name, supports_images);
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

/// Reference `action_suspend_with_message`: restore the terminal, print the
/// resume hint, stop, and repaint on return. Unsupported platforms do nothing.
fn suspend_session(
    terminal_guard: &mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut TuiState,
) -> Result<(), CliError> {
    if !cfg!(unix) {
        return Ok(());
    }
    terminal_guard
        .restore()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    println!("{}", exit::SUSPEND_MESSAGE);
    let suspended = vibe_core::process::suspend_current_process();
    terminal_guard
        .resume()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    // Reference `_on_driver_signal_resume` forces a full repaint. Resizing to
    // the current size clears the viewport and both buffers without asking the
    // terminal for its cursor, which a resumed session may never answer.
    let area = terminal
        .size()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    terminal
        .resize(area.into())
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    if let Err(error) = suspended {
        state.push_diagnostic(format!("Suspend is unavailable: {error}"));
    }
    Ok(())
}

/// Reference `_try_interrupt_no_job_steps`: `Esc` and `Ctrl+C` stop narration
/// before they mean anything else.
fn stop_narration(runtime: &mut Option<InteractiveRuntime>, state: &mut TuiState) -> bool {
    // The reference only cancels when narration is live, so an idle narrator
    // keeps tracking the turn the operator is interrupting for another reason.
    if state.narrator.state() == narrator::NarratorState::Idle {
        return false;
    }
    let Some(effect) = state.narrator.cancel() else {
        return false;
    };
    if let Some(runtime) = runtime.as_mut() {
        apply_narrator_effect(effect, runtime, state);
    }
    true
}

/// Reference `_track_narrator_event`: the narrator follows the canonical stream,
/// and a streamed assistant entry contributes only its new text.
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

    use std::task::{Context, Poll};

    use super::*;

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

    /// The shipped defaults reach the terminal client: with no configuration
    /// file at all, the session opens on the default model, knows that model
    /// takes images, and reads the hosted-surface flag from the same document.
    #[test]
    fn startup_preferences_read_the_shipped_default_configuration() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let release3 = Release3Service::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            temporary.path().join("workspace"),
        );
        let arguments = <Arguments as clap::Parser>::try_parse_from(["vibe"])
            .expect("interactive arguments parse");

        let preferences =
            startup_preferences(&arguments, &release3).expect("preferences read from defaults");

        assert_eq!(preferences.model, DEFAULT_MODEL);
        assert!(preferences.image_models.get(DEFAULT_MODEL).supports_images);
        assert!(
            preferences
                .image_models
                .get("mistral-vibe-cli-latest")
                .supports_images,
            "the model's own name resolves alongside its alias"
        );
        assert!(
            !preferences.image_models.get("local").supports_images,
            "a model that takes no image is published as such"
        );
        assert!(preferences.vibe_code_enabled);
        assert_eq!(preferences.reasoning_effort, None);
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

    #[test]
    fn local_notices_do_not_advance_the_server_watermark() {
        let mut state = TuiState::new("session");
        push_local_notice(&mut state, "ready", EntryStatus::Completed);
        push_local_notice(&mut state, "done", EntryStatus::Completed);
        assert_eq!(state.watermark, 0);
        assert_eq!(state.entries.len(), 2);
    }

    #[test]
    fn only_the_reference_slash_exit_alias_bypasses_dispatch() {
        assert!(is_exit_command("/exit"));
        assert!(!is_exit_command("/close"));
        assert!(!is_exit_command("/quit"));
        assert!(!is_exit_command("/close now"));
    }
}
