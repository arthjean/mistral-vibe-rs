//! Turning a rollout variant into a configuration value.
//!
//! This is the only place an experiment reaches the configuration, and it is
//! deliberately narrow: seven experiments, eight fields, and a mapper per
//! field that writes nothing at all unless the variant reads as the value the
//! field declares. A rollout that misfires therefore leaves the document
//! exactly as the other layers composed it, which is what keeps a malformed
//! payload from becoming a broken installation. Every routed model definition
//! that validates is also written into `models` under its alias, so a higher
//! layer can override one of its fields as it overrides any other model.
//!
//! Two properties are load-bearing beyond the mapping itself. The layer is fed
//! by [`crate::experiments::ExperimentManager::config_variants`] rather than by
//! `assignments`, so a forced value reaches configuration while only a
//! confirmed exposure reaches telemetry. And the layer composes *below* the
//! selected file (see the layer list in [`crate::config`]), so any value an
//! operator wrote beats any assignment.
//!
//! Reference: `vibe/core/config/layers/growthbook.py` at the pinned commit.

use sha2::{Digest, Sha256};
use toml::{Table, Value};

use crate::experiments::{ExperimentName, JsonValue, OrderedMap};
use crate::text::hex_encode;

use super::effective::validate_model_definition;

/// The name the reference gives this layer, which is how a caller addresses it
/// in the reference's orchestrator.
///
/// Reference `GrowthbookLayer.NAME`.
pub const EXPERIMENTS_LAYER_NAME: &str = "growthbook";

/// The experiments that write a field, in the order the reference declares
/// their mappings. The harness rollout is read before any configuration
/// exists, so it maps to none.
///
/// Reference `GROWTHBOOK_CONFIG_MAPPINGS`.
pub const MAPPED_EXPERIMENTS: [ExperimentName; 7] = [
    ExperimentName::SystemPrompt,
    ExperimentName::CliModelRouting,
    ExperimentName::CliExtraModels,
    ExperimentName::ManagedShellTools,
    ExperimentName::SmartApprove,
    ExperimentName::SmartApproveDefault,
    ExperimentName::RegistrySkills,
];

/// The configuration fields one experiment writes, in the order the reference
/// declares them, and none for an experiment that maps to nothing.
///
/// Reference `GROWTHBOOK_CONFIG_MAPPINGS`.
#[must_use]
pub const fn configured_fields(name: ExperimentName) -> &'static [&'static str] {
    match name {
        ExperimentName::SystemPrompt => &["system_prompt_id"],
        ExperimentName::ManagedShellTools => &["managed_shell_tools_enabled"],
        ExperimentName::CliModelRouting => &["routed_default_model", "routed_model_config"],
        ExperimentName::CliExtraModels => &["routed_extra_models"],
        ExperimentName::SmartApprove => &["smart_approve_available"],
        ExperimentName::SmartApproveDefault => &["smart_approve_default"],
        ExperimentName::RegistrySkills => &["experimental_enable_registry_skills"],
        ExperimentName::UnifiedHarnessRollout => &[],
    }
}

/// Whether a system prompt identifier resolves to a prompt this installation
/// can load.
///
/// The mapper validates before it writes, and what "resolvable" means belongs
/// to the caller rather than to the mapping: a session hands
/// [`crate::system_prompt::load_system_prompt`] over its prompt directories,
/// which covers them as well as the builtins. Reference `load_system_prompt`, whose
/// failure is what makes the reference mapper answer `None`.
pub type PromptResolves<'a> = &'a dyn Fn(&str) -> bool;

/// The document one set of configuration variants composes, with the token that
/// stands for its contents.
///
/// An empty variant set, and a variant set that maps to nothing, produce the
/// same thing: an empty document with no fingerprint, which composes as if the
/// layer were not there at all. Reference `GrowthbookLayer._build_config_snapshot`,
/// whose two `EMPTY_CONFIG_SNAPSHOT` branches this reproduces.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExperimentsLayer {
    values: Table,
    fingerprint: Option<String>,
}

