//! The ACP authentication surface: method gates, the delegated lifecycle,
//! status, and sign-out.

mod browser;
mod methods;
mod status;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use toml::Table;
use vibe_app_server::client::EchoTurnDriver;
use vibe_core::auth::{
    AuthState, AuthStateKind, KeyringFailure, PersistOutcome, RemoveError, SignInError,
    SignInErrorCode, SignInGateway, SignInPoll, SignInProcess, SignInService, SystemSignInRuntime,
    UtcTimestamp, default_mistral_provider,
};

use crate::agent::AcpAgent;
use crate::auth::{AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture};
use crate::protocol::{AcpClientCapabilities, AcpClientInfo, AcpInitializeRequest};

pub(super) fn signed_out_state() -> AuthState {
    AuthState {
        kind: AuthStateKind::SignedOut,
        can_use_active_provider: false,
        sign_out_available: false,
        env_key: Some("MISTRAL_API_KEY".to_owned()),
    }
}

fn keyring_state() -> AuthState {
    AuthState {
        kind: AuthStateKind::OsKeyring,
        can_use_active_provider: true,
        sign_out_available: true,
        env_key: Some("MISTRAL_API_KEY".to_owned()),
    }
}

fn process_env_state() -> AuthState {
    AuthState {
        kind: AuthStateKind::ProcessEnv,
        can_use_active_provider: true,
        sign_out_available: false,
        env_key: Some("MISTRAL_API_KEY".to_owned()),
    }
}

fn sign_in_process(process_id: &str) -> SignInProcess {
    SignInProcess {
        process_id: process_id.to_owned(),
        sign_in_url: "https://console.mistral.ai/vibe/sign-in/web".to_owned(),
        poll_url: "https://console.mistral.ai/api/vibe/sign-in/web/poll".to_owned(),
        // 2100-01-01T00:00:00Z, safely in the future for any clock.
        expires_at: UtcTimestamp::from_micros_since_epoch(4_102_444_800_000_000),
    }
}

/// Hands out one scripted creation response; the polling half is unreachable
/// from a delegated start.
struct OneShotGateway(Option<SignInProcess>);

impl SignInGateway for OneShotGateway {
    async fn create_process(
        &mut self,
        _code_challenge: &str,
    ) -> Result<SignInProcess, SignInError> {
        self.0
            .take()
            .ok_or_else(|| SignInError::new(SignInErrorCode::StartFailed))
    }

    async fn poll(&mut self, _poll_url: &str) -> Result<SignInPoll, SignInError> {
        Err(SignInError::new(SignInErrorCode::PollFailed))
    }

    async fn exchange(
        &mut self,
        _process_id: &str,
        _exchange_token: &str,
        _code_verifier: &str,
    ) -> Result<String, SignInError> {
        Err(SignInError::new(SignInErrorCode::ExchangeFailed))
    }

    async fn close(&mut self) {}
}

/// The reference's constructor ports as one scripted world: a fixed provider,
/// a scripted assessment, queued sign-in outcomes, and full call recording.
pub(super) struct ScriptedAuthEnvironment {
    pub(super) provider: Mutex<Table>,
    pub(super) state: Mutex<AuthState>,
    pub(super) remove_failure: Mutex<Option<KeyringFailure>>,
    pub(super) browser_results: Mutex<VecDeque<Result<String, SignInErrorCode>>>,
    pub(super) start_processes: Mutex<VecDeque<SignInProcess>>,
    pub(super) complete_results: Mutex<VecDeque<Result<String, SignInErrorCode>>>,
    pub(super) persisted: Mutex<Vec<(String, String, bool)>>,
    pub(super) persisted_providers: Mutex<Vec<Table>>,
    pub(super) removed: Mutex<Vec<String>>,
    pub(super) sign_in_providers: Mutex<Vec<Table>>,
    pub(super) start_calls: AtomicUsize,
}

impl Default for ScriptedAuthEnvironment {
    fn default() -> Self {
        Self {
            provider: Mutex::new(default_mistral_provider()),
            state: Mutex::new(signed_out_state()),
            remove_failure: Mutex::new(None),
            browser_results: Mutex::new(VecDeque::new()),
            start_processes: Mutex::new(VecDeque::new()),
            complete_results: Mutex::new(VecDeque::new()),
            persisted: Mutex::new(Vec::new()),
            persisted_providers: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
            sign_in_providers: Mutex::new(Vec::new()),
            start_calls: AtomicUsize::new(0),
        }
    }
}

