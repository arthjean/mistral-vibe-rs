//! The HTTP half of the browser sign-in.
//!
//! Reference `vibe/setup/auth/http_browser_sign_in_gateway.py`: three
//! endpoints under the configured API base, and an origin plus path-prefix
//! validation applied to every server-supplied URL before any request is
//! issued to it or any browser opened at it, on every use and not only on
//! first receipt.
//!
//! No function here writes a credential, an exchange token, a code verifier
//! or a full server-supplied URL into an error or a diagnostic; the only
//! strings a failure carries are this port's own sentences and, for a
//! provider error, the sentence the server chose to send.

use serde_json::{Map, Value, json};
use toml::Table;

use crate::pyurl::{PyUrl, normalize_url_origin};

use super::sign_in::{
    CODE_CHALLENGE_METHOD, SignInError, SignInErrorCode, SignInGateway, SignInPoll, SignInProcess,
    UtcTimestamp,
};

/// The creation endpoint under the API base.
pub const SIGN_IN_PATH: &str = "/vibe/sign-in";
/// The exchange endpoint under the API base.
pub const EXCHANGE_PATH_TEMPLATE: &str = "/vibe/sign-in/{process_id}/exchange";
/// The poll status the reference maps to an expired attempt rather than a
/// transport failure.
pub const HTTP_GONE: u16 = 410;
pub use crate::pyurl::DEFAULT_PORTS;

/// The browser-auth defaults the mistral provider resolves to when its entry
/// carries no explicit URL, matching the registry's published defaults.
pub const DEFAULT_BROWSER_AUTH_BASE_URL: &str = "https://console.mistral.ai";
pub const DEFAULT_BROWSER_AUTH_API_BASE_URL: &str = "https://console.mistral.ai/api";

/// The `(browser_base_url, api_base_url)` pair a provider entry resolves to.
///
/// Mirrors the reference provider model: an explicit value wins, and only the
/// mistral provider (backend `mistral`, or no backend recorded on a provider
/// named `mistral`) falls back to the shipped defaults. `None` when either
/// base is absent or empty, in which case browser sign-in is not available
/// for the entry.
///
/// The availability gate is the reference's own `supports_browser_sign_in`:
/// an entry that is neither the mistral backend nor named `mistral` has no
/// browser sign-in even when it carries both URLs explicitly.
pub fn browser_sign_in_bases(provider: &Table) -> Option<(String, String)> {
    let name = provider.get("name").and_then(toml::Value::as_str);
    let backend = provider.get("backend").and_then(toml::Value::as_str);
    if backend != Some("mistral") && name != Some("mistral") {
        return None;
    }
    let resolve =
        |key: &str| effective_browser_auth_url(provider, key).filter(|value| !value.is_empty());
    Some((
        resolve("browser_auth_base_url")?,
        resolve("browser_auth_api_base_url")?,
    ))
}

/// One browser-auth URL field as the reference provider model holds it after
/// validation: the explicit value, empty or not, and for the mistral provider
/// (backend `mistral`, or no backend recorded on a provider named `mistral`)
/// the shipped default when the entry carries none. Reference
/// `ProviderConfig._apply_legacy_mistral_browser_auth_defaults`.
#[must_use]
pub fn effective_browser_auth_url(provider: &Table, key: &str) -> Option<String> {
    if let Some(value) = provider.get(key).and_then(toml::Value::as_str) {
        return Some(value.to_owned());
    }
    let name = provider.get("name").and_then(toml::Value::as_str);
    let backend = provider.get("backend").and_then(toml::Value::as_str);
    let uses_mistral_defaults =
        name == Some("mistral") && matches!(backend, None | Some("mistral"));
    let default = match key {
        "browser_auth_base_url" => DEFAULT_BROWSER_AUTH_BASE_URL,
        "browser_auth_api_base_url" => DEFAULT_BROWSER_AUTH_API_BASE_URL,
        _ => return None,
    };
    uses_mistral_defaults.then(|| default.to_owned())
}

// --------------------------------------------------------------------------
// URL validation
// --------------------------------------------------------------------------

/// A URL the configured console did not vouch for. Deliberately opaque: the
/// rejected URL is server-supplied and must not travel into diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlRejection;

