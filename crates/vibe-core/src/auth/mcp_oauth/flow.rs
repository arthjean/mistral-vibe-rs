//! The MCP SDK's OAuth client, step for step.
//!
//! `OAuthClientProvider.async_auth_flow` (`mcp/client/auth/oauth2.py`, SDK
//! 1.28.1) wraps one request: it refreshes an expired grant it can refresh,
//! sends the request with the bearer it holds, and on a `401` walks the
//! discovery chain (protected-resource metadata, then authorization-server
//! metadata), picks the scope, registers a client, sends the operator to the
//! authorization endpoint, exchanges the code and sends the request again. A
//! `403` naming `insufficient_scope` authorizes again; any `403` resends. The
//! reference's `RefreshAwareOAuthClientProvider` changes how a refresh answer
//! is read, which [`Provider::handle_refresh_response`] carries.
//!
//! The helpers of `mcp/client/auth/utils.py` and `mcp/shared/auth_utils.py`
//! are here too, with Python's `urllib.parse` behavior where the URLs they
//! build depend on it.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    AuthUrlSink, ClientInformation, Headless, LATEST_PROTOCOL_VERSION, McpOAuthError,
    McpOAuthStore, OAUTH_INVALID_GRANT, OAuthClientConfig, OAuthToken, any_http_url, now_seconds,
};

/// What the flow may do when the operator has to authorize.
pub(crate) enum Interaction {
    /// Publish the authorization URL and wait on the loopback callback.
    Login { on_url: AuthUrlSink },
    /// Reference `_refresh_oauth`'s handlers: refuse, which ends the flow.
    Refuse,
}

#[derive(Clone)]
pub(crate) enum Body {
    Empty,
    Json(Value),
    Form(Vec<(String, String)>),
}

/// One request the flow sends, `httpx.Request`.
#[derive(Clone)]
pub(crate) struct Outgoing {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// What the request is for, named for diagnostics only.
    pub purpose: &'static str,
}

impl Outgoing {
    fn get(url: String, purpose: &'static str) -> Self {
        Self {
            method: Method::GET,
            url,
            headers: vec![(
                "mcp-protocol-version".to_owned(),
                LATEST_PROTOCOL_VERSION.to_owned(),
            )],
            body: Body::Empty,
            purpose,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn set_header(&mut self, name: &str, value: String) {
        self.headers
            .retain(|(header, _)| !header.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value));
    }
}

/// Why the flow stopped, by the class the SDK or the reference raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FlowError {
    /// `OAuthFlowError`, `OAuthTokenError` and `OAuthRegistrationError`.
    Flow(String),
    /// `MCPOAuthInvalidGrant`, with its reason.
    InvalidGrant(String),
    /// `MCPOAuthTransientRefreshError`, with its reason.
    TransientRefresh(String),
    /// The loopback callback's `MCPOAuthError`, with its detail.
    Callback(String),
    /// `MCPOAuthPortInUse`, with the port.
    PortInUse(u16),
    /// `httpx.HTTPError`.
    Http(String),
    /// `OSError`.
    Io(String),
    /// `keyring.errors.KeyringError`.
    Keyring(String),
    /// `MCPOAuthHeadlessError`.
    Headless,
}

impl FlowError {
    pub(crate) fn into_error(self, alias: &str) -> McpOAuthError {
        let alias = alias.to_owned();
        match self {
            Self::Flow(reason) | Self::Http(reason) | Self::Io(reason) => {
                McpOAuthError::LoginFailed { alias, reason }
            }
            Self::InvalidGrant(reason) => McpOAuthError::InvalidGrant { alias, reason },
            Self::TransientRefresh(reason) => McpOAuthError::TransientRefresh { alias, reason },
            Self::Callback(detail) => McpOAuthError::Callback { alias, detail },
            Self::PortInUse(port) => McpOAuthError::PortInUse { alias, port },
            Self::Keyring(reason) => McpOAuthError::Keyring(reason),
            Self::Headless => McpOAuthError::Headless { alias },
        }
    }
}

fn keyring(failure: super::KeyringFailure) -> FlowError {
    FlowError::Keyring(failure.to_string())
}

/// SDK `ProtectedResourceMetadata`, the fields the flow reads.
struct ProtectedResource {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Option<Vec<String>>,
}

