use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde::Deserialize;
use serde_json::Value;
use vibe_core::config::{ProxyEnvironmentStore, ProxyKey};

use super::{REFERENCE_COMMIT, Reference, pinned_python_oracle};
use crate::tui::chat_input::{InputMode, Safety, VoicePhase};
use crate::tui::cloud_workflow::format_loop_list;
use crate::tui::completion::CompletionEngine;
use crate::tui::input::PromptEditor;
use crate::tui::interaction::{
    AuthAction, AuthActionKind, IntegrationKind, Overlay, OverlayAction,
};
use crate::tui::pickers::{
    config_overlay, mcp_auth_overlay, mcp_overlay, proxy_overlay, remote_projects_overlay,
    teleport_push_overlay,
};
use crate::tui::render::{BannerContext, TokenState, UiContext, draw};
use crate::tui::setup::{DetectedTheme, Theme, resolve_theme};
use crate::tui::state::TuiState;
use crate::tui::workflow::{McpEffect, reduce_auth_action, valid_auth_url};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    oracle: Oracle,
    oracle_probe: OracleProbe,
    traces: Vec<Trace>,
    unavailable: Vec<Unavailable>,
}

/// A dimension the pinned reference cannot express, so no trace can prove it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Unavailable {
    dimension: String,
    reason: String,
}

