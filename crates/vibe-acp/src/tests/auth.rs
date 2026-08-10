//! The ACP authentication surface: method gates, the delegated lifecycle,
//! status, and sign-out.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use toml::Table;
use vibe_app_server::client::EchoTurnDriver;
use vibe_core::auth::{
    AuthState, AuthStateKind, KeyringFailure, KeyringStore, NativeKeyringBackend, PersistOutcome,
    RemoveError, SignInError, SignInErrorCode, SignInGateway, SignInPoll, SignInProcess,
    SignInService, SystemSignInRuntime, UtcTimestamp, default_mistral_provider,
};

use crate::agent::AcpAgent;
use crate::auth::{
    AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture, ProductionAuthEnvironment,
};
use crate::protocol::{AcpClientCapabilities, AcpClientInfo, AcpError, AcpInitializeRequest};

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

#[test]
fn advertised_methods_follow_the_reference_capability_gates() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    let agent = agent_with(environment);
    let initialized = agent
        .initialize_with(initialize_request(
            Some(json!({"browser-auth-delegated": true, "terminal-auth": true})),
            None,
        ))
        .expect("initialize");
    assert_eq!(
        method_ids(&initialized.auth_methods),
        ["browser-auth", "browser-auth-delegated", "vibe-setup"]
    );
    let terminal = &initialized.auth_methods[2];
    assert_eq!(terminal["type"], "terminal");
    assert_eq!(terminal["args"], json!(["--setup"]));
    let terminal_meta = &terminal["_meta"]["terminal-auth"];
    assert_eq!(terminal_meta["args"], json!(["--setup"]));
    // The advertised command has to be the binary that owns `--setup`, which
    // is the CLI and never this one: `vibe-acp` parses no arguments, so an
    // editor running it would get a second ACP server rather than the setup
    // flow the method promises.
    let command = terminal_meta["command"].as_str().expect("terminal command");
    assert_eq!(
        std::path::Path::new(command).file_stem(),
        Some(std::ffi::OsStr::new("vibe")),
        "{command}"
    );
}

#[test]
fn browser_methods_are_absent_without_their_gates() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    let agent = agent_with(environment);
    let initialized = agent
        .initialize_with(initialize_request(None, None))
        .expect("initialize");
    assert_eq!(method_ids(&initialized.auth_methods), ["browser-auth"]);

    let environment = Arc::new(ScriptedAuthEnvironment::default());
    *environment.provider.lock().expect("provider") = non_browser_provider();
    let agent = agent_with(environment);
    let initialized = agent
        .initialize_with(initialize_request(
            Some(json!({"browser-auth-delegated": true, "terminal-auth": true})),
            None,
        ))
        .expect("initialize");
    // No browser method without the provider predicate; the terminal method
    // is gated on the client capability alone.
    assert_eq!(method_ids(&initialized.auth_methods), ["vibe-setup"]);
}

#[test]
fn jetbrains_clients_with_a_usable_provider_see_no_methods() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    *environment.state.lock().expect("state") = keyring_state();
    let agent = agent_with(environment);
    let initialized = agent
        .initialize_with(initialize_request(
            Some(json!({"terminal-auth": true})),
            Some("JetBrains.Fleet"),
        ))
        .expect("initialize");
    assert!(initialized.auth_methods.is_empty());

    let environment = Arc::new(ScriptedAuthEnvironment::default());
    let agent = agent_with(environment);
    let initialized = agent
        .initialize_with(initialize_request(None, Some("JetBrains.Fleet")))
        .expect("initialize");
    // A signed-out JetBrains client keeps the methods.
    assert_eq!(method_ids(&initialized.auth_methods), ["browser-auth"]);
}

#[tokio::test]
async fn unknown_methods_are_refused_with_the_unsupported_method_error() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    let agent = agent_with(environment);
    agent.initialize().expect("initialize");
    let error = agent
        .authenticate("environment", &json!({}))
        .await
        .expect_err("refused");
    assert!(matches!(error, AcpError::UnsupportedAuthentication(_)));
    assert_eq!(error.json_rpc_code(), -32602);
}

