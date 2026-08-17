//! What the status extension reports, what sign-out clears, and how the
//! production environment reads the world it runs against.

use std::sync::Arc;

use serde_json::json;
use vibe_app_server::client::EchoTurnDriver;
use vibe_core::auth::{
    AuthStateKind, KeyringFailure, KeyringStore, NativeKeyringBackend, PersistOutcome,
};

use super::{
    ScriptedAuthEnvironment, agent_with, custom_domain_provider, keyring_state, process_env_state,
};
use crate::agent::AcpAgent;
use crate::auth::{AcpAuthEnvironment, ProductionAuthEnvironment};

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
