//! The datalake client, its envelope and the metadata census every event
//! carries.
//!
//! Reference `vibe/core/telemetry/`: `send.py` owns the endpoint, the credential
//! rule and the fire-and-forget delivery, `types.py` the two metadata models and
//! `build_metadata.py` the four builders. The envelope is open. One event is
//! `{"event", "properties", "correlation_id"?}` and `properties` is the base
//! metadata census merged with the event's own payload, the payload's keys
//! winning, which is what lets a client-authored event travel at all.
//!
//! Two rules survive that openness. The label validators still refuse a path, a
//! secret-shaped token and a control character in every value this port authors
//! itself, through [`TelemetryAttributes`]; they are never applied to properties
//! a client explicitly recorded, which travel unmodified. And the credential is
//! resolved from a Mistral provider only, so no third-party key ever reaches a
//! Mistral-controlled endpoint.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use toml::Table;
use url::Url;

use crate::compaction::CompactionStatus;
use crate::engine::EventObserver;
use crate::events::{EngineEvent, EventEnvelope};

pub mod records;

pub use records::TelemetryRecord;

/// The server a delivery falls back to when a provider's `api_base` carries no
/// version segment to derive one from. Reference
/// `_DEFAULT_TELEMETRY_BASE_URL`.
pub const TELEMETRY_DEFAULT_BASE_URL: &str = "https://api.mistral.ai";
/// Reference `_DATALAKE_EVENTS_PATH`.
pub const TELEMETRY_PATH: &str = "/v1/datalake/events";
/// The variable the shipped Mistral provider reads its key from, which is the
/// one a document that names none inherits from the defaults.
pub const TELEMETRY_DEFAULT_API_KEY_VARIABLE: &str = "MISTRAL_API_KEY";
/// Wall-clock ceiling on one delivery, matching the reference's 5.0 second
/// `httpx.Timeout`. Named rather than inlined so the parity replay reads the
/// value the transport actually uses.
pub const TELEMETRY_TIMEOUT_SECONDS: u64 = 5;
/// Idle connections kept per host, matching the reference's
/// `max_keepalive_connections`.
pub const TELEMETRY_MAX_KEEPALIVE_CONNECTIONS: usize = 5;
/// Deliveries in flight at once, matching the reference's `max_connections`.
/// `reqwest` caps idle connections rather than total ones, so the transport
/// holds this one itself.
pub const TELEMETRY_MAX_CONNECTIONS: usize = 10;
/// The scheme the credential is presented under.
pub const TELEMETRY_AUTHORIZATION_SCHEME: &str = "Bearer";
/// The source every request-scoped census reports. Reference
/// `TelemetryRequestMetadata.call_source`.
pub const TELEMETRY_CALL_SOURCE: &str = "vibe_code";
/// The only attachment kind the reference counts. Reference `AttachmentKind`.
pub const TELEMETRY_ATTACHMENT_IMAGE: &str = "image";
/// The backend value that makes a provider Mistral's. A provider entry that
/// declares none is not Mistral's, which is what keeps a third-party key away
/// from the datalake.
const MISTRAL_BACKEND: &str = "mistral";
/// Reference `get_user_agent`: every request identifies as the Vibe client, and
/// a Mistral backend prefixes the SDK marker. Reproduced verbatim so a datalake
/// consumer written against the reference reads this port's deliveries without
/// a translation layer.
const USER_AGENT_PRODUCT: &str = "Mistral-Vibe";
const USER_AGENT_MISTRAL_PREFIX: &str = "mistral-client-python/";
/// The version the user agent and the `version` census field report.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SAFE_LABEL_BYTES: usize = 128;

/// Reference `get_user_agent`.
#[must_use]
pub fn telemetry_user_agent(backend: Option<&str>) -> String {
    let agent = format!("{USER_AGENT_PRODUCT}/{VERSION}");
    if backend == Some(MISTRAL_BACKEND) {
        return format!("{USER_AGENT_MISTRAL_PREFIX}{agent}");
    }
    agent
}

