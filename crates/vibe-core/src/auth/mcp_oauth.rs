//! OAuth for remote MCP servers: the credential store and the login.
//!
//! Reference `vibe/core/auth/mcp_oauth.py`. A credential is filed in the OS
//! keyring under the server's configured name, as three entries of service
//! `ai.mistral.vibe`: `mcp-oauth:{alias}:tokens`, `mcp-oauth:{alias}:client_info`
//! and `mcp-oauth:{alias}:fingerprint`, each holding the JSON the reference
//! writes, so a credential one implementation stored is read by the other.
//! The fingerprint records which shape of the entry (url, scopes, client
//! identity) the tokens were minted for.
//!
//! The authorization flow itself is the MCP Python SDK's
//! `OAuthClientProvider.async_auth_flow` (`mcp/client/auth/oauth2.py`, SDK
//! 1.28.1), with the reference's `RefreshAwareOAuthClientProvider` override,
//! ported step for step in [`flow`]; the loopback callback it waits on is in
//! [`callback`].

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;

use super::keyring::{
    KEYRING_SERVICE, KeyringBackend, KeyringFailure, LEGACY_KEYRING_SERVICES, NativeKeyringBackend,
    PRIOR_BUILD_KEYRING_SERVICE,
};
use crate::mcp::{McpAuthConfig, McpServerConfig};

mod callback;
mod flow;

#[cfg(test)]
mod mcp_oauth_tests;

const USERNAME_PREFIX: &str = "mcp-oauth";
/// Reference `_CLIENT_NAME`, the name a registration and an `initialize`
/// identify the client by.
pub const CLIENT_NAME: &str = "Mistral Vibe";
/// Reference `_LOGIN_TIMEOUT_SECONDS`, httpx's per-operation timeout.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Reference `_MCP_ACCEPT`.
const MCP_ACCEPT: &str = "application/json, text/event-stream";
/// Reference `_OAUTH_INVALID_GRANT`.
const OAUTH_INVALID_GRANT: &str = "invalid_grant";
/// Reference `_EXPIRED_TOKEN_TIME`: a stored grant that states a lifetime and
/// no deadline reads as already expired.
const EXPIRED_TOKEN_TIME: f64 = -1.0;
/// The MCP protocol version the SDK sends, `LATEST_PROTOCOL_VERSION`.
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Where an MCP OAuth credential entry lives: reference `_kr_username`.
#[must_use]
pub fn mcp_oauth_username(alias: &str, kind: &str) -> String {
    format!("{USERNAME_PREFIX}:{alias}:{kind}")
}

/// Why an MCP OAuth operation ended without a credential.
///
/// The variants are the reference's exception classes; the sentences are
/// this port's own.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum McpOAuthError {
    /// Reference `MCPOAuthHeadlessError`.
    #[error(
        "MCP server `{alias}` cannot keep OAuth tokens: this host has no OS credential store. Use a static `api_key_env` credential instead"
    )]
    Headless { alias: String },
    /// Reference `MCPOAuthPortInUse`.
    #[error(
        "the OAuth callback for MCP server `{alias}` needs local port {port}, which another program holds; pick a free `auth.redirect_port` for it"
    )]
    PortInUse { alias: String, port: u16 },
    /// Reference `MCPOAuthInvalidGrant`.
    #[error(
        "the authorization server refused the stored refresh token of MCP server `{alias}` ({reason}); sign in again with `/mcp login {alias}`"
    )]
    InvalidGrant { alias: String, reason: String },
    /// Reference `MCPOAuthTransientRefreshError`.
    #[error(
        "refreshing the OAuth token of MCP server `{alias}` failed for now ({reason}); the stored credential is kept"
    )]
    TransientRefresh { alias: String, reason: String },
    /// Reference `MCPOAuthLoginFailed`.
    #[error("signing in to MCP server `{alias}` did not complete: {reason}")]
    LoginFailed { alias: String, reason: String },
    /// Reference `MCPOAuthError` raised by the loopback callback.
    #[error("the OAuth callback for MCP server `{alias}` {detail}")]
    Callback { alias: String, detail: String },
    /// Reference `MCPOAuthCredentialCleanupFailed`.
    #[error("the OAuth credential of MCP server `{alias}` could not be removed: {reason}")]
    CleanupFailed { alias: String, reason: String },
    /// Reference `MCPOAuthCredentialRestoreFailed`.
    #[error(
        "the OAuth credential of MCP server `{alias}` could not be put back after an interrupted removal: {reason}"
    )]
    RestoreFailed { alias: String, reason: String },
    /// A keyring write the reference lets escape as `KeyringError`.
    #[error("the OS credential store refused an OAuth entry: {0}")]
    Keyring(String),
    /// An entry the SDK refuses before any request, reference `ValueError`.
    #[error("{0}")]
    Config(String),
}

