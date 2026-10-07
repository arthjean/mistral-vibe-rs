use std::path::Path;

use crate::CliError;

use super::super::chat_input::ChatInputState;
use super::super::clipboard_images::ClipboardImageManager;
use super::super::composer::apply_effects as apply_composer_effects;
use super::super::controls::ControlState;
use super::super::prompt::{PromptContext, start_prompt};
use super::super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use super::super::{ActiveTurn, InteractiveRuntime, push_local_notice, start_teleport};
use super::PostMountAction;
use vibe_core::events::PublicNoticeLevel;

pub(in crate::tui) enum MountedStartup {
    Pending(Option<PostMountAction>),
    Ready,
    FatalPendingRender(CliError),
    FatalAwaitingKey(CliError),
}

impl MountedStartup {
    pub(in crate::tui) const fn new(action: Option<PostMountAction>) -> Self {
        Self::Pending(action)
    }

    pub(in crate::tui) const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::FatalPendingRender(_) | Self::FatalAwaitingKey(_)
        )
    }

    pub(in crate::tui) const fn is_awaiting_fatal_key(&self) -> bool {
        matches!(self, Self::FatalAwaitingKey(_))
    }

    pub(in crate::tui) const fn needs_fatal_render(&self) -> bool {
        matches!(self, Self::FatalPendingRender(_))
    }

    pub(in crate::tui) fn arm_fatal_acknowledgment(&mut self) {
        let current = std::mem::replace(self, Self::Ready);
        *self = match current {
            Self::FatalPendingRender(error) => Self::FatalAwaitingKey(error),
            current => current,
        };
    }

    pub(in crate::tui) fn into_initialization_error(self) -> Option<CliError> {
        match self {
            Self::FatalPendingRender(error) | Self::FatalAwaitingKey(error) => Some(error),
            Self::Pending(_) | Self::Ready => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::tui) async fn complete_mounted_startup(
    startup: &mut MountedStartup,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    input: &mut ChatInputState,
    clipboard_images: &mut ClipboardImageManager,
) -> Result<(), CliError> {
    let action = match std::mem::replace(startup, MountedStartup::Ready) {
        MountedStartup::Pending(action) => action,
        current @ (MountedStartup::Ready
        | MountedStartup::FatalPendingRender(_)
        | MountedStartup::FatalAwaitingKey(_)) => {
            *startup = current;
            return Ok(());
        }
    };

    let initialization = if let Some(runtime) = runtime.as_mut() {
        let session_id = runtime.session_id.clone();
        runtime
            .service
            .initialize_pending_mcp(&session_id)
            .await
            .map_err(CliError::from)
    } else {
        Ok(Vec::new())
    };
    state.waiting = false;
    // Reference `_mount_after_session_ready`.
    let effect = state.notifier.clear_waiting();
    state.attend(effect);
    if !record_initialization(startup, state, initialization) {
        return Ok(());
    }
    if runtime
        .as_ref()
        .is_some_and(|runtime| runtime.experimental_harness)
    {
        push_unified_harness_notice(state);
    }

    match action {
        Some(PostMountAction::Prompt(prompt)) => {
            dispatch_initial_prompt(
                prompt,
                working_directory,
                runtime,
                active,
                state,
                controls,
                input,
                clipboard_images,
            )
            .await?;
        }
        Some(PostMountAction::Teleport(prompt)) => {
            if let Some(runtime) = runtime.as_mut() {
                start_teleport(prompt.as_deref(), working_directory, runtime, state);
            } else {
                state.push_diagnostic(
                    "Startup Teleport could not start because setup is incomplete",
                );
            }
        }
        None => {}
    }
    Ok(())
}

/// Reference `_show_unified_harness_notice`, the last of the notices the
/// session's readiness mounts: a session on the unified mode is told how to
/// leave it.
pub(in crate::tui) const UNIFIED_HARNESS_NOTICE: &str = "You are using our new unified harness. If you encounter issues, restart with --legacy-harness.";

fn push_unified_harness_notice(state: &mut TuiState) {
    state.append_local(TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: UNIFIED_HARNESS_NOTICE.to_owned(),
        status: EntryStatus::Completed,
        source: EntrySource::notice(PublicNoticeLevel::Warning),
    });
}

fn record_initialization(
    startup: &mut MountedStartup,
    state: &mut TuiState,
    initialization: Result<Vec<String>, CliError>,
) -> bool {
    match initialization {
        Ok(diagnostics) => {
            for diagnostic in diagnostics {
                push_local_notice(
                    state,
                    &format!("MCP server failed to connect: {diagnostic}"),
                    EntryStatus::Failed,
                );
            }
            true
        }
        Err(error) => {
            push_local_notice(
                state,
                &format!("Background initialization failed: {error}"),
                EntryStatus::Failed,
            );
            push_local_notice(state, "Press any key to exit", EntryStatus::Completed);
            *startup = MountedStartup::FatalPendingRender(error);
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_initial_prompt(
    prompt: String,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    input: &mut ChatInputState,
    clipboard_images: &mut ClipboardImageManager,
) -> Result<(), CliError> {
    if prompt.trim().is_empty() {
        state.push_diagnostic("Initial prompt is empty; no turn was submitted");
        return Ok(());
    }
    if runtime.is_none() {
        state.push_diagnostic("Initial prompt could not start because setup is incomplete");
        return Ok(());
    }
    let draft = clipboard_images.draft(working_directory, prompt);
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
        state.push_diagnostic("Initial prompt submission failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::state::TuiState;
    use super::*;

    #[test]
    fn fatal_initialization_remains_typed_after_visible_failure() {
        let mut startup = MountedStartup::new(None);
        let mut state = TuiState::new("fatal-startup");
        assert!(!record_initialization(
            &mut startup,
            &mut state,
            Err(CliError::Terminal("host initialization failed".to_owned())),
        ));
        assert!(startup.is_fatal());
        assert!(!startup.is_awaiting_fatal_key());
        assert!(state.entries.iter().any(|entry| {
            entry.text.contains("Background initialization failed")
                && entry.status == EntryStatus::Failed
        }));
        assert!(
            state
                .entries
                .iter()
                .any(|entry| entry.text == "Press any key to exit")
        );
        startup.arm_fatal_acknowledgment();
        assert!(startup.is_awaiting_fatal_key());
        assert!(matches!(
            startup.into_initialization_error(),
            Some(CliError::Terminal(_))
        ));
    }

    /// Reference `_show_unified_harness_notice`: a borderless warning.
    #[test]
    fn the_unified_harness_notice_is_a_warning_naming_the_way_back() {
        let mut state = TuiState::new("unified-startup");
        push_unified_harness_notice(&mut state);
        let entry = state.entries.last().expect("the notice is mounted");
        assert_eq!(
            entry.text,
            "You are using our new unified harness. If you encounter issues, restart with --legacy-harness."
        );
        assert_eq!(
            entry.source,
            EntrySource::notice(PublicNoticeLevel::Warning)
        );
    }
}
