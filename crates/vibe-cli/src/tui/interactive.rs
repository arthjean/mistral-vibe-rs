//! The interactive session: what it owns between two frames, and the loop that
//! settles, paints and awaits.
//!
//! Every local the loop needs lives on [`Session`], so the two dispatch points
//! that hand the terminal a key borrow one value instead of fourteen, and the
//! loop body reads as the three phases it actually has.

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::update_notifier::UpdateCacheStore;
use crossterm::event::{Event, EventStream, KeyEvent, KeyEventKind};
use futures_util::StreamExt;
use serde_json::Value;
use vibe_core::cache_store::CacheStore;

use super::callback::{
    drain_callback_requests, hold_for_typing, sync_active_callbacks, sync_callback_presentation,
};
use super::chat_input::{ChatInputState, InputEvent, Safety, VoicePhase};
use super::clipboard_images::ClipboardImageManager;
use super::composer::{
    apply_effects as apply_composer_effects, apply_event as apply_composer_event,
};
use super::controls::{CallbackRequest, ControlState};
use super::history::PromptHistory;
use super::hydration::hydrate_initial_state;
use super::narration::{
    apply_narrator_effect, apply_speech_event, record_audio_telemetry, record_narrator_telemetry,
};
use super::paste_burst::PasteBurst;
use super::path_normalization::PathNormalizationManager;
use super::plan_review::PlanReviewMonitor;
use super::prompt::PromptContext;
use super::queue::start_next_queued_prompt;
use super::render::{BannerContext, TokenState, UiContext, draw};
use super::runtime::{
    BannerMetrics, InteractiveRuntime, apply_ui_operation_completion, command_context,
};
use super::session::{banner_metrics_from_workspace, start_runtime};
use super::setup::{EnvironmentThemeDetector, ResolvedTheme, TerminalThemeDetector, Theme};
use super::shell::{finish_shell, interrupt_shell};
use super::state::{EntryStatus, ServerEvent, TuiState};
use super::telemetry::{refresh_account_when_ready, report_session_opened, report_startup};
use super::terminal::FullScreen;
use super::turn::{
    ActiveTurn, CancellationPhase, drain_updates, finish_active, request_active_turn_interrupt,
    settle_unstarted_reservation, start_active_turn,
};
use super::{
    Arguments, CliError, DEFAULT_CONTEXT_WINDOW, FRAME_INTERVAL, apply_path_normalization_event,
    configured_theme, exit, push_local_notice, push_local_warning, shortcuts, startup, switching,
    unix_seconds, updates, workflow,
};
use drain::{
    INPUT_ENDED, ReadyInputDrain, drain_ready_interrupts, drain_ready_into,
    drain_ready_terminal_events,
};
use shutdown::shut_down;

/// How an interactive launch ended.
#[derive(Debug)]
pub struct InteractiveExit {
    pub initialization_error: Option<CliError>,
    /// Reference `SessionExitSummary`, printed after the terminal is restored.
    pub summary: Option<exit::SessionExitSummary>,
    /// A process exit code the flow decided before any session started, which
    /// is how a cancelled onboarding exits 0 and an unusable key variable
    /// exits 1, as the reference's `run_onboarding` callers do.
    pub exit_code: Option<u8>,
}

impl InteractiveExit {
    /// A launch a pre-session gate refused: nothing opened, so nothing settles
    /// beyond the exit code the gate decided.
    const fn aborted(exit_code: Option<u8>) -> Self {
        Self {
            initialization_error: None,
            summary: None,
            exit_code,
        }
    }
}

