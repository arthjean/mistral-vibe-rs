//! What a submitted line is, and what happens to it given what is running.
//!
//! Classification happens once, up front, and routing crosses that closed set
//! of intents with what occupies the session, so the executor branches on one
//! decision instead of an ordered chain of guards over the same catch-all.

use std::path::Path;

use super::chat_input::ChatInputState;
use super::commands::{CommandContext, CommandId, definition, parse_command_in};
use super::composer::apply_effects as apply_composer_effects;
use super::prompt::{PromptContext, enqueue_prompt, is_user_skill, start_prompt};
use super::remote_project_workflow::handle_teleport_command;
use super::shell::start_shell;
use super::state::TuiState;
use super::{CliError, InteractiveRuntime};

/// What a stripped submission is. Reference `classify`
/// (`vibe/cli/textual_ui/widgets/chat_input/input_kinds.py`), whose order is the
/// contract: `&` is a teleport whenever the registry carries `teleport`, a line
/// the registry parses is a command before it can be a skill, and only a slash
/// line that is no command is looked up as a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Submission {
    /// A line the registry parses, and whether it runs on the side channel.
    Command { side_channel: bool },
    /// `&target`, with the target exactly as typed after the `&`.
    Teleport(String),
    /// `/name` naming a skill the runtime exposes.
    Skill,
    /// `!command`, with the command exactly as typed after the `!`.
    Shell(String),
    /// A bare `!` with nothing to run.
    EmptyShell,
    /// Anything else: a model turn.
    Prompt,
}

#[must_use]
pub(super) fn classify(
    value: &str,
    context: &CommandContext,
    runtime: Option<&InteractiveRuntime>,
) -> Submission {
    if let Some(target) = value.strip_prefix('&')
        && context.is_available(CommandId::Teleport)
    {
        return Submission::Teleport(target.to_owned());
    }
    if let Some(parsed) = parse_command_in(value, context) {
        return Submission::Command {
            side_channel: definition(parsed.id).is_some_and(|command| command.side_channel),
        };
    }
    if value.starts_with('/') && is_user_skill(runtime, value) {
        return Submission::Skill;
    }
    match value.strip_prefix('!') {
        Some("") => Submission::EmptyShell,
        Some(command) => Submission::Shell(command.to_owned()),
        None => Submission::Prompt,
    }
}

/// What occupies the session when a line is submitted. Reference `_is_busy`
/// counts a running shell as busy, and `_handle_queue_submit` asks about the
/// shell before anything else, so the two are carried apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Occupancy {
    pub turn: bool,
    pub shell: bool,
    pub paused: bool,
}

/// Why a submission was refused. Reference `_warn_not_queueable` callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    SlashCommand,
    Shell,
    Teleport,
    ShellRunning,
    SideChannelBusy,
}

/// Which remedy a refusal names. Reference `_REJECT_HINT_BUSY` and
/// `_REJECT_HINT_PAUSED`; the side-channel refusal names neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hint {
    Busy,
    Paused,
    None,
}

/// What the dispatcher does with one classified submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Route {
    /// Run the parsed command now.
    Command,
    Teleport(String),
    Skill,
    Shell(String),
    Prompt,
    /// Report the bare `!`, and put the line back when it was not idle.
    EmptyShell {
        restore: bool,
    },
    /// Queue a model turn, and resume the queue when it was paused.
    Queue {
        skill: bool,
        resume: bool,
    },
    /// Refuse the line and put it back in the composer.
    Refuse {
        reason: Refusal,
        hint: Hint,
    },
}

