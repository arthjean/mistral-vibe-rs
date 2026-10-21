//! What the configuration selects out of a catalog: the `connectors`
//! entries, the tool filters, the toggles that rewrite them, and the revision
//! that hashes every decision (reference `resolve_connector_selection`,
//! `connector_source_enabled` and `connector_tool_enabled`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use toml::Value as TomlValue;
use vibe_core::config::ConfigSnapshot;
use vibe_core::matching::NameFilter;
use vibe_core::text::sha256_hex;

use super::{
    MAX_PUBLIC_NAME_CHARACTERS, ResolvedCatalog, canonical_json, connectors_enabled,
    normalize_connector_alias,
};

/// One `connectors` entry of the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectorSetting {
    pub(crate) alias: String,
    pub(crate) disabled: bool,
    pub(crate) disabled_tools: BTreeSet<String>,
}

/// What the configuration selects out of a catalog (reference
/// `ResolvedConnectorSelection`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectorSelection {
    pub(crate) revision: String,
    pub(crate) enable_connectors: bool,
    pub(crate) settings: Vec<ConnectorSetting>,
    pub(crate) enabled_tools: Vec<String>,
    pub(crate) disabled_tools: Vec<String>,
    /// Whether a connector no entry names is on: reference
    /// `implicit_source_enabled`, which only a Unified Harness session sets
    /// (`vibe/app_server/_runtime.py`).
    pub(crate) implicit: bool,
}

