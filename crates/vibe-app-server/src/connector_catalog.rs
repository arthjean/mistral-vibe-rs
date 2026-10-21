//! The account connector catalog a host owns: discovery, the cache it
//! persists, the selection the configuration makes, and the views
//! `connector_catalog/read` and `connector_catalog/refresh` publish.
//!
//! Reference `vibe/app_server/connector_catalog.py`. The bootstrap is fetched
//! from the Mistral provider's server, bounded and validated into a catalog
//! whose revision hashes what a session would route; the catalog is cached per
//! provider fingerprint for ten minutes, in memory and in
//! `connector_bootstrap_cache.json` under the Vibe home. A read never fetches:
//! it answers what memory or the cache hold, or `not_loaded`.

use serde_json::{Map, Value, json};
use toml::Value as TomlValue;
use vibe_core::config::ConfigSnapshot;
use vibe_core::pycompat::{self, Dumps};
use vibe_core::text::sha256_hex;

mod bootstrap;
mod cache;
mod selection;
mod sources;
mod tools;

pub(crate) use bootstrap::{fetch_bootstrap, resolve_catalog};
pub(crate) use cache::{CACHE_FILE, read_cache, write_cache};
pub(crate) use selection::{
    ConnectorSelection, ConnectorSetting, catalog_sources, connector_settings, resolve_selection,
    selections_view, toggled_selection, validate_toggle,
};
use sources::SourceStatus;

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
const MAX_PUBLIC_NAME_CHARACTERS: usize = 256;

/// The account the catalog is read for.
#[derive(Debug, Clone)]
pub(crate) struct CatalogProvider {
    pub(crate) fingerprint: String,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedTool {
    pub(crate) raw_name: String,
    pub(crate) description: Option<String>,
    pub(crate) input_schema: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedConnector {
    pub(crate) raw_id: String,
    pub(crate) alias: String,
    pub(crate) display_name: String,
    pub(crate) ready: bool,
    pub(crate) auth_action: &'static str,
    pub(crate) tools: Vec<ResolvedTool>,
    pub(crate) diagnostics: Vec<String>,
}

impl ResolvedConnector {
    /// The connector's diagnostics as one sentence, or `None` without any.
    fn diagnostic(&self) -> Option<String> {
        (!self.diagnostics.is_empty()).then(|| self.diagnostics.join("; "))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedCatalog {
    pub(crate) fingerprint: String,
    pub(crate) revision: String,
    pub(crate) connectors: Vec<ResolvedConnector>,
}

/// The catalog a session accepted and how its routes stand.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionConnectors {
    pub(crate) accepted: Option<(ResolvedCatalog, ConnectorSelection)>,
    pub(crate) route_revision: u64,
    /// Whether the catalog the session opens with was resolved yet. The
    /// reference resolves it while it builds the session; this port does it
    /// once the session is attached, before its opening is answered.
    pub(crate) opened: bool,
    /// Whether the session holds a connector registry, which reference
    /// `_build_connector_registry` builds for a Mistral provider with a
    /// credential whatever `enable_connectors` says. Without one the runtime
    /// publishes no connector total.
    pub(crate) registry: bool,
    /// The account the session's connector tools call the gateway with.
    pub(crate) account: Option<CatalogProvider>,
    /// A catalog accepted while a turn ran, which the session converges on
    /// once the turn ends (reference `_accept_candidate` and the
    /// `turn/completed` convergence in `vibe/app_server/server.py`).
    pub(crate) pending: Option<(ResolvedCatalog, ConnectorSelection)>,
    /// Why a unified session's catalog could not be loaded, which its MCP
    /// view reports as `connectorError` until a catalog is accepted.
    pub(crate) load_error: Option<String>,
    /// Whether a unified session accepted its opening catalog unannounced.
    /// The reference resolves it in the background and parks the session's
    /// reconfiguration until the next turn starts, which then publishes
    /// `runtime/updated` (`_resolve_connectors_background` and
    /// `_announce_runtime` in
    /// `vibe/app_server/_unified_harness_backend_adapter.py`).
    pub(crate) announce: bool,
}

/// Where a read found the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Memory,
    FreshCache,
    NotLoaded,
}

impl Disposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::FreshCache => "fresh_cache",
            Self::NotLoaded => "not_loaded",
        }
    }
}

