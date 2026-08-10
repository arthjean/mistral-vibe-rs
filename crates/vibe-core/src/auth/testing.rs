//! Scripted counterparts of the capture scripts' stubs.
//!
//! A scripted [`KeyringBackend`] mirrors `_KeyringScript`, and a scripted
//! [`SignInGateway`] plus [`SignInRuntime`] mirror `_StubGateway` and
//! `_FakeClock`: the corpus replay and the unit tests drive the port over the
//! same scripted stores, polls, clocks and call journals the reference was
//! measured with.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::keyring::{KeyringBackend, KeyringFailure};
use super::sign_in::{
    SignInError, SignInErrorCode, SignInGateway, SignInPoll, SignInProcess, SignInRuntime,
    UtcTimestamp,
};
use super::sign_in_http::{SignInHttpClient, SignInHttpResponse, SignInTransportError};

/// The failure kinds the capture scripts inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptedError {
    /// `KeyringError`: the store exists and the operation failed.
    Backend,
    /// `NoKeyringError`: no store at all.
    NoBackend,
}

impl ScriptedError {
    fn failure(self) -> KeyringFailure {
        match self {
            Self::Backend => KeyringFailure::Backend("scripted failure".to_owned()),
            Self::NoBackend => KeyringFailure::NoBackend,
        }
    }
}

#[derive(Default)]
pub(crate) struct ScriptedBackend {
    stored: Mutex<BTreeMap<String, String>>,
    set_error: Option<ScriptedError>,
    get_error: Option<ScriptedError>,
    delete_errors: BTreeMap<String, ScriptedError>,
    calls: Mutex<Vec<String>>,
}

/// One scripted backend: the seeded `service -> secret` entries, the error
/// every `set` or `get` answers, and the error `delete` answers per service.
pub(crate) fn scripted(
    stored: &[(&str, &str)],
    set_error: Option<ScriptedError>,
    get_error: Option<ScriptedError>,
    delete_errors: &[(&str, ScriptedError)],
) -> Arc<ScriptedBackend> {
    Arc::new(ScriptedBackend {
        stored: Mutex::new(
            stored
                .iter()
                .map(|(service, secret)| ((*service).to_owned(), (*secret).to_owned()))
                .collect(),
        ),
        set_error,
        get_error,
        delete_errors: delete_errors
            .iter()
            .map(|(service, error)| ((*service).to_owned(), *error))
            .collect(),
        calls: Mutex::new(Vec::new()),
    })
}

impl ScriptedBackend {
    /// The stored entries, sorted by service as the corpus records them.
    pub(crate) fn stored(&self) -> BTreeMap<String, String> {
        self.stored.lock().expect("scripted store").clone()
    }

    /// The ordered primitive calls, `op:service` as the corpus records them.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("scripted calls").clone()
    }

    fn record(&self, operation: &str, service: &str) {
        self.calls
            .lock()
            .expect("scripted calls")
            .push(format!("{operation}:{service}"));
    }
}

impl KeyringBackend for Arc<ScriptedBackend> {
    fn get(&self, service: &str, _account: &str) -> Result<Option<String>, KeyringFailure> {
        self.record("get", service);
        if let Some(error) = self.get_error {
            return Err(error.failure());
        }
        Ok(self
            .stored
            .lock()
            .expect("scripted store")
            .get(service)
            .cloned())
    }

