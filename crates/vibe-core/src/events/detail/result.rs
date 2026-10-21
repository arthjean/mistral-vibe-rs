//! How an effect settles: the header a client shows once the call answered,
//! failed, was skipped or was cut off, live or replayed from a transcript.

use std::path::Path;

use serde_json::Value;

use super::readers::{
    EDITED_FILE_PATH_KEYS, FILE_PATH_KEYS, answered_message, display_file_path, read_line_count,
    read_was_truncated, scratchpad_suffix, search_match_count, string_argument, tagged_text,
    truncated_flag,
};
use super::remote::RemoteToolResult;
use super::{
    EffectCallDisplay, EffectDetail, EffectResultDisplay, RemoteSettlement, RemoteToolOrigin,
    ToolEffectKind, has_output_model, project_output,
};
use crate::events::{EffectApproval, PersistedToolResult, PublicEffectState, PublicError};
use crate::pycompat;

/// The label a settled header carries when nothing named why a call was
/// skipped.
const SKIPPED: &str = "Skipped";

/// The label a proxied call settles under when its payload is not a result the
/// remote model accepts.
const NO_RESULT: &str = "No result";

impl EffectResultDisplay {
    /// The header of a call that failed. The subject is unchanged, so a failed
    /// call still names what it was acting on.
    #[must_use]
    pub fn failed(call: &EffectCallDisplay) -> Self {
        Self {
            success: false,
            verb: call.settled_verb.clone(),
            message: call.subject().to_owned(),
            warnings: Vec::new(),
            approval_note: None,
            suffix: String::new(),
        }
    }

    /// The header of a call that never ran to completion.
    #[must_use]
    pub fn skipped(tool_name: &str) -> Self {
        Self {
            success: false,
            verb: String::new(),
            message: format!("{tool_name}: skipped"),
            warnings: Vec::new(),
            approval_note: None,
            suffix: String::new(),
        }
    }

    /// The header a proxied call settles under.
    ///
    /// The reference reads the three branches in this order and stops at the
    /// first that answers: an errored call reports what went wrong, a skipped
    /// call reports why it never ran, and only then does the proxy read the
    /// result its server sent. Ordering them anywhere but here would let a
    /// remote failure render as a success.
    #[must_use]
    pub fn for_remote(
        remote: &RemoteToolOrigin,
        call: &EffectCallDisplay,
        settled: &RemoteSettlement<'_>,
    ) -> Self {
        if !settled.error.is_empty() {
            return Self {
                success: false,
                verb: call.settled_verb.clone(),
                message: settled.error.to_owned(),
                warnings: Vec::new(),
                approval_note: None,
                suffix: String::new(),
            };
        }
        if settled.skipped {
            return Self {
                success: false,
                verb: String::new(),
                message: if settled.skip_reason.is_empty() {
                    SKIPPED.to_owned()
                } else {
                    settled.skip_reason.to_owned()
                },
                warnings: Vec::new(),
                approval_note: None,
                suffix: String::new(),
            };
        }
        // A payload the remote result model does not accept settles as a
        // failure: the reference's proxies check the result's class before
        // reading it and report having no result rather than presenting a shape
        // they cannot read.
        let Ok(result) = serde_json::from_value::<RemoteToolResult>(settled.output.clone()) else {
            return Self {
                success: false,
                verb: String::new(),
                message: NO_RESULT.to_owned(),
                warnings: Vec::new(),
                approval_note: None,
                suffix: String::new(),
            };
        };
        // Only the proxy's own result answers for the call. The server's own
        // structured content says nothing about whether the call succeeded, so
        // a payload that is not the proxy's settles from what this port knows:
        // the call answered, under the name the origin carries.
        let (ok, answered) = if result.is_the_proxys_own() {
            let answered = result
                .tool
                .filter(|tool| !tool.is_empty())
                .unwrap_or_else(|| remote.remote_name.clone());
            (result.ok, answered)
        } else {
            (true, remote.remote_name.clone())
        };
        Self {
            success: ok,
            verb: "Ran".to_owned(),
            message: remote.settled_subject(&answered),
            warnings: Vec::new(),
            approval_note: None,
            suffix: String::new(),
        }
    }

    /// The header of a cancelled call, or nothing when it produced no result.
    ///
    /// A cancellation is the one settled state the reference lets carry a null
    /// display, because a call cancelled before it produced anything has no
    /// result to present.
    #[must_use]
    pub fn cancelled(tool_name: &str, output: &Value, emitted: &Value) -> Option<Self> {
        if let Ok(display) = serde_json::from_value::<Self>(emitted.clone()) {
            return Some(display);
        }
        (!output.is_null()).then(|| Self::skipped(tool_name))
    }

    /// The header of a completed call, read from its typed output.
    ///
    /// A tool that emitted its own result display keeps it: the reference
    /// widgets consume `state.display` without re-deriving it.
    #[must_use]
    pub fn completed(
        kind: ToolEffectKind,
        call: &EffectCallDisplay,
        output: &Value,
        emitted: &Value,
    ) -> Self {
        Self::completed_at(kind, call, output, emitted, None)
    }