impl McpOAuthError {
    /// The reference exception class this error stands for.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Headless { .. } => "headless",
            Self::PortInUse { .. } => "port_in_use",
            Self::InvalidGrant { .. } => "invalid_grant",
            Self::TransientRefresh { .. } => "transient_refresh",
            Self::LoginFailed { .. } => "login_failed",
            Self::Callback { .. } => "callback",
            Self::CleanupFailed { .. } => "cleanup_failed",
            Self::RestoreFailed { .. } => "restore_failed",
            Self::Keyring(_) => "keyring",
            Self::Config(_) => "config",
        }
    }
}

/// The keyring entries of one credential, reference `KeyringTokenStorage`
/// and the `_kr_*` helpers.
///
/// Reads consult the current service only (`search_legacy_services=False`)
/// and go through a cache, as `get_api_key_from_keyring` does; a write
/// remembers what it wrote and a delete forgets it. A host without any
/// credential store is `Headless`; the reference's
/// `VIBE_TEST_DISABLE_KEYRING` hook makes every read empty and every write
/// fail, which is a different thing.
pub struct McpOAuthStore {
    backend: Arc<dyn KeyringBackend>,
    disabled: bool,
    cache: Mutex<BTreeMap<String, Option<String>>>,
}

/// No credential store is reachable, reference `MCPOAuthHeadlessError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headless;