/// One event where the Rust surface deliberately departs from the captured
/// reference. Every mismatch must carry one of these, and every declared
/// divergence must correspond to a real mismatch.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Divergence {
    event: usize,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Oracle {
    engine: String,
    commit: String,
    deterministic_runs: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OracleProbe {
    script: String,
    expected: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trace {
    id: String,
    story: String,
    events: Vec<Event>,
    /// What the Rust runtime must observe.
    expected: Vec<String>,
    /// What the pinned Python reference observes, captured by the oracle.
    reference: Vec<String>,
    #[serde(default)]
    divergences: Vec<Divergence>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Config {
        snapshot: Value,
        schema: Value,
    },
    Proxy {
        settings: Value,
    },
    ProxyMutation {
        initial: String,
        changes: BTreeMap<String, Option<String>>,
    },
    Mcp {
        mcp: Value,
        connectors: Value,
    },
    McpDetail {
        mcp: Value,
        connectors: Value,
        source: String,
        source_kind: String,
    },
    McpAuth {
        source: String,
        source_kind: String,
        url: String,
    },
    AuthAction {
        source: String,
        source_kind: String,
        url: String,
        action: String,
    },
    Projects {
        view: Value,
        /// Kept in the fixture: the reference picker title is the same either way.
        #[allow(dead_code)]
        teleport: bool,
    },
    Loops {
        loops: Value,
        now_seconds: u64,
    },
    Teleport {
        event: Value,
    },
}

fn apply(event: Event) -> String {
    match event {
        Event::Config { snapshot, schema } => {
            overlay_observation("config", &config_overlay(&snapshot, &schema))
        }
        Event::Proxy { settings } => overlay_observation("proxy", &proxy_overlay(&settings)),
        Event::ProxyMutation { initial, changes } => {
            let temporary = tempfile::tempdir().expect("EP-009 proxy oracle directory");
            let path = temporary.path().join(".env");
            fs::write(&path, initial).expect("EP-009 proxy oracle fixture");
            let changes = changes
                .into_iter()
                .map(|(key, value)| {
                    ProxyKey::try_from(key.as_str())
                        .map(|key| (key, value))
                        .expect("known proxy oracle key")
                })
                .collect::<BTreeMap<_, _>>();
            let status = if ProxyEnvironmentStore::new(temporary.path())
                .write(&changes)
                .is_ok()
            {
                "ok"
            } else {
                "error"
            };
            let persisted = fs::read_to_string(path).expect("EP-009 proxy oracle result");
            format!(
                "proxy-fs|{status}|{}",
                serde_json::to_string(&persisted).expect("serialize proxy filesystem delta")
            )
        }
        Event::Mcp { mcp, connectors } => {
            overlay_observation("mcp", &mcp_overlay(&mcp, &connectors))
        }
        Event::McpDetail {
            mcp,
            connectors,
            source,
            source_kind,
        } => {
            let auth_state = connectors
                .pointer("/connectors/sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|candidate| {
                    candidate
                        .get("id")
                        .or_else(|| candidate.get("alias"))
                        .or_else(|| candidate.get("name"))
                        .and_then(Value::as_str)
                        == Some(source.as_str())
                })
                .and_then(|candidate| candidate.get("authState"))
                .and_then(Value::as_str);
            let target = crate::tui::interaction::IntegrationTarget {
                kind: integration_kind(&source_kind),
                source,
                tool: None,
                enabled: true,
                requires_auth: auth_state == Some("disconnected"),
                requires_setup: auth_state == Some("setup_required"),
            };
            overlay_observation(
                "mcp-detail",
                &crate::tui::pickers::mcp_detail_overlay(&mcp, &connectors, &target),
            )
        }
        Event::McpAuth {
            source,
            source_kind,
            url,
        } => {
            let kind = integration_kind(&source_kind);
            if !valid_auth_url(&url) {
                return "error:unsafe authentication URL".to_owned();
            }
            overlay_observation("auth", &mcp_auth_overlay(kind, &source, &url, false))
        }
        Event::AuthAction {
            source,
            source_kind,
            url,
            action,
        } => {
            let kind = integration_kind(&source_kind);
            let action = AuthAction {
                kind,
                source,
                url,
                action: auth_action_kind(&action),
                enable_on_complete: false,
            };
            effect_observation(reduce_auth_action(&action))
        }
        Event::Projects { view, .. } => {
            overlay_observation("projects", &remote_projects_overlay(&view))
        }
        Event::Loops { loops, now_seconds } => {
            format_loop_list(&loops, now_seconds).unwrap_or_else(|error| format!("error:{error}"))
        }
        Event::Teleport { event } => teleport_push_overlay(&event)
            .filter(|_| event.get("kind").and_then(Value::as_str) == Some("push_required"))
            .map_or_else(
                || {
                    super::super::teleport_event_message(Some(&event))
                        .map(|(message, status)| format!("{status:?}:{message}"))
                        .unwrap_or_else(|error| format!("error:{error}"))
                },
                |overlay| overlay_observation("teleport-approval", &overlay),
            ),
    }
}

fn integration_kind(value: &str) -> IntegrationKind {
    match value {
        "server" => IntegrationKind::McpServer,
        "connector" => IntegrationKind::Connector,
        value => panic!("unknown integration kind `{value}`"),
    }
}

fn auth_action_kind(value: &str) -> AuthActionKind {
    match value {
        "open" => AuthActionKind::Open,
        "copy" => AuthActionKind::Copy,
        "show" => AuthActionKind::Show,
        "refresh" => AuthActionKind::Refresh,
        "logout" => AuthActionKind::Logout,
        "close" => AuthActionKind::Close,
        value => panic!("unknown authentication action `{value}`"),
    }
}

fn effect_observation(effect: McpEffect) -> String {
    let mut calls = Vec::new();
    let observation = match effect {
        McpEffect::OpenUrl {
            kind, source, url, ..
        } => {
            calls.push(format!("url/open:{url}"));
            typed_effect("open_url", kind, &source, Some(&url))
        }
        McpEffect::CopyUrl {
            kind, source, url, ..
        } => {
            calls.push(format!("clipboard/write:{url}"));
            typed_effect("copy_url", kind, &source, Some(&url))
        }
        McpEffect::ShowUrl {
            kind, source, url, ..
        } => {
            calls.push(format!("transcript/write:{source}"));
            typed_effect("show_url", kind, &source, Some(&url))
        }
        McpEffect::Refresh { kind, source } => {
            calls.push(format!(
                "{}/refresh:{source}",
                integration_method_prefix(kind)
            ));
            typed_effect("refresh", kind, &source, None)
        }
        McpEffect::CompleteAuthentication {
            kind,
            source,
            enable_source,
        } => {
            match kind {
                IntegrationKind::Connector => calls.push(format!("connectors/refresh:{source}")),
                IntegrationKind::McpServer => {
                    calls.push(format!("mcp/auth/complete:{source}"));
                    if enable_source {
                        calls.push(format!("mcp/toggle:{source}"));
                    }
                }
            }
            typed_effect("complete_auth", kind, &source, None)
        }
        McpEffect::Logout { source } => {
            calls.push(format!("mcp/logout:{source}"));
            format!("effect|logout|server|{source}")
        }
        McpEffect::Close => {
            calls.push("overlay/close".to_owned());
            "effect|close|-|-".to_owned()
        }
        McpEffect::Show { .. }
        | McpEffect::ShowDetail { .. }
        | McpEffect::BeginAuth { .. }
        | McpEffect::SetEnabled { .. }
        | McpEffect::Status
        | McpEffect::Add { .. } => {
            panic!("EP-009 authentication fixtures must reduce to an authentication effect")
        }
    };
    format!("{observation}|calls={}", calls.join(","))
}

fn integration_method_prefix(kind: IntegrationKind) -> &'static str {
    match kind {
        IntegrationKind::McpServer => "mcp",
        IntegrationKind::Connector => "connectors",
    }
}

