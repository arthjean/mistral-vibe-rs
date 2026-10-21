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
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use toml::Table;

use crate::clock::now_seconds_signed;
use crate::credential_cache::TimedEntries;

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

/// One vibe home's eval cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCache {
    entries: TimedEntries,
}

impl EvalCache {
    /// The cache a vibe home holds.
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        let ttl_seconds = i64::try_from(EVAL_CACHE_TTL.as_secs()).unwrap_or(i64::MAX);
        Self {
            entries: TimedEntries::new(
                vibe_home.join(EVAL_CACHE_FILE_NAME),
                "eval-cache",
                ttl_seconds,
            ),
        }
    }

    /// Where the entries are read from and written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.entries.path()
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
        self.load_at(effective, credentials, now_seconds_signed())
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
        let payload = self.entries.payload(&key, now)?;
        serde_json::from_value(Value::Object(payload)).ok()
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
        self.store_at(effective, credentials, response, now_seconds_signed());
    }

    /// [`Self::store`] as of `now`, in seconds since the epoch.
    ///
    /// The response is stored as `model_dump(mode="json")` writes it: every
    /// declared field, in declaration order, the features in the order the
    /// response holds them.
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
        let Ok(payload) = serde_json::to_value(response) else {
            return;
        };
        self.entries.store(key, payload, now);
    }

    /// Deletes the file and every entry it holds, whichever credential stored
    /// them.
    ///
    /// A file that is already gone, or that cannot be removed, is not an
    /// error: what is left behind is read again under the same gates, so the
    /// worst a failure costs is the deletion itself.
    ///
    /// Reference `clear_cached_eval_responses`.
    pub fn clear(&self) {
        drop(fs::remove_file(self.entries.path()));
    }

    /// The variant of `feature` in the first recent entry that carries it,
    /// whichever credential stored the entry. Reference `_load_rollout_cache`
    /// and `_rollout_variant_from_cache`, which read the harness rollout before
    /// any configuration exists.
    #[must_use]
    pub fn rollout_variant(&self, feature: &str) -> Option<String> {
        self.rollout_variant_at(feature, now_seconds_signed())
    }

    /// [`Self::rollout_variant`] as of `now`, in seconds since the epoch.
    #[must_use]
    pub fn rollout_variant_at(&self, feature: &str, now: i64) -> Option<String> {
        let response = self.entries.payloads(now).find_map(|payload| {
            let response: EvalResponse = serde_json::from_value(Value::Object(payload)).ok()?;
            response.feature(feature).is_some().then_some(response)
        })?;
        match response.feature(feature)?.resolved_value() {
            Value::String(variant) => Some(variant.clone()),
            _ => None,
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