/// SDK `OAuthMetadata`, the fields the flow reads.
struct AuthorizationServer {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Option<Vec<String>>,
    client_id_metadata_document_supported: Option<bool>,
}

/// SDK `OAuthClientProvider` with its `OAuthContext`.
pub(crate) struct Provider<'a> {
    http: reqwest::Client,
    store: &'a McpOAuthStore,
    config: OAuthClientConfig,
    interaction: Interaction,
    /// `client_metadata.scope`.
    scope: Option<String>,
    protected_resource: Option<ProtectedResource>,
    oauth_metadata: Option<AuthorizationServer>,
    auth_server_url: Option<String>,
    protocol_version: Option<String>,
    client_info: Option<ClientInformation>,
    tokens: Option<OAuthToken>,
    token_expiry: Option<f64>,
    initialized: bool,
}

impl<'a> Provider<'a> {
    pub(crate) fn new(
        http: reqwest::Client,
        store: &'a McpOAuthStore,
        config: OAuthClientConfig,
        interaction: Interaction,
    ) -> Self {
        let scope = config.scope.clone();
        Self {
            http,
            store,
            config,
            interaction,
            scope,
            protected_resource: None,
            oauth_metadata: None,
            auth_server_url: None,
            protocol_version: None,
            client_info: None,
            tokens: None,
            token_expiry: None,
            initialized: false,
        }
    }

    fn alias(&self) -> &str {
        &self.config.alias
    }

    /// `_initialize`, with the reference override that reads the deadline
    /// back from the store.
    fn initialize(&mut self) -> Result<(), FlowError> {
        let stored = self
            .store
            .tokens(&self.config.alias)
            .map_err(|Headless| FlowError::Headless)?;
        self.token_expiry = stored.as_ref().and_then(super::StoredTokens::expiry);
        self.tokens = stored.as_ref().map(super::StoredTokens::to_token);
        self.client_info = self
            .store
            .client_info(&self.config.alias)
            .map_err(|Headless| FlowError::Headless)?
            .or_else(|| self.config.fallback_client_info.clone());
        self.initialized = true;
        Ok(())
    }

    fn is_token_valid(&self) -> bool {
        self.tokens
            .as_ref()
            .is_some_and(|tokens| !tokens.access_token.is_empty())
            && self
                .token_expiry
                .filter(|expiry| *expiry != 0.0)
                .is_none_or(|expiry| now_seconds() <= expiry)
    }

