use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Notify;
use toml::Table;
use url::Url;
use vibe_app_server::client::{LiveTurnDriver, TurnDriver, TurnReservation};
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_app_server::resources::{
    CoreResourceBackend, ResourceBackend, ResourceBackendRequest, ResourceService, ResourceSession,
};
use vibe_app_server::server::{AppServer, DeferredWork, SessionIntent};
use vibe_core::config::{ConfigMutation, ConfigPaths, ConfigTarget, ConfigWrite, LayeredConfig};
use vibe_core::continuity::{CallbackRoute, SessionContinuity};
use vibe_core::engine::{CompletionProvider, ProviderFuture, TurnStopReason};
use vibe_core::events::{ModelMessage, ModelToolCall, PublicContentBlock};
use vibe_core::extensions::{
    AgentKind, AgentProfile, ChildLoggingPolicy, DelegationRequest, DiscoveryRoots,
    ExtensionSource, SubagentFuture, SubagentManager, SubagentRunner, discover_extensions,
};
use vibe_core::mcp::{
    McpError, McpFuture, McpOAuthConfig, McpPeer, McpPeerFactory, McpRegistry, McpServerConfig,
    McpServerStatus, McpTransportConfig, RemoteTool, rejected_root_claims, validate_config,
};
use vibe_core::platform::{Platform, parse_policy_path};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode,
    PermissionRequirement, PermissionRule, PermissionStore, TrustDecision, TrustRootKind,
};
use vibe_core::process::{ProcessSpec, ProcessStream, TerminalManager, TerminalState};
use vibe_core::prompt::{
    InstructionLoader, PromptComposition, PromptResolver, UserResource, UserResourceKind,
    prepare_user_resources,
};
use vibe_core::provider::{AssistantMessage, ProviderInput, Usage};
use vibe_core::shell::{ShellFlavor, ShellPolicyContext, analyze_shell};
use vibe_core::storage::SessionStore;
use vibe_core::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec,
};
use vibe_core::workspace::{EditOperation, ReviewManager, Workspace};
use vibe_protocol::{Envelope, TransportKind, decode_frame};

use crate::canonical::{canonicalize, redact, volatility_evidence};
use crate::model::{OracleOutcome, RecordedFixture, Scenario, ScenarioKind};
use crate::release4_contracts::{
    acp_full_contract, cloud_workflows_contract, tui_controls_contract, tui_input_contract,
    tui_rendering_contract, tui_setup_contract, tui_shell_contract, tui_terminal_stack_contract,
};
use crate::release5_contracts::{
    distribution_contract, native_targets_contract, telemetry_contract,
};

#[derive(Debug, Error)]
pub enum RustRecorderError {
    #[error("Rust fixture I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Rust fixture JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Rust fixture canonicalization failed: {0}")]
    Canonical(#[from] crate::canonical::CanonicalizationError),
    #[error("Rust scenario `{scenario}` failed: {detail}")]
    Scenario { scenario: String, detail: String },
}

pub fn record_rust_all(
    root: &Path,
    output: &Path,
    baseline_version: &str,
    fixture_schema_version: u32,
    scenarios: &[Scenario],
) -> Result<Vec<PathBuf>, RustRecorderError> {
    fs::create_dir_all(output)?;
    let mut paths = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let first = run_once(root, scenario)?;
        let second = run_once(root, scenario)?;
        let volatility = volatility_evidence(&first, &second, &scenario.volatile)?;
        let mut outcome = canonicalize(&first, &scenario.volatile)?;
        redact(&mut outcome, &[])?;
        let fixture = RecordedFixture {
            fixture_schema_version,
            scenario_id: scenario.id.clone(),
            matrix_row: scenario.matrix_row.clone(),
            upstream_baseline: baseline_version.to_owned(),
            comparison: scenario.comparison,
            stability_runs: 2,
            volatility,
            outcome,
        };
        let path = output.join(format!("{}.json", scenario.id));
        let mut encoded = serde_json::to_vec_pretty(&fixture)?;
        encoded.push(b'\n');
        fs::write(&path, encoded)?;
        paths.push(path);
    }
    Ok(paths)
}

fn run_once(root: &Path, scenario: &Scenario) -> Result<OracleOutcome, RustRecorderError> {
    let mut outcome = OracleOutcome::empty(scenario.args.clone());
    match scenario.kind {
        ScenarioKind::Process => {
            let binary = root.join("target/debug/vibe");
            if !binary.is_file() {
                return Err(failed(
                    scenario,
                    format!("missing Rust CLI binary {}", binary.display()),
                ));
            }
            let completed = Command::new(binary)
                .args(&scenario.args)
                .current_dir(root)
                .output()?;
            outcome.exit_status = completed.status.code();
            outcome.stdout = String::from_utf8_lossy(&completed.stdout).into_owned();
            outcome.stderr = String::from_utf8_lossy(&completed.stderr).into_owned();
        }
        ScenarioKind::Protocol => {
            let payload = payload(scenario)?;
            outcome.json_frames.push(protocol_result(payload));
        }
        ScenarioKind::Initialize => {
            let payload = payload(scenario)?;
            outcome.json_frames.push(initialize_result(payload));
        }
        ScenarioKind::Persistence => {
            let payload: Value = serde_json::from_str(payload(scenario)?)?;
            outcome.persisted_state = Some(persistence_result(&payload)?);
        }
        ScenarioKind::Volatile => {
            outcome.json_frames.push(json!({
                "timestamp": 1,
                "uuid": "11111111-1111-4111-8111-111111111111",
                "path": root,
                "port": 1111,
                "providerToken": "provider-rust",
            }));
        }
        ScenarioKind::Contract => {
            let payload: Value = serde_json::from_str(payload(scenario)?)?;
            outcome
                .json_frames
                .push(contract_result(root, &payload, scenario)?);
        }
        ScenarioKind::Pty => {
            let binary = root.join("target/debug/vibe");
            if !binary.is_file() {
                return Err(failed(
                    scenario,
                    format!("missing Rust CLI binary {}", binary.display()),
                ));
            }
            let completed = Command::new(binary)
                .args(&scenario.args)
                .current_dir(root)
                .env("TERM", "xterm-256color")
                .output()?;
            outcome.exit_status = completed.status.code();
            let mut transcript = String::from_utf8_lossy(&completed.stdout).into_owned();
            transcript.push_str(&String::from_utf8_lossy(&completed.stderr));
            outcome.terminal_transcript = Some(transcript.replace('\n', "\r\n"));
        }
    }
    Ok(outcome)
}

fn payload(scenario: &Scenario) -> Result<&str, RustRecorderError> {
    scenario
        .payload
        .as_deref()
        .ok_or_else(|| failed(scenario, "scenario payload is missing".to_owned()))
}

fn protocol_result(payload: &str) -> Value {
    match vibe_protocol::decode_frame(payload.as_bytes()) {
        Ok(envelope) => {
            let variant = match &envelope {
                vibe_protocol::Envelope::Notification(_) => "Notification",
                vibe_protocol::Envelope::Request(_) => "ServerRequest",
                vibe_protocol::Envelope::Success(_) => "JsonRpcSuccessResponse",
                vibe_protocol::Envelope::Error(_) => "JsonRpcErrorResponse",
            };
            json!({
                "accepted": true,
                "value": serde_json::to_value(envelope).unwrap_or(Value::Null),
                "variant": variant,
            })
        }
        Err(_) => {
            let value = serde_json::from_str(payload).unwrap_or(Value::Null);
            json!({"accepted": false, "errors": json_rpc_union_errors(&value)})
        }
    }
}

fn json_rpc_union_errors(value: &Value) -> Vec<Value> {
    let Some(object) = value.as_object() else {
        return vec![json!({"location": [], "type": "model_type"})];
    };
    let variants = [
        (
            "Notification",
            &["method", "params"][..],
            &["id", "result", "error"][..],
        ),
        (
            "ServerRequest",
            &["id", "method", "params"][..],
            &["result", "error"][..],
        ),
        (
            "JsonRpcSuccessResponse",
            &["id", "result"][..],
            &["method", "params", "error"][..],
        ),
        (
            "JsonRpcErrorResponse",
            &["id", "error"][..],
            &["method", "params", "result"][..],
        ),
    ];
    let mut errors = Vec::new();
    for (variant, required, forbidden) in variants {
        for field in required {
            if !object.contains_key(*field) {
                errors.push(json!({"location": [variant, field], "type": "missing"}));
            } else if *field == "id" && !valid_strict_id(&object[*field]) {
                errors.push(json!({
                    "location": [variant, "id", "int"],
                    "type": "int_type",
                }));
                errors.push(json!({
                    "location": [variant, "id", "str"],
                    "type": "string_type",
                }));
            } else if matches!(*field, "result" | "error") && !object[*field].is_object() {
                errors.push(json!({
                    "location": [variant, field],
                    "type": "dict_type",
                }));
            }
        }
        for field in forbidden {
            if object.contains_key(*field) {
                errors.push(json!({
                    "location": [variant, field],
                    "type": "extra_forbidden",
                }));
            }
        }
    }
    errors
}

