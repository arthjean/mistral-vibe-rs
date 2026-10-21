//! The connector bootstrap: fetched from the Mistral provider's server, then
//! bounded and validated into a catalog whose revision hashes what a session
//! would route (reference `_fetch_bootstrap` and `_resolve_catalog` in
//! `vibe/app_server/connector_catalog.py`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::{Map, Value, json};
use vibe_core::text::sha256_hex;

use super::{
    CatalogError, CatalogProvider, ConnectorAuthAction, MAX_PUBLIC_NAME_CHARACTERS,
    ResolvedCatalog, ResolvedConnector, ResolvedTool, cache, canonical_json,
    normalize_connector_alias,
};

const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTORS: usize = 256;
const MAX_TOOLS_PER_CONNECTOR: usize = 128;
const MAX_INPUT_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTICS_PER_CONNECTOR: usize = 3;
const MAX_DIAGNOSTIC_CHARACTERS: usize = 512;

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

fn auth_action(action: Option<&str>) -> ConnectorAuthAction {
    match action {
        None => ConnectorAuthAction::None,
        Some("oauth") => ConnectorAuthAction::Oauth,
        Some("credentials_setup") => ConnectorAuthAction::CredentialsSetup,
        Some(_) => ConnectorAuthAction::Unknown,
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
    let revision = sha256_hex(
        canonical_json(&Value::Array(
            connectors.iter().map(revision_payload).collect(),
        ))
        .as_bytes(),
    );
    let catalog = ResolvedCatalog {
        fingerprint: fingerprint.to_owned(),
        revision,
        connectors,
    };
    if !cache::fits(&catalog) {
        return Err(CatalogError(
            "Connector bootstrap cache projection exceeds 2 MiB".to_owned(),
        ));
    }
    Ok(catalog)
}

#[cfg(test)]
mod bootstrap_tests;
