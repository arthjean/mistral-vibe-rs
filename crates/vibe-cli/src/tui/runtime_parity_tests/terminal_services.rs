use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;
use vibe_core::updates::{
    CachePlan, UpdateAvailability, UpdateCache, UpdateError, UpdateGatewayCause,
    UpdateGatewayError, Version, plan_update_check, resolve_fetch, select_latest_version,
    status_cause,
};

use super::{REFERENCE_COMMIT, Reference, pinned_python_oracle};
use crate::tui::attention::{
    AttentionNotifier, DEFAULT_TITLE, NotificationContext, NotificationPolicy, THROTTLE_MS,
};
use crate::tui::exit::{
    SUSPEND_MESSAGE, SessionExitSummary, SessionUsage, format_session_usage, quit_prompt,
    session_resume_lines, shorten_session_id,
};
use crate::tui::interaction::QUIT_CONFIRMATION_WINDOW_MS;
use crate::tui::narrator::{NarratorEffect, NarratorManager};
use crate::tui::setup::Theme;
use crate::tui::themes::{AUTO_THEME, sorted_theme_names, theme_polarity};
use crate::tui::updates::{
    CACHE_WRITE_FAILED_MESSAGE, UPDATE_DIALOG_TITLE, UpdateChoice, UpdatePromptMode,
    check_failed_message, up_to_date_message, update_failed_message, version_line,
};

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
    UpdateCheck {
        #[serde(rename = "currentVersion")]
        current_version: String,
        now: i64,
        #[serde(default)]
        force: bool,
        cache: Option<CachePayload>,
        fetch: FetchOutcome,
    },
    PendingUpdate {
        #[serde(rename = "currentVersion")]
        current_version: String,
        cache: Option<CachePayload>,
        #[serde(default)]
        dismiss: Option<String>,
    },
    WhatsNew {
        #[serde(rename = "currentVersion")]
        current_version: String,
        cache: Option<CachePayload>,
    },
    VersionOrder {
        left: String,
        right: String,
    },
    Pypi {
        payload: Value,
        #[serde(default = "ok_status")]
        status: u16,
    },
    GatewayMessage {
        cause: String,
    },
    CheckUpgradeOutput {
        outcome: String,
        #[serde(rename = "currentVersion")]
        current_version: String,
        #[serde(default)]
        reason: Option<String>,
    },
    UpdateDialog {
        #[serde(rename = "currentVersion")]
        current_version: String,
        #[serde(rename = "latestVersion")]
        latest_version: String,
        mode: String,
    },
    Attention {
        action: String,
        #[serde(default)]
        policy: Option<String>,
        /// Read by the Python oracle, which drives the reference boolean.
        #[allow(dead_code)]
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        context: Option<String>,
        #[serde(default, rename = "nowMs")]
        now_ms: Option<u64>,
    },
    Narrator {
        action: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        summary: Option<String>,
    },
    QuitPrompt {
        key: String,
        queued: usize,
    },
    SessionSummary {
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
        input: u64,
        output: u64,
    },
}

