//! Semantic projection of canonical history into reference transcript regions.
//!
//! The reference renders history by meaning: every effect carries a call
//! display (`verb`, `message`, `suffix`) and, once settled, a result display
//! whose `success` flag is authoritative. This module reproduces that contract
//! as pure functions over an already projected [`TranscriptEntry`], so the
//! renderer never re-derives status from the generation flag and a failed,
//! cancelled, or skipped effect can never be presented as a success.

use serde_json::Value;

use super::state::{EntryStatus, TranscriptEntry, TranscriptKind};

/// Reference `ToolEffectKind`. The variant decides the header verbs, the
/// result body, the default collapse, and whether the effect joins the
/// surrounding tool group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    Tool,
    Shell,
    FileEdit,
    FileSearch,
    FileRead,
    Todo,
    FileWrite,
    UserQuestion,
    WebSearch,
    WebFetch,
    Skill,
    Subagent,
}

impl EffectKind {
    #[must_use]
    pub fn from_tool_name(name: &str) -> Self {
        match name {
            "shell" | "bash" | "windows_shell" | "experimental_bash" | "git_bash" => Self::Shell,
            "edit" | "patch" => Self::FileEdit,
            "search" | "grep" => Self::FileSearch,
            "read" | "read_file" => Self::FileRead,
            "todo" => Self::Todo,
            "write" | "write_file" => Self::FileWrite,
            "ask_user_question" => Self::UserQuestion,
            "web_search" => Self::WebSearch,
            "web_fetch" => Self::WebFetch,
            "skill" => Self::Skill,
            "task" => Self::Subagent,
            _ => Self::Tool,
        }
    }

    /// Reference `EFFECT_WIDGETS[...].result.COLLAPSIBLE`: diff-shaped and
    /// question results always render in full, everything else folds into its
    /// header until the operator expands it.
    #[must_use]
    pub fn is_collapsible(self) -> bool {
        !matches!(self, Self::FileEdit | Self::FileWrite | Self::UserQuestion)
    }

    /// Reference `_NON_GROUPED_EFFECT_KINDS`: writes and edits break the
    /// current tool group and stand on their own.
    #[must_use]
    pub fn joins_tool_group(self) -> bool {
        !matches!(self, Self::FileEdit | Self::FileWrite)
    }

    /// Reference `get_status_text`, shown by the loading indicator.
    #[must_use]
    pub fn status_text(self) -> &'static str {
        match self {
            Self::Tool => "Running tool",
            Self::Shell => "Running command",
            Self::FileEdit => "Editing files",
            Self::FileSearch => "Searching files",
            Self::FileRead => "Reading file",
            Self::Todo => "Managing todos",
            Self::FileWrite => "Writing file",
            Self::UserQuestion => "Waiting for user input",
            Self::WebSearch => "Searching the web",
            Self::WebFetch => "Fetching page",
            Self::Skill => "Loading skill",
            Self::Subagent => "Running subagent",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Shell => "shell",
            Self::FileEdit => "file_edit",
            Self::FileSearch => "file_search",
            Self::FileRead => "file_read",
            Self::Todo => "todo",
            Self::FileWrite => "file_write",
            Self::UserQuestion => "user_question",
            Self::WebSearch => "web_search",
            Self::WebFetch => "web_fetch",
            Self::Skill => "skill",
            Self::Subagent => "subagent",
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

impl NoticeLevel {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("warning") => Self::Warning,
            Some("error") => Self::Error,
            _ => Self::Info,
        }
    }
}

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
    Callback { title: String, detail: String },
    Compaction { message: String },
    Checkpoint { message: String },
    Hook { icon: &'static str, line: String },
    Command { message: String },
    Notice { level: NoticeLevel, message: String },
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
            if entry.details.get("kind").and_then(Value::as_str) == Some("compaction") {
                Region::Compaction {
                    message: entry.text.clone(),
                }
            } else {
                Region::Checkpoint {
                    message: entry.text.clone(),
                }
            }
        }
        TranscriptKind::Notice => notice_region(entry),
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
        TranscriptKind::Effect => EffectKind::from_tool_name(
            entry
                .details
                .pointer("/detail/toolName")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .joins_tool_group(),
        TranscriptKind::Notice => entry
            .details
            .pointer("/detail/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("hook")),
        _ => false,
    }
}

