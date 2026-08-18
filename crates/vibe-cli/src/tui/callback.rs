use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use vibe_app_server::client::{
    ApprovalDecisionType, CallbackDetail, EffectDetail, NoticeDetail, PublicCallbackState,
    PublicHistoryEntry, UserQuestionRequest,
};
use vibe_core::policy::PermissionRequirement;

use super::controls::{
    CallbackChoice, CallbackEffect, CallbackInput, CallbackInputOutcome, CallbackOption,
    CallbackQuestion, CallbackRequest, ControlState, PendingCallback,
};
use super::input::SystemExternalEditor;
use super::state::{
    EntrySource, EntryStatus, PlanReviewState, TranscriptEntry, TranscriptKind, TuiState,
};
use super::terminal::{CrosstermOps, TerminalGuard};
use super::{
    ActiveTurn, CliError, InteractiveRuntime, push_local_notice, request_active_turn_interrupt,
    resync_current_projection, unix_millis,
};

const MAX_CALLBACK_DETAIL_BYTES: usize = 64 * 1024;
const MAX_CALLBACK_QUESTIONS: usize = 16;
const MAX_CALLBACK_OPTIONS: usize = 32;
const MAX_CALLBACK_TEXT_BYTES: usize = 8 * 1024;

pub(super) fn handle_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    active: &mut Option<ActiveTurn>,
    state: &mut TuiState,
    controls: &mut ControlState,
    terminal_guard: &mut TerminalGuard<CrosstermOps<std::io::Stdout>>,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<bool, CliError> {
    if controls.pending_callback().is_none() {
        return Ok(false);
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        request_active_turn_interrupt(runtime, active, controls, state);
        return Ok(true);
    }
    if key.code == KeyCode::Char('g')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && controls
            .pending_callback()
            .is_some_and(|pending| matches!(&pending.request, CallbackRequest::PlanReview { .. }))
    {
        let path = controls
            .pending_callback()
            .and_then(|pending| match &pending.request {
                CallbackRequest::PlanReview { plan_path, .. } => Some(plan_path.clone()),
                CallbackRequest::Approval { .. } | CallbackRequest::UserInput { .. } => None,
            })
            .ok_or_else(|| CliError::Terminal("plan review path disappeared".to_owned()))?;
        terminal_guard
            .restore()
            .map_err(|error| CliError::Terminal(error.to_string()))?;
        let edited = SystemExternalEditor::from_environment().edit_file(&path);
        terminal_guard
            .resume()
            .map_err(|error| CliError::Terminal(error.to_string()))?;
        terminal
            .clear()
            .map_err(|error| CliError::Terminal(error.to_string()))?;
        match edited {
            Ok(content) => {
                state.plan_review = Some(PlanReviewState {
                    path,
                    content,
                    error: None,
                });
            }
            Err(error) => state.push_diagnostic(format!("Could not open plan in editor: {error}")),
        }
        sync_callback_presentation(controls, state, unix_millis());
        return Ok(true);
    }
    if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        state.scroll_callback(if key.code == KeyCode::PageUp { -10 } else { 10 });
        return Ok(true);
    }

    let approval = controls
        .pending_callback()
        .is_some_and(|pending| matches!(&pending.request, CallbackRequest::Approval { .. }));
    let callback_input = match key.code {
        KeyCode::Up => Some(CallbackInput::Up),
        KeyCode::Down => Some(CallbackInput::Down),
        KeyCode::Left => Some(CallbackInput::PreviousQuestion),
        KeyCode::Right => Some(CallbackInput::NextQuestion),
        KeyCode::Enter => Some(CallbackInput::Select),
        KeyCode::Esc => Some(CallbackInput::Cancel),
        KeyCode::Backspace => Some(CallbackInput::Backspace),
        KeyCode::Char('y') if key.modifiers.is_empty() && approval => {
            Some(CallbackInput::Shortcut(0))
        }
        KeyCode::Char('n') if key.modifiers.is_empty() && approval => {
            Some(CallbackInput::Shortcut(3))
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty()
                && character.is_ascii_digit()
                && character != '0'
                && !controls.accepts_free_text() =>
        {
            character
                .to_digit(10)
                .and_then(|digit| usize::try_from(digit.saturating_sub(1)).ok())
                .map(CallbackInput::Shortcut)
        }
        KeyCode::Char('j') if key.modifiers.is_empty() && !controls.accepts_free_text() => {
            Some(CallbackInput::Down)
        }
        KeyCode::Char('k') if key.modifiers.is_empty() && !controls.accepts_free_text() => {
            Some(CallbackInput::Up)
        }
        KeyCode::Char(character) if key.modifiers.is_empty() => {
            Some(CallbackInput::Character(character))
        }
        _ => None,
    };
    if let Some(callback_input) = callback_input {
        let pending = controls.pending_callback().cloned();
        match controls.apply_callback_input(callback_input, unix_millis()) {
            CallbackInputOutcome::Submit(choice) => {
                let (Some(runtime), Some(pending)) = (runtime.as_mut(), pending.as_ref()) else {
                    state.push_diagnostic(
                        "The callback is no longer attached to an interactive session",
                    );
                    return Ok(true);
                };
                respond_to_pending_callback(runtime, controls, pending, choice, state);
            }
            CallbackInputOutcome::Invalid(message) => state.push_diagnostic(message),
            CallbackInputOutcome::Ignored
            | CallbackInputOutcome::Updated
            | CallbackInputOutcome::GraceBlocked => {}
        }
    }
    sync_callback_presentation(controls, state, unix_millis());
    Ok(true)
}

