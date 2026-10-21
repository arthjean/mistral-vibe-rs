//! What an effect's call header says, and the input it publishes, from the
//! arguments the call was made with.

use std::path::Path;

use serde_json::{Value, json};

use super::readers::{
    FILE_PATH_KEYS, bool_argument, display_file_path, host_of, number_argument,
    optional_string_argument, scratchpad_suffix, string_argument,
};
use super::{
    EffectCallDisplay, RemoteToolOrigin, TodoEffectItem, ToolEffectKind, UserQuestionRequest,
};
use crate::pycompat;

/// Projects raw call arguments onto the fields the kind's input model declares.
///
/// The reference input models forbid surplus fields, so the projection copies
/// what the variant declares and drops the rest instead of forwarding an object
/// a conforming client would reject.
pub(super) fn project_input(kind: ToolEffectKind, arguments: &Value) -> Value {
    match kind {
        ToolEffectKind::Tool => match arguments {
            Value::Object(_) => arguments.clone(),
            // A generic input is not nullable, so a call with no object
            // arguments publishes an empty one rather than a null.
            Value::Null => json!({}),
            value => json!({"value": value}),
        },
        ToolEffectKind::Shell => json!({
            "command": string_argument(arguments, &["command", "cmd"]),
        }),
        ToolEffectKind::FileEdit => json!({
            "filePath": string_argument(arguments, FILE_PATH_KEYS),
            "oldString": string_argument(arguments, &["old_string", "oldString"]),
            "newString": string_argument(arguments, &["new_string", "newString"]),
            "replaceAll": bool_argument(arguments, &["replace_all", "replaceAll"]),
        }),
        ToolEffectKind::FileSearch => json!({
            "pattern": string_argument(arguments, &["pattern", "query"]),
            "path": optional_string_argument(arguments, &["path"]).unwrap_or_else(|| ".".to_owned()),
            "maxMatches": number_argument(arguments, &["max_matches", "maxMatches"]),
        }),
        ToolEffectKind::FileRead => json!({
            "filePath": string_argument(arguments, FILE_PATH_KEYS),
            "offset": number_argument(arguments, &["offset", "startLine", "start_line"]),
            "limit": number_argument(arguments, &["limit", "maxLines", "max_lines"])
                .unwrap_or(DEFAULT_READ_LIMIT),
        }),
        ToolEffectKind::Todo => json!({
            "action": string_argument(arguments, &["action"]),
            "todos": arguments
                .get("todos")
                .and_then(Value::as_array)
                .map(|todos| todos.iter().map(TodoEffectItem::from_argument).collect::<Vec<_>>()),
        }),
        ToolEffectKind::FileWrite => json!({
            "filePath": string_argument(arguments, FILE_PATH_KEYS),
            "content": string_argument(arguments, &["content", "text"]),
        }),
        // Reference `UserQuestionEffectDetail.input` is the validated request,
        // published under its aliases with every default filled in.
        ToolEffectKind::UserQuestion
            if let Some(request) =
                serde_json::from_value::<UserQuestionRequest>(arguments.clone())
                    .ok()
                    .and_then(|request| serde_json::to_value(request).ok()) =>
        {
            request
        }
        ToolEffectKind::UserQuestion => json!({
            "questions": arguments
                .get("questions")
                .filter(|questions| questions.is_array())
                .cloned()
                .unwrap_or_else(|| json!([])),
            "footerNote": optional_string_argument(arguments, &["footer_note", "footerNote"]),
        }),
        ToolEffectKind::WebSearch => json!({
            "query": string_argument(arguments, &["query"]),
        }),
        ToolEffectKind::WebFetch => json!({
            "url": string_argument(arguments, &["url"]),
            "timeout": number_argument(arguments, &["timeout", "timeoutSeconds"]),
        }),
        ToolEffectKind::Skill => json!({
            "name": string_argument(arguments, &["name"]),
        }),
        ToolEffectKind::Subagent => json!({
            "task": string_argument(arguments, &["task", "prompt"]),
            "agent": string_argument(arguments, &["agent", "subagent_type"]),
        }),
        ToolEffectKind::Worktree => json!({
            "name": string_argument(arguments, &["name"]),
            "branch": string_argument(arguments, &["branch"]),
            "path": string_argument(arguments, &["path"]),
        }),
        // Reference `ProcessEffectDetail.input` is any JSON value, and no tool
        // of this port publishes a scratchpad call, so its arguments stand as
        // they came.
        ToolEffectKind::Process | ToolEffectKind::Scratchpad => arguments.clone(),
    }
}