/// Everything the interactive loop owns between two frames.
///
/// The terminal handle and its guard live here too: a key that opens an
/// external editor or suspends the process needs both, and threading them
/// separately is what used to force the dispatch context to be built by macro.
struct Session {
    arguments: Arguments,
    working_directory: PathBuf,
    /// The counts the banner falls back to before a session exists.
    fallback_banner: BannerMetrics,
    runtime: Option<InteractiveRuntime>,
    state: TuiState,
    controls: ControlState,
    input: ChatInputState,
    active: Option<ActiveTurn>,
    theme: ResolvedTheme,
    prompt_history: PromptHistory,
    clipboard_images: ClipboardImageManager,
    path_normalization: PathNormalizationManager,
    plan_review_monitor: PlanReviewMonitor,
    mounted_startup: startup::MountedStartup,
    screen: FullScreen,
    /// A submission held back until path normalization settles.
    deferred_enter: Option<KeyEvent>,
    /// Reference `_pending_new_session_telemetry` and `_startup_telemetry_sent`:
    /// both are once-per-process latches, the first fired where the session
    /// settles and the second where the first frame has been drawn.
    session_reported: bool,
    startup_reported: bool,
    /// The plan title the post-ready account read answers, once it lands.
    startup_account: Option<tokio::sync::oneshot::Receiver<Option<String>>>,
}

