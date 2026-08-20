use std::collections::BTreeSet;

use serde_json::Value;

mod integrations;
mod remote_projects;

pub use integrations::{mcp_auth_overlay, mcp_detail_overlay, mcp_overlay};
pub use remote_projects::{
    remote_project_create_overlay, remote_projects_overlay, teleport_push_overlay,
};

use super::interaction::{ConfigLayerTarget, Overlay, OverlayAction, OverlayItem, OverlayKind};
use super::rewind::RewindTarget;

/// The configuration rows the panel edits through a picker of their own rather
/// than through the generic value editor, plus the synthetic row that chooses
/// which layer a write lands in. The panel builds them and the selection
/// handler routes them, so both name them here.
pub const CONFIG_TARGET_ROW: &str = "config-target";
pub const ACTIVE_MODEL_FIELD: &str = "active_model";
pub const THINKING_FIELD: &str = "thinking";
pub const THEME_FIELD: &str = "theme";

const POPULAR_CONFIG_FIELDS: &[&str] = &[
    ACTIVE_MODEL_FIELD,
    THINKING_FIELD,
    THEME_FIELD,
    "notifications",
    "voice_mode_enabled",
    "narrator_enabled",
];

#[must_use]
pub fn config_overlay(snapshot: &Value, schema: &Value) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    let mut fields = Vec::new();
    collect_schema_fields("", schema, config, &mut fields);
    let known = fields
        .iter()
        .map(|(path, _, _, _)| path.clone())
        .collect::<BTreeSet<_>>();
    let mut fallback = Vec::new();
    flatten_config("", config, &mut fallback);
    fields.extend(
        fallback
            .into_iter()
            .filter(|(path, _)| !known.contains(path))
            .map(|(path, value)| (path, value, Value::Null, true)),
    );
    fields.sort_by(|left, right| {
        config_priority(&left.0)
            .cmp(&config_priority(&right.0))
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut popular = Vec::new();
    let mut advanced = Vec::new();
    for item in fields
        .into_iter()
        .map(|(path, value, field_schema, configured)| {
            let schema_available = !field_schema.is_null();
            let kind = schema_kind(&field_schema)
                .map_or_else(|| value_kind(&value).to_owned(), str::to_owned);
            let description = field_schema
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(if schema_available {
                    ""
                } else {
                    "schema unavailable; read-only"
                });
            let details = format!(
                "{} · {kind} · {}",
                compact_value(&value),
                origin_label(&config_origin(snapshot, &path, configured)),
            );
            let details = if description.is_empty() {
                details
            } else {
                format!("{details} · {description}")
            };
            OverlayItem::new(
                path.clone(),
                path.replace('.', " › ").replace('_', " "),
                details,
                !schema_available || config_field_is_secret(&path, &field_schema),
            )
        })
    {
        if POPULAR_CONFIG_FIELDS.contains(&item.id.as_str()) {
            popular.push(item);
        } else {
            advanced.push(item);
        }
    }
    let target = ConfigLayerTarget::from_selected_target(
        snapshot.get("selectedTarget").and_then(Value::as_str),
    );
    let mut items = Vec::with_capacity(popular.len() + advanced.len() + 3);
    items.push(OverlayItem::new(
        CONFIG_TARGET_ROW,
        "Save changes to",
        format!("{} configuration layer", target.as_str()),
        false,
    ));
    if !popular.is_empty() {
        items.push(OverlayItem::new(
            "heading:popular",
            "Popular settings",
            "",
            true,
        ));
        items.extend(popular);
    }
    if !advanced.is_empty() {
        items.push(OverlayItem::new(
            "heading:advanced",
            "Advanced settings",
            "",
            true,
        ));
        items.extend(advanced);
    }
    Overlay::new(OverlayKind::Config, "Settings", items)
}

#[must_use]
pub fn config_target_overlay(current: ConfigLayerTarget) -> Overlay {
    Overlay::new(
        OverlayKind::ConfigTarget,
        "Select configuration layer",
        [ConfigLayerTarget::User, ConfigLayerTarget::Project]
            .into_iter()
            .map(|target| {
                OverlayItem::new(
                    format!("config-target:{}", target.as_str()),
                    target.as_str(),
                    if target == current { "current" } else { "" },
                    false,
                )
                .with_action(OverlayAction::ConfigTarget(target))
            })
            .collect(),
    )
}

#[must_use]
pub fn config_choice_overlay(path: &str, schema: &Value, current: Option<&Value>) -> Overlay {
    let choices = schema
        .get("enum")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Overlay::new(
        OverlayKind::ConfigChoice,
        format!("Set {}", path.replace('_', " ")),
        choices
            .into_iter()
            .enumerate()
            .map(|(index, choice)| {
                let label = compact_value(&choice);
                OverlayItem::new(
                    format!("config-choice:{path}:{index}"),
                    label,
                    if current == Some(&choice) {
                        "current"
                    } else {
                        ""
                    },
                    false,
                )
                .with_action(OverlayAction::ConfigChoice {
                    path: path.to_owned(),
                    value: choice,
                })
            })
            .collect(),
    )
}

fn collect_schema_fields(
    prefix: &str,
    schema: &Value,
    config: &Value,
    output: &mut Vec<(String, Value, Value, bool)>,
) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, field_schema) in properties {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let value = config.get(name);
        let nested_properties = field_schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some();
        if nested_properties {
            collect_schema_fields(&path, field_schema, value.unwrap_or(&Value::Null), output);
        } else {
            output.push((
                path,
                value
                    .cloned()
                    .or_else(|| field_schema.get("default").cloned())
                    .unwrap_or(Value::Null),
                field_schema.clone(),
                value.is_some(),
            ));
        }
    }
}

