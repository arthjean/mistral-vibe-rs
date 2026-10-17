//! The shell tool tells the model which shell reads its commands.

use vibe_core::tools::shell::{CommandEnvironment, HostShells};

use super::model_tools;

fn bash_description(environment: CommandEnvironment) -> String {
    model_tools(environment)
        .into_iter()
        .find(|tool| tool.name == "bash")
        .map(|tool| tool.description)
        .expect("the shell tool is declared")
}

/// US-015: on Windows the description names the family the session resolved,
/// and a Git Bash command runs as `bash -c`, not as a login shell.
#[test]
fn the_shell_tool_names_the_family_the_session_runs() {
    let git_bash = bash_description(CommandEnvironment::GitBash);
    assert!(git_bash.contains("Git Bash"), "{git_bash}");
    assert!(git_bash.contains("`bash -c`"), "{git_bash}");
    assert!(!git_bash.contains("login shell"), "{git_bash}");
    let powershell = bash_description(CommandEnvironment::PowerShell);
    assert!(powershell.contains("PowerShell"), "{powershell}");
    assert!(!powershell.contains("Git Bash"), "{powershell}");
    let unix = bash_description(CommandEnvironment::Unix);
    assert!(unix.contains("login shell"), "{unix}");
    for description in [git_bash, powershell, unix] {
        assert!(!description.contains("  "), "{description:?}");
    }
}

/// The family the description names is the one the shell tool and `process.*`
/// run under on this host.
#[test]
fn the_described_family_is_the_one_the_host_resolves() {
    let host = HostShells::detect();
    let environment = host.command_environment();
    let resolved = host.command_shell(None, &std::env::vars().collect());
    let description = bash_description(environment);
    match environment {
        CommandEnvironment::GitBash => {
            assert!(description.contains("Git Bash"));
            assert_eq!(
                resolved.map(|shell| shell.arguments),
                Ok(vec!["-c".to_owned()])
            );
        }
        CommandEnvironment::PowerShell => {
            assert!(description.contains("PowerShell"));
            assert_eq!(
                resolved.map(|shell| shell.arguments),
                Ok(vec![
                    "-NoLogo".to_owned(),
                    "-NoProfile".to_owned(),
                    "-Command".to_owned()
                ])
            );
        }
        CommandEnvironment::Unix => {
            assert!(description.contains("login shell"));
            assert_eq!(
                resolved.map(|shell| shell.arguments),
                Ok(vec!["-lc".to_owned()])
            );
        }
    }
}
