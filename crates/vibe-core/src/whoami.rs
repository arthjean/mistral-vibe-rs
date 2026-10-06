//! The caller's account, as the console's `/api/vibe/whoami` answers it, and
//! the plan label every telemetry census derives from it.
//!
//! The answer is user-scoped rather than session-scoped, so it is cached twice:
//! in memory for the process, single-flight per `(base URL, credential)` pair,
//! and on disk under the vibe home for six hours, keyed by the same anonymous
//! digest of the credential the rollout buckets on. A session that starts
//! within that window reads the plan without a request. Only a success is ever
//! stored, so a failed read leaves the next caller free to retry, and a
//! refused credential drops what was stored for it.
//!
//! Reference: `vibe/setup/auth/whoami.py` at the pinned commit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::experiments::hash_api_key;
use crate::identity::IdentityFuture;
use crate::observability::{self, LogLevel};

#[cfg(test)]
mod whoami_tests;

#[cfg(test)]
pub(crate) mod recorder {
    //! An account endpoint for tests that never answers.

    use std::time::Duration;

    use super::{IdentityFuture, WhoAmIResolver, WhoAmIResult};

    /// Costs the plan fields and nothing else, which is what an unreachable
    /// account endpoint does.
    pub(crate) struct Unanswered;

    impl WhoAmIResolver for Unanswered {
        fn resolve<'a>(
            &'a self,
            _base_url: &'a str,
            _api_key: &'a str,
            _timeout: Option<Duration>,
        ) -> IdentityFuture<'a, Option<WhoAmIResult>> {
            Box::pin(std::future::ready(None))
        }
    }
}

/// The path the account is read from, under the console base URL.
///
/// Reference `_WHOAMI_PATH`.
pub const WHOAMI_PATH: &str = "/api/vibe/whoami";

/// The file the cross-session cache lives in, under the vibe home.
///
/// Reference `WHOAMI_CACHE_FILE`.
pub const WHOAMI_CACHE_FILE: &str = "whoami_cache.json";

/// How long a cached answer is served before it is fetched again.
///
/// Reference `_WHOAMI_CACHE_TTL_SECONDS`.
pub const WHOAMI_CACHE_TTL_SECONDS: i64 = 6 * 60 * 60;

/// The plan a census reports when no Mistral provider is configured at all.
///
/// Distinct from an absent plan on purpose: absence means a Mistral credential
/// exists and the lookup failed, which is a problem worth noticing, while this
/// value means there was nothing to look up.
///
/// Reference `NO_PLAN_DATA`.
pub const NO_PLAN_DATA: &str = "NO_PLAN_DATA";

/// The kind of plan the account is on. Reference `AccountPlanKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountPlanKind {
    Api,
    Chat,
    MistralCode,
}

impl AccountPlanKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Chat => "chat",
            Self::MistralCode => "mistral_code",
        }
    }

    /// The kind one value names exactly, or [`None`].
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        [Self::Api, Self::Chat, Self::MistralCode]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

impl Serialize for AccountPlanKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The plan type arrives in any case and with surrounding blanks, and anything
/// that is not a string naming a known kind refuses the whole answer.
fn plan_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<AccountPlanKind, D::Error> {
    let value = String::deserialize(deserializer)?;
    AccountPlanKind::from_value(&value.trim().to_lowercase())
        .ok_or_else(|| serde::de::Error::custom(format!("Unsupported plan_type: {value}")))
}

/// What the console answers.
///
/// The reference validates strictly while ignoring unknown fields: a declared
/// field has to carry its declared type, with no coercion, and anything else
/// is dropped. `serde` reads JSON the same way.
///
/// Reference `WhoAmIResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmIResult {
    #[serde(deserialize_with = "plan_kind")]
    pub plan_type: AccountPlanKind,
    pub plan_name: String,
    #[serde(default)]
    pub prompt_switching_to_pro_plan: bool,
    #[serde(default)]
    pub organization_kind: Option<String>,
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub vibe_base: Option<String>,
}

/// Why one read produced no account.
///
/// Reference `AccountGatewayUnauthorized` and `AccountGatewayUnavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhoAmIFailure {
    /// The console answered 401 or 403.
    Unauthorized,
    /// The request never completed, the status was not a success, or the body
    /// was not an account this build can read.
    Unavailable,
}

