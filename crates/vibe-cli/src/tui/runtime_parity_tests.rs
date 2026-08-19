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
use super::interaction::{Overlay, OverlayItem, OverlayKind, PromptQueue, QueuedIntentKind};
use super::plan_review::PlanReviewMonitor;
use super::prompt::PromptContext;
use super::queue::start_next_queued_prompt;
use super::render::{BannerContext, TokenState, UiContext, draw};
use super::rewind::{RewindEffect, RewindState, RewindTarget, reduce_key as reduce_rewind_key};
use super::session_picker::{
    SessionDeleteState, SessionPickerEffect, reduce_key as reduce_session_picker_key,
};
use super::setup::{DetectedTheme, Theme, resolve_theme};
use super::shell::{ActiveShell, ShellRead, apply_shell_read};
use super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use super::{ActiveTurn, InteractiveRuntime};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde::Deserialize;
use serde_json::Value;

use vibe_core::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

/// The Python reference lives in a checkout outside this repository, so the
/// live oracle probe can only run on a workstation that holds it at
/// [`REFERENCE_COMMIT`]. Returns the checkout root and the interpreter that can
/// drive it, and `None` when either is missing or the checkout has moved: every
/// corpus replay still runs against the reference captured in the checked-in
/// fixtures.
///
/// The interpreter is returned rather than merely checked, so a caller spawns
/// the one this guard admitted. Probing for `VIBE_PARITY_PYTHON` or a Windows
/// virtual environment and then hardcoding the POSIX path would turn a skip into
/// a panic on exactly the machines the override exists for.
fn pinned_python_oracle() -> Option<(PathBuf, PathBuf)> {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "Python oracle") {
        eprintln!("{reason}");
        return None;
    }
    let interpreter = pinned_interpreter(&root)?;
    Some((root, interpreter))
}

