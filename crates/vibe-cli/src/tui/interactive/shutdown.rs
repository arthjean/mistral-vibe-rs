//! Ending an interactive session: the terminal handed back, every worker the
//! session started settled, and the session closed.

use std::time::Duration;

use serde_json::Value;
use vibe_core::telemetry::TelemetryRecord;

use super::super::runtime::InteractiveRuntime;
use super::super::shell::{finish_shell, interrupt_shell};
use super::super::{CliError, exit};
use super::{InteractiveExit, Session};

/// Restores the terminal, settles every worker the session started, and closes
/// the session itself.
///
/// Every step runs whatever the loop answered, and the first failure among them
/// is what the launch reports: a session that ended badly still restores the
/// terminal it took over.
pub(super) async fn shut_down(
    session: Session,
    event_loop: Result<(), CliError>,
    update_check: Option<tokio::task::JoinHandle<()>>,
) -> Result<InteractiveExit, CliError> {
    let Session {
        mut runtime,
        mut state,
        active,
        prompt_history,
        mut clipboard_images,
        mut path_normalization,
        mut terminal_guard,
        terminal,
        mounted_startup,
        ..
    } = session;
    drop(terminal);
    let restoration = terminal_guard
        .restore()
        .map_err(|error| CliError::Terminal(error.to_string()));
    for diagnostic in prompt_history.finish().await {
        eprintln!("{diagnostic}");
    }
    let interrupt_result =
        if let (Some(runtime), Some(active)) = (runtime.as_mut(), active.as_ref()) {
            runtime
                .service
                .interrupt(&runtime.session_id, &active.turn_id)
                .map(|_| ())
                .map_err(CliError::from)
        } else {
            Ok(())
        };
    if let Some(active) = active {
        active.join_before_exit(Duration::from_secs(2)).await;
    }
    if let Some(runtime) = runtime.as_mut() {
        interrupt_shell(runtime, &mut state).await;
        finish_shell(Some(runtime), &mut state).await;
        runtime.voice.shutdown().await;
        runtime.speech.shutdown().await;
    }
    let cleanup_result = clipboard_images
        .shutdown()
        .await
        .map_err(CliError::Terminal);
    path_normalization.shutdown();
    // A discovery still in flight never delays the exit the operator asked for.
    if let Some(update_check) = update_check {
        update_check.abort();
    }
    let telemetry = runtime
        .as_ref()
        .and_then(|runtime| runtime.telemetry.clone());
    // Reference `exit_summary`, read before the session closes.
    let summary = runtime.as_mut().map(session_exit_summary);
    let (close_result, shutdown_result) = if let Some(mut runtime) = runtime {
        let session_id = runtime.session_id.clone();
        // Reference `aclose` cancels the experiments task before it closes
        // anything else, so a session that quits mid-lookup never waits on the
        // request the lookup is holding.
        if let Some(experiments) = runtime.experiments.take() {
            experiments.close().await;
        }
        // Reference `emit_session_closed_telemetry`, raised before the session
        // is closed so the census still names it.
        runtime.report(&TelemetryRecord::SessionClosed);
        let close = runtime
            .service
            .close_session(&session_id)
            .await
            .map_err(CliError::from);
        let shutdown = runtime.service.shutdown().map_err(CliError::from);
        (close, shutdown)
    } else {
        (Ok(()), Ok(()))
    };
    let result = event_loop
        .and(restoration)
        .and(interrupt_result)
        .and(cleanup_result)
        .and(close_result)
        .and(shutdown_result);
    if let Some(telemetry) = telemetry {
        telemetry.flush().await;
    }
    result.map(|()| InteractiveExit {
        initialization_error: mounted_startup.into_initialization_error(),
        summary,
        exit_code: None,
    })
}

/// Reference `AppServerSession.exit_summary`: the session identity plus the
/// tokens spent since the session baseline.
fn session_exit_summary(runtime: &mut InteractiveRuntime) -> exit::SessionExitSummary {
    let stats = runtime.session_stats();
    let tokens = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or_default();
    exit::SessionExitSummary {
        session_id: Some(runtime.session_id.clone()),
        usage: exit::SessionUsage {
            input_tokens: tokens("sessionPromptTokens"),
            output_tokens: tokens("sessionCompletionTokens"),
        },
    }
}
