//! What the terminal client reports about a session.
//!
//! Every record goes through `InteractiveRuntime::report`, so a session with
//! telemetry disabled and one whose delivery failed are handled in one place
//! rather than at each site.

use std::path::Path;

use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::{Startup, TelemetryCommandKind};

use super::Arguments;
use super::runtime::InteractiveRuntime;

pub(super) fn report_session_opened(
    runtime: &InteractiveRuntime,
    working_directory: &Path,
    arguments: &Arguments,
) {
    let census = TelemetryRecord::NewSession(crate::session_census(
        &runtime.workspace,
        working_directory,
        arguments.trust,
    ));
    // Reference `wait_until_ready`: `vibe.ready` and then `vibe.new_session`
    // once the experiments task settles, so both carry what it resolved.
    report_when_ready(runtime, move || {
        vec![
            TelemetryRecord::Ready {
                init_duration_ms: crate::since_process_start_ms(),
            },
            census,
        ]
    });
}

/// Reference `_complete_post_ready_startup`: once the session is ready, the
/// account and the identity are read side by side, the account's answer
/// reconciles the telemetry census, and its plan title reaches the banner.
pub(super) fn refresh_account_when_ready(
    runtime: &InteractiveRuntime,
) -> Option<tokio::sync::oneshot::Receiver<Option<String>>> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let workspace = runtime.workspace.clone();
    let experiments = runtime.experiments.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    handle.spawn(async move {
        if let Some(experiments) = experiments.as_ref() {
            experiments.settle().await;
        }
        let ((account, lookup), _) =
            tokio::join!(workspace.read_account_lookup(), workspace.read_identity());
        if let Some(experiments) = experiments.as_ref() {
            experiments.apply_account(&lookup).await;
        }
        let plan = account
            .get("plan")
            .and_then(|plan| plan.get("title"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let _ = sender.send(plan);
    });
    Some(receiver)
}

/// Reports what `records` builds once the session's experiments task has
/// settled, beside the interface rather than in front of it. A runtime with
/// no lookup to wait on reports at once.
fn report_when_ready(
    runtime: &InteractiveRuntime,
    records: impl FnOnce() -> Vec<TelemetryRecord> + Send + 'static,
) {
    if let (Some(experiments), Some(telemetry)) =
        (runtime.experiments.clone(), runtime.telemetry.clone())
        && let Ok(handle) = tokio::runtime::Handle::try_current()
    {
        let session_id = runtime.session_id.clone();
        handle.spawn(async move {
            experiments.settle().await;
            for record in records() {
                let _ = telemetry.enqueue(&record, Some(&session_id));
            }
        });
        return;
    }
    for record in records() {
        runtime.report(&record);
    }
}

/// Reference `_send_startup_telemetry_once`: the three durations, once per
/// process, and how the launch was asked to start
/// (`vibe/cli/cli.py:288-299`). The first frame is measured by the caller,
/// where it has just been drawn.
pub(super) fn report_startup(
    runtime: &InteractiveRuntime,
    arguments: &Arguments,
    first_frame_ms: Option<u64>,
) {
    let session_init_duration_ms = runtime.session_init_duration_ms;
    let startup = Startup {
        first_frame_duration_ms: first_frame_ms,
        agent_ready_duration_ms: None,
        session_init_duration_ms,
        has_initial_prompt: arguments
            .initial_prompt
            .as_deref()
            .is_some_and(|prompt| !prompt.is_empty()),
        teleport_on_start: arguments.teleport,
        show_resume_picker: arguments.resume.as_deref() == Some(""),
        is_resuming_session: arguments.continue_session
            || arguments.resume.as_deref().is_some_and(|id| !id.is_empty()),
        prompt_for_workspace_trust: !(arguments.trust || arguments.worktree.is_some()),
        // Reference `_is_cold_start` answers nothing for a frozen build, and a
        // compiled binary has no bytecode cache to compare against.
        is_cold_start: None,
        harness_selection_source: Some(
            vibe_app_server::harness::HarnessSelection::resolve(
                arguments.experimental_harness,
                arguments.legacy_harness,
            )
            .source
            .as_str()
            .to_owned(),
        ),
    };
    // Reference `_show_post_init_notices_once` sends it once the agent is
    // ready, which is what the second duration measures.
    report_when_ready(runtime, move || {
        vec![TelemetryRecord::Startup(Startup {
            agent_ready_duration_ms: Some(crate::since_process_start_ms()),
            ..startup
        })]
    });
}

/// Reference `_handle_command` and `_send_skill_telemetry`: one event, whose
/// type tells a built-in command from a skill invocation.
pub(super) fn report_slash_command(
    runtime: &InteractiveRuntime,
    command_line: &str,
    kind: TelemetryCommandKind,
) {
    let Some(command) = command_line.split_whitespace().next() else {
        return;
    };
    runtime.report(&TelemetryRecord::SlashCommandUsed {
        command: command.to_owned(),
        kind,
    });
}

/// Reference `action_toggle_voice_mode`.
pub(super) fn report_voice_mode_toggled(runtime: &InteractiveRuntime, enabled: bool) {
    runtime.report(&TelemetryRecord::VoiceModeToggled { enabled });
}

/// Reference `send_user_copied_text`, which the pinned reference publishes on
/// its client without a live call site. The copy shortcut is where this port
/// raises it, and the text itself never travels: only its length does.
pub(super) fn report_copied_text(runtime: Option<&InteractiveRuntime>, copied: &str) {
    if let Some(runtime) = runtime {
        runtime.report(&TelemetryRecord::UserCopiedText {
            text_length: copied.chars().count() as u64,
        });
    }
}

/// Reference `vibe.user_cancelled_action`, raised at the three sites the
/// reference raises it: an interrupted agent, a refused approval and a
/// cancelled question.
pub(super) fn report_cancelled_action(
    runtime: Option<&InteractiveRuntime>,
    action: CancelledAction,
) {
    if let Some(runtime) = runtime {
        runtime.report(&TelemetryRecord::UserCancelledAction {
            action: action.label().to_owned(),
            outcome: None,
        });
    }
}

/// The three actions the reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancelledAction {
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