const fn ok_status() -> u16 {
    200
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FetchOutcome {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    cause: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CachePayload {
    latest_version: String,
    stored_at_timestamp: i64,
    #[serde(default)]
    seen_whats_new_version: Option<String>,
    #[serde(default)]
    dismissed_version: Option<String>,
}

impl From<&CachePayload> for UpdateCache {
    fn from(payload: &CachePayload) -> Self {
        Self {
            latest_version: payload.latest_version.clone(),
            stored_at_timestamp: payload.stored_at_timestamp,
            seen_whats_new_version: payload.seen_whats_new_version.clone(),
            dismissed_version: payload.dismissed_version.clone(),
        }
    }
}

fn cause_label(cause: UpdateGatewayCause) -> &'static str {
    match cause {
        UpdateGatewayCause::TooManyRequests => "too_many_requests",
        UpdateGatewayCause::Forbidden => "forbidden",
        UpdateGatewayCause::NotFound => "not_found",
        UpdateGatewayCause::RequestFailed => "request_failed",
        UpdateGatewayCause::ErrorResponse => "error_response",
        UpdateGatewayCause::InvalidResponse => "invalid_response",
        UpdateGatewayCause::Unknown => "unknown",
    }
}

fn parse_cause(label: &str) -> UpdateGatewayCause {
    match label {
        "too_many_requests" => UpdateGatewayCause::TooManyRequests,
        "forbidden" => UpdateGatewayCause::Forbidden,
        "not_found" => UpdateGatewayCause::NotFound,
        "request_failed" => UpdateGatewayCause::RequestFailed,
        "error_response" => UpdateGatewayCause::ErrorResponse,
        "invalid_response" => UpdateGatewayCause::InvalidResponse,
        _ => UpdateGatewayCause::Unknown,
    }
}

fn encode_cache(cache: Option<&UpdateCache>) -> String {
    cache.map_or_else(
        || "none".to_owned(),
        |cache| {
            format!(
                "{}|{}|{}|{}",
                cache.latest_version,
                cache.stored_at_timestamp,
                cache.seen_whats_new_version.as_deref().unwrap_or("-"),
                cache.dismissed_version.as_deref().unwrap_or("-"),
            )
        },
    )
}

fn availability_label(availability: &UpdateAvailability) -> String {
    format!(
        "available|{}|notify={}",
        availability.latest_version,
        u8::from(availability.should_notify)
    )
}

/// Replays one trace. Attention and narration are sequences, so their state
/// lives for the whole trace exactly as it does in the reference.
#[derive(Default)]
struct Replay {
    notifier: Option<AttentionAccumulator>,
    narrator: Option<NarratorReplay>,
}

#[derive(Default)]
struct AttentionAccumulator {
    notifier: AttentionNotifier,
    bells: usize,
    writes: Vec<String>,
}

impl AttentionAccumulator {
    fn record(&mut self, effect: &crate::tui::attention::AttentionEffect) {
        if effect.bell {
            self.bells += 1;
        }
        // The reference rings the bell through the app and writes only the
        // title escape through the driver.
        self.writes.push(format!("\u{1b}]0;{}\u{7}", effect.title));
    }

    fn observation(&self) -> String {
        format!(
            "attention|bells={}|writes={}",
            self.bells,
            self.writes.join("⏎")
        )
    }
}

struct NarratorReplay {
    narrator: NarratorManager,
    generation: Option<u64>,
    summary: Option<String>,
}

impl Default for NarratorReplay {
    fn default() -> Self {
        Self {
            narrator: NarratorManager::new(true, true),
            generation: None,
            summary: None,
        }
    }
}

impl Replay {
    fn apply(&mut self, event: Event) -> String {
        match event {
            Event::UpdateCheck {
                current_version,
                now,
                force,
                cache,
                fetch,
            } => observe_update_check(&current_version, now, force, cache.as_ref(), &fetch),
            Event::PendingUpdate {
                current_version,
                cache,
                dismiss,
            } => {
                let mut cache = cache.as_ref().map(UpdateCache::from);
                let mut pending =
                    vibe_core::updates::pending_update_from_cache(cache.as_ref(), &current_version);
                if let Some(dismiss) = dismiss {
                    if let Some(dismissed) =
                        vibe_core::updates::dismiss_update(cache.as_ref(), &dismiss)
                    {
                        cache = Some(dismissed);
                    }
                    pending = vibe_core::updates::pending_update_from_cache(
                        cache.as_ref(),
                        &current_version,
                    );
                }
                format!(
                    "pending|{}|cache={}",
                    pending.unwrap_or_else(|| "none".to_owned()),
                    encode_cache(cache.as_ref())
                )
            }
            Event::WhatsNew {
                current_version,
                cache,
            } => {
                let mut cache = cache.as_ref().map(UpdateCache::from);
                let shown =
                    vibe_core::updates::should_show_whats_new(cache.as_ref(), &current_version);
                if shown {
                    cache = Some(vibe_core::updates::mark_version_as_seen(
                        cache.as_ref(),
                        &current_version,
                        // The reference only stamps a timestamp when no cache
                        // exists, and it never shows notes without one.
                        0,
                    ));
                }
                format!(
                    "whatsnew|{}|cache={}",
                    u8::from(shown),
                    encode_cache(cache.as_ref())
                )
            }
            Event::VersionOrder { left, right } => {
                match (Version::parse(&left), Version::parse(&right)) {
                    (Some(left), Some(right)) => {
                        let order = match left.cmp(&right) {
                            std::cmp::Ordering::Less => "<",
                            std::cmp::Ordering::Equal => "==",
                            std::cmp::Ordering::Greater => ">",
                        };
                        format!("version|{order}|00")
                    }
                    (left, right) => format!(
                        "version|invalid|{}{}",
                        u8::from(left.is_none()),
                        u8::from(right.is_none())
                    ),
                }
            }
            Event::Pypi { payload, status } => match status_cause(status) {
                Some(cause) => format!("pypi|error|{}", cause_label(cause)),
                None => format!(
                    "pypi|{}",
                    select_latest_version(&payload).unwrap_or_else(|| "none".to_owned())
                ),
            },
            Event::GatewayMessage { cause } => {
                format!("gateway|{}", parse_cause(&cause).default_message())
            }
            Event::CheckUpgradeOutput {
                outcome,
                current_version,
                reason,
            } => {
                let message = match outcome.as_str() {
                    "up_to_date" => up_to_date_message(&current_version),
                    "check_failed" => check_failed_message(reason.as_deref().unwrap_or_default()),
                    "cache_failed" => CACHE_WRITE_FAILED_MESSAGE.to_owned(),
                    "update_failed" => update_failed_message(&current_version),
                    other => panic!("unknown check-upgrade outcome {other}"),
                };
                format!("output|{}", message.replace('\n', "⏎"))
            }
            Event::UpdateDialog {
                current_version,
                latest_version,
                mode,
            } => {
                let mode = match mode.as_str() {
                    "startup" => UpdatePromptMode::Startup,
                    "check_upgrade" => UpdatePromptMode::CheckUpgrade,
                    other => panic!("unknown update prompt mode {other}"),
                };
                let labels = UpdateChoice::ORDER
                    .iter()
                    .map(|choice| choice.label(mode))
                    .collect::<Vec<_>>()
                    .join("|");
                format!(
                    "dialog|{UPDATE_DIALOG_TITLE}|{}|{labels}",
                    version_line(&current_version, &latest_version)
                )
            }
            Event::Attention {
                action,
                policy,
                context,
                now_ms,
                ..
            } => {
                let accumulator = self
                    .notifier
                    .get_or_insert_with(AttentionAccumulator::default);
                match action.as_str() {
                    "policy" => accumulator
                        .notifier
                        .set_policy(NotificationPolicy::from_config(policy.as_deref())),
                    "blur" => accumulator.notifier.on_blur(),
                    "focus" => {
                        if let Some(effect) = accumulator.notifier.on_focus() {
                            accumulator.record(&effect);
                        }
                    }
                    "notify" => {
                        let context = match context.as_deref() {
                            Some("action_required") => NotificationContext::ActionRequired,
                            Some("complete") => NotificationContext::Complete,
                            other => panic!("unknown notification context {other:?}"),
                        };
                        if let Some(effect) = accumulator
                            .notifier
                            .notify(context, now_ms.unwrap_or_default())
                        {
                            accumulator.record(&effect);
                        }
                    }
                    other => panic!("unknown attention action {other}"),
                }
                accumulator.observation()
            }
            Event::Narrator {
                action,
                text,
                id,
                message,
                summary,
            } => {
                let replay = self.narrator.get_or_insert_with(NarratorReplay::default);
                match action.as_str() {
                    "turn_start" => {
                        replay.narrator.cancel();
                        replay.narrator.on_turn_start(text.as_deref().unwrap_or(""));
                    }
                    "user_message" => replay
                        .narrator
                        .on_user_message(id.as_deref().unwrap_or_default()),
                    "assistant_text" => replay
                        .narrator
                        .on_assistant_text(text.as_deref().unwrap_or_default()),
                    "turn_error" => replay
                        .narrator
                        .on_turn_error(message.as_deref().unwrap_or_default()),
                    "turn_cancel" => replay.narrator.on_turn_cancel(),
                    "turn_end" => {
                        replay.summary = summary;
                        replay.generation = match replay.narrator.on_turn_end() {
                            Some(NarratorEffect::Summarize { generation, .. }) => Some(generation),
                            _ => None,
                        };
                    }
                    "settle" => {
                        if let Some(generation) = replay.generation {
                            let summary = replay.summary.clone();
                            if let Some(NarratorEffect::Speak { generation, .. }) =
                                replay.narrator.apply_summary(generation, summary)
                            {
                                replay.narrator.playback_started(generation);
                            }
                        }
                    }
                    "playback_finished" => {
                        if let Some(generation) = replay.generation {
                            replay.narrator.settle(generation);
                        }
                    }
                    "cancel" => {
                        replay.narrator.cancel();
                        replay.generation = None;
                    }
                    "disable" => {
                        replay.narrator.sync(false, false);
                        replay.generation = None;
                    }
                    other => panic!("unknown narrator action {other}"),
                }
                format!("narrator|{}", replay.narrator.state().label())
            }
            Event::QuitPrompt { key, queued } => format!("quit|{}", quit_prompt(&key, queued)),
            Event::SessionSummary {
                session_id,
                input,
                output,
            } => {
                let summary = SessionExitSummary {
                    session_id,
                    usage: SessionUsage {
                        input_tokens: input,
                        output_tokens: output,
                    },
                };
                format!("summary|{}", session_resume_lines(&summary).join("⏎"))
            }
        }
    }
}

/// Reference `get_update_if_available` over an in-memory repository.
fn observe_update_check(
    current_version: &str,
    now: i64,
    force: bool,
    cache: Option<&CachePayload>,
    fetch: &FetchOutcome,
) -> String {
    let cache = cache.map(UpdateCache::from);
    let (outcome, cache) = match plan_update_check(force, cache.as_ref(), current_version, now) {
        CachePlan::Unversioned | CachePlan::Cached(None) => ("none".to_owned(), cache),
        CachePlan::Cached(Some(availability)) => (availability_label(&availability), cache),
        CachePlan::Fetch => {
            let result = match (&fetch.cause, &fetch.version) {
                (Some(cause), _) => Err(UpdateGatewayError::new(parse_cause(cause))),
                (None, version) => Ok(version.clone()),
            };
            let resolution = resolve_fetch(result, cache.as_ref(), current_version, now);
            let outcome = match (&resolution.error, &resolution.availability) {
                (Some(UpdateError::Gateway(message)), _) => format!("error|{message}"),
                (Some(UpdateError::CacheWrite), _) => {
                    "error|update cache could not be written".to_owned()
                }
                (None, Some(availability)) => availability_label(availability),
                (None, None) => "none".to_owned(),
            };
            (outcome, resolution.cache_write.or(cache))
        }
    };
    format!("update|{outcome}|cache={}", encode_cache(cache.as_ref()))
}

#[test]
fn corpus_replays_update_attention_narration_and_exit_behavior() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../../tests/runtime-parity/terminal-services.json"
    ))
    .expect("strict terminal-services corpus");
    assert_eq!(corpus.schema_version, 1);
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.oracle.engine, "python-reference-capture");
    assert_eq!(corpus.oracle.commit, REFERENCE_COMMIT);
    assert_eq!(corpus.oracle.deterministic_runs, 10);
    assert!(!corpus.reference.version.is_empty());
    assert_eq!(corpus.reference.source_files.len(), 12);
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
        BTreeSet::from(["US-037", "US-038", "US-039"])
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
        for run in 0..corpus.oracle.deterministic_runs {
            let mut replay = Replay::default();
            for (event_index, (event, expected)) in trace
                .events
                .iter()
                .cloned()
                .zip(&trace.expected)
                .enumerate()
            {
                assert_eq!(
                    &replay.apply(event),
                    expected,
                    "trace {} diverged at event {event_index} on deterministic run {run}",
                    trace.id
                );
            }
        }
        for event_index in 0..trace.events.len() {
            let matches_reference = trace
                .reference
                .get(event_index)
                .is_some_and(|reference| Some(reference) == trace.expected.get(event_index));
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
        "Python terminal-services oracle drifted"
    );
    assert_terminal_service_contracts(&observed);
    Some(traces)
}

