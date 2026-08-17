//! A transport that stands where the connection would be.
//!
//! It records the request a real client built, answers with an authored
//! response, and opens no socket. The unit tests and the parity replay drive
//! every branch of the client through it, which is what lets a failure scenario
//! be measured without a service to fail.

use std::sync::Mutex;

use serde_json::Value;

use super::client::{EvalFuture, EvalHttpResponse, EvalRequest, EvalTransport, EvalTransportError};

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
}

/// One request, as the client handed it over.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    pub header_names: Vec<String>,
    pub payload: Value,
}

/// A transport that answers from a script and remembers what it was asked.
#[derive(Debug)]
pub struct RecordingTransport {
    outcome: Outcome,
    requests: Mutex<Vec<RecordedRequest>>,
    closes: Mutex<usize>,
}

impl RecordingTransport {
    pub fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            requests: Mutex::new(Vec::new()),
            closes: Mutex::new(0),
        }
    }

    /// A transport answering 200 with one authored body.
    pub fn answering(body: &str) -> Self {
        Self::new(Outcome::ok(body))
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
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

    pub fn closes(&self) -> usize {
        *self
            .closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl EvalTransport for RecordingTransport {
    fn send<'a>(
        &'a self,
        request: EvalRequest<'a>,
    ) -> EvalFuture<'a, Result<EvalHttpResponse, EvalTransportError>> {
        let recorded = RecordedRequest {
            method: request.method.to_owned(),
            url: request.url.to_owned(),
            header_names: request
                .headers
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            payload: serde_json::to_value(request.payload).unwrap_or(Value::Null),
        };
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(recorded);
        let outcome = self.outcome.clone();
        Box::pin(async move {
            match outcome {
                Outcome::Answer { status, body } => Ok(EvalHttpResponse { status, body }),
                Outcome::Failure => Err(EvalTransportError),
            }
        })
    }

    fn close(&self) -> EvalFuture<'_, ()> {
        let mut closes = self
            .closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *closes = closes.saturating_add(1);
        drop(closes);
        Box::pin(std::future::ready(()))
    }
}

/// A sink that stands where a session's own record of itself stands: it keeps
/// what would have been persisted and counts how often.
#[derive(Debug, Default)]
pub struct RecordingSink {
    persisted: Mutex<Vec<super::models::EvalResponse>>,
}

impl RecordingSink {
    pub fn count(&self) -> usize {
        self.persisted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn last(&self) -> Option<super::models::EvalResponse> {
        self.persisted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }
}

impl super::session::ExperimentStateSink for RecordingSink {
    fn persist(&self, state: &super::models::EvalResponse) {
        self.persisted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(state.clone());
    }
}
