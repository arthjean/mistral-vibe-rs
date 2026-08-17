//! A transport that stands where the identity connection would be.
//!
//! It records the request a real gateway built, answers with an authored
//! response, and opens no socket. The unit tests and the parity replay drive
//! every branch of the gateway through it, which is what lets a refused
//! credential and an unreachable service both be measured without a service.

use std::sync::Mutex;
use std::time::Duration;

use super::{
    IdentityEntity, IdentityFuture, IdentityHttpResponse, IdentityRequest, IdentityResolver,
    IdentityResult, IdentityTransport, IdentityTransportError,
};

/// What the transport does with a request.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Answer with this status and this body.
    Answer { status: u16, body: String },
    /// Never reach a response.
    Failure,
}

impl Outcome {
    /// A 200 carrying one authored body.
    pub fn ok(body: &str) -> Self {
        Self::Answer {
            status: 200,
            body: body.to_owned(),
        }
    }

    /// A status with an empty body, which is how every non-success branch is
    /// driven.
    pub fn status(status: u16) -> Self {
        Self::Answer {
            status,
            body: String::new(),
        }
    }
}

/// One request, as the gateway handed it over.
#[derive(Debug, Clone)]
pub struct RecordedIdentityRequest {
    pub method: String,
    pub url: String,
    pub header_names: Vec<String>,
    /// Whether the bearer header carried the credential, recorded as a boolean
    /// so no test or corpus ever holds one.
    pub bearer_carries_key: bool,
    pub timeout: Option<Duration>,
}

/// A transport that answers from a script and remembers what it was asked.
#[derive(Debug)]
pub struct RecordingIdentityTransport {
    outcome: Outcome,
    api_key: String,
    requests: Mutex<Vec<RecordedIdentityRequest>>,
}

impl RecordingIdentityTransport {
    pub fn new(outcome: Outcome, api_key: &str) -> Self {
        Self {
            outcome,
            api_key: api_key.to_owned(),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A transport answering 200 with one authored identity.
    pub fn answering(body: &str, api_key: &str) -> Self {
        Self::new(Outcome::ok(body), api_key)
    }

    pub fn requests(&self) -> Vec<RecordedIdentityRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl IdentityTransport for RecordingIdentityTransport {
    fn send<'a>(
        &'a self,
        request: IdentityRequest<'a>,
    ) -> IdentityFuture<'a, Result<IdentityHttpResponse, IdentityTransportError>> {
        let expected = format!("Bearer {}", self.api_key);
        let recorded = RecordedIdentityRequest {
            method: request.method.to_owned(),
            url: request.url.to_owned(),
            header_names: request
                .headers
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            bearer_carries_key: request
                .headers
                .iter()
                .any(|(name, value)| *name == "Authorization" && *value == expected),
            timeout: request.timeout,
        };
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(recorded);
        let outcome = self.outcome.clone();
        Box::pin(async move {
            match outcome {
                Outcome::Answer { status, body } => Ok(IdentityHttpResponse { status, body }),
                Outcome::Failure => Err(IdentityTransportError),
            }
        })
    }
}

/// One resolution, as the caller asked for it.
#[derive(Debug, Clone)]
pub struct RecordedResolution {
    pub base_url: String,
    pub timeout: Option<Duration>,
    /// Whether the resolution carried the credential the recorder was built
    /// with, recorded as a boolean so no test or corpus ever holds one.
    pub carries_key: bool,
}

/// A resolver that stands where the identity cache stands: it records what it
/// was asked and answers a scripted organization, without a gateway behind it.
#[derive(Debug)]
pub struct RecordingResolver {
    organization: Option<String>,
    api_key: String,
    calls: Mutex<Vec<RecordedResolution>>,
}

impl RecordingResolver {
    pub fn answering(organization: Option<&str>, api_key: &str) -> Self {
        Self {
            organization: organization.map(ToOwned::to_owned),
            api_key: api_key.to_owned(),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<RecordedResolution> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl IdentityResolver for RecordingResolver {
    fn resolve<'a>(
        &'a self,
        base_url: &'a str,
        api_key: &'a str,
        timeout: Option<Duration>,
    ) -> IdentityFuture<'a, Option<IdentityResult>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedResolution {
                base_url: base_url.to_owned(),
                timeout,
                carries_key: api_key == self.api_key,
            });
        let organization = self.organization.clone();
        Box::pin(async move {
            organization.map(|id| IdentityResult {
                id: "oracle-user".to_owned(),
                email: None,
                first_name: None,
                last_name: None,
                workspace: None,
                organization: Some(IdentityEntity {
                    id,
                    name: "Oracle".to_owned(),
                }),
            })
        })
    }
}
