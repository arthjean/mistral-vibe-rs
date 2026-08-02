use serde_json::Value;

use super::commands::{COMMANDS, CommandContext, command_available_in};
use super::interaction::{Overlay, OverlayItem, OverlayKind};
use super::rewind::{RewindState, RewindTarget};

#[must_use]
pub fn help_overlay(context: &CommandContext) -> Overlay {
    let mut items = vec![
        OverlayItem::new("enter", "Enter", "Submit message", true),
        OverlayItem::new("newline", "Ctrl+J / Shift+Enter", "Insert newline", true),
        OverlayItem::new("escape", "Escape", "Interrupt or close dialog", true),
        OverlayItem::new("quit", "Ctrl+C / Ctrl+D", "Confirm quit", true),
        OverlayItem::new("tools", "Ctrl+O", "Collapse or expand tool output", true),
        OverlayItem::new("agent", "Shift+Tab", "Cycle agent profile", true),
        OverlayItem::new("editor", "Ctrl+G", "Open external editor", true),
    ];
    items.extend(
        COMMANDS
            .iter()
            .filter(|command| command_available_in(command, context))
            .filter_map(|command| {
                let alias = command
                    .aliases
                    .iter()
                    .copied()
                    .find(|alias| alias.starts_with('/'))?;
                Some(OverlayItem::new(
                    format!("command:{alias}"),
                    alias,
                    command.description,
                    true,
                ))
            }),
    );
    Overlay::new(OverlayKind::Help, "Help", items)
}

#[must_use]
pub fn config_overlay(snapshot: &Value, schema: &Value) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut fields = Vec::new();
    flatten_config("", config, &mut fields);
    let configured = fields
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(properties) = properties {
        for (path, field_schema) in properties {
            if !fields.iter().any(|(field, _)| field == path) {
                fields.push((
                    path.clone(),
                    field_schema.get("default").cloned().unwrap_or(Value::Null),
                ));
            }
        }
    }
    fields.sort_by(|left, right| {
        config_priority(&left.0)
            .cmp(&config_priority(&right.0))
            .then_with(|| left.0.cmp(&right.0))
    });
    let items = fields
        .into_iter()
        .map(|(path, value)| {
            let schema = properties.and_then(|properties| properties.get(&path));
            let kind = schema
                .and_then(schema_kind)
                .map_or_else(|| value_kind(&value).to_owned(), str::to_owned);
            OverlayItem::new(
                path.clone(),
                path.replace('_', " "),
                format!(
                    "{} · {kind} · {}",
                    compact_value(&value),
                    config_origin(snapshot, &path, configured.contains(&path))
                ),
                false,
            )
        })
        .collect();
    Overlay::new(OverlayKind::Config, "Settings", items)
}

fn config_origin<'a>(snapshot: &'a Value, path: &str, configured: bool) -> &'a str {
    let pointer = format!("/{}", path.replace('.', "/"));
    snapshot
        .get("layerValues")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|layer| {
            layer
                .get("values")
                .is_some_and(|values| values.pointer(&pointer).is_some())
        })
        .and_then(|layer| layer.get("layer"))
        .and_then(Value::as_str)
        .map(|layer| {
            if layer == "selected_toml" {
                snapshot
                    .get("selectedTarget")
                    .and_then(Value::as_str)
                    .unwrap_or(layer)
            } else {
                layer
            }
        })
        .unwrap_or(if configured { "effective" } else { "default" })
}

#[must_use]
pub fn rewind_state(result: &Value) -> Option<RewindState> {
    let targets = result
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| {
            let message_index = message
                .get("messageIndex")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())?;
            let has_file_changes = message
                .get("hasFileChanges")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = message.get("message").and_then(Value::as_str)?.to_owned();
            Some(RewindTarget {
                message_index,
                message,
                has_file_changes,
            })
        })
        .collect();
    RewindState::new(targets)
}

#[must_use]
pub fn model_overlay(snapshot: &Value, current: &str) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    let mut models = config
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("alias")
                .or_else(|| model.get("name"))
                .and_then(Value::as_str)
                .or_else(|| model.as_str())
        })
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if !current.is_empty() && !models.iter().any(|model| model == current) {
        models.push(current.to_owned());
    }
    models.sort();
    models.dedup();
    Overlay::new(
        OverlayKind::Model,
        "Select model",
        models
            .into_iter()
            .map(|model| {
                let selected = model == current;
                OverlayItem::new(
                    model.clone(),
                    model,
                    if selected { "current" } else { "" },
                    false,
                )
            })
            .collect(),
    )
}

#[must_use]
pub fn thinking_overlay(current: &str) -> Overlay {
    fixed_choice_overlay(
        OverlayKind::Thinking,
        "Thinking level",
        &["off", "low", "medium", "high", "max"],
        current,
    )
}

#[must_use]
pub fn theme_overlay(current: &str) -> Overlay {
    fixed_choice_overlay(
        OverlayKind::Theme,
        "Select theme",
        &["system", "light", "dark"],
        current,
    )
}

#[must_use]
pub fn sessions_overlay(result: &Value, current_session_id: &str) -> Overlay {
    let items = result
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|session| {
            let id = session
                .get("id")
                .or_else(|| session.get("sessionId"))
                .and_then(Value::as_str)?;
            let title = session
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .unwrap_or(id);
            let messages = session
                .get("messageCount")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let ended = session
                .get("endTime")
                .and_then(Value::as_str)
                .unwrap_or("active");
            let current = id == current_session_id;
            Some(OverlayItem::new(
                id,
                title,
                format!(
                    "{messages} messages · {ended}{}",
                    if current { " · current" } else { "" }
                ),
                false,
            ))
        })
        .collect();
    Overlay::new(OverlayKind::Sessions, "Saved sessions", items)
}