impl Session {
    /// The borrow every terminal-event handler works through.
    fn keys(&mut self) -> shortcuts::KeyContext<'_> {
        shortcuts::KeyContext {
            arguments: &self.arguments,
            working_directory: &self.working_directory,
            runtime: &mut self.runtime,
            active: &mut self.active,
            state: &mut self.state,
            controls: &mut self.controls,
            prompt_history: &mut self.prompt_history,
            input: &mut self.input,
            theme: &mut self.theme,
            terminal_guard: &mut self.screen.guard,
            terminal: &mut self.screen.terminal,
            clipboard_images: &mut self.clipboard_images,
            path_normalization: &mut self.path_normalization,
            deferred_enter: &mut self.deferred_enter,
        }
    }

    /// Brings every asynchronous answer into the projection before the frame is
    /// painted: audio, narration, server updates, callbacks, the finished turn,
    /// the finished shell, and the next queued prompt.
    async fn settle(&mut self) -> Result<(), CliError> {
        if let Some(runtime) = self.runtime.as_ref()
            && !self.session_reported
        {
            self.session_reported = true;
            report_session_opened(runtime, &self.working_directory, &self.arguments);
            self.startup_account = refresh_account_when_ready(runtime);
        }
        if let (Some(runtime), Some(pending)) =
            (self.runtime.as_mut(), self.startup_account.as_mut())
        {
            match pending.try_recv() {
                Ok(plan) => {
                    runtime.banner.plan = plan;
                    self.startup_account = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.startup_account = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        if let Some(runtime) = self.runtime.as_mut() {
            while let Some(event) = runtime.voice.try_next_event() {
                apply_composer_event(
                    &mut self.input,
                    event,
                    &self.working_directory,
                    &mut self.state,
                );
            }
            record_audio_telemetry(runtime);
            record_narrator_telemetry(runtime, &mut self.state);
            // The narrator's own transport answers here, so a spoken summary
            // reaches the same state machine the effects came from.
            while let Some(event) = runtime.speech.try_next_event() {
                apply_speech_event(event, &mut self.state);
            }
        }
        for diagnostic in self.prompt_history.drain_ready().await {
            self.state.push_diagnostic(diagnostic);
        }
        drain_updates(
            &mut self.state,
            self.runtime.as_mut(),
            self.active.as_mut(),
            &mut self.controls,
        );
        drain_callback_requests(self.runtime.as_mut(), &mut self.state, &mut self.controls);
        if let Some(runtime) = self.runtime.as_mut() {
            super::subagents::poll(runtime, &mut self.state);
        }
        super::turn::report_slow_interrupt(
            self.runtime.as_ref(),
            self.active.as_mut(),
            &mut self.state,
        );
        finish_active(
            &mut self.state,
            &mut self.controls,
            &mut self.runtime,
            &mut self.active,
            &mut self.input,
        )
        .await?;
        finish_shell(self.runtime.as_mut(), &mut self.state).await;
        start_next_queued_prompt(PromptContext::new(
            &self.working_directory,
            &mut self.runtime,
            &mut self.active,
            &mut self.state,
            &mut self.controls,
            &mut self.clipboard_images,
        ))
        .await?;
        // Reference `_is_file_watcher_enabled`: the index consults the
        // preference on every query, so the refreshed value is published
        // before the next one is answered.
        self.input
            .set_file_watcher_enabled(self.state.file_watcher_for_autocomplete);
        if let Some(notice) = self.input.take_completion_notice() {
            self.state.push_diagnostic(&notice);
        }
        let effects = self.input.poll_completion();
        apply_composer_effects(
            &mut self.input,
            effects,
            &self.working_directory,
            &mut self.state,
        );
        let now_ms = super::unix_millis();
        let plan_path =
            self.controls
                .pending_callback()
                .and_then(|pending| match &pending.request {
                    CallbackRequest::PlanReview { plan_path, .. } => Some(plan_path.clone()),
                    CallbackRequest::Approval { .. } | CallbackRequest::UserInput { .. } => None,
                });
        self.plan_review_monitor
            .sync(plan_path, &mut self.state)
            .await;
        if let Some(runtime) = self.runtime.as_mut() {
            super::switching::sync_routed_model(runtime);
        }
        hold_for_typing(&mut self.controls, &mut self.state, now_ms);
        sync_callback_presentation(&self.controls, &mut self.state, now_ms);
        self.state.sync_activity(now_ms);
        if self
            .state
            .debug_console
            .as_ref()
            .is_some_and(|console| console.poll_due(now_ms))
            && let Some(runtime) = self.runtime.as_mut()
        {
            workflow::refresh_debug_console(runtime, &mut self.state, now_ms);
        }
        Ok(())
    }

    /// Paints one frame from the projection as it now stands.
    fn draw(&mut self) -> Result<(), CliError> {
        super::flush_attention(&mut self.state);
        let runtime = self.runtime.as_ref();
        let agent_name = runtime.map_or("default", |runtime| runtime.agent_name.as_str());
        let border_title = format!(" {} ", agent_name.to_lowercase());
        self.input.set_agent_name(agent_name);
        self.input
            .set_safety(runtime.map_or(Safety::Neutral, |runtime| runtime.safety));
        let banner = runtime.map_or(&self.fallback_banner, |runtime| &runtime.banner);
        let model = runtime.map_or(self.arguments.model(), |runtime| runtime.model.as_str());
        let thinking = runtime.map_or("off", |runtime| runtime.thinking.as_str());
        let tokens = runtime.map_or(
            TokenState {
                max_tokens: DEFAULT_CONTEXT_WINDOW,
                current_tokens: 0,
            },
            |runtime| TokenState {
                max_tokens: runtime.context_window,
                current_tokens: runtime.context_tokens,
            },
        );
        let context = UiContext {
            cwd: &self.working_directory,
            agent_name: &border_title,
            secret_input: false,
            safety: self.input.safety(),
            switching: self.input.switching(),
            feedback_active: self.input.feedback_active(),
            voice_phase: self.input.voice_phase(),
            voice_indicator: self.input.voice_indicator(),
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
                model_spinner: runtime
                    .and_then(|runtime| runtime.experiments.as_ref())
                    .filter(|experiments| experiments.awaiting_model())
                    .map(|_| super::render::model_spinner_frame(super::unix_millis())),
            },
            tokens,
        };
        let state = &mut self.state;
        let input = &self.input;
        let theme = self.theme;
        self.screen
            .terminal
            .draw(|frame| {
                draw(
                    frame,
                    state,
                    input.editor(),
                    input.completion(),
                    input.mode(),
                    theme,
                    context,
                );
            })
            .map_err(|error| CliError::Terminal(error.to_string()))?;
        if let Some(runtime) = self.runtime.as_ref()
            && !self.startup_reported
        {
            self.startup_reported = true;
            report_startup(
                runtime,
                &self.arguments,
                Some(crate::since_process_start_ms()),
            );
        }
        Ok(())
    }

    /// Reference `action_interrupt_or_quit` at the signal, rather than at a key:
    /// a fatal startup quits, a live turn is interrupted, a running shell is
    /// interrupted, and an idle session quits.
    async fn handle_interrupt(&mut self) -> bool {
        if self.mounted_startup.is_fatal() {
            return true;
        }
        if self.active.is_some() && self.runtime.is_some() {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.cancellation != CancellationPhase::Active)
            {
                return true;
            }
            request_active_turn_interrupt(
                &mut self.runtime,
                &mut self.active,
                &mut self.controls,
                &mut self.state,
            );
            return false;
        }
        if let Some(runtime) = self.runtime.as_mut()
            && runtime.shell.is_some()
        {
            interrupt_shell(runtime, &mut self.state).await;
            return false;
        }
        true
    }

    /// Starts the scheduled turn whose time has come, if the session is idle.
    ///
    /// Reference `_begin_unsolicited_turn`: a turn nobody typed still restarts
    /// narration, with no user message.
    async fn start_due_loop(&mut self) -> Result<(), CliError> {
        if self.active.is_some()
            || self
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.shell.is_some())
        {
            return Ok(());
        }
        let Some(runtime) = self.runtime.as_mut() else {
            return Ok(());
        };
        let Some(scheduled) = runtime
            .service
            .reserve_due_loop(&runtime.session_id, unix_seconds())
            .await?
        else {
            return Ok(());
        };
        let message = scheduled
            .notice
            .params
            .get("entry")
            .and_then(|entry| entry.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Scheduled loop fired")
            .to_owned();
        push_local_notice(&mut self.state, &message, EntryStatus::Completed);
        if let Some(effect) = self.state.narrator.cancel() {
            apply_narrator_effect(effect, runtime);
        }
        self.state.narrator.on_turn_start("");
        match start_active_turn(
            &runtime.service,
            scheduled.reservation,
            Some(scheduled.loop_id),
            &mut self.controls,
        ) {
            Ok(started) => {
                self.active = Some(started);
                self.state.waiting = true;
            }
            Err(failure) => {
                let (reservation, error) = *failure;
                let failure = format!("Reserved scheduled turn could not start locally: {error}");
                settle_unstarted_reservation(runtime, &mut self.state, &reservation, &failure);
            }
        }
        Ok(())
    }

    /// Reports the transport as lost and ends the session.
    fn lose_transport(&mut self, message: String) -> Result<(), CliError> {
        self.state
            .apply(ServerEvent::TransportLost(message))
            .map(|_| ())
            .map_err(|error| CliError::Terminal(error.to_string()))
    }
}