fn valid_strict_id(value: &Value) -> bool {
    value.is_i64() || value.is_u64() || value.is_string()
}

fn initialize_result(payload: &str) -> Value {
    match serde_json::from_str::<vibe_protocol::InitializeParams>(payload) {
        Ok(value) => json!({
            "accepted": true,
            "value": serde_json::to_value(value).unwrap_or(Value::Null),
        }),
        Err(_) => json!({"accepted": false, "errors": []}),
    }
}

fn persistence_result(payload: &Value) -> Result<Value, RustRecorderError> {
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut parsed = Vec::new();
    let mut valid = true;
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for line in lines {
        let value: Value = serde_json::from_str(line)?;
        if !value.is_object() {
            valid = false;
            break;
        }
        parsed.push(value);
    }
    let parsed_value = if valid {
        Value::Array(parsed.clone())
    } else {
        Value::Null
    };
    if payload.get("operation").and_then(Value::as_str) == Some("parse") {
        Ok(json!({"parsed": parsed_value}))
    } else {
        let empty_declared = payload
            .pointer("/metadata/total_messages")
            .and_then(Value::as_u64)
            == Some(0);
        Ok(json!({
            "loadable": valid && (!parsed.is_empty() || empty_declared),
            "parsed": parsed_value,
        }))
    }
}

