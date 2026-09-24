//! Public history entries as ACP session updates.
//!
//! Reference `vibe/acp/session_updates.py`: an entry the app server adds is
//! announced in full, a revision of one is projected as the delta between the
//! two versions, and a saved transcript is replayed through the same
//! announcement path. The projection reads the entries in their wire form, which
//! is the shape both implementations publish, so every rule here names the same
//! camelCase field the reference model aliases.

use std::path::Path;

use serde_json::{Map, Value, json};

/// The ACP tool kind an effect kind renders as.
fn tool_kind(effect_kind: &str) -> &'static str {
    match effect_kind {
        "shell" | "process" => "execute",
        "file_edit" | "file_write" => "edit",
        "file_search" | "web_search" => "search",
        "file_read" | "skill" => "read",
        "web_fetch" => "fetch",
        "subagent" => "think",
        _ => "other",
    }
}

/// What the default read limit is, which decides whether a read renders as a
/// file or as a line range.
const DEFAULT_READ_LIMIT: i64 = 2_000;

/// The updates a saved transcript replays as: the display title first, then
/// every entry except the title notices the title already stands for.
pub(crate) fn replay_session_updates(
    title: Option<&str>,
    updated_at: u64,
    history: &[Value],
) -> Vec<Value> {
    let mut updates = Vec::new();
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        updates.push(session_info_update(Some(title), updated_at));
    }
    for entry in history {
        if is_title_notice(entry) {
            continue;
        }
        updates.extend(replay_history_entry(entry));
    }
    updates
}

/// The updates one entry is announced with.
pub(crate) fn replay_history_entry(entry: &Value) -> Vec<Value> {
    match kind(entry) {
        "message" => message_updates(entry, None),
        "reasoning" => reasoning_updates(entry, None),
        "effect" => {
            let mut updates = vec![effect_start(entry)];
            updates.extend(plan_update(entry));
            updates
        }
        "checkpoint" => vec![checkpoint_update(entry, false)],
        "notice" if is_title_notice(entry) => {
            let title = entry.pointer("/detail/title").and_then(Value::as_str);
            vec![session_info_update(title, updated_at(entry))]
        }
        _ => Vec::new(),
    }
}

/// The updates an entry the app server just added is announced with.
pub(crate) fn added_entry_updates(entry: &Value) -> Vec<Value> {
    if is_title_notice(entry) {
        return Vec::new();
    }
    replay_history_entry(entry)
}

/// The updates a revision of an entry is projected as.
pub(crate) fn updated_entry_updates(previous: &Value, entry: &Value) -> Vec<Value> {
    match (kind(previous), kind(entry)) {
        ("message", "message") => {
            let delta = text_delta(&message_text(previous), &message_text(entry));
            if delta.is_empty() {
                Vec::new()
            } else {
                message_updates(entry, Some(&delta))
            }
        }
        ("reasoning", "reasoning") => {
            let delta = text_delta(text_field(previous, "text"), text_field(entry, "text"));
            if delta.is_empty() {
                Vec::new()
            } else {
                reasoning_updates(entry, Some(&delta))
            }
        }
        ("effect", "effect") => effect_progress_updates(previous, entry),
        ("checkpoint", "checkpoint") => vec![checkpoint_update(entry, true)],
        _ => Vec::new(),
    }
}

/// A session title as the client renders it, stamped with the session's
/// update time.
pub(crate) fn session_info_update(title: Option<&str>, updated_at_ms: u64) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".to_owned(), json!("session_info_update"));
    if let Some(title) = title {
        update.insert("title".to_owned(), json!(title));
    }
    update.insert("updatedAt".to_owned(), json!(iso_timestamp(updated_at_ms)));
    Value::Object(update)
}

