//! The remote evaluation lookup, and the contract that it never costs the
//! caller anything.
//!
//! The client is fail-open in the strong sense: a transport failure, a status
//! at or above 400, a body that is not JSON and a body the models do not accept
//! all resolve to "no state", are reported once at warning level, and propagate
//! nothing. A caller that gets nothing back keeps the client-side defaults,
//! which is why a rollout service being down is invisible to a session.
//!
//! When the configured URL is empty, because either the host or the client key
//! is blank, [`RemoteEvalClient::evaluate`] issues no request at all rather
//! than building one and failing.
//!
//! Reference: `vibe/core/experiments/client.py` and `_constants.py` at the
//! pinned commit.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::observability::{self, LogLevel};

use super::EVAL_REQUEST_TIMEOUT;
use super::json::JsonValue;
use super::models::{EvalResponse, ExperimentAttributes};

/// The status at or above which the reference stops reading a response.
const FAILING_STATUS: u16 = 400;

/// The body the client posts.
///
/// The remote evaluation endpoint declares `attributes`, `forcedVariations`,
/// `forcedFeatures` and `url` all optional, defaulting to `{}`, `{}`, `[]` and
/// `""`. The reference sends the last three as their defaults rather than
/// omitting them, so this port sends them too: a proxy that distinguished an
/// absent key from an empty one would see the same request from both clients.
#[derive(Debug, Clone, Serialize)]
pub struct EvalPayload {
    pub attributes: ExperimentAttributes,
    #[serde(rename = "forcedVariations")]
    pub forced_variations: super::json::OrderedMap<JsonValue>,
    #[serde(rename = "forcedFeatures")]
    pub forced_features: Vec<JsonValue>,
    pub url: String,
}

impl EvalPayload {
    /// The payload one set of attributes produces, with the three defaults the
    /// reference never fills.
    #[must_use]
    pub fn new(attributes: ExperimentAttributes) -> Self {
        Self {
            attributes,
            forced_variations: super::json::OrderedMap::new(),
            forced_features: Vec::new(),
            url: String::new(),
        }
    }
}

/// One request, as it stands the instant before a connection.
///
/// The transport receives the whole request rather than a URL and a body, so a
/// recorder standing where the connection would be observes exactly what a
/// running client would have sent, headers included.
#[derive(Debug)]
pub struct EvalRequest<'a> {
    pub method: &'static str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub payload: &'a EvalPayload,
}

/// What a transport read back.
#[derive(Debug, Clone)]
pub struct EvalHttpResponse {
    pub status: u16,
    pub body: String,
}

/// The request never completed. The cause is deliberately opaque: it is logged
/// and dropped, never inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalTransportError;

pub type EvalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the eval request goes. The production implementation is
/// [`ReqwestEvalTransport`]; tests and the parity replay stand a recorder here
/// so a scenario is measured one call before the connection.
pub trait EvalTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        request: EvalRequest<'a>,
    ) -> EvalFuture<'a, Result<EvalHttpResponse, EvalTransportError>>;

    /// Releases whatever the transport holds. Called at most once per built
    /// transport, which is what makes closing the client idempotent.
    fn close(&self) -> EvalFuture<'_, ()>;
}

/// Why one attempt produced no state.
///
/// Each arm is a branch the reference warns on and swallows, kept apart so the
/// warning names the cause instead of reporting a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalFailure {
    /// The request never reached a response.
    Transport,
    /// The endpoint answered at or above 400.
    Status(u16),
    /// The body was not JSON.
    Body,
    /// The body was JSON the eval models do not accept.
    Payload,
}

impl EvalFailure {
    /// The level and the message this failure is reported at.
    ///
    /// The prose is this port's own: `NOTICE` forbids reproducing the
    /// reference's log lines, and what parity holds is that a failure warns
    /// once and names its cause, not what it says while doing it.
    #[must_use]
    pub fn report(&self) -> (LogLevel, String) {
        let message = match self {
            Self::Transport => "experiment lookup: the eval request did not complete".to_owned(),
            Self::Status(status) => {
                format!("experiment lookup: the eval endpoint answered status {status}")
            }
            Self::Body => {
                "experiment lookup: the eval endpoint answered a body that is not JSON".to_owned()
            }
            Self::Payload => {
                "experiment lookup: the eval endpoint answered JSON this build cannot read"
                    .to_owned()
            }
        };
        (LogLevel::Warning, message)
    }
}

/// How a transport is produced the first time one is needed.
enum TransportSource {
    /// The production stack, built lazily so a session that never looks a
    /// rollout up never builds an HTTP client.
    Lazy,
    /// Already built by the caller.
    Supplied(Arc<dyn EvalTransport>),
}

/// The eval endpoint, or nothing when it is not configured.
pub struct RemoteEvalClient {
    url: Option<String>,
    source: TransportSource,
    /// Empty until the first request fills it and after a close empties it,
    /// which is what makes a second close a no-op.
    transport: Mutex<Option<Arc<dyn EvalTransport>>>,
}

impl RemoteEvalClient {
    /// The client one configured host and key produce. A blank host or a blank
    /// key leaves the URL unset, and an unset URL is a client that issues
    /// nothing.
    #[must_use]
    pub fn from_settings(api_host: &str, client_key: &str) -> Self {
        Self::with_url(super::build_eval_url(api_host, client_key))
    }

    #[must_use]
    pub fn with_url(url: Option<String>) -> Self {
        Self {
            url,
            source: TransportSource::Lazy,
            transport: Mutex::new(None),
        }
    }