/// Reference `get_server_url_from_api_base`: the origin an `api_base` carries
/// ahead of its version segment. `None` when it carries no version segment,
/// which is what sends the endpoint to [`TELEMETRY_DEFAULT_BASE_URL`].
static SERVER_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^(https?://.+)(/v\d+.*)").ok());

pub(crate) fn server_url_from_api_base(api_base: &str) -> Option<String> {
    let captures = SERVER_URL.as_ref()?.captures(api_base)?;
    Some(captures.get(1)?.as_str().to_owned())
}

/// Reference `TelemetryClient._get_telemetry_url`: the server derived from the
/// provider's `api_base`, or the default one, joined with the datalake path.
///
/// The credential travels as a bearer token, so a base that is not HTTPS, or
/// that carries a credential of its own, resolves to nothing rather than being
/// sent to. The reference's regex admits `http://`; this port refuses it, which
/// is recorded in the accepted-divergence table of `docs/parity.md`.
#[must_use]
pub fn telemetry_endpoint(api_base: &str) -> Option<Url> {
    let base =
        server_url_from_api_base(api_base).unwrap_or_else(|| TELEMETRY_DEFAULT_BASE_URL.to_owned());
    let endpoint = Url::parse(base.trim_end_matches('/'))
        .ok()?
        .join(TELEMETRY_PATH)
        .ok()?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || endpoint.username() != ""
        || endpoint.password().is_some()
    {
        return None;
    }
    Some(endpoint)
}

// --------------------------------------------------------------------------
// The provider a delivery authenticates as
// --------------------------------------------------------------------------

/// Reference `VibeConfigSchema.get_mistral_provider`: the active model's
/// provider when its backend is Mistral, and otherwise the first Mistral
/// provider configured.
#[must_use]
pub fn mistral_provider(effective: &Table) -> Option<Table> {
    let providers = provider_entries(effective);
    if let Some(active) = active_provider(effective, &providers)
        && is_mistral(&active)
    {
        return Some(active);
    }
    providers.into_iter().find(is_mistral)
}

fn provider_entries(effective: &Table) -> Vec<Table> {
    effective
        .get("providers")
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(toml::Value::as_table)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The provider entry serving the active model, in either persisted model
/// shape: an array of entries carrying their own alias, or a table keyed by it.
fn active_provider(effective: &Table, providers: &[Table]) -> Option<Table> {
    let alias = effective.get("active_model")?.as_str()?;
    let model = match effective.get("models") {
        Some(toml::Value::Table(models)) => models.get(alias)?.as_table()?.clone(),
        Some(toml::Value::Array(models)) => models
            .iter()
            .filter_map(toml::Value::as_table)
            .find(|entry| {
                ["alias", "name"]
                    .into_iter()
                    .any(|key| entry.get(key).and_then(toml::Value::as_str) == Some(alias))
            })?
            .clone(),
        _ => return None,
    };
    let provider = model.get("provider")?.as_str()?;
    providers
        .iter()
        .find(|entry| entry.get("name").and_then(toml::Value::as_str) == Some(provider))
        .cloned()
}

fn is_mistral(provider: &Table) -> bool {
    provider.get("backend").and_then(toml::Value::as_str) == Some(MISTRAL_BACKEND)
}

/// Where one delivery goes and how it identifies itself, resolved from the
/// merged configuration the way the reference resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryTarget {
    pub endpoint: Url,
    pub user_agent: String,
    /// The variable the credential is read from. The value is never held here:
    /// what names a credential and what carries one stay apart.
    pub credential_variable: String,
}

impl TelemetryTarget {
    /// Reference `get_mistral_provider_and_api_key` up to the key lookup: the
    /// provider decides the endpoint, the user agent and the variable, and the
    /// caller decides how a variable becomes a credential.
    #[must_use]
    pub fn resolve(effective: &Table) -> Option<Self> {
        let provider = mistral_provider(effective)?;
        let endpoint = telemetry_endpoint(
            provider
                .get("api_base")
                .and_then(toml::Value::as_str)
                .unwrap_or_default(),
        )?;
        let variable = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        if variable.is_empty() {
            // Reference `resolve_api_key` answers nothing for an empty
            // variable, so a provider that names none resolves no credential.
            return None;
        }
        Some(Self {
            endpoint,
            user_agent: telemetry_user_agent(provider.get("backend").and_then(toml::Value::as_str)),
            credential_variable: variable.to_owned(),
        })
    }
}

/// Whether telemetry is on for this process, and what it would deliver to.
///
/// Resolved on every send rather than once at startup, which is what makes a
/// document edited mid-session decide the next event.
pub struct TelemetryConfig {
    enabled: bool,
    target: Option<TelemetryTarget>,
    credential: Option<SecretString>,
}

impl TelemetryConfig {
    /// Reference `TelemetryClient._is_enabled` and
    /// `get_mistral_provider_and_api_key`: `enable_telemetry` defaults to true,
    /// and a delivery still needs a Mistral provider whose variable resolves.
    #[must_use]
    pub fn resolve(effective: &Table, credentials: &dyn Fn(&str) -> Option<String>) -> Self {
        let enabled = effective
            .get("enable_telemetry")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        let target = TelemetryTarget::resolve(effective);
        let credential = target
            .as_ref()
            .and_then(|target| credentials(&target.credential_variable))
            .filter(|value| !value.is_empty())
            .map(SecretString::from);
        Self {
            enabled,
            target,
            credential,
        }
    }

    /// What an unreadable configuration resolves to. Reference `_is_enabled`
    /// swallows the failure and answers `False`, so a document that cannot be
    /// read silences telemetry instead of failing the run.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            target: None,
            credential: None,
        }
    }

    /// Reference `TelemetryClient.is_active`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.credential.is_some() && self.target.is_some()
    }

    #[must_use]
    pub fn target(&self) -> Option<&TelemetryTarget> {
        self.target.as_ref()
    }
}

// --------------------------------------------------------------------------
// The metadata census
// --------------------------------------------------------------------------

/// What the process was launched as, supplied by the adapter that launched it.
/// Reference `LaunchContext`.
///
/// Serializing one is the reference's `telemetry_fields`: the terminal is
/// carried as null rather than dropped, and the census that consumes these
/// fields is what drops it. `None` means the adapter reports no terminal at
/// all; a terminal that cannot be identified reports `unknown` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchContext {
    pub agent_entrypoint: String,
    pub agent_version: String,
    pub client_name: String,
    pub client_version: String,
    #[serde(default)]
    pub terminal_emulator: Option<String>,
}

/// Reference `TelemetryBaseMetadata`, in its declaration order. Every field is
/// optional and every absent one is dropped rather than sent as null, which is
/// the reference's `exclude_none`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryBaseMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_emulator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Filled by the experiments manager upstream. Rank 16 of `docs/parity.md`
    /// is unshipped here, so the field is carried and left absent rather than
    /// fabricated; the accepted-divergence table records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiments: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_plan: Option<String>,
}

impl TelemetryBaseMetadata {
    /// The census as the properties an envelope carries.
    #[must_use]
    pub fn properties(&self) -> Map<String, Value> {
        properties_of(self)
    }
}

/// Reference `TelemetryCallType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryCallType {
    MainCall,
    SecondaryCall,
}

impl TelemetryCallType {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MainCall => "main_call",
            Self::SecondaryCall => "secondary_call",
        }
    }
}

/// Reference `TelemetryRequestMetadata`: the base census plus the three fields
/// a request-scoped event adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryRequestMetadata {
    #[serde(flatten)]
    pub base: TelemetryBaseMetadata,
    pub call_type: TelemetryCallType,
    pub call_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

impl TelemetryRequestMetadata {
    #[must_use]
    pub fn properties(&self) -> Map<String, Value> {
        properties_of(self)
    }
}

/// A serializable census as the object it serializes to. Neither census can
/// fail to serialize: every field is a string, a boolean or a map of strings.
fn properties_of<T: Serialize>(census: &T) -> Map<String, Value> {
    serde_json::to_value(census)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// What every event of this process reports before its own payload. Reference
/// `TelemetryClient.__init__`'s six getters, held as values because this port
/// reads the session from the event it is projecting rather than from a
/// getter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryContext {
    pub launch: Option<LaunchContext>,
    pub parent_session_id: Option<String>,
    pub experiments: BTreeMap<String, String>,
    pub user_plan: Option<String>,
}