/// The reference default read window, published when a call did not name one.
const DEFAULT_READ_LIMIT: u64 = 2000;

/// The header a client shows while the call runs.
///
/// A proxied call takes none of the per-kind branches: the reference presents
/// it through the base class, whose display is the published name alone.
pub(super) fn call_display(
    kind: ToolEffectKind,
    tool_name: &str,
    arguments: &Value,
    remote: Option<&RemoteToolOrigin>,
    wire_order: &[String],
    working_directory: Option<&Path>,
) -> EffectCallDisplay {
    if let Some(remote) = remote {
        let mut display = EffectCallDisplay {
            summary: tool_name.to_owned(),
            status_text: remote.status_text(tool_name),
            ..EffectCallDisplay::default()
        };
        display.fill_defaults();
        return display;
    }
    let (verb, settled_verb, summary, message) = if kind == ToolEffectKind::Todo {
        todo_call_display(arguments)
    } else {
        let (verb, settled_verb) = kind.verbs();
        let (summary, message) =
            call_summary(kind, tool_name, arguments, wire_order, working_directory);
        (verb.to_owned(), settled_verb.to_owned(), summary, message)
    };
    // The three file tools mark a path inside a session scratchpad, which is
    // the only suffix a call header carries.
    let suffix = match kind {
        ToolEffectKind::FileRead | ToolEffectKind::FileWrite | ToolEffectKind::FileEdit => {
            scratchpad_suffix(&string_argument(arguments, FILE_PATH_KEYS)).to_owned()
        }
        _ => String::new(),
    };
    // A call whose arguments never arrived would otherwise render a bare verb.
    let message = if message.is_empty() {
        summary.clone()
    } else {
        message
    };
    let mut display = EffectCallDisplay {
        summary,
        content: call_content(kind, arguments),
        suffix,
        verb,
        // A kind that names its own subject names it in both headers, which is
        // what keeps a settled call from renaming what it was acting on. Only a
        // presentation that names none falls back to the summary below.
        message: Some(message.clone()),
        settled_verb,
        settled_message: Some(message),
        status_text: kind.status_text().to_owned(),
    };
    display.fill_defaults();
    display
}

/// The body a call header shows under its summary. Reference
/// `WriteFile.format_call_display` shows what is being written and
/// `Edit.format_call_display` the two strings in Python's literal syntax;
/// every other tool shows none. Arguments its model would refuse fall back to
/// the generic display, which shows none either.
fn call_content(kind: ToolEffectKind, arguments: &Value) -> Option<String> {
    optional_string_argument(arguments, FILE_PATH_KEYS)?;
    match kind {
        ToolEffectKind::FileWrite => optional_string_argument(arguments, &["content"]),
        ToolEffectKind::FileEdit => Some(format!(
            "old_string: {}\nnew_string: {}",
            pycompat::repr_str(&optional_string_argument(arguments, &["old_string"])?),
            pycompat::repr_str(&optional_string_argument(arguments, &["new_string"])?)
        )),
        _ => None,
    }
}

