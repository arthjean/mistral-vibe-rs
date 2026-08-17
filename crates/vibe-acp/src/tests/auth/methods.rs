//! Which authentication methods the agent advertises, and which ids it serves.

use std::sync::Arc;

use serde_json::json;

use super::{
    ScriptedAuthEnvironment, agent_with, initialize_request, keyring_state, method_ids,
    non_browser_provider,
};
use crate::protocol::AcpError;

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