    /// A client whose transport the caller supplies, which is the seam the
    /// parity replay and the unit tests drive every failure branch through.
    #[must_use]
    pub fn with_transport(url: Option<String>, transport: Arc<dyn EvalTransport>) -> Self {
        Self {
            url,
            source: TransportSource::Supplied(Arc::clone(&transport)),
            transport: Mutex::new(Some(transport)),
        }
    }

    /// The eval URL this client would post to, or [`None`] when it is not
    /// configured.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The rollout state the endpoint answers, or [`None`].
    ///
    /// Nothing about a failure reaches the caller: every branch is reported
    /// once and collapses to [`None`], so a caller cannot tell a rollout that
    /// assigned nothing from a service that could not be reached, and behaves
    /// the same either way.
    pub async fn evaluate(&self, attributes: &ExperimentAttributes) -> Option<EvalResponse> {
        match self.attempt(attributes).await {
            Some(Ok(response)) => Some(response),
            Some(Err(failure)) => {
                let (level, message) = failure.report();
                observability::log(level, &message);
                None
            }
            None => None,
        }
    }

    /// One attempt, before [`RemoteEvalClient::evaluate`] reports its failure
    /// and drops it. [`None`] is an unset URL, which is the branch that issues
    /// no request and warns about nothing.
    ///
    /// Crate-visible because the parity replay measures how many warnings a
    /// scenario produces, and this port's warning sink is the process-global
    /// log file rather than a handler a test can attach.
    pub(crate) async fn attempt(
        &self,
        attributes: &ExperimentAttributes,
    ) -> Option<Result<EvalResponse, EvalFailure>> {
        let url = self.url.as_deref()?;
        Some(self.request(url, attributes).await)
    }

    /// The four failure branches, before they are reported and dropped.
    async fn request(
        &self,
        url: &str,
        attributes: &ExperimentAttributes,
    ) -> Result<EvalResponse, EvalFailure> {
        let payload = EvalPayload::new(attributes.clone());
        let transport = self.transport().ok_or(EvalFailure::Transport)?;
        let response = transport
            .send(EvalRequest {
                method: "POST",
                url,
                headers: &[],
                payload: &payload,
            })
            .await
            .map_err(|_| EvalFailure::Transport)?;
        if response.status >= FAILING_STATUS {
            return Err(EvalFailure::Status(response.status));
        }
        // Two passes rather than one, because the reference decodes and then
        // validates and warns differently for each. Both read the text rather
        // than a `serde_json::Value`, which would sort the object keys a forced
        // variant carries.
        serde_json::from_str::<serde::de::IgnoredAny>(&response.body)
            .map_err(|_| EvalFailure::Body)?;
        serde_json::from_str::<EvalResponse>(&response.body).map_err(|_| EvalFailure::Payload)
    }

    /// The transport, building the production one on first use.
    fn transport(&self) -> Option<Arc<dyn EvalTransport>> {
        let mut slot = self
            .transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(transport) = slot.as_ref() {
            return Some(Arc::clone(transport));
        }
        let built: Arc<dyn EvalTransport> = match &self.source {
            TransportSource::Lazy => Arc::new(ReqwestEvalTransport::try_new().ok()?),
            TransportSource::Supplied(transport) => Arc::clone(transport),
        };
        *slot = Some(Arc::clone(&built));
        Some(built)
    }

    /// Releases the transport if one was ever built. Closing a client that
    /// never issued a request does nothing, and closing twice closes once.
    pub async fn close(&self) {
        let taken = self
            .transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(transport) = taken {
            transport.close().await;
        }
    }
}

impl std::fmt::Debug for RemoteEvalClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteEvalClient")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// The production transport.
///
/// The reference gives its client one 5 second budget per phase; `reqwest`
/// expresses one deadline for the whole request, so the same number is applied
/// as the total and as the connect budget. This port is therefore never more
/// permissive than the reference, and being stricter costs a fail-open lookup
/// nothing.
///
/// The client is this module's own rather than the telemetry client's, which is
/// the question the PRD leaves to this story. The reference builds one per
/// consumer as well, its eval client and its telemetry sender each holding their
/// own, and the two budgets differ: telemetry keeps a pool alive for a session's
/// worth of events, while a rollout lookup runs at most once and is built
/// lazily, so sharing would tie one deadline and one pool to the other for a
/// single request.
pub struct ReqwestEvalTransport {
    client: reqwest::Client,
}

impl ReqwestEvalTransport {
    /// Builds the client, or fails so the caller can treat an unbuildable
    /// transport as one more thing that resolves to no state.
    pub fn try_new() -> Result<Self, EvalTransportError> {
        let client = reqwest::Client::builder()
            .timeout(EVAL_REQUEST_TIMEOUT)
            .connect_timeout(EVAL_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| EvalTransportError)?;
        Ok(Self { client })
    }
}

impl EvalTransport for ReqwestEvalTransport {
    fn send<'a>(
        &'a self,
        request: EvalRequest<'a>,
    ) -> EvalFuture<'a, Result<EvalHttpResponse, EvalTransportError>> {
        Box::pin(async move {
            // `method` is carried so a recorder observes what the client would
            // have sent; the eval endpoint takes one verb and this is it.
            let mut builder = self.client.post(request.url).json(request.payload);
            for (name, value) in request.headers {
                builder = builder.header(*name, *value);
            }
            let response = builder.send().await.map_err(|_| EvalTransportError)?;
            let status = response.status().as_u16();
            let body = response.text().await.map_err(|_| EvalTransportError)?;
            Ok(EvalHttpResponse { status, body })
        })
    }

    fn close(&self) -> EvalFuture<'_, ()> {
        // `reqwest` releases its pool when the client drops, so there is
        // nothing to await here.
        Box::pin(std::future::ready(()))
    }
}
