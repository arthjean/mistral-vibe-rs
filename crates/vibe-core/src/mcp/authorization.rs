//! Who may connect to an MCP server, and with which headers.
//!
//! Reference `MCPAuthenticationService` (`vibe/app_server/_mcp_auth.py`) owns
//! two counters per server: a descriptor revision, which names the tool
//! descriptors a credential may publish and keys the descriptor cache, and a
//! connection revision, which names the headers a connection was opened with.
//! A server's fingerprint hashes the part of its entry a credential is bound
//! to, so an edit that changes it advances both. `resolve` hands a discovery or
//! a call its headers, or says why there are none; `reject` records that a
//! server refused the headers it was given and advances both counters, so a
//! descriptor published under the refused credential stops resolving.
//!
//! The OAuth credentials themselves live in the keyring behind
//! [`McpOAuthStore`], and the login that mints them is
//! `crate::auth::mcp_oauth`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{McpAuthConfig, McpServerConfig, McpTransportConfig};
use crate::auth::mcp_oauth::{
    AuthUrlSink, CredentialBackup, McpOAuthError, McpOAuthStore, RefreshOutcome, perform_login,
    refresh_credential,
};

/// What kind of credential a server connects with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthorizationKind {
    None,
    Static,
    Oauth,
}

/// Reference `MCPAuthorizationRef`: a server as the catalog resolved it,
/// carrying the descriptor revision current at that moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthorizationRef {
    pub server_name: String,
    pub server_fingerprint: String,
    pub kind: McpAuthorizationKind,
    pub descriptor_revision: String,
}

/// Reference `MCPAuthorizationSnapshot`: the headers one connection may use.
#[derive(Clone, PartialEq, Eq)]
pub struct McpAuthorizationSnapshot {
    pub headers: BTreeMap<String, String>,
    pub connection_revision: String,
    pub descriptor_revision: String,
}

impl std::fmt::Debug for McpAuthorizationSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpAuthorizationSnapshot")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("connection_revision", &self.connection_revision)
            .field("descriptor_revision", &self.descriptor_revision)
            .finish()
    }
}

/// Why a server cannot be connected to until someone signs in again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthorizationReason {
    Missing,
    Expired,
    Rejected,
    Invalid,
}

impl McpAuthorizationReason {
    /// The value reference `MCPAuthorizationRequired.reason` carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
            Self::Invalid => "invalid",
        }
    }
}

/// Reference `MCPAuthorizationRequired`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAuthorizationRequired {
    pub reason: McpAuthorizationReason,
    pub descriptor_revision: String,
    pub observed_connection_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAuthorization {
    Snapshot(McpAuthorizationSnapshot),
    Required(McpAuthorizationRequired),
}

/// Who bound a catalog: a session by its id, or `None` for the anonymous
/// catalog every sessionless caller shares.
pub type McpCatalogOwner = Option<String>;

#[derive(Default)]
struct ServiceState {
    /// Every owner's configured servers, oldest binding first: reference
    /// `_config_catalogs`, where a binding is reinserted at the end so the
    /// merged view names whichever server bound a contested name last.
    catalogs: Vec<(McpCatalogOwner, BTreeMap<String, McpServerConfig>)>,
    fingerprints: BTreeMap<String, String>,
    descriptor_generations: BTreeMap<String, u64>,
    connection_generations: BTreeMap<String, u64>,
    authorization_material: BTreeMap<String, String>,
    rejected_material: BTreeMap<String, String>,
}

impl ServiceState {
    /// Reference `_config_view`: every owner's servers merged, a later
    /// binding winning a contested name.
    fn merged(&self, excluding: Option<&McpCatalogOwner>) -> BTreeMap<String, McpServerConfig> {
        let mut merged = BTreeMap::new();
        for (owner, servers) in &self.catalogs {
            if Some(owner) == excluding {
                continue;
            }
            merged.extend(
                servers
                    .iter()
                    .map(|(name, server)| (name.clone(), server.clone())),
            );
        }
        merged
    }

