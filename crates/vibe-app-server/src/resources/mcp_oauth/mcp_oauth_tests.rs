use super::*;

fn http_config(alias: &str, url: &str) -> McpServerConfig {
    McpServerConfig {
        alias: alias.to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: Url::parse(url).expect("fixture URL"),
            headers: BTreeMap::new(),
        },
        enabled: false,
        disabled_tools: Default::default(),
        startup_timeout_ms: 1_000,
        tool_timeout_ms: 1_000,
        auth: Default::default(),
        prompt: None,
        sampling_enabled: true,
    }
}

#[test]
fn extracts_quoted_resource_metadata() {
    assert_eq!(
        bearer_parameter(
            "Bearer resource_metadata=\"https://mcp.test/meta\", realm=\"mcp\"",
            "resource_metadata"
        ),
        Some("https://mcp.test/meta")
    );
}

#[test]
fn authorization_metadata_preserves_issuer_path() {
    let issuer = Url::parse("https://auth.test/tenant").expect("issuer");
    assert_eq!(
        authorization_metadata_url(&issuer).as_str(),
        "https://auth.test/.well-known/oauth-authorization-server/tenant"
    );
    let resource = Url::parse("https://mcp.test/tenant/rpc").expect("resource");
    assert_eq!(
        protected_resource_metadata_url(&resource).as_str(),
        "https://mcp.test/.well-known/oauth-protected-resource/tenant/rpc"
    );
    assert!(validate_authorization_issuer(&issuer, &issuer).is_ok());
    assert!(
        validate_authorization_issuer(
            &Url::parse("https://other.test/tenant").expect("other issuer"),
            &issuer,
        )
        .is_err()
    );
    assert!(validate_token_type("Bearer").is_ok());
    assert!(validate_token_type("bearer").is_ok());
    assert!(validate_token_type("DPoP").is_err());
}

#[test]
fn protected_resource_metadata_is_bound_to_the_requested_mcp() {
    let requested = Url::parse("https://mcp.test/rpc").expect("requested resource");
    let matching = ProtectedResourceMetadata {
        resource: requested.clone(),
        authorization_servers: Vec::new(),
        scopes_supported: Vec::new(),
    };
    assert!(validate_protected_resource(&matching, &requested).is_ok());

    let trailing_slash = ProtectedResourceMetadata {
        resource: Url::parse("https://mcp.test/rpc/").expect("metadata resource"),
        authorization_servers: Vec::new(),
        scopes_supported: Vec::new(),
    };
    assert!(validate_protected_resource(&trailing_slash, &requested).is_err());

    let mismatched = ProtectedResourceMetadata {
        resource: Url::parse("https://attacker.test/rpc").expect("metadata resource"),
        authorization_servers: Vec::new(),
        scopes_supported: Vec::new(),
    };
    assert!(matches!(
        validate_protected_resource(&mismatched, &requested),
        Err(ResourceError::Unavailable(message)) if message.contains("does not match")
    ));
    assert!(
        serde_json::from_str::<ProtectedResourceMetadata>(
            r#"{"authorization_servers":["https://auth.test"]}"#
        )
        .is_err()
    );
}

#[test]
fn resource_identity_and_tokens_are_exact() {
    let resource = Url::parse("https://mcp.test/rpc").expect("resource");
    let trailing = Url::parse("https://mcp.test/rpc/").expect("trailing resource");
    assert_ne!(
        resource_identity(&resource).expect("identity"),
        resource_identity(&trailing).expect("trailing identity")
    );
    assert!(
        resource_identity(&Url::parse("https://mcp.test/rpc#fragment").expect("fragment")).is_err()
    );
    assert!(
        validate_token_response(&TokenResponse {
            access_token: "   ".to_owned(),
            token_type: "Bearer".to_owned(),
            refresh_token: None,
            expires_in: None,
        })
        .is_err()
    );
}

#[tokio::test]
async fn completion_poll_is_non_blocking_and_pending_keys_are_session_resource_scoped() {
    let auth = ProductionMcpAuth::new().expect("OAuth client");
    let first = http_config("shared", "https://first.example/mcp");
    let second = http_config("shared", "https://second.example/mcp");
    let (sender, receiver) = watch::channel(None);
    let task = tokio::spawn(std::future::pending::<()>());
    auth.pending.lock().await.insert(
        PendingKey {
            session_id: "session-a".to_owned(),
            resource: "https://first.example/mcp".to_owned(),
        },
        PendingLogin {
            completion: receiver,
            task: task.abort_handle(),
        },
    );

    assert!(
        !tokio::time::timeout(
            Duration::from_millis(50),
            auth.complete_login("session-a", &first)
        )
        .await
        .expect("poll returns immediately")
        .expect("pending is not an error")
    );
    assert_eq!(auth.pending.lock().await.len(), 1);
    assert_ne!(
        keyring_account(transport_url(&first.transport).expect("first resource"))
            .expect("first account"),
        keyring_account(transport_url(&second.transport).expect("second resource"))
            .expect("second account")
    );
    drop(sender);
    task.abort();
}

#[tokio::test]
async fn closing_a_session_aborts_only_its_pending_callbacks_before_persistence() {
    let auth = ProductionMcpAuth::new().expect("OAuth client");
    let (_first_sender, first_receiver) = watch::channel(None);
    let (_second_sender, second_receiver) = watch::channel(None);
    let first_task = tokio::spawn(std::future::pending::<()>());
    let second_task = tokio::spawn(std::future::pending::<()>());
    auth.pending.lock().await.extend([
        (
            PendingKey {
                session_id: "session-a".to_owned(),
                resource: "https://first.example/mcp".to_owned(),
            },
            PendingLogin {
                completion: first_receiver,
                task: first_task.abort_handle(),
            },
        ),
        (
            PendingKey {
                session_id: "session-b".to_owned(),
                resource: "https://second.example/mcp".to_owned(),
            },
            PendingLogin {
                completion: second_receiver,
                task: second_task.abort_handle(),
            },
        ),
    ]);

    auth.close_session("session-a")
        .await
        .expect("session callback cleanup");
    assert_eq!(auth.pending.lock().await.len(), 1);
    assert_eq!(
        auth.pending
            .lock()
            .await
            .keys()
            .next()
            .map(|key| key.session_id.as_str()),
        Some("session-b")
    );
    assert!(
        first_task
            .await
            .expect_err("closed callback is aborted")
            .is_cancelled()
    );
    second_task.abort();
}
