pub mod attachments;
pub mod attention;
mod callback;
pub mod chat_input;
#[cfg(test)]
mod chat_input_parity_tests;
pub mod clipboard;
mod clipboard_images;
mod cloud_workflow;
pub mod commands;
#[cfg(test)]
mod commands_parity_tests;
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
mod interactive;
pub mod narration;
pub mod narrator;
pub mod onboarding;
#[cfg(test)]
mod onboarding_parity_tests;
mod path_mentions;
mod path_normalization;
mod path_resources;
pub mod pickers;
mod plan_review;
#[cfg(test)]
mod promo_parity_tests;
mod prompt;
mod queue;
mod remote_project_workflow;
pub mod render;
pub mod rewind;
mod runtime;
#[cfg(test)]
mod runtime_parity_tests;
mod session;
mod session_picker;
pub mod setup;
mod shell;
mod shortcuts;
pub mod startup;
pub mod state;
mod submission;
mod switching;
mod telemetry;
mod teleport;
pub mod terminal;
pub mod themes;
pub mod transcript;
pub mod transcript_view;
#[cfg(test)]
mod tui_parity_tests;
mod turn;
pub mod updates;
mod voice;
mod workflow;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, LiveTurnDriver, PublicDispatch};

use self::chat_input::{ChatInputState, InputEvent, Safety};
use self::composer::apply_event as apply_composer_event;
use self::hydration::{
    adopt_hydrated_session, history_entry, metadata_session_id, page_older_history,
    resync_current_projection,
};
use self::narration::apply_narrator_effect;
use self::path_normalization::PathNormalizationManager;
use self::remote_project_workflow::start_teleport;
use self::runtime::{InteractiveRuntime, RuntimeSkill, UiOperation, teleport_available};
use self::session::{parse_runtime_skills, refresh_server_banner_metrics};
use self::setup::{
    EnvironmentThemeDetector, ResolvedTheme, TerminalThemeDetector, Theme, resolve_theme,
};
use self::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use self::telemetry::{
    CancelledAction, report_cancelled_action, report_copied_text, report_slash_command,
    report_voice_mode_toggled,
};
use self::teleport::{record_teleport_progress, teleport_event_message};
use self::terminal::{CrosstermOps, TerminalGuard};
use self::turn::{
    ActiveTurn, request_active_turn_interrupt, settle_unstarted_reservation, start_active_turn,
};
use crate::{Arguments, CliError, CliTelemetryObserver, bootstrap, telemetry_observer};
use vibe_app_server::client::PublicNoticeLevel;
use vibe_core::clock::{now_millis as unix_millis, now_seconds as unix_seconds};

pub use interactive::{InteractiveExit, run_interactive};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const INITIAL_HISTORY_LIMIT: usize = 200;
const DEFAULT_MODEL: &str = "mistral-medium-3.5";
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

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
    runtime
        .published_config()?
        .get("theme")
        .and_then(Value::as_str)
        .and_then(themes::theme_polarity)
}

/// Reference `ThemePreviewed`: a highlighted theme applies before acceptance and
/// is never persisted.
pub(in crate::tui) fn preview_theme(value: &str, theme: &mut ResolvedTheme) {
    if let Some(preference) = themes::theme_polarity(value) {
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
    let Some(preference) = themes::theme_polarity(value) else {
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
                Ok((message, status)) => {
                    push_local_notice(state, &message, status);
                }
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
    clipboard::copy_and_report(state, "Selection", &selection);
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
/// Appends a notice this client wrote itself and answers the transcript id it
/// was filed under, which is what a later settlement addresses it by.
fn push_local_notice(state: &mut TuiState, message: &str, status: EntryStatus) -> String {
    state.append_local(TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: message.to_owned(),
        status,
        source: EntrySource::notice(PublicNoticeLevel::Info),
    })
}

#[cfg(test)]
mod tests {
    use vibe_app_server::workspace::WorkspaceService;

    use super::*;

    /// US-008: the session census is read off the services a session is built
    /// from rather than from the banner.
    #[test]
    fn the_session_census_counts_what_the_configuration_declares() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("the workspace is created");
        std::fs::write(workspace.join("AGENTS.md"), "instructions")
            .expect("the instructions file is written");
        let workspace_service = WorkspaceService::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            workspace.clone(),
        );

        let census = crate::session_census(&workspace_service, &workspace, true);

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