impl McpOAuthStore {
    #[must_use]
    pub fn new(backend: Arc<dyn KeyringBackend>, disabled: bool) -> Self {
        Self {
            backend,
            disabled,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The production store: the OS keyring, disabled by the reference's
    /// test hook.
    #[must_use]
    pub fn native() -> Self {
        Self::new(
            Arc::new(NativeKeyringBackend::new()),
            std::env::var(super::keyring::DISABLE_KEYRING_ENV_VAR).as_deref() == Ok("1"),
        )
    }

    /// Reference `_kr_get`.
    pub fn get(&self, account: &str) -> Result<Option<String>, Headless> {
        if self.disabled {
            return Ok(None);
        }
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(account)
        {
            return Ok(cached.clone());
        }
        match self.backend.get(KEYRING_SERVICE, account) {
            Ok(value) => {
                if let Ok(mut cache) = self.cache.lock() {
                    return Ok(cache.entry(account.to_owned()).or_insert(value).clone());
                }
                Ok(value)
            }
            Err(KeyringFailure::NoBackend) => Err(Headless),
            // Reference `get_api_key_from_keyring` answers a store failure as
            // an absent entry, and caches nothing.
            Err(_) => Ok(None),
        }
    }

    /// Whether any credential store is reachable, the check reference
    /// `KeyringTokenStorage.__init__` makes before touching an entry.
    pub fn available(&self, alias: &str) -> Result<(), Headless> {
        self.get(&mcp_oauth_username(alias, "tokens")).map(|_| ())
    }

    /// Reference `_kr_set`.
    pub fn set(&self, account: &str, value: &str) -> Result<(), KeyringFailure> {
        if self.disabled {
            self.forget(account);
            return Err(KeyringFailure::Backend(
                "the keyring is disabled for this run".to_owned(),
            ));
        }
        if let Err(failure) = self.backend.set(KEYRING_SERVICE, account, value) {
            self.forget(account);
            return Err(failure);
        }
        for legacy in LEGACY_KEYRING_SERVICES {
            let _ = self.backend.delete(legacy, account);
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(account.to_owned(), Some(value.to_owned()));
        }
        Ok(())
    }

    /// Reference `_kr_delete`: an absent entry is not a failure.
    pub fn delete(&self, account: &str) -> Result<(), KeyringFailure> {
        self.forget(account);
        if self.disabled {
            return Ok(());
        }
        let mut failure = None;
        for service in std::iter::once(KEYRING_SERVICE)
            .chain(LEGACY_KEYRING_SERVICES.iter().copied())
            .chain(std::iter::once(PRIOR_BUILD_KEYRING_SERVICE))
        {
            match self.backend.delete(service, account) {
                Ok(()) | Err(KeyringFailure::NoEntry) => {}
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn forget(&self, account: &str) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(account);
        }
    }

    /// Reference `KeyringTokenStorage.get_tokens`: the stored grant and the
    /// deadline it is read with.
    pub fn tokens(&self, alias: &str) -> Result<Option<StoredTokens>, Headless> {
        Ok(self
            .get(&mcp_oauth_username(alias, "tokens"))?
            .and_then(|raw| StoredTokens::parse(&raw)))
    }

    /// Reference `KeyringTokenStorage.set_tokens`.
    pub fn set_tokens(
        &self,
        alias: &str,
        token: &OAuthToken,
    ) -> Result<StoredTokens, KeyringFailure> {
        let stored = StoredTokens::from_token(token, now_seconds());
        self.set(&mcp_oauth_username(alias, "tokens"), &stored.to_json())?;
        Ok(stored)
    }

    pub fn delete_tokens(&self, alias: &str) -> Result<(), KeyringFailure> {
        self.delete(&mcp_oauth_username(alias, "tokens"))
    }

    pub fn client_info(&self, alias: &str) -> Result<Option<ClientInformation>, Headless> {
        Ok(self
            .get(&mcp_oauth_username(alias, "client_info"))?
            .and_then(|raw| ClientInformation::parse(&raw)))
    }

    pub fn set_client_info(
        &self,
        alias: &str,
        info: &ClientInformation,
    ) -> Result<(), KeyringFailure> {
        self.set(&mcp_oauth_username(alias, "client_info"), &info.to_json())
    }

    pub fn delete_client_info(&self, alias: &str) -> Result<(), KeyringFailure> {
        self.delete(&mcp_oauth_username(alias, "client_info"))
    }

    /// The stored fingerprint as its JSON text, reference `Fingerprint.load`.
    pub fn fingerprint(&self, alias: &str) -> Result<Option<String>, Headless> {
        self.get(&mcp_oauth_username(alias, "fingerprint"))
    }

    pub fn delete_fingerprint(&self, alias: &str) -> Result<(), KeyringFailure> {
        self.delete(&mcp_oauth_username(alias, "fingerprint"))
    }

    /// Reference `delete_oauth_credentials`.
    pub fn delete_credentials(&self, alias: &str) -> Result<(), McpOAuthError> {
        if self.available(alias).is_err() {
            return Ok(());
        }
        let cleanup = |failure: KeyringFailure| McpOAuthError::CleanupFailed {
            alias: alias.to_owned(),
            reason: failure.to_string(),
        };
        self.delete_tokens(alias).map_err(cleanup)?;
        self.delete_client_info(alias).map_err(cleanup)?;
        self.delete_fingerprint(alias).map_err(cleanup)
    }

    /// Reference `snapshot_oauth_credentials`.
    pub fn snapshot(&self, alias: &str) -> CredentialBackup {
        if self.available(alias).is_err() {
            return CredentialBackup {
                keyring_available: false,
                entries: BTreeMap::new(),
            };
        }
        let entries = ["tokens", "client_info", "fingerprint"]
            .into_iter()
            .map(|kind| {
                (
                    kind,
                    self.get(&mcp_oauth_username(alias, kind)).ok().flatten(),
                )
            })
            .collect();
        CredentialBackup {
            keyring_available: true,
            entries,
        }
    }

    /// Reference `restore_oauth_credentials`.
    pub fn restore(&self, alias: &str, backup: &CredentialBackup) -> Result<(), McpOAuthError> {
        if !backup.keyring_available {
            return Ok(());
        }
        let restore = |failure: KeyringFailure| McpOAuthError::RestoreFailed {
            alias: alias.to_owned(),
            reason: failure.to_string(),
        };
        for (kind, value) in &backup.entries {
            let account = mcp_oauth_username(alias, kind);
            match value {
                Some(value) => self.set(&account, value).map_err(restore)?,
                None => self.delete(&account).map_err(restore)?,
            }
        }
        Ok(())
    }
}

/// Reference `OAuthCredentialBackup`: what a removal deleted, held so it can
/// be put back.
pub struct CredentialBackup {
    keyring_available: bool,
    entries: BTreeMap<&'static str, Option<String>>,
}

impl std::fmt::Debug for CredentialBackup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialBackup")
            .field("keyring_available", &self.keyring_available)
            .finish_non_exhaustive()
    }
}

pub(crate) fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// SDK `OAuthToken`: what a token endpoint answers.
#[derive(Clone, PartialEq)]
pub struct OAuthToken {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthToken")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

impl OAuthToken {
    /// `OAuthToken.model_validate_json`: `token_type` title-cased and held to
    /// `Bearer`, `expires_in` read as pydantic's lax integer.
    pub(crate) fn validate(value: &Value) -> Result<Self, String> {
        let fields = value
            .as_object()
            .ok_or_else(|| "the token response is not an object".to_owned())?;
        let access_token = match fields.get("access_token") {
            Some(Value::String(token)) => token.clone(),
            _ => return Err("the token response has no string access_token".to_owned()),
        };
        let token_type = match fields.get("token_type") {
            None => "Bearer".to_owned(),
            Some(Value::String(kind)) => title_case(kind),
            Some(_) => return Err("the token response has an invalid token_type".to_owned()),
        };
        if token_type != "Bearer" {
            return Err(format!("the token type `{token_type}` is not Bearer"));
        }
        Ok(Self {
            access_token,
            token_type,
            expires_in: lax_optional_int(fields.get("expires_in"))?,
            scope: optional_string(fields.get("scope"), "scope")?,
            refresh_token: optional_string(fields.get("refresh_token"), "refresh_token")?,
        })
    }
}

/// Python's `str.title`.
fn title_case(text: &str) -> String {
    let mut titled = String::with_capacity(text.len());
    let mut previous_cased = false;
    for character in text.chars() {
        if previous_cased {
            titled.extend(character.to_lowercase());
        } else {
            titled.extend(character.to_uppercase());
        }
        previous_cased = character.is_alphabetic();
    }
    titled
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("`{field}` is not a string")),
    }
}

