//! Update discovery pinned to the reference `vibe.cli.update_notifier` contract.
//!
//! The decision logic is pure so both the forced `--check-upgrade` route and the
//! background startup check replay the same cache, freshness, dismissal, and
//! release-note rules the reference applies.

use std::cmp::Ordering;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::atomic_file::write_atomically;

/// Reference `UPDATE_CACHE_TTL_SECONDS`.
pub const UPDATE_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;

const CACHE_SECTION: &str = "update_cache";
const GATEWAY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CACHE_BYTES: u64 = 1024 * 1024;

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
    fn is_fresh(&self, now: i64) -> bool {
        self.stored_at_timestamp > now.saturating_sub(UPDATE_CACHE_TTL_SECONDS)
    }
}

/// Reference `UpdateAvailability`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAvailability {
    pub latest_version: String,
    pub should_notify: bool,
}

/// Reference `UpdateGatewayCause`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateGatewayCause {
    TooManyRequests,
    Forbidden,
    NotFound,
    RequestFailed,
    ErrorResponse,
    InvalidResponse,
    Unknown,
}

impl UpdateGatewayCause {
    /// Reference `DEFAULT_GATEWAY_MESSAGES`.
    #[must_use]
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::TooManyRequests => "Rate limit exceeded while checking for updates.",
            Self::Forbidden => "Request was forbidden while checking for updates.",
            Self::NotFound => "Unable to fetch the releases. Please check your permissions.",
            Self::RequestFailed => "Network error while checking for updates.",
            Self::ErrorResponse => "Unexpected response received while checking for updates.",
            Self::InvalidResponse => "Received an invalid response while checking for updates.",
            Self::Unknown => "Unable to determine whether an update is available.",
        }
    }
}

/// Reference `UpdateGatewayError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", self.user_message())]
pub struct UpdateGatewayError {
    pub cause: UpdateGatewayCause,
    pub message: Option<String>,
}

impl UpdateGatewayError {
    #[must_use]
    pub const fn new(cause: UpdateGatewayCause) -> Self {
        Self {
            cause,
            message: None,
        }
    }

    /// Reference `_describe_gateway_error`.
    #[must_use]
    pub fn user_message(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| self.cause.default_message().to_owned())
    }
}

/// Reference `UpdateError`, raised only where the reference raises it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpdateError {
    #[error("{0}")]
    Gateway(String),
    #[error("update cache could not be written")]
    CacheWrite,
}

pub type UpdateFetch<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, UpdateGatewayError>> + Send + 'a>>;

