//! Reference `vibe/cli/update_notifier/update.py`: whether an update is
//! available, from the cache or the gateway, and what the cache remembers of
//! the answer.
//!
//! The decision logic is pure so both the forced `--check-upgrade` route and the
//! background startup check replay the same cache, freshness, dismissal, and
//! release-note rules the reference applies.

use thiserror::Error;

use super::cache::{UpdateCache, UpdateCacheStore};
use super::gateway::{UpdateGateway, UpdateGatewayError};
use super::version::Version;

/// Reference `UpdateAvailability`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAvailability {
    pub latest_version: String,
    pub should_notify: bool,
}

/// Reference `UpdateError`, raised only where the reference raises it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpdateError {
    #[error("{0}")]
    Gateway(String),
}

/// Reference `get_update_if_available` before the gateway call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachePlan {
    /// The current version is unparseable, so no discovery may run.
    Unversioned,
    /// A fresh cache answers without contacting the gateway.
    Cached(Option<UpdateAvailability>),
    Fetch,
}

#[must_use]
pub fn plan_update_check(
    force: bool,
    cache: Option<&UpdateCache>,
    current_version: &str,
    now: i64,
) -> CachePlan {
    let Some(current) = Version::parse_notifier(current_version) else {
        return CachePlan::Unversioned;
    };
    if force {
        return CachePlan::Fetch;
    }
    match cache {
        Some(cache) if cache.is_fresh(now) => CachePlan::Cached(cached_update(cache, &current)),
        _ => CachePlan::Fetch,
    }
}

/// Reference `_get_cached_update_if_any`.
fn cached_update(cache: &UpdateCache, current: &Version) -> Option<UpdateAvailability> {
    let latest = Version::parse_notifier(&cache.latest_version)?;
    if latest <= *current {
        return None;
    }
    Some(UpdateAvailability {
        latest_version: cache.latest_version.clone(),
        should_notify: false,
    })
}

/// Reference `get_update_if_available` after the gateway call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResolution {
    /// The cache entry the reference persists, if any.
    pub cache_write: Option<UpdateCache>,
    pub availability: Option<UpdateAvailability>,
    pub error: Option<UpdateError>,
}

#[must_use]
pub fn resolve_fetch(
    result: Result<Option<String>, UpdateGatewayError>,
    cache: Option<&UpdateCache>,
    current_version: &str,
    now: i64,
) -> FetchResolution {
    let Some(current) = Version::parse_notifier(current_version) else {
        return FetchResolution {
            cache_write: None,
            availability: None,
            error: None,
        };
    };
    let refreshed = |version: &str| Some(write_cache(cache, version, now));
    match result {
        Err(error) => FetchResolution {
            cache_write: refreshed(current_version),
            availability: None,
            error: Some(UpdateError::Gateway(error.user_message())),
        },
        Ok(None) => FetchResolution {
            cache_write: refreshed(current_version),
            availability: None,
            error: None,
        },
        Ok(Some(latest_version)) => {
            let Some(latest) = Version::parse_notifier(&latest_version) else {
                // The reference returns before touching the cache when the
                // gateway reports a version it cannot parse.
                return FetchResolution {
                    cache_write: None,
                    availability: None,
                    error: None,
                };
            };
            if latest <= current {
                return FetchResolution {
                    cache_write: refreshed(current_version),
                    availability: None,
                    error: None,
                };
            }
            FetchResolution {
                cache_write: refreshed(&latest_version),
                availability: Some(UpdateAvailability {
                    latest_version,
                    should_notify: true,
                }),
                error: None,
            }
        }
    }
}

/// Reference `_write_update_cache`: only the version and timestamp change.
fn write_cache(cache: Option<&UpdateCache>, version: &str, now: i64) -> UpdateCache {
    match cache {
        Some(previous) => UpdateCache {
            latest_version: version.to_owned(),
            stored_at_timestamp: now,
            seen_whats_new_version: previous.seen_whats_new_version.clone(),
            dismissed_version: previous.dismissed_version.clone(),
        },
        None => UpdateCache::new(version, now),
    }
}

/// Reference `get_pending_update_from_cache`.
#[must_use]
pub fn pending_update_from_cache(
    cache: Option<&UpdateCache>,
    current_version: &str,
) -> Option<String> {
    let current = Version::parse_notifier(current_version)?;
    let cache = cache?;
    let latest = Version::parse_notifier(&cache.latest_version)?;
    if latest <= current {
        return None;
    }
    if cache.dismissed_version.as_deref() == Some(cache.latest_version.as_str()) {
        return None;
    }
    Some(cache.latest_version.clone())
}

/// Reference `should_show_whats_new`.
#[must_use]
pub fn should_show_whats_new(cache: Option<&UpdateCache>, current_version: &str) -> bool {
    cache.is_some_and(|cache| cache.seen_whats_new_version.as_deref() != Some(current_version))
}

/// Reference `mark_update_as_dismissed`: a missing cache stays missing.
#[must_use]
pub fn dismiss_update(cache: Option<&UpdateCache>, version: &str) -> Option<UpdateCache> {
    let mut cache = cache?.clone();
    cache.dismissed_version = Some(version.to_owned());
    Some(cache)
}

/// Reference `mark_version_as_seen`.
#[must_use]
pub fn mark_version_as_seen(cache: Option<&UpdateCache>, version: &str, now: i64) -> UpdateCache {
    match cache {
        Some(cache) => UpdateCache {
            seen_whats_new_version: Some(version.to_owned()),
            ..cache.clone()
        },
        None => UpdateCache {
            latest_version: version.to_owned(),
            stored_at_timestamp: now,
            seen_whats_new_version: Some(version.to_owned()),
            dismissed_version: None,
        },
    }
}

/// Reference `get_update_if_available`, including its cache writes.
///
/// The store is read when the reference reads its repository: before the
/// gateway only when the check is not forced, and otherwise only once the
/// answer calls for a write, which is where `_write_update_cache` asks for the
/// previous entry.
pub async fn get_update_if_available(
    gateway: &dyn UpdateGateway,
    store: &UpdateCacheStore,
    current_version: &str,
    now: i64,
    force: bool,
) -> Result<Option<UpdateAvailability>, UpdateError> {
    if Version::parse_notifier(current_version).is_none() {
        return Ok(None);
    }
    let cache = if force { None } else { store.load() };
    match plan_update_check(force, cache.as_ref(), current_version, now) {
        CachePlan::Unversioned => return Ok(None),
        CachePlan::Cached(availability) => return Ok(availability),
        CachePlan::Fetch => {}
    }
    let resolution = resolve_fetch(gateway.fetch_update().await, None, current_version, now);
    if let Some(write) = &resolution.cache_write {
        let previous = store.load();
        store.store(&write_cache(previous.as_ref(), &write.latest_version, now));
    }
    match resolution.error {
        Some(error) => Err(error),
        None => Ok(resolution.availability),
    }
}

#[cfg(test)]
mod update_tests;