    fn set(&self, service: &str, _account: &str, secret: &str) -> Result<(), KeyringFailure> {
        self.record("set", service);
        if let Some(error) = self.set_error {
            return Err(error.failure());
        }
        self.stored
            .lock()
            .expect("scripted store")
            .insert(service.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, service: &str, _account: &str) -> Result<(), KeyringFailure> {
        self.record("delete", service);
        if let Some(error) = self.delete_errors.get(service) {
            return Err(error.failure());
        }
        if self
            .stored
            .lock()
            .expect("scripted store")
            .remove(service)
            .is_none()
        {
            return Err(KeyringFailure::NoEntry);
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------
// The scripted sign-in gateway and runtime
// --------------------------------------------------------------------------

/// The instant the capture script's clock starts at, `CLOCK_START` in
/// `scripts/parity/setup_auth.py`.
pub(crate) const CLOCK_START_ISO: &str = "2026-01-01T00:00:00+00:00";

pub(crate) fn clock_start() -> UtcTimestamp {
    UtcTimestamp::parse_iso8601(CLOCK_START_ISO).expect("the scripted clock start parses")
}

pub(crate) fn error_code_from_value(value: &str) -> SignInErrorCode {
    *SignInErrorCode::ALL
        .iter()
        .find(|code| code.as_str() == value)
        .unwrap_or_else(|| panic!("no sign-in error code is named {value}"))
}

/// The counterpart of the capture script's `_StubGateway`: create answers a
/// fixed process, polls follow a scripted list in the corpus's own JSON
/// shape, and every call lands in an ordered journal.
pub(crate) struct ScriptedSignInGateway {
    pub(crate) create_error: Option<String>,
    pub(crate) expires_at: UtcTimestamp,
    /// Each step `{"status": ..}` with optional `exchangeToken`/`message`, or
    /// `{"raise": code}`; an exhausted list answers pending.
    pub(crate) polls: VecDeque<Value>,
    /// A string key, or `{"raise": code}`.
    pub(crate) exchange: Value,
    pub(crate) calls: Vec<String>,
    pub(crate) closed: usize,
    pub(crate) challenge: Option<String>,
}

impl ScriptedSignInGateway {
    pub(crate) fn new(
        create_error: Option<String>,
        expires_in_seconds: f64,
        polls: Vec<Value>,
        exchange: Value,
    ) -> Self {
        Self {
            create_error,
            expires_at: clock_start().plus_seconds(expires_in_seconds),
            polls: polls.into(),
            exchange,
            calls: Vec::new(),
            closed: 0,
            challenge: None,
        }
    }
}

impl SignInGateway for ScriptedSignInGateway {
    async fn create_process(&mut self, code_challenge: &str) -> Result<SignInProcess, SignInError> {
        self.calls.push("create".to_owned());
        self.challenge = Some(code_challenge.to_owned());
        if let Some(code) = &self.create_error {
            return Err(SignInError::with_message(
                error_code_from_value(code),
                "scripted start failure".to_owned(),
            ));
        }
        Ok(SignInProcess {
            process_id: "oracle-process".to_owned(),
            sign_in_url: "https://console.mistral.ai/oracle/sign-in".to_owned(),
            poll_url: "https://console.mistral.ai/api/oracle/poll".to_owned(),
            expires_at: self.expires_at,
        })
    }

    async fn poll(&mut self, _poll_url: &str) -> Result<SignInPoll, SignInError> {
        self.calls.push("poll".to_owned());
        let step = self
            .polls
            .pop_front()
            .unwrap_or_else(|| serde_json::json!({"status": "pending"}));
        if let Some(code) = step.get("raise").and_then(Value::as_str) {
            return Err(SignInError::with_message(
                error_code_from_value(code),
                "scripted poll failure".to_owned(),
            ));
        }
        let field = |name: &str| step.get(name).and_then(Value::as_str).map(str::to_owned);
        Ok(SignInPoll {
            status: field("status").unwrap_or_default(),
            exchange_token: field("exchangeToken"),
            message: field("message"),
        })
    }

    async fn exchange(
        &mut self,
        process_id: &str,
        exchange_token: &str,
        _code_verifier: &str,
    ) -> Result<String, SignInError> {
        self.calls
            .push(format!("exchange:{process_id}:{exchange_token}"));
        if let Some(code) = self.exchange.get("raise").and_then(Value::as_str) {
            return Err(SignInError::with_message(
                error_code_from_value(code),
                "scripted exchange failure".to_owned(),
            ));
        }
        Ok(self.exchange.as_str().unwrap_or_default().to_owned())
    }

    async fn close(&mut self) {
        self.closed += 1;
    }
}

/// What the scripted opener does when the service reaches it, mirroring the
/// capture's `accept`, `refuse` and `raise` kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptedOpener {
    Accept,
    Refuse,
    Raise,
}

impl ScriptedOpener {
    /// The corpus records the kind as the capture script's own string.
    pub(crate) fn from_kind(kind: &str) -> Self {
        match kind {
            "accept" => Self::Accept,
            "refuse" => Self::Refuse,
            "raise" => Self::Raise,
            other => panic!("no scripted opener is named {other}"),
        }
    }
}

/// The counterpart of the capture script's `_FakeClock` plus its opener and
/// verifier patches: sleeping advances the clock and lands in a journal, and
/// the verifier is the corpus's scripted one.
pub(crate) struct ScriptedSignInRuntime {
    pub(crate) now: UtcTimestamp,
    pub(crate) sleeps: Vec<f64>,
    pub(crate) opened: Vec<String>,
    pub(crate) opener: ScriptedOpener,
    pub(crate) verifier: String,
    /// When set, sleeping never resolves, which is how the cancellation test
    /// parks the flow mid-wait.
    pub(crate) hang_on_sleep: bool,
}

impl ScriptedSignInRuntime {
    pub(crate) fn new(opener: ScriptedOpener, verifier: &str) -> Self {
        Self {
            now: clock_start(),
            sleeps: Vec::new(),
            opened: Vec::new(),
            opener,
            verifier: verifier.to_owned(),
            hang_on_sleep: false,
        }
    }
}

impl SignInRuntime for ScriptedSignInRuntime {
    fn now(&mut self) -> UtcTimestamp {
        self.now
    }

    async fn sleep(&mut self, seconds: f64) {
        if self.hang_on_sleep {
            std::future::pending::<()>().await;
        }
        self.sleeps.push(seconds);
        self.now = self.now.plus_seconds(seconds);
    }

    fn open_browser(&mut self, url: &str) -> std::io::Result<bool> {
        self.opened.push(url.to_owned());
        match self.opener {
            ScriptedOpener::Accept => Ok(true),
            ScriptedOpener::Refuse => Ok(false),
            ScriptedOpener::Raise => Err(std::io::Error::other("scripted opener failure")),
        }
    }

    fn code_verifier(&mut self) -> Result<String, SignInError> {
        Ok(self.verifier.clone())
    }
}

/// The counterpart of the capture script's `_StubHTTPClient`: records every
/// request in the corpus's own shape and answers from a scripted list of
/// `{"transportError": true}`, `{"rawBody": .., "status"?: ..}` or
/// `{"body": .., "status"?: ..}` steps.
pub(crate) struct ScriptedHttpClient {
    pub(crate) responses: VecDeque<Value>,
    pub(crate) requests: Vec<Value>,
}

impl ScriptedHttpClient {
    pub(crate) fn new(responses: Vec<Value>) -> Self {
        Self {
            responses: responses.into(),
            requests: Vec::new(),
        }
    }

    fn next(
        &mut self,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<SignInHttpResponse, SignInTransportError> {
        self.requests.push(serde_json::json!({
            "method": method,
            "url": url,
            "json": body.cloned().unwrap_or(Value::Null),
        }));
        let step = self
            .responses
            .pop_front()
            .unwrap_or_else(|| panic!("the gateway issued more requests than scripted"));
        if step.get("transportError").and_then(Value::as_bool) == Some(true) {
            return Err(SignInTransportError);
        }
        let status = step
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .unwrap_or(200);
        let body = match step.get("rawBody").and_then(Value::as_str) {
            Some(raw) => raw.to_owned(),
            None => step
                .get("body")
                .map(|body| body.to_string())
                .unwrap_or_default(),
        };
        Ok(SignInHttpResponse { status, body })
    }
}

impl SignInHttpClient for ScriptedHttpClient {
    async fn post(
        &mut self,
        url: &str,
        body: &Value,
    ) -> Result<SignInHttpResponse, SignInTransportError> {
        self.next("POST", url, Some(body))
    }

    async fn get(&mut self, url: &str) -> Result<SignInHttpResponse, SignInTransportError> {
        self.next("GET", url, None)
    }

    async fn close(&mut self) {}
}