    fn can_refresh_token(&self) -> bool {
        self.tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.as_deref())
            .is_some_and(|token| !token.is_empty())
            && self.client_info.is_some()
    }

    fn clear_tokens(&mut self) {
        self.tokens = None;
        self.token_expiry = None;
    }

    fn add_auth_header(&self, request: &mut Outgoing) {
        if let Some(tokens) = &self.tokens
            && !tokens.access_token.is_empty()
        {
            request.set_header("Authorization", format!("Bearer {}", tokens.access_token));
        }
    }

    /// `async_auth_flow`.
    pub(crate) async fn run(
        &mut self,
        mut request: Outgoing,
    ) -> Result<reqwest::Response, FlowError> {
        if !self.initialized {
            self.initialize()?;
        }
        self.protocol_version = request.header("mcp-protocol-version").map(str::to_owned);
        if !self.is_token_valid() && self.can_refresh_token() {
            let refresh = self.refresh_request()?;
            let response = self.send(&refresh).await?;
            if !self.handle_refresh_response(response).await? {
                self.initialized = false;
            }
        }
        if self.is_token_valid() {
            self.add_auth_header(&mut request);
        }
        let response = self.send(&request).await?;
        match response.status().as_u16() {
            401 => {
                let challenge = www_authenticate(&response);
                drop(response);
                self.authorize_after_challenge(challenge.as_deref()).await?;
                self.add_auth_header(&mut request);
                self.send(&request).await
            }
            403 => {
                let challenge = www_authenticate(&response);
                drop(response);
                if extract_field(challenge.as_deref(), "error").as_deref()
                    == Some("insufficient_scope")
                {
                    self.scope = client_metadata_scopes(
                        extract_field(challenge.as_deref(), "scope"),
                        self.protected_resource.as_ref(),
                        None,
                    );
                    let token_request = self.perform_authorization().await?;
                    let token_response = self.send(&token_request).await?;
                    self.handle_token_response(token_response).await?;
                }
                self.add_auth_header(&mut request);
                self.send(&request).await
            }
            _ => Ok(response),
        }
    }

    /// The `401` branch of `async_auth_flow`.
    async fn authorize_after_challenge(
        &mut self,
        challenge: Option<&str>,
    ) -> Result<(), FlowError> {
        let advertised = extract_field(challenge, "resource_metadata");
        for url in protected_resource_metadata_urls(advertised.as_deref(), &self.config.server_url)
        {
            let response = self
                .send(&Outgoing::get(url, "protected-resource metadata"))
                .await?;
            if let Some(metadata) = protected_resource_response(response).await {
                self.validate_resource_match(&metadata)?;
                self.auth_server_url = metadata.authorization_servers.first().cloned();
                self.protected_resource = Some(metadata);
                break;
            }
        }
        for url in authorization_server_metadata_urls(
            self.auth_server_url.as_deref(),
            &self.config.server_url,
        ) {
            let response = self
                .send(&Outgoing::get(url, "authorization-server metadata"))
                .await?;
            let (proceed, metadata) = authorization_server_response(response).await;
            if !proceed {
                break;
            }
            if let Some(metadata) = metadata {
                self.oauth_metadata = Some(metadata);
                break;
            }
        }
        self.scope = client_metadata_scopes(
            extract_field(challenge, "scope"),
            self.protected_resource.as_ref(),
            self.oauth_metadata.as_ref(),
        );
        if self.client_info.is_none() {
            let client_info = if let Some(document) =
                self.config.client_metadata_url.clone().filter(|_| {
                    self.oauth_metadata.as_ref().is_some_and(|metadata| {
                        metadata.client_id_metadata_document_supported == Some(true)
                    })
                }) {
                ClientInformation::declared(&document, &self.config.redirect_uri, None, false)
            } else {
                let request = self.registration_request();
                let response = self.send(&request).await?;
                registration_response(response).await?
            };
            self.store
                .set_client_info(self.alias(), &client_info)
                .map_err(keyring)?;
            self.client_info = Some(client_info);
        }
        let token_request = self.perform_authorization().await?;
        let token_response = self.send(&token_request).await?;
        self.handle_token_response(token_response).await
    }

    /// `_validate_resource_match`.
    fn validate_resource_match(&self, metadata: &ProtectedResource) -> Result<(), FlowError> {
        let expected = resource_url_from_server_url(&self.config.server_url);
        if check_resource_allowed(&expected, &metadata.resource) {
            Ok(())
        } else {
            Err(FlowError::Flow(format!(
                "the protected resource {} does not cover {expected}",
                metadata.resource
            )))
        }
    }

    fn authorization_base_url(&self) -> String {
        let parsed = PyUrl::parse(&self.config.server_url);
        format!("{}://{}", parsed.scheme, parsed.netloc)
    }

    /// `get_resource_url`.
    fn resource_url(&self) -> String {
        let resource = resource_url_from_server_url(&self.config.server_url);
        match &self.protected_resource {
            Some(metadata) if check_resource_allowed(&resource, &metadata.resource) => {
                metadata.resource.clone()
            }
            _ => resource,
        }
    }

    /// `should_include_resource_param`.
    fn include_resource(&self) -> bool {
        self.protected_resource.is_some()
            || self
                .protocol_version
                .as_deref()
                .is_some_and(|version| !version.is_empty() && version >= "2025-06-18")
    }

    fn token_endpoint(&self) -> String {
        self.oauth_metadata.as_ref().map_or_else(
            || format!("{}/token", self.authorization_base_url()),
            |metadata| metadata.token_endpoint.clone(),
        )
    }

    /// `create_client_registration_request`.
    fn registration_request(&self) -> Outgoing {
        let url = self
            .oauth_metadata
            .as_ref()
            .and_then(|metadata| metadata.registration_endpoint.clone())
            .unwrap_or_else(|| format!("{}/register", self.authorization_base_url()));
        let mut metadata = json!({
            "redirect_uris": [self.config.redirect_uri],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "client_name": super::CLIENT_NAME,
        });
        if let Some(scope) = &self.scope {
            metadata["scope"] = json!(scope);
        }
        Outgoing {
            method: Method::POST,
            url,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: Body::Json(metadata),
            purpose: "client registration",
        }
    }

    /// `prepare_token_auth`.
    fn prepare_token_auth(
        &self,
        data: &mut Vec<(String, String)>,
        headers: &mut Vec<(String, String)>,
    ) {
        let Some(client) = &self.client_info else {
            return;
        };
        match (
            client.auth_method(),
            client.client_id(),
            client.client_secret(),
        ) {
            (Some("client_secret_basic"), Some(id), Some(secret))
                if !id.is_empty() && !secret.is_empty() =>
            {
                let credentials = format!("{}:{}", quote(id), quote(secret));
                headers.push((
                    "Authorization".to_owned(),
                    format!("Basic {}", STANDARD.encode(credentials)),
                ));
                data.retain(|(name, _)| name != "client_secret");
            }
            (Some("client_secret_post"), _, Some(secret)) if !secret.is_empty() => {
                data.push(("client_secret".to_owned(), secret.to_owned()));
            }
            _ => {}
        }
    }

    fn token_request(&self, mut data: Vec<(String, String)>, purpose: &'static str) -> Outgoing {
        if self.include_resource() {
            data.push(("resource".to_owned(), self.resource_url()));
        }
        let mut headers = vec![(
            "Content-Type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        )];
        self.prepare_token_auth(&mut data, &mut headers);
        Outgoing {
            method: Method::POST,
            url: self.token_endpoint(),
            headers,
            body: Body::Form(data),
            purpose,
        }
    }

    /// `_refresh_token`.
    fn refresh_request(&self) -> Result<Outgoing, FlowError> {
        let refresh_token = self
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.clone())
            .ok_or_else(|| FlowError::Flow("no refresh token is available".to_owned()))?;
        let client_id = self
            .client_info
            .as_ref()
            .and_then(ClientInformation::client_id)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| FlowError::Flow("no client information is available".to_owned()))?;
        Ok(self.token_request(
            vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), refresh_token),
                ("client_id".to_owned(), client_id.to_owned()),
            ],
            "token refresh",
        ))
    }

    /// `RefreshAwareOAuthClientProvider._handle_refresh_response`.
    async fn handle_refresh_response(
        &mut self,
        response: reqwest::Response,
    ) -> Result<bool, FlowError> {
        let status = response.status().as_u16();
        if status == 200 {
            let previous = self
                .tokens
                .as_ref()
                .and_then(|tokens| tokens.refresh_token.clone());
            let body = response
                .bytes()
                .await
                .map_err(|error| FlowError::Http(error.to_string()))?;
            let Some(mut token) = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| OAuthToken::validate(&value).ok())
            else {
                self.clear_tokens();
                return Ok(false);
            };
            self.store_token(&token)?;
            if token.refresh_token.is_none()
                && let Some(previous) = previous
            {
                token.refresh_token = Some(previous);
                self.store_token(&token)?;
            }
            return Ok(true);
        }
        let (reason, invalid_grant) = classify_refresh_error(status, response).await;
        if invalid_grant {
            self.clear_tokens();
            self.store.delete_tokens(self.alias()).map_err(keyring)?;
            self.store
                .delete_client_info(self.alias())
                .map_err(keyring)?;
            return Err(FlowError::InvalidGrant(reason));
        }
        Err(FlowError::TransientRefresh(reason))
    }

    fn store_token(&mut self, token: &OAuthToken) -> Result<(), FlowError> {
        #[allow(clippy::cast_precision_loss)]
        let expiry = token
            .expires_in
            .map(|seconds| now_seconds() + seconds as f64);
        self.store
            .set_tokens(self.alias(), token)
            .map_err(keyring)?;
        self.tokens = Some(token.clone());
        self.token_expiry = expiry;
        Ok(())
    }

    /// `_perform_authorization`: the redirect, the callback and the token
    /// request it leads to.
    async fn perform_authorization(&mut self) -> Result<Outgoing, FlowError> {
        let endpoint = self.oauth_metadata.as_ref().map_or_else(
            || format!("{}/authorize", self.authorization_base_url()),
            |metadata| metadata.authorization_endpoint.clone(),
        );
        let client_id = self
            .client_info
            .as_ref()
            .ok_or_else(|| FlowError::Flow("no client information is available".to_owned()))?
            .client_id()
            .unwrap_or("None")
            .to_owned();
        let verifier = code_verifier()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = token_urlsafe(32)?;
        let mut parameters = vec![
            ("response_type", "code".to_owned()),
            ("client_id", client_id.clone()),
            ("redirect_uri", self.config.redirect_uri.clone()),
            ("state", state.clone()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256".to_owned()),
        ];
        if self.include_resource() {
            parameters.push(("resource", self.resource_url()));
        }
        if let Some(scope) = self.scope.clone().filter(|scope| !scope.is_empty()) {
            parameters.push(("scope", scope));
        }
        let url = format!("{endpoint}?{}", urlencode(&parameters));
        let (code, returned_state) = match &self.interaction {
            Interaction::Refuse => {
                return Err(FlowError::Flow(
                    "signing in requires an interactive login".to_owned(),
                ));
            }
            Interaction::Login { on_url } => {
                on_url(url).await;
                super::callback::serve_once(self.config.redirect_port).await?
            }
        };
        if returned_state.as_deref() != Some(state.as_str()) {
            return Err(FlowError::Flow(
                "the callback returned a state this login did not issue".to_owned(),
            ));
        }
        if code.is_empty() {
            return Err(FlowError::Flow(
                "the callback carried no authorization code".to_owned(),
            ));
        }
        Ok(self.token_request(
            vec![
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("code".to_owned(), code),
                ("redirect_uri".to_owned(), self.config.redirect_uri.clone()),
                ("client_id".to_owned(), client_id),
                ("code_verifier".to_owned(), verifier),
            ],
            "token exchange",
        ))
    }

    /// `_handle_token_response`.
    async fn handle_token_response(
        &mut self,
        response: reqwest::Response,
    ) -> Result<(), FlowError> {
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|error| FlowError::Http(error.to_string()))?;
        if status != 200 {
            return Err(FlowError::Flow(format!(
                "the token endpoint answered HTTP {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let token = serde_json::from_slice::<Value>(&body)
            .map_err(|error| error.to_string())
            .and_then(|value| OAuthToken::validate(&value))
            .map_err(|reason| {
                FlowError::Flow(format!(
                    "the token endpoint answered an unusable token: {reason}"
                ))
            })?;
        self.store_token(&token)
    }

    async fn send(&self, request: &Outgoing) -> Result<reqwest::Response, FlowError> {
        let mut builder = self.http.request(request.method.clone(), &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        builder = match &request.body {
            Body::Empty => builder,
            Body::Json(value) => {
                if request.header("content-type").is_none() {
                    builder = builder.header("Content-Type", "application/json");
                }
                builder.body(value.to_string())
            }
            Body::Form(pairs) => builder.body(form_encode(pairs)),
        };
        builder
            .send()
            .await
            .map_err(|error| FlowError::Http(format!("{} failed: {error}", request.purpose)))
    }
}

/// `response.headers.get("WWW-Authenticate")`: every value, comma-joined.
fn www_authenticate(response: &reqwest::Response) -> Option<String> {
    let values = response
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(", "))
}

/// `extract_field_from_www_auth`.
pub(crate) fn extract_field(header: Option<&str>, field: &str) -> Option<String> {
    let header = header.filter(|header| !header.is_empty())?;
    let pattern = regex::Regex::new(&format!(
        r#"{}=(?:"([^"]+)"|([^\s,]+))"#,
        regex::escape(field)
    ))
    .ok()?;
    let captures = pattern.captures(header)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str().to_owned())
}

/// `build_protected_resource_metadata_discovery_urls`.
pub(crate) fn protected_resource_metadata_urls(
    advertised: Option<&str>,
    server_url: &str,
) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(advertised) = advertised.filter(|url| !url.is_empty()) {
        urls.push(advertised.to_owned());
    }
    let parsed = PyUrl::parse(server_url);
    let base = format!("{}://{}", parsed.scheme, parsed.netloc);
    if !parsed.path.is_empty() && parsed.path != "/" {
        urls.push(format!(
            "{base}/.well-known/oauth-protected-resource{}",
            parsed.path
        ));
    }
    urls.push(format!("{base}/.well-known/oauth-protected-resource"));
    urls
}

/// `build_oauth_authorization_server_metadata_discovery_urls`.
pub(crate) fn authorization_server_metadata_urls(
    auth_server_url: Option<&str>,
    server_url: &str,
) -> Vec<String> {
    let Some(auth_server_url) = auth_server_url.filter(|url| !url.is_empty()) else {
        let parsed = PyUrl::parse(server_url);
        return vec![format!(
            "{}://{}/.well-known/oauth-authorization-server",
            parsed.scheme, parsed.netloc
        )];
    };
    let parsed = PyUrl::parse(auth_server_url);
    let base = format!("{}://{}", parsed.scheme, parsed.netloc);
    if !parsed.path.is_empty() && parsed.path != "/" {
        let path = parsed.path.trim_end_matches('/');
        return vec![
            format!("{base}/.well-known/oauth-authorization-server{path}"),
            format!("{base}/.well-known/openid-configuration{path}"),
            format!("{base}{path}/.well-known/openid-configuration"),
        ];
    }
    vec![
        format!("{base}/.well-known/oauth-authorization-server"),
        format!("{base}/.well-known/openid-configuration"),
    ]
}

/// `get_client_metadata_scopes`.
fn client_metadata_scopes(
    challenge: Option<String>,
    protected: Option<&ProtectedResource>,
    server: Option<&AuthorizationServer>,
) -> Option<String> {
    if challenge.is_some() {
        return challenge;
    }
    if let Some(scopes) = protected.and_then(|metadata| metadata.scopes_supported.as_ref()) {
        return Some(scopes.join(" "));
    }
    server
        .and_then(|metadata| metadata.scopes_supported.as_ref())
        .map(|scopes| scopes.join(" "))
}

/// `handle_protected_resource_response`.
async fn protected_resource_response(response: reqwest::Response) -> Option<ProtectedResource> {
    if response.status().as_u16() != 200 {
        return None;
    }
    let body = response.bytes().await.ok()?;
    let value = serde_json::from_slice::<Value>(&body).ok()?;
    let fields = value.as_object()?;
    let resource = any_http_url(fields.get("resource")?.as_str()?)?;
    let authorization_servers = fields
        .get("authorization_servers")?
        .as_array()?
        .iter()
        .map(|server| server.as_str().and_then(any_http_url))
        .collect::<Option<Vec<_>>>()?;
    if authorization_servers.is_empty() {
        return None;
    }
    for name in [
        "jwks_uri",
        "resource_documentation",
        "resource_policy_uri",
        "resource_tos_uri",
    ] {
        optional_http_url(fields.get(name))?;
    }
    for name in [
        "bearer_methods_supported",
        "resource_signing_alg_values_supported",
        "authorization_details_types_supported",
        "dpop_signing_alg_values_supported",
    ] {
        optional_string_list(fields.get(name))?;
    }
    for name in [
        "tls_client_certificate_bound_access_tokens",
        "dpop_bound_access_tokens_required",
    ] {
        optional_bool(fields.get(name))?;
    }
    optional_text(fields.get("resource_name"))?;
    Some(ProtectedResource {
        resource,
        authorization_servers,
        scopes_supported: optional_string_list(fields.get("scopes_supported"))?,
    })
}

/// `handle_auth_metadata_response`: whether to keep looking, and what was
/// found.
async fn authorization_server_response(
    response: reqwest::Response,
) -> (bool, Option<AuthorizationServer>) {
    let status = response.status().as_u16();
    if status == 200 {
        let metadata = response
            .bytes()
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
            .and_then(|value| authorization_server_metadata(&value));
        return (true, metadata);
    }
    if !(400..500).contains(&status) {
        return (false, None);
    }
    (true, None)
}

/// `OAuthMetadata.model_validate`.
fn authorization_server_metadata(value: &Value) -> Option<AuthorizationServer> {
    let fields = value.as_object()?;
    let required = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .and_then(any_http_url)
    };
    required("issuer")?;
    let authorization_endpoint = required("authorization_endpoint")?;
    let token_endpoint = required("token_endpoint")?;
    for name in [
        "service_documentation",
        "op_policy_uri",
        "op_tos_uri",
        "revocation_endpoint",
        "introspection_endpoint",
    ] {
        optional_http_url(fields.get(name))?;
    }
    for name in [
        "response_modes_supported",
        "grant_types_supported",
        "token_endpoint_auth_methods_supported",
        "token_endpoint_auth_signing_alg_values_supported",
        "ui_locales_supported",
        "revocation_endpoint_auth_methods_supported",
        "revocation_endpoint_auth_signing_alg_values_supported",
        "introspection_endpoint_auth_methods_supported",
        "introspection_endpoint_auth_signing_alg_values_supported",
        "code_challenge_methods_supported",
    ] {
        optional_string_list(fields.get(name))?;
    }
    if let Some(types) = fields.get("response_types_supported") {
        optional_string_list(Some(types))?.filter(|_| !types.is_null())?;
    }
    Some(AuthorizationServer {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: optional_http_url(fields.get("registration_endpoint"))?,
        scopes_supported: optional_string_list(fields.get("scopes_supported"))?,
        client_id_metadata_document_supported: optional_bool(
            fields.get("client_id_metadata_document_supported"),
        )?,
    })
}