/// Reference `_dispatch_submitted_value`, `_try_side_channel_command`,
/// `_handle_paused_submit` and `_handle_queue_submit`, as one decision.
///
/// A paused queue answers before a running job. A side-channel command runs
/// whatever is running unless another side-channel command still holds the
/// channel. A running shell refuses everything else, and otherwise a command,
/// a shell line and a teleport are refused while a model turn queues.
#[must_use]
pub(super) fn route(
    submission: Submission,
    occupancy: Occupancy,
    side_channel_free: bool,
) -> Route {
    let busy = occupancy.turn || occupancy.shell;
    if !occupancy.paused && !busy {
        return match submission {
            Submission::Command { .. } => Route::Command,
            Submission::Teleport(target) => Route::Teleport(target),
            Submission::Skill => Route::Skill,
            Submission::Shell(command) => Route::Shell(command),
            Submission::EmptyShell => Route::EmptyShell { restore: false },
            Submission::Prompt => Route::Prompt,
        };
    }
    if let Submission::Command { side_channel: true } = submission {
        return if side_channel_free {
            Route::Command
        } else {
            Route::Refuse {
                reason: Refusal::SideChannelBusy,
                hint: Hint::None,
            }
        };
    }
    let hint = if occupancy.paused {
        Hint::Paused
    } else {
        Hint::Busy
    };
    let refuse = |reason| Route::Refuse { reason, hint };
    if occupancy.shell {
        return refuse(Refusal::ShellRunning);
    }
    match submission {
        Submission::Command { .. } => refuse(Refusal::SlashCommand),
        Submission::Teleport(_) => refuse(Refusal::Teleport),
        Submission::Shell(_) => refuse(Refusal::Shell),
        Submission::EmptyShell => Route::EmptyShell { restore: true },
        Submission::Skill => Route::Queue {
            skill: true,
            resume: occupancy.paused,
        },
        Submission::Prompt => Route::Queue {
            skill: false,
            resume: occupancy.paused,
        },
    }
}

/// The sentence a refusal shows. The reference's are authored prose `NOTICE`
/// forbids shipping, so these are this port's own; what they refuse and which
/// remedy they name is what the dispatch corpus compares.
#[must_use]
pub(super) fn refusal_message(reason: Refusal, hint: Hint) -> String {
    let subject = match reason {
        Refusal::SlashCommand => "Slash commands cannot be queued",
        Refusal::Shell => "Shell commands cannot be queued",
        Refusal::Teleport => "Teleport cannot be queued",
        Refusal::ShellRunning => "Nothing can be queued while a shell command runs",
        Refusal::SideChannelBusy => {
            return "Another slash command is still running; try again once it finishes".to_owned();
        }
    };
    let remedy = match hint {
        Hint::Paused => "clear the paused queue or remove this input first",
        Hint::Busy | Hint::None => "let the running job finish first",
    };
    format!("{subject}: {remedy}")
}

/// Reference `_empty_bash_error`.
pub(super) const EMPTY_SHELL_ERROR: &str = "No command provided after '!'";

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

/// Runs a route that is not a command. The caller has already run a command
/// route through the handlers, which own their own echo and effects.
pub(super) async fn execute(
    submitted: String,
    route: Route,
    mut context: PromptContext<'_>,
    input: &mut ChatInputState,
) -> Result<(), CliError> {
    let working_directory = context.working_directory;
    match route {
        Route::Command => {}
        Route::Refuse { reason, hint } => {
            context.state.push_diagnostic(refusal_message(reason, hint));
            restore_draft(input, submitted, working_directory, context.state);
        }
        // Reference `_empty_bash_error`: the error alone, and the line goes back
        // to the composer only when it was refused rather than run.
        Route::EmptyShell { restore } => {
            context.state.push_diagnostic(EMPTY_SHELL_ERROR);
            if restore {
                restore_draft(input, submitted, working_directory, context.state);
            }
        }
        Route::Teleport(target) => {
            if let Some(runtime) = context.runtime.as_mut() {
                handle_teleport_command(Some(&target), working_directory, runtime, context.state);
            }
        }
        Route::Shell(_) => {
            if !start_shell(&submitted, context.runtime, context.state).await? {
                restore_draft(input, submitted, working_directory, context.state);
            }
        }
        Route::Queue { skill, resume } => {
            let draft = context.clipboard_images.draft(working_directory, submitted);
            if enqueue_prompt(working_directory, &draft, context.runtime, context.state).await? {
                let label = if skill { "Skill" } else { "Input" };
                let pending = context.state.prompt_queue.len();
                context
                    .state
                    .push_diagnostic(format!("{label} queued ({pending} pending)"));
                if resume {
                    context.state.prompt_queue.resume();
                }
            } else {
                restore_draft(input, draft.into_text(), working_directory, context.state);
            }
        }
        Route::Skill | Route::Prompt => {
            let draft = context.clipboard_images.draft(working_directory, submitted);
            if !start_prompt(context.reborrow(), &draft).await? {
                restore_draft(input, draft.into_text(), working_directory, context.state);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "submission_tests.rs"]
mod submission_tests;
