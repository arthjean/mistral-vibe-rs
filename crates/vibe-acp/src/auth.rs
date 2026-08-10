//! The ACP authentication surface: method declaration, browser sign-in over
//! the editor protocol, status, and sign-out.
//!
//! Reference `vibe/acp/auth.py`. The controller mirrors `AcpAuthController`:
//! it advertises the browser methods under the provider predicate, drives the
//! delegated attempt lifecycle, persists what a sign-in returned, and owns the
//! only credential removal path the product has. Everything ambient, the
//! effective configuration, the credential stores and the sign-in transport,
//! sits behind [`AcpAuthEnvironment`] so the adapter tests script it the way
//! the reference injects its constructor ports.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use toml::Table;
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_core::auth::{
    AuthState, KeyringStore, PersistOutcome, RemoveError, SignInAttempt, SignInError,
    SignInErrorCode, SignInService, SystemSignInRuntime, configured_custom_domain,
    is_valid_custom_domain, resolve_active_provider, resolve_api_key_provider,
    resolve_browser_auth_urls,
};
use vibe_core::auth::{DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL};
use vibe_core::auth::{HttpSignInGateway, supports_browser_sign_in};

use crate::protocol::AcpError;

/// The reference's sign-in target vocabulary.
const SIGN_IN_TARGET_MISTRAL: &str = "mistral";
const SIGN_IN_TARGET_CUSTOM: &str = "custom";

/// The default home the production environment resolves when the process
/// environment names none, mirroring the CLI's `$VIBE_HOME` then `~/.vibe`.
#[must_use]
pub fn default_vibe_home() -> PathBuf {
    std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibe"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".vibe")
        })
}

pub type AuthKeyFuture<'a> = Pin<Box<dyn Future<Output = Result<String, SignInError>> + Send + 'a>>;
pub type AuthAttemptFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SignInAttempt, SignInError>> + Send + 'a>>;

/// The ambient world the authentication surface runs against. Reference
/// `AcpAuthController`'s constructor ports: the context loader, the sign-in
/// service factory, the key persister and remover, and the provider persister.
pub trait AcpAuthEnvironment: Send + Sync {
    /// The provider the flows authenticate, resolved from the effective
    /// configuration; the shipped Mistral entry when nothing resolves.
    fn load_provider(&self) -> Table;

    /// Reassesses credential provenance for `env_key`, rereading the global
    /// dotenv, which is what the reference's `load_dotenv_values` reload
    /// amounts to.
    ///
    /// # Errors
    ///
    /// Propagates the dotenv read failure when the file exists and cannot be
    /// read.
    fn assess(&self, env_key: &str) -> io::Result<AuthState>;

    /// Persists `api_key` with the reference's outcomes: environment, then
    /// keyring, then the global dotenv when the keyring refuses.
    fn persist_api_key(
        &self,
        env_key: &str,
        backend_is_mistral: bool,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome;

    /// Removes the credential for `env_key` from every source it owns.
    ///
    /// # Errors
    ///
    /// The deferred backend failure and the dotenv rewrite failure, as
    /// `vibe_core::auth::remove_api_key` reports them.
    fn remove_api_key(&self, env_key: &str) -> Result<(), RemoveError>;

    /// Upserts `provider` into the configuration; `false` when the write did
    /// not land.
    fn persist_provider(&self, provider: &Table) -> bool;

    /// The whole browser flow for `provider`: create, open, poll, exchange.
    fn browser_authenticate<'a>(&'a self, provider: &'a Table) -> AuthKeyFuture<'a>;

    /// Creates a sign-in process without opening a browser, which is the
    /// delegated start where the editor owns the browser.
    fn start_attempt<'a>(&'a self, provider: &'a Table) -> AuthAttemptFuture<'a>;

    /// Polls and exchanges a previously started attempt.
    fn complete_attempt<'a>(
        &'a self,
        provider: &'a Table,
        attempt: &'a SignInAttempt,
    ) -> AuthKeyFuture<'a>;
}