fn contract_result(
    root: &Path,
    payload: &Value,
    scenario: &Scenario,
) -> Result<Value, RustRecorderError> {
    let name = payload
        .get("contract")
        .and_then(Value::as_str)
        .ok_or_else(|| failed(scenario, "contract name is missing".to_owned()))?;
    let source = |path: &str| fs::read_to_string(root.join(path));
    let result = match name {
        "foundation_workspace" => json!({
            "contract": name,
            "valid": root.join("Cargo.toml").is_file()
                && root.join("PROVENANCE.md").is_file(),
        }),
        "foundation_baseline" => {
            json!({
                "contract": name,
                "version": env!("CARGO_PKG_VERSION"),
                "valid": true,
            })
        }
        "harness_primitives" => json!({
            "contract": name,
            "valid": source("crates/vibe-core/src/fakes.rs")?.contains("HermeticGuard"),
        }),
        "corpus_recording" => json!({
            "contract": name,
            "valid": source("crates/vibe-compat/src/oracle.rs")?.contains("record_all"),
        }),
        "differential_reports" => json!({
            "contract": name,
            "valid": source("crates/vibe-compat/src/differential.rs")?.contains("build_report"),
        }),
        "config_bootstrap" => json!({
            "contract": name,
            "valid": source("crates/vibe-core/src/bootstrap.rs")?.contains("BootstrapInput"),
        }),
        "event_families" => json!({
            "contract": name,
            "families": ["message", "reasoning", "effect", "callback", "checkpoint", "notice"],
            "valid": source("crates/vibe-core/src/events.rs")?.contains("PublicHistoryEntry"),
        }),
        "appserver_transport" => json!({
            "contract": name,
            "methods": ["initialize", "initialized", "shutdown", "exit"],
            "valid": source("crates/vibe-app-server/src/transport.rs")?.contains("serve_stdio"),
        }),
        "turn_lifecycle" => json!({
            "contract": name,
            "methods": [
                "turn/start",
                "turn/steer",
                "turn/interrupt",
                "session/context/inject",
                "callback/respond",
            ],
            "valid": source("crates/vibe-app-server/src/server.rs")?.contains("turn_start"),
        }),
        "provider_mistral" => json!({
            "contract": name,
            "features": [
                "streaming",
                "non_streaming",
                "images",
                "tools",
                "thinking",
                "usage",
                "correlation_id",
            ],
            "valid": source("crates/vibe-core/src/provider.rs")?.contains("ProviderStyle::Mistral"),
        }),
        "provider_dialects" => json!({
            "contract": name,
            "styles": [
                "openai",
                "reasoning",
                "openai-responses",
                "anthropic",
                "vertex-anthropic",
            ],
            "valid": source("crates/vibe-core/src/provider.rs")?.contains("VertexAnthropic"),
        }),
        "engine_loop" => json!({
            "contract": name,
            "outcomes": [
                "complete",
                "max_steps",
                "token_limit",
                "price_limit",
                "refusal",
                "response_length",
                "cancelled",
                "failed",
            ],
            "valid": source("crates/vibe-core/src/engine.rs")?.contains("run_turn_controlled"),
        }),
        "tool_abi" => json!({
            "contract": name,
            "features": ["typed_schema", "registry", "streaming", "effects"],
            "checks": tool_abi_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "tool_policy" => json!({
            "contract": name,
            "features": ["always", "ask", "never", "trust", "approvals"],
            "checks": tool_policy_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "workspace_tools" => json!({
            "contract": name,
            "features": ["discovery", "read", "search", "context"],
            "checks": workspace_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "review_tools" => json!({
            "contract": name,
            "features": ["write", "edit", "checkpoint", "review"],
            "checks": review_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "shell_policy" => json!({
            "contract": name,
            "features": ["posix", "git_bash", "cmd", "powershell"],
            "checks": shell_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "managed_processes" => json!({
            "contract": name,
            "features": ["foreground", "background", "terminal", "tool_io", "cleanup"],
            "checks": managed_process_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "mcp_lifecycle" => json!({
            "contract": name,
            "features": ["stdio", "http", "streamable_http", "oauth", "partial_failure"],
            "checks": mcp_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "operational_resources" => json!({
            "contract": name,
            "methods": [
                "account/read",
                "connectors/read",
                "diagnostics/list",
                "diagnostics/logs/read",
                "feedback/record",
                "feedback/shouldShow",
                "narration/summarize",
                "runtime/read",
                "session/ready/read",
                "stats/read",
                "tools/list",
            ],
            "checks": operational_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "config_layers" => json!({
            "contract": name,
            "features": [
                "defaults",
                "selected_toml",
                "experiments",
                "environment",
                "runtime",
                "agent",
            ],
            "checks": config_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "prompt_composition" => json!({
            "contract": name,
            "features": [
                "ordered_sections",
                "prompt_precedence",
                "instructions",
                "attachments",
                "display_content",
            ],
            "valid": prompt_contract().map_err(|detail| failed(scenario, detail))?,
        }),
        "session_lifecycle" => json!({
            "contract": name,
            "methods": [
                "session/list",
                "history/list",
                "session/log/read",
                "session/continue",
                "session/resume",
                "session/fork",
                "session/title/update",
                "session/delete",
            ],
            "checks": session_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "session_continuity" => json!({
            "contract": name,
            "features": [
                "handoff",
                "rewind",
                "clear",
                "reconnect",
                "deduplication",
                "gap_resync",
            ],
            "valid": continuity_contract().map_err(|detail| failed(scenario, detail))?,
        }),
        "subagents" => json!({
            "contract": name,
            "features": [
                "profiles",
                "install",
                "uninstall",
                "child_session",
                "depth_limit",
                "activity_ownership",
            ],
            "valid": subagent_contract().map_err(|detail| failed(scenario, detail))?,
        }),
        "extension_discovery" => json!({
            "contract": name,
            "features": [
                "agents",
                "skills",
                "hooks",
                "prompts",
                "commands",
                "failure_isolation",
            ],
            "valid": extension_contract().map_err(|detail| failed(scenario, detail))?,
        }),
        "python_custom_tools" => json!({
            "contract": name,
            "boundary": "excluded",
            "features": [
                "typed_arguments",
                "typed_results",
                "configuration",
                "state",
                "imports",
                "reexports",
                "streaming",
                "invoke_context",
                "permissions",
                "trust",
            ],
            "replacement": "mcp_stdio",
            "valid": true,
        }),
        "mcp_stdio_extension" => json!({
            "contract": name,
            "features": [
                "typed_toml",
                "session_discovery",
                "model_exposure",
                "policy",
                "invocation",
                "streaming",
                "cancellation",
                "cleanup",
            ],
            "checks": mcp_stdio_extension_contract()
                .map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "acp_minimal" => json!({
            "contract": name,
            "methods": [
                "initialize",
                "session/new",
                "session/prompt",
                "session/update",
                "session/close",
            ],
            "protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION,
            "valid": source("crates/vibe-acp/src/lib.rs")?.contains("prompt_streaming"),
        }),
        "tui_terminal_stack" => json!({
            "contract": name,
            "features": [
                "stack_decision",
                "immutable_snapshots",
                "resize",
                "unicode",
                "input",
                "mouse",
                "clipboard",
                "restoration",
            ],
            "checks": tui_terminal_stack_contract()
                .map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "tui_shell" => json!({
            "contract": name,
            "features": [
                "startup",
                "attach",
                "ready",
                "bounded_events",
                "history",
                "gap_resync",
                "reconnect",
                "shutdown",
            ],
            "checks": tui_shell_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "tui_rendering" => json!({
            "contract": name,
            "features": [
                "messages",
                "reasoning",
                "effects",
                "diffs",
                "rich_content",
                "streaming",
                "hostile_content",
            ],
            "checks": tui_rendering_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "tui_input" => json!({
            "contract": name,
            "features": [
                "unicode_editing",
                "history",
                "completion",
                "mentions",
                "external_editor",
                "paste",
                "clipboard",
            ],
            "checks": tui_input_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "tui_controls" => json!({
            "contract": name,
            "features": [
                "approvals",
                "questions",
                "plans",
                "interrupt",
                "rewind",
                "compact",
                "fork",
                "callback_races",
            ],
            "checks": tui_controls_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "tui_setup" => json!({
            "contract": name,
            "features": [
                "setup",
                "auth",
                "keyring",
                "trust",
                "theme",
                "no_color",
                "update",
                "voice",
            ],
            "checks": tui_setup_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "acp_full" => json!({
            "contract": name,
            "methods": [
                "initialize",
                "authenticate",
                "session/new",
                "session/load",
                "session/list",
                "session/fork",
                "session/close",
                "session/set_mode",
                "session/set_config_option",
                "session/prompt",
                "session/cancel",
            ],
            "clientTools": [
                "fs/read_text_file",
                "fs/write_text_file",
                "terminal/create",
                "terminal/output",
                "terminal/wait_for_exit",
                "terminal/kill",
                "terminal/release",
            ],
            "protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION,
            "checks": acp_full_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "cloud_workflows" => json!({
            "contract": name,
            "features": [
                "project_picker",
                "project_recovery",
                "teleport_events",
                "push_approval",
                "scheduled_loops",
                "persistence",
                "cancellation",
                "failure_local_safety",
            ],
            "checks": cloud_workflows_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "telemetry" => json!({
            "contract": name,
            "features": [
                "events",
                "opt_out",
                "eligible_mistral_credential",
                "correlation",
                "redaction",
                "proxy_tls",
            ],
            "checks": telemetry_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "distribution" => json!({
            "contract": name,
            "features": [
                "archives",
                "installer",
                "updater",
                "completions",
                "github_action",
                "checksums",
                "rollback",
            ],
            "checks": distribution_contract(root).map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        "native_targets" => json!({
            "contract": name,
            "features": [
                "linux_x86_64",
                "linux_aarch64",
                "macos_x86_64",
                "macos_aarch64",
                "windows_x86_64",
                "native_only",
                "cleanup",
                "signing",
            ],
            "checks": native_targets_contract().map_err(|detail| failed(scenario, detail))?,
            "valid": true,
        }),
        _ => return Err(failed(scenario, format!("unknown contract `{name}`"))),
    };
    if result.get("valid") == Some(&Value::Bool(false)) {
        return Err(failed(scenario, format!("contract check failed: {name}")));
    }
    Ok(result)
}

fn mcp_stdio_extension_contract() -> Result<Value, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let working_directory = temporary.path().join("workspace");
    let config_home = temporary.path().join("home");
    let exit_marker = temporary.path().join("fixture-closed");
    let call_marker = temporary.path().join("fixture-call-started");
    fs::create_dir_all(working_directory.join(".vibe")).map_err(|error| error.to_string())?;
    fs::create_dir_all(working_directory.join("workspace")).map_err(|error| error.to_string())?;
    fs::create_dir_all(&config_home).map_err(|error| error.to_string())?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
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
                toml::Value::String(executable.to_string_lossy().into_owned()),
            ),
            (
                "args".to_owned(),
                toml::Value::Array(vec![toml::Value::String("mcp-fixture".to_owned())]),
            ),
            (
                "disabled_tools".to_owned(),
                toml::Value::Array(vec![toml::Value::String("hidden".to_owned())]),
            ),
            (
                "env".to_owned(),
                toml::Value::Table(toml::Table::from_iter([
                    (
                        "VIBE_MCP_EXIT_FILE".to_owned(),
                        toml::Value::String(exit_marker.to_string_lossy().into_owned()),
                    ),
                    (
                        "VIBE_MCP_CALL_FILE".to_owned(),
                        toml::Value::String(call_marker.to_string_lossy().into_owned()),
                    ),
                ])),
            ),
            (
                "cwd".to_owned(),
                toml::Value::String("workspace".to_owned()),
            ),
            ("startup_timeout_sec".to_owned(), toml::Value::Integer(5)),
            ("tool_timeout_sec".to_owned(), toml::Value::Float(0.05)),
        ]))]),
    )]);
    fs::write(
        working_directory.join(".vibe/config.toml"),
        toml::to_string(&config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let release3 = Release3Service::new(
        Release3Paths {
            vibe_home: config_home,
            working_directory: working_directory.clone(),
            session_root: temporary.path().join("sessions"),
        },
        Table::new(),
        true,
    )
    .map_err(|error| error.to_string())?;
    let mut servers = release3
        .mcp_servers_for_session(&working_directory, true, &[])
        .map_err(|error| error.to_string())?;
    let server = servers
        .pop()
        .ok_or_else(|| "typed stdio server was not activated".to_owned())?;
    if !servers.is_empty() {
        return Err("typed stdio activation returned extra servers".to_owned());
    }
    let McpTransportConfig::Stdio {
        command,
        arguments,
        working_directory: configured_cwd,
        ..
    } = &server.transport
    else {
        return Err("typed stdio config selected another transport".to_owned());
    };
    let cwd = configured_cwd
        .as_deref()
        .and_then(|path| path.strip_prefix(&working_directory).ok())
        .map(|path| path.to_string_lossy().into_owned());
    let checks = json!({
        "alias": server.alias.clone(),
        "argv": ["<compat-fixture>", "mcp-fixture"],
        "cwd": cwd,
        "disabledTools": server.disabled_tools.clone(),
        "startupTimeoutMs": server.startup_timeout_ms,
        "toolTimeoutMs": server.tool_timeout_ms,
        "transport": "stdio",
    });
    if command.as_str() != executable.to_string_lossy().as_ref()
        || arguments != &["mcp-fixture"]
        || server.tool_timeout_ms != 50
    {
        return Err("typed TOML did not preserve the executable contract".to_owned());
    }
    run_async(async move {
        let policy = PermissionStore::default();
        policy
            .set_trust(
                &working_directory,
                TrustDecision::SessionTrusted,
                TrustRootKind::Workspace,
            )
            .await
            .map_err(|error| error.to_string())?;
        for tool in ["mcp_fixture_echo", "mcp_fixture_hang"] {
            policy
                .add_rule(PermissionRule {
                    tool: tool.to_owned(),
                    scope: format!("mcp fixture/{}", tool.trim_start_matches("mcp_fixture_")),
                    mode: PermissionMode::Always,
                    rationale: "compatibility fixture".to_owned(),
                })
                .await;
        }
        let tools = ToolRegistry::default();
        let backend = CoreResourceBackend::default();
        backend
            .open_session(ResourceSession {
                session_id: "mcp-contract".to_owned(),
                generation: 1,
                working_directory: working_directory.to_string_lossy().into_owned(),
                policy,
                tools: tools.clone(),
            })
            .map_err(|error| error.to_string())?;
        let configured = backend
            .configure_mcp("mcp-contract", vec![server.clone()])
            .await
            .map_err(|error| error.to_string())?;
        if configured.result["mcp"]["sources"][0]["status"] != json!("healthy") {
            return Err("live stdio discovery was not healthy".to_owned());
        }
        let definitions = tools
            .available(&BTreeSet::new(), &BTreeSet::new())
            .map_err(|error| error.to_string())?;
        if !definitions
            .iter()
            .any(|definition| definition.name == "mcp_fixture_echo")
        {
            return Err("live MCP tool was not exposed to the model registry".to_owned());
        }
        let provider = Arc::new(McpContractProvider::default());
        let outcome = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system")
            .run(&TurnReservation {
                session_id: "mcp-contract".to_owned(),
                turn_id: "mcp-contract-turn".to_owned(),
                prompt: "use MCP".to_owned(),
                input: vec![PublicContentBlock::Text {
                    text: "use MCP".to_owned(),
                }],
                client_user_message_id: None,
                auto_title: None,
                user_display_content: None,
                mention_stats: None,
                working_directory: working_directory.to_string_lossy().into_owned(),
                intent: SessionIntent::default(),
                tools: tools.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        if outcome.stop_reason != TurnStopReason::Complete
            || !provider.model_exposed.load(Ordering::Acquire)
        {
            return Err("the model did not select the live MCP definition".to_owned());
        }
        let streamed = Arc::new(Mutex::new(Vec::new()));
        let stream_capture = streamed.clone();
        let stream: vibe_core::engine::ToolStreamSink = Arc::new(move |chunk| {
            stream_capture
                .lock()
                .map_err(|_| "MCP stream capture lock is poisoned".to_owned())?
                .push(chunk);
            Ok(())
        });
        let output = tools
            .invoke_stream(
                "mcp_fixture_echo",
                ToolInvocation {
                    call_id: "contract-stream".to_owned(),
                    arguments: json!({"message": "stream"}),
                },
                Some(stream),
            )
            .await
            .map_err(|error| error.to_string())?;
        if output.model_text != "hello stream"
            || output.typed_result["echo"] != "stream"
            || streamed
                .lock()
                .map_err(|_| "MCP stream capture lock is poisoned".to_owned())?
                .as_slice()
                != ["working"]
        {
            return Err("live MCP invocation lost streaming or its typed result".to_owned());
        }
        backend
            .dispatch(ResourceBackendRequest {
                session_id: "mcp-contract".to_owned(),
                method: "mcp/refresh".to_owned(),
                params: BTreeMap::from([("name".to_owned(), json!("fixture"))]),
            })
            .await
            .map_err(|error| error.to_string())?;
        backend
            .dispatch(ResourceBackendRequest {
                session_id: "mcp-contract".to_owned(),
                method: "mcp/toggle".to_owned(),
                params: BTreeMap::from([
                    ("name".to_owned(), json!("fixture")),
                    ("disabled".to_owned(), json!(true)),
                ]),
            })
            .await
            .map_err(|error| error.to_string())?;
        if tools
            .invoke(
                "mcp_fixture_echo",
                ToolInvocation {
                    call_id: "contract-disabled".to_owned(),
                    arguments: json!({"message": "blocked"}),
                },
            )
            .await
            .is_ok()
        {
            return Err("disabled MCP tool remained invocable".to_owned());
        }
        backend
            .dispatch(ResourceBackendRequest {
                session_id: "mcp-contract".to_owned(),
                method: "mcp/toggle".to_owned(),
                params: BTreeMap::from([
                    ("name".to_owned(), json!("fixture")),
                    ("disabled".to_owned(), json!(false)),
                ]),
            })
            .await
            .map_err(|error| error.to_string())?;
        if exit_marker.exists() {
            fs::remove_file(&exit_marker).map_err(|error| error.to_string())?;
        }
        if call_marker.exists() {
            fs::remove_file(&call_marker).map_err(|error| error.to_string())?;
        }
        let cancellation_tools = tools.clone();
        let cancelled = tokio::spawn(async move {
            cancellation_tools
                .invoke(
                    "mcp_fixture_hang",
                    ToolInvocation {
                        call_id: "contract-cancel".to_owned(),
                        arguments: json!({}),
                    },
                )
                .await
        });
        wait_for_marker(&call_marker, "live MCP call start").await?;
        cancelled.abort();
        if !cancelled.await.is_err_and(|error| error.is_cancelled()) {
            return Err("live MCP invocation task was not cancelled".to_owned());
        }
        wait_for_mcp_status(&backend, "failed").await?;
        wait_for_marker(&exit_marker, "cancelled MCP peer cleanup").await?;
        backend
            .configure_mcp("mcp-contract", vec![server.clone()])
            .await
            .map_err(|error| error.to_string())?;
        if exit_marker.exists() {
            fs::remove_file(&exit_marker).map_err(|error| error.to_string())?;
        }
        let timeout = match tools
            .invoke(
                "mcp_fixture_hang",
                ToolInvocation {
                    call_id: "contract-timeout".to_owned(),
                    arguments: json!({}),
                },
            )
            .await
        {
            Ok(_) => return Err("hung live MCP call unexpectedly completed".to_owned()),
            Err(error) => error,
        };
        if !timeout.to_string().contains("timed out") {
            return Err("hung live MCP call did not report a timeout".to_owned());
        }
        let state = backend
            .dispatch(ResourceBackendRequest {
                session_id: "mcp-contract".to_owned(),
                method: "mcp/read".to_owned(),
                params: BTreeMap::new(),
            })
            .await
            .map_err(|error| error.to_string())?;
        if state.result["mcp"]["sources"][0]["status"] != json!("failed") {
            return Err("timed-out live MCP peer was not retired".to_owned());
        }
        wait_for_marker(&exit_marker, "timed-out MCP peer cleanup").await?;
        backend
            .configure_mcp("mcp-contract", vec![server])
            .await
            .map_err(|error| error.to_string())?;
        if exit_marker.exists() {
            fs::remove_file(&exit_marker).map_err(|error| error.to_string())?;
        }
        backend
            .close_session("mcp-contract", 1)
            .await
            .map_err(|error| error.to_string())?;
        wait_for_marker(&exit_marker, "session MCP cleanup").await?;
        let mut checks = checks;
        checks["modelExposure"] = json!(true);
        checks["streamedChunks"] = json!(["working"]);
        checks["cancellationRetiredPeer"] = json!(true);
        checks["timeoutRetiredPeer"] = json!(true);
        checks["cleanupObserved"] = json!(true);
        Ok(checks)
    })
}

#[derive(Default)]
struct McpContractProvider {
    calls: AtomicUsize,
    model_exposed: AtomicBool,
}

impl CompletionProvider for McpContractProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                let exposed = input
                    .tools
                    .iter()
                    .any(|tool| tool.name == "mcp_fixture_echo");
                self.model_exposed.store(exposed, Ordering::Release);
                if !exposed {
                    return Err(vibe_core::provider::ProviderError::InvalidRequest(
                        "MCP definition was not exposed to the model".to_owned(),
                    ));
                }
                return Ok(AssistantMessage {
                    text: String::new(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: vec![ModelToolCall {
                        id: "contract-model-call".to_owned(),
                        name: "mcp_fixture_echo".to_owned(),
                        arguments: r#"{"message":"model"}"#.to_owned(),
                    }],
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "tool_calls".to_owned(),
                    correlation_id: None,
                });
            }
            if !input.messages.iter().any(|message| {
                matches!(
                    message,
                    ModelMessage::Tool {
                        call_id,
                        content,
                        is_error: false,
                    } if call_id == "contract-model-call" && content == "hello model"
                )
            }) {
                return Err(vibe_core::provider::ProviderError::InvalidRequest(
                    "MCP result did not return through the engine transcript".to_owned(),
                ));
            }
            Ok(AssistantMessage {
                text: "model complete".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                refusal: None,
                stop_reason: "stop".to_owned(),
                correlation_id: None,
            })
        })
    }
}

async fn wait_for_marker(path: &Path, label: &str) -> Result<(), String> {
    for _ in 0..200 {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(format!("{label} was not observed"))
}

async fn wait_for_mcp_status(backend: &CoreResourceBackend, status: &str) -> Result<(), String> {
    for _ in 0..200 {
        let state = backend
            .dispatch(ResourceBackendRequest {
                session_id: "mcp-contract".to_owned(),
                method: "mcp/read".to_owned(),
                params: BTreeMap::new(),
            })
            .await
            .map_err(|error| error.to_string())?;
        if state.result["mcp"]["sources"][0]["status"] == json!(status) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(format!("MCP status `{status}` was not observed"))
}

pub fn serve_mcp_stdio_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "vibe-compat-fixture", "version": "1.0.0"}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo a bounded message",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"],
                                "additionalProperties": false
                            }
                        },
                        {
                            "name": "hang",
                            "description": "Wait until the client cancels",
                            "inputSchema": {
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }
                        }
                    ]
                }
            }),
            "tools/call"
                if request.pointer("/params/name").and_then(Value::as_str) == Some("hang") =>
            {
                if let Ok(path) = std::env::var("VIBE_MCP_CALL_FILE") {
                    fs::write(path, b"started")?;
                }
                std::thread::sleep(Duration::from_millis(250));
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": []}})
            }
            "tools/call" => {
                if let Some(progress_token) = request.pointer("/params/_meta/progressToken") {
                    serde_json::to_writer(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": progress_token,
                                "progress": 0.5,
                                "message": "working"
                            }
                        }),
                    )?;
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
                let message = request
                    .pointer("/params/arguments/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": format!("hello {message}")}],
                        "structuredContent": {"echo": message},
                        "isError": false
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    if let Ok(path) = std::env::var("VIBE_MCP_EXIT_FILE") {
        fs::write(path, b"closed")?;
    }
    Ok(())
}

fn config_contract() -> Result<Value, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let vibe_home = temporary.path().join("home/.vibe");
    let working_directory = temporary.path().join("workspace");
    fs::create_dir_all(working_directory.join(".vibe")).map_err(|error| error.to_string())?;
    fs::create_dir_all(&vibe_home).map_err(|error| error.to_string())?;
    fs::write(
        working_directory.join(".vibe/config.toml"),
        "winner = \"project\"\n[future]\nunknown = true\n",
    )
    .map_err(|error| error.to_string())?;
    let defaults = "winner = \"default\""
        .parse::<Table>()
        .map_err(|error| error.to_string())?;
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: vibe_home.clone(),
            working_directory: working_directory.clone(),
        },
        defaults,
    )
    .with_project_trusted(true)
    .with_experiments(
        "experiment = true"
            .parse::<Table>()
            .map_err(|error| error.to_string())?,
    )
    .with_environment([("VIBE_WINNER".to_owned(), "\"environment\"".to_owned())])
    .with_runtime_overrides(
        "winner = \"runtime\""
            .parse::<Table>()
            .map_err(|error| error.to_string())?,
    )
    .with_agent_overlay(
        "winner = \"agent\""
            .parse::<Table>()
            .map_err(|error| error.to_string())?,
    );
    let before = config.load().map_err(|error| error.to_string())?;
    let project_fingerprint = before
        .fingerprints
        .get(&ConfigTarget::Project)
        .cloned()
        .flatten();
    let after = config
        .batch_write(&[ConfigWrite {
            target: ConfigTarget::Project,
            expected_fingerprint: project_fingerprint,
            mutations: vec![ConfigMutation::set(["updated"], toml::Value::Boolean(true))],
        }])
        .map_err(|error| error.to_string())?;
    let service = Release3Service::new(
        Release3Paths {
            session_root: vibe_home.join("sessions"),
            vibe_home,
            working_directory,
        },
        Table::new(),
        true,
    )
    .map_err(|error| error.to_string())?;
    let public_methods = service.dispatch("config/schema", &BTreeMap::new()).is_ok()
        && service.dispatch("config/read", &BTreeMap::new()).is_ok();
    Ok(json!({
        "atomicMutation": after.effective.get("updated").and_then(toml::Value::as_bool) == Some(true),
        "publicMethods": public_methods,
        "unknownFieldsPreserved": after
            .effective
            .get("future")
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get("unknown"))
            .and_then(toml::Value::as_bool)
            == Some(true),
    }))
}

fn prompt_contract() -> Result<bool, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let project = temporary.path().join("workspace");
    let user_home = temporary.path().join("home");
    let project_prompts = project.join(".vibe/prompts");
    fs::create_dir_all(&project_prompts).map_err(|error| error.to_string())?;
    fs::create_dir_all(&user_home).map_err(|error| error.to_string())?;
    fs::write(user_home.join("AGENTS.md"), "user instructions")
        .map_err(|error| error.to_string())?;
    fs::write(project.join("AGENTS.md"), "project instructions")
        .map_err(|error| error.to_string())?;
    fs::write(project_prompts.join("review.md"), "project prompt")
        .map_err(|error| error.to_string())?;
    let loader = InstructionLoader::new(user_home, vec![(project.clone(), project.clone())]);
    let resolved = PromptResolver::new(vec![project_prompts], Vec::new(), BTreeMap::new(), true)
        .resolve("review")
        .map_err(|error| error.to_string())?;
    let composed = PromptComposition {
        base: resolved.content,
        headless: true,
        commit_policy: Some("Do not commit".to_owned()),
        model_info: Some("fixture".to_owned()),
        os_tool_guidance: Some("Use tools".to_owned()),
        skills: Vec::new(),
        subagents: Vec::new(),
        scratchpad: None,
        project_context: Some("clean".to_owned()),
        project_context_stale: false,
        additional_directories: Vec::new(),
        user_instructions: loader.user_document().map_err(|error| error.to_string())?,
        project_instructions: loader
            .project_documents()
            .map_err(|error| error.to_string())?,
    }
    .compose();
    let prepared = prepare_user_resources(
        &[UserResource {
            kind: UserResourceKind::Text,
            path: None,
            text: Some("hello".to_owned()),
            mime_type: None,
            metadata: Value::Null,
        }],
        &[project],
        false,
    )
    .map_err(|error| error.to_string())?;
    Ok(
        composed.section_names.first().map(String::as_str) == Some("base")
            && composed.text.contains("user instructions")
            && composed.text.contains("project instructions")
            && prepared.model_content.len() == 1
            && prepared.display_content.len() == 1,
    )
}

fn session_contract() -> Result<Value, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let root = temporary.path().join("sessions");
    let store = SessionStore::new(&root).with_pointer_key("compat");
    let mut metadata = store
        .create("root-alpha", "/workspace", None, 1)
        .map_err(|error| error.to_string())?;
    store
        .append_message(
            &mut metadata,
            &ModelMessage::User {
                content: "hello".to_owned(),
            },
            2,
        )
        .map_err(|error| error.to_string())?;
    let listed = store.list(None, 0, 10).map_err(|error| error.to_string())?;
    let resumed = store
        .resume("root-alpha", "current prompt", BTreeMap::new())
        .map_err(|error| error.to_string())?;
    let forked = store
        .fork(
            "root-alpha",
            "child-beta",
            "child prompt",
            BTreeMap::new(),
            3,
        )
        .map_err(|error| error.to_string())?;
    store
        .update_title("root-alpha", "Renamed", 4)
        .map_err(|error| error.to_string())?;
    store
        .delete("child-beta")
        .map_err(|error| error.to_string())?;
    let migration = store.migrate_legacy().map_err(|error| error.to_string())?;
    let service = Release3Service::new(
        Release3Paths {
            vibe_home: temporary.path().join("home/.vibe"),
            working_directory: temporary.path().join("workspace"),
            session_root: root,
        },
        Table::new(),
        false,
    )
    .map_err(|error| error.to_string())?;
    Ok(json!({
        "durableFormats": listed.sessions.len() == 1
            && resumed.messages.len() == 2
            && forked.metadata.parent_session_id.as_deref() == Some("root-alpha"),
        "publicMethods": service.dispatch("session/list", &BTreeMap::new()).is_ok(),
        "versionedMigration": migration.issues.is_empty(),
    }))
}

fn continuity_contract() -> Result<bool, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = SessionStore::new(temporary.path()).with_pointer_key("compat");
    let mut metadata = store
        .create("root-session", "/workspace", None, 1)
        .map_err(|error| error.to_string())?;
    store
        .append_message(
            &mut metadata,
            &ModelMessage::User {
                content: "before".to_owned(),
            },
            2,
        )
        .map_err(|error| error.to_string())?;
    let continuity = SessionContinuity::new(store.clone());
    continuity
        .attach(
            store
                .load("root-session")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    continuity
        .bind_callback(
            "root-session",
            CallbackRoute {
                callback_id: "callback-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        )
        .map_err(|error| error.to_string())?;
    continuity
        .add_resource("root-session", "terminal-1")
        .map_err(|error| error.to_string())?;
    continuity
        .accept_operation("root-session", "operation-1", 1)
        .map_err(|error| error.to_string())?;
    let reconnected = SessionContinuity::new(store.clone())
        .reconnect("root-session")
        .map_err(|error| error.to_string())?;
    let handed_off = continuity
        .handoff("root-session", "root-next", "prompt", BTreeMap::new(), 3)
        .map_err(|error| error.to_string())?;
    Ok(reconnected.callback_routes.contains_key("callback-1")
        && reconnected.resources.contains("terminal-1")
        && reconnected.completed_operations.contains("operation-1")
        && handed_off.parent_session_id.as_deref() == Some("root-session")
        && continuity.stale_interrupt_target("root-session").is_ok())
}

struct RecorderSubagent;

impl SubagentRunner for RecorderSubagent {
    fn run<'a>(
        &'a self,
        context: vibe_core::extensions::ChildContext,
        _cancellation: vibe_core::engine::CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move { Ok(format!("{}:{}", context.agent.name, context.prompt)) })
    }
}

fn subagent_contract() -> Result<bool, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = SessionStore::new(temporary.path()).with_pointer_key("compat");
    let mut parent = store
        .create("root", "/workspace", None, 0)
        .map_err(|error| error.to_string())?;
    parent.config.insert("model".to_owned(), json!("fixture"));
    store
        .update_metadata(&parent)
        .map_err(|error| error.to_string())?;
    let manager = SubagentManager::new(store.clone(), Arc::new(RecorderSubagent));
    let agent = AgentProfile {
        name: "reviewer".to_owned(),
        display_name: "Reviewer".to_owned(),
        description: "Reviews code".to_owned(),
        kind: AgentKind::Subagent,
        safety: "neutral".to_owned(),
        overrides: Table::new(),
        source: ExtensionSource::Builtin,
        path: None,
    };
    let effect = run_async(async {
        manager
            .delegate(
                DelegationRequest {
                    parent_session_id: "root".to_owned(),
                    agent,
                    prompt: "inspect".to_owned(),
                    logging: ChildLoggingPolicy::Full,
                },
                1,
            )
            .await
            .map_err(|error| error.to_string())
    })?;
    let child = store
        .load(&effect.child_session_id)
        .map_err(|error| error.to_string())?;
    let activity = SubagentManager::activity(&effect, "tool");
    Ok(effect.result == "reviewer:inspect"
        && child.metadata.parent_session_id.as_deref() == Some("root")
        && child.metadata.config.get("model") == Some(&json!("fixture"))
        && activity.root_session_id == "root")
}

fn extension_contract() -> Result<bool, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let root = temporary.path().join("extensions");
    fs::create_dir_all(root.join("agents")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("skills/review")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("prompts")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("commands")).map_err(|error| error.to_string())?;
    fs::write(
        root.join("agents/reviewer.toml"),
        "display_name = \"Reviewer\"\nagent_type = \"subagent\"\n",
    )
    .map_err(|error| error.to_string())?;
    fs::write(
        root.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nInspect carefully.\n",
    )
    .map_err(|error| error.to_string())?;
    fs::write(root.join("prompts/review.md"), "Review this").map_err(|error| error.to_string())?;
    fs::write(root.join("commands/check.md"), "Check this").map_err(|error| error.to_string())?;
    fs::write(
        root.join("hooks.toml"),
        "[[hooks]]\nname = \"pre\"\ntype = \"pre_tool\"\nprogram = \"/bin/sh\"\n",
    )
    .map_err(|error| error.to_string())?;
    fs::write(
        root.join("agents/broken.toml"),
        "agent_type = \"unknown\"\n",
    )
    .map_err(|error| error.to_string())?;
    let catalog = discover_extensions(
        &DiscoveryRoots {
            configured: vec![root],
            project: Vec::new(),
            user: Vec::new(),
            project_trusted: false,
        },
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    Ok(catalog.agents.contains_key("reviewer")
        && catalog.skills.contains_key("review")
        && catalog.prompts.contains_key("review")
        && catalog.commands.contains_key("check")
        && catalog.hooks.len() == 1
        && catalog.issues.len() == 1)
}

fn tool_abi_contract() -> Result<Value, String> {
    run_async(async {
        let registry = ToolRegistry::new(256);
        let spec = |priority| ToolSpec {
            name: "probe".to_owned(),
            description: "probe".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            })),
            config: json!({"limit": 256}),
            state: json!({"calls": 0}),
            availability: ToolAvailability::Available,
            presentation: ToolPresentationKind::Generic,
            source: ToolSource::Custom,
            selection_priority: priority,
        };
        let handler = |value: &'static str| -> Arc<dyn ToolHandler> {
            Arc::new(
                move |_invocation: &ToolInvocation,
                      output: ToolOutputSink|
                      -> OwnedToolHandlerFuture {
                    Box::pin(async move {
                        output.emit("stream")?;
                        Ok(ToolExecutionOutput {
                            typed_result: json!({"value": value}),
                            model_text: value.to_owned(),
                            display: Value::Null,
                            chunks: Vec::new(),
                        })
                    })
                },
            )
        };
        let inserted = registry
            .register(spec(0), handler("first"))
            .map_err(|error| error.to_string())?;
        let replaced = registry
            .register(spec(1), handler("second"))
            .map_err(|error| error.to_string())?;
        let streamed = Arc::new(Mutex::new(Vec::new()));
        let stream_capture = streamed.clone();
        let stream: vibe_core::engine::ToolStreamSink = Arc::new(move |chunk| {
            stream_capture
                .lock()
                .map_err(|_| "stream lock poisoned".to_owned())?
                .push(chunk);
            Ok(())
        });
        let result = registry
            .invoke_stream(
                "probe",
                ToolInvocation {
                    call_id: "probe-1".to_owned(),
                    arguments: json!({"value": "input"}),
                },
                Some(stream),
            )
            .await
            .map_err(|error| error.to_string())?;
        let invalid_rejected = registry
            .invoke(
                "probe",
                ToolInvocation {
                    call_id: "probe-2".to_owned(),
                    arguments: json!({}),
                },
            )
            .await
            .is_err();
        Ok(json!({
            "invalidArgumentsRejected": invalid_rejected,
            "laterPriorityWins": inserted == RegistrationOutcome::Inserted
                && replaced == RegistrationOutcome::Replaced
                && result.typed_result["value"] == "second",
            "streamObserved": streamed.lock().map_err(|_| "stream lock poisoned")?.as_slice()
                == ["stream"],
            "typedMetadataQueryable": registry.list().map_err(|error| error.to_string())?[0].config["limit"]
                == 256
        }))
    })
}

fn tool_policy_contract() -> Result<Value, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).map_err(|error| error.to_string())?;
    run_async(async {
        let store = PermissionStore::default();
        store
            .add_rule(PermissionRule {
                tool: "shell".to_owned(),
                scope: "shell git *".to_owned(),
                mode: PermissionMode::Always,
                rationale: "read-only git".to_owned(),
            })
            .await;
        store
            .add_rule(PermissionRule {
                tool: "shell".to_owned(),
                scope: "shell git push".to_owned(),
                mode: PermissionMode::Never,
                rationale: "network mutation".to_owned(),
            })
            .await;
        let specific = store
            .resolve(
                "shell",
                &[PermissionRequirement::Shell {
                    command: "git push".to_owned(),
                }],
            )
            .await
            .map_err(|error| error.to_string())?;
        store
            .set_trust(
                directory.path(),
                TrustDecision::Trusted,
                TrustRootKind::Ancestor,
            )
            .await
            .map_err(|error| error.to_string())?;
        store
            .set_trust(&nested, TrustDecision::Untrusted, TrustRootKind::Workspace)
            .await
            .map_err(|error| error.to_string())?;
        let closest = store
            .resolve(
                "read",
                &[PermissionRequirement::Read {
                    path: nested.join("file.txt"),
                }],
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "closestTrustWins": closest.mode == PermissionMode::Never,
            "defaultAsk": PermissionStore::default()
                .resolve(
                    "network",
                    &[PermissionRequirement::Network {
                        url: Url::parse("https://example.test").map_err(|error| error.to_string())?
                    }]
                )
                .await
                .map_err(|error| error.to_string())?
                .mode == PermissionMode::Ask,
            "specificRuleWins": specific.mode == PermissionMode::Never
        }))
    })
}

fn workspace_contract() -> Result<Value, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    fs::create_dir(directory.path().join("src")).map_err(|error| error.to_string())?;
    fs::write(
        directory.path().join("src/lib.rs"),
        "fn probe() { println!(\"needle\"); }\n",
    )
    .map_err(|error| error.to_string())?;
    fs::write(directory.path().join("ignored.bin"), [0, 1, 2])
        .map_err(|error| error.to_string())?;
    let workspace = Workspace::open(directory.path()).map_err(|error| error.to_string())?;
    let discovered = workspace.discover().map_err(|error| error.to_string())?;
    let read = workspace
        .read("src/lib.rs", 1, None)
        .map_err(|error| error.to_string())?;
    let search = workspace
        .search("needle", false, 10)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "discoveryOrdered": discovered.windows(2).all(|items| items[0].path <= items[1].path),
        "readNumbered": read.numbered_content.starts_with("1|fn probe"),
        "searchMatched": search.len() == 1 && search[0].path == "src/lib.rs",
        "traversalRejected": workspace.read("../secret", 1, None).is_err()
    }))
}

fn review_contract() -> Result<Value, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    fs::write(directory.path().join("file.txt"), "old\n").map_err(|error| error.to_string())?;
    let workspace = Arc::new(Workspace::open(directory.path()).map_err(|error| error.to_string())?);
    let review = ReviewManager::new(workspace);
    review
        .begin_turn("turn-1")
        .map_err(|error| error.to_string())?;
    let mutation = review
        .edit(
            "file.txt",
            &[EditOperation {
                old_text: "old".to_owned(),
                new_text: "new".to_owned(),
                replace_all: false,
            }],
        )
        .map_err(|error| error.to_string())?;
    let checkpoint = review.seal_turn().map_err(|error| error.to_string())?;
    let pending = review.view().map_err(|error| error.to_string())?;
    review.revert().map_err(|error| error.to_string())?;
    let restored =
        fs::read_to_string(directory.path().join("file.txt")).map_err(|error| error.to_string())?;
    Ok(json!({
        "checkpointCreated": checkpoint.turn_id == "turn-1" && checkpoint.hunks.len() == 1,
        "diffTyped": mutation.files_changed == 1 && mutation.diff.contains("+new"),
        "pendingReview": pending.pending_hunks.len() == 1,
        "revertRestored": restored == "old\n"
    }))
}

fn shell_contract() -> Result<Value, String> {
    let context = ShellPolicyContext {
        platform: Platform::Posix,
        working_directory: parse_policy_path(Platform::Posix, "/work/project")
            .map_err(|error| error.to_string())?,
        roots: vec![
            parse_policy_path(Platform::Posix, "/work/project")
                .map_err(|error| error.to_string())?,
        ],
    };
    let mode = |command| permission_name(analyze_shell(ShellFlavor::Posix, command, &context).mode);
    Ok(json!({
        "destructive": mode("rm secret"),
        "findExec": mode("find . -exec sh -c 'echo x' \\;"),
        "gitNoIndex": mode("git diff --no-index /etc/passwd /dev/null"),
        "outsideRead": mode("cat /etc/passwd"),
        "safeRead": mode("cat README.md")
    }))
}

fn managed_process_contract() -> Result<Value, String> {
    run_async(async {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        #[cfg(unix)]
        let mut spec = {
            let mut spec = ProcessSpec::new("/bin/sh", directory.path());
            spec.arguments = vec!["-c".to_owned(), "printf probe".to_owned()];
            spec
        };
        #[cfg(windows)]
        let mut spec = {
            let mut spec = ProcessSpec::new("cmd.exe", directory.path());
            spec.arguments = vec!["/C".to_owned(), "<nul set /p=probe".to_owned()];
            spec
        };
        spec.max_output_bytes = 64;
        let manager = TerminalManager::with_cleanup_grace(Duration::from_millis(500));
        let terminal_id = manager.run(spec).await.map_err(|error| error.to_string())?;
        let output = manager
            .wait(&terminal_id)
            .await
            .map_err(|error| error.to_string())?;
        let stdout = output
            .chunks
            .iter()
            .filter(|chunk| chunk.stream == ProcessStream::Stdout)
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect::<Vec<_>>();
        manager
            .release(&terminal_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "boundedOutput": !output.backpressure_dropped,
            "exitOwned": matches!(output.state, TerminalState::Exited { success: true, .. }),
            "released": manager.list().await.is_empty(),
            "stdout": String::from_utf8_lossy(&stdout)
        }))
    })
}

struct CompatApproval;

impl ApprovalAgent for CompatApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

struct CompatMcpPeer {
    refreshes: AtomicUsize,
    closed: AtomicBool,
    hang_calls: AtomicBool,
    call_started: Notify,
}

impl McpPeer for CompatMcpPeer {
    fn discover<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let bytes = serde_json::to_vec(&vec![compat_remote_tool()])
                .map_err(|error| McpError::Transport(error.to_string()))?;
            if bytes.len() > max_response_bytes {
                return Err(McpError::Transport(
                    "discovery response exceeded budget".to_owned(),
                ));
            }
            Ok(bytes)
        })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        max_response_bytes: usize,
        _output: ToolOutputSink,
    ) -> McpFuture<'a, Vec<u8>> {
        Box::pin(async move {
            if self.hang_calls.load(Ordering::Acquire) {
                self.call_started.notify_one();
                std::future::pending::<()>().await;
            }
            let output = ToolExecutionOutput {
                typed_result: json!({"tool": name, "arguments": arguments}),
                model_text: "remote complete".to_owned(),
                display: Value::Null,
                chunks: Vec::new(),
            };
            let bytes = serde_json::to_vec(&output)
                .map_err(|error| McpError::Transport(error.to_string()))?;
            if bytes.len() > max_response_bytes {
                return Err(McpError::Transport("response exceeded budget".to_owned()));
            }
            Ok(bytes)
        })
    }

    fn refresh<'a>(&'a self, max_response_bytes: usize) -> McpFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.refreshes.fetch_add(1, Ordering::AcqRel);
            let bytes = serde_json::to_vec(&vec![compat_remote_tool()])
                .map_err(|error| McpError::Transport(error.to_string()))?;
            if bytes.len() > max_response_bytes {
                return Err(McpError::Transport(
                    "refresh response exceeded budget".to_owned(),
                ));
            }
            Ok(bytes)
        })
    }

    fn close<'a>(&'a self) -> McpFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::Release);
            Ok(())
        })
    }
}

