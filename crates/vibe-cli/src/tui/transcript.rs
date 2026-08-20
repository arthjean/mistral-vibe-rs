//! Semantic projection of canonical history into reference transcript regions.
//!
//! The reference renders history by meaning: every effect carries a call
//! display (`verb`, `message`, `suffix`) and, once settled, a result display
//! whose `success` flag is authoritative. This module reproduces that contract
//! as pure functions over an already projected [`TranscriptEntry`], so the
//! renderer never re-derives status from the generation flag and a failed,
//! cancelled, or skipped effect can never be presented as a success.

use serde_json::Value;
use vibe_app_server::client::{
    EffectDetail, EffectResultDisplay, HookNotice, HookSeverity, NoticeDetail, PublicEffectState,
    PublicHistoryEntry, PublicNoticeLevel, ToolEffectKind,
};

use super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind};

/// The kind the app server published with the effect. Its verbs, subject and
/// status text arrive on the entry; what stays here is how the terminal lays
/// the result out.
pub type EffectKind = ToolEffectKind;

/// How the terminal folds and groups an effect, which is presentation the wire
/// contract does not carry.
pub trait EffectLayout {
    /// Reference `EFFECT_WIDGETS[...].result.COLLAPSIBLE`: diff-shaped and
    /// question results always render in full, everything else folds into its
    /// header until the operator expands it.
    fn collapses(&self) -> bool;

    /// Reference `_NON_GROUPED_EFFECT_KINDS`: writes and edits break the
    /// current tool group and stand on their own.
    fn joins_tool_group(&self) -> bool;
}

impl EffectLayout for EffectKind {
    fn collapses(&self) -> bool {
        !matches!(self, Self::FileEdit | Self::FileWrite | Self::UserQuestion)
    }

    fn joins_tool_group(&self) -> bool {
        !matches!(self, Self::FileEdit | Self::FileWrite)
    }
}

/// The published effect an entry projects, when the entry is one and its
/// canonical form was restored with it.
///
/// An entry replayed from saved history carries no canonical effect, which is
/// what [`restored_effect_region`] presents instead.
fn published_effect(entry: &TranscriptEntry) -> Option<(&EffectDetail, &PublicEffectState)> {
    match entry.source.server()? {
        PublicHistoryEntry::Effect { detail, state, .. } => Some((detail, state)),
        _ => None,
    }
}

/// Reference `IndicatorState` plus the in-flight spinner it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    Running,
    Success,
    Error,
    Muted,
}