/// Reference `UpdateGateway`.
pub trait UpdateGateway: Send + Sync {
    fn fetch_update(&self) -> UpdateFetch<'_>;
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
    let Some(current) = Version::parse(current_version) else {
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
    let latest = Version::parse(&cache.latest_version)?;
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
    let Some(current) = Version::parse(current_version) else {
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
            let Some(latest) = Version::parse(&latest_version) else {
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
    let current = Version::parse(current_version)?;
    let cache = cache?;
    let latest = Version::parse(&cache.latest_version)?;
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

/// Reference `FileSystemUpdateCacheRepository`: a `[update_cache]` section of the
/// shared `cache.toml`, where any unreadable or malformed state reads as absent.
#[derive(Debug, Clone)]
pub struct UpdateCacheStore {
    path: PathBuf,
}

impl UpdateCacheStore {
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        Self {
            path: vibe_home.join("cache.toml"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn load(&self) -> Option<UpdateCache> {
        let table = self.read_document()?;
        let section = table.get(CACHE_SECTION)?.as_table()?;
        let latest_version = section.get("latest_version")?.as_str()?.to_owned();
        let stored_at_timestamp = section.get("stored_at_timestamp")?.as_integer()?;
        Some(UpdateCache {
            latest_version,
            stored_at_timestamp,
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

    pub fn store(&self, cache: &UpdateCache) -> Result<(), UpdateError> {
        let mut document = self.read_document().unwrap_or_default();
        let mut section = toml::map::Map::new();
        section.insert(
            "latest_version".to_owned(),
            toml::Value::String(cache.latest_version.clone()),
        );
        section.insert(
            "stored_at_timestamp".to_owned(),
            toml::Value::Integer(cache.stored_at_timestamp),
        );
        if let Some(seen) = &cache.seen_whats_new_version {
            section.insert(
                "seen_whats_new_version".to_owned(),
                toml::Value::String(seen.clone()),
            );
        }
        if let Some(dismissed) = &cache.dismissed_version {
            section.insert(
                "dismissed_version".to_owned(),
                toml::Value::String(dismissed.clone()),
            );
        }
        document.insert(CACHE_SECTION.to_owned(), toml::Value::Table(section));
        let encoded = toml::to_string_pretty(&toml::Value::Table(document))
            .map_err(|_| UpdateError::CacheWrite)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|_| UpdateError::CacheWrite)?;
        }
        write_atomically(&self.path, "cache.toml", encoded.as_bytes())
            .map_err(|_| UpdateError::CacheWrite)
    }

    fn read_document(&self) -> Option<toml::Table> {
        let metadata = fs::metadata(&self.path).ok()?;
        if metadata.len() > MAX_CACHE_BYTES {
            return None;
        }
        let text = fs::read_to_string(&self.path).ok()?;
        toml::from_str(&text).ok()
    }
}

/// Reference `get_update_if_available`, including its cache writes.
pub async fn get_update_if_available(
    gateway: &dyn UpdateGateway,
    store: &UpdateCacheStore,
    current_version: &str,
    now: i64,
    force: bool,
) -> Result<Option<UpdateAvailability>, UpdateError> {
    let cache = store.load();
    match plan_update_check(force, cache.as_ref(), current_version, now) {
        CachePlan::Unversioned => return Ok(None),
        CachePlan::Cached(availability) => return Ok(availability),
        CachePlan::Fetch => {}
    }
    let resolution = resolve_fetch(
        gateway.fetch_update().await,
        cache.as_ref(),
        current_version,
        now,
    );
    if let Some(write) = &resolution.cache_write {
        store.store(write)?;
    }
    match resolution.error {
        Some(error) => Err(error),
        None => Ok(resolution.availability),
    }
}

/// Reference `PyPIUpdateGateway`, including its non-yanked selection rule.
pub struct PyPiUpdateGateway {
    project: String,
    base_url: String,
    client: reqwest::Client,
}

impl PyPiUpdateGateway {
    pub fn new(project: impl Into<String>) -> Result<Self, UpdateGatewayError> {
        Self::with_base_url(project, "https://pypi.org")
    }

    pub fn with_base_url(
        project: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, UpdateGatewayError> {
        let client = reqwest::Client::builder()
            .timeout(GATEWAY_TIMEOUT)
            .build()
            .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::RequestFailed))?;
        Ok(Self {
            project: project.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
        })
    }
}

impl UpdateGateway for PyPiUpdateGateway {
    fn fetch_update(&self) -> UpdateFetch<'_> {
        Box::pin(async move {
            let url = format!("{}/simple/{}/", self.base_url, self.project);
            let response = self
                .client
                .get(url)
                .header("Accept", "application/vnd.pypi.simple.v1+json")
                .send()
                .await
                .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::RequestFailed))?;
            let status = response.status().as_u16();
            if let Some(cause) = status_cause(status) {
                return Err(UpdateGatewayError::new(cause));
            }
            let body = response
                .text()
                .await
                .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::InvalidResponse))?;
            let payload: serde_json::Value = serde_json::from_str(&body)
                .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::InvalidResponse))?;
            Ok(select_latest_version(&payload))
        })
    }
}

/// Reference `_STATUS_CAUSES` plus the generic error branch.
#[must_use]
pub fn status_cause(status: u16) -> Option<UpdateGatewayCause> {
    match status {
        404 => Some(UpdateGatewayCause::NotFound),
        403 => Some(UpdateGatewayCause::Forbidden),
        429 => Some(UpdateGatewayCause::TooManyRequests),
        status if (400..600).contains(&status) => Some(UpdateGatewayCause::ErrorResponse),
        _ => None,
    }
}

