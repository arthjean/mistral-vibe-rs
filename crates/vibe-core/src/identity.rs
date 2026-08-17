//! Who the caller is, as the Mistral API answers it.
//!
//! One endpoint, `GET {base}/users/me`, read for one reason here: a rollout can
//! be targeted at an organization rather than only at a hashed user, and the
//! organization identifier is what the experiment attributes carry. The read is
//! fail-open in the same strong sense the eval lookup is: an unauthorized
//! answer, an unreachable host, an unexpected status and a body the models do
//! not accept all resolve to "no identity", are reported once, and propagate
//! nothing. A caller that gets nothing back proceeds without an organization.
//!
//! [`IdentityCache`] is what keeps a session to one round trip. It is
//! single-flight per `(base URL, credential)` pair, so two callers resolving at
//! once coalesce onto one request, and it caches only successes, so a failure
//! never pins a session to an answer it could not get.
//!
//! Reference: `vibe/core/identity.py` and `vibe/core/identity_cache.py` at the
//! pinned commit.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::observability::{self, LogLevel};

/// The transport the unit tests and the parity replay stand one call before a
/// connection.
#[cfg(test)]
pub mod recorder;

#[cfg(test)]
mod identity_tests;

/// The path the caller's identity is read from, under whichever API base the
/// provider names.
///
/// Reference `_IDENTITY_PATH`.
pub const IDENTITY_PATH: &str = "/users/me";

/// The status codes that mean the credential was refused rather than the
/// service being unavailable.
const UNAUTHORIZED: u16 = 401;
const FORBIDDEN: u16 = 403;

/// A workspace or an organization, as the identity payload names one.
///
/// Reference `_IdentityEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityEntity {
    pub id: String,
    pub name: String,
}

/// The caller, as `/users/me` answers.
///
/// The reference validates this strictly while ignoring unknown fields: a field
/// it declares has to carry the declared type, and everything else the endpoint
/// adds is dropped. `serde` gives the same reading, so a payload from a newer
/// API resolves rather than failing the whole read.
///
/// Reference `IdentityResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityResult {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<IdentityEntity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<IdentityEntity>,
}

impl IdentityResult {
    /// The organization a rollout can target, or [`None`] when the caller
    /// belongs to none.
    #[must_use]
    pub fn organization_id(&self) -> Option<&str> {
        self.organization
            .as_ref()
            .map(|organization| organization.id.as_str())
    }
}

/// Why one read produced no identity.
///
/// The two arms are kept apart because the reference reports them apart: a
/// refused credential is an operator-visible fact at info level, and everything
/// else is a warning about a service.
///
/// Reference `IdentityGatewayUnauthorized` and `IdentityGatewayUnavailable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityFailure {
    /// The endpoint answered 401 or 403.
    Unauthorized,
    /// The request never completed, the status was not a success, or the body
    /// was not an identity this build can read.
    Unavailable(UnavailableCause),
}

/// What made an identity read unavailable, kept so the report names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableCause {
    /// The request never reached a response.
    Transport,
    /// The endpoint answered a status that is neither a success nor a refusal.
    Status(u16),
    /// The body was not JSON, or was JSON the identity models do not accept.
    Payload,
}

impl IdentityFailure {
    /// The level and the message this failure is reported at.
    ///
    /// The prose is this port's own: `NOTICE` forbids reproducing the
    /// reference's log lines, and what parity holds is that a refusal is
    /// reported at info and everything else at warning, not what either says.
    #[must_use]
    pub fn report(&self) -> (LogLevel, String) {
        match self {
            Self::Unauthorized => (
                LogLevel::Info,
                "identity lookup: the endpoint refused the credential".to_owned(),
            ),
            Self::Unavailable(UnavailableCause::Transport) => (
                LogLevel::Warning,
                "identity lookup: the request did not complete".to_owned(),
            ),
            Self::Unavailable(UnavailableCause::Status(status)) => (
                LogLevel::Warning,
                format!("identity lookup: the endpoint answered status {status}"),
            ),
            Self::Unavailable(UnavailableCause::Payload) => (
                LogLevel::Warning,
                "identity lookup: the endpoint answered a body this build cannot read".to_owned(),
            ),
        }
    }
}

