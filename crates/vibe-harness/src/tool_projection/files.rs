//! Files, the shell and skills: `read_file`, `write_file`, `search_replace`,
//! `bash` and `skill.read`.

use serde_json::{Value, json};
use vibe_core::events::detail::shell_transcript;

use super::{
    Projected, call_display, completed_state, detail, meta, nonempty, plural, result_display,
    str_field, structured,
};

/// The page a read takes when the call names none.
const DEFAULT_READ_LIMIT: u64 = 2000;

const SEARCH_REPLACE_ANNOTATION: &str = "mistralai.vibe.sdk.search_replace";
const WRITE_FILE_ANNOTATION: &str = "mistralai.vibe.sdk.write_file";

fn file_display(active: &str, settled: &str, path: &str, content: Option<&str>) -> Value {
    let mut display = call_display(
        format!("{active} {path}"),
        if active == "Writing" {
            "Creating"
        } else {
            active
        },
        path,
        settled,
        path,
        format!("{active} file"),
    );
    display["content"] = json!(content);
    display
}

fn display_offset(offset: i64) -> Value {
    match offset {
        0 => Value::Null,
        positive if positive > 0 => json!(positive + 1),
        negative => json!(negative),
    }
}

pub(super) fn project_read(name: &str, input: &Value) -> Option<Projected> {
    let path = nonempty(input, "path")?.to_owned();
    let offset = match input.get("offset") {
        None | Some(Value::Null) => 0,
        Some(value) => value.as_i64().filter(|offset| *offset >= -1)?,
    };
    let limit = match input.get("limit") {
        None => Some(DEFAULT_READ_LIMIT),
        Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().filter(|limit| *limit >= 1)?),
    };
    let requested_offset = display_offset(offset);
    let detail = detail(
        "file_read",
        name,
        json!({"filePath": path, "offset": requested_offset, "limit": limit}),
        file_display("Reading", "Read", &path, None),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let path = result
                .get("path")?
                .as_str()
                .filter(|path| !path.is_empty())?;
            let content = result.get("content")?.as_str()?;
            let start = result.get("offset")?.as_u64()?;
            let lines = result.get("lines_read")?.as_u64()?;
            let truncated = result
                .get("was_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = json!({
                "filePath": path,
                "content": content,
                "numLines": lines,
                "startLine": start + 1,
                "requestedOffset": requested_offset,
                "requestedLimit": limit,
                "totalLines": null,
                "wasTruncated": truncated,
            });
            let mut display = result_display(
                true,
                "Read",
                format!("{} from {path}", plural(lines, "line", "lines")),
            );
            if truncated {
                display["suffix"] = json!("(truncated)");
            }
            Some(completed_state(state, output, display, None))
        })),
    })
}

pub(super) fn project_write(name: &str, input: &Value) -> Option<Projected> {
    let path = nonempty(input, "path")?.to_owned();
    let content = str_field(input, "content")?.to_owned();
    let detail = detail(
        "file_write",
        name,
        json!({"filePath": path, "content": content}),
        file_display("Writing", "Created", &path, Some(&content)),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let path = result
                .get("path")?
                .as_str()
                .filter(|path| !path.is_empty())?;
            let existed = result.get("file_existed")?.as_bool()?;
            let previous = match meta(state).and_then(|meta| meta.get(WRITE_FILE_ANNOTATION)) {
                None | Some(Value::Null) => Value::Null,
                Some(annotation) => json!(annotation.get("previous_content")?.as_str()?),
            };
            let output = json!({
                "filePath": path,
                "content": content,
                "fileExisted": existed,
                "previousContent": previous,
            });
            let display = result_display(true, if existed { "Updated" } else { "Created" }, path);
            Some(completed_state(state, output, display, None))
        })),
    })
}