/// The dimensions the reference exposes as values rather than event sequences.
fn assert_terminal_service_contracts(observed: &Value) {
    let names = observed["themes"]["names"]
        .as_array()
        .expect("reference theme names")
        .iter()
        .map(|name| name.as_str().expect("theme name"))
        .collect::<Vec<_>>();
    assert_eq!(sorted_theme_names(), names, "theme catalog and order");
    assert_eq!(observed["themes"]["auto"].as_str(), Some(AUTO_THEME));
    for (name, dark) in observed["themes"]["dark"]
        .as_object()
        .expect("reference theme polarity")
    {
        let expected = if dark.as_bool().expect("polarity") {
            Theme::Dark
        } else {
            Theme::Light
        };
        assert_eq!(
            theme_polarity(name),
            Some(expected),
            "polarity of the {name} theme"
        );
    }
    assert_eq!(observed["suspendMessage"].as_str(), Some(SUSPEND_MESSAGE));
    assert_eq!(
        observed["quitConfirmDelaySeconds"].as_f64(),
        Some(QUIT_CONFIRMATION_WINDOW_MS as f64 / 1000.0)
    );
    assert_eq!(
        observed["notificationTitles"]["action_required"].as_str(),
        Some(NotificationContext::ActionRequired.title_suffix())
    );
    assert_eq!(
        observed["notificationTitles"]["complete"].as_str(),
        Some(NotificationContext::Complete.title_suffix())
    );
    assert_eq!(
        observed["notificationThrottleSeconds"].as_f64(),
        Some(THROTTLE_MS as f64 / 1000.0)
    );
    assert_eq!(
        observed["usageLine"].as_str(),
        Some(
            format_session_usage(SessionUsage {
                input_tokens: 1_234,
                output_tokens: 567,
            })
            .as_str()
        )
    );
    assert_eq!(
        observed["shortSessionId"].as_str(),
        Some(shorten_session_id("0123456789abcdef").as_str())
    );
    // The reference default title is not configurable.
    assert_eq!(DEFAULT_TITLE, "Vibe");
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
        .expect("execute the pinned Python terminal-services oracle");
    assert!(
        output.status.success(),
        "Python terminal-services oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut observed: Value = serde_json::from_slice(&output.stdout)
        .expect("strict Python terminal-services observation");
    let trace_expected = observed
        .as_object_mut()
        .and_then(|observed| observed.remove("traceExpected"))
        .expect("Python terminal-services oracle emitted every trace");
    Some((
        observed,
        serde_json::from_value(trace_expected)
            .expect("strict Python terminal-services trace observations"),
    ))
}

/// Rewrites the checked-in corpus from the current runtime and the pinned
/// reference. Declared divergences are preserved: only a human can justify one.
#[test]
#[ignore = "maintenance: regenerates the terminal-services corpus"]
fn regenerate_corpus() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity")
        .join("terminal-services.json");
    let mut corpus: Value = serde_json::from_str(
        &fs::read_to_string(&path).expect("read the checked-in terminal-services corpus"),
    )
    .expect("strict terminal-services corpus");
    let probe: OracleProbe = serde_json::from_value(corpus["oracleProbe"].clone())
        .expect("strict terminal-services oracle probe");
    let (observed, captured) =
        run_python_oracle(&probe).expect("the pinned Python oracle checkout must be available");
    corpus["oracleProbe"]["expected"] = observed;
    for trace in corpus["traces"]
        .as_array_mut()
        .expect("terminal-services corpus traces")
    {
        let id = trace["id"].as_str().expect("trace id").to_owned();
        let events: Vec<Event> = serde_json::from_value(trace["events"].clone())
            .expect("strict terminal-services events");
        let mut replay = Replay::default();
        trace["expected"] = Value::from(
            events
                .into_iter()
                .map(|event| replay.apply(event))
                .collect::<Vec<_>>(),
        );
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
            serde_json::to_string_pretty(&corpus).expect("serialize the terminal-services corpus")
        ),
    )
    .expect("write the terminal-services corpus");
}