#[tokio::test]
async fn browser_auth_persists_the_key_and_reports_completion() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    environment
        .browser_results
        .lock()
        .expect("browser queue")
        .push_back(Ok("signed-in-key".to_owned()));
    let agent = agent_with(environment.clone());
    agent.initialize().expect("initialize");
    let response = agent
        .authenticate("browser-auth", &json!({}))
        .await
        .expect("browser auth");
    let meta = &response["_meta"]["browser-auth"];
    assert_eq!(meta["persistResult"], "completed");
    assert_eq!(meta["status"], "completed");
    assert!(meta.get("persistProviderResult").is_none());
    assert_eq!(
        environment
            .persisted
            .lock()
            .expect("persist log")
            .as_slice(),
        [(
            "MISTRAL_API_KEY".to_owned(),
            "signed-in-key".to_owned(),
            false
        )]
    );
}

#[tokio::test]
async fn browser_auth_failures_and_unknown_actions_are_mapped() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    environment
        .browser_results
        .lock()
        .expect("browser queue")
        .push_back(Err(SignInErrorCode::Denied));
    let agent = agent_with(environment.clone());
    agent.initialize().expect("initialize");
    let error = agent
        .authenticate("browser-auth", &json!({}))
        .await
        .expect_err("denied");
    assert_eq!(error.json_rpc_code(), -32603);
    assert!(
        environment
            .persisted
            .lock()
            .expect("persist log")
            .is_empty()
    );

    let error = agent
        .authenticate("browser-auth", &json!({"action": "poke"}))
        .await
        .expect_err("unknown action");
    assert_eq!(error.json_rpc_code(), -32602);
}

#[tokio::test]
async fn sign_in_targets_replace_the_browser_auth_urls() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    *environment.provider.lock().expect("provider") = custom_domain_provider();
    environment
        .browser_results
        .lock()
        .expect("browser queue")
        .push_back(Ok("key-one".to_owned()));
    environment
        .browser_results
        .lock()
        .expect("browser queue")
        .push_back(Ok("key-two".to_owned()));
    let agent = agent_with(environment.clone());
    agent.initialize().expect("initialize");

    let response = agent
        .authenticate("browser-auth", &json!({"signInTarget": "mistral"}))
        .await
        .expect("default target");
    // The provider was modified back to the defaults, so the entry persists.
    assert_eq!(
        response["_meta"]["browser-auth"]["persistProviderResult"],
        "completed"
    );
    {
        let providers = environment.sign_in_providers.lock().expect("providers");
        assert_eq!(
            providers[0]["browser_auth_base_url"].as_str(),
            Some("https://console.mistral.ai")
        );
    }

    agent
        .authenticate(
            "browser-auth",
            &json!({"signInTarget": "custom", "domain": "console.corp.example"}),
        )
        .await
        .expect("custom target");
    {
        let providers = environment.sign_in_providers.lock().expect("providers");
        assert_eq!(
            providers[1]["browser_auth_base_url"].as_str(),
            Some("https://console.corp.example")
        );
        assert_eq!(
            providers[1]["browser_auth_api_base_url"].as_str(),
            Some("https://console.corp.example/api")
        );
    }

    let error = agent
        .authenticate(
            "browser-auth",
            &json!({"signInTarget": "custom", "domain": "https:/broken"}),
        )
        .await
        .expect_err("invalid domain");
    assert_eq!(error.json_rpc_code(), -32602);
    let error = agent
        .authenticate("browser-auth", &json!({"signInTarget": "elsewhere"}))
        .await
        .expect_err("unknown target");
    assert_eq!(error.json_rpc_code(), -32602);
}

#[tokio::test]
async fn browser_auth_is_refused_when_the_provider_cannot_sign_in() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    *environment.provider.lock().expect("provider") = non_browser_provider();
    let agent = agent_with(environment);
    agent.initialize().expect("initialize");
    let error = agent
        .authenticate("browser-auth", &json!({}))
        .await
        .expect_err("no browser sign-in");
    assert_eq!(error.json_rpc_code(), -32602);
}