impl WhoAmIFailure {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unauthorized => "AccountGatewayUnauthorized",
            Self::Unavailable => "AccountGatewayUnavailable",
        }
    }
}

/// Where the account is read from. Reference `AccountGateway`.
pub trait WhoAmIGateway: Send + Sync {
    fn read<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Result<WhoAmIResult, WhoAmIFailure>>;
}

/// The URL one console base resolves to. Every trailing slash is stripped,
/// as the reference's `rstrip("/")` strips them.
#[must_use]
pub fn whoami_url(base_url: &str) -> String {
    format!("{}{WHOAMI_PATH}", base_url.trim_end_matches('/'))
}

/// The HTTP gateway. Reference `HttpAccountGateway`.
pub struct HttpWhoAmIGateway {
    client: reqwest::Client,
}

impl HttpWhoAmIGateway {
    /// The production gateway, or [`None`] when an HTTP client cannot be built,
    /// which a fail-open caller reads as one more way to get no account.
    #[must_use]
    pub fn production() -> Option<Self> {
        reqwest::Client::builder()
            .build()
            .ok()
            .map(|client| Self { client })
    }
}

impl WhoAmIGateway for HttpWhoAmIGateway {
    fn read<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Result<WhoAmIResult, WhoAmIFailure>> {
        Box::pin(async move {
            let mut request = self
                .client
                .get(whoami_url(base_url))
                .header("Authorization", format!("Bearer {api_key}"));
            if let Some(timeout) = timeout {
                request = request.timeout(timeout);
            }
            let response = request
                .send()
                .await
                .map_err(|_| WhoAmIFailure::Unavailable)?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return read_whoami_response(status, "");
            }
            let body = response
                .text()
                .await
                .map_err(|_| WhoAmIFailure::Unavailable)?;
            read_whoami_response(status, &body)
        })
    }
}

/// What one console answer reads as: 401 and 403 refuse the credential, any
/// other non-success status or a body that is not an account this build can
/// read leaves the console unavailable. Reference `HttpAccountGateway`.
pub fn read_whoami_response(status: u16, body: &str) -> Result<WhoAmIResult, WhoAmIFailure> {
    if status == 401 || status == 403 {
        return Err(WhoAmIFailure::Unauthorized);
    }
    if !(200..300).contains(&status) {
        return Err(WhoAmIFailure::Unavailable);
    }
    serde_json::from_str(body).map_err(|_| WhoAmIFailure::Unavailable)
}

/// The account, or [`None`] on any failure, reported once and dropped.
///
/// Reference `fetch_whoami`.
pub async fn fetch_whoami(
    gateway: &dyn WhoAmIGateway,
    base_url: &str,
    api_key: &str,
    timeout: Option<Duration>,
) -> Option<WhoAmIResult> {
    match gateway.read(base_url, api_key, timeout).await {
        Ok(result) => Some(result),
        Err(failure) => {
            observability::log(
                LogLevel::Info,
                &format!("Failed to fetch /whoami ({}), skipping", failure.label()),
            );
            None
        }
    }
}

// --------------------------------------------------------------------------
// The cross-session cache
// --------------------------------------------------------------------------

/// The cache file under one vibe home.
#[must_use]
pub fn whoami_cache_path(vibe_home: &Path) -> PathBuf {
    vibe_home.join(WHOAMI_CACHE_FILE)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn read_entries(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(entries) => Some(entries),
            _ => None,
        })
        .unwrap_or_default()
}

/// Writes the entries through a sibling file and a rename, so a reader never
/// sees half a document. Best effort: a failure leaves the previous file.
fn write_entries(path: &Path, entries: &Map<String, Value>) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let staged = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let Ok(text) = serde_json::to_string(entries) else {
        return;
    };
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&staged, text))
        .and_then(|()| std::fs::rename(&staged, path));
    if written.is_err() {
        drop(std::fs::remove_file(&staged));
    }
}

