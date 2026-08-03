use std::path::Path;

use crate::CliError;

use super::super::chat_input::ChatInputState;
use super::super::clipboard_images::ClipboardImageManager;
use super::super::composer::apply_effects as apply_composer_effects;
use super::super::controls::ControlState;
use super::super::prompt::{PromptContext, start_prompt};
use super::super::state::{EntryStatus, TuiState};
use super::super::{ActiveTurn, InteractiveRuntime, push_local_notice, start_teleport};
use super::PostMountAction;

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

    pub(in crate::tui) fn arm_fatal_acknowledgement(&mut self) {
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
    if !record_initialization(startup, state, initialization) {
        return Ok(());
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
                if runtime.vibe_code_enabled {
                    start_teleport(prompt.as_deref(), working_directory, runtime, state);
                } else {
                    state.push_diagnostic(
                        "Startup Teleport is unavailable in the active configuration",
                    );
                }
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
        startup.arm_fatal_acknowledgement();
        assert!(startup.is_awaiting_fatal_key());
        assert!(matches!(
            startup.into_initialization_error(),
            Some(CliError::Terminal(_))
        ));
    }
}