/// Reference `PyPIUpdateGateway.fetch_update`: the highest published version that
/// still has a non-yanked artifact.
#[must_use]
pub fn select_latest_version(payload: &serde_json::Value) -> Option<String> {
    let published = payload
        .get("files")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|file| file.get("yanked").and_then(serde_json::Value::as_bool) != Some(true))
        .filter_map(|file| file.get("filename").and_then(serde_json::Value::as_str))
        .filter_map(artifact_version)
        .collect::<Vec<_>>();
    let mut candidates = payload
        .get("versions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter_map(|raw| Version::parse(raw).map(|version| (version, raw.to_owned())))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    candidates
        .into_iter()
        .find(|(version, _)| published.contains(version))
        .map(|(_, raw)| raw)
}

/// Reference `_parse_filename_version` for wheels and source distributions.
fn artifact_version(filename: &str) -> Option<Version> {
    if let Some(stem) = filename.strip_suffix(".whl") {
        let mut parts = stem.split('-');
        parts.next()?;
        return parts.next().and_then(Version::parse);
    }
    let stem = filename
        .strip_suffix(".tar.gz")
        .or_else(|| filename.strip_suffix(".zip"))?;
    let (_, version) = stem.rsplit_once('-')?;
    Version::parse(version)
}

/// The PEP 440 subset the reference version comparison needs: release segments
/// with optional dev, pre-release, post-release, and local parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    release: Vec<u64>,
    dev: Option<u64>,
    pre: Option<(PreKind, u64)>,
    post: Option<u64>,
    local: Option<Vec<LocalSegment>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PreKind {
    Alpha,
    Beta,
    ReleaseCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalSegment {
    Text(String),
    Number(u64),
}

impl Version {
    /// Reference `_parse_version`, including its `-` to `+` normalization.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().replace('-', "+").to_ascii_lowercase();
        let normalized = normalized.strip_prefix('v').unwrap_or(&normalized);
        let (public, local) = match normalized.split_once('+') {
            Some((public, local)) => (public, Some(parse_local(local)?)),
            None => (normalized, None),
        };
        let mut rest = public;
        let mut dev = None;
        if let Some((head, tail)) = rest.split_once(".dev") {
            dev = Some(parse_number(tail)?);
            rest = head;
        }
        let mut post = None;
        if let Some((head, tail)) = rest.split_once(".post") {
            post = Some(parse_number(tail)?);
            rest = head;
        }
        let mut pre = None;
        for (marker, kind) in [
            ("rc", PreKind::ReleaseCandidate),
            ("a", PreKind::Alpha),
            ("b", PreKind::Beta),
        ] {
            if let Some((head, tail)) = rest.split_once(marker)
                && head.ends_with(|character: char| character.is_ascii_digit())
            {
                pre = Some((kind, parse_number(tail)?));
                rest = head;
                break;
            }
        }
        let mut release = rest
            .split('.')
            .map(parse_number)
            .collect::<Option<Vec<_>>>()?;
        if release.is_empty() {
            return None;
        }
        // PEP 440 pads shorter releases with zeros, so `2.23` and `2.23.0` must
        // compare and hash as the same version.
        while release.len() > 1 && release.last() == Some(&0) {
            release.pop();
        }
        Some(Self {
            release,
            dev,
            pre,
            post,
            local,
        })
    }

    fn release_at(&self, index: usize) -> u64 {
        self.release.get(index).copied().unwrap_or(0)
    }

    /// PEP 440 ordering of the pre/post/dev triple.
    fn stage(&self) -> (u8, u64, u64) {
        match (self.dev, self.pre, self.post) {
            (Some(dev), None, None) => (0, 0, dev),
            (_, Some((kind, number)), None) => (1, kind as u64, number),
            (_, None, Some(post)) => (3, post, 0),
            (_, Some((kind, number)), Some(_)) => (3, kind as u64, number),
            (None, None, None) => (2, 0, 0),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let width = self.release.len().max(other.release.len());
        for index in 0..width {
            match self.release_at(index).cmp(&other.release_at(index)) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        match self.stage().cmp(&other.stage()) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
        match (&self.local, &other.local) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(left), Some(right)) => compare_local(left, right),
        }
    }
}

