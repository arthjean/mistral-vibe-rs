#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use vibe_cli::Arguments;
use vibe_cli::tui::startup::PreparedInvocation;

#[tokio::main]
async fn main() -> ExitCode {
    // Reference `PROCESS_START_MONOTONIC`, read at import time: the startup
    // durations are measured from here, so the reading is taken before any
    // work rather than at the first event that reports it.
    vibe_cli::mark_process_start();
    // `vibe mcp` is a command of its own upstream, decided before the
    // interactive parser runs so that `mcp` is never read as a prompt.
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if vibe_cli::mcp_command::intercepts(&arguments) {
        let mut stdout = std::io::stdout().lock();
        return match vibe_cli::mcp_command::run(&arguments[1..], &mut stdout) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    let invocation = match PreparedInvocation::prepare(Arguments::parse()) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match invocation {
        PreparedInvocation::CheckUpgrade(arguments) => {
            let mut stdout = std::io::stdout().lock();
            match vibe_cli::tui::startup::run_check_upgrade(
                &arguments,
                env!("CARGO_PKG_VERSION"),
                &mut stdout,
            )
            .await
            {
                Ok(false) => ExitCode::SUCCESS,
                Ok(true) => ExitCode::FAILURE,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        PreparedInvocation::Interactive(invocation) => {
            let worktree = invocation.workspace.worktree.clone();
            if let Some(worktree) = &worktree {
                eprintln!("Using worktree: {}", worktree.path.display());
            }
            match vibe_cli::tui::run_interactive(invocation).await {
                Ok(exit) => {
                    let session_started = exit.session_started;
                    let initialization_error = exit.initialization_error;
                    if let Some(summary) = &exit.summary {
                        let mut stdout = std::io::stdout().lock();
                        for line in vibe_cli::tui::exit::session_resume_lines(summary) {
                            let _ = writeln!(stdout, "{line}");
                        }
                    }
                    if session_started
                        && let Some(worktree) = worktree
                        && let Err(error) = worktree.cleanup_terminal()
                    {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "Could not clean up worktree: {error}"
                        );
                    }
                    if let Some(error) = initialization_error {
                        eprintln!("Startup closed after initialization failure: {error}");
                    }
                    match exit.exit_code {
                        Some(code) => ExitCode::from(code),
                        None => ExitCode::SUCCESS,
                    }
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
