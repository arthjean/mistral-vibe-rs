//! The `providers` list, read from the effective document and written back.
//!
//! Reference `persist_provider_to_config` writes only what differs from the
//! provider model's defaults, so today's defaults are never pinned into an
//! operator's file. Keeping that rule and the field table it reads beside each
//! other is what makes "which fields does this port model" answerable in one
//! place instead of at every write site.

use toml::{Table, Value};

use super::{ConfigError, ConfigSnapshot, ConfigWrite, JsonPointer, patch};

impl super::LayeredConfig {
    /// The provider entry `name` resolves to in the effective document, which
    /// is the entry the setup flow starts from and hands back to
    /// [`Self::persist_provider`] once it may have modified it.
    pub fn effective_provider(&self, name: &str) -> Result<Option<Table>, ConfigError> {
        let snapshot = self.load()?;
        Ok(provider_entry(snapshot.effective.get("providers"), name).cloned())
    }

    /// Upserts one provider entry into the `providers` list writes land in,
    /// keyed by `name`.
    ///
    /// Reference `persist_provider_to_config`: the payload carries only the
    /// fields that differ from the provider model's defaults, so today's
    /// defaults are not pinned into the file. Two deliberate refinements: a
    /// provider identical to what the configuration already resolves is not
    /// written at all, and fields this port does not model survive on an
    /// existing entry instead of being replaced away.
    ///
    /// Answers `None` when no write was needed, and the written snapshot
    /// otherwise.
    pub fn persist_provider(
        &self,
        provider: &Table,
    ) -> Result<Option<ConfigSnapshot>, ConfigError> {
        let name = provider
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                ConfigError::InvalidProvider(
                    "a provider entry requires a non-empty name".to_owned(),
                )
            })?;
        let snapshot = self.load()?;
        if provider_entry(snapshot.effective.get("providers"), name) == Some(provider) {
            return Ok(None);
        }
        let target = snapshot.selected_target;
        let existing = snapshot
            .target_values
            .get(&target)
            .and_then(|values| values.get("providers"));
        let mut entry = provider_entry(existing, name).cloned().unwrap_or_default();
        for field in MODELED_PROVIDER_FIELDS {
            entry.remove(*field);
        }
        for (key, value) in provider {
            if !is_provider_field_default(key, value) {
                entry.insert(key.clone(), value.clone());
            }
        }
        let mutation = patch::resolve_upsert(
            existing,
            &JsonPointer::from_segments(["providers"]),
            "name",
            entry,
        );
        self.batch_write(&[ConfigWrite {
            target,
            expected_fingerprint: snapshot.fingerprints.get(&target).cloned().flatten(),
            mutations: vec![mutation],
        }])
        .map(Some)
    }
}

/// The provider fields this port models, mirroring the reference
/// `ProviderConfig` field set. An upsert replaces exactly these on an existing
/// entry and leaves every other field where it stands.
const MODELED_PROVIDER_FIELDS: &[&str] = &[
    "name",
    "api_base",
    "api_key_env_var",
    "browser_auth_base_url",
    "browser_auth_api_base_url",
    "api_style",
    "backend",
    "reasoning_field_name",
    "project_id",
    "region",
    "extra_headers",
];

/// Whether `value` is the reference provider model's default for `key`, which
/// is what `model_dump(exclude_defaults=True)` leaves out of the payload. The
/// two browser-auth URLs default to nothing, so any present value commits.
fn is_provider_field_default(key: &str, value: &Value) -> bool {
    match key {
        "api_key_env_var" | "project_id" | "region" => value.as_str() == Some(""),
        "api_style" => value.as_str() == Some("openai"),
        "backend" => value.as_str() == Some("generic"),
        "reasoning_field_name" => value.as_str() == Some("reasoning_content"),
        "extra_headers" => value.as_table().is_some_and(Table::is_empty),
        _ => false,
    }
}

/// The `providers` entry named `name`, when the value is a list holding one.
pub(super) fn provider_entry<'a>(entries: Option<&'a Value>, name: &str) -> Option<&'a Table> {
    entries?
        .as_array()?
        .iter()
        .filter_map(Value::as_table)
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
}
