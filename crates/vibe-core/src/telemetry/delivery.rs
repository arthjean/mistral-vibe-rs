//! One event on the wire and the transport that posts it.
//!
//! Reference `vibe/core/telemetry/send.py`: `send_telemetry_event` builds the
//! payload and posts it fire-and-forget through one shared `httpx` client.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use super::{
    TELEMETRY_AUTHORIZATION_SCHEME, TELEMETRY_MAX_CONNECTIONS, TELEMETRY_MAX_KEEPALIVE_CONNECTIONS,
    TELEMETRY_TIMEOUT_SECONDS, TelemetryError,
};

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
