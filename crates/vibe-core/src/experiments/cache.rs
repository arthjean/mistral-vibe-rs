//! The last response a user's lookup resolved, kept across sessions.
//!
//! The lookup sits on the startup path: an identity request and an eval
//! request before a variant is known. A session that finds a recent answer for
//! the same credential here applies it before the first frame, without a
//! request, and its own lookup replaces it in the background. The file is
//! shared with every other client that runs under the same vibe home,
//! including the Python reference, so its layout is the reference's to the
//! byte: one compact JSON object keyed by the API-key digest, each entry the
//! second it was stored at and the response as `model_dump(mode="json")`
//! writes it.
//!
//! Reference: `vibe/core/experiments/cache.py` at the pinned commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use toml::Table;

use super::json::{JsonValue, OrderedMap};
use super::manager::hash_api_key;
use super::models::EvalResponse;
use super::session::{CredentialSource, experiments_allowed, mistral_provider_and_api_key};

/// The file the cache lives in, under the vibe home.
///
/// Reference `EXPERIMENT_EVAL_CACHE_FILE`.
pub const EVAL_CACHE_FILE_NAME: &str = "experiment_eval_cache.json";

/// How long an entry is applied after it was stored, so a variant the rollout
/// has long since moved off is never served.
///
/// Reference `_EVAL_CACHE_TTL_SECONDS`.
pub const EVAL_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The key an entry records the second it was stored at.
const STORED_AT_KEY: &str = "stored_at_timestamp";
/// The key an entry records the response under.
const PAYLOAD_KEY: &str = "payload";

/// One vibe home's eval cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCache {
    path: PathBuf,
}

