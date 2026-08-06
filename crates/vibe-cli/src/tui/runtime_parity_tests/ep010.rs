use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_app_server::client::{EffectDetail, EffectResultDisplay};

use super::{REFERENCE_COMMIT, Reference, pinned_python_oracle};
use crate::tui::diagnostics::{
    Activity, classify, debug_log_line, log_level_colour, safe_link_spans,
};
use crate::tui::render::{TokenState, format_context_progress};
use crate::tui::state::{EntryStatus, TranscriptEntry, TranscriptKind};
use crate::tui::transcript::{EffectLayout, EffectRegion, Region, region};

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
    Effect {
        tool: String,
        #[serde(default)]
        arguments: Value,
        outcome: Outcome,
    },
    Hook {
        #[serde(rename = "hookName")]
        hook_name: String,
        content: String,
        severity: String,
    },
    TurnError {
        #[serde(default)]
        code: Option<String>,
        message: String,
        #[serde(default)]
        details: Value,
        #[serde(default, rename = "upgradeAvailable")]
        upgrade_available: bool,
    },
    Context {
        #[serde(rename = "maxTokens")]
        max_tokens: u64,
        #[serde(rename = "currentTokens")]
        current_tokens: u64,
    },
    Activity {
        status: String,
        #[serde(rename = "elapsedSeconds")]
        elapsed_seconds: u64,
        #[serde(default)]
        queued: usize,
    },
    DebugLog {
        #[allow(dead_code)]
        id: String,
        timestamp: u64,
        level: String,
        message: String,
    },
    Link {
        text: String,
        url: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Outcome {
    Pending,
    Running {
        #[serde(default, rename = "outputText")]
        output_text: String,
    },
    Blocked {
        #[serde(default, rename = "callbackId")]
        callback_id: Option<String>,
    },
    Failed {
        error: String,
    },
    Skipped {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    Completed {
        #[serde(default)]
        result: Option<Value>,
    },
}

fn apply(event: Event) -> String {
    match event {
        Event::Effect {
            tool,
            arguments,
            outcome,
        } => observe_effect(&tool, &arguments, &outcome),
        Event::Hook {
            hook_name,
            content,
            severity,
        } => {
            let entry = notice_entry(
                &content,
                json!({"kind": "hook_completed", "hookName": hook_name, "content": content, "status": severity}),
                "info",
            );
            match region(&entry) {
                Region::Hook { icon, line } => format!("hook|{icon}|{line}"),
                other => panic!("expected a hook region, got {other:?}"),
            }
        }
        Event::TurnError {
            code,
            message,
            details,
            upgrade_available,
        } => {
            let classified = classify(code.as_deref(), &message, &details, upgrade_available);
            format!("error|{}", classified.message.replace('\n', "⏎"))
        }
        Event::Context {
            max_tokens,
            current_tokens,
        } => format!(
            "context|{}",
            format_context_progress(TokenState {
                max_tokens,
                current_tokens,
            })
        ),
        Event::Activity {
            status,
            elapsed_seconds,
            queued,
        } => {
            let activity = Activity::new(status, elapsed_seconds, queued);
            format!(
                "activity|{}|{}|{}",
                activity.status,
                crate::tui::diagnostics::format_elapsed(elapsed_seconds),
                activity.hint()
            )
        }
        Event::DebugLog {
            timestamp,
            level,
            message,
            ..
        } => format!(
            "log|{}|{}",
            log_level_colour(&level),
            debug_log_line(timestamp, &level, &message)
        ),
        Event::Link { text, url } => format!(
            "link|safe={}|spans={}",
            u8::from(crate::tui::diagnostics::is_safe_url(&url)),
            safe_link_spans(&text).len()
        ),
    }
}

fn notice_entry(text: &str, detail: Value, level: &str) -> TranscriptEntry {
    TranscriptEntry {
        id: "notice".to_owned(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: text.to_owned(),
        status: EntryStatus::Completed,
        details: json!({"type": "notice", "level": level, "detail": detail}),
    }
}

fn observe_effect(tool: &str, arguments: &Value, outcome: &Outcome) -> String {
    // The detail and the settled display are what the app server publishes with
    // the entry, so the observation renders exactly the strings a reference
    // client would receive rather than deriving its own from the arguments.
    let detail = EffectDetail::for_call(tool, arguments);
    let entry = TranscriptEntry {
        id: "effect".to_owned(),
        revision: 1,
        kind: TranscriptKind::Effect,
        text: tool.to_owned(),
        status: EntryStatus::Streaming,
        details: json!({
            "type": "effect",
            "detail": detail,
            "state": effect_state(&detail, outcome),
        }),
    };
    let effect = match region(&entry) {
        Region::Effect(effect) => effect,
        other => panic!("expected an effect region, got {other:?}"),
    };
    format!(
        "effect|{}|{}|{}|{}|{}|collapsible={}|grouped={}|body={}",
        effect.kind.label(),
        indicator_label(&effect),
        effect.verb,
        effect.message,
        effect.suffix,
        u8::from(effect.kind.collapses()),
        u8::from(effect.kind.joins_tool_group()),
        effect
            .body
            .iter()
            // The reference drops empty rows from its composed body, so the
            // observation compares content, not spacing.
            .filter(|line| !line.text.is_empty())
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("⏎")
    )
}

fn indicator_label(effect: &EffectRegion) -> &'static str {
    match effect.indicator {
        crate::tui::transcript::Indicator::Running => "running",
        crate::tui::transcript::Indicator::Success => "success",
        crate::tui::transcript::Indicator::Error => "error",
        crate::tui::transcript::Indicator::Muted => "muted",
    }
}

/// Rebuilds the canonical effect state the app server would project, so both
/// sides read the same nested contract.
fn effect_state(detail: &EffectDetail, outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Pending => json!({"status": "pending"}),
        Outcome::Running { output_text } => {
            json!({"status": "running", "outputText": output_text})
        }
        Outcome::Blocked { callback_id } => json!({
            "status": "blocked",
            "callbackId": callback_id.clone().unwrap_or_else(|| "callback-1".to_owned()),
            "outputText": "",
        }),
        Outcome::Failed { error } => json!({
            "status": "failed",
            "error": {"message": error},
            "outputText": error,
            "durationMs": 0,
            "display": EffectResultDisplay::failed(&detail.display),
        }),
        Outcome::Skipped { reason } => json!({
            "status": "skipped",
            "reason": reason,
            "display": EffectResultDisplay::skipped(&detail.tool_name),
        }),
        Outcome::Cancelled { reason } => json!({
            "status": "cancelled",
            "reason": reason,
            "outputText": "",
            "durationMs": 0,
            "display": EffectResultDisplay::skipped(&detail.tool_name),
        }),
        Outcome::Completed { result } => {
            // Without a result the reference server projects an explicit
            // unsuccessful display, which stays authoritative here.
            let emitted = if result.is_none() {
                json!({"success": false, "message": "No result"})
            } else {
                Value::Null
            };
            let output = result.clone().unwrap_or(Value::Null);
            json!({
                "status": "completed",
                "display": EffectResultDisplay::completed(
                    detail.kind,
                    &detail.display,
                    &output,
                    &emitted,
                ),
                "output": output,
                "outputText": "",
                "durationMs": 0,
            })
        }
    }
}

#[test]
fn corpus_replays_semantic_regions_errors_activity_and_diagnostics() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../../tests/runtime-parity/semantic-transcript-ep010.json"
    ))
    .expect("strict EP-010 corpus");
    assert_eq!(corpus.schema_version, 1);
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.oracle.engine, "python-textual-capture");
    assert_eq!(corpus.oracle.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.oracle.deterministic_runs, 10);
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
        BTreeSet::from(["US-034", "US-035", "US-036"])
    );
    let python_expected = assert_python_oracle_probe(&corpus.oracle_probe);

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
    assert_eq!(observed, probe.expected, "Python EP-010 oracle drifted");
    Some(traces)
}

