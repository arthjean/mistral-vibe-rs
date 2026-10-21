//! Typed effect details for the calls the harness reports generically.
//!
//! The harness publishes every tool call under its namespaced key with a
//! generic detail (`file_system.read_file`, `vibe.todo`); the adapter rewrites
//! the ones a client renders richly into their typed detail and settles their
//! results into the matching output models. Reference
//! `vibe/app_server/_unified_tool_projection.py`, over the wire models of
//! `vibe/app_server/_effect_models.py`.

use serde_json::{Map, Value, json};
use vibe_core::pycompat;

/// The page a read takes when the call names none.
pub const DEFAULT_READ_LIMIT: u64 = 2000;

const SEARCH_REPLACE_ANNOTATION: &str = "mistralai.vibe.sdk.search_replace";
const WRITE_FILE_ANNOTATION: &str = "mistralai.vibe.sdk.write_file";

/// How a projected call's result settles.
type SettleResult = Box<dyn Fn(&Value) -> Option<Value>>;

/// A call rewritten into its typed detail, and how its result settles.
struct Projected {
    detail: Value,
    result: Option<SettleResult>,
}

/// Re-projects one wire effect entry (already in wire shape, detail generic).
/// An entry whose call this surface does not know, or whose arguments do not
/// fit, stays as it is.
pub fn project_effect(entry: &mut Value) {
    let Some(projected) = project_call(&entry["detail"]) else {
        return;
    };
    if entry["state"]["status"] == "completed"
        && let Some(settle) = &projected.result
    {
        match settle(&entry["state"]) {
            Some(state) => entry["state"] = state,
            None => return,
        }
    }
    entry["detail"] = projected.detail;
}

/// Re-projects the effect an approval callback describes.
pub fn project_callback_effect(effect: &mut Value) {
    if let Some(projected) = project_call(effect) {
        *effect = projected.detail;
    }
}

fn project_call(detail: &Value) -> Option<Projected> {
    if detail["kind"] != "tool" {
        return None;
    }
    let name = detail["toolName"].as_str()?;
    let input = &detail["input"];
    let projector: Option<fn(&str, &Value) -> Option<Projected>> = match name {
        "read_file" | "file_system.read_file" => Some(project_read),
        "write_file" | "file_system.write_file" => Some(project_write),
        "search_replace" | "file_system.search_replace" => Some(project_search_replace),
        "bash" | "file_system.bash" => Some(project_shell),
        "skill.read" => Some(project_skill),
        "process.start" => Some(project_process_start),
        "process.stop" => Some(project_process_stop),
        "process.output" => Some(project_process_output),
        "process.list" => Some(project_process_list),
        "process.write" => Some(project_process_write),
        "ui.ask_user_question" => Some(project_question),
        "web_search" => Some(project_web_search),
        "web_fetch" => Some(project_web_fetch),
        "grep" => Some(project_grep),
        "todo" | "vibe.todo" => Some(project_todo),
        "vibe.cron" => Some(project_cron),
        "vibe.unified_harness_scratchpad" => Some(project_scratchpad),
        _ => None,
    };
    if let Some(projector) = projector {
        return projector(name, input);
    }
    label(name).map(|(verb, noun, settled)| project_labeled(detail, verb, noun, settled))
}

// --------------------------------------------------------------------------
// Shapes
// --------------------------------------------------------------------------

fn call_display(
    summary: impl Into<String>,
    verb: &str,
    message: impl Into<String>,
    settled_verb: &str,
    settled_message: impl Into<String>,
    status_text: impl Into<String>,
) -> Value {
    json!({
        "summary": summary.into(),
        "content": null,
        "suffix": "",
        "verb": verb,
        "message": message.into(),
        "settledVerb": settled_verb,
        "settledMessage": settled_message.into(),
        "statusText": status_text.into(),
    })
}

/// Reference `EffectResultDisplay` with every default.
#[must_use]
pub fn result_display(success: bool, verb: &str, message: impl Into<String>) -> Value {
    json!({
        "success": success,
        "verb": verb,
        "message": message.into(),
        "warnings": [],
        "approvalNote": null,
        "suffix": "",
    })
}

fn detail(kind: &str, tool_name: &str, input: Value, display: Value) -> Value {
    json!({"toolName": tool_name, "display": display, "kind": kind, "input": input})
}

