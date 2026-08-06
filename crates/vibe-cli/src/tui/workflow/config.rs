use serde_json::{Map, Value, json};

use super::super::state::{EntryStatus, TuiState};
use super::super::{InteractiveRuntime, call_runtime, persist_setting, push_local_notice};

pub(in crate::tui) fn apply_thinking(
    runtime: &mut InteractiveRuntime,
    value: &str,
    target: Option<&str>,
    state: &mut TuiState,
) {
    if !matches!(value, "off" | "low" | "medium" | "high" | "max") {
        state.push_diagnostic("Thinking must be off, low, medium, high, or max");
        return;
    }
    let previous = runtime.thinking.clone();
    let params = thinking_params(&runtime.session_id, value);
    if call_runtime(
        runtime,
        "session/overrides/write",
        Value::Object(params),
        state,
    )
    .is_none()
    {
        return;
    }
    if !persist_setting(
        runtime,
        target.unwrap_or("user"),
        &["thinking"],
        json!(value),
        false,
        state,
    ) {
        if let Err(error) = runtime.service.public_call(
            "session/overrides/write",
            Value::Object(thinking_params(&runtime.session_id, &previous)),
        ) {
            state.push_diagnostic(format!(
                "Thinking preference save failed and the active session rollback also failed: {error}"
            ));
        }
        return;
    }
    runtime.thinking = value.to_owned();
    push_local_notice(
        state,
        "Thinking preference and active session updated",
        EntryStatus::Completed,
    );
}

fn thinking_params(session_id: &str, value: &str) -> Map<String, Value> {
    let mut params = Map::from_iter([
        ("sessionId".to_owned(), json!(session_id)),
        ("thinking".to_owned(), json!(value != "off")),
    ]);
    if value != "off" {
        params.insert("reasoningEffort".to_owned(), json!(value));
    }
    params
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
                "Usage: /config set --target <user|project> <path> <JSON-or-text-value>",
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
            "Usage: /config set [--target user|project] <path> <JSON-or-text-value>",
        );
        return;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        state.push_diagnostic("Configuration value cannot be empty");
        return;
    }
    let path = path
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if path.is_empty() {
        state.push_diagnostic("Configuration path cannot be empty");
        return;
    }
    let schema = match runtime
        .service
        .public_call("config/schema", json!({"sessionId": runtime.session_id}))
    {
        Ok(result) => result.get("schema").cloned().unwrap_or(Value::Null),
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return;
        }
    };
    let Some(field_schema) = config_schema_at(&schema, &path) else {
        state.push_diagnostic(format!(
            "Configuration field `{}` is not supported",
            path.join(".")
        ));
        return;
    };
    if field_is_secret(&path, field_schema) {
        state.push_diagnostic("Secret configuration fields cannot be edited in the TUI");
        return;
    }
    let value = match parse_typed_config_value(raw, field_schema) {
        Ok(value) => value,
        Err(message) => {
            state.push_diagnostic(message);
            return;
        }
    };
    if persist_config_setting(runtime, target, &path, value, false, state) {
        apply_render_preferences(runtime, state);
        push_local_notice(state, "Configuration value saved", EntryStatus::Completed);
    }
}

pub(super) fn update_proxy_value(
    arguments: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let Some(parts) = shlex::split(arguments) else {
        state.push_diagnostic("Invalid quoting in /proxy-setup arguments");
        return;
    };
    let Some(key) = parts.first().map(|key| key.to_ascii_uppercase()) else {
        state.push_diagnostic("Usage: /proxy-setup <key> [value]");
        return;
    };
    const SUPPORTED: [&str; 6] = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    if !SUPPORTED.contains(&key.as_str()) {
        state.push_diagnostic(format!("Unknown proxy key `{key}`"));
        return;
    }
    if parts.len() > 2 {
        state.push_diagnostic("Quote proxy values that contain spaces");
        return;
    }
    let value = parts.get(1).cloned().map_or(Value::Null, Value::String);
    if call_runtime(
        runtime,
        "config/proxy/write",
        json!({"changes": {key: value}}),
        state,
    )
    .is_some()
    {
        push_local_notice(
            state,
            if value.is_null() {
                "Proxy environment value cleared"
            } else {
                "Proxy environment value saved"
            },
            EntryStatus::Completed,
        );
    }
}