/// The cached account for one credential, while it is fresh.
///
/// Reference `load_cached_whoami`: any read or parse error, a missing entry and
/// a stale one all answer [`None`], so the caller fetches.
#[must_use]
pub fn load_cached_whoami(path: &Path, api_key: &str) -> Option<WhoAmIResult> {
    let entries = read_entries(path);
    let entry = entries.get(&hash_api_key(api_key))?.as_object()?;
    // The reference's `isinstance(stored_at, int)` admits an integer and
    // nothing else, a float included.
    let stored_at = entry.get("stored_at_timestamp")?.as_i64()?;
    let payload = entry.get("payload")?.as_object()?;
    if stored_at <= now_seconds() - WHOAMI_CACHE_TTL_SECONDS {
        return None;
    }
    serde_json::from_value(Value::Object(payload.clone())).ok()
}

/// Records one account under the credential's digest. Reference
/// `store_cached_whoami`.
pub fn store_cached_whoami(path: &Path, api_key: &str, result: &WhoAmIResult) {
    let Ok(payload) = serde_json::to_value(result) else {
        return;
    };
    let mut entries = read_entries(path);
    let mut entry = Map::new();
    entry.insert("stored_at_timestamp".to_owned(), Value::from(now_seconds()));
    entry.insert("payload".to_owned(), payload);
    entries.insert(hash_api_key(api_key), Value::Object(entry));
    write_entries(path, &entries);
}

/// Drops what was cached for one credential. Reference `clear_cached_whoami`.
pub fn clear_cached_whoami(path: &Path, api_key: &str) {
    let mut entries = read_entries(path);
    if entries.remove(&hash_api_key(api_key)).is_some() {
        write_entries(path, &entries);
    }
}

/// How a caller that needs the account asks for one, which is the seam the
/// session tests drive the plan through.
pub trait WhoAmIResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Option<WhoAmIResult>>;
}

/// The account, read through memory, then disk, then the network.
///
/// The lock is held across the fetch, so concurrent callers coalesce onto one
/// request; a success is stored in both caches and a failure in neither.
///
/// Reference `WhoAmICache`.
pub struct WhoAmICache {
    gateway: Option<Arc<dyn WhoAmIGateway>>,
    path: PathBuf,
    entries: tokio::sync::Mutex<BTreeMap<(String, String), WhoAmIResult>>,
}

