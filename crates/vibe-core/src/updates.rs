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
use crate::child::{ChildGroup, Rung};

/// Reference `UPDATE_CACHE_TTL_SECONDS`.
pub const UPDATE_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;

const CACHE_SECTION: &str = "update_cache";
/// Reference `FileSystemUpdateCacheRepository._legacy_json`.
const LEGACY_CACHE_FILE: &str = "update_cache.json";
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
/// A sibling `update_cache.json` written by the pre-TOML layout is read once and
/// migrated into the section.
#[derive(Debug, Clone)]
pub struct UpdateCacheStore {
    path: PathBuf,
    legacy_path: PathBuf,
}

impl UpdateCacheStore {
    #[must_use]
    pub fn new(vibe_home: &Path) -> Self {
        Self {
            path: vibe_home.join("cache.toml"),
            legacy_path: vibe_home.join(LEGACY_CACHE_FILE),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pre-TOML file [`Self::load`] migrates from, named by the reference's
    /// `_legacy_json`.
    #[must_use]
    pub fn legacy_path(&self) -> &Path {
        &self.legacy_path
    }

    /// Reference `_read_section` followed by `_parse`.
    ///
    /// The reference tests the section for truthiness, so a missing section, an
    /// empty one, and a section that is not a table all fall through to the
    /// legacy file, while a populated section answers on its own even when its
    /// contents do not parse.
    #[must_use]
    pub fn load(&self) -> Option<UpdateCache> {
        let section = self.read_document().and_then(|document| {
            document
                .get(CACHE_SECTION)
                .and_then(toml::Value::as_table)
                .cloned()
        });
        match section {
            Some(section) if !section.is_empty() => Self::parse_section(&section),
            _ => self.migrate_legacy(),
        }
    }

    /// Reference `set`: the cache becomes the payload the section merge applies,
    /// with an unset optional key omitted rather than written as empty.
    pub fn store(&self, cache: &UpdateCache) -> Result<(), UpdateError> {
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
        self.write_section(payload)
    }

    /// Reference `FileSystemCacheStore.write_section`, which updates the section
    /// in place: a key this port does not model survives the write, an optional
    /// key the payload omits keeps whatever the file held, and only a section
    /// that is not a table is replaced outright.
    fn write_section(
        &self,
        payload: toml::map::Map<String, toml::Value>,
    ) -> Result<(), UpdateError> {
        let mut document = self.read_document().unwrap_or_default();
        let mut section = match document.remove(CACHE_SECTION) {
            Some(toml::Value::Table(section)) => section,
            _ => toml::map::Map::new(),
        };
        section.extend(payload);
        document.insert(CACHE_SECTION.to_owned(), toml::Value::Table(section));
        let encoded = toml::to_string_pretty(&toml::Value::Table(document))
            .map_err(|_| UpdateError::CacheWrite)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|_| UpdateError::CacheWrite)?;
        }
        write_atomically(&self.path, "cache.toml", encoded.as_bytes())
            .map_err(|_| UpdateError::CacheWrite)
    }

    /// Reference `_read_section`'s fallback: the pre-TOML JSON is read once, its
    /// non-null keys are merged into the section, and the values reach the
    /// caller whether or not that write succeeded, because the reference's
    /// `write_section` logs and swallows its own failure.
    fn migrate_legacy(&self) -> Option<UpdateCache> {
        let text = read_bounded(&self.legacy_path)?;
        let legacy: serde_json::Value = serde_json::from_str(&text).ok()?;
        // The reference hands whatever the file held to `_parse` and raises an
        // attribute error on anything but an object. This port reads a non-object
        // as no cache, which is the absence every caller already handles.
        let legacy = legacy.as_object()?;
        let payload = legacy
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), legacy_value(value)?)))
            .collect();
        let _ = self.write_section(payload);
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

    fn read_document(&self) -> Option<toml::Table> {
        toml::from_str(&read_bounded(&self.path)?).ok()
    }
}