/// Pydantic's lax `int | None`: an integer, a float without a fraction, or a
/// string of digits.
fn lax_optional_int(value: Option<&Value>) -> Result<Option<i64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|float| float.fract() == 0.0 && float.is_finite())
                    .map(|float| {
                        #[allow(clippy::cast_possible_truncation)]
                        let integer = float as i64;
                        integer
                    })
            })
            .map(Some)
            .ok_or_else(|| "`expires_in` is not an integer".to_owned()),
        Some(Value::String(text)) => text
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| "`expires_in` is not an integer".to_owned()),
        Some(_) => Err("`expires_in` is not an integer".to_owned()),
    }
}

/// Reference `StoredOAuthTokens`: the grant as the keyring holds it.
#[derive(Clone, PartialEq)]
pub struct StoredTokens {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<f64>,
}

impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredTokens")
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl StoredTokens {
    fn from_token(token: &OAuthToken, now: f64) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let expires_at = token.expires_in.map(|seconds| now + seconds as f64);
        Self {
            access_token: token.access_token.clone(),
            token_type: token.token_type.clone(),
            expires_in: token.expires_in,
            scope: token.scope.clone(),
            refresh_token: token.refresh_token.clone(),
            expires_at,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        let value = serde_json::from_str::<Value>(raw).ok()?;
        let fields = value.as_object()?;
        Some(Self {
            access_token: fields.get("access_token")?.as_str()?.to_owned(),
            token_type: match fields.get("token_type") {
                None => "Bearer".to_owned(),
                Some(kind) => kind.as_str()?.to_owned(),
            },
            expires_in: lax_optional_int(fields.get("expires_in")).ok()?,
            scope: optional_string(fields.get("scope"), "scope").ok()?,
            refresh_token: optional_string(fields.get("refresh_token"), "refresh_token").ok()?,
            expires_at: match fields.get("expires_at") {
                None | Some(Value::Null) => None,
                Some(number) => Some(number.as_f64()?),
            },
        })
    }

    /// `model_dump_json()`, fields in declaration order.
    fn to_json(&self) -> String {
        ordered_json(&[
            ("access_token", json!(self.access_token)),
            ("token_type", json!(self.token_type)),
            ("expires_in", json!(self.expires_in)),
            ("scope", json!(self.scope)),
            ("refresh_token", json!(self.refresh_token)),
            ("expires_at", json!(self.expires_at)),
        ])
    }

    /// The deadline `KeyringTokenStorage.get_tokens` reads the grant with.
    #[must_use]
    pub fn expiry(&self) -> Option<f64> {
        match (self.expires_at, self.expires_in) {
            (Some(deadline), _) => Some(deadline),
            (None, Some(_)) => Some(EXPIRED_TOKEN_TIME),
            (None, None) => None,
        }
    }

    /// The `Authorization` value reference `_resolve_oauth` builds from the
    /// grant `get_tokens` validated, whose type is title-cased.
    #[must_use]
    pub fn authorization_value(&self) -> String {
        format!("{} {}", title_case(&self.token_type), self.access_token)
    }

    fn to_token(&self) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token.clone(),
            token_type: title_case(&self.token_type),
            expires_in: self.expires_in,
            scope: self.scope.clone(),
            refresh_token: self.refresh_token.clone(),
        }
    }
}