/// The reference surfaces terminal-services adds to the frame: the theme catalog, the quit
/// prompt in the path display, and the narrator status.
#[test]
fn terminal_service_surfaces_render_at_reference_widths() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::chat_input::{InputMode, Safety, VoicePhase};
    use crate::tui::completion::CompletionEngine;
    use crate::tui::input::PromptEditor;
    use crate::tui::pickers::theme_overlay;
    use crate::tui::render::{BannerContext, TokenState, UiContext, draw};
    use crate::tui::setup::{DetectedTheme, resolve_theme};
    use crate::tui::state::TuiState;

    let mut catalog = TuiState::new("session");
    catalog.overlay = Some(theme_overlay("nord"));

    let mut quitting = TuiState::new("session");
    quitting
        .prompt_queue
        .push(crate::tui::attachments::PromptDraft::text_only("queued"));
    assert_eq!(quitting.prompt_queue.len(), 1);
    assert!(!quitting.quit_confirmation.request("Ctrl+C", 0));

    let mut narrating = TuiState::new("session");
    narrating.narrator = NarratorManager::new(true, true);
    narrating.narrator.on_turn_start("write the parser");
    narrating.narrator.on_turn_end();

    let cases = [
        (&mut catalog, "Select Theme"),
        (&mut quitting, "Press Ctrl+C again to quit"),
        (&mut narrating, "summarizing"),
    ];
    for (state, expected) in cases {
        for width in [40_u16, 80, 120] {
            let backend = TestBackend::new(width, 18);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        state,
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
                                mcp_servers_enabled: 0,
                                mcp_servers_total: 0,
                                connectors_connected: 0,
                                connectors_total: 0,
                                hooks_count: 0,
                                plan: None,
                            },
                            tokens: TokenState::default(),
                        },
                    );
                })
                .expect("terminal-services surface renders");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            let visible = expected
                .chars()
                .take(usize::from(width) - 4)
                .collect::<String>();
            assert!(
                rendered.contains(&visible),
                "{expected:?} missing at width {width}: {rendered}"
            );
        }
    }
}