/// One request, as it stands the instant before a connection.
///
/// The transport receives the whole request rather than a URL and a header, so
/// a recorder standing where the connection would be observes exactly what a
/// running client would have sent, its bearer header and its budget included.
#[derive(Debug)]
pub struct IdentityRequest<'a> {
    pub method: &'static str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub timeout: Option<Duration>,
}

/// What a transport read back.
#[derive(Debug, Clone)]
pub struct IdentityHttpResponse {
    pub status: u16,
    pub body: String,
}

/// The request never completed. The cause is deliberately opaque: it is
/// reported and dropped, never inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityTransportError;

pub type IdentityFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the identity request goes. The production implementation is
/// [`ReqwestIdentityTransport`]; tests and the parity replay stand a recorder
/// here so a scenario is measured one call before the connection.
pub trait IdentityTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        request: IdentityRequest<'a>,
    ) -> IdentityFuture<'a, Result<IdentityHttpResponse, IdentityTransportError>>;
}

/// The gateway `/users/me` is read through.
///
/// Reference `IdentityGateway`, whose one implementation is
/// `HttpIdentityGateway`. The trait exists here for the same reason it exists
/// there: a caller resolving an identity is not the caller that decides how it
/// travels.
pub trait IdentityGateway: Send + Sync {
    fn read<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Result<IdentityResult, IdentityFailure>>;
}

/// The HTTP gateway. Reference `HttpIdentityGateway`.
pub struct HttpIdentityGateway {
    transport: Arc<dyn IdentityTransport>,
}

impl HttpIdentityGateway {
    /// The production gateway, or [`None`] when an HTTP client cannot be built,
    /// which a fail-open caller reads as one more way to get no identity.
    #[must_use]
    pub fn production() -> Option<Self> {
        ReqwestIdentityTransport::try_new()
            .ok()
            .map(|transport| Self::with_transport(Arc::new(transport)))
    }

    /// A gateway whose transport the caller supplies, which is the seam the
    /// unit tests and the parity replay drive every branch through.
    #[must_use]
    pub fn with_transport(transport: Arc<dyn IdentityTransport>) -> Self {
        Self { transport }
    }

    /// The URL one API base resolves to.
    ///
    /// One trailing slash is stripped, as the reference strips it, so a base
    /// written with or without one addresses the same endpoint.
    #[must_use]
    pub fn url(base_url: &str) -> String {
        format!("{}{IDENTITY_PATH}", base_url.trim_end_matches('/'))
    }
}

impl IdentityGateway for HttpIdentityGateway {
    fn read<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Result<IdentityResult, IdentityFailure>> {
        Box::pin(async move {
            let url = Self::url(base_url);
            let headers = [("Authorization", format!("Bearer {api_key}"))];
            let response = self
                .transport
                .send(IdentityRequest {
                    method: "GET",
                    url: &url,
                    headers: &headers,
                    timeout,
                })
                .await
                .map_err(|_| IdentityFailure::Unavailable(UnavailableCause::Transport))?;
            if response.status == UNAUTHORIZED || response.status == FORBIDDEN {
                return Err(IdentityFailure::Unauthorized);
            }
            if !(200..300).contains(&response.status) {
                return Err(IdentityFailure::Unavailable(UnavailableCause::Status(
                    response.status,
                )));
            }
            serde_json::from_str::<IdentityResult>(&response.body)
                .map_err(|_| IdentityFailure::Unavailable(UnavailableCause::Payload))
        })
    }
}

/// The caller's identity, or [`None`].
///
/// Every failure is reported once and dropped, so a consumer that only needs an
/// organization identifier never learns why it did not get one and behaves the
/// same either way.
///
/// Reference `fetch_identity`.
pub async fn fetch_identity(
    gateway: &dyn IdentityGateway,
    base_url: &str,
    api_key: &str,
    timeout: Option<Duration>,
) -> Option<IdentityResult> {
    match gateway.read(base_url, api_key, timeout).await {
        Ok(identity) => Some(identity),
        Err(failure) => {
            let (level, message) = failure.report();
            observability::log(level, &message);
            None
        }
    }
}