/// SDK `OAuthClientInformationFull`: a registered client.
#[derive(Clone, PartialEq)]
pub struct ClientInformation {
    fields: Vec<(&'static str, Value)>,
}

impl std::fmt::Debug for ClientInformation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientInformation")
            .field("client_id", &self.client_id())
            .finish_non_exhaustive()
    }
}

/// `OAuthClientInformationFull`'s fields in declaration order, with the kind
/// pydantic validates each as.
const CLIENT_FIELDS: [(&str, FieldKind); 19] = [
    ("redirect_uris", FieldKind::UrlList),
    ("token_endpoint_auth_method", FieldKind::AuthMethod),
    ("grant_types", FieldKind::StringList),
    ("response_types", FieldKind::StringList),
    ("scope", FieldKind::String),
    ("client_name", FieldKind::String),
    ("client_uri", FieldKind::HttpUrl),
    ("logo_uri", FieldKind::HttpUrl),
    ("contacts", FieldKind::StringList),
    ("tos_uri", FieldKind::HttpUrl),
    ("policy_uri", FieldKind::HttpUrl),
    ("jwks_uri", FieldKind::HttpUrl),
    ("jwks", FieldKind::Any),
    ("software_id", FieldKind::String),
    ("software_version", FieldKind::String),
    ("client_id", FieldKind::String),
    ("client_secret", FieldKind::String),
    ("client_id_issued_at", FieldKind::Integer),
    ("client_secret_expires_at", FieldKind::Integer),
];

#[derive(Clone, Copy)]
enum FieldKind {
    UrlList,
    AuthMethod,
    StringList,
    String,
    HttpUrl,
    Any,
    Integer,
}

