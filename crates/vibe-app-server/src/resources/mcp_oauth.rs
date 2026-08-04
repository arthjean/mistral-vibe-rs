use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use url::Url;
use vibe_core::integrations::redact;
use vibe_core::mcp::{
    DefaultMcpPeerFactory, McpError, McpFuture, McpPeer, McpPeerFactory, McpServerConfig,
    McpTransportConfig,
};

use super::{McpAuthBackend, ResourceError, ResourceFuture};
use crate::host::now_seconds;

const KEYRING_SERVICE: &str = "mistral-vibe-rs";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CALLBACK_BYTES: usize = 8 * 1024;
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

pub type ProductionMcpAdapters = (Arc<dyn McpPeerFactory>, Arc<dyn McpAuthBackend>);

pub fn production_mcp_adapters() -> Result<ProductionMcpAdapters, ResourceError> {
    let auth = Arc::new(ProductionMcpAuth::new()?);
    Ok((
        Arc::new(AuthenticatedMcpPeerFactory { auth: auth.clone() }),
        auth,
    ))
}

struct AuthenticatedMcpPeerFactory {
    auth: Arc<ProductionMcpAuth>,
}

impl McpPeerFactory for AuthenticatedMcpPeerFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        Box::pin(async move {
            let mut config = config.clone();
            if let Some(resource) = transport_url(&config.transport).cloned()
                && !transport_headers(&config.transport)
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
                && let Some(header) = self
                    .auth
                    .authorization_header(&config.alias, &resource)
                    .await
                    .map_err(|error| McpError::Transport(redact(&error.to_string())))?
                && let Some(headers) = transport_headers_mut(&mut config.transport)
            {
                headers.insert(
                    "Authorization".to_owned(),
                    header.expose_secret().to_owned(),
                );
            }
            DefaultMcpPeerFactory.connect(&config).await
        })
    }
}

