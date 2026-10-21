//! Where an effect points the client: the files it touched, the matches it
//! found, the pages it read.

use std::path::Path;

use serde_json::{Value, json};

use super::{detail, effect_kind, effect_output};
use crate::projection::object;

/// What the default read limit is, which decides whether a read renders as a
/// file or as a line range.
const DEFAULT_READ_LIMIT: i64 = 2_000;

pub(super) fn effect_locations(entry: &Value) -> Option<Vec<Value>> {
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
pub(super) fn resolved_path(value: &str) -> String {
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
        if let Ok(resolved) = vibe_core::platform::canonicalize(&existing) {
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
