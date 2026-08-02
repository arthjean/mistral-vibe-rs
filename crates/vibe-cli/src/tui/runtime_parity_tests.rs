use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::attachments::{PromptDraft, prepare_submission};
use super::callback::activate_pending_callback_state;
use super::chat_input::InputMode;
use super::clipboard_images::ClipboardImageManager;
use super::completion::CompletionEngine;
use super::controls::{
    ApprovalScope, CallbackChoice, CallbackEffect, CallbackInput, CallbackInputOutcome,
    CallbackOption, CallbackPresentation, CallbackQuestion, CallbackRequest, ControlState,
    PendingCallback, UserInputChoice,
};
use super::input::PromptEditor;
use super::interaction::{PromptQueue, QueuedIntentKind};
use super::plan_review::PlanReviewMonitor;
use super::prompt::PromptContext;
use super::queue::start_next_queued_prompt;
use super::render::{BannerContext, TokenState, UiContext, draw};
use super::setup::{DetectedTheme, Theme, resolve_theme};
use super::shell::{ActiveShell, ShellRead, apply_shell_read};
use super::state::{EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use super::{ActiveTurn, InteractiveRuntime};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde::Deserialize;
use serde_json::Value;

const REFERENCE_COMMIT: &str = "99a6efa9ca1fb48671adebe0f6f5d931945bd8c9";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    traces: Vec<Trace>,
    unavailable: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
    version: String,
    source_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trace {
    id: String,
    story: String,
    events: Vec<TraceEvent>,
    expected: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TraceEvent {
    PresentApproval { id: String, at_ms: u64 },
    PresentQuestions { at_ms: u64 },
    Input { input: ReplayInput, at_ms: u64 },
    AnswerAndPresentApproval { id: String, at_ms: u64 },
    PlanReviewStart,
    RemovePlan,
    WritePlan { content: String },
    RefreshPlan,
    Queue { text: String },
    DrainBatch,
    DrainUnavailable,
    ResumeQueue,
    ShellStart,
    ShellChunk { text: String },
    ShellCancel,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ReplayInput {
    Select,
    Down,
    Next,
    Text { value: String },
}

struct Replay {
    controls: ControlState,
    state: TuiState,
    clipboard_images: ClipboardImageManager,
    plan_monitor: PlanReviewMonitor,
    plan_path: PathBuf,
    shell_id: Option<String>,
    shell: Option<ActiveShell>,
    shell_cursor: u64,
    temporary: tempfile::TempDir,
}

impl Replay {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("runtime parity workspace");
        let plan_path = temporary.path().join("plans/session.md");
        Self {
            controls: ControlState::new("session"),
            state: TuiState::new("session"),
            clipboard_images: ClipboardImageManager::default(),
            plan_monitor: PlanReviewMonitor::default(),
            plan_path,
            shell_id: None,
            shell: None,
            shell_cursor: 0,
            temporary,
        }
    }

    async fn apply(&mut self, event: &TraceEvent) -> String {
        match event {
            TraceEvent::PresentApproval { id, at_ms } => {
                assert!(activate_pending_callback_state(
                    &mut self.state,
                    &mut self.controls,
                    approval(id),
                    *at_ms,
                ));
                let presentation = self
                    .controls
                    .callback_presentation(at_ms.saturating_add(500))
                    .expect("approval presentation");
                format!(
                    "active:{id}:effect={}:permissions={}",
                    presentation.lines.iter().any(|line| line == "cargo test"),
                    presentation
                        .lines
                        .iter()
                        .any(|line| line == "Permissions: cargo test")
                )
            }
            TraceEvent::PresentQuestions { at_ms } => {
                assert!(activate_pending_callback_state(
                    &mut self.state,
                    &mut self.controls,
                    questions(),
                    *at_ms,
                ));
                "active:questions".to_owned()
            }
            TraceEvent::Input { input, at_ms } => self.apply_input(input, *at_ms),
            TraceEvent::AnswerAndPresentApproval { id, at_ms } => {
                let active = self
                    .controls
                    .pending_callback()
                    .expect("active callback")
                    .callback_id
                    .clone();
                let choice = CallbackChoice::Approve {
                    scope: ApprovalScope::Once,
                };
                let dispatch = self
                    .controls
                    .prepare_answer("turn", &active, &choice)
                    .expect("answer prepares");
                self.controls
                    .accept_answer("turn", &active, choice, &dispatch)
                    .expect("answer commits");
                self.controls.reconcile_active_callback(None);
                assert!(activate_pending_callback_state(
                    &mut self.state,
                    &mut self.controls,
                    approval(id),
                    *at_ms,
                ));
                format!("active:{id}")
            }
            TraceEvent::PlanReviewStart => {
                assert!(activate_pending_callback_state(
                    &mut self.state,
                    &mut self.controls,
                    plan_review(self.plan_path.clone()),
                    0,
                ));
                format!(
                    "plan:{}",
                    self.controls
                        .pending_callback()
                        .and_then(|pending| match &pending.request {
                            CallbackRequest::PlanReview { plan_path, .. } => Some(plan_path),
                            CallbackRequest::Approval { .. }
                            | CallbackRequest::UserInput { .. } => None,
                        })
                        .is_some_and(|path| path == &self.plan_path)
                )
            }
            TraceEvent::WritePlan { content } => {
                fs::create_dir_all(self.plan_path.parent().expect("plan parent"))
                    .expect("plan directory");
                fs::write(&self.plan_path, content).expect("plan writes");
                "plan:written".to_owned()
            }
            TraceEvent::RemovePlan => {
                if self.plan_path.exists() {
                    fs::remove_file(&self.plan_path).expect("plan removes");
                }
                "plan:removed".to_owned()
            }
            TraceEvent::RefreshPlan => {
                tokio::time::sleep(Duration::from_millis(260)).await;
                self.plan_monitor
                    .sync(Some(self.plan_path.clone()), &mut self.state)
                    .await;
                self.state.plan_review.as_ref().map_or_else(
                    || "plan:missing".to_owned(),
                    |plan| {
                        plan.error.as_ref().map_or_else(
                            || format!("plan:{}", plan.content),
                            |_| "plan:unreadable".to_owned(),
                        )
                    },
                )
            }
            TraceEvent::Queue { text } => {
                self.state.prompt_queue.push(PromptDraft::text_only(text));
                format!("queued:{text}")
            }
            TraceEvent::DrainBatch => {
                let batch = self
                    .state
                    .prompt_queue
                    .take_next_batch()
                    .expect("batch drains");
                let kind = match batch[0].kind {
                    QueuedIntentKind::Prompt => "prompt",
                    QueuedIntentKind::Shell => "shell",
                };
                let observation = format!("batch:{kind}:{}", batch.len());
                observation
            }
            TraceEvent::DrainUnavailable => {
                let mut runtime: Option<InteractiveRuntime> = None;
                let mut active: Option<ActiveTurn> = None;
                start_next_queued_prompt(PromptContext::new(
                    self.temporary.path(),
                    &mut runtime,
                    &mut active,
                    &mut self.state,
                    &mut self.controls,
                    &mut self.clipboard_images,
                ))
                .await
                .expect("unavailable runtime pauses without losing the batch");
                format!("queue:paused:{}", self.state.prompt_queue.len())
            }
            TraceEvent::ResumeQueue => {
                self.state.prompt_queue.resume();
                format!("queue:resumed:{}", self.state.prompt_queue.len())
            }
            TraceEvent::ShellStart => {
                let id = self.state.append_local(shell_entry(String::new()));
                self.shell = Some(ActiveShell::new("fixture", "operation", id.clone()));
                self.shell_id = Some(id);
                "shell:streaming:".to_owned()
            }
            TraceEvent::ShellChunk { text } => {
                if let Some(shell) = self.shell.take() {
                    apply_shell_read(
                        &mut self.shell,
                        &mut self.state,
                        shell,
                        ShellRead::running(self.shell_cursor, text.as_bytes().to_vec()),
                    );
                    self.shell_cursor = self.shell_cursor.saturating_add(1);
                }
                self.observe_shell()
            }
            TraceEvent::ShellCancel => {
                if let Some(shell) = self.shell.take() {
                    apply_shell_read(
                        &mut self.shell,
                        &mut self.state,
                        shell,
                        ShellRead::interrupted(),
                    );
                }
                self.observe_shell()
            }
        }
    }

    fn apply_input(&mut self, input: &ReplayInput, at_ms: u64) -> String {
        let outcomes = match input {
            ReplayInput::Select => vec![
                self.controls
                    .apply_callback_input(CallbackInput::Select, at_ms),
            ],
            ReplayInput::Down => vec![
                self.controls
                    .apply_callback_input(CallbackInput::Down, at_ms),
            ],
            ReplayInput::Next => vec![
                self.controls
                    .apply_callback_input(CallbackInput::NextQuestion, at_ms),
            ],
            ReplayInput::Text { value } => value
                .chars()
                .map(|character| {
                    self.controls
                        .apply_callback_input(CallbackInput::Character(character), at_ms)
                })
                .collect(),
        };
        outcomes
            .into_iter()
            .map(observe_callback_outcome)
            .collect::<Vec<_>>()
            .join(",")
    }

    fn observe_shell(&self) -> String {
        let id = self.shell_id.as_deref().expect("shell entry exists");
        let entry = self
            .state
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .expect("shell entry remains");
        let output = entry
            .text
            .lines()
            .filter_map(|line| line.strip_prefix("    "))
            .collect::<String>();
        format!("shell:{:?}:{output}", entry.status).to_lowercase()
    }
}

fn observe_callback_outcome(outcome: CallbackInputOutcome) -> String {
    match outcome {
        CallbackInputOutcome::Ignored => "ignored".to_owned(),
        CallbackInputOutcome::Updated => "updated".to_owned(),
        CallbackInputOutcome::GraceBlocked => "grace_blocked".to_owned(),
        CallbackInputOutcome::Invalid(message) => format!("invalid:{message}"),
        CallbackInputOutcome::Submit(CallbackChoice::Approve {
            scope: ApprovalScope::Once,
        }) => "submit:approve_once".to_owned(),
        CallbackInputOutcome::Submit(CallbackChoice::UserInput { answers }) => {
            let answers = answers
                .iter()
                .map(|answer| match answer {
                    UserInputChoice::Option { id } => format!("option:{id}"),
                    UserInputChoice::Options { ids } => format!("options:{}", ids.join("+")),
                    UserInputChoice::Combined { ids, other } => {
                        format!("combined:{}+{other}", ids.join("+"))
                    }
                    UserInputChoice::FreeText { value } => format!("text:{value}"),
                })
                .collect::<Vec<_>>()
                .join("|");
            format!("submit:{answers}")
        }
        CallbackInputOutcome::Submit(choice) => format!("submit:{choice:?}"),
    }
}

fn option(id: &str, label: &str) -> CallbackOption {
    CallbackOption {
        id: id.to_owned(),
        label: label.to_owned(),
        description: String::new(),
    }
}

fn approval(id: &str) -> PendingCallback {
    PendingCallback {
        callback_id: id.to_owned(),
        session_id: "session".to_owned(),
        turn_id: "turn".to_owned(),
        prompt: "Approve shell?".to_owned(),
        request: CallbackRequest::Approval {
            options: vec![
                option("approve", "Allow once"),
                option("approve_for_session", "Allow for session"),
                option("approve_permanently", "Always allow"),
                option("deny", "Deny"),
            ],
            effect: CallbackEffect {
                tool_name: "shell".to_owned(),
                summary: "Run tests".to_owned(),
                content: "cargo test".to_owned(),
                permissions: vec!["cargo test".to_owned()],
            },
        },
    }
}

fn questions() -> PendingCallback {
    PendingCallback {
        callback_id: "questions".to_owned(),
        session_id: "session".to_owned(),
        turn_id: "turn".to_owned(),
        prompt: "Questions".to_owned(),
        request: CallbackRequest::UserInput {
            questions: vec![
                CallbackQuestion {
                    header: "Runtime".to_owned(),
                    question: "Choose runtimes".to_owned(),
                    options: vec![option("rust", "Rust"), option("python", "Python")],
                    allows_free_text: true,
                    multi_select: true,
                },
                CallbackQuestion {
                    header: "Constraint".to_owned(),
                    question: "Constraint?".to_owned(),
                    options: vec![option("fast", "Fast"), option("small", "Small")],
                    allows_free_text: true,
                    multi_select: false,
                },
            ],
            footer_note: Some("Answer both".to_owned()),
        },
    }
}

fn plan_review(path: PathBuf) -> PendingCallback {
    let mut pending = questions();
    pending.callback_id = "plan-review".to_owned();
    let CallbackRequest::UserInput { questions, .. } = &pending.request else {
        unreachable!("question fixture is typed")
    };
    let questions = questions.iter().take(1).cloned().collect();
    pending.request = CallbackRequest::PlanReview {
        questions,
        footer_note: Some("Answer both".to_owned()),
        plan_path: path,
    };
    pending
}

fn shell_entry(text: String) -> TranscriptEntry {
    TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Effect,
        text,
        status: EntryStatus::Streaming,
        details: Value::Null,
    }
}

#[tokio::test]
async fn ep007_corpus_replays_every_event_against_runtime_reducers() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../tests/runtime-parity/active-turn-ep007.json"
    ))
    .expect("strict EP-007 corpus");
    assert_eq!(corpus.schema_version, 2);
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert!(!corpus.reference.version.is_empty());
    assert_eq!(corpus.reference.source_files.len(), 5);
    assert!(corpus.unavailable.is_empty());
    assert_eq!(
        corpus
            .traces
            .iter()
            .map(|trace| trace.story.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["US-024", "US-025", "US-026", "US-027", "US-028"])
    );

    for trace in corpus.traces {
        assert!(!trace.id.is_empty());
        let mut replay = Replay::new();
        let mut actual = Vec::with_capacity(trace.events.len());
        for event in &trace.events {
            actual.push(replay.apply(event).await);
        }
        assert_eq!(actual, trace.expected, "trace {} diverged", trace.id);
    }
}