/// Reads one cache file, treating anything unreadable as absent.
///
/// The reference reads both files with no ceiling. This port refuses one past
/// [`MAX_CACHE_BYTES`] so a file something else appended to cannot be parsed
/// into memory, which reads as absent exactly like a corrupt one.
fn read_bounded(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_CACHE_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
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

/// Reference `GitHubUpdateGateway`: the releases of one repository, newest
/// published first.
pub struct GitHubUpdateGateway {
    owner: String,
    repository: String,
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

/// The `User-Agent` the reference sends under its own name. GitHub answers an
/// unidentified client with a 403, so this port names itself instead.
const GITHUB_USER_AGENT: &str = concat!(
    "mistral-vibe-rs-update-notifier/",
    env!("CARGO_PKG_VERSION")
);

/// The header GitHub spends down on every answer, error responses included.
const RATE_LIMIT_REMAINING_HEADER: &str = "X-RateLimit-Remaining";

impl GitHubUpdateGateway {
    /// The host the reference reads, and this port's default.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.github.com";

    /// The sentence this port publishes for `NotFound`. `NOTICE` forbids
    /// shipping the reference's own prose, so this states the same cause and
    /// the same next action in this port's words, and
    /// `the_not_found_sentence_is_this_port_s_own` holds the two unequal.
    const NOT_FOUND_MESSAGE: &'static str = "The releases of this repository could not be read. Export a GITHUB_TOKEN that can read \
         them, then check again.";

    pub fn with_base_url(
        owner: impl Into<String>,
        repository: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, UpdateGatewayError> {
        Self::with_timeout(owner, repository, base_url, GATEWAY_TIMEOUT)
    }

    fn with_timeout(
        owner: impl Into<String>,
        repository: impl Into<String>,
        base_url: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, UpdateGatewayError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::RequestFailed))?;
        Ok(Self {
            owner: owner.into(),
            repository: repository.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: None,
            client,
        })
    }

    /// Reference `GitHubUpdateGateway.__init__`: the token is optional, and the
    /// reference's falsy check makes an empty one no token at all.
    #[must_use]
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|value| !value.trim().is_empty());
        self
    }

    #[must_use]
    pub fn releases_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/releases",
            self.base_url, self.owner, self.repository
        )
    }
}

impl UpdateGateway for GitHubUpdateGateway {
    fn fetch_update(&self) -> UpdateFetch<'_> {
        Box::pin(async move {
            let mut request = self
                .client
                .get(self.releases_url())
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", GITHUB_USER_AGENT);
            if let Some(token) = &self.token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let response = request
                .send()
                .await
                .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::RequestFailed))?;
            let status = response.status().as_u16();
            let remaining = response
                .headers()
                .get(RATE_LIMIT_REMAINING_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if let Some(cause) = github_status_cause(status, remaining.as_deref()) {
                return Err(UpdateGatewayError {
                    cause,
                    message: match cause {
                        UpdateGatewayCause::NotFound => Some(Self::NOT_FOUND_MESSAGE.to_owned()),
                        _ => None,
                    },
                });
            }
            let body = response
                .text()
                .await
                .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::InvalidResponse))?;
            let payload: serde_json::Value = serde_json::from_str(&body)
                .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::InvalidResponse))?;
            Ok(select_latest_release(&payload))
        })
    }
}

/// Reference `GitHubUpdateGateway.fetch_update` response branches, in the order
/// it takes them: the rate limit is read from the header on any status, so an
/// exhausted budget reports the limit rather than the status it arrived with.
#[must_use]
fn github_status_cause(
    status: u16,
    rate_limit_remaining: Option<&str>,
) -> Option<UpdateGatewayCause> {
    if status == 429 || rate_limit_remaining.is_some_and(|value| value == "0") {
        return Some(UpdateGatewayCause::TooManyRequests);
    }
    match status {
        403 => Some(UpdateGatewayCause::Forbidden),
        404 => Some(UpdateGatewayCause::NotFound),
        status if (400..600).contains(&status) => Some(UpdateGatewayCause::ErrorResponse),
        _ => None,
    }
}

