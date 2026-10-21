//! The body under an effect's header: the error or skip line of an unsettled
//! call, and the settled output laid out the way the reference result widget
//! of its kind declares (`vibe/cli/textual_ui/widgets/tool_widgets.py`).

use serde_json::Value;
use vibe_app_server::client::{EffectResultDisplay, PublicEffectState};

use super::super::render::clean_output;
use super::{BodyLine, BodyStyle, EffectKind};

pub(super) fn effect_body(
    kind: EffectKind,
    state: &PublicEffectState,
    settled: Option<&EffectResultDisplay>,
) -> Vec<BodyLine> {
    match state {
        PublicEffectState::Pending
        | PublicEffectState::Running { .. }
        | PublicEffectState::Blocked { .. } => Vec::new(),
        PublicEffectState::Failed { error, .. } => vec![BodyLine::styled(
            format!("Error: {}", error.message),
            BodyStyle::Error,
        )],
        PublicEffectState::Cancelled { reason, .. } | PublicEffectState::Skipped { reason, .. } => {
            vec![BodyLine::styled(
                format!("Skipped: {reason}"),
                BodyStyle::Muted,
            )]
        }
        PublicEffectState::Completed {
            output,
            output_text,
            ..
        } => {
            // Reference `ToolResultMessage`: a hook that replaced the result
            // leaves no structured output, so its text stands in for the body.
            if output.is_null() {
                let fallback = clean_output(output_text);
                let fallback = fallback.trim();
                if !fallback.is_empty() {
                    return fallback.split('\n').map(BodyLine::plain).collect();
                }
            }
            let mut lines = if shows_advisories(kind, output) {
                advisories(settled)
            } else {
                Vec::new()
            };
            lines.extend(completed_body(kind, output));
            lines
        }
    }
}

/// Which result widgets open with their advisories: the todo, question and
/// web widgets never do, and the read and edit widgets only above a result.
fn shows_advisories(kind: EffectKind, output: &Value) -> bool {
    match kind {
        EffectKind::Todo
        | EffectKind::UserQuestion
        | EffectKind::WebSearch
        | EffectKind::WebFetch
        | EffectKind::Process => false,
        EffectKind::FileRead | EffectKind::FileEdit => !output.is_null(),
        _ => true,
    }
}

/// Reference `ToolResultWidget._advisories`: the approval note, which says the
/// call was allowed and so carries no glyph, then each warning.
fn advisories(settled: Option<&EffectResultDisplay>) -> Vec<BodyLine> {
    let Some(display) = settled else {
        return Vec::new();
    };
    display
        .approval_note
        .iter()
        .filter(|note| !note.is_empty())
        .map(|note| BodyLine::styled(note.clone(), BodyStyle::Muted))
        .chain(
            display
                .warnings
                .iter()
                .map(|warning| BodyLine::styled(format!("⚠ {warning}"), BodyStyle::Warning)),
        )
        .collect()
}

/// The settled output an effect produced, laid out the way its kind's result
/// widget declares (`vibe/cli/textual_ui/widgets/tool_widgets.py`). A widget
/// given no output draws nothing but the todo widget's empty line.
///
/// The output itself stays a [`Value`]: its shape is the tool's own, and the
/// wire contract publishes it as one.
fn completed_body(kind: EffectKind, output: &Value) -> Vec<BodyLine> {
    let text = |key: &str| output.get(key).and_then(Value::as_str).unwrap_or_default();
    if output.is_null() && kind != EffectKind::Todo {
        return Vec::new();
    }
    match kind {
        // Reference `BashResultWidget`: the arrival-ordered transcript, or the
        // two captures joined without fabricating a line.
        EffectKind::Shell => {
            let transcript = shell_transcript(output);
            let transcript = transcript.trim_matches('\n');
            if transcript.is_empty() {
                return vec![BodyLine::styled("(no content)", BodyStyle::Muted)];
            }
            detail_lines(transcript)
        }
        EffectKind::FileRead => text_lines(&strip_line_numbers(text("content"))),
        EffectKind::FileWrite => text_lines(text("content")),
        EffectKind::FileEdit => occurrence_diff_lines(output),
        EffectKind::FileSearch => match output.get("matches") {
            Some(Value::String(matches)) => detail_lines(matches),
            _ => search_lines(output),
        },
        EffectKind::Todo => todo_lines(output),
        EffectKind::UserQuestion => Vec::new(),
        EffectKind::WebSearch => web_search_lines(output),
        EffectKind::WebFetch => detail_lines(text("content")),
        EffectKind::Process => process_lines(output),
        // A worktree entry settles with no output of its own, and the
        // reference terminal has no result widget for a scratchpad call.
        EffectKind::Skill
        | EffectKind::Subagent
        | EffectKind::Tool
        | EffectKind::Worktree
        | EffectKind::Scratchpad => generic_lines(output),
    }
}