    /// The same, for a session sitting in `working_directory`, which is what a
    /// file path the result names is displayed against.
    #[must_use]
    pub fn completed_at(
        kind: ToolEffectKind,
        call: &EffectCallDisplay,
        output: &Value,
        emitted: &Value,
        working_directory: Option<&Path>,
    ) -> Self {
        if let Ok(display) = serde_json::from_value::<Self>(emitted.clone()) {
            return display;
        }
        let warnings = output
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let Header {
            verb,
            message,
            suffix,
        } = completed_header(kind, call, output, working_directory);
        // Reference `Task.get_result_display`: the header counts the child's
        // turns, and a child that did not run to its end reads as interrupted.
        if kind == ToolEffectKind::Subagent
            && let Some(completed) = output.get("completed").and_then(Value::as_bool)
        {
            let turns = ["turnsUsed", "turns_used"]
                .iter()
                .find_map(|key| output.get(*key).and_then(Value::as_u64))
                .unwrap_or(0);
            let unit = if turns == 1 { "turn" } else { "turns" };
            return Self {
                success: completed,
                verb: if completed {
                    "Completed"
                } else {
                    "Interrupted"
                }
                .to_owned(),
                message: if completed {
                    format!("in {turns} {unit}")
                } else {
                    format!("after {turns} {unit}")
                },
                warnings,
                approval_note: None,
                suffix: String::new(),
            };
        }
        if kind == ToolEffectKind::UserQuestion
            && output
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            return Self {
                success: false,
                verb: "Cancelled".to_owned(),
                message: "by user".to_owned(),
                warnings,
                approval_note: None,
                suffix: String::new(),
            };
        }
        Self {
            success: true,
            verb,
            message,
            warnings,
            approval_note: None,
            suffix,
        }
    }
}

/// The three strings a settled effect's header is built from, named so a caller
/// cannot transpose them.
struct Header {
    /// What the tool did, in the past tense a client renders.
    verb: String,
    /// What it did it to.
    message: String,
    /// The parenthetical a truncated or capped result carries, empty otherwise.
    suffix: String,
}

impl Header {
    fn new(verb: impl Into<String>, message: impl Into<String>, suffix: impl Into<String>) -> Self {
        Self {
            verb: verb.into(),
            message: message.into(),
            suffix: suffix.into(),
        }
    }
}

/// The header a settled call publishes, per effect kind.
///
/// Reference builds each one from the tool's own typed result, so the shape a
/// client renders is one table rather than a widget per tool.
fn completed_header(
    kind: ToolEffectKind,
    call: &EffectCallDisplay,
    output: &Value,
    working_directory: Option<&Path>,
) -> Header {
    /// The parenthetical a capped or clipped result carries.
    const TRUNCATED: &str = "(truncated)";
    let truncated = |clipped: bool| if clipped { TRUNCATED } else { "" };

    match kind {
        ToolEffectKind::Shell => {
            let command = string_argument(output, &["command"]);
            let subject = if command.is_empty() {
                call.subject().to_owned()
            } else {
                command
            };
            Header::new("Ran", subject, "")
        }
        ToolEffectKind::FileRead => {
            let lines = read_line_count(output);
            let word = if lines == 1 { "line" } else { "lines" };
            let path = string_argument(output, FILE_PATH_KEYS);
            let shown = display_file_path(&path, working_directory);
            // Reference `ReadFileTool.get_result_display` joins both marks with
            // a space, the scratchpad one first.
            let suffix = [
                scratchpad_suffix(&path),
                truncated(read_was_truncated(output, lines)),
            ]
            .into_iter()
            .filter(|mark| !mark.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            Header::new("Read", format!("{lines} {word} from {shown}"), suffix)
        }
        ToolEffectKind::FileWrite => {
            let path = string_argument(output, FILE_PATH_KEYS);
            Header::new(
                "Created",
                display_file_path(&path, working_directory),
                scratchpad_suffix(&path),
            )
        }
        ToolEffectKind::FileEdit => {
            let path = string_argument(output, EDITED_FILE_PATH_KEYS);
            Header::new(
                "Edited",
                display_file_path(&path, working_directory),
                scratchpad_suffix(&path),
            )
        }
        ToolEffectKind::FileSearch => {
            let count = search_match_count(output);
            let word = if count == 1 { "match" } else { "matches" };
            let pattern = string_argument(output, &["pattern"]);
            let subject = if pattern.is_empty() {
                format!("({count} {word})")
            } else {
                format!("{pattern} ({count} {word})")
            };
            Header::new("Searched", subject, truncated(truncated_flag(output)))
        }
        ToolEffectKind::Todo => {
            let verb = string_argument(output, &["verb"]);
            let count = output
                .get("total_count")
                .or_else(|| output.get("totalCount"))
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    output
                        .get("todos")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len) as u64
                });
            let verb = if verb.is_empty() {
                "Updated".to_owned()
            } else {
                verb
            };
            Header::new(verb, format!("{count} todos"), "")
        }
        ToolEffectKind::UserQuestion => Header::new("Answered", answered_message(output), ""),
        ToolEffectKind::WebSearch => {
            let sources = output
                .get("sources")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let plural = if sources == 1 { "" } else { "s" };
            Header::new(
                "Searched",
                format!(
                    "'{}' ({sources} source{plural})",
                    string_argument(output, &["query"])
                ),
                "",
            )
        }
        ToolEffectKind::WebFetch => {
            let content_length = output
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let content_type = string_argument(output, &["content_type", "contentType"]);
            Header::new(
                "Fetched",
                format!(
                    "{} ({} chars, {})",
                    string_argument(output, &["url"]),
                    pycompat::thousands(content_length),
                    content_type.split(';').next().unwrap_or_default()
                ),
                truncated(truncated_flag(output)),
            )
        }
        ToolEffectKind::Skill => Header::new("Loaded", call.subject(), ""),
        ToolEffectKind::Subagent => Header::new("Completed", call.subject(), ""),
        ToolEffectKind::Worktree => Header::new("Created", call.subject(), ""),
        ToolEffectKind::Tool | ToolEffectKind::Process | ToolEffectKind::Scratchpad => {
            Header::new("Ran", call.subject(), "")
        }
    }
}