fn notice_region(entry: &TranscriptEntry) -> Region {
    let detail = entry.details.get("detail").unwrap_or(&Value::Null);
    let kind = detail
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "hook_completed" | "hook_started" | "hook_run_started" | "hook_run_completed" => {
            let name = detail
                .get("hookName")
                .and_then(Value::as_str)
                .unwrap_or("hook");
            let content = detail
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(entry.text.as_str());
            Region::Hook {
                icon: hook_icon(detail.get("status").and_then(Value::as_str)),
                line: format!("[{name}] {content}"),
            }
        }
        "scheduled_loop_fired" => Region::Command {
            message: entry.text.clone(),
        },
        _ => Region::Notice {
            level: NoticeLevel::parse(entry.details.get("level").and_then(Value::as_str)),
            message: entry.text.clone(),
        },
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
        collapsed_by_default: EffectKind::Tool.is_collapsible(),
        stream: None,
        body: text_lines(output),
    }
}

/// Reference `_HOOK_SEVERITY_ICONS`, defaulting to the warning icon.
fn hook_icon(severity: Option<&str>) -> &'static str {
    match severity {
        Some("ok") => "✓",
        Some("error") => "✗",
        _ => "⚠",
    }
}

fn effect_region(entry: &TranscriptEntry) -> EffectRegion {
    // An entry restored from saved history carries no canonical effect state:
    // its own settled status and text are then the only truth about it.
    let Some(state) = entry.details.get("state") else {
        return restored_effect_region(entry);
    };
    let detail = entry.details.get("detail").unwrap_or(&Value::Null);
    let tool_name = detail
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or(entry.text.lines().next().unwrap_or_default());
    let kind = EffectKind::from_tool_name(tool_name);
    let arguments = effect_arguments(detail);
    let call = call_display(kind, tool_name, &arguments);
    let status = effect_status(state);
    let settled = settled_display(kind, tool_name, &call, state, status);
    let (verb, message, suffix) = settled.as_ref().map_or_else(
        || (call.verb.to_owned(), call.message.clone(), String::new()),
        |display| {
            (
                display.verb.clone(),
                display.message.clone(),
                display.suffix.clone(),
            )
        },
    );
    EffectRegion {
        kind,
        status,
        indicator: indicator(status, settled.as_ref()),
        verb,
        message,
        suffix,
        collapsed_by_default: kind.is_collapsible(),
        stream: running_stream(state, status),
        body: effect_body(kind, state, status, settled.as_ref()),
    }
}

/// Reference `ToolCallMessage.set_stream_message`: streaming output is only
/// shown while the effect is still running.
fn running_stream(state: &Value, status: EntryStatus) -> Option<String> {
    if status.is_terminal() {
        return None;
    }
    let output = state
        .get("outputText")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('\n');
    let last = output.lines().next_back().unwrap_or_default();
    (!last.is_empty()).then(|| format!("→ {last}"))
}

fn effect_arguments(detail: &Value) -> Value {
    match detail.get("arguments") {
        Some(Value::String(encoded)) => {
            serde_json::from_str(encoded).unwrap_or_else(|_| Value::String(encoded.clone()))
        }
        Some(value) => value.clone(),
        None => detail.get("input").cloned().unwrap_or(Value::Null),
    }
}

