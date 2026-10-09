//! Probing an endpoint with a one-token request to learn which models it
//! answers for, and remembering the answer for each endpoint, model and key.
//!
//! Reference `vibe/core/llm/model_probe.py`. A verdict is kept in memory and
//! in `$VIBE_HOME/utility_model_cache.json`: a model found served stays so for
//! a week, one refused for an hour. A probe that reaches no verdict (a
//! timeout, a rate limit, a server error) is not asked again in this process
//! for ten minutes, and after finding no entry on disk it waits a minute
//! before reading the file again, since another process may write it.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::error::BackendFailure;
use super::retry::{Clock, NoRetryObserver, SystemClock};
use super::types::Message;
use super::{Backend, BackendContext, Credentials, ModelRequest};
use crate::experiments::{JsonValue, OrderedMap};
use crate::observability::{self, LogLevel};
use crate::provider::config::{ApiSettings, ModelConfig, ProviderConfig};
use crate::telemetry::TelemetryCallType;

/// The cache file's name under the vibe home. Reference
/// `UTILITY_MODEL_CACHE_FILE`.
pub const CACHE_FILE: &str = "utility_model_cache.json";
/// The budget for a whole check, which runs while a session opens.
/// Reference `PROBE_TIMEOUT_SECONDS`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A served verdict holds for a week; a refused one for an hour, since a new
/// key or a redeployment can turn it around.
const AVAILABLE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const UNAVAILABLE_TTL_SECONDS: i64 = 60 * 60;
/// How long this process waits before asking again when a check ended
/// without an answer either way.
const UNDECIDED_RETRY_SECONDS: f64 = 10.0 * 60.0;
/// How long an entry missing from the file stays missing before the file is
/// read again.
const DISK_RECHECK_SECONDS: f64 = 60.0;
/// What the probe asks, and how much of an answer it lets the model give.
const PROBE_PROMPT: &str = "ping";
const PROBE_MAX_TOKENS: u64 = 1;

#[derive(Debug, Clone, Copy)]
struct Entry {
    available: bool,
    stored_at: i64,
}

impl Entry {
    fn fresh(self, now: i64) -> bool {
        let ttl = if self.available {
            AVAILABLE_TTL_SECONDS
        } else {
            UNAVAILABLE_TTL_SECONDS
        };
        self.stored_at > now - ttl
    }

    /// An entry as the file holds one: `available` a boolean and
    /// `stored_at_timestamp` an integer, which in Python a boolean also is.
    fn read(value: &JsonValue) -> Option<Self> {
        let JsonValue::Object(fields) = value else {
            return None;
        };
        let Some(JsonValue::Bool(available)) = fields.get("available") else {
            return None;
        };
        let stored_at = match fields.get("stored_at_timestamp")? {
            JsonValue::Bool(flag) => i64::from(*flag),
            JsonValue::Number(number) if !number.is_f64() => number.as_i64()?,
            _ => return None,
        };
        Some(Self {
            available: *available,
            stored_at,
        })
    }
}

#[derive(Default)]
struct State {
    memory: HashMap<String, Entry>,
    /// Deadlines on the monotonic clock, kept in this process alone.
    absent_until: HashMap<String, f64>,
    retry_after: HashMap<String, f64>,
}

/// Where the verdicts persist.
enum CacheFile {
    /// `$VIBE_HOME/utility_model_cache.json`, resolved on every access as the
    /// reference resolves its global paths.
    Ambient,
    At(PathBuf),
}

/// Availability verdicts: reads are synchronous, asking is not. Reference
/// `ModelAvailabilityCache`.
pub struct ModelAvailability {
    file: CacheFile,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    asking: tokio::sync::Mutex<()>,
}