/// Milliseconds since the epoch as an ISO 8601 UTC timestamp with an explicit
/// offset, the form Python's `datetime.isoformat` writes for an aware time.
pub(crate) fn iso_timestamp(millis: u64) -> String {
    let seconds = i64::try_from(millis / 1_000).unwrap_or(i64::MAX);
    let micros = (millis % 1_000) * 1_000;
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    if micros == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00")
    }
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn kind(entry: &Value) -> &str {
    entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn text_field<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn updated_at(entry: &Value) -> u64 {
    entry
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn is_title_notice(entry: &Value) -> bool {
    kind(entry) == "notice"
        && entry.pointer("/detail/kind").and_then(Value::as_str) == Some("session_title_updated")
}

fn is_completed(entry: &Value) -> bool {
    entry.get("generationStatus").and_then(Value::as_str) == Some("completed")
}

/// The concatenated text of a message's text blocks, which is what a
/// revision's delta is measured against.
fn message_text(entry: &Value) -> String {
    entry
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect()
}

/// What `current` adds to `previous`, or all of it when it is not an
/// extension.
fn text_delta(previous: &str, current: &str) -> String {
    current.strip_prefix(previous).unwrap_or(current).to_owned()
}

/// Drops every `null` member, which is what a model serialized without its
/// unset fields publishes.
fn object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn message_updates(entry: &Value, text: Option<&str>) -> Vec<Value> {
    if let Some(text) = text {
        return vec![message_chunk(entry, json!({"type": "text", "text": text}))];
    }
    let mut updates = Vec::new();
    for block in entry
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = match block.get("type").and_then(Value::as_str) {
            Some("text") => match block.get("text").and_then(Value::as_str) {
                Some(text) if !text.is_empty() => json!({"type": "text", "text": text}),
                _ => continue,
            },
            Some("image") => match image_block(block.get("attachment")) {
                Some(content) => content,
                None => continue,
            },
            Some("resource") => match resource_block(block.get("resource")) {
                Some(content) => content,
                None => continue,
            },
            _ => continue,
        };
        updates.push(message_chunk(entry, content));
    }
    updates
}

fn image_block(attachment: Option<&Value>) -> Option<Value> {
    let attachment = attachment?;
    let source = attachment.get("source")?;
    let mime_type = attachment.get("mimeType").cloned().unwrap_or(Value::Null);
    match source.get("kind").and_then(Value::as_str) {
        Some("inline") => Some(object(vec![
            ("type", json!("image")),
            ("data", source.get("data").cloned().unwrap_or(Value::Null)),
            ("mimeType", mime_type),
        ])),
        Some("file") => Some(object(vec![
            ("type", json!("resource_link")),
            (
                "name",
                attachment.get("alias").cloned().unwrap_or(Value::Null),
            ),
            ("uri", source.get("path").cloned().unwrap_or(Value::Null)),
            ("mimeType", mime_type),
        ])),
        _ => None,
    }
}

fn resource_block(resource: Option<&Value>) -> Option<Value> {
    let resource = resource?;
    let field = |key: &str| resource.get(key).cloned().unwrap_or(Value::Null);
    match resource.get("kind").and_then(Value::as_str) {
        Some("text") => Some(json!({
            "type": "resource",
            "resource": object(vec![
                ("uri", field("uri")),
                ("mimeType", field("mediaType")),
                ("text", field("text")),
            ]),
        })),
        Some("blob") => Some(json!({
            "type": "resource",
            "resource": object(vec![
                ("uri", field("uri")),
                ("mimeType", field("mediaType")),
                ("blob", field("blob")),
            ]),
        })),
        Some("link") => {
            let name = ["name", "title", "uri"]
                .into_iter()
                .find_map(|key| {
                    resource
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .map_or(Value::Null, |name| json!(name));
            Some(object(vec![
                ("type", json!("resource_link")),
                ("name", name),
                ("uri", field("uri")),
                ("title", field("title")),
                ("description", field("description")),
                ("mimeType", field("mediaType")),
                ("size", field("size")),
            ]))
        }
        _ => None,
    }
}

fn message_chunk(entry: &Value, content: Value) -> Value {
    let id = entry.get("id").cloned().unwrap_or(Value::Null);
    if entry.get("role").and_then(Value::as_str) == Some("user") {
        let meta = entry
            .get("userDisplayContent")
            .filter(|display| !display.is_null())
            .map_or(
                Value::Null,
                |display| json!({"user_display_content": display}),
            );
        return object(vec![
            ("sessionUpdate", json!("user_message_chunk")),
            ("content", content),
            ("messageId", id),
            ("_meta", meta),
        ]);
    }
    object(vec![
        ("sessionUpdate", json!("agent_message_chunk")),
        ("content", content),
        ("messageId", id),
    ])
}

fn reasoning_updates(entry: &Value, text: Option<&str>) -> Vec<Value> {
    let value = text.unwrap_or_else(|| text_field(entry, "text"));
    if value.is_empty() {
        return Vec::new();
    }
    vec![object(vec![
        ("sessionUpdate", json!("agent_thought_chunk")),
        ("content", json!({"type": "text", "text": value})),
        ("messageId", entry.get("id").cloned().unwrap_or(Value::Null)),
    ])]
}

fn detail(entry: &Value) -> &Value {
    entry.get("detail").unwrap_or(&Value::Null)
}

fn effect_kind(entry: &Value) -> &str {
    detail(entry)
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("tool")
}

fn state(entry: &Value) -> &Value {
    entry.get("state").unwrap_or(&Value::Null)
}

fn state_status(entry: &Value) -> &str {
    state(entry)
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The typed input an effect was called with, as the reference's
/// `effect_input_json` publishes it.
fn effect_input(entry: &Value) -> Value {
    detail(entry).get("input").cloned().unwrap_or(Value::Null)
}

fn effect_title(entry: &Value) -> Value {
    let summary = detail(entry)
        .pointer("/display/summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.is_empty());
    match summary {
        Some(summary) => json!(summary),
        None => entry.get("title").cloned().unwrap_or(Value::Null),
    }
}

fn effect_start(entry: &Value) -> Value {
    object(vec![
        ("sessionUpdate", json!("tool_call")),
        (
            "toolCallId",
            entry.get("id").cloned().unwrap_or(Value::Null),
        ),
        ("title", effect_title(entry)),
        ("kind", json!(tool_kind(effect_kind(entry)))),
        ("status", json!(effect_status(entry))),
        ("content", content_or_null(replayed_effect_content(entry))),
        ("locations", locations_or_null(effect_locations(entry))),
        ("rawInput", effect_input(entry)),
        ("rawOutput", raw_output(entry)),
        ("_meta", Value::Object(effect_meta(entry))),
    ])
}

fn effect_progress_updates(previous: &Value, entry: &Value) -> Vec<Value> {
    let output_delta = text_delta(output_text(previous), output_text(entry));
    let became_terminal = !is_completed(previous) && is_completed(entry);
    let state_changed = state_status(previous) != state_status(entry);
    let detail_changed = detail(previous) != detail(entry);

    let mut content = Vec::new();
    if !output_delta.is_empty() {
        content.push(text_tool_content(&output_delta));
    }
    if became_terminal || state_changed {
        content.extend(result_effect_content(entry, &output_delta));
    }
    if detail_changed {
        content.extend(call_effect_content(entry).unwrap_or_default());
    }
    let progress = object(vec![
        ("sessionUpdate", json!("tool_call_update")),
        (
            "toolCallId",
            entry.get("id").cloned().unwrap_or(Value::Null),
        ),
        (
            "title",
            if detail_changed {
                effect_title(entry)
            } else {
                Value::Null
            },
        ),
        ("kind", json!(tool_kind(effect_kind(entry)))),
        ("status", json!(effect_status(entry))),
        ("content", content_or_null(content)),
        (
            "locations",
            if detail_changed || became_terminal || state_changed {
                locations_or_null(effect_locations(entry))
            } else {
                Value::Null
            },
        ),
        (
            "rawInput",
            if detail_changed {
                effect_input(entry)
            } else {
                Value::Null
            },
        ),
        (
            "rawOutput",
            if became_terminal || state_changed {
                raw_output(entry)
            } else {
                Value::Null
            },
        ),
        ("_meta", Value::Object(effect_meta(entry))),
    ]);
    let mut updates = vec![progress];
    if became_terminal || state_changed {
        updates.extend(plan_update(entry));
    }
    updates
}

fn content_or_null(content: Vec<Value>) -> Value {
    if content.is_empty() {
        Value::Null
    } else {
        Value::Array(content)
    }
}

fn locations_or_null(locations: Option<Vec<Value>>) -> Value {
    match locations {
        Some(locations) if !locations.is_empty() => Value::Array(locations),
        Some(locations) if locations.is_empty() => Value::Null,
        _ => Value::Null,
    }
}

fn replayed_effect_content(entry: &Value) -> Vec<Value> {
    if !is_completed(entry) {
        let mut content = call_effect_content(entry).unwrap_or_default();
        let output = output_text(entry);
        if !output.is_empty() {
            content.push(text_tool_content(output));
        }
        return content;
    }
    let mut content = result_effect_content(entry, "");
    let output = output_text(entry);
    if !output.is_empty() {
        content.insert(0, text_tool_content(output));
    }
    content
}

fn diff_content(path: &Value, old_text: &Value, new_text: &Value) -> Value {
    object(vec![
        ("type", json!("diff")),
        ("path", path.clone()),
        ("oldText", old_text.clone()),
        ("newText", new_text.clone()),
    ])
}

fn result_effect_content(entry: &Value, output_delta: &str) -> Vec<Value> {
    let mut content = Vec::new();
    match effect_kind(entry) {
        "shell" if state_status(entry) == "completed" => {
            // The transcript arrives as output; a trailer would repeat its tail.
            return content;
        }
        "file_edit" => {
            if let Some(output) = effect_output(entry) {
                let occurrences = output
                    .get("occurrences")
                    .and_then(Value::as_array)
                    .filter(|occurrences| !occurrences.is_empty());
                if let Some(occurrences) = occurrences {
                    for occurrence in occurrences {
                        content.push(diff_content(
                            output.get("file").unwrap_or(&Value::Null),
                            occurrence.get("oldText").unwrap_or(&Value::Null),
                            occurrence.get("newText").unwrap_or(&Value::Null),
                        ));
                    }
                } else if let (Some(old), Some(new)) = (
                    output.get("oldString").filter(|value| !value.is_null()),
                    output.get("newString").filter(|value| !value.is_null()),
                ) {
                    content.push(diff_content(
                        output.get("file").unwrap_or(&Value::Null),
                        old,
                        new,
                    ));
                }
            }
        }
        "file_write" => {
            if let Some(output) = effect_output(entry) {
                content.push(diff_content(
                    output.get("filePath").unwrap_or(&Value::Null),
                    &Value::Null,
                    output.get("content").unwrap_or(&Value::Null),
                ));
            }
        }
        _ => {}
    }
    let display = result_display_text(entry);
    if !display.is_empty() && display != output_delta {
        content.push(text_tool_content(&display));
    }
    content
}

fn call_effect_content(entry: &Value) -> Option<Vec<Value>> {
    let input = detail(entry).get("input").unwrap_or(&Value::Null);
    match effect_kind(entry) {
        "file_edit" if input.get("changes").is_some() => {
            return Some(
                input
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|change| {
                        diff_content(
                            input.get("filePath").unwrap_or(&Value::Null),
                            change.get("oldString").unwrap_or(&Value::Null),
                            change.get("newString").unwrap_or(&Value::Null),
                        )
                    })
                    .collect(),
            );
        }
        "file_edit" if input.is_object() => {
            return Some(vec![diff_content(
                input.get("filePath").unwrap_or(&Value::Null),
                input.get("oldString").unwrap_or(&Value::Null),
                input.get("newString").unwrap_or(&Value::Null),
            )]);
        }
        "file_write" if input.is_object() => {
            return Some(vec![diff_content(
                input.get("filePath").unwrap_or(&Value::Null),
                &Value::Null,
                input.get("content").unwrap_or(&Value::Null),
            )]);
        }
        _ => {}
    }
    detail(entry)
        .pointer("/display/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| vec![text_tool_content(text)])
}

fn effect_locations(entry: &Value) -> Option<Vec<Value>> {
    let input = detail(entry).get("input").filter(|input| input.is_object());
    let output = effect_output(entry);
    match effect_kind(entry) {
        "file_edit" => {
            let input = input?;
            let path = output
                .as_ref()
                .and_then(|output| output.get("file"))
                .or_else(|| input.get("filePath"))
                .and_then(Value::as_str)?;
            Some(vec![json!({"path": resolved_path(path)})])
        }
        "file_write" => {
            let input = input?;
            let path = output
                .as_ref()
                .and_then(|output| output.get("filePath"))
                .or_else(|| input.get("filePath"))
                .and_then(Value::as_str)?;
            Some(vec![json!({"path": resolved_path(path)})])
        }
        "file_search" => {
            let output = output?;
            Some(
                output
                    .get("parsedMatches")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|found| {
                        object(vec![
                            ("path", found.get("path").cloned().unwrap_or(Value::Null)),
                            ("line", found.get("line").cloned().unwrap_or(Value::Null)),
                        ])
                    })
                    .collect(),
            )
        }
        "file_read" => {
            let input = input?;
            Some(vec![match output {
                Some(output) => read_result_location(&output),
                None => read_call_location(input),
            }])
        }
        "web_search" => {
            let output = output?;
            Some(
                output
                    .get("sources")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|source| {
                        json!({
                            "path": source.get("url").cloned().unwrap_or(Value::Null),
                            "_meta": {
                                "type": "url",
                                "title": source.get("title").cloned().unwrap_or(Value::Null),
                            },
                        })
                    })
                    .collect(),
            )
        }
        "web_fetch" => {
            let input = input?;
            Some(vec![match output {
                None => json!({
                    "path": normalized_web_url(
                        input.get("url").and_then(Value::as_str).unwrap_or_default()
                    ),
                    "_meta": {"type": "url"},
                }),
                Some(output) => json!({
                    "path": output.get("url").cloned().unwrap_or(Value::Null),
                    "_meta": {
                        "type": "url",
                        "char_count": output
                            .get("content")
                            .and_then(Value::as_str)
                            .map_or(0, |content| content.chars().count()),
                        "truncated": output.get("wasTruncated").cloned().unwrap_or(json!(false)),
                    },
                }),
            }])
        }
        "skill" => {
            let directory = output?.get("skillDir")?.as_str()?.to_owned();
            Some(vec![json!({"path": resolved_path(&directory)})])
        }
        _ => None,
    }
}

fn read_call_location(input: &Value) -> Value {
    let path = resolved_path(
        input
            .get("filePath")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let limit = input.get("limit").cloned().unwrap_or(Value::Null);
    let offset = input.get("offset").cloned().unwrap_or(Value::Null);
    if limit.as_i64() != Some(DEFAULT_READ_LIMIT) {
        return json!({
            "path": path,
            "_meta": {"type": "file_range", "offset": offset, "limit": limit},
        });
    }
    object(vec![
        ("path", json!(path)),
        ("line", offset),
        ("_meta", json!({"type": "file"})),
    ])
}

fn read_result_location(output: &Value) -> Value {
    let path = resolved_path(
        output
            .get("filePath")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let requested_limit = output
        .get("requestedLimit")
        .cloned()
        .unwrap_or(json!(DEFAULT_READ_LIMIT));
    let truncated = output
        .get("wasTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if requested_limit.as_i64() != Some(DEFAULT_READ_LIMIT) || truncated {
        return json!({
            "path": path,
            "_meta": {
                "type": "file_range",
                "offset": output.get("startLine").cloned().unwrap_or(Value::Null),
                "limit": output.get("numLines").cloned().unwrap_or(Value::Null),
            },
        });
    }
    object(vec![
        ("path", json!(path)),
        (
            "line",
            output
                .get("requestedOffset")
                .cloned()
                .unwrap_or(Value::Null),
        ),
        ("_meta", json!({"type": "file"})),
    ])
}

/// An absolute, symlink-resolved path, the way `Path.resolve` answers even
/// for a path that does not exist.
fn resolved_path(value: &str) -> String {
    let path = Path::new(value);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    // Resolve the longest existing prefix and keep the rest as written.
    let mut existing = absolute.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&existing) {
            let mut resolved = resolved;
            for component in rest.iter().rev() {
                resolved.push(component);
            }
            return resolved.to_string_lossy().into_owned();
        }
        match (
            existing.file_name().map(ToOwned::to_owned),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return absolute.to_string_lossy().into_owned(),
        }
    }
}

fn normalized_web_url(value: &str) -> String {
    let raw = if value.starts_with("//") {
        value.trim_start_matches('/')
    } else {
        value
    };
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    }
}

/// The typed result of a completed effect, when it published one.
fn effect_output(entry: &Value) -> Option<Value> {
    if state_status(entry) != "completed" {
        return None;
    }
    state(entry)
        .get("output")
        .filter(|output| !output.is_null())
        .cloned()
}

fn effect_status(entry: &Value) -> &'static str {
    match state_status(entry) {
        "pending" => "pending",
        "running" | "blocked" => "in_progress",
        "completed" => {
            if effect_kind(entry) == "subagent"
                && effect_output(entry)
                    .and_then(|output| output.get("completed").and_then(Value::as_bool))
                    == Some(false)
            {
                "failed"
            } else {
                "completed"
            }
        }
        _ => "failed",
    }
}

fn raw_output(entry: &Value) -> Value {
    let state = state(entry);
    match state_status(entry) {
        "completed" => state.get("output").cloned().unwrap_or(Value::Null),
        "failed" => match state.get("output").filter(|output| !output.is_null()) {
            Some(output) => output.clone(),
            None => state.get("error").cloned().unwrap_or(Value::Null),
        },
        "cancelled" | "skipped" => state.get("reason").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn output_text(entry: &Value) -> &str {
    match state_status(entry) {
        "running" | "blocked" | "completed" | "failed" | "cancelled" => state(entry)
            .get("outputText")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        _ => "",
    }
}

/// Reference `EffectResultDisplay.text`: the verb and the message.
fn result_display_text(entry: &Value) -> String {
    let display = match state_status(entry) {
        "completed" | "failed" | "cancelled" | "skipped" => state(entry).get("display"),
        _ => None,
    };
    let Some(display) = display.filter(|display| !display.is_null()) else {
        return String::new();
    };
    let field = |key: &str| display.get(key).and_then(Value::as_str).unwrap_or_default();
    format!("{} {}", field("verb"), field("message"))
        .trim()
        .to_owned()
}

fn effect_meta(entry: &Value) -> Map<String, Value> {
    let detail = detail(entry);
    let mut meta = Map::new();
    meta.insert(
        "tool_name".to_owned(),
        detail.get("toolName").cloned().unwrap_or(Value::Null),
    );
    meta.insert("effect_kind".to_owned(), json!(effect_kind(entry)));
    let input = detail.get("input").filter(|input| input.is_object());
    let output = effect_output(entry);
    match effect_kind(entry) {
        "shell" => {
            if output
                .as_ref()
                .and_then(|output| output.get("truncated"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                meta.insert("output_truncated".to_owned(), json!(true));
            }
        }
        "file_search" => {
            if let Some(input) = input {
                meta.insert(
                    "query".to_owned(),
                    input.get("pattern").cloned().unwrap_or(Value::Null),
                );
                meta.insert(
                    "search_path".to_owned(),
                    json!(resolved_path(
                        input.get("path").and_then(Value::as_str).unwrap_or(".")
                    )),
                );
            }
        }
        "web_search" => {
            if let Some(input) = input {
                meta.insert(
                    "query".to_owned(),
                    input.get("query").cloned().unwrap_or(Value::Null),
                );
            }
        }
        "skill" => {
            if let Some(input) = input {
                let name = output
                    .as_ref()
                    .and_then(|output| output.get("name"))
                    .or_else(|| input.get("name"))
                    .cloned()
                    .unwrap_or(Value::Null);
                meta.insert("skill_name".to_owned(), name);
            }
        }
        "subagent" => {
            if let Some(input) = input {
                meta.insert(
                    "agent".to_owned(),
                    input.get("agent").cloned().unwrap_or(Value::Null),
                );
                meta.insert(
                    "task".to_owned(),
                    input.get("task").cloned().unwrap_or(Value::Null),
                );
            }
            if let Some(child) = detail
                .get("childSessionId")
                .filter(|child| !child.is_null())
            {
                meta.insert("child_session_id".to_owned(), child.clone());
            }
            if let Some(output) = output {
                meta.insert(
                    "turn_count".to_owned(),
                    output.get("turnsUsed").cloned().unwrap_or(Value::Null),
                );
                meta.insert(
                    "response".to_owned(),
                    output.get("response").cloned().unwrap_or(Value::Null),
                );
            }
        }
        _ => {}
    }
    meta
}

fn plan_update(entry: &Value) -> Option<Value> {
    if effect_kind(entry) != "todo" {
        return None;
    }
    let todos = effect_output(entry)
        .and_then(|output| output.get("todos").cloned())
        .or_else(|| {
            detail(entry)
                .get("input")
                .and_then(|input| input.get("todos"))
                .filter(|todos| !todos.is_null())
                .cloned()
        })
        .unwrap_or_else(|| json!([]));
    let entries = todos
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            if status == "cancelled" {
                return None;
            }
            let status = match status {
                "pending" | "in_progress" | "completed" => status,
                _ => "pending",
            };
            Some(json!({
                "content": item.get("content").cloned().unwrap_or(Value::Null),
                "status": status,
                "priority": item.get("priority").cloned().unwrap_or(json!("medium")),
            }))
        })
        .collect::<Vec<_>>();
    Some(json!({
        "sessionUpdate": "plan",
        "entries": entries,
        "_meta": {"effect_entry_id": entry.get("id").cloned().unwrap_or(Value::Null)},
    }))
}

fn checkpoint_update(entry: &Value, progress: bool) -> Value {
    let message = entry
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty());
    let kind_name = entry
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = message.map_or_else(|| checkpoint_title(kind_name), ToOwned::to_owned);
    let content = entry
        .get("message")
        .and_then(Value::as_str)
        .map_or(Value::Null, |message| json!([text_tool_content(message)]));
    let details = entry.get("details").cloned().unwrap_or(Value::Null);
    let status = if is_completed(entry) {
        "completed"
    } else {
        "in_progress"
    };
    object(vec![
        (
            "sessionUpdate",
            json!(if progress {
                "tool_call_update"
            } else {
                "tool_call"
            }),
        ),
        (
            "toolCallId",
            entry.get("id").cloned().unwrap_or(Value::Null),
        ),
        ("title", json!(title)),
        ("kind", json!("think")),
        ("status", json!(status)),
        ("content", content),
        (if progress { "rawOutput" } else { "rawInput" }, details),
        ("_meta", json!({"checkpoint_kind": kind_name})),
    ])
}

/// The label a checkpoint without a message is shown under: its kind, spaced
/// and capitalized the way Python's `str.capitalize` does it.
fn checkpoint_title(kind: &str) -> String {
    let spaced = kind.replace('_', " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first
            .to_uppercase()
            .chain(characters.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

fn text_tool_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}
