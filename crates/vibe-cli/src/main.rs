#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use vibe_cli::Arguments;
use vibe_cli::tui::startup::PreparedInvocation;

#[tokio::main]
async fn main() -> ExitCode {
    let invocation = match PreparedInvocation::prepare(Arguments::parse()) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match invocation {
        PreparedInvocation::Interactive(invocation) => {
            let worktree = invocation.workspace.worktree.clone();
            if let Some(worktree) = &worktree {
                eprintln!("Using worktree: {}", worktree.path.display());
            }
            match vibe_cli::tui::run_interactive(invocation).await {
                Ok(exit) => {
                    if exit.session_started
                        && let Some(worktree) = worktree
                        && let Err(error) = worktree.cleanup_terminal()
                    {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "Could not clean up worktree: {error}"
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(error.exit_code())
                }
            }
        }
        PreparedInvocation::Programmatic(invocation) => {
            if let Some(worktree) = &invocation.workspace.worktree {
                eprintln!("Using worktree: {}", worktree.path.display());
            }
            let mut stdout = std::io::stdout().lock();
            let mut stderr = std::io::stderr().lock();
            match vibe_cli::run(invocation.arguments, &mut stdout, &mut stderr).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    ExitCode::from(error.exit_code())
                }
            }
        }
    }
}
