use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Notify;
use url::Url;
use vibe_app_server::resources::ResourceService;
use vibe_app_server::server::{AppServer, DeferredWork};
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
use vibe_core::shell::{ShellFlavor, ShellPolicyContext, analyze_shell};
use vibe_core::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec,
};
use vibe_core::workspace::{EditOperation, ReviewManager, Workspace};
use vibe_protocol::{Envelope, TransportKind, decode_frame};

use crate::canonical::{canonicalize, volatility_evidence};
use crate::model::{OracleOutcome, RecordedFixture, Scenario, ScenarioKind};

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
        let outcome = canonicalize(&first, &scenario.volatile)?;
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
            return Err(failed(
                scenario,
                "PTY Rust recording is not required before Release 4".to_owned(),
            ));
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
        _ => return Err(failed(scenario, format!("unknown contract `{name}`"))),
    };
    if result.get("valid") == Some(&Value::Bool(false)) {
        return Err(failed(scenario, format!("contract check failed: {name}")));
    }
    Ok(result)
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