impl ClientInformation {
    /// `OAuthClientInformationFull.model_validate`, normalized as
    /// `model_dump(mode="json")` writes it back.
    pub(crate) fn validate(value: &Value) -> Result<Self, String> {
        let source = value
            .as_object()
            .ok_or_else(|| "the client information is not an object".to_owned())?;
        if !source.contains_key("redirect_uris") {
            return Err("the client information has no redirect_uris".to_owned());
        }
        let mut fields = Vec::with_capacity(CLIENT_FIELDS.len());
        for (name, kind) in CLIENT_FIELDS {
            let raw = source.get(name);
            let normalized = match (kind, raw) {
                (FieldKind::StringList, None) if name == "grant_types" => {
                    json!(["authorization_code", "refresh_token"])
                }
                (FieldKind::StringList, None) if name == "response_types" => json!(["code"]),
                (_, None | Some(Value::Null)) => Value::Null,
                (FieldKind::UrlList, Some(Value::Array(items))) => {
                    if items.is_empty() {
                        return Err("redirect_uris is empty".to_owned());
                    }
                    Value::Array(
                        items
                            .iter()
                            .map(|item| {
                                item.as_str()
                                    .and_then(any_url)
                                    .map(Value::String)
                                    .ok_or_else(|| "a redirect URI is invalid".to_owned())
                            })
                            .collect::<Result<_, _>>()?,
                    )
                }
                (FieldKind::AuthMethod, Some(Value::String(method)))
                    if matches!(
                        method.as_str(),
                        "none" | "client_secret_post" | "client_secret_basic" | "private_key_jwt"
                    ) =>
                {
                    json!(method)
                }
                (FieldKind::StringList, Some(Value::Array(items)))
                    if items.iter().all(Value::is_string) =>
                {
                    Value::Array(items.clone())
                }
                (FieldKind::String, Some(Value::String(text))) => json!(text),
                (FieldKind::HttpUrl, Some(Value::String(text))) if text.is_empty() => Value::Null,
                (FieldKind::HttpUrl, Some(Value::String(text))) => {
                    json!(any_http_url(text).ok_or_else(|| format!("`{name}` is not a URL"))?)
                }
                (FieldKind::Any, Some(other)) => other.clone(),
                (FieldKind::Integer, Some(number)) => json!(
                    lax_optional_int(Some(number))
                        .map_err(|_| format!("`{name}` is not an integer"))?
                ),
                _ => return Err(format!("`{name}` has an invalid value")),
            };
            fields.push((name, normalized));
        }
        Ok(Self { fields })
    }

    fn parse(raw: &str) -> Option<Self> {
        Self::validate(&serde_json::from_str(raw).ok()?).ok()
    }

    /// A client the entry names by id, reference `build_oauth_provider`'s
    /// `fallback_client_info`, or one a metadata document identifies.
    pub(crate) fn declared(
        client_id: &str,
        redirect_uri: &str,
        scope: Option<&str>,
        full: bool,
    ) -> Self {
        let mut source = json!({
            "client_id": client_id,
            "redirect_uris": [redirect_uri],
            "token_endpoint_auth_method": "none",
        });
        if full {
            source["scope"] = json!(scope);
            source["grant_types"] = json!(["authorization_code", "refresh_token"]);
            source["response_types"] = json!(["code"]);
            source["client_name"] = json!(CLIENT_NAME);
        }
        Self::validate(&source).unwrap_or_else(|_| Self { fields: Vec::new() })
    }

    fn to_json(&self) -> String {
        ordered_json(&self.fields)
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| *field == name)
            .and_then(|(_, value)| value.as_str())
    }

    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        self.text("client_id")
    }

    fn client_secret(&self) -> Option<&str> {
        self.text("client_secret")
    }

    fn auth_method(&self) -> Option<&str> {
        self.text("token_endpoint_auth_method")
    }
}

/// Pydantic's `AnyUrl` string form.
fn any_url(text: &str) -> Option<String> {
    url::Url::parse(text).ok().map(String::from)
}

/// Pydantic's `AnyHttpUrl` string form: an `http` or `https` URL with a host.
pub(crate) fn any_http_url(text: &str) -> Option<String> {
    let url = url::Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host().is_some()).then(|| String::from(url))
}