struct ProductionMcpAuth {
    client: reqwest::Client,
    pending: Mutex<BTreeMap<PendingKey, PendingLogin>>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingKey {
    session_id: String,
    resource: String,
}

struct PendingLogin {
    completion: watch::Receiver<Option<Result<StoredCredential, String>>>,
    task: tokio::task::AbortHandle,
}

impl ProductionMcpAuth {
    fn new() -> Result<Self, ResourceError> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| ResourceError::Unavailable(error.to_string()))?,
            pending: Mutex::new(BTreeMap::new()),
        })
    }

    async fn begin_login(
        &self,
        session_id: &str,
        config: &McpServerConfig,
    ) -> Result<String, ResourceError> {
        let resource = transport_url(&config.transport).ok_or_else(|| {
            ResourceError::InvalidParams(format!(
                "MCP server `{}` does not use HTTP transport",
                config.alias
            ))
        })?;
        let oauth = self.discover(resource).await?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| ResourceError::Unavailable(error.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|error| ResourceError::Unavailable(error.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let client_id = self.register_client(&oauth, &redirect_uri).await?;
        let verifier = random_url_token(48)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_url_token(32)?;
        let mut authorization_url = oauth.authorization_endpoint.clone();
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("resource", resource.as_str());
            if !oauth.scopes.is_empty() {
                query.append_pair("scope", &oauth.scopes.join(" "));
            }
        }
        let (completion, receiver) = watch::channel(None);
        let pending_key = PendingKey {
            session_id: session_id.to_owned(),
            resource: resource_identity(resource)?,
        };
        let client = self.client.clone();
        let token_endpoint = oauth.token_endpoint;
        let resource = resource.clone();
        let task = tokio::spawn(async move {
            let result: Result<StoredCredential, ResourceError> = async {
                let code = receive_callback(listener, &state).await?;
                let response = client
                    .post(token_endpoint.clone())
                    .form(&[
                        ("grant_type", "authorization_code"),
                        ("code", code.as_str()),
                        ("redirect_uri", redirect_uri.as_str()),
                        ("client_id", client_id.as_str()),
                        ("code_verifier", verifier.as_str()),
                        ("resource", resource.as_str()),
                    ])
                    .send()
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let token = decode_token_response(response).await?;
                Ok(StoredCredential::from_token(
                    resource,
                    token_endpoint,
                    client_id,
                    token,
                ))
            }
            .await;
            let _ = completion.send(Some(result.map_err(|error| error.to_string())));
        });
        let mut pending = self.pending.lock().await;
        if let Some(previous) = pending.insert(
            pending_key,
            PendingLogin {
                completion: receiver,
                task: task.abort_handle(),
            },
        ) {
            previous.task.abort();
        }
        Ok(authorization_url.to_string())
    }

    async fn discover(&self, resource: &Url) -> Result<OAuthMetadata, ResourceError> {
        let challenge = self
            .client
            .get(resource.clone())
            .header("Accept", "application/json, text/event-stream")
            .send()
            .await
            .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
        let advertised = challenge
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| bearer_parameter(value, "resource_metadata"))
            .and_then(|value| Url::parse(value).ok());
        let protected_url = advertised.unwrap_or_else(|| protected_resource_metadata_url(resource));
        require_https(&protected_url, "protected-resource metadata")?;
        let protected = self
            .client
            .get(protected_url)
            .send()
            .await
            .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
        let protected = response_json::<ProtectedResourceMetadata>(protected).await?;
        validate_protected_resource(&protected, resource)?;
        let issuer = protected
            .authorization_servers
            .first()
            .cloned()
            .ok_or_else(|| {
                ResourceError::Unavailable(
                    "MCP OAuth metadata omitted authorization_servers".to_owned(),
                )
            })?;
        require_https(&issuer, "authorization server")?;
        let metadata_url = authorization_metadata_url(&issuer);
        let response = self
            .client
            .get(metadata_url)
            .send()
            .await
            .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
        let server = response_json::<AuthorizationServerMetadata>(response).await?;
        validate_authorization_issuer(&server.issuer, &issuer)?;
        require_https(&server.authorization_endpoint, "authorization endpoint")?;
        require_https(&server.token_endpoint, "token endpoint")?;
        if !server
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(ResourceError::Unavailable(
                "MCP authorization server does not support PKCE S256".to_owned(),
            ));
        }
        Ok(OAuthMetadata {
            authorization_endpoint: server.authorization_endpoint,
            token_endpoint: server.token_endpoint,
            registration_endpoint: server.registration_endpoint,
            scopes: protected.scopes_supported,
        })
    }

    async fn register_client(
        &self,
        oauth: &OAuthMetadata,
        redirect_uri: &str,
    ) -> Result<String, ResourceError> {
        let endpoint = oauth.registration_endpoint.as_ref().ok_or_else(|| {
            ResourceError::Unavailable(
                "MCP authorization server does not expose dynamic client registration".to_owned(),
            )
        })?;
        require_https(endpoint, "registration endpoint")?;
        let response = self
            .client
            .post(endpoint.clone())
            .json(&json!({
                "client_name": "Mistral Vibe",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            }))
            .send()
            .await
            .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
        let client = response_json::<ClientRegistration>(response).await?;
        if client.client_id.is_empty() {
            return Err(ResourceError::Unavailable(
                "MCP client registration omitted client_id".to_owned(),
            ));
        }
        Ok(client.client_id)
    }

    async fn authorization_header(
        &self,
        alias: &str,
        resource: &Url,
    ) -> Result<Option<SecretString>, ResourceError> {
        let Some(mut credential) = load_credential(resource).await? else {
            return Ok(None);
        };
        if resource_identity(&credential.resource)? != resource_identity(resource)? {
            delete_credential(resource).await?;
            return Ok(None);
        }
        if credential.expires_at <= now_seconds().saturating_add(30) {
            credential = self.refresh(alias, credential).await?;
        }
        Ok(Some(SecretString::from(format!(
            "Bearer {}",
            credential.access_token
        ))))
    }

    async fn complete_login(
        &self,
        session_id: &str,
        config: &McpServerConfig,
    ) -> Result<bool, ResourceError> {
        let resource = transport_url(&config.transport).ok_or_else(|| {
            ResourceError::InvalidParams(format!(
                "MCP server `{}` does not use HTTP transport",
                config.alias
            ))
        })?;
        let key = PendingKey {
            session_id: session_id.to_owned(),
            resource: resource_identity(resource)?,
        };
        let mut pending = self.pending.lock().await;
        let completion = pending
            .get(&key)
            .map(|login| login.completion.borrow().clone());
        match completion {
            Some(None) => Ok(false),
            Some(Some(Err(error))) => {
                pending.remove(&key);
                Err(ResourceError::Unavailable(redact(&error)))
            }
            Some(Some(Ok(credential))) => {
                save_credential(credential).await?;
                pending.remove(&key);
                Ok(true)
            }
            None => Ok(load_credential(resource).await?.is_some()),
        }
    }

    async fn refresh(
        &self,
        alias: &str,
        credential: StoredCredential,
    ) -> Result<StoredCredential, ResourceError> {
        let refresh_token = credential.refresh_token.as_deref().ok_or_else(|| {
            ResourceError::Unavailable(format!(
                "MCP credentials for `{alias}` expired; run `/mcp login {alias}`"
            ))
        })?;
        let response = self
            .client
            .post(credential.token_endpoint.clone())
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", credential.client_id.as_str()),
                ("resource", credential.resource.as_str()),
            ])
            .send()
            .await
            .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
        let token = decode_token_response(response).await?;
        let refreshed = StoredCredential::from_token(
            credential.resource,
            credential.token_endpoint,
            credential.client_id,
            token.with_fallback_refresh(refresh_token),
        );
        save_credential(refreshed.clone()).await?;
        Ok(refreshed)
    }
}

