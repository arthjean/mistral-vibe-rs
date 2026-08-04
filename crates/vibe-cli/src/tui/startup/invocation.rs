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
    pub fn prepare(mut arguments: Arguments) -> Result<Self, StartupError> {
        // The reference resolves forced update discovery before it reads stdin,
        // prepares a workspace, or loads any project configuration.
        if InvocationIntent::from_arguments(&arguments).route == InvocationRoute::CheckUpgrade {
            return Ok(Self::CheckUpgrade(Box::new(arguments)));
        }
        populate_piped_prompt(&mut arguments)?;
        let workspace = LaunchWorkspace::prepare(&mut arguments)?;
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

    #[must_use]
    pub const fn route(&self) -> InvocationRoute {
        match self {
            Self::Interactive(_) => InvocationRoute::Interactive,
            Self::Programmatic(_) => InvocationRoute::Programmatic,
            Self::CheckUpgrade(_) => InvocationRoute::CheckUpgrade,
        }
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
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use serde::Deserialize;

    use super::*;

    const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct StartupCorpus {
        schema_version: u32,
        reference: Reference,
        traces: Vec<StartupTrace>,
        unavailable: Vec<UnavailableTrace>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Reference {
        commit: String,
        version: String,
        source_files: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct StartupTrace {
        id: String,
        story: String,
        reference_commit: String,
        arguments: TraceArguments,
        expected: TraceExpectation,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct TraceArguments {
        initial_prompt: Option<String>,
        prompt: Option<String>,
        resume: Option<String>,
        continue_session: bool,
        worktree: Option<String>,
        teleport: bool,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct TraceExpectation {
        route: InvocationRoute,
        post_mount_action: Option<PostMountAction>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct UnavailableTrace {
        id: String,
        reason: String,
    }

    #[test]
    fn pinned_startup_intents_drive_the_executed_invocation_variant() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/runtime-parity/startup-ep006.json");
        let text = fs::read_to_string(&path).expect("startup corpus");
        let corpus: StartupCorpus = serde_json::from_str(&text).expect("strict startup corpus");
        assert_eq!(corpus.schema_version, 1);
        assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
        assert_eq!(corpus.reference.commit.len(), 40);
        assert!(!corpus.reference.version.is_empty());
        assert!(!corpus.reference.source_files.is_empty());
        for unavailable in &corpus.unavailable {
            assert!(
                !unavailable.id.trim().is_empty() && !unavailable.reason.trim().is_empty(),
                "unexplained unavailable startup trace"
            );
        }
        let mut ids = BTreeSet::new();
        for trace in corpus.traces {
            assert!(ids.insert(trace.id.clone()), "duplicate trace {}", trace.id);
            assert!(
                trace.story.starts_with("US-"),
                "invalid story on {}",
                trace.id
            );
            assert_eq!(trace.reference_commit, corpus.reference.commit);
            let mut arguments = crate::arguments_for_test();
            arguments.initial_prompt = trace.arguments.initial_prompt;
            arguments.prompt = trace.arguments.prompt;
            arguments.resume = trace.arguments.resume;
            arguments.continue_session = trace.arguments.continue_session;
            arguments.worktree = trace.arguments.worktree;
            arguments.teleport = trace.arguments.teleport;
            let intent = InvocationIntent::from_arguments(&arguments);
            assert_eq!(intent.route, trace.expected.route, "route in {}", trace.id);
            assert_eq!(
                intent.post_mount_action, trace.expected.post_mount_action,
                "post-mount action in {}",
                trace.id
            );
        }
    }

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
