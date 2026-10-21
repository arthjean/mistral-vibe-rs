//! Describing the configuration surface for a settings screen.
//!
//! A client renders the settings list from this description alone: each field
//! carries the editor kind, the prose, the effective value, the pointer a write
//! addresses it by, the per-layer values behind it and whether the reference
//! surfaces it first. Reference `vibe/app_server/_config_introspect.py`.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value as JsonValue;

use super::merge;
use super::models::model_entry;
use super::patch::JsonPointer;
use super::registry::{self, FieldDefault};
use super::{ConfigError, ConfigLayerKind, ConfigSnapshot, ConfigTarget, LayeredConfig, source_of};
use crate::redaction::{REDACTED, is_sensitive_key, redact_value};

/// Reference `HIDDEN_SETTINGS`: the fields a runtime fills rather than an
/// operator, which no settings screen offers an editor for. Per-tool settings
/// are excluded because neither side renders a per-tool editor; the other six
/// are written by the experiments layer and would be meaningless as manual
/// entries.
pub const HIDDEN_FIELDS: [&str; 7] = [
    "managed_shell_tools_enabled",
    "routed_default_model",
    "routed_model_config",
    "smart_approve_available",
    "smart_approve_default",
    "routed_extra_models",
    "tools",
];

/// The compaction threshold, whose settings row is the active model's.
const AUTO_COMPACT_THRESHOLD: &str = "auto_compact_threshold";
/// The context window, which lives only on models and gets a row of its own.
const MAX_CONTEXT_LENGTH: &str = "max_context_length";

/// One field's value in one layer, the layer named as the reference names it
/// ([`ConfigLayerKind::reference_name`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayerValue {
    pub layer: &'static str,
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
    /// Display labels for enum values. Reference `ConfigFieldWire.value_labels`
    /// (`vibe/app_server/protocol.py:917`), which no field fills at the pin.
    pub value_labels: BTreeMap<String, String>,
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

/// Describes every published field of `snapshot`, then the active model's
/// context window, which lives only on models and so has no field of its own
/// to describe it. Reference `build_field_wires`.
#[must_use]
pub fn describe_fields(snapshot: &ConfigSnapshot) -> Vec<ConfigFieldView> {
    let active = ActiveModel::of(snapshot);
    let mut fields: Vec<ConfigFieldView> = registry::FIELDS
        .iter()
        .filter(|spec| spec.published && !HIDDEN_FIELDS.contains(&spec.name))
        .map(|spec| {
            let mut field = ConfigFieldView {
                name: spec.name,
                kind: spec.kind.as_str(),
                description: spec.description,
                value: snapshot.public_value(spec.name),
                path: JsonPointer::from_segments([spec.name]).as_str().to_owned(),
                popular: spec.popular,
                enum_choices: spec.choices.to_vec(),
                value_labels: BTreeMap::new(),
                layer_values: layer_values(snapshot, spec.name),
            };
            if spec.name == AUTO_COMPACT_THRESHOLD {
                scope_threshold(active.as_ref(), snapshot, &mut field);
            } else if !field
                .layer_values
                .iter()
                .any(|entry| entry.layer == ConfigLayerKind::Defaults.reference_name())
            {
                // The defaults layer closes the list even when it does not
                // carry the field: a client showing where a value comes from
                // needs the registry default to fall back on.
                field.layer_values.push(ConfigLayerValue {
                    layer: ConfigLayerKind::Defaults.reference_name(),
                    value: default_value(spec.name, spec.default),
                });
            }
            field
        })
        .collect();
    fields.push(window_row(active.as_ref(), snapshot));
    fields
}

impl LayeredConfig {
    /// The settings surface: every published field with its per-layer values,
    /// and the targets a write can be routed to.
    pub fn describe_fields(&self) -> Result<ConfigFields, ConfigError> {
        let snapshot = self.load()?;
        let targets = self.writable_targets(&snapshot);
        Ok(ConfigFields {
            fields: describe_fields(&snapshot),
            targets,
        })
    }

    /// The configuration files a write can land in, the one an unrouted
    /// operation goes to first.
    ///
    /// The project file is only writable while the workspace is trusted, which
    /// is the same rule [`Self::batch_write`] enforces.
    fn writable_targets(&self, snapshot: &ConfigSnapshot) -> Vec<ConfigTarget> {
        let mut targets = vec![snapshot.selected_target];
        for target in [ConfigTarget::User, ConfigTarget::Project] {
            let enabled = source_of(target).is_some_and(|source| self.sources.contains(&source));
            let writable = enabled && (target != ConfigTarget::Project || self.project_writable());
            if writable && !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }
}

/// The model the settings rows scoped to a model describe: reference
/// `get_active_model`.
struct ActiveModel {
    alias: String,
    entry: toml::Table,
}

impl ActiveModel {
    fn of(snapshot: &ConfigSnapshot) -> Option<Self> {
        Some(Self {
            alias: snapshot.active_model_alias()?.to_owned(),
            entry: snapshot.active_model()?,
        })
    }