fn typed_effect(name: &str, kind: IntegrationKind, source: &str, url: Option<&str>) -> String {
    let kind = match kind {
        IntegrationKind::McpServer => "server",
        IntegrationKind::Connector => "connector",
    };
    format!("effect|{name}|{kind}|{source}|{}", url.unwrap_or("-"))
}

fn overlay_observation(label: &str, overlay: &Overlay) -> String {
    format!(
        "{label}|{}|{}|{}",
        overlay.title,
        overlay.notice.as_deref().unwrap_or("-"),
        overlay
            .items
            .iter()
            .map(|item| format!(
                "{}:{}:{}",
                item.label,
                if item.disabled { "disabled" } else { "enabled" },
                item.description
            ))
            .collect::<Vec<_>>()
            .join(";")
    )
}

#[test]
fn corpus_replays_presentations_and_typed_auth_effects() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../../tests/runtime-parity/configuration-integrations-ep009.json"
    ))
    .expect("strict EP-009 corpus");
    assert_eq!(corpus.schema_version, 5);
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.oracle.engine, "python-textual-capture");
    assert_eq!(corpus.oracle.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.oracle.deterministic_runs, 10);
    let python_expected = assert_python_oracle_probe(&corpus.oracle_probe);
    assert!(!corpus.reference.version.is_empty());
    assert_eq!(corpus.reference.source_files.len(), 11);
    for entry in &corpus.unavailable {
        assert!(
            !entry.dimension.is_empty() && !entry.reason.is_empty(),
            "an unavailable dimension must name itself and say why"
        );
    }
    assert_eq!(
        corpus
            .traces
            .iter()
            .map(|trace| trace.story.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["US-031", "US-032", "US-033"])
    );

    for trace in corpus.traces {
        assert!(!trace.id.is_empty());
        assert_eq!(
            trace.events.len(),
            trace.expected.len(),
            "trace {} has an incomplete expectation",
            trace.id
        );
        if let Some(python_expected) = &python_expected {
            let captured = python_expected
                .get(&trace.id)
                .unwrap_or_else(|| panic!("Python oracle omitted trace {}", trace.id));
            assert_eq!(
                &trace.reference, captured,
                "checked-in reference for {} drifted from the pinned Python oracle",
                trace.id
            );
        }
        let declared = trace
            .divergences
            .iter()
            .map(|divergence| {
                assert!(
                    !divergence.reason.is_empty(),
                    "trace {} declares a divergence without a reason",
                    trace.id
                );
                divergence.event
            })
            .collect::<BTreeSet<_>>();
        for (event_index, (event, expected)) in
            trace.events.into_iter().zip(&trace.expected).enumerate()
        {
            for run in 0..corpus.oracle.deterministic_runs {
                assert_eq!(
                    &apply(event.clone()),
                    expected,
                    "trace {} diverged at event {event_index} on deterministic run {run}",
                    trace.id
                );
            }
            let matches_reference = trace
                .reference
                .get(event_index)
                .is_some_and(|reference| reference == expected);
            assert_eq!(
                matches_reference,
                !declared.contains(&event_index),
                "trace {} event {event_index}: a departure from the reference must be \
                 declared exactly once, and a declared divergence must be real",
                trace.id
            );
        }
    }
}