impl TelemetryContext {
    /// Reference `build_base_metadata`.
    #[must_use]
    pub fn base_metadata(&self, session_id: Option<&str>) -> TelemetryBaseMetadata {
        let launch = self.launch.as_ref();
        TelemetryBaseMetadata {
            agent_entrypoint: launch.map(|launch| launch.agent_entrypoint.clone()),
            agent_version: launch.map(|launch| launch.agent_version.clone()),
            client_name: launch.map(|launch| launch.client_name.clone()),
            client_version: launch.map(|launch| launch.client_version.clone()),
            os: Some(platform_id()),
            os_version: platform_version(),
            version: Some(VERSION.to_owned()),
            terminal_emulator: launch.and_then(|launch| launch.terminal_emulator.clone()),
            session_id: session_id.map(ToOwned::to_owned),
            parent_session_id: self.parent_session_id.clone(),
            // Reference `experiments or None`: an empty map is absent rather
            // than an empty object.
            experiments: (!self.experiments.is_empty()).then(|| self.experiments.clone()),
            user_plan: self.user_plan.clone(),
        }
    }

    /// Reference `build_request_metadata`.
    #[must_use]
    pub fn request_metadata(
        &self,
        session_id: Option<&str>,
        call_type: TelemetryCallType,
        message_id: Option<String>,
    ) -> TelemetryRequestMetadata {
        TelemetryRequestMetadata {
            base: TelemetryBaseMetadata {
                // The reference's request model carries no experiments: the
                // field is declared on the base and left unset by the builder.
                experiments: None,
                ..self.base_metadata(session_id)
            },
            call_type,
            call_source: TELEMETRY_CALL_SOURCE.to_owned(),
            message_id,
        }
    }
}

/// Reference `build_attachment_counts`: images are reported only when the
/// provider serving the request accepts them, and a message carrying none
/// reports no key rather than a zero.
#[must_use]
pub fn attachment_counts(images: usize, supports_images: bool) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    if supports_images && images > 0 {
        counts.insert(TELEMETRY_ATTACHMENT_IMAGE.to_owned(), images as u64);
    }
    counts
}

/// Reference `get_platform_id`: the canonical lowercase platform identifier.
/// Rust names macOS `macos` where the reference names it `darwin`, and agrees
/// everywhere else.
#[must_use]
pub fn platform_id() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_owned(),
        other => other.to_owned(),
    }
}

/// Reference `get_platform_version`: the distribution version on Linux, the
/// product version on macOS and the system version on Windows.
///
/// Resolved once per process: the macOS and Windows branches read a system
/// tool, which a per-event census must not do.
#[must_use]
pub fn platform_version() -> Option<String> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION.get_or_init(resolve_platform_version).clone()
}

#[cfg(target_os = "linux")]
fn resolve_platform_version() -> Option<String> {
    let release = std::fs::read_to_string("/etc/os-release").ok()?;
    let field = |key: &str| {
        release.lines().find_map(|line| {
            line.strip_prefix(key)
                .map(|value| value.trim_matches('"').to_owned())
        })
    };
    field("VERSION_ID=")
        .or_else(|| field("VERSION="))
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "macos")]
fn resolve_platform_version() -> Option<String> {
    command_output("sw_vers", &["-productVersion"])
}

#[cfg(target_os = "windows")]
fn resolve_platform_version() -> Option<String> {
    // `cmd /C ver` prints `Microsoft Windows [Version 10.0.19045.1234]`, and
    // the reference reports the bracketed number alone.
    let printed = command_output("cmd", &["/C", "ver"])?;
    let version = printed.split_once('[')?.1.rsplit_once(']')?.0;
    version
        .rsplit_once(' ')
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn resolve_platform_version() -> Option<String> {
    None
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let printed = String::from_utf8(output.stdout).ok()?;
    let printed = printed.trim().to_owned();
    (!printed.is_empty()).then_some(printed)
}

/// Reference `detect_terminal`: the terminal the process is attached to, named
/// from the vocabulary `vibe_protocol::TerminalEmulator` publishes, and
/// `unknown` when nothing identifies one.
#[must_use]
pub fn detect_terminal_emulator() -> &'static str {
    terminal_emulator_from(&|name| std::env::var(name).ok())
}

/// The environment markers, in the order the reference consults them:
/// `TERM_PROGRAM` first, with the Cursor and Insiders splits under `vscode`,
/// then the per-terminal variables, then JetBrains.
fn terminal_emulator_from(lookup: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let value = |name: &str| lookup(name).unwrap_or_default().to_ascii_lowercase();
    let program = value("TERM_PROGRAM");
    if program == "vscode" {
        if [
            "VSCODE_GIT_ASKPASS_NODE",
            "VSCODE_GIT_ASKPASS_MAIN",
            "VSCODE_IPC_HOOK_CLI",
            "VSCODE_NLS_CONFIG",
        ]
        .into_iter()
        .any(|name| value(name).contains("cursor"))
        {
            return "cursor";
        }
        if value("TERM_PROGRAM_VERSION").ends_with("-insider") {
            return "vscode_insiders";
        }
        return "vscode";
    }
    for (marker, terminal) in [
        ("apple_terminal", "apple_terminal"),
        ("iterm.app", "iterm2"),
        ("wezterm", "wezterm"),
        ("ghostty", "ghostty"),
        ("alacritty", "alacritty"),
        ("kitty", "kitty"),
        ("hyper", "hyper"),
    ] {
        if program == marker {
            return terminal;
        }
    }
    for (variable, terminal) in [
        ("WEZTERM_PANE", "wezterm"),
        ("GHOSTTY_RESOURCES_DIR", "ghostty"),
        ("KITTY_WINDOW_ID", "kitty"),
        ("ALACRITTY_SOCKET", "alacritty"),
        ("ALACRITTY_LOG", "alacritty"),
        ("WT_SESSION", "windows_terminal"),
        ("WT_PROFILE_ID", "windows_terminal"),
    ] {
        if !value(variable).is_empty() {
            return terminal;
        }
    }
    if value("TERMINAL_EMULATOR").contains("jetbrains") {
        return "jetbrains";
    }
    "unknown"
}

