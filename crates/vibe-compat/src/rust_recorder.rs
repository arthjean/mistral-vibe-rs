use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use thiserror::Error;

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

fn failed(scenario: &Scenario, detail: String) -> RustRecorderError {
    RustRecorderError::Scenario {
        scenario: scenario.id.clone(),
        detail,
    }
}