pub(super) fn drain_callback_requests(
    runtime: Option<&mut InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
) {
    let Some(runtime) = runtime else {
        return;
    };
    let entries = match runtime.service.drain_callbacks() {
        Ok(entries) => entries,
        Err(error) => {
            state.push_diagnostic(format!("Interactive callbacks are unavailable: {error}"));
            return;
        }
    };
    if entries.is_empty() {
        return;
    }
    resync_current_projection(runtime, state);
    let plan_reviews = plan_reviews(state);
    for entry in entries {
        let plan_review = plan_reviews.get(entry.metadata().id.as_str());
        let pending = match pending_callback_from_entry(&entry, plan_review.map(PathBuf::as_path)) {
            Ok(Some(pending)) => pending,
            Ok(None) => continue,
            Err(error) => {
                state.push_diagnostic(format!("Interactive callback is invalid: {error}"));
                continue;
            }
        };
        activate_pending_callback(runtime, state, controls, pending);
    }
}

pub(super) fn respond_to_pending_callback(
    runtime: &mut InteractiveRuntime,
    controls: &mut ControlState,
    pending: &PendingCallback,
    choice: CallbackChoice,
    state: &mut TuiState,
) {
    if matches!(&pending.request, CallbackRequest::PlanReview { .. })
        && runtime.mode != "plan"
        && choice != CallbackChoice::Cancel
    {
        state.push_diagnostic("Exit plan mode is only valid while the session is in plan mode");
        return;
    }
    let plan_transition = matches!(&pending.request, CallbackRequest::PlanReview { .. })
        .then(|| plan_transition(&choice))
        .flatten();
    if plan_transition.is_some()
        && let Some(error) = plan_approval_error(state)
    {
        state.push_diagnostic(error);
        return;
    }
    let callback_cancelled = choice == CallbackChoice::Cancel;
    // Reference `app.py:3349` and `app.py:3361`: a refused approval and a
    // cancelled question are both reported, under the action each names.
    match (&pending.request, &choice) {
        (CallbackRequest::Approval { .. }, CallbackChoice::Deny { .. }) => {
            super::report_cancelled_action(Some(runtime), super::CancelledAction::RejectApproval);
        }
        (CallbackRequest::UserInput { .. }, CallbackChoice::Cancel) => {
            super::report_cancelled_action(Some(runtime), super::CancelledAction::CancelQuestion);
        }
        _ => {}
    }
    let dispatch = match controls.prepare_answer(&pending.turn_id, &pending.callback_id, &choice) {
        Ok(dispatch) => dispatch,
        Err(error) => {
            state.push_diagnostic(error.to_string());
            return;
        }
    };
    let previous_settings = if let Some(auto_approve) = plan_transition {
        let previous = (runtime.mode.clone(), runtime.auto_approve);
        if let Err(error) = update_session_settings(runtime, "code", auto_approve) {
            state.push_diagnostic(format!(
                "Cannot approve the plan until the session can enter code mode: {error}"
            ));
            return;
        }
        Some(previous)
    } else {
        None
    };
    match runtime.service.respond_callback(dispatch.params.clone()) {
        Ok(_) => {
            if let Err(error) =
                controls.accept_answer(&pending.turn_id, &pending.callback_id, choice, &dispatch)
            {
                state.push_diagnostic(format!(
                    "Server accepted the callback, but the local control state diverged: {error}"
                ));
                resync_current_projection(runtime, state);
                sync_active_callbacks(runtime, state, controls);
            }
            if let Some(auto_approve) = plan_transition {
                runtime.mode = "code".to_owned();
                runtime.auto_approve = auto_approve;
            }
            settle_callback_notice(
                state,
                &pending.callback_id,
                if callback_cancelled {
                    EntryStatus::Cancelled
                } else {
                    EntryStatus::Completed
                },
            );
            push_local_notice(state, "Callback response accepted", EntryStatus::Completed);
            resync_current_projection(runtime, state);
            sync_active_callbacks(runtime, state, controls);
        }
        Err(error) => {
            let still_pending = recover_from_callback_response_error(
                runtime,
                controls,
                state,
                &pending.callback_id,
                error,
            );
            if still_pending && let Some((mode, auto_approve)) = previous_settings {
                if let Err(error) = update_session_settings(runtime, &mode, auto_approve) {
                    state.push_diagnostic(format!(
                        "Callback retry remains open, but restoring plan mode failed: {error}"
                    ));
                } else {
                    runtime.mode = mode;
                    runtime.auto_approve = auto_approve;
                }
            } else if let Some(auto_approve) = plan_transition {
                runtime.mode = "code".to_owned();
                runtime.auto_approve = auto_approve;
                settle_callback_notice(
                    state,
                    &pending.callback_id,
                    if callback_cancelled {
                        EntryStatus::Cancelled
                    } else {
                        EntryStatus::Completed
                    },
                );
            }
        }
    }
}

