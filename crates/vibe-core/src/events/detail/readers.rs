//! Reading a call's arguments and a tool's output, the way every display of
//! an effect reads them.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::scratchpad::SCRATCHPAD_PREFIX;
use crate::workspace::tool_path;

/// The spellings a call or a result names a file under.
///
/// Reference argument models are snake_case and the wire projection is
/// camelCase, so a value read back from either side answers to both, and a tool
/// that names the field `path` answers to that too. Naming the list once is
/// what keeps two readers of the same field from accepting different spellings.
pub(super) const FILE_PATH_KEYS: &[&str] = &["file_path", "filePath", "path"];

/// The same, for a result the edit tool wrote: its `EditResult` publishes the
/// path under `file`, which is the spelling read first.
pub(super) const EDITED_FILE_PATH_KEYS: &[&str] = &["file", "file_path", "filePath", "path"];

pub(super) fn string_argument(arguments: &Value, keys: &[&str]) -> String {
    optional_string_argument(arguments, keys).unwrap_or_default()
}

pub(super) fn optional_string_argument(arguments: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

pub(super) fn number_argument(arguments: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_u64))
}

/// The vocabulary member `key` names, or the member the reference defaults to.
pub(super) fn enum_argument<T: Default + serde::de::DeserializeOwned>(
    arguments: &Value,
    key: &str,
) -> T {
    arguments
        .get(key)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

pub(super) fn bool_argument(arguments: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_bool))
        .unwrap_or(false)
}

/// A file path as a header shows it.
///
/// Reference `display_file_path` (`vibe/core/tools/utils.py:116-133`): the
/// path is expanded and normalized without following links, then shown
/// relative to the session's directory when it sits under it, as that
/// directory's own name when it is the directory itself, and as the normalized
/// absolute path everywhere else. An argument naming no path shows nothing,
/// where the reference would name the directory: a call only reaches this with
/// an empty path when its arguments never arrived, which it presents
/// differently anyway.
pub(super) fn display_file_path(path: &str, working_directory: Option<&Path>) -> String {
    if path.is_empty() {
        return String::new();
    }
    let Some(root) = working_directory
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
    else {
        return path.to_owned();
    };
    let root = lexically_normalized(&tool_path::expanded(&root));
    let requested = tool_path::expanded(Path::new(path));
    let target = lexically_normalized(&if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    });
    match target.strip_prefix(&root) {
        Ok(relative) if relative.as_os_str().is_empty() => target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.display().to_string()),
        Ok(relative) => relative.display().to_string(),
        Err(_) => target.display().to_string(),
    }
}

/// `os.path.normpath`: `.` dropped, `..` folded into its parent, and a `..`
/// above the root kept at the root, all without touching the filesystem.
fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// The mark a file header carries when its path sits in a session scratchpad.
///
/// Reference `is_scratchpad_display_path` (`vibe/core/scratchpad.py:50-55`)
/// resolves the path against the process directory, following links, and asks
/// whether any component carries the scratchpad prefix, so any session's
/// scratchpad is marked and not only this one's.
pub(super) fn scratchpad_suffix(path: &str) -> &'static str {
    let expanded = tool_path::expanded(Path::new(path));
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        match std::env::current_dir() {
            Ok(directory) => directory.join(expanded),
            Err(_) => return "",
        }
    };
    let in_scratchpad =
        tool_path::resolved(&absolute)
            .components()
            .any(|component| match component {
                Component::Normal(name) => name.to_string_lossy().starts_with(SCRATCHPAD_PREFIX),
                _ => false,
            });
    if in_scratchpad { "(scratchpad)" } else { "" }
}

pub(super) fn host_of(url: &str) -> String {
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

pub(super) fn read_line_count(output: &Value) -> u64 {
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

pub(super) fn truncated_flag(output: &Value) -> bool {
    output
        .get("was_truncated")
        .or_else(|| output.get("wasTruncated"))
        .or_else(|| output.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The read suffix: the flag, or a window that stops short of the file.
pub(super) fn read_was_truncated(output: &Value, lines: u64) -> bool {
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

pub(super) fn search_match_count(output: &Value) -> usize {
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

pub(super) fn answered_message(output: &Value) -> String {
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

/// The tags reference `TaggedText` recognizes (`vibe/core/utils/tags.py`).
const KNOWN_TAGS: [&str; 4] = [
    "user_cancellation",
    "tool_error",
    "vibe_stop_event",
    "vibe_warning",
];

/// Reference `TaggedText.from_string`: every known tag pair, scanned left to
/// right, is replaced by its content, and the first one found names the text.
/// Text that carries no pair is returned whole with no tag.
#[must_use]
pub fn tagged_text(text: &str) -> (Option<&'static str>, String) {
    let mut found = None;
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while let Some(start) = rest.find('<') {
        for tag in KNOWN_TAGS {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if !rest[start..].starts_with(&open) {
                continue;
            }
            let body = &rest[start + open.len()..];
            if let Some(end) = body.find(&close) {
                result.push_str(&rest[..start]);
                result.push_str(&body[..end]);
                found.get_or_insert(tag);
                rest = &body[end + close.len()..];
                continue 'scan;
            }
        }
        result.push_str(&rest[..=start]);
        rest = &rest[start + 1..];
    }
    result.push_str(rest);
    match found {
        Some(tag) => (Some(tag), result),
        None => (None, text.to_owned()),
    }
}