// --------------------------------------------------------------------------
// The events this port authors
// --------------------------------------------------------------------------

/// Every event name this port publishes.
///
/// The reference raises 26 across its client and its five satellite emitters.
/// This vocabulary carries 25 of them: `vibe.admin_config_applied` reports on
/// the org-managed configuration layer, which no part of this port fetches or
/// composes, and the accepted-divergence table of `docs/parity.md` records that
/// rather than declaring a name nothing can raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryEvent {
    NewSession,
    SessionClosed,
    Ready,
    Startup,
    RequestSent,
    ToolCallFinished,
    AtMentionInserted,
    AutoCompactTriggered,
    CompactionFailed,
    SlashCommandUsed,
    UserCopiedText,
    UserCancelledAction,
    VoiceModeToggled,
    OnboardingApiKeyAdded,
    TeleportCompleted,
    TeleportFailed,
    FeedbackSubmitted,
    RemoteProjectConfigured,
    TranscriptionStarted,
    TranscriptionCancelled,
    TranscriptionDone,
    TranscriptionFailed,
    ReadAloudRequested,
    ReadAloudPlayStarted,
    ReadAloudEnded,
}

impl TelemetryEvent {
    /// Every name this port publishes, which is what the replay measures the
    /// vocabulary against.
    pub const ALL: [Self; 25] = [
        Self::NewSession,
        Self::SessionClosed,
        Self::Ready,
        Self::Startup,
        Self::RequestSent,
        Self::ToolCallFinished,
        Self::AtMentionInserted,
        Self::AutoCompactTriggered,
        Self::CompactionFailed,
        Self::SlashCommandUsed,
        Self::UserCopiedText,
        Self::UserCancelledAction,
        Self::VoiceModeToggled,
        Self::OnboardingApiKeyAdded,
        Self::TeleportCompleted,
        Self::TeleportFailed,
        Self::FeedbackSubmitted,
        Self::RemoteProjectConfigured,
        Self::TranscriptionStarted,
        Self::TranscriptionCancelled,
        Self::TranscriptionDone,
        Self::TranscriptionFailed,
        Self::ReadAloudRequested,
        Self::ReadAloudPlayStarted,
        Self::ReadAloudEnded,
    ];

    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::NewSession => "vibe.new_session",
            Self::SessionClosed => "vibe.session_closed",
            Self::Ready => "vibe.ready",
            Self::Startup => "vibe.startup",
            Self::RequestSent => "vibe.request_sent",
            Self::ToolCallFinished => "vibe.tool_call_finished",
            Self::AtMentionInserted => "vibe.at_mention_inserted",
            Self::AutoCompactTriggered => "vibe.auto_compact_triggered",
            Self::CompactionFailed => "vibe.compaction_failed",
            Self::SlashCommandUsed => "vibe.slash_command_used",
            Self::UserCopiedText => "vibe.user_copied_text",
            Self::UserCancelledAction => "vibe.user_cancelled_action",
            Self::VoiceModeToggled => "vibe.voice_mode_toggled",
            Self::OnboardingApiKeyAdded => "vibe.onboarding_api_key_added",
            Self::TeleportCompleted => "vibe.teleport_completed",
            Self::TeleportFailed => "vibe.teleport_failed",
            Self::FeedbackSubmitted => "vibe.user_rating_feedback",
            Self::RemoteProjectConfigured => "vibe.remote_project_configured",
            Self::TranscriptionStarted => "vibe.audio.transcription.start",
            Self::TranscriptionCancelled => "vibe.audio.transcription.cancel_recording",
            Self::TranscriptionDone => "vibe.audio.transcription.done",
            Self::TranscriptionFailed => "vibe.audio.transcription.error",
            Self::ReadAloudRequested => "vibe.read_aloud.requested",
            Self::ReadAloudPlayStarted => "vibe.read_aloud.play_started",
            Self::ReadAloudEnded => "vibe.read_aloud.ended",
        }
    }
}

/// Every property key an event this port authors can carry, drawn from the
/// reference's own senders. A key is spelled once, here, so a payload cannot
/// invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryField {
    Action,
    AgentProfileName,
    AgentReadyDurationMs,
    ApprovalType,
    AttachmentCounts,
    AutoCompactThreshold,
    BashBackground,
    CallSource,
    CallType,
    ClientName,
    ClientVersion,
    Command,
    CommandType,
    ContextSummary,
    ContextSummaryChars,
    ContextTypes,
    CustomDomain,
    Decision,
    ElapsedSeconds,
    Enabled,
    Entrypoint,
    ErrorClass,
    ErrorMessage,
    ErrorType,
    FailureKind,
    FileExtension,
    FileExtensions,
    FirstFrameDurationMs,
    HasAgentsMd,
    HttpStatusCode,
    InitDurationMs,
    MessageId,
    Model,
    NbContextChars,
    NbContextMessages,
    NbContextTokensBefore,
    NbFilesCreated,
    NbFilesModified,
    NbMcpServers,
    NbMentions,
    NbModels,
    NbPromptChars,
    NbSessionMessages,
    NbSkills,
    Outcome,
    ProjectCandidateCountLoaded,
    ProjectMultiRepoMatchCount,
    ProjectPickerShown,
    ProjectRepoRemoteChanged,
    ProjectSelectionSource,
    PushRequired,
    Rating,
    ReadAloudSessionId,
    Reason,
    RecordingDurationMs,
    RecordingId,
    SavedProjectLinkCleared,
    SessionInitDurationMs,
    SpeedSelection,
    Stage,
    Status,
    TerminalEmulator,
    TextLength,
    TimeToFirstReadS,
    ToolName,
    TranscriptLength,
    TranscriptionDurationMs,
    Trigger,
    Version,
}