fn assert_python_oracle_probe(probe: &OracleProbe) -> Option<BTreeMap<String, Vec<String>>> {
    let (observed, traces) = run_python_oracle(probe)?;
    assert_eq!(observed, probe.expected, "Python EP-009 oracle drifted");
    Some(traces)
}

/// Runs the pinned reference oracle and splits its per-trace observations from
/// the standalone probe dimensions. Returns `None` where the pinned checkout is
/// unavailable, which is every machine but the reference workstation.
fn run_python_oracle(probe: &OracleProbe) -> Option<(Value, BTreeMap<String, Vec<String>>)> {
    let (oracle_root, interpreter) = pinned_python_oracle()?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity")
        .join(&probe.script);
    let output = Command::new(interpreter)
        .arg(script)
        .current_dir(&oracle_root)
        .output()
        .expect("execute the pinned Python EP-009 oracle");
    assert!(
        output.status.success(),
        "Python EP-009 oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut observed: Value =
        serde_json::from_slice(&output.stdout).expect("strict Python EP-009 observation");
    let trace_expected = observed
        .as_object_mut()
        .and_then(|observed| observed.remove("traceExpected"))
        .expect("Python EP-009 oracle emitted every trace");
    Some((
        observed,
        serde_json::from_value(trace_expected).expect("strict Python EP-009 trace observations"),
    ))
}

/// Rewrites the checked-in corpus from the current runtime and the pinned
/// reference. Declared divergences are preserved: only a human can justify one.
#[test]
#[ignore = "maintenance: regenerates the EP-009 corpus"]
fn regenerate_corpus() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity")
        .join("configuration-integrations-ep009.json");
    let mut corpus: Value = serde_json::from_str(
        &fs::read_to_string(&path).expect("read the checked-in EP-009 corpus"),
    )
    .expect("strict EP-009 corpus");
    let probe: OracleProbe =
        serde_json::from_value(corpus["oracleProbe"].clone()).expect("strict EP-009 oracle probe");
    let (observed, captured) =
        run_python_oracle(&probe).expect("the pinned Python oracle checkout must be available");
    corpus["oracleProbe"]["expected"] = observed;
    for trace in corpus["traces"]
        .as_array_mut()
        .expect("EP-009 corpus traces")
    {
        let id = trace["id"].as_str().expect("trace id").to_owned();
        let events: Vec<Event> =
            serde_json::from_value(trace["events"].clone()).expect("strict EP-009 events");
        trace["expected"] = Value::from(events.into_iter().map(apply).collect::<Vec<_>>());
        trace["reference"] = Value::from(
            captured
                .get(&id)
                .unwrap_or_else(|| panic!("Python oracle omitted trace {id}"))
                .clone(),
        );
    }
    fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&corpus).expect("serialize the EP-009 corpus")
        ),
    )
    .expect("write the EP-009 corpus");
}