/// Reference `_persisted_effect_state` (`vibe/app_server/_projection.py`):
/// how a replayed call settled, read from the answer its transcript holds.
/// A call with no answer was cut off by the session's end; a tagged answer is
/// a cancellation or a failure; an answer with a recorded result settles as
/// that result presented.
#[must_use]
pub fn persisted_effect_state(
    detail: &EffectDetail,
    answer: Option<(&str, Option<&PersistedToolResult>)>,
) -> PublicEffectState {
    let Some((content, persisted)) = answer else {
        let reason = "Tool did not complete before the session ended";
        return PublicEffectState::Cancelled {
            reason: reason.to_owned(),
            output_text: String::new(),
            duration_ms: 0,
            display: Some(EffectResultDisplay {
                success: false,
                message: reason.to_owned(),
                ..EffectResultDisplay::default()
            }),
            approval: EffectApproval::default(),
        };
    };
    let (tag, message) = tagged_text(content);
    let display_message = if message.is_empty() {
        format!("{} completed", detail.tool_name)
    } else {
        message.clone()
    };
    let plain = |message: &str, success: bool| EffectResultDisplay {
        success,
        message: message.to_owned(),
        ..EffectResultDisplay::default()
    };
    match tag {
        Some("user_cancellation") => {
            return PublicEffectState::Cancelled {
                display: Some(plain(&display_message, false)),
                reason: display_message,
                output_text: message,
                duration_ms: 0,
                approval: EffectApproval::default(),
            };
        }
        Some("tool_error") => {
            return PublicEffectState::Failed {
                error: PublicError {
                    message: display_message.clone(),
                    code: None,
                    details: Value::Null,
                },
                output: Value::Null,
                output_text: message,
                duration_ms: 0,
                display: plain(&display_message, false),
                approval: EffectApproval::default(),
            };
        }
        _ => {}
    }
    let Some(persisted) = persisted else {
        return PublicEffectState::Completed {
            output: Value::Null,
            output_text: message,
            duration_ms: 0,
            display: plain(&display_message, true),
            approval: EffectApproval::default(),
        };
    };
    let presentation = persisted.presentation.as_ref();
    let display = presentation
        .and_then(|presentation| presentation.get("display"))
        .and_then(|display| serde_json::from_value(display.clone()).ok())
        .unwrap_or_else(|| plain(&display_message, true));
    // The reference keeps fractional milliseconds; a call that ran is never
    // reported as taking none.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let duration_ms = persisted
        .duration
        .as_ref()
        .and_then(serde_json::Number::as_f64)
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map_or(0, |seconds| (seconds * 1_000.0).ceil() as u64);
    if persisted.cancelled {
        return PublicEffectState::Cancelled {
            reason: display.message.clone(),
            output_text: message,
            duration_ms,
            display: Some(display),
            approval: EffectApproval::default(),
        };
    }
    let kind = presentation
        .and_then(|presentation| presentation.get("kind"))
        .and_then(|kind| serde_json::from_value(kind.clone()).ok())
        .unwrap_or(detail.kind);
    let output = presentation
        .and_then(|presentation| presentation.get("projectedOutput"))
        .filter(|projected| !projected.is_null())
        .unwrap_or(&persisted.output);
    PublicEffectState::Completed {
        output: if has_output_model(kind) {
            project_output(kind, output)
        } else {
            output.clone()
        },
        output_text: message,
        duration_ms,
        display,
        approval: EffectApproval::default(),
    }
}