/// `json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`.
fn canonical_json(value: &Value) -> String {
    pycompat::dumps(
        value,
        Dumps {
            ensure_ascii: false,
            sort_keys: true,
            separators: Some((",", ":")),
            ..Dumps::DEFAULT
        },
    )
}

/// Reference `normalize_connector_alias`.
pub(crate) fn normalize_connector_alias(name: &str) -> String {
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = normalized
        .trim_matches(|character| character == '_' || character == '-')
        .chars()
        .take(MAX_PUBLIC_NAME_CHARACTERS)
        .collect::<String>();
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed
    }
}

fn is_mistral(provider: &toml::Table) -> bool {
    provider
        .get("backend")
        .and_then(TomlValue::as_str)
        .unwrap_or("mistral")
        == "mistral"
}

/// Reference `get_mistral_provider`: the active provider when it is a
/// Mistral one, the first Mistral provider otherwise.
fn mistral_provider(snapshot: &ConfigSnapshot) -> Option<toml::Table> {
    snapshot
        .active_provider()
        .filter(is_mistral)
        .or_else(|| snapshot.entries("providers").into_iter().find(is_mistral))
}

/// The Mistral provider's API base, which its identity is read under.
pub(crate) fn connector_api_base(snapshot: &ConfigSnapshot) -> Option<String> {
    mistral_provider(snapshot)?
        .get("api_base")
        .and_then(TomlValue::as_str)
        .map(str::to_owned)
}

/// Whether the configuration turns connectors on (`enable_connectors`,
/// true by default).
pub(crate) fn connectors_enabled(snapshot: &ConfigSnapshot) -> bool {
    snapshot
        .effective
        .get("enable_connectors")
        .and_then(TomlValue::as_bool)
        .unwrap_or(true)
}

/// Reference `_resolve_provider`: connectors enabled, a Mistral provider and
/// a credential for it.
pub(crate) fn resolve_provider(
    snapshot: &ConfigSnapshot,
    resolve_key: impl Fn(&str) -> Option<String>,
) -> Option<CatalogProvider> {
    connectors_enabled(snapshot)
        .then(|| mistral_account(snapshot, resolve_key))
        .flatten()
}

/// The Mistral provider with its credential, whether or not connectors are
/// enabled: what a session's connector registry is built for.
pub(crate) fn mistral_account(
    snapshot: &ConfigSnapshot,
    resolve_key: impl Fn(&str) -> Option<String>,
) -> Option<CatalogProvider> {
    let provider = mistral_provider(snapshot)?;
    let variable = provider
        .get("api_key_env_var")
        .and_then(TomlValue::as_str)
        .filter(|variable| !variable.is_empty())
        .unwrap_or("MISTRAL_API_KEY");
    let api_key = resolve_key(variable).filter(|key| !key.is_empty())?;
    let api_base = provider
        .get("api_base")
        .and_then(TomlValue::as_str)
        .unwrap_or_default();
    let base_url = vibe_core::llm::mistral::server_url(api_base)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned();
    Some(CatalogProvider {
        fingerprint: sha256_hex(format!("{base_url}\0{api_key}").as_bytes()),
        base_url,
        api_key,
    })
}

/// Why a bootstrap could not become a catalog. The message is what the
/// refresh publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogError(pub(crate) String);

/// Reference `_connector_readiness`: the catalog's own word for how a
/// connector stands, `ready` where a session would read it connected.
fn readiness(connector: &ResolvedConnector) -> &'static str {
    match SourceStatus::derive(true, connector.ready, Some(connector.auth_action)) {
        SourceStatus::Connected => "ready",
        status => status.as_str(),
    }
}

/// Reference `_project_catalog`.
pub(crate) fn catalog_view(catalog: Option<&ResolvedCatalog>, disposition: Disposition) -> Value {
    json!({
        "disposition": disposition.as_str(),
        "catalogRevision": catalog.map(|catalog| catalog.revision.clone()),
        "connectors": catalog.map(|catalog| catalog.connectors.iter().map(|connector| json!({
            "alias": connector.alias,
            "displayName": connector.display_name,
            "readiness": readiness(connector),
            "authAction": connector.auth_action,
            "tools": connector.tools.iter().map(|tool| json!({
                "name": tool.raw_name,
                "description": tool.description,
            })).collect::<Vec<_>>(),
            "diagnostic": connector.diagnostic(),
        })).collect::<Vec<_>>()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod connector_catalog_tests;