/// Mirrors the reference `screens/config/_common.origin_label` vocabulary.
fn origin_label(origin: &str) -> String {
    match origin {
        "overrides" => "temporary".to_owned(),
        "environment" => "env".to_owned(),
        other => other
            .strip_suffix("-toml")
            .map_or_else(|| other.to_owned(), |layer| format!("{layer} config")),
    }
}

fn config_origin(snapshot: &Value, path: &str, configured: bool) -> String {
    let pointer = format!(
        "/{}",
        path.split('.')
            .map(|segment| segment.replace('~', "~0").replace('/', "~1"))
            .collect::<Vec<_>>()
            .join("/")
    );
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
                // The reference names TOML layers `<target>-toml`; the wire keeps
                // the selected target beside the layer.
                format!(
                    "{}-toml",
                    snapshot
                        .get("selectedTarget")
                        .and_then(Value::as_str)
                        .unwrap_or("selected")
                )
            } else {
                layer.to_owned()
            }
        })
        .unwrap_or_else(|| if configured { "effective" } else { "defaults" }.to_owned())
}

/// The rewindable points of one page of saved history.
///
/// A rewind targets a user message, addressed by the identity the server
/// resolves rather than by a position in a list the next compaction renumbers.
/// The entries come from `history/list`, which pages the stored transcript, so
/// `offset` is where this page starts in it. Whether each point would change
/// files is filled in by the caller, which is the only one that can ask.
#[must_use]
pub fn rewind_targets(history: &Value, offset: usize) -> Vec<RewindTarget> {
    history
        .get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(position, message)| {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let content = message.get("content").and_then(Value::as_str)?.to_owned();
            Some(RewindTarget {
                entry_id: format!("history:{}:user", offset.saturating_add(position)),
                message: content,
                has_file_changes: false,
            })
        })
        .collect()
}

#[must_use]
pub fn model_overlay(snapshot: &Value, current: &str) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    // The effective configuration keys models by alias, as the reference does
    // once a persisted `[[models]]` list is read back.
    let mut models = config
        .get("models")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(alias, _)| alias.clone())
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