impl Indicator {
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Running => "⠋",
            Self::Success => "✓",
            Self::Error => "✕",
            Self::Muted => "□",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyStyle {
    Plain,
    Added,
    Removed,
    Warning,
    Error,
    Muted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLine {
    pub text: String,
    pub style: BodyStyle,
}

impl BodyLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: BodyStyle::Plain,
        }
    }

    fn styled(text: impl Into<String>, style: BodyStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// How a notice reads, which is the level the server publishes.
pub type NoticeLevel = PublicNoticeLevel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRegion {
    pub kind: EffectKind,
    pub status: EntryStatus,
    pub indicator: Indicator,
    pub verb: String,
    pub message: String,
    pub suffix: String,
    /// Reference default: collapsible results show only their header.
    pub collapsed_by_default: bool,
    /// Streaming output rendered under the header while the effect runs.
    pub stream: Option<String>,
    pub body: Vec<BodyLine>,
}

impl EffectRegion {
    /// Reference `EffectResultDisplay.text`.
    #[must_use]
    pub fn header_text(&self) -> String {
        let head = if self.verb.is_empty() {
            self.message.clone()
        } else {
            format!("{} {}", self.verb, self.message).trim().to_owned()
        };
        if self.suffix.is_empty() {
            head
        } else {
            format!("{head} {}", self.suffix)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    UserMessage,
    AssistantMessage,
    Reasoning,
    Effect(Box<EffectRegion>),
    Callback {
        title: String,
        detail: String,
    },
    Compaction {
        message: String,
    },
    Checkpoint {
        message: String,
    },
    Hook {
        icon: &'static str,
        line: String,
    },
    Command {
        message: String,
    },
    /// Markdown a command wrote into the transcript, rendered as Markdown
    /// rather than as wrapped text.
    Document {
        message: String,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
    Plan,
}

/// Projects one canonical entry into the region the reference renders.
#[must_use]
pub fn region(entry: &TranscriptEntry) -> Region {
    match entry.kind {
        TranscriptKind::UserMessage => Region::UserMessage,
        TranscriptKind::AssistantMessage => Region::AssistantMessage,
        TranscriptKind::Reasoning => Region::Reasoning,
        TranscriptKind::Plan => Region::Plan,
        TranscriptKind::Effect => Region::Effect(Box::new(effect_region(entry))),
        TranscriptKind::Callback => {
            let (title, detail) = entry.text.split_once('\n').unwrap_or((&entry.text, ""));
            Region::Callback {
                title: title.to_owned(),
                detail: detail.to_owned(),
            }
        }
        TranscriptKind::Checkpoint => {
            let message = entry.text.clone();
            if checkpoint_kind(entry) == Some(COMPACTION_CHECKPOINT) {
                Region::Compaction { message }
            } else {
                Region::Checkpoint { message }
            }
        }
        TranscriptKind::Notice => notice_region(entry),
        TranscriptKind::Document => Region::Document {
            message: entry.text.clone(),
        },
    }
}

/// Reference loading status: the newest unsettled effect names what the
/// runtime is doing, reasoning reads as thinking, and anything else falls back
/// to the default generation status.
#[must_use]
pub fn activity_status(entries: &[TranscriptEntry]) -> String {
    for entry in entries.iter().rev() {
        if entry.status.is_terminal() {
            continue;
        }
        match region(entry) {
            Region::Effect(effect) => return effect.kind.status_text().to_owned(),
            Region::Reasoning => return "Thinking".to_owned(),
            _ => {}
        }
    }
    super::diagnostics::DEFAULT_ACTIVITY_STATUS.to_owned()
}

/// Reference `_entry_keeps_tool_group`: effects other than writes and edits,
/// reasoning, and hook notices stay inside the current tool group.
#[must_use]
pub fn keeps_tool_group(entry: &TranscriptEntry) -> bool {
    // Grouping is decided per frame for every visible entry, so it reads the
    // discriminants directly instead of projecting the whole region.
    match entry.kind {
        TranscriptKind::Reasoning => true,
        TranscriptKind::Effect => published_effect(entry)
            .map_or(EffectKind::Tool, |(detail, _)| detail.kind)
            .joins_tool_group(),
        TranscriptKind::Notice => matches!(notice_detail(entry), Some(detail) if is_hook(detail)),
        _ => false,
    }
}

/// The checkpoint kind the server published, which is what tells a compaction
/// from every other checkpoint.
fn checkpoint_kind(entry: &TranscriptEntry) -> Option<&str> {
    match entry.source.server()? {
        PublicHistoryEntry::Checkpoint { kind, .. } => Some(kind),
        _ => None,
    }
}

/// Reference `CompactionCheckpoint`.
const COMPACTION_CHECKPOINT: &str = "compaction";

/// Why the server wrote this notice, when the server wrote it at all.
fn notice_detail(entry: &TranscriptEntry) -> Option<&NoticeDetail> {
    match entry.source.server()? {
        PublicHistoryEntry::Notice { detail, .. } => Some(detail),
        _ => None,
    }
}

/// The hook run a notice reports, in the four shapes a hook is reported under.
const fn hook_notice(detail: &NoticeDetail) -> Option<&HookNotice> {
    match detail {
        NoticeDetail::HookRunStarted(hook)
        | NoticeDetail::HookRunCompleted(hook)
        | NoticeDetail::HookStarted(hook)
        | NoticeDetail::HookCompleted(hook) => Some(hook),
        _ => None,
    }
}

const fn is_hook(detail: &NoticeDetail) -> bool {
    hook_notice(detail).is_some()
}

fn notice_region(entry: &TranscriptEntry) -> Region {
    match notice_detail(entry) {
        Some(NoticeDetail::ScheduledLoopFired { .. }) => Region::Command {
            message: entry.text.clone(),
        },
        Some(detail) => match hook_notice(detail) {
            Some(hook) => Region::Hook {
                icon: hook_icon(hook.status),
                line: format!(
                    "[{}] {}",
                    hook.hook_name.as_deref().unwrap_or("hook"),
                    hook.content.as_deref().unwrap_or(entry.text.as_str())
                ),
            },
            None => plain_notice(entry),
        },
        None => plain_notice(entry),
    }
}

/// A notice with nothing to present beyond its own text: the level it was
/// written at is the level the server published, or the one this client chose.
fn plain_notice(entry: &TranscriptEntry) -> Region {
    let level = match &entry.source {
        EntrySource::Server(published) => match published.as_ref() {
            PublicHistoryEntry::Notice { level, .. } => *level,
            _ => NoticeLevel::Info,
        },
        EntrySource::Notice { level, .. } => *level,
        EntrySource::Restored => NoticeLevel::Info,
    };
    Region::Notice {
        level,
        message: entry.text.clone(),
    }
}

/// Presents an effect whose canonical state was not restored with it: the
/// first line is the header, the rest is the recorded output, and the entry's
/// own status stays authoritative.
fn restored_effect_region(entry: &TranscriptEntry) -> EffectRegion {
    let (title, output) = entry.text.split_once('\n').unwrap_or((&entry.text, ""));
    EffectRegion {
        kind: EffectKind::Tool,
        status: entry.status,
        indicator: match entry.status {
            EntryStatus::Failed => Indicator::Error,
            EntryStatus::Cancelled | EntryStatus::Skipped => Indicator::Muted,
            EntryStatus::Completed => Indicator::Success,
            EntryStatus::Pending | EntryStatus::Streaming | EntryStatus::Blocked => {
                Indicator::Running
            }
        },
        verb: String::new(),
        message: title.to_owned(),
        suffix: String::new(),
        collapsed_by_default: EffectKind::Tool.collapses(),
        stream: None,
        body: text_lines(output),
    }
}

/// Reference `_HOOK_SEVERITY_ICONS`, defaulting to the warning icon.
const fn hook_icon(severity: Option<HookSeverity>) -> &'static str {
    match severity {
        Some(HookSeverity::Ok) => "✓",
        Some(HookSeverity::Error) => "✗",
        Some(HookSeverity::Warning) | None => "⚠",
    }
}

fn effect_region(entry: &TranscriptEntry) -> EffectRegion {
    // An entry restored from saved history carries no canonical effect: its own
    // settled status and text are then the only truth about it.
    let Some((detail, state)) = published_effect(entry) else {
        return restored_effect_region(entry);
    };
    let status = EntryStatus::of_effect(state);
    // The call and result displays are published with the entry, so the header
    // a reference client renders and the one this terminal renders are the same
    // strings rather than two derivations of the same arguments.
    let settled = settled_display(state);
    let (verb, message, suffix) = settled.map_or_else(
        || {
            (
                detail.display.verb.clone(),
                detail.display.subject().to_owned(),
                detail.display.suffix.clone(),
            )
        },
        |display| {
            (
                display.verb.clone(),
                display.message.clone(),
                display.suffix.clone(),
            )
        },
    );
    EffectRegion {
        kind: detail.kind,
        status,
        indicator: indicator(status, settled),
        verb,
        message,
        suffix,
        collapsed_by_default: detail.kind.collapses(),
        stream: running_stream(state),
        body: effect_body(detail.kind, state, settled),
    }
}

/// The result display a settled effect published. A cancellation that produced
/// no result is the one settled state the reference lets publish none.
const fn settled_display(state: &PublicEffectState) -> Option<&EffectResultDisplay> {
    match state {
        PublicEffectState::Completed { display, .. }
        | PublicEffectState::Failed { display, .. }
        | PublicEffectState::Skipped { display, .. } => Some(display),
        PublicEffectState::Cancelled { display, .. } => display.as_ref(),
        PublicEffectState::Pending
        | PublicEffectState::Running { .. }
        | PublicEffectState::Blocked { .. } => None,
    }
}

/// Reference `ToolCallMessage.set_stream_message`: streaming output is only
/// shown while the effect is still running.
fn running_stream(state: &PublicEffectState) -> Option<String> {
    let output_text = match state {
        PublicEffectState::Running { output_text }
        | PublicEffectState::Blocked { output_text, .. } => output_text.as_str(),
        _ => return None,
    };
    let last = output_text
        .trim_end_matches('\n')
        .lines()
        .next_back()
        .unwrap_or_default();
    (!last.is_empty()).then(|| format!("→ {last}"))
}

fn indicator(status: EntryStatus, settled: Option<&EffectResultDisplay>) -> Indicator {
    match status {
        EntryStatus::Pending | EntryStatus::Streaming | EntryStatus::Blocked => Indicator::Running,
        EntryStatus::Cancelled | EntryStatus::Skipped => Indicator::Muted,
        EntryStatus::Failed => Indicator::Error,
        EntryStatus::Completed => {
            if settled.is_some_and(|display| display.success) {
                Indicator::Success
            } else {
                Indicator::Error
            }
        }
    }
}

fn effect_body(
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
            let mut lines = settled
                .into_iter()
                .flat_map(|display| display.warnings.iter())
                .map(|warning| BodyLine::styled(format!("⚠ {warning}"), BodyStyle::Warning))
                .collect::<Vec<_>>();
            lines.extend(completed_body(kind, output, output_text));
            lines
        }
    }
}

/// The settled output an effect produced, laid out the way its kind declares.
///
/// The output itself stays a [`Value`]: its shape is the tool's own, and the
/// wire contract publishes it as one.
fn completed_body(kind: EffectKind, output: &Value, output_text: &str) -> Vec<BodyLine> {
    match kind {
        EffectKind::Shell => {
            let mut parts = Vec::new();
            for key in ["stdout", "stderr"] {
                let text = output.get(key).and_then(Value::as_str).unwrap_or_default();
                if !text.trim_matches('\n').is_empty() {
                    parts.push(text.trim_matches('\n').to_owned());
                }
            }
            if parts.is_empty() && !output_text.trim().is_empty() {
                parts.push(output_text.trim_matches('\n').to_owned());
            }
            if parts.is_empty() {
                return vec![BodyLine::styled("(no content)", BodyStyle::Muted)];
            }
            text_lines(&parts.join("\n"))
        }
        EffectKind::FileRead => {
            let content = output
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(output_text);
            text_lines(&strip_line_numbers(content))
        }
        EffectKind::FileWrite => {
            let content = output
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(output_text);
            text_lines(content)
        }
        EffectKind::FileEdit => {
            let occurrences = occurrence_diff_lines(output);
            if occurrences.is_empty() {
                diff_lines(
                    output
                        .get("diff")
                        .and_then(Value::as_str)
                        .unwrap_or(output_text),
                )
            } else {
                occurrences
            }
        }
        EffectKind::FileSearch => match output.get("matches") {
            Some(Value::String(matches)) => text_lines(matches),
            _ => search_lines(output, output_text),
        },
        EffectKind::Todo => todo_lines(output),
        EffectKind::UserQuestion => Vec::new(),
        EffectKind::WebSearch => web_search_lines(output),
        EffectKind::WebFetch => text_lines(
            output
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(output_text),
        ),
        EffectKind::Skill | EffectKind::Subagent | EffectKind::Tool => {
            generic_lines(output, output_text)
        }
    }
}

/// Reference `GenericToolResultWidget`: `key: value` per populated field, and
/// the raw value otherwise.
fn generic_lines(output: &Value, output_text: &str) -> Vec<BodyLine> {
    match output {
        Value::Object(fields) => fields
            .iter()
            .filter(|(_, value)| !is_empty_value(value))
            .map(|(key, value)| BodyLine::plain(format!("{key}: {}", scalar_text(value))))
            .collect(),
        Value::Null => text_lines(output_text),
        value => text_lines(&scalar_text(value)),
    }
}

fn search_lines(output: &Value, output_text: &str) -> Vec<BodyLine> {
    let Some(matches) = output.as_array() else {
        return text_lines(output_text);
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
        lines.extend(text_lines(&format!("answer: {answer}")));
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
            lines.push(BodyLine::plain(format!("  • {label}")));
        }
    }
    lines
}

