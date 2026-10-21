//! Reference `FileSystemUpdateCacheRepository`
//! (`vibe/cli/update_notifier/adapters/filesystem_update_cache_repository.py`)
//! and the `UpdateCache` it stores.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use vibe_core::cache_store::CacheStore;

/// Reference `UPDATE_CACHE_TTL_SECONDS`.
pub const UPDATE_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;

const CACHE_SECTION: &str = "update_cache";
/// Reference `FileSystemUpdateCacheRepository._legacy_json`.
const LEGACY_CACHE_FILE: &str = "update_cache.json";

/// Reference `UpdateCache`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCache {
    pub latest_version: String,
    pub stored_at_timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_whats_new_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_version: Option<String>,
}

impl UpdateCache {
    #[must_use]
    pub fn new(latest_version: impl Into<String>, stored_at_timestamp: i64) -> Self {
        Self {
            latest_version: latest_version.into(),
            stored_at_timestamp,
            seen_whats_new_version: None,
            dismissed_version: None,
        }
    }

    #[must_use]
    pub(super) fn is_fresh(&self, now: i64) -> bool {
        self.stored_at_timestamp > now.saturating_sub(UPDATE_CACHE_TTL_SECONDS)
    }
}

/// Reference `FileSystemUpdateCacheRepository`: a `[update_cache]` section of the
/// shared `cache.toml`, where any unreadable or malformed state reads as absent.
/// A sibling `update_cache.json` written by the pre-TOML layout is read once and
/// migrated into the section.
///
/// Like the reference repository, one store reads the disk once: the first
/// [`Self::load`] is remembered, and every [`Self::store`] replaces what is
/// remembered whether or not the write reached the disk. Clones share that
/// memory, which is how one launch threads a single repository through its
/// startup prompt, its release notes and its background check.
#[derive(Debug, Clone)]
pub struct UpdateCacheStore {
    cache: CacheStore,
    legacy_path: PathBuf,
    remembered: Arc<Mutex<Option<Option<UpdateCache>>>>,
}

impl UpdateCacheStore {
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        Self {
            cache: CacheStore::new(vibe_home),
            legacy_path: vibe_home.join(LEGACY_CACHE_FILE),
            remembered: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.cache.path()
    }

    /// The pre-TOML file [`Self::load`] migrates from, named by the reference's
    /// `_legacy_json`.
    #[must_use]
    pub fn legacy_path(&self) -> &Path {
        &self.legacy_path
    }

    /// Reference `get`: `_read_section` followed by `_parse` on the first call,
    /// and the remembered value on every later one.
    ///
    /// The reference tests the section for truthiness, so a missing section, an
    /// empty one, and a section that is not a table all fall through to the
    /// legacy file, while a populated section answers on its own even when its
    /// contents do not parse.
    #[must_use]
    pub fn load(&self) -> Option<UpdateCache> {
        let mut remembered = self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cache) = remembered.as_ref() {
            return cache.clone();
        }
        let section = self.cache.read_section(CACHE_SECTION);
        let cache = if section.is_empty() {
            self.migrate_legacy()
        } else {
            Self::parse_section(&section)
        };
        *remembered = Some(cache.clone());
        cache
    }

    /// Reference `set`: the cache becomes the payload the section merge applies,
    /// with an unset optional key omitted rather than written as empty, and it
    /// is remembered even when the write fails, because the reference's
    /// `write_section` logs that failure and returns.
    pub fn store(&self, cache: &UpdateCache) {
        let mut payload = toml::map::Map::new();
        payload.insert(
            "latest_version".to_owned(),
            toml::Value::String(cache.latest_version.clone()),
        );
        payload.insert(
            "stored_at_timestamp".to_owned(),
            toml::Value::Integer(cache.stored_at_timestamp),
        );
        if let Some(seen) = &cache.seen_whats_new_version {
            payload.insert(
                "seen_whats_new_version".to_owned(),
                toml::Value::String(seen.clone()),
            );
        }
        if let Some(dismissed) = &cache.dismissed_version {
            payload.insert(
                "dismissed_version".to_owned(),
                toml::Value::String(dismissed.clone()),
            );
        }
        self.cache.write_section(CACHE_SECTION, payload);
        *self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Some(cache.clone()));
    }

    /// Reference `_read_section`'s fallback: the pre-TOML JSON is read once, its
    /// non-null keys are merged into the section, and the values reach the
    /// caller whether or not that write succeeded.
    fn migrate_legacy(&self) -> Option<UpdateCache> {
        let text = fs::read_to_string(&self.legacy_path).ok()?;
        let legacy: serde_json::Value = serde_json::from_str(&text).ok()?;
        // The reference hands whatever the file held to `_parse` and raises an
        // attribute error on anything but an object. This port reads a non-object
        // as no cache, which is the absence every caller already handles.
        let legacy = legacy.as_object()?;
        let payload = legacy
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), legacy_value(value)?)))
            .collect();
        self.cache.write_section(CACHE_SECTION, payload);
        Self::parse_legacy(legacy)
    }

    /// Reference `_parse` over the TOML section.
    fn parse_section(section: &toml::Table) -> Option<UpdateCache> {
        Some(UpdateCache {
            latest_version: section.get("latest_version")?.as_str()?.to_owned(),
            stored_at_timestamp: section.get("stored_at_timestamp")?.as_integer()?,
            seen_whats_new_version: section
                .get("seen_whats_new_version")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            dismissed_version: section
                .get("dismissed_version")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Reference `_parse` over the legacy JSON object, whose type guards are the
    /// same two required keys and the same two optional ones.
    fn parse_legacy(legacy: &serde_json::Map<String, serde_json::Value>) -> Option<UpdateCache> {
        Some(UpdateCache {
            latest_version: legacy.get("latest_version")?.as_str()?.to_owned(),
            stored_at_timestamp: legacy.get("stored_at_timestamp")?.as_i64()?,
            seen_whats_new_version: legacy
                .get("seen_whats_new_version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            dismissed_version: legacy
                .get("dismissed_version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// One legacy JSON value as the reference's TOML writer records it. A null is
/// dropped upstream by the migration's own filter, and a value TOML cannot hold
/// is dropped here rather than failing the migration the reference completes.
fn legacy_value(value: &serde_json::Value) -> Option<toml::Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(value) => Some(toml::Value::Boolean(*value)),
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| number.as_f64().map(toml::Value::Float)),
        serde_json::Value::String(value) => Some(toml::Value::String(value.clone())),
        serde_json::Value::Array(values) => Some(toml::Value::Array(
            values.iter().filter_map(legacy_value).collect(),
        )),
        serde_json::Value::Object(entries) => Some(toml::Value::Table(
            entries
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), legacy_value(value)?)))
                .collect(),
        )),
    }
}

#[cfg(test)]
mod cache_tests;