fn structured(state: &Value) -> Option<&Map<String, Value>> {
    state["output"]["structured_content"].as_object()
}

fn meta(state: &Value) -> Option<&Map<String, Value>> {
    state["output"]["_meta"].as_object()
}

/// Reference `_completed_state`: the projected output and display, carrying
/// over the approval note and warnings the incoming display had.
fn completed_state(
    state: &Value,
    output: Value,
    mut display: Value,
    output_text: Option<String>,
) -> Value {
    let incoming = &state["display"];
    let mut warnings: Vec<Value> = display["warnings"].as_array().cloned().unwrap_or_default();
    for warning in incoming["warnings"].as_array().into_iter().flatten() {
        if !warnings.contains(warning) {
            warnings.push(warning.clone());
        }
    }
    display["warnings"] = Value::Array(warnings);
    if incoming["approvalNote"].is_string() {
        display["approvalNote"] = incoming["approvalNote"].clone();
    }
    let mut updated = state.clone();
    updated["output"] = output;
    updated["display"] = display;
    if let Some(text) = output_text {
        updated["outputText"] = json!(text);
    }
    updated
}

fn str_field<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn nonempty<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    str_field(object, key).filter(|value| !value.is_empty())
}

fn plural(count: u64, singular: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {many}")
    }
}

// --------------------------------------------------------------------------
// Files and the shell
// --------------------------------------------------------------------------

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

