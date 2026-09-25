//! The HTTP exchange under both backends.
//!
//! The reference sends through `VibeAsyncHTTPClient`, an `httpx` client: its
//! timeout is per phase rather than for the whole exchange, the generic
//! backend follows no redirect while the Mistral client follows them, and the
//! certificates `SSL_CERT_FILE` and `SSL_CERT_DIR` name are trusted in addition
//! to the system roots (`vibe/utils/http.py`). A failure is named as `httpx`
//! names it, because that name is the detail a retry notice carries and the
//! class decides whether the failure is retried.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};

use super::error::{TransportFailure, TransportKind};

/// The per-phase budgets of one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpSettings {
    /// The longest wait for any read, the answer's head included.
    pub read_timeout: Duration,
    pub connect_timeout: Duration,
    pub follow_redirects: bool,
}

pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportFailure>> + Send>>;

/// An answer whose body has not been read yet.
pub struct HttpResponse {
    pub status: u16,
    pub reason: Option<String>,
    /// Header names lowercased; repeated headers joined with `, `.
    pub headers: BTreeMap<String, String>,
    pub body: BodyStream,
}

impl HttpResponse {
    /// Reads the whole body.
    ///
    /// # Errors
    ///
    /// The connection failing before the body is complete.
    pub async fn read_all(&mut self) -> Result<Vec<u8>, TransportFailure> {
        let mut body = Vec::new();
        while let Some(piece) = self.body.next().await {
            body.extend(piece?);
        }
        Ok(body)
    }

    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// A client one backend sends through.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
}

impl HttpClient {
    /// # Errors
    ///
    /// The TLS stack refusing to initialize.
    pub fn new(settings: HttpSettings) -> Result<Self, TransportFailure> {
        let redirects = if settings.follow_redirects {
            reqwest::redirect::Policy::limited(20)
        } else {
            reqwest::redirect::Policy::none()
        };
        let builder = reqwest::Client::builder()
            .connect_timeout(settings.connect_timeout)
            .read_timeout(settings.read_timeout)
            .pool_idle_timeout(Duration::from_secs(60))
            .redirect(redirects);
        crate::http_trust::trust_certificate_environment(builder)
            .build()
            .map(|client| Self { client })
            .map_err(|error| classify(&error))
    }

    /// Sends one `POST` and returns as soon as the answer's head arrived.
    ///
    /// # Errors
    ///
    /// The request never getting an answer.
    pub async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<HttpResponse, TransportFailure> {
        let mut request = self.client.post(url).body(body);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|error| classify(&error))?;
        let status = response.status();
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in response.headers() {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            headers
                .entry(name.as_str().to_ascii_lowercase())
                .and_modify(|known| {
                    known.push_str(", ");
                    known.push_str(&value);
                })
                .or_insert(value);
        }
        let body = response.bytes_stream().map(|piece| {
            piece
                .map(|bytes| bytes.to_vec())
                .map_err(|error| classify(&error))
        });
        Ok(HttpResponse {
            status: status.as_u16(),
            reason: status.canonical_reason().map(str::to_owned),
            headers,
            body: Box::pin(body),
        })
    }
}

/// Names a client failure the way `httpx` would have raised it.
fn classify(error: &reqwest::Error) -> TransportFailure {
    let message = error_chain(error);
    let lowered = message.to_ascii_lowercase();
    let kind = if error.is_connect() {
        if error.is_timeout() {
            TransportKind::ConnectTimeout
        } else {
            TransportKind::ConnectError
        }
    } else if error.is_timeout() {
        TransportKind::ReadTimeout
    } else if error.is_redirect() {
        TransportKind::TooManyRedirects
    } else if error.is_builder() {
        TransportKind::UnsupportedProtocol
    } else if lowered.contains("connection reset") || lowered.contains("broken pipe") {
        TransportKind::ReadError
    } else {
        // A peer that closed before the head or the whole body arrived.
        TransportKind::RemoteProtocolError
    };
    TransportFailure { kind, message }
}

/// The error and every cause under it, on one line.
fn error_chain(error: &reqwest::Error) -> String {
    let mut parts = vec![error.without_url_ref()];
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(": ")
}

trait WithoutUrl {
    fn without_url_ref(&self) -> String;
}

impl WithoutUrl for reqwest::Error {
    /// The error's own text without the URL it names, which may carry a key
    /// in its query.
    fn without_url_ref(&self) -> String {
        let text = self.to_string();
        match self.url() {
            Some(url) => text.replace(url.as_str(), "").replace(" for url ()", ""),
            None => text,
        }
    }
}
