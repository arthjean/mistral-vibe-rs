//! `vibeCode/*`: the project picker a session links its repository through,
//! and the Teleport runs that hand the session to Vibe Code Web.
//!
//! Reference `VibeCodeController` (`vibe/app_server/_vibe_code.py`) with the
//! picker service (`vibe/core/vibe_code_project/picker_service.py`), the
//! orchestrator (`vibe/core/teleport/orchestrator.py`) and the run itself
//! (`vibe/core/teleport/teleport.py`). Each session owns one controller: one
//! picker at a time ([`picker`]), and any number of Teleport runs
//! ([`teleport`]).
//!
//! What the controller needs from the session (its configuration, its
//! transcript, its execution slot, the account, the model, telemetry and the
//! wire) comes through [`Host`], which the server implements. Every sentence a
//! refusal or a failure carries is this port's own; the predicate that tells a
//! stale saved project from any other failure reads the status and the body the
//! service answered with, which those sentences quote.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use vibe_core::events::ModelMessage;
use vibe_core::provider::AssistantMessage;
use vibe_core::telemetry::TelemetryRecord;
use vibe_protocol::ProtocolErrorCode;

pub(crate) mod git;
pub(crate) mod http;
mod picker;
mod teleport;

use git::normalize_repo_url;
use http::{Project, ProjectClient};

/// Where the hosted chat lives when the configuration names nothing.
const DEFAULT_BASE_URL: &str = "https://chat.mistral.ai";

/// The request timeout a configuration that names none runs with.
const DEFAULT_API_TIMEOUT_SECONDS: f64 = 720.0;

pub(crate) type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A refused request: the code the wire answers with and its sentence, with
/// no structured detail (reference `RequestFailure` with `data: null`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) code: ProtocolErrorCode,
    pub(crate) message: String,
}

impl Refusal {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ProtocolErrorCode::InvalidParams,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: ProtocolErrorCode::Conflict,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            code: ProtocolErrorCode::Forbidden,
            message: message.into(),
        }
    }
}

/// A saved association between a checkout and a project (reference
/// `RemoteProjectLink`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedLink {
    pub(crate) repo_root: String,
    pub(crate) repo_url: String,
    pub(crate) project_id: String,
    pub(crate) project_name: String,
}

/// What a controller reads and drives of the session it belongs to
/// (reference `VibeCodeSession`).
pub(crate) trait Host: Send + Sync + 'static {
    /// Reference `SessionExecution.require_idle`.
    fn require_idle(&self) -> Result<(), Refusal>;
    /// Reference `SessionExecution.begin` for a Teleport run.
    fn begin_teleport(&self, operation_id: &str) -> Result<(), Refusal>;
    /// Releases the slot when `operation_id` still holds it.
    fn finish_teleport(&self, operation_id: &str);
    /// The merged configuration the session runs under.
    fn config(&self) -> toml::Table;
    /// The directory the session runs in.
    fn cwd(&self) -> PathBuf;
    fn session_id(&self) -> String;
    /// The session's transcript, system message first.
    fn messages(&self) -> Vec<ModelMessage>;
    /// `account/read`'s view.
    fn read_account(&self) -> HostFuture<'_, Value>;
    /// The credential a variable names, or `None`.
    fn credential(&self, variable: &str) -> Option<String>;
    /// One model call with no tools, on the model the session compacts with.
    fn summarize(
        &self,
        messages: Vec<ModelMessage>,
    ) -> HostFuture<'_, Result<AssistantMessage, String>>;
    fn record(&self, record: &TelemetryRecord);
    /// Publishes one `vibeCode/teleport/event`.
    fn notify(&self, event: Value);
    /// What the client that started the session declared about itself:
    /// its entrypoint and its name.
    fn launch(&self) -> (String, Option<String>);
    fn saved_link(&self, repo_root: &str) -> Option<SavedLink>;
    fn save_link(&self, link: &SavedLink) -> Result<(), String>;
    fn delete_link(&self, repo_root: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Configure,
    Teleport,
}

impl Purpose {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            Some("teleport") => Self::Teleport,
            _ => Self::Configure,
        }
    }
}

/// The project API a picker was opened against (reference
/// `VibeCodeProjectPickerService`).
#[derive(Debug, Clone)]
pub(crate) struct Service {
    base_url: String,
    api_key: String,
    timeout: Duration,
}

impl Service {
    /// Reference `_build_service`: the endpoint and timeout the configuration
    /// names, under the Mistral provider's key, or `None` when no key
    /// resolves.
    pub(crate) fn from_config(
        config: &toml::Table,
        credential: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let api_key = configured_api_key(config, credential)?;
        let timeout = config
            .get("api_timeout")
            .and_then(|value| {
                value
                    .as_float()
                    .or_else(|| value.as_integer().map(|value| value as f64))
            })
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or(DEFAULT_API_TIMEOUT_SECONDS);
        Some(Self {
            base_url: config_text(config, "vibe_code_sessions_base_url", DEFAULT_BASE_URL),
            api_key,
            timeout: Duration::from_secs_f64(timeout),
        })
    }

    pub(crate) fn client(&self) -> Result<ProjectClient, String> {
        ProjectClient::new(&self.base_url, &self.api_key, self.timeout)
    }

    pub(crate) async fn page(&self, cursor: Option<&str>) -> Result<http::Page, String> {
        self.client()?.list(cursor).await
    }
}

/// What `vibeCode/teleport/start` asked for.
#[derive(Debug, Clone)]
pub(crate) struct StartParams {
    pub(crate) picker_id: String,
    pub(crate) operation_id: String,
    pub(crate) prompt: Option<String>,
    pub(crate) project_id: String,
}

#[derive(Default)]
pub(crate) struct VibeCodeController {
    /// The picker, behind the lock that serializes every picker call
    /// (reference `_picker_lock`).
    picker: tokio::sync::Mutex<Option<picker::Picker>>,
    runs: Arc<Mutex<teleport::Runs>>,
}

impl std::fmt::Debug for VibeCodeController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("VibeCodeController")
    }
}

type Answer = Result<Value, Refusal>;

fn configured_api_key(
    config: &toml::Table,
    credential: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let provider = vibe_core::telemetry::mistral_provider(config)?;
    credential(provider.get("api_key_env_var")?.as_str()?)
}

fn config_text(config: &toml::Table, key: &str, default: &str) -> String {
    config
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

/// Reference `session_message_count`: the transcript without its system
/// message.
fn message_count(messages: &[ModelMessage]) -> u64 {
    let system = usize::from(matches!(
        messages.first(),
        Some(ModelMessage::System { .. })
    ));
    messages.len().saturating_sub(system) as u64
}

/// Reference `is_project_linked_to_repo`.
pub(crate) fn is_project_linked_to_repo(project: &Project, repo_url: &str) -> bool {
    let current = normalize_repo_url(repo_url);
    project
        .repositories
        .iter()
        .any(|repository| normalize_repo_url(&repository.repo_url) == current)
}
