//! Skills a user invokes by typing `/name`.
//!
//! The harness has no way to insert a synthetic tool exchange, so the host
//! appends each invoked skill's rendered body to the user message and names
//! the skills in a reserved `userDisplayContent` block. The public projection
//! recognizes those bodies again and presents the literal prompt followed by
//! one settled `skill` effect per invoked skill, as a model-initiated load
//! reads. Reference `vibe/app_server/_skill_invocation.py`.

use serde_json::{Value, json};
use vibe_core::skills::{
    SKILL_CONTENT_CLOSING, SKILL_CONTENT_MARKER_PREFIX, skill_already_loaded_name,
    skill_content_marker,
};

/// The `userDisplayContent` block type that records an invoked skill.
pub const DISPLAY_BLOCK: &str = "vibe.skill_invocation";

const TRAILING_PUNCTUATION: &[char] =
    &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"', '`'];
/// What replaces a code span, so it never opens a new mention boundary.
const REMOVED_CODE: char = '\0';

/// The slash command's skill name, casefolded.
#[must_use]
pub fn leading_skill_name(text: &str) -> Option<String> {
    let rest = text.trim().strip_prefix('/')?;
    rest.split_whitespace().next().map(str::to_lowercase)
}

/// Names of the `/name` mentions outside code, in order, without repeats.
/// A leading `/name` is the slash command, not a mention.
#[must_use]
pub fn mentioned_skill_names(text: &str) -> Vec<String> {
    let prose = without_inline_code(&without_fenced_code(text));
    let command_at = prose.len() - prose.trim_start().len();
    let mut names: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    for (index, character) in prose.char_indices() {
        let boundary = previous.is_none_or(char::is_whitespace);
        previous = Some(character);
        if character != '/' || !boundary || index == command_at {
            continue;
        }
        let word: String = prose[index + 1..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        if word.is_empty() {
            continue;
        }
        let name = word.trim_end_matches(TRAILING_PUNCTUATION);
        if is_skill_name(name) && !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    }
    names
}

fn is_skill_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | ':' | '-')
        })
}

/// Fenced blocks, from an opening run of three or more backticks or tildes
/// at a line start to the next line that starts with the same run, or to the
/// end, each replaced by one removed-code mark.
fn without_fenced_code(text: &str) -> String {
    let line_end = |from: usize| text[from..].find('\n').map_or(text.len(), |at| from + at);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut line = 0;
    while line < text.len() {
        let end_of_line = line_end(line);
        let Some(fence) = fence_at(text[line..end_of_line].trim_start_matches([' ', '\t'])) else {
            line = end_of_line + 1;
            continue;
        };
        out.push_str(&text[cursor..line]);
        out.push(REMOVED_CODE);
        cursor = text.len();
        let mut next = end_of_line + 1;
        while next < text.len() {
            let body = text[next..line_end(next)].trim_start_matches([' ', '\t']);
            if body.starts_with(fence) {
                cursor = line_end(next) - body.len() + fence.len();
                break;
            }
            next = line_end(next) + 1;
        }
        line = line_end(cursor) + 1;
    }
    out.push_str(&text[cursor..]);
    out
}

fn fence_at(line: &str) -> Option<&str> {
    let marker = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = line.len() - line.trim_start_matches(marker).len();
    (run >= 3).then(|| &line[..run])
}