/// Reference `EditResultWidget`: one diff per replaced occurrence, anchored on
/// the replaced text, falling back to the whole-call replacement.
fn occurrence_diff_lines(output: &Value) -> Vec<BodyLine> {
    let replacement = |old: &str, new: &str, lines: &mut Vec<BodyLine>| {
        for line in old.trim_end_matches('\n').split('\n') {
            lines.push(BodyLine::styled(format!("- {line}"), BodyStyle::Removed));
        }
        for line in new.trim_end_matches('\n').split('\n') {
            lines.push(BodyLine::styled(format!("+ {line}"), BodyStyle::Added));
        }
    };
    let mut lines = Vec::new();
    let occurrences = output
        .get("occurrences")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for occurrence in &occurrences {
        replacement(
            &string_argument(occurrence, &["old_text", "oldText"]),
            &string_argument(occurrence, &["new_text", "newText"]),
            &mut lines,
        );
    }
    if !lines.is_empty() {
        return lines;
    }
    let old = string_argument(output, &["old_string", "oldString"]);
    let new = string_argument(output, &["new_string", "newString"]);
    if old.is_empty() && new.is_empty() {
        return lines;
    }
    replacement(&old, &new, &mut lines);
    lines
}

fn diff_lines(diff: &str) -> Vec<BodyLine> {
    diff.lines()
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                BodyStyle::Muted
            } else if line.starts_with('+') {
                BodyStyle::Added
            } else if line.starts_with('-') {
                BodyStyle::Removed
            } else {
                BodyStyle::Muted
            };
            BodyLine::styled(line, style)
        })
        .collect()
}

