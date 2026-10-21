//! The catalog cache: one entry per provider fingerprint, fresh for ten
//! minutes, in `connector_bootstrap_cache.json` under the Vibe home (reference
//! `ConnectorCatalogCache` in `vibe/app_server/connector_catalog.py`).

use std::path::Path;

use serde_json::{Map, Value, json};
use vibe_core::atomic_file::{FileLock, FileMode, write_atomically};

use super::{ResolvedCatalog, canonical_json, resolve_catalog};

const CACHE_FORMAT: i64 = 2;
const CACHE_TTL_SECONDS: i64 = 10 * 60;
const MAX_CACHE_ENTRY_BYTES: usize = 2 * 1024 * 1024;
/// The cache file under the Vibe home (reference `CONNECTOR_BOOTSTRAP_CACHE_FILE`).
pub(crate) const CACHE_FILE: &str = "connector_bootstrap_cache.json";

/// Whether `catalog`'s cache entry stays within the bound a cache entry is
/// read back under.
pub(super) fn fits(catalog: &ResolvedCatalog) -> bool {
    canonical_json(&cache_entry(catalog, 0)).len() <= MAX_CACHE_ENTRY_BYTES
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
/// providers, under the cache's lock and through a temporary file flushed and
/// renamed over it (reference `ConnectorCatalogCache.write`).
pub(crate) fn write_cache(
    path: &Path,
    catalog: &ResolvedCatalog,
    stored_at: i64,
) -> std::io::Result<()> {
    let entry = cache_entry(catalog, stored_at);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = FileLock::acquire(&path.with_file_name(format!(".{CACHE_FILE}.lock")))?;
    let mut entries = read_cache_entries(path)
        .into_iter()
        .filter(|(fingerprint, entry)| parse_cache_entry(fingerprint, entry, stored_at).is_some())
        .collect::<Map<_, _>>();
    entries.insert(catalog.fingerprint.clone(), entry);
    // The entries are keyed by credential, so the file is owner-only.
    write_atomically(
        path,
        CACHE_FILE,
        canonical_json(&Value::Object(entries)).as_bytes(),
        FileMode::Private,
    )
    .map_err(|error| error.source)
}

#[cfg(test)]
mod cache_tests;