#[test]
fn queued_prompts_retain_the_prepared_image_snapshot() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let image = workspace.path().join("queued.png");
    fs::write(&image, b"original image").expect("image fixture writes");
    let draft = PromptDraft::text_only("inspect @queued.png");
    let prepared = prepare_submission(workspace.path(), &draft, "vision", true)
        .expect("queued prompt prepares once");
    fs::write(&image, b"changed image").expect("image fixture changes");

    let mut queue = PromptQueue::default();
    queue.push_prepared(draft, prepared);
    let batch = queue.take_next_batch().expect("prepared prompt drains");
    let retained = batch[0]
        .prepared
        .as_ref()
        .expect("prepared payload remains owned by the queue");
    assert_eq!(
        BASE64_STANDARD
            .decode(&retained.provider_images.as_slice()[0].data)
            .expect("prepared image decodes"),
        b"original image"
    );
}

#[test]
fn callback_viewport_keeps_the_focused_action_visible_at_fixed_widths() {
    for width in [40, 80, 120] {
        let mut state = TuiState::new("session");
        let mut lines = (0..39)
            .map(|index| format!("effect line {index}"))
            .collect::<Vec<_>>();
        lines.push(format!("permission {} effect-tail", "x".repeat(120)));
        lines.push("› focused approval".to_owned());
        state.set_callback_presentation(Some(CallbackPresentation {
            callback_id: "approval".to_owned(),
            focus_line: 40,
            lines,
        }));
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
                        safety: super::chat_input::Safety::Neutral,
                        switching: false,
                        feedback_active: false,
                        voice_phase: super::chat_input::VoicePhase::Disabled,
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
            .expect("fixed-width render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("focused approval"));
        assert!(rendered.contains("effect-tail"));
    }
}
