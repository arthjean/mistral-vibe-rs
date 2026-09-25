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
use crate::tui::command_handlers::format_loop_list;
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
use crate::tui::workflow::{McpEffect, auth_panel_context, reduce_auth_action, scheduled_loop};

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
            let temporary =
                tempfile::tempdir().expect("configuration-integrations proxy oracle directory");
            let path = temporary.path().join(".env");
            fs::write(&path, initial).expect("configuration-integrations proxy oracle fixture");
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
            let persisted =
                fs::read_to_string(path).expect("configuration-integrations proxy oracle result");
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
        } => overlay_observation(
            "auth",
            &mcp_auth_overlay(integration_kind(&source_kind), &source, &url, false, false),
        ),
        Event::AuthAction {
            source,
            source_kind,
            url,
            action,
        } => auth_action_observation(&source_kind, &source, &url, &action),
        Event::Projects { view, .. } => {
            overlay_observation("projects", &remote_projects_overlay(&view))
        }
        Event::Loops { loops, now_seconds } => {
            let parsed = loops
                .as_array()
                .and_then(|loops| loops.iter().map(scheduled_loop).collect::<Option<Vec<_>>>());
            parsed.map_or_else(
                || "error:Scheduled-loop list is malformed".to_owned(),
                |loops| format_loop_list(&loops, now_seconds as f64),
            )
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

/// The panel rows the corpus drives, or `None` for an action the panel does
/// not offer.
fn auth_action_kind(value: &str) -> Option<AuthActionKind> {
    match value {
        "open" => Some(AuthActionKind::Open),
        "copy" => Some(AuthActionKind::Copy),
        "show" => Some(AuthActionKind::Show),
        "refresh" => Some(AuthActionKind::Refresh),
        "close" => Some(AuthActionKind::Close),
        _ => None,
    }
}

/// One action on a freshly opened authentication panel, observed as the
/// oracle observes the reference panel: what the panel shows below its rows
/// afterward, and every call it made in the oracle's order (the URL opened,
/// the text copied, the sign-in client, then the message the panel posted).
///
/// The panel exists because its sign-in began: a server login
/// (`McpEffect::BeginAuth`, whose URL opened the panel) or a connector's URL
/// fetch. That call is the one the oracle's stub client records on mount.
fn auth_action_observation(source_kind: &str, source: &str, url: &str, action: &str) -> String {
    let Some(action_kind) = auth_action_kind(action) else {
        return format!("reference|{action}|unsupported");
    };
    let kind = integration_kind(source_kind);
    let mut state = TuiState::new("session");
    state.overlay = Some(mcp_auth_overlay(kind, source, url, false, false));
    let panel = auth_panel_context(&state).expect("the panel carries its context");
    let effect = reduce_auth_action(&AuthAction {
        action: action_kind,
        ..panel
    });
    let mut effects = Vec::new();
    let mut client = vec![match kind {
        IntegrationKind::McpServer => format!("mcp/login:{source}"),
        IntegrationKind::Connector => format!("connectors/auth-url:{source}"),
    }];
    let mut messages = Vec::new();
    match effect {
        None => {}
        Some(McpEffect::OpenUrl { url }) => effects.push(format!("url/open:{url}")),
        Some(McpEffect::CopyUrl { url }) => effects.push(format!("clipboard/write:{url}")),
        Some(McpEffect::ToggleUrl { action }) => {
            state.overlay = Some(mcp_auth_overlay(
                action.kind,
                &action.source,
                &action.url,
                action.enable_on_complete,
                !action.url_visible,
            ));
        }
        Some(McpEffect::CheckConnector { source }) => {
            client.push(format!("connectors/refresh:{source}"));
        }
        Some(McpEffect::Show { filter: None }) => messages.push(match kind {
            IntegrationKind::McpServer => "MCPOAuthClosed(refreshed=False,server_name=)",
            IntegrationKind::Connector => "ConnectorAuthClosed(refreshed=False,connector_name=)",
        }),
        Some(other) => panic!("the authentication panel reduced `{action}` to {other:?}"),
    }
    let detail = state
        .overlay
        .as_ref()
        .and_then(|overlay| overlay.notice.as_deref())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let calls = effects
        .into_iter()
        .chain(client)
        .chain(messages.into_iter().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    format!(
        "reference|{action}|{source_kind}|{source}|detail={detail}|calls={}",
        calls.join(",")
    )
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
        "../../../tests/runtime-parity/configuration-integrations.json"
    ))
    .expect("strict configuration-integrations corpus");
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
    assert_eq!(
        observed, probe.expected,
        "Python configuration-integrations oracle drifted"
    );
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
        .expect("execute the pinned Python configuration-integrations oracle");
    assert!(
        output.status.success(),
        "Python configuration-integrations oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut observed: Value = serde_json::from_slice(&output.stdout)
        .expect("strict Python configuration-integrations observation");
    let trace_expected = observed
        .as_object_mut()
        .and_then(|observed| observed.remove("traceExpected"))
        .expect("Python configuration-integrations oracle emitted every trace");
    Some((
        observed,
        serde_json::from_value(trace_expected)
            .expect("strict Python configuration-integrations trace observations"),
    ))
}

/// Rewrites the checked-in corpus from the current runtime and the pinned
/// reference. Declared divergences are preserved: only a human can justify one.
#[test]
#[ignore = "maintenance: regenerates the configuration-integrations corpus"]
fn regenerate_corpus() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity")
        .join("configuration-integrations.json");
    let mut corpus: Value = serde_json::from_str(
        &fs::read_to_string(&path).expect("read the checked-in configuration-integrations corpus"),
    )
    .expect("strict configuration-integrations corpus");
    let probe: OracleProbe = serde_json::from_value(corpus["oracleProbe"].clone())
        .expect("strict configuration-integrations oracle probe");
    let (observed, captured) =
        run_python_oracle(&probe).expect("the pinned Python oracle checkout must be available");
    corpus["oracleProbe"]["expected"] = observed;
    for trace in corpus["traces"]
        .as_array_mut()
        .expect("configuration-integrations corpus traces")
    {
        let id = trace["id"].as_str().expect("trace id").to_owned();
        let events: Vec<Event> = serde_json::from_value(trace["events"].clone())
            .expect("strict configuration-integrations events");
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
            serde_json::to_string_pretty(&corpus)
                .expect("serialize the configuration-integrations corpus")
        ),
    )
    .expect("write the configuration-integrations corpus");
}

#[test]
fn overlays_preserve_typed_actions_and_render_at_fixed_widths() {
    let auth = mcp_auth_overlay(
        IntegrationKind::Connector,
        "drive",
        "https://auth.example/drive",
        false,
        false,
    );
    assert!(
        auth.items
            .iter()
            .filter(|item| !item.disabled)
            .all(|item| matches!(
                &item.action,
                OverlayAction::Authenticate(action)
                    if action.kind == IntegrationKind::Connector && action.source == "drive"
            ))
    );
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
        (auth, "Connector: drive"),
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
                .expect("configuration-integrations overlay renders");
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