pub async fn run_interactive(
    invocation: startup::InteractiveInvocation,
) -> Result<InteractiveExit, CliError> {
    let startup::ReadyStartup {
        mut arguments,
        working_directory,
        workspace,
        credential,
        post_mount_action,
        update_cache,
    } = match startup::preflight(invocation).await? {
        ControlFlow::Break(exit_code) => return Ok(InteractiveExit::aborted(exit_code)),
        ControlFlow::Continue(ready) => ready,
    };
    let update_checks_enabled = startup::update_checks_enabled(&workspace);
    let fallback_banner = banner_metrics_from_workspace(&workspace, &arguments, &working_directory);
    // The runtime schedules interactive calls through this channel from the
    // moment it exists, so it is opened before the session rather than patched
    // in afterward.
    let (ui_operation_sender, mut ui_operation_receiver) = tokio::sync::mpsc::unbounded_channel();
    let continuing = arguments.continue_session;
    let mut runtime = match credential {
        Some(credential) => Some(start_runtime(
            &mut arguments,
            &working_directory,
            workspace,
            credential,
            ui_operation_sender,
        )?),
        None => None,
    };
    let session_id = runtime
        .as_ref()
        .map_or_else(|| "setup".to_owned(), |runtime| runtime.session_id.clone());
    let mut state = match runtime.as_mut() {
        Some(runtime) => {
            let mut state = hydrate_initial_state(runtime, &arguments);
            if continuing && !arguments.continue_session {
                push_local_notice(
                    &mut state,
                    "No previous sessions found.",
                    EntryStatus::Completed,
                );
            }
            state
        }
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
                arguments.credential_environment()
            ),
            EntryStatus::Completed,
        );
    }
    if let Some(runtime) = runtime.as_mut() {
        // Reference `_on_config_changed`: rendering, notification, and narration
        // preferences apply before the first frame, not only after an edit.
        workflow::apply_render_preferences(runtime, &mut state);
        // Reference `_build_command_registry` reads the registry-skills gate
        // once, when the session is ready, and never rebuilds the registry.
        runtime.registry_skills_enabled = runtime
            .published_config()
            .as_ref()
            .is_some_and(super::runtime::registry_skills_enabled);
    }
    announce_release_notes(&update_cache, &mut state);
    if runtime.is_some() {
        warn_about_untrusted_config(
            &startup::vibe_home_directory(&arguments, &working_directory),
            &working_directory,
            &mut state,
        );
    }
    // Reference `_schedule_update_notification`: refresh the cache for the next
    // startup without rendering anything or blocking input.
    let update_check = startup::scheduled_update_gateway(update_checks_enabled).map(|gateway| {
        let store = update_cache.clone();
        tokio::spawn(async move {
            startup::refresh_update_cache(&gateway, &store, env!("CARGO_PKG_VERSION")).await;
        })
    });
    let mut controls = ControlState::new(session_id);
    if let Some(runtime) = runtime.as_mut() {
        sync_active_callbacks(runtime, &mut state, &mut controls);
    }
    let (prompt_history, history_load) = PromptHistory::open(
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
    input.set_command_context(command_context(runtime.as_ref()));
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
    let theme = super::setup::resolve_theme(
        configured_theme(runtime.as_mut()).unwrap_or(Theme::System),
        EnvironmentThemeDetector.detect(),
        std::env::var_os("NO_COLOR").is_some(),
    );
    let screen = FullScreen::enter().map_err(CliError::Terminal)?;
    state.waiting |= runtime.is_some();
    let mut session = Session {
        arguments,
        working_directory,
        fallback_banner,
        runtime,
        state,
        controls,
        input,
        active: None,
        theme,
        prompt_history,
        clipboard_images: ClipboardImageManager::default(),
        path_normalization: PathNormalizationManager::new().map_err(CliError::Terminal)?,
        plan_review_monitor: PlanReviewMonitor::default(),
        mounted_startup: startup::MountedStartup::new(post_mount_action),
        screen,
        deferred_enter: None,
        session_reported: false,
        startup_reported: false,
        startup_account: None,
    };

    let mut events = EventStream::new();
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let _ = drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut voice_ticker = tokio::time::interval(Duration::from_millis(100));
    voice_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Reference `PEAK_POLL_INTERVAL`: the recording indicator reads the
    // microphone's level every 50 ms while it records.
    let mut peak_ticker = tokio::time::interval(Duration::from_millis(50));
    peak_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut loop_ticker = tokio::time::interval(Duration::from_secs(1));
    loop_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut paste_burst = PasteBurst::for_platform();

    let event_loop = async {
        let mut exit = false;
        while !exit {
            session.settle().await?;
            if !session.path_normalization.has_pending()
                && let Some(key) = session.deferred_enter.take()
            {
                exit =
                    shortcuts::handle_terminal_event(Event::Key(key), &mut session.keys()).await?;
                continue;
            }
            // One released event per pass, so a submission deferred by one
            // holds back the events behind it.
            if session.deferred_enter.is_none()
                && let Some(event) = paste_burst.next_ready()
            {
                exit = shortcuts::handle_terminal_event(event, &mut session.keys()).await?;
                continue;
            }
            // Held keys have not changed the screen, and a repaint between two
            // reads would space a paste's keys out past the burst window.
            if !paste_burst.is_collecting() {
                session.draw()?;
            }
            if session.mounted_startup.needs_fatal_render() {
                match drain_ready_terminal_events(&mut events)? {
                    ReadyInputDrain::Empty => {
                        let _ = drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
                        session.mounted_startup.arm_fatal_acknowledgment();
                    }
                    ReadyInputDrain::Saturated => continue,
                    ReadyInputDrain::Closed => {
                        exit = true;
                        continue;
                    }
                }
            }
            startup::complete_mounted_startup(
                &mut session.mounted_startup,
                &session.working_directory,
                &mut session.runtime,
                &mut session.active,
                &mut session.state,
                &mut session.controls,
                &mut session.input,
                &mut session.clipboard_images,
            )
            .await?;
            if session.mounted_startup.needs_fatal_render() {
                continue;
            }
            let burst_deadline = paste_burst.deadline();
            tokio::select! {
                signal = interrupt.as_mut() => {
                    signal.map_err(|error| CliError::Terminal(error.to_string()))?;
                    interrupt.set(tokio::signal::ctrl_c());
                    let _ = drain_ready_interrupts(&mut interrupt, tokio::signal::ctrl_c)?;
                    exit = session.handle_interrupt().await;
                }
                event = events.next(), if session.deferred_enter.is_none() => {
                    match event {
                        Some(Ok(Event::Key(key)))
                            if session.mounted_startup.is_awaiting_fatal_key()
                                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            exit = true;
                        }
                        Some(Ok(_)) if session.mounted_startup.is_fatal() => {}
                        Some(Ok(event)) => {
                            paste_burst.push(event, Instant::now());
                            if let Some(error) = drain_ready_into(&mut events, &mut paste_burst) {
                                session.lose_transport(error)?;
                                exit = true;
                            }
                        }
                        Some(Err(error)) => {
                            session.lose_transport(error.to_string())?;
                            exit = true;
                        }
                        None => {
                            session.lose_transport(INPUT_ENDED.to_owned())?;
                            exit = true;
                        }
                    }
                }
                completion = session.clipboard_images.next_completion(),
                    if session.clipboard_images.has_pending_capture() =>
                {
                    if let Some(completion) = completion {
                        session.clipboard_images.apply_completion(
                            completion,
                            session.runtime.as_ref().map(InteractiveRuntime::image_model),
                            &mut session.input,
                            &mut session.state,
                        ).await;
                    }
                }
                completion = ui_operation_receiver.recv() => {
                    let completion = completion.ok_or_else(|| {
                        CliError::Terminal("interactive operation worker stopped".to_owned())
                    })?;
                    apply_ui_operation_completion(
                        completion,
                        &mut session.runtime,
                        &mut session.state,
                    );
                }
                event = session.path_normalization.next_event(),
                    if session.path_normalization.has_pending() =>
                {
                    let event = event.ok_or_else(|| {
                        CliError::Terminal("path normalization worker stopped".to_owned())
                    })?;
                    apply_path_normalization_event(
                        &mut session.path_normalization,
                        &mut session.input,
                        event,
                        &session.working_directory,
                        &mut session.state,
                    )?;
                }
                () = tokio::time::sleep_until(
                    tokio::time::Instant::from_std(burst_deadline.unwrap_or_else(Instant::now)),
                ), if burst_deadline.is_some() => {
                    if let Some(error) = drain_ready_into(&mut events, &mut paste_burst) {
                        session.lose_transport(error)?;
                        exit = true;
                    } else {
                        paste_burst.expire(Instant::now());
                    }
                }
                _ = ticker.tick() => {
                    if let Some(runtime) = session.runtime.as_mut() {
                        switching::apply_pending(runtime, &mut session.input, &mut session.state);
                    }
                }
                _ = peak_ticker.tick(),
                    if session.input.voice_phase() == VoicePhase::Recording =>
                {
                    if let Some(level) = session.runtime.as_ref().map(|runtime| runtime.voice.peak_level()) {
                        let generation = session.input.voice_generation();
                        apply_composer_event(
                            &mut session.input,
                            InputEvent::VoicePeak { generation, level },
                            &session.working_directory,
                            &mut session.state,
                        );
                    }
                }
                _ = voice_ticker.tick() => {
                    let now = vibe_core::clock::now_millis();
                    session.state.banner_cat.advance(now);
                    session.state.expire_inline_notice(now);
                    session.state.animation_frame = session.state.animation_frame.wrapping_add(1);
                    // Only a busy session shows the loading line.
                    if session.state.waiting {
                        session.state.loading.tick();
                    }
                    if session.input.voice_phase() == VoicePhase::Transcribing {
                        apply_composer_event(
                            &mut session.input,
                            InputEvent::VoiceIndicatorTick,
                            &session.working_directory,
                            &mut session.state,
                        );
                    }
                }
                _ = loop_ticker.tick() => session.start_due_loop().await?,
            }
        }
        Ok::<(), CliError>(())
    }
    .await;

    shut_down(session, event_loop, update_check).await
}