/// The configuration's `connectors` entries.
pub(crate) fn connector_settings(snapshot: &ConfigSnapshot) -> Vec<ConnectorSetting> {
    snapshot
        .entries("connectors")
        .into_iter()
        .filter_map(|entry| {
            Some(ConnectorSetting {
                alias: entry.get("name")?.as_str()?.to_owned(),
                disabled: entry
                    .get("disabled")
                    .and_then(TomlValue::as_bool)
                    .unwrap_or(false),
                disabled_tools: entry
                    .get("disabled_tools")
                    .and_then(TomlValue::as_array)
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(TomlValue::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

impl ConnectorSelection {
    /// Reference `connector_source_enabled`: a connector no entry names is
    /// off on the legacy backend and on under the Unified Harness.
    pub(crate) fn source_enabled(&self, alias: &str) -> bool {
        self.enable_connectors
            && self
                .settings
                .iter()
                .find(|setting| setting.alias == alias)
                .map_or(self.implicit, |setting| !setting.disabled)
    }

    /// Reference `connector_tool_enabled`.
    pub(crate) fn tool_enabled(&self, alias: &str, raw_tool_name: &str) -> bool {
        if !self.source_enabled(alias) {
            return false;
        }
        if self
            .settings
            .iter()
            .find(|setting| setting.alias == alias)
            .is_some_and(|setting| setting.disabled_tools.contains(raw_tool_name))
        {
            return false;
        }
        let published = format!("connector_{alias}_{raw_tool_name}");
        if !self.enabled_tools.is_empty()
            && !NameFilter::new(&self.enabled_tools).matches(&published)
        {
            return false;
        }
        !(!self.disabled_tools.is_empty()
            && NameFilter::new(&self.disabled_tools).matches(&published))
    }
}

/// Reference `resolve_connector_selection` with the legacy backend's
/// `implicit_source_enabled=False`.
pub(crate) fn resolve_selection(
    snapshot: &ConfigSnapshot,
    catalog: Option<&ResolvedCatalog>,
) -> ConnectorSelection {
    let mut selection = ConnectorSelection {
        revision: String::new(),
        enable_connectors: connectors_enabled(snapshot),
        settings: connector_settings(snapshot),
        enabled_tools: snapshot.enabled_tools(),
        disabled_tools: snapshot.disabled_tools(),
        implicit: false,
    };
    selection.revision = selection_revision(&selection, &catalog_sources(catalog));
    selection
}

/// Reference `_project_selections`.
pub(crate) fn selections_view(
    snapshot: &ConfigSnapshot,
    catalog: Option<&ResolvedCatalog>,
) -> Value {
    let resolved = catalog
        .map(|catalog| {
            catalog
                .connectors
                .iter()
                .map(|connector| connector.alias.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    // The entries are listed as written: the disabled tools keep their order
    // and repetitions, which the selection itself reads as a set.
    Value::Array(
        snapshot
            .entries("connectors")
            .into_iter()
            .filter_map(|entry| {
                let alias = entry.get("name")?.as_str()?.to_owned();
                let disabled_tools = entry
                    .get("disabled_tools")
                    .and_then(TomlValue::as_array)
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(TomlValue::as_str)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Some(json!({
                    "alias": alias,
                    "disabled": entry.get("disabled").and_then(TomlValue::as_bool).unwrap_or(false),
                    "disabledTools": disabled_tools,
                    "state": if resolved.contains(&alias) { "resolved" } else { "pending" },
                }))
            })
            .collect(),
    )
}

/// Reference `_validate_toggle`.
pub(crate) fn validate_toggle(alias: &str, tool_name: Option<&str>) -> Result<(), &'static str> {
    if alias != normalize_connector_alias(alias) {
        return Err("Connector alias must already be normalized");
    }
    if let Some(tool_name) = tool_name
        && (tool_name.trim().is_empty() || tool_name.chars().count() > MAX_PUBLIC_NAME_CHARACTERS)
    {
        return Err("Connector tool name must contain 1 to 256 characters");
    }
    Ok(())
}

/// The selection `selection` becomes once a toggle of `alias` (or of its
/// `tool_name`) to `disabled` is written, over `sources`: the reference's
/// candidate configuration, resolved before anything is persisted.
pub(crate) fn toggled_selection(
    selection: &ConnectorSelection,
    alias: &str,
    disabled: bool,
    tool_name: Option<&str>,
    sources: &[(String, Vec<String>)],
) -> ConnectorSelection {
    let mut candidate = selection.clone();
    match candidate
        .settings
        .iter_mut()
        .find(|setting| setting.alias == alias)
    {
        Some(setting) => match tool_name {
            Some(tool) if disabled => {
                setting.disabled_tools.insert(tool.to_owned());
            }
            Some(tool) => {
                setting.disabled_tools.remove(tool);
            }
            None => setting.disabled = disabled,
        },
        None => candidate.settings.push(ConnectorSetting {
            alias: alias.to_owned(),
            disabled: tool_name.is_none() && disabled,
            disabled_tools: tool_name
                .filter(|_| disabled)
                .map(|tool| BTreeSet::from([tool.to_owned()]))
                .unwrap_or_default(),
        }),
    }
    candidate.revision = selection_revision(&candidate, sources);
    candidate
}

fn selection_revision(selection: &ConnectorSelection, sources: &[(String, Vec<String>)]) -> String {
    let sources = sources.iter().cloned().collect::<BTreeMap<_, _>>();
    let decisions = sources
        .iter()
        .map(|(alias, tools)| {
            let mut tools = tools.clone();
            tools.sort();
            json!({
                "alias": alias,
                "sourceEnabled": selection.source_enabled(alias),
                "tools": tools.iter().map(|tool| json!({
                    "name": tool,
                    "enabled": selection.tool_enabled(alias, tool),
                })).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    sha256_hex(canonical_json(&Value::Array(decisions)).as_bytes())
}

/// The `(alias, tools)` pairs a catalog routes.
pub(crate) fn catalog_sources(catalog: Option<&ResolvedCatalog>) -> Vec<(String, Vec<String>)> {
    catalog
        .map(|catalog| catalog.connectors.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|connector| {
            (
                connector.alias.clone(),
                connector
                    .tools
                    .iter()
                    .map(|tool| tool.raw_name.clone())
                    .collect(),
            )
        })
        .collect()
}