/// One session's identity, fetched at most once per `(base URL, credential)`.
///
/// The lock is held across the fetch, which is what makes the cache
/// single-flight: a second caller arriving while the first is in flight waits
/// and reads the answer rather than issuing its own request. Only a success is
/// stored, so a failed read leaves a later caller free to retry.
///
/// Reference `IdentityCache`.
#[derive(Debug, Default)]
pub struct IdentityCache {
    entries: tokio::sync::Mutex<BTreeMap<(String, String), IdentityResult>>,
}

impl IdentityCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached identity, or the one a fetch resolves.
    ///
    /// Reference `IdentityCache.resolve`.
    pub async fn resolve(
        &self,
        gateway: &dyn IdentityGateway,
        base_url: &str,
        api_key: &str,
        timeout: Option<Duration>,
    ) -> Option<IdentityResult> {
        let key = (base_url.to_owned(), api_key.to_owned());
        let mut entries = self.entries.lock().await;
        if let Some(cached) = entries.get(&key) {
            return Some(cached.clone());
        }
        let resolved = fetch_identity(gateway, base_url, api_key, timeout).await;
        if let Some(identity) = resolved.as_ref() {
            entries.insert(key, identity.clone());
        }
        resolved
    }

    /// An already-fetched identity, without fetching one.
    ///
    /// Reference `IdentityCache.peek`.
    pub async fn peek(&self, base_url: &str, api_key: &str) -> Option<IdentityResult> {
        self.entries
            .lock()
            .await
            .get(&(base_url.to_owned(), api_key.to_owned()))
            .cloned()
    }
}

/// How a caller that only needs an organization identifier asks for one.
///
/// The reference passes `IdentityCache.resolve` itself where this port passes
/// an implementation, which is what lets the session gates be driven over a
/// recorder that counts requests without a service to count them against.
pub trait IdentityResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Option<IdentityResult>>;
}

/// The production resolver: one gateway behind one session-scoped cache.
///
/// Reference passes the session's `IdentityCache.resolve` bound method, which
/// is this pairing.
pub struct CachedIdentity {
    gateway: Arc<dyn IdentityGateway>,
    cache: Arc<IdentityCache>,
}

impl CachedIdentity {
    #[must_use]
    pub fn new(gateway: Arc<dyn IdentityGateway>, cache: Arc<IdentityCache>) -> Self {
        Self { gateway, cache }
    }

    /// The production pairing, or [`None`] when no HTTP client builds.
    #[must_use]
    pub fn production(cache: Arc<IdentityCache>) -> Option<Self> {
        HttpIdentityGateway::production()
            .map(|gateway| Self::new(Arc::new(gateway) as Arc<dyn IdentityGateway>, cache))
    }
}

impl IdentityResolver for CachedIdentity {
    fn resolve<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Option<IdentityResult>> {
        Box::pin(async move {
            self.cache
                .resolve(self.gateway.as_ref(), base_url, api_key, timeout)
                .await
        })
    }
}

/// The production transport.
///
/// The budget is applied per request rather than to the client, because the
/// identity read is the only thing this transport carries and its caller
/// decides the deadline: the experiments path gives it four seconds, and a
/// caller that gives it none inherits `reqwest`'s own.
pub struct ReqwestIdentityTransport {
    client: reqwest::Client,
}

impl ReqwestIdentityTransport {
    /// Builds the client, or fails so the caller can treat an unbuildable
    /// transport as one more thing that resolves to no identity.
    pub fn try_new() -> Result<Self, IdentityTransportError> {
        reqwest::Client::builder()
            .build()
            .map(|client| Self { client })
            .map_err(|_| IdentityTransportError)
    }
}

impl IdentityTransport for ReqwestIdentityTransport {
    fn send<'a>(
        &'a self,
        request: IdentityRequest<'a>,
    ) -> IdentityFuture<'a, Result<IdentityHttpResponse, IdentityTransportError>> {
        Box::pin(async move {
            // `method` is carried so a recorder observes what the client would
            // have sent; the identity endpoint takes one verb and this is it.
            let mut builder = self.client.get(request.url);
            for (name, value) in request.headers {
                builder = builder.header(*name, value);
            }
            if let Some(timeout) = request.timeout {
                builder = builder.timeout(timeout);
            }
            let response = builder.send().await.map_err(|_| IdentityTransportError)?;
            let status = response.status().as_u16();
            let body = response.text().await.map_err(|_| IdentityTransportError)?;
            Ok(IdentityHttpResponse { status, body })
        })
    }
}
