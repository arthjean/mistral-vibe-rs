use serde_json::{Map, Value, json};

use super::super::state::{EntryStatus, TuiState};
use super::super::{InteractiveRuntime, call_runtime, persist_user_setting, push_local_notice};

pub(in crate::tui) fn apply_thinking(
    runtime: &mut InteractiveRuntime,
    value: &str,
    state: &mut TuiState,
) {
    if !matches!(value, "off" | "low" | "medium" | "high" | "max") {
        state.push_diagnostic("Thinking must be off, low, medium, high, or max");
        return;
    }
    if !persist_user_setting(runtime, &["thinking"], json!(value), false, state) {
        return;
    }
    let mut params = Map::from_iter([
        ("sessionId".to_owned(), json!(runtime.session_id)),
        ("thinking".to_owned(), json!(value != "off")),
    ]);
    if value != "off" {
        params.insert("reasoningEffort".to_owned(), json!(value));
    }
    if call_runtime(
        runtime,
        "session/settings/update",
        Value::Object(params),
        state,
    )
    .is_some()
    {
        runtime.thinking = value.to_owned();
        push_local_notice(
            state,
            "Thinking preference and active session updated",
            EntryStatus::Completed,
        );
    }
}

pub(super) fn set_config_value(
    arguments: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let arguments = arguments.trim();
    let (target, arguments) = if let Some(arguments) = arguments.strip_prefix("--target ") {
        let Some((target, arguments)) = arguments.split_once(char::is_whitespace) else {
            state.push_diagnostic(
                "Usage: /settings set --target <user|project> <path> <JSON-or-text-value>",
            );
            return;
        };
        (target, arguments.trim())
    } else {
        ("user", arguments)
    };
    if !matches!(target, "user" | "project") {
        state.push_diagnostic("Configuration target must be `user` or `project`");
        return;
    }
    let Some((path, raw)) = arguments.split_once(char::is_whitespace) else {
        state.push_diagnostic(
            "Usage: /settings set [--target user|project] <path> <JSON-or-text-value>",
        );
        return;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        state.push_diagnostic("Configuration value cannot be empty");
        return;
    }
    let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
    let path = path
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if path.is_empty() {
        state.push_diagnostic("Configuration path cannot be empty");
    } else if persist_config_setting(runtime, target, &path, value, false, state) {
        apply_render_preferences(runtime, state);
        push_local_notice(state, "Configuration value saved", EntryStatus::Completed);
    }
}

pub(super) fn reset_config_value(
    path: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    reset_config_value_at(path, "user", runtime, state);
}

pub(super) fn reset_config_value_at(
    path: &str,
    target: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let path = path
        .trim()
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if path.is_empty() {
        state.push_diagnostic("Usage: /settings reset <path>");
    } else if persist_config_setting(runtime, target, &path, Value::Null, true, state) {
        apply_render_preferences(runtime, state);
        push_local_notice(state, "Configuration value reset", EntryStatus::Completed);
    }
}

pub(super) fn selected_config_target(runtime: &mut InteractiveRuntime) -> Option<String> {
    runtime
        .service
        .public_call("config/read", json!({}))
        .ok()?
        .get("snapshot")?
        .get("selectedTarget")?
        .as_str()
        .map(ToOwned::to_owned)
}

pub(super) fn persist_config_setting(
    runtime: &mut InteractiveRuntime,
    target: &str,
    path: &[&str],
    value: Value,
    remove: bool,
    state: &mut TuiState,
) -> bool {
    let snapshot = match runtime.service.public_call("config/read", json!({})) {
        Ok(result) => result,
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return false;
        }
    };
    let expected_fingerprint = snapshot
        .get("snapshot")
        .and_then(|snapshot| snapshot.pointer(&format!("/fingerprints/{target}")))
        .cloned()
        .unwrap_or(Value::Null);
    let mutation = if remove {
        json!({"path": path, "remove": true})
    } else {
        json!({"path": path, "value": value})
    };
    call_runtime(
        runtime,
        "config/batchWrite",
        json!({
            "writes": [{
                "target": target,
                "expectedFingerprint": expected_fingerprint,
                "mutations": [mutation],
            }],
        }),
        state,
    )
    .is_some()
}

pub(super) fn configured_value(runtime: &mut InteractiveRuntime, key: &str) -> Option<Value> {
    runtime
        .service
        .public_call("config/read", json!({}))
        .ok()?
        .get("snapshot")?
        .get("config")?
        .pointer(&format!("/{}", key.replace('.', "/")))
        .cloned()
}

pub(super) fn apply_render_preferences(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    state.show_reasoning = configured_value(runtime, "show_thinking_nodes")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
}