fn compare_local(left: &[LocalSegment], right: &[LocalSegment]) -> Ordering {
    for index in 0..left.len().max(right.len()) {
        match (left.get(index), right.get(index)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(LocalSegment::Number(left)), Some(LocalSegment::Number(right))) => {
                match left.cmp(right) {
                    Ordering::Equal => {}
                    ordering => return ordering,
                }
            }
            (Some(LocalSegment::Text(left)), Some(LocalSegment::Text(right))) => {
                match left.cmp(right) {
                    Ordering::Equal => {}
                    ordering => return ordering,
                }
            }
            (Some(LocalSegment::Number(_)), Some(LocalSegment::Text(_))) => {
                return Ordering::Greater;
            }
            (Some(LocalSegment::Text(_)), Some(LocalSegment::Number(_))) => return Ordering::Less,
        }
    }
    Ordering::Equal
}

fn parse_local(raw: &str) -> Option<Vec<LocalSegment>> {
    if raw.is_empty() {
        return None;
    }
    raw.split(['.', '+'])
        .map(|segment| {
            if segment.is_empty() || !segment.chars().all(|c| c.is_ascii_alphanumeric()) {
                return None;
            }
            Some(match segment.parse::<u64>() {
                Ok(number) => LocalSegment::Number(number),
                Err(_) => LocalSegment::Text(segment.to_owned()),
            })
        })
        .collect()
}