fn project_read(name: &str, input: &Value) -> Option<Projected> {
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

fn project_write(name: &str, input: &Value) -> Option<Projected> {
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

fn project_search_replace(name: &str, input: &Value) -> Option<Projected> {
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

/// Reference `ShellEffectOutput.transcript`.
#[must_use]
pub fn shell_transcript(stdout: &str, stderr: &str, output: &str) -> String {
    if !output.is_empty() {
        return output.to_owned();
    }
    if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
        return format!("{stdout}\n{stderr}");
    }
    format!("{stdout}{stderr}")
}

fn project_shell(name: &str, input: &Value) -> Option<Projected> {
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
                let transcript = shell_transcript(text("stdout"), text("stderr"), text("output"));
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

fn project_skill(_name: &str, input: &Value) -> Option<Projected> {
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

// --------------------------------------------------------------------------
// Background processes
// --------------------------------------------------------------------------

fn short_process_id(process_id: &str) -> String {
    let chars: Vec<char> = process_id.chars().collect();
    if chars.len() > 8 {
        chars[chars.len() - 8..].iter().collect()
    } else {
        process_id.to_owned()
    }
}

fn process_display(verb: &str, message: &str, settled: &str, status_text: Option<&str>) -> Value {
    let summary = format!("{verb} {message}").trim().to_owned();
    call_display(
        summary.clone(),
        verb,
        message,
        settled,
        message,
        status_text.map_or(summary, str::to_owned),
    )
}

/// Reference `_format_bytes`.
#[must_use]
pub fn format_bytes(count: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    #[allow(clippy::cast_precision_loss)]
    let scaled = |unit: u64| count as f64 / unit as f64;
    if count < KB {
        format!("{count}B")
    } else if count < MB {
        format!("{:.1}KB", scaled(KB))
    } else {
        format!("{:.1}MB", scaled(MB))
    }
}

/// Reference `ProcessEffectOutput` as the adapter builds it: the model
/// reads its fields under their Python names from a result keyed by their
/// camelCase aliases, so only `status`, `output` and `processes` come through
/// and every other field keeps its default.
fn process_envelope(tool_name: &str, fields: &[(&str, Value)]) -> Value {
    let mut envelope = json!({
        "tool_name": tool_name,
        "processId": null,
        "status": null,
        "exitCode": null,
        "output": null,
        "bytesAvailable": null,
        "hasMore": false,
        "truncatedBefore": false,
        "processes": null,
        "bytesWritten": null,
    });
    for (key, value) in fields {
        if matches!(*key, "status" | "output" | "processes") {
            envelope[*key] = value.clone();
        }
    }
    envelope
}

fn process_id(input: &Value) -> Option<String> {
    nonempty(input, "processId").map(str::to_owned)
}

fn project_process_start(name: &str, input: &Value) -> Option<Projected> {
    let command = nonempty(input, "command")?.to_owned();
    let display = process_display("Starting", &command, "Started", Some("Starting process"));
    Some(Projected {
        detail: detail("process", name, input.clone(), display),
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let process = result
                .get("processId")?
                .as_str()
                .filter(|id| !id.is_empty())?;
            let status = result.get("status")?.as_str()?;
            let mut display = result_display(true, "Started", command.as_str());
            if !status.is_empty() {
                display["suffix"] = json!(format!("({status})"));
            }
            let output = process_envelope(
                "process.start",
                &[("processId", json!(process)), ("status", json!(status))],
            );
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn project_process_stop(name: &str, input: &Value) -> Option<Projected> {
    let short = short_process_id(&process_id(input)?);
    let message = format!("process {short}");
    let display = process_display("Stopping", &message, "Stopped", None);
    Some(Projected {
        detail: detail("process", name, input.clone(), display),
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let process = result
                .get("processId")?
                .as_str()
                .filter(|id| !id.is_empty())?;
            let status = result.get("status")?.as_str()?;
            let output = process_envelope(
                "process.stop",
                &[
                    ("processId", json!(process)),
                    ("status", json!(status)),
                    (
                        "exitCode",
                        result.get("exitCode").cloned().unwrap_or(Value::Null),
                    ),
                ],
            );
            let display = result_display(true, "Stopped", message.as_str());
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn project_process_output(name: &str, input: &Value) -> Option<Projected> {
    let short = short_process_id(&process_id(input)?);
    let message = format!("process {short}");
    let display = process_display("Reading output from", &message, "Read", None);
    Some(Projected {
        detail: detail("process", name, input.clone(), display),
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let process = result
                .get("processId")?
                .as_str()
                .filter(|id| !id.is_empty())?;
            let status = result.get("status")?.as_str()?;
            let text = result
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let available = result
                .get("bytesAvailable")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let has_more = result
                .get("hasMore")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut display = result_display(
                true,
                "Read",
                format!("{} from {message}", format_bytes(available)),
            );
            if has_more {
                display["suffix"] = json!("(truncated)");
            }
            let output = process_envelope(
                "process.output",
                &[
                    ("processId", json!(process)),
                    ("status", json!(status)),
                    (
                        "exitCode",
                        result.get("exitCode").cloned().unwrap_or(Value::Null),
                    ),
                    (
                        "output",
                        if text.is_empty() {
                            Value::Null
                        } else {
                            json!(text)
                        },
                    ),
                    ("bytesAvailable", json!(available)),
                    ("hasMore", json!(has_more)),
                    (
                        "truncatedBefore",
                        json!(
                            result
                                .get("truncatedBefore")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                        ),
                    ),
                ],
            );
            let output_text = (!text.is_empty()).then(|| text.to_owned());
            Some(completed_state(state, output, display, output_text))
        })),
    })
}

fn project_process_list(name: &str, input: &Value) -> Option<Projected> {
    let display = process_display("Listing", "background processes", "Listed", None);
    Some(Projected {
        detail: detail("process", name, input.clone(), display),
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let processes: Vec<Value> = result
                .get("processes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|item| {
                    json!({
                        "processId": item["processId"],
                        "command": item["command"],
                        "status": item["status"],
                        "exitCode": item.get("exitCode").cloned().unwrap_or(Value::Null),
                        "outputPath": item.get("outputPath").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            let count = processes.len() as u64;
            let display = result_display(
                true,
                "Listed",
                format!(
                    "{count} background {}",
                    if count == 1 { "process" } else { "processes" }
                ),
            );
            let output = process_envelope("process.list", &[("processes", json!(processes))]);
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn project_process_write(name: &str, input: &Value) -> Option<Projected> {
    let short = short_process_id(&process_id(input)?);
    let message = format!("process {short}");
    let display = process_display("Writing to", &message, "Wrote", None);
    Some(Projected {
        detail: detail("process", name, input.clone(), display),
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let process = result
                .get("processId")?
                .as_str()
                .filter(|id| !id.is_empty())?;
            let status = result.get("status")?.as_str()?;
            let written = result.get("bytesWritten")?.as_u64()?;
            let display = result_display(
                true,
                "Wrote",
                format!("{} to {message}", format_bytes(written)),
            );
            let output = process_envelope(
                "process.write",
                &[
                    ("processId", json!(process)),
                    ("status", json!(status)),
                    ("bytesWritten", json!(written)),
                ],
            );
            Some(completed_state(state, output, display, None))
        })),
    })
}

// --------------------------------------------------------------------------
// Questions, the web and search
// --------------------------------------------------------------------------

/// Reference `UserQuestionRequest` with its defaults, or `None` when the
/// arguments do not validate.
#[must_use]
pub fn question_request(input: &Value) -> Option<Value> {
    let questions = input
        .get("questions")?
        .as_array()
        .filter(|q| !q.is_empty())?;
    let mut projected = Vec::new();
    for question in questions {
        let options = question
            .get("options")?
            .as_array()
            .filter(|o| o.len() >= 2)?;
        let header = question.get("header").map_or(Some(""), Value::as_str)?;
        if header.chars().count() > 20 {
            return None;
        }
        let mut choices = Vec::new();
        for option in options {
            choices.push(json!({
                "label": option.get("label")?.as_str()?,
                "description": option.get("description").map_or(Some(""), Value::as_str)?,
            }));
        }
        projected.push(json!({
            "question": question.get("question")?.as_str()?,
            "header": header,
            "options": choices,
            "multiSelect": question.get("multiSelect").map_or(Some(false), Value::as_bool)?,
            "hideOther": question.get("hideOther").map_or(Some(false), Value::as_bool)?,
        }));
    }
    let footer = match input.get("footerNote") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(note)) => json!(note),
        Some(_) => return None,
    };
    Some(json!({"questions": projected, "footerNote": footer}))
}

fn project_question(name: &str, input: &Value) -> Option<Projected> {
    let request = question_request(input)?;
    let questions = request["questions"].as_array()?;
    let text = if questions.len() == 1 {
        questions[0]["question"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    } else {
        format!("{} questions", questions.len())
    };
    let detail = detail(
        "user_question",
        name,
        request,
        call_display(
            format!("Asking: {text}"),
            "Asking",
            text.as_str(),
            "Asked",
            text.as_str(),
            "Waiting for user input",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let cancelled = result
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut answers = Vec::new();
            for answer in result.get("answers")?.as_array()? {
                answers.push(json!({
                    "question": answer.get("question")?.as_str()?,
                    "answer": answer.get("answer")?.as_str()?,
                    "isOther": answer.get("isOther").map_or(Some(false), Value::as_bool)?,
                }));
            }
            let display = if cancelled {
                result_display(false, "Cancelled", "by user")
            } else {
                let parts: Vec<String> = answers
                    .iter()
                    .map(|answer| {
                        format!(
                            "\"{}\" → {}{}",
                            answer["question"].as_str().unwrap_or_default(),
                            if answer["isOther"] == true {
                                "(Other) "
                            } else {
                                ""
                            },
                            answer["answer"].as_str().unwrap_or_default()
                        )
                    })
                    .collect();
                result_display(true, "Answered", parts.join(" · "))
            };
            let output = json!({"answers": answers, "cancelled": cancelled});
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn project_web_search(name: &str, input: &Value) -> Option<Projected> {
    let query = nonempty(input, "query")?.to_owned();
    let quoted = pycompat::repr_str(&query);
    let detail = detail(
        "web_search",
        name,
        json!({"query": query}),
        call_display(
            format!("Searching the web: {quoted}"),
            "Searching",
            format!("the web: {quoted}"),
            "Searched",
            format!("the web: {quoted}"),
            "Searching the web",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let query = result.get("query")?.as_str()?;
            let answer = result.get("answer").map_or(Some(""), Value::as_str)?;
            let mut sources = Vec::new();
            for source in result
                .get("sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                sources.push(json!({
                    "title": source.get("title")?.as_str()?,
                    "url": source.get("url")?.as_str()?,
                }));
            }
            let count = sources.len();
            let display = result_display(
                true,
                "Searched",
                format!(
                    "{} ({count} source{})",
                    pycompat::repr_str(query),
                    if count == 1 { "" } else { "s" }
                ),
            );
            let output = json!({"query": query, "answer": answer, "sources": sources});
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn url_domain(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest);
    let netloc = after_scheme
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    if netloc.is_empty() {
        url.chars().take(50).collect()
    } else {
        netloc.to_owned()
    }
}

fn project_web_fetch(name: &str, input: &Value) -> Option<Projected> {
    let url = nonempty(input, "url")?.to_owned();
    let timeout = match input.get("timeout") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64()?),
    };
    let mut message = url_domain(&url);
    if let Some(timeout) = timeout {
        message.push_str(&format!(" (timeout {timeout}s)"));
    }
    let detail = detail(
        "web_fetch",
        name,
        json!({"url": url, "timeout": timeout}),
        call_display(
            format!("Fetching: {message}"),
            "Fetching",
            message.as_str(),
            "Fetched",
            message.as_str(),
            "Fetching URL",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let url = result.get("url")?.as_str()?;
            let content = result.get("content").map_or(Some(""), Value::as_str)?;
            let content_type = result.get("content_type").map_or(Some(""), Value::as_str)?;
            let truncated = result
                .get("was_truncated")
                .map_or(Some(false), Value::as_bool)?;
            let mut display = result_display(
                true,
                "Fetched",
                format!(
                    "{url} ({} chars, {})",
                    pycompat::thousands(u64::try_from(content.chars().count()).unwrap_or(u64::MAX)),
                    content_type.split(';').next().unwrap_or_default()
                ),
            );
            if truncated {
                display["suffix"] = json!("(truncated)");
            }
            let output = json!({
                "url": url,
                "content": content,
                "contentType": content_type,
                "wasTruncated": truncated,
            });
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn project_grep(name: &str, input: &Value) -> Option<Projected> {
    let pattern = nonempty(input, "pattern")?.to_owned();
    let path = input
        .get("path")
        .map_or(Some("."), Value::as_str)?
        .to_owned();
    let max_matches = match input.get("max_matches") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64()?),
    };
    let default_ignore = input
        .get("use_default_ignore")
        .map_or(Some(true), Value::as_bool)?;
    let mut message = format!("'{pattern}'");
    if path != "." {
        message.push_str(&format!(" in {path}"));
    }
    if let Some(max) = max_matches {
        message.push_str(&format!(" (max {max} matches)"));
    }
    if !default_ignore {
        message.push_str(" [no-ignore]");
    }
    let detail = detail(
        "file_search",
        name,
        json!({"pattern": pattern, "path": path, "maxMatches": max_matches}),
        call_display(
            format!("Grepping {message}"),
            "Searching",
            message.as_str(),
            "Searched",
            message.as_str(),
            "Searching files",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let count = result.get("match_count")?.as_u64()?;
            let truncated = result
                .get("was_truncated")
                .map_or(Some(false), Value::as_bool)?;
            let mut display = result_display(
                true,
                "Searched",
                format!("{pattern} ({})", plural(count, "match", "matches")),
            );
            if truncated {
                display["suffix"] = json!("(truncated)");
            }
            let output = json!({
                "matches": result.get("matches").and_then(Value::as_str).unwrap_or_default(),
                "matchCount": count,
                "wasTruncated": truncated,
                "parsedMatches": result.get("parsed_matches").cloned().unwrap_or_else(|| json!([])),
            });
            Some(completed_state(state, output, display, None))
        })),
    })
}

// --------------------------------------------------------------------------
// Session tools
// --------------------------------------------------------------------------

/// A todo item with the wire's defaults, or `None` when it does not validate.
#[must_use]
pub fn todo_item(item: &Value) -> Option<Value> {
    let status = item.get("status").map_or(Some("pending"), Value::as_str)?;
    let priority = item.get("priority").map_or(Some("medium"), Value::as_str)?;
    if !["pending", "in_progress", "completed", "cancelled"].contains(&status)
        || !["low", "medium", "high"].contains(&priority)
    {
        return None;
    }
    Some(json!({
        "id": item.get("id")?.as_str()?,
        "content": item.get("content")?.as_str()?,
        "status": status,
        "priority": priority,
    }))
}

fn todo_items(value: &Value) -> Option<Vec<Value>> {
    value.as_array()?.iter().map(todo_item).collect()
}

fn project_todo(name: &str, input: &Value) -> Option<Projected> {
    let action = nonempty(input, "action")?.to_owned();
    let todos = match input.get("todos") {
        None | Some(Value::Null) => None,
        Some(todos) => Some(todo_items(todos)?),
    };
    let display = match action.as_str() {
        "read" => call_display(
            "Reading todos",
            "Retrieving",
            "todos",
            "Retrieved",
            "todos",
            "Managing todos",
        ),
        "write" => {
            let count = todos.as_ref().map_or(0, Vec::len);
            call_display(
                format!("Writing {count} todos"),
                "Updating",
                format!("{count} todos"),
                "Updated",
                format!("{count} todos"),
                "Managing todos",
            )
        }
        other => call_display(
            format!("Unknown action: {other}"),
            "Running",
            format!("unknown todo action: {other}"),
            "Ran",
            format!("unknown todo action: {other}"),
            "Managing todos",
        ),
    };
    let detail = detail(
        "todo",
        name,
        json!({"action": action, "todos": todos}),
        display,
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let verb = result.get("verb").map_or(Some(""), Value::as_str)?;
            let todos = match result.get("todos") {
                None => Vec::new(),
                Some(todos) => todo_items(todos)?,
            };
            let total = result.get("total_count")?.as_u64()?;
            let display = result_display(true, verb, format!("{total} todos"));
            Some(completed_state(
                state,
                json!({"todos": todos}),
                display,
                None,
            ))
        })),
    })
}

fn project_scratchpad(name: &str, input: &Value) -> Option<Projected> {
    let action = nonempty(input, "action")?;
    let path = str_field(input, "path");
    let (semantic, verb, settled, noun) = match action {
        "list" => (
            json!({"action": "list"}),
            "Listing",
            "Listed",
            "the scratchpad".to_owned(),
        ),
        "read" => {
            let path = path?;
            (
                json!({"action": "read", "path": path}),
                "Reading",
                "Read",
                path.to_owned(),
            )
        }
        "write" => {
            let path = path?;
            let content = str_field(input, "content")?;
            (
                json!({"action": "write", "path": path, "content": content}),
                "Saving",
                "Saved",
                path.to_owned(),
            )
        }
        _ => return None,
    };
    let display = call_display(
        format!("{verb} {noun}"),
        verb,
        noun.as_str(),
        settled,
        noun.as_str(),
        "Using the scratchpad",
    );
    Some(Projected {
        detail: detail("scratchpad", name, semantic, display),
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let verb = result
                .get("verb")?
                .as_str()
                .filter(|verb| !verb.is_empty())?;
            let settled = match result.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => path.to_owned(),
                _ => format!(
                    "{} files",
                    result
                        .get("files")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                ),
            };
            let mut updated = state.clone();
            updated["display"]["verb"] = json!(verb);
            updated["display"]["message"] = json!(settled);
            Some(updated)
        })),
    })
}

