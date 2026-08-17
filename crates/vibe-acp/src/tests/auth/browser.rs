//! The browser sign-in flows: the direct one, and the delegated lifecycle an
//! editor drives in three calls.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::json;
use vibe_core::auth::SignInErrorCode;

use super::{
    ScriptedAuthEnvironment, agent_with, custom_domain_provider, non_browser_provider,
    sign_in_process,
};

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
