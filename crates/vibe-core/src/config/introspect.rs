//! Describing the configuration surface for a settings screen.
//!
//! A client renders the settings list from this description alone: each field
//! carries the editor kind, the prose, the effective value, the pointer a write
//! addresses it by, the per-layer values behind it and whether the reference
//! surfaces it first. Reference `vibe/app_server/_config_introspect.py`.

use serde::Serialize;
use serde_json::Value as JsonValue;

use super::patch::JsonPointer;
use super::registry::{self, FieldDefault};
use super::{ConfigLayerKind, ConfigSnapshot, ConfigTarget, is_sensitive_key, redact_value};

/// Reference `HIDDEN_SETTINGS`: the fields a runtime fills rather than an
/// operator, which no settings screen offers an editor for. Per-tool settings
/// are excluded because neither side renders a per-tool editor; the other three
/// are written by the experiments layer and would be meaningless as manual
/// entries.
pub const HIDDEN_FIELDS: [&str; 4] = [
    "managed_shell_tools_enabled",
    "routed_default_model",
    "routed_model_config",
    "tools",
];

/// One field's value in one layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayerValue {
    pub layer: ConfigLayerKind,
    pub value: JsonValue,
}

/// Everything a settings screen needs to render one field.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFieldView {
    pub name: &'static str,
    pub kind: &'static str,
    pub description: &'static str,
    pub value: JsonValue,
    /// The JSON Pointer `config/patch` addresses this field by.
    pub path: String,
    pub popular: bool,
    pub enum_choices: Vec<&'static str>,
    /// Highest priority first, ending with the defaults layer.
    pub layer_values: Vec<ConfigLayerValue>,
}

/// The settings surface: every published field, and the targets a write can go
/// to.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFields {
    pub fields: Vec<ConfigFieldView>,
    pub targets: Vec<ConfigTarget>,
}

/// Describes every published field of `snapshot`.
#[must_use]
pub fn describe_fields(snapshot: &ConfigSnapshot) -> Vec<ConfigFieldView> {
    registry::FIELDS
        .iter()
        .filter(|spec| spec.published && !HIDDEN_FIELDS.contains(&spec.name))
        .map(|spec| ConfigFieldView {
            name: spec.name,
            kind: spec.kind.as_str(),
            description: spec.description,
            value: snapshot.public_value(spec.name),
            path: JsonPointer::from_segments([spec.name]).as_str().to_owned(),
            popular: spec.popular,
            enum_choices: spec.choices.to_vec(),
            layer_values: layer_values(snapshot, spec.name, spec.default),
        })
        .collect()
}

/// The value `name` holds in each layer that sets it, highest priority first.
///
/// The defaults layer closes the list even when it does not carry the field: a
/// client showing where a value comes from needs the registry default to fall
/// back on, which is what the reference appends.
fn layer_values(
    snapshot: &ConfigSnapshot,
    name: &str,
    default: FieldDefault,
) -> Vec<ConfigLayerValue> {
    let mut values: Vec<ConfigLayerValue> = snapshot
        .layer_values
        .iter()
        .rev()
        .filter_map(|layer| {
            layer.values.get(name).map(|value| ConfigLayerValue {
                layer: layer.kind,
                value: redacted(name, value),
            })
        })
        .collect();
    if !values
        .iter()
        .any(|entry| entry.layer == ConfigLayerKind::Defaults)
    {
        values.push(ConfigLayerValue {
            layer: ConfigLayerKind::Defaults,
            value: default
                .to_toml()
                .map_or(JsonValue::Null, |value| redacted(name, &value)),
        });
    }
    values
}

fn redacted(name: &str, value: &toml::Value) -> JsonValue {
    if is_sensitive_key(name) {
        JsonValue::String("[redacted]".to_owned())
    } else {
        redact_value(value)
    }
}