fn parse_number(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    #[test]
    fn version_ordering_matches_the_reference_subset() {
        assert!(version("2.23.2") > version("2.23.1"));
        assert!(version("2.24") > version("2.23.9"));
        assert_eq!(version("2.23"), version("2.23.0"));
        assert!(version("2.23.1rc1") < version("2.23.1"));
        assert!(version("2.23.1.dev4") < version("2.23.1rc1"));
        assert!(version("2.23.1.post1") > version("2.23.1"));
        // `-` normalizes to a local segment, which outranks the bare release.
        assert!(version("2.23.1-dev") > version("2.23.1"));
        assert!(Version::parse("not-a-version").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn fresh_cache_answers_without_contacting_the_gateway() {
        let cache = UpdateCache::new("2.24.0", 1_000);
        assert_eq!(
            plan_update_check(false, Some(&cache), "2.23.1", 1_100),
            CachePlan::Cached(Some(UpdateAvailability {
                latest_version: "2.24.0".to_owned(),
                should_notify: false,
            }))
        );
        assert_eq!(
            plan_update_check(false, Some(&cache), "2.24.0", 1_100),
            CachePlan::Cached(None)
        );
        assert_eq!(
            plan_update_check(true, Some(&cache), "2.23.1", 1_100),
            CachePlan::Fetch
        );
        assert_eq!(
            plan_update_check(
                false,
                Some(&cache),
                "2.23.1",
                1_000 + UPDATE_CACHE_TTL_SECONDS + 1
            ),
            CachePlan::Fetch
        );
        assert_eq!(
            plan_update_check(false, None, "not-a-version", 0),
            CachePlan::Unversioned
        );
    }

    #[test]
    fn gateway_failure_refreshes_the_cache_without_a_new_version() {
        let previous = UpdateCache {
            latest_version: "2.24.0".to_owned(),
            stored_at_timestamp: 1,
            seen_whats_new_version: Some("2.23.1".to_owned()),
            dismissed_version: Some("2.24.0".to_owned()),
        };
        let resolution = resolve_fetch(
            Err(UpdateGatewayError::new(UpdateGatewayCause::RequestFailed)),
            Some(&previous),
            "2.23.1",
            99,
        );
        assert_eq!(
            resolution.error,
            Some(UpdateError::Gateway(
                "Network error while checking for updates.".to_owned()
            ))
        );
        assert_eq!(resolution.availability, None);
        let write = resolution.cache_write.expect("cache refresh");
        assert_eq!(write.latest_version, "2.23.1");
        assert_eq!(write.stored_at_timestamp, 99);
        assert_eq!(write.seen_whats_new_version.as_deref(), Some("2.23.1"));
        assert_eq!(write.dismissed_version.as_deref(), Some("2.24.0"));
    }

    #[test]
    fn malformed_gateway_version_never_becomes_authoritative() {
        let resolution = resolve_fetch(Ok(Some("banana".to_owned())), None, "2.23.1", 5);
        assert_eq!(resolution.cache_write, None);
        assert_eq!(resolution.availability, None);
        assert_eq!(resolution.error, None);
    }

    #[test]
    fn newer_gateway_version_notifies_and_persists() {
        let resolution = resolve_fetch(Ok(Some("2.24.0".to_owned())), None, "2.23.1", 5);
        assert_eq!(
            resolution.availability,
            Some(UpdateAvailability {
                latest_version: "2.24.0".to_owned(),
                should_notify: true,
            })
        );
        assert_eq!(resolution.cache_write, Some(UpdateCache::new("2.24.0", 5)));
    }

    #[test]
    fn dismissal_and_release_notes_follow_the_cache() {
        let cache = UpdateCache::new("2.24.0", 10);
        assert_eq!(
            pending_update_from_cache(Some(&cache), "2.23.1").as_deref(),
            Some("2.24.0")
        );
        let dismissed = dismiss_update(Some(&cache), "2.24.0").expect("dismissal");
        assert_eq!(pending_update_from_cache(Some(&dismissed), "2.23.1"), None);
        assert!(should_show_whats_new(Some(&cache), "2.23.1"));
        let seen = mark_version_as_seen(Some(&cache), "2.23.1", 11);
        assert!(!should_show_whats_new(Some(&seen), "2.23.1"));
        assert_eq!(seen.latest_version, "2.24.0");
        // Without a cache the reference shows nothing and dismisses nothing.
        assert!(!should_show_whats_new(None, "2.23.1"));
        assert_eq!(dismiss_update(None, "2.24.0"), None);
    }

    #[test]
    fn cache_store_survives_corruption_and_preserves_other_sections() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        assert_eq!(store.load(), None);
        fs::write(store.path(), "not = [toml").expect("corrupt cache");
        assert_eq!(store.load(), None);
        fs::write(store.path(), "[other]\nkept = true\n").expect("existing cache");
        let cache = UpdateCache {
            latest_version: "2.24.0".to_owned(),
            stored_at_timestamp: 42,
            seen_whats_new_version: Some("2.23.1".to_owned()),
            dismissed_version: None,
        };
        store.store(&cache).expect("cache write");
        assert_eq!(store.load(), Some(cache));
        let text = fs::read_to_string(store.path()).expect("cache text");
        assert!(text.contains("kept = true"));
    }

    #[test]
    fn pypi_selection_skips_yanked_and_unpublished_versions() {
        let payload = serde_json::json!({
            "versions": ["2.23.1", "2.24.0", "2.25.0", "nonsense"],
            "files": [
                {"filename": "mistral_vibe-2.23.1-py3-none-any.whl", "yanked": false},
                {"filename": "mistral_vibe-2.24.0-py3-none-any.whl", "yanked": true},
                {"filename": "mistral_vibe-2.24.0.tar.gz", "yanked": true}
            ]
        });
        assert_eq!(
            select_latest_version(&payload).as_deref(),
            Some("2.23.1"),
            "a yanked artifact and an unpublished version are both ignored"
        );
        assert_eq!(select_latest_version(&serde_json::json!({})), None);
    }

    struct StubGateway(Result<Option<String>, UpdateGatewayError>);

    impl UpdateGateway for StubGateway {
        fn fetch_update(&self) -> UpdateFetch<'_> {
            let result = self.0.clone();
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn discovery_persists_exactly_what_the_reference_persists() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        let gateway = StubGateway(Ok(Some("2.24.0".to_owned())));
        let availability = get_update_if_available(&gateway, &store, "2.23.1", 7, false)
            .await
            .expect("discovery");
        assert_eq!(
            availability,
            Some(UpdateAvailability {
                latest_version: "2.24.0".to_owned(),
                should_notify: true,
            })
        );
        assert_eq!(store.load(), Some(UpdateCache::new("2.24.0", 7)));

        // A fresh cache answers the next non-forced check without notifying.
        let offline = StubGateway(Err(UpdateGatewayError::new(UpdateGatewayCause::Unknown)));
        let cached = get_update_if_available(&offline, &store, "2.23.1", 8, false)
            .await
            .expect("cached discovery");
        assert_eq!(
            cached,
            Some(UpdateAvailability {
                latest_version: "2.24.0".to_owned(),
                should_notify: false,
            })
        );

        let error = get_update_if_available(&offline, &store, "2.23.1", 9, true)
            .await
            .expect_err("forced discovery reaches the gateway");
        assert_eq!(
            error,
            UpdateError::Gateway("Unable to determine whether an update is available.".to_owned())
        );
    }
}