/// Reference `_check_and_show_whats_new`: release notes appear once per version,
/// and the version is marked as seen even when no notes ship.
fn announce_release_notes(store: &UpdateCacheStore, state: &mut TuiState) {
    let version = env!("CARGO_PKG_VERSION");
    let cache = store.load();
    if !crate::update_notifier::should_show_whats_new(cache.as_ref(), version) {
        return;
    }
    if let Some(content) = updates::whats_new_content() {
        push_local_notice(state, content, EntryStatus::Completed);
    }
    let seen = crate::update_notifier::mark_version_as_seen(
        cache.as_ref(),
        version,
        vibe_core::clock::now_seconds_signed(),
    );
    store.store(&seen);
}

/// Reference `_show_untrusted_config_warning`: a trusted workspace whose own
/// `.vibe` or `.agents` is declined has that folder's configuration skipped,
/// which is said once per folder so a deliberate decline does not nag.
fn warn_about_untrusted_config(
    vibe_home: &std::path::Path,
    working_directory: &std::path::Path,
    state: &mut TuiState,
) {
    let store = vibe_core::trust::TrustStore::for_vibe_home(vibe_home);
    let dirs = vibe_core::trust::find_untrusted_config_dirs(working_directory, &store)
        .into_iter()
        .map(|directory| directory.display().to_string())
        .collect::<Vec<_>>();
    if !untrusted_config_warning_due(&CacheStore::new(vibe_home), &dirs) {
        return;
    }
    let folders = dirs
        .iter()
        .map(|directory| format!("  • {directory}"))
        .collect::<Vec<_>>()
        .join("\n");
    let message = format!(
        "⚠ Vibe is skipping these local config folders because they are marked untrusted:\n\n\
         {folders}\n\nTo use them, take them out of the \"untrusted\" list in {}, or ask Vibe \
         to do it for you.",
        store.settings_path().display()
    );
    push_local_warning(state, message);
}

