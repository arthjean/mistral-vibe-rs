use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::engine::EventObserver;
use crate::events::{EngineEvent, EventEnvelope, LifecycleState};

pub const TELEMETRY_SCHEMA_VERSION: u32 = 1;
pub const TELEMETRY_PATH: &str = "/v1/datalake/events";
/// Wall-clock ceiling on one delivery, matching the reference's 5.0 second
/// `httpx.Timeout`. Named rather than inlined so the parity replay reads the
/// value the transport actually uses.
pub const TELEMETRY_TIMEOUT_SECONDS: u64 = 5;
/// Idle connections kept per host, matching the reference's
/// `max_keepalive_connections`.
pub const TELEMETRY_MAX_KEEPALIVE_CONNECTIONS: usize = 5;
/// The user agent a delivery identifies itself with.
pub const TELEMETRY_USER_AGENT: &str = concat!("mistral-vibe-rs/", env!("CARGO_PKG_VERSION"));
/// The scheme the credential is presented under.
pub const TELEMETRY_AUTHORIZATION_SCHEME: &str = "Bearer";
const MAX_SAFE_LABEL_BYTES: usize = 128;

/// The headers one delivery carries, built from the credential it authenticates
/// with.
///
/// Split out of the transport so the header set is observable without issuing a
/// request, which is what the parity replay compares.
pub fn telemetry_headers(credential: &SecretString) -> Result<HeaderMap, TelemetryError> {
    let authorization = HeaderValue::from_str(&format!(
        "{TELEMETRY_AUTHORIZATION_SCHEME} {}",
        credential.expose_secret()
    ))
    .map_err(|_| TelemetryError::InvalidCredential)?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(USER_AGENT, HeaderValue::from_static(TELEMETRY_USER_AGENT));
    Ok(headers)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryEvent {
    NewSession,
    SessionClosed,
    Ready,
    RequestSent,
    ToolCallFinished,
    AtMentionInserted,
    AutoCompactTriggered,
    CompactionFailed,
    TeleportCompleted,
    TeleportFailed,
    FeedbackSubmitted,
    RemoteProjectConfigured,
}

impl TelemetryEvent {
    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::NewSession => "vibe.new_session",
            Self::SessionClosed => "vibe.session_closed",
            Self::Ready => "vibe.ready",
            Self::RequestSent => "vibe.request_sent",
            Self::ToolCallFinished => "vibe.tool_call_finished",
            Self::AtMentionInserted => "vibe.at_mention_inserted",
            Self::AutoCompactTriggered => "vibe.auto_compact_triggered",
            Self::CompactionFailed => "vibe.compaction_failed",
            Self::TeleportCompleted => "vibe.teleport_completed",
            Self::TeleportFailed => "vibe.teleport_failed",
            Self::FeedbackSubmitted => "vibe.user_rating_feedback",
            Self::RemoteProjectConfigured => "vibe.remote_project_configured",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryField {
    Action,
    AgentProfile,
    ApprovalType,
    AttachmentCount,
    AutoCompactThreshold,
    CallType,
    ClientName,
    ClientVersion,
    CommandType,
    ContextMessageCount,
    ContextTokensBefore,
    Decision,
    DurationMs,
    Entrypoint,
    FailureKind,
    FileCount,
    FileExtension,
    MentionKind,
    Model,
    Platform,
    PromptCharacterCount,
    Rating,
    Reason,
    Stage,
    Status,
    ToolName,
    Version,
}

impl TelemetryField {
    const fn key(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::AgentProfile => "agent_profile",
            Self::ApprovalType => "approval_type",
            Self::AttachmentCount => "attachment_count",
            Self::AutoCompactThreshold => "auto_compact_threshold",
            Self::CallType => "call_type",
            Self::ClientName => "client_name",
            Self::ClientVersion => "client_version",
            Self::CommandType => "command_type",
            Self::ContextMessageCount => "context_message_count",
            Self::ContextTokensBefore => "nb_context_tokens_before",
            Self::Decision => "decision",
            Self::DurationMs => "duration_ms",
            Self::Entrypoint => "entrypoint",
            Self::FailureKind => "failure_kind",
            Self::FileCount => "file_count",
            Self::FileExtension => "file_extension",
            Self::MentionKind => "mention_kind",
            Self::Model => "model",
            Self::Platform => "platform",
            Self::PromptCharacterCount => "prompt_character_count",
            Self::Rating => "rating",
            Self::Reason => "reason",
            Self::Stage => "stage",
            Self::Status => "status",
            Self::ToolName => "tool_name",
            Self::Version => "version",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TelemetryValue {
    Label(String),
    Count(u64),
    Flag(bool),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TelemetryAttributes(BTreeMap<String, TelemetryValue>);

impl TelemetryAttributes {
    pub fn label(
        &mut self,
        field: TelemetryField,
        value: impl Into<String>,
    ) -> Result<&mut Self, TelemetryError> {
        let value = value.into();
        validate_safe_label(&value)?;
        self.0
            .insert(field.key().to_owned(), TelemetryValue::Label(value));
        Ok(self)
    }

    pub fn count(&mut self, field: TelemetryField, value: u64) -> &mut Self {
        self.0
            .insert(field.key().to_owned(), TelemetryValue::Count(value));
        self
    }

    pub fn flag(&mut self, field: TelemetryField, value: bool) -> &mut Self {
        self.0
            .insert(field.key().to_owned(), TelemetryValue::Flag(value));
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryMetadata {
    pub product: String,
    pub version: String,
    pub platform: String,
    pub entrypoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

impl TelemetryMetadata {
    pub fn new(
        version: impl Into<String>,
        platform: impl Into<String>,
        entrypoint: impl Into<String>,
    ) -> Result<Self, TelemetryError> {
        let version = version.into();
        let platform = platform.into();
        let entrypoint = entrypoint.into();
        validate_safe_label(&version)?;
        validate_safe_label(&platform)?;
        validate_safe_label(&entrypoint)?;
        Ok(Self {
            product: "mistral-vibe-rs".to_owned(),
            version,
            platform,
            entrypoint,
            client_name: None,
            client_version: None,
        })
    }

    pub fn with_client(
        mut self,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, TelemetryError> {
        let name = name.into();
        let version = version.into();
        validate_safe_label(&name)?;
        validate_safe_label(&version)?;
        self.client_name = Some(name);
        self.client_version = Some(version);
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryProperties {
    pub metadata: TelemetryMetadata,
    #[serde(default, skip_serializing_if = "TelemetryAttributes::is_empty")]
    pub attributes: TelemetryAttributes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEnvelope {
    pub schema_version: u32,
    pub event: String,
    pub properties: TelemetryProperties,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl TelemetryEnvelope {
    pub fn new(
        event: TelemetryEvent,
        metadata: TelemetryMetadata,
        attributes: TelemetryAttributes,
        correlation_id: Option<String>,
    ) -> Result<Self, TelemetryError> {
        if let Some(correlation_id) = correlation_id.as_deref() {
            validate_safe_identifier(correlation_id)?;
        }
        Ok(Self {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            event: event.event_name().to_owned(),
            properties: TelemetryProperties {
                metadata,
                attributes,
            },
            correlation_id,
        })
    }
}

pub struct TelemetryConfig {
    enabled: bool,
    endpoint: Url,
    credential: Option<SecretString>,
}

impl TelemetryConfig {
    pub fn new(
        enabled: bool,
        api_base: &Url,
        mistral_credential: Option<SecretString>,
    ) -> Result<Self, TelemetryError> {
        if api_base.scheme() != "https"
            || api_base.cannot_be_a_base()
            || api_base.host_str().is_none()
            || api_base.username() != ""
            || api_base.password().is_some()
        {
            return Err(TelemetryError::UntrustedEndpoint);
        }
        let mut endpoint = api_base.clone();
        endpoint.set_path(TELEMETRY_PATH);
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        Ok(Self {
            enabled,
            endpoint,
            credential: mistral_credential,
        })
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.credential.is_some()
    }
}

pub type TelemetryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), TelemetryError>> + Send + 'a>>;

pub trait TelemetryTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        endpoint: &'a Url,
        credential: &'a SecretString,
        envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a>;
}

#[derive(Clone)]
pub struct ReqwestTelemetryTransport {
    client: reqwest::Client,
}

impl ReqwestTelemetryTransport {
    pub fn try_new() -> Result<Self, TelemetryError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(TELEMETRY_TIMEOUT_SECONDS))
            .pool_max_idle_per_host(TELEMETRY_MAX_KEEPALIVE_CONNECTIONS)
            .build()
            .map_err(|_| TelemetryError::TransportSetup)?;
        Ok(Self { client })
    }
}

impl TelemetryTransport for ReqwestTelemetryTransport {
    fn send<'a>(
        &'a self,
        endpoint: &'a Url,
        credential: &'a SecretString,
        envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .post(endpoint.clone())
                .headers(telemetry_headers(credential)?)
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

pub struct TelemetryClient<T> {
    config: TelemetryConfig,
    transport: T,
}

impl<T: TelemetryTransport> TelemetryClient<T> {
    #[must_use]
    pub fn new(config: TelemetryConfig, transport: T) -> Self {
        Self { config, transport }
    }

    pub async fn record(
        &self,
        envelope: &TelemetryEnvelope,
    ) -> Result<TelemetryOutcome, TelemetryError> {
        if !self.config.enabled {
            return Ok(TelemetryOutcome::Disabled);
        }
        let Some(credential) = self.config.credential.as_ref() else {
            return Ok(TelemetryOutcome::NoEligibleCredential);
        };
        self.transport
            .send(&self.config.endpoint, credential, envelope)
            .await?;
        Ok(TelemetryOutcome::Sent)
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.config.is_active()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryOutcome {
    Disabled,
    NoEligibleCredential,
    Sent,
}

pub struct TelemetryProjection {
    pub event: TelemetryEvent,
    pub attributes: TelemetryAttributes,
    pub correlation_id: Option<String>,
}

pub struct TelemetryEventObserver<T> {
    client: Arc<TelemetryClient<T>>,
    metadata: TelemetryMetadata,
    pending: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl<T> TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    #[must_use]
    pub fn new(client: TelemetryClient<T>, metadata: TelemetryMetadata) -> Self {
        Self {
            client: Arc::new(client),
            metadata,
            pending: Mutex::new(Vec::new()),
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
}

impl<T> EventObserver for TelemetryEventObserver<T>
where
    T: TelemetryTransport + 'static,
{
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        if !self.client.is_active() {
            return Ok(());
        }
        let projections =
            TelemetryProjection::from_engine_event(event).map_err(|error| error.to_string())?;
        for projection in projections {
            let envelope = TelemetryEnvelope::new(
                projection.event,
                self.metadata.clone(),
                projection.attributes,
                projection.correlation_id,
            )
            .map_err(|error| error.to_string())?;
            let client = Arc::clone(&self.client);
            let task = tokio::spawn(async move {
                let _ = client.record(&envelope).await;
            });
            if let Ok(mut pending) = self.pending.lock() {
                pending.retain(|task| !task.is_finished());
                pending.push(task);
            }
        }
        Ok(())
    }
}

impl TelemetryProjection {
    /// The records one engine event reports.
    ///
    /// Most events report one or none; a compaction reports two when its
    /// summarization failed with a classified reason, because the reference
    /// sends the auto-compaction record from the loop and the failure record
    /// from the manager, for the same compaction.
    pub fn from_engine_event(event: &EventEnvelope) -> Result<Vec<Self>, TelemetryError> {
        let mut attributes = TelemetryAttributes::default();
        let projected = match &event.event {
            EngineEvent::ToolResult {
                duration_ms,
                is_error,
                cancelled,
                ..
            } => {
                attributes
                    .count(TelemetryField::DurationMs, *duration_ms)
                    .label(
                        TelemetryField::Status,
                        if *cancelled {
                            "cancelled"
                        } else if *is_error {
                            "failure"
                        } else {
                            "success"
                        },
                    )?;
                Some(TelemetryEvent::ToolCallFinished)
            }
            EngineEvent::CompactionOutcome {
                status,
                context_tokens_before,
                threshold,
                reason,
                ..
            } => {
                attributes
                    .count(TelemetryField::ContextTokensBefore, *context_tokens_before)
                    .count(TelemetryField::AutoCompactThreshold, *threshold)
                    .label(TelemetryField::Status, status.label())?;
                if let Some(reason) = reason {
                    // The reference's failure record carries the classified
                    // reason and nothing else: no prompt, no transcript, no
                    // summary text.
                    let mut failure = TelemetryAttributes::default();
                    failure.label(TelemetryField::Reason, reason.label())?;
                    return Ok(vec![
                        Self {
                            event: TelemetryEvent::AutoCompactTriggered,
                            attributes,
                            correlation_id: event.turn_id.clone(),
                        },
                        Self {
                            event: TelemetryEvent::CompactionFailed,
                            attributes: failure,
                            correlation_id: event.turn_id.clone(),
                        },
                    ]);
                }
                Some(TelemetryEvent::AutoCompactTriggered)
            }
            // The variant this port emitted before the boundary pair existed.
            // Nothing emits it any more; a transcript that carries one still
            // reports the compaction it recorded, without a status it never
            // held.
            EngineEvent::Compaction { .. } => Some(TelemetryEvent::AutoCompactTriggered),
            EngineEvent::Lifecycle { state, .. } => match state {
                LifecycleState::Running => {
                    attributes.label(TelemetryField::Status, lifecycle_label(*state))?;
                    Some(TelemetryEvent::RequestSent)
                }
                LifecycleState::Idle
                | LifecycleState::WaitingCallback
                | LifecycleState::Completed
                | LifecycleState::Failed
                | LifecycleState::Cancelled => None,
            },
            _ => None,
        };
        Ok(projected
            .map(|projected| Self {
                event: projected,
                attributes,
                correlation_id: event.turn_id.clone(),
            })
            .into_iter()
            .collect())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TelemetryError {
    #[error("telemetry label is not an allowed bounded identifier")]
    UnsafeLabel,
    #[error("telemetry correlation identifier is invalid")]
    InvalidIdentifier,
    #[error("telemetry endpoint is not the Mistral HTTPS event endpoint")]
    UntrustedEndpoint,
    #[error("telemetry credential cannot be represented as an authorization header")]
    InvalidCredential,
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

const fn lifecycle_label(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Idle => "idle",
        LifecycleState::Running => "running",
        LifecycleState::WaitingCallback => "waiting_callback",
        LifecycleState::Completed => "completed",
        LifecycleState::Failed => "failed",
        LifecycleState::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod telemetry_parity_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::compaction::{CompactionFailureReason, CompactionStatus};

    struct CountingTransport {
        calls: AtomicUsize,
    }

    impl TelemetryTransport for CountingTransport {
        fn send<'a>(
            &'a self,
            _endpoint: &'a Url,
            _credential: &'a SecretString,
            _envelope: &'a TelemetryEnvelope,
        ) -> TelemetryFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct SharedCountingTransport {
        calls: Arc<AtomicUsize>,
    }

    impl TelemetryTransport for SharedCountingTransport {
        fn send<'a>(
            &'a self,
            _endpoint: &'a Url,
            _credential: &'a SecretString,
            _envelope: &'a TelemetryEnvelope,
        ) -> TelemetryFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn metadata() -> TelemetryMetadata {
        TelemetryMetadata::new("2.23.1", "linux-x86_64", "cli").expect("safe metadata")
    }

    fn endpoint() -> Url {
        Url::parse("https://api.mistral.ai/v1/chat/completions").expect("valid URL")
    }

    fn envelope() -> TelemetryEnvelope {
        let mut attributes = TelemetryAttributes::default();
        attributes
            .label(TelemetryField::ToolName, "read_file")
            .expect("safe tool name")
            .count(TelemetryField::DurationMs, 7);
        TelemetryEnvelope::new(
            TelemetryEvent::ToolCallFinished,
            metadata(),
            attributes,
            Some("turn-1".to_owned()),
        )
        .expect("safe envelope")
    }

    #[tokio::test]
    async fn disabled_and_ineligible_telemetry_create_no_send() {
        for (enabled, credential, expected) in [
            (
                false,
                Some(SecretString::from("secret")),
                TelemetryOutcome::Disabled,
            ),
            (true, None, TelemetryOutcome::NoEligibleCredential),
        ] {
            let transport = CountingTransport {
                calls: AtomicUsize::new(0),
            };
            let client = TelemetryClient::new(
                TelemetryConfig::new(enabled, &endpoint(), credential).expect("config"),
                transport,
            );
            assert_eq!(client.record(&envelope()).await.expect("outcome"), expected);
            assert_eq!(client.transport.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn eligible_events_use_the_versioned_safe_envelope() {
        let transport = CountingTransport {
            calls: AtomicUsize::new(0),
        };
        let client = TelemetryClient::new(
            TelemetryConfig::new(
                true,
                &endpoint(),
                Some(SecretString::from("eligible-credential")),
            )
            .expect("config"),
            transport,
        );
        assert_eq!(
            client.record(&envelope()).await.expect("sent"),
            TelemetryOutcome::Sent
        );
        assert_eq!(client.transport.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_value(envelope()).expect("JSON"),
            json!({
                "schemaVersion": 1,
                "event": "vibe.tool_call_finished",
                "properties": {
                    "metadata": {
                        "product": "mistral-vibe-rs",
                        "version": "2.23.1",
                        "platform": "linux-x86_64",
                        "entrypoint": "cli"
                    },
                    "attributes": {
                        "duration_ms": 7,
                        "tool_name": "read_file"
                    }
                },
                "correlationId": "turn-1"
            })
        );
    }

    #[test]
    fn unsafe_content_has_no_telemetry_representation() {
        for sensitive in [
            "/home/arthur/private.rs",
            "sk-secret",
            "https://user:password@proxy.invalid",
            "exception message",
            "tool\noutput",
        ] {
            let mut attributes = TelemetryAttributes::default();
            assert_eq!(
                attributes.label(TelemetryField::Model, sensitive),
                Err(TelemetryError::UnsafeLabel)
            );
        }
        let encoded = serde_json::to_string(&envelope()).expect("JSON");
        for forbidden in ["prompt", "file_content", "proxy", "exception", "output"] {
            assert!(!encoded.contains(forbidden), "{encoded}");
        }
    }

    #[test]
    fn endpoint_and_correlation_boundaries_fail_closed() {
        let foreign = Url::parse("http://telemetry.invalid").expect("URL");
        assert!(matches!(
            TelemetryConfig::new(true, &foreign, Some(SecretString::from("secret"))),
            Err(TelemetryError::UntrustedEndpoint)
        ));
        assert_eq!(
            TelemetryEnvelope::new(
                TelemetryEvent::Ready,
                metadata(),
                TelemetryAttributes::default(),
                Some("../session".to_owned())
            ),
            Err(TelemetryError::InvalidIdentifier)
        );
        let credential_in_url = Url::parse("https://user:password@api.mistral.ai").expect("URL");
        assert!(matches!(
            TelemetryConfig::new(true, &credential_in_url, Some(SecretString::from("secret"))),
            Err(TelemetryError::UntrustedEndpoint)
        ));
    }

    #[test]
    fn engine_projection_never_copies_event_content() {
        let event = EventEnvelope {
            session_id: "session".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::ToolResult {
                call_id: "call".to_owned(),
                content: "sk-secret /home/arthur/private".to_owned(),
                typed_result: json!({"secret": "token"}),
                display: json!({"output": "private"}),
                duration_ms: 9,
                is_error: true,
                cancelled: false,
            },
        };
        let projection = TelemetryProjection::from_engine_event(&event)
            .expect("projection")
            .pop()
            .expect("telemetry event");
        let envelope = TelemetryEnvelope::new(
            projection.event,
            metadata(),
            projection.attributes,
            projection.correlation_id,
        )
        .expect("envelope");
        let encoded = serde_json::to_string(&envelope).expect("JSON");
        assert!(!encoded.contains("sk-secret"));
        assert!(!encoded.contains("/home/arthur"));
        assert!(!encoded.contains("private"));
    }

    #[test]
    fn turn_completion_is_not_misreported_as_session_close() {
        let event = EventEnvelope {
            session_id: "session".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::Lifecycle {
                state: LifecycleState::Completed,
                message: None,
            },
        };
        assert!(
            TelemetryProjection::from_engine_event(&event)
                .expect("projection")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn engine_observer_delivers_supported_events_and_flushes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = TelemetryClient::new(
            TelemetryConfig::new(
                true,
                &endpoint(),
                Some(SecretString::from("eligible-credential")),
            )
            .expect("config"),
            SharedCountingTransport {
                calls: Arc::clone(&calls),
            },
        );
        let observer = TelemetryEventObserver::new(client, metadata());
        observer
            .observe(&EventEnvelope {
                session_id: "session".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 1,
                event_id: 1,
                event: EngineEvent::Lifecycle {
                    state: LifecycleState::Running,
                    message: None,
                },
            })
            .expect("observe");
        observer.flush().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn compaction_outcome(
        status: CompactionStatus,
        reason: Option<CompactionFailureReason>,
    ) -> EventEnvelope {
        EventEnvelope {
            session_id: "session".to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::CompactionOutcome {
                compaction_id: "compaction-1".to_owned(),
                status,
                context_tokens_before: 150_000,
                threshold: 120_000,
                reason,
            },
        }
    }

    /// US-151: the auto-compaction record carries the tokens before, the
    /// threshold and the status, which is the reference's payload.
    #[test]
    fn a_compaction_outcome_reports_the_reference_payload() {
        for (status, label) in [
            (CompactionStatus::Success, "success"),
            (CompactionStatus::Failure, "failure"),
            (CompactionStatus::Cancelled, "cancelled"),
        ] {
            let projections =
                TelemetryProjection::from_engine_event(&compaction_outcome(status, None))
                    .expect("projection");
            let [record] = projections.as_slice() else {
                panic!("an unclassified outcome reports one record");
            };
            assert_eq!(record.event, TelemetryEvent::AutoCompactTriggered);
            let encoded = serde_json::to_value(&record.attributes).expect("attributes");
            assert_eq!(encoded["nb_context_tokens_before"], 150_000);
            assert_eq!(encoded["auto_compact_threshold"], 120_000);
            assert_eq!(encoded["status"], label);
        }
    }

    /// US-151: a classified failure reports the failure record too, carrying
    /// the reason and nothing else.
    #[test]
    fn a_classified_failure_reports_both_records() {
        let projections = TelemetryProjection::from_engine_event(&compaction_outcome(
            CompactionStatus::Failure,
            Some(CompactionFailureReason::ToolCall),
        ))
        .expect("projection");
        let [triggered, failed] = projections.as_slice() else {
            panic!("a classified failure reports two records");
        };
        assert_eq!(triggered.event, TelemetryEvent::AutoCompactTriggered);
        assert_eq!(failed.event, TelemetryEvent::CompactionFailed);
        assert_eq!(
            serde_json::to_value(&failed.attributes).expect("attributes"),
            json!({"reason": "tool_call"}),
            "the failure record carries the reason and no transcript"
        );
    }

    /// US-151: telemetry disabled writes nothing, whatever the compaction did.
    #[tokio::test]
    async fn a_disabled_client_writes_no_compaction_record() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = TelemetryClient::new(
            TelemetryConfig::new(false, &endpoint(), Some(SecretString::from("key")))
                .expect("config"),
            SharedCountingTransport {
                calls: Arc::clone(&calls),
            },
        );
        let observer = TelemetryEventObserver::new(client, metadata());
        observer
            .observe(&compaction_outcome(
                CompactionStatus::Failure,
                Some(CompactionFailureReason::EmptySummary),
            ))
            .expect("observe");
        observer.flush().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