impl TelemetryField {
    const fn key(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::AgentProfileName => "agent_profile_name",
            Self::AgentReadyDurationMs => "agent_ready_duration_ms",
            Self::ApprovalType => "approval_type",
            Self::AttachmentCounts => "attachment_counts",
            Self::AutoCompactThreshold => "auto_compact_threshold",
            Self::BashBackground => "bash_background",
            Self::CallSource => "call_source",
            Self::CallType => "call_type",
            Self::ClientName => "client_name",
            Self::ClientVersion => "client_version",
            Self::Command => "command",
            Self::CommandType => "command_type",
            Self::ContextSummary => "context_summary",
            Self::ContextSummaryChars => "context_summary_chars",
            Self::ContextTypes => "context_types",
            Self::CustomDomain => "custom_domain",
            Self::Decision => "decision",
            Self::ElapsedSeconds => "elapsed_seconds",
            Self::Enabled => "enabled",
            Self::Entrypoint => "entrypoint",
            Self::ErrorClass => "error_class",
            Self::ErrorMessage => "error_message",
            Self::ErrorType => "error_type",
            Self::FailureKind => "failure_kind",
            Self::FileExtension => "file_extension",
            Self::FileExtensions => "file_extensions",
            Self::FirstFrameDurationMs => "first_frame_duration_ms",
            Self::HasAgentsMd => "has_agents_md",
            Self::HttpStatusCode => "http_status_code",
            Self::InitDurationMs => "init_duration_ms",
            Self::MessageId => "message_id",
            Self::Model => "model",
            Self::NbContextChars => "nb_context_chars",
            Self::NbContextMessages => "nb_context_messages",
            Self::NbContextTokensBefore => "nb_context_tokens_before",
            Self::NbFilesCreated => "nb_files_created",
            Self::NbFilesModified => "nb_files_modified",
            Self::NbMcpServers => "nb_mcp_servers",
            Self::NbMentions => "nb_mentions",
            Self::NbModels => "nb_models",
            Self::NbPromptChars => "nb_prompt_chars",
            Self::NbSessionMessages => "nb_session_messages",
            Self::NbSkills => "nb_skills",
            Self::Outcome => "outcome",
            Self::ProjectCandidateCountLoaded => "project_candidate_count_loaded",
            Self::ProjectMultiRepoMatchCount => "project_multi_repo_match_count",
            Self::ProjectPickerShown => "project_picker_shown",
            Self::ProjectRepoRemoteChanged => "project_repo_remote_changed",
            Self::ProjectSelectionSource => "project_selection_source",
            Self::PushRequired => "push_required",
            Self::Rating => "rating",
            Self::ReadAloudSessionId => "read_aloud_session_id",
            Self::Reason => "reason",
            Self::RecordingDurationMs => "recording_duration_ms",
            Self::RecordingId => "recording_id",
            Self::SavedProjectLinkCleared => "saved_project_link_cleared",
            Self::SessionInitDurationMs => "session_init_duration_ms",
            Self::SpeedSelection => "speed_selection",
            Self::Stage => "stage",
            Self::Status => "status",
            Self::TerminalEmulator => "terminal_emulator",
            Self::TextLength => "text_length",
            Self::TimeToFirstReadS => "time_to_first_read_s",
            Self::ToolName => "tool_name",
            Self::TranscriptLength => "transcript_length",
            Self::TranscriptionDurationMs => "transcription_duration_ms",
            Self::Trigger => "trigger",
            Self::Version => "version",
        }
    }
}

/// The payload of one event this port authors itself.
///
/// Every label passes [`validate_safe_label`], which is the invariant that
/// survived the move to the reference's open envelope: a path, a secret-shaped
/// token or a control character has no representation here. Properties a client
/// recorded through `telemetry/record` never travel through this type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TelemetryAttributes(Map<String, Value>);

impl TelemetryAttributes {
    pub fn label(
        &mut self,
        field: TelemetryField,
        value: impl Into<String>,
    ) -> Result<&mut Self, TelemetryError> {
        let value = value.into();
        validate_safe_label(&value)?;
        self.0.insert(field.key().to_owned(), Value::String(value));
        Ok(self)
    }

    /// A label the reference carries as null rather than dropping when its
    /// source has no value: `decision`, `message_id`, `file_extension` and the
    /// read-aloud error type all travel that way.
    pub fn optional_label(
        &mut self,
        field: TelemetryField,
        value: Option<impl Into<String>>,
    ) -> Result<&mut Self, TelemetryError> {
        match value {
            Some(value) => self.label(field, value),
            None => Ok(self.null(field)),
        }
    }

    /// A value that is content rather than a label.
    ///
    /// The one payload that needs it is the transcription failure, whose whole
    /// subject is the endpoint's message; the validators exist to keep a path
    /// or a secret out of a *label*, and refusing this one would drop the event
    /// the reference sends.
    pub fn text(&mut self, field: TelemetryField, value: impl Into<String>) -> &mut Self {
        self.0
            .insert(field.key().to_owned(), Value::String(value.into()));
        self
    }

    pub fn count(&mut self, field: TelemetryField, value: u64) -> &mut Self {
        self.0
            .insert(field.key().to_owned(), Value::Number(value.into()));
        self
    }

    pub fn optional_count(&mut self, field: TelemetryField, value: Option<u64>) -> &mut Self {
        match value {
            Some(value) => self.count(field, value),
            None => self.null(field),
        }
    }

    pub fn flag(&mut self, field: TelemetryField, value: bool) -> &mut Self {
        self.0.insert(field.key().to_owned(), Value::Bool(value));
        self
    }

    /// A map of counts, as `attachment_counts`, `context_types` and
    /// `file_extensions` carry. An empty map travels as an empty object, which
    /// is what the reference's comprehension over no attachments leaves.
    pub fn counts(&mut self, field: TelemetryField, values: &BTreeMap<String, u64>) -> &mut Self {
        let object = values
            .iter()
            .map(|(key, value)| (key.clone(), Value::Number((*value).into())))
            .collect::<Map<String, Value>>();
        self.0.insert(field.key().to_owned(), Value::Object(object));
        self
    }

    pub fn optional_counts(
        &mut self,
        field: TelemetryField,
        values: Option<&BTreeMap<String, u64>>,
    ) -> &mut Self {
        match values {
            Some(values) => self.counts(field, values),
            None => self.null(field),
        }
    }

