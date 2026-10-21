//! `/loop`: scheduling, listing and cancelling the session's loops.

use super::{CommandBackend, Effect, ScheduledLoop};

const LOOP_USAGE: &str =
    "Usage:\n  /loop <interval> <prompt>\n  /loop list\n  /loop cancel <id|all>\n";

/// Reference `_format_duration`: every nonzero unit, or only the largest.
pub(super) fn format_duration(mut seconds: u64, short: bool) -> String {
    let mut parts = Vec::new();
    for (unit_seconds, suffix) in [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")] {
        let value = seconds / unit_seconds;
        if value > 0 {
            parts.push(format!("{value}{suffix}"));
            seconds %= unit_seconds;
        }
    }
    if parts.is_empty() {
        parts.push("0s".to_owned());
    }
    if short {
        parts.swap_remove(0)
    } else {
        parts.concat()
    }
}

/// Reference `_format_loop_list`.
pub(in crate::tui) fn format_loop_list(loops: &[ScheduledLoop], now: f64) -> String {
    if loops.is_empty() {
        return "No scheduled loops.".to_owned();
    }
    let mut rows = vec![
        "| Prompt | Next in | Every | ID |".to_owned(),
        "|--------|------|-------|----|".to_owned(),
    ];
    for scheduled in loops {
        // Python's `int` truncates toward zero before `max(0, ...)` clamps.
        let remaining = (scheduled.next_fire_at - now).trunc().max(0.0) as u64;
        let prompt = scheduled.prompt.replace('|', "\\|").replace('\n', " ");
        rows.push(format!(
            "| {prompt} | {} | {} | `{}` |",
            format_duration(remaining, true),
            format_duration(scheduled.interval_seconds, false),
            scheduled.id
        ));
    }
    rows.join("\n")
}

/// Reference `ScheduledLoopCommands.handle_command`.
pub(super) async fn handle<B: CommandBackend>(arguments: &str, backend: &mut B) {
    let arguments = arguments.trim();
    let outcome = loop_outcome(arguments, backend).await;
    match outcome {
        Ok(message) => backend.emit(Effect::Message(message)),
        Err(error) => backend.emit(Effect::Error(format!("{error}\n{LOOP_USAGE}"))),
    }
}

async fn loop_outcome<B: CommandBackend>(
    arguments: &str,
    backend: &mut B,
) -> Result<String, String> {
    if arguments.is_empty() || matches!(arguments.to_lowercase().as_str(), "list" | "ls") {
        let loops = backend.loops_list().await?;
        return Ok(format_loop_list(&loops, backend.now()));
    }
    let (verb, rest) = arguments.split_once(' ').unwrap_or((arguments, ""));
    if !matches!(
        verb.to_lowercase().as_str(),
        "cancel" | "rm" | "stop" | "delete"
    ) {
        let created = backend.loops_create(verb, rest).await?;
        return Ok(format!(
            "Scheduled loop `{}` every {}: {}",
            created.id,
            format_duration(created.interval_seconds, false),
            created.prompt
        ));
    }
    let id = rest.trim();
    if id.is_empty() {
        return Err("Missing loop id.".to_owned());
    }
    if id.to_lowercase() == "all" {
        let count = backend.loops_clear().await?;
        return Ok(format!("Cancelled {count} scheduled loop(s)."));
    }
    let deleted = backend.loops_delete(id).await?;
    Ok(format!(
        "Cancelled loop `{}`: {}",
        deleted.id, deleted.prompt
    ))
}