impl ExperimentsLayer {
    /// The layer `variants` composes, keyed by feature key as
    /// [`crate::experiments::ExperimentManager::config_variants`] answers.
    ///
    /// A key this build does not map is skipped, and so is a variant whose
    /// field mapper refuses it.
    #[must_use]
    pub fn from_variants(
        variants: &OrderedMap<JsonValue>,
        prompt_resolves: PromptResolves,
    ) -> Self {
        let mut data: OrderedMap<JsonValue> = OrderedMap::new();
        for name in MAPPED_EXPERIMENTS {
            let Some(variant) = variants
                .get(name.key())
                .filter(|variant| !variant.is_null())
            else {
                continue;
            };
            for field in configured_fields(name) {
                if let Some(value) = mapped_field(field, variant, prompt_resolves) {
                    data.insert((*field).to_owned(), value);
                }
            }
        }
        let models = routed_models(&data);
        if !models.is_empty() {
            data.insert("models".to_owned(), JsonValue::Object(models));
        }
        let fingerprint = (!data.is_empty()).then(|| fingerprint(&data));
        let values = data
            .iter()
            .filter_map(|(key, value)| Some((key.to_owned(), toml_value(value)?)))
            .collect();
        Self {
            values,
            fingerprint,
        }
    }

    /// The document this layer contributes.
    #[must_use]
    pub const fn values(&self) -> &Table {
        &self.values
    }

    /// The document this layer contributes, consumed.
    #[must_use]
    pub fn into_values(self) -> Table {
        self.values
    }

    /// The token standing for the contents, or [`None`] where the layer wrote
    /// nothing.
    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }
}

/// What one variant writes into one field, or [`None`] where it writes nothing.
///
/// Every arm refuses rather than repairs: a prompt that does not resolve, a
/// routing payload that is not an object, an empty alias, an extra-models
/// payload with no model in it and a flag outside its "on" spellings all leave
/// the field unwritten.
///
/// Reference `_map_system_prompt_variant`, `_map_default_routing_model`,
/// `_map_routed_model_config`, `_map_routed_extra_models`, `_map_on` and the
/// inline managed-shell mapper.
fn mapped_field(
    field: &str,
    variant: &JsonValue,
    prompt_resolves: PromptResolves,
) -> Option<JsonValue> {
    match field {
        "system_prompt_id" => {
            let prompt = variant.as_str()?;
            prompt_resolves(prompt).then(|| variant.clone())
        }
        "routed_default_model" => as_json_value(variant)
            .as_object()?
            .get("active_model")?
            .as_str()
            .filter(|alias| !alias.is_empty())
            .map(|alias| JsonValue::String(alias.to_owned())),
        // The definition travels as the text of one model, which the merged
        // document coerces back into the model itself. Re-encoding it here
        // rather than carrying the decoded value is what the reference does,
        // and the ordered JSON is what keeps the text byte-identical to the
        // payload the proxy wrote.
        "routed_model_config" => {
            let payload = as_json_value(variant);
            let definition = payload.as_object()?.get("model_config")?;
            definition
                .as_object()
                .map(|_| JsonValue::String(definition.python_json()))
        }
        "routed_extra_models" => {
            let payload = as_json_value(variant);
            let listed = match &payload {
                JsonValue::Object(entries) => entries.get("models")?,
                other => other,
            };
            let models = listed
                .as_array()?
                .iter()
                .filter(|model| model.as_object().is_some())
                .cloned()
                .collect::<Vec<_>>();
            (!models.is_empty()).then(|| JsonValue::String(JsonValue::Array(models).python_json()))
        }
        // The reference compares against the managed arm and writes `True` for
        // it alone, so `legacy` writes nothing rather than writing `false`: the
        // legacy family is what an unwritten field already selects.
        "managed_shell_tools_enabled" => {
            switched_on(variant, &["managed", "true"]).then_some(JsonValue::Bool(true))
        }
        "smart_approve_available"
        | "smart_approve_default"
        | "experimental_enable_registry_skills" => {
            switched_on(variant, &["on", "true"]).then_some(JsonValue::Bool(true))
        }
        _ => None,
    }
}