fn update_session_settings(
    runtime: &mut InteractiveRuntime,
    mode: &str,
    auto_approve: bool,
) -> Result<(), vibe_app_server::client::ClientError> {
    runtime.service.public_call(
        "session/overrides/write",
        json!({
            "sessionId": runtime.session_id,
            "mode": mode,
            "autoApprove": auto_approve,
        }),
    )?;
    Ok(())
}

pub(super) fn recover_from_callback_response_error(
    runtime: &mut InteractiveRuntime,
    controls: &mut ControlState,
    state: &mut TuiState,
    callback_id: &str,
    error: impl std::fmt::Display,
) -> bool {
    let error = error.to_string();
    // The driver can fail after the server commits the response. Canonical state
    // decides whether the callback is still actionable.
    resync_current_projection(runtime, state);
    sync_active_callbacks(runtime, state, controls);
    let still_pending = controls.contains_callback(callback_id);
    state.push_diagnostic(if still_pending {
        format!("Callback response was rejected: {error}")
    } else {
        format!("Callback response settled before the driver failed: {error}")
    });
    still_pending
}

/// The auto-approval an accepted plan switches the session to, if the choice
/// accepts it.
///
/// Clearing the planning context is not read here: the plan review tool raises
/// it on the running turn, so the transcript rotates inside the turn rather
/// than between two of them.
pub(super) fn plan_transition(choice: &CallbackChoice) -> Option<bool> {
    match choice {
        CallbackChoice::Option { id } if id == "clear_auto" || id == "auto" => Some(true),
        CallbackChoice::Option { id } if id == "manual" => Some(false),
        _ => None,
    }
}

pub(super) fn plan_approval_error(state: &TuiState) -> Option<String> {
    let Some(plan) = state.plan_review.as_ref() else {
        return Some("Cannot approve the plan before its file has loaded".to_owned());
    };
    if let Some(error) = &plan.error {
        return Some(format!(
            "Cannot approve the plan until its file is readable: {error}"
        ));
    }
    if plan.content.trim().is_empty() {
        return Some("Cannot approve an empty plan".to_owned());
    }
    None
}