fn call_summary(
    kind: ToolEffectKind,
    tool_name: &str,
    arguments: &Value,
    wire_order: &[String],
    working_directory: Option<&Path>,
) -> (String, String) {
    match kind {
        ToolEffectKind::Shell => {
            let command = string_argument(arguments, &["command", "cmd"]);
            (format!("bash: {command}"), command)
        }
        ToolEffectKind::FileRead => {
            let mut message = display_file_path(
                &string_argument(arguments, FILE_PATH_KEYS),
                working_directory,
            );
            let mut extras = Vec::new();
            if let Some(offset) = number_argument(arguments, &["offset", "startLine", "start_line"])
                && offset > 0
            {
                extras.push(format!("from line {offset}"));
            }
            // Reference `ReadFileTool.format_call_display` names the window only
            // when it differs from the default one, so a call that spells the
            // default out reads the same as one that leaves it implied.
            if let Some(limit) = number_argument(arguments, &["limit", "maxLines", "max_lines"])
                && limit != DEFAULT_READ_LIMIT
            {
                extras.push(format!("limit {limit} lines"));
            }
            if !extras.is_empty() {
                message = format!("{message} ({})", extras.join(", "));
            }
            (format!("Reading {message}"), message)
        }
        ToolEffectKind::FileWrite => {
            let shown = display_file_path(
                &string_argument(arguments, FILE_PATH_KEYS),
                working_directory,
            );
            (format!("Writing {shown}"), shown)
        }
        ToolEffectKind::FileEdit => {
            let shown = display_file_path(
                &string_argument(arguments, FILE_PATH_KEYS),
                working_directory,
            );
            (format!("Editing {shown}"), shown)
        }
        ToolEffectKind::FileSearch => {
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
        ToolEffectKind::UserQuestion => {
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
        ToolEffectKind::WebSearch => {
            let query = string_argument(arguments, &["query"]);
            let message = format!("the web: '{query}'");
            (format!("Searching {message}"), message)
        }
        ToolEffectKind::WebFetch => {
            let message = host_of(&string_argument(arguments, &["url"]));
            (format!("Fetching: {message}"), message)
        }
        ToolEffectKind::Skill => {
            let message = format!("skill: {}", string_argument(arguments, &["name"]));
            (format!("Loading {message}"), message)
        }
        ToolEffectKind::Subagent => {
            let message = format!(
                "{} agent: {}",
                string_argument(arguments, &["agent", "subagent_type"]),
                string_argument(arguments, &["task", "prompt"])
            );
            (format!("Running {message}"), message)
        }
        // A tool with no presentation of its own shows its arguments, and the
        // summary is all there is to show. No tool of this port publishes the
        // worktree, process or scratchpad kinds, so a call under any of them
        // reads the same way.
        ToolEffectKind::Tool
        | ToolEffectKind::Todo
        | ToolEffectKind::Worktree
        | ToolEffectKind::Process
        | ToolEffectKind::Scratchpad => {
            let summary = generic_call_summary(tool_name, arguments, wire_order);
            (summary.clone(), summary)
        }
    }
}

/// The todo effect names its own verbs: the same kind reads, writes, or fails
/// to recognize its action.
fn todo_call_display(arguments: &Value) -> (String, String, String, String) {
    match string_argument(arguments, &["action"]).as_str() {
        "read" => (
            "Retrieving".to_owned(),
            "Retrieved".to_owned(),
            "Reading todos".to_owned(),
            "todos".to_owned(),
        ),
        "write" => {
            let count = arguments
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            (
                "Updating".to_owned(),
                "Updated".to_owned(),
                format!("Writing {count} todos"),
                format!("{count} todos"),
            )
        }
        action => (
            "Running".to_owned(),
            "Ran".to_owned(),
            format!("Unknown action: {action}"),
            format!("unknown todo action: {action}"),
        ),
    }
}

/// The fallback for tools without their own presentation: the first three
/// arguments, in the order the call carried them.
pub(super) fn generic_call_summary(
    tool_name: &str,
    arguments: &Value,
    wire_order: &[String],
) -> String {
    let object = arguments.as_object();
    let ordered = if wire_order.is_empty() {
        object
            .into_iter()
            .flatten()
            .map(|(key, value)| (key.as_str(), value))
            .collect::<Vec<_>>()
    } else {
        wire_order
            .iter()
            .filter_map(|key| {
                object.and_then(|fields| fields.get(key).map(|value| (key.as_str(), value)))
            })
            .collect()
    };
    let rendered = ordered
        .into_iter()
        .take(3)
        .map(|(key, value)| format!("{key}={}", pycompat::repr_json(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{tool_name}({rendered})")
}