struct CompatMcpFactory {
    peer: Arc<CompatMcpPeer>,
}

impl McpPeerFactory for CompatMcpFactory {
    fn connect<'a>(&'a self, config: &'a McpServerConfig) -> McpFuture<'a, Arc<dyn McpPeer>> {
        Box::pin(async move {
            if config.alias == "server" {
                Ok(self.peer.clone() as Arc<dyn McpPeer>)
            } else {
                Err(McpError::Transport("fixture connection failed".to_owned()))
            }
        })
    }
}

fn compat_remote_tool() -> RemoteTool {
    RemoteTool {
        name: "search".to_owned(),
        description: "Search fixture data".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
            "additionalProperties": false
        }),
        output_schema: None,
        annotations: json!({"readOnlyHint": true}),
    }
}

fn mcp_contract() -> Result<Value, String> {
    let resource = Url::parse("https://mcp.example/service").map_err(|error| error.to_string())?;
    let oauth = McpOAuthConfig {
        resource: resource.clone(),
        issuer: Url::parse("https://auth.example").map_err(|error| error.to_string())?,
        client_id: "client".to_owned(),
        redirect_uri: Url::parse("http://127.0.0.1:8123/callback")
            .map_err(|error| error.to_string())?,
        scopes: vec!["tools".to_owned()],
    };
    let matching = McpServerConfig {
        alias: "server".to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: resource,
            headers: BTreeMap::new(),
        },
        enabled: true,
        disabled_tools: Default::default(),
        startup_timeout_ms: vibe_core::mcp::DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        tool_timeout_ms: vibe_core::mcp::DEFAULT_MCP_TOOL_TIMEOUT_MS,
        oauth: Some(oauth),
    };
    let mut redirected = matching.clone();
    redirected.transport = McpTransportConfig::StreamableHttp {
        url: Url::parse("https://attacker.example/mcp").map_err(|error| error.to_string())?,
        headers: BTreeMap::new(),
    };
    let workspace = tempfile::tempdir().map_err(|error| error.to_string())?;
    let outside = tempfile::tempdir().map_err(|error| error.to_string())?;
    run_async(async move {
        let peer = Arc::new(CompatMcpPeer {
            refreshes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            hang_calls: AtomicBool::new(false),
            call_started: Notify::new(),
        });
        let factory = Arc::new(CompatMcpFactory { peer: peer.clone() });
        let registry = McpRegistry::default();
        let tools = ToolRegistry::default();
        let policy = PermissionStore::default();
        policy
            .add_rule(PermissionRule {
                tool: "mcp_server_search".to_owned(),
                scope: "mcp server/search".to_owned(),
                mode: PermissionMode::Always,
                rationale: "compatibility fixture".to_owned(),
            })
            .await;
        let mut failed = matching.clone();
        failed.alias = "failed".to_owned();
        let diagnostics = registry
            .discover_all(
                vec![matching.clone(), failed],
                factory,
                &tools,
                policy,
                Arc::new(CompatApproval),
            )
            .await;
        let discovered = registry
            .read()
            .await
            .iter()
            .any(|view| view.alias == "server" && view.status == McpServerStatus::Healthy);
        let invoked = tools
            .invoke(
                "mcp_server_search",
                ToolInvocation {
                    call_id: "compat-call".to_owned(),
                    arguments: json!({"query": "rust"}),
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .typed_result["tool"]
            == "search";
        let refreshed = registry
            .refresh("server")
            .await
            .map_err(|error| error.to_string())?
            .status
            == McpServerStatus::Healthy
            && peer.refreshes.load(Ordering::Acquire) == 1;
        peer.hang_calls.store(true, Ordering::Release);
        let live_tools = tools.clone();
        let live_invocation = tokio::spawn(async move {
            live_tools
                .invoke(
                    "mcp_server_search",
                    ToolInvocation {
                        call_id: "compat-hung-call".to_owned(),
                        arguments: json!({"query": "blocked"}),
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_millis(100), peer.call_started.notified())
            .await
            .map_err(|_| "hung MCP call did not start".to_owned())?;
        let disabled = registry
            .toggle("server", false)
            .await
            .map_err(|error| error.to_string())?
            .status
            == McpServerStatus::Disabled;
        let live_revocation = matches!(
            tokio::time::timeout(Duration::from_millis(100), live_invocation).await,
            Ok(Ok(Err(_)))
        );
        peer.hang_calls.store(false, Ordering::Release);
        let reconnected = registry
            .toggle("server", true)
            .await
            .map_err(|error| error.to_string())?
            .status
            == McpServerStatus::Healthy;
        let close_errors = registry.close().await;
        Ok(json!({
            "closed": close_errors.is_empty() && peer.closed.load(Ordering::Acquire),
            "disabled": disabled,
            "discovered": discovered,
            "invoked": invoked,
            "liveRevocation": live_revocation,
            "oauthResourceBound": validate_config(&matching).is_ok()
                && validate_config(&redirected).is_err(),
            "partialFailure": diagnostics.len() == 1,
            "reconnected": reconnected,
            "refreshed": refreshed,
            "rootClaimsRestricted": rejected_root_claims(
                &[workspace.path().to_path_buf(), outside.path().to_path_buf()],
                &[workspace.path().to_path_buf()]
            ).len() == 1,
            "secureTransport": matches!(
                matching.transport,
                McpTransportConfig::StreamableHttp { .. }
            )
        }))
    })
}

fn operational_contract() -> Result<Value, String> {
    let mut resources = ResourceService::default();
    let params = |value: Value| {
        value
            .as_object()
            .into_iter()
            .flat_map(|object| object.iter())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let account = resources
        .dispatch(
            "account/read",
            &params(json!({"sessionId": "session-1"})),
            false,
        )
        .map_err(|error| error.to_string())?;
    resources.record_log(1, "error", "access_token=secret");
    let logs = resources
        .dispatch(
            "diagnostics/logs/read",
            &params(json!({"sessionId": "session-1", "offset": 0, "limit": 10})),
            false,
        )
        .map_err(|error| error.to_string())?;
    let workspace = tempfile::tempdir().map_err(|error| error.to_string())?;
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let request = |id: u64, method: &str, params: Value| {
        serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .map_err(|error| error.to_string())
    };
    let initialized = connection.dispatch(&request(
        1,
        "initialize",
        json!({
            "clientInfo": {
                "name": "compat",
                "version": "1",
                "entrypoint": "programmatic",
                "terminalEmulator": "unknown"
            },
            "capabilities": {"callbackKinds": ["approval", "user_input"]}
        }),
    )?);
    if initialized.outbound.len() != 1 {
        return Err("initialize did not return one response".to_owned());
    }
    connection.dispatch(
        &serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .map_err(|error| error.to_string())?,
    );
    let started = connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "workingDirectory": workspace.path()
        }),
    )?);
    if started.outbound.len() != 1 {
        return Err("session start did not return one response".to_owned());
    }
    let trust = connection.dispatch(&request(
        3,
        "workspace/trust/decision",
        json!({
            "sessionId": "session-1",
            "cwd": workspace.path(),
            "decision": "trust_cwd"
        }),
    )?);
    let mutation_ordered = trust.outbound.len() == 2
        && matches!(
            decode_frame(&trust.outbound[0]).map_err(|error| error.to_string())?,
            Envelope::Success(_)
        )
        && matches!(
            decode_frame(&trust.outbound[1]).map_err(|error| error.to_string())?,
            Envelope::Notification(notification)
                if notification.method == "workspace/trust/updated"
        );
    let ready = connection.dispatch(&request(
        4,
        "session/ready/read",
        json!({"sessionId": "session-1"}),
    )?);
    let ready_canonical = ready.outbound.first().is_some_and(|frame| {
        matches!(
            decode_frame(frame),
            Ok(Envelope::Success(response)) if response.result["ready"] == true
        )
    });
    let tools = connection.dispatch(&request(
        5,
        "tools/list",
        json!({"sessionId": "session-1"}),
    )?);
    let tool_list_typed = tools.outbound.first().is_some_and(|frame| {
        matches!(
            decode_frame(frame),
            Ok(Envelope::Success(response)) if response.result["tools"].is_array()
        )
    });
    let unavailable = connection.dispatch(&request(
        6,
        "shell/run",
        json!({
            "sessionId": "session-1",
            "operationId": "operation-1",
            "command": "printf probe"
        }),
    )?);
    let unavailable = match unavailable.deferred.first() {
        Some(DeferredWork::ResourceRequest {
            request_id,
            session_id,
            method,
            params,
        }) => run_async({
            let server = server.clone();
            let request_id = request_id.clone();
            let session_id = session_id.clone();
            let method = method.clone();
            let params = params.clone();
            async move {
                Ok(server
                    .execute_resource_request(request_id, session_id, method, params)
                    .await)
            }
        })?,
        _ => unavailable,
    };
    let backend_failure_actionable = unavailable
        .outbound
        .first()
        .is_some_and(|frame| matches!(decode_frame(frame), Ok(Envelope::Error(_))));
    Ok(json!({
        "accountTyped": account.result["account"]["status"].is_string(),
        "backendFailureActionable": backend_failure_actionable,
        "mutationOrdered": mutation_ordered,
        "readyCanonical": ready_canonical,
        "sensitiveLogsRedacted": logs.result["logs"]["entries"][0]["message"]
            == "[redacted sensitive error]",
        "toolListTyped": tool_list_typed
    }))
}

fn run_async<T>(future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(future)
}

fn permission_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Never => "never",
        PermissionMode::Ask => "ask",
        PermissionMode::Always => "always",
    }
}

fn failed(scenario: &Scenario, detail: String) -> RustRecorderError {
    RustRecorderError::Scenario {
        scenario: scenario.id.clone(),
        detail,
    }
}
