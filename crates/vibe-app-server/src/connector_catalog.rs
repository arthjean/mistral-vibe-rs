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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use toml::Value as TomlValue;
use vibe_core::config::ConfigSnapshot;
use vibe_core::matching::NameFilter;
use vibe_core::mcp::authorization::python_json_unicode;

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
const CACHE_FORMAT: i64 = 2;
const CACHE_TTL_SECONDS: i64 = 10 * 60;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTORS: usize = 256;
const MAX_TOOLS_PER_CONNECTOR: usize = 128;
const MAX_INPUT_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTICS_PER_CONNECTOR: usize = 3;
const MAX_DIAGNOSTIC_CHARACTERS: usize = 512;
const MAX_CACHE_ENTRY_BYTES: usize = 2 * 1024 * 1024;
const MAX_PUBLIC_NAME_CHARACTERS: usize = 256;
/// The cache file under the Vibe home (reference `CONNECTOR_BOOTSTRAP_CACHE_FILE`).
pub(crate) const CACHE_FILE: &str = "connector_bootstrap_cache.json";

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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedCatalog {
    pub(crate) fingerprint: String,
    pub(crate) revision: String,
    pub(crate) connectors: Vec<ResolvedConnector>,
}

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
}

/// The catalog a session accepted and how its routes stand.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionConnectors {
    pub(crate) accepted: Option<(ResolvedCatalog, ConnectorSelection)>,
    pub(crate) route_revision: u64,
    /// Whether the catalog the session opens with was resolved yet. The
    /// reference resolves it while it builds the session; this port does it
    /// before the first connector operation reads the session.
    pub(crate) opened: bool,
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

fn canonical_json(value: &Value) -> String {
    python_json_unicode(value)
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

/// Reference `get_server_url_from_api_base`: the scheme and host of an API
/// base that ends in a `/v<digits>` segment.
fn server_url(api_base: &str) -> Option<String> {
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|scheme| api_base.starts_with(scheme))?;
    let rest = &api_base[scheme.len()..];
    // The server part is greedy up to the last `/v<digit>` that leaves a
    // non-empty host, as the reference's `(https?://.+)(/v\d+.*)` matches it.
    let mut cut = None;
    for (index, _) in rest.match_indices("/v") {
        if index > 0 && rest[index + 2..].starts_with(|character: char| character.is_ascii_digit())
        {
            cut = Some(index);
        }
    }
    cut.map(|index| format!("{scheme}{}", &rest[..index]))
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
    if !connectors_enabled(snapshot) {
        return None;
    }
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
    let base_url = server_url(api_base)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned();
    Some(CatalogProvider {
        fingerprint: sha256_hex(&format!("{base_url}\0{api_key}")),
        base_url,
        api_key,
    })
}

/// Why a bootstrap could not become a catalog. The message is what the
/// refresh publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogError(pub(crate) String);

/// Reference `_fetch_bootstrap` and `_redacted_bootstrap_error`.
pub(crate) async fn fetch_bootstrap(provider: &CatalogProvider) -> Result<Value, CatalogError> {
    let unavailable = |kind: &str| CatalogError(format!("Failed to load connectors: {kind}"));
    let client = reqwest::Client::builder()
        .timeout(BOOTSTRAP_TIMEOUT)
        .build()
        .map_err(|_| unavailable("RuntimeError"))?;
    let response = client
        .get(format!("{}/v1/connectors/bootstrap", provider.base_url))
        .bearer_auth(&provider.api_key)
        .query(&[
            ("include_auth_actionable_connectors", "true"),
            ("builtin_connectors", "web_search"),
            ("supports_mcp", "true"),
        ])
        .send()
        .await
        .map_err(|error| unavailable(transport_error_kind(&error)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(CatalogError(format!(
            "Failed to load connectors (HTTP {}).",
            status.as_u16()
        )));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| unavailable(transport_error_kind(&error)))?;
    serde_json::from_slice(&body).map_err(|_| unavailable("JSONDecodeError"))
}

/// The `httpx` exception a transport failure of this kind raises.
fn transport_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "ReadTimeout"
    } else if error.is_connect() {
        "ConnectError"
    } else if error.is_redirect() {
        "TooManyRedirects"
    } else {
        "RemoteProtocolError"
    }
}