/// The production environment: the effective configuration through the
/// release-3 service, the OS keyring, the global dotenv, and the HTTP sign-in
/// gateway under the system runtime.
pub struct ProductionAuthEnvironment {
    vibe_home: PathBuf,
    working_directory: PathBuf,
    env_file: PathBuf,
    store: KeyringStore,
    /// The process environment as it stood at construction, which is the one
    /// fact a later assessment cannot re-derive: whether the variable was set
    /// before any dotenv value could have been injected.
    initial_environment: BTreeMap<String, String>,
    /// Keys persisted this run. The reference writes `os.environ`; this port
    /// cannot mutate the process environment without `unsafe`, so the overlay
    /// stands in for those writes everywhere the environment is consulted.
    overlay: Mutex<BTreeMap<String, String>>,
}

impl ProductionAuthEnvironment {
    #[must_use]
    pub fn new(vibe_home: PathBuf) -> Self {
        Self::with_store(vibe_home, KeyringStore::native())
    }

    /// The same environment over a caller-supplied keyring store, which is
    /// how the tests keep the OS keyring out of the loop.
    #[must_use]
    pub fn with_store(vibe_home: PathBuf, store: KeyringStore) -> Self {
        let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            env_file: vibe_core::config::global_env_file(&vibe_home),
            store,
            initial_environment: std::env::vars().collect(),
            overlay: Mutex::new(BTreeMap::new()),
            working_directory,
            vibe_home,
        }
    }

    fn release3(&self) -> Result<Release3Service, vibe_app_server::release3::Release3Error> {
        Release3Service::new(
            Release3Paths {
                vibe_home: self.vibe_home.clone(),
                working_directory: self.working_directory.clone(),
                session_root: self.vibe_home.join("sessions"),
            },
            false,
        )
    }

    fn environ(&self) -> BTreeMap<String, String> {
        let mut environ: BTreeMap<String, String> = std::env::vars().collect();
        if let Ok(overlay) = self.overlay.lock() {
            for (key, value) in overlay.iter() {
                environ.insert(key.clone(), value.clone());
            }
        }
        environ
    }

    fn overlay(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.overlay.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn gateway(
        provider: &Table,
    ) -> Result<HttpSignInGateway<vibe_core::auth::ReqwestSignInClient>, SignInError> {
        HttpSignInGateway::for_provider(provider).ok_or_else(|| {
            SignInError::with_message(
                SignInErrorCode::StartFailed,
                "the provider entry is missing its browser sign-in URLs".to_owned(),
            )
        })
    }
}

impl AcpAuthEnvironment for ProductionAuthEnvironment {
    fn load_provider(&self) -> Table {
        // Reference `OnboardingContext.load` falls back to the shipped
        // defaults rather than failing when the configuration cannot be read.
        // The raw effective snapshot is load-bearing here: the public view
        // redacts `api_key_env_var` as a sensitive key, and a redacted name
        // cannot address a credential.
        let document = self
            .release3()
            .ok()
            .and_then(|service| service.layered_config().load().ok())
            .and_then(|snapshot| serde_json::to_value(snapshot.effective).ok());
        let field = |name: &str| document.as_ref().and_then(|document| document.get(name));
        resolve_active_provider(
            field("active_model").and_then(Value::as_str),
            field("models"),
            field("providers"),
        )
    }

    fn assess(&self, env_key: &str) -> io::Result<AuthState> {
        vibe_core::auth::assess_auth_state(
            env_key,
            &self.env_file,
            &self.environ(),
            self.initial_environment
                .get(env_key)
                .is_some_and(|value| !value.is_empty()),
            &self.store,
        )
    }

