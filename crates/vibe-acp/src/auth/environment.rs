//! The production authentication environment: the effective configuration
//! through the release-3 service, the OS keyring, the global dotenv, and the
//! HTTP sign-in gateway under the system runtime.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use toml::Table;
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_core::auth::{
    AuthState, HttpSignInGateway, KeyringStore, PersistOutcome, RemoveError, SignInAttempt,
    SignInError, SignInErrorCode, SignInService, SystemSignInRuntime, resolve_active_provider,
};

use crate::auth::{AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture};

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