fn text_field(object: &Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

/// One bootstrap connector as `_BootstrapConnector` validates it, or
/// `None` when it is malformed.
struct BootstrapConnector {
    id: Option<String>,
    name: Option<String>,
    display_name: Option<String>,
    ready: bool,
    tools: Vec<Value>,
    auth_action: Option<String>,
    bootstrap_errors: Value,
}

fn bootstrap_connector(value: &Value) -> Option<BootstrapConnector> {
    let object = value.as_object()?;
    let id = text_field(object, "id").ok()?;
    let name = text_field(object, "name").ok()?;
    let display_name = text_field(object, "display_name").ok()?;
    text_field(object, "protocol").ok()?;
    let ready = match object.get("status") {
        None => false,
        Some(Value::Object(status)) => match status.get("is_ready") {
            None => false,
            Some(Value::Bool(ready)) => *ready,
            Some(_) => return None,
        },
        Some(_) => return None,
    };
    let tools = match object.get("tools") {
        None => Vec::new(),
        Some(Value::Array(tools)) => tools.clone(),
        Some(_) => return None,
    };
    let auth_action = match object.get("auth_action") {
        None | Some(Value::Null) => None,
        Some(Value::Object(action)) => match action.get("type") {
            None => Some("none".to_owned()),
            Some(Value::String(kind)) => Some(kind.clone()),
            Some(_) => return None,
        },
        Some(_) => return None,
    };
    Some(BootstrapConnector {
        id,
        name,
        display_name,
        ready,
        tools,
        auth_action,
        bootstrap_errors: object
            .get("bootstrap_errors")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

fn resolve_tool(value: &Value) -> Option<ResolvedTool> {
    let object = value.as_object()?;
    let name = object.get("name")?.as_str()?.trim().to_owned();
    let description = match object.get("description") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return None,
    };
    let input_schema = match object
        .get("inputSchema")
        .or_else(|| object.get("input_schema"))
    {
        None => Map::new(),
        Some(Value::Object(schema)) => schema.clone(),
        Some(_) => return None,
    };
    if name.is_empty()
        || canonical_json(&Value::Object(input_schema.clone())).len() > MAX_INPUT_SCHEMA_BYTES
    {
        return None;
    }
    Some(ResolvedTool {
        raw_name: name,
        description,
        input_schema,
    })
}

fn auth_action(action: Option<&str>) -> &'static str {
    match action {
        None => "none",
        Some("oauth") => "oauth",
        Some("credentials_setup") => "credentials_setup",
        Some(_) => "unknown",
    }
}

/// Reference `_bounded_diagnostics`: up to three public sentences, each a
/// generic failure or the issue code a backend message leads with.
fn bounded_diagnostics(value: &Value) -> Vec<String> {
    let values = match value {
        Value::Array(values) => values.clone(),
        other => vec![other.clone()],
    };
    let is_code = |code: &str| {
        let mut characters = code.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
            && code.len() <= 64
            && characters.all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
            })
    };
    let mut diagnostics = Vec::new();
    for raw in values {
        if diagnostics.len() == MAX_DIAGNOSTICS_PER_CONNECTOR {
            break;
        }
        let Value::String(raw) = raw else {
            continue;
        };
        let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        if text == "Connector failed to bootstrap."
            || text
                .strip_prefix("Connector bootstrap issue: ")
                .is_some_and(is_code)
        {
            diagnostics.push(text);
            continue;
        }
        let diagnostic = match text.split_once(':') {
            Some((code, _)) if is_code(code) => format!("Connector bootstrap issue: {code}"),
            _ => "Connector failed to bootstrap.".to_owned(),
        };
        diagnostics.push(diagnostic.chars().take(MAX_DIAGNOSTIC_CHARACTERS).collect());
    }
    diagnostics
}

