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

use thiserror::Error;

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
/// Reference `TelemetryRequestMetadata.host_kind`.
pub const TELEMETRY_HOST_KIND: &str = "local";
/// The only attachment kind the reference counts. Reference `AttachmentKind`.
pub const TELEMETRY_ATTACHMENT_IMAGE: &str = "image";
/// The version the user agent and the `version` census field report.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version this build reports about itself, which is what an event census
/// and an experiment attribute both carry. Reference `vibe.__version__`.
#[must_use]
pub const fn version() -> &'static str {
    VERSION
}
const MAX_SAFE_LABEL_BYTES: usize = 128;

mod census;
mod context;
mod delivery;
mod host;
mod observer;
pub mod records;
mod target;
mod vocabulary;

pub use census::{
    ExperimentAssignment, ExperimentCensus, HARNESS_LEGACY, HARNESS_UNIFIED, LaunchContext,
    TelemetryBaseMetadata, TelemetryCallType, TelemetryRequestMetadata, attachment_counts,
    harness_backend,
};
pub use context::{
    DeclaredLaunch, ExperimentExposures, SessionParents, SessionReset, SessionResets,
    TelemetryContext,
};
pub use delivery::{
    ReqwestTelemetryTransport, TelemetryEnvelope, TelemetryFuture, TelemetryTransport,
    merge_properties, telemetry_headers,
};
#[cfg(test)]
pub(crate) use host::terminal_emulator_from;
pub use host::{detect_terminal_emulator, platform_arch, platform_id, platform_version};
pub use observer::{
    ClientTelemetry, NoClientTelemetry, TelemetryClient, TelemetryConfigGetter,
    TelemetryEventObserver, TelemetryOutcome,
};
pub use records::TelemetryRecord;
pub use target::{
    TelemetryConfig, TelemetryTarget, is_active_model_mistral, mistral_provider,
    telemetry_endpoint, telemetry_user_agent,
};
pub use vocabulary::{TelemetryAttributes, TelemetryEvent, TelemetryField};

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