impl ScriptedAuthEnvironment {
    #[allow(clippy::unwrap_in_result)]
    fn pop_key_result(
        queue: &Mutex<VecDeque<Result<String, SignInErrorCode>>>,
    ) -> Result<String, SignInError> {
        queue
            .lock()
            .expect("scripted queue")
            .pop_front()
            .unwrap_or(Err(SignInErrorCode::StartFailed))
            .map_err(SignInError::new)
    }
}

#[allow(clippy::unwrap_in_result)]
impl AcpAuthEnvironment for ScriptedAuthEnvironment {
    fn load_provider(&self) -> Table {
        self.provider.lock().expect("scripted provider").clone()
    }

    fn assess(&self, _env_key: &str) -> std::io::Result<AuthState> {
        Ok(self.state.lock().expect("scripted state").clone())
    }

    fn persist_api_key(
        &self,
        env_key: &str,
        _backend_is_mistral: bool,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome {
        self.persisted.lock().expect("persist log").push((
            env_key.to_owned(),
            api_key.to_owned(),
            custom_domain,
        ));
        PersistOutcome::Completed
    }

    fn remove_api_key(&self, env_key: &str) -> Result<(), RemoveError> {
        if let Some(failure) = self.remove_failure.lock().expect("remove failure").take() {
            return Err(RemoveError::Keyring(failure));
        }
        self.removed
            .lock()
            .expect("remove log")
            .push(env_key.to_owned());
        *self.state.lock().expect("scripted state") = signed_out_state();
        Ok(())
    }

    fn persist_provider(&self, provider: &Table) -> bool {
        self.persisted_providers
            .lock()
            .expect("provider log")
            .push(provider.clone());
        true
    }

    fn browser_authenticate<'a>(&'a self, provider: &'a Table) -> AuthKeyFuture<'a> {
        self.sign_in_providers
            .lock()
            .expect("sign-in providers")
            .push(provider.clone());
        Box::pin(async move { Self::pop_key_result(&self.browser_results) })
    }

    fn start_attempt<'a>(&'a self, provider: &'a Table) -> AuthAttemptFuture<'a> {
        self.sign_in_providers
            .lock()
            .expect("sign-in providers")
            .push(provider.clone());
        self.start_calls.fetch_add(1, Ordering::SeqCst);
        let process = self
            .start_processes
            .lock()
            .expect("start queue")
            .pop_front();
        Box::pin(async move {
            let mut service = SignInService::new(OneShotGateway(process), SystemSignInRuntime);
            let attempt = service.start_attempt().await;
            service.close().await;
            attempt
        })
    }

    fn complete_attempt<'a>(
        &'a self,
        _provider: &'a Table,
        _attempt: &'a vibe_core::auth::SignInAttempt,
    ) -> AuthKeyFuture<'a> {
        Box::pin(async move { Self::pop_key_result(&self.complete_results) })
    }
}

pub(super) fn agent_with(environment: Arc<ScriptedAuthEnvironment>) -> AcpAgent<EchoTurnDriver> {
    AcpAgent::new(EchoTurnDriver::new("answer"))
        .expect("agent starts")
        .with_auth_environment(environment)
}

fn initialize_request(meta: Option<Value>, client_name: Option<&str>) -> AcpInitializeRequest {
    AcpInitializeRequest {
        client_capabilities: AcpClientCapabilities {
            meta,
            ..AcpClientCapabilities::default()
        },
        client_info: client_name.map(|name| AcpClientInfo {
            name: name.to_owned(),
            version: "1.0".to_owned(),
            title: None,
        }),
        ..AcpInitializeRequest::default()
    }
}

fn method_ids(methods: &[Value]) -> Vec<&str> {
    methods
        .iter()
        .filter_map(|method| method["id"].as_str())
        .collect()
}

fn non_browser_provider() -> Table {
    let mut provider = default_mistral_provider();
    // An explicitly empty base is `None` for the predicate; a merely absent
    // one would fall back to the shipped defaults on the mistral entry.
    provider.insert(
        "browser_auth_base_url".to_owned(),
        toml::Value::String(String::new()),
    );
    provider.insert(
        "browser_auth_api_base_url".to_owned(),
        toml::Value::String(String::new()),
    );
    provider
}

fn custom_domain_provider() -> Table {
    let mut provider = default_mistral_provider();
    provider.insert(
        "browser_auth_base_url".to_owned(),
        toml::Value::String("https://console.corp.example".to_owned()),
    );
    provider.insert(
        "browser_auth_api_base_url".to_owned(),
        toml::Value::String("https://console.corp.example/api".to_owned()),
    );
    provider
}