    fn persist_api_key(
        &self,
        env_key: &str,
        backend_is_mistral: bool,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome {
        let mut overlay = self.overlay();
        vibe_core::auth::persist_api_key(
            env_key,
            backend_is_mistral,
            api_key,
            custom_domain,
            &mut overlay,
            &self.env_file,
            &self.store,
        )
        .outcome
    }

    fn remove_api_key(&self, env_key: &str) -> Result<(), RemoveError> {
        let mut overlay = self.overlay();
        vibe_core::auth::remove_api_key(env_key, &mut overlay, &self.env_file, &self.store)
    }

    fn persist_provider(&self, provider: &Table) -> bool {
        self.release3()
            .and_then(|service| service.persist_provider(provider))
            .is_ok()
    }

    fn browser_authenticate<'a>(&'a self, provider: &'a Table) -> AuthKeyFuture<'a> {
        Box::pin(async move {
            let mut service = SignInService::new(Self::gateway(provider)?, SystemSignInRuntime);
            let mut sink = |_: vibe_core::auth::SignInEvent| {};
            let result = service.authenticate(&mut sink).await;
            service.close().await;
            result
        })
    }

    fn start_attempt<'a>(&'a self, provider: &'a Table) -> AuthAttemptFuture<'a> {
        Box::pin(async move {
            let mut service = SignInService::new(Self::gateway(provider)?, SystemSignInRuntime);
            let result = service.start_attempt().await;
            service.close().await;
            result
        })
    }

    fn complete_attempt<'a>(
        &'a self,
        provider: &'a Table,
        attempt: &'a SignInAttempt,
    ) -> AuthKeyFuture<'a> {
        Box::pin(async move {
            let mut service = SignInService::new(Self::gateway(provider)?, SystemSignInRuntime);
            let result = service.complete_attempt(attempt).await;
            service.close().await;
            result
        })
    }
}

/// A delegated attempt waiting for its completion call.
#[derive(Clone)]
struct PendingSignIn {
    attempt: SignInAttempt,
    provider: Table,
}

/// Reference `AcpAuthController`, over the environment above.
pub(crate) struct AuthController {
    environment: Arc<dyn AcpAuthEnvironment>,
    pending: Mutex<BTreeMap<String, PendingSignIn>>,
}

