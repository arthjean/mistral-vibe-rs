//! The onboarding seed, and what the wizard's submitted choices write.
//!
//! Every case names the shipped `llamacpp` provider or no provider at all, so
//! none of them reaches the OS keyring: a provider whose key variable is empty
//! needs no key.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use tempfile::tempdir;
use vibe_protocol::ProtocolErrorCode;

use crate::workspace::{WorkspacePaths, WorkspaceService, WorkspaceServiceError};

fn service() -> (tempfile::TempDir, WorkspaceService) {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace,
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    (temporary, service)
}

fn params(value: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(value).expect("a parameter object")
}

#[test]
fn the_seed_describes_the_named_provider_and_the_resolved_configuration() {
    let (_temporary, service) = service();
    let seed = service
        .dispatch("setup/status", &params(json!({"provider": "llamacpp"})))
        .expect("the seed is read")
        .result;
    assert_eq!(
        Value::Object(seed.into_iter().collect()),
        json!({
            "provider": {
                "name": "llamacpp",
                "apiBase": "http://127.0.0.1:8080/v1",
                "apiKeyEnvVar": "",
                "browserAuthBaseUrl": null,
                "browserAuthApiBaseUrl": null,
                "browserAuthAllowOriginRewrite": false,
            },
            "consoleBaseUrl": vibe_core::auth::DEFAULT_CONSOLE_BASE_URL,
            "vibeBaseUrl": vibe_core::auth::DEFAULT_VIBE_BASE_URL,
            "activeModel": "mistral-medium-3.5",
            "theme": "auto",
            "supportsBrowserSignIn": false,
            "hasApiKey": true,
            "enableSystemTrustStore": false,
        })
    );
}

#[test]
fn a_provider_nobody_configured_or_shipped_is_refused() {
    let (_temporary, service) = service();
    for (method, request) in [
        ("setup/status", json!({"provider": "nowhere"})),
        (
            "setup/store-credential",
            json!({"provider": "nowhere", "apiKey": "secret"}),
        ),
    ] {
        let refused = service
            .dispatch(method, &params(request))
            .expect_err("an unknown provider is refused");
        assert!(
            matches!(
                &refused,
                WorkspaceServiceError::Refused(ProtocolErrorCode::InvalidParams, message)
                    if message == "Unknown setup provider: nowhere"
            ),
            "{method}: {refused:?}"
        );
    }
}

#[test]
fn choices_that_move_nothing_write_nothing() {
    let (temporary, service) = service();
    let answer = service
        .dispatch("setup/submit-choices", &params(json!({})))
        .expect("the choices are submitted")
        .result;
    assert_eq!(answer["outcome"], "completed");
    assert_eq!(answer["failures"], json!([]));
    assert!(
        !temporary.path().join("home/config.toml").exists(),
        "no field drifted, so no file is written"
    );
}

#[test]
fn a_moved_provider_and_a_chosen_theme_are_written_to_the_user_file() {
    let (temporary, service) = service();
    let answer = service
        .dispatch(
            "setup/submit-choices",
            &params(json!({
                "provider": {
                    "name": "llamacpp",
                    "apiBase": "http://127.0.0.1:9090/v1",
                },
                "theme": "light",
            })),
        )
        .expect("the choices are submitted")
        .result;
    assert_eq!(answer["outcome"], "completed", "{answer:?}");
    let written = std::fs::read_to_string(temporary.path().join("home/config.toml"))
        .expect("the user file is written");
    assert!(written.contains("http://127.0.0.1:9090/v1"), "{written}");
    assert!(written.contains("theme = \"light\""), "{written}");

    let seed = service
        .dispatch("setup/status", &params(json!({"provider": "llamacpp"})))
        .expect("the seed is read")
        .result;
    assert_eq!(seed["provider"]["apiBase"], "http://127.0.0.1:9090/v1");
    assert_eq!(seed["theme"], "light");
}
