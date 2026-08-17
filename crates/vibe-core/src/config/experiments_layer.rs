//! Turning a rollout variant into a configuration value.
//!
//! This is the only place an experiment reaches the configuration, and it is
//! deliberately narrow: three experiments, four fields, and a mapper per field
//! that writes nothing at all unless the variant reads as the value the field
//! declares. A rollout that misfires therefore leaves the document exactly as
//! the other layers composed it, which is what keeps a malformed payload from
//! becoming a broken installation.
//!
//! Two properties are load-bearing beyond the mapping itself. The layer is fed
//! by [`crate::experiments::ExperimentManager::config_variants`] rather than by
//! `assignments`, so a forced value reaches configuration while only a
//! confirmed exposure reaches telemetry. And the layer composes *below* the
//! selected file (see the layer list in [`crate::config`]), so any value an
//! operator wrote beats any assignment.
//!
//! Reference: `vibe/core/config/layers/growthbook.py` at the pinned commit.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use toml::{Table, Value};

use crate::experiments::{ExperimentName, JsonValue};
use crate::text::hex_encode;

/// The name the reference gives this layer, which is how a caller addresses it
/// in the reference's orchestrator.
///
/// Reference `GrowthbookLayer.NAME`.
pub const EXPERIMENTS_LAYER_NAME: &str = "growthbook";

/// The configuration fields one experiment writes, in the order the reference
/// declares them.
///
/// The pairing is exhaustive over [`ExperimentName`], so an experiment added
/// without a field set does not compile.
///
/// Reference `GROWTHBOOK_CONFIG_MAPPINGS`.
#[must_use]
pub const fn configured_fields(name: ExperimentName) -> &'static [&'static str] {
    match name {
        ExperimentName::SystemPrompt => &["system_prompt_id"],
        ExperimentName::ManagedShellTools => &["managed_shell_tools_enabled"],
        ExperimentName::CliModelRouting => &["routed_default_model", "routed_model_config"],
    }
}

/// Whether a system prompt identifier resolves to a prompt this installation
/// can load.
///
/// The mapper validates before it writes, and what "resolvable" means belongs
/// to the caller rather than to the mapping: a session hands its own
/// [`crate::prompt::PromptResolver`], which covers the project and user prompt
/// directories as well as the builtins. Reference `load_system_prompt`, whose
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
    /// A key this build does not know is skipped, and so is a variant whose
    /// field mapper refuses it.
    #[must_use]
    pub fn from_variants(
        variants: &BTreeMap<String, String>,
        prompt_resolves: PromptResolves,
    ) -> Self {
        let mut values = Table::new();
        for name in ExperimentName::ALL {
            let Some(variant) = variants.get(name.key()) else {
                continue;
            };
            for field in configured_fields(name) {
                if let Some(value) = mapped_field(name, field, variant, prompt_resolves) {
                    values.insert((*field).to_owned(), value);
                }
            }
        }
        let fingerprint = (!values.is_empty()).then(|| fingerprint(&values));
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
/// routing payload that is not an object, an empty alias and a shell variant
/// outside the managed arm all leave the field unwritten.
///
/// Reference `_map_system_prompt_variant`, `_map_default_routing_model`,
/// `_map_routed_model_config` and the inline managed-shell mapper.
fn mapped_field(
    name: ExperimentName,
    field: &str,
    variant: &str,
    prompt_resolves: PromptResolves,
) -> Option<Value> {
    match (name, field) {
        (ExperimentName::SystemPrompt, "system_prompt_id") => {
            prompt_resolves(variant).then(|| Value::String(variant.to_owned()))
        }
        // The reference compares against the managed arm and writes `True` for
        // it alone, so `legacy` writes nothing rather than writing `false`: the
        // legacy family is what an unwritten field already selects.
        (ExperimentName::ManagedShellTools, "managed_shell_tools_enabled") => {
            (variant == "managed").then_some(Value::Boolean(true))
        }
        (ExperimentName::CliModelRouting, "routed_default_model") => routing_payload(variant)?
            .get("active_model")
            .and_then(JsonValue::as_str)
            .filter(|alias| !alias.is_empty())
            .map(|alias| Value::String(alias.to_owned())),
        // The definition travels as the text of one model, which the merged
        // document coerces back into the model itself. Re-encoding it here
        // rather than carrying the decoded value is what the reference does,
        // and the ordered JSON is what keeps the text byte-identical to the
        // payload the proxy wrote.
        (ExperimentName::CliModelRouting, "routed_model_config") => {
            let payload = routing_payload(variant)?;
            let definition = payload.get("model_config")?;
            definition
                .as_object()
                .map(|_| Value::String(definition.python_json()))
        }
        _ => None,
    }
}

/// The routing variant as the object it carries, or [`None`] where it is not
/// JSON or not an object.
fn routing_payload(variant: &str) -> Option<crate::experiments::OrderedMap<JsonValue>> {
    let value = serde_json::from_str::<JsonValue>(variant).ok()?;
    value.as_object().cloned()
}

/// The token standing for one composed document.
///
/// Reference `create_dict_fingerprint`: the SHA-256 of the JSON dump of the
/// data, keys sorted and separators compact.
fn fingerprint(values: &Table) -> String {
    let payload = values
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let text = serde_json::to_string(&serde_json::Value::Object(payload)).unwrap_or_default();
    hex_encode(&Sha256::digest(text.as_bytes()))
}