    fn value(&self, field: &str) -> JsonValue {
        self.entry.get(field).map_or(JsonValue::Null, redact_value)
    }

    fn path(&self, field: &str) -> String {
        JsonPointer::from_segments([merge::MODELS_FIELD, self.alias.as_str(), field])
            .as_str()
            .to_owned()
    }
}

/// The threshold row describes what the active model compacts at, the
/// threshold the load resolved for it. An organization enforcing the
/// global threshold keeps the row on the global field, so its provenance
/// stays visible; a layer setting the model's own threshold moves the row
/// onto that threshold, which is what an edit should change.
fn scope_threshold(
    active: Option<&ActiveModel>,
    snapshot: &ConfigSnapshot,
    field: &mut ConfigFieldView,
) {
    let Some(active) = active else {
        return;
    };
    field.value = active.value(AUTO_COMPACT_THRESHOLD);
    let enforced = field
        .layer_values
        .first()
        .filter(|entry| entry.layer == ConfigLayerKind::Admin.reference_name())
        .map(|entry| entry.value.clone());
    if let Some(enforced) = enforced {
        field.value = enforced;
        return;
    }
    let own = model_layer_values(snapshot, &active.alias, AUTO_COMPACT_THRESHOLD);
    if !own.is_empty() {
        field.path = active.path(AUTO_COMPACT_THRESHOLD);
        field.layer_values = own;
    } else if field.layer_values.is_empty() {
        field.layer_values.push(ConfigLayerValue {
            layer: ConfigLayerKind::Defaults.reference_name(),
            value: field.value.clone(),
        });
    }
}

/// The active model's context window, as a row of its own.
fn window_row(active: Option<&ActiveModel>, snapshot: &ConfigSnapshot) -> ConfigFieldView {
    let (value, path, mut layer_values) = match active {
        Some(active) => (
            active.value(MAX_CONTEXT_LENGTH),
            active.path(MAX_CONTEXT_LENGTH),
            model_layer_values(snapshot, &active.alias, MAX_CONTEXT_LENGTH),
        ),
        None => (JsonValue::Null, String::new(), Vec::new()),
    };
    if layer_values.is_empty() {
        layer_values.push(ConfigLayerValue {
            layer: ConfigLayerKind::Defaults.reference_name(),
            value: value.clone(),
        });
    }
    ConfigFieldView {
        name: MAX_CONTEXT_LENGTH,
        kind: registry::FieldKind::Int.as_str(),
        description: "Context window of the active model, in tokens. A model that sets no \
                          compaction threshold compacts at 80% of it.",
        value,
        path,
        popular: false,
        enum_choices: Vec::new(),
        value_labels: BTreeMap::new(),
        layer_values,
    }
}

/// The value `name` holds in each layer that sets it, highest priority first.
/// The unset compaction threshold the defaults compose means "no layer set
/// one", so it is no value to show.
fn layer_values(snapshot: &ConfigSnapshot, name: &str) -> Vec<ConfigLayerValue> {
    snapshot
        .layer_values
        .iter()
        .rev()
        .filter_map(|layer| {
            let value = layer.values.get(name)?;
            if name == AUTO_COMPACT_THRESHOLD && is_unset_threshold(value) {
                return None;
            }
            Some(ConfigLayerValue {
                layer: layer.kind.reference_name(),
                value: redacted(name, value),
            })
        })
        .collect()
}

/// The value `field` holds on the model `alias` in each layer whose entry for
/// that model sets it, highest priority first, in either persisted shape of
/// `models`.
///
/// The reference's defaults layer materializes every model field, so a model
/// the shipped document declares carries an unknown window there, and its
/// unset threshold is no value to show.
fn model_layer_values(
    snapshot: &ConfigSnapshot,
    alias: &str,
    field: &str,
) -> Vec<ConfigLayerValue> {
    snapshot
        .layer_values
        .iter()
        .rev()
        .filter_map(|layer| {
            let entry = model_entry(layer.values.get(merge::MODELS_FIELD)?, alias)?;
            let value = match entry.get(field) {
                Some(value) if field == AUTO_COMPACT_THRESHOLD && is_unset_threshold(value) => {
                    return None;
                }
                Some(value) => redact_value(value),
                None if layer.kind == ConfigLayerKind::Defaults && field == MAX_CONTEXT_LENGTH => {
                    JsonValue::Null
                }
                None => return None,
            };
            Some(ConfigLayerValue {
                layer: layer.kind.reference_name(),
                value,
            })
        })
        .collect()
}

fn is_unset_threshold(value: &toml::Value) -> bool {
    value.as_integer() == Some(registry::UNSET_AUTO_COMPACT_THRESHOLD)
}

fn default_value(name: &str, default: FieldDefault) -> JsonValue {
    default
        .to_toml()
        .map_or(JsonValue::Null, |value| redacted(name, &value))
}

fn redacted(name: &str, value: &toml::Value) -> JsonValue {
    if is_sensitive_key(name) {
        JsonValue::String(REDACTED.to_owned())
    } else {
        redact_value(value)
    }
}