/// Reference `GitHubUpdateGateway.fetch_update` selection: the most recently
/// published release that is neither a draft nor a prerelease.
///
/// A payload that is not a list of releases reads as no update rather than as
/// an error, which is where this port stops short of the reference: the
/// reference reaches an unhandled attribute error there.
#[must_use]
fn select_latest_release(payload: &serde_json::Value) -> Option<String> {
    let mut releases = payload
        .as_array()?
        .iter()
        .filter(|release| !flag(release, "prerelease") && !flag(release, "draft"))
        .map(|release| {
            (
                release
                    .get("published_at")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                release,
            )
        })
        .collect::<Vec<_>>();
    // Python's `sorted` is stable, so releases published at the same instant
    // keep the order the API returned them in.
    releases.sort_by(|left, right| right.0.cmp(left.0));
    releases.into_iter().find_map(|(_, release)| {
        release
            .get("tag_name")
            .and_then(serde_json::Value::as_str)
            .and_then(extract_release_version)
    })
}

fn flag(release: &serde_json::Value, key: &str) -> bool {
    release.get(key).and_then(serde_json::Value::as_bool) == Some(true)
}

/// Reference `GitHubUpdateGateway._extract_version`: one optional `v` prefix
/// around a trimmed tag, and an empty tag is no version.
#[must_use]
fn extract_release_version(tag_name: &str) -> Option<String> {
    let tag = tag_name.trim();
    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    (!version.is_empty()).then(|| version.to_owned())
}

/// Reference `_terminate`: a cancelled command has two seconds to answer the
/// termination signal before it is killed.
const UPGRADE_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Reference `do_update` once every command has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// At least one command exited zero.
    Succeeded,
    /// Every command failed, or the list named none.
    Failed,
    /// The operator cancelled, so the remaining commands never ran.
    Cancelled,
}

/// The shell the reference's `create_subprocess_shell` uses on each platform.
#[cfg(windows)]
const UPGRADE_SHELL: (&str, &str) = ("cmd", "/C");
#[cfg(not(windows))]
const UPGRADE_SHELL: (&str, &str) = ("sh", "-c");