fn unique_alias(candidate: String, used: &mut BTreeSet<String>) -> String {
    let mut alias = candidate.clone();
    let mut suffix = 2;
    while used.contains(&alias) {
        let suffix_text = format!("_{suffix}");
        let keep = MAX_PUBLIC_NAME_CHARACTERS.saturating_sub(suffix_text.len());
        alias = format!(
            "{}{suffix_text}",
            candidate.chars().take(keep).collect::<String>()
        );
        suffix += 1;
    }
    used.insert(alias.clone());
    alias
}

fn revision_payload(connector: &ResolvedConnector) -> Value {
    json!({
        "raw_id": connector.raw_id,
        "alias": connector.alias,
        "display_name": connector.display_name,
        "ready": connector.ready,
        "auth_action": connector.auth_action,
        "tools": connector.tools.iter().map(|tool| json!({
            "raw_name": tool.raw_name,
            "description": tool.description,
            "input_schema": tool.input_schema,
        })).collect::<Vec<_>>(),
        "diagnostics": connector.diagnostics,
    })
}

/// Reference `_resolve_catalog`: only a broken envelope fails; malformed
/// connectors, duplicated ids and connectors breaking the tool bounds are
/// dropped, and the rest are ordered by id with unique aliases.
pub(crate) fn resolve_catalog(
    payload: &Value,
    fingerprint: &str,
) -> Result<ResolvedCatalog, CatalogError> {
    let malformed = || CatalogError("Connector bootstrap payload is malformed".to_owned());
    let object = payload.as_object().ok_or_else(malformed)?;
    let items = match object.get("connectors") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(malformed()),
    };
    let parsed = items
        .iter()
        .filter_map(bootstrap_connector)
        .collect::<Vec<_>>();
    let mut id_counts = BTreeMap::<String, usize>::new();
    for connector in &parsed {
        let id = connector.id.as_deref().unwrap_or_default().trim();
        if !id.is_empty() {
            *id_counts.entry(id.to_owned()).or_default() += 1;
        }
    }
    let mut prepared = parsed
        .into_iter()
        .filter_map(|connector| {
            let raw_id = connector
                .id
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_owned();
            if raw_id.is_empty() || id_counts.get(&raw_id).copied().unwrap_or_default() > 1 {
                return None;
            }
            let pick = |value: Option<&str>| {
                let value = value.unwrap_or(&raw_id).trim();
                if value.is_empty() {
                    raw_id.clone()
                } else {
                    value.to_owned()
                }
            };
            let alias_source = pick(connector.name.as_deref().filter(|name| !name.is_empty()));
            let display_name = pick(
                connector
                    .display_name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .or(connector.name.as_deref().filter(|name| !name.is_empty())),
            );
            Some((raw_id, alias_source, display_name, connector))
        })
        .collect::<Vec<_>>();
    prepared.sort_by(|left, right| left.0.cmp(&right.0));
    let mut aliases = BTreeSet::new();
    let mut connectors = Vec::new();
    for (raw_id, alias_source, display_name, connector) in prepared {
        let alias = unique_alias(normalize_connector_alias(&alias_source), &mut aliases);
        if connectors.len() >= MAX_CONNECTORS || connector.tools.len() > MAX_TOOLS_PER_CONNECTOR {
            continue;
        }
        let mut tools = connector
            .tools
            .iter()
            .filter_map(resolve_tool)
            .collect::<Vec<_>>();
        let names = tools
            .iter()
            .map(|tool| tool.raw_name.clone())
            .collect::<BTreeSet<_>>();
        if names.len() != tools.len() {
            continue;
        }
        tools.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
        connectors.push(ResolvedConnector {
            raw_id,
            alias,
            display_name,
            ready: connector.ready,
            auth_action: auth_action(connector.auth_action.as_deref()),
            tools,
            diagnostics: bounded_diagnostics(&connector.bootstrap_errors),
        });
    }
    let revision = sha256_hex(&canonical_json(&Value::Array(
        connectors.iter().map(revision_payload).collect(),
    )));
    let catalog = ResolvedCatalog {
        fingerprint: fingerprint.to_owned(),
        revision,
        connectors,
    };
    if canonical_json(&cache_entry(&catalog, 0)).len() > MAX_CACHE_ENTRY_BYTES {
        return Err(CatalogError(
            "Connector bootstrap cache projection exceeds 2 MiB".to_owned(),
        ));
    }
    Ok(catalog)
}