/// Inline spans: a run of backticks, text without a backtick or newline, and
/// a run of the same length, each replaced by one removed-code mark.
fn without_inline_code(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }
        let run = bytes[index..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        let span = (1..=run).rev().find_map(|width| {
            let open_end = index + width;
            let close = bytes[open_end..]
                .iter()
                .position(|byte| *byte == b'`' || *byte == b'\n')
                .map(|at| open_end + at)?;
            let closing = bytes[close..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            (bytes[close] == b'`' && closing >= width).then_some(close + width)
        });
        match span {
            Some(end) => {
                out.push_str(&text[cursor..index]);
                out.push(REMOVED_CODE);
                cursor = end;
                index = end;
            }
            None => index += 1,
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Casefolded names the prompt invokes, the leading `/name` first.
#[must_use]
pub fn invoked_skill_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = leading_skill_name(text).into_iter().collect();
    for name in mentioned_skill_names(text) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The display content with one marker per invoked skill appended.
#[must_use]
pub fn with_display(display: Option<Value>, names: &[String]) -> Option<Value> {
    if names.is_empty() {
        return display;
    }
    let markers = names
        .iter()
        .map(|name| json!({"type": DISPLAY_BLOCK, "name": name}));
    match display {
        Some(mut display) if display["content"].is_array() => {
            if let Some(content) = display["content"].as_array_mut() {
                content.extend(markers);
            }
            Some(display)
        }
        _ => Some(json!({"version": "1", "host": "vibe", "content": markers.collect::<Vec<_>>()})),
    }
}

/// The names the display content records.
#[must_use]
pub fn display_names(display: &Value) -> Vec<String> {
    display["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == DISPLAY_BLOCK)
        .filter_map(|block| block["name"].as_str().filter(|name| !name.is_empty()))
        .map(ToOwned::to_owned)
        .collect()
}

/// The display content without its invocation markers, or nothing when
/// they were all it held.
#[must_use]
pub fn without_display(display: &Value) -> Value {
    let Some(content) = display.get("content").and_then(Value::as_array) else {
        return display.clone();
    };
    let kept: Vec<Value> = content
        .iter()
        .filter(|block| block["type"] != DISPLAY_BLOCK)
        .cloned()
        .collect();
    if kept.is_empty() {
        return Value::Null;
    }
    let mut display = display.clone();
    display["content"] = Value::Array(kept);
    display
}

/// Where the appended bodies sit among a message's blocks, as
/// `(block index, skill name)` in content order. `texts` holds each block's
/// text, `None` for other blocks. A block counts when it renders or
/// acknowledges a skill the display names (any name, before the display
/// recorded them) and the rest of the text still invokes that skill.
#[must_use]
pub fn payloads(texts: &[Option<&str>], names: &[String]) -> Vec<(usize, String)> {
    let mut candidates: Vec<(usize, String)> = Vec::new();
    let mut claimed: Vec<String> = Vec::new();
    for (index, text) in texts.iter().enumerate().rev() {
        let Some(text) = text else { continue };
        let Some(payload) = content_name(text).or_else(|| skill_already_loaded_name(text)) else {
            continue;
        };
        let name = if names.is_empty() {
            Some(payload.to_owned())
        } else {
            names
                .iter()
                .find(|name| name.to_lowercase() == payload.to_lowercase())
                .cloned()
        };
        let Some(name) = name else { continue };
        if claimed.contains(&name.to_lowercase()) {
            continue;
        }
        claimed.push(name.to_lowercase());
        candidates.push((index, name));
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let visible = texts
        .iter()
        .enumerate()
        .filter(|(index, _)| !candidates.iter().any(|(at, _)| at == index))
        .filter_map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join("\n\n");
    let invoked = invoked_skill_names(&visible);
    let mut found: Vec<(usize, String)> = candidates
        .into_iter()
        .filter(|(_, name)| invoked.contains(&name.to_lowercase()))
        .collect();
    found.sort();
    found
}

fn content_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(SKILL_CONTENT_MARKER_PREFIX)?;
    if !text.trim_end().ends_with(SKILL_CONTENT_CLOSING) {
        return None;
    }
    let end = rest.find("\">")?;
    Some(&rest[..end]).filter(|name| !name.is_empty())
}

/// A user message entry's text blocks, `None` for the others.
fn block_texts(content: &[Value]) -> Vec<Option<&str>> {
    content
        .iter()
        .map(|block| {
            (block["type"] == "text")
                .then(|| block["text"].as_str())
                .flatten()
        })
        .collect()
}

/// Reference `_project_history_entries`: a user message without its appended
/// bodies, then one settled `skill` effect per invoked skill.
#[must_use]
pub fn expand_message(message: &Value) -> Vec<Value> {
    if message["role"] != "user" {
        return vec![message.clone()];
    }
    let display = &message["userDisplayContent"];
    let mut public = message.clone();
    if !display.is_null() {
        public["userDisplayContent"] = without_display(display);
    }
    let Some(content) = message["content"].as_array() else {
        return vec![public];
    };
    let found = payloads(&block_texts(content), &display_names(display));
    if found.is_empty() {
        return vec![public];
    }
    public["content"] = Value::Array(
        content
            .iter()
            .enumerate()
            .filter(|(index, _)| !found.iter().any(|(at, _)| at == index))
            .map(|(_, block)| block.clone())
            .collect(),
    );
    let mut entries = vec![public.clone()];
    for (index, name) in found {
        let output = content[index]["text"].as_str().unwrap_or_default();
        entries.push(skill_effect(&public, &name, output));
    }
    entries
}

fn skill_effect(message: &Value, name: &str, output_text: &str) -> Value {
    let label = format!("skill: {name}");
    json!({
        "id": format!("{}:invoked-skill:{name}", message["id"].as_str().unwrap_or_default()),
        "sessionId": message["sessionId"],
        "turnId": message["turnId"],
        "inputEntryId": null,
        "createdAt": message["createdAt"],
        "updatedAt": message["updatedAt"],
        "generationStatus": "completed",
        "relatedEntryId": message["id"],
        "type": "effect",
        "title": "skill",
        "detail": {
            "kind": "skill",
            "toolName": "skill",
            "input": {"name": name},
            "display": {
                "summary": format!("Loading skill: {name}"),
                "content": null,
                "suffix": "",
                "verb": "Loading",
                "message": label,
                "settledVerb": "Loaded",
                "settledMessage": label,
                "statusText": "Loading skill",
            },
        },
        "state": {
            "status": "completed",
            "output": null,
            "outputText": output_text,
            "durationMs": 0.0,
            "display": {
                "success": true,
                "verb": "Loaded",
                "message": label,
                "warnings": [],
                "approvalNote": null,
                "suffix": "",
            },
            "decision": null,
            "approvalType": null,
            "approvalSource": null,
        },
    })
}

/// Reference `_project_queued_entry`: a queued user entry without its
/// appended bodies or invocation markers.
pub fn project_queued_entry(entry: &mut Value) {
    if entry["type"] != "user_input" {
        return;
    }
    let display = entry["annotations"]["vibe_user_display_content"].clone();
    let names = display_names(&display);
    if !display.is_null() {
        entry["annotations"]["vibe_user_display_content"] = without_display(&display);
    }
    let Some(content) = entry["content"].as_array() else {
        return;
    };
    let found = payloads(&block_texts(content), &names);
    if found.is_empty() {
        return;
    }
    let kept: Vec<Value> = content
        .iter()
        .enumerate()
        .filter(|(index, _)| !found.iter().any(|(at, _)| at == index))
        .map(|(_, block)| block.clone())
        .collect();
    entry["content"] = Value::Array(kept);
}

/// Reference `public_session_preview`: a preview without the bodies the
/// host appended, whole or cut.
#[must_use]
pub fn public_preview(preview: &str) -> String {
    let names = invoked_skill_names(preview);
    if names.is_empty() {
        return preview.to_owned();
    }
    let searchable = preview.strip_suffix('…').unwrap_or(preview).trim_end();
    let start = names
        .iter()
        .filter_map(|name| {
            content_start(searchable, name).or_else(|| truncated_marker_start(searchable, name))
        })
        .min();
    match start {
        Some(start) => preview[..start].trim_end().to_owned(),
        None => preview.to_owned(),
    }
}

fn starts_with_heading(content: &str, name: &str) -> bool {
    let heading = format!("# Skill: {name}");
    content.starts_with(&heading) || heading.starts_with(content)
}

fn truncated_marker_start(text: &str, name: &str) -> Option<usize> {
    let at = text.rfind("<skill_content")?;
    skill_content_marker(name)
        .to_lowercase()
        .starts_with(&text[at..].to_lowercase())
        .then_some(at)
}

/// Where the outermost rendered body for `name` opens, preferring one that
/// closes at the end of the text.
fn content_start(text: &str, name: &str) -> Option<usize> {
    let mut stack: Vec<Option<usize>> = Vec::new();
    let mut rendered: Vec<usize> = Vec::new();
    let mut completed: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let marker = text[cursor..]
            .find(SKILL_CONTENT_MARKER_PREFIX)
            .map(|at| cursor + at);
        let closing = text[cursor..]
            .find(SKILL_CONTENT_CLOSING)
            .map(|at| cursor + at);
        if let Some(marker_at) = marker
            && closing.is_none_or(|closing_at| marker_at < closing_at)
        {
            let name_start = marker_at + SKILL_CONTENT_MARKER_PREFIX.len();
            let Some(name_end) = text[name_start..].find("\">").map(|at| name_start + at) else {
                break;
            };
            let opened = &text[name_start..name_end];
            let parent = stack.last().copied().flatten();
            let is_rendered = opened.to_lowercase() == name.to_lowercase()
                && starts_with_heading(text[name_end + 2..].trim_start(), opened);
            let mut root = parent;
            if is_rendered && root.is_none() {
                root = Some(marker_at);
                rendered.push(marker_at);
            }
            stack.push(root);
            cursor = name_end + 2;
            continue;
        }
        let Some(closing_at) = closing else { break };
        if let Some(root) = stack.pop()
            && let Some(root) = root
            && !stack.contains(&Some(root))
        {
            completed.insert(root, closing_at + SKILL_CONTENT_CLOSING.len());
        }
        cursor = closing_at + SKILL_CONTENT_CLOSING.len();
    }
    let end = text.trim_end().len();
    if let Some(last) = rendered
        .iter()
        .rev()
        .find(|root| completed.get(root) == Some(&end))
    {
        return Some(*last);
    }
    if rendered.iter().any(|root| stack.contains(&Some(*root))) {
        return rendered.first().copied();
    }
    if text.trim_end().ends_with(SKILL_CONTENT_CLOSING) && !rendered.is_empty() {
        return rendered.first().copied();
    }
    rendered
        .iter()
        .find(|root| completed.contains_key(root))
        .copied()
}

#[cfg(test)]
mod invocation_tests;