mod configuration_integrations;
mod semantic_transcript;
mod terminal_services;

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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionManagementCorpus {
    schema_version: u32,
    reference: Reference,
    traces: Vec<SessionManagementTrace>,
    unavailable: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionManagementTrace {
    id: String,
    story: String,
    events: Vec<SessionManagementEvent>,
    expected: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SessionManagementEvent {
    OpenRewind {
        targets: Vec<SessionManagementTarget>,
    },
    RewindPrevious,
    RewindNext,
    RewindActionDown,
    RewindAccept,
    RewindFailure,
    RewindCancel,
    OpenSessions {
        sessions: Vec<String>,
        current: Option<String>,
    },
    SelectSession {
        session_id: String,
    },
    DeleteRequest {
        session_id: String,
    },
    DeleteFailure,
    DeleteSuccess,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionManagementTarget {
    message_index: usize,
    message: String,
    has_file_changes: bool,
}

#[derive(Default)]
struct SessionManagementReplay {
    rewind: Option<RewindState>,
    delete: Option<SessionDeleteState>,
    sessions: Option<Overlay>,
    current: Option<String>,
    dispatched_delete: Option<String>,
    delete_calls: usize,
}

impl SessionManagementReplay {
    fn apply(&mut self, event: SessionManagementEvent) -> String {
        match event {
            SessionManagementEvent::OpenRewind { targets } => {
                self.rewind = RewindState::new(
                    targets
                        .into_iter()
                        .map(|target| RewindTarget {
                            // The corpus records the position the reference
                            // rewound to; the identity this port addresses is
                            // the one the server derives from that position.
                            entry_id: format!("history:{}:user", target.message_index),
                            message: target.message,
                            has_file_changes: target.has_file_changes,
                        })
                        .collect(),
                );
                self.observe_rewind("open")
            }
            SessionManagementEvent::RewindPrevious => {
                self.reduce_rewind(KeyCode::Left);
                self.observe_rewind("previous")
            }
            SessionManagementEvent::RewindNext => {
                self.reduce_rewind(KeyCode::Right);
                self.observe_rewind("next")
            }
            SessionManagementEvent::RewindActionDown => {
                self.reduce_rewind(KeyCode::Down);
                self.observe_rewind("action")
            }
            SessionManagementEvent::RewindAccept => match self.reduce_rewind(KeyCode::Enter) {
                RewindEffect::Accept {
                    entry_id,
                    restore_files,
                } => format!("rewind:dispatch:{entry_id}:{restore_files}"),
                effect => panic!("unexpected rewind effect: {effect:?}"),
            },
            SessionManagementEvent::RewindFailure => {
                let rewind = self.rewind.as_mut().expect("rewind is open");
                rewind.set_error("Rewind failed: injected failure");
                format!(
                    "rewind:failure:retained={}:visible={}",
                    self.rewind.is_some(),
                    self.rewind.as_ref().and_then(RewindState::error).is_some()
                )
            }
            SessionManagementEvent::RewindCancel => {
                assert_eq!(self.reduce_rewind(KeyCode::Char('q')), RewindEffect::Cancel);
                self.rewind = None;
                "rewind:cancelled".to_owned()
            }
            SessionManagementEvent::OpenSessions { sessions, current } => {
                let count = sessions.len();
                self.sessions = Some(Overlay::new(
                    OverlayKind::Sessions,
                    "Sessions",
                    sessions
                        .into_iter()
                        .map(|session_id| {
                            OverlayItem::new(session_id.clone(), session_id, "saved session", false)
                        })
                        .collect(),
                ));
                self.current = current;
                self.delete = None;
                self.dispatched_delete = None;
                format!("sessions:open:{count}")
            }
            SessionManagementEvent::SelectSession { session_id } => {
                let overlay = self.sessions.as_mut().expect("sessions are open");
                for _ in 0..overlay.items.len() {
                    if overlay
                        .selected_item()
                        .is_some_and(|item| item.id == session_id)
                    {
                        break;
                    }
                    let effect = reduce_session_picker_key(
                        overlay,
                        &mut self.delete,
                        self.current.as_deref().unwrap_or_default(),
                        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    );
                    assert_eq!(effect, SessionPickerEffect::None);
                }
                assert!(
                    overlay
                        .selected_item()
                        .is_some_and(|item| item.id == session_id)
                );
                format!("sessions:selected:{session_id}")
            }
            SessionManagementEvent::DeleteRequest { session_id } => {
                let overlay = self.sessions.as_mut().expect("sessions are open");
                assert_eq!(
                    overlay.selected_item().map(|item| item.id.as_str()),
                    Some(session_id.as_str())
                );
                match reduce_session_picker_key(
                    overlay,
                    &mut self.delete,
                    self.current.as_deref().unwrap_or_default(),
                    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                ) {
                    SessionPickerEffect::None => {
                        let message = self.delete.as_ref().expect("delete state").message();
                        if message.contains("current session") {
                            format!("delete:active-guard:{session_id}")
                        } else {
                            format!("delete:confirm:{session_id}")
                        }
                    }
                    SessionPickerEffect::Delete(dispatched) => {
                        self.delete_calls = self.delete_calls.saturating_add(1);
                        self.dispatched_delete = Some(dispatched);
                        format!("delete:dispatch:{}", self.delete_calls)
                    }
                    effect => panic!("unexpected session effect: {effect:?}"),
                }
            }
            SessionManagementEvent::DeleteFailure => {
                let session_id = self
                    .dispatched_delete
                    .take()
                    .expect("delete was dispatched");
                let retained = self
                    .sessions
                    .as_ref()
                    .is_some_and(|overlay| overlay.items.iter().any(|item| item.id == session_id));
                self.delete = Some(SessionDeleteState::failure(session_id, "injected failure"));
                format!("delete:failure:retained={retained}")
            }
            SessionManagementEvent::DeleteSuccess => {
                let session_id = self
                    .dispatched_delete
                    .take()
                    .expect("delete was dispatched");
                let overlay = self.sessions.as_mut().expect("sessions are open");
                overlay.items.retain(|item| item.id != session_id);
                overlay.set_query(overlay.query.clone());
                self.delete = None;
                format!("delete:success:remaining={}", overlay.items.len())
            }
        }
    }

    fn reduce_rewind(&mut self, code: KeyCode) -> RewindEffect {
        reduce_rewind_key(
            self.rewind.as_mut().expect("rewind is open"),
            KeyEvent::new(code, KeyModifiers::NONE),
        )
    }

    fn observe_rewind(&self, prefix: &str) -> String {
        let rewind = self.rewind.as_ref().expect("rewind is open");
        format!(
            "rewind:{prefix}:target={}:actions={}:selected={:?}",
            rewind.target().entry_id,
            rewind.actions().len(),
            rewind.selected_action()
        )
        .to_lowercase()
    }
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
        source: EntrySource::Restored,
    }
}

#[tokio::test]
async fn active_turn_corpus_replays_every_event_against_runtime_reducers() {
    let corpus: Corpus =
        serde_json::from_str(include_str!("../../tests/runtime-parity/active-turn.json"))
            .expect("strict active-turn corpus");
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
fn session_management_corpus_replays_rewind_and_delete_state_machines() {
    let corpus: SessionManagementCorpus = serde_json::from_str(include_str!(
        "../../tests/runtime-parity/session-management.json"
    ))
    .expect("strict session-management corpus");
    assert_eq!(corpus.schema_version, 2);
    assert_eq!(corpus.reference.commit, REFERENCE_COMMIT);
    assert!(!corpus.reference.version.is_empty());
    assert_eq!(corpus.reference.source_files.len(), 3);
    assert_eq!(
        corpus
            .traces
            .iter()
            .map(|trace| trace.story.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["US-030"])
    );
    // US-029 left the replayable set when the reference moved to a two-step
    // rewind at v2.23.3, measured by `tests/runtime-parity/session-management-oracle.py`.
    // A dropped trace stays visible: it names the story it covered and why the
    // corpus carries no expectation for it.
    for entry in &corpus.unavailable {
        for field in ["id", "story", "reason"] {
            assert!(
                entry
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty()),
                "unavailable trace is missing `{field}`: {entry}"
            );
        }
    }

    for trace in corpus.traces {
        assert!(!trace.id.is_empty());
        let mut replay = SessionManagementReplay::default();
        let actual = trace
            .events
            .into_iter()
            .map(|event| replay.apply(event))
            .collect::<Vec<_>>();
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
