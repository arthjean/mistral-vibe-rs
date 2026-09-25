//! The provider and model entries a model call is made from.
//!
//! Reference `ProviderConfig` and `ModelConfig` (`vibe/core/config/models.py`)
//! and the five API budgets of `VibeConfigSchema`. The configuration layer
//! already merged precedence and filled per-entry defaults into an effective
//! table; these types read that table the way the reference's models validate
//! it, and [`ModelRouting`] answers the one question a backend needs: which
//! model and which provider a call runs on.

use std::collections::BTreeMap;
use std::time::Duration;

use toml::{Table, Value};

use crate::matching::NameFilter;

/// Reference `Backend`: the Mistral SDK client, or the generic HTTP client
/// with an adapter picked by `api_style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Mistral,
    Generic,
}

impl BackendKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mistral => "mistral",
            Self::Generic => "generic",
        }
    }
}

/// One `[[providers]]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub name: String,
    pub api_base: String,
    pub api_key_env_var: String,
    pub api_style: String,
    pub backend: BackendKind,
    pub reasoning_field_name: String,
    /// Whether a stream that ends without a finish reason is incomplete.
    pub emits_finish_reason: bool,
    pub project_id: String,
    pub region: String,
    pub extra_headers: BTreeMap<String, String>,
}

impl ProviderConfig {
    /// A provider with every field at the reference default.
    #[must_use]
    pub fn new(name: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            api_base: api_base.into(),
            api_key_env_var: String::new(),
            api_style: "openai".to_owned(),
            backend: BackendKind::Generic,
            reasoning_field_name: "reasoning_content".to_owned(),
            emits_finish_reason: true,
            project_id: String::new(),
            region: String::new(),
            extra_headers: BTreeMap::new(),
        }
    }

    /// The provider this port's launch arguments describe: a style, the
    /// address the backend appends its endpoint to, and the variable the key
    /// is read from. `mistral` is the Mistral backend and names the provider
    /// the reference ships; every other style is the generic backend under
    /// that `api_style`. `None` for a style no backend speaks.
    #[must_use]
    pub fn for_style(style: &str, api_base: &str, api_key_env_var: &str) -> Option<Self> {
        let mut provider = Self::new(style, api_base);
        provider.api_key_env_var = api_key_env_var.to_owned();
        match style {
            "mistral" => provider.backend = BackendKind::Mistral,
            "openai" | "reasoning" | "anthropic" | "openai-responses" | "vertex-anthropic" => {
                provider.api_style = style.to_owned();
            }
            _ => return None,
        }
        Some(provider)
    }

    /// Reads an entry as `ProviderConfig.model_validate` does. `None` when the
    /// two required fields are missing.
    #[must_use]
    pub fn from_table(table: &Table) -> Option<Self> {
        let name = table.get("name")?.as_str()?.to_owned();
        let api_base = table.get("api_base")?.as_str()?.to_owned();
        let mut provider = Self::new(name, api_base);
        if let Some(value) = string(table, "api_key_env_var") {
            provider.api_key_env_var = value;
        }
        if let Some(value) = string(table, "api_style") {
            provider.api_style = value;
        }
        if let Some(value) = string(table, "backend") {
            provider.backend = if value == "mistral" {
                BackendKind::Mistral
            } else {
                BackendKind::Generic
            };
        }
        if let Some(value) = string(table, "reasoning_field_name") {
            provider.reasoning_field_name = value;
        }
        if let Some(value) = table.get("emits_finish_reason").and_then(Value::as_bool) {
            provider.emits_finish_reason = value;
        }
        if let Some(value) = string(table, "project_id") {
            provider.project_id = value;
        }
        if let Some(value) = string(table, "region") {
            provider.region = value;
        }
        if let Some(headers) = table.get("extra_headers").and_then(Value::as_table) {
            provider.extra_headers = headers
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect();
        }
        Some(provider)
    }
}