pub(super) fn reset_config_value(
    path: &str,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let target = selected_config_target(runtime).unwrap_or_else(|| "user".to_owned());
    reset_config_value_at(path, &target, runtime, state);
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
        state.push_diagnostic("Usage: /config reset <path>");
        return;
    }
    let Some(surface) = field_surface(runtime, state) else {
        return;
    };
    let field = path.join(".");
    // Only the selected configuration file is published as a layer of its own,
    // so a value that is not in it has nothing to clear in the target the write
    // would go to.
    if !written_in_selected_file(&surface, &field) {
        match top_config_origin(&surface, &field) {
            Some(origin) if !matches!(origin, "defaults" | "selected_toml") => {
                push_local_notice(
                    state,
                    &format!("`{field}` is pinned by {origin}; nothing to clear."),
                    EntryStatus::Completed,
                );
            }
            _ => {
                push_local_notice(
                    state,
                    &format!("`{field}` has nothing to clear in the {target} layer."),
                    EntryStatus::Completed,
                );
            }
        }
        return;
    }
    if persist_config_setting(runtime, target, &path, Value::Null, true, state) {
        apply_render_preferences(runtime, state);
        push_local_notice(state, "Configuration value reset", EntryStatus::Completed);
    }
}

/// The published configuration surface, as `config/fields/read` answers it.
///
/// Every read a settings command makes goes through this: the field's effective
/// value, the layer it comes from and the files a write may land in are all on
/// this one answer, and none of them is on `ConfigReadResponse`.
pub(super) fn field_surface(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) -> Option<Value> {
    let result = call_runtime(runtime, "config/fields/read", json!({}), state)?;
    Some(Value::Object(result.into_iter().collect()))
}

/// The published fields as an effective document, keyed by field name.
///
/// The pickers render values rather than layers, so they read this shape.
pub(super) fn published_fields(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) -> Option<Value> {
    let surface = field_surface(runtime, state)?;
    Some(Value::Object(
        fields_of(&surface)
            .filter_map(|field| {
                let name = field.get("name")?.as_str()?.to_owned();
                Some((name, field.get("value").cloned().unwrap_or(Value::Null)))
            })
            .collect(),
    ))
}

fn fields_of(surface: &Value) -> impl Iterator<Item = &Value> {
    surface
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn field_of<'a>(surface: &'a Value, name: &str) -> Option<&'a Value> {
    fields_of(surface).find(|field| field.get("name").and_then(Value::as_str) == Some(name))
}

/// Whether the selected configuration file carries the field, which is what a
/// reset would remove.
fn written_in_selected_file(surface: &Value, name: &str) -> bool {
    field_of(surface, name)
        .and_then(|field| field.get("layerValues"))
        .and_then(Value::as_array)
        .is_some_and(|layers| {
            layers
                .iter()
                .any(|layer| layer.get("layer").and_then(Value::as_str) == Some("selected_toml"))
        })
}

pub(super) fn selected_config_target(runtime: &mut InteractiveRuntime) -> Option<String> {
    if let Some(target) = runtime.config_target {
        return Some(target.as_str().to_owned());
    }
    // The writable targets are published with the selected one first, which is
    // the file an unrouted write lands in.
    runtime
        .service
        .public_call(
            "config/fields/read",
            json!({"sessionId": runtime.session_id}),
        )
        .ok()?
        .get("targets")?
        .get(0)?
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
    persist_setting(runtime, target, path, value, remove, state)
}

pub(super) fn configured_value(runtime: &mut InteractiveRuntime, key: &str) -> Option<Value> {
    let surface = Value::Object(
        runtime
            .service
            .public_call(
                "config/fields/read",
                json!({"sessionId": runtime.session_id}),
            )
            .ok()?
            .into_iter()
            .collect(),
    );
    field_of(&surface, key)?.get("value").cloned()
}

pub(super) fn config_field_schema(
    runtime: &mut InteractiveRuntime,
    path: &str,
    state: &mut TuiState,
) -> Option<Value> {
    let schema = call_runtime(runtime, "config/schema", json!({}), state)?
        .get("schema")?
        .clone();
    let segments = path.split('.').collect::<Vec<_>>();
    config_schema_at(&schema, &segments).cloned()
}