/// Reference `do_update`: every command runs through the platform shell, and
/// one command exiting zero is enough for the upgrade to have succeeded.
///
/// `cancelled` stands for the reference's `CancelledError`: the running command
/// is terminated, killed [`UPGRADE_TERMINATE_GRACE`] later if it is still
/// alive, and the commands after it never start. Output is captured and never
/// rendered, as the reference captures it.
pub async fn run_upgrade_commands<C>(commands: &[String], cancelled: C) -> UpgradeOutcome
where
    C: Future<Output = ()>,
{
    let mut any_succeeded = false;
    let mut cancelled = std::pin::pin!(cancelled);
    for command in commands {
        let mut builder = tokio::process::Command::new(UPGRADE_SHELL.0);
        builder.arg(UPGRADE_SHELL.1).arg(command);
        let Ok((mut child, _pipes)) = ChildGroup::spawn(&mut builder) else {
            // A command the shell cannot start is a command that did not exit
            // zero, which is the reference's `returncode != 0`.
            continue;
        };
        let exit = tokio::select! {
            exit = child.wait() => exit,
            () = &mut cancelled => {
                let _ = child.shut_down(UPGRADE_TERMINATE_GRACE, Rung::Terminate).await;
                return UpgradeOutcome::Cancelled;
            }
        };
        any_succeeded |= exit.is_ok_and(|exit| exit.success);
    }
    if any_succeeded {
        UpgradeOutcome::Succeeded
    } else {
        UpgradeOutcome::Failed
    }
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

    /// A one-shot HTTP server on loopback. Update discovery must never leave
    /// the machine during tests, and the returned string is the request the
    /// gateway actually sent.
    fn serve(response: String) -> (String, std::thread::JoinHandle<String>) {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the fixture accepts");
            let request = read_request(&mut stream);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            request
        });
        (format!("http://{address}"), handle)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read as _;

        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => request.push(byte[0]),
            }
        }
        String::from_utf8_lossy(&request).to_lowercase()
    }

    fn http_response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             {headers}\r\n{body}",
            body.len()
        )
    }

    fn github_gateway(base_url: &str) -> GitHubUpdateGateway {
        GitHubUpdateGateway::with_base_url("arthjean", "mistral-vibe-rs", base_url)
            .expect("gateway")
    }

    #[test]
    fn github_selection_takes_the_newest_published_release() {
        let payload = serde_json::json!([
            {"tag_name": "V2.24.0", "published_at": "2026-02-01T00:00:00Z"},
            {"tag_name": "v2.27.0", "published_at": "2026-05-01T00:00:00Z", "prerelease": true},
            {"tag_name": "v2.26.0", "published_at": "2026-04-01T00:00:00Z", "draft": true},
            {"tag_name": "v2.25.0", "published_at": "2026-03-01T00:00:00Z"},
        ]);
        assert_eq!(
            select_latest_release(&payload).as_deref(),
            Some("2.25.0"),
            "a draft and a prerelease are skipped even when published later"
        );
        // The newest release carrying no usable tag falls through to the next.
        let tagless = serde_json::json!([
            {"tag_name": "  ", "published_at": "2026-05-01T00:00:00Z"},
            {"tag_name": "v", "published_at": "2026-04-01T00:00:00Z"},
            {"tag_name": "2.24.0", "published_at": "2026-02-01T00:00:00Z"},
        ]);
        assert_eq!(select_latest_release(&tagless).as_deref(), Some("2.24.0"));
        // A release with no publication date sorts last, not first.
        let undated = serde_json::json!([
            {"tag_name": "v9.9.9"},
            {"tag_name": "v2.24.0", "published_at": "2026-02-01T00:00:00Z"},
        ]);
        assert_eq!(select_latest_release(&undated).as_deref(), Some("2.24.0"));
        // An empty list is no update rather than an error.
        assert_eq!(select_latest_release(&serde_json::json!([])), None);
        assert_eq!(
            select_latest_release(&serde_json::json!([
                {"tag_name": "v3.0.0", "draft": true}
            ])),
            None
        );
        assert_eq!(select_latest_release(&serde_json::json!({})), None);

        assert_eq!(
            extract_release_version(" v2.24.0 ").as_deref(),
            Some("2.24.0")
        );
        assert_eq!(
            extract_release_version("V2.24.0").as_deref(),
            Some("2.24.0")
        );
        assert_eq!(extract_release_version("2.24.0").as_deref(), Some("2.24.0"));
        assert_eq!(extract_release_version("v"), None);
        assert_eq!(extract_release_version("   "), None);
    }

    #[test]
    fn github_causes_follow_the_reference_branch_order() {
        assert_eq!(
            github_status_cause(429, None),
            Some(UpdateGatewayCause::TooManyRequests)
        );
        assert_eq!(
            github_status_cause(200, Some("0")),
            Some(UpdateGatewayCause::TooManyRequests),
            "the reference reads the budget header before the status, on any status"
        );
        assert_eq!(
            github_status_cause(403, Some("0")),
            Some(UpdateGatewayCause::TooManyRequests),
            "an exhausted budget reports the limit rather than the 403 it arrives as"
        );
        assert_eq!(
            github_status_cause(403, Some("57")),
            Some(UpdateGatewayCause::Forbidden)
        );
        assert_eq!(
            github_status_cause(404, None),
            Some(UpdateGatewayCause::NotFound)
        );
        assert_eq!(
            github_status_cause(500, None),
            Some(UpdateGatewayCause::ErrorResponse)
        );
        assert_eq!(
            github_status_cause(422, Some("57")),
            Some(UpdateGatewayCause::ErrorResponse)
        );
        assert_eq!(github_status_cause(200, Some("57")), None);
        assert_eq!(github_status_cause(304, None), None);
    }

    #[test]
    fn the_not_found_sentence_is_this_port_s_own() {
        use sha2::{Digest, Sha256};

        // `NOTICE`: the reference sentence is never committed as text. It is
        // recorded as a length plus a SHA-256 measured from
        // `vibe/cli/update_notifier/adapters/github_update_gateway.py` at
        // `crate::parity::REFERENCE_COMMIT`, so this port's own sentence can be
        // held permanently unequal to it.
        const REFERENCE_LENGTH: usize = 88;
        const REFERENCE_DIGEST: &str =
            "db0a0fb435e9d4b4c3100840ef7749c9810fc581102d3e67de0f9ab084e9f630";

        let sentence = GitHubUpdateGateway::NOT_FOUND_MESSAGE;
        assert!(
            !sentence.is_empty(),
            "the not-found cause must still name a next action"
        );
        let digest: String = Sha256::digest(sentence.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(
            sentence.len() != REFERENCE_LENGTH || digest != REFERENCE_DIGEST,
            "the not-found sentence became the reference's own; write an original one"
        );
        assert_ne!(digest, REFERENCE_DIGEST);
    }

    #[tokio::test]
    async fn the_github_gateway_asks_for_the_repository_releases_and_selects_one() {
        let body = serde_json::json!([
            {"tag_name": "v2.24.0", "published_at": "2026-02-01T00:00:00Z"},
            {"tag_name": "v2.25.0", "published_at": "2026-03-01T00:00:00Z"},
        ])
        .to_string();
        let (base_url, served) = serve(http_response("200 OK", "", &body));
        let gateway = github_gateway(&base_url).with_token(Some("token-value".to_owned()));
        assert_eq!(
            gateway.releases_url(),
            format!("{base_url}/repos/arthjean/mistral-vibe-rs/releases")
        );
        assert_eq!(
            gateway.fetch_update().await.expect("fetch"),
            Some("2.25.0".to_owned())
        );
        let request = served.join().expect("fixture thread");
        assert!(
            request.starts_with("get /repos/arthjean/mistral-vibe-rs/releases "),
            "the gateway asked for {request}"
        );
        assert!(request.contains("accept: application/vnd.github+json"));
        assert!(request.contains("user-agent: mistral-vibe-rs-update-notifier/"));
        assert!(request.contains("authorization: bearer token-value"));

        // An empty token is no token, which is what keeps an unauthenticated
        // check anonymous rather than sending `Bearer `.
        let (base_url, served) = serve(http_response("200 OK", "", "[]"));
        let anonymous = github_gateway(&base_url).with_token(Some(String::new()));
        assert_eq!(anonymous.fetch_update().await.expect("fetch"), None);
        assert!(
            !served
                .join()
                .expect("fixture thread")
                .contains("authorization:")
        );
    }

    #[tokio::test]
    async fn the_github_gateway_maps_every_failure_the_reference_maps() {
        let cases = [
            ("404 Not Found", "", "{}", UpdateGatewayCause::NotFound),
            ("403 Forbidden", "", "{}", UpdateGatewayCause::Forbidden),
            (
                "429 Too Many Requests",
                "",
                "{}",
                UpdateGatewayCause::TooManyRequests,
            ),
            (
                "200 OK",
                "x-ratelimit-remaining: 0\r\n",
                "[]",
                UpdateGatewayCause::TooManyRequests,
            ),
            (
                "500 Internal Server Error",
                "",
                "{}",
                UpdateGatewayCause::ErrorResponse,
            ),
            (
                "200 OK",
                "",
                "not json at all",
                UpdateGatewayCause::InvalidResponse,
            ),
        ];
        for (status, headers, body, expected) in cases {
            let (base_url, served) = serve(http_response(status, headers, body));
            let error = github_gateway(&base_url)
                .fetch_update()
                .await
                .expect_err("the gateway reports a cause");
            assert_eq!(error.cause, expected, "status {status} mapped wrongly");
            if expected == UpdateGatewayCause::NotFound {
                assert_eq!(error.user_message(), GitHubUpdateGateway::NOT_FOUND_MESSAGE);
            } else {
                assert_eq!(error.user_message(), expected.default_message());
            }
            let _ = served.join();
        }

        // A refused connection is a transport failure, not a status.
        let refused =
            GitHubUpdateGateway::with_base_url("arthjean", "mistral-vibe-rs", "http://127.0.0.1:9")
                .expect("gateway");
        assert_eq!(
            refused.fetch_update().await.expect_err("no listener").cause,
            UpdateGatewayCause::RequestFailed
        );
    }

    #[tokio::test]
    async fn a_silent_server_fails_the_check_within_the_gateway_timeout() {
        assert_eq!(
            GATEWAY_TIMEOUT,
            Duration::from_secs(5),
            "the reference gives the gateway five seconds"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let address = listener.local_addr().expect("listener address").to_string();
        let silent = std::thread::spawn(move || {
            let accepted = listener.accept();
            std::thread::sleep(Duration::from_millis(500));
            drop(accepted);
        });
        let gateway = GitHubUpdateGateway::with_timeout(
            "arthjean",
            "mistral-vibe-rs",
            format!("http://{address}"),
            Duration::from_millis(100),
        )
        .expect("gateway");
        let started = std::time::Instant::now();
        let error = gateway
            .fetch_update()
            .await
            .expect_err("a server that never answers");
        assert_eq!(error.cause, UpdateGatewayCause::RequestFailed);
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the check returned the timeout rather than waiting for the server"
        );
        let _ = silent.join();
    }

    fn commands(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|command| (*command).to_owned()).collect()
    }

    #[tokio::test]
    async fn one_upgrade_command_exiting_zero_carries_the_whole_upgrade() {
        assert_eq!(
            run_upgrade_commands(&commands(&["exit 1", "exit 0"]), std::future::pending()).await,
            UpgradeOutcome::Succeeded,
            "the reference's any-succeeded rule ignores the commands that failed"
        );
        assert_eq!(
            run_upgrade_commands(&commands(&["exit 1", "exit 3"]), std::future::pending()).await,
            UpgradeOutcome::Failed
        );
        assert_eq!(
            run_upgrade_commands(
                &commands(&["definitely-not-an-installed-command"]),
                std::future::pending()
            )
            .await,
            UpgradeOutcome::Failed,
            "a command the shell cannot run is a command that did not exit zero"
        );
        assert_eq!(
            run_upgrade_commands(&[], std::future::pending()).await,
            UpgradeOutcome::Failed
        );
    }

    #[tokio::test]
    async fn cancelling_an_upgrade_terminates_the_running_command() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = sender.send(());
        });
        let sleeper = if cfg!(windows) {
            "ping -n 60 127.0.0.1 > NUL"
        } else {
            "sleep 60"
        };
        let started = std::time::Instant::now();
        let outcome = run_upgrade_commands(&commands(&[sleeper, "exit 0"]), async {
            let _ = receiver.await;
        })
        .await;
        assert_eq!(
            outcome,
            UpgradeOutcome::Cancelled,
            "cancellation stops the upgrade instead of running the commands after it"
        );
        assert!(
            started.elapsed() < UPGRADE_TERMINATE_GRACE,
            "a command that answers termination never reaches the kill rung"
        );
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_command_that_ignores_termination_is_killed_after_the_grace() {
        // Time is paused, so the two-second grace elapses as soon as nothing
        // else can run: what this measures is the escalation, not the wait.
        let outcome = run_upgrade_commands(
            &commands(&["trap '' TERM; sleep 60"]),
            std::future::ready(()),
        )
        .await;
        assert_eq!(outcome, UpgradeOutcome::Cancelled);
    }

    /// Reference `_read_section`'s migration, whose null filter is what keeps an
    /// unset optional key out of the file it writes.
    #[test]
    fn a_legacy_json_cache_migrates_into_the_section_without_its_null_keys() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        fs::write(
            store.legacy_path(),
            r#"{"latest_version": "2.24.0", "stored_at_timestamp": 42,
                "seen_whats_new_version": null, "dismissed_version": "2.24.0"}"#,
        )
        .expect("legacy cache");
        assert_eq!(
            store.load(),
            Some(UpdateCache {
                latest_version: "2.24.0".to_owned(),
                stored_at_timestamp: 42,
                seen_whats_new_version: None,
                dismissed_version: Some("2.24.0".to_owned()),
            })
        );
        let migrated = fs::read_to_string(store.path()).expect("migrated cache");
        assert!(migrated.contains("dismissed_version"));
        assert!(
            !migrated.contains("seen_whats_new_version"),
            "a null legacy key is never written into the section"
        );
        // The section now answers on its own, which is what makes the migration
        // one-directional.
        fs::remove_file(store.legacy_path()).expect("drop the legacy file");
        assert_eq!(
            store.load().map(|cache| cache.latest_version),
            Some("2.24.0".to_owned())
        );
    }

    #[test]
    fn a_populated_section_answers_before_the_legacy_file_is_read() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        fs::write(
            store.path(),
            "[update_cache]\nlatest_version = \"1.0.0\"\nstored_at_timestamp = 1\n",
        )
        .expect("existing section");
        fs::write(
            store.legacy_path(),
            r#"{"latest_version": "2.24.0", "stored_at_timestamp": 42}"#,
        )
        .expect("legacy cache");
        assert_eq!(store.load(), Some(UpdateCache::new("1.0.0", 1)));
        assert!(
            !fs::read_to_string(store.path())
                .expect("cache text")
                .contains("2.24.0"),
            "the legacy file is never read once the section is populated"
        );
    }

    /// The reference reaches `_parse` with whatever the legacy file held and
    /// raises on anything but an object. This port answers absent instead, which
    /// is the one place its cache store is deliberately more defensive.
    #[test]
    fn a_legacy_file_that_is_not_an_object_reads_as_absent_and_writes_nothing() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        for legacy in ["[]", "\"2.24.0\"", "7", "{not json", "null"] {
            fs::write(store.legacy_path(), legacy).expect("legacy cache");
            assert_eq!(store.load(), None, "legacy payload {legacy}");
            assert!(
                !store.path().exists(),
                "legacy payload {legacy} must write nothing"
            );
        }
    }

    /// Reference `_read_section` returns the legacy payload it just tried to
    /// write, and `write_section` swallows its own failure, so a home nothing
    /// can be written into still answers with the state it holds.
    #[test]
    fn a_migration_whose_write_fails_still_returns_the_legacy_values() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        // A directory where the cache file belongs fails every write on every
        // platform, without depending on file permissions.
        fs::create_dir(store.path()).expect("block the cache file");
        fs::write(
            store.legacy_path(),
            r#"{"latest_version": "2.24.0", "stored_at_timestamp": 42}"#,
        )
        .expect("legacy cache");
        assert_eq!(store.load(), Some(UpdateCache::new("2.24.0", 42)));
        assert!(store.path().is_dir(), "the failed write staged nothing");
    }

    /// Reference `write_section`, which updates the section in place instead of
    /// rebuilding it, so nothing the payload omits is dropped.
    #[test]
    fn a_write_merges_into_the_section_and_keeps_every_key_it_does_not_carry() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        fs::write(
            store.path(),
            "[other]\nkept = true\n\n[update_cache]\ncustom = \"unmodeled\"\n\
             latest_version = \"1.0.0\"\nstored_at_timestamp = 1\n\
             dismissed_version = \"1.0.0\"\n",
        )
        .expect("existing cache");
        store
            .store(&UpdateCache::new("2.24.0", 42))
            .expect("cache write");
        let text = fs::read_to_string(store.path()).expect("cache text");
        assert!(text.contains("kept = true"), "a sibling table survives");
        assert!(
            text.contains("custom = \"unmodeled\""),
            "a key this port does not model survives"
        );
        assert_eq!(
            store.load(),
            Some(UpdateCache {
                latest_version: "2.24.0".to_owned(),
                stored_at_timestamp: 42,
                seen_whats_new_version: None,
                // The merge never removes a key, so a dismissal the payload
                // omits outlives the write.
                dismissed_version: Some("1.0.0".to_owned()),
            })
        );
    }

    /// Reference `write_section`'s `isinstance` guard: a section that is not a
    /// table is replaced rather than merged into.
    #[test]
    fn a_section_that_is_not_a_table_is_replaced_by_the_written_keys() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        fs::write(store.path(), "update_cache = 5\n\n[other]\nkept = true\n")
            .expect("existing cache");
        let cache = UpdateCache::new("2.24.0", 42);
        store.store(&cache).expect("cache write");
        assert_eq!(store.load(), Some(cache));
        assert!(
            fs::read_to_string(store.path())
                .expect("cache text")
                .contains("kept = true")
        );
    }

    /// The ceiling is this port's own: the reference reads the file whatever its
    /// size, so an oversized document is the one load the two answer differently.
    #[test]
    fn an_oversized_cache_reads_as_absent_and_the_next_write_produces_a_valid_file() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        let padding = "#".repeat(usize::try_from(MAX_CACHE_BYTES).expect("ceiling fits a usize"));
        fs::write(store.path(), format!("[other]\nkept = true\n{padding}\n"))
            .expect("oversized cache");
        assert_eq!(store.load(), None);
        let cache = UpdateCache::new("2.24.0", 42);
        store.store(&cache).expect("cache write");
        assert_eq!(store.load(), Some(cache), "the rewritten file is readable");
        assert!(
            !fs::read_to_string(store.path())
                .expect("cache text")
                .contains("kept = true"),
            "an unreadable document is replaced rather than appended to"
        );
    }

    /// The reference logs and swallows a failed cache write. This port reports
    /// it, which is what routes the failure into the update flow's own
    /// cache-write outcome instead of losing it.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_reports_the_error_and_leaves_the_previous_file_intact() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        let previous = UpdateCache::new("2.24.0", 42);
        store.store(&previous).expect("first cache write");

        let seal = |mode| {
            let mut permissions = fs::metadata(directory.path())
                .expect("vibe home metadata")
                .permissions();
            permissions.set_mode(mode);
            fs::set_permissions(directory.path(), permissions).expect("vibe home permissions");
        };
        seal(0o500);
        let ignores_permissions = fs::write(directory.path().join("probe"), b"").is_ok();
        let outcome = store.store(&UpdateCache::new("2.25.0", 43));
        // Restored before any assertion, so a failure still leaves a removable
        // temporary directory behind.
        seal(0o700);

        if ignores_permissions {
            // A process the mode bits do not bind cannot fail this write, so
            // only the sealed run proves anything.
            return;
        }
        assert_eq!(outcome, Err(UpdateError::CacheWrite));
        assert_eq!(
            store.load(),
            Some(previous),
            "a staged write that never renamed leaves the previous cache in place"
        );
    }

    /// The migration is reached from the same entry point the startup check and
    /// `--check-upgrade` both call, and its values answer before the gateway is
    /// contacted.
    #[tokio::test]
    async fn discovery_answers_from_a_migrated_legacy_cache_without_reaching_the_gateway() {
        let directory = tempfile::tempdir().expect("temporary vibe home");
        let store = UpdateCacheStore::new(directory.path());
        fs::write(
            store.legacy_path(),
            r#"{"latest_version": "2.24.0", "stored_at_timestamp": 100}"#,
        )
        .expect("legacy cache");
        let offline = StubGateway(Err(UpdateGatewayError::new(UpdateGatewayCause::Unknown)));
        let availability = get_update_if_available(&offline, &store, "2.23.1", 101, false)
            .await
            .expect("discovery");
        assert_eq!(
            availability,
            Some(UpdateAvailability {
                latest_version: "2.24.0".to_owned(),
                should_notify: false,
            })
        );
        assert!(
            fs::read_to_string(store.path())
                .expect("migrated cache")
                .contains("2.24.0"),
            "the migration wrote the section the next run reads"
        );
    }
}