    /// The server `name` resolves to, whoever bound it.
    fn owned_server(&self, name: &str) -> Option<McpServerConfig> {
        self.catalogs
            .iter()
            .rev()
            .find_map(|(_, servers)| servers.get(name).cloned())
    }

    /// Reference `_bound`: the server a name means to the session asking, or
    /// across every owner for a sessionless caller or one that never bound.
    fn bound(&self, name: &str, owner: &McpCatalogOwner) -> Option<McpServerConfig> {
        if owner.is_some()
            && let Some((_, servers)) = self.catalogs.iter().find(|(key, _)| key == owner)
        {
            return servers.get(name).cloned();
        }
        self.owned_server(name)
    }

    /// Reference `_alias_is_shared`: more than one owner answers to `name`,
    /// so the one credential filed under it is theirs together.
    fn alias_is_shared(&self, name: &str) -> bool {
        self.catalogs
            .iter()
            .filter(|(_, servers)| servers.contains_key(name))
            .count()
            > 1
    }

    /// Reference `_install_fingerprint`.
    fn install_fingerprint(&mut self, name: &str, server: &McpServerConfig) {
        let fingerprint = server_fingerprint(server);
        let previous = self
            .fingerprints
            .insert(name.to_owned(), fingerprint.clone());
        if previous.is_some_and(|previous| previous != fingerprint) {
            self.credential_changed(name);
        }
    }

    fn descriptor_revision(&self, name: &str) -> String {
        let fingerprint = self
            .fingerprints
            .get(name)
            .map_or("missing", String::as_str);
        let prefix = fingerprint.get(..16).unwrap_or(fingerprint);
        let generation = self.descriptor_generations.get(name).copied().unwrap_or(0);
        format!("mcp-auth-descriptor:{prefix}:{generation}")
    }

    fn connection_revision(&self, name: &str) -> String {
        let generation = self.connection_generations.get(name).copied().unwrap_or(0);
        format!("mcp-auth-connection:{name}:{generation}")
    }

    fn advance(&mut self, name: &str) {
        *self
            .descriptor_generations
            .entry(name.to_owned())
            .or_default() += 1;
        self.advance_connection(name);
    }

    fn advance_connection(&mut self, name: &str) {
        *self
            .connection_generations
            .entry(name.to_owned())
            .or_default() += 1;
    }

    /// Reference `_delete_credentials_locked` and `login`, after the store
    /// changed: both revisions move on and no headers are remembered.
    fn credential_changed(&mut self, name: &str) {
        self.advance(name);
        self.authorization_material.remove(name);
        self.rejected_material.remove(name);
    }

    fn required(
        &self,
        name: &str,
        reason: McpAuthorizationReason,
        observed: Option<String>,
    ) -> McpAuthorization {
        McpAuthorization::Required(McpAuthorizationRequired {
            reason,
            descriptor_revision: self.descriptor_revision(name),
            observed_connection_revision: observed,
        })
    }

    fn snapshot(&self, name: &str, headers: BTreeMap<String, String>) -> McpAuthorization {
        McpAuthorization::Snapshot(McpAuthorizationSnapshot {
            headers,
            connection_revision: self.connection_revision(name),
            descriptor_revision: self.descriptor_revision(name),
        })
    }

    /// Reference `_accept_material`: new headers name a new connection.
    fn accept_material(&mut self, name: &str, material: String) {
        if self.authorization_material.get(name) == Some(&material) {
            return;
        }
        self.authorization_material
            .insert(name.to_owned(), material);
        self.rejected_material.remove(name);
        self.advance_connection(name);
    }

    /// Reference `_resolve_static` and the tail of `_resolve_oauth`.
    fn offer(&mut self, name: &str, headers: BTreeMap<String, String>) -> McpAuthorization {
        let material = authorization_material(&headers);
        if self.rejected_material.get(name) == Some(&material) {
            return self.required(name, McpAuthorizationReason::Rejected, None);
        }
        self.accept_material(name, material);
        self.snapshot(name, headers)
    }
}