impl EvalCache {
    /// The cache a vibe home holds.
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        Self {
            path: vibe_home.join(EVAL_CACHE_FILE_NAME),
        }
    }

    /// Where the entries are read from and written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The response stored for this configuration's credential, while it is
    /// recent enough to apply.
    ///
    /// Nothing is answered when telemetry or the experiments are turned off,
    /// when no Mistral credential resolves, when the file or its entry does
    /// not read, when the entry is older than [`EVAL_CACHE_TTL`], and when its
    /// response does not validate.
    ///
    /// Reference `load_cached_eval_response`.
    #[must_use]
    pub fn load(&self, effective: &Table, credentials: &CredentialSource) -> Option<EvalResponse> {
        self.load_at(effective, credentials, now_seconds())
    }

    /// [`Self::load`] as of `now`, in seconds since the epoch.
    #[must_use]
    pub fn load_at(
        &self,
        effective: &Table,
        credentials: &CredentialSource,
        now: i64,
    ) -> Option<EvalResponse> {
        let key = cache_key(effective, credentials)?;
        let entries = self.read_entries();
        let entry = entries.get(&key)?.as_object()?;
        let stored_at = integer(entry.get(STORED_AT_KEY)?)?;
        let payload = entry.get(PAYLOAD_KEY)?;
        payload.as_object()?;
        if i128::from(stored_at) <= i128::from(now) - i128::from(ttl_seconds()) {
            return None;
        }
        serde_json::from_str(&payload.python_json()).ok()
    }

    /// Records `response` for this configuration's credential, keeping every
    /// other entry the file holds in its place.
    ///
    /// A configuration that would not load an entry writes none. A write that
    /// fails leaves the previous file in place, removes its staging file and
    /// reports nothing, because a cache that cannot be written only costs the
    /// next session a request.
    ///
    /// Reference `store_cached_eval_response`.
    pub fn store(
        &self,
        effective: &Table,
        credentials: &CredentialSource,
        response: &EvalResponse,
    ) {
        self.store_at(effective, credentials, response, now_seconds());
    }

    /// [`Self::store`] as of `now`, in seconds since the epoch.
    pub fn store_at(
        &self,
        effective: &Table,
        credentials: &CredentialSource,
        response: &EvalResponse,
        now: i64,
    ) {
        let Some(key) = cache_key(effective, credentials) else {
            return;
        };
        let Some(payload) = dumped(response) else {
            return;
        };
        let mut entries = self.read_entries();
        let mut entry = OrderedMap::new();
        entry.insert(STORED_AT_KEY.to_owned(), JsonValue::Number(now.into()));
        entry.insert(PAYLOAD_KEY.to_owned(), payload);
        entries.insert(key, JsonValue::Object(entry));
        self.write_entries(&entries);
    }

    /// The variant of `feature` in the first recent entry that carries it,
    /// whichever credential stored the entry. Reference `_load_rollout_cache`
    /// and `_rollout_variant_from_cache`, which read the harness rollout before
    /// any configuration exists.
    #[must_use]
    pub fn rollout_variant(&self, feature: &str) -> Option<String> {
        self.rollout_variant_at(feature, now_seconds())
    }

    /// [`Self::rollout_variant`] as of `now`, in seconds since the epoch.
    #[must_use]
    pub fn rollout_variant_at(&self, feature: &str, now: i64) -> Option<String> {
        let response = self.read_entries().iter().find_map(|(_, entry)| {
            let entry = entry.as_object()?;
            let stored_at = integer(entry.get(STORED_AT_KEY)?)?;
            let payload = entry.get(PAYLOAD_KEY)?;
            payload.as_object()?;
            if i128::from(stored_at) <= i128::from(now) - i128::from(ttl_seconds()) {
                return None;
            }
            let response: EvalResponse = serde_json::from_str(&payload.python_json()).ok()?;
            response.features.get(feature).is_some().then_some(response)
        })?;
        match response.features.get(feature)?.resolved_value() {
            JsonValue::String(variant) => Some(variant.clone()),
            _ => None,
        }
    }

    /// Every entry the file holds, in the order it holds them, or none when it
    /// does not read as a JSON object.
    ///
    /// Reference `_read_entries`.
    fn read_entries(&self) -> OrderedMap<JsonValue> {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<JsonValue>(&text).ok())
            .and_then(|value| match value {
                JsonValue::Object(entries) => Some(entries),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Replaces the file with `entries`, staged beside it under a name that
    /// carries this process's identifier and moved into place, so a reader
    /// never sees half a document.
    ///
    /// Reference `_write_entries`.
    fn write_entries(&self, entries: &OrderedMap<JsonValue>) {
        let Some(name) = self.path.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let staged = self
            .path
            .with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let text = JsonValue::Object(entries.clone()).python_json_compact();
        let written = self
            .path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&staged, text))
            .and_then(|()| fs::rename(&staged, &self.path));
        if written.is_err() {
            drop(fs::remove_file(&staged));
        }
    }
}

/// The digest an entry is keyed by, or [`None`] where this configuration
/// neither reads nor writes the cache.
///
/// Reference `_cache_key`.
fn cache_key(effective: &Table, credentials: &CredentialSource) -> Option<String> {
    if !experiments_allowed(effective) {
        return None;
    }
    let (_, api_key) = mistral_provider_and_api_key(effective, credentials)?;
    Some(hash_api_key(&api_key))
}

/// A response as `model_dump(mode="json")` writes it: every declared field, in
/// declaration order, the features in the order the response holds them.
fn dumped(response: &EvalResponse) -> Option<JsonValue> {
    let text = serde_json::to_string(response).ok()?;
    serde_json::from_str(&text).ok()
}

/// A stored second, read as Python's `isinstance(value, int)` reads one: an
/// integer, or a boolean, which Python counts as one.
fn integer(value: &JsonValue) -> Option<i64> {
    match value {
        JsonValue::Bool(flag) => Some(i64::from(*flag)),
        JsonValue::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn ttl_seconds() -> i64 {
    i64::try_from(EVAL_CACHE_TTL.as_secs()).unwrap_or(i64::MAX)
}

/// The current second, as `int(time.time())` answers it.
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}