fn cron_schedule(interval: Option<i64>, cron: Option<&str>) -> Option<String> {
    match (interval, cron) {
        (None, Some(cron)) => Some(format!("cron {cron} (local time)")),
        (Some(mut seconds), None) => {
            let mut parts = Vec::new();
            for (divisor, unit) in [
                (86400, "day"),
                (3600, "hour"),
                (60, "minute"),
                (1, "second"),
            ] {
                let count = seconds / divisor;
                seconds %= divisor;
                if count != 0 {
                    parts.push(format!(
                        "{count} {unit}{}",
                        if count == 1 { "" } else { "s" }
                    ));
                }
            }
            Some(format!("every {}", parts.join(" ")))
        }
        _ => None,
    }
}

/// Reference `_cron_prompt_summary`.
#[must_use]
pub fn cron_prompt_summary(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

fn project_cron(name: &str, input: &Value) -> Option<Projected> {
    let action = str_field(input, "action")?.to_owned();
    let (verb, settled, message) = match action.as_str() {
        "schedule" => (
            "Scheduling",
            "Scheduled",
            format!(
                "{}: {}",
                cron_schedule(Some(input.get("interval_seconds")?.as_i64()?), None)?,
                cron_prompt_summary(nonempty(input, "prompt")?)
            ),
        ),
        "schedule_cron" => (
            "Scheduling",
            "Scheduled",
            format!(
                "{}: {}",
                cron_schedule(None, Some(nonempty(input, "cron")?))?,
                cron_prompt_summary(nonempty(input, "prompt")?)
            ),
        ),
        "list" => ("Listing", "Listed", "scheduled prompts".to_owned()),
        "cancel" => (
            "Cancelling",
            "Cancelled",
            format!("scheduled prompt {}", nonempty(input, "id")?),
        ),
        "clear" => ("Clearing", "Cleared", "scheduled prompts".to_owned()),
        _ => return None,
    };
    let display = call_display(
        format!("{verb} {message}"),
        verb,
        message.as_str(),
        settled,
        message.as_str(),
        format!("{verb} recurring prompts"),
    );
    let projected = json!({
        "toolName": name,
        "display": display,
        "kind": "tool",
        "input": input,
    });
    Some(Projected {
        detail: projected,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let loops = result.get("loops")?.as_array()?.clone();
            let cleared = result.get("cleared_count").and_then(Value::as_u64);
            let settled_message = match action.as_str() {
                "list" => {
                    let count = loops.len();
                    format!(
                        "{count} scheduled prompt{}",
                        if count == 1 { "" } else { "s" }
                    )
                }
                "cancel" if !loops.is_empty() => {
                    cron_prompt_summary(loops[0]["prompt"].as_str().unwrap_or_default())
                }
                "clear" => cleared.map_or_else(
                    || "scheduled prompts".to_owned(),
                    |count| {
                        format!(
                            "{count} scheduled loop{}",
                            if count == 1 { "" } else { "s" }
                        )
                    },
                ),
                _ => message.clone(),
            };
            let body = cron_result_text(result, &loops, cleared, &action)?;
            let mut updated = state.clone();
            updated["display"]["verb"] = json!(settled);
            updated["display"]["message"] = json!(settled_message);
            updated["output"] = json!(body);
            updated["outputText"] = json!(body);
            Some(updated)
        })),
    })
}

