//! What a submitted line means once no slash command has claimed it.
//!
//! Classification happens once, up front, so the executor branches on a closed
//! set of intents crossed with the busy flag instead of an ordered chain of
//! guards over the same catch-all.

use std::path::Path;

use super::chat_input::ChatInputState;
use super::composer::apply_effects as apply_composer_effects;
use super::prompt::{PromptContext, enqueue_prompt, is_user_skill, start_prompt};
use super::remote_project_workflow::handle_teleport_command;
use super::shell::start_shell;
use super::state::TuiState;
use super::{CliError, InteractiveRuntime, teleport_available};

/// Whether a submitted line can run now, and why it cannot when it cannot.
///
/// Reference `on_chat_input_container_submitted` asks the two questions in this
/// order: a paused queue answers first with `_REJECT_HINT_PAUSED`, a running job
/// second with `_REJECT_HINT_BUSY`. The two hints are what tells an operator to
/// wait from what tells them to clear, so the refusal carries the reason rather
/// than collapsing both into one sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Availability {
    Idle,
    Busy,
    QueuePaused,
}

impl Availability {
    /// Reference `_is_busy()` crossed with `_input_queue.paused`: both refuse a
    /// slash command and both queue a prompt, so the paths that only branch on
    /// "not now" read this rather than the reason.
    pub(super) const fn is_busy(self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// What the refusal tells the operator to do about it. Reference
    /// `_REJECT_HINT_BUSY` and `_REJECT_HINT_PAUSED`, written in this port's own
    /// words: `NOTICE` forbids shipping the reference's.
    pub(super) const fn reject_hint(self) -> &'static str {
        match self {
            Self::QueuePaused => "clear the paused queue or remove this input first",
            // A line that reached a refusal was not idle, so the busy hint is
            // what an idle availability would answer with too.
            Self::Idle | Self::Busy => "let the running job finish first",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Submission {
    /// `/name` naming a skill the runtime exposes.
    Skill,
    /// A bare `!` with nothing to run.
    EmptyShell,
    /// `&prompt`, only while the runtime exposes Vibe Code.
    Teleport,
    /// `!command`.
    Shell,
    /// Anything else: a model turn.
    Prompt,
}

#[must_use]
pub(super) fn classify(text: &str, runtime: Option<&InteractiveRuntime>) -> Submission {
    if text.starts_with('/') && is_user_skill(runtime, text) {
        Submission::Skill
    } else if text.trim() == "!" {
        Submission::EmptyShell
    } else if text.starts_with('&') && teleport_available(runtime) {
        Submission::Teleport
    } else if text.trim_start().starts_with('!') {
        Submission::Shell
    } else {
        Submission::Prompt
    }
}

/// Puts a refused submission back in the composer, so nothing the operator typed
/// is lost when the runtime cannot accept it.
pub(super) fn restore_draft(
    input: &mut ChatInputState,
    text: impl Into<String>,
    working_directory: &Path,
    state: &mut TuiState,
) {
    input.replace_text(text);
    let effects = input.refresh_after_adapter_mutation();
    apply_composer_effects(input, effects, working_directory, state);
}

pub(super) async fn execute(
    submitted: String,
    availability: Availability,
    mut context: PromptContext<'_>,
    input: &mut ChatInputState,
) -> Result<(), CliError> {
    let working_directory = context.working_directory;
    let busy = availability.is_busy();
    match (classify(&submitted, context.runtime.as_ref()), busy) {
        (Submission::EmptyShell, busy) => {
            context
                .state
                .push_diagnostic("No command provided after '!'");
            if busy {
                context.state.prompt_queue.resume();
            }
        }
        // Reference `_handle_queue_submit`: a teleport refuses through the same
        // path a slash command refuses through, so it carries the same two
        // reasons rather than a busy-only sentence.
        (Submission::Teleport, true) => {
            restore_draft(input, submitted, working_directory, context.state);
            context.state.push_diagnostic(format!(
                "Teleport cannot be queued: {}",
                availability.reject_hint()
            ));
        }
        (Submission::Teleport, false) => {
            if let Some(runtime) = context.runtime.as_mut() {
                handle_teleport_command(
                    submitted.trim_start_matches('&').trim(),
                    working_directory,
                    runtime,
                    context.state,
                );
            }
        }
        // A shell line queues verbatim: it needs no image preparation and no
        // model turn, so it never goes through prompt preparation.
        (Submission::Shell, true) => {
            let draft = context.clipboard_images.draft(working_directory, submitted);
            context.state.prompt_queue.push(draft);
            context.state.prompt_queue.resume();
            let pending = context.state.prompt_queue.len();
            context
                .state
                .push_diagnostic(format!("Input queued ({pending} pending)"));
        }
        (Submission::Shell, false) => {
            if !start_shell(&submitted, context.runtime, context.state).await? {
                restore_draft(input, submitted, working_directory, context.state);
            }
        }
        (kind, true) => {
            let draft = context.clipboard_images.draft(working_directory, submitted);
            if enqueue_prompt(working_directory, &draft, context.runtime, context.state).await? {
                let label = if kind == Submission::Skill {
                    "Skill"
                } else {
                    "Input"
                };
                let pending = context.state.prompt_queue.len();
                context
                    .state
                    .push_diagnostic(format!("{label} queued ({pending} pending)"));
            } else {
                restore_draft(input, draft.into_text(), working_directory, context.state);
            }
        }
        (_, false) => {
            let draft = context.clipboard_images.draft(working_directory, submitted);
            if !start_prompt(context.reborrow(), &draft).await? {
                restore_draft(input, draft.into_text(), working_directory, context.state);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_ordered_by_specificity_and_survives_leading_whitespace() {
        assert_eq!(classify("!", None), Submission::EmptyShell);
        assert_eq!(classify("  ! ", None), Submission::EmptyShell);
        assert_eq!(classify("!ls -la", None), Submission::Shell);
        assert_eq!(classify("  !ls", None), Submission::Shell);
        // Teleport needs a runtime that exposes Vibe Code, so without one the
        // line stays an ordinary prompt instead of silently disappearing.
        assert_eq!(classify("&ship it", None), Submission::Prompt);
        // A slash line that names no known skill is a prompt, not a command:
        // dispatch already refused it.
        assert_eq!(classify("/unknown", None), Submission::Prompt);
        assert_eq!(classify("hello", None), Submission::Prompt);
    }
}