/// Accepts `value` only when its origin equals the base's origin, with
/// omitted and explicit default ports treated as equal, and its decoded,
/// dot-segment-normalized path sits at or under the base's path on a segment
/// boundary. Everything else, including a URL whose port or bracketed host
/// does not parse, is rejected.
pub fn validate_url_against_base(value: &str, base_url: &str) -> Result<(), UrlRejection> {
    rehome_url_against_base(value, base_url, false).map(|_| ())
}

/// [`validate_url_against_base`], answering the URL to use.
///
/// With `allow_origin_rewrite`, the provider's `browser_auth_allow_origin_rewrite`,
/// a URL on another origin is not rejected but re-homed onto the base's scheme
/// and network location, its path still held under the base path. That is what
/// a split-horizon deployment needs, where the server only knows its own public
/// host and the client reaches the console under another one. Both URLs are
/// split as `urlsplit` splits them, so a URL on the base's origin comes back
/// verbatim and a re-homed one keeps its own path, query and fragment exactly
/// as written, with the base's network location spliced in whole, credentials
/// and explicit port included. Reference `_validate_url_against_base_url`.
pub fn rehome_url_against_base(
    value: &str,
    base_url: &str,
    allow_origin_rewrite: bool,
) -> Result<String, UrlRejection> {
    let current = PyUrl::try_split(value).map_err(|_| UrlRejection)?;
    let base = PyUrl::try_split(base_url).map_err(|_| UrlRejection)?;
    let current_origin = normalize_url_origin(&current).map_err(|_| UrlRejection)?;
    let base_origin = normalize_url_origin(&base).map_err(|_| UrlRejection)?;
    let origin_matches = current_origin == base_origin;
    if !origin_matches && !allow_origin_rewrite {
        return Err(UrlRejection);
    }
    if !is_path_under_base_path(&current.path, &base.path) {
        return Err(UrlRejection);
    }
    let returned = if origin_matches {
        value.to_owned()
    } else {
        PyUrl {
            scheme: base.scheme,
            netloc: base.netloc,
            ..current
        }
        .unsplit()
    };
    if !sends_under_base(&returned, base_url) {
        return Err(UrlRejection);
    }
    Ok(returned)
}

/// Whether the HTTP client, which parses URLs as WHATWG does rather than as
/// `urlsplit` does, would also send `url` to the base's origin and under its
/// path. The two parsers disagree on inputs such as a backslash before an
/// `@`, which `urlsplit` reads as credentials and WHATWG as a path delimiter,
/// so a URL vouched for by the first could still reach another host through
/// the second. The reference sends through a parser of its own and never
/// meets this difference; refusing it here keeps the check about the request
/// that is actually made.
fn sends_under_base(url: &str, base_url: &str) -> bool {
    let (Ok(url), Ok(base)) = (url::Url::parse(url), url::Url::parse(base_url)) else {
        return false;
    };
    url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
        && is_path_under_base_path(url.path(), base.path())
}

fn is_path_under_base_path(path: &str, base_path: &str) -> bool {
    let normalized_path = normalize_url_path(path);
    let normalized_base = normalize_url_path(base_path);
    let normalized_base = normalized_base.trim_end_matches('/');
    if normalized_base.is_empty() {
        return true;
    }
    normalized_path == normalized_base
        || normalized_path.starts_with(&format!("{normalized_base}/"))
}

/// Percent-decodes once, then collapses `.` and `..` segments the way POSIX
/// path normalization does, so an encoded escape cannot slip past the prefix
/// comparison.
fn normalize_url_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    posix_normpath(&percent_decode(path))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                (bytes[index + 1] as char).to_digit(16),
                (bytes[index + 2] as char).to_digit(16),
            )
        {
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `posixpath.normpath` semantics, including the preserved double leading
/// slash and the `..` that cannot climb above the root.
fn posix_normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let leading_slashes = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if leading_slashes == 0 {
                    segments.push(segment);
                }
            }
            _ => segments.push(segment),
        }
    }
    let joined = format!("{}{}", "/".repeat(leading_slashes), segments.join("/"));
    if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