/// Reference `ProcessResultWidget`: one layout per `process.*` operation, read
/// off the envelope's `tool_name`. Only the Unified harness publishes the kind;
/// the layout is kept so a transcript carrying one still reads as it did.
pub(super) fn process_lines(output: &Value) -> Vec<BodyLine> {
    let field = |key: &str| output.get(key).filter(|value| !value.is_null());
    let text = |key: &str| field(key).and_then(Value::as_str).unwrap_or_default();
    let labeled = |keys: &[&str]| {
        keys.iter()
            .filter_map(|key| {
                field(key)
                    .filter(|value| !matches!(value, Value::String(text) if text.is_empty()))
                    .map(|value| format!("{key}: {}", scalar_text(value)))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    match text("tool_name") {
        "process.start" | "process.stop" => {
            detail_lines(&labeled(&["processId", "status", "exitCode"]))
        }
        "process.output" => {
            let body = text("output");
            let mut lines = if body.trim().is_empty() {
                vec![BodyLine::plain("(no output)")]
            } else {
                detail_lines(body)
            };
            let mut hints = Vec::new();
            if let Some(available) = field("bytesAvailable").and_then(Value::as_i64)
                && available > 0
            {
                hints.push(format!("{available} bytes available"));
            }
            if field("hasMore").and_then(Value::as_bool) == Some(true) {
                hints.push("more output pending".to_owned());
            }
            if field("truncatedBefore").and_then(Value::as_bool) == Some(true) {
                hints.push("earlier output truncated".to_owned());
            }
            if !hints.is_empty() {
                lines.push(BodyLine::styled(hints.join(" | "), BodyStyle::Muted));
            }
            lines
        }
        "process.list" => {
            let processes = field("processes")
                .and_then(Value::as_array)
                .filter(|processes| !processes.is_empty());
            let Some(processes) = processes else {
                return vec![BodyLine::plain("(no background processes)")];
            };
            let listed = processes
                .iter()
                .map(|process| {
                    let value =
                        |key: &str| process.get(key).map_or_else(|| "?".to_owned(), scalar_text);
                    let id = process
                        .get("processId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    format!(
                        "{}  {}  {}",
                        short_process_id(id),
                        value("status"),
                        value("command")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            detail_lines(&listed)
        }
        "process.write" => detail_lines(&labeled(&["bytesWritten", "status"])),
        _ => Vec::new(),
    }
}

/// Reference `_short_pid`: the last eight characters of a longer id.
fn short_process_id(id: &str) -> &str {
    let count = id.chars().count();
    if count > 8 {
        id.char_indices()
            .nth(count - 8)
            .map_or(id, |(index, _)| &id[index..])
    } else {
        id
    }
}

/// Reference `GenericToolResultWidget`: `key: value` per populated field, and
/// the raw value otherwise.
fn generic_lines(output: &Value) -> Vec<BodyLine> {
    match output {
        Value::Object(fields) => detail_lines(
            &fields
                .iter()
                .filter(|(_, value)| !is_empty_value(value))
                .map(|(key, value)| format!("{key}: {}", scalar_text(value)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        value => detail_lines(&scalar_text(value)),
    }
}

fn search_lines(output: &Value) -> Vec<BodyLine> {
    let Some(matches) = output.as_array() else {
        return Vec::new();
    };
    matches
        .iter()
        .map(|entry| {
            BodyLine::plain(format!(
                "{}:{}:{}",
                entry
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                entry
                    .get("line")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                entry
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ))
        })
        .collect()
}

/// Reference `TodoResultWidget`: grouped by status, in reference order, with
/// the reference glyphs.
fn todo_lines(output: &Value) -> Vec<BodyLine> {
    let todos = todo_items(output);
    if todos.is_empty() {
        return vec![BodyLine::styled("No todos", BodyStyle::Muted)];
    }
    let mut lines = Vec::new();
    for status in ["in_progress", "pending", "completed", "cancelled"] {
        for todo in &todos {
            if todo.get("status").and_then(Value::as_str) != Some(status) {
                continue;
            }
            let icon = match status {
                "completed" => "☑",
                "cancelled" => "☒",
                _ => "☐",
            };
            let content = todo
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            lines.push(BodyLine::plain(format!("{icon} {content}")));
        }
    }
    lines
}

fn todo_items(output: &Value) -> Vec<Value> {
    output
        .get("todos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Reference `WebSearchResultWidget`.
fn web_search_lines(output: &Value) -> Vec<BodyLine> {
    let mut lines = vec![BodyLine::plain(format!(
        "query: {}",
        output
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
    ))];
    if let Some(answer) = output.get("answer").and_then(Value::as_str)
        && !answer.is_empty()
    {
        lines.extend(detail_lines(&format!("answer: {answer}")));
    }
    let sources = output
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !sources.is_empty() {
        lines.push(BodyLine::plain(String::new()));
        if sources.len() > 1 {
            lines.push(BodyLine::plain("Sources:"));
        }
        for source in &sources {
            let url = source
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let label = source
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .unwrap_or(url);
            let mut line = BodyLine::plain(format!("  • {label}"));
            // Reference `_is_safe_url`: only a web address becomes clickable.
            line.link = is_safe_link(url).then(|| url.to_owned());
            lines.push(line);
        }
    }
    lines
}

/// Reference `_SAFE_SCHEMES` (`vibe/cli/textual_ui/widgets/links.py`).
fn is_safe_link(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

/// Reference `EditResultWidget` over `render_edit_diff`
/// (`vibe/cli/textual_ui/widgets/diff_rendering.py`): each occurrence is its
/// own unified diff with two lines of context, numbered from the line it starts
/// on, with a gap row between occurrences and between hunks. The output is
/// keyed by its model's aliases (`vibe_core::events::detail::project_output`).
fn occurrence_diff_lines(output: &Value) -> Vec<BodyLine> {
    let mut occurrences = output
        .get("occurrences")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Reference `EditResultWidget.__init__`: without occurrences, the bare
    // replacement is the one occurrence, anchored nowhere.
    if occurrences.is_empty()
        && let (Some(old), Some(new)) = (
            output.get("oldString").and_then(Value::as_str),
            output.get("newString").and_then(Value::as_str),
        )
    {
        occurrences.push(serde_json::json!({"oldText": old, "newText": new}));
    }
    let mut lines = Vec::new();
    for (index, occurrence) in occurrences.iter().enumerate() {
        if index > 0 {
            lines.push(diff_gap());
        }
        let start_line = occurrence
            .get("startLine")
            .and_then(Value::as_u64)
            .filter(|line| *line > 0);
        let text = |key: &str| {
            occurrence
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        let (old, new) = (text("oldText"), text("newText"));
        let old = old.trim_end_matches('\n').split('\n').collect::<Vec<_>>();
        let new = new.trim_end_matches('\n').split('\n').collect::<Vec<_>>();
        occurrence_rows(
            &vibe_core::difflib::unified_diff(&old, &new, 2),
            start_line,
            &mut lines,
        );
    }
    lines
}

/// Reference `_gap_line`.
fn diff_gap() -> BodyLine {
    BodyLine::styled("⋯", BodyStyle::Muted)
}

/// Reference `_render_occurrence`: the hunk headers become line numbers in the
/// gutter, and every hunk after the first is announced by a gap row.
fn occurrence_rows(diff: &[String], start_line: Option<u64>, lines: &mut Vec<BodyLine>) {
    let offset = start_line.map_or(0, |line| line - 1);
    let (mut old_line, mut new_line) = (0u64, 0u64);
    let mut first_hunk = true;
    for row in diff {
        let mut characters = row.chars();
        let Some(prefix) = characters.next() else {
            continue;
        };
        let code = characters.as_str();
        if prefix == '@' {
            if !first_hunk {
                lines.push(diff_gap());
            }
            first_hunk = false;
            if let Some((old_start, new_start)) = hunk_starts(row) {
                old_line = old_start + offset;
                new_line = new_start + offset;
            }
            continue;
        }
        let (number, style) = match prefix {
            '-' => {
                old_line += 1;
                (old_line - 1, BodyStyle::Removed)
            }
            '+' => {
                new_line += 1;
                (new_line - 1, BodyStyle::Added)
            }
            _ => {
                old_line += 1;
                new_line += 1;
                (new_line - 1, BodyStyle::Plain)
            }
        };
        let gutter = start_line.map_or_else(String::new, |_| format!("{number:>4} "));
        lines.push(BodyLine::styled(format!("{gutter}{prefix} {code}"), style));
    }
}

/// The old and new start lines of an `@@ -a,b +c,d @@` header.
fn hunk_starts(header: &str) -> Option<(u64, u64)> {
    let ranges = header.strip_prefix("@@ -")?;
    let (old, rest) = ranges.split_once(" +")?;
    let new = rest.split_once(" @@")?.0;
    let start = |range: &str| range.split(',').next()?.parse::<u64>().ok();
    Some((start(old)?, start(new)?))
}

/// Reference `ShellEffectOutput.transcript`.
fn shell_transcript(output: &Value) -> String {
    let field = |key: &str| {
        output
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let transcript = field("output");
    if !transcript.is_empty() {
        return transcript;
    }
    let (stdout, stderr) = (field("stdout"), field("stderr"));
    if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
        return format!("{stdout}\n{stderr}");
    }
    stdout + stderr.as_str()
}

/// Reference `ToolResultWidget._yield_text`: captured output is cleaned for
/// display, and nothing is shown when nothing is left.
pub(super) fn detail_lines(text: &str) -> Vec<BodyLine> {
    let cleaned = clean_output(text.trim_matches('\n'));
    if cleaned.is_empty() {
        return Vec::new();
    }
    cleaned.split('\n').map(BodyLine::plain).collect()
}

pub(super) fn text_lines(text: &str) -> Vec<BodyLine> {
    let trimmed = text.trim_matches('\n');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.lines().map(BodyLine::plain).collect()
}

/// Reference `_strip_line_numbers`, extended to the `12|` form the Rust
/// workspace reader emits.
fn strip_line_numbers(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
            if digits == 0 {
                return line.to_owned();
            }
            let rest = &trimmed[digits..];
            rest.strip_prefix('→')
                .or_else(|| rest.strip_prefix('|'))
                .map_or_else(|| line.to_owned(), str::to_owned)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}
