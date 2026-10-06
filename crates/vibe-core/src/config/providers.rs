//! The `providers` list, read from the effective document and written back.
//!
//! Reference `apply_provider_to_config` writes only what differs from the
//! provider model's defaults, so today's defaults are never pinned into an
//! operator's file. Keeping that rule and the field table it reads beside each
//! other is what makes "which fields does this port model" answerable in one
//! place instead of at every write site.

use toml::{Table, Value};

use super::{ConfigError, ConfigMutation, ConfigPatchOp, ConfigSnapshot, JsonPointer, patch};

impl super::LayeredConfig {
    /// Upserts one provider entry into the `providers` list implicit writes
    /// land in, keyed by `name`.
    ///
    /// Reference `apply_provider_to_config`: the payload is the provider model
    /// dumped without its defaults, in model field order, so today's defaults
    /// are never pinned into the file, and it replaces the whole entry of the
    /// same name, any field the model does not know included. The write
    /// happens even when the configuration already resolves the same provider.
    /// `reason` labels the change event, as the reference's `reason=` does.
    pub fn persist_provider(
        &self,
        provider: &Table,
        reason: &str,
    ) -> Result<ConfigSnapshot, ConfigError> {
        provider
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                ConfigError::InvalidProvider(
                    "a provider entry requires a non-empty name".to_owned(),
                )
            })?;
        let snapshot = self.load()?;
        let target = snapshot.selected_target;
        let existing = snapshot
            .target_values
            .get(&target)
            .and_then(|values| values.get("providers"));
        let mutation = patch::resolve_upsert(
            existing,
            &JsonPointer::from_segments(["providers"]),
            "name",
            provider_payload(provider),
        );
        self.patch_implicit_target(mutation, reason)
    }

    /// Sets one top-level field in the file implicit writes land in.
    /// Reference `ConfigOrchestrator.set_field`, as `apply_console_base_url`
    /// and `apply_vibe_base_url` call it: the value is written even when the
    /// effective configuration already carries it.
    pub fn persist_field(
        &self,
        key: &str,
        value: Value,
        reason: &str,
    ) -> Result<ConfigSnapshot, ConfigError> {
        self.patch_implicit_target(ConfigMutation::set([key], value), reason)
    }

    /// One mutation routed where an operation naming no layer goes, a failed
    /// write reported as an error. Reference `ConfigOrchestrator.apply_patch`.
    fn patch_implicit_target(
        &self,
        mutation: ConfigMutation,
        reason: &str,
    ) -> Result<ConfigSnapshot, ConfigError> {
        let outcome = self.apply_patch(
            &[ConfigPatchOp {
                mutation,
                target: None,
            }],
            reason,
        )?;
        match outcome.failures.first() {
            Some(failure) => Err(ConfigError::WriteFailed(failure.clone())),
            None => Ok(outcome.snapshot),
        }
    }
}

/// Reference `ProviderConfig.model_dump(exclude_none=True, exclude_defaults=True)`:
/// the modeled fields in model order, a browser-auth URL the model fills in
/// for the mistral provider included, and every default left out.
fn provider_payload(provider: &Table) -> Table {
    let mut payload = Table::new();
    for field in MODELED_PROVIDER_FIELDS {
        let value = match *field {
            "browser_auth_base_url" | "browser_auth_api_base_url" => {
                crate::auth::effective_browser_auth_url(provider, field).map(Value::String)
            }
            _ => provider.get(*field).cloned(),
        };
        if let Some(value) = value
            && !is_provider_field_default(field, &value)
        {
            payload.insert((*field).to_owned(), value);
        }
    }
    payload
}

/// The provider fields this port models, mirroring the reference
/// `ProviderConfig` field set in its declaration order.
const MODELED_PROVIDER_FIELDS: &[&str] = &[
    "name",
    "api_base",
    "api_key_env_var",
    "browser_auth_base_url",
    "browser_auth_api_base_url",
    "browser_auth_allow_origin_rewrite",
    "api_style",
    "backend",
    "reasoning_field_name",
    "emits_finish_reason",
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
        "emits_finish_reason" => value.as_bool() == Some(true),
        "browser_auth_allow_origin_rewrite" => value.as_bool() == Some(false),
        "extra_headers" => value.as_table().is_some_and(Table::is_empty),
        _ => false,
    }
}