#[test]
fn refresh_and_auth_completion_execute_distinct_effect_contracts() {
    let refresh = effect_observation(McpEffect::Refresh {
        kind: IntegrationKind::McpServer,
        source: "github".to_owned(),
    });
    let completion = effect_observation(McpEffect::CompleteAuthentication {
        kind: IntegrationKind::McpServer,
        source: "github".to_owned(),
        enable_source: true,
    });
    assert_eq!(
        refresh,
        "effect|refresh|server|github|-|calls=mcp/refresh:github"
    );
    assert_eq!(
        completion,
        "effect|complete_auth|server|github|-|calls=mcp/auth/complete:github,mcp/toggle:github"
    );
}

#[test]
fn overlays_preserve_typed_actions_and_render_at_fixed_widths() {
    let auth = mcp_auth_overlay(
        IntegrationKind::Connector,
        "drive",
        "https://auth.example/drive",
        false,
    );
    assert!(auth.items.iter().all(|item| matches!(
        &item.action,
        OverlayAction::Authenticate(action)
            if action.kind == IntegrationKind::Connector && action.source == "drive"
    )));
    let projects = remote_projects_overlay(&serde_json::json!({
        "state": {"projects": [{
            "projectId": "project-1",
            "name": "Parity",
            "repositories": [],
            "isReadOnly": false
        }]}
    }));
    assert!(
        projects
            .items
            .iter()
            .filter(|item| !item.disabled)
            .all(|item| matches!(item.action, OverlayAction::RemoteProject(_))),
        "every selectable picker row carries a typed project action"
    );
    assert!(
        projects.items.iter().any(|item| item.disabled),
        "the picker groups its rows like the reference"
    );

    let overlays = vec![
        (
            config_overlay(
                &serde_json::json!({
                    "config": {"active_model": "codestral"},
                    "selectedTarget": "user",
                    "layerValues": [{
                        "layer": "selected_toml",
                        "values": {"active_model": "codestral"}
                    }]
                }),
                &serde_json::json!({
                    "properties": {
                        "active_model": {"type": "string", "description": "Model"}
                    }
                }),
            ),
            "Settings",
        ),
        (
            mcp_overlay(
                &serde_json::json!({
                    "mcp": {"sources": [{
                        "name": "github",
                        "transport": "streamable-http",
                        "status": "healthy",
                        "enabled": true,
                        "tools": [{"name": "search", "enabled": true}]
                    }]}
                }),
                &serde_json::json!({"connectors": {"sources": []}}),
            ),
            "MCP",
        ),
        (auth, "Authenticate"),
        (
            proxy_overlay(&serde_json::json!({"values": {"HTTP_PROXY": null}})),
            "Proxy",
        ),
        (projects, "Vibe Code project"),
    ];

    for width in [40, 80, 120] {
        for (overlay, expected_title) in &overlays {
            let mut state = TuiState::new("session");
            state.overlay = Some(overlay.clone());
            let backend = TestBackend::new(width, 18);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &mut state,
                        &PromptEditor::default(),
                        &CompletionEngine::default(),
                        InputMode::Prompt,
                        resolve_theme(Theme::Dark, DetectedTheme::Dark, true),
                        UiContext {
                            cwd: Path::new("/workspace"),
                            agent_name: "default",
                            secret_input: false,
                            safety: Safety::Neutral,
                            switching: false,
                            feedback_active: false,
                            voice_phase: VoicePhase::Disabled,
                            voice_indicator: 0,
                            banner: BannerContext {
                                version: "test",
                                model: "model",
                                thinking: "off",
                                models_count: 1,
                                skills_count: 0,
                                mcp_servers_enabled: 1,
                                mcp_servers_total: 1,
                                connectors_connected: 0,
                                connectors_total: 0,
                                hooks_count: 0,
                                plan: None,
                            },
                            tokens: TokenState::default(),
                        },
                    );
                })
                .expect("EP-009 overlay renders");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                rendered.contains(expected_title),
                "{expected_title} missing at width {width}"
            );
            assert!(
                rendered.contains("Enter"),
                "action hint missing at width {width}"
            );
        }
    }
}
