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
    runtime.report(&TelemetryRecord::NewSession(crate::session_census(
        &runtime.release3,
        working_directory,
        arguments.trust,
    )));
    runtime.report(&TelemetryRecord::Ready {
        init_duration_ms: crate::since_process_start_ms(),
    });
}

/// Reference `_send_startup_telemetry_once`: the three durations, once per
/// process, taken where the first frame has just been drawn.
pub(super) fn report_startup(runtime: &InteractiveRuntime) {
    let elapsed = crate::since_process_start_ms();
    runtime.report(&TelemetryRecord::Startup(Startup {
        first_frame_duration_ms: Some(elapsed),
        agent_ready_duration_ms: Some(elapsed),
        session_init_duration_ms: runtime.session_init_duration_ms,
    }));
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