fn cache_entry(catalog: &ResolvedCatalog, stored_at: i64) -> Value {
    let connectors = catalog
        .connectors
        .iter()
        .map(|connector| {
            let mut payload = json!({
                "id": connector.raw_id,
                "name": connector.alias,
                "display_name": connector.display_name,
                "protocol": "mcp",
                "status": {"is_ready": connector.ready},
                "tools": connector.tools.iter().map(|tool| json!({
                    "name": tool.raw_name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })).collect::<Vec<_>>(),
            });
            if connector.auth_action != "none" {
                payload["auth_action"] = json!({"type": connector.auth_action});
            }
            if !connector.diagnostics.is_empty() {
                payload["diagnostics"] = json!(connector.diagnostics);
            }
            payload
        })
        .collect::<Vec<_>>();
    json!({
        "format": CACHE_FORMAT,
        "stored_at": stored_at,
        "payload": {"connectors": connectors},
    })
}

const fn is_fresh(stored_at: i64, now: i64) -> bool {
    stored_at <= now && stored_at > now - CACHE_TTL_SECONDS
}

/// Reference `_parse_cache_entry`.
fn parse_cache_entry(fingerprint: &str, entry: &Value, now: i64) -> Option<(ResolvedCatalog, i64)> {
    let object = entry.as_object()?;
    if canonical_json(entry).len() > MAX_CACHE_ENTRY_BYTES {
        return None;
    }
    let stored_at = match object.get("format") {
        None => object.get("stored_at_timestamp"),
        Some(format) if format.as_i64() == Some(CACHE_FORMAT) => object.get("stored_at"),
        Some(_) => return None,
    }?
    .as_i64()?;
    if !is_fresh(stored_at, now) {
        return None;
    }
    let payload = object.get("payload")?.as_object()?;
    let connectors = match payload.get("connectors") {
        Some(Value::Array(items)) => Value::Array(
            items
                .iter()
                .map(|item| match item {
                    Value::Object(connector) => {
                        let mut connector = connector.clone();
                        let diagnostics = connector.remove("diagnostics");
                        let source = diagnostics
                            .filter(|value| !value.is_null())
                            .or_else(|| connector.get("bootstrap_errors").cloned());
                        if let Some(source) = source.filter(|value| !value.is_null()) {
                            connector.insert("bootstrap_errors".to_owned(), source);
                        }
                        Value::Object(connector)
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.cloned().unwrap_or(Value::Null),
    };
    let catalog = resolve_catalog(&json!({"connectors": connectors}), fingerprint).ok()?;
    Some((catalog, stored_at))
}

fn read_cache_entries(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(entries) => Some(entries),
            _ => None,
        })
        .unwrap_or_default()
}

/// The fresh cached catalog for `fingerprint`, when the cache holds one.
pub(crate) fn read_cache(
    path: &Path,
    fingerprint: &str,
    now: i64,
) -> Option<(ResolvedCatalog, i64)> {
    parse_cache_entry(fingerprint, read_cache_entries(path).get(fingerprint)?, now)
}

/// Writes `catalog` into the cache, dropping the stale entries of other
/// providers, through a temporary file renamed over the cache.
pub(crate) fn write_cache(
    path: &Path,
    catalog: &ResolvedCatalog,
    stored_at: i64,
) -> std::io::Result<()> {
    let entry = cache_entry(catalog, stored_at);
    let mut entries = read_cache_entries(path)
        .into_iter()
        .filter(|(fingerprint, entry)| parse_cache_entry(fingerprint, entry, stored_at).is_some())
        .collect::<Map<_, _>>();
    entries.insert(catalog.fingerprint.clone(), entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(".{CACHE_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, python_json_unicode(&Value::Object(entries)))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
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
    /// Reference `connector_source_enabled`, for the legacy backend, whose
    /// connectors no entry names are off.
    pub(crate) fn source_enabled(&self, alias: &str) -> bool {
        self.enable_connectors
            && self
                .settings
                .iter()
                .find(|setting| setting.alias == alias)
                .is_some_and(|setting| !setting.disabled)
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
    };
    selection.revision = selection_revision(&selection, &catalog_sources(catalog));
    selection
}

fn readiness(connector: &ResolvedConnector) -> &'static str {
    if connector.ready {
        "ready"
    } else if connector.auth_action == "oauth" {
        "needs_auth"
    } else if connector.auth_action == "credentials_setup" {
        "needs_setup"
    } else {
        "unavailable"
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
            "diagnostic": (!connector.diagnostics.is_empty()).then(|| connector.diagnostics.join("; ")),
        })).collect::<Vec<_>>()).unwrap_or_default(),
    })
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
    Value::Array(
        connector_settings(snapshot)
            .into_iter()
            .map(|setting| {
                json!({
                    "alias": setting.alias,
                    "disabled": setting.disabled,
                    "disabledTools": setting.disabled_tools.iter().collect::<Vec<_>>(),
                    "state": if resolved.contains(&setting.alias) { "resolved" } else { "pending" },
                })
            })
            .collect(),
    )
}