/// Reads one environment variable.
pub type McpEnvironment = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Why a sign-in, a sign-out or a removal could not go ahead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpAuthenticationError {
    /// Reference `ValueError`: the name is not an OAuth server of the catalog.
    #[error("MCP server `{0}` does not sign in with OAuth")]
    NotOauth(String),
    #[error(transparent)]
    OAuth(#[from] McpOAuthError),
}

/// Why [`McpAuthenticationService::remove_with_credentials`] left the entry.
#[derive(Debug, thiserror::Error)]
pub enum McpServerRemoveError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// Reference `MCPServerRemoveError` over a credential failure.
    #[error("{0}")]
    Credentials(McpAuthenticationError),
}

/// What a removal deleted, held until the configuration entry is gone.
#[derive(Debug)]
pub struct McpCredentialRemoval {
    name: String,
    backup: CredentialBackup,
    previous: AuthorizationState,
    pub descriptor_revision: String,
}

/// Reference `_AuthorizationState`.
#[derive(Debug, Clone)]
struct AuthorizationState {
    descriptor_generation: Option<u64>,
    connection_generation: Option<u64>,
    authorization_material: Option<String>,
    rejected_material: Option<String>,
}

/// Reference `MCPAuthenticationService`, one per process for every session's
/// catalog.
///
/// Every operation on one server name runs under that name's lock, as the
/// reference's do, so a login that waits on a browser holds back only the
/// server it signs in to.
pub struct McpAuthenticationService {
    state: StdMutex<ServiceState>,
    locks: StdMutex<BTreeMap<String, Arc<Mutex<()>>>>,
    oauth: Option<Arc<McpOAuthStore>>,
    /// Where a static block's `api_key_env` is read, the process environment
    /// unless a host supplies another.
    environment: McpEnvironment,
}

impl McpAuthenticationService {
    /// A service keeping OAuth credentials in `oauth`; without one every
    /// OAuth server reads as never signed in.
    #[must_use]
    pub fn new(oauth: Option<Arc<McpOAuthStore>>) -> Self {
        Self {
            state: StdMutex::new(ServiceState::default()),
            locks: StdMutex::new(BTreeMap::new()),
            oauth,
            environment: Arc::new(|variable| std::env::var(variable).ok()),
        }
    }