/// One `[[models]]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelConfig {
    pub name: String,
    pub provider: String,
    pub alias: String,
    pub temperature: f64,
    /// `off`, `low`, `medium`, `high` or `max`.
    pub thinking: String,
    pub supports_images: bool,
}

impl ModelConfig {
    /// A model with every optional field at the reference default.
    #[must_use]
    pub fn new(name: impl Into<String>, provider: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            alias: name.clone(),
            name,
            provider: provider.into(),
            temperature: 0.2,
            thinking: "off".to_owned(),
            supports_images: false,
        }
    }

    /// Reads an entry, defaulting the alias to the name as
    /// `_default_alias_to_name` does. `fallback_alias` is the key the entry was
    /// stored under when `models` is a table.
    #[must_use]
    pub fn from_table(table: &Table, fallback_alias: Option<&str>) -> Option<Self> {
        let name = table.get("name")?.as_str()?.to_owned();
        let provider = table.get("provider")?.as_str()?.to_owned();
        let mut model = Self::new(name, provider);
        if let Some(alias) = string(table, "alias").or_else(|| fallback_alias.map(str::to_owned)) {
            model.alias = alias;
        }
        if let Some(value) = table.get("temperature").and_then(float) {
            model.temperature = value;
        }
        if let Some(value) = string(table, "thinking") {
            model.thinking = value;
        }
        if let Some(value) = table.get("supports_images").and_then(Value::as_bool) {
            model.supports_images = value;
        }
        Some(model)
    }

    /// The same model at another thinking level.
    #[must_use]
    pub fn with_thinking(mut self, thinking: impl Into<String>) -> Self {
        self.thinking = thinking.into();
        self
    }
}

/// The five budgets every backend is built with. Reference
/// `vibe/core/config/_defaults.py`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApiSettings {
    pub timeout: Duration,
    pub retry_max_elapsed_time: Duration,
    pub connect_timeout: Duration,
    pub write_timeout: Duration,
    pub pool_timeout: Duration,
}

impl Default for ApiSettings {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(720),
            retry_max_elapsed_time: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(30),
            pool_timeout: Duration::from_secs(10),
        }
    }
}

impl ApiSettings {
    /// Reads the five `api_*` keys of an effective table; a key that is absent
    /// or not a non-negative number keeps its default.
    #[must_use]
    pub fn from_table(table: &Table) -> Self {
        let defaults = Self::default();
        let read = |key: &str, fallback: Duration| {
            table
                .get(key)
                .and_then(float)
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map_or(fallback, Duration::from_secs_f64)
        };
        Self {
            timeout: read("api_timeout", defaults.timeout),
            retry_max_elapsed_time: read(
                "api_retry_max_elapsed_time",
                defaults.retry_max_elapsed_time,
            ),
            connect_timeout: read("api_connect_timeout", defaults.connect_timeout),
            write_timeout: read("api_write_timeout", defaults.write_timeout),
            pool_timeout: read("api_pool_timeout", defaults.pool_timeout),
        }
    }
}

/// Every provider and model a configuration declares, and the model new turns
/// run on.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRouting {
    pub providers: Vec<ProviderConfig>,
    pub models: Vec<ModelConfig>,
    /// The alias `active_model` resolves to, with the unpinned sentinel
    /// already resolved.
    pub active_alias: Option<String>,
    pub allowed_models: Vec<String>,
    pub api: ApiSettings,
}

/// Why a call has no model or provider to run on. Reference raises
/// `ValueError` from `get_active_model` and `get_provider_for_model`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    #[error("Active model '{0}' not found in configuration.")]
    UnknownModel(String),
    #[error("Provider '{provider}' for model '{model}' not found in configuration.")]
    UnknownProvider { provider: String, model: String },
}