/// Reference `Fingerprint.compute(server).model_dump_json()`.
#[must_use]
pub fn fingerprint_json(server: &McpServerConfig) -> Option<String> {
    let value = crate::mcp::authorization::oauth_fingerprint(server)?;
    let field = |name: &str| value.get(name).cloned().unwrap_or(Value::Null);
    Some(ordered_json(&[
        ("url", field("url")),
        ("scopes_sorted", field("scopes_sorted")),
        ("client_marker", field("client_marker")),
    ]))
}

/// Pydantic's `model_dump_json()`: compact, fields in declaration order,
/// non-ASCII text left as it is.
fn ordered_json(fields: &[(&str, Value)]) -> String {
    let mut rendered = String::from("{");
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            rendered.push(',');
        }
        rendered.push_str(&Value::String((*name).to_owned()).to_string());
        rendered.push(':');
        rendered.push_str(&value.to_string());
    }
    rendered.push('}');
    rendered
}

/// Where the login flow publishes the authorization URL, reference `on_url`.
pub type AuthUrlSink =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The OAuth client an MCP server entry declares, reference
/// `build_oauth_provider`'s inputs.
#[derive(Debug, Clone)]
pub(crate) struct OAuthClientConfig {
    pub alias: String,
    pub server_url: String,
    pub redirect_port: u16,
    pub redirect_uri: String,
    pub scope: Option<String>,
    pub client_metadata_url: Option<String>,
    pub fallback_client_info: Option<ClientInformation>,
}

impl OAuthClientConfig {
    /// Reference `build_oauth_provider`, before the provider exists.
    pub(crate) fn for_server(server: &McpServerConfig) -> Result<Self, McpOAuthError> {
        let McpAuthConfig::Oauth(oauth) = &server.auth else {
            return Err(McpOAuthError::Config(format!(
                "MCP server `{}` does not sign in with OAuth",
                server.alias
            )));
        };
        let redirect_uri = format!("http://127.0.0.1:{}/callback", oauth.redirect_port);
        let scope = oauth
            .scopes
            .iter()
            .filter(|scope| !scope.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let scope = (!scope.is_empty()).then_some(scope);
        let client_metadata_url = oauth.client_metadata_url.as_ref().map(ToString::to_string);
        if let Some(document) = &client_metadata_url
            && !flow::is_valid_client_metadata_url(document)
        {
            return Err(McpOAuthError::Config(format!(
                "the client metadata URL `{document}` must be an HTTPS URL with a path"
            )));
        }
        let fallback_client_info = oauth.client_id.as_deref().map(|client_id| {
            ClientInformation::declared(client_id, &redirect_uri, scope.as_deref(), true)
        });
        Ok(Self {
            alias: server.alias.clone(),
            server_url: declared_url(server),
            redirect_port: oauth.redirect_port,
            redirect_uri,
            scope,
            client_metadata_url,
            fallback_client_info,
        })
    }
}

fn declared_url(server: &McpServerConfig) -> String {
    server
        .declared
        .as_ref()
        .and_then(|declared| declared.url.clone())
        .or_else(|| crate::mcp::transport_url(&server.transport).map(ToString::to_string))
        .unwrap_or_default()
}

/// The HTTP client the flow sends through, reference `VibeAsyncHTTPClient`
/// without redirects.
fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .build()
        .map_err(|error| error.to_string())
}