impl ModelAvailability {
    /// A cache persisting at `path`, reading time from `clock`.
    #[must_use]
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self::with_file(CacheFile::At(path), clock)
    }

    fn with_file(file: CacheFile, clock: Arc<dyn Clock>) -> Self {
        Self {
            file,
            clock,
            state: Mutex::new(State::default()),
            asking: tokio::sync::Mutex::new(()),
        }
    }

    /// The process-wide cache under the ambient vibe home. Reference
    /// `MODEL_AVAILABILITY`.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<ModelAvailability> = OnceLock::new();
        GLOBAL.get_or_init(|| Self::with_file(CacheFile::Ambient, Arc::new(SystemClock::default())))
    }

    /// The file the verdicts persist in.
    #[must_use]
    pub fn cache_path(&self) -> PathBuf {
        match &self.file {
            CacheFile::Ambient => ambient_vibe_home().join(CACHE_FILE),
            CacheFile::At(path) => path.clone(),
        }
    }

    /// What the cache holds for `model`: served, refused, or nothing yet.
    /// Reference `peek`.
    pub fn peek(
        &self,
        provider: &ProviderConfig,
        model: &ModelConfig,
        credentials: &dyn Credentials,
    ) -> Option<bool> {
        let key = cache_key(provider, model, &credential(provider, credentials));
        let now = self.unix_now();
        let mut state = self.lock();
        if let Some(entry) = state
            .memory
            .get(&key)
            .copied()
            .filter(|entry| entry.fresh(now))
        {
            return Some(entry.available);
        }
        state.memory.remove(&key);
        let entry = self.read(&mut state, &key, now)?;
        state.memory.insert(key, entry);
        Some(entry.available)
    }

    /// Records a verdict in memory and on disk. Reference `remember`.
    pub fn remember(
        &self,
        provider: &ProviderConfig,
        model: &ModelConfig,
        credentials: &dyn Credentials,
        available: bool,
    ) {
        let key = cache_key(provider, model, &credential(provider, credentials));
        let entry = Entry {
            available,
            stored_at: self.unix_now(),
        };
        {
            let mut state = self.lock();
            state.memory.insert(key.clone(), entry);
            state.absent_until.remove(&key);
            state.retry_after.remove(&key);
        }
        self.write_entry(&key, entry);
    }

    /// Clears what this process holds; the file is untouched. Reference
    /// `reset`.
    pub fn reset(&self) {
        *self.lock() = State::default();
    }

    /// Probes, in the order given, every model with no verdict that comes
    /// before the first one already known served. Reference
    /// `ensure_first_available`.
    pub async fn ensure_first_available(
        &self,
        provider: &ProviderConfig,
        models: &[ModelConfig],
        budget: Duration,
        context: &BackendContext,
    ) {
        let credentials = context.credentials.as_ref();
        if self.askable(provider, models, credentials).is_empty() {
            return;
        }
        let _asking = self.asking.lock().await;
        let askable = self.askable(provider, models, credentials);
        if askable.is_empty() {
            return;
        }
        if !provider.api_key_env_var.is_empty()
            && credentials.resolve(&provider.api_key_env_var).is_none()
        {
            return;
        }
        let verdicts = check(provider, &askable, budget, context).await;
        let retry_after = self.clock.monotonic() + UNDECIDED_RETRY_SECONDS;
        for model in &askable {
            match verdicts.get(&model.name) {
                Some(available) => self.remember(provider, model, credentials, *available),
                None => {
                    let key = cache_key(provider, model, &credential(provider, credentials));
                    self.lock().retry_after.insert(key, retry_after);
                }
            }
        }
    }

    fn askable(
        &self,
        provider: &ProviderConfig,
        models: &[ModelConfig],
        credentials: &dyn Credentials,
    ) -> Vec<ModelConfig> {
        let now = self.clock.monotonic();
        let mut askable = Vec::new();
        for model in models {
            let known = self.peek(provider, model, credentials);
            if known == Some(true) {
                break;
            }
            let key = cache_key(provider, model, &credential(provider, credentials));
            let due = self
                .lock()
                .retry_after
                .get(&key)
                .is_none_or(|after| *after <= now);
            if known.is_none() && due {
                askable.push(model.clone());
            }
        }
        askable
    }

    fn read(&self, state: &mut State, key: &str, now: i64) -> Option<Entry> {
        let monotonic = self.clock.monotonic();
        if state
            .absent_until
            .get(key)
            .is_some_and(|until| *until > monotonic)
        {
            return None;
        }
        let entry = self
            .read_entries()
            .get(key)
            .and_then(Entry::read)
            .filter(|entry| entry.fresh(now));
        if entry.is_none() {
            state
                .absent_until
                .insert(key.to_owned(), monotonic + DISK_RECHECK_SECONDS);
        }
        entry
    }

    fn read_entries(&self) -> OrderedMap<JsonValue> {
        std::fs::read_to_string(self.cache_path())
            .ok()
            .and_then(|text| serde_json::from_str::<JsonValue>(&text).ok())
            .and_then(|value| match value {
                JsonValue::Object(entries) => Some(entries),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Rewrites the file with `entry` under `key`, dropping every other entry
    /// that is malformed or expired. Reference `_write_entry`.
    fn write_entry(&self, key: &str, entry: Entry) {
        let now = self.unix_now();
        let mut kept: OrderedMap<JsonValue> = self
            .read_entries()
            .iter()
            .filter(|(existing, value)| {
                *existing != key && Entry::read(value).is_some_and(|entry| entry.fresh(now))
            })
            .map(|(existing, value)| (existing.to_owned(), value.clone()))
            .collect();
        let mut fields = OrderedMap::new();
        fields.insert("available".to_owned(), JsonValue::Bool(entry.available));
        fields.insert(
            "stored_at_timestamp".to_owned(),
            JsonValue::Number(entry.stored_at.into()),
        );
        kept.insert(key.to_owned(), JsonValue::Object(fields));
        let text = JsonValue::Object(kept).python_json_compact();
        let path = self.cache_path();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&staging, text))
            .and_then(|()| std::fs::rename(&staging, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&staging);
            observability::log(
                LogLevel::Debug,
                &format!(
                    "Could not save the fast-model verdicts to {}",
                    path.display()
                ),
            );
        }
    }

    fn unix_now(&self) -> i64 {
        self.clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `$VIBE_HOME`, else `~/.vibe`. Reference `get_vibe_home`; the user's home
/// is read per platform, as `Path.home()` reads it.
fn ambient_vibe_home() -> PathBuf {
    let home = || crate::config::user_home_directory().unwrap_or_default();
    match std::env::var("VIBE_HOME")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(value) => match value.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                home().join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(value),
        },
        None => home().join(".vibe"),
    }
}