/// An optional `AnyHttpUrl` field: the outer `None` is a validation failure.
fn optional_http_url(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => any_http_url(text).map(Some),
        Some(_) => None,
    }
}

fn optional_string_list(value: Option<&Value>) -> Option<Option<Vec<String>>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some),
        Some(_) => None,
    }
}

fn optional_text(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => None,
    }
}

/// Pydantic's lax `bool | None`.
fn optional_bool(value: Option<&Value>) -> Option<Option<bool>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::Bool(flag)) => Some(Some(*flag)),
        Some(Value::Number(number)) => match number.as_f64() {
            Some(0.0) => Some(Some(false)),
            Some(1.0) => Some(Some(true)),
            _ => None,
        },
        Some(Value::String(text)) => match text.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Some(Some(false)),
            "1" | "on" | "t" | "true" | "y" | "yes" => Some(Some(true)),
            _ => None,
        },
        Some(_) => None,
    }
}

/// `handle_registration_response`.
async fn registration_response(
    response: reqwest::Response,
) -> Result<ClientInformation, FlowError> {
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| FlowError::Http(error.to_string()))?;
    if status != 200 && status != 201 {
        return Err(FlowError::Flow(format!(
            "client registration answered HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    serde_json::from_slice::<Value>(&body)
        .map_err(|error| error.to_string())
        .and_then(|value| ClientInformation::validate(&value))
        .map_err(|reason| {
            FlowError::Flow(format!(
                "client registration answered an unusable client: {reason}"
            ))
        })
}

/// Reference `_classify_refresh_error`: the reason, and whether the refresh
/// token itself was refused.
async fn classify_refresh_error(status: u16, response: reqwest::Response) -> (String, bool) {
    let fallback = format!("HTTP {status}");
    let Some(payload) = response
        .bytes()
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
    else {
        return (fallback, false);
    };
    let Some(fields) = payload.as_object() else {
        return (fallback, false);
    };
    let error = fields
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = fields
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let reason = [error, description]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    (
        if reason.is_empty() { fallback } else { reason },
        error == OAUTH_INVALID_GRANT,
    )
}

/// `is_valid_client_metadata_url`.
pub(crate) fn is_valid_client_metadata_url(url: &str) -> bool {
    let parsed = PyUrl::parse(url);
    parsed.scheme == "https" && !parsed.path.is_empty() && parsed.path != "/"
}

/// `resource_url_from_server_url`.
pub(crate) fn resource_url_from_server_url(url: &str) -> String {
    let parsed = PyUrl::split(url);
    PyUrl {
        netloc: parsed.netloc.to_lowercase(),
        fragment: String::new(),
        ..parsed
    }
    .unsplit()
}

/// `check_resource_allowed`.
pub(crate) fn check_resource_allowed(requested: &str, configured: &str) -> bool {
    let requested = PyUrl::parse(requested);
    let configured = PyUrl::parse(configured);
    if requested.scheme.to_lowercase() != configured.scheme.to_lowercase()
        || requested.netloc.to_lowercase() != configured.netloc.to_lowercase()
    {
        return false;
    }
    let with_slash = |path: &str| {
        if path.ends_with('/') {
            path.to_owned()
        } else {
            format!("{path}/")
        }
    };
    with_slash(&requested.path).starts_with(&with_slash(&configured.path))
}

/// The parts `urllib.parse` splits a URL into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PyUrl {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

impl PyUrl {
    /// `urlsplit`: the scheme lowercased, nothing else normalized.
    pub(crate) fn split(url: &str) -> Self {
        let mut rest = url;
        let mut parts = Self::default();
        if let Some(colon) = rest.find(':') {
            let candidate = &rest[..colon];
            if candidate
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic())
                && candidate
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
            {
                parts.scheme = candidate.to_ascii_lowercase();
                rest = &rest[colon + 1..];
            }
        }
        if let Some(after) = rest.strip_prefix("//") {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            parts.netloc = after[..end].to_owned();
            rest = &after[end..];
        }
        if let Some(hash) = rest.find('#') {
            parts.fragment = rest[hash + 1..].to_owned();
            rest = &rest[..hash];
        }
        if let Some(question) = rest.find('?') {
            parts.query = rest[question + 1..].to_owned();
            rest = &rest[..question];
        }
        parts.path = rest.to_owned();
        parts
    }

    /// `urlparse`: as `urlsplit`, with the `;params` of the last segment
    /// taken off the path.
    pub(crate) fn parse(url: &str) -> Self {
        let mut parts = Self::split(url);
        let last = parts.path.rfind('/').unwrap_or(0);
        if let Some(semicolon) = parts.path[last..].find(';') {
            parts.path.truncate(last + semicolon);
        }
        parts
    }

    /// `urlunsplit`.
    pub(crate) fn unsplit(&self) -> String {
        let mut url = self.path.clone();
        if !self.netloc.is_empty() {
            if !url.is_empty() && !url.starts_with('/') {
                url.insert(0, '/');
            }
            url = format!("//{}{url}", self.netloc);
        } else if url.starts_with("//")
            || (matches!(
                self.scheme.as_str(),
                "http" | "https" | "ftp" | "file" | "ws" | "wss"
            ) && (url.is_empty() || url.starts_with('/')))
        {
            url = format!("//{url}");
        }
        if !self.scheme.is_empty() {
            url = format!("{}:{url}", self.scheme);
        }
        if !self.query.is_empty() {
            let _ = write!(url, "?{}", self.query);
        }
        if !self.fragment.is_empty() {
            let _ = write!(url, "#{}", self.fragment);
        }
        url
    }
}

/// `urllib.parse.quote(text, safe="")`.
fn quote(text: &str) -> String {
    percent_encode(text, false)
}

/// `urllib.parse.quote_plus`.
pub(crate) fn quote_plus(text: &str) -> String {
    percent_encode(text, true)
}

fn percent_encode(text: &str, plus: bool) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                encoded.push(char::from(byte));
            }
            b' ' if plus => encoded.push('+'),
            other => {
                let _ = write!(encoded, "%{other:02X}");
            }
        }
    }
    encoded
}