/// Runs the pinned reference oracle and splits its per-trace observations from
/// the standalone probe dimensions. Returns `None` where the pinned checkout is
/// unavailable, which is every machine but the reference workstation.
fn run_python_oracle(probe: &OracleProbe) -> Option<(Value, BTreeMap<String, Vec<String>>)> {
    let oracle_root = pinned_python_oracle()?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity")
        .join(&probe.script);
    let output = Command::new(oracle_root.join(".venv/bin/python"))
        .arg(script)
        .current_dir(&oracle_root)
        .output()
        .expect("execute the pinned Python EP-010 oracle");
    assert!(
        output.status.success(),
        "Python EP-010 oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut observed: Value =
        serde_json::from_slice(&output.stdout).expect("strict Python EP-010 observation");
    let trace_expected = observed
        .as_object_mut()
        .and_then(|observed| observed.remove("traceExpected"))
        .expect("Python EP-010 oracle emitted every trace");
    Some((
        observed,
        serde_json::from_value(trace_expected).expect("strict Python EP-010 trace observations"),
    ))
}

/// Rewrites the checked-in corpus from the current runtime and the pinned
/// reference. Declared divergences are preserved: only a human can justify one.
#[test]
#[ignore = "maintenance: regenerates the EP-010 corpus"]
fn regenerate_corpus() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity")
        .join("semantic-transcript-ep010.json");
    let mut corpus: Value = serde_json::from_str(
        &fs::read_to_string(&path).expect("read the checked-in EP-010 corpus"),
    )
    .expect("strict EP-010 corpus");
    let probe: OracleProbe =
        serde_json::from_value(corpus["oracleProbe"].clone()).expect("strict EP-010 oracle probe");
    let (observed, captured) =
        run_python_oracle(&probe).expect("the pinned Python oracle checkout must be available");
    corpus["oracleProbe"]["expected"] = observed;
    for trace in corpus["traces"]
        .as_array_mut()
        .expect("EP-010 corpus traces")
    {
        let id = trace["id"].as_str().expect("trace id").to_owned();
        let events: Vec<Event> =
            serde_json::from_value(trace["events"].clone()).expect("strict EP-010 events");
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
            serde_json::to_string_pretty(&corpus).expect("serialize the EP-010 corpus")
        ),
    )
    .expect("write the EP-010 corpus");
}