/// Reference `_legacy_connector_source`: how a session publishes one
/// accepted connector. A ready connector is one the session connected.
fn source_status(connector: &ResolvedConnector, selection: &ConnectorSelection) -> &'static str {
    if !selection.source_enabled(&connector.alias) {
        "disabled"
    } else if !connector.ready && connector.auth_action == "oauth" {
        "needs_auth"
    } else if !connector.ready && connector.auth_action == "credentials_setup" {
        "needs_setup"
    } else if !connector.ready || connector.auth_action == "unknown" {
        "unavailable"
    } else {
        "connected"
    }
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
    sha256_hex(&canonical_json(&Value::Array(decisions)))
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

impl SessionConnectors {
    fn route_revision_text(&self) -> String {
        format!("legacy-connector-routes:{}", self.route_revision)
    }

    /// Reference `LegacySessionBackend._session_connector_state` projected
    /// as `SessionConnectorStateView`. A connector the catalog marks ready is
    /// one the session connected.
    pub(crate) fn view(&self) -> Value {
        let Some((catalog, selection)) = &self.accepted else {
            return json!({
                "acceptedCatalogRevision": "",
                "acceptedSelectionRevision": "",
                "routeRevision": self.route_revision_text(),
                "sources": [],
            });
        };
        let sources = catalog
            .connectors
            .iter()
            .map(|connector| {
                let status = source_status(connector, selection);
                json!({
                    "alias": connector.alias,
                    "displayName": connector.display_name,
                    "status": status,
                    "tools": connector.tools.iter().map(|tool| json!({
                        "name": tool.raw_name,
                        "description": tool.description,
                        "enabled": selection.tool_enabled(&connector.alias, &tool.raw_name),
                    })).collect::<Vec<_>>(),
                    "error": (!connector.diagnostics.is_empty()).then(|| connector.diagnostics.join("; ")),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "acceptedCatalogRevision": catalog.revision,
            "acceptedSelectionRevision": selection.revision,
            "routeRevision": self.route_revision_text(),
            "sources": sources,
        })
    }

    /// The source `alias` publishes, with the connector behind it.
    pub(crate) fn source(&self, alias: &str) -> Option<(&ResolvedConnector, &'static str)> {
        let (catalog, selection) = self.accepted.as_ref()?;
        let connector = catalog
            .connectors
            .iter()
            .find(|connector| connector.alias == alias)?;
        Some((connector, source_status(connector, selection)))
    }

    /// Reference `ConnectorCounts` over the accepted sources.
    pub(crate) fn counts(&self) -> Value {
        let (connected, total) = self
            .accepted
            .as_ref()
            .map_or((0, 0), |(catalog, selection)| {
                (
                    catalog
                        .connectors
                        .iter()
                        .filter(|connector| source_status(connector, selection) == "connected")
                        .count(),
                    catalog.connectors.len(),
                )
            });
        json!({"connected": connected, "total": total})
    }

    /// Reference `_project_mcp_connectors` once the legacy backend handed the
    /// accepted catalog to the session's registry: one row per connector the
    /// configuration names or the catalog publishes, sorted by name. `None`
    /// until a catalog is accepted.
    ///
    /// A connector no entry names, or one its entry disables, is disabled; a
    /// ready one publishing at least one tool is connected. Its published
    /// tools are listed with the selection deciding which are enabled, and a
    /// connector that is not ready lists its catalog tools, all disabled.
    pub(crate) fn mcp_sources(&self, settings: &[ConnectorSetting]) -> Option<Vec<Value>> {
        let (catalog, selection) = self.accepted.as_ref()?;
        let mut names = settings
            .iter()
            .map(|setting| setting.alias.as_str())
            .chain(
                catalog
                    .connectors
                    .iter()
                    .map(|connector| connector.alias.as_str()),
            )
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        let sources = names
            .into_iter()
            .map(|name| {
                let setting = settings.iter().find(|setting| setting.alias == name);
                let connector = catalog.connectors.iter().find(|connector| connector.alias == name);
                let connected = connector.is_some_and(|connector| connector.ready && !connector.tools.is_empty());
                let status = if setting.is_none_or(|setting| setting.disabled) {
                    "disabled"
                } else if connected {
                    "connected"
                } else {
                    match connector.map(|connector| connector.auth_action) {
                        Some("oauth") => "needs_auth",
                        Some("credentials_setup") => "needs_setup",
                        _ => "unavailable",
                    }
                };
                let mut tools = connector
                    .map(|connector| {
                        connector
                            .tools
                            .iter()
                            .map(|tool| {
                                let (description, enabled) = if connected {
                                    let description = tool
                                        .description
                                        .clone()
                                        .unwrap_or_else(|| format!("Connector tool '{}'", tool.raw_name));
                                    let first = description.split('\n').next().unwrap_or_default().trim().to_owned();
                                    (first, selection.tool_enabled(name, &tool.raw_name))
                                } else {
                                    (tool.description.clone().unwrap_or_default(), false)
                                };
                                json!({"name": tool.raw_name, "description": description, "enabled": enabled})
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                tools.sort_by(|left, right| {
                    left["name"]
                        .as_str()
                        .unwrap_or_default()
                        .cmp(right["name"].as_str().unwrap_or_default())
                });
                json!({
                    "name": name,
                    "displayName": name,
                    "kind": "connector",
                    "transport": "connector",
                    "status": status,
                    "tools": tools,
                    "error": connector
                        .filter(|connector| !connector.diagnostics.is_empty())
                        .map(|connector| connector.diagnostics.join("; ")),
                    "pluginName": null,
                })
            })
            .collect();
        Some(sources)
    }

    /// Accepts a catalog and the selection over it, which reroutes the
    /// session's connectors.
    pub(crate) fn accept(
        &mut self,
        catalog: ResolvedCatalog,
        selection: ConnectorSelection,
    ) -> Value {
        self.accepted = Some((catalog, selection));
        self.route_revision = self.route_revision.saturating_add(1);
        self.view()
    }
}

#[cfg(test)]
#[path = "connector_catalog_tests.rs"]
mod connector_catalog_tests;