fn cron_result_text(
    result: &Map<String, Value>,
    loops: &[Value],
    cleared: Option<u64>,
    action: &str,
) -> Option<String> {
    if action == "clear" {
        return Some(cleared.map_or_else(
            || {
                result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            },
            |count| {
                format!(
                    "Cleared {count} scheduled loop{}",
                    if count == 1 { "" } else { "s" }
                )
            },
        ));
    }
    if loops.is_empty() {
        return Some("No scheduled prompts.".to_owned());
    }
    let mut sections = Vec::new();
    for entry in loops {
        let schedule = cron_schedule(
            entry.get("interval_seconds").and_then(Value::as_i64),
            entry.get("cron").and_then(Value::as_str),
        )?;
        let prompt = entry.get("prompt")?.as_str()?.replace('\n', "\n  ");
        let mut lines = vec![format!("Prompt: {prompt}"), format!("Schedule: {schedule}")];
        if action != "cancel" {
            let next = entry.get("next_fire_at")?.as_f64()?;
            lines.push(format!(
                "Next run: {}",
                crate::clock::local_timestamp(next)?
            ));
        }
        lines.push(format!("ID: {}", entry.get("id")?.as_str()?));
        sections.push(lines.join("\n"));
    }
    Some(sections.join("\n\n"))
}