/// Reference `PublicEffectState` discriminant, which is authoritative over the
/// entry generation flag.
fn effect_status(state: &Value) -> EntryStatus {
    match state.get("status").and_then(Value::as_str) {
        Some("pending") => EntryStatus::Pending,
        Some("blocked") => EntryStatus::Blocked,
        Some("completed") => EntryStatus::Completed,
        Some("failed") => EntryStatus::Failed,
        Some("cancelled") => EntryStatus::Cancelled,
        Some("skipped") => EntryStatus::Skipped,
        _ => EntryStatus::Streaming,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultDisplay {
    pub success: bool,
    pub verb: String,
    pub message: String,
    pub suffix: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallDisplay {
    /// The collapsed one-line form.
    summary: String,
    /// What is being acted on. The live and settled headers share it, so a
    /// finished call never renames its own subject.
    message: String,
    verb: &'static str,
    settled_verb: &'static str,
}

/// Reference `ToolUIDataAdapter.get_call_display` verbs. `Todo` is absent
/// because its `action` argument, not its kind, decides what is happening.
const EFFECT_VERBS: &[(EffectKind, &str, &str)] = &[
    (EffectKind::Shell, "Running", "Ran"),
    (EffectKind::FileRead, "Reading", "Read"),
    (EffectKind::FileWrite, "Creating", "Created"),
    (EffectKind::FileEdit, "Editing", "Edited"),
    (EffectKind::FileSearch, "Searching", "Searched"),
    (EffectKind::UserQuestion, "Asking", "Asked"),
    (EffectKind::WebSearch, "Searching", "Searched"),
    (EffectKind::WebFetch, "Fetching", "Fetched"),
    (EffectKind::Skill, "Loading", "Loaded"),
    (EffectKind::Subagent, "Running", "Ran"),
];

/// Reference fallback for a tool with no presentation of its own.
const DEFAULT_VERBS: (&str, &str) = ("Running", "Ran");

fn effect_verbs(kind: EffectKind) -> (&'static str, &'static str) {
    EFFECT_VERBS
        .iter()
        .find(|(candidate, _, _)| *candidate == kind)
        .map_or(DEFAULT_VERBS, |(_, verb, settled)| (*verb, *settled))
}

/// Reference `ToolUIDataAdapter.get_call_display`, including the summary
/// fallback it applies to a call whose arguments are missing.
fn call_display(kind: EffectKind, tool_name: &str, arguments: &Value) -> CallDisplay {
    if kind == EffectKind::Todo {
        return todo_call_display(arguments);
    }
    let (verb, settled_verb) = effect_verbs(kind);
    let (summary, message) = match kind {
        EffectKind::Shell => {
            let command = string_argument(arguments, &["command", "cmd"]);
            (format!("bash: {command}"), command)
        }
        EffectKind::FileRead => {
            let mut message = string_argument(arguments, &["file_path", "filePath", "path"]);
            let mut extras = Vec::new();
            if let Some(offset) = number_argument(arguments, &["offset", "startLine", "start_line"])
                && offset > 0
            {
                extras.push(format!("from line {offset}"));
            }
            if let Some(limit) = number_argument(arguments, &["limit", "maxLines", "max_lines"]) {
                extras.push(format!("limit {limit} lines"));
            }
            if !extras.is_empty() {
                message = format!("{message} ({})", extras.join(", "));
            }
            (format!("Reading {message}"), message)
        }
        EffectKind::FileWrite => {
            let path = string_argument(arguments, &["file_path", "filePath", "path"]);
            (format!("Writing {path}"), path)
        }
        EffectKind::FileEdit => {
            let name = file_name(&string_argument(
                arguments,
                &["file_path", "filePath", "path"],
            ));
            (format!("Editing {name}"), name)
        }
        EffectKind::FileSearch => {
            let pattern = string_argument(arguments, &["pattern", "query"]);
            let mut message = format!("'{pattern}'");
            let path = string_argument(arguments, &["path"]);
            if !path.is_empty() && path != "." {
                message = format!("{message} in {path}");
            }
            if let Some(max) = number_argument(arguments, &["max_matches", "maxMatches"]) {
                message = format!("{message} (max {max} matches)");
            }
            (format!("Grepping {message}"), message)
        }
        EffectKind::UserQuestion => {
            let questions = arguments
                .get("questions")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            match questions {
                [only] => {
                    let message = only
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    (format!("Asking: {message}"), message)
                }
                many => {
                    let message = format!("{} questions", many.len());
                    (format!("Asking {message}"), message)
                }
            }
        }
        EffectKind::WebSearch => {
            let query = string_argument(arguments, &["query"]);
            let message = format!("the web: '{query}'");
            (format!("Searching {message}"), message)
        }
        EffectKind::WebFetch => {
            let message = host_of(&string_argument(arguments, &["url"]));
            (format!("Fetching: {message}"), message)
        }
        EffectKind::Skill => {
            let message = format!("skill: {}", string_argument(arguments, &["name"]));
            (format!("Loading {message}"), message)
        }
        EffectKind::Subagent => {
            let message = format!(
                "{} agent: {}",
                string_argument(arguments, &["agent"]),
                string_argument(arguments, &["task"])
            );
            (format!("Running {message}"), message)
        }
        // A tool with no presentation of its own shows its arguments, and the
        // summary is all there is to show.
        EffectKind::Tool | EffectKind::Todo => {
            let summary = generic_call_summary(tool_name, arguments);
            (summary.clone(), summary)
        }
    };
    // A call whose arguments never arrived would otherwise render a bare verb.
    let message = if message.is_empty() {
        summary.clone()
    } else {
        message
    };
    CallDisplay {
        summary,
        message,
        verb,
        settled_verb,
    }
}

/// The todo effect names its own verbs: the same kind reads, writes, or fails
/// to recognise its action.
fn todo_call_display(arguments: &Value) -> CallDisplay {
    match string_argument(arguments, &["action"]).as_str() {
        "read" => CallDisplay {
            summary: "Reading todos".to_owned(),
            message: "todos".to_owned(),
            verb: "Retrieving",
            settled_verb: "Retrieved",
        },
        "write" => {
            let count = arguments
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            CallDisplay {
                summary: format!("Writing {count} todos"),
                message: format!("{count} todos"),
                verb: "Updating",
                settled_verb: "Updated",
            }
        }
        action => CallDisplay {
            summary: format!("Unknown action: {action}"),
            message: format!("unknown todo action: {action}"),
            verb: "Running",
            settled_verb: "Ran",
        },
    }
}

/// Reference `ToolUIDataAdapter.get_call_display` fallback for tools without
/// their own presentation: the first three arguments, in schema order.
fn generic_call_summary(tool_name: &str, arguments: &Value) -> String {
    let rendered = arguments
        .as_object()
        .into_iter()
        .flatten()
        .take(3)
        .map(|(key, value)| match value {
            Value::String(text) => format!("{key}='{text}'"),
            value => format!("{key}={value}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{tool_name}({rendered})")
}

/// Reference `ToolCallMessage._settled_display` combined with
/// `_failed_header_display` and `ToolResultMessage._get_result_parts`.
fn settled_display(
    kind: EffectKind,
    tool_name: &str,
    call: &CallDisplay,
    state: &Value,
    status: EntryStatus,
) -> Option<ResultDisplay> {
    match status {
        EntryStatus::Pending | EntryStatus::Streaming | EntryStatus::Blocked => None,
        EntryStatus::Failed => Some(ResultDisplay {
            success: false,
            verb: call.settled_verb.to_owned(),
            message: call.message.clone(),
            suffix: String::new(),
            warnings: Vec::new(),
        }),
        EntryStatus::Cancelled | EntryStatus::Skipped => Some(ResultDisplay {
            success: false,
            verb: String::new(),
            message: format!("{tool_name}: skipped"),
            suffix: String::new(),
            warnings: Vec::new(),
        }),
        EntryStatus::Completed => Some(completed_display(kind, call, state)),
    }
}

/// A server-provided result display is authoritative, exactly as the reference
/// widgets consume `state.display` without re-deriving it.
fn explicit_result_display(state: &Value) -> Option<ResultDisplay> {
    let display = state.get("display")?;
    let success = display.get("success")?.as_bool()?;
    Some(ResultDisplay {
        success,
        verb: display
            .get("verb")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        message: display
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        suffix: display
            .get("suffix")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        warnings: display
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    })
}

fn completed_display(kind: EffectKind, call: &CallDisplay, state: &Value) -> ResultDisplay {
    if let Some(display) = explicit_result_display(state) {
        return display;
    }
    let output = state.get("output").unwrap_or(&Value::Null);
    let warnings = output
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let (verb, message, suffix) = match kind {
        EffectKind::Shell => {
            let command = string_argument(output, &["command"]);
            (
                "Ran".to_owned(),
                if command.is_empty() {
                    call.message.clone()
                } else {
                    command
                },
                String::new(),
            )
        }
        EffectKind::FileRead => {
            let lines = read_line_count(output);
            let word = if lines == 1 { "line" } else { "lines" };
            let name = file_name(&string_argument(output, &["file_path", "filePath", "path"]));
            (
                "Read".to_owned(),
                format!("{lines} {word} from {name}"),
                if read_was_truncated(output, lines) {
                    "(truncated)".to_owned()
                } else {
                    String::new()
                },
            )
        }
        EffectKind::FileWrite => (
            "Created".to_owned(),
            file_name(&string_argument(output, &["file_path", "filePath", "path"])),
            String::new(),
        ),
        EffectKind::FileEdit => (
            "Edited".to_owned(),
            file_name(&string_argument(
                output,
                &["file", "file_path", "filePath", "path"],
            )),
            String::new(),
        ),
        EffectKind::FileSearch => {
            let count = search_match_count(output);
            let word = if count == 1 { "match" } else { "matches" };
            let pattern = string_argument(output, &["pattern"]);
            (
                "Searched".to_owned(),
                if pattern.is_empty() {
                    format!("({count} {word})")
                } else {
                    format!("{pattern} ({count} {word})")
                },
                if truncated_flag(output) {
                    "(truncated)".to_owned()
                } else {
                    String::new()
                },
            )
        }
        EffectKind::Todo => {
            let verb = string_argument(output, &["verb"]);
            let count = output
                .get("total_count")
                .or_else(|| output.get("totalCount"))
                .and_then(Value::as_u64)
                .map_or_else(
                    || todo_items(output).len(),
                    |count| usize::try_from(count).unwrap_or(usize::MAX),
                );
            (
                if verb.is_empty() {
                    "Updated".to_owned()
                } else {
                    verb
                },
                format!("{count} todos"),
                String::new(),
            )
        }
        EffectKind::UserQuestion => {
            if output
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return ResultDisplay {
                    success: false,
                    verb: "Cancelled".to_owned(),
                    message: "by user".to_owned(),
                    suffix: String::new(),
                    warnings,
                };
            }
            (
                "Answered".to_owned(),
                answered_message(output),
                String::new(),
            )
        }
        EffectKind::WebSearch => {
            let sources = output
                .get("sources")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let plural = if sources == 1 { "" } else { "s" };
            (
                "Searched".to_owned(),
                format!(
                    "'{}' ({sources} source{plural})",
                    string_argument(output, &["query"])
                ),
                String::new(),
            )
        }
        EffectKind::WebFetch => {
            let content_length = output
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let content_type = string_argument(output, &["content_type", "contentType"]);
            (
                "Fetched".to_owned(),
                format!(
                    "{} ({} chars, {})",
                    string_argument(output, &["url"]),
                    grouped_thousands(content_length),
                    content_type.split(';').next().unwrap_or_default()
                ),
                if truncated_flag(output) {
                    "(truncated)".to_owned()
                } else {
                    String::new()
                },
            )
        }
        EffectKind::Skill => ("Loaded".to_owned(), call.message.clone(), String::new()),
        EffectKind::Subagent => ("Completed".to_owned(), call.message.clone(), String::new()),
        EffectKind::Tool => ("Ran".to_owned(), call.message.clone(), String::new()),
    };
    ResultDisplay {
        success: true,
        verb,
        message,
        suffix,
        warnings,
    }
}

/// Reference `ToolCallMessage.__init__` and `ToolResultMessage.on_mount`: the
/// nested result display decides the indicator, so a completed effect whose
/// display reports failure still shows the error glyph.
fn indicator(status: EntryStatus, settled: Option<&ResultDisplay>) -> Indicator {
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
    state: &Value,
    status: EntryStatus,
    settled: Option<&ResultDisplay>,
) -> Vec<BodyLine> {
    match status {
        EntryStatus::Pending | EntryStatus::Streaming | EntryStatus::Blocked => Vec::new(),
        EntryStatus::Failed => {
            let message = state
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown failure");
            vec![BodyLine::styled(
                format!("Error: {message}"),
                BodyStyle::Error,
            )]
        }
        EntryStatus::Cancelled | EntryStatus::Skipped => {
            let reason = state
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            vec![BodyLine::styled(
                format!("Skipped: {reason}"),
                BodyStyle::Muted,
            )]
        }
        EntryStatus::Completed => {
            let mut lines = settled
                .into_iter()
                .flat_map(|display| display.warnings.iter())
                .map(|warning| BodyLine::styled(format!("⚠ {warning}"), BodyStyle::Warning))
                .collect::<Vec<_>>();
            lines.extend(completed_body(kind, state));
            lines
        }
    }
}

fn completed_body(kind: EffectKind, state: &Value) -> Vec<BodyLine> {
    let output = state.get("output").unwrap_or(&Value::Null);
    let output_text = state
        .get("outputText")
        .and_then(Value::as_str)
        .unwrap_or_default();
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

fn read_line_count(output: &Value) -> u64 {
    if let Some(count) = output
        .get("num_lines")
        .or_else(|| output.get("numLines"))
        .and_then(Value::as_u64)
    {
        return count;
    }
    let start = output
        .get("startLine")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let end = output
        .get("endLine")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if end >= start && start > 0 {
        return end.saturating_sub(start).saturating_add(1);
    }
    output
        .get("content")
        .and_then(Value::as_str)
        .map_or(0, |content| content.lines().count() as u64)
}

fn truncated_flag(output: &Value) -> bool {
    output
        .get("was_truncated")
        .or_else(|| output.get("wasTruncated"))
        .or_else(|| output.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Reference read suffix: the flag, or a window that stops short of the file.
fn read_was_truncated(output: &Value, lines: u64) -> bool {
    if truncated_flag(output) {
        return true;
    }
    let Some(total) = output
        .get("total_lines")
        .or_else(|| output.get("totalLines"))
        .and_then(Value::as_u64)
    else {
        return false;
    };
    let start = output
        .get("start_line")
        .or_else(|| output.get("startLine"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    start.saturating_add(lines).saturating_sub(1) < total
}

/// Reference `f"{value:,}"`.
fn grouped_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn search_match_count(output: &Value) -> usize {
    if let Some(count) = output
        .get("match_count")
        .or_else(|| output.get("matchCount"))
        .and_then(Value::as_u64)
    {
        return usize::try_from(count).unwrap_or(usize::MAX);
    }
    match output {
        Value::Array(matches) => matches.len(),
        value => value
            .get("matches")
            .and_then(Value::as_str)
            .map_or(0, |matches| matches.lines().count()),
    }
}

/// Reference `AskUserQuestionUIData.get_result_display`.
fn answered_message(output: &Value) -> String {
    output
        .get("answers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|answer| {
            let question = answer
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let other = answer
                .get("is_other")
                .or_else(|| answer.get("isOther"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = answer
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!(
                "\"{question}\" → {}{text}",
                if other { "(Other) " } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
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

fn number_argument(arguments: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_u64))
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn host_of(url: &str) -> String {
    let without_scheme = url
        .split_once("://")
        .map_or(url, |(_, remainder)| remainder);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    if host.is_empty() {
        url.chars().take(50).collect()
    } else {
        host.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    fn effect(tool: &str, arguments: Value, state: Value) -> TranscriptEntry {
        TranscriptEntry {
            id: "effect".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: tool.to_owned(),
            status: EntryStatus::Streaming,
            details: json!({
                "type": "effect",
                "detail": {"toolName": tool, "arguments": arguments},
                "state": state,
            }),
        }
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
                directory.path(),
                true,
                &registry,
                policy,
                Arc::new(DenyEverything),
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
        let notice = |detail: Value, level: &str| TranscriptEntry {
            id: "notice".to_owned(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: "message body".to_owned(),
            status: EntryStatus::Completed,
            details: json!({"type": "notice", "level": level, "detail": detail}),
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
            region(&notice(json!({"kind": "scheduled_loop_fired"}), "info")),
            Region::Command {
                message: "message body".to_owned()
            }
        );
        assert_eq!(
            region(&notice(json!({"kind": "turn_failed"}), "error")),
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

        let malformed = TranscriptEntry {
            id: "effect".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "broken".to_owned(),
            status: EntryStatus::Streaming,
            details: Value::Null,
        };
        let malformed = effect_of(&malformed);
        assert_eq!(malformed.status, EntryStatus::Streaming);
        assert_eq!(malformed.indicator, Indicator::Running);
        assert_eq!(malformed.kind, EffectKind::Tool);
    }

    #[test]
    fn an_effect_restored_without_its_state_keeps_its_settled_status_and_output() {
        let restored = TranscriptEntry {
            id: "persisted:session:3".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "Tool call-7\nexit status 1".to_owned(),
            status: EntryStatus::Failed,
            details: json!({"source": "history/list"}),
        };
        let restored = effect_of(&restored);
        assert_eq!(restored.status, EntryStatus::Failed);
        assert_eq!(restored.indicator, Indicator::Error);
        assert_eq!(restored.header_text(), "Tool call-7");
        assert_eq!(restored.body[0].text, "exit status 1");
        assert_eq!(restored.stream, None);
    }
}