    /// A duration in fractional milliseconds, which is what the reference's
    /// `time.monotonic()` arithmetic produces for the audio events.
    pub fn millis(&mut self, field: TelemetryField, value: Duration) -> &mut Self {
        self.number(field, value.as_secs_f64() * 1_000.0)
    }

    pub fn optional_millis(&mut self, field: TelemetryField, value: Option<Duration>) -> &mut Self {
        match value {
            Some(value) => self.millis(field, value),
            None => self.null(field),
        }
    }

    /// A duration in fractional seconds, as the two read-aloud measures carry.
    pub fn seconds(&mut self, field: TelemetryField, value: Duration) -> &mut Self {
        self.number(field, value.as_secs_f64())
    }

    /// A key the reference sends with no value, as opposed to one it omits.
    pub fn null(&mut self, field: TelemetryField) -> &mut Self {
        self.0.insert(field.key().to_owned(), Value::Null);
        self
    }

    /// A finite fractional number, which is the only kind JSON has. A reading
    /// that is not finite cannot be serialized at all, so it travels as null
    /// rather than dropping the event that carries it.
    fn number(&mut self, field: TelemetryField, value: f64) -> &mut Self {
        match serde_json::Number::from_f64(value) {
            Some(number) => {
                self.0.insert(field.key().to_owned(), Value::Number(number));
            }
            None => {
                self.0.insert(field.key().to_owned(), Value::Null);
            }
        }
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn into_properties(self) -> Map<String, Value> {
        self.0
    }
}

/// One event on the wire. Reference `send_telemetry_event`'s payload:
/// `{"event", "properties"}` plus `"correlation_id"` only when one is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEnvelope {
    pub event: String,
    pub properties: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl TelemetryEnvelope {
    /// The properties are carried unmodified, which is what lets a
    /// client-recorded event travel. An empty correlation id is dropped, as the
    /// reference's falsy check drops it.
    #[must_use]
    pub fn new(
        event: impl Into<String>,
        properties: Map<String, Value>,
        correlation_id: Option<String>,
    ) -> Self {
        Self {
            event: event.into(),
            properties,
            correlation_id: correlation_id.filter(|id| !id.is_empty()),
        }
    }
}

/// Reference `build_client_event_metadata() | properties`: the census first, the
/// event's own payload second, so a caller's key wins over the census.
#[must_use]
pub fn merge_properties(
    census: Map<String, Value>,
    payload: Map<String, Value>,
) -> Map<String, Value> {
    let mut properties = census;
    properties.extend(payload);
    properties
}

/// The headers one delivery carries.
///
/// Split out of the transport so the header set is observable without issuing a
/// request, which is what the parity replay compares.
pub fn telemetry_headers(
    user_agent: &str,
    credential: &SecretString,
) -> Result<HeaderMap, TelemetryError> {
    let authorization = HeaderValue::from_str(&format!(
        "{TELEMETRY_AUTHORIZATION_SCHEME} {}",
        credential.expose_secret()
    ))
    .map_err(|_| TelemetryError::InvalidCredential)?;
    let user_agent = HeaderValue::from_str(user_agent).map_err(|_| TelemetryError::InvalidAgent)?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(USER_AGENT, user_agent);
    Ok(headers)
}

pub type TelemetryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), TelemetryError>> + Send + 'a>>;

pub trait TelemetryTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        endpoint: &'a Url,
        user_agent: &'a str,
        credential: &'a SecretString,
        envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a>;
}

#[derive(Clone)]
pub struct ReqwestTelemetryTransport {
    client: reqwest::Client,
    /// The total-connection cap the reference sets on its client and `reqwest`
    /// does not expose, held here so a burst of events cannot open more
    /// sockets than the reference would.
    connections: Arc<tokio::sync::Semaphore>,
}

impl ReqwestTelemetryTransport {
    pub fn try_new() -> Result<Self, TelemetryError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(TELEMETRY_TIMEOUT_SECONDS))
            .pool_max_idle_per_host(TELEMETRY_MAX_KEEPALIVE_CONNECTIONS)
            .build()
            .map_err(|_| TelemetryError::TransportSetup)?;
        Ok(Self {
            client,
            connections: Arc::new(tokio::sync::Semaphore::new(TELEMETRY_MAX_CONNECTIONS)),
        })
    }
}

impl TelemetryTransport for ReqwestTelemetryTransport {
    fn send<'a>(
        &'a self,
        endpoint: &'a Url,
        user_agent: &'a str,
        credential: &'a SecretString,
        envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a> {
        Box::pin(async move {
            let _permit = self
                .connections
                .acquire()
                .await
                .map_err(|_| TelemetryError::TransportSetup)?;
            let response = self
                .client
                .post(endpoint.clone())
                .headers(telemetry_headers(user_agent, credential)?)
                .json(envelope)
                .send()
                .await
                .map_err(|_| TelemetryError::Delivery)?;
            if !response.status().is_success() {
                return Err(TelemetryError::Rejected(response.status().as_u16()));
            }
            Ok(())
        })
    }
}

/// How the client reaches the configuration on every send. Reference
/// `TelemetryClient._config_getter`.
pub type TelemetryConfigGetter = Arc<dyn Fn() -> TelemetryConfig + Send + Sync>;

pub struct TelemetryClient<T> {
    config: TelemetryConfigGetter,
    transport: T,
}

impl<T: TelemetryTransport> TelemetryClient<T> {
    #[must_use]
    pub fn new(config: TelemetryConfigGetter, transport: T) -> Self {
        Self { config, transport }
    }

    /// A client that never delivers, for a caller that has no configuration to
    /// read.
    #[must_use]
    pub fn disabled(transport: T) -> Self {
        Self::new(Arc::new(TelemetryConfig::disabled), transport)
    }