impl McpAuthBackend for ProductionMcpAuth {
    fn login<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, String> {
        Box::pin(self.begin_login(session_id, config))
    }

    fn complete<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, bool> {
        Box::pin(self.complete_login(session_id, config))
    }

    fn logout<'a>(
        &'a self,
        session_id: &'a str,
        config: &'a McpServerConfig,
    ) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            let resource = transport_url(&config.transport).ok_or_else(|| {
                ResourceError::InvalidParams(format!(
                    "MCP server `{}` does not use HTTP transport",
                    config.alias
                ))
            })?;
            let key = PendingKey {
                session_id: session_id.to_owned(),
                resource: resource_identity(resource)?,
            };
            if let Some(pending) = self.pending.lock().await.remove(&key) {
                pending.task.abort();
            }
            delete_credential(resource).await
        })
    }

    fn close_session<'a>(&'a self, session_id: &'a str) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            let mut pending = self.pending.lock().await;
            let keys = pending
                .keys()
                .filter(|key| key.session_id == session_id)
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                if let Some(login) = pending.remove(&key) {
                    login.task.abort();
                }
            }
            Ok(())
        })
    }
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    resource: Url,
    #[serde(default)]
    authorization_servers: Vec<Url>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

fn validate_protected_resource(
    metadata: &ProtectedResourceMetadata,
    requested: &Url,
) -> Result<(), ResourceError> {
    if metadata.resource.fragment().is_none()
        && requested.fragment().is_none()
        && metadata.resource.as_str() == requested.as_str()
    {
        Ok(())
    } else {
        Err(ResourceError::Unavailable(
            "MCP protected-resource metadata does not match the requested resource".to_owned(),
        ))
    }
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    issuer: Url,
    authorization_endpoint: Url,
    token_endpoint: Url,
    #[serde(default)]
    registration_endpoint: Option<Url>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

struct OAuthMetadata {
    authorization_endpoint: Url,
    token_endpoint: Url,
    registration_endpoint: Option<Url>,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct ClientRegistration {
    client_id: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn with_fallback_refresh(mut self, fallback: &str) -> Self {
        if self.refresh_token.is_none() {
            self.refresh_token = Some(fallback.to_owned());
        }
        self
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredCredential {
    resource: Url,
    token_endpoint: Url,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64,
}

impl StoredCredential {
    fn from_token(
        resource: Url,
        token_endpoint: Url,
        client_id: String,
        token: TokenResponse,
    ) -> Self {
        Self {
            resource,
            token_endpoint,
            client_id,
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token
                .expires_in
                .map_or(u64::MAX, |ttl| now_seconds().saturating_add(ttl)),
        }
    }
}

async fn decode_token_response(
    response: reqwest::Response,
) -> Result<TokenResponse, ResourceError> {
    let token: TokenResponse = response_json(response).await?;
    validate_token_response(&token)?;
    Ok(token)
}

fn validate_token_response(token: &TokenResponse) -> Result<(), ResourceError> {
    validate_token_type(&token.token_type)?;
    if token.access_token.trim().is_empty() {
        return Err(ResourceError::Unavailable(
            "MCP OAuth access_token must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn validate_token_type(token_type: &str) -> Result<(), ResourceError> {
    if token_type.eq_ignore_ascii_case("bearer") {
        Ok(())
    } else {
        Err(ResourceError::Unavailable(
            "MCP OAuth token_type must be Bearer".to_owned(),
        ))
    }
}

async fn response_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ResourceError> {
    if !response.status().is_success() {
        return Err(ResourceError::Unavailable(format!(
            "MCP OAuth endpoint returned HTTP {}",
            response.status()
        )));
    }
    super::bounded_json(response, "MCP OAuth response", MAX_OAUTH_RESPONSE_BYTES).await
}

async fn receive_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<String, ResourceError> {
    let (mut stream, _) = tokio::time::timeout(LOGIN_TIMEOUT, listener.accept())
        .await
        .map_err(|_| ResourceError::Unavailable("MCP OAuth login timed out".to_owned()))?
        .map_err(|error| ResourceError::Unavailable(error.to_string()))?;
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| ResourceError::Unavailable(error.to_string()))?;
        if read == 0 {
            break;
        }
        if request.len().saturating_add(read) > MAX_CALLBACK_BYTES {
            return Err(ResourceError::Unavailable(
                "MCP OAuth callback exceeded its byte budget".to_owned(),
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&request)
        .map_err(|_| ResourceError::Unavailable("MCP OAuth callback was not UTF-8".to_owned()))?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| ResourceError::Unavailable("MCP OAuth callback was malformed".to_owned()))?;
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| ResourceError::Unavailable("MCP OAuth callback URL was invalid".to_owned()))?;
    let params = callback.query_pairs().collect::<BTreeMap<_, _>>();
    let code = params.get("code").map(ToString::to_string);
    let valid_state = params
        .get("state")
        .is_some_and(|state| state.as_ref() == expected_state);
    let (status, body) = if code.is_some() && valid_state {
        ("200 OK", "Login complete. You can close this tab.")
    } else {
        ("400 Bad Request", "Login failed. Return to Mistral Vibe.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| ResourceError::Unavailable(error.to_string()))?;
    if !valid_state {
        return Err(ResourceError::Unavailable(
            "MCP OAuth callback state did not match".to_owned(),
        ));
    }
    code.ok_or_else(|| ResourceError::Unavailable("MCP OAuth callback omitted code".to_owned()))
}

fn bearer_parameter<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    let header = header.trim();
    let challenge = header.get(..6)?;
    if !challenge.eq_ignore_ascii_case("bearer") {
        return None;
    }
    header
        .get(6..)?
        .trim_start()
        .split(',')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .map(|value| value.trim_matches('"'))
}

fn protected_resource_metadata_url(resource: &Url) -> Url {
    let mut url = resource.clone();
    let resource_path = resource.path().trim_end_matches('/');
    url.set_path(&format!(
        "/.well-known/oauth-protected-resource{resource_path}"
    ));
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn authorization_metadata_url(issuer: &Url) -> Url {
    let mut url = issuer.clone();
    let issuer_path = issuer.path().trim_end_matches('/');
    url.set_path(&format!(
        "/.well-known/oauth-authorization-server{issuer_path}"
    ));
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn validate_authorization_issuer(
    metadata_issuer: &Url,
    advertised_issuer: &Url,
) -> Result<(), ResourceError> {
    require_https(metadata_issuer, "authorization issuer")?;
    if metadata_issuer.fragment().is_none()
        && advertised_issuer.fragment().is_none()
        && metadata_issuer.as_str() == advertised_issuer.as_str()
    {
        Ok(())
    } else {
        Err(ResourceError::Unavailable(
            "MCP authorization metadata issuer does not match the advertised issuer".to_owned(),
        ))
    }
}

fn require_https(url: &Url, field: &str) -> Result<(), ResourceError> {
    if url.scheme() == "https" {
        Ok(())
    } else {
        Err(ResourceError::Unavailable(format!(
            "MCP OAuth {field} must use HTTPS"
        )))
    }
}

fn random_url_token(bytes: usize) -> Result<String, ResourceError> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value)
        .map_err(|_| ResourceError::Unavailable("secure randomness is unavailable".to_owned()))?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn transport_url(transport: &McpTransportConfig) -> Option<&Url> {
    match transport {
        McpTransportConfig::StreamableHttp { url, .. } => Some(url),
        McpTransportConfig::Stdio { .. } => None,
    }
}

fn transport_headers(transport: &McpTransportConfig) -> &BTreeMap<String, String> {
    match transport {
        McpTransportConfig::StreamableHttp { headers, .. } => headers,
        McpTransportConfig::Stdio { .. } => &EMPTY_HEADERS,
    }
}

static EMPTY_HEADERS: BTreeMap<String, String> = BTreeMap::new();

fn transport_headers_mut(
    transport: &mut McpTransportConfig,
) -> Option<&mut BTreeMap<String, String>> {
    match transport {
        McpTransportConfig::StreamableHttp { headers, .. } => Some(headers),
        McpTransportConfig::Stdio { .. } => None,
    }
}

fn resource_identity(url: &Url) -> Result<String, ResourceError> {
    if url.fragment().is_some() {
        return Err(ResourceError::InvalidParams(
            "MCP OAuth resource URL must not contain a fragment".to_owned(),
        ));
    }
    Ok(url.as_str().to_owned())
}

fn keyring_account(resource: &Url) -> Result<String, ResourceError> {
    let identity = resource_identity(resource)?;
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(identity.as_bytes()));
    Ok(format!("mcp-oauth:{fingerprint}"))
}

async fn load_credential(resource: &Url) -> Result<Option<StoredCredential>, ResourceError> {
    let account = keyring_account(resource)?;
    let encoded = tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
            .map_err(|_| ResourceError::Unavailable("native keyring is unavailable".to_owned()))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(ResourceError::Unavailable(
                "native keyring is unavailable".to_owned(),
            )),
        }
    })
    .await
    .map_err(|_| ResourceError::Unavailable("native keyring task failed".to_owned()))??;
    encoded
        .map(|encoded| {
            serde_json::from_str(&encoded).map_err(|_| {
                ResourceError::Unavailable("stored MCP OAuth credential is corrupt".to_owned())
            })
        })
        .transpose()
}

async fn save_credential(credential: StoredCredential) -> Result<(), ResourceError> {
    let account = keyring_account(&credential.resource)?;
    let encoded = serde_json::to_string(&credential)
        .map_err(|_| ResourceError::Unavailable("cannot encode MCP credential".to_owned()))?;
    tokio::task::spawn_blocking(move || {
        keyring::Entry::new(KEYRING_SERVICE, &account)
            .and_then(|entry| entry.set_password(&encoded))
            .map_err(|_| ResourceError::Unavailable("native keyring is unavailable".to_owned()))
    })
    .await
    .map_err(|_| ResourceError::Unavailable("native keyring task failed".to_owned()))?
}

async fn delete_credential(resource: &Url) -> Result<(), ResourceError> {
    let account = keyring_account(resource)?;
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
            .map_err(|_| ResourceError::Unavailable("native keyring is unavailable".to_owned()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(ResourceError::Unavailable(
                "native keyring is unavailable".to_owned(),
            )),
        }
    })
    .await
    .map_err(|_| ResourceError::Unavailable("native keyring task failed".to_owned()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_config(alias: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            alias: alias.to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: Url::parse(url).expect("fixture URL"),
                headers: BTreeMap::new(),
            },
            enabled: false,
            disabled_tools: Default::default(),
            startup_timeout_ms: 1_000,
            tool_timeout_ms: 1_000,
        }
    }

    #[test]
    fn extracts_quoted_resource_metadata() {
        assert_eq!(
            bearer_parameter(
                "Bearer resource_metadata=\"https://mcp.test/meta\", realm=\"mcp\"",
                "resource_metadata"
            ),
            Some("https://mcp.test/meta")
        );
    }

    #[test]
    fn authorization_metadata_preserves_issuer_path() {
        let issuer = Url::parse("https://auth.test/tenant").expect("issuer");
        assert_eq!(
            authorization_metadata_url(&issuer).as_str(),
            "https://auth.test/.well-known/oauth-authorization-server/tenant"
        );
        let resource = Url::parse("https://mcp.test/tenant/rpc").expect("resource");
        assert_eq!(
            protected_resource_metadata_url(&resource).as_str(),
            "https://mcp.test/.well-known/oauth-protected-resource/tenant/rpc"
        );
        assert!(validate_authorization_issuer(&issuer, &issuer).is_ok());
        assert!(
            validate_authorization_issuer(
                &Url::parse("https://other.test/tenant").expect("other issuer"),
                &issuer,
            )
            .is_err()
        );
        assert!(validate_token_type("Bearer").is_ok());
        assert!(validate_token_type("bearer").is_ok());
        assert!(validate_token_type("DPoP").is_err());
    }

    #[test]
    fn protected_resource_metadata_is_bound_to_the_requested_mcp() {
        let requested = Url::parse("https://mcp.test/rpc").expect("requested resource");
        let matching = ProtectedResourceMetadata {
            resource: requested.clone(),
            authorization_servers: Vec::new(),
            scopes_supported: Vec::new(),
        };
        assert!(validate_protected_resource(&matching, &requested).is_ok());

        let trailing_slash = ProtectedResourceMetadata {
            resource: Url::parse("https://mcp.test/rpc/").expect("metadata resource"),
            authorization_servers: Vec::new(),
            scopes_supported: Vec::new(),
        };
        assert!(validate_protected_resource(&trailing_slash, &requested).is_err());

        let mismatched = ProtectedResourceMetadata {
            resource: Url::parse("https://attacker.test/rpc").expect("metadata resource"),
            authorization_servers: Vec::new(),
            scopes_supported: Vec::new(),
        };
        assert!(matches!(
            validate_protected_resource(&mismatched, &requested),
            Err(ResourceError::Unavailable(message)) if message.contains("does not match")
        ));
        assert!(
            serde_json::from_str::<ProtectedResourceMetadata>(
                r#"{"authorization_servers":["https://auth.test"]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn resource_identity_and_tokens_are_exact() {
        let resource = Url::parse("https://mcp.test/rpc").expect("resource");
        let trailing = Url::parse("https://mcp.test/rpc/").expect("trailing resource");
        assert_ne!(
            resource_identity(&resource).expect("identity"),
            resource_identity(&trailing).expect("trailing identity")
        );
        assert!(
            resource_identity(&Url::parse("https://mcp.test/rpc#fragment").expect("fragment"))
                .is_err()
        );
        assert!(
            validate_token_response(&TokenResponse {
                access_token: "   ".to_owned(),
                token_type: "Bearer".to_owned(),
                refresh_token: None,
                expires_in: None,
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn completion_poll_is_non_blocking_and_pending_keys_are_session_resource_scoped() {
        let auth = ProductionMcpAuth::new().expect("OAuth client");
        let first = http_config("shared", "https://first.example/mcp");
        let second = http_config("shared", "https://second.example/mcp");
        let (sender, receiver) = watch::channel(None);
        let task = tokio::spawn(std::future::pending::<()>());
        auth.pending.lock().await.insert(
            PendingKey {
                session_id: "session-a".to_owned(),
                resource: "https://first.example/mcp".to_owned(),
            },
            PendingLogin {
                completion: receiver,
                task: task.abort_handle(),
            },
        );

        assert!(
            !tokio::time::timeout(
                Duration::from_millis(50),
                auth.complete_login("session-a", &first)
            )
            .await
            .expect("poll returns immediately")
            .expect("pending is not an error")
        );
        assert_eq!(auth.pending.lock().await.len(), 1);
        assert_ne!(
            keyring_account(transport_url(&first.transport).expect("first resource"))
                .expect("first account"),
            keyring_account(transport_url(&second.transport).expect("second resource"))
                .expect("second account")
        );
        drop(sender);
        task.abort();
    }

    #[tokio::test]
    async fn closing_a_session_aborts_only_its_pending_callbacks_before_persistence() {
        let auth = ProductionMcpAuth::new().expect("OAuth client");
        let (_first_sender, first_receiver) = watch::channel(None);
        let (_second_sender, second_receiver) = watch::channel(None);
        let first_task = tokio::spawn(std::future::pending::<()>());
        let second_task = tokio::spawn(std::future::pending::<()>());
        auth.pending.lock().await.extend([
            (
                PendingKey {
                    session_id: "session-a".to_owned(),
                    resource: "https://first.example/mcp".to_owned(),
                },
                PendingLogin {
                    completion: first_receiver,
                    task: first_task.abort_handle(),
                },
            ),
            (
                PendingKey {
                    session_id: "session-b".to_owned(),
                    resource: "https://second.example/mcp".to_owned(),
                },
                PendingLogin {
                    completion: second_receiver,
                    task: second_task.abort_handle(),
                },
            ),
        ]);

        auth.close_session("session-a")
            .await
            .expect("session callback cleanup");
        assert_eq!(auth.pending.lock().await.len(), 1);
        assert_eq!(
            auth.pending
                .lock()
                .await
                .keys()
                .next()
                .map(|key| key.session_id.as_str()),
            Some("session-b")
        );
        assert!(
            first_task
                .await
                .expect_err("closed callback is aborted")
                .is_cancelled()
        );
        second_task.abort();
    }
}