pub(in crate::tui) fn apply_render_preferences(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    state.show_reasoning = configured_value(runtime, "show_thinking_nodes")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    state.autocopy_to_clipboard = configured_value(runtime, "autocopy_to_clipboard")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    state.ask_confirmation_on_exit = configured_value(runtime, "ask_confirmation_on_exit")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    state
        .notifier
        .set_policy(crate::tui::attention::NotificationPolicy::from_config(
            configured_value(runtime, "notifications")
                .as_ref()
                .and_then(Value::as_str),
        ));
    // Reference `_refresh_config_from_disk`: a preference change cancels live
    // narration before the new preference applies.
    let narrator_enabled = configured_value(runtime, "narrator_enabled")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if let Some(effect) = state.narrator.sync(narrator_enabled, narrator_enabled) {
        crate::tui::apply_narrator_effect(effect, runtime, state);
    }
}

fn config_schema_at<'a>(schema: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = schema;
    for segment in path {
        current = current.get("properties")?.get(*segment)?;
    }
    Some(current)
}

fn field_is_secret(path: &[&str], schema: &Value) -> bool {
    schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || schema.get("format").and_then(Value::as_str) == Some("password")
        || path.iter().any(|segment| {
            let normalized = segment.to_ascii_lowercase();
            normalized.contains("password")
                || normalized.contains("secret")
                || normalized.contains("api_key")
                || normalized.contains("token")
        })
}

/// The layer a field's effective value comes from.
///
/// The published layers are ordered highest priority first, so the first one is
/// what a client would have to change to move the value.
fn top_config_origin<'a>(surface: &'a Value, name: &str) -> Option<&'a str> {
    field_of(surface, name)?
        .get("layerValues")?
        .as_array()?
        .first()?
        .get("layer")?
        .as_str()
}

fn parse_typed_config_value(raw: &str, schema: &Value) -> Result<Value, String> {
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        let candidate = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
        return choices
            .contains(&candidate)
            .then_some(candidate)
            .ok_or_else(|| {
                format!(
                    "Configuration value must be one of {}",
                    choices
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
    }
    let types = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if raw == "null" && types.contains(&"null") {
        return Ok(Value::Null);
    }
    let parsed = match types.as_slice() {
        kinds if kinds.contains(&"boolean") => serde_json::from_str::<bool>(raw).map(Value::Bool),
        kinds if kinds.contains(&"integer") => serde_json::from_str::<i64>(raw).map(Value::from),
        kinds if kinds.contains(&"number") => serde_json::from_str::<f64>(raw).map(Value::from),
        kinds if kinds.contains(&"array") => {
            serde_json::from_str::<Vec<Value>>(raw).map(Value::Array)
        }
        kinds if kinds.contains(&"object") => {
            serde_json::from_str::<Map<String, Value>>(raw).map(Value::Object)
        }
        kinds if kinds.contains(&"string") => {
            return Ok(Value::String(
                serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_owned()),
            ));
        }
        _ => return Err("Configuration field has an unsupported type".to_owned()),
    };
    parsed.map_err(|error| format!("Invalid typed configuration value: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_config_values_reject_invalid_or_unknown_shapes() {
        assert_eq!(
            parse_typed_config_value("12", &json!({"type": "integer"})),
            Ok(json!(12))
        );
        assert_eq!(
            parse_typed_config_value("null", &json!({"type": ["string", "null"]})),
            Ok(Value::Null)
        );
        assert!(parse_typed_config_value("yes", &json!({"type": "boolean"})).is_err());
        assert!(parse_typed_config_value("[]", &json!({"type": "mystery"})).is_err());
    }

    #[test]
    fn secret_detection_covers_schema_and_conventional_names() {
        assert!(field_is_secret(&["api_key"], &json!({"type": "string"})));
        assert!(field_is_secret(
            &["credential"],
            &json!({"type": "string", "writeOnly": true})
        ));
        assert!(!field_is_secret(
            &["active_model"],
            &json!({"type": "string"})
        ));
    }

    /// US-091: the published surface orders a field's layers highest first, so
    /// the origin is read off the front rather than searched for.
    #[test]
    fn reset_origin_prefers_the_highest_layer() {
        let surface = json!({
            "fields": [{
                "name": "theme",
                "layerValues": [
                    {"layer": "environment", "value": "pinned"},
                    {"layer": "selected_toml", "value": "light"},
                    {"layer": "defaults", "value": "dark"}
                ]
            }],
            "targets": ["user"]
        });
        assert_eq!(top_config_origin(&surface, "theme"), Some("environment"));
        assert_eq!(top_config_origin(&surface, "missing"), None);
        assert!(written_in_selected_file(&surface, "theme"));
        assert!(!written_in_selected_file(&surface, "missing"));
    }
}