/// httpx's form body, `urlencode(data, doseq=True)`.
fn form_encode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", quote_plus(name), quote_plus(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `urllib.parse.urlencode`.
fn urlencode(parameters: &[(&str, String)]) -> String {
    parameters
        .iter()
        .map(|(name, value)| format!("{}={}", quote_plus(name), quote_plus(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn random_bytes(count: usize) -> Result<Vec<u8>, FlowError> {
    let mut bytes = vec![0_u8; count];
    getrandom::fill(&mut bytes)
        .map_err(|_| FlowError::Io("secure randomness is unavailable".to_owned()))?;
    Ok(bytes)
}

/// `PKCEParameters.generate`'s verifier: 128 characters drawn uniformly from
/// the unreserved set.
fn code_verifier() -> Result<String, FlowError> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut verifier = String::with_capacity(128);
    while verifier.len() < 128 {
        for byte in random_bytes(256)? {
            // 256 = 3 * 66 + 58: rejecting the top 58 keeps the draw uniform.
            if usize::from(byte) < ALPHABET.len() * 3 && verifier.len() < 128 {
                verifier.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
            }
        }
    }
    Ok(verifier)
}

/// `secrets.token_urlsafe(count)`.
fn token_urlsafe(count: usize) -> Result<String, FlowError> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(count)?))
}