impl AuthController {
    pub(crate) fn new(environment: Arc<dyn AcpAuthEnvironment>) -> Self {
        Self {
            environment,
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    /// The browser methods the provider predicate admits. Reference
    /// `browser_methods`: none at all when the provider cannot browser
    /// sign-in, and the delegated variant only when the client advertised it.
    pub(crate) fn browser_methods(&self, delegated: bool) -> Vec<Value> {
        if !supports_browser_sign_in(&self.environment.load_provider()) {
            return Vec::new();
        }
        let mut methods = vec![browser_method("browser-auth")];
        if delegated {
            methods.push(browser_method("browser-auth-delegated"));
        }
        methods
    }

    pub(crate) async fn authenticate(
        &self,
        method_id: &str,
        arguments: &Value,
    ) -> Result<Value, AcpError> {
        match method_id {
            "browser-auth" => self.authenticate_browser(arguments).await,
            "browser-auth-delegated" => self.authenticate_delegated(arguments).await,
            _ => Err(AcpError::UnsupportedAuthentication(method_id.to_owned())),
        }
    }

    /// Reference `status`: reassess provenance from a fresh dotenv read.
    ///
    /// # Errors
    ///
    /// The dotenv read failure, surfaced as the internal failure the
    /// reference's raised `OSError` becomes.
    pub(crate) fn status(&self) -> Result<AuthState, AcpError> {
        let provider = self.environment.load_provider();
        let env_key = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        self.environment.assess(env_key).map_err(|error| {
            AcpError::AuthFailure(format!("auth state assessment failed: {error}"))
        })
    }

    /// The `auth/status` payload, with the reference's four field names.
    pub(crate) fn status_payload(&self) -> Result<Value, AcpError> {
        let state = self.status()?;
        Ok(json!({
            "authenticated": state.can_use_active_provider,
            "authState": state.kind.as_str(),
            "signOutAvailable": state.sign_out_available,
            "customDomain": self.custom_domain(),
        }))
    }

    /// The configured console domain when it differs from the shipped
    /// default. Reference `custom_domain`.
    pub(crate) fn custom_domain(&self) -> Option<String> {
        configured_custom_domain(&self.environment.load_provider()).map(str::to_owned)
    }

    /// Reference `sign_out`: refused unless the assessed state marks it
    /// available, and a storage failure surfaces after the removal cleared
    /// what it could.
    pub(crate) fn sign_out(&self) -> Result<(), AcpError> {
        let provider = self.environment.load_provider();
        let state = self.status()?;
        if !state.sign_out_available {
            return Err(AcpError::InvalidParams(format!(
                "sign-out is not available in auth state `{}`",
                state.kind.as_str()
            )));
        }
        let env_key = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        self.environment
            .remove_api_key(env_key)
            .map_err(|error| AcpError::AuthFailure(format!("sign-out did not complete: {error}")))
    }

    async fn authenticate_browser(&self, arguments: &Value) -> Result<Value, AcpError> {
        match arguments.get("action") {
            None | Some(Value::Null) => {}
            Some(Value::String(action)) if action == "start" => {}
            Some(action) => {
                return Err(AcpError::InvalidParams(format!(
                    "browser auth action `{action}` is not supported"
                )));
            }
        }
        let provider = self.resolve_sign_in_provider(arguments)?;
        let api_key = self
            .environment
            .browser_authenticate(&provider)
            .await
            .map_err(|error| AcpError::AuthFailure(error.message().to_owned()))?;
        let mut meta = self.persist_credentials(&provider, &api_key);
        meta.insert("status".to_owned(), json!("completed"));
        Ok(json!({"_meta": {"browser-auth": meta}}))
    }

    async fn authenticate_delegated(&self, arguments: &Value) -> Result<Value, AcpError> {
        let action = match arguments.get("action") {
            None => "start",
            Some(Value::String(action)) => action.as_str(),
            Some(action) => {
                return Err(AcpError::InvalidParams(format!(
                    "delegated browser auth action `{action}` is not supported"
                )));
            }
        };
        match action {
            "start" => self.start_delegated(arguments).await,
            "complete" => self.complete_delegated(arguments).await,
            action => Err(AcpError::InvalidParams(format!(
                "delegated browser auth action `{action}` is not supported"
            ))),
        }
    }

    async fn start_delegated(&self, arguments: &Value) -> Result<Value, AcpError> {
        let provider = self.resolve_sign_in_provider(arguments)?;
        let attempt = self
            .environment
            .start_attempt(&provider)
            .await
            .map_err(|error| AcpError::AuthFailure(error.message().to_owned()))?;
        let response = json!({
            "_meta": {
                "browser-auth-delegated": {
                    "attemptId": attempt.process_id,
                    "expiresAt": attempt.expires_at.to_iso8601().replace("+00:00", "Z"),
                    "signInUrl": attempt.sign_in_url,
                }
            }
        });
        self.pending().insert(
            attempt.process_id.clone(),
            PendingSignIn { attempt, provider },
        );
        Ok(response)
    }

    async fn complete_delegated(&self, arguments: &Value) -> Result<Value, AcpError> {
        let attempt_id = arguments
            .get("attemptId")
            .or_else(|| arguments.get("attempt_id"))
            .and_then(Value::as_str)
            .filter(|attempt_id| !attempt_id.is_empty())
            .ok_or_else(|| {
                AcpError::InvalidParams("a browser sign-in attempt ID is required".to_owned())
            })?
            .to_owned();
        let pending = self.pending().get(&attempt_id).cloned().ok_or_else(|| {
            AcpError::InvalidParams(format!(
                "no browser sign-in attempt is pending under `{attempt_id}`"
            ))
        })?;
        let completed = self
            .environment
            .complete_attempt(&pending.provider, &pending.attempt)
            .await;
        let api_key = match completed {
            Ok(api_key) => api_key,
            Err(error) => {
                // A poll or exchange hiccup leaves the attempt completable;
                // every other failure discards it, as the reference does.
                if !matches!(
                    error.code,
                    SignInErrorCode::ExchangeFailed | SignInErrorCode::PollFailed
                ) {
                    self.pending().remove(&attempt_id);
                }
                return Err(AcpError::InvalidParams(error.message().to_owned()));
            }
        };
        self.pending().remove(&attempt_id);
        let mut meta = self.persist_credentials(&pending.provider, &api_key);
        meta.insert("status".to_owned(), json!("completed"));
        let mut delegated = Map::new();
        delegated.insert("attemptId".to_owned(), json!(attempt_id));
        for (key, value) in meta {
            delegated.insert(key, value);
        }
        Ok(json!({"_meta": {"browser-auth-delegated": delegated}}))
    }

    /// Reference `_persist_credentials`: the key first, then the provider
    /// entry when the flow modified it, without rolling the key back when the
    /// provider write fails.
    fn persist_credentials(&self, provider: &Table, api_key: &str) -> Map<String, Value> {
        let resolved = resolve_api_key_provider(provider);
        let env_key = resolved
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let backend_is_mistral =
            resolved.get("backend").and_then(toml::Value::as_str) == Some("mistral");
        let custom_domain = configured_custom_domain(provider).is_some();
        let outcome =
            self.environment
                .persist_api_key(env_key, backend_is_mistral, api_key, custom_domain);
        let mut meta = Map::new();
        meta.insert(
            "persistResult".to_owned(),
            json!(outcome.as_reference_string()),
        );
        if *provider != self.environment.load_provider() {
            let persisted = self.environment.persist_provider(provider);
            meta.insert(
                "persistProviderResult".to_owned(),
                json!(if persisted { "completed" } else { "failed" }),
            );
        }
        meta
    }

    /// Reference `_resolve_sign_in_provider`: the configured provider as-is,
    /// or with its browser-auth URLs replaced by the shipped defaults or by a
    /// validated custom domain.
    fn resolve_sign_in_provider(&self, arguments: &Value) -> Result<Table, AcpError> {
        let provider = self.enabled_provider()?;
        let target = match arguments.get("signInTarget") {
            None | Some(Value::Null) => return Ok(provider),
            Some(target) => target,
        };
        if target == SIGN_IN_TARGET_MISTRAL {
            return Ok(with_browser_auth_urls(
                provider,
                DEFAULT_BROWSER_AUTH_BASE_URL,
                DEFAULT_BROWSER_AUTH_API_BASE_URL,
            ));
        }
        if target != SIGN_IN_TARGET_CUSTOM {
            return Err(AcpError::InvalidParams(format!(
                "sign-in target `{target}` is not supported"
            )));
        }
        let domain = arguments
            .get("domain")
            .and_then(Value::as_str)
            .filter(|domain| is_valid_custom_domain(domain))
            .ok_or_else(|| {
                AcpError::InvalidParams("the custom sign-in domain is not valid".to_owned())
            })?;
        let (base_url, api_base_url) = resolve_browser_auth_urls(domain);
        Ok(with_browser_auth_urls(provider, &base_url, &api_base_url))
    }

    fn enabled_provider(&self) -> Result<Table, AcpError> {
        let provider = self.environment.load_provider();
        if !supports_browser_sign_in(&provider) {
            return Err(AcpError::InvalidParams(
                "the configured provider does not support browser sign-in".to_owned(),
            ));
        }
        Ok(provider)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, PendingSignIn>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn with_browser_auth_urls(mut provider: Table, base_url: &str, api_base_url: &str) -> Table {
    provider.insert(
        "browser_auth_base_url".to_owned(),
        toml::Value::String(base_url.to_owned()),
    );
    provider.insert(
        "browser_auth_api_base_url".to_owned(),
        toml::Value::String(api_base_url.to_owned()),
    );
    provider
}

/// One advertised browser method. The reference's `AuthMethodAgent` carries
/// an id, a name and a description; the sentences are this port's own, which
/// `NOTICE` keeps permanently unequal to the reference's.
fn browser_method(method_id: &str) -> Value {
    json!({
        "id": method_id,
        "name": "Sign in with Mistral",
        "description": "Open your browser to connect Mistral Vibe to your Mistral account.",
    })
}

/// The command that reaches the setup flow. The reference relaunches its own
/// entrypoint because that entrypoint runs the onboarding itself; here the
/// flow ships in the `vibe` binary, so the method names the sibling installed
/// beside this one and falls back to the bare name for a `PATH` lookup. An
/// editor that runs it gets the setup flow, which is what the method promises.
fn setup_command() -> String {
    let binary = if cfg!(windows) { "vibe.exe" } else { "vibe" };
    std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name(binary))
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.to_owned())
}

/// The terminal method a `terminal-auth` client receives: the command and
/// arguments that relaunch this build in setup mode. Reference
/// `TerminalAuthMethod` with id `vibe-setup`.
pub(crate) fn terminal_method() -> Value {
    let command = setup_command();
    let args = json!(["--setup"]);
    json!({
        "type": "terminal",
        "id": "vibe-setup",
        "name": "Add an API key",
        "description": "Run the Mistral Vibe setup flow in a terminal to save an API key.",
        "args": args,
        "_meta": {
            "terminal-auth": {
                "command": command,
                "args": args,
                "label": "Vibe setup",
            }
        },
    })
}
