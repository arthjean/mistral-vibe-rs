//! The production authentication environment: the effective configuration
//! through the workspace service, the OS keyring, the global dotenv, and the
//! HTTP sign-in gateway under the system runtime.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use toml::Table;
use vibe_app_server::workspace::{WorkspacePaths, WorkspaceService};
use vibe_core::auth::{
    AuthState, DEFAULT_CONSOLE_BASE_URL, DEFAULT_VIBE_BASE_URL, HttpSignInGateway, KeyringStore,
    PersistOutcome, ProviderCredentialsRequest, ProviderCredentialsResult, RemoveError,
    SignInAttempt, SignInError, SignInErrorCode, SignInService, SystemSignInRuntime,
    resolve_active_provider,
};
use vibe_core::whoami::{HttpWhoAmIGateway, resolve_tenant_domains};

use crate::auth::{
    AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture, ConfiguredBases, TenantDomainsFuture,
};

/// The production environment: the effective configuration through the
/// workspace service, the OS keyring, the global dotenv, and the HTTP sign-in
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

    fn workspace(
        &self,
    ) -> Result<WorkspaceService, vibe_app_server::workspace::WorkspaceServiceError> {
        WorkspaceService::new(
            WorkspacePaths {
                vibe_home: self.vibe_home.clone(),
                working_directory: self.working_directory.clone(),
                session_root: self.vibe_home.join("sessions"),
            },
            false,
        )
    }

    /// The raw effective configuration, `None` when it cannot be read.
    /// The raw snapshot is load-bearing: the public view redacts
    /// `api_key_env_var` as a sensitive key, and a redacted name cannot
    /// address a credential.
    fn effective_config(&self) -> Option<Value> {
        self.workspace()
            .ok()
            .and_then(|service| service.layered_config().load().ok())
            .and_then(|snapshot| serde_json::to_value(snapshot.effective).ok())
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
        let document = self.effective_config();
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

    fn load_bases(&self) -> ConfiguredBases {
        let document = self.effective_config();
        let field = |name: &str, default: &str| {
            document
                .as_ref()
                .and_then(|document| document.get(name))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(default)
                .to_owned()
        };
        ConfiguredBases {
            console_base_url: field("console_base_url", DEFAULT_CONSOLE_BASE_URL),
            vibe_base_url: field("vibe_base_url", DEFAULT_VIBE_BASE_URL),
        }
    }

    fn persist_provider_credentials(
        &self,
        request: &ProviderCredentialsRequest,
    ) -> ProviderCredentialsResult {
        match self.workspace() {
            Ok(service) => {
                vibe_core::auth::persist_provider_credentials(&service.layered_config(), request)
            }
            Err(_) => ProviderCredentialsResult {
                provider: false,
                console_base_url: request.console_base_url.as_ref().map(|_| false),
                vibe_base_url: request.vibe_base_url.as_ref().map(|_| false),
            },
        }
    }

    fn resolve_tenant_domains<'a>(
        &'a self,
        provider: Table,
        console_base_url: &'a str,
        api_key: &'a str,
        vibe_base_url: &'a str,
    ) -> TenantDomainsFuture<'a> {
        Box::pin(async move {
            match HttpWhoAmIGateway::production() {
                Some(gateway) => {
                    resolve_tenant_domains(
                        &gateway,
                        provider,
                        console_base_url,
                        api_key,
                        vibe_base_url,
                    )
                    .await
                }
                None => (provider, vibe_base_url.to_owned()),
            }
        })
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