    pub async fn record(
        &self,
        envelope: &TelemetryEnvelope,
    ) -> Result<TelemetryOutcome, TelemetryError> {
        let config = (self.config)();
        if !config.enabled {
            return Ok(TelemetryOutcome::Disabled);
        }
        let (Some(target), Some(credential)) = (config.target.as_ref(), config.credential.as_ref())
        else {
            return Ok(TelemetryOutcome::NoEligibleCredential);
        };
        self.transport
            .send(&target.endpoint, &target.user_agent, credential, envelope)
            .await?;
        Ok(TelemetryOutcome::Sent)
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        (self.config)().is_active()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryOutcome {
    Disabled,
    NoEligibleCredential,
    Sent,
}

/// What the turn's own events reported about the request they belong to.
///
/// The reference reads the model, the profile and the current message
/// identifier off its agent loop where it builds each event. This port projects
/// events from a stream, so the same facts are remembered here as they pass:
/// [`EngineEvent::RequestSent`] sets them and the tool events of that request
/// read them back.
#[derive(Debug, Default)]
struct TurnContext {
    model: String,
    agent_profile: String,
    message_id: Option<String>,
    /// What a rating correlates with. Reference `last_correlation_id`, which
    /// its loop fills from the identifier the backend answered with; this port
    /// has no such header, so the turn that made the request is what a rating
    /// of that request points at.
    last_correlation_id: Option<String>,
    /// Tool calls the model declared and that have not answered yet, by
    /// identifier. The reference reads the same pair off its `ResolvedToolCall`.
    calls: BTreeMap<String, PendingToolCall>,
}

/// What a tool event reports for a fact no request named yet. The reference
/// always has an active model and an active profile; a stream that answers a
/// tool call before any request has none, and reports the same `unknown` its
/// absent entrypoint reports rather than an empty label.
fn known(value: &str) -> String {
    if value.is_empty() {
        return "unknown".to_owned();
    }
    value.to_owned()
}

#[derive(Debug)]
struct PendingToolCall {
    name: String,
    arguments: Value,
}

/// Where an event a client authored is handed to, so a wire dispatch can ship
/// one without holding a transport of its own.
///
/// Reference `TelemetryResource.record`, whose `telemetry/record` request the
/// app server hands to the agent loop's own telemetry client
/// (`vibe/app_server/_resources.py:488-499`). The name and the properties are
/// the client's, so neither is rewritten here: only the census is merged
/// underneath them, and the caller's keys win. The reference validates nothing
/// on this path either, which is why the label validators this port applies to
/// its own events are absent from it, asserted by
/// `authored_labels_are_validated_and_client_properties_are_not`.
pub trait ClientTelemetry: Send + Sync {
    fn record_client_event(
        &self,
        name: &str,
        properties: Map<String, Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    );
}

/// The sink a server with no telemetry client installed answers with, which
/// keeps `telemetry/record` answering empty rather than failing.
pub struct NoClientTelemetry;

impl ClientTelemetry for NoClientTelemetry {
    fn record_client_event(
        &self,
        _name: &str,
        _properties: Map<String, Value>,
        _session_id: Option<&str>,
        _correlate_last_request: bool,
    ) {
    }
}

pub struct TelemetryEventObserver<T> {
    client: Arc<TelemetryClient<T>>,
    context: TelemetryContext,
    pending: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    turn: Mutex<TurnContext>,
}

impl<T> TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    #[must_use]
    pub fn new(client: TelemetryClient<T>, context: TelemetryContext) -> Self {
        Self {
            client: Arc::new(client),
            context,
            pending: Mutex::new(Vec::new()),
            turn: Mutex::new(TurnContext::default()),
        }
    }

    /// Queues one event raised outside the engine stream, which is how every
    /// client surface reports: the rating prompt, the slash commands, the voice
    /// toggle, the two audio managers and the teleport tracker.
    pub fn record(
        &self,
        record: &TelemetryRecord,
        session_id: Option<&str>,
    ) -> Result<(), TelemetryError> {
        let correlation_id = self.correlation(record.correlates_last_request());
        self.queue(record, session_id, correlation_id)
    }

    /// Builds the envelope and hands the delivery to a task, so neither the
    /// configuration read nor the request touches the caller's path.
    fn queue(
        &self,
        record: &TelemetryRecord,
        session_id: Option<&str>,
        correlation_id: Option<String>,
    ) -> Result<(), TelemetryError> {
        if let Some(correlation_id) = correlation_id.as_deref() {
            validate_safe_identifier(correlation_id)?;
        }
        let census = match record.call_type() {
            Some(call_type) => self
                .context
                .request_metadata(session_id, call_type, None)
                .properties(),
            None => self.context.base_metadata(session_id).properties(),
        };
        let attributes = record.attributes(self.context.launch.as_ref())?;
        let envelope = TelemetryEnvelope::new(
            record.event().event_name(),
            merge_properties(census, attributes.into_properties()),
            correlation_id,
        );
        self.deliver(envelope);
        Ok(())
    }

    /// Hands one envelope to a task and remembers it, so [`Self::flush`] still
    /// awaits a delivery the caller never sees.
    fn deliver(&self, envelope: TelemetryEnvelope) {
        // Telemetry never decides whether a caller runs: an observer reached
        // from outside a runtime drops the delivery rather than failing the
        // path that produced the event.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = Arc::clone(&self.client);
        let task = runtime.spawn(async move {
            let _ = client.record(&envelope).await;
        });
        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|task| !task.is_finished());
            pending.push(task);
        }
    }

    pub async fn flush(&self) {
        let pending = self
            .pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for task in pending {
            let _ = task.await;
        }
    }

    /// What a request that asked to correlate points at: the turn that made the
    /// last backend request, which is this port's `last_correlation_id`.
    fn correlation(&self, correlate_last_request: bool) -> Option<String> {
        if !correlate_last_request {
            return None;
        }
        self.turn
            .lock()
            .ok()
            .and_then(|turn| turn.last_correlation_id.clone())
    }

    /// The records one engine event reports, with the turn context updated by
    /// the events that carry it.
    ///
    /// Most events report one or none; a compaction reports two when its
    /// summarization failed with a classified reason, because the reference
    /// sends the auto-compaction record from the loop and the failure record
    /// from the manager, for the same compaction.
    fn project(&self, event: &EventEnvelope) -> Vec<TelemetryRecord> {
        let Ok(mut turn) = self.turn.lock() else {
            return Vec::new();
        };
        match &event.event {
            EngineEvent::RequestSent {
                model,
                agent_profile,
                nb_context_chars,
                nb_context_messages,
                nb_prompt_chars,
                nb_images,
                supports_images,
                message_id,
            } => {
                turn.model.clone_from(model);
                turn.agent_profile.clone_from(agent_profile);
                turn.message_id.clone_from(message_id);
                turn.last_correlation_id.clone_from(&event.turn_id);
                vec![TelemetryRecord::RequestSent(records::RequestSent {
                    model: model.clone(),
                    nb_context_chars: *nb_context_chars,
                    nb_context_messages: *nb_context_messages,
                    nb_prompt_chars: *nb_prompt_chars,
                    // Every request this port makes is the turn's own. The
                    // summarization request is the reference's only secondary
                    // call, and `docs/parity.md` records that it carries no
                    // census here.
                    call_type: TelemetryCallType::MainCall,
                    message_id: message_id.clone(),
                    attachment_counts: attachment_counts(*nb_images as usize, *supports_images),
                })]
            }
            EngineEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                turn.calls.insert(
                    call_id.clone(),
                    PendingToolCall {
                        name: name.clone(),
                        arguments: serde_json::from_str(arguments).unwrap_or(Value::Null),
                    },
                );
                Vec::new()
            }
            EngineEvent::ToolResult {
                call_id,
                content,
                typed_result,
                is_error,
                ..
            } => {
                let Some(call) = turn.calls.remove(call_id) else {
                    return Vec::new();
                };
                // Reference `_ask_approval`: a refusal is a `skip` verdict on an
                // `ask` permission, and the event it produces is `skipped`
                // rather than a failure. The refusal crosses the tool boundary
                // as the message the policy denied with, which is the only
                // thing that tells it from a tool that failed on its own.
                let declined = *is_error && content.starts_with(crate::policy::DENIAL_PREFIX);
                let status = match (declined, *is_error) {
                    (true, _) => records::TelemetryToolStatus::Skipped,
                    (false, true) => records::TelemetryToolStatus::Failure,
                    (false, false) => records::TelemetryToolStatus::Success,
                };
                let decision = declined.then_some(records::ToolDecision {
                    verdict: records::TelemetryToolVerdict::Skip,
                    approval_type: records::TelemetryApprovalType::Ask,
                });
                vec![TelemetryRecord::ToolCallFinished(
                    records::ToolCallFinished::new(records::ToolCallReport {
                        tool_name: &call.name,
                        status,
                        arguments: &call.arguments,
                        result: (!*is_error).then_some(typed_result),
                        decision,
                        agent_profile_name: &known(&turn.agent_profile),
                        model: &known(&turn.model),
                        message_id: turn.message_id.clone(),
                    }),
                )]
            }
            EngineEvent::CompactionOutcome {
                status,
                context_tokens_before,
                threshold,
                reason,
                ..
            } => {
                let compaction = TelemetryRecord::AutoCompactTriggered {
                    nb_context_tokens_before: *context_tokens_before,
                    auto_compact_threshold: *threshold,
                    status: status.label(),
                };
                match reason {
                    // The reference's failure record carries the classified
                    // reason and nothing else: no prompt, no transcript, no
                    // summary text.
                    Some(reason) => vec![
                        compaction,
                        TelemetryRecord::CompactionFailed {
                            reason: reason.label(),
                        },
                    ],
                    None => vec![compaction],
                }
            }
            // The variant this port emitted before the boundary pair existed.
            // Nothing emits it any more; a transcript that carries one still
            // reports the compaction it recorded, without a status it never
            // held.
            EngineEvent::Compaction { .. } => vec![TelemetryRecord::AutoCompactTriggered {
                nb_context_tokens_before: 0,
                auto_compact_threshold: 0,
                status: CompactionStatus::Success.label(),
            }],
            _ => Vec::new(),
        }
    }
}

