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
pub mod onboarding;
#[cfg(test)]
mod onboarding_parity_tests;
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
    EnvironmentThemeDetector, PersistedCredentialStore, ResolvedTheme, TerminalThemeDetector,
    Theme, resolve_theme,
};
use self::shell::{finish_shell, interrupt_shell};
use self::state::{EntryStatus, ServerEvent, TranscriptEntry, TranscriptKind, TuiState};
use self::terminal::{CrosstermOps, TerminalGuard};
use self::turn::{
    ActiveTurn, CancellationPhase, drain_updates, finish_active, request_active_turn_interrupt,
    settle_unstarted_reservation, start_active_turn,
};
use self::voice::{SpeechEvent, SpeechManager, VoiceManager};
use crate::{
    Arguments, CliError, CliTelemetryObserver, bootstrap, telemetry_observer, validate_arguments,
};
use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::{Startup, TelemetryCommandKind, TeleportProgress};

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
    /// A process exit code the flow decided before any session started, which
    /// is how a cancelled onboarding exits 0 and an unusable key variable
    /// exits 1, as the reference's `run_onboarding` callers do.
    pub exit_code: Option<u8>,
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
            exit_code: None,
        });
    }
    if !startup::resolve_location_safety(trust.dangerous_warning.as_deref())? {
        return Ok(InteractiveExit {
            session_started: false,
            initialization_error: None,
            summary: None,
            exit_code: None,
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
                exit_code: None,
            });
        }
    }
    let vibe_home = startup::vibe_home_directory(&arguments, &working_directory);
    let credential_store =
        PersistedCredentialStore::new(vibe_core::config::global_env_file(&vibe_home));
    // The process environment first, then `{vibe_home}/.env`, then the
    // keyring under the shared service names: a key the operator keeps in
    // the dotenv file is as usable here as an exported one, and a keyring
    // that cannot be reached reads as absent, as the reference reads it.
    let resolve_credential = |store: &PersistedCredentialStore| {
        vibe_core::config::DotenvValues::global(&vibe_home)
            .variable(&arguments.credential_environment)
            .filter(|credential| !credential.is_empty())
            .or_else(|| store.resolve(&arguments.credential_environment))
    };
    let mut initial_credential = resolve_credential(&credential_store);
    // Reference `run_cli` and `load_config_orchestrator_or_exit`: `--setup`
    // always runs the onboarding screens and exits afterward, and an
    // interactive launch with no resolvable credential runs them and then
    // continues into the session it can now start.
    if arguments.setup || initial_credential.is_none() {
        match onboarding::run_onboarding(
            &arguments,
            &working_directory,
            &vibe_home,
            &credential_store,
        )
        .await?
        {
            onboarding::OnboardingConclusion::Exit(code) => {
                return Ok(InteractiveExit {
                    session_started: false,
                    initialization_error: None,
                    summary: None,
                    exit_code: Some(code),
                });
            }
            onboarding::OnboardingConclusion::Continue if arguments.setup => {
                return Ok(InteractiveExit {
                    session_started: false,
                    initialization_error: None,
                    summary: None,
                    exit_code: None,
                });
            }
            onboarding::OnboardingConclusion::Continue => {
                initial_credential = resolve_credential(&credential_store);
            }
        }
    }
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
            exit_code: None,
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
    if runtime.is_none() {
        push_local_notice(
            &mut state,
            &format!(
                "No API key resolved for {}. Set it in your environment or in the global .env \
                 under the vibe home, then restart.",
                arguments.credential_environment
            ),
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
                runtime: &mut runtime,
                active: &mut active,
                state: &mut state,
                controls: &mut controls,
                prompt_history: &mut prompt_history,
                input: &mut input,
                theme: &mut theme,
                terminal_guard: &mut terminal_guard,
                terminal: &mut terminal,
                clipboard_images: &mut clipboard_images,
                path_normalization: &mut path_normalization,
                deferred_enter: &mut deferred_enter,
            }
        };
    }

    // Reference `_pending_new_session_telemetry` and
    // `_startup_telemetry_sent`: both are once-per-process latches, the first
    // fired where the session settles and the second where the first frame has
    // been drawn.
    let mut session_reported = false;
    let mut startup_reported = false;
    let event_loop = async {
        let mut exit = false;
        while !exit {
            session_started |= runtime.is_some();
            if let Some(runtime) = runtime.as_ref()
                && !session_reported
            {
                session_reported = true;
                report_session_opened(runtime, &working_directory, &arguments);
            }
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
                record_audio_telemetry(runtime);
                record_narrator_telemetry(runtime, &mut state);
                // The narrator's own transport answers here, so a spoken summary
                // reaches the same state machine the effects came from.
                while let Some(event) = runtime.speech.try_next_event() {
                    apply_speech_event(event, &mut state);
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
            // Reference `_is_file_watcher_enabled`: the index consults the
            // preference on every query, so the refreshed value is published
            // before the next one is answered.
            input.set_file_watcher_enabled(state.file_watcher_for_autocomplete);
            if let Some(notice) = input.take_completion_notice() {
                state.push_diagnostic(&notice);
            }
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
                            secret_input: false,
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
            if let Some(runtime) = runtime.as_ref()
                && !startup_reported
            {
                startup_reported = true;
                report_startup(runtime);
            }
            if mounted_startup.needs_fatal_render() {
                match drain_ready_terminal_events(&mut events)? {
                    ReadyInputDrain::Empty => {
                        let _ =
                            drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
                        mounted_startup.arm_fatal_acknowledgment();
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
        runtime.speech.shutdown().await;
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
        // Reference `emit_session_closed_telemetry`, raised before the session
        // is closed so the census still names it.
        if let Some(telemetry) = runtime.telemetry.as_ref() {
            let _ = telemetry.enqueue(&TelemetryRecord::SessionClosed, Some(&session_id));
        }
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
        exit_code: None,
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
/// resource, and a spoken summary is posted to the configured speech model and
/// played through the default output device by [`SpeechManager`], which answers
/// asynchronously with the two events the same state machine settles on.
pub(in crate::tui) fn apply_narrator_effect(
    effect: narrator::NarratorEffect,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    match effect {
        // Reference `cancel`: playback stops before the machine returns to idle.
        narrator::NarratorEffect::Stop => runtime.speech.stop(),
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
            if let Some(narrator::NarratorEffect::Speak { generation, text }) =
                state.narrator.apply_summary(generation, summary)
            {
                runtime.speech.speak(generation, text);
            }
        }
        narrator::NarratorEffect::Speak { generation, text } => {
            runtime.speech.speak(generation, text);
        }
    }
}

/// Applies one answer from the speech transport. The generation each answer
/// carries is what the state machine discards a superseded turn by, so a result
/// that outlived its turn settles nothing and plays nothing.
fn apply_speech_event(event: SpeechEvent, state: &mut TuiState) {
    match event {
        SpeechEvent::PlaybackStarted { generation } => state.narrator.playback_started(generation),
        SpeechEvent::Finished { generation, error } => {
            match error {
                // Reference `_speak_summary` reports the exception's class
                // name; this port's transport answers a message rather than an
                // exception, so the class it names is the failure itself.
                Some(failure) => {
                    state.narrator.fail(generation, SPEECH_ERROR_CLASS);
                    report_speech_failure(state, failure);
                }
                None => state.narrator.settle(generation),
            }
        }
    }
}

/// What a read-aloud failure reports as its error type.
const SPEECH_ERROR_CLASS: &str = "SpeechError";

/// Sends the read-aloud events the narrator produced, on the same terms as the
/// transcription ones.
pub(in crate::tui) fn record_narrator_telemetry(
    runtime: &InteractiveRuntime,
    state: &mut TuiState,
) {
    let records = state.narrator.take_telemetry();
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    for record in &records {
        let _ = telemetry.enqueue(record, Some(runtime.session_id.as_str()));
    }
}

/// Reports a speech failure once per session. An unconfigured model, an absent
/// output device and an endpoint that refuses the request are all the same fact
/// on every following turn, so the operator is told once rather than once per
/// turn, and the turn itself stays successful.
fn report_speech_failure(state: &mut TuiState, failure: String) {
    if state.speech_notice_shown {
        return;
    }
    state.speech_notice_shown = true;
    state.push_diagnostic(failure);
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
        .get("config")
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

/// Sends the audio lifecycle events the voice manager produced.
///
/// The reference hands each one to the agent loop's telemetry client
/// (`vibe/cli/voice_manager/voice_manager.py:202-251`), and so does this port:
/// the same client, the same census and the same `enable_telemetry` gate as
/// every other event. A delivery failure is never surfaced to the operator:
/// telemetry is best effort on both sides, and a diagnostic here would put an
/// audio event in the transcript.
pub(in crate::tui) fn record_audio_telemetry(runtime: &mut InteractiveRuntime) {
    let records = runtime.voice.take_telemetry();
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    for record in &records {
        let _ = telemetry.enqueue(record, Some(runtime.session_id.as_str()));
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
    // The write names no fingerprint: the service takes the one on disk inside
    // the transaction that compares it, which is a narrower window than reading
    // it here one call earlier.
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
                "mutations": [mutation],
            }],
        }),
        state,
    )
    .is_some()
}

fn apply_public_notifications(
    dispatch: &PublicDispatch,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    for notification in &dispatch.notifications {
        if notification.method == "vibeCode/teleport/event" {
            record_teleport_progress(notification.params.get("event"), runtime);
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

/// Reference `TeleportTelemetryTracker.record_event` and the two senders that
/// close a run: the tracker walks the stages the run reports and answers a
/// completed or a failed event where it ends.
fn record_teleport_progress(event: Option<&Value>, runtime: &mut InteractiveRuntime) {
    let Some(event) = event else {
        return;
    };
    let Some(tracker) = runtime.teleport_telemetry.as_mut() else {
        return;
    };
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(progress) = match kind {
        "summarizing_context" => Some(TeleportProgress::SummarizingContext),
        "checking_git" => Some(TeleportProgress::CheckingGit),
        "push_required" => Some(TeleportProgress::PushRequired),
        "pushing" => Some(TeleportProgress::Pushing),
        "starting_workflow" => Some(TeleportProgress::StartingWorkflow),
        "complete" => Some(TeleportProgress::Complete),
        _ => None,
    } {
        tracker.record_progress(progress);
    }
    let record = match kind {
        "complete" => Some(tracker.completed()),
        "cancelled" => {
            tracker.record_cancelled();
            tracker.failed()
        }
        "failed" => {
            let (code, status) = service_error_of(event);
            tracker.record_service_error(code, status.map(|_| "http".to_owned()), status);
            tracker.failed()
        }
        _ => None,
    };
    let Some(record) = record else {
        return;
    };
    runtime.teleport_telemetry = None;
    runtime.project_picker = None;
    if let Some(telemetry) = runtime.telemetry.as_ref() {
        let _ = telemetry.enqueue(&record, Some(runtime.session_id.as_str()));
    }
}

/// Reference `record_service_error`'s two arguments, read off the failure event
/// the server published: the class the service named, and the HTTP status it
/// answered with when it answered with one. A saved-link selection the service
/// refused with a 403 or a 404 is what the status decides.
fn service_error_of(event: &Value) -> (&str, Option<u64>) {
    let error = event.get("error");
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(TELEPORT_ERROR_CLASS);
    let status = error
        .and_then(|error| error.pointer("/details/httpStatusCode"))
        .and_then(Value::as_u64);
    (code, status)
}

/// What a failure that named no class reports as one.
const TELEPORT_ERROR_CLASS: &str = "TeleportError";

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
        // A drag copies as it goes, so no event is raised here: the reference
        // reports a copy the operator asked for, not one the selection made.
        let _ = copy_transcript_selection(state);
    }
}

/// Reference `action_copy_selection`: copies the transcript selection when one
/// exists, and reports a clipboard refusal without discarding it. The copied
/// text is answered back because its length is what `vibe.user_copied_text`
/// reports; the text itself never leaves the process.
fn copy_transcript_selection(state: &mut TuiState) -> Option<String> {
    let selection = state.transcript_view.selected_text()?;
    match clipboard::SystemClipboardPort::copy_text(&clipboard::SystemClipboard, &selection) {
        Ok(()) => push_local_notice(
            state,
            "Selection copied to clipboard",
            EntryStatus::Completed,
        ),
        Err(_) => state.push_diagnostic("Failed to copy: clipboard not available"),
    }
    Some(selection)
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
    let telemetry = telemetry_observer(arguments, &release3)?;
    let mut driver = LiveTurnDriver::from_credential(
        bootstrap::live_driver_config(
            arguments,
            &preferences.model,
            release3.compaction_prompts(),
        )?,
        credential.clone(),
    )?;
    driver = driver.with_event_observer(telemetry.clone());
    let configuration = release3.clone();
    let server = bootstrap::resource_server(
        arguments,
        release3,
        credential.clone(),
        Some(driver.sampling_handler(&preferences.model)),
    )?
    .using_release4_service(bootstrap::cloud_service(credential)?);
    let mut service =
        HeadlessService::new_interactive_shared_with_server(Arc::new(driver), server)?;
    let session_start = std::time::Instant::now();
    let session_id = service.start_session(&bootstrap::session_options(
        arguments,
        working_directory,
        preferences.model.clone(),
        Some(preferences.mode.clone()),
        preferences.reasoning_effort.clone(),
    ))?;
    let session_init_duration_ms =
        u64::try_from(session_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    // The audio surface is resolved from the configuration this session
    // publishes, not from the LLM endpoint: the transcription model, its wire
    // values, the provider's endpoint and the variable its credential is read
    // from all come from the same view a settings screen renders.
    let published_config = service
        .public_call("config/read", json!({"sessionId": session_id}))
        .ok()
        .and_then(|result| result.get("config").cloned())
        .unwrap_or(Value::Null);
    let voice_enabled = published_config
        .get("voiceModeEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let vibe_home = startup::vibe_home_directory(arguments, working_directory);
    let voice = VoiceManager::production(
        &published_config,
        &voice_credential,
        &vibe_home,
        voice_enabled,
    );
    // Reference `_make_tts_client`: the read-aloud client comes from the same
    // view, and a configuration it cannot be built from leaves the narrator
    // silent rather than failing the session.
    let speech = SpeechManager::production(&published_config, &voice_credential, &vibe_home);
    let session = service.session(&session_id)?;
    let agent_name = session
        .intent
        .agent
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    let safety = active_agent_safety(&mut service, &session_id, &agent_name);
    Ok(InteractiveRuntime {
        service,
        release3: configuration,
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
        telemetry: Some(telemetry),
        project_picker: None,
        teleport_telemetry: None,
        session_init_duration_ms: Some(session_init_duration_ms),
        voice,
        speech,
    })
}

/// Reference `emit_new_session_telemetry` and `emit_ready_telemetry`, which the
/// agent loop raises together once initialization settles: the session census
/// first, then how long reaching it took.
fn report_session_opened(
    runtime: &InteractiveRuntime,
    working_directory: &Path,
    arguments: &Arguments,
) {
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    let session = Some(runtime.session_id.as_str());
    let _ = telemetry.enqueue(
        &TelemetryRecord::NewSession(crate::session_census(
            &runtime.release3,
            working_directory,
            arguments.trust,
        )),
        session,
    );
    let _ = telemetry.enqueue(
        &TelemetryRecord::Ready {
            init_duration_ms: crate::since_process_start_ms(),
        },
        session,
    );
}

/// Reference `_send_startup_telemetry_once`: the three durations, once per
/// process, taken where the first frame has just been drawn.
fn report_startup(runtime: &InteractiveRuntime) {
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    let elapsed = crate::since_process_start_ms();
    let _ = telemetry.enqueue(
        &TelemetryRecord::Startup(Startup {
            first_frame_duration_ms: Some(elapsed),
            agent_ready_duration_ms: Some(elapsed),
            session_init_duration_ms: runtime.session_init_duration_ms,
        }),
        Some(runtime.session_id.as_str()),
    );
}

/// Reference `_handle_command` and `_send_skill_telemetry`: one event, whose
/// type tells a built-in command from a skill invocation.
pub(in crate::tui) fn report_slash_command(
    runtime: &InteractiveRuntime,
    command_line: &str,
    kind: TelemetryCommandKind,
) {
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    let Some(command) = command_line.split_whitespace().next() else {
        return;
    };
    let _ = telemetry.enqueue(
        &TelemetryRecord::SlashCommandUsed {
            command: command.to_owned(),
            kind,
        },
        Some(runtime.session_id.as_str()),
    );
}

/// Reference `action_toggle_voice_mode`.
pub(in crate::tui) fn report_voice_mode_toggled(runtime: &InteractiveRuntime, enabled: bool) {
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    let _ = telemetry.enqueue(
        &TelemetryRecord::VoiceModeToggled { enabled },
        Some(runtime.session_id.as_str()),
    );
}

/// Reference `send_user_copied_text`, which the pinned reference publishes on
/// its client without a live call site. The copy shortcut is where this port
/// raises it, and the text itself never travels: only its length does.
pub(in crate::tui) fn report_copied_text(runtime: Option<&InteractiveRuntime>, copied: &str) {
    let Some(runtime) = runtime else {
        return;
    };
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    let _ = telemetry.enqueue(
        &TelemetryRecord::UserCopiedText {
            text_length: copied.chars().count() as u64,
        },
        Some(runtime.session_id.as_str()),
    );
}

/// Reference `vibe.user_cancelled_action`, raised at the three sites the
/// reference raises it: an interrupted agent, a refused approval and a
/// cancelled question.
pub(in crate::tui) fn report_cancelled_action(
    runtime: Option<&InteractiveRuntime>,
    action: CancelledAction,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let Some(telemetry) = runtime.telemetry.as_ref() else {
        return;
    };
    let _ = telemetry.enqueue(
        &TelemetryRecord::UserCancelledAction {
            action: action.label().to_owned(),
        },
        Some(runtime.session_id.as_str()),
    );
}

/// The three actions the reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum CancelledAction {
    InterruptAgent,
    RejectApproval,
    CancelQuestion,
}

impl CancelledAction {
    const fn label(self) -> &'static str {
        match self {
            Self::InterruptAgent => "interrupt_agent",
            Self::RejectApproval => "reject_approval",
            Self::CancelQuestion => "cancel_question",
        }
    }
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
        // The banner reports the skills the operator added, so the two seeded
        // builtins are excluded the way the reference's `custom_skills_count`
        // excludes them.
        banner.skills_count = dispatch
            .result
            .get("skills")
            .and_then(Value::as_array)
            .map_or(0, |skills| {
                skills
                    .iter()
                    .filter(|skill| skill.get("source").and_then(Value::as_str) != Some("builtin"))
                    .count()
            });
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
        // The published list carries the connectors too, and the banner counts
        // them on their own line.
        let servers = sources
            .iter()
            .filter(|source| source.get("kind").and_then(Value::as_str) != Some("connector"))
            .collect::<Vec<_>>();
        banner.mcp_servers_total = servers.len();
        banner.mcp_servers_enabled = servers
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

fn startup_preferences(
    arguments: &Arguments,
    release3: &Release3Service,
) -> Result<StartupPreferences, CliError> {
    let document = release3
        .config_document()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    let config = document.get("config");
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

    /// US-011: a teleport run walks the stages its own notifications report,
    /// and the completed event carries the picker payload the run started with.
    #[test]
    fn a_teleport_run_reports_the_stage_it_reached() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let release3 = Release3Service::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            temporary.path().join("workspace"),
        );
        let mut runtime = runtime::interactive_test_runtime_with_server(
            "teleport-telemetry",
            vibe_app_server::server::AppServer::with_release3_service(release3),
        );
        runtime.project_picker = Some(vibe_core::telemetry::records::ProjectPicker {
            shown: true,
            selection_source: Some(
                vibe_core::telemetry::records::ProjectSelectionSource::SavedLink,
            ),
            candidate_count_loaded: Some(3),
            multi_repo_match_count: Some(1),
            saved_project_link_cleared: Some(false),
            repo_remote_changed: Some(false),
        });
        runtime.teleport_telemetry = Some(vibe_core::telemetry::records::TeleportTracker::new(
            12,
            vibe_core::telemetry::records::TeleportFailureStage::Ineligible,
            runtime.project_picker,
        ));

        for kind in ["summarizing_context", "checking_git", "pushing"] {
            record_teleport_progress(Some(&json!({"kind": kind})), &mut runtime);
        }
        let tracker = runtime
            .teleport_telemetry
            .clone()
            .expect("the run is still open");
        let failed = tracker.failed();
        assert!(
            failed.is_none(),
            "a run that classified no error reports nothing"
        );

        // The service refuses the saved link with a 403, which is what clears
        // it, and the failure is attributed to the stage the run had reached.
        record_teleport_progress(
            Some(&json!({
                "kind": "failed",
                "error": {
                    "code": "ServiceTeleportError",
                    "message": "refused",
                    "details": {"httpStatusCode": 403},
                },
            })),
            &mut runtime,
        );
        assert!(
            runtime.teleport_telemetry.is_none() && runtime.project_picker.is_none(),
            "a terminal event closes the run"
        );

        let mut tracker = vibe_core::telemetry::records::TeleportTracker::new(
            12,
            vibe_core::telemetry::records::TeleportFailureStage::Ineligible,
            None,
        );
        tracker.record_progress(vibe_core::telemetry::records::TeleportProgress::Pushing);
        tracker.record_service_error("ServiceTeleportError", Some("http".to_owned()), Some(403));
        let record = tracker.failed().expect("a classified error is a failure");
        let properties = record
            .attributes(None)
            .expect("the payload carries no unsafe label")
            .into_properties();
        assert_eq!(properties["stage"], json!("push"));
        assert_eq!(properties["http_status_code"], json!(403));
    }

    /// US-011: the failure event the server publishes is where the class and
    /// the status come from, and a failure carrying neither still names a
    /// class rather than an empty one.
    #[test]
    fn a_failure_event_answers_the_class_and_the_status_the_service_named() {
        assert_eq!(
            service_error_of(&json!({
                "kind": "failed",
                "error": {
                    "message": "refused",
                    "code": "ServiceTeleportError",
                    "details": {"httpStatusCode": 403},
                },
            })),
            ("ServiceTeleportError", Some(403))
        );
        assert_eq!(
            service_error_of(&json!({
                "kind": "failed",
                "error": {"message": "the remote went away", "details": Value::Null},
            })),
            ("TeleportError", None)
        );
    }

    /// US-008: the session census is read off the services a session is built
    /// from rather than from the banner.
    #[test]
    fn the_session_census_counts_what_the_configuration_declares() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("the workspace is created");
        std::fs::write(workspace.join("AGENTS.md"), "instructions")
            .expect("the instructions file is written");
        let release3 = Release3Service::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            workspace.clone(),
        );

        let census = crate::session_census(&release3, &workspace, true);

        assert!(
            census.has_agents_md,
            "the workspace publishes instructions to the agent"
        );
        assert!(
            census.nb_models >= 1,
            "the shipped defaults declare at least the default model"
        );
        assert!(
            !crate::has_agents_md(temporary.path()),
            "a directory with no instructions file reports none"
        );
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

    /// US-169: the composer's skill map is built from `skills/list` and keeps
    /// only user-invocable entries, so `/vibe` resolves no skill and stays an
    /// ordinary prompt while `/skill-creator` is invocable.
    #[test]
    fn a_model_only_builtin_is_not_invocable_from_the_composer() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let release3 = Release3Service::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            temporary.path().join("workspace"),
        );

        let skills = runtime_skills(&release3);

        assert!(
            !skills.contains_key("vibe"),
            "`vibe` is not user invocable, so the composer cannot invoke it"
        );
        assert!(
            skills.contains_key("skill-creator"),
            "`skill-creator` is user invocable and reachable as a slash word"
        );
    }

    /// US-171: the banner counts the skills the operator added, read through
    /// the same `banner_metrics_from_release3` the startup path calls. The two
    /// seeded builtins never count, the user's own skills do, and one withheld
    /// by `disabled_skills` drops out because the count reads the filtered
    /// catalog rather than the walked one.
    #[test]
    fn the_banner_counts_custom_skills_only() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace = temporary.path().join("workspace");
        let vibe_home = temporary.path().join(".vibe");
        std::fs::create_dir_all(&vibe_home).expect("vibe home");
        let release3 = Release3Service::for_runtime_session_root(
            vibe_home.join("sessions"),
            workspace.clone(),
        );
        let arguments = <Arguments as clap::Parser>::try_parse_from(["vibe"])
            .expect("interactive arguments parse");
        let counted =
            || banner_metrics_from_release3(&release3, &arguments, &workspace).skills_count;

        assert_eq!(
            counted(),
            0,
            "only the two builtins are published, and neither is the operator's"
        );

        for name in ["alpha", "beta", "gamma"] {
            let directory = vibe_home.join("skills").join(name);
            std::fs::create_dir_all(&directory).expect("skill directory");
            std::fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: A user skill.\n---\n\nBody.\n"),
            )
            .expect("skill file");
        }
        assert_eq!(
            counted(),
            3,
            "the three user skills count and the builtins beside them do not"
        );

        std::fs::write(
            vibe_home.join("config.toml"),
            "disabled_skills = [\"beta\"]\n",
        )
        .expect("user configuration");
        assert_eq!(
            counted(),
            2,
            "the withheld skill is never published, so the count reads the filtered catalog"
        );
    }

    #[test]
    fn stale_inputs_are_drained_before_fatal_acknowledgment_arms() {
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
        startup.arm_fatal_acknowledgment();
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