/// Reference `perform_oauth_login`: signs in to `server` interactively,
/// publishing the authorization URL through `on_url`, and records the
/// fingerprint the new credential was minted for.
pub async fn perform_login(
    store: &McpOAuthStore,
    server: &McpServerConfig,
    declared_headers: &BTreeMap<String, String>,
    on_url: AuthUrlSink,
) -> Result<(), McpOAuthError> {
    let config = OAuthClientConfig::for_server(server)?;
    let alias = config.alias.clone();
    let failed = |reason: String| McpOAuthError::LoginFailed {
        alias: alias.clone(),
        reason,
    };
    let attempt = || attempt_login(store, &config, declared_headers, on_url.clone());
    let outcome = match attempt().await {
        Err(flow::FlowError::InvalidGrant(_)) => attempt().await,
        Err(flow::FlowError::TransientRefresh(_)) => {
            store.delete_credentials(&alias)?;
            attempt().await
        }
        other => other,
    };
    match outcome {
        Ok(()) => {}
        Err(flow::FlowError::TransientRefresh(reason)) => {
            return Err(failed(format!("a temporary failure: {reason}")));
        }
        Err(
            flow::FlowError::Flow(reason)
            | flow::FlowError::Http(reason)
            | flow::FlowError::Io(reason),
        ) => return Err(failed(reason)),
        Err(other) => return Err(other.into_error(&alias)),
    }
    if store
        .tokens(&alias)
        .map_err(|Headless| McpOAuthError::Headless {
            alias: alias.clone(),
        })?
        .is_none()
    {
        return Err(failed(
            "the server answered without asking for authorization, so no token was issued"
                .to_owned(),
        ));
    }
    let fingerprint = fingerprint_json(server).unwrap_or_default();
    store
        .set(&mcp_oauth_username(&alias, "fingerprint"), &fingerprint)
        .map_err(|failure| McpOAuthError::Keyring(failure.to_string()))
}

async fn attempt_login(
    store: &McpOAuthStore,
    config: &OAuthClientConfig,
    declared_headers: &BTreeMap<String, String>,
    on_url: AuthUrlSink,
) -> Result<(), flow::FlowError> {
    store
        .available(&config.alias)
        .map_err(|Headless| flow::FlowError::Headless)?;
    let http = http_client(LOGIN_TIMEOUT).map_err(flow::FlowError::Http)?;
    let mut provider = flow::Provider::new(
        http,
        store,
        config.clone(),
        flow::Interaction::Login { on_url },
    );
    let mut headers = declared_headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("accept"))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Vec<_>>();
    headers.push(("Accept".to_owned(), MCP_ACCEPT.to_owned()));
    let request = flow::Outgoing {
        method: reqwest::Method::POST,
        url: config.server_url.clone(),
        headers,
        body: flow::Body::Json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": LATEST_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": crate::telemetry::version()},
            },
        })),
        purpose: "request",
    };
    // A login is done at the headers: an authorized server may answer with an
    // event stream that never ends, so the body is never read.
    provider.run(request).await.map(drop)
}

/// How a refresh of an expired credential ended, for the authorization
/// service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Nothing was refused; the store holds whatever the exchange left.
    Completed,
    InvalidGrant,
    Failed,
    /// A failure the reference lets escape `resolve`.
    Unexpected(String),
}

/// Reference `MCPAuthenticationService._refresh_oauth`: a plain `GET` of the
/// server through a provider that refuses to open a browser.
pub async fn refresh_credential(store: &McpOAuthStore, server: &McpServerConfig) -> RefreshOutcome {
    let config = match OAuthClientConfig::for_server(server) {
        Ok(config) => config,
        Err(error) => return RefreshOutcome::Unexpected(error.to_string()),
    };
    let http = match http_client(Duration::from_millis(server.startup_timeout_ms)) {
        Ok(http) => http,
        Err(error) => return RefreshOutcome::Unexpected(error),
    };
    let url = config.server_url.clone();
    let mut provider = flow::Provider::new(http, store, config, flow::Interaction::Refuse);
    let request = flow::Outgoing {
        method: reqwest::Method::GET,
        url,
        headers: Vec::new(),
        body: flow::Body::Empty,
        purpose: "request",
    };
    match provider.run(request).await {
        Ok(response) => {
            // httpx reads the whole answer of a plain `get`.
            let _ = response.bytes().await;
            RefreshOutcome::Completed
        }
        Err(flow::FlowError::InvalidGrant(_)) => RefreshOutcome::InvalidGrant,
        Err(
            flow::FlowError::TransientRefresh(_)
            | flow::FlowError::Flow(_)
            | flow::FlowError::Callback(_)
            | flow::FlowError::PortInUse(_),
        ) => RefreshOutcome::Failed,
        Err(other) => RefreshOutcome::Unexpected(other.into_error(&server.alias).to_string()),
    }
}
