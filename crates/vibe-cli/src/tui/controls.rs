use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

mod interaction;
mod response;

use interaction::{approval_input, question_input, render_question_lines};
use response::{callback_params, validate_choice};

pub const CALLBACK_INPUT_GRACE_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    Once,
    Session,
    Permanent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallbackChoice {
    Approve { scope: ApprovalScope },
    Deny { scope: ApprovalScope },
    Cancel,
    Option { id: String },
    Options { ids: Vec<String> },
    FreeText { value: String },
    UserInput { answers: Vec<UserInputChoice> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserInputChoice {
    Option { id: String },
    Options { ids: Vec<String> },
    Combined { ids: Vec<String>, other: String },
    FreeText { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackOption {
    pub id: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<CallbackOption>,
    pub allows_free_text: bool,
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingCallback {
    pub callback_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub prompt: String,
    pub request: CallbackRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackEffect {
    pub tool_name: String,
    pub summary: String,
    pub content: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallbackRequest {
    Approval {
        options: Vec<CallbackOption>,
        effect: CallbackEffect,
    },
    UserInput {
        questions: Vec<CallbackQuestion>,
        footer_note: Option<String>,
    },
    PlanReview {
        questions: Vec<CallbackQuestion>,
        footer_note: Option<String>,
        plan_path: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackInput {
    Up,
    Down,
    PreviousQuestion,
    NextQuestion,
    Select,
    Cancel,
    Shortcut(usize),
    Character(char),
    Backspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackInputOutcome {
    Ignored,
    Updated,
    GraceBlocked,
    Invalid(String),
    Submit(CallbackChoice),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackPresentation {
    pub callback_id: String,
    pub lines: Vec<String>,
    pub focus_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFocus {
    Prompt,
    Callback,
    Plan,
    SessionPicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlState {
    pub session_id: String,
    pub active_turn_id: Option<String>,
    pub focus: ControlFocus,
    pub waiting: bool,
    pub notifications: Vec<String>,
    pending: Option<PendingCallback>,
    answered: BTreeMap<String, AnsweredCallback>,
    interaction: Option<CallbackInteraction>,
    activated_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnsweredCallback {
    choice: CallbackChoice,
    params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CallbackInteraction {
    Approval { selected: usize },
    Questions(QuestionInteraction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionInteraction {
    current: usize,
    cursors: Vec<usize>,
    selections: Vec<BTreeSet<usize>>,
    other_text: Vec<String>,
    answers: Vec<Option<UserInputChoice>>,
}

impl ControlState {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            active_turn_id: None,
            focus: ControlFocus::Prompt,
            waiting: false,
            notifications: Vec::new(),
            pending: None,
            answered: BTreeMap::new(),
            interaction: None,
            activated_at_ms: None,
        }
    }

    pub fn begin_turn(&mut self, turn_id: impl Into<String>) -> Result<(), ControlError> {
        if self.active_turn_id.is_some() {
            return Err(ControlError::TurnAlreadyActive);
        }
        self.active_turn_id = Some(turn_id.into());
        self.waiting = true;
        Ok(())
    }

    pub fn present_callback(&mut self, callback: PendingCallback) -> Result<(), ControlError> {
        self.present_callback_at(callback, 0)
    }

    pub fn present_callback_at(
        &mut self,
        callback: PendingCallback,
        now_ms: u64,
    ) -> Result<(), ControlError> {
        if callback.session_id != self.session_id {
            return Err(ControlError::ForeignSession(callback.session_id));
        }
        if self.active_turn_id.as_deref() != Some(&callback.turn_id) {
            return Err(ControlError::StaleTurn(callback.turn_id));
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.callback_id == callback.callback_id)
            || self.answered.contains_key(&callback.callback_id)
        {
            return Err(ControlError::DuplicateCallback(callback.callback_id));
        }
        if let Some(active) = &self.pending {
            return Err(ControlError::CallbackAlreadyActive(
                active.callback_id.clone(),
            ));
        }
        self.waiting = true;
        self.pending = Some(callback);
        self.activate(now_ms);
        Ok(())
    }

    pub fn answer(
        &mut self,
        turn_id: &str,
        callback_id: &str,
        choice: CallbackChoice,
    ) -> Result<CallbackDispatch, ControlError> {
        let dispatch = self.prepare_answer(turn_id, callback_id, &choice)?;
        self.accept_answer(turn_id, callback_id, choice, &dispatch)?;
        Ok(dispatch)
    }

    pub fn prepare_answer(
        &self,
        turn_id: &str,
        callback_id: &str,
        choice: &CallbackChoice,
    ) -> Result<CallbackDispatch, ControlError> {
        if self.active_turn_id.as_deref() != Some(turn_id) {
            return Err(ControlError::StaleTurn(turn_id.to_owned()));
        }
        if let Some(previous) = self.answered.get(callback_id) {
            if previous.choice == *choice {
                return Ok(CallbackDispatch {
                    method: "callback/respond",
                    params: previous.params.clone(),
                    retry: true,
                });
            }
            return Err(ControlError::ConflictingAnswer(callback_id.to_owned()));
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| ControlError::StaleCallback(callback_id.to_owned()))?;
        if pending.callback_id != callback_id {
            return Err(ControlError::StaleCallback(callback_id.to_owned()));
        }
        validate_choice(pending, choice)?;
        let params = callback_params(&self.session_id, callback_id, pending, choice)?;
        Ok(CallbackDispatch {
            method: "callback/respond",
            params,
            retry: false,
        })
    }

    pub fn accept_answer(
        &mut self,
        turn_id: &str,
        callback_id: &str,
        choice: CallbackChoice,
        dispatch: &CallbackDispatch,
    ) -> Result<(), ControlError> {
        let prepared = self.prepare_answer(turn_id, callback_id, &choice)?;
        if prepared != *dispatch {
            return Err(ControlError::InvalidChoice);
        }
        if prepared.retry {
            return Ok(());
        }
        let active = self
            .pending
            .as_ref()
            .ok_or_else(|| ControlError::StaleCallback(callback_id.to_owned()))?;
        if active.callback_id != callback_id {
            return Err(ControlError::StaleCallback(callback_id.to_owned()));
        }
        self.deactivate();
        self.answered.insert(
            callback_id.to_owned(),
            AnsweredCallback {
                choice,
                params: prepared.params,
            },
        );
        self.waiting = true;
        Ok(())
    }

    #[must_use]
    pub fn pending_callback(&self) -> Option<&PendingCallback> {
        self.pending.as_ref()
    }

    #[must_use]
    pub fn contains_callback(&self, callback_id: &str) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.callback_id == callback_id)
            || self.answered.contains_key(callback_id)
    }

    pub fn reconcile_active_callback(&mut self, active_callback_id: Option<&str>) {
        self.reconcile_active_callback_at(active_callback_id, 0);
    }

    pub fn reconcile_active_callback_at(&mut self, active_callback_id: Option<&str>, now_ms: u64) {
        let retained = self
            .pending
            .as_ref()
            .is_some_and(|pending| active_callback_id == Some(pending.callback_id.as_str()));
        if !retained {
            self.deactivate();
        } else if self.interaction.is_none() {
            self.activate(now_ms);
        }
    }

    pub fn interrupt(&mut self) -> Result<ControlDispatch, ControlError> {
        let turn_id = self
            .active_turn_id
            .as_ref()
            .ok_or(ControlError::NoActiveTurn)?
            .clone();
        self.deactivate();
        self.waiting = true;
        Ok(ControlDispatch {
            method: "turn/interrupt",
            params: json!({
                "sessionId": self.session_id,
                "expectedTurnId": turn_id,
            }),
        })
    }

    pub fn complete_turn(&mut self, turn_id: &str, message: impl Into<String>) {
        if self.active_turn_id.as_deref() != Some(turn_id) {
            return;
        }
        self.active_turn_id = None;
        self.deactivate();
        self.answered.clear();
        self.waiting = false;
        self.notifications.push(message.into());
    }

    #[must_use]
    pub fn input_is_ready(&self, now_ms: u64) -> bool {
        self.activated_at_ms
            .is_some_and(|activated| now_ms.saturating_sub(activated) >= CALLBACK_INPUT_GRACE_MS)
    }

    #[must_use]
    pub fn accepts_free_text(&self) -> bool {
        let (Some(pending), Some(CallbackInteraction::Questions(interaction))) =
            (self.pending.as_ref(), self.interaction.as_ref())
        else {
            return false;
        };
        let questions = match &pending.request {
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. } => questions,
            CallbackRequest::Approval { .. } => return false,
        };
        questions.get(interaction.current).is_some_and(|question| {
            question.allows_free_text
                && interaction.cursors[interaction.current] == question.options.len()
        })
    }

    pub fn apply_callback_input(
        &mut self,
        input: CallbackInput,
        now_ms: u64,
    ) -> CallbackInputOutcome {
        let Some(pending) = self.pending.as_ref() else {
            return CallbackInputOutcome::Ignored;
        };
        let guarded = matches!(
            input,
            CallbackInput::Select | CallbackInput::Cancel | CallbackInput::Shortcut(_)
        );
        if guarded && !self.input_is_ready(now_ms) {
            return CallbackInputOutcome::GraceBlocked;
        }
        match (&pending.request, self.interaction.as_mut()) {
            (
                CallbackRequest::Approval { options, .. },
                Some(CallbackInteraction::Approval { selected }),
            ) => approval_input(options, selected, input),
            (
                CallbackRequest::UserInput { questions, .. }
                | CallbackRequest::PlanReview { questions, .. },
                Some(CallbackInteraction::Questions(interaction)),
            ) => question_input(questions, interaction, input),
            (_, None) => CallbackInputOutcome::Ignored,
            _ => CallbackInputOutcome::Invalid(
                "Callback interaction does not match its typed request".to_owned(),
            ),
        }
    }

    #[must_use]
    pub fn callback_presentation(&self, now_ms: u64) -> Option<CallbackPresentation> {
        let pending = self.pending.as_ref()?;
        let mut lines = vec![pending.prompt.clone()];
        if let CallbackRequest::Approval { effect, .. } = &pending.request {
            lines.push(format!("Tool: {}", effect.tool_name));
            if !effect.summary.is_empty() {
                lines.push(effect.summary.clone());
            }
            if !effect.content.is_empty() {
                lines.extend(effect.content.lines().map(ToOwned::to_owned));
            }
            if !effect.permissions.is_empty() {
                lines.push(format!("Permissions: {}", effect.permissions.join(", ")));
            }
        }
        let focus_line = match (&pending.request, self.interaction.as_ref()) {
            (
                CallbackRequest::Approval { options, .. },
                Some(CallbackInteraction::Approval { selected }),
            ) => {
                let focus_line = lines.len().saturating_add(*selected);
                lines.extend(options.iter().enumerate().map(|(index, option)| {
                    format!(
                        "{}{}. {}{}",
                        if index == *selected { "› " } else { "  " },
                        index + 1,
                        option.label,
                        if option.description.is_empty() {
                            String::new()
                        } else {
                            format!(" - {}", option.description)
                        }
                    )
                }));
                lines.push("↑↓/jk navigate  Enter select  Esc deny".to_owned());
                focus_line
            }
            (
                CallbackRequest::UserInput {
                    questions,
                    footer_note,
                }
                | CallbackRequest::PlanReview {
                    questions,
                    footer_note,
                    ..
                },
                Some(CallbackInteraction::Questions(interaction)),
            ) => render_question_lines(questions, footer_note.as_deref(), interaction, &mut lines),
            (_, None) => 0,
            _ => {
                lines.push("Callback interaction is inconsistent".to_owned());
                0
            }
        };
        if !self.input_is_ready(now_ms) {
            lines.push("Input is briefly locked to ignore stale typing".to_owned());
        }
        Some(CallbackPresentation {
            callback_id: pending.callback_id.clone(),
            lines,
            focus_line,
        })
    }

    fn activate(&mut self, now_ms: u64) {
        let Some(pending) = self.pending.as_ref() else {
            self.deactivate();
            return;
        };
        self.focus = match &pending.request {
            CallbackRequest::PlanReview { .. } => ControlFocus::Plan,
            CallbackRequest::Approval { .. } | CallbackRequest::UserInput { .. } => {
                ControlFocus::Callback
            }
        };
        self.interaction = Some(match &pending.request {
            CallbackRequest::Approval { .. } => CallbackInteraction::Approval { selected: 0 },
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. } => {
                CallbackInteraction::Questions(QuestionInteraction::new(questions))
            }
        });
        self.activated_at_ms = Some(now_ms);
    }

    fn deactivate(&mut self) {
        self.pending = None;
        self.interaction = None;
        self.activated_at_ms = None;
        if matches!(self.focus, ControlFocus::Callback | ControlFocus::Plan) {
            self.focus = ControlFocus::Prompt;
        }
    }

    #[must_use]
    pub fn session_command(&self, command: SessionCommand) -> ControlDispatch {
        command.dispatch(&self.session_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    Rewind,
    Clear,
    Compact,
    Fork,
    Resume,
    Continue,
    Rename,
    Close,
    History,
}

impl SessionCommand {
    fn dispatch(self, session_id: &str) -> ControlDispatch {
        let (method, params) = match self {
            Self::Rewind => (
                "session/rewind",
                json!({"sessionId": session_id, "keepMessages": 0}),
            ),
            Self::Clear => ("session/history/clear", json!({"sessionId": session_id})),
            Self::Compact => ("session/compact/start", json!({"sessionId": session_id})),
            Self::Fork => ("session/fork", json!({"sessionId": session_id})),
            Self::Resume => ("session/resume", json!({"sessionId": session_id})),
            Self::Continue => ("session/continue", json!({})),
            Self::Rename => (
                "session/title/update",
                json!({"sessionId": session_id, "title": ""}),
            ),
            Self::Close => ("session/close", json!({"sessionId": session_id})),
            Self::History => ("session/list", json!({"offset": 0, "limit": 50})),
        };
        ControlDispatch { method, params }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControlDispatch {
    pub method: &'static str,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallbackDispatch {
    pub method: &'static str,
    pub params: Value,
    pub retry: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ControlError {
    #[error("another turn is already active")]
    TurnAlreadyActive,
    #[error("no turn is active")]
    NoActiveTurn,
    #[error("callback belongs to foreign session `{0}`")]
    ForeignSession(String),
    #[error("turn `{0}` is stale")]
    StaleTurn(String),
    #[error("callback `{0}` is stale")]
    StaleCallback(String),
    #[error("callback `{0}` is already active")]
    CallbackAlreadyActive(String),
    #[error("callback `{0}` was already presented")]
    DuplicateCallback(String),
    #[error("callback `{0}` already has a different answer")]
    ConflictingAnswer(String),
    #[error("callback choice is not valid for the pending request")]
    InvalidChoice,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(id: &str, turn: &str) -> PendingCallback {
        PendingCallback {
            callback_id: id.to_owned(),
            session_id: "session".to_owned(),
            turn_id: turn.to_owned(),
            prompt: "Run command?".to_owned(),
            request: CallbackRequest::Approval {
                options: vec![
                    CallbackOption {
                        id: "approve".to_owned(),
                        label: "Allow once".to_owned(),
                        description: String::new(),
                    },
                    CallbackOption {
                        id: "approve_for_session".to_owned(),
                        label: "Allow for session".to_owned(),
                        description: String::new(),
                    },
                    CallbackOption {
                        id: "approve_permanently".to_owned(),
                        label: "Always allow".to_owned(),
                        description: String::new(),
                    },
                    CallbackOption {
                        id: "deny".to_owned(),
                        label: "Deny".to_owned(),
                        description: String::new(),
                    },
                ],
                effect: CallbackEffect {
                    tool_name: "shell".to_owned(),
                    summary: String::new(),
                    content: String::new(),
                    permissions: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn identical_callback_answers_are_retry_safe_but_conflicts_fail() {
        let mut state = ControlState::new("session");
        state.begin_turn("turn").expect("turn starts");
        state
            .present_callback(callback("callback", "turn"))
            .expect("callback presents");
        let choice = CallbackChoice::Approve {
            scope: ApprovalScope::Once,
        };
        assert!(
            !state
                .answer("turn", "callback", choice.clone())
                .expect("first answer")
                .retry
        );
        let dispatch = state
            .answer("turn", "callback", choice.clone())
            .expect("retry shape");
        assert_eq!(
            dispatch.params,
            json!({
                "sessionId": "session",
                "callbackId": "callback",
                "output": {
                    "type": "approval",
                    "decision": {"type": "approve"}
                }
            })
        );
        assert!(
            state
                .answer("turn", "callback", choice)
                .expect("retry")
                .retry
        );
        assert!(matches!(
            state.answer(
                "turn",
                "callback",
                CallbackChoice::Deny {
                    scope: ApprovalScope::Once,
                }
            ),
            Err(ControlError::ConflictingAnswer(_))
        ));
    }

    #[test]
    fn interruption_drops_callbacks_before_input_can_reach_a_later_turn() {
        let mut state = ControlState::new("session");
        state.begin_turn("turn-1").expect("turn starts");
        state
            .present_callback(callback("callback", "turn-1"))
            .expect("callback presents");
        let dispatch = state.interrupt().expect("interrupt dispatches");
        assert_eq!(dispatch.method, "turn/interrupt");
        state.complete_turn("turn-1", "Interrupted");
        state.begin_turn("turn-2").expect("next turn starts");
        assert!(matches!(
            state.answer(
                "turn-2",
                "callback",
                CallbackChoice::Approve {
                    scope: ApprovalScope::Once,
                }
            ),
            Err(ControlError::StaleCallback(_))
        ));
    }

    #[test]
    fn canonical_callback_reconciliation_drops_stale_pending_state_and_focus() {
        let mut state = ControlState::new("session");
        state.begin_turn("turn").expect("turn starts");
        state
            .present_callback(callback("stale", "turn"))
            .expect("callback presents");

        state.reconcile_active_callback(None);

        assert!(state.pending_callback().is_none());
        assert!(!state.contains_callback("stale"));
        assert_eq!(state.focus, ControlFocus::Prompt);
        assert!(state.waiting);
    }

    #[test]
    fn canonical_callback_reconciliation_preserves_answered_retry_records() {
        let mut state = ControlState::new("session");
        state.begin_turn("turn").expect("turn starts");
        state
            .present_callback(callback("answered", "turn"))
            .expect("callback presents");
        let choice = CallbackChoice::Approve {
            scope: ApprovalScope::Once,
        };
        state
            .answer("turn", "answered", choice.clone())
            .expect("answer records");

        state.reconcile_active_callback(None);

        let retry = state
            .prepare_answer("turn", "answered", &choice)
            .expect("identical answer remains retry-safe");
        assert!(retry.retry);
        assert_eq!(state.focus, ControlFocus::Prompt);
    }

    #[test]
    fn canonical_callback_reconciliation_retains_the_active_pending_callback() {
        let mut state = ControlState::new("session");
        state.begin_turn("turn").expect("turn starts");
        state
            .present_callback(callback("active", "turn"))
            .expect("callback presents");

        state.reconcile_active_callback(Some("active"));

        assert_eq!(
            state
                .pending_callback()
                .map(|callback| callback.callback_id.as_str()),
            Some("active")
        );
        assert_eq!(state.focus, ControlFocus::Callback);
    }

    #[test]
    fn canonical_server_advance_activates_only_one_callback_at_a_time() {
        let mut state = ControlState::new("session");
        state.begin_turn("turn").expect("turn starts");
        state
            .present_callback_at(callback("first", "turn"), 1_000)
            .expect("first callback presents");
        assert!(matches!(
            state.present_callback_at(callback("second", "turn"), 1_010),
            Err(ControlError::CallbackAlreadyActive(id)) if id == "first"
        ));
        state
            .answer(
                "turn",
                "first",
                CallbackChoice::Approve {
                    scope: ApprovalScope::Once,
                },
            )
            .expect("first callback answers");
        state.reconcile_active_callback_at(None, 1_500);
        state
            .present_callback_at(callback("second", "turn"), 2_000)
            .expect("server advances to second callback");

        assert_eq!(
            state.apply_callback_input(CallbackInput::Select, 2_499),
            CallbackInputOutcome::GraceBlocked
        );
        assert_eq!(
            state.apply_callback_input(CallbackInput::Select, 2_500),
            CallbackInputOutcome::Submit(CallbackChoice::Approve {
                scope: ApprovalScope::Once,
            })
        );
    }

    #[test]
    fn session_controls_are_canonical_server_requests() {
        let state = ControlState::new("session");
        assert_eq!(
            state.session_command(SessionCommand::Compact).method,
            "session/compact/start"
        );
        assert_eq!(
            state.session_command(SessionCommand::Fork).params["sessionId"],
            "session"
        );
    }

    #[test]
    fn multi_question_output_matches_the_canonical_user_input_envelope() {
        let questions = vec![
            CallbackQuestion {
                header: "Runtime".to_owned(),
                question: "Choose runtimes".to_owned(),
                options: vec![
                    CallbackOption {
                        id: "rust".to_owned(),
                        label: "Rust".to_owned(),
                        description: String::new(),
                    },
                    CallbackOption {
                        id: "python".to_owned(),
                        label: "Python".to_owned(),
                        description: String::new(),
                    },
                ],
                allows_free_text: false,
                multi_select: true,
            },
            CallbackQuestion {
                header: "Constraint".to_owned(),
                question: "Any constraints?".to_owned(),
                options: vec![
                    CallbackOption {
                        id: "fast".to_owned(),
                        label: "Fast".to_owned(),
                        description: String::new(),
                    },
                    CallbackOption {
                        id: "small".to_owned(),
                        label: "Small".to_owned(),
                        description: String::new(),
                    },
                ],
                allows_free_text: true,
                multi_select: false,
            },
        ];
        let mut state = ControlState::new("session");
        state.begin_turn("turn").expect("turn starts");
        state
            .present_callback(PendingCallback {
                callback_id: "callback".to_owned(),
                session_id: "session".to_owned(),
                turn_id: "turn".to_owned(),
                prompt: "Questions".to_owned(),
                request: CallbackRequest::UserInput {
                    questions,
                    footer_note: None,
                },
            })
            .expect("callback presents");
        let dispatch = state
            .prepare_answer(
                "turn",
                "callback",
                &CallbackChoice::UserInput {
                    answers: vec![
                        UserInputChoice::Options {
                            ids: vec!["rust".to_owned(), "python".to_owned()],
                        },
                        UserInputChoice::FreeText {
                            value: "No network".to_owned(),
                        },
                    ],
                },
            )
            .expect("answer prepares");
        assert_eq!(
            dispatch.params["output"],
            json!({
                "type": "user_input",
                "result": {
                    "answers": [
                        {
                            "question": "Choose runtimes",
                            "answer": "Rust, Python",
                            "isOther": false,
                        },
                        {
                            "question": "Any constraints?",
                            "answer": "No network",
                            "isOther": true,
                        },
                    ],
                    "cancelled": false,
                },
            })
        );
    }
}