/// Whether a flag variant switches its field on: the boolean `true` itself,
/// never a number equal to it, or one of `words` once stripped and lowercased
/// as Python's `str.strip().lower()` reads a string.
///
/// Reference `_map_on`, and the managed-shell mapper with its own two words.
fn switched_on(variant: &JsonValue, words: &[&str]) -> bool {
    match variant {
        JsonValue::Bool(flag) => *flag,
        JsonValue::String(text) => words.contains(&python_strip(text).to_lowercase().as_str()),
        _ => false,
    }
}

/// A string with the characters Python's `str.strip()` removes taken off both
/// ends: Unicode white space, plus the four separators Python also counts.
fn python_strip(text: &str) -> &str {
    text.trim_matches(|character: char| {
        character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
    })
}

/// A variant as the payload it carries: the JSON a string spells, the string
/// itself where it spells none, and any other value as it came.
///
/// Reference `_as_json_value`.
fn as_json_value(variant: &JsonValue) -> JsonValue {
    match variant {
        JsonValue::String(text) => serde_json::from_str(text).unwrap_or_else(|_| variant.clone()),
        other => other.clone(),
    }
}

/// Every routed definition that validates as a model, keyed by its alias: the
/// routed model first, then the extra ones, a later alias replacing an earlier
/// one. A definition naming no alias is keyed by its name and carries it,
/// because the reference's alias defaulting writes it into the very entry it
/// validates.
///
/// Reference `_routed_models`.
fn routed_models(data: &OrderedMap<JsonValue>) -> OrderedMap<JsonValue> {
    let mut models = OrderedMap::new();
    for field in ["routed_model_config", "routed_extra_models"] {
        let Some(text) = data.get(field).and_then(JsonValue::as_str) else {
            continue;
        };
        let Ok(decoded) = serde_json::from_str::<JsonValue>(text) else {
            continue;
        };
        let entries = match decoded {
            JsonValue::Array(entries) => entries,
            other => vec![other],
        };
        for entry in entries {
            let JsonValue::Object(mut entry) = entry else {
                continue;
            };
            if entry.get("alias").is_none_or(JsonValue::is_null) {
                let name = entry.get("name").cloned().unwrap_or_default();
                entry.insert("alias".to_owned(), name);
            }
            let Some(validated) = serde_json::to_value(JsonValue::Object(entry.clone()))
                .ok()
                .and_then(|value| validate_model_definition(&value))
            else {
                continue;
            };
            let Some(alias) = validated
                .get("alias")
                .and_then(Value::as_str)
                .filter(|alias| !alias.is_empty())
            else {
                continue;
            };
            models.insert(alias.to_owned(), JsonValue::Object(entry));
        }
    }
    models
}

/// One JSON value as the TOML value the layer composes, or [`None`] for a
/// null, which TOML cannot hold and which an absent key already stands for.
fn toml_value(value: &JsonValue) -> Option<Value> {
    Some(match value {
        JsonValue::Null => return None,
        JsonValue::Bool(flag) => Value::Boolean(*flag),
        JsonValue::Number(number) => number
            .as_i64()
            .map(Value::Integer)
            .or_else(|| number.as_f64().map(Value::Float))?,
        JsonValue::String(text) => Value::String(text.clone()),
        JsonValue::Array(items) => Value::Array(items.iter().filter_map(toml_value).collect()),
        JsonValue::Object(entries) => Value::Table(
            entries
                .iter()
                .filter_map(|(key, value)| Some((key.to_owned(), toml_value(value)?)))
                .collect(),
        ),
    })
}

/// The token standing for one composed document.
///
/// Reference `create_dict_fingerprint`: the SHA-256 of the JSON dump of the
/// data, keys sorted and separators compact.
fn fingerprint(data: &OrderedMap<JsonValue>) -> String {
    let text = serde_json::to_value(JsonValue::Object(data.clone()))
        .and_then(|value| serde_json::to_string(&value))
        .unwrap_or_default();
    hex_encode(&Sha256::digest(text.as_bytes()))
}