// --------------------------------------------------------------------------
// The HTTP client contract
// --------------------------------------------------------------------------

/// A transport-level failure. Carries no detail on purpose: transport error
/// text embeds the full request URL, which no diagnostic may repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignInTransportError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInHttpResponse {
    pub status: u16,
    pub body: String,
}

impl SignInHttpResponse {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The two verbs the gateway issues. Production is [`ReqwestSignInClient`];
/// the parity replay records and scripts one.
pub trait SignInHttpClient {
    fn post(
        &mut self,
        url: &str,
        body: &Value,
    ) -> impl std::future::Future<Output = Result<SignInHttpResponse, SignInTransportError>> + Send;

    fn get(
        &mut self,
        url: &str,
    ) -> impl std::future::Future<Output = Result<SignInHttpResponse, SignInTransportError>> + Send;

    fn close(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

/// The production client over the shared HTTP stack.
#[derive(Debug, Default, Clone)]
pub struct ReqwestSignInClient {
    client: reqwest::Client,
}

impl ReqwestSignInClient {
    pub fn new() -> Self {
        Self::default()
    }

    async fn read(
        response: Result<reqwest::Response, reqwest::Error>,
    ) -> Result<SignInHttpResponse, SignInTransportError> {
        let response = response.map_err(|_| SignInTransportError)?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|_| SignInTransportError)?;
        Ok(SignInHttpResponse { status, body })
    }
}

impl SignInHttpClient for ReqwestSignInClient {
    async fn post(
        &mut self,
        url: &str,
        body: &Value,
    ) -> Result<SignInHttpResponse, SignInTransportError> {
        Self::read(self.client.post(url).json(body).send().await).await
    }

    async fn get(&mut self, url: &str) -> Result<SignInHttpResponse, SignInTransportError> {
        Self::read(self.client.get(url).send().await).await
    }

    async fn close(&mut self) {}
}

// --------------------------------------------------------------------------
// The gateway
// --------------------------------------------------------------------------

/// The production [`SignInGateway`]: three endpoints under the configured
/// bases, every server-supplied URL validated before use.
pub struct HttpSignInGateway<C> {
    browser_base_url: String,
    api_base_url: String,
    /// Reference `browser_auth_allow_origin_rewrite`: a server-supplied URL on
    /// another origin is re-homed onto the configured base instead of refused.
    allow_origin_rewrite: bool,
    client: C,
}

impl<C: SignInHttpClient> HttpSignInGateway<C> {
    pub fn new(browser_base_url: &str, api_base_url: &str, client: C) -> Self {
        Self {
            browser_base_url: browser_base_url.trim_end_matches('/').to_owned(),
            api_base_url: api_base_url.trim_end_matches('/').to_owned(),
            allow_origin_rewrite: false,
            client,
        }
    }

    /// Re-homes server-supplied URLs onto the configured bases when they name
    /// another origin, as `browser_auth_allow_origin_rewrite` asks.
    #[must_use]
    pub fn with_origin_rewrite(mut self, allow: bool) -> Self {
        self.allow_origin_rewrite = allow;
        self
    }