impl ModelRouting {
    /// Reads the routing out of an effective configuration table.
    #[must_use]
    pub fn from_effective(effective: &Table, active_alias: Option<&str>) -> Self {
        let providers = effective
            .get("providers")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_table)
                    .filter_map(ProviderConfig::from_table)
                    .collect()
            })
            .unwrap_or_default();
        let models = match effective.get("models") {
            Some(Value::Table(models)) => models
                .iter()
                .filter_map(|(alias, entry)| {
                    ModelConfig::from_table(entry.as_table()?, Some(alias))
                })
                .collect(),
            Some(Value::Array(models)) => models
                .iter()
                .filter_map(Value::as_table)
                .filter_map(|entry| ModelConfig::from_table(entry, None))
                .collect(),
            _ => Vec::new(),
        };
        let allowed_models = effective
            .get("allowed_models")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            providers,
            models,
            active_alias: active_alias.map(str::to_owned),
            allowed_models,
            api: ApiSettings::from_table(effective),
        }
    }

    /// The model a call names, by alias first and then by API name, or the
    /// active model when it names none.
    ///
    /// # Errors
    ///
    /// A name that matches no entry, as `get_active_model` refuses one.
    pub fn model(&self, requested: Option<&str>) -> Result<ModelConfig, RoutingError> {
        let wanted = requested
            .or(self.active_alias.as_deref())
            .unwrap_or_default();
        self.models
            .iter()
            .find(|model| model.alias == wanted)
            .or_else(|| self.models.iter().find(|model| model.name == wanted))
            .cloned()
            .ok_or_else(|| RoutingError::UnknownModel(wanted.to_owned()))
    }

    /// Reference `get_provider_for_model`.
    ///
    /// # Errors
    ///
    /// A model whose provider is not declared.
    pub fn provider_for(&self, model: &ModelConfig) -> Result<ProviderConfig, RoutingError> {
        self.providers
            .iter()
            .find(|provider| provider.name == model.provider)
            .cloned()
            .ok_or_else(|| RoutingError::UnknownProvider {
                provider: model.provider.clone(),
                model: model.name.clone(),
            })
    }

    /// Reference `get_mistral_provider`: the active provider when it is served
    /// by the Mistral backend, else the first provider that is.
    #[must_use]
    pub fn mistral_provider(&self) -> Option<ProviderConfig> {
        if let Ok(active) = self.model(None)
            && let Ok(provider) = self.provider_for(&active)
            && provider.backend == BackendKind::Mistral
        {
            return Some(provider);
        }
        self.providers
            .iter()
            .find(|provider| provider.backend == BackendKind::Mistral)
            .cloned()
    }

    /// The provider a launch's own arguments name (a style, an address and a
    /// key variable), with what only a configuration entry can say taken
    /// from the active model's provider when that entry is of the same kind:
    /// its name, its extra headers, whether it reports a finish reason, the
    /// reasoning field it uses and its Vertex project and region. `None` for
    /// a style no backend speaks.
    #[must_use]
    pub fn launch_provider(
        &self,
        style: &str,
        api_base: &str,
        api_key_env_var: &str,
    ) -> Option<ProviderConfig> {
        let mut provider = ProviderConfig::for_style(style, api_base, api_key_env_var)?;
        let configured = self
            .model(None)
            .ok()
            .and_then(|model| self.provider_for(&model).ok())
            .filter(|entry| {
                entry.backend == provider.backend
                    && (entry.backend == BackendKind::Mistral
                        || entry.api_style == provider.api_style)
            });
        if let Some(entry) = configured {
            provider.name = entry.name;
            provider.extra_headers = entry.extra_headers;
            provider.emits_finish_reason = entry.emits_finish_reason;
            provider.reasoning_field_name = entry.reasoning_field_name;
            provider.project_id = entry.project_id;
            provider.region = entry.region;
        }
        Some(provider)
    }

    /// Whether the allowlist admits an alias, as `name_matches` reads it; an
    /// empty allowlist admits every model.
    #[must_use]
    pub fn allows(&self, alias: &str) -> bool {
        self.allowed_models.is_empty() || NameFilter::new(&self.allowed_models).matches(alias)
    }
}

fn string(table: &Table, key: &str) -> Option<String> {
    table.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Float(value) => Some(*value),
        #[allow(clippy::cast_precision_loss)]
        Value::Integer(value) => Some(*value as f64),
        _ => None,
    }
}