impl<T> ClientTelemetry for TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    /// Reference `TelemetryClient.send_telemetry_event`: the census first, the
    /// client's own properties second, and a correlation id only when the
    /// caller asked for one and a request has already been made.
    fn record_client_event(
        &self,
        name: &str,
        properties: Map<String, Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    ) {
        let census = self.context.base_metadata(session_id).properties();
        self.deliver(TelemetryEnvelope::new(
            name,
            merge_properties(census, properties),
            self.correlation(correlate_last_request),
        ));
    }
}

impl<T> EventObserver for TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    /// A projection failure never reaches the turn: an event whose label the
    /// validators refuse is dropped, on the same terms as a delivery that
    /// fails, because telemetry decides nothing about whether a caller runs.
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        for record in self.project(event) {
            let _ = self.queue(&record, Some(&event.session_id), None);
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TelemetryError {
    #[error("telemetry label is not an allowed bounded identifier")]
    UnsafeLabel,
    #[error("telemetry correlation identifier is invalid")]
    InvalidIdentifier,
    #[error("telemetry credential cannot be represented as an authorization header")]
    InvalidCredential,
    #[error("telemetry user agent cannot be represented as a header")]
    InvalidAgent,
    #[error("telemetry transport could not be initialized")]
    TransportSetup,
    #[error("telemetry delivery failed")]
    Delivery,
    #[error("telemetry endpoint rejected the event with HTTP {0}")]
    Rejected(u16),
}

fn validate_safe_label(value: &str) -> Result<(), TelemetryError> {
    let lowercase = value.to_ascii_lowercase();
    if value.is_empty()
        || value.len() > MAX_SAFE_LABEL_BYTES
        || ["sk-", "token", "secret", "password", "credential"]
            .iter()
            .any(|pattern| lowercase.contains(pattern))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(TelemetryError::UnsafeLabel);
    }
    Ok(())
}

fn validate_safe_identifier(value: &str) -> Result<(), TelemetryError> {
    if value.is_empty()
        || value.len() > MAX_SAFE_LABEL_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(TelemetryError::InvalidIdentifier);
    }
    Ok(())
}

#[cfg(test)]
mod telemetry_parity_tests;

#[cfg(test)]
mod telemetry_tests;