    /// The same service reading `api_key_env` through `environment`.
    #[must_use]
    pub fn with_environment(mut self, environment: McpEnvironment) -> Self {
        self.environment = environment;
        self
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ServiceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_for(&self, name: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    /// Reference `bind_catalog`: installs the servers `owner` configured,
    /// advancing both revisions of any whose fingerprint changed, and forgets
    /// the ones it no longer configures unless another owner still does.
    pub async fn bind_catalog(&self, servers: &[McpServerConfig], owner: McpCatalogOwner) {
        let mut state = self.state();
        let position = state.catalogs.iter().position(|(key, _)| *key == owner);
        let mut owned = position
            .map(|position| state.catalogs.remove(position).1)
            .unwrap_or_default();
        let others = state.merged(Some(&owner));
        let active = servers
            .iter()
            .map(|server| (server.alias.clone(), server.clone()))
            .collect::<BTreeMap<_, _>>();
        for (name, server) in &active {
            owned.insert(name.clone(), server.clone());
            state.install_fingerprint(name, server);
        }
        let removed = owned
            .keys()
            .filter(|name| !active.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        for name in removed {
            owned.remove(&name);
            // Handed back to a server another owner still binds under the
            // name, whose references would otherwise resolve as `invalid`.
            if let Some(survivor) = others.get(&name) {
                state.install_fingerprint(&name, survivor);
                continue;
            }
            state.fingerprints.remove(&name);
            state.authorization_material.remove(&name);
            state.rejected_material.remove(&name);
        }
        state.catalogs.push((owner, owned));
    }

    /// Forgets what `owner` bound, as the reference's weak keys do when the
    /// session holding one ends.
    pub async fn release(&self, owner: &McpCatalogOwner) {
        self.state().catalogs.retain(|(key, _)| key != owner);
    }

    /// The server `name` means to `owner`.
    pub async fn bound(&self, name: &str, owner: &McpCatalogOwner) -> Option<McpServerConfig> {
        self.state().bound(name, owner)
    }

    /// Reference `reference_for`.
    pub async fn reference_for(&self, server: &McpServerConfig) -> McpAuthorizationRef {
        let state = self.state();
        McpAuthorizationRef {
            server_name: server.alias.clone(),
            server_fingerprint: server_fingerprint(server),
            kind: authorization_kind(server),
            descriptor_revision: state.descriptor_revision(&server.alias),
        }
    }

    pub async fn descriptor_revision(&self, name: &str) -> String {
        self.state().descriptor_revision(name)
    }

    pub async fn connection_revision(&self, name: &str) -> String {
        self.state().connection_revision(name)
    }

    /// Reference `resolve`.
    pub async fn resolve(&self, reference: &McpAuthorizationRef) -> McpAuthorization {
        let lock = self.lock_for(&reference.server_name);
        let _guard = lock.lock().await;
        self.resolve_locked(reference).await
    }

    /// Reference `reject`: the server refused the headers of the connection
    /// `observed` names.
    pub async fn reject(
        &self,
        reference: &McpAuthorizationRef,
        observed: &str,
    ) -> McpAuthorization {
        let lock = self.lock_for(&reference.server_name);
        let _guard = lock.lock().await;
        let current = self.resolve_locked(reference).await;
        if let McpAuthorization::Snapshot(snapshot) = &current
            && snapshot.connection_revision != observed
        {
            return current;
        }
        let name = reference.server_name.as_str();
        let discard = {
            let mut state = self.state();
            let material = state
                .authorization_material
                .get(name)
                .cloned()
                .unwrap_or_default();
            state.rejected_material.insert(name.to_owned(), material);
            // Kept where the alias is shared: the stored credential may be the
            // one another owner's server of that name is connected with.
            !state.alias_is_shared(name)
                && state.owned_server(name).is_some_and(|server| {
                    authorization_kind(&server) == McpAuthorizationKind::Oauth
                })
        };
        if discard && let Some(store) = &self.oauth {
            let _ = store.delete_tokens(name);
        }
        let mut state = self.state();
        state.advance(name);
        state.required(
            name,
            McpAuthorizationReason::Rejected,
            Some(observed.to_owned()),
        )
    }

    /// Reference `login`: signs in to `name` interactively, publishing the
    /// authorization URL through `on_url`, and answers the descriptor
    /// revision the new credential publishes under.
    pub async fn login(
        &self,
        name: &str,
        on_url: AuthUrlSink,
        owner: &McpCatalogOwner,
    ) -> Result<String, McpAuthenticationError> {
        let lock = self.lock_for(name);
        let _guard = lock.lock().await;
        let server = self.require_oauth_server(name, owner)?;
        let store = self.store(name)?;
        // An OAuth entry declares no headers of its own, reference
        // `_declared_headers` reading `http_headers()` off it.
        let declared = http_headers_with(&server, self.environment.as_ref());
        perform_login(&store, &server, &declared, on_url).await?;
        let mut state = self.state();
        state.credential_changed(name);
        Ok(state.descriptor_revision(name))
    }

    /// Reference `logout`.
    pub async fn logout(
        &self,
        name: &str,
        owner: &McpCatalogOwner,
    ) -> Result<String, McpAuthenticationError> {
        let lock = self.lock_for(name);
        let _guard = lock.lock().await;
        self.require_oauth_server(name, owner)?;
        self.store(name)?.delete_credentials(name)?;
        let mut state = self.state();
        state.credential_changed(name);
        Ok(state.descriptor_revision(name))
    }

    /// Reference `credential_removal`, entered: the credential is deleted
    /// and kept aside until [`Self::finish_removal`] says whether the entry
    /// went too.
    pub async fn begin_removal(
        &self,
        name: &str,
        owner: &McpCatalogOwner,
    ) -> Result<McpCredentialRemoval, McpAuthenticationError> {
        self.require_oauth_server(name, owner)?;
        let store = self.store(name)?;
        let backup = store.snapshot(name);
        let previous = {
            let state = self.state();
            AuthorizationState {
                descriptor_generation: state.descriptor_generations.get(name).copied(),
                connection_generation: state.connection_generations.get(name).copied(),
                authorization_material: state.authorization_material.get(name).cloned(),
                rejected_material: state.rejected_material.get(name).cloned(),
            }
        };
        store.delete_credentials(name)?;
        let mut state = self.state();
        state.credential_changed(name);
        Ok(McpCredentialRemoval {
            name: name.to_owned(),
            backup,
            previous,
            descriptor_revision: state.descriptor_revision(name),
        })
    }

    /// Reference `credential_removal`, left: a removal whose entry could not
    /// be deleted puts the credential and the revisions back.
    pub async fn finish_removal(
        &self,
        removal: McpCredentialRemoval,
        removed: bool,
    ) -> Result<(), McpAuthenticationError> {
        if removed {
            return Ok(());
        }
        let name = removal.name.as_str();
        let restored = self.store(name).and_then(|store| {
            store
                .restore(name, &removal.backup)
                .map_err(McpAuthenticationError::from)
        });
        let mut state = self.state();
        let previous = removal.previous;
        restore_entry(
            &mut state.descriptor_generations,
            name,
            previous.descriptor_generation,
        );
        restore_entry(
            &mut state.connection_generations,
            name,
            previous.connection_generation,
        );
        restore_entry(
            &mut state.authorization_material,
            name,
            previous.authorization_material,
        );
        restore_entry(
            &mut state.rejected_material,
            name,
            previous.rejected_material,
        );
        restored
    }

    /// Reference `_remove_server_with_credentials`: an OAuth server's
    /// credentials are deleted before its entry leaves `store`, and put back
    /// when the entry could not be.
    pub async fn remove_with_credentials(
        &self,
        store: &crate::config::LayeredConfig,
        name: &str,
        owner: &McpCatalogOwner,
    ) -> Result<crate::config::mcp::McpRemoval, McpServerRemoveError> {
        let configured = store
            .load()
            .and_then(|snapshot| snapshot.mcp_servers(store.working_directory()))?
            .into_iter()
            .find(|server| server.alias == name);
        if !configured
            .is_some_and(|server| authorization_kind(&server) == McpAuthorizationKind::Oauth)
        {
            return Ok(store.persist_mcp_remove(name)?);
        }
        let removal = self
            .begin_removal(name, owner)
            .await
            .map_err(McpServerRemoveError::Credentials)?;
        let removed = store.persist_mcp_remove(name);
        let restored = self.finish_removal(removal, removed.is_ok()).await;
        let removed = removed?;
        restored.map_err(McpServerRemoveError::Credentials)?;
        Ok(removed)
    }

    fn require_oauth_server(
        &self,
        name: &str,
        owner: &McpCatalogOwner,
    ) -> Result<McpServerConfig, McpAuthenticationError> {
        self.state()
            .bound(name, owner)
            .filter(|server| authorization_kind(server) == McpAuthorizationKind::Oauth)
            .ok_or_else(|| McpAuthenticationError::NotOauth(name.to_owned()))
    }

    fn store(&self, name: &str) -> Result<Arc<McpOAuthStore>, McpAuthenticationError> {
        self.oauth.clone().ok_or_else(|| {
            McpAuthenticationError::OAuth(McpOAuthError::Headless {
                alias: name.to_owned(),
            })
        })
    }

    async fn resolve_locked(&self, reference: &McpAuthorizationRef) -> McpAuthorization {
        let name = reference.server_name.as_str();
        let server = {
            let mut state = self.state();
            let Some(server) = state.owned_server(name) else {
                return state.required(name, McpAuthorizationReason::Invalid, None);
            };
            if reference.descriptor_revision != state.descriptor_revision(name)
                || state.fingerprints.get(name) != Some(&reference.server_fingerprint)
            {
                return state.required(name, McpAuthorizationReason::Invalid, None);
            }
            match authorization_kind(&server) {
                McpAuthorizationKind::None => return state.snapshot(name, BTreeMap::new()),
                McpAuthorizationKind::Static => {
                    let headers = http_headers_with(&server, self.environment.as_ref());
                    return state.offer(name, headers);
                }
                McpAuthorizationKind::Oauth => server,
            }
        };
        self.resolve_oauth(&server).await
    }

    /// Reference `_resolve_oauth`.
    async fn resolve_oauth(&self, server: &McpServerConfig) -> McpAuthorization {
        let name = server.alias.as_str();
        let required = |reason| self.state().required(name, reason, None);
        let Some(store) = &self.oauth else {
            return required(McpAuthorizationReason::Missing);
        };
        let current = oauth_fingerprint(server).map(|fingerprint| python_json(&fingerprint));
        let (Ok(saved), Ok(tokens)) = (store.fingerprint(name), store.tokens(name)) else {
            return required(McpAuthorizationReason::Missing);
        };
        let saved = saved.and_then(|saved| normalized_json(&saved));
        if saved != current {
            // What is stored was minted for another shape of this server, or,
            // where the alias is shared, is another owner's live grant, which
            // is left alone: the fingerprint keeps it from being handed over.
            let stale = tokens.is_some() || saved.is_some();
            if stale && !self.state().alias_is_shared(name) {
                let _ = store.delete_credentials(name);
                self.state().advance(name);
            }
            return required(McpAuthorizationReason::Invalid);
        }
        let Some(mut tokens) = tokens else {
            return required(McpAuthorizationReason::Missing);
        };
        if tokens
            .expiry()
            .is_some_and(|expiry| expiry <= crate::auth::mcp_oauth::now_seconds())
        {
            match refresh_credential(store, server).await {
                RefreshOutcome::Completed => {}
                RefreshOutcome::InvalidGrant => {
                    self.state().advance(name);
                    return required(McpAuthorizationReason::Expired);
                }
                RefreshOutcome::Failed | RefreshOutcome::Unexpected(_) => {
                    return required(McpAuthorizationReason::Expired);
                }
            }
            match store.tokens(name) {
                Ok(Some(refreshed)) => tokens = refreshed,
                _ => return required(McpAuthorizationReason::Expired),
            }
        }
        let mut headers = http_headers_with(server, self.environment.as_ref());
        headers.insert("Authorization".to_owned(), tokens.authorization_value());
        self.state().offer(name, headers)
    }
}

fn restore_entry<T>(values: &mut BTreeMap<String, T>, name: &str, previous: Option<T>) {
    match previous {
        Some(value) => {
            values.insert(name.to_owned(), value);
        }
        None => {
            values.remove(name);
        }
    }
}

#[must_use]
pub fn authorization_kind(server: &McpServerConfig) -> McpAuthorizationKind {
    match (&server.transport, &server.auth) {
        (McpTransportConfig::Stdio { .. }, _) => McpAuthorizationKind::None,
        (_, McpAuthConfig::Oauth(_)) => McpAuthorizationKind::Oauth,
        (_, McpAuthConfig::Static(_)) => McpAuthorizationKind::Static,
    }
}

/// Reference `MCPHttp.http_headers`: the declared headers plus the token
/// header a static block renders from the environment, and nothing for an
/// OAuth entry, whose bearer is added from the store.
#[must_use]
pub fn http_headers(server: &McpServerConfig) -> BTreeMap<String, String> {
    http_headers_with(server, &|variable| std::env::var(variable).ok())
}

fn http_headers_with(
    server: &McpServerConfig,
    environment: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let declared = match &server.transport {
        McpTransportConfig::Http { headers, .. }
        | McpTransportConfig::StreamableHttp { headers, .. } => headers,
        McpTransportConfig::Stdio { .. } => return BTreeMap::new(),
    };
    match &server.auth {
        McpAuthConfig::Static(statics) => {
            let mut headers = declared.clone();
            if let Some((name, value)) = statics.token_header(declared, environment) {
                headers.insert(name, value);
            }
            headers
        }
        McpAuthConfig::Oauth(_) => BTreeMap::new(),
    }
}

/// Reference `_authorization_material`: a digest of the headers themselves.
fn authorization_material(headers: &BTreeMap<String, String>) -> String {
    let encoded = python_json(&json!(headers));
    hex::encode(Sha256::digest(encoded.as_bytes()))
}

/// The seconds a timeout is configured in, as the reference's float field.
fn seconds(milliseconds: u64) -> Value {
    #[allow(clippy::cast_precision_loss)]
    let seconds = milliseconds as f64 / 1_000.0;
    json!(seconds)
}

/// Reference `Fingerprint.compute(server).model_dump(mode="json")`.
#[must_use]
pub fn oauth_fingerprint(server: &McpServerConfig) -> Option<Value> {
    let McpAuthConfig::Oauth(oauth) = &server.auth else {
        return None;
    };
    let mut scopes = oauth
        .scopes
        .iter()
        .map(|scope| scope.trim())
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    let marker = oauth.client_id.clone().unwrap_or_else(|| {
        oauth
            .client_metadata_url
            .as_ref()
            .map_or_else(|| "<dcr>".to_owned(), ToString::to_string)
    });
    Some(json!({
        "url": declared_url(server),
        "scopes_sorted": scopes,
        "client_marker": marker,
    }))
}

fn declared_url(server: &McpServerConfig) -> String {
    server
        .declared
        .as_ref()
        .and_then(|declared| declared.url.clone())
        .or_else(|| super::transport_url(&server.transport).map(ToString::to_string))
        .unwrap_or_default()
}

/// The canonical identity reference `_server_fingerprint` hashes.
#[must_use]
pub fn server_identity(server: &McpServerConfig) -> Value {
    let timeouts = |value: &mut Value| {
        value["startup_timeout_sec"] = seconds(server.startup_timeout_ms);
        value["tool_timeout_sec"] = seconds(server.tool_timeout_ms);
    };
    match &server.transport {
        McpTransportConfig::Stdio {
            command,
            arguments,
            environment,
            working_directory,
        } => {
            let declared = server.declared.clone().unwrap_or_default();
            let (command, args) = match declared.command {
                Some(super::McpDeclaredCommand::Text(text)) => (json!(text), declared.args),
                Some(super::McpDeclaredCommand::Argv(argv)) => (json!(argv), declared.args),
                None => {
                    let mut argv = vec![command.clone()];
                    argv.extend(arguments.iter().cloned());
                    (json!(argv), Vec::new())
                }
            };
            let cwd = declared.cwd.or_else(|| {
                working_directory
                    .as_ref()
                    .map(|directory| directory.display().to_string())
            });
            let mut value = json!({
                "name": server.alias,
                "transport": "stdio",
                "prompt": server.prompt,
                "sampling_enabled": server.sampling_enabled,
                "disabled": !server.enabled,
                "disabled_tools": server.disabled_tools,
                "command": command,
                "args": args,
                "cwd": cwd,
                "env_names": environment.keys().collect::<Vec<_>>(),
            });
            timeouts(&mut value);
            value
        }
        McpTransportConfig::Http { headers, .. }
        | McpTransportConfig::StreamableHttp { headers, .. } => {
            let auth = match &server.auth {
                McpAuthConfig::Oauth(_) => oauth_fingerprint(server).unwrap_or(Value::Null),
                McpAuthConfig::Static(statics) => json!({
                    "type": "static",
                    "header_names": headers.keys().collect::<Vec<_>>(),
                    "api_key_env": statics.api_key_env,
                    "api_key_header": statics.api_key_header,
                    "api_key_format": statics.api_key_format,
                }),
            };
            let mut value = json!({
                "name": server.alias,
                "transport": super::transport_name(&server.transport),
                "url": declared_url(server),
                "auth": auth,
                "prompt": server.prompt,
                "sampling_enabled": server.sampling_enabled,
            });
            timeouts(&mut value);
            value
        }
    }
}

/// Reference `_server_fingerprint`.
#[must_use]
pub fn server_fingerprint(server: &McpServerConfig) -> String {
    let encoded = python_json(&server_identity(server));
    hex::encode(Sha256::digest(encoded.as_bytes()))
}

/// Python's `json.dumps(value, sort_keys=True, separators=(",", ":"))`:
/// ASCII-only, keys sorted by code point, floats in their `repr`.
#[must_use]
pub fn python_json(value: &Value) -> String {
    let mut rendered = String::new();
    write_python_json(value, &mut rendered);
    rendered
}

fn write_python_json(value: &Value, rendered: &mut String) {
    match value {
        Value::Null => rendered.push_str("null"),
        Value::Bool(flag) => rendered.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            if number.is_f64() {
                let float = number.as_f64().unwrap_or_default();
                rendered.push_str(&match super::render::python_float(float).as_str() {
                    "nan" => "NaN".to_owned(),
                    "inf" => "Infinity".to_owned(),
                    "-inf" => "-Infinity".to_owned(),
                    other => other.to_owned(),
                });
            } else {
                rendered.push_str(&number.to_string());
            }
        }
        Value::String(text) => write_python_string(text, rendered),
        Value::Array(items) => {
            rendered.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    rendered.push(',');
                }
                write_python_json(item, rendered);
            }
            rendered.push(']');
        }
        Value::Object(fields) => {
            let mut keys = fields.keys().collect::<Vec<_>>();
            keys.sort_by(|left, right| left.chars().cmp(right.chars()));
            rendered.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    rendered.push(',');
                }
                write_python_string(key, rendered);
                rendered.push(':');
                if let Some(field) = fields.get(key) {
                    write_python_json(field, rendered);
                }
            }
            rendered.push('}');
        }
    }
}

fn write_python_string(text: &str, rendered: &mut String) {
    rendered.push('"');
    for character in text.chars() {
        match character {
            '"' => rendered.push_str("\\\""),
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            '\u{8}' => rendered.push_str("\\b"),
            '\u{c}' => rendered.push_str("\\f"),
            printable if (' '..='~').contains(&printable) => rendered.push(printable),
            other => {
                let mut units = [0_u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    let _ = write!(rendered, "\\u{unit:04x}");
                }
            }
        }
    }
    rendered.push('"');
}

/// A stored fingerprint re-spelled canonically, so one written by either
/// implementation compares by content rather than by spacing.
fn normalized_json(stored: &str) -> Option<String> {
    serde_json::from_str::<Value>(stored)
        .ok()
        .map(|value| python_json(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_json_matches_json_dumps_with_sorted_compact_ascii_output() {
        let value = json!({"b": 10.0, "a": ["é", null, true, 0.5], "c": "q\"\\\n\u{1f600}"});
        assert_eq!(
            python_json(&value),
            r#"{"a":["\u00e9",null,true,0.5],"b":10.0,"c":"q\"\\\n\ud83d\ude00"}"#
        );
    }
}