    /// Gives the client back, which is how the parity replay reads a scripted
    /// client's request journal after a drive.
    pub fn into_client(self) -> C {
        self.client
    }
}

impl HttpSignInGateway<ReqwestSignInClient> {
    /// The gateway a provider entry configures, honoring
    /// `browser_auth_base_url` and `browser_auth_api_base_url`. `None` when
    /// the entry resolves no sign-in bases.
    pub fn for_provider(provider: &Table) -> Option<Self> {
        let (browser_base_url, api_base_url) = browser_sign_in_bases(provider)?;
        let allow_origin_rewrite = provider
            .get("browser_auth_allow_origin_rewrite")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        Some(
            Self::new(&browser_base_url, &api_base_url, ReqwestSignInClient::new())
                .with_origin_rewrite(allow_origin_rewrite),
        )
    }
}

impl<C: SignInHttpClient + Send> SignInGateway for HttpSignInGateway<C> {
    async fn create_process(&mut self, code_challenge: &str) -> Result<SignInProcess, SignInError> {
        let code = SignInErrorCode::StartFailed;
        let url = format!("{}{SIGN_IN_PATH}", self.api_base_url);
        let body = json!({
            "code_challenge": code_challenge,
            "code_challenge_method": CODE_CHALLENGE_METHOD,
        });
        let response = self
            .client
            .post(&url, &body)
            .await
            .map_err(|SignInTransportError| SignInError::new(code))?;
        if !response.is_success() {
            return Err(SignInError::new(code));
        }
        let payload = response_object(&response, code)?;
        let process_id = string_field(&payload, "process_id", code)?;
        let sign_in_url = string_field(&payload, "sign_in_url", code)?;
        let poll_url = string_field(&payload, "poll_url", code)?;
        let expires_at = string_field(&payload, "expires_at", code)?;
        let sign_in_url = rehome_url_against_base(
            &sign_in_url,
            &self.browser_base_url,
            self.allow_origin_rewrite,
        )
        .map_err(|UrlRejection| SignInError::new(code))?;
        let poll_url =
            rehome_url_against_base(&poll_url, &self.api_base_url, self.allow_origin_rewrite)
                .map_err(|UrlRejection| SignInError::new(code))?;
        let expires_at =
            UtcTimestamp::parse_iso8601(&expires_at).ok_or_else(|| SignInError::new(code))?;
        Ok(SignInProcess {
            process_id,
            sign_in_url,
            poll_url,
            expires_at,
        })
    }

    async fn poll(&mut self, poll_url: &str) -> Result<SignInPoll, SignInError> {
        let code = SignInErrorCode::PollFailed;
        let poll_url =
            rehome_url_against_base(poll_url, &self.api_base_url, self.allow_origin_rewrite)
                .map_err(|UrlRejection| SignInError::new(code))?;
        let response = self
            .client
            .get(&poll_url)
            .await
            .map_err(|SignInTransportError| SignInError::new(code))?;
        if response.status == HTTP_GONE {
            return Ok(SignInPoll {
                status: "expired".to_owned(),
                exchange_token: None,
                message: None,
            });
        }
        if !response.is_success() {
            return Err(SignInError::new(code));
        }
        let payload = response_object(&response, code)?;
        let status = payload.get("status").and_then(Value::as_str);
        let status = match status {
            Some(known @ ("pending" | "completed" | "expired" | "denied" | "error")) => known,
            _ => return Err(SignInError::new(SignInErrorCode::UnknownState)),
        };
        Ok(SignInPoll {
            status: status.to_owned(),
            exchange_token: optional_string_field(&payload, "exchange_token"),
            message: optional_string_field(&payload, "message"),
        })
    }

    async fn exchange(
        &mut self,
        process_id: &str,
        exchange_token: &str,
        code_verifier: &str,
    ) -> Result<String, SignInError> {
        let code = SignInErrorCode::ExchangeFailed;
        let url = format!(
            "{}{}",
            self.api_base_url,
            EXCHANGE_PATH_TEMPLATE.replace("{process_id}", process_id)
        );
        let body = json!({
            "exchange_token": exchange_token,
            "code_verifier": code_verifier,
        });
        let response = self
            .client
            .post(&url, &body)
            .await
            .map_err(|SignInTransportError| SignInError::new(code))?;
        if !response.is_success() {
            return Err(SignInError::new(code));
        }
        let payload = response_object(&response, code)?;
        match optional_string_field(&payload, "api_key") {
            Some(api_key) => Ok(api_key),
            None => Err(SignInError::new(SignInErrorCode::MissingApiKey)),
        }
    }

    async fn close(&mut self) {
        self.client.close().await;
    }
}

fn response_object(
    response: &SignInHttpResponse,
    code: SignInErrorCode,
) -> Result<Map<String, Value>, SignInError> {
    serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .ok_or_else(|| SignInError::new(code))
}

fn string_field(
    payload: &Map<String, Value>,
    field: &str,
    code: SignInErrorCode,
) -> Result<String, SignInError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SignInError::new(code))
}

/// A present, non-empty string field; the reference reads these through
/// truthiness, so an empty string reads as absent.
fn optional_string_field(payload: &Map<String, Value>, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
