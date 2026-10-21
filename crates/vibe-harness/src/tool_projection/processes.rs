//! The background process group: `process.start`, `stop`, `output`, `list`
//! and `write`.

use serde_json::{Value, json};

use super::{
    Projected, call_display, completed_state, detail, nonempty, result_display, structured,
};

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
fn format_bytes(count: u64) -> String {
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

pub(super) fn project_process_start(name: &str, input: &Value) -> Option<Projected> {
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

pub(super) fn project_process_stop(name: &str, input: &Value) -> Option<Projected> {
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

pub(super) fn project_process_output(name: &str, input: &Value) -> Option<Projected> {
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

pub(super) fn project_process_list(name: &str, input: &Value) -> Option<Projected> {
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

pub(super) fn project_process_write(name: &str, input: &Value) -> Option<Projected> {
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