/// Reference `ThemePickerApp`: the full catalog, automatic entry first.
#[must_use]
pub fn theme_overlay(current: &str) -> Overlay {
    fixed_choice_overlay(
        OverlayKind::Theme,
        "Select Theme",
        &crate::tui::themes::sorted_theme_names(),
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

/// The two audio families the voice settings publish a choice list for, as
/// `(field, the `ConfigView` alias list that feeds it, label)`.
///
/// The reference promotes exactly these two fields from plain strings to
/// enums when its settings screen renders them, and builds each list from the
/// aliases the projection publishes
/// (`vibe/cli/textual_ui/screens/config/config_screen.py:110-111` reading
/// `vibe/app_server/config.py:73-74`).
pub(super) const VOICE_MODEL_FIELDS: [(&str, &str, &str); 2] = [
    (
        "active_transcribe_model",
        "transcribeModels",
        "Transcription model",
    ),
    ("active_tts_model", "ttsModels", "Speech model"),
];

/// `snapshot` is the published field document and `view` the `ConfigView` the
/// two alias lists live on, because the field surface publishes the audio
/// families as the entry lists they are written as and only the view projects
/// them down to aliases.
#[must_use]
pub fn voice_overlay(snapshot: &Value, view: &Value) -> Overlay {
    let config = snapshot.get("config").unwrap_or(snapshot);
    let mut items = [
        ("voice_mode_enabled", "Voice mode"),
        ("narrator_enabled", "Narrator (experimental)"),
    ]
    .into_iter()
    .map(|(key, label)| {
        let enabled = config.get(key).and_then(Value::as_bool).unwrap_or(false);
        OverlayItem::new(key, label, if enabled { "On" } else { "Off" }, false)
    })
    .collect::<Vec<_>>();
    items.extend(VOICE_MODEL_FIELDS.map(|(field, list, label)| {
        let current = config
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default();
        // A family declaring one entry still shows it: the reference offers a
        // one-option list rather than hiding the field.
        let declared = audio_model_aliases(view, list, current).len();
        OverlayItem::new(
            field,
            label,
            format!(
                "{} · {declared} declared",
                if current.is_empty() { "—" } else { current }
            ),
            false,
        )
    }));
    Overlay::new(OverlayKind::Voice, "Voice settings", items)
}

/// The aliases one audio family publishes, which is what its choice list is
/// built from.
///
/// The configured value is kept even when the list does not carry it, so a
/// session running on an alias the active configuration no longer declares
/// still shows what it runs on, exactly as the model picker does.
#[must_use]
pub fn audio_model_aliases(view: &Value, list: &str, current: &str) -> Vec<String> {
    let mut aliases = view
        .get(list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if !current.is_empty() && !aliases.iter().any(|alias| alias == current) {
        aliases.push(current.to_owned());
    }
    aliases.sort();
    aliases.dedup();
    aliases
}

/// The choice list one audio family offers, written back through the same
/// configuration path every other choice overlay writes through.
#[must_use]
pub fn voice_model_overlay(field: &str, label: &str, aliases: &[String], current: &str) -> Overlay {
    Overlay::new(
        OverlayKind::VoiceModel,
        format!("Select {}", label.to_lowercase()),
        aliases
            .iter()
            .map(|alias| {
                OverlayItem::new(
                    format!("{field}:{alias}"),
                    alias.clone(),
                    if alias == current { "current" } else { "" },
                    false,
                )
                .with_action(OverlayAction::ConfigChoice {
                    path: field.to_owned(),
                    value: Value::String(alias.clone()),
                })
            })
            .collect(),
    )
}

#[must_use]
pub fn proxy_overlay(settings: &Value) -> Overlay {
    let values = settings.get("values").unwrap_or(settings);
    let descriptions = settings.get("descriptions").and_then(Value::as_object);
    Overlay::new(
        OverlayKind::Proxy,
        "Proxy Configuration",
        [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ]
        .into_iter()
        .map(|key| {
            let description = descriptions
                .and_then(|descriptions| descriptions.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let value = values.get(key).filter(|value| !value.is_null());
            OverlayItem::new(
                key,
                key,
                value.map_or_else(
                    || format!("not set · {description}"),
                    |value| format!("{} · {description}", compact_value(value)),
                ),
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
    POPULAR_CONFIG_FIELDS
        .iter()
        .position(|popular| *popular == path)
        .unwrap_or(POPULAR_CONFIG_FIELDS.len())
}

fn schema_kind(schema: &Value) -> Option<&str> {
    if schema.get("enum").is_some() || schema.get("const").is_some() {
        return Some("choice");
    }
    schema
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            schema
                .get("type")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|kind| *kind != "null")
        })
        .or_else(|| schema.get("format").and_then(Value::as_str))
}

fn config_field_is_secret(path: &str, schema: &Value) -> bool {
    schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || schema.get("format").and_then(Value::as_str) == Some("password")
        || path.split('.').any(|segment| {
            let normalized = segment.to_ascii_lowercase();
            normalized.contains("password")
                || normalized.contains("secret")
                || normalized.contains("api_key")
                || normalized.contains("token")
        })
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

/// Mirrors the reference `screens/config/_common.format_value` vocabulary so a
/// value reads the same in both runtimes.
fn compact_value(value: &Value) -> String {
    let rendered = match value {
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "—".to_owned(),
        Value::String(value) if value.is_empty() => "\"\"".to_owned(),
        Value::String(value) => value.clone(),
        Value::Array(items) => format!(
            "[{} item{}]",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ),
        Value::Object(entries) => format!(
            "{{{} entr{}}}",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        ),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".to_owned()),
    };
    if rendered.chars().count() <= 80 {
        rendered
    } else {
        format!("{}…", rendered.chars().take(79).collect::<String>())
    }
}

pub(super) fn to_camel_case(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::model_overlay;

    /// The effective configuration keys models by alias, so the picker lists
    /// keys rather than list entries.
    #[test]
    fn the_model_picker_lists_the_aliases_the_configuration_is_keyed_by() {
        let snapshot = json!({
            "config": {
                "models": {
                    "mistral-medium-3.5": {"name": "mistral-vibe-cli-latest", "provider": "mistral"},
                    "local": {"name": "devstral", "provider": "llamacpp"}
                }
            }
        });
        let overlay = model_overlay(&snapshot, "local");
        let listed = overlay
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(listed, ["local", "mistral-medium-3.5"]);
        let selected = overlay
            .items
            .iter()
            .find(|item| item.id == "local")
            .expect("the current model is listed");
        assert_eq!(selected.description, "current");
    }

    /// A model the configuration does not declare still has to be selectable,
    /// so a session started with `--model` keeps showing what it runs on.
    #[test]
    fn the_current_model_is_listed_even_when_it_is_not_configured() {
        let snapshot = json!({"config": {"models": {"local": {"name": "devstral"}}}});
        let overlay = model_overlay(&snapshot, "scratch");
        let listed = overlay
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(listed, ["local", "scratch"]);
    }
}