/// Reference `ThemePreviewed` and `ThemePickerApp.Cancelled`: previews apply
/// immediately and cancellation restores the persisted theme.
#[test]
fn theme_previews_apply_before_acceptance_and_cancel_restores() {
    use crate::tui::preview_theme;
    use crate::tui::setup::{DetectedTheme, resolve_theme};

    let persisted = "catppuccin-latte";
    let mut theme = resolve_theme(
        theme_polarity(persisted).expect("persisted theme"),
        DetectedTheme::Dark,
        true,
    );
    assert_eq!(theme.theme, Theme::Light);
    assert!(!theme.colors_enabled, "NO_COLOR survives every preview");

    preview_theme("nord", &mut theme);
    assert_eq!(theme.theme, Theme::Dark, "a highlighted theme previews");
    assert!(!theme.colors_enabled);

    preview_theme("not-a-theme", &mut theme);
    assert_eq!(
        theme.theme,
        Theme::Dark,
        "an unknown name never changes the frame"
    );

    // Cancelling re-applies whatever the configuration still holds.
    preview_theme(persisted, &mut theme);
    assert_eq!(theme.theme, Theme::Light);

    // The automatic entry follows the detected terminal appearance.
    preview_theme(AUTO_THEME, &mut theme);
    assert_eq!(theme.theme, Theme::Dark);
}
