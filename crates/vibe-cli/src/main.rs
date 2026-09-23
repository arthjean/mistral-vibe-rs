#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::Write;
use std::process::ExitCode;

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
        let mut stderr = std::io::stderr().lock();
        let environment = vibe_cli::mcp_command::McpEnvironment::from_process();
        return ExitCode::from(
            vibe_cli::mcp_command::run(&arguments[1..], &environment, &mut stdout, &mut stderr)
                .await,
        );
    }
    // The refusal is rendered here rather than by clap's own exit path: the
    // reference reports a bad argv as a usage block and one `vibe: error:`
    // line, and only a parse that returns the error can be re-shaped.
    let arguments = match vibe_cli::argv::parse_arguments(std::env::args_os()) {
        Ok(arguments) => arguments,
        Err(failure) => {
            if failure.use_stderr {
                let _ = write!(std::io::stderr().lock(), "{}", failure.rendered);
            } else {
                let _ = write!(std::io::stdout().lock(), "{}", failure.rendered);
            }
            return ExitCode::from(failure.exit);
        }
    };
    // The log file opens before anything else can fail, so a startup that dies
    // before the app server attaches still leaves a line behind.
    vibe_cli::install_file_logging(&arguments);
    // The span exporter is installed before any turn can open a span, and its
    // guard lives as long as the process: dropping it flushes the batch.
    let _tracing = vibe_cli::install_tracing(&arguments);
    // The flag as it was parsed, kept before preparation consumes the
    // arguments: the reference's cleanup gate reads that same value, and a
    // prompt piped in later is not what it tests
    // (`vibe/cli/entrypoint.py:349-353`).
    let prompt = arguments.prompt.clone();
    let invocation = match PreparedInvocation::prepare(arguments, &mut std::io::stderr().lock()) {
        Ok(invocation) => invocation,
        Err(error) => {
            // The reference reports this one failure on stdout, where the rest
            // of its worktree narration goes to stderr
            // (`vibe/cli/entrypoint.py:301-304`).
            let _ = writeln!(std::io::stdout().lock(), "Error: {error}");
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
            let workspace = invocation.workspace.clone();
            let worktree = workspace.worktree.clone();
            let outcome = match vibe_cli::tui::run_interactive(invocation).await {
                Ok(exit) => {
                    let initialization_error = exit.initialization_error;
                    if let Some(summary) = &exit.summary {
                        let mut stdout = std::io::stdout().lock();
                        for line in vibe_cli::tui::exit::session_resume_lines(summary) {
                            let _ = writeln!(stdout, "{line}");
                        }
                    }
                    // The offer reads the run's own exit code, so a launch that
                    // ended at the update prompt or under Ctrl-C is asked while
                    // a failure keeps the worktree for the next attempt.
                    if vibe_cli::tui::startup::cleanup_is_offered(
                        worktree.as_ref(),
                        prompt.as_deref(),
                        exit.exit_code,
                    ) && let Some(worktree) = worktree
                        && let Err(error) =
                            vibe_cli::tui::startup::cleanup_worktree_terminal(worktree)
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
            };
            // Dropped on every path, cleanup offered or not: a holder left
            // behind reads as a live session to every later release.
            workspace.release_holder();
            outcome
        }
        PreparedInvocation::Programmatic(invocation) => {
            let mut stdout = std::io::stdout().lock();
            let mut stderr = std::io::stderr().lock();
            let workspace = invocation.workspace;
            let outcome = match vibe_cli::run(invocation.arguments, &mut stdout, &mut stderr).await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    ExitCode::from(error.exit_code())
                }
            };
            workspace.release_holder();
            outcome
        }
    }
}