#[tokio::test]
async fn the_delegated_lifecycle_starts_completes_and_discards() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    environment
        .start_processes
        .lock()
        .expect("start queue")
        .push_back(sign_in_process("process-1"));
    environment
        .complete_results
        .lock()
        .expect("complete queue")
        .push_back(Ok("delegated-key".to_owned()));
    let agent = agent_with(environment.clone());
    agent.initialize().expect("initialize");

    let started = agent
        .authenticate("browser-auth-delegated", &json!({"action": "start"}))
        .await
        .expect("delegated start");
    let meta = &started["_meta"]["browser-auth-delegated"];
    assert_eq!(meta["attemptId"], "process-1");
    assert_eq!(
        meta["signInUrl"],
        "https://console.mistral.ai/vibe/sign-in/web"
    );
    assert_eq!(meta["expiresAt"], "2100-01-01T00:00:00Z");

    let completed = agent
        .authenticate(
            "browser-auth-delegated",
            &json!({"action": "complete", "attemptId": "process-1"}),
        )
        .await
        .expect("delegated complete");
    let meta = &completed["_meta"]["browser-auth-delegated"];
    assert_eq!(meta["attemptId"], "process-1");
    assert_eq!(meta["persistResult"], "completed");
    assert_eq!(meta["status"], "completed");

    // The completed attempt is gone.
    let error = agent
        .authenticate(
            "browser-auth-delegated",
            &json!({"action": "complete", "attemptId": "process-1"}),
        )
        .await
        .expect_err("attempt consumed");
    assert_eq!(error.json_rpc_code(), -32602);
}

#[tokio::test]
async fn delegated_completion_for_an_unknown_attempt_starts_nothing() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    let agent = agent_with(environment.clone());
    agent.initialize().expect("initialize");
    let error = agent
        .authenticate(
            "browser-auth-delegated",
            &json!({"action": "complete", "attemptId": "never-started"}),
        )
        .await
        .expect_err("unknown attempt");
    assert_eq!(error.json_rpc_code(), -32602);
    assert_eq!(environment.start_calls.load(Ordering::SeqCst), 0);

    let error = agent
        .authenticate("browser-auth-delegated", &json!({"action": "complete"}))
        .await
        .expect_err("missing attempt id");
    assert_eq!(error.json_rpc_code(), -32602);
    let error = agent
        .authenticate("browser-auth-delegated", &json!({"action": "abandon"}))
        .await
        .expect_err("unknown action");
    assert_eq!(error.json_rpc_code(), -32602);
}

#[tokio::test]
async fn recoverable_delegated_failures_keep_the_attempt_completable() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    environment
        .start_processes
        .lock()
        .expect("start queue")
        .extend([sign_in_process("retryable"), sign_in_process("fatal")]);
    environment.complete_results.lock().expect("queue").extend([
        Err(SignInErrorCode::PollFailed),
        Ok("second-try-key".to_owned()),
        Err(SignInErrorCode::Denied),
    ]);
    let agent = agent_with(environment.clone());
    agent.initialize().expect("initialize");

    agent
        .authenticate("browser-auth-delegated", &json!({}))
        .await
        .expect("first start");
    let complete = json!({"action": "complete", "attemptId": "retryable"});
    let error = agent
        .authenticate("browser-auth-delegated", &complete)
        .await
        .expect_err("poll hiccup");
    assert_eq!(error.json_rpc_code(), -32602);
    // The poll failure left the attempt in place, so the retry completes.
    agent
        .authenticate("browser-auth-delegated", &complete)
        .await
        .expect("retry completes");

    agent
        .authenticate("browser-auth-delegated", &json!({}))
        .await
        .expect("second start");
    let complete = json!({"action": "complete", "attemptId": "fatal"});
    agent
        .authenticate("browser-auth-delegated", &complete)
        .await
        .expect_err("denied");
    let error = agent
        .authenticate("browser-auth-delegated", &complete)
        .await
        .expect_err("attempt discarded");
    assert!(error.to_string().contains("fatal"), "{error}");
}

#[tokio::test]
async fn status_answers_with_the_reference_field_names() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    let agent = agent_with(environment.clone());
    assert_eq!(
        agent.auth_status().expect("status"),
        json!({
            "authenticated": false,
            "authState": "signed_out",
            "signOutAvailable": false,
            "customDomain": null,
        })
    );

    *environment.provider.lock().expect("provider") = custom_domain_provider();
    *environment.state.lock().expect("state") = keyring_state();
    assert_eq!(
        agent.auth_status().expect("status"),
        json!({
            "authenticated": true,
            "authState": "os_keyring",
            "signOutAvailable": true,
            "customDomain": "https://console.corp.example",
        })
    );
}