/// Reference `_UNTRUSTED_CONFIG_WARNING_SECTION`: the `cache.toml` section
/// recording which untrusted configuration folders were already warned about.
const UNTRUSTED_CONFIG_WARNING_SECTION: &str = "untrusted_config_warning";

/// The bookkeeping of [`warn_about_untrusted_config`]: whether `dirs` holds a
/// folder no earlier launch warned about, in which case all of them are
/// recorded as warned and the caller shows the warning.
fn untrusted_config_warning_due(cache: &CacheStore, dirs: &[String]) -> bool {
    if dirs.is_empty() {
        return false;
    }
    let mut acknowledged = cache
        .read_section(UNTRUSTED_CONFIG_WARNING_SECTION)
        .get("dirs")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    if dirs
        .iter()
        .all(|directory| acknowledged.contains(directory))
    {
        return false;
    }
    acknowledged.extend(dirs.iter().cloned());
    let dirs = acknowledged.into_iter().map(toml::Value::String).collect();
    cache.write_section(
        UNTRUSTED_CONFIG_WARNING_SECTION,
        toml::Table::from_iter([("dirs".to_owned(), toml::Value::Array(dirs))]),
    );
    true
}

mod drain;
mod shutdown;

#[cfg(test)]
#[path = "interactive/interactive_tests.rs"]
mod interactive_tests;