impl WhoAmICache {
    /// A cache persisting under `vibe_home`, reading through `gateway`. A
    /// cache with no gateway answers from its caches alone.
    #[must_use]
    pub fn new(vibe_home: &Path, gateway: Option<Arc<dyn WhoAmIGateway>>) -> Self {
        Self {
            gateway,
            path: whoami_cache_path(vibe_home),
            entries: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// The production cache.
    #[must_use]
    pub fn production(vibe_home: &Path) -> Self {
        Self::new(
            vibe_home,
            HttpWhoAmIGateway::production()
                .map(|gateway| Arc::new(gateway) as Arc<dyn WhoAmIGateway>),
        )
    }

    /// An already-fetched account, without fetching. Reference `peek`.
    pub async fn peek(&self, base_url: &str, api_key: &str) -> Option<WhoAmIResult> {
        self.entries
            .lock()
            .await
            .get(&(base_url.to_owned(), api_key.to_owned()))
            .cloned()
    }

    /// Stores a known account without fetching. Reference `populate`.
    pub async fn populate(&self, base_url: &str, api_key: &str, result: WhoAmIResult) {
        self.entries
            .lock()
            .await
            .insert((base_url.to_owned(), api_key.to_owned()), result);
    }

    /// Forgets every entry for one credential, in memory and on disk.
    /// Reference `invalidate`.
    pub async fn invalidate(&self, api_key: &str) {
        self.entries
            .lock()
            .await
            .retain(|(_, key), _| key != api_key);
        clear_cached_whoami(&self.path, api_key);
    }
}

impl WhoAmIResolver for WhoAmICache {
    fn resolve<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Option<WhoAmIResult>> {
        Box::pin(async move {
            let key = (base_url.to_owned(), api_key.to_owned());
            let mut entries = self.entries.lock().await;
            if let Some(cached) = entries.get(&key) {
                return Some(cached.clone());
            }
            if let Some(cached) = load_cached_whoami(&self.path, api_key) {
                entries.insert(key, cached.clone());
                return Some(cached);
            }
            let gateway = self.gateway.as_ref()?;
            match gateway.read(base_url, api_key, timeout).await {
                Ok(result) => {
                    entries.insert(key, result.clone());
                    store_cached_whoami(&self.path, api_key, &result);
                    Some(result)
                }
                Err(failure) => {
                    observability::log(
                        LogLevel::Info,
                        &format!(
                            "Failed to fetch /whoami for cache ({}), skipping",
                            failure.label()
                        ),
                    );
                    None
                }
            }
        })
    }
}

// --------------------------------------------------------------------------
// The plan label
// --------------------------------------------------------------------------

/// The `user_plan` label one plan type and name map to.
///
/// [`NO_PLAN_DATA`] in either passes through; a missing type, an unknown one
/// and a name outside the known mapping answer [`None`]. An API plan with a
/// name is free when the name says so and pay-as-you-go otherwise, and one
/// without a name is unmapped rather than guessed.
///
/// Reference `resolve_user_plan`.
#[must_use]
pub fn resolve_user_plan(plan_type: Option<&str>, plan_name: Option<&str>) -> Option<String> {
    if plan_type == Some(NO_PLAN_DATA) || plan_name == Some(NO_PLAN_DATA) {
        return Some(NO_PLAN_DATA.to_owned());
    }
    let kind = AccountPlanKind::from_value(plan_type?)?;
    let name = crate::text::python_strip(plan_name.unwrap_or_default()).to_uppercase();
    let label = match kind {
        AccountPlanKind::Chat => match name.as_str() {
            "FREE" => "Free",
            "INDIVIDUAL" => "Pro",
            "EDU" => "Student",
            "TEAM" => "Team",
            _ => return None,
        },
        AccountPlanKind::MistralCode => match name.as_str() {
            "F" => "Free Codestral",
            "E" => "Code Enterprise",
            _ => return None,
        },
        AccountPlanKind::Api if name.is_empty() => return None,
        AccountPlanKind::Api if name.contains("FREE") => "Free API",
        AccountPlanKind::Api => "PAYG API",
    };
    Some(label.to_owned())
}

/// The label one account maps to, or [`None`] without one. Reference
/// `derive_user_plan`.
#[must_use]
pub fn derive_user_plan(result: Option<&WhoAmIResult>) -> Option<String> {
    let result = result?;
    resolve_user_plan(Some(result.plan_type.as_str()), Some(&result.plan_name))
}

// --------------------------------------------------------------------------
// Tenant domains
// --------------------------------------------------------------------------

/// How long tenant discovery waits on the console. The reference builds its
/// client without a timeout, so httpx's five-second default applies.
pub const TENANT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The tenant URL `/whoami` advertised, when it is safe to send traffic to.
///
/// Trust boundary: the answer decides where later API and chat traffic goes,
/// so anything that is not plainly an HTTPS origin is refused, and so is a
/// path that climbs with `..`. Trailing slashes are dropped from what is kept.
/// Reference `_sanitize_tenant_url`.
#[must_use]
pub fn sanitize_tenant_url(candidate: &str, field: &str) -> Option<String> {
    let stripped = candidate.trim_end_matches('/');
    let warn = |reason: &str| {
        observability::log(
            LogLevel::Warning,
            &format!("Rejecting tenant {field} URL: {reason} ({candidate:?})"),
        );
    };
    let Ok(parsed) = crate::pyurl::PyUrl::try_parse(stripped) else {
        warn("unparsable value");
        return None;
    };
    if parsed.scheme != "https" || parsed.netloc.is_empty() {
        warn("expected https origin");
        return None;
    }
    if parsed.path.contains("..") {
        warn("path contains '..'");
        return None;
    }
    Some(stripped.to_owned())
}

/// The provider entry and chat base after adopting what one `/whoami` answer
/// advertises: the API base as `{api_base}/v1` and the chat base as given,
/// each only when present, non-empty and safe. Without an answer, or with
/// one that advertises neither, both come back unchanged.
#[must_use]
pub fn adopt_tenant_domains(
    whoami: Option<&WhoAmIResult>,
    mut provider: toml::Table,
    current_vibe_base_url: &str,
) -> (toml::Table, String) {
    let mut vibe_base_url = current_vibe_base_url.to_owned();
    let Some(whoami) = whoami else {
        return (provider, vibe_base_url);
    };
    if let Some(api_base) = whoami.api_base.as_deref().filter(|value| !value.is_empty())
        && let Some(sanitized) = sanitize_tenant_url(api_base, "api")
    {
        provider.insert(
            "api_base".to_owned(),
            toml::Value::String(format!("{sanitized}/v1")),
        );
    }
    if let Some(vibe_base) = whoami
        .vibe_base
        .as_deref()
        .filter(|value| !value.is_empty())
        && let Some(sanitized) = sanitize_tenant_url(vibe_base, "vibe_base")
    {
        vibe_base_url = sanitized;
    }
    (provider, vibe_base_url)
}

/// Fetches `/whoami` from `console_base_url` and adopts the tenant domains it
/// advertises. Every failure degrades to the inputs unchanged rather than
/// blocking the sign-in. Reference `resolve_tenant_domains`.
pub async fn resolve_tenant_domains(
    gateway: &dyn WhoAmIGateway,
    provider: toml::Table,
    console_base_url: &str,
    api_key: &str,
    current_vibe_base_url: &str,
) -> (toml::Table, String) {
    let whoami = fetch_whoami(
        gateway,
        console_base_url,
        api_key,
        Some(TENANT_DISCOVERY_TIMEOUT),
    )
    .await;
    adopt_tenant_domains(whoami.as_ref(), provider, current_vibe_base_url)
}

/// The change-event reason an account read's configuration heal carries.
const RECONCILE_REASON: &str = "tenant-domain-reconcile";

/// Heals the configuration with the tenant hosts one `/whoami` answer
/// advertises: the Mistral provider named `provider_name` moves to
/// `{api_base}/v1` and the chat base to `vibe_base`, each written only when
/// it is safe and differs from what the configuration resolves now. Called
/// after every successful account read, so a failed discovery at sign-in or a
/// tenant that moved heals on the next read. A failed write is logged and
/// dropped. Reference `reconcile_tenant_domains` in
/// `vibe/app_server/_account.py`.
pub fn reconcile_tenant_domains(
    config: &crate::config::LayeredConfig,
    whoami: &WhoAmIResult,
    provider_name: &str,
) {
    if whoami.api_base.is_none() && whoami.vibe_base.is_none() {
        return;
    }
    let Ok(current) = config.load() else {
        return;
    };
    let report = |field: &str, result: Result<crate::config::ConfigSnapshot, _>| {
        if let Err(error) = result {
            observability::log(
                LogLevel::Error,
                &format!("Failed to persist {field} to config: {error}"),
            );
        }
    };
    if let Some(api_base) = whoami.api_base.as_deref().filter(|value| !value.is_empty())
        && let Some(sanitized) = sanitize_tenant_url(api_base, "api_base")
    {
        let desired = format!("{sanitized}/v1");
        let provider = current
            .effective
            .get("providers")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
            .find(|candidate| {
                candidate.get("name").and_then(toml::Value::as_str) == Some(provider_name)
                    && candidate.get("backend").and_then(toml::Value::as_str) == Some("mistral")
            });
        if let Some(provider) = provider
            && provider.get("api_base").and_then(toml::Value::as_str) != Some(desired.as_str())
        {
            let mut moved = provider.clone();
            moved.insert("api_base".to_owned(), toml::Value::String(desired));
            report(
                "provider",
                config.persist_provider(&moved, RECONCILE_REASON),
            );
        }
    }
    if let Some(vibe_base) = whoami
        .vibe_base
        .as_deref()
        .filter(|value| !value.is_empty())
        && let Some(sanitized) = sanitize_tenant_url(vibe_base, "vibe_base")
        && current
            .effective
            .get("vibe_base_url")
            .and_then(toml::Value::as_str)
            != Some(sanitized.as_str())
    {
        report(
            "vibe_base_url",
            config.persist_field(
                "vibe_base_url",
                toml::Value::String(sanitized),
                RECONCILE_REASON,
            ),
        );
    }
}