#[must_use]
pub fn mcp_overlay(result: &Value) -> Overlay {
    let items = result
        .pointer("/mcp/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source| {
            let name = source.get("name").and_then(Value::as_str)?;
            let status = source
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let enabled = source
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(status != "disabled");
            let tools = source
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Some(OverlayItem::new(
                name,
                name,
                format!(
                    "{status} · {tools} tool{}{}",
                    if tools == 1 { "" } else { "s" },
                    if enabled { "" } else { " · disabled" }
                ),
                false,
            ))
        })
        .collect();
    Overlay::new(OverlayKind::Mcp, "MCP servers and connectors", items)
}

#[must_use]
pub fn voice_overlay(snapshot: &Value) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    Overlay::new(
        OverlayKind::Voice,
        "Voice settings",
        [
            ("voice_mode_enabled", "Voice mode"),
            ("narrator_enabled", "Narrator (experimental)"),
        ]
        .into_iter()
        .map(|(key, label)| {
            let enabled = config.get(key).and_then(Value::as_bool).unwrap_or(false);
            OverlayItem::new(key, label, if enabled { "On" } else { "Off" }, false)
        })
        .collect(),
    )
}

#[must_use]
pub fn proxy_overlay(snapshot: &Value) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    Overlay::new(
        OverlayKind::Proxy,
        "Proxy and TLS",
        [("proxy", "Proxy URL"), ("tls_ca_path", "TLS certificate")]
            .into_iter()
            .map(|(key, label)| {
                OverlayItem::new(
                    key,
                    label,
                    config
                        .get(key)
                        .map_or_else(|| "not set".to_owned(), compact_value),
                    false,
                )
            })
            .collect(),
    )
}

#[must_use]
pub fn status_overlay(result: &Value) -> Overlay {
    let stats = result.get("stats").unwrap_or(result);
    let fields = [
        ("steps", "Steps"),
        ("session_prompt_tokens", "Session prompt tokens"),
        ("session_completion_tokens", "Session completion tokens"),
        ("session_total_llm_tokens", "Session total LLM tokens"),
        ("last_turn_total_tokens", "Last turn tokens"),
        ("session_cost", "Cost"),
    ];
    Overlay::new(
        OverlayKind::Status,
        "Agent statistics",
        fields
            .into_iter()
            .map(|(key, label)| {
                let value = stats
                    .get(key)
                    .or_else(|| stats.get(to_camel_case(key).as_str()))
                    .map_or_else(|| "0".to_owned(), compact_value);
                OverlayItem::new(key, label, value, true)
            })
            .collect(),
    )
}

#[must_use]
pub fn debug_overlay(result: &Value) -> Overlay {
    let mut items = result
        .pointer("/logs/entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .take(100)
        .enumerate()
        .map(|(index, log)| {
            let level = log.get("level").and_then(Value::as_str).unwrap_or("log");
            let message = log
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            OverlayItem::new(format!("log:{index}"), level, message, true)
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        items.push(OverlayItem::new(
            "empty",
            "No debug events",
            "The runtime log buffer is empty",
            true,
        ));
    }
    Overlay::new(OverlayKind::Debug, "Debug console", items)
}

fn fixed_choice_overlay(
    kind: OverlayKind,
    title: &str,
    choices: &[&str],
    current: &str,
) -> Overlay {
    Overlay::new(
        kind,
        title,
        choices
            .iter()
            .map(|choice| {
                OverlayItem::new(
                    *choice,
                    *choice,
                    if *choice == current { "current" } else { "" },
                    false,
                )
            })
            .collect(),
    )
}

fn flatten_config(prefix: &str, value: &Value, output: &mut Vec<(String, Value)>) {
    let Some(object) = value.as_object() else {
        if !prefix.is_empty() {
            output.push((prefix.to_owned(), value.clone()));
        }
        return;
    };
    for (key, value) in object {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if value.is_object() {
            flatten_config(&path, value, output);
        } else {
            output.push((path, value.clone()));
        }
    }
}

fn config_priority(path: &str) -> usize {
    const POPULAR: &[&str] = &[
        "active_model",
        "thinking",
        "theme",
        "notifications",
        "voice_mode_enabled",
        "narrator_enabled",
    ];
    POPULAR
        .iter()
        .position(|popular| *popular == path)
        .unwrap_or(POPULAR.len())
}

fn schema_kind(schema: &Value) -> Option<&str> {
    if schema.get("enum").is_some() {
        return Some("choice");
    }
    schema
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| schema.get("format").and_then(Value::as_str))
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "unset",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

fn compact_value(value: &Value) -> String {
    let rendered = match value {
        Value::String(value) => value.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".to_owned()),
    };
    if rendered.chars().count() <= 80 {
        rendered
    } else {
        format!("{}…", rendered.chars().take(79).collect::<String>())
    }
}

fn to_camel_case(value: &str) -> String {
    let mut output = String::new();
    let mut uppercase = false;
    for character in value.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            output.push(character.to_ascii_uppercase());
            uppercase = false;
        } else {
            output.push(character);
        }
    }
    output
}