pub(super) fn project_search_replace(name: &str, input: &Value) -> Option<Projected> {
    let path = nonempty(input, "file_path")?.to_owned();
    let changes = input
        .get("content")?
        .as_array()
        .filter(|changes| !changes.is_empty())?;
    let mut projected = Vec::new();
    for change in changes {
        let old = nonempty(change, "old_str")?;
        let new = str_field(change, "new_str")?;
        if old == new {
            return None;
        }
        let replace_all = match change.get("replace_all") {
            None => false,
            Some(value) => value.as_bool()?,
        };
        projected.push(json!({"oldString": old, "newString": new, "replaceAll": replace_all}));
    }
    let detail = detail(
        "file_edit",
        name,
        json!({"filePath": path, "changes": projected}),
        file_display("Editing", "Edited", &path, None),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let file = result
                .get("file")?
                .as_str()
                .filter(|file| !file.is_empty())?;
            let warnings = result.get("warnings").cloned().unwrap_or_else(|| json!([]));
            let blocks = meta(state)?
                .get(SEARCH_REPLACE_ANNOTATION)?
                .get("blocks")?
                .as_array()
                .filter(|blocks| !blocks.is_empty())?;
            let mut occurrences = Vec::new();
            for block in blocks {
                let lines = |key: &str| -> Option<String> {
                    Some(
                        block
                            .get(key)?
                            .as_array()?
                            .iter()
                            .map(|line| line.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()?
                            .concat(),
                    )
                };
                occurrences.push(json!({
                    "startLine": block.get("old_start_line")?.as_u64().filter(|line| *line >= 1)?,
                    "oldText": lines("old_lines")?,
                    "newText": lines("new_lines")?,
                }));
            }
            let output = json!({
                "file": file,
                "oldString": null,
                "newString": null,
                "occurrences": occurrences,
            });
            let mut display = result_display(true, "Edited", file);
            display["warnings"] = warnings;
            Some(completed_state(state, output, display, None))
        })),
    })
}

pub(super) fn project_shell(name: &str, input: &Value) -> Option<Projected> {
    let command = nonempty(input, "command")?.to_owned();
    let detail = detail(
        "shell",
        name,
        json!({"command": command}),
        call_display(
            format!("bash: {command}"),
            "Running",
            command.as_str(),
            "Ran",
            command.as_str(),
            "Running command",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let text = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default();
            let parsed = ["stdout", "stderr", "output"]
                .iter()
                .all(|key| result.get(*key).is_none_or(Value::is_string))
                && result.get("returncode").is_none_or(Value::is_i64)
                && result.get("was_truncated").is_none_or(Value::is_boolean);
            let (output, output_text, failed) = if parsed {
                let output = json!({
                    "stdout": text("stdout"),
                    "stderr": text("stderr"),
                    "output": text("output"),
                    "truncated": result.get("was_truncated").and_then(Value::as_bool).unwrap_or(false),
                });
                let transcript = shell_transcript(&output);
                let code = result
                    .get("returncode")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                (output, transcript, code != 0)
            } else {
                let stderr = match result.get("error") {
                    Some(Value::String(error)) => error.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let output =
                    json!({"stdout": "", "stderr": stderr, "output": "", "truncated": false});
                (output, stderr, true)
            };
            let display = result_display(!failed, "Ran", command.as_str());
            Some(completed_state(state, output, display, Some(output_text)))
        })),
    })
}

pub(super) fn project_skill(_name: &str, input: &Value) -> Option<Projected> {
    let skill = nonempty(input, "name")?.to_owned();
    let detail = detail(
        "skill",
        "skill",
        json!({"name": skill}),
        call_display(
            format!("Loading skill: {skill}"),
            "Loading",
            format!("skill: {skill}"),
            "Loaded",
            format!("skill: {skill}"),
            "Loading skill",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(move |state| {
            let mut updated = state.clone();
            updated["output"] = Value::Null;
            updated["display"] = result_display(true, "Loaded", format!("skill: {skill}"));
            Some(updated)
        })),
    })
}