#[tokio::test]
async fn sign_out_is_refused_where_the_reference_refuses_it() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    *environment.state.lock().expect("state") = process_env_state();
    let agent = agent_with(environment.clone());
    let error = agent.auth_sign_out().expect_err("not owned");
    assert_eq!(error.json_rpc_code(), -32602);
    assert!(environment.removed.lock().expect("remove log").is_empty());

    *environment.state.lock().expect("state") = keyring_state();
    assert_eq!(agent.auth_sign_out().expect("sign out"), json!({}));
    assert_eq!(
        environment.removed.lock().expect("remove log").as_slice(),
        ["MISTRAL_API_KEY".to_owned()]
    );
    // The scripted removal flips the state, as a real removal empties the
    // sources the next assessment consults.
    assert_eq!(
        agent.auth_status().expect("status")["authState"],
        "signed_out"
    );
}

#[tokio::test]
async fn a_storage_failure_during_sign_out_is_an_internal_error() {
    let environment = Arc::new(ScriptedAuthEnvironment::default());
    *environment.state.lock().expect("state") = keyring_state();
    *environment.remove_failure.lock().expect("remove failure") =
        Some(KeyringFailure::Backend("store exploded".to_owned()));
    let agent = agent_with(environment);
    let error = agent.auth_sign_out().expect_err("storage failure");
    assert_eq!(error.json_rpc_code(), -32603);
}

const TEST_ENV_KEY: &str = "VIBE_ACP_AUTH_TEST_KEY";

fn production_environment(vibe_home: &std::path::Path) -> ProductionAuthEnvironment {
    ProductionAuthEnvironment::with_store(
        vibe_home.to_path_buf(),
        KeyringStore::disabled(Box::new(NativeKeyringBackend::new())),
    )
}

#[test]
fn the_production_environment_rereads_the_dotenv_on_every_assessment() {
    let home = tempfile::tempdir().expect("vibe home");
    let environment = production_environment(home.path());
    assert_eq!(
        environment.assess(TEST_ENV_KEY).expect("assess").kind,
        AuthStateKind::SignedOut
    );
    // The file is written after construction, so only a fresh read sees it.
    std::fs::write(home.path().join(".env"), format!("{TEST_ENV_KEY}=held\n"))
        .expect("dotenv write");
    let state = environment.assess(TEST_ENV_KEY).expect("assess");
    assert_eq!(state.kind, AuthStateKind::UnsupportedProvider);
    assert!(state.can_use_active_provider);
}

#[test]
fn the_production_environment_persists_and_removes_through_the_dotenv_fallback() {
    let home = tempfile::tempdir().expect("vibe home");
    let environment = production_environment(home.path());
    // The keyring is disabled, so the reference behavior is the fallback
    // write, reported as a completion because the key is durably saved.
    assert_eq!(
        environment.persist_api_key(TEST_ENV_KEY, true, "fallback-secret", false),
        PersistOutcome::Completed
    );
    let env_file = home.path().join(".env");
    assert!(
        std::fs::read_to_string(&env_file)
            .expect("dotenv")
            .contains("fallback-secret")
    );
    environment.remove_api_key(TEST_ENV_KEY).expect("remove");
    assert!(
        !std::fs::read_to_string(&env_file)
            .expect("dotenv")
            .contains("fallback-secret")
    );
    assert_eq!(
        environment.assess(TEST_ENV_KEY).expect("assess").kind,
        AuthStateKind::SignedOut
    );
}

#[tokio::test]
async fn the_production_environment_reports_the_configured_custom_domain() {
    let home = tempfile::tempdir().expect("vibe home");
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[[providers]]\n\
             name = \"mistral\"\n\
             api_key_env_var = \"{TEST_ENV_KEY}\"\n\
             browser_auth_base_url = \"https://console.corp.example\"\n\
             browser_auth_api_base_url = \"https://console.corp.example/api\"\n"
        ),
    )
    .expect("user config");
    let agent = AcpAgent::new(EchoTurnDriver::new("answer"))
        .expect("agent starts")
        .with_auth_environment(Arc::new(production_environment(home.path())));
    let status = agent.auth_status().expect("status");
    assert_eq!(status["customDomain"], "https://console.corp.example");
    assert_eq!(status["authState"], "signed_out");
}
