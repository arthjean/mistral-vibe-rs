//! What one load of a layered configuration composes, and the readers a
//! session takes its settings through.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use toml::{Table, Value};

use super::integrations::{IntegrationCollection, config_array};
use super::mcp::decode_mcp_server;
use super::{ConfigError, ConfigLayer, ConfigTarget, active_model_alias, registry};
use crate::mcp::McpServerConfig;
use crate::redaction::{is_sensitive_key, redact_table, redact_value};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    pub effective: Table,
    pub selected_target: ConfigTarget,
    pub selected_path: PathBuf,
    pub fingerprints: BTreeMap<ConfigTarget, Option<String>>,
    pub target_values: BTreeMap<ConfigTarget, Table>,
    pub layer_values: Vec<ConfigLayer>,
    /// What the load repaired rather than rejected, in the order the repairs
    /// were made. Reference `VibeConfigSchema.validation_warnings`.
    pub validation_warnings: Vec<String>,
    /// Whether a session started without a cached rollout is still waiting
    /// for the lookup that may route it onto another model. Reference
    /// `AgentLoop.awaiting_experiment_model`, published as
    /// `ConfigView.awaitingExperimentModel`.
    pub awaiting_experiment_model: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntegrationPreference {
    pub enabled: bool,
    pub disabled_tools: BTreeSet<String>,
}

impl ConfigSnapshot {
    /// The alias new turns run on, with the unpinned sentinel resolved.
    ///
    /// The merged document keeps the sentinel, so anything selecting a model
    /// reads it through here rather than through `active_model` directly.
    #[must_use]
    pub fn active_model_alias(&self) -> Option<&str> {
        active_model_alias(&self.effective)
    }

    #[must_use]
    pub fn public_view(&self) -> JsonValue {
        json!({
            "config": redact_table(&self.effective),
            "selectedTarget": self.selected_target,
            "selectedPath": self.selected_path,
            "fingerprints": self.fingerprints,
            "targetValues": self.target_values.iter().map(|(target, values)| {
                (target, redact_table(values))
            }).collect::<BTreeMap<_, _>>(),
            "layers": self.layer_values.iter().map(|layer| layer.kind).collect::<Vec<_>>(),
            "layerValues": self.layer_values.iter().map(|layer| json!({
                "layer": layer.kind,
                "values": redact_table(&layer.values),
            })).collect::<Vec<_>>(),
            // What the load repaired rather than rejected, so a client can say
            // so instead of silently running on a different model.
            "validationWarnings": self.validation_warnings,
        })
    }

    #[must_use]
    pub fn public_value(&self, key: &str) -> JsonValue {
        self.effective
            .get(key)
            .map(|value| {
                if is_sensitive_key(key) {
                    JsonValue::String("[redacted]".to_owned())
                } else {
                    redact_value(value)
                }
            })
            .unwrap_or(JsonValue::Null)
    }

    /// The effective keys [`registry::FIELDS`] does not declare, in document
    /// order.
    ///
    /// The reference merge drops a key its schema does not know; this port keeps
    /// it, so a key persisted by a newer client survives a round trip through
    /// this one. This accessor is how that set is reported rather than silently
    /// carried.
    #[must_use]
    pub fn unregistered_keys(&self) -> Vec<&str> {
        self.effective
            .keys()
            .map(String::as_str)
            .filter(|key| registry::field(key).is_none())
            .collect()
    }

    /// The `enabled_tools` allowlist the configuration carries.
    ///
    /// Reference `VibeConfigSchema.enabled_tools`: when it holds an entry, only
    /// the tools it matches are published. Entries are glob or `re:` patterns,
    /// matched by [`crate::matching::NameFilter`].
    #[must_use]
    pub fn enabled_tools(&self) -> Vec<String> {
        self.string_array("enabled_tools")
    }

    /// The `disabled_tools` denylist the configuration carries, applied after
    /// the allowlist as reference `available_tools` applies it.
    #[must_use]
    pub fn disabled_tools(&self) -> Vec<String> {
        self.string_array("disabled_tools")
    }

    /// The extra tool directories the configuration names.
    ///
    /// Reference `VibeConfigSchema.tool_paths`: entries are concatenated across
    /// layers and expanded by `_expand_paths`, and `_compute_search_paths`
    /// walks them right after the builtin directory. They stay strings here
    /// because expansion needs the home and the working directory, which
    /// [`crate::tools::descriptions::search_paths`] is the one place that
    /// knows.
    #[must_use]
    pub fn tool_paths(&self) -> Vec<String> {
        self.string_array("tool_paths")
    }

    /// The extra skill directories the configuration names.
    ///
    /// Reference `VibeConfigSchema.skill_paths`: entries are concatenated
    /// across layers and expanded by `_expand_paths`. They stay strings here
    /// because expansion needs the home and the working directory, which
    /// [`crate::skills::search_paths`] is the one place that knows.
    #[must_use]
    pub fn skill_paths(&self) -> Vec<String> {
        self.string_array("skill_paths")
    }

    /// The `enabled_skills` allowlist. When it holds an entry, only the skills
    /// it matches are published and `disabled_skills` is ignored.
    #[must_use]
    pub fn enabled_skills(&self) -> Vec<String> {
        self.string_array("enabled_skills")
    }

    /// The `disabled_skills` denylist, consulted only when `enabled_skills`
    /// holds nothing.
    #[must_use]
    pub fn disabled_skills(&self) -> Vec<String> {
        self.string_array("disabled_skills")
    }

    /// Whether the registry-skills experiment is enabled.
    ///
    /// Reference `experimental_enable_registry_skills` gates loading the
    /// pinned registry skills into a session, the session-start sync, and the
    /// `/skills` browser (`vibe/core/skills/manager.py:188`,
    /// `vibe/core/agent_loop/_loop.py:858-866`).
    #[must_use]
    pub fn registry_skills_enabled(&self) -> bool {
        self.effective
            .get("experimental_enable_registry_skills")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The string entries of a top-level array key, skipping anything that is
    /// not a string so one mistyped entry cannot take the session down.
    fn string_array(&self, key: &str) -> Vec<String> {
        self.effective
            .get(key)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn mcp_servers(
        &self,
        working_directory: &Path,
    ) -> Result<Vec<McpServerConfig>, ConfigError> {
        let Some(value) = self.effective.get("mcp_servers") else {
            return Ok(Vec::new());
        };
        let entries = value.as_array().ok_or_else(|| {
            ConfigError::InvalidMcp("mcp_servers must be an array of tables".to_owned())
        })?;
        let mut aliases = BTreeSet::new();
        let mut servers = Vec::new();
        for entry in entries {
            let table = entry.as_table().ok_or_else(|| {
                ConfigError::InvalidMcp("each mcp_servers entry must be a table".to_owned())
            })?;
            let server = decode_mcp_server(table, working_directory)?;
            if !aliases.insert(server.alias.clone()) {
                return Err(ConfigError::InvalidMcp(
                    "MCP server names must be unique".to_owned(),
                ));
            }
            servers.push(server);
        }
        Ok(servers)
    }

    /// The alias of every configured server, under the name a reader sees.
    pub fn mcp_aliases(&self) -> Result<BTreeSet<String>, ConfigError> {
        config_array(&self.effective, IntegrationCollection::McpServers)?
            .into_iter()
            .map(|entry| {
                entry
                    .as_table()
                    .and_then(|entry| IntegrationCollection::McpServers.identity_key(entry))
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ConfigError::InvalidMcp(
                            "each effective mcp_servers entry requires a non-empty name".to_owned(),
                        )
                    })
            })
            .collect()
    }
}