fn text_lines(text: &str) -> Vec<BodyLine> {
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

fn string_argument(arguments: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use vibe_app_server::client::{PublicEntryGenerationStatus, PublicEntryMetadata};

    use super::*;

    /// An entry in the shape the app server publishes: a typed detail carrying
    /// the call display, and a settled state carrying the result display the
    /// projection computed for it.
    ///
    /// `state` is written in its wire form so a fixture reads like the payload
    /// a client receives, and is deserialized back into the published union
    /// here, which is what the projection under test consumes.
    fn effect(tool: &str, arguments: Value, mut state: Value) -> TranscriptEntry {
        let detail = EffectDetail::for_call(tool, &arguments);
        if let Some(object) = state.as_object_mut() {
            let output = object.get("output").cloned().unwrap_or(Value::Null);
            let display = match object.get("status").and_then(Value::as_str) {
                Some("completed") => Some(EffectResultDisplay::completed(
                    detail.kind,
                    &detail.display,
                    &output,
                    &Value::Null,
                )),
                Some("failed") => Some(EffectResultDisplay::failed(&detail.display)),
                Some("skipped") => Some(EffectResultDisplay::skipped(&detail.tool_name)),
                Some("cancelled") => {
                    EffectResultDisplay::cancelled(&detail.tool_name, &output, &Value::Null)
                }
                _ => None,
            };
            if let Some(display) = display {
                object.insert("display".to_owned(), json!(display));
            }
        }
        published(PublicHistoryEntry::Effect {
            metadata: metadata("effect"),
            title: tool.to_owned(),
            detail: Box::new(detail),
            state: serde_json::from_value(state).expect("the fixture is a published effect state"),
            tool_call_id: String::new(),
        })
    }

    fn metadata(id: &str) -> PublicEntryMetadata {
        PublicEntryMetadata {
            id: id.to_owned(),
            session_id: "session".to_owned(),
            turn_id: None,
            created_at: 1,
            updated_at: 1,
            generation_status: PublicEntryGenerationStatus::Completed,
            related_entry_id: None,
        }
    }

    /// Projects a published entry exactly the way the session does, so a
    /// fixture and a live entry reach the region under test the same way.
    fn published(entry: PublicHistoryEntry) -> TranscriptEntry {
        crate::tui::hydration::history_entry(entry)
    }

    fn effect_of(entry: &TranscriptEntry) -> EffectRegion {
        match region(entry) {
            Region::Effect(effect) => *effect,
            other => panic!("expected an effect region, got {other:?}"),
        }
    }

    /// The transcript renders the payload the `todo` tool actually produces,
    /// taken from the tool itself rather than from a hand-written fixture, so
    /// a change to either side breaks here.
    #[tokio::test]
    async fn the_todo_tool_result_renders_through_the_todo_effect() {
        let directory = tempfile::tempdir().expect("tempdir");
        let policy = vibe_core::policy::PermissionStore::default();
        let registry = vibe_core::tools::ToolRegistry::default();
        vibe_core::tools::builtins::BuiltinTools::new(directory.path(), None)
            .register(
                "session-1",
                vibe_core::skills::SkillDiscovery::default(),
                &registry,
                &vibe_core::policy::ToolGuard::new(policy, Arc::new(DenyEverything)),
            )
            .expect("universal tools register");
        let written = registry
            .invoke(
                "todo",
                vibe_core::tools::ToolInvocation {
                    call_id: "todo-1".to_owned(),
                    arguments: json!({
                        "action": "write",
                        "todos": [
                            {"id": "a", "content": "draft", "status": "completed"},
                            {"id": "b", "content": "ship", "status": "in_progress"}
                        ],
                    }),
                },
            )
            .await
            .expect("write");

        let entry = effect(
            "todo",
            json!({"action": "write"}),
            json!({
                "status": "completed",
                "output": written.typed_result,
                "outputText": written.model_text,
            }),
        );
        assert_eq!(EffectKind::from_tool_name("todo"), EffectKind::Todo);
        let rendered = effect_of(&entry);
        // Reference `TodoResultWidget` order: in_progress before completed.
        assert_eq!(
            rendered
                .body
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["☐ ship", "☑ draft"]
        );
    }

    struct DenyEverything;

    impl vibe_core::policy::ApprovalAgent for DenyEverything {
        fn request<'a>(
            &'a self,
            _request: vibe_core::policy::ApprovalRequest,
        ) -> vibe_core::policy::ApprovalFuture<'a> {
            Box::pin(async { Ok(vibe_core::policy::ApprovalDecision::Deny) })
        }
    }

    #[test]
    fn a_completed_generation_never_outranks_a_failed_or_skipped_effect_state() {
        let failed = effect_of(&effect(
            "shell",
            json!({"command": "cargo test"}),
            json!({"status": "failed", "error": {"message": "exit 101"}}),
        ));
        assert_eq!(failed.indicator, Indicator::Error);
        assert_eq!(failed.header_text(), "Ran cargo test");
        assert_eq!(failed.body[0].text, "Error: exit 101");

        let skipped = effect_of(&effect(
            "shell",
            json!({"command": "rm -rf /"}),
            json!({"status": "skipped", "reason": "denied by the operator"}),
        ));
        assert_eq!(skipped.indicator, Indicator::Muted);
        assert_eq!(skipped.header_text(), "shell: skipped");
        assert_eq!(skipped.body[0].text, "Skipped: denied by the operator");

        let cancelled = effect_of(&effect(
            "shell",
            json!({"command": "sleep 60"}),
            json!({"status": "cancelled", "reason": "interrupted"}),
        ));
        assert_eq!(cancelled.indicator, Indicator::Muted);
        assert_eq!(cancelled.status, EntryStatus::Cancelled);
    }

    #[test]
    fn in_flight_effects_stream_their_latest_line_and_settle_without_it() {
        let running = effect_of(&effect(
            "shell",
            json!({"command": "cargo build"}),
            json!({"status": "running", "outputText": "compiling\nlinking\n"}),
        ));
        assert_eq!(running.indicator, Indicator::Running);
        assert_eq!(running.stream.as_deref(), Some("→ linking"));
        assert!(running.body.is_empty());

        let pending = effect_of(&effect("shell", json!({}), json!({"status": "pending"})));
        assert_eq!(pending.status, EntryStatus::Pending);
        assert_eq!(pending.stream, None);

        let blocked = effect_of(&effect(
            "shell",
            json!({"command": "cargo build"}),
            json!({"status": "blocked", "callbackId": "callback-1", "outputText": ""}),
        ));
        assert_eq!(blocked.status, EntryStatus::Blocked);
        assert_eq!(blocked.indicator, Indicator::Running);
    }

    /// The rename moved the published names onto `read_file` and `grep`. Both
    /// must project to the effect kind their predecessor did, and render the
    /// same header from the reference argument keys, while the old names keep
    /// working so a persisted transcript still replays.
    #[test]
    fn the_renamed_file_tools_render_exactly_as_their_predecessors_did() {
        assert_eq!(
            EffectKind::from_tool_name("read_file"),
            EffectKind::from_tool_name("read")
        );
        assert_eq!(
            EffectKind::from_tool_name("grep"),
            EffectKind::from_tool_name("search")
        );

        let output = json!({
            "status": "completed",
            "output": {
                "path": "src/lib.rs",
                "content": "1|use std;\n2|fn main() {}",
                "startLine": 1,
                "endLine": 2,
            },
        });
        let renamed = effect_of(&effect(
            "read_file",
            json!({"file_path": "src/lib.rs", "offset": 10}),
            output.clone(),
        ));
        let previous = effect_of(&effect(
            "read",
            json!({"path": "src/lib.rs", "offset": 10}),
            output,
        ));
        assert_eq!(renamed.header_text(), previous.header_text());
        assert_eq!(renamed.body[0].text, previous.body[0].text);
        assert_eq!(renamed.collapsed_by_default, previous.collapsed_by_default);
    }

    #[test]
    fn every_audited_effect_kind_keeps_its_reference_header_body_and_collapse() {
        let read = effect_of(&effect(
            "read",
            json!({"path": "src/lib.rs", "offset": 10}),
            json!({
                "status": "completed",
                "output": {"path": "src/lib.rs", "content": "1|use std;\n2|fn main() {}", "startLine": 1, "endLine": 2},
            }),
        ));
        assert_eq!(read.header_text(), "Read 2 lines from lib.rs");
        assert_eq!(read.body[0].text, "use std;");
        assert!(read.collapsed_by_default);

        let write = effect_of(&effect(
            "write",
            json!({"file_path": "docs/readme.md"}),
            json!({"status": "completed", "output": {"path": "docs/readme.md", "content": "hello"}}),
        ));
        assert_eq!(write.header_text(), "Created readme.md");
        assert!(!write.collapsed_by_default);

        let edit = effect_of(&effect(
            "edit",
            json!({"path": "src/lib.rs"}),
            json!({
                "status": "completed",
                "output": {"path": "src/lib.rs", "diff": "--- a\n+++ b\n-old\n+new"},
            }),
        ));
        assert_eq!(edit.header_text(), "Edited lib.rs");
        assert!(!edit.collapsed_by_default);
        assert_eq!(
            edit.body
                .iter()
                .map(|line| (line.text.as_str(), line.style))
                .collect::<Vec<_>>(),
            vec![
                ("--- a", BodyStyle::Muted),
                ("+++ b", BodyStyle::Muted),
                ("-old", BodyStyle::Removed),
                ("+new", BodyStyle::Added),
            ]
        );

        let occurrence_edit = effect_of(&effect(
            "edit",
            json!({"file_path": "src/lib.rs"}),
            json!({
                "status": "completed",
                "output": {
                    "file": "src/lib.rs",
                    "old_string": "old",
                    "new_string": "new",
                    "occurrences": [],
                },
            }),
        ));
        assert_eq!(
            occurrence_edit
                .body
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["- old", "+ new"]
        );

        let search = effect_of(&effect(
            "grep",
            json!({"pattern": "todo"}),
            json!({
                "status": "completed",
                "output": {
                    "matches": "src/lib.rs:4:// todo",
                    "match_count": 1,
                    "pattern": "todo",
                    "was_truncated": false,
                },
            }),
        ));
        assert_eq!(search.header_text(), "Searched todo (1 match)");
        assert_eq!(search.body[0].text, "src/lib.rs:4:// todo");

        let workspace_search = effect_of(&effect(
            "search",
            json!({"pattern": "todo"}),
            json!({
                "status": "completed",
                "output": [{"path": "src/lib.rs", "line": 4, "text": "// todo"}],
            }),
        ));
        assert_eq!(workspace_search.header_text(), "Searched (1 match)");
        assert_eq!(workspace_search.body[0].text, "src/lib.rs:4:// todo");

        let todo = effect_of(&effect(
            "todo",
            json!({"action": "write", "todos": [{}, {}]}),
            json!({
                "status": "completed",
                "output": {"verb": "Updated", "total_count": 2, "todos": [
                    {"status": "completed", "content": "ship"},
                    {"status": "in_progress", "content": "review"},
                ]},
            }),
        ));
        assert_eq!(todo.header_text(), "Updated 2 todos");
        assert_eq!(
            todo.body
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["☐ review", "☑ ship"]
        );

        let question = effect_of(&effect(
            "ask_user_question",
            json!({"questions": [{"question": "Ship it?"}]}),
            json!({
                "status": "completed",
                "output": {"answers": [{"question": "Ship it?", "answer": "yes"}]},
            }),
        ));
        assert_eq!(question.header_text(), "Answered \"Ship it?\" → yes");
        assert!(!question.collapsed_by_default);
        assert!(question.body.is_empty());

        let web = effect_of(&effect(
            "web_search",
            json!({"query": "ratatui"}),
            json!({
                "status": "completed",
                "output": {"query": "ratatui", "answer": "", "sources": [{"title": "Ratatui", "url": "https://ratatui.rs"}]},
            }),
        ));
        assert_eq!(web.header_text(), "Searched 'ratatui' (1 source)");
        assert_eq!(web.body[0].text, "query: ratatui");

        let fetch = effect_of(&effect(
            "web_fetch",
            json!({"url": "https://example.com/page?a=1"}),
            json!({
                "status": "completed",
                "output": {
                    "url": "https://example.com/page?a=1",
                    "content": "body",
                    "content_type": "text/html; charset=utf-8",
                },
            }),
        ));
        assert_eq!(
            fetch.header_text(),
            "Fetched https://example.com/page?a=1 (4 chars, text/html)"
        );
    }

    #[test]
    fn writes_and_edits_break_the_tool_group_that_other_effects_keep() {
        let shell = effect("shell", json!({}), json!({"status": "running"}));
        let edit = effect("edit", json!({}), json!({"status": "running"}));
        assert!(keeps_tool_group(&shell));
        assert!(!keeps_tool_group(&edit));

        let mut reasoning = shell.clone();
        reasoning.kind = TranscriptKind::Reasoning;
        assert!(keeps_tool_group(&reasoning));

        let mut assistant = shell;
        assistant.kind = TranscriptKind::AssistantMessage;
        assert!(!keeps_tool_group(&assistant));
    }

    #[test]
    fn notices_split_into_hook_loop_and_severity_regions() {
        let notice = |detail: Value, level: &str| {
            crate::tui::hydration::published_fixture(
                "notice",
                json!({
                    "type": "notice",
                    "level": level,
                    "message": "message body",
                    "detail": detail,
                }),
            )
        };
        assert_eq!(
            region(&notice(
                json!({"kind": "hook_completed", "hookName": "format", "content": "reformatted", "status": "ok"}),
                "info"
            )),
            Region::Hook {
                icon: "✓",
                line: "[format] reformatted".to_owned()
            }
        );
        assert_eq!(
            region(&notice(
                json!({"kind": "scheduled_loop_fired", "loopId": "loop-1"}),
                "info"
            )),
            Region::Command {
                message: "message body".to_owned()
            }
        );
        // Every notice with no presentation of its own reads at the level the
        // server published it at.
        assert_eq!(
            region(&notice(json!({"kind": "plan_review_ended"}), "error")),
            Region::Notice {
                level: NoticeLevel::Error,
                message: "message body".to_owned()
            }
        );
    }

    #[test]
    fn unknown_tools_and_malformed_details_stay_visible_without_inventing_success() {
        let unknown = effect_of(&effect(
            "mcp__thing__do",
            json!({"target": "x", "count": 2}),
            json!({"status": "completed", "output": {"ok": true}}),
        ));
        assert_eq!(unknown.kind, EffectKind::Tool);
        assert_eq!(
            unknown.header_text(),
            "Ran mcp__thing__do(count=2, target='x')"
        );
        assert_eq!(unknown.body[0].text, "ok: true");

        // An effect entry with no canonical state behind it is presented from
        // its own text, and never invents a settled outcome.
        let restored = TranscriptEntry {
            id: "effect".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "broken".to_owned(),
            status: EntryStatus::Streaming,
            source: EntrySource::Restored,
        };
        let restored = effect_of(&restored);
        assert_eq!(restored.status, EntryStatus::Streaming);
        assert_eq!(restored.indicator, Indicator::Running);
        assert_eq!(restored.kind, EffectKind::Tool);
    }

    #[test]
    fn an_effect_restored_without_its_state_keeps_its_settled_status_and_output() {
        let restored = TranscriptEntry {
            id: "persisted:session:3".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "Tool call-7\nexit status 1".to_owned(),
            status: EntryStatus::Failed,
            source: EntrySource::Restored,
        };
        let restored = effect_of(&restored);
        assert_eq!(restored.status, EntryStatus::Failed);
        assert_eq!(restored.indicator, Indicator::Error);
        assert_eq!(restored.header_text(), "Tool call-7");
        assert_eq!(restored.body[0].text, "exit status 1");
        assert_eq!(restored.stream, None);
    }
}