pub(super) fn sync_active_callbacks(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    controls: &mut ControlState,
) {
    let result = match runtime
        .service
        .public_call("session/read", json!({"sessionId": runtime.session_id}))
    {
        Ok(result) => result,
        Err(error) => {
            state.push_diagnostic(format!("Active callbacks are unavailable: {error}"));
            return;
        }
    };
    let active = result
        .get("state")
        .and_then(|state| state.get("activeCallbacks"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let entries = match serde_json::from_value::<Vec<PublicHistoryEntry>>(active) {
        Ok(entries) if entries.len() <= 1 => entries,
        Ok(_) => {
            state.push_diagnostic("Server projected more than one active callback");
            return;
        }
        Err(error) => {
            state.push_diagnostic(format!("Active callback projection is invalid: {error}"));
            return;
        }
    };
    let plan_reviews = plan_reviews(state);
    let pending_callbacks = entries
        .into_iter()
        .filter_map(|entry| {
            let plan_review = plan_reviews.get(entry.metadata().id.as_str());
            match pending_callback_from_entry(&entry, plan_review.map(PathBuf::as_path)) {
                Ok(pending) => pending,
                Err(error) => {
                    state.push_diagnostic(format!("Active callback is invalid: {error}"));
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    let active_callback_id = pending_callbacks
        .first()
        .map(|pending| pending.callback_id.as_str());
    fail_inactive_callback_notices(state, active_callback_id);
    controls.reconcile_active_callback_at(active_callback_id, unix_millis());

    for pending in pending_callbacks {
        activate_pending_callback(runtime, state, controls, pending);
    }
}

fn activate_pending_callback(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    controls: &mut ControlState,
    pending: PendingCallback,
) {
    if !activate_pending_callback_state(state, controls, pending.clone(), unix_millis()) {
        return;
    }
    if matches!(&pending.request, CallbackRequest::PlanReview { .. }) && runtime.mode != "plan" {
        state.push_diagnostic("Rejected exit-plan callback outside plan mode");
        respond_to_pending_callback(runtime, controls, &pending, CallbackChoice::Cancel, state);
    }
}

pub(super) fn activate_pending_callback_state(
    state: &mut TuiState,
    controls: &mut ControlState,
    pending: PendingCallback,
    now_ms: u64,
) -> bool {
    if controls.contains_callback(&pending.callback_id) {
        return false;
    }
    if controls.active_turn_id.is_none()
        && let Err(error) = controls.begin_turn(&pending.turn_id)
    {
        state.push_diagnostic(error.to_string());
        return false;
    }
    if let Err(error) = controls.present_callback_at(pending.clone(), now_ms) {
        state.push_diagnostic(error.to_string());
        return false;
    }
    append_callback_notice(state, &pending, &pending.prompt);
    // Reference `_show_callback`: an activated callback asks for attention.
    super::notify_attention(
        state,
        super::attention::NotificationContext::ActionRequired,
        now_ms,
    );
    true
}

/// The plan each `plan_review_started` notice opened, by the callback entry it
/// relates to.
///
/// The reference publishes a plan review as its own notice entry, so the path
/// is read from the transcript rather than from the callback, whose detail
/// declares no field for it. The relation is what keeps a stale review from
/// turning a later, unrelated question into one.
pub(super) fn plan_reviews(state: &TuiState) -> BTreeMap<String, PathBuf> {
    state
        .entries
        .iter()
        .filter_map(|entry| {
            let PublicHistoryEntry::Notice {
                metadata,
                detail: NoticeDetail::PlanReviewStarted { file_path },
                ..
            } = entry.source.server()?
            else {
                return None;
            };
            let related = metadata.related_entry_id.clone()?;
            (!file_path.is_empty()).then(|| (related, PathBuf::from(file_path)))
        })
        .collect()
}

pub(super) fn pending_callback_from_entry(
    entry: &PublicHistoryEntry,
    plan_review: Option<&Path>,
) -> Result<Option<PendingCallback>, String> {
    let PublicHistoryEntry::Callback {
        metadata,
        callback_id,
        title,
        detail,
        state: PublicCallbackState::Open,
    } = entry
    else {
        return Ok(None);
    };
    // The limit is on what the callback would occupy on the wire, so it is
    // measured on the wire form rather than on the in-memory one.
    if serde_json::to_vec(detail)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_CALLBACK_DETAIL_BYTES
    {
        return Err("callback detail exceeds the interactive safety limit".to_owned());
    }
    required_callback_text(title, "callback title")?;
    let turn_id = metadata
        .turn_id
        .clone()
        .ok_or_else(|| "active callback omitted its turn ID".to_owned())?;
    let (prompt, request) = match detail {
        CallbackDetail::Approval {
            effect,
            required_permissions,
            choices,
            ..
        } => (
            title.clone(),
            CallbackRequest::Approval {
                options: approval_options(choices)?,
                effect: callback_effect(effect, required_permissions)?,
            },
        ),
        CallbackDetail::UserInput { request, .. } => {
            let questions = callback_questions(request)?;
            let footer_note = request
                .footer_note
                .as_deref()
                .filter(|footer| !footer.is_empty())
                .map(ToOwned::to_owned);
            let prompt = callback_prompt(title, &questions, footer_note.as_deref());
            let request = match plan_review {
                Some(plan_path) => CallbackRequest::PlanReview {
                    questions,
                    footer_note,
                    plan_path: plan_path.to_path_buf(),
                },
                None => CallbackRequest::UserInput {
                    questions,
                    footer_note,
                },
            };
            (prompt, request)
        }
    };
    Ok(Some(PendingCallback {
        callback_id: callback_id.clone(),
        session_id: metadata.session_id.clone(),
        turn_id,
        prompt,
        request,
    }))
}

fn callback_effect(
    effect: &EffectDetail,
    required_permissions: &[PermissionRequirement],
) -> Result<CallbackEffect, String> {
    required_callback_text(&effect.tool_name, "approval tool name")?;
    bounded_callback_text(&effect.display.summary, "approval summary")?;
    let content = effect
        .display
        .content
        .as_deref()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| callback_effect_content(&effect.tool_name, &effect.input));
    bounded_callback_text(&content, "approval effect content")?;
    // US-105: each entry is the reference `RequiredPermission` model. The
    // terminal renders its label, which is the one field written to be read by
    // a person; the two patterns are what a client with a richer prompt shows.
    let permissions = required_permissions
        .iter()
        .map(|permission| {
            required_callback_text(&permission.label, "approval permission")?;
            Ok(permission.label.clone())
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(CallbackEffect {
        tool_name: effect.tool_name.clone(),
        summary: effect.display.summary.clone(),
        content,
        permissions,
    })
}

/// How an approval renders the call it is asking about, for the tools whose
/// arguments read better as a command line than as JSON.
fn callback_effect_content(tool_name: &str, value: &Value) -> String {
    let field = |key: &str| value.get(key).and_then(Value::as_str);
    match (tool_name, field("command"), field("file_path")) {
        ("shell" | "bash", Some(command), _) => format!("$ {command}"),
        ("read_file", _, Some(path)) => format!("Read {path}"),
        ("write_file", _, Some(path)) => {
            format!("Write {path}\n{}", field("content").unwrap_or_default())
        }
        ("edit", _, Some(path)) => format!(
            "Edit {path}\n{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        ),
        _ => match value {
            Value::String(value) => value.clone(),
            value => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        },
    }
}

/// The decisions the operator may pick from, labeled the way the reference
/// labels them. `cancel_turn` is offered through the interrupt shortcut rather
/// than as a list entry, so it never appears here.
fn approval_options(choices: &[ApprovalDecisionType]) -> Result<Vec<CallbackOption>, String> {
    if choices.is_empty() || choices.len() > MAX_CALLBACK_OPTIONS {
        return Err("approval callback has an invalid choice count".to_owned());
    }
    Ok(choices
        .iter()
        .filter_map(|choice| {
            let (id, label, description) = match choice {
                ApprovalDecisionType::Approve => {
                    ("approve", "Allow once", "Approve this invocation")
                }
                ApprovalDecisionType::ApproveForSession => (
                    "approve_for_session",
                    "Allow for session",
                    "Approve matching requests for this session",
                ),
                ApprovalDecisionType::ApprovePermanently => (
                    "approve_permanently",
                    "Always allow",
                    "Persist approval for matching requests",
                ),
                ApprovalDecisionType::Deny => ("deny", "Deny", "Reject this invocation"),
                ApprovalDecisionType::CancelTurn => return None,
            };
            Some(CallbackOption {
                id: id.to_owned(),
                label: label.to_owned(),
                description: description.to_owned(),
            })
        })
        .collect())
}

fn callback_questions(request: &UserQuestionRequest) -> Result<Vec<CallbackQuestion>, String> {
    if request.questions.is_empty() || request.questions.len() > MAX_CALLBACK_QUESTIONS {
        return Err("user-input callback has an invalid question count".to_owned());
    }
    request
        .questions
        .iter()
        .enumerate()
        .map(|(question_index, question)| {
            required_callback_text(&question.question, "callback question")?;
            bounded_callback_text(&question.header, "callback question header")?;
            if question.options.is_empty() || question.options.len() > MAX_CALLBACK_OPTIONS {
                return Err("callback question has an invalid option count".to_owned());
            }
            let options = question
                .options
                .iter()
                .enumerate()
                .map(|(option_index, option)| {
                    required_callback_text(&option.label, "callback option label")?;
                    bounded_callback_text(&option.description, "callback option description")?;
                    // The published choice carries no identity, so the plan
                    // review's four fixed answers keep the ids the session
                    // settles a plan decision by, and every other option is
                    // addressed by its position.
                    let id = plan_option_id(&option.label).map_or_else(
                        || format!("q{question_index}-o{option_index}"),
                        ToOwned::to_owned,
                    );
                    Ok(CallbackOption {
                        id,
                        label: option.label.clone(),
                        description: option.description.clone(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if options.iter().enumerate().any(|(index, option)| {
                options[..index]
                    .iter()
                    .any(|previous| previous.id == option.id)
            }) {
                return Err("callback question contains duplicate option IDs".to_owned());
            }
            Ok(CallbackQuestion {
                header: question.header.clone(),
                question: question.question.clone(),
                options,
                allows_free_text: !question.hide_other,
                multi_select: question.multi_select,
            })
        })
        .collect()
}

fn callback_prompt(
    title: &str,
    questions: &[CallbackQuestion],
    footer_note: Option<&str>,
) -> String {
    let mut prompt = title.to_owned();
    for (index, question) in questions.iter().enumerate() {
        prompt.push_str(&format!(
            "\n{}. {}{}",
            index + 1,
            if question.header.is_empty() {
                String::new()
            } else {
                format!("{}: ", question.header)
            },
            question.question
        ));
        for (option_index, option) in question.options.iter().enumerate() {
            prompt.push_str(&format!(
                "\n   {}. {}{}",
                option_index + 1,
                option.label,
                if option.description.is_empty() {
                    String::new()
                } else {
                    format!(": {}", option.description)
                }
            ));
        }
        if question.allows_free_text {
            prompt.push_str("\n   Other: enter free text");
        }
    }
    if let Some(footer) = footer_note.filter(|footer| !footer.is_empty()) {
        prompt.push('\n');
        prompt.push_str(footer);
    }
    prompt
}

fn bounded_callback_text(value: &str, label: &str) -> Result<(), String> {
    if value.len() > MAX_CALLBACK_TEXT_BYTES {
        return Err(format!("{label} exceeds the safety limit"));
    }
    Ok(())
}

fn required_callback_text(value: &str, label: &str) -> Result<(), String> {
    bounded_callback_text(value, label)?;
    if value.trim().is_empty() {
        return Err(format!("{label} is empty"));
    }
    Ok(())
}

fn plan_option_id(label: &str) -> Option<&'static str> {
    match label {
        "Yes, clear context and auto approve edits" => Some("clear_auto"),
        "Yes, and auto approve edits" => Some("auto"),
        "Yes, and request approval for edits" => Some("manual"),
        "No" => Some("no"),
        _ => None,
    }
}

pub(super) fn sync_callback_presentation(
    controls: &ControlState,
    state: &mut TuiState,
    now_ms: u64,
) {
    let mut presentation = controls.callback_presentation(now_ms);
    if !controls
        .pending_callback()
        .is_some_and(|pending| matches!(&pending.request, CallbackRequest::PlanReview { .. }))
    {
        state.plan_review = None;
        state.set_callback_presentation(presentation);
        return;
    }
    if let (Some(plan), Some(presentation)) = (&state.plan_review, presentation.as_mut()) {
        presentation.lines.push(String::new());
        presentation
            .lines
            .push(format!("Plan file: {}", plan.path.display()));
        if let Some(error) = &plan.error {
            presentation
                .lines
                .push(format!("Plan could not be refreshed: {error}"));
        } else if plan.content.is_empty() {
            presentation.lines.push("(plan is empty)".to_owned());
        } else {
            presentation
                .lines
                .extend(plan.content.lines().map(ToOwned::to_owned));
        }
        presentation
            .lines
            .push("PgUp/PgDn inspect  Ctrl+G edit the live plan".to_owned());
    }
    state.set_callback_presentation(presentation);
}

fn append_callback_notice(
    state: &mut TuiState,
    pending: &PendingCallback,
    message: &str,
) -> String {
    state.append_local(TranscriptEntry {
        id: String::new(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: message.to_owned(),
        status: EntryStatus::Streaming,
        source: EntrySource::tracking(&pending.callback_id),
    })
}

fn settle_callback_notice(state: &mut TuiState, callback_id: &str, status: EntryStatus) {
    let notices = state
        .entries
        .iter()
        .filter(|entry| {
            entry.status == EntryStatus::Streaming
                && entry.source.tracked_callback() == Some(callback_id)
        })
        .map(|entry| (entry.id.clone(), entry.text.clone()))
        .collect::<Vec<_>>();
    for (entry_id, text) in notices {
        let _ = state.update_local(&entry_id, text, status);
    }
}

pub(super) fn fail_inactive_callback_notices(
    state: &mut TuiState,
    active_callback_id: Option<&str>,
) {
    let inactive = state
        .entries
        .iter()
        .filter_map(|entry| {
            let callback_id = entry.source.tracked_callback()?;
            (entry.status == EntryStatus::Streaming && active_callback_id != Some(callback_id))
                .then(|| callback_id.to_owned())
        })
        .collect::<Vec<_>>();
    for callback_id in inactive {
        settle_callback_notice(state, &callback_id, EntryStatus::Failed);
    }
}

pub(super) fn cancel_open_callback_notices(state: &mut TuiState) {
    settle_open_callback_notices(state, EntryStatus::Cancelled);
}

pub(super) fn fail_open_callback_notices(state: &mut TuiState) {
    settle_open_callback_notices(state, EntryStatus::Failed);
}

fn settle_open_callback_notices(state: &mut TuiState, status: EntryStatus) {
    let callbacks = state
        .entries
        .iter()
        .filter_map(|entry| {
            (entry.status == EntryStatus::Streaming)
                .then(|| entry.source.tracked_callback())
                .flatten()
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    for callback_id in callbacks {
        settle_callback_notice(state, &callback_id, status);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use vibe_app_server::client::PublicHistoryEntry;

    use super::super::controls::ControlFocus;
    use super::super::hydration::canonical_session_projection;
    use super::super::runtime::interactive_test_runtime;
    use super::super::state::PlanReviewState;
    use super::*;

    #[test]
    fn plan_review_choices_and_callback_bounds_are_exact() {
        assert_eq!(
            plan_option_id("Yes, clear context and auto approve edits"),
            Some("clear_auto")
        );
        assert_eq!(
            plan_transition(&CallbackChoice::Option {
                id: "manual".to_owned()
            }),
            Some(false)
        );
        // The clearing choice differs from the plain one only in what the tool
        // raises on the turn; both switch the session to auto approval.
        assert_eq!(
            plan_transition(&CallbackChoice::Option {
                id: "clear_auto".to_owned()
            }),
            Some(true)
        );
        assert_eq!(
            plan_transition(&CallbackChoice::FreeText {
                value: "Revise it".to_owned()
            }),
            None
        );
        assert!(
            required_callback_text(&"x".repeat(MAX_CALLBACK_TEXT_BYTES + 1), "callback").is_err()
        );
        assert_eq!(
            callback_effect_content("shell", &json!({"command": "cargo test"})),
            "$ cargo test"
        );
    }

    /// The reference publishes a plan review as its own notice, so the callback
    /// detail carries no marker and the open notice is what turns the same
    /// user-input question into a plan review.
    #[test]
    fn the_plan_review_notice_decides_what_a_user_input_callback_is() {
        let entry = serde_json::from_value::<PublicHistoryEntry>(json!({
            "type": "callback",
            "id": "callback-entry",
            "sessionId": "session",
            "turnId": "turn",
            "createdAt": 1,
            "updatedAt": 1,
            "generationStatus": "in_progress",
            "callbackId": "callback-1",
            "title": "Review plan",
            "detail": {
                "kind": "user_input",
                "request": {
                    "questions": [{
                        "header": "Plan",
                        "question": "Approve?",
                        "options": [
                            {"label": "Yes", "description": "Approve plan"},
                            {"label": "No", "description": "Keep planning"}
                        ],
                        "multiSelect": false,
                        "hideOther": true
                    }]
                }
            },
            "state": {"status": "open"}
        }))
        .expect("callback fixture");

        let plain = pending_callback_from_entry(&entry, None)
            .expect("callback is valid")
            .expect("callback is active");
        assert!(matches!(plain.request, CallbackRequest::UserInput { .. }));

        let reviewing = pending_callback_from_entry(&entry, Some(Path::new("/tmp/plan.md")))
            .expect("callback is valid")
            .expect("callback is active");
        let CallbackRequest::PlanReview { plan_path, .. } = &reviewing.request else {
            panic!("expected a plan-review callback");
        };
        assert_eq!(plan_path, Path::new("/tmp/plan.md"));
    }

    /// The relation is what stops a plan review from leaking onto the next
    /// question the session asks.
    #[test]
    fn a_plan_review_notice_only_reaches_the_callback_it_relates_to() {
        let mut state = TuiState::new("session");
        assert!(plan_reviews(&state).is_empty());

        state.entries.push(crate::tui::hydration::history_entry(
            PublicHistoryEntry::Notice {
                metadata: vibe_app_server::client::PublicEntryMetadata {
                    id: "notice:callback-1:plan-review".to_owned(),
                    session_id: "session".to_owned(),
                    turn_id: None,
                    created_at: 1,
                    updated_at: 1,
                    generation_status:
                        vibe_app_server::client::PublicEntryGenerationStatus::Completed,
                    related_entry_id: Some("callback:callback-1".to_owned()),
                },
                level: vibe_app_server::client::PublicNoticeLevel::Info,
                message: "Review plan".to_owned(),
                detail: NoticeDetail::PlanReviewStarted {
                    file_path: "/tmp/plan.md".to_owned(),
                },
            },
        ));

        let reviews = plan_reviews(&state);
        assert_eq!(
            reviews.get("callback:callback-1"),
            Some(&PathBuf::from("/tmp/plan.md"))
        );
        assert_eq!(reviews.get("callback:callback-2"), None);
    }

    #[test]
    fn callback_absence_fails_an_unsettled_notice_closed() {
        let mut state = TuiState::new("session");
        state.append_local(TranscriptEntry {
            id: String::new(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: "approval pending".to_owned(),
            status: EntryStatus::Streaming,
            source: EntrySource::tracking("callback"),
        });

        fail_inactive_callback_notices(&mut state, None);

        assert_eq!(state.entries[0].status, EntryStatus::Failed);
    }

    #[test]
    fn canonical_multi_question_callbacks_preserve_structure() {
        let entry = serde_json::from_value::<PublicHistoryEntry>(json!({
            "type": "callback",
            "id": "callback-entry",
            "sessionId": "session",
            "turnId": "turn",
            "createdAt": 1,
            "updatedAt": 1,
            "generationStatus": "in_progress",
            "callbackId": "callback-1",
            "title": "Need input\u{1b}[31m",
            "detail": {
                "kind": "user_input",
                "request": {
                    "questions": [
                        {
                            "header": "Runtime",
                            "question": "Choose runtimes",
                            "options": [
                                {"label": "Rust", "description": "Native"},
                                {"label": "Python", "description": "Portable"}
                            ],
                            "multiSelect": true,
                            "hideOther": true
                        },
                        {
                            "header": "Constraint",
                            "question": "Any constraints? \u{4e16}\u{754c}",
                            "options": [
                                {"label": "Fast", "description": ""},
                                {"label": "Small", "description": ""}
                            ],
                            "multiSelect": false,
                            "hideOther": false
                        }
                    ],
                    "footerNote": "All answers are submitted together."
                }
            },
            "state": {"status": "open"}
        }))
        .expect("callback fixture");
        let pending = pending_callback_from_entry(&entry, None)
            .expect("callback is valid")
            .expect("callback is active");
        let CallbackRequest::UserInput { questions, .. } = &pending.request else {
            panic!("expected user-input callback");
        };
        assert_eq!(questions.len(), 2);
        assert!(questions[0].multi_select);
        assert!(
            pending
                .prompt
                .contains("All answers are submitted together.")
        );
    }

    #[test]
    fn plan_approval_fails_closed_until_the_live_file_is_readable_and_nonempty() {
        let mut state = TuiState::new("session");
        assert!(plan_approval_error(&state).is_some());
        state.plan_review = Some(PlanReviewState {
            path: PathBuf::from("plan.md"),
            content: String::new(),
            error: Some("missing".to_owned()),
        });
        assert!(plan_approval_error(&state).is_some());
        state.plan_review = Some(PlanReviewState {
            path: PathBuf::from("plan.md"),
            content: "# Ready".to_owned(),
            error: None,
        });
        assert_eq!(plan_approval_error(&state), None);
    }

    #[tokio::test]
    async fn plan_callback_stays_open_when_code_mode_cannot_be_committed() {
        let mut runtime = interactive_test_runtime("plan-settings-failure");
        runtime.mode = "plan".to_owned();
        let session_id = runtime.session_id.clone();
        runtime
            .service
            .close_session(&session_id)
            .await
            .expect("session closes");
        let mut state = TuiState::new(&session_id);
        state.plan_review = Some(PlanReviewState {
            path: PathBuf::from("plan.md"),
            content: "# Ready".to_owned(),
            error: None,
        });
        let mut controls = ControlState::new(&session_id);
        controls.begin_turn("turn").expect("turn begins");
        let pending = PendingCallback {
            callback_id: "plan-callback".to_owned(),
            session_id,
            turn_id: "turn".to_owned(),
            prompt: "Approve plan?".to_owned(),
            request: CallbackRequest::PlanReview {
                questions: vec![CallbackQuestion {
                    header: "Plan".to_owned(),
                    question: "Approve?".to_owned(),
                    options: vec![CallbackOption {
                        id: "manual".to_owned(),
                        label: "Yes, and request approval for edits".to_owned(),
                        description: String::new(),
                    }],
                    allows_free_text: true,
                    multi_select: false,
                }],
                footer_note: None,
                plan_path: PathBuf::from("plan.md"),
            },
        };
        controls
            .present_callback(pending.clone())
            .expect("plan callback presents");

        respond_to_pending_callback(
            &mut runtime,
            &mut controls,
            &pending,
            CallbackChoice::Option {
                id: "manual".to_owned(),
            },
            &mut state,
        );

        assert_eq!(runtime.mode, "plan");
        assert!(controls.contains_callback("plan-callback"));
        assert!(
            state
                .diagnostics()
                .any(|message| message.contains("Cannot approve the plan"))
        );
    }

    #[test]
    fn committed_callback_driver_error_reconciles_stale_controls() {
        let mut runtime = interactive_test_runtime("callback-race-session");
        let session_id = runtime.session_id.clone();
        let mut state = canonical_session_projection(&mut runtime, &session_id, false)
            .expect("initial projection");
        let mut controls = ControlState::new(&session_id);
        controls.begin_turn("turn").expect("turn begins");
        controls
            .present_callback(PendingCallback {
                callback_id: "committed-callback".to_owned(),
                session_id,
                turn_id: "turn".to_owned(),
                prompt: "Approve?".to_owned(),
                request: CallbackRequest::Approval {
                    options: Vec::new(),
                    effect: CallbackEffect {
                        tool_name: "test".to_owned(),
                        summary: String::new(),
                        content: String::new(),
                        permissions: Vec::new(),
                    },
                },
            })
            .expect("local callback presents");
        assert_eq!(controls.focus, ControlFocus::Callback);

        recover_from_callback_response_error(
            &mut runtime,
            &mut controls,
            &mut state,
            "committed-callback",
            "driver failed after commit",
        );

        assert!(controls.pending_callback().is_none());
        assert_eq!(controls.focus, ControlFocus::Prompt);
        assert!(
            state
                .diagnostics()
                .any(|diagnostic| diagnostic.contains("driver failed after commit"))
        );
    }
}