// --------------------------------------------------------------------------
// Labels
// --------------------------------------------------------------------------

fn project_labeled(
    detail: &Value,
    verb: &'static str,
    noun: &'static str,
    settled: &'static str,
) -> Projected {
    let tool_name = detail["toolName"].as_str().unwrap_or_default().to_owned();
    let summary = format!("{verb} {noun}").trim().to_owned();
    let mut projected = detail.clone();
    projected["display"] = call_display(summary.clone(), verb, noun, settled, noun, summary);
    Projected {
        detail: projected,
        result: Some(Box::new(move |state| {
            if state["display"]["message"] != json!(tool_name) {
                return Some(state.clone());
            }
            let mut updated = state.clone();
            updated["display"]["verb"] = json!(settled);
            updated["display"]["message"] = json!(noun);
            Some(updated)
        })),
    }
}

/// The human header of a namespaced tool with no richer projection.
fn label(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match name {
        "ui.ask_user_question" => ("Asking", "a question", "Asked"),
        "self.sleep" => ("Sleeping", "", "Slept"),
        "subagent.list" => ("Listing", "subagents", "Listed"),
        "subagent.wait" => ("Waiting", "for a subagent", "Waited"),
        "subagent.send_message" => ("Sending", "a message to a subagent", "Sent"),
        "subagent.interrupt" => ("Interrupting", "a subagent", "Interrupted"),
        "subagent.stop" => ("Stopping", "a subagent", "Stopped"),
        _ => return crate::connector_labels::label(name),
    })
}
