//! Replays `tests/runtime-parity/tui-interactions.json`, captured by
//! `tui-interactions-oracle.py`: tool-group summaries, the composer's click
//! chain and drags, the transcript's word and line spans, and queue mode.

use std::path::Path;
use std::process::Command;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

use super::{REFERENCE_COMMIT, Reference, pinned_python_oracle};
use crate::tui::chat_input::{ChatInputState, InputEffect, InputEvent, KeyName};
use crate::tui::clipboard_images::ClipboardImageManager;
use crate::tui::controls::ControlState;
use crate::tui::loading::{LoadingAnimation, TARGET_COLORS};
use crate::tui::prompt::PromptContext;
use crate::tui::queue::{
    CONSUMED_HINT, EDIT_HINT, SELECTION_HINT, enter_queue_selection, leave_queue_edit,
    queue_selection_key, submit_queue_edit,
};
use crate::tui::render::group_label;
use crate::tui::runtime::interactive_test_runtime;
use crate::tui::state::{ServerEvent, TranscriptEntry, TuiState};
use crate::tui::transcript::EffectKind;
use crate::tui::transcript_view::{Cell, TranscriptView};

const CORPUS: &str = include_str!("../../../tests/runtime-parity/tui-interactions.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    oracle: String,
    reference: Reference,
    group_header: Vec<GroupHeader>,
    loading_sweep: Vec<LoadingSweep>,
    stream_delta: Vec<StreamTrace>,
    composer: Vec<ComposerTrace>,
    transcript: Vec<TranscriptCase>,
    queue: Vec<QueueTrace>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupHeader {
    kinds: Vec<EffectKind>,
    reasoning: bool,
    running: bool,
    label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadingSweep {
    status: String,
    frames: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamTrace {
    id: String,
    outputs: Vec<Option<String>>,
    reference: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposerTrace {
    id: String,
    text: String,
    steps: Vec<PointerStep>,
    reference: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PointerStep {
    kind: String,
    x: u16,
    y: u16,
    at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptCase {
    lines: Vec<String>,
    spans: Vec<TranscriptSpan>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptSpan {
    x: usize,
    y: usize,
    granularity: String,
    reference: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueTrace {
    id: String,
    queue: Vec<String>,
    keys: Vec<String>,
    reference: Vec<String>,
}

fn corpus() -> Corpus {
    serde_json::from_str(CORPUS).expect("strict tui-interactions corpus")
}

#[test]
fn the_corpus_names_the_pinned_reference() {
    let corpus = corpus();
    assert_eq!(corpus.schema_version, 1);
    assert_eq!(corpus.oracle, "tui-interactions-oracle.py");
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert!(!corpus.reference.version.is_empty());
    assert_eq!(corpus.reference.source_files.len(), 8);
}

#[test]
fn tool_group_summaries_match_the_reference_header() {
    let corpus = corpus();
    assert_eq!(corpus.group_header.len(), 10);
    for case in corpus.group_header {
        assert_eq!(
            group_label(&case.kinds, case.reasoning, case.running),
            case.label,
            "{:?} reasoning={} running={}",
            case.kinds,
            case.reasoning,
            case.running
        );
    }
}

/// Reference `LoadingWidget._update_animation`: each frame shows the sweep's
/// colors cell by cell, then advances it.
#[test]
fn the_loading_sweep_colors_every_frame_as_the_reference_does() {
    let corpus = corpus();
    for sweep in corpus.loading_sweep {
        let mut loading = LoadingAnimation::seeded(1);
        let cells = loading.status(&sweep.status, 1, 1).chars().count() + 2;
        let observed = sweep
            .frames
            .iter()
            .map(|_| {
                let frame = (0..cells)
                    .map(|cell| {
                        let color = loading.color_at(cell);
                        TARGET_COLORS
                            .iter()
                            .position(|target| *target == color)
                            .and_then(|index| u8::try_from(index).ok())
                            .map_or('?', |index| char::from(b'0' + index))
                    })
                    .collect::<String>();
                loading.tick();
                frame
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, sweep.frames, "{}", sweep.status);
    }
}

fn effect_update(revision: u64, output: Option<&str>) -> TranscriptEntry {
    let state = match output {
        Some(output) => json!({"status": "running", "outputText": output}),
        None => json!({
            "status": "completed",
            "outputText": "",
            "display": {"success": true, "message": "make"},
        }),
    };
    let mut entry = crate::tui::hydration::published_fixture(
        "call",
        json!({
            "type": "effect",
            "title": "bash",
            "detail": vibe_app_server::client::EffectDetail::for_call(
                "bash",
                &json!({"command": "make"}),
            ),
            "state": state,
            "generationStatus": if output.is_some() { "in_progress" } else { "completed" },
        }),
    );
    entry.revision = revision;
    entry
}

/// Reference `set_stream_message` fed by `_appended_text`: the line under a
/// running call shows what the last growing update appended.
#[test]
fn a_running_call_streams_the_delta_the_reference_patch_carries() {
    let corpus = corpus();
    assert_eq!(corpus.stream_delta.len(), 3);
    for trace in corpus.stream_delta {
        let mut state = TuiState::new("session");
        let mut observed = Vec::new();
        for (index, output) in trace.outputs.iter().enumerate() {
            let revision = u64::try_from(index).unwrap_or(u64::MAX) + 1;
            let entry = effect_update(revision, output.as_deref());
            let event = if index == 0 {
                ServerEvent::EntryAdded {
                    event_id: revision,
                    entry,
                }
            } else {
                ServerEvent::EntryUpdated {
                    event_id: revision,
                    entry,
                }
            };
            state.apply(event).expect("the update applies");
            observed.push(
                state
                    .effect_streams
                    .get("call")
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        assert_eq!(observed, trace.reference, "{}", trace.id);
    }
}

/// A grapheme index in `text` as the reference's `row:column` location.
fn location(text: &str, index: usize) -> String {
    let before = text.graphemes(true).take(index).collect::<String>();
    let row = before.matches('\n').count();
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |line| line.graphemes(true).count());
    format!("{row}:{column}")
}

fn observe_composer(input: &ChatInputState) -> String {
    let editor = input.editor();
    match editor.selection().filter(|range| !range.is_empty()) {
        Some(range) => format!(
            "{}-{}",
            location(editor.text(), range.start),
            location(editor.text(), range.end)
        ),
        None => format!("caret {}", location(editor.text(), editor.cursor())),
    }
}

#[test]
fn composer_presses_and_drags_select_what_the_reference_selects() {
    let corpus = corpus();
    assert_eq!(corpus.composer.len(), 5);
    for trace in corpus.composer {
        let mut input = ChatInputState::new();
        input.replace_text(trace.text.clone());
        let observed = trace
            .steps
            .iter()
            .map(|step| {
                // A release changes nothing the reference observes: the chain
                // a drag ended is broken at the next press instead.
                if step.kind != "up" {
                    input.apply(InputEvent::Mouse {
                        x: step.x,
                        y: step.y,
                        extend_selection: step.kind == "move",
                        at_ms: step.at_ms,
                    });
                }
                observe_composer(&input)
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, trace.reference, "{}", trace.id);
    }
}

#[test]
fn transcript_word_and_line_presses_span_what_the_reference_spans() {
    let corpus = corpus();
    for case in corpus.transcript {
        for span in case.spans {
            let mut view = TranscriptView::default();
            view.publish(0, case.lines.clone());
            let cell = Cell {
                line: span.y,
                column: span.x,
            };
            let presses = if span.granularity == "word" { 2 } else { 3 };
            for press in 0..presses {
                view.press(cell, (0, 0), press * 100);
            }
            let observed = view.selection_ranges().first().map_or_else(
                || "none".to_owned(),
                |(line, from, to)| format!("{line}:{from}-{line}:{to}"),
            );
            assert_eq!(observed, span.reference, "{span:?}");
        }
    }
}

fn observe_queue(input: &ChatInputState, state: &TuiState) -> Value {
    let notice = state
        .inline_notice
        .as_ref()
        .map(|notice| match notice.text.as_str() {
            SELECTION_HINT => "selection",
            EDIT_HINT => "edit",
            CONSUMED_HINT => "consumed",
            _ => "other",
        });
    let mut queue = state
        .prompt_queue
        .newest_first()
        .into_iter()
        .map(|(_, text)| text.to_owned())
        .collect::<Vec<_>>();
    queue.reverse();
    json!({
        "cursor": state
            .queue_selection
            .as_ref()
            .map_or(-1, |selection| i64::try_from(selection.position).unwrap_or(i64::MAX)),
        "editing": state.queue_selection.as_ref().is_some_and(|selection| selection.editing),
        "text": input.editor().text(),
        "notice": notice,
        "queue": queue,
    })
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn key_name(name: &str) -> (KeyCode, KeyName) {
    match name {
        "up" => (KeyCode::Up, KeyName::Up),
        "down" => (KeyCode::Down, KeyName::Down),
        "enter" => (KeyCode::Enter, KeyName::Enter),
        "escape" => (KeyCode::Esc, KeyName::Escape),
        "backspace" => (KeyCode::Backspace, KeyName::Backspace),
        "delete" => (KeyCode::Delete, KeyName::Delete),
        "home" => (KeyCode::Home, KeyName::Home),
        other => panic!("the corpus names an unknown key `{other}`"),
    }
}

/// The routing `shortcuts::handle_key` gives these keys: queue mode first,
/// then an open edit, then the composer.
#[tokio::test]
async fn queue_mode_walks_edits_and_removes_what_the_reference_does() {
    let corpus = corpus();
    assert_eq!(corpus.queue.len(), 4);
    for trace in corpus.queue {
        let mut runtime = Some(interactive_test_runtime("queue-mode"));
        let session = runtime.as_ref().map(|runtime| runtime.session_id.clone());
        let mut state = TuiState::new(session.as_deref().unwrap_or("session"));
        for text in &trace.queue {
            state
                .prompt_queue
                .push(crate::tui::attachments::PromptDraft::text_only(
                    text.clone(),
                ));
        }
        let mut input = ChatInputState::new();
        let mut active = None;
        let mut controls = ControlState::new(session.as_deref().unwrap_or("session"));
        let mut images = ClipboardImageManager::default();
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace = workspace.path().to_path_buf();
        let mut observed = Vec::new();
        for name in &trace.keys {
            if name == "promote" {
                let oldest = state
                    .prompt_queue
                    .newest_first()
                    .last()
                    .map(|(id, _)| (*id).to_owned());
                if let Some(oldest) = oldest {
                    state.prompt_queue.remove(&oldest);
                }
            } else if let Some(text) = name.strip_prefix("type:") {
                for character in text.chars() {
                    let event = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
                    if !queue_selection_key(event, &mut input, &mut state) {
                        input.apply(InputEvent::Key {
                            key: KeyName::Char,
                            char: Some(character),
                            mods: Vec::new(),
                        });
                    }
                }
            } else if name.len() == 1 {
                let character = name.chars().next().unwrap_or(' ');
                let event = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
                if !queue_selection_key(event, &mut input, &mut state) {
                    input.apply(InputEvent::Key {
                        key: KeyName::Char,
                        char: Some(character),
                        mods: Vec::new(),
                    });
                }
            } else {
                let (code, name) = key_name(name);
                let editing = state
                    .queue_selection
                    .as_ref()
                    .is_some_and(|selection| selection.editing);
                if queue_selection_key(key(code), &mut input, &mut state) {
                } else if editing && code == KeyCode::Esc {
                    leave_queue_edit(&mut input, &mut state);
                } else if editing && code == KeyCode::Enter {
                    let context = PromptContext::new(
                        &workspace,
                        &mut runtime,
                        &mut active,
                        &mut state,
                        &mut controls,
                        &mut images,
                    );
                    submit_queue_edit(context, &mut input)
                        .await
                        .expect("the edit is answered");
                } else {
                    input.set_queue_selectable(
                        state.queue_selection.is_none() && !state.prompt_queue.is_empty(),
                    );
                    let effects = input.apply(InputEvent::Key {
                        key: name,
                        char: None,
                        mods: Vec::new(),
                    });
                    if effects
                        .iter()
                        .any(|effect| matches!(effect, InputEffect::QueueSelectionRequested))
                    {
                        enter_queue_selection(&input, &mut state);
                    }
                }
            }
            observed.push(observe_queue(&input, &state));
        }
        let reference = trace
            .reference
            .iter()
            .map(|observation| serde_json::from_str::<Value>(observation).expect("observation"))
            .collect::<Vec<_>>();
        assert_eq!(observed, reference, "{}", trace.id);
    }
}

/// Recaptures the corpus from the pinned reference and requires it unchanged,
/// so a drift in the reference cannot hide behind the committed file.
#[test]
fn the_live_reference_still_produces_the_committed_corpus() {
    let Some((root, interpreter)) = pinned_python_oracle() else {
        eprintln!("skipping the live tui-interactions probe");
        return;
    };
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/runtime-parity/tui-interactions-oracle.py");
    let output = Command::new(interpreter)
        .arg(script)
        .arg("--reference")
        .arg(&root)
        .current_dir(&root)
        .output()
        .expect("execute the pinned Python tui-interactions oracle");
    assert!(
        output.status.success(),
        "Python tui-interactions oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let live: Value = serde_json::from_slice(&output.stdout).expect("oracle output");
    let committed: Value = serde_json::from_str(CORPUS).expect("committed corpus");
    assert_eq!(live, committed);
}