/// The key a provider's credential resolves to, or the empty string.
fn credential(provider: &ProviderConfig, credentials: &dyn Credentials) -> String {
    credentials
        .resolve(&provider.api_key_env_var)
        .map(|(key, _)| key)
        .unwrap_or_default()
}

/// The key a verdict is stored under, a digest that tells credentials apart
/// because two keys may see different models. Reference `_cache_key`.
#[must_use]
pub fn cache_key(provider: &ProviderConfig, model: &ModelConfig, credential: &str) -> String {
    let material = [
        provider.api_base.trim_end_matches('/'),
        provider.backend.label(),
        &model.name,
        &hex::encode(Sha256::digest(credential.as_bytes())),
    ]
    .join("|");
    let mut key = hex::encode(Sha256::digest(material.as_bytes()));
    key.truncate(32);
    key
}

/// Probes the models in turn and stops at the first served one or when the
/// budget is spent. Reference `CompletionProbeSource.check`.
async fn check(
    provider: &ProviderConfig,
    models: &[ModelConfig],
    budget: Duration,
    context: &BackendContext,
) -> BTreeMap<String, bool> {
    let deadline = Instant::now() + budget;
    let mut verdicts = BTreeMap::new();
    for model in models {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Some(available) = probe(provider, model, remaining, context).await else {
            continue;
        };
        verdicts.insert(model.name.clone(), available);
        if available {
            break;
        }
    }
    verdicts
}

/// A single one-token request: served, refused by a client error, or no
/// verdict at all. Reference `_probe`.
async fn probe(
    provider: &ProviderConfig,
    model: &ModelConfig,
    remaining: Duration,
    context: &BackendContext,
) -> Option<bool> {
    let context = BackendContext {
        api: ApiSettings {
            timeout: remaining,
            retry_max_elapsed_time: Duration::ZERO,
            ..ApiSettings::default()
        },
        ..context.clone()
    };
    let messages = [Message::user(PROBE_PROMPT)];
    let headers = [(
        "user-agent".to_owned(),
        super::call::user_agent(provider.backend),
    )];
    // The `secondary_call` label keeps the probe out of model-turn counts.
    let metadata = probe_metadata();
    let attempt = async {
        let backend = Backend::new(provider.clone(), context).map_err(BackendFailure::Local)?;
        backend
            .complete(
                &ModelRequest {
                    model,
                    messages: &messages,
                    temperature: 0.0,
                    tools: None,
                    max_tokens: Some(PROBE_MAX_TOKENS),
                    tool_choice: None,
                    extra_headers: &headers,
                    metadata: Some(&metadata),
                },
                &NoRetryObserver,
            )
            .await
    };
    let status = match tokio::time::timeout(remaining, attempt).await {
        Ok(Ok(_)) => return Some(true),
        Ok(Err(BackendFailure::Backend(error))) => error.status,
        Ok(Err(_)) | Err(_) => None,
    };
    if !is_refusal(status) {
        return None;
    }
    observability::log(
        LogLevel::Info,
        &format!(
            "Provider '{}' refused model '{}' with HTTP {}; background features run on the \
             session model instead.",
            provider.name,
            model.name,
            status.unwrap_or_default()
        ),
    );
    Some(false)
}

/// Whether a status settles the question as "not served": any client error
/// except 408 and 429, which only say "not now".
fn is_refusal(status: Option<u16>) -> bool {
    status.is_some_and(|status| (400..500).contains(&status) && status != 408 && status != 429)
}

/// Reference `build_request_metadata(launch_context=None, session_id=None,
/// call_type="secondary_call")`.
fn probe_metadata() -> Map<String, Value> {
    super::utility::request_metadata(None, None, TelemetryCallType::SecondaryCall)
}
