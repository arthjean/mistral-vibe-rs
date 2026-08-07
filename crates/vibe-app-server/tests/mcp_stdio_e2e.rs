#![cfg(feature = "test-fixtures")]

use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::tempdir;
use vibe_app_server::client::{LiveTurnDriver, TurnDriver, TurnReservation};
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_app_server::resources::{CoreResourceBackend, ResourceBackend, ResourceSession};
use vibe_app_server::server::SessionIntent;
use vibe_core::engine::{CompletionProvider, ProviderFuture};
use vibe_core::events::{ModelMessage, ModelToolCall, PublicContentBlock};
use vibe_core::policy::{PermissionMode, PermissionStore, TrustDecision, TrustRootKind};
use vibe_core::provider::{AssistantMessage, ProviderInput, ToolDefinition, Usage};
use vibe_core::tools::ToolRegistry;

#[derive(Default)]
struct ModelSelectsMcp {
    calls: Mutex<u8>,
    definitions: Mutex<Vec<Vec<ToolDefinition>>>,
}

impl CompletionProvider for ModelSelectsMcp {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.definitions
                .lock()
                .map_err(|_| vibe_core::provider::ProviderError::InvalidRequest("lock".to_owned()))?
                .push(input.tools.clone());
            let mut calls = self.calls.lock().map_err(|_| {
                vibe_core::provider::ProviderError::InvalidRequest("lock".to_owned())
            })?;
            let response = if *calls == 0 {
                AssistantMessage {
                    text: String::new(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: vec![ModelToolCall {
                        id: "call-1".to_owned(),
                        name: "fixture_echo".to_owned(),
                        arguments: r#"{"message":"rust"}"#.to_owned(),
                    }],
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "tool_calls".to_owned(),
                    correlation_id: None,
                }
            } else {
                if !input.messages.iter().any(|message| {
                    matches!(
                        message,
                        ModelMessage::Tool {
                            call_id,
                            content,
                            is_error: false,
                        } if call_id == "call-1" && content == "hello rust"
                    )
                }) {
                    return Err(vibe_core::provider::ProviderError::InvalidRequest(
                        "MCP result did not return through the engine transcript".to_owned(),
                    ));
                }
                AssistantMessage {
                    text: "done".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                }
            };
            *calls = calls.saturating_add(1);
            Ok(response)
        })
    }
}

#[tokio::test]
async fn production_stdio_server_reaches_model_registry_and_effect_lifecycle() {
    let temporary = tempdir().expect("temporary workspace");
    let exit_file = temporary.path().join("server-closed");
    let descendant_file = temporary.path().join("descendant-leaked");
    let policy = PermissionStore::default();
    policy
        .set_trust(
            temporary.path(),
            TrustDecision::SessionTrusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("workspace trust");
    // An MCP tool raises no granular requirement, so an operator who trusts the
    // server grants the tool itself for the session.
    policy.set_tool_permission("fixture_echo", PermissionMode::Always);
    let tools = ToolRegistry::default();
    let backend = CoreResourceBackend::default();
    backend
        .open_session(ResourceSession {
            session_id: "session-1".to_owned(),
            generation: 1,
            working_directory: temporary.path().to_string_lossy().into_owned(),
            project_trusted: true,
            policy,
            tools: tools.clone(),
        })
        .expect("resource session");
    let fixture = env!("CARGO_BIN_EXE_vibe-mcp-stdio-fixture");
    // The vibe home sits inside its own parent, as a real one does: project
    // discovery stops at the directory holding the vibe home, so a home that is
    // a direct child of the working directory would end the walk before the
    // project file is seen.
    let config_home = temporary.path().join("home/.vibe");
    std::fs::create_dir_all(&config_home).expect("config home");
    std::fs::create_dir_all(temporary.path().join(".vibe")).expect("project config directory");
    let config = toml::Table::from_iter([(
        "mcp_servers".to_owned(),
        toml::Value::Array(vec![toml::Value::Table(toml::Table::from_iter([
            ("name".to_owned(), toml::Value::String("fixture".to_owned())),
            (
                "transport".to_owned(),
                toml::Value::String("stdio".to_owned()),
            ),
            (
                "command".to_owned(),
                toml::Value::String(fixture.to_owned()),
            ),
            ("args".to_owned(), toml::Value::Array(Vec::new())),
            (
                "env".to_owned(),
                toml::Value::Table(toml::Table::from_iter([
                    (
                        "VIBE_MCP_EXIT_FILE".to_owned(),
                        toml::Value::String(exit_file.to_string_lossy().into_owned()),
                    ),
                    (
                        "VIBE_MCP_DESCENDANT_FILE".to_owned(),
                        toml::Value::String(descendant_file.to_string_lossy().into_owned()),
                    ),
                ])),
            ),
            (
                "cwd".to_owned(),
                toml::Value::String(temporary.path().to_string_lossy().into_owned()),
            ),
            ("startup_timeout_sec".to_owned(), toml::Value::Integer(5)),
            ("tool_timeout_sec".to_owned(), toml::Value::Integer(5)),
        ]))]),
    )]);
    std::fs::write(
        temporary.path().join(".vibe/config.toml"),
        toml::to_string(&config).expect("typed config serializes"),
    )
    .expect("typed config persists");
    let release3 = Release3Service::new(
        Release3Paths {
            vibe_home: config_home,
            working_directory: temporary.path().to_path_buf(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("release-3 config service");
    let configs = release3
        .mcp_servers_for_session(temporary.path(), true, &[])
        .expect("typed TOML MCP config");
    let dispatch = backend
        .configure_mcp("session-1", configs)
        .await
        .expect("production MCP discovery");
    assert!(
        dispatch.signals.runtime_updated,
        "discovery moved runtime state, so `runtime/updated` follows the answer"
    );
    assert!(dispatch.signals.warnings.is_empty());
    // The answer declares no state of its own; what the backend learned travels
    // on the signals, which is what the runtime snapshot is composed from.
    let integrations = dispatch
        .signals
        .integrations
        .as_ref()
        .expect("discovery reports the integration state");
    assert_eq!(integrations.mcp["sources"][0]["status"], json!("connected"));

    assert_eq!(
        tools.list().expect("registered tools").len(),
        2,
        "all paginated tools are registered"
    );
    let provider = Arc::new(ModelSelectsMcp::default());
    let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system");
    let outcome = driver
        .run(&TurnReservation {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "use the fixture".to_owned(),
            input: vec![PublicContentBlock::Text {
                text: "use the fixture".to_owned(),
            }],
            prepared_images: None,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: temporary.path().to_string_lossy().into_owned(),
            intent: SessionIntent::default(),
            tools: tools.clone(),
        })
        .await
        .expect("engine turn");
    assert_eq!(
        outcome.stop_reason,
        vibe_core::engine::TurnStopReason::Complete
    );
    {
        let seen = provider.definitions.lock().expect("definitions");
        assert!(seen[0].iter().any(|tool| tool.name == "fixture_echo"));
    }

    backend
        .close_session("session-1", 1)
        .await
        .expect("owned MCP cleanup");
    assert_eq!(
        std::fs::read(&exit_file).expect("fixture observed stdin closure"),
        b"closed"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1_700)).await;
    assert!(
        !descendant_file.exists(),
        "the MCP descendant survived session cleanup"
    );
}
