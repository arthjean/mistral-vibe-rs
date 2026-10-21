//! The validators the reference writes itself, by the name the schema records.
//!
//! Each takes the value the schema already validated and answers it, possibly
//! rewritten, or the reason it is refused. The reasons are this port's own
//! sentences.

use serde_json::Value;

pub(super) fn run(function: &str, value: Value) -> Result<Value, String> {
    match function {
        // Reference `AgentConfig._reject_empty_cwd`.
        "AgentConfig._reject_empty_cwd" => match &value {
            Value::String(text) if text.is_empty() => {
                Err("a session directory cannot be empty".to_owned())
            }
            _ => Ok(value),
        },
        // Reference `UserDisplayContent.strip_nonempty`.
        "UserDisplayContent.strip_nonempty" => match value {
            Value::String(text) => {
                let stripped = text.trim();
                if stripped.is_empty() {
                    Err("the value cannot be blank".to_owned())
                } else {
                    Ok(Value::String(stripped.to_owned()))
                }
            }
            other => Ok(other),
        },
        // Reference `SessionSettingsUpdateParams.require_update`.
        "SessionSettingsUpdateParams.require_update" => {
            let set = |key: &str| value.get(key).is_some_and(|value| !value.is_null());
            if set("maxTurns") || set("maxTokens") {
                Ok(value)
            } else {
                Err("name at least one setting to update".to_owned())
            }
        }
        // Reference `SessionShellCommandParams.validate_action`.
        "SessionShellCommandParams.validate_action" => {
            let action = value.get("action").and_then(Value::as_str).unwrap_or("run");
            let command_blank = value
                .get("command")
                .and_then(Value::as_str)
                .is_none_or(|command| command.trim().is_empty());
            let operation_missing = value.get("operationId").is_none_or(Value::is_null);
            if action == "run" && command_blank {
                Err("running a shell command needs a command".to_owned())
            } else if action == "interrupt" && operation_missing {
                Err("interrupting a shell command needs its operation".to_owned())
            } else {
                Ok(value)
            }
        }
        // Reference `_TurnQueueInputParams.validate_entries`, through
        // `validate_turn_input_entries`.
        "_TurnQueueInputParams.validate_entries" => {
            let roles = value
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| {
                            entry
                                .get("role")
                                .and_then(Value::as_str)
                                .unwrap_or("context")
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let users = roles.iter().filter(|role| **role == "user").count();
            if users > 1 {
                Err("a turn takes at most one user entry".to_owned())
            } else if users == 1 && roles.last() != Some(&"user") {
                Err("the user entry has to come last".to_owned())
            } else {
                Ok(value)
            }
        }
        // Reference `WorkspaceWorktreeReapParams.validate_request_identity`.
        "WorkspaceWorktreeReapParams.validate_request_identity" => {
            let present = |key: &str| value.get(key).is_some_and(|value| !value.is_null());
            if present("requesterId") == present("requestId") {
                Ok(value)
            } else {
                Err("name both the requester and the request, or neither".to_owned())
            }
        }
        // Reference `WorkspaceWorktreeReapCancelParams.validate_request_identity`.
        "WorkspaceWorktreeReapCancelParams.validate_request_identity" => {
            let present = |key: &str| value.get(key).is_some_and(|value| !value.is_null());
            if present("requestId") && !present("requesterId") {
                Err("a request cannot be named without its requester".to_owned())
            } else {
                Ok(value)
            }
        }
        // Reference `SessionEmbeddedResourceContentBlock.validate_content`.
        "SessionEmbeddedResourceContentBlock.validate_content" => {
            let present = |key: &str| value.get(key).is_some_and(|value| !value.is_null());
            if present("text") == present("blob") {
                Err("an embedded resource carries exactly one of text or blob".to_owned())
            } else {
                Ok(value)
            }
        }
        _ => Ok(value),
    }
}

/// Each `function-before` validator: it takes the raw input and answers
/// what the schema under it validates.
pub(super) fn before(function: &str, input: &Value) -> Value {
    match (function, input) {
        // Reference `_infer_resource_kind` (`vibe/user_content.py`): a
        // resource stored without its `kind` tag takes the variant its
        // payload names, `text` before `blob`.
        ("_infer_resource_kind", Value::Object(entries)) if input.get("kind").is_none() => {
            let kind = if input.get("text").is_some() {
                "text"
            } else if input.get("blob").is_some() {
                "blob"
            } else {
                return input.clone();
            };
            let mut entries = entries.clone();
            entries.insert("kind".to_owned(), Value::String(kind.to_owned()));
            Value::Object(entries)
        }
        _ => input.clone(),
    }
}

/// The validator names this module implements, which the schema has to
/// stay within.
#[cfg(test)]
pub(super) const KNOWN: &[&str] = &[
    "AgentConfig._reject_empty_cwd",
    "SessionEmbeddedResourceContentBlock.validate_content",
    "SessionSettingsUpdateParams.require_update",
    "SessionShellCommandParams.validate_action",
    "UserDisplayContent.strip_nonempty",
    "WorkspaceWorktreeReapCancelParams.validate_request_identity",
    "WorkspaceWorktreeReapParams.validate_request_identity",
    "_TurnQueueInputParams.validate_entries",
    "_infer_resource_kind",
];
