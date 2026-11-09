use std::io::{IsTerminal, Read};

use serde::{Deserialize, Serialize};

use crate::Arguments;

use super::{LaunchWorkspace, StartupError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InvocationRoute {
    Interactive,
    Programmatic,
    /// Reference `run_cli`: forced update discovery replaces the session.
    CheckUpgrade,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", content = "prompt", rename_all = "camelCase")]
pub enum PostMountAction {
    Prompt(String),
    Teleport(Option<String>),
}

pub struct InteractiveInvocation {
    pub arguments: Arguments,
    pub workspace: LaunchWorkspace,
    pub post_mount_action: Option<PostMountAction>,
}

pub struct ProgrammaticInvocation {
    pub arguments: Arguments,
    pub workspace: LaunchWorkspace,
}

pub enum PreparedInvocation {
    Interactive(InteractiveInvocation),
    Programmatic(ProgrammaticInvocation),
    CheckUpgrade(Box<Arguments>),
}

impl PreparedInvocation {
    /// Routes an invocation, narrating worktree preparation as it goes.
    ///
    /// The writer is the caller's stderr in production: the reference prints
    /// both worktree lines from `main()` itself, before the route is known
    /// (`vibe/cli/entrypoint.py:291-304`).
    pub fn prepare(
        mut arguments: Arguments,
        narration: &mut impl std::io::Write,
    ) -> Result<Self, StartupError> {
        // The reference validates `--workdir` and `--add-dir` for every route,
        // skipping only the worktree, and resolves forced update discovery
        // before it reads stdin or opens a session
        // (`vibe/cli/entrypoint.py:422-456`, `vibe/cli/cli.py:433-439`).
        if InvocationIntent::from_arguments(&arguments).route == InvocationRoute::CheckUpgrade {
            LaunchWorkspace::prepare(&mut arguments, narration)?;
            return Ok(Self::CheckUpgrade(Box::new(arguments)));
        }
        // The worktree is entered before stdin is read, so a piped prompt
        // never names an unnamed one (`vibe/cli/entrypoint.py:432-436`).
        let workspace = LaunchWorkspace::prepare(&mut arguments, narration)?;
        if let Err(error) = populate_piped_prompt(&mut arguments) {
            workspace.release_holder();
            return Err(error);
        }
        let intent = InvocationIntent::from_arguments(&arguments);
        Ok(match intent.route {
            InvocationRoute::Programmatic => Self::Programmatic(ProgrammaticInvocation {
                arguments,
                workspace,
            }),
            InvocationRoute::Interactive | InvocationRoute::CheckUpgrade => {
                Self::Interactive(InteractiveInvocation {
                    arguments,
                    workspace,
                    post_mount_action: intent.post_mount_action,
                })
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InvocationIntent {
    route: InvocationRoute,
    post_mount_action: Option<PostMountAction>,
}

impl InvocationIntent {
    fn from_arguments(arguments: &Arguments) -> Self {
        // Reference `run_cli` runs onboarding before update discovery.
        if arguments.check_upgrade && !arguments.setup {
            return Self {
                route: InvocationRoute::CheckUpgrade,
                post_mount_action: None,
            };
        }
        if arguments.prompt.is_some() {
            return Self {
                route: InvocationRoute::Programmatic,
                post_mount_action: None,
            };
        }
        let post_mount_action = if arguments.teleport {
            Some(PostMountAction::Teleport(arguments.initial_prompt.clone()))
        } else {
            arguments
                .initial_prompt
                .clone()
                .map(PostMountAction::Prompt)
        };
        Self {
            route: InvocationRoute::Interactive,
            post_mount_action,
        }
    }
}

fn populate_piped_prompt(arguments: &mut Arguments) -> Result<(), StartupError> {
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    let mut content = String::new();
    std::io::stdin()
        .read_to_string(&mut content)
        .map_err(StartupError::Stdin)?;
    apply_piped_prompt(arguments, &content);
    Ok(())
}

fn apply_piped_prompt(arguments: &mut Arguments, content: &str) {
    let content = content.trim();
    if content.is_empty() {
        return;
    }
    if arguments.prompt.as_deref() == Some("") {
        arguments.prompt = Some(content.to_owned());
    } else if arguments.prompt.is_none() && arguments.initial_prompt.is_none() {
        arguments.initial_prompt = Some(content.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_prompt_preserves_programmatic_and_positional_precedence() {
        let mut interactive = crate::arguments_for_test();
        apply_piped_prompt(&mut interactive, "  piped prompt\n");
        assert_eq!(interactive.initial_prompt.as_deref(), Some("piped prompt"));
        assert_eq!(
            InvocationIntent::from_arguments(&interactive).route,
            InvocationRoute::Interactive
        );

        let mut programmatic = crate::arguments_for_test();
        programmatic.prompt = Some(String::new());
        apply_piped_prompt(&mut programmatic, "piped prompt");
        assert_eq!(programmatic.prompt.as_deref(), Some("piped prompt"));
        assert_eq!(
            InvocationIntent::from_arguments(&programmatic).route,
            InvocationRoute::Programmatic
        );

        let mut positional = crate::arguments_for_test();
        positional.initial_prompt = Some("positional".to_owned());
        apply_piped_prompt(&mut positional, "piped prompt");
        assert_eq!(positional.initial_prompt.as_deref(), Some("positional"));
    }
}
