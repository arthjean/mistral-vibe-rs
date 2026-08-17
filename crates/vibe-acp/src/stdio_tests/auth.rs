//! Authentication over the wire: advertised methods and the two extension
//! methods that read and clear a credential.

use std::sync::Arc;

use serde_json::json;
use vibe_app_server::client::EchoTurnDriver;

use super::{StaticAuthEnvironment, initialize, request, spawn_stdio, spawn_stdio_with_auth};

#[tokio::test]
async fn the_advertised_auth_methods_follow_the_client_capability_gates() {
    let mut peer = spawn_stdio(EchoTurnDriver::new("ok"));
    peer.send(request(
        1,
        "initialize",
        json!({
            "protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "_meta": {"browser-auth-delegated": true, "terminal-auth": true},
            },
        }),
    ))
    .await;
    let (_, response) = peer.response(1).await;
    let methods = response["result"]["authMethods"]
        .as_array()
        .expect("auth methods")
        .iter()
        .map(|method| method["id"].as_str().expect("method id").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        ["browser-auth", "browser-auth-delegated", "vibe-setup"]
    );
    let terminal = &response["result"]["authMethods"][2];
    assert_eq!(terminal["args"], json!(["--setup"]));
    let command = terminal["_meta"]["terminal-auth"]["command"]
        .as_str()
        .expect("terminal command");
    assert_eq!(
        std::path::Path::new(command).file_stem(),
        Some(std::ffi::OsStr::new("vibe")),
        "{command}"
    );

    // The reference refuses every id `authenticate` does not serve itself,
    // including the terminal id and this port's former `environment` method.
    peer.send(request(
        2,
        "authenticate",
        json!({"methodId": "environment"}),
    ))
    .await;
    let (_, refused) = peer.response(2).await;
    assert_eq!(refused["error"]["code"], -32602);
    peer.shutdown(3).await;
}

#[tokio::test]
async fn jetbrains_clients_with_a_usable_provider_get_no_auth_methods() {
    let mut peer = spawn_stdio_with_auth(
        EchoTurnDriver::new("ok"),
        None,
        Arc::new(StaticAuthEnvironment::dotenv_backed()),
    );
    peer.send(request(
        1,
        "initialize",
        json!({
            "protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION,
            "clientInfo": {"name": "JetBrains.IntelliJ", "version": "2026.2"},
        }),
    ))
    .await;
    let (_, response) = peer.response(1).await;
    assert_eq!(response["result"]["authMethods"], json!([]));
    peer.shutdown(2).await;
}

#[tokio::test]
async fn the_auth_extension_methods_serve_status_and_sign_out() {
    let mut peer = spawn_stdio_with_auth(
        EchoTurnDriver::new("ok"),
        None,
        Arc::new(StaticAuthEnvironment::dotenv_backed()),
    );
    initialize(&mut peer, 1).await;
    peer.send(request(2, "_auth/status", json!({}))).await;
    let (_, status) = peer.response(2).await;
    assert_eq!(
        status["result"],
        json!({
            "authenticated": true,
            "authState": "vibe_home_env_file",
            "signOutAvailable": true,
            "customDomain": null,
        })
    );

    peer.send(request(3, "_auth/signOut", json!({}))).await;
    let (_, signed_out) = peer.response(3).await;
    assert_eq!(signed_out["result"], json!({}));

    peer.send(request(4, "_auth/status", json!({}))).await;
    let (_, status) = peer.response(4).await;
    assert_eq!(status["result"]["authState"], "signed_out");
    assert_eq!(status["result"]["signOutAvailable"], false);

    // Sign-out is now unavailable, so a second call is refused with the
    // invalid-request error and clears nothing.
    peer.send(request(5, "_auth/signOut", json!({}))).await;
    let (_, refused) = peer.response(5).await;
    assert_eq!(refused["error"]["code"], -32602);
    peer.shutdown(6).await;
}
